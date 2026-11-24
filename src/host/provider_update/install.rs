//! Official provider update channels. Prepare downloads while launches remain
//! available, then verify the selected executable under the launch fence.

mod claude;
mod muse;
mod npm;

use std::{
    ffi::{CString, OsStr},
    fs,
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

use tempfile::TempDir;
use tokio::process::Command;

use crate::{
    account::provider_install::{
        ProviderInstallMethod, ProviderStatus, read_version, run_provider_command,
    },
    agent::Provider,
};

const UPDATE_TIMEOUT: Duration = Duration::from_secs(240);
const STAGE_PREFIX: &str = ".cagents-provider-update-";
const NPM_IN_USE: &str = "A running Codex process is using this npm package.";

pub(super) enum Preparation {
    Skipped(String),
    Deferred(String),
    Current(String),
    Ready(PreparedUpdate),
}

pub(super) async fn prepare(
    status: &ProviderStatus,
    search_path: Option<&OsStr>,
) -> Result<Preparation, String> {
    let context = CommandContext { search_path };
    if matches!(
        status.install_method,
        ProviderInstallMethod::Native
            | ProviderInstallMethod::Npm { .. }
            | ProviderInstallMethod::Standalone
    ) {
        let current = ProviderStatus::detect(status.provider, search_path);
        if current.executable != status.executable
            || current.resolved_executable != status.resolved_executable
            || current.install_method != status.install_method
        {
            return Err("Provider installation changed during its update check".into());
        }
    }
    match (&status.install_method, status.provider) {
        (ProviderInstallMethod::Native, Provider::Claude) => claude::prepare(status, search_path),
        (ProviderInstallMethod::Native, Provider::Muse) => muse::prepare(status, &context).await,
        (ProviderInstallMethod::Npm { prefix }, Provider::Codex) => {
            npm::prepare(status, prefix, &context).await
        }
        (ProviderInstallMethod::Standalone, Provider::Antigravity | Provider::Omp) => {
            prepare_standalone(status, &context).await
        }
        (ProviderInstallMethod::Missing, _) => {
            Ok(Preparation::Skipped("Executable was not found".into()))
        }
        (ProviderInstallMethod::System, _) => {
            Ok(Preparation::Skipped("Managed by your system".into()))
        }
        (ProviderInstallMethod::VersionManager(manager), _) => Ok(Preparation::Skipped(format!(
            "Managed by {manager}. Use it to update this provider."
        ))),
        _ => Ok(Preparation::Skipped(
            "This installation has no supported automatic update method".into(),
        )),
    }
}

pub(super) enum Applied {
    Deferred(String),
    Current(String),
    Updated(String),
}

enum Activation {
    Staged {
        transaction: Transaction,
        expected_version: String,
    },
    Claude(claude::Backup),
}

pub(super) struct PreparedUpdate {
    installation: SelectedInstall,
    activation: Box<Activation>,
    muse_lock: Option<Box<muse::LiveLock>>,
}

impl PreparedUpdate {
    fn staged(
        installation: SelectedInstall,
        stage: TempDir,
        replacements: Vec<Replacement>,
        expected_version: String,
    ) -> Self {
        Self {
            installation,
            activation: Box::new(Activation::Staged {
                transaction: Transaction {
                    stage: Some(stage),
                    replacements,
                    applied: 0,
                },
                expected_version,
            }),
            muse_lock: None,
        }
    }

    fn hold_muse_lock(mut self, lock: muse::LiveLock) -> Self {
        self.muse_lock = Some(Box::new(lock));
        self
    }

    pub(super) async fn apply(
        self,
        search_path: Option<&OsStr>,
        cancelled: impl Fn() -> bool,
    ) -> Result<Applied, String> {
        let Self {
            installation,
            activation,
            muse_lock: _muse_lock,
        } = self;
        installation.check(search_path)?;
        match *activation {
            Activation::Staged {
                mut transaction,
                expected_version,
            } => {
                let package_roots = transaction
                    .replacements
                    .iter()
                    .filter(|replacement| {
                        replacement
                            .original
                            .as_ref()
                            .is_some_and(|entry| entry.mode & libc::S_IFMT == libc::S_IFDIR)
                    })
                    .map(|replacement| replacement.live.as_path())
                    .collect::<Vec<_>>();
                if npm_running_in(&package_roots)? {
                    return Ok(Applied::Deferred(NPM_IN_USE.into()));
                }
                let result = async {
                    transaction.apply()?;
                    let actual_version = installation.version(search_path).await?;
                    if actual_version != expected_version {
                        return Err(format!(
                            "Updated executable reports {actual_version}; expected {expected_version}"
                        ));
                    }
                    transaction.verify_applied()?;
                    Ok(actual_version)
                }
                .await;

                match result {
                    Ok(version) => {
                        transaction.commit();
                        Ok(Applied::Updated(version))
                    }
                    Err(error) => Err(transaction.failure(error)),
                }
            }
            Activation::Claude(backup) => {
                backup
                    .update(&installation, &CommandContext { search_path }, cancelled)
                    .await
            }
        }
    }
}

struct CommandContext<'a> {
    search_path: Option<&'a OsStr>,
}

impl CommandContext<'_> {
    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command.current_dir("/");
        if let Some(search_path) = self.search_path {
            command.env("PATH", search_path);
        }
        for variable in crate::account::inherited_selections(&|name| std::env::var(name).ok()) {
            command.env_remove(variable);
        }
        command
    }
}

async fn run_update(command: Command) -> Result<Vec<u8>, String> {
    let executable = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let (status, output) = run_provider_command(command, UPDATE_TIMEOUT).await?;
    if !status.success() {
        return Err(format!("{executable} exited with {status}"));
    }
    Ok(output)
}

async fn prepare_standalone(
    status: &ProviderStatus,
    context: &CommandContext<'_>,
) -> Result<Preparation, String> {
    let installation = SelectedInstall::capture(status, context.search_path)?;
    let executable = &installation.resolved;
    let directory = parent(executable)?;
    let stage = stage_in(directory)?;
    let bin = stage.path().join(".local/bin");
    fs::create_dir_all(&bin).map_err(fs_error)?;
    let staged = bin.join(status.provider.executable());

    // The updater receives a different inode. An updater which truncates its
    // own file or leaves a partial download can only damage the staged copy.
    let mut copy = context.command(Path::new("/usr/bin/cp"));
    copy.arg("--reflink=auto")
        .arg("--")
        .arg(executable)
        .arg(&staged);
    run_update(copy).await?;

    // Both standalone installers recognize the conventional user bin
    // directory. OMP locates its update target via PATH. Give it only the
    // staged executable and platform tools, without version-manager shims.
    let search_path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
            .map_err(|error| format!("Cannot construct update PATH: {error}"))?;
    let mut command = context.command(&staged);
    command
        .env_clear()
        .env("PATH", &search_path)
        .env("HOME", stage.path())
        .current_dir(stage.path())
        .arg("update");
    for name in [
        "https_proxy",
        "HTTPS_PROXY",
        "no_proxy",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    run_update(command).await?;

    let expected_version = read_version(status.provider, &staged, Some(&search_path)).await?;
    let replacement = Replacement::new(staged, executable.clone())?;
    installation.check(context.search_path)?;
    if status.version.as_deref() == Some(expected_version.as_str()) {
        return Ok(Preparation::Current(expected_version));
    }
    if status.version.as_deref().is_some_and(|installed| {
        matches!(
            (semver::Version::parse(installed), semver::Version::parse(&expected_version)),
            (Ok(installed), Ok(next)) if next < installed
        )
    }) {
        return Ok(Preparation::Skipped(format!(
            "The updater selected older release {expected_version}"
        )));
    }
    Ok(Preparation::Ready(PreparedUpdate::staged(
        installation,
        stage,
        vec![replacement],
        expected_version,
    )))
}

// Detect replacement of the PATH entry or of the resolved executable during
// staging. Also distinguish an in-place edit from a rename to another inode.
#[derive(Debug, PartialEq, Eq)]
struct EntryStamp {
    device: u64,
    inode: u64,
    change_time: (i64, i64),
    length: u64,
    mode: u32,
}

impl EntryStamp {
    fn read(path: &Path) -> io::Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                change_time: (metadata.ctime(), metadata.ctime_nsec()),
                length: metadata.len(),
                mode: metadata.mode(),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn same_file(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

struct SelectedInstall {
    provider: Provider,
    executable: PathBuf,
    resolved: PathBuf,
    executable_stamp: EntryStamp,
    resolved_stamp: EntryStamp,
    method: ProviderInstallMethod,
}

impl SelectedInstall {
    fn capture(status: &ProviderStatus, search_path: Option<&OsStr>) -> Result<Self, String> {
        let executable = status
            .executable
            .as_ref()
            .ok_or("Provider executable is missing")?;
        let resolved = status
            .resolved_executable
            .as_ref()
            .ok_or("Provider executable could not be resolved")?;
        if executable.canonicalize().map_err(fs_error)? != *resolved {
            return Err("Provider executable changed during its update check".into());
        }
        Ok(Self {
            provider: status.provider,
            executable: executable.clone(),
            resolved: resolved.clone(),
            executable_stamp: EntryStamp::read(executable)
                .map_err(fs_error)?
                .ok_or("Provider executable is missing")?,
            resolved_stamp: EntryStamp::read(resolved)
                .map_err(fs_error)?
                .ok_or("Provider executable is missing")?,
            method: ProviderStatus::detect(status.provider, search_path).install_method,
        })
    }

    fn check(&self, search_path: Option<&OsStr>) -> Result<(), String> {
        let selected = ProviderStatus::detect(self.provider, search_path);
        if selected.executable.as_ref() != Some(&self.executable)
            || selected.resolved_executable.as_ref() != Some(&self.resolved)
            || selected.install_method != self.method
            || EntryStamp::read(&self.executable)
                .map_err(fs_error)?
                .as_ref()
                != Some(&self.executable_stamp)
            || EntryStamp::read(&self.resolved).map_err(fs_error)?.as_ref()
                != Some(&self.resolved_stamp)
        {
            return Err("Provider installation changed during its update check".into());
        }
        Ok(())
    }

    async fn version(&self, search_path: Option<&OsStr>) -> Result<String, String> {
        read_version(self.provider, &self.executable, search_path).await
    }
}

struct Replacement {
    staged: PathBuf,
    live: PathBuf,
    original: Option<EntryStamp>,
    candidate: EntryStamp,
}

impl Replacement {
    fn new(staged: PathBuf, live: PathBuf) -> Result<Self, String> {
        let original = EntryStamp::read(&live).map_err(fs_error)?;
        let candidate = fs::symlink_metadata(&staged).map_err(fs_error)?.file_type();
        if !candidate.is_dir() && !candidate.is_file() {
            return Err(format!("Invalid staged update at {}", staged.display()));
        }
        if let Ok(existing) = fs::symlink_metadata(&live)
            && candidate != existing.file_type()
        {
            return Err(format!("Unexpected installation at {}", live.display()));
        }
        let candidate = EntryStamp::read(&staged)
            .map_err(fs_error)?
            .ok_or("The staged update is missing")?;
        Ok(Self {
            staged,
            live,
            original,
            candidate,
        })
    }

    fn candidate_is_live(&self) -> io::Result<bool> {
        Ok(EntryStamp::read(&self.live)?.is_some_and(|entry| entry.same_file(&self.candidate)))
    }
}

struct Transaction {
    stage: Option<TempDir>,
    replacements: Vec<Replacement>,
    applied: usize,
}

impl Transaction {
    fn apply(&mut self) -> Result<(), String> {
        for replacement in &self.replacements {
            if EntryStamp::read(&replacement.live).map_err(fs_error)? != replacement.original
                || EntryStamp::read(&replacement.staged)
                    .map_err(fs_error)?
                    .as_ref()
                    != Some(&replacement.candidate)
            {
                return Err(format!(
                    "Installation changed during its update check at {}",
                    replacement.live.display()
                ));
            }
        }
        for replacement in &self.replacements {
            let flags = if replacement.original.is_some() {
                libc::RENAME_EXCHANGE
            } else {
                libc::RENAME_NOREPLACE
            };
            rename(&replacement.staged, &replacement.live, flags).map_err(fs_error)?;
            self.applied += 1;
            if let Some(original) = &replacement.original
                && !EntryStamp::read(&replacement.staged)
                    .map_err(fs_error)?
                    .is_some_and(|exchanged| original.same_file(&exchanged))
            {
                return Err(format!(
                    "Installation changed during its update at {}",
                    replacement.live.display()
                ));
            }
        }
        Ok(())
    }

    fn verify_applied(&self) -> Result<(), String> {
        for replacement in &self.replacements {
            if !replacement.candidate_is_live().map_err(fs_error)? {
                return Err(format!(
                    "Installation changed during verification at {}",
                    replacement.live.display()
                ));
            }
        }
        Ok(())
    }

    fn commit(&mut self) {
        self.applied = 0;
    }

    fn rollback(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        while self.applied > 0 {
            self.applied -= 1;
            let replacement = &self.replacements[self.applied];
            let flags = if replacement.original.is_some() {
                libc::RENAME_EXCHANGE
            } else {
                libc::RENAME_NOREPLACE
            };
            let restore = || {
                if !replacement.candidate_is_live()? {
                    return Err(io::Error::other("another installer replaced this path"));
                }
                rename(&replacement.live, &replacement.staged, flags)?;
                if !EntryStamp::read(&replacement.staged)?
                    .is_some_and(|entry| entry.same_file(&replacement.candidate))
                {
                    rename(&replacement.staged, &replacement.live, flags)?;
                    return Err(io::Error::other("another installer replaced this path"));
                }
                Ok(())
            };
            if let Err(error) = restore() {
                errors.push(format!(
                    "Cannot restore {} ({error})",
                    replacement.live.display()
                ));
            }
        }
        if errors.is_empty() {
            return Ok(());
        }
        let mut message = errors.join(". ");
        if let Some(stage) = self.stage.take() {
            message.push_str(&format!(
                ". Update files retained at {}",
                stage.keep().display()
            ));
        }
        Err(message)
    }

    fn failure(&mut self, error: String) -> String {
        match self.rollback() {
            Ok(()) => error,
            Err(rollback) => format!("{error}. {rollback}"),
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if let Err(error) = self.rollback() {
            tracing::error!(%error, "Could not restore provider installation");
        }
    }
}

fn rename(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: The NUL-terminated paths live through the call.
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn parent(path: &Path) -> Result<&Path, String> {
    path.parent()
        .ok_or_else(|| format!("Cannot find installation directory for {}", path.display()))
}

fn stage_in(directory: &Path) -> Result<TempDir, String> {
    tempfile::Builder::new()
        .prefix(STAGE_PREFIX)
        .tempdir_in(directory)
        .map_err(|error| {
            format!(
                "Cannot stage provider update at {}: {error}",
                directory.display()
            )
        })
}

/// Codex caches absolute package paths and uses them to load bundled tools
/// for the lifetime of a session. Swapping an npm package directory under
/// that session could send it a helper built for a newer Codex.
fn npm_running_in(roots: &[&Path]) -> Result<bool, String> {
    if roots.is_empty() {
        return Ok(false);
    }
    let processes = fs::read_dir("/proc")
        .map_err(|error| format!("Cannot check which Codex packages are in use: {error}"))?;
    for process in processes.flatten() {
        if !process
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let process = process.path();
        match fs::read_link(process.join("exe")) {
            Ok(executable) if roots.iter().any(|root| executable.starts_with(root)) => {
                return Ok(true);
            }
            Ok(executable)
                if matches!(
                    executable.file_name().and_then(OsStr::to_str),
                    Some("node" | "nodejs" | "bun" | "sh" | "bash" | "dash")
                ) =>
            {
                // A wrapper can be starting its Codex child while its own
                // executable still points to the interpreter.
                if command_references_packages(&process, roots) {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                // A non-dumpable process can deny access to its executable
                // symlink. Its task name is still readable in the usual proc
                // mount. Defer on an identifiable Codex even without its path.
                if fs::read(process.join("comm")).is_ok_and(|name| name.starts_with(b"codex"))
                    || command_references_packages(&process, roots)
                {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn command_references_packages(process: &Path, roots: &[&Path]) -> bool {
    let mut arguments = Vec::new();
    if fs::File::open(process.join("cmdline"))
        .and_then(|file| file.take(16 * 1024).read_to_end(&mut arguments))
        .is_err()
    {
        return false;
    }
    let cwd = fs::read_link(process.join("cwd")).ok();
    arguments
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty() && !argument.starts_with(b"-"))
        .any(|argument| {
            let path = Path::new(OsStr::from_bytes(argument));
            if path.is_absolute() {
                return refers_to_package(path, roots);
            }
            cwd.as_ref()
                .is_some_and(|cwd| refers_to_package(&cwd.join(path), roots))
        })
}

fn refers_to_package(path: &Path, roots: &[&Path]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
        || path
            .canonicalize()
            .is_ok_and(|resolved| roots.iter().any(|root| resolved.starts_with(root)))
}

fn fs_error(error: io::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::fs::PermissionsExt, process::Stdio};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const FAKE_AGY: &str = r#"#!/bin/sh
set -eu
case "$1" in
  --version) echo 'agy 1.0.0' ;;
  --session)
    echo ready
    read -r line
    echo "old agy: $line"
    ;;
  update)
    test "$0" = "$HOME/.local/bin/agy"
    cat > "$0.next" <<'NEW'
#!/bin/sh
test "$1" = --version
test ! -f "$(dirname "$0")/.reject-update" || exit 9
echo 'agy 2.0.0'
NEW
    chmod +x "$0.next"
    mv "$0.next" "$0"
    ;;
  *) exit 1 ;;
esac
"#;

    pub(super) fn fixture_status(
        provider: Provider,
        entry: &Path,
        method: ProviderInstallMethod,
    ) -> ProviderStatus {
        ProviderStatus {
            provider,
            executable: Some(entry.to_owned()),
            resolved_executable: Some(entry.canonicalize().unwrap()),
            version: Some("1.0.0".into()),
            install_method: method,
            last_checked_at: None,
            last_result: None,
            error: None,
        }
    }

    pub(super) fn write_executable(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub(super) fn fixture_path(directory: &Path) -> OsString {
        std::env::join_paths([directory, Path::new("/usr/bin"), Path::new("/bin")]).unwrap()
    }

    async fn stage_agy(entry: &Path, path: &OsStr) -> PreparedUpdate {
        let status = fixture_status(
            Provider::Antigravity,
            entry,
            ProviderInstallMethod::Standalone,
        );
        let context = CommandContext {
            search_path: Some(path),
        };
        let Preparation::Ready(update) = prepare_standalone(&status, &context).await.unwrap()
        else {
            panic!("expected staged Antigravity update")
        };
        update
    }

    #[tokio::test]
    async fn switched_binary_leaves_running_provider_on_old_inode() {
        let root = tempfile::tempdir().unwrap();
        let agy = root.path().join("agy");
        write_executable(&agy, FAKE_AGY);
        let path = fixture_path(root.path());
        let old_inode = fs::metadata(&agy).unwrap().ino();
        let mut child = Command::new(&agy)
            .arg("--session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ready"));

        let update = stage_agy(&agy, &path).await;
        assert_eq!(fs::metadata(&agy).unwrap().ino(), old_inode);
        assert!(matches!(
            update.apply(Some(&path), || false).await.unwrap(),
            Applied::Updated(version) if version == "2.0.0"
        ));
        assert_ne!(fs::metadata(&agy).unwrap().ino(), old_inode);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"continue\n")
            .await
            .unwrap();
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("old agy: continue")
        );
        assert!(child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn failed_live_verification_restores_standalone_binary() {
        let root = tempfile::tempdir().unwrap();
        let agy = root.path().join("agy");
        write_executable(&agy, FAKE_AGY);
        let path = fixture_path(root.path());
        let old_inode = fs::metadata(&agy).unwrap().ino();
        let update = stage_agy(&agy, &path).await;
        fs::write(root.path().join(".reject-update"), []).unwrap();

        assert!(update.apply(Some(&path), || false).await.is_err());
        assert_eq!(fs::metadata(&agy).unwrap().ino(), old_inode);
        assert_eq!(
            read_version(Provider::Antigravity, &agy, Some(&path))
                .await
                .unwrap(),
            "1.0.0"
        );
    }

    #[tokio::test]
    async fn stale_standalone_update_leaves_external_replacement_in_place() {
        let root = tempfile::tempdir().unwrap();
        let agy = root.path().join("agy");
        write_executable(&agy, FAKE_AGY);
        let path = fixture_path(root.path());
        let update = stage_agy(&agy, &path).await;
        let replacement = root.path().join("replacement");
        write_executable(&replacement, "#!/bin/sh\necho 'agy 3.0.0'\n");
        fs::rename(replacement, &agy).unwrap();

        assert!(update.apply(Some(&path), || false).await.is_err());
        assert_eq!(
            read_version(Provider::Antigravity, &agy, Some(&path))
                .await
                .unwrap(),
            "3.0.0"
        );
    }

    #[tokio::test]
    async fn npm_package_with_a_running_resource_user_is_deferred() {
        let root = tempfile::tempdir().unwrap();
        let entry = root.path().join("codex");
        let package = root.path().join("lib/node_modules/@openai/codex");
        fs::create_dir_all(&package).unwrap();
        write_executable(&entry, "#!/bin/sh\necho 'codex-cli 1.0.0'\n");
        let resource = package.join("resource");
        fs::write(&resource, "previous package resource").unwrap();
        // Use a local ELF fixture so procfs records the actual executable,
        // as it would for Codex, and wait on stdin before opening the resource.
        let fake_client = package.join("fake-codex");
        fs::copy("/usr/bin/cat", &fake_client).unwrap();
        let mut client = Command::new(&fake_client)
            .arg("-")
            .arg(&resource)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let path = fixture_path(root.path());
        let status = fixture_status(
            Provider::Codex,
            &entry,
            ProviderInstallMethod::Npm {
                prefix: root.path().to_owned(),
            },
        );
        let installation = SelectedInstall::capture(&status, Some(&path)).unwrap();
        let stage = stage_in(root.path()).unwrap();
        let next = stage.path().join("codex");
        fs::create_dir(&next).unwrap();
        fs::write(next.join("resource"), "updated package resource").unwrap();
        let replacement = Replacement::new(next, package.clone()).unwrap();
        let update = PreparedUpdate::staged(installation, stage, vec![replacement], "2.0.0".into());

        assert!(matches!(
            update.apply(Some(&path), || false).await.unwrap(),
            Applied::Deferred(_)
        ));
        drop(client.stdin.take());
        let output = client.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"previous package resource");
    }

    #[test]
    fn npm_package_with_a_starting_interpreter_is_in_use() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        fs::create_dir(&package).unwrap();
        let script = package.join("codex");
        write_executable(&script, "#!/bin/sh\nread -r line\n");
        std::os::unix::fs::symlink(&script, root.path().join("codex")).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .current_dir(root.path())
            .arg("./codex")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();

        let running = npm_running_in(&[&package]).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(running);
    }

    #[test]
    fn rollback_keeps_replacement_from_an_external_installer() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("codex");
        fs::write(&live, "old").unwrap();
        let stage = stage_in(root.path()).unwrap();
        let saved = stage.path().join("codex");
        fs::write(&saved, "prepared").unwrap();
        let other_live = root.path().join("platform");
        fs::write(&other_live, "old platform").unwrap();
        let other_saved = stage.path().join("platform");
        fs::write(&other_saved, "prepared platform").unwrap();
        let mut transaction = Transaction {
            stage: Some(stage),
            replacements: vec![
                Replacement::new(other_saved, other_live.clone()).unwrap(),
                Replacement::new(saved.clone(), live.clone()).unwrap(),
            ],
            applied: 0,
        };
        transaction.apply().unwrap();
        let external = root.path().join("external");
        fs::write(&external, "external").unwrap();
        fs::rename(external, &live).unwrap();

        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read_to_string(live).unwrap(), "external");
        assert_eq!(fs::read_to_string(saved).unwrap(), "old");
        assert_eq!(fs::read_to_string(other_live).unwrap(), "old platform");
    }

    #[tokio::test]
    async fn system_and_version_manager_installs_are_skipped() {
        for method in [
            ProviderInstallMethod::System,
            ProviderInstallMethod::VersionManager("mise".into()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let omp = root.path().join("omp");
            write_executable(&omp, "#!/bin/sh\nexit 19\n");
            let status = fixture_status(Provider::Omp, &omp, method);
            let path = fixture_path(root.path());
            assert!(matches!(
                prepare(&status, Some(&path)).await.unwrap(),
                Preparation::Skipped(_)
            ));
        }
    }
}
