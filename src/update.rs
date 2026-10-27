//! Explicit, self-managed release updates. `gh` owns authentication; discovery
//! never writes installation state. The detached worker alone may run an installer.
mod candidate;
mod confirmation;
mod installed;
mod source;
pub use confirmation::{Choice as UpdateChoice, Confirmation as UpdateConfirmation, decide};
pub mod impact;
#[cfg(feature = "ui")]
pub mod startup;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const REPOSITORY: &str = "CarsonBurke/cagents";
const METADATA_LIMIT: u64 = 4 * 1024 * 1024;
const SMALL_ASSET_LIMIT: u64 = 1024 * 1024;
const JOB_STATE_LIMIT: u64 = crate::terminal_ipc::MAX_FRAME_BYTES as u64;
const ARCHIVE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const WORKER_LOCK_FD: i32 = 198;

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("{0}")]
    Invalid(String),
    #[error("another update is already running")]
    Busy,
    #[error("update check or download cancelled")]
    Cancelled,
    #[error("{0} timed out")]
    Timeout(&'static str),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("invalid release metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{operation}; streaming command cleanup is unconfirmed: {cleanup}")]
    Cleanup {
        #[source]
        operation: Box<UpdateError>,
        cleanup: io::Error,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    id: u64,
    archive: Asset,
    checksums: Asset,
    installer: Asset,
}

impl Release {
    pub fn notice_identity(&self) -> String {
        format!("release:{}:{}", self.id, self.version)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Asset {
    id: u64,
    name: String,
    size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdateState {
    Unsupported { reason: String },
    Current { version: String },
    Available { release: Release },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UpdatePhase {
    Starting,
    Downloading,
    Verifying,
    AwaitingConfirmation,
    WaitingForIdle,
    ClosingWindows,
    Installing,
    Complete,
    Failed,
    Cancelled,
}

impl UpdatePhase {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateJob {
    pub id: String,
    pub version: String,
    pub phase: UpdatePhase,
    pub message: String,
    pub log_path: PathBuf,
    #[serde(default)]
    pub confirmation: Option<confirmation::Confirmation>,
}

impl UpdateJob {
    pub fn active_confirmation(&self) -> Option<&UpdateConfirmation> {
        matches!(
            self.phase,
            UpdatePhase::AwaitingConfirmation | UpdatePhase::WaitingForIdle
        )
        .then_some(self.confirmation.as_ref())
        .flatten()
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

/// One version directory inside an installation tree, established from a path
/// alone: `<prefix>/versions/<version>/bin/<executable>` under a privately
/// owned root that carries the installer's marker.
///
/// This deliberately says nothing about which version the installation is
/// currently serving. A release update needs the running tree to be the served
/// one ([`Installation`]); noticing that the served tree has *changed*
/// ([`superseding_ui`]) needs exactly the opposite.
pub(crate) struct InstallLayout {
    pub(crate) prefix: PathBuf,
    pub(crate) version_root: PathBuf,
    version: String,
}

pub(crate) fn locate(executable: &Path) -> Result<InstallLayout, UpdateError> {
    let executable = executable.canonicalize()?;
    let bin = executable.parent().ok_or_else(unsupported_install)?;
    if bin.file_name().is_none_or(|name| name != "bin") {
        return Err(unsupported_install());
    }
    let version_root = bin.parent().ok_or_else(unsupported_install)?;
    let version = version_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(unsupported_install)?;
    let versions = version_root.parent().ok_or_else(unsupported_install)?;
    if versions.file_name().is_none_or(|name| name != "versions") {
        return Err(unsupported_install());
    }
    let prefix = versions.parent().ok_or_else(unsupported_install)?;
    let marker = prefix.join(".cagents-install-root");
    let meta = fs::symlink_metadata(&marker).map_err(|_| unsupported_install())?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 64
        || fs::read_to_string(marker)?.trim_end_matches('\n') != "cagents install root"
        || !matched_set(bin)
    {
        return Err(unsupported_install());
    }
    let prefix_meta = fs::metadata(prefix)?;
    if prefix_meta.uid() != unsafe { libc::geteuid() } || prefix_meta.mode() & 0o022 != 0 {
        return Err(invalid("installation root is not privately owned"));
    }
    Ok(InstallLayout {
        prefix: prefix.to_owned(),
        version_root: version_root.to_owned(),
        version: version.to_owned(),
    })
}

/// The three executables are released, installed and spoken as one set: a
/// version directory holding only some of them cannot host a window.
fn matched_set(bin: &Path) -> bool {
    ["cagents", "cagents-ui", "cagentsd"]
        .iter()
        .all(|name| bin.join(name).is_file())
}

struct Installation {
    prefix: PathBuf,
    version: String,
}
impl Installation {
    fn detect() -> Result<Self, UpdateError> {
        if !cfg!(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "gnu"
        )) {
            return Err(invalid(
                "self-update supports only Linux x86_64 GNU release installs",
            ));
        }
        if !cfg!(feature = "full") {
            return Err(invalid(
                "browserless builds must be updated with their package manager or installer",
            ));
        }
        Self::from_executable(&std::env::current_exe()?)
    }

    fn from_executable(executable: &Path) -> Result<Self, UpdateError> {
        let layout = locate(executable)?;
        installed_version(&layout.version)?;
        if layout.prefix.join("current").canonicalize()? != layout.version_root {
            return Err(unsupported_install());
        }
        // Source builds copied into an old release tree must not impersonate it.
        if layout.version != crate::VERSION {
            return Err(invalid(
                "installed directory and running binary versions differ; reinstall manually",
            ));
        }
        Ok(Self {
            prefix: layout.prefix,
            version: layout.version,
        })
    }

    fn state_dir(&self) -> Result<PathBuf, UpdateError> {
        let path = self.prefix.join(".update");
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(invalid("unsafe updater state directory"));
        }
        Ok(path)
    }
}

fn invalid(message: impl Into<String>) -> UpdateError {
    UpdateError::Invalid(message.into())
}
fn unsupported_install() -> UpdateError {
    invalid(
        "self-update requires a full user installation made by install.sh; source and OS-package installs must use their original installation method",
    )
}
fn stable_version(text: &str) -> Result<Version, UpdateError> {
    let version = Version::parse(text).map_err(|_| invalid("invalid release version"))?;
    if !version.pre.is_empty() || !version.build.is_empty() || version.to_string() != text {
        return Err(invalid(
            "only canonical stable release versions are supported",
        ));
    }
    Ok(version)
}

/// Build metadata identifies a local package but has no release precedence.
/// Keep release tags themselves under `stable_version`'s stricter contract.
fn installed_version(text: &str) -> Result<Version, UpdateError> {
    let mut version = Version::parse(text).map_err(|_| invalid("invalid installed version"))?;
    if !version.pre.is_empty() || version.to_string() != text {
        return Err(invalid(
            "only canonical release-based installations are supported",
        ));
    }
    version.build = semver::BuildMetadata::EMPTY;
    Ok(version)
}

/// Blocking, bounded network discovery. Run on an async runtime's blocking pool.
/// No installation or updater files are created by this call.
pub fn check(cancellation: &CancellationToken) -> Result<UpdateState, UpdateError> {
    let installation = match Installation::detect() {
        Ok(installation) => installation,
        Err(error) => {
            return Ok(UpdateState::Unsupported {
                reason: error.to_string(),
            });
        }
    };
    let mut releases = Vec::new();
    for page in 1..=5 {
        let endpoint = format!("repos/{REPOSITORY}/releases?per_page=100&page={page}");
        let page_releases: Vec<GitHubRelease> =
            serde_json::from_slice(&metadata(&endpoint, cancellation)?)?;
        let complete = page_releases.len() < 100;
        releases.extend(page_releases);
        if complete {
            return select_release(releases, &installation.version);
        }
    }
    Err(invalid(
        "release discovery exceeded its 500-release safety limit",
    ))
}

fn select_release(releases: Vec<GitHubRelease>, current: &str) -> Result<UpdateState, UpdateError> {
    let current_version = installed_version(current)?;
    let newest = releases
        .into_iter()
        .filter_map(|release| {
            if release.draft || release.prerelease {
                return None;
            }
            let version = stable_version(release.tag_name.strip_prefix('v')?).ok()?;
            Some((version, release))
        })
        .max_by(|left, right| left.0.cmp(&right.0));
    match newest {
        Some((version, release)) if version > current_version => Ok(UpdateState::Available {
            release: applicable_release(release)?,
        }),
        _ => Ok(UpdateState::Current {
            version: current.to_owned(),
        }),
    }
}

fn applicable_release(release: GitHubRelease) -> Result<Release, UpdateError> {
    if release.id == 0 || release.draft || release.prerelease {
        return Err(invalid("release is not a published stable release"));
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .ok_or_else(|| invalid("release must use a v-prefixed version tag"))?;
    stable_version(version)?;
    let archive_name = format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz");
    let asset = |name: &str, limit: u64| -> Result<Asset, UpdateError> {
        let mut matching = release.assets.iter().filter(|asset| asset.name == name);
        let asset = matching
            .next()
            .ok_or_else(|| invalid(format!("release lacks required asset {name}")))?;
        if matching.next().is_some() || asset.id == 0 || asset.size == 0 || asset.size > limit {
            return Err(invalid(format!(
                "invalid or duplicate release asset {name}"
            )));
        }
        Ok(asset.clone())
    };
    let archive = asset(&archive_name, ARCHIVE_LIMIT)?;
    let checksums = asset("SHA256SUMS", SMALL_ASSET_LIMIT)?;
    let installer = asset("install.sh", SMALL_ASSET_LIMIT)?;
    if archive.id == checksums.id || archive.id == installer.id || checksums.id == installer.id {
        return Err(invalid("release asset IDs are not distinct"));
    }
    Ok(Release {
        version: version.to_owned(),
        url: format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
        id: release.id,
        archive,
        checksums,
        installer,
    })
}

/// Explicit user authorization: this starts a detached worker, not an in-process
/// task. Its inherited lock closes the double-click / concurrent-window race.
#[derive(Serialize, Deserialize)]
enum UpdateRequest {
    Release(Release),
    Installed { root: PathBuf },
}

pub fn apply(release: &Release) -> Result<UpdateJob, UpdateError> {
    let installation = Installation::detect()?;
    if stable_version(&release.version)? <= installed_version(&installation.version)? {
        return Err(invalid(
            "selected release is not newer than the running installation",
        ));
    }
    start_job(
        &installation,
        &release.version,
        &UpdateRequest::Release(release.clone()),
    )
}

pub fn apply_installed(executable: &Path) -> Result<UpdateJob, UpdateError> {
    let mut installation = running_installation()?;
    let layout = locate(&std::env::current_exe()?)?;
    let served = served_version_root(&layout)?;
    if executable.canonicalize()? != served.join("bin/cagents-ui").canonicalize()? {
        return Err(invalid(
            "the installed candidate changed; inspect the installation again",
        ));
    }
    // A newly opened UI may already be current while an older daemon still
    // owns the sessions. Ordinary skew uses that owner's CLI; an authenticated
    // supported source also permits bootstrap from the staged coordinator.
    installation.version = installed_entry_worker(&layout, &served)?;
    let version = served
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(unsupported_install)?
        .to_owned();
    start_job(
        &installation,
        &version,
        &UpdateRequest::Installed { root: served },
    )
}

fn installed_entry_worker(window: &InstallLayout, served: &Path) -> Result<String, UpdateError> {
    let source = source::inspect_installed_identity()?;
    let owner = installed_daemon_owner(window)?;
    // A header cannot choose a coordinator. Keep the original socket peer's
    // pidfd/proof through this exact kernel-owned installation recheck.
    let executable = owner
        .as_ref()
        .map(|owner| owner.version_root.join("bin/cagentsd"));
    source.verify_owner(executable.as_deref())?;
    if source.requires_matched_owner_worker()
        && owner
            .as_ref()
            .is_none_or(|owner| owner.version_root == served)
    {
        // The UI-only fallback uses the old window's CLI. It cannot speak an
        // unqualified service already owned by the served candidate, so do
        // not infer that workflow from an unknown version header.
        return Err(invalid(
            "The installed source requires its own matched application to finish this update",
        ));
    }
    installed_update_worker(
        window,
        served,
        owner.as_ref(),
        source.protocol,
        source.proof.as_ref(),
    )
}

/// This entry decision consumes an authenticated source observation. A failed
/// ordinary-client decode is neither proof of compatibility nor permission to
/// route around a source owner. Header discovery alone never reaches here.
fn installed_update_worker(
    window: &InstallLayout,
    served: &Path,
    daemon: Option<&InstallLayout>,
    source_protocol: u16,
    source_proof: Option<&crate::model::HostSessionProof>,
) -> Result<String, UpdateError> {
    let replaces_daemon = source_proof.is_some()
        && (source_protocol != crate::terminal_ipc::PROTOCOL_VERSION
            || daemon.is_some_and(|owner| owner.version_root != served));
    if served == window.version_root && !replaces_daemon {
        return Err(invalid("the installed application is already running"));
    }
    let legacy_source = source_proof.is_some()
        && matches!(source_protocol, 77 | 119)
        && crate::terminal_ipc::PROTOCOL_VERSION > 77;
    // The staged CLI can speak an older build of its exact current protocol.
    // Its source was authenticated above; a version-root difference is not a
    // reason to delegate the update back to that older build's updater.
    let candidate_source = legacy_source
        || (window.version_root == served
            && candidate_can_upgrade_source(served, daemon, source_protocol, source_proof));
    let worker = installation_worker_version(window, served, daemon, candidate_source);
    if !candidate_source && served == window.prefix.join("versions").join(&worker) {
        return Err(invalid("the installed application is already running"));
    }
    Ok(worker)
}

fn candidate_can_upgrade_source(
    served: &Path,
    daemon: Option<&InstallLayout>,
    protocol: u16,
    proof: Option<&crate::model::HostSessionProof>,
) -> bool {
    proof.is_some_and(crate::model::HostSessionProof::is_complete)
        && ((matches!(protocol, 77 | 119) && crate::terminal_ipc::PROTOCOL_VERSION > 77)
            || (protocol == crate::terminal_ipc::PROTOCOL_VERSION
                && daemon.is_some_and(|owner| owner.version_root != served)))
}

fn installation_worker_version(
    window: &InstallLayout,
    served: &Path,
    daemon: Option<&InstallLayout>,
    use_window_worker: bool,
) -> String {
    if use_window_worker {
        return window.version.clone();
    }
    // Once the candidate already owns the daemon, the old window's worker
    // must perform the remaining UI-only replacement. A current worker would
    // correctly reject replacing its own installation with itself.
    daemon
        .filter(|owner| owner.version_root != served)
        .unwrap_or(window)
        .version
        .clone()
}

fn installed_daemon_owner(layout: &InstallLayout) -> Result<Option<InstallLayout>, UpdateError> {
    let Some(executable) =
        crate::daemon::running_executable().map_err(|error| invalid(error.to_string()))?
    else {
        return Ok(None);
    };
    let owner = locate(&executable)?;
    if owner.prefix != layout.prefix || executable.file_name().is_none_or(|name| name != "cagentsd")
    {
        return Err(invalid(
            "The running terminal service belongs to another installation; finish its update from that installation.",
        ));
    }
    Ok(Some(owner))
}

/// CLI entry point for the same leased worker, exact preview and window fence
/// used by the application. The running daemon's matched CLI owns the worker.
pub fn upgrade_installed(yes: bool, interrupt: bool) -> Result<UpdateJob, UpdateError> {
    use std::io::IsTerminal;
    if !yes && !io::stdin().is_terminal() {
        return Err(invalid(
            "Review the update in the app, or pass --yes to approve its displayed impact; --interrupt additionally permits active native turns and their background subagents to stop.",
        ));
    }
    let layout = locate(&std::env::current_exe()?)?;
    let candidate = served_version_root(&layout)?.join("bin/cagents-ui");
    let initial = apply_installed(&candidate)?;
    let policy = if interrupt {
        impact::Policy::Interrupt
    } else {
        impact::Policy::WhenIdle
    };
    let mut submitted = None;
    let mut policy_requested = None;
    let deadline = Instant::now() + Duration::from_secs(45 * 60);
    loop {
        let job = status()?
            .filter(|job| job.id == initial.id)
            .ok_or_else(|| {
                invalid("The update job changed; inspect the current update in the app.")
            })?;
        if job.phase.is_terminal() {
            return if job.phase == UpdatePhase::Complete {
                Ok(job)
            } else {
                Err(invalid(job.message))
            };
        }
        if let Some(confirmation) = job.active_confirmation() {
            if confirmation
                .preview
                .daemon
                .as_ref()
                .is_some_and(|daemon| daemon.policy != policy)
            {
                if policy_requested.as_ref() != Some(&confirmation.nonce) {
                    decide(&job.id, &confirmation.nonce, UpdateChoice::Policy(policy))?;
                    policy_requested = Some(confirmation.nonce.clone());
                }
            } else if submitted.as_ref() != Some(&confirmation.nonce) {
                if submitted.is_some() {
                    decide(&job.id, &confirmation.nonce, UpdateChoice::Cancel)?;
                    return Err(invalid(
                        "Update impact changed after approval; review and retry the update.",
                    ));
                }
                eprintln!(
                    "Update to {}{}",
                    job.version,
                    if confirmation.preview.replaces_daemon {
                        " (replaces terminal service)"
                    } else {
                        " (keeps terminal service)"
                    }
                );
                if let Some(daemon) = &confirmation.preview.daemon {
                    for session in &daemon.sessions {
                        eprintln!(
                            "  {} ({}): {}",
                            session.title,
                            session.disposition.label(),
                            session.reason
                        );
                    }
                    if daemon.blocked() {
                        for blocker in &daemon.blockers {
                            eprintln!("  {blocker}");
                        }
                        decide(&job.id, &confirmation.nonce, UpdateChoice::Cancel)?;
                        return Err(invalid(
                            "Resolve the displayed update blockers, then retry.",
                        ));
                    }
                }
                if !yes {
                    eprint!("Proceed with this update? [y/N] ");
                    io::stderr().flush()?;
                    let mut answer = String::new();
                    io::stdin().read_line(&mut answer)?;
                    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        decide(&job.id, &confirmation.nonce, UpdateChoice::Cancel)?;
                        return Err(UpdateError::Cancelled);
                    }
                }
                decide(&job.id, &confirmation.nonce, UpdateChoice::Confirm)?;
                submitted = Some(confirmation.nonce.clone());
            }
        }
        if Instant::now() >= deadline {
            return Err(invalid(
                "The update is still running; inspect its progress in the app.",
            ));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn running_installation() -> Result<Installation, UpdateError> {
    let layout = locate(&std::env::current_exe()?)?;
    if layout.version != crate::VERSION {
        return Err(unsupported_install());
    }
    Ok(Installation {
        prefix: layout.prefix,
        version: layout.version,
    })
}

fn start_job(
    installation: &Installation,
    version: &str,
    request: &UpdateRequest,
) -> Result<UpdateJob, UpdateError> {
    let state = installation.state_dir()?;
    let lock = acquire_lock(&state)?;
    let id = uuid::Uuid::new_v4().to_string();
    let job = UpdateJob {
        id: id.clone(),
        version: version.into(),
        phase: UpdatePhase::Starting,
        message: "Starting verified update".into(),
        log_path: state.join(format!("{id}.log")),
        confirmation: None,
    };
    let log = private_create(&job.log_path)?;
    write_job(&state, &job)?;
    let request_file = private_create(&state.join(format!("{id}.request")))?;
    serde_json::to_writer(&request_file, request)?;
    request_file.sync_all()?;
    let mut command = Command::new(
        installation
            .prefix
            .join("versions")
            .join(&installation.version)
            .join("bin/cagents"),
    );
    command
        .arg("update-worker")
        .arg(&id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    let fd = lock.as_raw_fd();
    // SAFETY: only async-signal-safe syscalls run after fork. The duplicate keeps
    // the same flock alive until the worker exits, even if every UI disappears.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(fd, WORKER_LOCK_FD) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(WORKER_LOCK_FD, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match command.spawn() {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(job)
        }
        Err(error) => {
            let mut failed = job;
            failed.phase = UpdatePhase::Failed;
            failed.message = format!("Could not start update helper: {error}");
            write_job(&state, &failed)?;
            Err(error.into())
        }
    }
}

/// Returns the latest operation, including retained failure diagnostics.
pub fn status() -> Result<Option<UpdateJob>, UpdateError> {
    let executable = std::env::current_exe()?;
    if executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::file_name)
        != Some(std::ffi::OsStr::new("versions"))
    {
        return Ok(None);
    }
    let installation = running_installation()?;
    let state = installation.prefix.join(".update");
    if !state.exists() {
        return Ok(None);
    }
    let state = installation.state_dir()?;
    let mut job: UpdateJob = match read_private(&state.join("status.json"), JOB_STATE_LIMIT) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !job.phase.is_terminal() {
        match acquire_lock(&state) {
            Ok(_lock) => {
                // A worker may have completed (or a later job may have run)
                // between the initial snapshot and acquiring this lease.
                job = serde_json::from_slice(&read_private(
                    &state.join("status.json"),
                    JOB_STATE_LIMIT,
                )?)?;
                if !job.phase.is_terminal() {
                    job.phase = UpdatePhase::Failed;
                    job.confirmation = None;
                    job.message =
                        "Update helper exited before completion; see the private update log".into();
                    write_job(&state, &job)?;
                }
            }
            Err(UpdateError::Busy) => (),
            Err(error) => return Err(error),
        }
    }
    Ok(Some(job))
}

/// Keep historical diagnostics available to the CLI without showing a resolved
/// installation failure as a warning in every subsequently installed window.
#[cfg(feature = "ui")]
pub(crate) fn status_for_ui() -> Result<Option<UpdateJob>, UpdateError> {
    let Some(job) = status()? else {
        return Ok(None);
    };
    // Failed local builds cannot be ordered by their metadata. A later switch
    // of `current`, followed by a healthy matched application set, demonstrates
    // supersession; a rollback before the failure was recorded does not.
    let superseded = (|| -> Result<bool, UpdateError> {
        if job.phase != UpdatePhase::Failed {
            return Ok(false);
        }
        let window = locate(&std::env::current_exe()?)?;
        let failed = fs::symlink_metadata(window.prefix.join(".update/status.json"))?;
        let current = fs::symlink_metadata(window.prefix.join("current"))?;
        let switched_after_failure = current.file_type().is_symlink()
            && (current.ctime(), current.ctime_nsec()) > (failed.ctime(), failed.ctime_nsec());
        let served = served_version_root(&window)?;
        let daemon = installed_daemon_owner(&window)?;
        if !failed_job_is_superseded(
            &job,
            &window,
            &served,
            daemon.as_ref(),
            switched_after_failure,
        ) {
            return Ok(false);
        }
        let socket = crate::terminal_ipc::daemon_socket_path()?;
        let mut client = crate::control::ControlClient::connect(&socket)
            .map_err(|error| invalid(error.to_string()))?;
        Ok(client
            .daemon_version()
            .map_err(|error| invalid(error.to_string()))?
            == window.version)
    })()
    // Missing evidence or an incompatible/starting daemon keeps the failure
    // visible. Reconciliation must never conceal an unresolved replacement.
    .unwrap_or(false);
    Ok((!superseded).then_some(job))
}

#[cfg(any(feature = "ui", test))]
fn failed_job_is_superseded(
    job: &UpdateJob,
    window: &InstallLayout,
    served: &Path,
    daemon: Option<&InstallLayout>,
    switched_after_failure: bool,
) -> bool {
    job.phase == UpdatePhase::Failed
        && switched_after_failure
        && job.version != window.version
        && window.version_root == served
        && daemon.is_some_and(|owner| {
            owner.prefix == window.prefix && owner.version_root == window.version_root
        })
        && installed_version(&job.version)
            .ok()
            .zip(installed_version(&window.version).ok())
            .is_some_and(|(target, running)| target <= running)
}

/// The installed UI executable, when the installation this window runs from has
/// since switched `current` to another version.
///
/// An install performed under a running window - `install.sh`, a package
/// upgrade, or this application's own updater working on behalf of a window
/// that has since been replaced - leaves that window speaking the previous
/// wire protocol to a daemon that already speaks the new one. Every terminal it
/// owns then fails its handshake, and no amount of retrying can succeed,
/// because the mismatch is between two executables rather than two processes.
///
/// Unlike [`Installation`] this accepts any version string, including the
/// `+local` builds a developer installs, since nothing here is downloaded: the
/// bytes are already installed under a privately owned root.
pub fn superseding_ui() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    if let Some(replacement) = superseding_ui_from(&executable) {
        return Some(replacement);
    }
    let layout = locate(&executable).ok()?;
    if !daemon_protocol_mismatch() {
        return None;
    }
    let owner = installed_daemon_owner(&layout).ok()??;
    (owner.version_root != layout.version_root).then(|| layout.version_root.join("bin/cagents-ui"))
}

/// Whether this process runs from an installation tree: the only place an
/// install can leave it behind, so the only process [`superseding_ui`] can
/// ever find a replacement for. A source build never is one.
pub fn runs_from_installation() -> bool {
    std::env::current_exe().is_ok_and(|executable| locate(&executable).is_ok())
}

fn daemon_protocol_mismatch() -> bool {
    crate::terminal_ipc::daemon_socket_path()
        .ok()
        .is_some_and(|socket| {
            matches!(
                crate::control::ControlClient::connect(&socket),
                Err(crate::control::ControlError::UnsupportedVersion { .. })
            )
        })
}

fn superseding_ui_from(executable: &Path) -> Option<PathBuf> {
    let layout = locate(executable).ok()?;
    let served = served_version_root(&layout).ok()?;
    (served != layout.version_root).then(|| served.join("bin/cagents-ui"))
}

/// The version directory this installation currently serves.
///
/// The served tree must be another version of this same installation and as
/// complete as the one running: closing every window in favour of a tree that
/// cannot open one is far worse than staying stale. `install.sh` only ever
/// renames `current` over a finished version directory, so an incomplete
/// candidate means something else wrote the link.
fn served_version_root(layout: &InstallLayout) -> Result<PathBuf, UpdateError> {
    let served = layout.prefix.join("current").canonicalize()?;
    let served_layout = locate(&served.join("bin/cagents-ui"))?;
    if served_layout.prefix != layout.prefix || served_layout.version_root != served {
        return Err(invalid(
            "the installation does not serve a complete version of itself",
        ));
    }
    Ok(served)
}

/// Hands the relaunch to a detached helper from the installed version.
///
/// The window cannot orchestrate its own replacement: closing the windows waits
/// for every one of them to exit, and this process is one of them. The helper
/// outlives it, and comes from the installed tree so that the windows it
/// reopens and the daemon they attach to are one matched set.
pub fn restart_into_installed(installed_ui: &Path) -> Result<(), UpdateError> {
    let mut command = Command::new(crate::companion::Product::Cli.beside(installed_ui)?);
    command
        .args(["restart-windows", "--instance", crate::instance::NAME])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and the only call after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Hidden CLI entrypoint for [`restart_into_installed`].
///
/// Saves and closes every running window, then reopens each one from the served
/// tree. It holds the same launch fence the updater uses, so a release update in
/// flight and this relaunch can never interleave, and a window with unsaved
/// editor work vetoes the close and stays open.
pub fn restart_windows(instance: &str) -> Result<(), UpdateError> {
    // The window that asked and this helper have to agree about which instance
    // they belong to: a development build sitting in a version directory would
    // otherwise have the production helper close windows that are not its own,
    // while its own window stayed open and asked again.
    if instance != crate::instance::NAME {
        return Err(invalid(format!(
            "a {instance} window cannot be reopened by the {} installation",
            crate::instance::NAME
        )));
    }
    let layout = locate(&std::env::current_exe()?)?;
    if served_version_root(&layout)? != layout.version_root {
        return Err(invalid(
            "only the installed version may reopen saved windows",
        ));
    }
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    let windows = guard
        .prepare_and_close_windows_with_preflight(Duration::from_secs(45), |guard| {
            crate::install_guard::preflight_source_panes(guard, None, false)
        })?;
    // Saving and closing takes as long as the windows need, and another install
    // can land inside it. Reopen whatever is served now, so this relaunch does
    // not hand the reader a window that is already stale again.
    let served = served_version_root(&layout)?;
    guard
        .relaunch_windows(&windows, &served.join("bin/cagents-ui"))
        .map_err(UpdateError::from)
}

/// Hidden CLI entrypoint. Only a worker spawned by `apply` has the inherited
/// lease. Re-fetches the selected release from the fixed repository before use.
pub fn run_worker(job_id: &str) -> Result<(), UpdateError> {
    if uuid::Uuid::parse_str(job_id)
        .ok()
        .is_none_or(|id| id.to_string() != job_id)
    {
        return Err(invalid("invalid update job identifier"));
    }
    let installation = running_installation()?;
    let state = installation.state_dir()?;
    let _lock = inherited_lock(&state)?;
    let mut job: UpdateJob =
        serde_json::from_slice(&read_private(&state.join("status.json"), JOB_STATE_LIMIT)?)?;
    if job.id != job_id || job.phase != UpdatePhase::Starting {
        return Err(invalid("update job is no longer pending"));
    }
    let result = perform_update(&installation, &state, &mut job);
    job.phase = if matches!(result, Err(UpdateError::Cancelled)) {
        UpdatePhase::Cancelled
    } else if result.is_ok() {
        UpdatePhase::Complete
    } else {
        UpdatePhase::Failed
    };
    job.confirmation = None;
    job.message = match &result {
        Ok(()) => "Update installed; saved windows reopened".into(),
        Err(error) => error.to_string(),
    };
    write_job(&state, &job)?;
    result
}

fn perform_update(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
) -> Result<(), UpdateError> {
    let request: UpdateRequest = serde_json::from_slice(&read_private(
        &state.join(format!("{}.request", job.id)),
        SMALL_ASSET_LIMIT,
    )?)?;
    let selected = match request {
        UpdateRequest::Release(release) => {
            Installation::detect()?;
            release
        }
        UpdateRequest::Installed { root } => {
            return installed::perform(installation, state, job, &root);
        }
    };
    let cancellation = CancellationToken::new();
    let endpoint = format!("repos/{REPOSITORY}/releases/{}", selected.id);
    let release = applicable_release(serde_json::from_slice(&metadata(
        &endpoint,
        &cancellation,
    )?)?)?;
    if release != selected
        || stable_version(&release.version)? <= installed_version(&installation.version)?
    {
        return Err(invalid(
            "selected release changed or is no longer an upgrade; check again",
        ));
    }
    set_phase(
        state,
        job,
        UpdatePhase::Downloading,
        "Downloading release assets through GitHub authentication",
    )?;
    let mut checksums = download(&release.checksums, SMALL_ASSET_LIMIT, &cancellation)?;
    let mut manifest = String::new();
    checksums.read_to_string(&mut manifest)?;
    let archive_hash = checksum_for(&manifest, &release.archive.name)?;
    let installer_hash = checksum_for(&manifest, &release.installer.name)?;
    let mut installer = download(&release.installer, SMALL_ASSET_LIMIT, &cancellation)?;
    verify_digest(&mut installer, &installer_hash)?;
    let mut archive = download(&release.archive, ARCHIVE_LIMIT, &cancellation)?;
    set_phase(
        state,
        job,
        UpdatePhase::Verifying,
        "Verifying sealed release bytes",
    )?;
    verify_digest(&mut archive, &archive_hash)?;
    let staged = candidate::Staged::from_archive(&release, &archive, &archive_hash, &cancellation)?;
    let (source_protocol, _) = source::inspect_identity(None)?;
    if staged.identity.protocol == source_protocol {
        staged.verify_compatible_handshake(&cancellation)?;
    }
    reconcile_saved_sources(staged.identity.protocol)?;
    let confirmed = confirmation::wait(state, job, |policy| {
        Installation::detect()?;
        observe_impact(&staged.identity, policy)
    })?;
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing all application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    crate::install_guard::preflight_editor_companions(Some(staged.identity.protocol))?;
    let windows = guard.prepare_and_close_windows_with_preflight(Duration::from_secs(45), |guard| {
        crate::install_guard::preflight_source_panes(guard,
            confirmed.daemon.as_ref().map(|daemon| daemon.capability.proof.clone()), confirmed.replaces_daemon)?;
        let latest = observe_impact(&staged.identity, confirmed.daemon.as_ref().map_or(impact::Policy::WhenIdle, |daemon| daemon.policy))
            .map_err(io::Error::other)?;
        if !confirmed.preserves_consent(&latest) {
            return Err(io::Error::other("Session ownership changed during pane inspection; existing windows were kept open"));
        }
        Ok(())
    })?;
    let install_result = (|| {
        Installation::detect()?;
        let current = observe_impact(
            &staged.identity,
            confirmed
                .daemon
                .as_ref()
                .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy),
        )?;
        if !confirmed.preserves_consent(&current) {
            return Err(invalid(
                "Session ownership changed while saving windows; reopen and review the update again",
            ));
        }
        if let Some(daemon) = current.daemon {
            let socket = crate::terminal_ipc::daemon_socket_path()?;
            let mut client =
                source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
            client
                .prepare_confirmed_upgrade(daemon, false)
                .map_err(|error| invalid(error.to_string()))?;
            wait_daemon_stopped(&socket)?;
        }
        set_phase(
            state,
            job,
            UpdatePhase::Installing,
            "Checkpointing sessions and switching the installation",
        )?;
        run_installer(
            installation,
            &release,
            &installer,
            &archive,
            &checksums,
            current.replaces_daemon,
            &guard,
        )
    })();
    let reopen_result = guard
        .relaunch_windows(
            &windows,
            &installation.prefix.join("current/bin/cagents-ui"),
        )
        .map_err(UpdateError::from);
    install_result?;
    reopen_result
}

/// Invoke only the matched host implementation: SQLite stays out of the CLI
/// graph, and an older helper must refuse its unknown option before admission.
pub(crate) fn inspect_source_panes(
    guard: &crate::instance::UiUpdateGuard,
    proof: Option<crate::model::HostSessionProof>,
    require_resumable: bool,
) -> Result<(), UpdateError> {
    inspect_source_panes_mode(guard, proof, require_resumable, false)
}

// Recover only already-owned providers before publishing the inventory the
// user will consent to. Never expand consent after windows have prepared.
fn reconcile_saved_sources(destination_protocol: u16) -> Result<(), UpdateError> {
    let guard = crate::instance::UiUpdateGuard::acquire()?;
    let (protocol, proof) = source::inspect_identity(None)?;
    let mut problems = Vec::new();
    if let Err(error) =
        crate::install_guard::preflight_editor_companions(Some(destination_protocol))
    {
        problems.push(error.to_string());
    }
    if let Err(error) =
        inspect_source_panes_mode(&guard, proof, protocol != destination_protocol, true)
    {
        problems.push(error.to_string());
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(invalid(problems.join("\n")))
    }
}

fn inspect_source_panes_mode(
    guard: &crate::instance::UiUpdateGuard,
    proof: Option<crate::model::HostSessionProof>,
    require_resumable: bool,
    reconcile_retained: bool,
) -> Result<(), UpdateError> {
    let (protocol, proof) = source::inspect_identity(proof.as_ref())?;
    let helper = crate::companion::Product::Daemon.beside(&std::env::current_exe()?)?;
    let before = fs::symlink_metadata(&helper)?;
    if !before.is_file()
        || !crate::install_guard::trusted_executable_owner(before.uid())
        || before.mode() & 0o022 != 0
    {
        return Err(invalid(
            "The matched source inspector is unavailable; source windows remain usable",
        ));
    }
    let request = crate::install_guard::SourceInspectionRequest {
        protocol,
        proof,
        require_resumable,
        reconcile_retained,
    };
    let mut command = Command::new(&helper);
    crate::install_guard::sanitize_child_environment(&mut command);
    command.args([
        "--inspect-upgrade-source",
        &serde_json::to_string(&request)?,
    ]);
    guard.inherit_for_install(&mut command)?;
    let mut output = bounded_command(
        &mut command,
        "source pane inspection (windows remain open)",
        16 * 1024,
        Duration::from_secs(35),
        &CancellationToken::new(),
    )?;
    let after = fs::symlink_metadata(helper)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(invalid(
            "The source inspector changed during inspection; source windows remain usable",
        ));
    }
    let result: Result<(), String> = serde_json::from_reader(&mut output)?;
    result.map_err(invalid)?;
    let (current_protocol, current_proof) = source::inspect_identity(request.proof.as_ref())?;
    if current_protocol != request.protocol || current_proof != request.proof {
        return Err(invalid(
            "Source ownership changed during pane inspection; source windows remain usable",
        ));
    }
    Ok(())
}

/// Probe only a fixed, owned companion beside an authenticated editor process.
/// No daemon startup command or inherited installer authority is passed.
pub(crate) fn companion_protocol(executable: &Path) -> Result<u16, UpdateError> {
    let envelope = companion_read(executable, &["--json", "daemon", "protocol-version"], None)?;
    envelope
        .get("data")
        .and_then(|data| data.get("protocol_version"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u16::try_from(version).ok())
        .filter(|version| *version > 0)
        .ok_or_else(|| invalid("Ceditor's companion did not report a valid protocol"))
}

pub(crate) fn companion_namespace(executable: &Path) -> Result<String, UpdateError> {
    // `daemon status` reports the compiled instance even when stopped. Give it
    // a fresh nonexistent runtime, so it cannot contact production or start a
    // service. This also works with older unmarked companion sets.
    let isolated = std::env::temp_dir().join(format!(
        "cagents-namespace-inspect-{}",
        uuid::Uuid::new_v4()
    ));
    let envelope = companion_read(executable, &["--json", "daemon", "status"], Some(&isolated))?;
    match envelope
        .get("data")
        .and_then(|data| data.get("instance"))
        .and_then(serde_json::Value::as_str)
    {
        Some(instance @ ("production" | "development")) => Ok(instance.into()),
        _ => Err(invalid(
            "Ceditor's companion has no verifiable application instance",
        )),
    }
}

fn companion_read(
    executable: &Path,
    arguments: &[&str],
    runtime: Option<&Path>,
) -> Result<serde_json::Value, UpdateError> {
    let before = fs::symlink_metadata(executable)?;
    if !before.is_file()
        || !crate::install_guard::trusted_executable_owner(before.uid())
        || before.mode() & 0o022 != 0
    {
        return Err(invalid("Ceditor's companion is not an owned executable"));
    }
    let mut command = Command::new(executable);
    crate::install_guard::sanitize_child_environment(&mut command);
    command.args(arguments);
    if let Some(runtime) = runtime {
        command.env("XDG_RUNTIME_DIR", runtime);
    }
    let mut output = bounded_command(
        &mut command,
        "editor companion protocol check",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )?;
    let after = fs::symlink_metadata(executable)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || before.uid() != after.uid()
        || before.mode() != after.mode()
    {
        return Err(invalid("Ceditor's companion changed during preflight"));
    }
    Ok(serde_json::from_reader(&mut output)?)
}

fn observe_impact(
    candidate: &impact::Candidate,
    policy: impact::Policy,
) -> Result<impact::Preview, UpdateError> {
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let mut client = match source::Client::connect(&socket) {
        Ok(client) => Some(client),
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            let paths = crate::daemon::RuntimePaths::discover()
                .map_err(|error| invalid(error.to_string()))?;
            if crate::daemon::HostLease::is_held(&paths)
                .map_err(|error| invalid(error.to_string()))?
            {
                return Err(invalid(
                    "The terminal service is starting or stopping; retry the update preview shortly",
                ));
            }
            None
        }
        Err(error) => return Err(invalid(error.to_string())),
    };
    let replaces_daemon = client
        .as_ref()
        .is_some_and(|client| candidate.protocol != client.protocol());
    let daemon = if replaces_daemon {
        Some(
            client
                .as_mut()
                .expect("connected above")
                .upgrade_impact(policy)
                .map_err(|error| invalid(error.to_string()))?,
        )
    } else {
        None
    };
    Ok(impact::Preview {
        candidate: candidate.clone(),
        replaces_daemon,
        daemon,
    })
}

fn wait_daemon_stopped(socket: &Path) -> Result<(), UpdateError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    loop {
        if !crate::daemon::HostLease::is_held(&paths).map_err(|error| invalid(error.to_string()))?
            && !socket.exists()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(UpdateError::Timeout("terminal service checkpoint"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_installer(
    installation: &Installation,
    release: &Release,
    installer: &File,
    archive: &File,
    checksums: &File,
    restart_daemon: bool,
    guard: &crate::instance::UiUpdateGuard,
) -> Result<(), UpdateError> {
    let files = [
        installer.as_raw_fd(),
        archive.as_raw_fd(),
        checksums.as_raw_fd(),
    ];
    let mut command = Command::new("/bin/sh");
    command
        .arg(format!("/proc/self/fd/{}", files[0]))
        .args(["--version", &release.version, "--prefix"])
        .arg(&installation.prefix)
        .env("CAGENTS_ARCHIVE", format!("/proc/self/fd/{}", files[1]))
        .env("CAGENTS_CHECKSUMS", format!("/proc/self/fd/{}", files[2]))
        .env("CAGENTS_REPOSITORY", REPOSITORY)
        .env(
            "CAGENTS_INSTALL_OWNER_CLI",
            installation
                .prefix
                .join("versions")
                .join(&installation.version)
                .join("bin/cagents"),
        )
        .env_remove("CAGENTS_INSTALL_RESULT_FD")
        .env_remove("ENV")
        .env_remove("BASH_ENV")
        .stdin(Stdio::null());
    if restart_daemon {
        command.arg("--restart-daemon");
    }
    guard.inherit_for_install(&mut command)?;
    // SAFETY: descriptor flags only; sealed anonymous files cannot be replaced
    // between checksum verification and the installer's own second verification.
    unsafe {
        command.pre_exec(move || {
            for fd in files {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    // Do not kill an installer mid-transaction. Its migration protocol owns
    // bounded shutdown/startup and rollback, and its log survives every UI.
    let result = command.status()?;
    if result.success() {
        Ok(())
    } else {
        Err(invalid(format!(
            "installer failed ({result}); inspect the update log before retrying"
        )))
    }
}

fn set_phase(
    state: &Path,
    job: &mut UpdateJob,
    phase: UpdatePhase,
    message: &str,
) -> Result<(), UpdateError> {
    job.phase = phase;
    job.message = message.into();
    write_job(state, job)
}

fn private_create(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > limit
    {
        return Err(invalid("unsafe updater state file"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("updater state exceeds limit"));
    }
    Ok(bytes)
}
fn write_job(state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
    let temporary = state.join(format!("status-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = private_create(&temporary)?;
    serde_json::to_writer(&mut file, job)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, state.join("status.json"))?;
    File::open(state)?.sync_all()?;
    Ok(())
}
fn acquire_lock(state: &Path) -> Result<File, UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state.join("lock"))?;
    validate_lock(&file)?;
    // SAFETY: descriptor remains owned by the returned File.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::WouldBlock {
            UpdateError::Busy
        } else {
            error.into()
        });
    }
    Ok(file)
}
fn validate_lock(file: &File) -> Result<(), UpdateError> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(invalid("unsafe updater lock"));
    }
    Ok(())
}
fn inherited_lock(state: &Path) -> Result<File, UpdateError> {
    // SAFETY: F_GETFD probes existence before taking ownership of the reserved fd.
    if unsafe { libc::fcntl(WORKER_LOCK_FD, libc::F_GETFD) } == -1 {
        return Err(invalid(
            "update-worker must be started by the Update action",
        ));
    }
    let file = unsafe { File::from_raw_fd(WORKER_LOCK_FD) };
    validate_lock(&file)?;
    let actual = file.metadata()?;
    let expected = fs::symlink_metadata(state.join("lock"))?;
    if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err(invalid("worker inherited the wrong lock"));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(UpdateError::Busy);
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(file)
}

fn metadata(endpoint: &str, cancellation: &CancellationToken) -> Result<Vec<u8>, UpdateError> {
    let mut file = gh_request(
        endpoint,
        false,
        METADATA_LIMIT,
        Duration::from_secs(30),
        cancellation,
    )?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn download(
    asset: &Asset,
    limit: u64,
    cancellation: &CancellationToken,
) -> Result<File, UpdateError> {
    let endpoint = format!("repos/{REPOSITORY}/releases/assets/{}", asset.id);
    let file = gh_request(
        &endpoint,
        true,
        limit.min(asset.size),
        Duration::from_secs(15 * 60),
        cancellation,
    )?;
    if file.metadata()?.len() != asset.size {
        return Err(invalid(format!("download size differs for {}", asset.name)));
    }
    Ok(file)
}

// The existing GitHub executor is UI-gated. This small streaming runner keeps
// the CLI dependency graph separate, bounds memory/output/time, and never
// captures gh stderr (which could contain authentication diagnostics).
fn gh_request(
    endpoint: &str,
    binary: bool,
    limit: u64,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<File, UpdateError> {
    if cancellation.cancelled() {
        return Err(UpdateError::Cancelled);
    }
    let mut command = Command::new("gh");
    command
        .args([
            "api",
            "--hostname",
            "github.com",
            "--method",
            "GET",
            "--header",
            if binary {
                "Accept: application/octet-stream"
            } else {
                "Accept: application/vnd.github+json"
            },
            endpoint,
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env_remove("GH_FORCE_TTY")
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .env_remove("GH_DEBUG");
    bounded_command(&mut command, "GitHub request", limit, timeout, cancellation)
}

/// Stream a subprocess into sealed storage without buffering a release binary
/// in memory. Extraction and candidate probes share the downloader's limits.
fn bounded_command(
    command: &mut Command,
    operation: &'static str,
    limit: u64,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<File, UpdateError> {
    if cancellation.cancelled() {
        return Err(UpdateError::Cancelled);
    }
    let mut file = anonymous_file()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let child = command
        .spawn()
        .map_err(|error| invalid(format!("cannot start {operation}: {error}")))?;
    let mut child = StreamingChild::new(child)?;
    let mut pipe = child.child.stdout.take().expect("piped stdout");
    let result = (|| {
        let fd = pipe.as_raw_fd();
        // SAFETY: flags are changed on the live stdout descriptor only.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error().into());
        }
        let start = Instant::now();
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        let mut eof = false;
        loop {
            if cancellation.cancelled() {
                return Err(UpdateError::Cancelled);
            }
            if start.elapsed() > timeout {
                return Err(UpdateError::Timeout(operation));
            }
            match pipe.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(length) => {
                    total += length as u64;
                    if total > limit {
                        return Err(invalid(format!("{operation} exceeds its size limit")));
                    }
                    file.write_all(&buffer[..length])?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
            if eof && let Some(status) = child.observed_exit_status()? {
                if !status.success() {
                    return Err(invalid(format!("{operation} failed ({status})")));
                }
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        seal_file(&file)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(())
    })();
    // Keep the original leader unreaped through failed status and sealing/seek
    // errors, since each still needs original-group cleanup. Success preserves
    // the existing policy of leaving independently running descendants alone.
    let cleanup = child.finish(result.is_err());
    match (result, cleanup) {
        (Err(operation), Err(cleanup)) => Err(UpdateError::Cleanup {
            operation: Box::new(operation),
            cleanup,
        }),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
        (Ok(()), Ok(())) => Ok(file),
    }
}

/// Sole owner of the streaming command. No observation releases its leader
/// until every operation that can still fail and require group cleanup ends.
struct StreamingChild {
    child: std::process::Child,
    identity: OwnedFd,
    finished: bool,
}

impl StreamingChild {
    fn new(child: std::process::Child) -> io::Result<Self> {
        // Capture before the freshly spawned child can be reaped or handed off.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
        let identity = if raw < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful pidfd_open returns a new owned descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        };
        Self::with_identity(child, identity)
    }

    fn with_identity(
        mut child: std::process::Child,
        identity: io::Result<OwnedFd>,
    ) -> io::Result<Self> {
        let identity = match identity {
            Ok(identity) => identity,
            Err(error) => {
                // Capture failed before any reaping/handoff. The fresh leader
                // still reserves its original group; no lifecycle fallback follows.
                let group = if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } == 0 {
                    Ok(())
                } else {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        Ok(())
                    } else {
                        Err(error)
                    }
                };
                // Never synchronously wait on a known-live, signal-denied child.
                let cleanup = child
                    .kill()
                    .and_then(|()| child.wait().map(|_| ()))
                    .and(group);
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup) => io::Error::new(
                        error.kind(),
                        format!("{error}; streaming startup cleanup is unconfirmed: {cleanup}"),
                    ),
                });
            }
        };
        Ok(Self {
            child,
            identity,
            finished: false,
        })
    }

    fn status(&self, flags: i32) -> io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: the captured original pidfd cannot select a recycled PID.
            // Only finish omits WNOWAIT, after its last possible group signal.
            if unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    self.identity.as_raw_fd() as libc::id_t,
                    &mut information,
                    flags,
                )
            } == 0
            {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        let (pid, status) = unsafe { (information.si_pid(), information.si_status()) };
        if pid == 0 {
            return Ok(None);
        }
        if pid as u32 != self.child.id() {
            return Err(io::Error::other(
                "streaming child observation returned another process",
            ));
        }
        let raw = match information.si_code {
            libc::CLD_EXITED => status << 8,
            libc::CLD_KILLED => status,
            libc::CLD_DUMPED => status | 0x80,
            _ => {
                return Err(io::Error::other(
                    "streaming child observation returned an unexpected status",
                ));
            }
        };
        Ok(Some(std::process::ExitStatus::from_raw(raw)))
    }

    fn observed_exit_status(&self) -> io::Result<Option<std::process::ExitStatus>> {
        self.status(libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
    }

    fn finish(&mut self, terminate_group: bool) -> io::Result<()> {
        self.finish_with(terminate_group, |group| {
            // SAFETY: the sole owner retains its original leader until final reap.
            if unsafe { libc::kill(-(group as i32), libc::SIGKILL) } == 0 {
                Ok(())
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        })
    }

    fn finish_with(
        &mut self,
        terminate_group: bool,
        signal: impl FnOnce(u32) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        // Ownership loss is final; Drop cannot retry a numeric signal later.
        self.finished = true;
        let observed = self.observed_exit_status()?;
        let cleanup = if terminate_group {
            signal(self.child.id())
        } else {
            Ok(())
        };
        // A failed signal must not leave a known-exited original leader as a
        // zombie. Conversely, a known-live signal-denied child cannot be waited
        // on indefinitely just to report cleanup uncertainty.
        if cleanup.is_ok() || observed.is_some() {
            let reap = self.status(libc::WEXITED).and_then(|status| {
                status.ok_or_else(|| {
                    io::Error::other("streaming child final reap returned no status")
                })
            });
            if let Err(reap) = reap {
                return Err(match cleanup {
                    Err(cleanup) => io::Error::new(
                        cleanup.kind(),
                        format!("{cleanup}; final reap also failed: {reap}"),
                    ),
                    Ok(()) => reap,
                });
            }
        }
        cleanup
    }
}

impl Drop for StreamingChild {
    fn drop(&mut self) {
        let _ = self.finish(true);
    }
}

#[cfg(target_os = "linux")]
fn anonymous_file() -> io::Result<File> {
    let name = CString::new("cagents-verified-update").expect("literal has no nul");
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[cfg(not(target_os = "linux"))]
fn anonymous_file() -> io::Result<File> {
    Err(io::Error::other("self-update requires Linux"))
}
#[cfg(target_os = "linux")]
fn seal_file(file: &File) -> io::Result<()> {
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn seal_file(_: &File) -> io::Result<()> {
    Err(io::Error::other("self-update requires Linux"))
}

fn checksum_for(manifest: &str, name: &str) -> Result<[u8; 32], UpdateError> {
    let mut found = None;
    for line in manifest.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 67 || bytes[64] != b' ' || !matches!(bytes[65], b' ' | b'*') {
            return Err(invalid("malformed release checksum manifest"));
        }
        if &line[66..] != name {
            continue;
        }
        if found.is_some() {
            return Err(invalid(format!("duplicate checksum for {name}")));
        }
        let mut digest = [0u8; 32];
        for (index, pair) in bytes[..64].chunks_exact(2).enumerate() {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or_else(|| invalid("invalid SHA-256 digest"))?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or_else(|| invalid("invalid SHA-256 digest"))?;
            digest[index] = ((high << 4) | low) as u8;
        }
        found = Some(digest);
    }
    found.ok_or_else(|| invalid(format!("release lacks checksum for {name}")))
}
fn verify_digest(file: &mut File, expected: &[u8; 32]) -> Result<(), UpdateError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    if hasher.finalize().as_slice() != expected {
        return Err(invalid(
            "release SHA-256 verification failed; installation was not changed",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn streaming_capture_failure_cleans_fresh_original_before_returning_error() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
        assert!(raw >= 0);
        let identity = unsafe { OwnedFd::from_raw_fd(raw) };
        let error = match StreamingChild::with_identity(
            child,
            Err(io::Error::from_raw_os_error(libc::EMFILE)),
        ) {
            Err(error) => error,
            Ok(_) => panic!("injected identity capture failure must refuse startup"),
        };
        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    identity.as_raw_fd() as libc::id_t,
                    &mut status,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn bounded_stream_preserves_success_and_size_failure() {
        let mut file = bounded_command(
            Command::new("/bin/sh").args(["-c", "printf fixture"]),
            "owned fixture",
            7,
            Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"fixture");
        assert!(
            bounded_command(
                Command::new("/bin/sh").args(["-c", "printf fixture"]),
                "owned fixture",
                6,
                Duration::from_secs(2),
                &CancellationToken::new()
            )
            .unwrap_err()
            .to_string()
            .contains("size limit")
        );
    }

    #[test]
    fn bounded_stream_keeps_live_timeout_and_cancellation_cleanup() {
        let cancellation = CancellationToken::new();
        let started = Instant::now();
        assert!(matches!(
            bounded_command(
                Command::new("/bin/sleep").arg("30"),
                "owned fixture",
                10,
                Duration::from_millis(20),
                &cancellation
            ),
            Err(UpdateError::Timeout("owned fixture"))
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            signal.cancel();
        });
        let started = Instant::now();
        let result = bounded_command(
            Command::new("/bin/sleep").arg("30"),
            "owned fixture",
            10,
            Duration::from_secs(2),
            &cancellation,
        );
        canceller.join().unwrap();
        assert!(matches!(result, Err(UpdateError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn streaming_child_observation_keeps_leader_until_final_cleanup() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut child = StreamingChild::new(child).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.observed_exit_status().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(status.code(), Some(7));
        assert_eq!(
            child.observed_exit_status().unwrap().unwrap().code(),
            Some(7)
        );
        // Mirrors any late sealing/seek error after EOF+failed/successful status.
        child.finish(true).unwrap();
        assert_eq!(
            child.observed_exit_status().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(child.finish(true).is_ok(), "completion remains disarmed");
    }

    #[test]
    fn streaming_signal_error_reaps_exited_leader_and_refuses_live_wait() {
        let child = Command::new("/bin/true").process_group(0).spawn().unwrap();
        let mut child = StreamingChild::new(child).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.observed_exit_status().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            child
                .finish_with(true, |_| Err(io::Error::from_raw_os_error(libc::EPERM)))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            child.observed_exit_status().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );

        let child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut child = StreamingChild::new(child).unwrap();
        struct OwnedCleanup(OwnedFd);
        impl Drop for OwnedCleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                    let mut status: libc::siginfo_t = std::mem::zeroed();
                    libc::waitid(
                        libc::P_PIDFD,
                        self.0.as_raw_fd() as libc::id_t,
                        &mut status,
                        libc::WEXITED,
                    );
                }
            }
        }
        let _cleanup = OwnedCleanup(child.identity.try_clone().unwrap());
        let started = Instant::now();
        assert_eq!(
            child
                .finish_with(true, |_| Err(io::Error::from_raw_os_error(libc::EPERM)))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(child.observed_exit_status().unwrap().is_none());
        // Own test-only cleanup uses the still-original capability, not a PGID.
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    child.identity.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0
        );
        child.status(libc::WEXITED).unwrap().unwrap();

        let error = UpdateError::Cleanup {
            operation: Box::new(UpdateError::Cancelled),
            cleanup: io::Error::from_raw_os_error(libc::EPERM),
        };
        assert!(
            error
                .to_string()
                .contains("cancelled; streaming command cleanup is unconfirmed")
        );
        assert!(
            matches!(error, UpdateError::Cleanup { operation, .. } if matches!(*operation, UpdateError::Cancelled))
        );
    }

    #[test]
    fn bounded_failed_stream_cleans_owned_descendants_after_exit() {
        const MARKER: &str = "CAGENTS_UPDATE_DESCENDANT_CHILD";
        const TEST: &str =
            "update::tests::bounded_failed_stream_cleans_owned_descendants_after_exit";
        if std::env::var_os(MARKER).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(MARKER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("owned failed update stream descendant cleaned")
            );
            return;
        }
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        let directory = tempfile::tempdir().unwrap();
        let pidfile = directory.path().join("descendant.pid");
        let script = r#"import os,signal,sys
r,w=os.pipe()
pid=os.fork()
if pid==0:
 os.close(r); os.close(1)
 with open(sys.argv[1],'w') as f: f.write(str(os.getpid()))
 os.write(w,b'1'); os.close(w)
 while True: signal.pause()
os.close(w); os.read(r,1); os.close(r)
print('partial',flush=True)
sys.exit(7)
"#;
        let result = bounded_command(
            Command::new("/usr/bin/python3")
                .args(["-c", script])
                .arg(&pidfile),
            "owned fixture",
            100,
            Duration::from_secs(2),
            &CancellationToken::new(),
        );
        assert!(result.unwrap_err().to_string().contains("failed"));
        let pid: libc::pid_t = fs::read_to_string(pidfile).unwrap().parse().unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        assert!(raw >= 0);
        struct Cleanup {
            identity: OwnedFd,
            pid: libc::pid_t,
            reaped: bool,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if !self.reaped {
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            self.identity.as_raw_fd(),
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                        libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                    }
                }
            }
        }
        let mut cleanup = Cleanup {
            identity: unsafe { OwnedFd::from_raw_fd(raw) },
            pid,
            reaped: false,
        };
        let mut poll = libc::pollfd {
            fd: cleanup.identity.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 2000) }, 1);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        cleanup.reaped = true;
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            std::process::ExitStatus::from_raw(status).signal(),
            Some(libc::SIGKILL)
        );
        println!("owned failed update stream descendant cleaned");
    }

    #[test]
    #[ignore = "requires an owned user/PID namespace and namespace-local PID allocation control"]
    fn streaming_child_refuses_a_kernel_reused_original_pid() {
        const MARKER: &str = "CAGENTS_UPDATE_FIXED_REUSE_CHILD";
        const NS: &str = "CAGENTS_UPDATE_FIXED_OUTER_NS";
        const TEST: &str = "update::tests::streaming_child_refuses_a_kernel_reused_original_pid";
        if std::env::var_os(MARKER).is_none() {
            let output = Command::new("/usr/bin/unshare")
                .args([
                    "--user",
                    "--map-root-user",
                    "--pid",
                    "--fork",
                    "--mount-proc",
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    TEST,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(MARKER, "1")
                .env(NS, fs::read_link("/proc/self/ns/pid").unwrap())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("original update pidfd refused recycled group")
            );
            return;
        }
        assert_eq!(std::process::id(), 1);
        assert_ne!(
            fs::read_link("/proc/self/ns/pid").unwrap().as_os_str(),
            std::env::var_os(NS).unwrap()
        );
        fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
        let child = Command::new("/bin/true").process_group(0).spawn().unwrap();
        assert_eq!(child.id(), 200);
        let mut original = StreamingChild::new(child).unwrap();
        original.child.wait().unwrap(); // Deliberately violate sole-reaper ownership.
        fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
        let mut victim = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        assert_eq!(victim.id(), 200);
        assert_eq!(unsafe { libc::getpgid(200) }, 200);
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, 200, 0) } as i32;
        assert!(raw >= 0);
        struct Cleanup(OwnedFd);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
        let cleanup = Cleanup(unsafe { OwnedFd::from_raw_fd(raw) });
        assert_eq!(
            original.finish(true).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        drop(original);
        let mut poll = libc::pollfd {
            fd: cleanup.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 250) }, 0);
        assert!(victim.try_wait().unwrap().is_none());
        println!("original update pidfd refused recycled group PID/PGID200");
        drop(cleanup);
        victim.wait().unwrap();
    }
    use super::*;

    #[test]
    fn update_preview_replaces_a_nonempty_incompatible_source_and_leaves_compatible_source_untouched()
     {
        const CASE: &str = "CAGENTS_TEST_SCROLLBACK_UPDATE_PREVIEW_CASE";
        let selector = thread::current().name().unwrap().to_owned();
        let Some(case) = std::env::var_os(CASE) else {
            for case in ["compatible", "incompatible"] {
                let directory = tempfile::Builder::new().prefix("u").tempdir().unwrap();
                for leaf in ["runtime", "config", "data", "cache", "state"] {
                    let path = directory.path().join(leaf);
                    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
                }
                let mut child = Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                    .env(CASE, case)
                    .env("TMPDIR", directory.path());
                for (key, leaf) in [
                    ("XDG_RUNTIME_DIR", "runtime"),
                    ("XDG_CONFIG_HOME", "config"),
                    ("XDG_DATA_HOME", "data"),
                    ("XDG_CACHE_HOME", "cache"),
                    ("XDG_STATE_HOME", "state"),
                ] {
                    child.env(key, directory.path().join(leaf));
                }
                // HOME and the opaque invoking authority remain inherited;
                // this child can contact only its private synthetic source.
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let text = String::from_utf8_lossy(&output.stdout);
                assert!(
                    text.contains("running 1 test")
                        && text.contains("1 passed; 0 failed; 0 ignored"),
                    "{text}"
                );
            }
            return;
        };
        let incompatible = match case.to_str() {
            Some("compatible") => false,
            Some("incompatible") => true,
            _ => panic!("unexpected private preview fixture case"),
        };
        use crate::terminal_ipc::{
            ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
            read_frame, write_frame,
        };
        use std::os::unix::net::UnixListener;
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        assert!(
            socket.starts_with(&runtime),
            "fixture must never connect outside its private runtime"
        );
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(socket.parent().unwrap())
            .unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("private-budget-preview"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("private-source-incarnation"),
        };
        // The current direct client does not require a lease for this fake
        // peer. Hold a real private lease as well, so the same fixture remains
        // valid when composed with authenticated source-version discovery.
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(socket.with_file_name("cagentsd.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        serde_json::to_writer(
            &lease,
            &serde_json::json!({"pid": std::process::id(), "proof": &proof}),
        )
        .unwrap();
        lease.sync_all().unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let id = crate::model::TerminalId::new();
        let expected = impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof.clone(),
                token: "private-preview-only".into(),
                inventory_generation: 7,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: "none".into(),
                expires_at_unix_ms: 100,
            },
            policy: impact::Policy::WhenIdle,
            sessions: vec![impact::SessionImpact {
                id,
                kind: impact::SessionKind::Terminal,
                title: "Existing private terminal".into(),
                identity: "exact-private-source-identity".into(),
                disposition: impact::Disposition::Continues,
                reason: "Synthetic retained source for read-only preview".into(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        };
        let response = expected.clone();
        let (finished, completion) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut connections = 0;
            let mut impact_requests = 0;
            loop {
                assert!(
                    Instant::now() < deadline,
                    "private source fixture did not finish"
                );
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        match completion.recv_timeout(Duration::from_millis(10)) {
                            Ok(()) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(error) => panic!("private caller disappeared: {error}"),
                        }
                    }
                    Err(error) => panic!("private accept: {error}"),
                };
                connections += 1;
                assert!(connections <= 2, "unexpected discovery/reconnect replay");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
                assert_eq!(connect.version, crate::terminal_ipc::PROTOCOL_VERSION);
                assert_eq!(connect.request_id, 1);
                assert!(connect.proof.is_none());
                assert!(
                    matches!(connect.request, ClientRequest::Connect { expected_host: None, ref resume } if resume.is_empty())
                );
                write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: crate::terminal_ipc::PROTOCOL_VERSION,
                        request_id: Some(1),
                        proof: proof.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: Vec::new(),
                            },
                        },
                    },
                )
                .unwrap();
                let Some(request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() else {
                    // A header-discovery connection and a compatible preview
                    // both end without issuing an inventory/mutation request.
                    continue;
                };
                impact_requests += 1;
                assert!(incompatible && impact_requests == 1);
                assert_eq!(request.version, crate::terminal_ipc::PROTOCOL_VERSION);
                assert_eq!(request.request_id, 2);
                assert_eq!(request.proof, Some(proof.clone()));
                assert_eq!(
                    request.request,
                    ClientRequest::UpgradeImpact {
                        policy: impact::Policy::WhenIdle
                    }
                );
                write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: crate::terminal_ipc::PROTOCOL_VERSION,
                        request_id: Some(2),
                        proof: proof.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::UpgradeImpact(response.clone()),
                        },
                    },
                )
                .unwrap();
                assert!(
                    read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none(),
                    "preview must never prepare/stop/replay"
                );
            }
            assert_eq!(impact_requests, usize::from(incompatible));
        });
        let candidate = impact::Candidate {
            version: "9.8.7".into(),
            digest: "private-verified-build".into(),
            // This synthetic differing version exercises the actual updater
            // path; it reserves no production protocol beyond allocated125.
            protocol: crate::terminal_ipc::PROTOCOL_VERSION + u16::from(incompatible),
        };
        let observed = observe_impact(&candidate, impact::Policy::WhenIdle).unwrap();
        assert_eq!(observed.candidate, candidate);
        assert_eq!(observed.replaces_daemon, incompatible);
        assert_eq!(observed.daemon, incompatible.then_some(expected));
        finished.send(()).unwrap();
        server.join().unwrap();
        drop(lease);
    }

    #[test]
    fn installed_entry_routes_ordinary_skew_only_through_its_exact_owned_old_cli() {
        const CASE: &str = "CAGENTS_TEST_INSTALLED_ENTRY_OWNER";
        let selector = thread::current().name().unwrap().to_owned();
        if std::env::var_os(CASE).is_none() {
            let directory = tempfile::Builder::new().prefix("u").tempdir().unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(CASE, "1")
                .env("TMPDIR", directory.path());
            for (key, leaf) in [
                ("XDG_RUNTIME_DIR", "run"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_CACHE_HOME", "cache"),
                ("XDG_STATE_HOME", "state"),
            ] {
                let path = directory.path().join(leaf);
                fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
                child.env(key, path);
            }
            // HOME and operational authority are inherited unchanged. Only
            // this fresh private Unix source can be reached by the child.
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.contains("running 1 test") && text.contains("1 passed; 0 failed; 0 ignored"),
                "{text}"
            );
            return;
        }
        use crate::terminal_ipc::{
            ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
        };
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        assert!(socket.starts_with(&runtime));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(socket.parent().unwrap())
            .unwrap();
        struct Fixture {
            child: std::process::Child,
            release: PathBuf,
        }
        impl Fixture {
            fn finish(&mut self) {
                fs::write(&self.release, b"finish").unwrap();
                assert!(self.child.wait().unwrap().success());
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::write(&self.release, b"finish");
                let _ = self.child.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let prefix = root.join("installed");
        fs::DirBuilder::new().mode(0o700).create(&prefix).unwrap();
        install_tree(&prefix, &["old", "new"], "new");
        fs::copy("/usr/bin/python3", prefix.join("versions/old/bin/cagentsd")).unwrap();
        let window = locate(&prefix.join("versions/new/bin/cagents-ui")).unwrap();
        let old_window = locate(&prefix.join("versions/old/bin/cagents-ui")).unwrap();
        let served = window.version_root.clone();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("private-installed-entry"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("private-entry-incarnation"),
        };
        let connect = postcard::to_stdvec(&ClientEnvelope {
            version: crate::terminal_ipc::PROTOCOL_VERSION,
            request_id: 1,
            proof: None,
            request: ClientRequest::Connect {
                expected_host: None,
                resume: Vec::new(),
            },
        })
        .unwrap();
        let current_reply = postcard::to_stdvec(&ServerEnvelope {
            version: crate::terminal_ipc::PROTOCOL_VERSION,
            request_id: Some(1),
            proof: proof.clone(),
            message: ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
            },
        })
        .unwrap();
        let mut other_reply =
            postcard::to_stdvec(&(crate::terminal_ipc::PROTOCOL_VERSION - 1)).unwrap();
        // Deliberately not a current or protocol77 envelope. Entry routing
        // must never decode this unknown source's response DTO.
        other_reply.extend_from_slice(b"opaque-unqualified-source-dto");
        let script = root.join("source.py");
        fs::write(&script, r#"
import fcntl,json,os,socket,struct,sys,time
from pathlib import Path
c=json.loads(Path(sys.argv[1]).read_text());s=Path(c['socket']);lock=s.with_name('cagentsd.lock')
deadline=time.monotonic()+15
f=lock.open('x+');os.chmod(lock,0o600)
if c['held']: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)
json.dump(dict(pid=os.getpid(),proof=c['proof']),f);f.flush();os.fsync(f.fileno())
listener=socket.socket(socket.AF_UNIX);listener.bind(str(s));listener.listen(2);listener.settimeout(.05)
Path(c['ready']).write_text(str(os.getpid()))
count=0
try:
 while not Path(c['release']).exists():
  assert time.monotonic()<deadline
  try: stream,_=listener.accept()
  except TimeoutError: continue
  with stream:
   stream.settimeout(5)
   def read(n):
    data=b''
    while len(data)<n:
     part=stream.recv(n-len(data));assert part;data+=part
    return data
   n=struct.unpack('>I',read(4))[0];assert n<=1024*1024
   assert read(n).hex()==c['connect']
   count+=1;assert count<=c['connections']
   reply=bytes.fromhex(c['reply']);stream.sendall(struct.pack('>I',len(reply))+reply)
   assert stream.recv(1)==b'', 'entry must not preview/prepare/stop'
 assert count==c['connections']
finally:
 listener.close();f.close();s.unlink();lock.unlink()
"#).unwrap();
        let mut sequence = 0;
        let mut start = |executable: &Path, current: bool, held: bool, connections: usize| {
            sequence += 1;
            let ready = root.join(format!("ready-{sequence}"));
            let release = root.join(format!("release-{sequence}"));
            let config = root.join(format!("source-{sequence}.json"));
            fs::write(&config, serde_json::to_vec(&serde_json::json!({
                "socket": socket, "proof": proof, "held": held,
                "ready": ready, "release": release,
                "connect": connect.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                "reply": (if current { &current_reply } else { &other_reply }).iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                "connections": connections,
            })).unwrap()).unwrap();
            let child = Command::new(executable)
                .args([script.as_os_str(), config.as_os_str()])
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let mut fixture = Fixture { child, release };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(fixture.child.try_wait().unwrap().is_none());
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            fixture
        };
        let executable = prefix.join("versions/old/bin/cagentsd");
        // The served window is already new. This exact production entry
        // decision must nevertheless route the unknown old owner to its CLI.
        let mut fixture = start(&executable, false, true, 1);
        assert_eq!(installed_entry_worker(&window, &served).unwrap(), "old");
        fixture.finish();
        // Fully qualified current peers still follow the original UI-only
        // replacement route; no inventory or preparation request is sent.
        let mut fixture = start(&executable, true, true, 2);
        assert_eq!(installed_entry_worker(&old_window, &served).unwrap(), "old");
        fixture.finish();
        let mut fixture = start(&executable, false, false, 1);
        assert!(installed_entry_worker(&window, &served).is_err());
        fixture.finish();
        // Unknown service at the served candidate must not fall back to the
        // old window's CLI merely to replace that window's UI.
        fs::copy("/usr/bin/python3", prefix.join("versions/new/bin/cagentsd")).unwrap();
        let mut fixture = start(&prefix.join("versions/new/bin/cagentsd"), false, true, 1);
        assert!(installed_entry_worker(&old_window, &served).is_err());
        fixture.finish();
        let foreign = root.join("foreign");
        fs::DirBuilder::new().mode(0o700).create(&foreign).unwrap();
        install_tree(&foreign, &["old"], "old");
        fs::copy(
            "/usr/bin/python3",
            foreign.join("versions/old/bin/cagentsd"),
        )
        .unwrap();
        let mut fixture = start(&foreign.join("versions/old/bin/cagentsd"), false, true, 1);
        assert!(installed_entry_worker(&window, &served).is_err());
        fixture.finish();
        fs::remove_file(foreign.join(".cagents-install-root")).unwrap();
        let mut fixture = start(&foreign.join("versions/old/bin/cagentsd"), false, true, 1);
        assert!(installed_entry_worker(&window, &served).is_err());
        fixture.finish();
        // A replacement using the very same executable and proof text is
        // still another process: the held original pidfd/epoch must refuse it.
        let mut fixture = start(&executable, false, true, 1);
        let original = source::inspect_installed_identity().unwrap();
        fixture.finish();
        let mut replacement = start(&executable, false, true, 0);
        // Replacement serves no request; only its real lock/exe are inspected.
        let owner = installed_daemon_owner(&window).unwrap().unwrap();
        assert!(
            original
                .verify_owner(Some(&owner.version_root.join("bin/cagentsd")))
                .is_err()
        );
        // No connection to replacement was made: the original peer's pidfd
        // and proof fence reject it before any header/DTO or mutation.
        replacement.finish();
    }

    #[test]
    fn staged_update_selects_the_worker_for_both_directions_of_version_skew() {
        let layout = |version: &str| InstallLayout {
            prefix: "/owned/install".into(),
            version_root: PathBuf::from("/owned/install/versions").join(version),
            version: version.into(),
        };
        let old = layout("old");
        let new = layout("new");
        // A newer UI cannot speak the old service's protocol.
        assert_eq!(
            installation_worker_version(&new, &new.version_root, Some(&old), false),
            "old"
        );
        // Once that service is already new, the old UI still needs replacement:
        // selecting the new worker would reject its candidate as itself.
        assert_eq!(
            installation_worker_version(&old, &new.version_root, Some(&new), false),
            "old"
        );
        // Repeated staging must use the actual owner, not the window or current.
        let staged = layout("third");
        assert_eq!(
            installation_worker_version(&old, &staged.version_root, Some(&new), false),
            "new"
        );
        // An authenticated legacy source must be migrated by the new matched
        // coordinator, even when that coordinator is already the served build.
        assert_eq!(
            installation_worker_version(&new, &new.version_root, Some(&old), true),
            "new"
        );
        // Bootstrap from the staged CLI also works when the installed daemon
        // has the current wire protocol but an older executable/version root.
        // Reusing its old updater would preserve the very bug being repaired.
        let proof = crate::model::HostSessionProof {
            host_session_id: "current-source".into(),
            boot_id: "current-boot".into(),
            host_incarnation: "current-incarnation".into(),
        };
        assert_eq!(
            installed_update_worker(
                &new,
                &new.version_root,
                Some(&old),
                crate::terminal_ipc::PROTOCOL_VERSION,
                Some(&proof),
            )
            .unwrap(),
            "new"
        );
        assert!(candidate_can_upgrade_source(
            &new.version_root,
            Some(&old),
            crate::terminal_ipc::PROTOCOL_VERSION,
            Some(&proof)
        ));
        assert!(!candidate_can_upgrade_source(
            &new.version_root,
            Some(&new),
            crate::terminal_ipc::PROTOCOL_VERSION,
            Some(&proof)
        ));
        assert!(!candidate_can_upgrade_source(
            &new.version_root,
            Some(&old),
            crate::terminal_ipc::PROTOCOL_VERSION,
            None
        ));
        // The real entry decision must not mistake an already-served window
        // for a completed daemon migration. No-daemon/current observations keep
        // the old already-running refusal instead of granting a legacy route.
        for source_proof in [
            None,
            Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("current"),
                boot_id: crate::model::BootId::new("current"),
                host_incarnation: crate::model::HostIncarnation::new("current"),
            }),
        ] {
            assert!(
                installed_update_worker(
                    &new,
                    &new.version_root,
                    Some(&new),
                    crate::terminal_ipc::PROTOCOL_VERSION,
                    source_proof.as_ref()
                )
                .is_err()
            );
        }
        assert_eq!(
            installed_update_worker(
                &old,
                &new.version_root,
                None,
                crate::terminal_ipc::PROTOCOL_VERSION,
                None
            )
            .unwrap(),
            "old"
        );
        // Without a service, the old window's worker can start the candidate.
        assert_eq!(
            installation_worker_version(&old, &new.version_root, None, false),
            "old"
        );
    }
    use std::os::unix::fs::PermissionsExt;

    fn release(version: &str) -> GitHubRelease {
        GitHubRelease {
            id: 1,
            tag_name: format!("v{version}"),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    id: 11,
                    name: format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz"),
                    size: 100,
                },
                Asset {
                    id: 12,
                    name: "SHA256SUMS".into(),
                    size: 200,
                },
                Asset {
                    id: 13,
                    name: "install.sh".into(),
                    size: 100,
                },
            ],
        }
    }

    #[test]
    fn stable_versions_are_ordered_numerically_and_previews_ignored() {
        let mut draft = release("99.0.0");
        draft.draft = true;
        let mut preview = release("98.0.0");
        preview.prerelease = true;
        let state = select_release(
            vec![
                release("1.9.0"),
                release("1.10.0"),
                draft,
                preview,
                release("2.0.0-rc.1"),
                release("2.0.0+local"),
            ],
            "1.8.0",
        )
        .unwrap();
        assert!(matches!(state, UpdateState::Available { release } if release.version == "1.10.0"));
        assert!(matches!(
            select_release(vec![release("1.9.0")], "1.10.0").unwrap(),
            UpdateState::Current { .. }
        ));
    }

    #[test]
    fn local_install_updates_only_to_a_newer_stable_release() {
        assert!(matches!(
            select_release(vec![release("0.1.3")], "0.1.3+local.937921c").unwrap(),
            UpdateState::Current { .. }
        ));
        assert!(matches!(
            select_release(vec![release("0.1.4")], "0.1.3+local.937921c").unwrap(),
            UpdateState::Available { release } if release.version == "0.1.4"
        ));
        assert!(select_release(vec![release("0.1.4")], "0.1.3-rc.1").is_err());
        assert!(stable_version("0.1.4+local.abc").is_err());
    }

    #[test]
    fn missing_wrong_duplicate_and_oversized_assets_never_fallback() {
        let mut wrong = release("2.0.0");
        wrong.assets[0].name = "cagents-2.0.0-aarch64-apple-darwin.tar.gz".into();
        assert!(select_release(vec![release("1.5.0"), wrong], "1.0.0").is_err());
        let mut missing = release("2.0.0");
        missing.assets.pop();
        assert!(applicable_release(missing).is_err());
        let mut duplicate = release("2.0.0");
        duplicate.assets.push(duplicate.assets[0].clone());
        assert!(applicable_release(duplicate).is_err());
        let mut oversized = release("2.0.0");
        oversized.assets[0].size = ARCHIVE_LIMIT + 1;
        assert!(applicable_release(oversized).is_err());
    }

    #[test]
    fn checksum_requires_exact_filename_and_unique_entry() {
        let hash = "a".repeat(64);
        assert_eq!(
            checksum_for(&format!("{hash}  install.sh\n"), "install.sh").unwrap(),
            [0xaa; 32]
        );
        assert!(checksum_for(&format!("{hash}  other/install.sh\n"), "install.sh").is_err());
        assert!(
            checksum_for(
                &format!("{hash}  install.sh\n{hash}  install.sh\n"),
                "install.sh"
            )
            .is_err()
        );
        assert!(checksum_for("not-a-hash  install.sh", "install.sh").is_err());
    }

    #[test]
    fn tampered_bytes_fail_and_verified_bytes_cannot_change() {
        let mut file = anonymous_file().unwrap();
        file.write_all(b"trusted archive").unwrap();
        seal_file(&file).unwrap();
        let trusted: [u8; 32] = Sha256::digest(b"trusted archive").into();
        verify_digest(&mut file, &trusted).unwrap();
        assert!(file.write_all(b"tamper").is_err());
        let other: [u8; 32] = Sha256::digest(b"different archive").into();
        assert!(verify_digest(&mut file, &other).is_err());
    }

    #[test]
    fn source_package_and_unmarked_installations_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        for relative in [
            "target/release/cagents",
            "usr/bin/cagents",
            "versions/0.1.0/bin/cagents",
        ] {
            let path = root.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"binary").unwrap();
            assert!(Installation::from_executable(&path).is_err());
        }
    }

    #[test]
    fn installed_version_must_match_the_running_binary_including_build_metadata() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        install_tree(root, &[crate::VERSION], crate::VERSION);
        let executable = root
            .join("versions")
            .join(crate::VERSION)
            .join("bin/cagents-ui");
        assert_eq!(
            Installation::from_executable(&executable).unwrap().version,
            crate::VERSION
        );

        let impostor = format!("{}+local.impostor", env!("CARGO_PKG_VERSION"));
        let another = tempfile::tempdir().unwrap();
        let another = another.path();
        install_tree(another, &[&impostor], &impostor);
        assert!(
            Installation::from_executable(
                &another
                    .join("versions")
                    .join(&impostor)
                    .join("bin/cagents-ui")
            )
            .is_err()
        );
    }

    /// Builds `<root>/{.cagents-install-root,versions/<v>/bin/{cagents,cagents-ui,cagentsd}}`
    /// and points `current` at `served`.
    fn install_tree(root: &Path, versions: &[&str], served: &str) {
        fs::write(root.join(".cagents-install-root"), b"cagents install root").unwrap();
        for version in versions {
            let bin = root.join("versions").join(version).join("bin");
            fs::create_dir_all(&bin).unwrap();
            for name in ["cagents", "cagents-ui", "cagentsd"] {
                fs::write(bin.join(name), b"binary").unwrap();
            }
        }
        std::os::unix::fs::symlink(
            root.join("versions").join(served),
            root.join("current-next"),
        )
        .unwrap();
        fs::rename(root.join("current-next"), root.join("current")).unwrap();
    }

    #[test]
    fn a_failed_build_requires_a_later_matched_installation_to_be_superseded() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let old = "0.1.3+local.old";
        let current = "0.1.3+local.current";
        install_tree(root, &[old, current], current);
        let window = locate(&root.join("current/bin/cagents-ui")).unwrap();
        let owner = locate(&root.join("current/bin/cagentsd")).unwrap();
        let served = served_version_root(&window).unwrap();
        let mut job = UpdateJob {
            id: "historical-failure".into(),
            version: old.into(),
            phase: UpdatePhase::Failed,
            message: "installed daemon startup failed".into(),
            log_path: root.join(".update/historical-failure.log"),
            confirmation: None,
        };
        let superseded = |job: &UpdateJob, switched| {
            failed_job_is_superseded(job, &window, &served, Some(&owner), switched)
        };
        assert!(superseded(&job, true));
        assert!(
            !superseded(&job, false),
            "a rollback does not resolve its failure"
        );
        job.version = "0.1.4".into();
        assert!(
            !superseded(&job, true),
            "a failed newer release remains actionable"
        );
        job.version = current.into();
        assert!(
            !superseded(&job, true),
            "the running build can still have failed"
        );
        job.version = "0.1.2".into();
        assert!(superseded(&job, true));
        job.version = "invalid".into();
        assert!(!superseded(&job, true));
        job.version = old.into();
        job.phase = UpdatePhase::Installing;
        assert!(!superseded(&job, true), "never filter a live operation");
        job.phase = UpdatePhase::Failed;
        assert!(!failed_job_is_superseded(
            &job, &window, &served, None, true
        ));
        let old_owner = locate(&root.join("versions").join(old).join("bin/cagentsd")).unwrap();
        assert!(!failed_job_is_superseded(
            &job,
            &window,
            &served,
            Some(&old_owner),
            true
        ));
        assert!(
            !failed_job_is_superseded(&job, &old_owner, &served, Some(&owner), true),
            "an old window still needs replacing"
        );
    }

    #[test]
    fn a_window_left_behind_by_an_install_finds_the_installed_ui() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        install_tree(root, &["0.1.0", "0.2.0+local.abc"], "0.1.0");
        let running = root.join("versions/0.1.0/bin/cagents-ui");
        assert_eq!(superseding_ui_from(&running), None);

        install_tree(root, &[], "0.2.0+local.abc");
        assert_eq!(
            superseding_ui_from(&running),
            Some(
                root.canonicalize()
                    .unwrap()
                    .join("versions/0.2.0+local.abc/bin/cagents-ui")
            ),
            "a local install is still an install, and the running window is still stale"
        );

        // Half of a version directory cannot host a window: its daemon or CLI
        // would come from somewhere else.
        fs::remove_file(root.join("versions/0.2.0+local.abc/bin/cagentsd")).unwrap();
        assert_eq!(superseding_ui_from(&running), None);

        // Nor can a complete tree that belongs to some other installation:
        // reopening the windows there would abandon this one.
        let elsewhere = tempfile::tempdir().unwrap();
        install_tree(elsewhere.path(), &["0.3.0"], "0.3.0");
        fs::remove_file(root.join("current")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("versions/0.3.0"),
            root.join("current"),
        )
        .unwrap();
        assert_eq!(superseding_ui_from(&running), None);
    }

    #[test]
    fn a_source_build_is_never_superseded_by_a_directory_beside_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        install_tree(root, &["0.1.0"], "0.1.0");
        for relative in ["target/dev-runtime/cagents-ui", "usr/bin/cagents-ui"] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"binary").unwrap();
            assert_eq!(superseding_ui_from(&path), None);
        }
    }

    #[test]
    fn concurrent_apply_lease_survives_parent_handle_and_excludes_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let first = acquire_lock(root.path()).unwrap();
        let inherited = first.try_clone().unwrap();
        assert!(matches!(acquire_lock(root.path()), Err(UpdateError::Busy)));
        drop(first);
        assert!(matches!(acquire_lock(root.path()), Err(UpdateError::Busy)));
        drop(inherited);
        // Other tests may fork while either handle is live; those children
        // retain the description until their CLOEXEC boundary.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match acquire_lock(root.path()) {
                Ok(next) => {
                    drop(next);
                    break;
                }
                Err(UpdateError::Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                result => panic!("worker lease was not released: {result:?}"),
            }
        }
    }

    #[test]
    fn unsafe_update_directory_and_symlink_lock_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let installation = Installation {
            prefix: root.path().into(),
            version: "1.0.0".into(),
        };
        fs::create_dir(root.path().join(".update")).unwrap();
        fs::set_permissions(
            root.path().join(".update"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(installation.state_dir().is_err());
        let destination = root.path().join("destination");
        fs::write(&destination, b"untouched").unwrap();
        std::os::unix::fs::symlink(&destination, root.path().join("lock")).unwrap();
        assert!(acquire_lock(root.path()).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"untouched");
    }
}
