//! Meta's launcher selects an adjacent versioned binary using a small state file.

use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::account::provider_install::{ProviderStatus, is_muse_launcher, read_version};

use super::{
    CommandContext, Preparation, PreparedUpdate, Replacement, SelectedInstall, rename, run_update,
    stage_in,
};

const LAUNCHER_LIMIT: u64 = 64 * 1024;
const ACTIVE_RELEASE: &str = ".muse-version";
const RELEASE_INFO: &str = ".muse-release-info.json";
const UPDATE_LOCK: &str = ".muse-update-lock";

pub(super) async fn prepare(
    status: &ProviderStatus,
    context: &CommandContext<'_>,
) -> Result<Preparation, String> {
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
    run_update(command)
        .await
        .map_err(|error| format!("The Muse launcher could not check for an update ({error})"))?;

    let next = selected_release(stage.path())?;
    let version = read_version(status.provider, &staged_launcher, context.search_path).await?;
    installation.check(context.search_path)?;
    if next == release {
        ensure_active_release(directory, &release)?;
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
    private: TempDir,
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
    // The launcher requires a numeric release and an -R build number.
    let valid = |value: &str, lengths: std::ops::RangeInclusive<usize>| {
        let mut parts = value.split('.');
        lengths.contains(&parts.clone().count())
            && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    if contents.len() > 128
        || !release
            .split_once("-R")
            .is_some_and(|(version, build)| valid(version, 3..=3) && valid(build, 1..=2))
    {
        return Err("The active Muse version has an invalid format".into());
    }
    Ok(release.to_owned())
}

#[cfg(test)]
mod tests {
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
        let inode = fs::metadata(&old_binary).unwrap().ino();
        let search = fixture.search_path();
        let context = CommandContext {
            search_path: Some(search.as_os_str()),
        };
        let Preparation::Ready(update) = prepare(&fixture.status, &context).await.unwrap() else {
            panic!("expected a staged update");
        };

        assert_eq!(selected_release(&fixture.directory).unwrap(), OLD_RELEASE);
        assert!(
            !fixture
                .directory
                .join(format!("muse-bin-{NEW_RELEASE}"))
                .exists()
        );
        let Applied::Updated(version) = update
            .apply(Some(search.as_os_str()), || false)
            .await
            .unwrap()
        else {
            panic!("expected an installed update");
        };
        assert_eq!(version, "1.3.0");
        assert_eq!(selected_release(&fixture.directory).unwrap(), NEW_RELEASE);
        assert_eq!(fs::metadata(old_binary).unwrap().ino(), inode);
        assert!(!fixture.directory.join(UPDATE_LOCK).exists());
        assert_eq!(
            fs::read_to_string(fixture.status.executable.unwrap()).unwrap(),
            LAUNCHER
        );
    }

    #[tokio::test]
    async fn leaves_a_live_launcher_update_in_charge() {
        let fixture = Fixture::new();
        let lock = LiveLock::acquire(&fixture.directory).unwrap();
        let search = fixture.search_path();
        let context = CommandContext {
            search_path: Some(search.as_os_str()),
        };
        let result = prepare(&fixture.status, &context).await;
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
        let Preparation::Ready(update) = prepare(&fixture.status, &context).await.unwrap() else {
            panic!("expected a staged update");
        };

        assert!(
            update
                .apply(Some(search.as_os_str()), || false)
                .await
                .is_err()
        );
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
