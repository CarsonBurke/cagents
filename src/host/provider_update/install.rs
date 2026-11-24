//! Official provider update channels. Prepare downloads while launches remain
//! available, then verify the selected executable under the launch fence.

mod claude;
mod muse;
mod npm;
mod staging;

use std::{
    collections::BTreeSet,
    ffi::{CString, OsStr, OsString},
    fs,
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::process::Command;

use crate::{
    account::provider_install::{
        ProviderInstallMethod, ProviderStatus, read_version, run_provider_command,
    },
    agent::Provider,
};

const UPDATE_TIMEOUT: Duration = Duration::from_secs(240);
const NPM_IN_USE: &str = "A running Codex process is using this npm package.";

use staging::{Stage, collect_stages, stage_in};

pub(super) enum Preparation {
    Skipped(String),
    Deferred(String),
    Current(String),
    Ready(PreparedUpdate),
}

pub(super) async fn prepare(
    status: &ProviderStatus,
    search_path: Option<&OsStr>,
    automatic: bool,
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
        for directory in recovery_directories(status, search_path) {
            collect_stages(&directory);
        }
    }
    match (&status.install_method, status.provider) {
        (ProviderInstallMethod::Native, Provider::Claude) => {
            claude::prepare(status, &context, automatic).await
        }
        (ProviderInstallMethod::Native, Provider::Muse) => {
            muse::prepare(status, &context, automatic).await
        }
        (ProviderInstallMethod::Npm { prefix }, Provider::Codex) => {
            npm::prepare(status, prefix, &context, automatic).await
        }
        (ProviderInstallMethod::Standalone, Provider::Antigravity | Provider::Omp) => {
            prepare_standalone(status, &context, automatic).await
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

pub(super) fn recovery_issue(
    status: &ProviderStatus,
    search_path: Option<&OsStr>,
) -> Option<String> {
    let issues = recovery_directories(status, search_path)
        .iter()
        .flat_map(|directory| staging::recovery_issues(directory, status.provider))
        .collect::<Vec<_>>();
    (!issues.is_empty()).then(|| issues.join(". "))
}

#[derive(Debug)]
pub(super) struct RecoveryError {
    pub(super) deferred: bool,
    pub(super) reason: String,
}

pub(super) fn recover(
    status: &ProviderStatus,
    search_path: Option<&OsStr>,
) -> Result<(), RecoveryError> {
    let issues = recovery_directories(status, search_path)
        .iter()
        .flat_map(|directory| staging::reap_stages(directory, status.provider))
        .collect::<BTreeSet<_>>();
    if issues.is_empty() {
        Ok(())
    } else {
        Err(RecoveryError {
            deferred: issues
                .iter()
                .all(|(kind, _)| *kind == io::ErrorKind::WouldBlock),
            reason: issues
                .into_iter()
                .map(|(_, error)| error)
                .collect::<Vec<_>>()
                .join(". "),
        })
    }
}

fn recovery_directories(status: &ProviderStatus, search_path: Option<&OsStr>) -> BTreeSet<PathBuf> {
    use crate::agent::Provider::{Antigravity, Claude, Codex, Muse, Omp};

    if !matches!(status.provider, Claude | Codex | Muse | Omp | Antigravity) {
        return BTreeSet::new();
    }
    let mut directories = BTreeSet::new();
    let mut add = |directory: &Path| {
        if let Ok(directory) = directory.canonicalize()
            && directory.is_dir()
        {
            directories.insert(directory);
        }
    };
    for executable in [&status.executable, &status.resolved_executable]
        .into_iter()
        .flatten()
    {
        if let Some(directory) = executable.parent() {
            add(directory);
        }
    }
    if let ProviderInstallMethod::Npm { prefix } = &status.install_method {
        add(&prefix.join(npm::SCOPE_DIRECTORY));
    }
    if let Some(base) = directories::BaseDirs::new() {
        if status.provider != Codex {
            add(&base.home_dir().join(".local/bin"));
        }
        if status.provider == Claude {
            add(&base.data_dir().join("claude/versions"));
            add(&base.home_dir().join(".local/share/claude/versions"));
        }
    }
    // Follow npm launcher links even when their target is missing. A later
    // Codex on PATH can otherwise conceal a partial publication in the first.
    if status.provider == Codex
        && let Some(search_path) = search_path
    {
        for bin in std::env::split_paths(search_path).filter(|bin| bin.is_absolute()) {
            let mut executable = bin.join(status.provider.executable());
            for _ in 0..40 {
                if let Some(scope) = executable
                    .ancestors()
                    .find(|path| path.ends_with(npm::SCOPE_DIRECTORY))
                {
                    add(scope);
                    break;
                }
                let Ok(target) = fs::read_link(&executable) else {
                    break;
                };
                executable = executable.with_file_name(target);
            }
        }
    }
    directories
}

pub(super) enum Applied {
    Deferred(String),
    Updated(String),
}

pub(super) struct PreparedUpdate {
    installation: SelectedInstall,
    transaction: Box<Transaction>,
    muse_lock: Option<Box<muse::LiveLock>>,
}

impl PreparedUpdate {
    fn staged(
        installation: SelectedInstall,
        stage: Stage,
        replacements: Vec<Replacement>,
        expected_version: String,
    ) -> Self {
        Self {
            installation,
            transaction: Box::new(Transaction {
                stages: vec![stage],
                replacements,
                applied: 0,
                expected_version,
            }),
            muse_lock: None,
        }
    }

    fn hold_stage(mut self, stage: Stage) -> Self {
        self.transaction.stages.push(stage);
        self
    }

    fn hold_muse_lock(mut self, lock: muse::LiveLock) -> Self {
        self.muse_lock = Some(Box::new(lock));
        self
    }

    pub(super) fn retire_previous(&self) {
        if let Some(lock) = &self.muse_lock {
            lock.retire_previous();
        }
    }

    pub(super) fn opt_out(&self, automatic: bool) -> Result<Option<String>, String> {
        if self.installation.provider == Provider::Claude {
            return claude::opt_out(automatic, &self.transaction.expected_version);
        }
        Ok(None)
    }

    pub(super) async fn apply(&mut self, search_path: Option<&OsStr>) -> Result<Applied, String> {
        self.installation.check(search_path)?;
        if matches!(
            self.installation.provider,
            Provider::Antigravity | Provider::Omp
        ) && self.installation.standalone_running()?
        {
            return Ok(Applied::Deferred(self.installation.in_use_reason()));
        }
        let package_roots = self
            .transaction
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
            self.transaction.apply(self.installation.provider)?;
            let actual_version = self.installation.version(search_path).await?;
            if actual_version != self.transaction.expected_version {
                return Err(format!(
                    "Updated executable reports {actual_version}; expected {}",
                    self.transaction.expected_version
                ));
            }
            self.transaction.verify_applied()?;
            Ok(actual_version)
        }
        .await;
        match result {
            Ok(version) => {
                if let Err(error) = self.transaction.commit() {
                    return Err(self.transaction.failure(error));
                }
                Ok(Applied::Updated(version))
            }
            Err(error) => Err(self.transaction.failure(error)),
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

fn isolated_installer(
    executable: &Path,
    network_variables: &[&str],
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .envs(environment.into_iter().filter(|(name, _)| {
            let name = name.as_encoded_bytes();
            matches!(name, b"TMPDIR" | b"LANG" | b"LANGUAGE")
                || name.starts_with(b"LC_")
                || network_variables
                    .iter()
                    .any(|allowed| name == allowed.as_bytes())
        }));
    command
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
    automatic: bool,
) -> Result<Preparation, String> {
    if automatic
        && status.provider == Provider::Antigravity
        && std::env::var("AGY_CLI_DISABLE_AUTO_UPDATE")
            .is_ok_and(|value| value.eq_ignore_ascii_case("true"))
    {
        return Ok(Preparation::Skipped(
            "Automatic updates are disabled in Antigravity".into(),
        ));
    }
    let installation = SelectedInstall::capture(status, context.search_path)?;
    let version = status
        .version
        .as_deref()
        .ok_or("Installed version is missing")?;
    let installed = semver::Version::parse(version).map_err(|error| error.to_string())?;
    let (available, canary) = match status.provider {
        Provider::Antigravity => (Some(agy_latest(context).await?), false),
        Provider::Omp => omp_latest(&installation.resolved, context).await?,
        _ => unreachable!("only the standalone updaters reach this function"),
    };
    installation.check(context.search_path)?;
    if available.is_none_or(|available| available.cmp_precedence(&installed).is_le()) {
        return Ok(Preparation::Current(version.into()));
    }
    stage_standalone(installation, &installed, canary, context).await
}

async fn agy_latest(context: &CommandContext<'_>) -> Result<semver::Version, String> {
    // The official installer consults this platform manifest. agy update has
    // no check-only mode, so read only the release metadata here.
    let (arch, musl_loader) = match std::env::consts::ARCH {
        "x86_64" => ("amd64", "/lib/libc.musl-x86_64.so.1"),
        "aarch64" => ("arm64", "/lib/libc.musl-aarch64.so.1"),
        _ => return Err("Antigravity has no release for this architecture".into()),
    };
    let flavor = if cfg!(target_env = "musl") || Path::new(musl_loader).exists() {
        "_musl"
    } else {
        ""
    };
    let mut command = context.command(Path::new("/usr/bin/curl"));
    command.args([
        "--disable",
        "--fail",
        "--silent",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-time",
        "30",
        "--max-filesize",
        "32768",
    ]);
    command.arg(format!(
        "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/linux_{arch}{flavor}.json"
    ));
    let output = run_update(command)
        .await
        .map_err(|error| format!("Could not read the Antigravity release manifest ({error})"))?;
    #[derive(serde::Deserialize)]
    struct ManifestVersion {
        version: String,
    }
    let manifest: ManifestVersion = serde_json::from_slice(&output)
        .map_err(|error| format!("Invalid Antigravity release manifest ({error})"))?;
    semver::Version::parse(&manifest.version)
        .map_err(|error| format!("Invalid Antigravity release version ({error})"))
}

async fn omp_latest(
    executable: &Path,
    context: &CommandContext<'_>,
) -> Result<(Option<semver::Version>, bool), String> {
    // The check-only command uses OMP's own selection of an update channel.
    // Reuse its existing native modules cache instead of extracting it into
    // a temporary home just to run the version check.
    let mut command = context.command(executable);
    command
        .env("PATH", standalone_path(parent(executable)?)?)
        .env("NO_COLOR", "1")
        .env_remove("FORCE_COLOR")
        .args(["update", "--check"]);
    let output = run_update(command).await?;
    let output = String::from_utf8_lossy(&output);
    let canary = output.lines().any(|line| line == "Current channel: canary");
    if let Some(version) = output
        .lines()
        .find_map(|line| line.strip_prefix("New version available: "))
    {
        let version = semver::Version::parse(version.trim())
            .map_err(|error| format!("Invalid OMP release version ({error})"))?;
        return Ok((Some(version), canary));
    }
    if output
        .lines()
        .any(|line| line.contains("Already up to date"))
    {
        return Ok((None, canary));
    }
    Err("OMP did not report whether an update is available".into())
}

fn standalone_path(bin: &Path) -> Result<std::ffi::OsString, String> {
    std::env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")])
        .map_err(|error| format!("Cannot construct update PATH: {error}"))
}

async fn stage_standalone(
    installation: SelectedInstall,
    installed: &semver::Version,
    canary: bool,
    context: &CommandContext<'_>,
) -> Result<Preparation, String> {
    if installation.standalone_running()? {
        return Ok(Preparation::Deferred(installation.in_use_reason()));
    }
    let provider = installation.provider;
    let executable = &installation.resolved;
    let directory = parent(executable)?;
    let stage = stage_in(directory)?;
    let bin = stage.path().join(".local/bin");
    fs::create_dir_all(&bin).map_err(fs_error)?;
    let staged = bin.join(provider.executable());

    // The updater receives a different inode. An updater which truncates its
    // own file or leaves a partial download can only damage the staged copy.
    let mut copy = context.command(Path::new("/usr/bin/cp"));
    copy.arg("--reflink=auto")
        .arg("--")
        .arg(executable)
        .arg(&staged);
    stage.protect(&mut copy);
    run_update(copy).await?;

    // Both standalone installers recognize the conventional user bin
    // directory. OMP locates its update target via PATH. Give it only the
    // staged executable and platform tools, without version-manager shims.
    let search_path = standalone_path(&bin)?;
    let mut command = isolated_installer(
        &staged,
        &[
            "https_proxy",
            "HTTPS_PROXY",
            "no_proxy",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ],
        std::env::vars_os(),
    );
    command
        .env("PATH", &search_path)
        .env("HOME", stage.path())
        .current_dir(stage.path())
        .arg("update");
    if canary {
        command.arg("--canary");
    }
    stage.protect(&mut command);
    run_update(command).await?;

    let expected_version = stage.version(provider, &staged, Some(&search_path)).await?;
    let next = semver::Version::parse(&expected_version).map_err(|error| error.to_string())?;
    let replacement = Replacement::new(staged, executable.clone())?;
    installation.check(context.search_path)?;
    if next.cmp_precedence(installed).is_eq() {
        return Ok(Preparation::Current(expected_version));
    }
    if next.cmp_precedence(installed).is_lt() {
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    fn standalone_running(&self) -> Result<bool, String> {
        standalone_running(&[&self.executable, &self.resolved], &[&self.resolved_stamp]).map_err(
            |error| {
                format!(
                    "Cannot check whether {} is in use: {error}",
                    self.provider.label()
                )
            },
        )
    }

    fn in_use_reason(&self) -> String {
        format!(
            "A running {} process is using this installation.",
            self.provider.label()
        )
    }
}

fn standalone_running(paths: &[&Path], stamps: &[&EntryStamp]) -> io::Result<bool> {
    let uid = crate::daemon::current_uid();
    for process in provider_processes()? {
        let running = match fs::metadata(process.join("exe")) {
            Ok(metadata) => stamps
                .iter()
                .any(|stamp| metadata.dev() == stamp.device && metadata.ino() == stamp.inode),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                fs::read(process.join("comm")).is_ok_and(|name| {
                    let name = name.strip_suffix(b"\n").unwrap_or(&name);
                    paths
                        .iter()
                        .filter_map(|path| path.file_name())
                        .any(|binary| {
                            let binary = binary.as_encoded_bytes();
                            name == &binary[..binary.len().min(15)]
                        })
                }) && process_may_share_uid(&process, uid)
            }
            Err(_) => false,
        };
        if running && !version_only(&process) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Replacement {
    #[serde(with = "crate::selected_file_path::required")]
    staged: PathBuf,
    #[serde(with = "crate::selected_file_path::required")]
    live: PathBuf,
    original: Option<EntryStamp>,
    candidate: EntryStamp,
}

impl Replacement {
    fn new(staged: PathBuf, live: PathBuf) -> Result<Self, String> {
        Self::with_type(staged, live, |kind| kind.is_dir() || kind.is_file())
    }

    fn symlink(staged: PathBuf, live: PathBuf) -> Result<Self, String> {
        Self::with_type(staged, live, |kind| kind.is_symlink())
    }

    fn with_type(
        staged: PathBuf,
        live: PathBuf,
        accepts: impl FnOnce(fs::FileType) -> bool,
    ) -> Result<Self, String> {
        let original = EntryStamp::read(&live).map_err(fs_error)?;
        let candidate = fs::symlink_metadata(&staged).map_err(fs_error)?.file_type();
        if !accepts(candidate) {
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

    fn rollback_operation(&self) -> io::Result<Option<(&Path, &Path, u32)>> {
        let live = EntryStamp::read(&self.live)?;
        let staged = EntryStamp::read(&self.staged)?;
        let matches = |actual: &Option<EntryStamp>, expected: &EntryStamp| {
            actual
                .as_ref()
                .is_some_and(|entry| entry.same_file(expected))
        };
        let operation = match &self.original {
            Some(original)
                if matches(&live, original)
                    && (staged.is_none() || matches(&staged, &self.candidate)) =>
            {
                None
            }
            Some(original) if matches(&staged, original) && matches(&live, &self.candidate) => {
                Some((&*self.live, &*self.staged, libc::RENAME_EXCHANGE))
            }
            Some(original) if matches(&staged, original) && live.is_none() => {
                Some((&*self.staged, &*self.live, libc::RENAME_NOREPLACE))
            }
            None if live.is_none() && matches(&staged, &self.candidate) => None,
            None if matches(&live, &self.candidate) && staged.is_none() => {
                Some((&*self.live, &*self.staged, libc::RENAME_NOREPLACE))
            }
            _ => {
                return Err(io::Error::other(format!(
                    "the live or saved installation at {} changed",
                    self.live.display()
                )));
            }
        };
        Ok(operation)
    }

    fn restore(&self) -> io::Result<()> {
        let Some((from, to, flags)) = self.rollback_operation()? else {
            return Ok(());
        };
        rename(from, to, flags)?;
        if !matches!(self.rollback_operation(), Ok(None)) {
            rename(to, from, flags)?;
            return Err(io::Error::other(format!(
                "the installation at {} changed during recovery",
                self.live.display()
            )));
        }
        Ok(())
    }

    fn undo_applied(&self) -> io::Result<()> {
        if !self.candidate_is_live()? {
            return Err(io::Error::other("another installer replaced this path"));
        }
        let flags = if self.original.is_some() {
            libc::RENAME_EXCHANGE
        } else {
            libc::RENAME_NOREPLACE
        };
        rename(&self.live, &self.staged, flags)?;
        if !EntryStamp::read(&self.staged)?.is_some_and(|entry| entry.same_file(&self.candidate)) {
            rename(&self.staged, &self.live, flags)?;
            return Err(io::Error::other("another installer replaced this path"));
        }
        Ok(())
    }
}

struct Transaction {
    stages: Vec<Stage>,
    replacements: Vec<Replacement>,
    applied: usize,
    expected_version: String,
}

impl Transaction {
    fn apply(&mut self, provider: Provider) -> Result<(), String> {
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
        // Once the first rename succeeds these stages may contain the only
        // previous copy of a live file. Keep them if the process crashes.
        staging::begin_apply(&self.stages, provider, &self.replacements).map_err(fs_error)?;
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

    fn commit(&mut self) -> Result<(), String> {
        self.clear_recovery().map_err(fs_error)?;
        self.applied = 0;
        Ok(())
    }

    fn clear_recovery(&self) -> io::Result<()> {
        if let Some(primary) = self.stages.first() {
            primary.finish_apply()?;
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        while self.applied > 0 {
            self.applied -= 1;
            let replacement = &self.replacements[self.applied];
            if let Err(error) = replacement.undo_applied() {
                errors.push(format!(
                    "Cannot restore {} ({error})",
                    replacement.live.display()
                ));
            }
        }
        if errors.is_empty() {
            return Ok(());
        }
        Err(self.retain_stages(errors.join(". ")))
    }

    fn retain_stages(&mut self, mut message: String) -> String {
        for stage in self.stages.drain(..) {
            message.push_str(&format!(
                ". Update files retained at {}",
                stage.keep().display()
            ));
        }
        message
    }

    fn failure(&mut self, error: String) -> String {
        match self.rollback() {
            Ok(()) => match self.clear_recovery() {
                Ok(()) => error,
                Err(recovery) => self.retain_stages(format!(
                    "{error}. Could not clear the update recovery marker ({recovery})"
                )),
            },
            Err(rollback) => format!("{error}. {rollback}"),
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let result = self.rollback().and_then(|()| {
            self.clear_recovery().map_err(|error| {
                self.retain_stages(format!(
                    "Could not clear the update recovery marker ({error})"
                ))
            })
        });
        if let Err(error) = result {
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

/// Codex caches absolute package paths and uses them to load bundled tools
/// for the lifetime of a session. Swapping an npm package directory under
/// that session could send it a helper built for a newer Codex.
fn npm_running_in(roots: &[&Path]) -> Result<bool, String> {
    if roots.is_empty() {
        return Ok(false);
    }
    let mut processes = provider_processes()
        .map_err(|error| format!("Cannot check which Codex packages are in use: {error}"))?;
    let uid = crate::daemon::current_uid();
    Ok(processes
        .any(|process| npm_uses_packages(&process, fs::read_link(process.join("exe")), roots, uid)))
}

fn provider_processes() -> io::Result<impl Iterator<Item = PathBuf>> {
    Ok(fs::read_dir("/proc")?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .as_encoded_bytes()
                .iter()
                .all(u8::is_ascii_digit)
        })
        .map(|entry| entry.path()))
}

fn npm_uses_packages(
    process: &Path,
    executable: io::Result<PathBuf>,
    roots: &[&Path],
    uid: u32,
) -> bool {
    let uses_packages = match executable {
        Ok(executable) if roots.iter().any(|root| executable.starts_with(root)) => true,
        // A wrapper can be starting its Codex child while its own
        // executable still points to the interpreter.
        Ok(executable) if can_run_provider_script(&executable) => {
            command_references_packages(process, roots)
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // A non-dumpable process can deny access to exe. The task name
            // on its own only tells us about possible users of our package
            // if the process can share our uid.
            (fs::read(process.join("comm")).is_ok_and(|name| name.starts_with(b"codex"))
                || command_references_packages(process, roots))
                && process_may_share_uid(process, uid)
        }
        _ => false,
    };
    uses_packages && !version_only(process)
}

fn process_may_share_uid(process: &Path, uid: u32) -> bool {
    // A non-dumpable process's proc directory can be owned by root. Use
    // the kernel's real, effective, saved and filesystem UIDs instead.
    let Ok(status) = fs::read(process.join("status")) else {
        return true;
    };
    let Some(fields) = status
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))
        .and_then(|fields| std::str::from_utf8(fields).ok())
    else {
        return true;
    };
    let mut fields = fields.split_ascii_whitespace();
    let ids: [Option<u32>; 4] =
        std::array::from_fn(|_| fields.next().and_then(|id| id.parse().ok()));
    ids.contains(&Some(uid)) || ids.contains(&None) || fields.next().is_some()
}

fn can_run_provider_script(executable: &Path) -> bool {
    matches!(
        executable.file_name().and_then(OsStr::to_str),
        Some("node" | "nodejs" | "bun" | "sh" | "bash" | "dash")
    )
}

const MAX_CMDLINE: usize = 16 * 1024;

fn process_arguments(process: &Path) -> io::Result<Vec<u8>> {
    let mut arguments = Vec::new();
    fs::File::open(process.join("cmdline"))?
        .take((MAX_CMDLINE + 1) as u64)
        .read_to_end(&mut arguments)?;
    Ok(arguments)
}

fn version_only(process: &Path) -> bool {
    let Ok(arguments) = process_arguments(process) else {
        return false;
    };
    // Status probes only ask for the version. They can finish on the old
    // executable while the updater publishes the new one.
    let Some(arguments) = arguments
        .strip_suffix(b"\0")
        .filter(|arguments| arguments.len() < MAX_CMDLINE)
    else {
        return false;
    };
    let mut arguments = arguments.split(|byte| *byte == 0);
    let program = arguments.next().unwrap_or_default();
    let first = arguments.next();
    let version = if can_run_provider_script(Path::new(OsStr::from_bytes(program))) {
        if first.is_none_or(|script| script.is_empty() || script.starts_with(b"-")) {
            return false;
        }
        arguments.next()
    } else {
        first
    };
    version == Some(b"--version") && arguments.next().is_none()
}

fn command_references_packages(process: &Path, roots: &[&Path]) -> bool {
    let Ok(arguments) = process_arguments(process) else {
        return false;
    };
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
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, process::Stdio};

    const FAKE_AGY: &str = r#"#!/bin/sh
set -eu
case "$1" in
  --version) echo 'agy 1.0.0' ;;
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

    #[tokio::test]
    async fn isolated_installers_keep_the_temporary_directory_and_locale() {
        let expected = [
            ("TMPDIR", "/scratch/provider downloads"),
            ("LANG", "en_CA.UTF-8"),
            ("LANGUAGE", "fr_CA:fr"),
            ("LC_ALL", "C.UTF-8"),
            ("LC_CTYPE", "en_CA.UTF-8"),
            ("LC_TIME", "fr_CA.UTF-8"),
            ("HTTPS_PROXY", "https://example.test"),
        ];
        let environment = expected
            .into_iter()
            .chain([
                ("HOME", "/home/user"),
                ("CLAUDE_CONFIG_DIR", "/real/config"),
            ])
            .map(|(key, value)| (key.into(), value.into()));
        let output = run_update(isolated_installer(
            Path::new("/usr/bin/env"),
            &["HTTPS_PROXY"],
            environment,
        ))
        .await
        .unwrap();
        let actual = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect::<BTreeMap<_, _>>();
        assert_eq!(actual, BTreeMap::from(expected));
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
        let installation = SelectedInstall::capture(&status, Some(path)).unwrap();
        let installed = semver::Version::new(1, 0, 0);
        let Preparation::Ready(update) =
            stage_standalone(installation, &installed, false, &context)
                .await
                .unwrap()
        else {
            panic!("expected staged Antigravity update")
        };
        update
    }

    #[tokio::test]
    async fn current_standalone_checks_the_live_binary_without_staging_a_copy() {
        let root = tempfile::tempdir().unwrap();
        let omp = root.path().join("omp");
        write_executable(
            &omp,
            r#"#!/bin/sh
set -eu
test "$1" = update
test "$2" = --check
test "$0" = "${PATH%%:*}/omp"
for stage in "$(dirname "$0")"/.cagents-provider-update-*; do
    test ! -e "$stage"
done
printf 'Current version: 1.0.0\n✔ Already up to date\n'
"#,
        );
        let status = fixture_status(Provider::Omp, &omp, ProviderInstallMethod::Standalone);
        let path = fixture_path(root.path());
        let context = CommandContext {
            search_path: Some(&path),
        };
        assert!(matches!(
            prepare_standalone(&status, &context, true).await.unwrap(),
            Preparation::Current(version) if version == "1.0.0"
        ));
    }

    #[tokio::test]
    async fn standalone_update_defers_while_the_executable_is_running() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture_path(root.path());
        let context = CommandContext {
            search_path: Some(&path),
        };
        let installed = semver::Version::new(1, 0, 0);
        for provider in [Provider::Antigravity, Provider::Omp] {
            let live = root.path().join(provider.executable());
            // Use an ELF like the supported standalone installs. The process
            // can reach the live inode through a different executable name.
            fs::copy("/bin/cat", &live).unwrap();
            let alias = root.path().join(format!("{}-alias", provider.executable()));
            fs::hard_link(&live, &alias).unwrap();
            let old_inode = fs::metadata(&live).unwrap().ino();
            let status = fixture_status(provider, &live, ProviderInstallMethod::Standalone);
            let installation = SelectedInstall::capture(&status, Some(&path)).unwrap();
            let stage = stage_in(root.path()).unwrap();
            let next = stage.path().join(provider.executable());
            write_executable(&next, "#!/bin/sh\necho 2.0.0\n");
            let replacement = Replacement::new(next, live.clone()).unwrap();
            let mut update =
                PreparedUpdate::staged(installation, stage, vec![replacement], "2.0.0".into());

            let mut child = Command::new(&alias)
                .stdin(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let installation = SelectedInstall::capture(&status, Some(&path)).unwrap();
            assert!(matches!(
                stage_standalone(installation, &installed, false, &context)
                    .await
                    .unwrap(),
                Preparation::Deferred(_)
            ));
            assert!(matches!(
                update.apply(Some(&path)).await.unwrap(),
                Applied::Deferred(_)
            ));
            assert_eq!(fs::metadata(&live).unwrap().ino(), old_inode);

            drop(child.stdin.take());
            assert!(child.wait().await.unwrap().success());
            assert!(matches!(
                update.apply(Some(&path)).await.unwrap(),
                Applied::Updated(version) if version == "2.0.0"
            ));
            assert_ne!(fs::metadata(&live).unwrap().ino(), old_inode);
        }
    }

    #[tokio::test]
    async fn failed_live_verification_restores_standalone_binary() {
        let root = tempfile::tempdir().unwrap();
        let agy = root.path().join("agy");
        write_executable(&agy, FAKE_AGY);
        let path = fixture_path(root.path());
        let old_inode = fs::metadata(&agy).unwrap().ino();
        let mut update = stage_agy(&agy, &path).await;
        fs::write(root.path().join(".reject-update"), []).unwrap();

        assert!(update.apply(Some(&path)).await.is_err());
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
        let mut update = stage_agy(&agy, &path).await;
        let replacement = root.path().join("replacement");
        write_executable(&replacement, "#!/bin/sh\necho 'agy 3.0.0'\n");
        fs::rename(replacement, &agy).unwrap();

        assert!(update.apply(Some(&path)).await.is_err());
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
        let mut update =
            PreparedUpdate::staged(installation, stage, vec![replacement], "2.0.0".into());

        assert!(matches!(
            update.apply(Some(&path)).await.unwrap(),
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
    fn npm_ignores_an_unreadable_codex_from_another_user() {
        let process = tempfile::tempdir().unwrap();
        let process = process.path();
        fs::write(process.join("comm"), b"codex\n").unwrap();
        let using_package = || {
            npm_uses_packages(
                process,
                Err(io::ErrorKind::PermissionDenied.into()),
                &[Path::new("/test/node_modules/@openai/codex")],
                1000,
            )
        };

        fs::write(process.join("status"), b"Uid:\t1001\t1001\t1001\t1001\n").unwrap();
        assert!(!using_package());

        // A same-user, non-dumpable process can have a root-owned proc entry.
        // A matching saved UID still permits its use of the user install.
        fs::write(process.join("status"), b"Uid:\t1001\t1001\t1000\t1001\n").unwrap();
        assert!(using_package());
    }

    #[test]
    fn version_probes_do_not_hold_npm_packages_in_use() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("codex-package");
        let program = package.join("codex");
        let args = root.path().join("cmdline");
        let using_package = |executable: &Path| {
            npm_uses_packages(root.path(), Ok(executable.to_owned()), &[&package], 1000)
        };

        fs::write(
            &args,
            [program.as_os_str().as_bytes(), b"\0--version\0"].concat(),
        )
        .unwrap();
        assert!(!using_package(&program));
        let node = Path::new("/usr/bin/node");
        fs::write(
            &args,
            [
                b"/usr/bin/node\0",
                program.as_os_str().as_bytes(),
                b"\0--version\0",
            ]
            .concat(),
        )
        .unwrap();
        assert!(!using_package(node));
        fs::write(
            args,
            [
                b"/usr/bin/node\0",
                program.as_os_str().as_bytes(),
                b"\0exec\0--version\0",
            ]
            .concat(),
        )
        .unwrap();
        assert!(using_package(node));
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
            stages: vec![stage],
            replacements: vec![
                Replacement::new(other_saved, other_live.clone()).unwrap(),
                Replacement::new(saved.clone(), live.clone()).unwrap(),
            ],
            applied: 0,
            expected_version: "2.0.0".into(),
        };
        transaction.apply(Provider::Codex).unwrap();
        let external = root.path().join("external");
        fs::write(&external, "external").unwrap();
        fs::rename(external, &live).unwrap();

        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read_to_string(live).unwrap(), "external");
        assert_eq!(fs::read_to_string(saved).unwrap(), "old");
        assert_eq!(fs::read_to_string(other_live).unwrap(), "old platform");
    }

    #[test]
    fn recovery_follows_a_dangling_npm_launcher_before_a_path_fallback() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let fallback = root.path().join("fallback");
        let scope = root.path().join(npm::SCOPE_DIRECTORY);
        let wrapper = scope.join("codex");
        let script = wrapper.join("bin/codex.js");
        for directory in [&bin, &fallback, script.parent().unwrap()] {
            fs::create_dir_all(directory).unwrap();
        }
        let launcher = bin.join("codex");
        let fallback_launcher = fallback.join("codex");
        write_executable(&script, "#!/bin/sh\necho 'codex-cli 1.0.0'\n");
        write_executable(&fallback_launcher, "#!/bin/sh\necho 'codex-cli 2.0.0'\n");
        std::os::unix::fs::symlink(&script, &launcher).unwrap();

        let stage = stage_in(&scope).unwrap();
        let staged_wrapper = stage.path().join("codex");
        fs::create_dir(&staged_wrapper).unwrap();
        let replacement = Replacement::new(staged_wrapper.clone(), wrapper.clone()).unwrap();
        staging::begin_apply(
            std::slice::from_ref(&stage),
            Provider::Codex,
            &[replacement],
        )
        .unwrap();
        rename(&staged_wrapper, &wrapper, libc::RENAME_EXCHANGE).unwrap();
        let stage = stage.keep();
        let path = std::env::join_paths([bin, fallback]).unwrap();
        let status = ProviderStatus::detect(Provider::Codex, Some(&path));
        assert_eq!(status.executable, Some(fallback_launcher));
        assert!(
            recovery_issue(&status, Some(&path))
                .unwrap()
                .contains(&*stage.to_string_lossy())
        );

        recover(&status, Some(&path)).unwrap();
        let status = ProviderStatus::detect(Provider::Codex, Some(&path));
        assert_eq!(status.executable, Some(launcher));
        assert!(recovery_issue(&status, Some(&path)).is_none());
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
                prepare(&status, Some(&path), true).await.unwrap(),
                Preparation::Skipped(_)
            ));
        }
    }
}
