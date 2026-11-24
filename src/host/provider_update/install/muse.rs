//! Meta's launcher selects an adjacent versioned binary using a small state file.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions, Permissions},
    io::{self, ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use crate::account::provider_install::{ProviderStatus, is_muse_launcher};

use super::{
    CommandContext, EntryStamp, Preparation, PreparedUpdate, Replacement, SelectedInstall, Stage,
    can_run_provider_script, command_references_packages, rename, run_update, stage_in,
};

const LAUNCHER_LIMIT: u64 = 64 * 1024;
const ACTIVE_RELEASE: &str = ".muse-version";
const RELEASE_INFO: &str = ".muse-release-info.json";
const UPDATE_LOCK: &str = ".muse-update-lock";

pub(super) async fn prepare(
    status: &ProviderStatus,
    context: &CommandContext<'_>,
    automatic: bool,
) -> Result<Preparation, String> {
    if automatic && std::env::var_os("MUSE_NO_AUTO_UPDATE").is_some_and(|value| value == "1") {
        return Ok(Preparation::Skipped(
            "Automatic updates are disabled in Muse".into(),
        ));
    }
    let installation = SelectedInstall::capture(status, context.search_path)?;
    let launcher = &installation.resolved;
    let directory = launcher.parent().ok_or("Muse has no install directory")?;
    let release = selected_release(directory)?;
    let binary = format!("muse-bin-{release}");
    let active = directory.join(&binary);
    check_executable(&active)?;

    let stage = stage_in(directory)?;
    let staged_launcher = stage.path().join("muse");
    let mut script = Vec::new();
    owned_file(launcher)?
        .take(LAUNCHER_LIMIT + 1)
        .read_to_end(&mut script)
        .map_err(|error| format!("Could not read the Muse launcher ({error})"))?;
    if script.len() as u64 > LAUNCHER_LIMIT {
        return Err("The Muse launcher is larger than expected".into());
    }
    fs::write(&staged_launcher, script)
        .and_then(|()| fs::set_permissions(&staged_launcher, Permissions::from_mode(0o700)))
        .map_err(|error| format!("Could not stage the Muse launcher ({error})"))?;
    if !is_muse_launcher(&staged_launcher) {
        return Err("The Muse launcher changed before its update could start".into());
    }
    fs::write(stage.path().join(ACTIVE_RELEASE), &release)
        .map_err(|error| format!("Could not stage the installed Muse version ({error})"))?;
    // In the install mode below, the official launcher renames the verified
    // candidate into place and unlinks its old version. The live entry in
    // the parent directory and its executable inode are untouched.
    fs::hard_link(&active, stage.path().join(&binary))
        .map_err(|error| format!("Could not stage the current Muse executable ({error})"))?;

    // curl reads a default config before the launcher's arguments. Keep this
    // update on the provider's channel regardless of URLs in a user's curlrc.
    fs::write(stage.path().join(".curlrc"), []).map_err(|error| error.to_string())?;
    let mut command = context.command(&staged_launcher);
    command
        .current_dir(stage.path())
        .arg("--version")
        .env_remove("BASH_ENV")
        .env("CURL_HOME", stage.path())
        .env("MUSE_LAUNCHER_INSTALL", "1")
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .env("MUSE_LOGIN", "0");
    stage.protect(&mut command);
    run_update(command)
        .await
        .map_err(|error| format!("The Muse launcher could not check for an update ({error})"))?;

    let next = selected_release(stage.path())?;
    let version = stage
        .version(status.provider, &staged_launcher, context.search_path)
        .await?;
    installation.check(context.search_path)?;
    if next == release {
        ensure_active_release(directory, &release)?;
        // A predecessor kept for a live process can be retired at the next
        // check once that process exits, even when there is no newer release.
        if let Err(error) = retire_on_current(directory, &release) {
            tracing::debug!(%error, "Could not retire previous Muse releases");
        }
        return Ok(Preparation::Current(version));
    }

    let lock = LiveLock::acquire(directory)?;
    ensure_active_release(directory, &release)?;
    let next_binary = format!("muse-bin-{next}");
    check_executable(&stage.path().join(&next_binary))?;
    owned_file(&stage.path().join(RELEASE_INFO))?;
    // Publish the complete candidate before changing the version the launcher selects.
    let replacements = [next_binary.as_str(), RELEASE_INFO, ACTIVE_RELEASE]
        .into_iter()
        .map(|name| {
            let live = directory.join(name);
            // A foreign link, directory or file is outside the supported layout.
            match fs::symlink_metadata(&live) {
                Ok(_) => {
                    owned_file(&live)?;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
            Replacement::new(stage.path().join(name), live)
        })
        .collect::<Result<Vec<_>, _>>()?;
    installation.check(context.search_path)?;
    let update = PreparedUpdate::staged(installation, stage, replacements, version);
    Ok(Preparation::Ready(update.hold_muse_lock(lock)))
}

fn ensure_active_release(directory: &Path, release: &str) -> Result<(), String> {
    if selected_release(directory)? != release {
        return Err("Muse changed while its update was being prepared".into());
    }
    Ok(())
}

/// The official launcher also updates on startup. Its PID lock keeps that
/// updater from changing the active version or pruning a rollback target
/// while cagents publishes and verifies a staged update.
pub(super) struct LiveLock {
    live: PathBuf,
    private: Stage,
    identity: (u64, u64),
}

impl LiveLock {
    fn acquire(directory: &Path) -> Result<Self, String> {
        let private = stage_in(directory)?;
        let staged = private.path().join(UPDATE_LOCK);
        fs::create_dir(&staged).map_err(|error| error.to_string())?;
        fs::write(staged.join("pid"), format!("{}\n", std::process::id()))
            .map_err(|error| error.to_string())?;
        let metadata = fs::metadata(&staged).map_err(|error| error.to_string())?;
        let live = directory.join(UPDATE_LOCK);
        // The launcher discards locks with no live PID. Publish the entire
        // directory at once so it cannot see an incomplete lock.
        if let Err(error) = rename(&staged, &live, libc::RENAME_NOREPLACE) {
            return Err(match error.kind() {
                ErrorKind::AlreadyExists => "Muse's installer already holds the update lock".into(),
                _ => format!("Could not lock the Muse installation ({error})"),
            });
        }
        Ok(Self {
            live,
            private,
            identity: (metadata.dev(), metadata.ino()),
        })
    }

    pub(super) fn retire_previous(&self) {
        let result = || {
            let directory = self.live.parent().ok_or("Muse has no install directory")?;
            let active = selected_release(directory)?;
            let candidates = retired_binaries(directory, &active)?;
            retire_unoccupied(directory, &active, candidates)
        };
        if let Err(error) = result() {
            tracing::debug!(%error, "Could not retire previous Muse releases");
        }
    }
}

impl Drop for LiveLock {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.live)
            .is_ok_and(|metadata| self.identity == (metadata.dev(), metadata.ino()))
            && let Err(error) = rename(
                &self.live,
                &self.private.path().join(UPDATE_LOCK),
                libc::RENAME_NOREPLACE,
            )
        {
            tracing::warn!(%error, "Could not release the Muse update lock");
        }
    }
}

fn retire_on_current(directory: &Path, active: &str) -> Result<(), String> {
    let candidates = retired_binaries(directory, active)?;
    if candidates.is_empty() {
        return Ok(());
    }
    let _lock = LiveLock::acquire(directory)?;
    retire_unoccupied(directory, active, candidates)
}

fn retired_binaries(directory: &Path, active: &str) -> Result<Vec<(PathBuf, EntryStamp)>, String> {
    let active = release_key(active).ok_or("The active Muse version has an invalid format")?;
    let entries = fs::read_dir(directory).map_err(|error| error.to_string())?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let Some(release) = name
            .to_str()
            .and_then(|name| name.strip_prefix("muse-bin-"))
            .and_then(release_key)
        else {
            continue;
        };
        if release >= active {
            continue;
        }
        let path = entry.path();
        if let Ok(metadata) =
            owned_file(&path).and_then(|file| file.metadata().map_err(|error| error.to_string()))
            && let Some(stamp) = EntryStamp::read(&path).map_err(|error| error.to_string())?
            && stamp.mode & libc::S_IFMT == libc::S_IFREG
            && stamp.mode & 0o111 != 0
            && (stamp.device, stamp.inode) == (metadata.dev(), metadata.ino())
        {
            candidates.push((path, stamp));
        }
    }
    Ok(candidates)
}

fn retire_unoccupied(
    directory: &Path,
    active: &str,
    candidates: Vec<(PathBuf, EntryStamp)>,
) -> Result<(), String> {
    ensure_active_release(directory, active)?;
    if candidates.is_empty() {
        return Ok(());
    }
    let in_use = running_binaries(directory, &candidates).map_err(|error| error.to_string())?;
    for (binary, stamp) in candidates {
        if !in_use.contains(&(stamp.device, stamp.inode))
            && EntryStamp::read(&binary).map_err(|error| error.to_string())? == Some(stamp)
        {
            fs::remove_file(&binary).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn running_binaries(
    directory: &Path,
    candidates: &[(PathBuf, EntryStamp)],
) -> io::Result<HashSet<(u64, u64)>> {
    let mut in_use = HashSet::new();
    let launcher = directory.join("muse");
    let upstream = directory.join("muse-upstream");
    let launchers = [launcher.as_path(), upstream.as_path()];
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let process = entry.path();
        match fs::metadata(process.join("exe")) {
            Ok(_)
                if fs::read_link(process.join("exe"))
                    .is_ok_and(|path| can_run_provider_script(&path))
                    && command_references_packages(&process, &launchers) =>
            {
                // A launcher may have read the previous release before
                // publication and still be about to exec it.
                return Ok(candidates
                    .iter()
                    .map(|(_, stamp)| (stamp.device, stamp.inode))
                    .collect());
            }
            Ok(executable) => {
                in_use.insert((executable.dev(), executable.ino()));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied
                ) =>
            {
                // procfs can hide the executable of a running process from
                // this user. Preserve matching task names too.
                if let Ok(name) = fs::read(process.join("comm")) {
                    let name = name.strip_suffix(b"\n").unwrap_or(&name);
                    for (path, stamp) in candidates {
                        let binary = path.file_name().unwrap_or_default().as_encoded_bytes();
                        // Linux task names contain at most 15 bytes.
                        if name == &binary[..binary.len().min(15)] {
                            in_use.insert((stamp.device, stamp.inode));
                        }
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(in_use)
}

fn owned_file(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("Could not read {} ({error})", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "{} is not owned by the current user",
            path.display()
        ));
    }
    Ok(file)
}

fn check_executable(path: &Path) -> Result<(), String> {
    let file = owned_file(path)?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if metadata.mode() & 0o111 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(())
}

fn selected_release(directory: &Path) -> Result<String, String> {
    let mut contents = String::new();
    owned_file(&directory.join(ACTIVE_RELEASE))?
        .take(129)
        .read_to_string(&mut contents)
        .map_err(|error| format!("Could not read the active Muse version ({error})"))?;
    let release = contents.trim_end_matches('\n');
    if contents.len() > 128 || release_key(release).is_none() {
        return Err("The active Muse version has an invalid format".into());
    }
    Ok(release.to_owned())
}

fn release_key(release: &str) -> Option<[(usize, &str); 5]> {
    // The launcher requires a numeric release and an -R build number.
    let (version, build) = release.split_once("-R")?;
    let mut version = version.split('.');
    let (major, minor, patch) = (version.next()?, version.next()?, version.next()?);
    if version.next().is_some() {
        return None;
    }
    let (build, revision) = build.split_once('.').unwrap_or((build, "0"));
    let mut key = [(0, ""); 5];
    for (slot, part) in key.iter_mut().zip([major, minor, patch, build, revision]) {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let digits = part.trim_start_matches('0');
        *slot = (digits.len(), digits);
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use tempfile::TempDir;

    use crate::{account::provider_install::ProviderInstallMethod, agent::Provider};

    use super::{super::Applied, *};

    const OLD_RELEASE: &str = "1.2.3-R5";
    const NEW_RELEASE: &str = "1.3.0-R1.2";

    const LAUNCHER: &str = r#"#!/bin/sh
set -eu
# Matches the layout and explicit install entry point of Meta's launcher.
launcher_url="https://api.meta.ai/muse-launcher.sh"
dir=$(dirname "$0")
state="$dir/.muse-version"
before=$(cat "$state")
if [ "${MUSE_LAUNCHER_INSTALL:-0}" = 1 ]; then
    [ "${MUSE_LOGIN:-1}" = 0 ]
    [ -f "$CURL_HOME/.curlrc" ]
    if [ "$before" = "1.3.0-R1.2" ]; then exit 0; fi
    cat > "$dir/muse-bin-1.3.0-R1.2" <<'BINARY'
#!/bin/sh
case "$1" in
    --version)
        [ ! -e "$(dirname "$0")/.reject-activation" ] || exit 9
        printf 'muse 1.3.0\n'
        ;;
    *) exit 3 ;;
esac
BINARY
    chmod +x "$dir/muse-bin-1.3.0-R1.2"
    echo '{"version":"1.3.0-R1.2"}' > "$dir/.muse-release-info.json"
    printf '1.3.0-R1.2\n' > "$state"
    rm "$dir/muse-bin-$before"
    exit 0
fi
[ "${MUSE_NO_AUTO_UPDATE:-0}" = 1 ]
exec "$dir/muse-bin-$before" "$@"
"#;

    struct Fixture {
        _root: TempDir,
        directory: PathBuf,
        status: ProviderStatus,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join(".local/bin");
            fs::create_dir_all(&directory).unwrap();
            let directory = directory.canonicalize().unwrap();
            let launcher = directory.join("muse");
            write_executable(&launcher, LAUNCHER);
            write_executable(
                &directory.join(format!("muse-bin-{OLD_RELEASE}")),
                "#!/bin/sh\nprintf 'muse 1.2.3\\n'\n",
            );
            fs::write(directory.join(ACTIVE_RELEASE), format!("{OLD_RELEASE}\n")).unwrap();
            fs::write(directory.join(RELEASE_INFO), "{}").unwrap();
            let status = ProviderStatus {
                provider: Provider::Muse,
                executable: Some(launcher.clone()),
                resolved_executable: Some(launcher),
                version: Some("1.2.3".into()),
                install_method: ProviderInstallMethod::Native,
                last_checked_at: None,
                last_result: None,
                error: None,
            };
            Self {
                _root: root,
                directory,
                status,
            }
        }

        fn search_path(&self) -> std::ffi::OsString {
            std::env::join_paths([
                self.directory.as_path(),
                Path::new("/usr/bin"),
                Path::new("/bin"),
            ])
            .unwrap()
        }
    }

    fn write_executable(path: &Path, source: &str) {
        fs::write(path, source).unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o700)).unwrap();
    }

    #[tokio::test]
    async fn selects_a_working_version_after_staging() {
        let fixture = Fixture::new();
        let old_binary = fixture.directory.join(format!("muse-bin-{OLD_RELEASE}"));
        let search = fixture.search_path();
        let context = CommandContext {
            search_path: Some(search.as_os_str()),
        };
        let Preparation::Ready(mut update) =
            prepare(&fixture.status, &context, false).await.unwrap()
        else {
            panic!("expected a staged update");
        };

        assert_eq!(selected_release(&fixture.directory).unwrap(), OLD_RELEASE);
        assert!(
            !fixture
                .directory
                .join(format!("muse-bin-{NEW_RELEASE}"))
                .exists()
        );
        let Applied::Updated(version) = update.apply(Some(search.as_os_str())).await.unwrap()
        else {
            panic!("expected an installed update");
        };
        assert_eq!(version, "1.3.0");
        assert_eq!(selected_release(&fixture.directory).unwrap(), NEW_RELEASE);
        update.retire_previous();
        assert!(!old_binary.exists());
        drop(update);
        assert!(!fixture.directory.join(UPDATE_LOCK).exists());
        assert_eq!(
            fs::read_to_string(fixture.status.executable.unwrap()).unwrap(),
            LAUNCHER
        );
    }

    #[tokio::test]
    async fn retires_the_running_previous_release_after_it_exits() {
        let fixture = Fixture::new();
        let previous = fixture.directory.join(format!("muse-bin-{OLD_RELEASE}"));
        fs::copy("/usr/bin/cat", &previous).unwrap();
        let mut child = tokio::process::Command::new(&previous)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let search = fixture.search_path();
        let context = CommandContext {
            search_path: Some(search.as_os_str()),
        };
        let Preparation::Ready(mut update) =
            prepare(&fixture.status, &context, false).await.unwrap()
        else {
            panic!("expected a staged update");
        };
        assert!(matches!(
            update.apply(Some(search.as_os_str())).await.unwrap(),
            Applied::Updated(_)
        ));
        update.retire_previous();
        drop(update);
        assert!(previous.exists());

        drop(child.stdin.take());
        assert!(child.wait().await.unwrap().success());
        assert!(matches!(
            prepare(&fixture.status, &context, false).await.unwrap(),
            Preparation::Current(version) if version == "1.3.0"
        ));
        assert!(!previous.exists());
    }

    #[tokio::test]
    async fn leaves_a_live_launcher_update_in_charge() {
        let fixture = Fixture::new();
        let lock = LiveLock::acquire(&fixture.directory).unwrap();
        let search = fixture.search_path();
        let context = CommandContext {
            search_path: Some(search.as_os_str()),
        };
        let result = prepare(&fixture.status, &context, false).await;
        assert!(matches!(result, Err(error) if error.contains("already holds")));
        assert_eq!(selected_release(&fixture.directory).unwrap(), OLD_RELEASE);
        assert!(fixture.directory.join(UPDATE_LOCK).exists());
        drop(lock);
        assert!(!fixture.directory.join(UPDATE_LOCK).exists());
    }

    #[tokio::test]
    async fn restores_the_previous_release_on_activation_failure() {
        let fixture = Fixture::new();
        fs::write(fixture.directory.join(".reject-activation"), "").unwrap();
        let search = fixture.search_path();
        let context = CommandContext {
            search_path: Some(search.as_os_str()),
        };
        let Preparation::Ready(mut update) =
            prepare(&fixture.status, &context, false).await.unwrap()
        else {
            panic!("expected a staged update");
        };

        assert!(update.apply(Some(search.as_os_str())).await.is_err());
        assert_eq!(selected_release(&fixture.directory).unwrap(), OLD_RELEASE);
        assert_eq!(
            fs::read_to_string(fixture.directory.join(RELEASE_INFO)).unwrap(),
            "{}"
        );
        assert!(
            !fixture
                .directory
                .join(format!("muse-bin-{NEW_RELEASE}"))
                .exists()
        );
    }
}
