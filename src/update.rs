//! Explicit, self-managed release updates. Releases are read through public
//! download URLs and trusted only through their signed metadata ([`feed`]);
//! discovery never writes installation state. The detached worker alone may
//! run an installer.
mod cancellation;
mod candidate;
mod confirmation;
mod feed;
mod floor;
mod floor_display;
mod floor_prediction;
mod handoff;
pub mod in_flight;
mod installed;
mod pending_recovery;
pub(crate) mod recovery;
mod retention;
#[cfg(all(feature = "ui", feature = "development"))]
mod review_fixture;
pub use recovery::TargetRecoveryRequired;
pub(crate) mod source;
pub use confirmation::{Choice as UpdateChoice, Confirmation as UpdateConfirmation, decide};
pub mod impact;
mod review;
pub mod source_inspection;
pub use review::render_review;
mod signature;
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
const NO_DAEMON_INSTALLED: &str = "The application files are installed. No daemon was serving before the update, and none is running now; the daemon will start at graphical login.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handoff {
    Served,
    NoDaemon,
}

struct Completion {
    handoff: Handoff,
    pane_notes: Option<String>,
}

/// SIGINT or SIGTERM to this process while installed, without SA_RESTART.
pub(crate) struct WorkerInterrupt {
    _installed: cancellation::Interrupt,
}

impl WorkerInterrupt {
    pub(crate) fn install() -> io::Result<Self> {
        cancellation::Interrupt::install().map(|installed| Self {
            _installed: installed,
        })
    }
    pub(crate) fn requested() -> bool {
        cancellation::requested()
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken {
    requested: Arc<AtomicBool>,
    marker: Option<PathBuf>,
    terminal_interrupts: bool,
}
impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }
    fn for_terminal() -> Self {
        Self {
            terminal_interrupts: true,
            ..Self::default()
        }
    }
    fn cancelled(&self) -> bool {
        (self.terminal_interrupts && cancellation::requested())
            || self.requested.load(Ordering::Relaxed)
            || self.marker.as_ref().is_some_and(|marker| marker.exists())
    }
    fn for_job(state: &Path, job: &str) -> Result<Self, UpdateError> {
        Ok(Self {
            requested: Arc::default(),
            marker: confirmation::cancellation_path(state, job)?,
            terminal_interrupts: false,
        })
    }
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("{0}")]
    Invalid(String),
    #[error("another update is already running")]
    Busy,
    #[error(transparent)]
    TargetRecoveryRequired(Box<recovery::TargetRecoveryRequired>),
    #[error("Update cancelled")]
    Cancelled,
    /// The review was closed, or expired, while something it named had to
    /// be resolved by hand first. Unlike a cancellation, this says what.
    #[error("{}", .0.report())]
    Refused(Box<impact::Refusal>),
    #[error("{0} timed out")]
    Timeout(&'static str),
    /// A bounded subprocess exited unsuccessfully. Callers that know the
    /// program, like the release feed's curl, turn its status into words.
    #[error("{operation} failed ({status})")]
    Exited {
        operation: &'static str,
        status: std::process::ExitStatus,
    },
    /// The source daemon refused a request as busy each time it was asked,
    /// before admitting anything. `serving` is the authenticated daemon that
    /// answered, so a receipt can say a daemon is serving.
    #[error("{message}")]
    SourceBusy {
        serving: Box<crate::model::HostSessionProof>,
        message: String,
    },
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

impl From<recovery::TargetRecoveryRequired> for UpdateError {
    fn from(required: recovery::TargetRecoveryRequired) -> Self {
        Self::TargetRecoveryRequired(Box::new(required))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// What changed, from the release's signed metadata (Markdown).
    #[serde(default)]
    pub notes: String,
    /// SHA-256 of the signed metadata this release was read from, so the
    /// worker installs exactly the release the person chose.
    metadata: [u8; 32],
    archive: Asset,
    checksums: Asset,
    installer: Asset,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Asset {
    name: String,
    size: u64,
    sha256: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdateState {
    Unsupported { reason: String },
    Current { version: String },
    Available { release: Box<Release> },
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
    /// What held a refused update back. A refused job ends `Failed` with this
    /// set and `message` naming the same reasons, so a reader that predates
    /// this field still shows them rather than a cancellation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<impact::Refusal>,
    /// The conversations that had work in flight when the update began, and
    /// what became of it. Empty until the update has changed a provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_flight: Vec<in_flight::Outcome>,
    /// The release whose worker owns this job. Releases up to 0.2.3 do not
    /// record it; their workers checked the successor daemon through their
    /// own wire protocol, so a newer target could fail that check while
    /// serving correctly. A worker that records its release confirms the
    /// successor without the protocol, so its failure is its own verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
}

impl UpdateError {
    /// What a refused update's record keeps. A refusal is not a phase of its
    /// own: windows and CLIs of other versions read the job, and an unknown
    /// phase would hide it from them. They read `Failed` and the message.
    fn refusal(&self) -> Option<impact::Refusal> {
        match self {
            Self::Refused(refusal) => Some((**refusal).clone()),
            _ => None,
        }
    }

    /// The job's message for this failure. A refusal keeps every reason on
    /// one line there, the form readers of other versions parse.
    fn record_message(&self) -> String {
        match self {
            Self::Refused(refusal) => refusal.message(),
            error => error.to_string(),
        }
    }
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
        if layout.version != crate::version() {
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
        state_dir(&self.prefix)
    }
}

fn state_dir(prefix: &Path) -> Result<PathBuf, UpdateError> {
    let path = prefix.join(".update");
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

/// Holds the update job lease of the installation at `prefix`, so no job can
/// run or start while the caller changes what that installation serves.
/// [`UpdateError::Busy`] means a job is active: its worker keeps the lease for
/// its whole lifetime, including review and confirmation.
pub(crate) fn exclude_update_jobs(
    prefix: &Path,
) -> Result<crate::instance::OwnedLock, UpdateError> {
    acquire_lock(&state_dir(prefix)?)
}

/// Left by `install.sh` in a version tree whose readiness it could not
/// verify; a later install of that release revalidates the tree.
const RETAINED_CANDIDATE_MARKER: &str = ".cagents-retained-candidate-v1.json";
/// Left by `install.sh` in a tree it extracted for an update worker, including
/// one kept after that worker's installation failed and rolled back.
const PREPARED_CANDIDATE_MARKER: &str = ".cagents-prepared-candidate-v1.json";

/// Whether `install.sh` left the version tree at `root` as a candidate it has
/// not verified, such as one kept after a failed or rolled-back update. It
/// removes both markers once it verifies a publication; a later install of that
/// release revalidates a marked tree. Releases before that removal left the
/// prepared marker on trees they published, which are refused here too.
pub(crate) fn is_unverified_candidate(root: &Path) -> bool {
    [RETAINED_CANDIDATE_MARKER, PREPARED_CANDIDATE_MARKER]
        .iter()
        .any(|marker| fs::symlink_metadata(root.join(marker)).is_ok())
}

/// Whether `install.sh` would refuse to install `version` again because this
/// installation already holds it as an ordinary tree, served or not, such as
/// a staged build that was withdrawn. Only candidate trees it marked can be
/// reinstalled, which revalidates them.
fn holds_installed_tree(prefix: &Path, version: &str) -> bool {
    let root = prefix.join("versions").join(version);
    fs::symlink_metadata(&root).is_ok() && !is_unverified_candidate(&root)
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
    #[cfg(all(feature = "ui", feature = "development"))]
    if let Some(root) = std::env::var_os("CAGENTS_EVIDENCE_FIXTURE")
        && let Some(release) = review_fixture::release(Path::new(&root))?
    {
        return Ok(UpdateState::Available {
            release: Box::new(release),
        });
    }
    let installation = match Installation::detect() {
        Ok(installation) => installation,
        Err(error) => {
            return Ok(UpdateState::Unsupported {
                reason: unmanaged_reason(error),
            });
        }
    };
    let feed = match feed::Feed::configured() {
        Ok(feed) => feed,
        Err(reason) => return Ok(UpdateState::Unsupported { reason }),
    };
    let release = feed.latest(
        |version| {
            // Offering a version already unpacked would only end in the
            // installer's refusal. That build is offered through `current`.
            Ok(is_newer(version, &installation.version)?
                && !holds_installed_tree(&installation.prefix, version))
        },
        cancellation,
    )?;
    Ok(match release {
        Some(release) => UpdateState::Available {
            release: Box::new(release),
        },
        None => UpdateState::Current {
            version: installation.version,
        },
    })
}

/// Whether the published `release` is an upgrade from the `installed`
/// version. A local build's metadata gives it no precedence over a release
/// with the same base version.
fn is_newer(release: &str, installed: &str) -> Result<bool, UpdateError> {
    Ok(stable_version(release)? > installed_version(installed)?)
}

/// Why this build cannot update itself, in the words its About page shows.
/// Installs another tool manages say which tool, rather than describing the
/// layout this build looked for.
fn unmanaged_reason(error: UpdateError) -> String {
    // Only a layout install.sh did not make belongs to another tool; any
    // other failure of a self-managed install is reported as it is.
    if error.to_string() != unsupported_install().to_string() {
        return error.to_string();
    }
    let executable = std::env::current_exe().and_then(|path| path.canonicalize());
    match executable {
        Ok(path) if path.components().any(|part| part.as_os_str() == "target") => {
            "This is a build from source. Install a release to receive updates.".into()
        }
        Ok(path)
            if (path.starts_with("/usr") && !path.starts_with("/usr/local"))
                || path.starts_with("/opt") =>
        {
            "Installed by your system's package manager. Update cagents with it.".into()
        }
        _ => "This copy of cagents was not installed by its installer, so it cannot update itself. Update it the way you installed it.".into(),
    }
}

/// Explicit user authorization: this starts a detached worker, not an in-process
/// task. Its inherited lock closes the double-click / concurrent-window race.
#[derive(Serialize, Deserialize)]
enum UpdateRequest {
    Release(Release),
    ReleaseFromTerminal(Release),
    Installed { root: PathBuf },
    InstalledFromTerminal { root: PathBuf },
}

/// Whether this build asks for newer releases. A development build never
/// does, apart from offering the native evidence fixture's release.
pub fn discovers_releases() -> bool {
    #[cfg(all(feature = "ui", feature = "development"))]
    if std::env::var_os("CAGENTS_EVIDENCE_FIXTURE")
        .is_some_and(|root| review_fixture::offers_release(Path::new(&root)))
    {
        return true;
    }
    crate::instance::NAME != "development"
}

pub fn apply(release: &Release) -> Result<UpdateJob, UpdateError> {
    // A native evidence session may be offered the fixture's release, and
    // never installs any release.
    #[cfg(all(feature = "ui", feature = "development"))]
    if std::env::var_os("CAGENTS_EVIDENCE_FIXTURE").is_some() {
        return Err(invalid(
            "a native evidence session never installs a release",
        ));
    }
    recovery::inspect_pending_current()?;
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
    apply_installed_from(executable, false)
}

fn apply_installed_from(executable: &Path, terminal: bool) -> Result<UpdateJob, UpdateError> {
    recovery::inspect_pending_current()?;
    let mut installation = running_installation()?;
    let layout = locate(&std::env::current_exe()?)?;
    let served = served_version_root(&layout)?;
    if executable.canonicalize()? != served.join("bin/cagents-ui").canonicalize()? {
        return Err(invalid(
            "the installed candidate changed; inspect the installation again",
        ));
    }
    // The served build owns every update, including an older daemon's
    // replacement and a remaining window-only replacement.
    installation.version = installed_entry_worker(&layout, &served)?;
    let version = served
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(unsupported_install)?
        .to_owned();
    start_job(
        &installation,
        &version,
        &if terminal {
            UpdateRequest::InstalledFromTerminal { root: served }
        } else {
            UpdateRequest::Installed { root: served }
        },
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
    installed_candidate_version(
        window,
        served,
        owner.as_ref(),
        source.protocol,
        source.proof.as_ref(),
    )
}

/// Return the served candidate's version after source admission and the no-op
/// check. The candidate updater rechecks the source and its installed owner.
fn installed_candidate_version(
    window: &InstallLayout,
    served: &Path,
    daemon: Option<&InstallLayout>,
    source_protocol: u16,
    source_proof: Option<&crate::model::HostSessionProof>,
) -> Result<String, UpdateError> {
    if !source::is_qualified_source(source_protocol) {
        return Err(source::unsupported_source(source_protocol));
    }
    let replaces_daemon = source_proof.is_some()
        && (source_protocol != crate::terminal_ipc::PROTOCOL_VERSION
            || daemon.is_some_and(|owner| owner.version_root != served));
    if !replaces_daemon && served == window.version_root {
        return Err(invalid("the installed application is already running"));
    }
    served
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(unsupported_install)
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
/// used by the application. The served candidate always supplies the worker.
pub fn upgrade_installed(yes: bool, interrupt: bool) -> Result<UpdateJob, UpdateError> {
    let _interrupt = cancellation::Interrupt::install()?;
    // An interrupted preparation has its own immutable source census. Do not
    // route this retry through old `current` or construct a fresh empty source.
    recovery::inspect_pending_current()?;
    let layout = locate(&std::env::current_exe()?)?;
    let candidate = served_version_root(&layout)?.join("bin/cagents-ui");
    if cancellation::requested() {
        return Err(UpdateError::Cancelled);
    }
    let initial = apply_installed_from(&candidate, true)?;
    drive_terminal_review(initial, yes, interrupt)
}

pub fn upgrade_release(
    version: &str,
    yes: bool,
    interrupt: bool,
) -> Result<UpdateJob, UpdateError> {
    let _interrupt = cancellation::Interrupt::install()?;
    recovery::inspect_pending_current()?;
    let installation = Installation::detect()?;
    let parsed = stable_version(version)?;
    let selected = feed::Feed::configured()
        .map_err(invalid)?
        .release(&parsed.to_string(), &CancellationToken::for_terminal())?;
    if stable_version(&selected.version)? != parsed
        || parsed <= installed_version(&installation.version)?
    {
        return Err(invalid(
            "selected release is not newer than the running installation",
        ));
    }
    if cancellation::requested() {
        return Err(UpdateError::Cancelled);
    }
    let initial = start_job(
        &installation,
        &selected.version,
        &UpdateRequest::ReleaseFromTerminal(selected.clone()),
    )?;
    drive_terminal_review(initial, yes, interrupt)
}

/// Inspect retained preparation before constructing a new update census.
pub fn inspect_pending_recovery(
    candidate_cli: &Path,
    census: Option<&Path>,
) -> Result<(), UpdateError> {
    recovery::inspect_pending(candidate_cli, census)
}

/// Explicit checkpoint-bound continuation through the shared recovery owner.
pub fn recover_pending(
    checkpoint: &str,
    yes: bool,
    census: Option<&Path>,
) -> Result<String, UpdateError> {
    pending_recovery::run(checkpoint, yes, census)
}

fn drive_terminal_review(
    initial: UpdateJob,
    yes: bool,
    interrupt: bool,
) -> Result<UpdateJob, UpdateError> {
    drive_terminal_review_with(initial, yes, interrupt, &mut NativeTerminalReview)
}

trait TerminalReview: Write {
    fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError>;
    fn decide(&mut self, job: &str, nonce: &str, choice: UpdateChoice) -> Result<(), UpdateError>;
    fn cancel(&mut self, job: &str) -> Result<bool, UpdateError>;
    fn read_line(&mut self, answer: &mut String) -> io::Result<usize>;
    fn is_terminal(&self) -> bool;
    fn interrupted(&self) -> bool;
    fn worker_running(&mut self) -> Result<bool, UpdateError> {
        Ok(false)
    }
    fn pause(&mut self) {
        thread::sleep(Duration::from_millis(250));
    }
}

struct NativeTerminalReview;
impl Write for NativeTerminalReview {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        io::stderr().lock().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stderr().lock().flush()
    }
}
impl TerminalReview for NativeTerminalReview {
    fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
        status()
    }
    fn decide(&mut self, job: &str, nonce: &str, choice: UpdateChoice) -> Result<(), UpdateError> {
        decide(job, nonce, choice)
    }
    fn cancel(&mut self, job: &str) -> Result<bool, UpdateError> {
        confirmation::cancel(job)
    }
    fn read_line(&mut self, answer: &mut String) -> io::Result<usize> {
        cancellation::read_line(answer)
    }
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        io::stdin().is_terminal()
    }
    fn interrupted(&self) -> bool {
        cancellation::requested()
    }
    fn worker_running(&mut self) -> Result<bool, UpdateError> {
        let state = running_installation()?.state_dir()?;
        match acquire_lock(&state) {
            Ok(_lease) => Ok(false),
            Err(UpdateError::Busy) => Ok(true),
            Err(error) => Err(error),
        }
    }
}

fn drive_terminal_review_with(
    initial: UpdateJob,
    yes: bool,
    interrupt: bool,
    terminal: &mut impl TerminalReview,
) -> Result<UpdateJob, UpdateError> {
    let mut policy = if interrupt {
        impact::Policy::Interrupt
    } else {
        impact::Policy::WhenIdle
    };
    let mut submitted = std::collections::BTreeSet::new();
    let mut waiting = None;
    let mut policy_requested = None;
    let mut reviewed = None;
    // The review the person confirmed. A renewal it still covers, by the same
    // rule the worker uses to keep an approval, needs no second answer.
    let mut confirmed: Option<impact::Preview> = None;
    let deadline = Instant::now() + Duration::from_secs(45 * 60);
    let mut finishing_message_shown = false;
    loop {
        if Instant::now() >= deadline {
            return Err(invalid(
                "The update is still running; inspect its progress in the app.",
            ));
        }
        let job = terminal
            .status()?
            .filter(|job| job.id == initial.id)
            .ok_or_else(|| {
                invalid("The update job changed; inspect the current update in the app.")
            })?;
        if job.phase.is_terminal() {
            if terminal.worker_running()? {
                terminal.pause();
                continue;
            }
            // Refresh after proving the worker has settled: the snapshot read
            // before that lease probe may precede its final recovery report.
            let job = terminal
                .status()?
                .filter(|current| current.id == initial.id)
                .ok_or_else(|| {
                    invalid("The update job changed; inspect the current update in the app.")
                })?;
            return if job.phase == UpdatePhase::Complete {
                Ok(job)
            } else {
                Err(invalid(job.message))
            };
        }
        if terminal.interrupted() {
            if terminal.cancel(&job.id)? {
                return Err(UpdateError::Cancelled);
            }
            if !finishing_message_shown {
                writeln!(
                    terminal,
                    "The update is already being applied; finishing it before returning."
                )?;
                finishing_message_shown = true;
            }
        } else if let Some(confirmation) = job.active_confirmation() {
            if job.phase == UpdatePhase::WaitingForIdle {
                let labels: Vec<_> = confirmation
                    .preview
                    .daemon
                    .iter()
                    .flat_map(|daemon| &daemon.sessions)
                    .filter(|item| item.disposition == impact::Disposition::WaitsForIdle)
                    .map(|item| {
                        confirmation
                            .preview
                            .labels
                            .names
                            .get(&item.id)
                            .unwrap_or(&item.title)
                            .clone()
                    })
                    .collect();
                if !labels.is_empty() && waiting.as_ref() != Some(&labels) {
                    writeln!(
                        terminal,
                        "Waiting for these conversations to finish their current work:\n  {}\nCtrl-C cancels the update; nothing has been ended or stopped.",
                        labels.join("\n  ")
                    )?;
                    waiting = Some(labels);
                }
                // Approval belongs to this review, even if a worker republishes
                // it with a fresh nonce during its cancellable idle wait.
                submitted.insert(confirmation.nonce.clone());
            }
            if confirmation.stop.is_none()
                && confirmation
                    .preview
                    .daemon
                    .as_ref()
                    .is_some_and(|daemon| daemon.policy != policy)
            {
                if policy_requested.as_ref() != Some(&confirmation.nonce) {
                    terminal.decide(&job.id, &confirmation.nonce, UpdateChoice::Policy(policy))?;
                    policy_requested = Some(confirmation.nonce.clone());
                }
            } else if !submitted.contains(&confirmation.nonce) {
                if let Some(list) = &confirmation.stop {
                    write!(
                        terminal,
                        "{}",
                        if terminal.is_terminal() {
                            list.render()
                        } else {
                            list.render_preview()
                        }
                    )?;
                    if !terminal.is_terminal() {
                        writeln!(
                            terminal,
                            "Stopping live work needs a terminal. Rerun this command in a terminal."
                        )?;
                    }
                    let is_terminal = terminal.is_terminal();
                    let choice = match floor_display::read_stop_choice_from(
                        |answer| terminal.read_line(answer),
                        is_terminal,
                        list,
                    ) {
                        Ok(choice) => choice,
                        Err(error) => {
                            if terminal.interrupted() {
                                if terminal.cancel(&job.id)? {
                                    return Err(UpdateError::Cancelled);
                                }
                                continue;
                            }
                            let _ = terminal.cancel(&job.id);
                            return Err(error.into());
                        }
                    };
                    if !submit_current_choice(
                        terminal,
                        &job.id,
                        &confirmation.nonce,
                        choice.clone(),
                    )? {
                        // Live work changed while the person was reading or
                        // typing. Reload and display its new list in this CLI.
                        continue;
                    }
                    if matches!(choice, UpdateChoice::Cancel) {
                        return Err(confirmation::closed(&confirmation.preview));
                    }
                    submitted.insert(confirmation.nonce.clone());
                    continue;
                }
                if confirmed
                    .as_ref()
                    .is_some_and(|approved| approved.preserves_consent(&confirmation.preview))
                {
                    if submit_current_choice(
                        terminal,
                        &job.id,
                        &confirmation.nonce,
                        UpdateChoice::Confirm,
                    )? {
                        submitted.insert(confirmation.nonce.clone());
                    }
                    continue;
                }
                write!(
                    terminal,
                    "{}",
                    match &reviewed {
                        Some(previous) =>
                            review::render_review_changes(previous, &confirmation.preview),
                        None => render_review(&confirmation.preview),
                    }
                )?;
                let choice = if ordinary_approval(yes, terminal.is_terminal())
                    == Some(OrdinaryChoice::Cancel)
                {
                    let stopped_preview = floor::preview_stop_list(&confirmation.preview);
                    if let Err(error) = &stopped_preview {
                        writeln!(terminal, "Stop preview unavailable: {error}")?;
                    }
                    if let Some(list) = stopped_preview.ok().flatten() {
                        write!(terminal, "{}", list.render_preview())?;
                    }
                    writeln!(
                        terminal,
                        "Rerun this command in a terminal, or with --yes for the ordinary review. Stopping live work always needs a terminal."
                    )?;
                    OrdinaryChoice::Cancel
                } else if let Some(choice) = ordinary_approval(yes, terminal.is_terminal()) {
                    choice
                } else {
                    match prompt_review(terminal, &confirmation.preview) {
                        Ok(choice) => choice,
                        Err(error) => {
                            if terminal.interrupted() {
                                if terminal.cancel(&job.id)? {
                                    return Err(UpdateError::Cancelled);
                                }
                                continue;
                            }
                            // A failed read is not the person's answer, so
                            // it ends the update as the stop list's does.
                            let _ = terminal.cancel(&job.id);
                            return Err(error.into());
                        }
                    }
                };
                match choice {
                    OrdinaryChoice::Cancel => {
                        terminal.decide(&job.id, &confirmation.nonce, choice.into())?;
                        // Says what the worker records for this review.
                        return Err(confirmation::closed(&confirmation.preview));
                    }
                    OrdinaryChoice::InterruptNow => {
                        confirmed = None;
                        reviewed = Some(confirmation.preview.clone());
                        if !submit_current_choice(
                            terminal,
                            &job.id,
                            &confirmation.nonce,
                            choice.into(),
                        )? {
                            continue;
                        }
                        reviewed = Some(review::interruption_review(&confirmation.preview));
                        policy = impact::Policy::Interrupt;
                        policy_requested = Some(confirmation.nonce.clone());
                        submitted.insert(confirmation.nonce.clone());
                    }
                    OrdinaryChoice::Confirm => {
                        confirmed = Some(confirmation.preview.clone());
                        reviewed = Some(confirmation.preview.clone());
                        if submit_current_choice(
                            terminal,
                            &job.id,
                            &confirmation.nonce,
                            choice.into(),
                        )? {
                            submitted.insert(confirmation.nonce.clone());
                        }
                    }
                }
            }
        }
        terminal.pause();
    }
}

/// A stale nonce means re-display, while a real failure must release prepared
/// windows through cancellation of the current review.
fn submit_current_choice(
    terminal: &mut impl TerminalReview,
    job: &str,
    nonce: &str,
    choice: UpdateChoice,
) -> Result<bool, UpdateError> {
    match terminal.decide(job, nonce, choice) {
        Ok(()) => Ok(true),
        Err(error) => {
            if terminal
                .status()?
                .filter(|current| current.id == job)
                .and_then(|current| current.active_confirmation().cloned())
                .is_some_and(|confirmation| confirmation.nonce != nonce)
            {
                return Ok(false);
            }
            let _ = terminal.cancel(job);
            Err(error)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrdinaryChoice {
    Confirm,
    InterruptNow,
    Cancel,
}
impl From<OrdinaryChoice> for UpdateChoice {
    fn from(choice: OrdinaryChoice) -> Self {
        match choice {
            OrdinaryChoice::Confirm => Self::Confirm,
            OrdinaryChoice::InterruptNow => Self::InterruptNow,
            OrdinaryChoice::Cancel => Self::Cancel,
        }
    }
}

fn ordinary_approval(yes: bool, terminal: bool) -> Option<OrdinaryChoice> {
    if yes {
        Some(OrdinaryChoice::Confirm)
    } else if !terminal {
        Some(OrdinaryChoice::Cancel)
    } else {
        None
    }
}

fn review_answer(answer: &str, working: bool) -> Option<OrdinaryChoice> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Some(OrdinaryChoice::Confirm),
        "i" if working => Some(OrdinaryChoice::InterruptNow),
        "n" | "no" => Some(OrdinaryChoice::Cancel),
        _ => None,
    }
}

fn prompt_review(
    terminal: &mut impl TerminalReview,
    preview: &impact::Preview,
) -> io::Result<OrdinaryChoice> {
    let working = preview.daemon.as_ref().is_some_and(|daemon| {
        daemon.policy == impact::Policy::WhenIdle
            && daemon
                .sessions
                .iter()
                .any(|session| session.disposition == impact::Disposition::WaitsForIdle)
    });
    let stops = preview
        .daemon
        .as_ref()
        .is_some_and(impact::DaemonImpact::blocked);
    read_review_choice_from(
        |prompt, answer| {
            terminal.write_all(prompt.as_bytes())?;
            terminal.flush()?;
            terminal.read_line(answer)
        },
        working,
        stops,
    )
}

#[cfg(test)]
fn read_review_choice(
    input: &mut impl io::BufRead,
    output: &mut impl Write,
    working: bool,
    stops: bool,
) -> io::Result<OrdinaryChoice> {
    read_review_choice_from(
        |prompt, answer| {
            output.write_all(prompt.as_bytes())?;
            output.flush()?;
            input.read_line(answer)
        },
        working,
        stops,
    )
}

fn read_review_choice_from(
    mut read_line: impl FnMut(&str, &mut String) -> io::Result<usize>,
    working: bool,
    stops: bool,
) -> io::Result<OrdinaryChoice> {
    loop {
        let prompt = if working && stops {
            "Enter: review stops when they finish   i: interrupt them now   n: cancel\n"
        } else if working {
            "Enter: update when they finish   i: interrupt them now   n: cancel\n"
        } else if stops {
            "Enter: review what stops   n: cancel\n"
        } else {
            "Update now? [Y/n] "
        };
        let mut answer = String::new();
        if read_line(prompt, &mut answer)? == 0 {
            return Ok(OrdinaryChoice::Cancel);
        }
        if let Some(choice) = review_answer(&answer, working) {
            return Ok(choice);
        }
    }
}

/// The reviewed source, authenticated again: a preview never authorizes
/// acting on a different owner.
fn source_client_for(daemon: &impact::DaemonImpact) -> Result<source::Client, UpdateError> {
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let client = source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
    if *client.proof() != daemon.capability.proof {
        return Err(invalid(
            "The source changed since its update impact was reviewed",
        ));
    }
    Ok(client)
}

fn running_installation() -> Result<Installation, UpdateError> {
    let layout = locate(&std::env::current_exe()?)?;
    if layout.version != crate::version() {
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
    recovery::inspect_pending_current()?;
    let state = installation.state_dir()?;
    let lock = acquire_lock(&state)?;
    retention::before_start(&state);
    let id = uuid::Uuid::new_v4().to_string();
    let job = UpdateJob {
        id: id.clone(),
        version: version.into(),
        phase: UpdatePhase::Starting,
        message: "Starting verified update".into(),
        log_path: state.join(format!("{id}.log")),
        confirmation: None,
        refusal: None,
        in_flight: Vec::new(),
        recorded_by: Some(crate::version().into()),
    };
    let log = private_create(&job.log_path)?;
    confirmation::initialize(&state, &id)?;
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
            // spawn confirmed exec; the worker now owns the inherited lease.
            // Close our copy without unlocking its shared description.
            drop(lock.into_file());
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

/// The update job as a window should present it.
///
/// A failure recorded by an older worker after this window's own build, its
/// target, took over is settled here: while that worker is still finishing it
/// reads as still updating, and once the target's own verifier has proven the
/// service it reads, and is recorded, as complete. A failure that a later
/// switch of `current` resolved is not shown at all. Anything unproven keeps
/// the failure visible.
#[cfg(feature = "ui")]
pub fn status_for_ui() -> Result<Option<UpdateJob>, UpdateError> {
    #[cfg(feature = "development")]
    if let Some(root) = std::env::var_os("CAGENTS_EVIDENCE_FIXTURE")
        && let Some(job) = review_fixture::read(Path::new(&root))?
    {
        return Ok(Some(job));
    }
    let Some(job) = status()? else {
        return Ok(None);
    };
    Ok(present_for_ui(job, reconcile_failure))
}

/// `job` as a window shows it, settling a failure through `reconcile`.
#[cfg(feature = "ui")]
fn present_for_ui(
    job: UpdateJob,
    reconcile: impl FnOnce(&UpdateJob) -> Result<Reconciled, UpdateError>,
) -> Option<UpdateJob> {
    if job.phase != UpdatePhase::Failed {
        return Some(job);
    }
    // Missing evidence or an incompatible/starting daemon keeps the failure
    // visible. Reconciliation must never conceal an unresolved replacement.
    match reconcile(&job).unwrap_or(Reconciled::Shown) {
        Reconciled::Shown => Some(job),
        Reconciled::Superseded => None,
        Reconciled::Finishing => Some(finishing_presentation(job)),
        // Shown complete only once recorded so. A lease taken since, or a
        // record or receipt changed since, is judged again at the next poll.
        Reconciled::Completed { proof, state } => match settle_as_completed(&state, &job, &proof) {
            Ok(true) => Some(completed_presentation(job)),
            Ok(false) => Some(finishing_presentation(job)),
            Err(error) => {
                tracing::warn!(%error, "could not record the verified update as complete");
                Some(job)
            }
        },
    }
}

#[cfg(feature = "ui")]
enum Reconciled {
    Shown,
    /// A later switch of `current` resolved it; there is nothing to show.
    Superseded,
    /// The worker that recorded it has not finished; ask again.
    Finishing,
    /// The target's verifier proved `proof` serving, and it still serves.
    Completed {
        proof: crate::model::HostSessionProof,
        state: PathBuf,
    },
}

#[cfg(feature = "ui")]
fn reconcile_failure(job: &UpdateJob) -> Result<Reconciled, UpdateError> {
    let window = locate(&std::env::current_exe()?)?;
    let served = served_version_root(&window)?;
    let daemon = installed_daemon_owner(&window)?;
    if job.version == window.version {
        // The job's own recovery receipt, written by the target's verifier,
        // decides it.
        if !failed_job_reached_its_target(job, &window, &served, daemon.as_ref())
            || !protocol_check_could_have_failed_it(job)
        {
            return Ok(Reconciled::Shown);
        }
        let state = window.prefix.join(".update");
        let proof = match judge_target_failure(
            &target_evidence(&state, &job.id)?,
            std::time::SystemTime::now(),
        ) {
            TargetFailure::Finishing => return Ok(Reconciled::Finishing),
            TargetFailure::Unproven => return Ok(Reconciled::Shown),
            TargetFailure::Reached(proof) => proof,
        };
        // The receipt must describe the daemon serving now, not one that
        // has since restarted.
        let socket = crate::terminal_ipc::local_daemon_socket_path()?;
        source::Peer::confirm_serving(&socket, &served.join("bin/cagentsd"), &proof)?;
        return Ok(if daemon_speaks_version(&window.version)? {
            Reconciled::Completed { proof, state }
        } else {
            Reconciled::Shown
        });
    }
    // Failed local builds cannot be ordered by their metadata. A later switch
    // of `current`, followed by a healthy matched application set, demonstrates
    // supersession; a rollback before the failure was recorded does not.
    let failed = fs::symlink_metadata(window.prefix.join(".update/status.json"))?;
    let current = fs::symlink_metadata(window.prefix.join("current"))?;
    let switched_after_failure = current.file_type().is_symlink()
        && (current.ctime(), current.ctime_nsec()) > (failed.ctime(), failed.ctime_nsec());
    Ok(
        if failed_job_is_superseded(
            job,
            &window,
            &served,
            daemon.as_ref(),
            switched_after_failure,
        ) && daemon_speaks_version(&window.version)?
        {
            Reconciled::Superseded
        } else {
            Reconciled::Shown
        },
    )
}

/// The refusals with which e8bbb84f2 and 0.2.3 workers fail the successor
/// check through their own client, which cannot speak a newer protocol, each
/// as the text that opens it and the text that closes it. Those releases are
/// fixed, so these texts are too. The first is the connection refusal of the
/// protocol discovery; the second is a handshake at another protocol.
#[cfg(any(feature = "ui", feature = "host", test))]
const PROTOCOL_REFUSALS: [(&str, &str); 2] = [
    (
        "could not configure the daemon connection: The running cagents service uses protocol ",
        "without --restart-daemon, then reopen cagents.",
    ),
    (
        "The confirmed successor does not serve this update's matched protocol",
        "",
    ),
];

/// What the worker reported after the protocol refusal that failed `job`,
/// when that refusal is all that failed it: e8bbb84f2 and 0.2.3 record no
/// release and lead their record with the refusal. Every other failure,
/// including windows that did not reopen, an installer that failed, or a
/// helper that exited early, opens with its own cause and stands.
///
/// Those workers append any later failure to the record after a space, so
/// beyond the refusal only the reports they write are allowed: a line they
/// did not write is a failure of its own, and the record stands.
#[cfg(any(feature = "ui", feature = "host", test))]
fn protocol_refusal_reports(job: &UpdateJob) -> Option<String> {
    if job.recorded_by.is_some() {
        return None;
    }
    let reports = PROTOCOL_REFUSALS.iter().find_map(|(opening, closing)| {
        job.message
            .strip_prefix(opening)?
            .split_once(closing)
            .map(|(_, reports)| reports)
    })?;
    let mut lines = reports.split('\n');
    // The refusal's own line ends with it, or with the verified service
    // when the worker restarted it and had no reports to write first.
    if !matches!(lines.next(), Some("" | SERVICE_VERIFIED)) {
        return None;
    }
    for line in lines {
        // The worker lists what still runs last; those lines name processes.
        if line == "Still running, not attached:"
            || line == "Still running; attachment could not be checked:"
        {
            break;
        }
        if !(line.is_empty()
            || line.starts_with("  ")
            || line == SERVICE_VERIFIED
            || REPORT_LINES.contains(&line)
            || stopped_background_tasks(line))
        {
            return None;
        }
    }
    // Completion says the service was verified already.
    Some(reports.replacen(SERVICE_VERIFIED, "", 1))
}

/// What e8bbb84f2 and 0.2.3 workers add once they restarted and verified
/// the target's service.
#[cfg(any(feature = "ui", feature = "host", test))]
const SERVICE_VERIFIED: &str =
    " The terminal service is ready and its original terminal inventory was verified.";

/// The unindented lines of the reports e8bbb84f2 and 0.2.3 workers append to
/// a failure, other than the stopped background tasks; their items are
/// indented.
#[cfg(any(feature = "ui", feature = "host", test))]
const REPORT_LINES: [&str; 5] = [
    "The terminal service accepted the stop.",
    "Ended or stopped:",
    "Current turns interrupted:",
    "Interruption was not confirmed:",
    "Paused after update recovery:",
];

#[cfg(any(feature = "ui", feature = "host", test))]
fn stopped_background_tasks(line: &str) -> bool {
    line == "1 background task stopped; it does not resume automatically."
        || line
            .strip_suffix(" background tasks stopped; they do not resume automatically.")
            .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(any(feature = "ui", feature = "host", test))]
fn protocol_check_could_have_failed_it(job: &UpdateJob) -> bool {
    protocol_refusal_reports(job).is_some()
}

#[cfg(feature = "ui")]
fn daemon_speaks_version(version: &str) -> Result<bool, UpdateError> {
    let socket = crate::terminal_ipc::local_daemon_socket_path()?;
    let mut client = crate::control::ControlClient::connect(&socket)
        .map_err(|error| invalid(error.to_string()))?;
    Ok(client
        .daemon_version()
        .map_err(|error| invalid(error.to_string()))?
        == version)
}

/// Whether the worker that owns the update state is still running.
#[cfg(any(feature = "ui", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Worker {
    Running,
    Finished,
    Unknown,
}

/// What a window can read of a failure recorded against its own build.
#[cfg(any(feature = "ui", test))]
#[derive(Debug)]
struct TargetEvidence {
    /// Read first, so a receipt read after a finished worker is its last.
    worker: Worker,
    receipt: recovery::Reading,
    /// When the failure was recorded.
    recorded: std::time::SystemTime,
}

#[cfg(any(feature = "ui", test))]
#[derive(Debug, PartialEq, Eq)]
enum TargetFailure {
    /// The worker is still verifying the target; its verdict is not in.
    Finishing,
    /// The target's verifier found the service usable, served by this proof.
    Reached(crate::model::HostSessionProof),
    Unproven,
}

/// How long a pending placeholder can stand for a verifier still running: the
/// coordinator stops the verifier at its limit, then saves what it left.
#[cfg(any(feature = "ui", test))]
const PENDING_RECEIPT_PATIENCE: Duration =
    Duration::from_secs(recovery::VERIFIER_LIMIT.as_secs() + 10);

/// How long after recording a failure the worker may still be reopening
/// windows and verifying them before a window stops waiting for it. Past
/// this, the failure is shown as recorded; the target's daemon still settles
/// it once the worker lets go of the lease.
#[cfg(any(feature = "ui", test))]
const FINISHING_WORKER_PATIENCE: Duration = Duration::from_secs(3 * 60);

/// Whether a failed job whose target this window runs is settled, and how.
///
/// An older worker records the failure before it reopens windows, then saves
/// a pending placeholder and runs the target's verifier once more, so a
/// window opened in between can read either the earlier receipt or the
/// placeholder. Only the receipt left by a worker that no longer holds the
/// lease decides; a worker past the bounds above leaves its failure shown.
///
/// A worker killed right after recording the failure, or one whose last
/// verification failed before it saved the placeholder, reads the same as
/// one that finished. If it left the placeholder, nothing proved the service.
/// If it left a usable receipt, its verifier proved this build's daemon and
/// lease owner, which the caller matches to the live daemon, so the update
/// did install and serve; only its window reopening and session reports are
/// missing, and those were never written.
#[cfg(any(feature = "ui", test))]
fn judge_target_failure(evidence: &TargetEvidence, now: std::time::SystemTime) -> TargetFailure {
    // A timestamp in the future means the clock moved; stop waiting.
    let within = |since: std::time::SystemTime, patience: Duration| {
        now.duration_since(since)
            .is_ok_and(|waited| waited < patience)
    };
    match &evidence.receipt {
        recovery::Reading::Pending { since }
            if evidence.worker != Worker::Finished && within(*since, PENDING_RECEIPT_PATIENCE) =>
        {
            TargetFailure::Finishing
        }
        recovery::Reading::Pending { .. } => TargetFailure::Unproven,
        // A worker still holding the lease has not given its last word: no
        // receipt it may yet replace proves anything.
        _ if evidence.worker == Worker::Running => {
            if within(evidence.recorded, FINISHING_WORKER_PATIENCE) {
                TargetFailure::Finishing
            } else {
                TargetFailure::Unproven
            }
        }
        recovery::Reading::Usable(proof) => TargetFailure::Reached(proof.clone()),
        recovery::Reading::Unusable => TargetFailure::Unproven,
    }
}

#[cfg(feature = "ui")]
fn target_evidence(state: &Path, job: &str) -> Result<TargetEvidence, UpdateError> {
    let worker = match crate::instance::private_lock_is_held(&state.join("lock")) {
        Ok(true) => Worker::Running,
        Ok(false) => Worker::Finished,
        Err(_) => Worker::Unknown,
    };
    Ok(TargetEvidence {
        worker,
        receipt: recovery::read_receipt(state, job)?,
        recorded: fs::symlink_metadata(state.join("status.json"))?.modified()?,
    })
}

/// A failure an older worker recorded, while it is still verifying the target.
#[cfg(feature = "ui")]
fn finishing_presentation(mut job: UpdateJob) -> UpdateJob {
    job.phase = UpdatePhase::Installing;
    job.message = "Finishing the update".into();
    job.confirmation = None;
    job.refusal = None;
    job
}

#[cfg(any(feature = "ui", feature = "host"))]
fn completed_presentation(mut job: UpdateJob) -> UpdateJob {
    // Keep what the worker reported about sessions it ended, paused or left
    // running; the success notice shows it as details.
    let reports = protocol_refusal_reports(&job).unwrap_or_default();
    job.phase = UpdatePhase::Complete;
    job.message = format!(
        "Update installed; cagents {} verified its terminal service{reports}",
        job.version
    );
    job.confirmation = None;
    job.refusal = None;
    job
}

/// Settle a failure that the target's own verifier proved wrong: record the
/// job complete through the ordinary job writer, so the CLI, later windows
/// and later daemons agree, even once the daemon that proved it restarts. The
/// failure as recorded is kept beside it, in `<job>.settled.json`.
///
/// Only with the updater lease held, which no worker then holds, and only
/// while the job and its receipt are still those that were judged: a failure
/// `protocol_check_could_have_failed_it`, for the build serving now, whose
/// receipt shows `proof` serving. Whether `proof` is the daemon serving now
/// is the caller's to establish. Returns whether the job is now complete.
#[cfg(any(feature = "ui", feature = "host"))]
fn settle_as_completed(
    state: &Path,
    judged: &UpdateJob,
    proof: &crate::model::HostSessionProof,
) -> Result<bool, UpdateError> {
    // A worker still holding the lease records its own verdict.
    let _lock = match acquire_lock(state) {
        Err(UpdateError::Busy) => return Ok(false),
        lock => lock?,
    };
    let current: UpdateJob =
        serde_json::from_slice(&read_private(&state.join("status.json"), JOB_STATE_LIMIT)?)?;
    if current.id != judged.id
        || current.phase != UpdatePhase::Failed
        || current.version != judged.version
        || current.message != judged.message
        || !protocol_check_could_have_failed_it(&current)
        || recovery::read_receipt(state, &judged.id)? != recovery::Reading::Usable(proof.clone())
    {
        return Ok(false);
    }
    let history = state.join(format!("{}.settled.json", current.id));
    match private_create(&history) {
        Ok(mut file) => {
            serde_json::to_writer(
                &mut file,
                &serde_json::json!({
                    "settled_by": crate::version(),
                    "settled_at_unix": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |since| since.as_secs()),
                    "serving": proof,
                    "recorded": current,
                }),
            )?;
            file.sync_all()?;
        }
        // An earlier settle that stopped before the job was rewritten.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let completed = completed_presentation(current);
    write_job(state, &completed)?;
    retention::remember_success(state, &completed.id, &completed.phase);
    Ok(true)
}

/// How long a daemon of an update's target waits for the worker that
/// installed it to finish, before leaving its failure to a window.
#[cfg(feature = "host")]
const SUCCESSOR_SETTLE_PATIENCE: Duration = Duration::from_secs(30 * 60);

/// Run once by a daemon that has finished starting, serving as `serving`.
///
/// A 0.2.3 worker records its update failed after this build already took
/// over, because it checks the successor through its own older protocol
/// (#3877). The daemon of the target is the one process that can prove that
/// record wrong without a window: it waits for that worker to finish, and if
/// the worker's own verifier receipt names this daemon serving, it settles the
/// job complete. Anything else leaves the record as it is.
#[cfg(feature = "host")]
pub fn settle_as_successor(serving: crate::model::HostSessionProof) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let Ok(layout) = locate(&executable) else {
        // Not an installed build; there is no update record to settle.
        return;
    };
    let state = layout.prefix.join(".update");
    if !state.is_dir() {
        return;
    }
    let spawned = thread::Builder::new()
        .name("update-settle".into())
        .spawn(move || {
            let deadline = Instant::now() + SUCCESSOR_SETTLE_PATIENCE;
            loop {
                match successor_settle_step(&layout, &state, &serving) {
                    Ok(SettleStep::Wait) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(500));
                    }
                    Ok(SettleStep::Settled(job)) => {
                        tracing::info!(job, "settled an older worker's update failure as complete");
                        return;
                    }
                    Ok(_) => return,
                    Err(error) => {
                        tracing::warn!(%error, "could not settle the last update's record");
                        return;
                    }
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not watch the last update's record");
    }
}

#[cfg(feature = "host")]
#[derive(Debug, PartialEq, Eq)]
enum SettleStep {
    /// The worker is still running; look again.
    Wait,
    /// Nothing here for this daemon to settle.
    Done,
    Settled(String),
}

#[cfg(feature = "host")]
fn successor_settle_step(
    layout: &InstallLayout,
    state: &Path,
    serving: &crate::model::HostSessionProof,
) -> Result<SettleStep, UpdateError> {
    // The lease first: a receipt read once its worker has gone is its last.
    let worker_running = match acquire_lock(state) {
        Ok(_lease) => false,
        Err(UpdateError::Busy) => true,
        Err(error) => return Err(error),
    };
    if worker_running {
        return Ok(SettleStep::Wait);
    }
    let job: UpdateJob = match read_private(&state.join("status.json"), JOB_STATE_LIMIT) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SettleStep::Done);
        }
        Err(error) => return Err(error),
    };
    if job.phase != UpdatePhase::Failed
        || job.version != layout.version
        || !protocol_check_could_have_failed_it(&job)
        || served_version_root(layout)? != layout.version_root
    {
        return Ok(SettleStep::Done);
    }
    Ok(if settle_as_completed(state, &job, serving)? {
        SettleStep::Settled(job.id)
    } else {
        SettleStep::Done
    })
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
        && (installed_version(&job.version)
            .ok()
            .zip(installed_version(&window.version).ok())
            .is_some_and(|(target, running)| target <= running)
            // A newer build still installed here but no longer served was
            // withdrawn by selecting this one; its failure no longer applies.
            || matches!(
                Path::new(&job.version).components().collect::<Vec<_>>()[..],
                [std::path::Component::Normal(_)]
            ) && {
                let root = window.prefix.join("versions").join(&job.version);
                root.is_dir() && !is_unverified_candidate(&root)
            })
}

/// A failed job whose target is the build this window runs, served by its own
/// daemon. Its recovery receipt must still show the service was usable.
#[cfg(any(feature = "ui", test))]
fn failed_job_reached_its_target(
    job: &UpdateJob,
    window: &InstallLayout,
    served: &Path,
    daemon: Option<&InstallLayout>,
) -> bool {
    job.phase == UpdatePhase::Failed
        && job.version == window.version
        && window.version_root == served
        && daemon.is_some_and(|owner| {
            owner.prefix == window.prefix && owner.version_root == window.version_root
        })
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
            crate::install_guard::preflight_source_panes(guard, None, false).map(|_| ())
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
    // Reserve startup until readiness or a recorded failure. Success closes
    // our copy before reopening windows; recovery closes it before restarting
    // the selected version. Neither close may unlock a serving successor.
    let mut handoff = None;
    let mut survivors = floor::Survivors::default();
    let result = perform_update(
        &installation,
        &state,
        &mut job,
        &mut handoff,
        &mut survivors,
    );
    finish_worker(&state, &mut job, result, &mut survivors)
}

fn finish_worker(
    state: &Path,
    job: &mut UpdateJob,
    result: Result<Completion, UpdateError>,
    survivors: &mut floor::Survivors,
) -> Result<(), UpdateError> {
    let settled_failure = job.phase == UpdatePhase::Failed && result.is_err();
    job.phase = if matches!(result, Err(UpdateError::Cancelled)) {
        UpdatePhase::Cancelled
    } else if result.is_ok() {
        UpdatePhase::Complete
    } else {
        UpdatePhase::Failed
    };
    job.confirmation = None;
    job.refusal = result.as_ref().err().and_then(UpdateError::refusal);
    job.message = completion_message(&result, job, settled_failure);
    if !job.message.contains("Ended or stopped:") {
        job.message.push_str(&survivors.recovery_report());
    }
    if !job.message.contains("Current turns interrupted:") {
        job.message.push_str(&survivors.prepared_report());
    }
    job.message.push_str(&survivors.paused_report());
    let (remaining, unchecked) = survivors.report();
    if !remaining.is_empty() {
        job.message.push_str("\nStill running, not attached:\n");
        job.message.push_str(&remaining.join("\n"));
    }
    if !unchecked.is_empty() {
        job.message
            .push_str("\nStill running; attachment could not be checked:\n");
        job.message.push_str(&unchecked.join("\n"));
    }
    job.in_flight = survivors.in_flight_outcomes(unattached_after_update);
    if let Err(error) = write_job(state, job) {
        if result.is_ok() {
            return Err(error);
        }
        // Preserve a typed recovery request even when its human-readable job
        // status cannot be recorded. The caller still owns handoff custody.
        eprintln!("Could not record update result: {error}");
    } else {
        retention::remember_success(state, &job.id, &job.phase);
    }
    result.map(|_| ())
}

fn completion_message(
    result: &Result<Completion, UpdateError>,
    job: &UpdateJob,
    settled_failure: bool,
) -> String {
    match result {
        Ok(Completion {
            handoff,
            pane_notes,
        }) => {
            let message = match handoff {
                Handoff::NoDaemon => NO_DAEMON_INSTALLED,
                Handoff::Served => "Update installed; saved windows reopened",
            };
            match pane_notes {
                Some(notes) => format!("{message}\n\n{notes}"),
                None => message.into(),
            }
        }
        Err(_) if settled_failure => job.message.clone(),
        Err(error) => error.record_message(),
    }
}

/// What the serving daemon recorded of the conversations its restore could
/// not reattach. A record that can't be read reports none: the fates it
/// would refine are still the reviewed ones.
fn unattached_after_update() -> Vec<crate::upgrade::handoff::UnattachedSummary> {
    match crate::upgrade::handoff::Store::existing() {
        Ok(Some(store)) => store.unattached().unwrap_or_else(|error| {
            eprintln!("Could not read the conversations the update could not reattach: {error}");
            Vec::new()
        }),
        Ok(None) => Vec::new(),
        Err(error) => {
            eprintln!("Could not read the conversations the update could not reattach: {error}");
            Vec::new()
        }
    }
}

fn perform_update(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    survivors: &mut floor::Survivors,
) -> Result<Completion, UpdateError> {
    let request: UpdateRequest = serde_json::from_slice(&read_private(
        &state.join(format!("{}.request", job.id)),
        SMALL_ASSET_LIMIT,
    )?)?;
    let terminal = matches!(request, UpdateRequest::ReleaseFromTerminal(_));
    let selected = match request {
        UpdateRequest::Release(release) | UpdateRequest::ReleaseFromTerminal(release) => {
            Installation::detect()?;
            release
        }
        UpdateRequest::Installed { root } => {
            return installed::perform(installation, state, job, &root, handoff, survivors, false);
        }
        UpdateRequest::InstalledFromTerminal { root } => {
            return installed::perform(installation, state, job, &root, handoff, survivors, true);
        }
    };
    let cancellation = CancellationToken::for_job(state, &job.id)?;
    let feed = feed::Feed::configured().map_err(invalid)?;
    let release = feed.release(&selected.version, &cancellation)?;
    if release != selected || !is_newer(&release.version, &installation.version)? {
        return Err(invalid(
            "selected release changed or is no longer an upgrade; check again",
        ));
    }
    set_phase(
        state,
        job,
        UpdatePhase::Downloading,
        "Downloading the release",
    )?;
    // The signed metadata names every digest. The checksum file the installer
    // reads again must agree with it, or the two would vouch for different
    // bytes.
    let mut checksums = feed.download(&release, &release.checksums, &cancellation)?;
    let mut manifest = String::new();
    checksums.read_to_string(&mut manifest)?;
    checksums.seek(SeekFrom::Start(0))?;
    let archive_hash = release.archive.sha256;
    if checksum_for(&manifest, &release.archive.name)? != archive_hash
        || checksum_for(&manifest, &release.installer.name)? != release.installer.sha256
    {
        return Err(invalid(
            "the release's checksum file disagrees with its signed metadata",
        ));
    }
    let installer = feed.download(&release, &release.installer, &cancellation)?;
    let archive = feed.download(&release, &release.archive, &cancellation)?;
    set_phase(
        state,
        job,
        UpdatePhase::Verifying,
        "Verifying the downloaded release",
    )?;
    let daemon_digest = archive_daemon_digest(&release, &archive, &cancellation)?;
    let staged = candidate::Staged::from_archive(&release, &archive, &archive_hash, &cancellation)?;
    let (source_protocol, _) = source::inspect_identity(None)?;
    if staged.identity.protocol == source_protocol {
        staged.verify_compatible_handshake(&cancellation)?;
    }
    let inspection = reconcile_saved_sources(staged.identity.protocol, false)?;
    let context = ReviewContext::capture(installation)?;
    let mut turns = source::TurnReview::default();
    let mut observe = |policy| {
        Installation::detect()?;
        turns.apply(observe_impact(&staged.identity, policy)?)
    };
    let confirmed = confirmation::wait(state, job, terminal, |policy| {
        Ok(context.apply(observe(policy)?.with_inspection(&inspection)))
    })?;
    if confirmed.replaces_daemon {
        // Recovery may need to carry high-numbered descriptors through the
        // release installer. Establish its shell support before the source
        // receives any shutdown request.
        preflight_recovery_shell(Path::new("/bin/bash"))?;
        preflight_candidate_preparation(&installer)?;
    }
    let mut recovery = recovery::Context::default();
    recovery.capture(state, job)?;
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing all application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    confirmation::check_cancelled(state, job)?;
    let windows = guard.prepare_windows(Duration::from_secs(45))?;
    // The build these windows run, to return to if the new one cannot open.
    let previous = installation.prefix.join("current").canonicalize()?;
    // The daemon serving these windows; one still serving after the install
    // is one they already spoke to, whichever version directory it runs from.
    let daemon_before = crate::daemon::running_executable().ok().flatten();
    {
        let inspected = crate::install_guard::preflight_source_panes(
            &guard,
            confirmed
                .daemon
                .as_ref()
                .map(|daemon| daemon.capability.proof.clone()),
            confirmed.replaces_daemon,
        )?;
        let latest = observe(
            confirmed
                .daemon
                .as_ref()
                .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy),
        )
        .map_err(io::Error::other)?
        .with_inspection(&inspected);
        if !confirmed.preserves_consent(&context.apply(latest)) {
            return Err(invalid(
                "Session ownership changed during pane inspection; existing windows were kept open",
            ));
        }
    }
    let mut install_result = (|| {
        if confirmed.replaces_daemon {
            // A failed prepare can strand an immutable Prepared checkpoint.
            // Retain the complete verified recovery owner before asking the
            // source to stop; this phase never changes current or clients.
            run_installer(
                installation,
                &release,
                &installer,
                &archive,
                &checksums,
                true,
                &guard,
                None,
                &job.id,
                survivors,
                true,
            )?;
        }
        recovery.shutdown_requested = confirmed.replaces_daemon;
        if let Some(daemon) = &confirmed.daemon
            && recovery.census.as_ref().and_then(|c| c.source.as_ref())
                != Some(&daemon.capability.proof)
        {
            return Err(invalid(
                "The census belongs to a different source; shutdown was not requested",
            ));
        }
        floor::quiesce(
            state,
            job,
            &confirmed,
            terminal,
            |policy| Ok(context.apply(observe(policy)?.with_inspection(&inspection))),
            handoff,
            survivors,
            &guard,
        )?;
        let target_daemon = installation
            .prefix
            .join("versions")
            .join(&release.version)
            .join("bin/cagentsd");
        if confirmed.replaces_daemon {
            survivors.authorize_successor(
                &target_daemon,
                Some(&daemon_digest),
                handoff.as_ref(),
            )?;
        }
        confirmation::prepared(state, job)?;
        guard.close_prepared_windows(&windows, Duration::from_secs(45))?;
        Installation::detect()?;
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
            confirmed.replaces_daemon,
            &guard,
            handoff.as_ref(),
            &job.id,
            survivors,
            false,
        )?;
        Ok(())
    })();
    announce_primary_failure(&install_result);
    let recovery_result = finish_handoff(
        installation,
        state,
        job,
        &mut install_result,
        handoff,
        &recovery,
        survivors,
        &guard,
    );
    // Do not let a recovery UI autostart the frozen previous executable while
    // ownership or Prepared recovery is unresolved.
    recovery_result?;
    let slots = guard.reopened_slots();
    let reopen_result = guard
        .relaunch_closed_windows(&installation.prefix.join("current/bin/cagents-ui"))
        .map_err(UpdateError::from);
    // A build that cannot open a single window must not leave the person
    // with none. While the daemon is still the previous build's, that build
    // matches it, so it takes the installation back and reopens the windows.
    let restored = match &reopen_result {
        Err(error)
            if restores_previous_build(
                install_result.is_ok(),
                confirmed.replaces_daemon,
                &slots,
                daemon_before.as_deref(),
                crate::daemon::running_executable()
                    .ok()
                    .flatten()
                    .as_deref(),
            ) =>
        {
            eprintln!(
                "cagents {} could not open its window: {error}",
                release.version
            );
            let candidate = installation.prefix.join("versions").join(&release.version);
            Some(restore_previous_build(
                &installation.prefix,
                &candidate,
                &previous,
                &slots,
            ))
        }
        _ => None,
    };
    let handoff_kind = verify_reopened_service(
        &recovery,
        state,
        job,
        install_result.is_ok().then_some(&*survivors),
    )?;
    let (new, old) = (&release.version, &installation.version);
    match restored {
        Some(Ok(Ok(()))) => {
            // The phase in progress no longer describes anything.
            job.message.clear();
            return report_recovery_failure(
                state,
                job,
                invalid(format!(
                    "cagents {new} could not start, so cagents {old} was put back and your windows reopened with it. Agents and terminals kept running."
                )),
            );
        }
        Some(Ok(Err(error))) => {
            eprintln!("cagents {old} could not reopen the windows: {error}");
            job.message.clear();
            return report_recovery_failure(
                state,
                job,
                invalid(format!(
                    "cagents {new} could not start, so cagents {old} was put back, but its windows did not reopen either. Run `cagents ui` to open them. Agents and terminals kept running."
                )),
            );
        }
        Some(Err(error)) => eprintln!("Could not put back cagents {old}: {error}"),
        None => (),
    }
    finish_reopening_windows(state, job, install_result, handoff_kind, reopen_result)
}

fn finish_reopening_windows(
    state: &Path,
    job: &mut UpdateJob,
    install_result: Result<(), UpdateError>,
    handoff: Handoff,
    reopened: Result<Option<String>, UpdateError>,
) -> Result<Completion, UpdateError> {
    let pane_notes = match reopened {
        Ok(notes) => notes,
        Err(error) => {
            if install_result.is_ok() {
                job.message.clear();
            }
            return report_recovery_failure(
                state,
                job,
                invalid(format!(
                    "Some saved windows did not reopen: {error}. Run `cagents ui` to reopen the saved workspace."
                )),
            );
        }
    };
    install_result.map(|()| Completion {
        handoff,
        pane_notes,
    })
}

/// Whether an installed update whose windows did not reopen goes back to the
/// build those windows ran: only when it installed, closed windows that all
/// failed to reopen, and the daemon that served them still serves. A daemon
/// the update started, even at the same protocol, may have moved the store on.
fn restores_previous_build(
    installed: bool,
    replaced_daemon: bool,
    slots: &[String],
    daemon_before: Option<&Path>,
    daemon_after: Option<&Path>,
) -> bool {
    installed
        && !replaced_daemon
        && !slots.is_empty()
        && daemon_before.is_some()
        && daemon_before == daemon_after
}

/// Point the installation back at `previous` and reopen `slots` with its
/// application, unless a window of the new build did open after all. The
/// outer error means the installation was left as it was; the inner one that
/// it was put back but the windows did not reopen.
fn restore_previous_build(
    prefix: &Path,
    candidate: &Path,
    previous: &Path,
    slots: &[String],
) -> Result<io::Result<()>, UpdateError> {
    // A window or daemon starting this moment holds the launch fence briefly.
    let deadline = Instant::now() + Duration::from_secs(2);
    let guard = loop {
        match crate::instance::UiUpdateGuard::acquire() {
            Ok(guard) => break guard,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    };
    if guard.has_running_windows()? {
        return Err(invalid("a window of the new build is open"));
    }
    installed::restore_current(prefix, &candidate.canonicalize()?, previous)?;
    Ok(guard.relaunch_windows(slots, &previous.join("bin/cagents-ui")))
}

fn verify_reopened_service(
    recovery: &recovery::Context,
    state: &Path,
    job: &mut UpdateJob,
    successor: Option<&floor::Survivors>,
) -> Result<Handoff, UpdateError> {
    let census = recovery
        .census
        .as_ref()
        .ok_or_else(|| invalid("No pre-update census exists; recovery readiness is unverified"))?;
    let absence = (!recovery.shutdown_requested && census.source.is_none())
        .then(|| census.verify_absence().ok())
        .flatten();
    let receipt = if let Some(absence) = absence {
        absence
    } else {
        census.verify_with_target_retiring(
            &state
                .parent()
                .ok_or_else(|| invalid("Update state has no installation root"))?
                .join("versions")
                .join(&job.version),
            state,
            &job.id,
        )?
    };
    receipt.save(&recovery::outcome_path(state, &job.id))?;
    if matches!(receipt.outcome, recovery::Outcome::NoSource) {
        Ok(Handoff::NoDaemon)
    } else if receipt.usable() {
        if let Some(successor) = successor
            && let Err(error) = successor.confirm_successor(&receipt)
        {
            return report_recovery_failure(state, job, error);
        }
        Ok(Handoff::Served)
    } else {
        report_recovery_failure(state, job, receipt.failure())
    }
}

struct ReviewContext {
    from_version: String,
    windows: usize,
}

impl ReviewContext {
    fn capture(installation: &Installation) -> Result<Self, UpdateError> {
        let guard = crate::instance::UiUpdateGuard::acquire()?;
        let windows = guard.verified_windows()?;
        let layout = locate(&std::env::current_exe()?)?;
        let from_version = installed_daemon_owner(&layout)?
            .map(|owner| owner.version)
            .or_else(|| {
                windows
                    .first()
                    .and_then(|window| locate(&window.executable).ok().map(|layout| layout.version))
            })
            .unwrap_or_else(|| installation.version.clone());
        Ok(Self {
            from_version,
            windows: windows.len(),
        })
    }

    fn apply(&self, mut preview: impact::Preview) -> impact::Preview {
        preview.from_version = self.from_version.clone();
        preview.windows = self.windows;
        preview
    }
}

/// Invoke only the matched host implementation: SQLite stays out of the CLI
/// graph, and an older helper must refuse its unknown option before admission.
pub(crate) fn inspect_source_panes(
    guard: &crate::instance::UiUpdateGuard,
    proof: Option<crate::model::HostSessionProof>,
    require_resumable: bool,
) -> Result<source_inspection::SourceInspection, UpdateError> {
    inspect_source_panes_mode(guard, proof, require_resumable, false)
}

// Recover only already-owned providers before publishing the inventory the
// user will consent to. Never expand consent after windows have prepared.
fn reconcile_saved_sources(
    destination_protocol: u16,
    replaces_compatible_daemon: bool,
) -> Result<source_inspection::SourceInspection, UpdateError> {
    let guard = crate::instance::UiUpdateGuard::acquire()?;
    let (protocol, proof) = source::inspect_identity(None)?;
    inspect_source_panes_mode(
        &guard,
        proof,
        protocol != destination_protocol || replaces_compatible_daemon,
        true,
    )
}

fn inspect_source_panes_mode(
    guard: &crate::instance::UiUpdateGuard,
    proof: Option<crate::model::HostSessionProof>,
    require_resumable: bool,
    reconcile_retained: bool,
) -> Result<source_inspection::SourceInspection, UpdateError> {
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
        JOB_STATE_LIMIT,
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
    let result: Result<source_inspection::SourceInspection, String> =
        serde_json::from_reader(&mut output)?;
    let inspection = result.map_err(invalid)?;
    let (current_protocol, current_proof) = source::inspect_identity(request.proof.as_ref())?;
    if current_protocol != request.protocol || current_proof != request.proof {
        return Err(invalid(
            "Source ownership changed during pane inspection; source windows remain usable",
        ));
    }
    Ok(inspection)
}

/// Probe only a fixed, owned companion beside an authenticated editor process.
/// No daemon startup command or inherited installer authority is passed.
pub(crate) fn companion_protocol(executable: &Path) -> Result<u16, UpdateError> {
    owned_cli_protocol(
        executable,
        CEDITOR_COMPANION,
        "editor companion protocol check",
    )
}

const CEDITOR_COMPANION: &str = "Ceditor's companion";

fn owned_cli_protocol(
    executable: &Path,
    subject: &str,
    operation: &'static str,
) -> Result<u16, UpdateError> {
    let envelope = companion_read(
        executable,
        &["--json", "daemon", "protocol-version"],
        None,
        subject,
        operation,
    )?;
    envelope
        .get("data")
        .and_then(|data| data.get("protocol_version"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u16::try_from(version).ok())
        .filter(|version| *version > 0)
        .ok_or_else(|| invalid(format!("{subject} did not report a valid protocol")))
}

pub(crate) fn companion_identity(
    executable: &Path,
) -> Result<(String, Option<String>), UpdateError> {
    // `daemon status` reports the compiled instance even when stopped. Give it
    // a fresh nonexistent runtime, so it cannot contact production or start a
    // service. This also works with older unmarked companion sets.
    let isolated = std::env::temp_dir().join(format!(
        "cagents-namespace-inspect-{}",
        uuid::Uuid::new_v4()
    ));
    let envelope = companion_read(
        executable,
        &["--json", "daemon", "status"],
        Some(&isolated),
        CEDITOR_COMPANION,
        "editor companion instance check",
    )?;
    match envelope
        .get("data")
        .and_then(|data| data.get("instance"))
        .and_then(serde_json::Value::as_str)
    {
        Some(instance @ ("production" | "development")) => Ok((
            instance.into(),
            envelope
                .get("data")
                .and_then(|data| data.get("cli_version"))
                .and_then(serde_json::Value::as_str)
                .filter(|version| !version.is_empty())
                .map(str::to_owned),
        )),
        _ => Err(invalid(
            "Ceditor's companion has no verifiable application instance",
        )),
    }
}

fn companion_read(
    executable: &Path,
    arguments: &[&str],
    runtime: Option<&Path>,
    subject: &str,
    operation: &'static str,
) -> Result<serde_json::Value, UpdateError> {
    let before = fs::symlink_metadata(executable)?;
    if !before.is_file()
        || !crate::install_guard::trusted_executable_owner(before.uid())
        || before.mode() & 0o022 != 0
    {
        return Err(invalid(format!("{subject} is not an owned executable")));
    }
    let mut command = Command::new(executable);
    crate::install_guard::sanitize_child_environment(&mut command);
    command.args(arguments);
    if let Some(runtime) = runtime {
        command.env("XDG_RUNTIME_DIR", runtime);
    }
    let mut output = bounded_command(
        &mut command,
        operation,
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
        return Err(invalid(format!("{subject} changed during preflight")));
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
        from_version: String::new(),
        windows: 0,
        stopped: Vec::new(),
        stopped_windows: Vec::new(),
        labels: Default::default(),
        editors: crate::install_guard::editor_companions(),
        candidate: candidate.clone(),
        replaces_daemon,
        daemon,
    })
}

use handoff::wait_daemon_stopped;

/// Recovery windows may autostart the old version. Publish failure before
/// allowing that start, rather than keeping the runtime reserved until reopen.
fn record_handoff_failure(
    state: &Path,
    job: &mut UpdateJob,
    result: &Result<(), UpdateError>,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    recovery_report: &str,
    prepared_report: &str,
) -> Result<(), UpdateError> {
    if let Err(error) = result {
        record_primary_failure(job, error, recovery_report, prepared_report);
        write_job(state, job)?;
        drop(handoff.take());
    }
    Ok(())
}

/// Print why the update failed, before any rollback or recovery text, so the
/// log always leads with the cause. Call once, where the result is settled.
fn announce_primary_failure(result: &Result<(), UpdateError>) {
    if let Err(error) = result {
        eprintln!("{error}");
    }
}

/// Lead the job's message with the failure that stopped the update. Recovery
/// reports are appended to it and never replace it.
fn record_primary_failure(
    job: &mut UpdateJob,
    error: &UpdateError,
    recovery_report: &str,
    prepared_report: &str,
) {
    job.phase = if matches!(error, UpdateError::Cancelled) {
        UpdatePhase::Cancelled
    } else {
        UpdatePhase::Failed
    };
    job.confirmation = None;
    job.refusal = error.refusal();
    job.message = error.record_message();
    if !job.message.contains("Ended or stopped:") {
        job.message.push_str(recovery_report);
    }
    if !job.message.contains("Current turns interrupted:") {
        job.message.push_str(prepared_report);
    }
}

/// Settle the reservation before reopening clients. The worker, including a
/// headless update with no windows, owns restarting the selected installation.
fn finish_handoff(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    result: &mut Result<(), UpdateError>,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    recovery: &recovery::Context,
    survivors: &mut floor::Survivors,
    guard: &crate::instance::UiUpdateGuard,
) -> Result<Handoff, UpdateError> {
    let needed =
        recovery.shutdown_requested || handoff.is_some() || survivors.needs_daemon_recovery();
    if let Err(error) = result {
        record_primary_failure(
            job,
            error,
            &survivors.recovery_report(),
            &survivors.prepared_report(),
        );
    }
    let Some(census) = recovery.census.as_ref() else {
        if needed || result.is_ok() {
            return report_recovery_failure(
                state,
                job,
                invalid(
                    "No pre-update terminal census exists. Readiness and surviving terminals are unverified. Keep saved panes and inspect `cagents --no-start terminal list` before retrying.",
                ),
            );
        }
        record_handoff_failure(
            state,
            job,
            result,
            handoff,
            &survivors.recovery_report(),
            &survivors.prepared_report(),
        )?;
        return Ok(Handoff::Served);
    };
    let outcome_path = recovery::outcome_path(state, &job.id);
    let target = installation.prefix.join("versions").join(&job.version);
    // Persistence is subordinate to restoring service. Keep recording errors
    // separate so disk-full or an unwritable job directory cannot suppress the
    // one permitted start after shutdown has already been requested.
    let mut recording_errors = Vec::new();
    let mut verified = probe_recovery(census, &target, state, &job.id, &mut recording_errors)?;
    if !needed
        && census.source.is_none()
        && let Ok(absence) = census.verify_absence()
    {
        // A file-only installation did not remove a daemon. Verified absence
        // is a separate outcome, never readiness or preservation of a service.
        recording_errors.extend(absence.save(&outcome_path).err().map(|e| e.to_string()));
        let recorded = record_handoff_failure(
            state,
            job,
            result,
            handoff,
            &survivors.recovery_report(),
            &survivors.prepared_report(),
        );
        recording_errors.extend(recorded.err().map(|e| e.to_string()));
        if result.is_ok() {
            job.message = NO_DAEMON_INSTALLED.into();
            recording_errors.extend(write_job(state, job).err().map(|e| e.to_string()));
        }
        return finish_recording_recovery(state, job, recording_errors).map(|()| Handoff::NoDaemon);
    }
    if verified.usable() {
        let saved = verified.save(&outcome_path);
        let recorded = record_handoff_failure(
            state,
            job,
            result,
            handoff,
            &survivors.recovery_report(),
            &survivors.prepared_report(),
        );
        drop(handoff.take());
        survivors.release_recovery_reservation();
        recording_errors.extend(saved.err().map(|e| e.to_string()));
        recording_errors.extend(recorded.err().map(|e| e.to_string()));
        if require_successor(job, result, survivors, &verified) {
            recording_errors.extend(write_job(state, job).err().map(|e| e.to_string()));
        }
        return finish_recording_recovery(state, job, recording_errors).map(|()| Handoff::Served);
    }
    if needed {
        let permission = match census.inspect_restart() {
            Ok(p) => p,
            Err(error) => {
                let receipt = census.unverified(error.to_string())?;
                if let Err(error) = receipt.save(&outcome_path) {
                    eprintln!("Could not save recovery proof: {error}");
                }
                return report_recovery_failure(state, job, receipt.failure());
            }
        };
        if let Some(mut required) = permission.target_required() {
            // This worker cannot consume Prepared. Hand custody back for the
            // explicit target recovery command, closing only our duplicates;
            // never LOCK_UN an open-file description shared with a daemon.
            drop(handoff.take());
            survivors.release_recovery_reservation();
            if let Err(error) = permission.save(&outcome_path) {
                eprintln!("Could not save target recovery request: {error}");
            }
            let original = recovery::census_path(state, &job.id);
            match recovery::pending_command_scope(&original) {
                Ok(Some(scope)) => {
                    if scope.pending.checkpoint_hash != required.checkpoint_digest
                        || scope.pending.source != required.source
                    {
                        return report_recovery_failure(
                            state,
                            job,
                            invalid(
                                "The Prepared checkpoint changed before target recovery; keep the retained evidence and inspect it before recovering",
                            ),
                        );
                    }
                    required.recovery_command = Some(
                        scope
                            .pending
                            .command(&target.join("bin/cagents"), scope.original),
                    );
                }
                Ok(None) => {
                    return report_recovery_failure(
                        state,
                        job,
                        invalid(
                            "The Prepared checkpoint disappeared before target recovery; inspect the retained update evidence before restarting",
                        ),
                    );
                }
                Err(error) => {
                    return report_recovery_failure(
                        state,
                        job,
                        invalid(format!(
                            "The original recovery census could not be retained: {error}. Keep {} and the Prepared checkpoint; the previous build was not restarted",
                            original.display()
                        )),
                    );
                }
            }
            return report_recovery_failure(
                state,
                job,
                UpdateError::TargetRecoveryRequired(Box::new(required)),
            );
        }
    }
    // Protocol rejection or a failed verifier is not proof that the socket is
    // dead. Preserve any listener rather than starting a competing daemon.
    let socket_has_listener = crate::terminal_ipc::daemon_socket_path()
        .ok()
        .is_some_and(|p| std::os::unix::net::UnixStream::connect(p).is_ok());
    if verified.has_listener() || socket_has_listener {
        if let Err(error) = verified.save(&outcome_path) {
            eprintln!("Could not save recovery proof: {error}");
        }
        return report_recovery_failure(state, job, verified.failure());
    }
    if needed {
        if let Err(error) = survivors.require_normal_restart() {
            return report_recovery_failure(state, job, error);
        }
        // The restarted daemon refuses to serve beside a half-exited window.
        // Settle windows while this job still holds the startup reservation.
        let settled = guard.settle_committed_windows();
        if let Err(error) = &settled
            && error.kind() != io::ErrorKind::Interrupted
        {
            eprintln!("Restarting the terminal service, but windows could not be checked: {error}");
        }
        let recorded = record_handoff_failure(
            state,
            job,
            result,
            handoff,
            &survivors.recovery_report(),
            &survivors.prepared_report(),
        );
        drop(handoff.take());
        survivors.release_recovery_reservation();
        if let Err(error) = settled
            && error.kind() == io::ErrorKind::Interrupted
        {
            if let Err(record_error) = recorded {
                eprintln!("Could not record the update failure: {record_error}");
            }
            return report_recovery_failure(state, job, invalid(error.to_string()));
        }
        let mut command = Command::new(installation.prefix.join("current/bin/cagents"));
        command.args(["daemon", "start"]);
        crate::install_guard::sanitize_child_environment(&mut command);
        unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
        guard.inherit_for_install(&mut command)?;
        let started = crate::install_guard::wait_daemon_start(
            &mut command,
            crate::install_guard::daemon_start_timeout()?,
        );
        verified = probe_recovery(census, &target, state, &job.id, &mut recording_errors)?;
        recording_errors.extend(recorded.err().map(|e| e.to_string()));
        recording_errors.extend(verified.save(&outcome_path).err().map(|e| e.to_string()));
        if !verified.usable() {
            return report_recovery_failure(state, job, verified.failure());
        }
        if let Err(error) = started {
            return report_recovery_failure(
                state,
                job,
                invalid(format!(
                    "The daemon is Ready and its original inventory was verified, but its startup command reported failure: {error}. Run `cagents --no-start daemon status` and inspect the update log before retrying.",
                    error = error.error,
                )),
            );
        }
        require_successor(job, result, survivors, &verified);
        job.message.push_str(
            " The terminal service is ready and its original terminal inventory was verified.",
        );
        recording_errors.extend(write_job(state, job).err().map(|e| e.to_string()));
        finish_recording_recovery(state, job, recording_errors).map(|()| Handoff::Served)
    } else {
        if let Err(error) = verified.save(&outcome_path) {
            eprintln!("Could not save recovery proof: {error}");
        }
        report_recovery_failure(state, job, verified.failure())
    }
}

/// Whether the daemon serving after a successful install failed to prove it
/// is the successor this update selected. That is an install failure, as it
/// was when the installer checked it: `result` becomes that failure and the
/// job records it, and the caller still reopens windows from `current`, the
/// build matched to the serving daemon. Returning early instead would leave
/// them to the update guard, which reopens this worker's own, older build.
fn require_successor(
    job: &mut UpdateJob,
    result: &mut Result<(), UpdateError>,
    survivors: &mut floor::Survivors,
    verified: &recovery::Receipt,
) -> bool {
    if result.is_err() {
        return false;
    }
    let Err(error) = survivors.verify_successor(verified) else {
        return false;
    };
    // Lead the log with the cause, as for any other install failure.
    eprintln!("{error}");
    record_primary_failure(
        job,
        &error,
        &survivors.recovery_report(),
        &survivors.prepared_report(),
    );
    *result = Err(error);
    true
}

fn probe_recovery(
    census: &recovery::Census,
    target: &Path,
    state: &Path,
    job_id: &str,
    errors: &mut Vec<String>,
) -> Result<recovery::Receipt, UpdateError> {
    match census.verify_with_target(target, state, job_id) {
        Ok(receipt) => Ok(receipt),
        Err(error) => {
            errors.push(error.to_string());
            census.unobserved(&error)
        }
    }
}

fn finish_recording_recovery(
    state: &Path,
    job: &mut UpdateJob,
    errors: Vec<String>,
) -> Result<(), UpdateError> {
    if errors.is_empty() {
        return Ok(());
    }
    report_recovery_failure(
        state,
        job,
        invalid(format!(
            "The daemon is Ready and its original inventory was verified, but the update could not record recovery: {}. Run `cagents --no-start daemon status` and `cagents --no-start terminal list`; repair the update-state directory permissions or free disk space before retrying.",
            errors.join("; ")
        )),
    )
}

fn report_recovery_failure<T>(
    state: &Path,
    job: &mut UpdateJob,
    error: UpdateError,
) -> Result<T, UpdateError> {
    job.phase = UpdatePhase::Failed;
    job.confirmation = None;
    if !job.message.is_empty() {
        job.message.push(' ');
    }
    job.message.push_str(&error.to_string());
    if let Err(record_error) = write_job(state, job) {
        eprintln!("Could not record update failure: {record_error}. {error}");
    }
    Err(error)
}

fn archive_daemon_digest(
    release: &Release,
    archive: &File,
    cancellation: &CancellationToken,
) -> Result<String, UpdateError> {
    let archive_fd = archive.as_raw_fd();
    let mut extract = Command::new("tar");
    extract
        .args(["-xOz", "--file"])
        .arg(format!("/proc/self/fd/{archive_fd}"))
        .arg("--")
        .arg(format!("cagents-{}/bin/cagentsd", release.version));
    candidate::inherit(&mut extract, archive_fd);
    let mut daemon = bounded_command(
        &mut extract,
        "candidate daemon extraction",
        512 * 1024 * 1024,
        Duration::from_secs(120),
        cancellation,
    )?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = daemon.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn preflight_recovery_shell(shell: &Path) -> Result<(), UpdateError> {
    let mut command = Command::new(shell);
    crate::install_guard::sanitize_child_environment(&mut command);
    command
        .args([
            "--noprofile",
            "--norc",
            "-p",
            "-c",
            "[[ -n $BASH_VERSION ]] || exit 1; exec 189</dev/null 193</dev/null 194</dev/null 195</dev/null 196</dev/null; exec 189<&- 193<&- 194<&- 195<&- 196<&-",
        ])
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .stdin(Stdio::null());
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    bounded_command(
        &mut command,
        "Checking the recovery installer shell",
        4096,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .map(|_| ())
    .map_err(|error| {
        invalid(format!(
            "The update needs /bin/bash to preserve recovery ownership: {error}. The source daemon and panes are still running; nothing was changed in the installation"
        ))
    })
}

fn preflight_candidate_preparation(installer: &File) -> Result<(), UpdateError> {
    let fd = installer.as_raw_fd();
    let mut command = Command::new("/bin/bash");
    crate::install_guard::sanitize_child_environment(&mut command);
    command
        .args(["--noprofile", "--norc", "-p"])
        .arg(format!("/proc/self/fd/{fd}"))
        .arg("--supports-prepared-candidate")
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .stdin(Stdio::null());
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    candidate::inherit(&mut command, fd);
    bounded_command(
        &mut command, "Checking retained candidate support", 4096,
        Duration::from_secs(10), &CancellationToken::new(),
    ).map(|_| ()).map_err(|error| invalid(format!(
        "This release installer cannot retain a recovery target before shutdown: {error}. The source daemon and panes are still running; choose a release with prepared-candidate support"
    )))
}

fn run_installer(
    installation: &Installation,
    release: &Release,
    installer: &File,
    archive: &File,
    checksums: &File,
    restart_daemon: bool,
    guard: &crate::instance::UiUpdateGuard,
    handoff: Option<&crate::daemon::lease_handoff::LeaseHandoff>,
    job_id: &str,
    survivors: &floor::Survivors,
    prepare_candidate: bool,
) -> Result<(), UpdateError> {
    // Keep sources above the destinations so remapping cannot overwrite a
    // still-needed input, even in a worker with many open descriptors.
    let files: Vec<File> = [installer, archive, checksums]
        .into_iter()
        .map(|file| {
            let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { File::from_raw_fd(fd) })
        })
        .collect::<io::Result<_>>()?;
    let inherited_files = [190, 191, 192];
    let mut command = Command::new(if restart_daemon {
        "/bin/bash"
    } else {
        "/bin/sh"
    });
    if restart_daemon {
        command.args(["--noprofile", "--norc", "-p"]);
    }
    command
        .arg(format!("/proc/self/fd/{}", inherited_files[0]))
        .args(["--version", &release.version, "--prefix"])
        .arg(&installation.prefix)
        .env(
            "CAGENTS_ARCHIVE",
            format!("/proc/self/fd/{}", inherited_files[1]),
        )
        .env(
            "CAGENTS_CHECKSUMS",
            format!("/proc/self/fd/{}", inherited_files[2]),
        )
        .env("CAGENTS_REPOSITORY", REPOSITORY)
        .env(
            recovery::CENSUS_ENV,
            recovery::census_path(&installation.prefix.join(".update"), job_id),
        )
        .env(
            recovery::OUTCOME_ENV,
            recovery::outcome_path(&installation.prefix.join(".update"), job_id),
        )
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
    if prepare_candidate {
        command.args(["--prepare-candidate", job_id]);
    } else if restart_daemon {
        command.args(["--prepared-candidate", job_id]);
        command.arg("--restart-daemon");
    }
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    guard.inherit_for_install(&mut command)?;
    // SAFETY: descriptor flags only; sealed anonymous files cannot be replaced
    // between checksum verification and the installer's own second verification.
    unsafe {
        command.pre_exec(move || {
            for (fd, target) in files.iter().zip(inherited_files) {
                if libc::dup2(fd.as_raw_fd(), target) == -1
                    || libc::fcntl(target, libc::F_SETFD, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    if let Some(handoff) = handoff {
        handoff.inherit_for_installer(&mut command)?;
    }
    survivors.inherit_recovery(&mut command)?;
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
fn acquire_lock(state: &Path) -> Result<crate::instance::OwnedLock, UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state.join("lock"))?;
    validate_lock(&file)?;
    lock_file(file)
}

fn lock_file(file: File) -> Result<crate::instance::OwnedLock, UpdateError> {
    crate::instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            UpdateError::Busy
        } else {
            error.into()
        }
    })
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
fn inherited_lock(state: &Path) -> Result<crate::instance::OwnedLock, UpdateError> {
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
    let lock = lock_file(file)?;
    if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(lock)
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
                    return Err(UpdateError::Exited { operation, status });
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
    fn only_an_update_that_kept_the_daemon_puts_the_previous_build_back() {
        // A daemon kept by earlier updates still runs from its own version.
        let kept = Some(Path::new("/install/versions/0.9.0/bin/cagentsd"));
        let started = Some(Path::new("/install/versions/1.0.1/bin/cagentsd"));
        let slots = ["primary".to_owned()];
        assert!(restores_previous_build(true, false, &slots, kept, kept));
        // The installer started a daemon of the new build: the previous
        // application may not speak to it, and it may have moved the store on.
        assert!(!restores_previous_build(true, false, &slots, kept, started));
        assert!(!restores_previous_build(true, false, &slots, None, started));
        assert!(!restores_previous_build(true, false, &slots, None, None));
        assert!(!restores_previous_build(true, false, &slots, kept, None));
        assert!(!restores_previous_build(true, true, &slots, kept, kept));
        assert!(!restores_previous_build(false, false, &slots, kept, kept));
        // Windows that never closed are reopened by the guard itself.
        assert!(!restores_previous_build(true, false, &[], kept, kept));
    }

    #[test]
    fn recovery_installer_shell_requires_high_descriptor_support_before_shutdown() {
        preflight_recovery_shell(Path::new("/bin/bash")).unwrap();
        for shell in ["/bin/false", "/missing-cagents-recovery-shell"] {
            let error = preflight_recovery_shell(Path::new(shell)).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("source daemon and panes are still running"));
            assert!(message.contains("nothing was changed in the installation"));
        }
    }

    #[test]
    fn replacement_requires_retained_candidate_support_before_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let installer = root.path().join("install.sh");
        fs::write(
            &installer,
            b"#!/bin/sh\n[ \"$1\" = --supports-prepared-candidate ]\n",
        )
        .unwrap();
        preflight_candidate_preparation(&File::open(&installer).unwrap()).unwrap();
        fs::write(&installer, b"#!/bin/sh\nexit 2\n").unwrap();
        let error = preflight_candidate_preparation(&File::open(&installer).unwrap()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("source daemon and panes are still running")
        );
        assert!(error.to_string().contains("prepared-candidate support"));
    }

    #[test]
    fn review_prompt_defaults_to_update_but_eof_cancels() {
        let mut output = Vec::new();
        assert!(matches!(
            read_review_choice(&mut &b"\n"[..], &mut output, false, false).unwrap(),
            OrdinaryChoice::Confirm
        ));
        assert_eq!(String::from_utf8(output).unwrap(), "Update now? [Y/n] ");
        let mut output = Vec::new();
        assert!(matches!(
            read_review_choice(&mut &b"i\n"[..], &mut output, true, false).unwrap(),
            OrdinaryChoice::InterruptNow
        ));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Enter: update when they finish   i: interrupt them now   n: cancel\n"
        );
        assert!(matches!(
            read_review_choice(&mut &b""[..], &mut Vec::new(), false, false).unwrap(),
            OrdinaryChoice::Cancel
        ));
        assert!(matches!(
            read_review_choice(&mut &b"n\n"[..], &mut Vec::new(), true, false).unwrap(),
            OrdinaryChoice::Cancel
        ));
        assert!(matches!(
            read_review_choice(&mut &b"maybe\ny\n"[..], &mut Vec::new(), false, false).unwrap(),
            OrdinaryChoice::Confirm
        ));
    }

    #[test]
    fn unattended_ordinary_review_cannot_approve_without_yes() {
        assert_eq!(
            ordinary_approval(false, false),
            Some(OrdinaryChoice::Cancel)
        );
        assert_eq!(
            ordinary_approval(true, false),
            Some(OrdinaryChoice::Confirm)
        );
        assert_eq!(ordinary_approval(false, true), None);
        assert_eq!(ordinary_approval(true, true), Some(OrdinaryChoice::Confirm));
    }

    #[test]
    fn app_release_stop_instructions_select_the_same_release_without_staging() {
        let directory = tempfile::tempdir().unwrap();
        let release = release("9.8.7");
        let job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: release.version.clone(),
            phase: UpdatePhase::AwaitingConfirmation,
            message: String::new(),
            log_path: directory.path().join("worker.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let file = private_create(&directory.path().join(format!("{}.request", job.id))).unwrap();
        serde_json::to_writer(file, &UpdateRequest::Release(release)).unwrap();
        assert_eq!(
            floor::terminal_confirmation(directory.path(), &job).unwrap(),
            "Stopping live work needs confirmation in a terminal. Run `cagents daemon upgrade --version 9.8.7` from a system terminal outside cagents."
        );
    }

    #[test]
    fn recovery_daemon_digest_uses_the_exact_verified_archive_member() {
        let directory = tempfile::tempdir().unwrap();
        let release = release("9.8.7");
        let package = format!("cagents-{}", release.version);
        let bin = directory.path().join(&package).join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("cagentsd"), b"verified candidate daemon").unwrap();
        fs::write(bin.join("cagents"), b"different CLI bytes").unwrap();
        let archive = directory.path().join("release.tar.gz");
        assert!(
            Command::new("tar")
                .args(["-czf"])
                .arg(&archive)
                .arg("-C")
                .arg(directory.path())
                .arg(&package)
                .status()
                .unwrap()
                .success()
        );
        let archive = File::open(archive).unwrap();
        assert_eq!(
            archive_daemon_digest(&release, &archive, &CancellationToken::new()).unwrap(),
            format!("{:x}", Sha256::digest(b"verified candidate daemon"))
        );
        let mut absent = release;
        absent.version = "9.8.8".into();
        assert!(archive_daemon_digest(&absent, &archive, &CancellationToken::new()).is_err());
    }

    #[test]
    fn working_floor_review_waits_before_the_destructive_list() {
        let mut output = Vec::new();
        assert_eq!(
            read_review_choice(&mut b"n\n".as_slice(), &mut output, true, true).unwrap(),
            OrdinaryChoice::Cancel
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Enter: review stops when they finish   i: interrupt them now   n: cancel\n"
        );
    }

    #[test]
    fn review_enter_proceeds_and_interrupt_requires_working_review() {
        assert!(matches!(
            review_answer("\n", false),
            Some(OrdinaryChoice::Confirm)
        ));
        assert!(matches!(
            review_answer("", true),
            Some(OrdinaryChoice::Confirm)
        ));
        assert!(matches!(
            review_answer("i", true),
            Some(OrdinaryChoice::InterruptNow)
        ));
        assert!(review_answer("i", false).is_none());
        assert!(matches!(
            review_answer("n", true),
            Some(OrdinaryChoice::Cancel)
        ));
        assert!(review_answer("maybe", true).is_none());
    }

    #[test]
    fn update_lock_distinguishes_contention_from_a_descriptor_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock");
        let first = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let held = lock_file(first).unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(matches!(lock_file(other), Err(UpdateError::Busy)));
        // O_PATH is a live owned regular-file descriptor but cannot take flock.
        let path_only = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(&path)
            .unwrap();
        assert!(
            matches!(lock_file(path_only), Err(UpdateError::Io(error)) if error.raw_os_error() == Some(libc::EBADF))
        );
        drop(held);
    }

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
        // The children outlive the test's wait, so a command that waited for
        // them instead of its timeout or cancellation never returns.
        let result = crate::test_timing::returns("a timed-out command", || {
            bounded_command(
                Command::new("/bin/sleep").arg("3600"),
                "owned fixture",
                10,
                Duration::from_millis(20),
                &CancellationToken::new(),
            )
        });
        assert!(matches!(result, Err(UpdateError::Timeout("owned fixture"))));
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            signal.cancel();
        });
        let result = crate::test_timing::returns("a cancelled command", move || {
            bounded_command(
                Command::new("/bin/sleep").arg("3600"),
                "owned fixture",
                10,
                Duration::from_secs(3600),
                &cancellation,
            )
        });
        canceller.join().unwrap();
        assert!(matches!(result, Err(UpdateError::Cancelled)));
    }

    #[test]
    fn streaming_child_observation_keeps_leader_until_final_cleanup() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut child = StreamingChild::new(child).unwrap();
        let status = crate::test_timing::wait_for("the child's exit", || {
            child.observed_exit_status().unwrap()
        });
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
        crate::test_timing::eventually("the child's exit", || {
            child.observed_exit_status().unwrap().is_some()
        });
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

        // Outlives the test's wait: a finish that waited for it never returns.
        let child = Command::new("/bin/sleep")
            .arg("3600")
            .process_group(0)
            .spawn()
            .unwrap();
        let child = StreamingChild::new(child).unwrap();
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
        let (child, error) = crate::test_timing::returns("a refused finish", move || {
            let mut child = child;
            let error = child
                .finish_with(true, |_| Err(io::Error::from_raw_os_error(libc::EPERM)))
                .unwrap_err();
            (child, error)
        });
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
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
    fn installed_entry_admits_supported_sources_and_uses_the_candidate_worker() {
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
        let mut other_reply = postcard::to_stdvec(&127u16).unwrap();
        // Deliberately not a current or candidate-line envelope. Entry routing
        // must never decode this unknown source's response DTO.
        other_reply.extend_from_slice(b"opaque-unqualified-source-dto");
        let script = root.join("source.py");
        fs::write(&script, r#"
import concurrent.futures,fcntl,json,os,socket,struct,sys,threading,time
from pathlib import Path
c=json.loads(Path(sys.argv[1]).read_text());s=Path(c['socket']);lock=s.with_name('cagentsd.lock')
deadline=time.monotonic()+15
f=lock.open('x+');os.chmod(lock,0o600)
if c['held']: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)
json.dump(dict(pid=os.getpid(),proof=c['proof']),f);f.flush();os.fsync(f.fileno())
listener=socket.socket(socket.AF_UNIX);listener.bind(str(s));listener.listen(2);listener.settimeout(.05)
Path(c['ready']).write_text(str(os.getpid()))
count=0;impacts=0;counts=threading.Lock()
def serve(stream):
 global count,impacts
 with stream:
   stream.settimeout(5)
   def read(n):
    data=b''
    while len(data)<n:
     part=stream.recv(n-len(data));assert part;data+=part
    return data
   n=struct.unpack('>I',read(4))[0];assert n<=1024*1024
   assert read(n).hex()==c['connect']
   with counts:
    count+=1;assert count<=c['connections']
   reply=bytes.fromhex(c['reply']);stream.sendall(struct.pack('>I',len(reply))+reply)
   first=stream.recv(1)
   if first:
    n=struct.unpack('>I',first+read(3))[0];assert n<=1024*1024
    assert c['impact_request'] is not None, 'entry must not preview/prepare/stop'
    assert read(n).hex()==c['impact_request']
    with counts:
     impacts+=1;assert impacts==1
    if c['impact_reply'] is not None:
     reply=bytes.fromhex(c['impact_reply']);stream.sendall(struct.pack('>I',len(reply))+reply)
     assert stream.recv(1)==b'', 'preview must never prepare/stop'
try:
 # Like the real daemon, keep the impact connection alive while accepting
 # the observer's independent fresh owner-authentication connections.
 with concurrent.futures.ThreadPoolExecutor(max_workers=8) as workers:
  pending=[]
  while not Path(c['release']).exists():
   assert time.monotonic()<deadline
   try: stream,_=listener.accept()
   except TimeoutError: continue
   assert len(pending)<c['connections'], 'unexpected source connection'
   pending.append(workers.submit(serve,stream))
  for worker in pending: worker.result()
  assert count==c['connections']
  assert impacts==int(c['impact_request'] is not None)
finally:
 listener.close();f.close();s.unlink();lock.unlink()
"#).unwrap();
        let mut sequence = 0;
        let mut start = |executable: &Path,
                         current: bool,
                         held: bool,
                         connections: usize,
                         impact: Option<(&[u8], Option<&[u8]>)>| {
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
                "impact_request": impact.map(|(request, _)| request.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
                "impact_reply": impact.and_then(|(_, reply)| reply).map(|reply| reply.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
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
        // Unknown source protocols are refused without decoding their DTOs
        // or asking any older worker to perform the update.
        let mut fixture = start(&executable, false, true, 1, None);
        let error = installed_entry_worker(&window, &served)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("127")
                && error.contains("protocol 156")
                && error.contains("0.1.3+local.991daa43e")
        );
        fixture.finish();
        // A fully qualified current peer owned by an older build is updated
        // by the staged build's worker even when the old window asks, as it
        // is from the staged CLI (#2431); entry sends no impact or
        // preparation request and never probes an older worker.
        let mut fixture = start(&executable, true, true, 2, None);
        assert_eq!(installed_entry_worker(&old_window, &served).unwrap(), "new");
        fixture.finish();
        let mut fixture = start(&executable, true, false, 2, None);
        assert!(installed_entry_worker(&window, &served).is_err());
        fixture.finish();
        // Unknown service at the served candidate must not fall back to the
        // old window's CLI merely to replace that window's UI.
        fs::copy("/usr/bin/python3", prefix.join("versions/new/bin/cagentsd")).unwrap();
        let mut fixture = start(
            &prefix.join("versions/new/bin/cagentsd"),
            false,
            true,
            1,
            None,
        );
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
        let mut fixture = start(
            &foreign.join("versions/old/bin/cagentsd"),
            true,
            true,
            2,
            None,
        );
        assert!(installed_entry_worker(&window, &served).is_err());
        fixture.finish();
        fs::remove_file(foreign.join(".cagents-install-root")).unwrap();
        let mut fixture = start(
            &foreign.join("versions/old/bin/cagentsd"),
            true,
            true,
            2,
            None,
        );
        assert!(installed_entry_worker(&window, &served).is_err());
        fixture.finish();
        // An older installed executable at the CURRENT protocol still owns
        // real sessions. Exercise the production installed-preview path, not
        // just the worker selector or a candidate-version inequality.
        let expected = impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof.clone(),
                token: "private-installed-checkpoint".into(),
                inventory_generation: 7,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: "none".into(),
                expires_at_unix_ms: 100,
            },
            policy: impact::Policy::WhenIdle,
            sessions: vec![impact::SessionImpact {
                id: crate::model::TerminalId::new(),
                kind: impact::SessionKind::Terminal,
                title: "Existing installed terminal".into(),
                identity: "exact-installed-source-owner".into(),
                disposition: impact::Disposition::Continues,
                reason: "Checkpoint must preserve this actual source row".into(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        };
        let impact_request = postcard::to_stdvec(&ClientEnvelope {
            version: crate::terminal_ipc::PROTOCOL_VERSION,
            request_id: 2,
            proof: Some(proof.clone()),
            request: ClientRequest::UpgradeImpact {
                policy: impact::Policy::WhenIdle,
            },
        })
        .unwrap();
        let impact_reply = postcard::to_stdvec(&ServerEnvelope {
            version: crate::terminal_ipc::PROTOCOL_VERSION,
            request_id: Some(2),
            proof: proof.clone(),
            message: ServerMessage::Response {
                response: ServerResponse::UpgradeImpact(expected.clone()),
            },
        })
        .unwrap();
        let fallback_marker = root.join("unexpected-compatible-fallback");
        let mut candidate_bytes = anonymous_file().unwrap();
        write!(
            candidate_bytes,
            "#!/bin/sh\nif [ \"$3\" = protocol-version ]; then\n  echo '{{\"data\":{{\"protocol_version\":{}}}}}'\nelse\n  : > '{}'\n  echo '{{\"data\":{{\"running\":true,\"daemon_version\":\"fixture\"}}}}'\nfi\n",
            crate::terminal_ipc::PROTOCOL_VERSION,
            fallback_marker.display(),
        )
        .unwrap();
        seal_file(&candidate_bytes).unwrap();
        let staged = candidate::Staged::from_file(
            candidate_bytes,
            "new".into(),
            "private-installed-bytes".into(),
            &CancellationToken::new(),
        )
        .unwrap();
        let mut fixture = start(
            &executable,
            true,
            true,
            6,
            Some((&impact_request, Some(&impact_reply))),
        );
        let observed = installed::observe_installed_source_for_test(
            &window,
            &served,
            &staged,
            impact::Policy::WhenIdle,
        )
        .unwrap();
        assert!(observed.replaces_daemon);
        assert_eq!(observed.daemon, Some(expected));
        assert!(!fallback_marker.exists());
        fixture.finish();
        // The source then refuses impact by closing the actual request. A
        // compatible candidate status is deliberately available, but cannot
        // downgrade that failed checkpoint to a UI-only update.
        let mut fixture = start(&executable, true, true, 4, Some((&impact_request, None)));
        assert!(
            installed::observe_installed_source_for_test(
                &window,
                &served,
                &staged,
                impact::Policy::WhenIdle,
            )
            .is_err()
        );
        assert!(!fallback_marker.exists());
        fixture.finish();
        // A replacement using the very same executable and proof text is
        // still another process: the held original pidfd/epoch must refuse it.
        let mut fixture = start(&executable, true, true, 4, None);
        let original = source::inspect_installed_identity().unwrap();
        let same_owner = source::inspect_installed_identity().unwrap();
        original.verify_same_owner(&same_owner).unwrap();
        fixture.finish();
        let mut replacement = start(&executable, true, true, 2, None);
        let replacement_identity = source::inspect_installed_identity().unwrap();
        let owner = installed_daemon_owner(&window).unwrap().unwrap();
        replacement_identity
            .verify_owner(Some(&owner.version_root.join("bin/cagentsd")))
            .unwrap();
        // Fresh authentication succeeds, but it cannot renew the original
        // process's authority even with the very same executable and proof.
        assert!(original.verify_same_owner(&replacement_identity).is_err());
        assert!(
            original
                .verify_owner(Some(&owner.version_root.join("bin/cagentsd")))
                .is_err()
        );
        // Neither observation requests inventory, preparation or mutation.
        replacement.finish();
    }

    #[test]
    fn staged_update_always_uses_the_candidate_worker() {
        let layout = |version: &str| InstallLayout {
            prefix: "/owned/install".into(),
            version_root: PathBuf::from("/owned/install/versions").join(version),
            version: version.into(),
        };
        let old = layout("old");
        let new = layout("new");
        let third = layout("third");
        let proof = crate::model::HostSessionProof {
            host_session_id: "source".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        };
        for protocol in source::protocols() {
            for window in [&old, &new] {
                for owner in [&old, &third] {
                    assert_eq!(
                        installed_candidate_version(
                            window,
                            &new.version_root,
                            Some(owner),
                            protocol,
                            Some(&proof)
                        )
                        .unwrap(),
                        "new"
                    );
                }
            }
        }
        // UI-only replacement and a stopped source also use the candidate.
        assert_eq!(
            installed_candidate_version(
                &old,
                &new.version_root,
                Some(&new),
                crate::terminal_ipc::PROTOCOL_VERSION,
                Some(&proof)
            )
            .unwrap(),
            "new"
        );
        assert_eq!(
            installed_candidate_version(
                &old,
                &new.version_root,
                None,
                crate::terminal_ipc::PROTOCOL_VERSION,
                None
            )
            .unwrap(),
            "new"
        );
        // Preserve no-op refusal once both window and daemon are already served.
        for (owner, proof) in [(None, None), (Some(&new), Some(&proof))] {
            assert!(
                installed_candidate_version(
                    &new,
                    &new.version_root,
                    owner,
                    crate::terminal_ipc::PROTOCOL_VERSION,
                    proof
                )
                .is_err()
            );
        }
        let error =
            installed_candidate_version(&new, &new.version_root, Some(&old), 127, Some(&proof))
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("127")
                && error.contains("protocol 156")
                && error.contains("0.1.3+local.991daa43e")
        );
    }
    use std::os::unix::fs::PermissionsExt;

    fn release(version: &str) -> Release {
        let asset = |name: String| Asset {
            name,
            size: 100,
            sha256: [0xab; 32],
        };
        Release {
            version: version.into(),
            url: format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
            notes: String::new(),
            metadata: [1; 32],
            archive: asset(format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz")),
            checksums: asset("SHA256SUMS".into()),
            installer: asset("install.sh".into()),
        }
    }

    #[test]
    fn local_install_updates_only_to_a_newer_stable_release() {
        assert!(is_newer("1.10.0", "1.9.0").unwrap());
        assert!(!is_newer("1.9.0", "1.10.0").unwrap());
        assert!(!is_newer("0.1.3", "0.1.3+local.937921c").unwrap());
        assert!(is_newer("0.1.4", "0.1.3+local.937921c").unwrap());
        assert!(is_newer("0.1.4", "0.1.3-rc.1").is_err());
        assert!(is_newer("0.1.4+local.abc", "0.1.3").is_err());
        assert!(is_newer("2.0.0-rc.1", "1.0.0").is_err());
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
        install_tree(root, &[crate::version()], crate::version());
        let executable = root
            .join("versions")
            .join(crate::version())
            .join("bin/cagents-ui");
        assert_eq!(
            Installation::from_executable(&executable).unwrap().version,
            crate::version()
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
    fn an_installed_ordinary_tree_is_not_offered_for_download_again() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        install_tree(root, &["0.1.3", "0.1.4", "0.1.5", "0.1.6"], "0.1.3");
        assert!(holds_installed_tree(root, "0.1.4"), "a withdrawn build");
        assert!(!holds_installed_tree(root, "0.1.7"));
        for (version, marker) in [
            ("0.1.5", RETAINED_CANDIDATE_MARKER),
            ("0.1.6", PREPARED_CANDIDATE_MARKER),
        ] {
            fs::write(root.join("versions").join(version).join(marker), b"{}").unwrap();
            assert!(
                !holds_installed_tree(root, version),
                "{marker}: installing again revalidates a candidate"
            );
        }
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
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
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
        job.version = "0.1.4".into();
        fs::create_dir_all(root.join("versions/0.1.4/bin")).unwrap();
        assert!(
            superseded(&job, true),
            "a newer build withdrawn by selecting this one no longer applies"
        );
        assert!(!superseded(&job, false), "nor is a rollback a withdrawal");
        fs::write(
            root.join("versions/0.1.4").join(PREPARED_CANDIDATE_MARKER),
            b"{}",
        )
        .unwrap();
        assert!(
            !superseded(&job, true),
            "a candidate kept after its update failed was never withdrawn"
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
    fn a_failed_job_reached_its_target_only_when_this_build_serves_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let old = "0.1.3+local.e8bbb84f2";
        let target = "0.2.2";
        install_tree(root, &[old, target], target);
        let window = locate(&root.join("current/bin/cagents-ui")).unwrap();
        let owner = locate(&root.join("current/bin/cagentsd")).unwrap();
        let old_owner = locate(&root.join("versions").join(old).join("bin/cagentsd")).unwrap();
        let served = served_version_root(&window).unwrap();
        let mut job = UpdateJob {
            id: "late-successor-check".into(),
            version: target.into(),
            phase: UpdatePhase::Failed,
            message: "could not configure the daemon connection".into(),
            log_path: root.join(".update/late-successor-check.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        assert!(failed_job_reached_its_target(
            &job,
            &window,
            &served,
            Some(&owner)
        ));
        assert!(
            !failed_job_reached_its_target(&job, &window, &served, Some(&old_owner)),
            "the previous daemon still serving is an unresolved replacement"
        );
        assert!(!failed_job_reached_its_target(&job, &window, &served, None));
        let old_window = locate(&root.join("versions").join(old).join("bin/cagents-ui")).unwrap();
        assert!(
            !failed_job_reached_its_target(&job, &old_window, &served, Some(&owner)),
            "a window of another build is not the target"
        );
        job.phase = UpdatePhase::Installing;
        assert!(
            !failed_job_reached_its_target(&job, &window, &served, Some(&owner)),
            "never filter a live operation"
        );
        job.phase = UpdatePhase::Failed;
        job.version = "0.2.3".into();
        assert!(!failed_job_reached_its_target(
            &job,
            &window,
            &served,
            Some(&owner)
        ));
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
        // The parent deliberately hands the shared description to the worker.
        let parent = first.into_file();
        let inherited = crate::instance::owned_file_lock(
            parent.try_clone().unwrap(),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let unrelated_fork = parent.try_clone().unwrap();
        assert!(matches!(acquire_lock(root.path()), Err(UpdateError::Busy)));
        drop(parent);
        assert!(matches!(acquire_lock(root.path()), Err(UpdateError::Busy)));
        drop(inherited);
        let next = acquire_lock(root.path()).unwrap();
        drop(unrelated_fork);
        assert!(matches!(acquire_lock(root.path()), Err(UpdateError::Busy)));
        drop(next);
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
    #[test]
    fn completion_message_preserves_the_handoff_outcome() {
        let job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Complete,
            message: String::new(),
            log_path: PathBuf::new(),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        assert_eq!(
            completion_message(
                &Ok(Completion {
                    handoff: Handoff::NoDaemon,
                    pane_notes: None
                }),
                &job,
                false,
            ),
            NO_DAEMON_INSTALLED
        );
        assert_eq!(
            completion_message(
                &Ok(Completion {
                    handoff: Handoff::Served,
                    pane_notes: None
                }),
                &job,
                false,
            ),
            "Update installed; saved windows reopened"
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn startup_failure_is_persisted_before_releasing_the_handoff() {
        use crate::daemon::{HostLease, RuntimePaths, lease_handoff::LeaseHandoff};
        if !crate::daemon::isolated_handoff_coordinator() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("daemon");
        let paths = RuntimePaths {
            socket: directory.join(crate::terminal_ipc::SOCKET_FILE_NAME),
            lock: directory.join("cagentsd.lock"),
            directory,
        };
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("handoff-failure"),
            boot_id: crate::model::BootId::new("handoff-failure"),
            host_incarnation: crate::model::HostIncarnation::new("handoff-failure"),
        };
        drop(HostLease::acquire(&paths, &proof).unwrap());
        let mut handoff = Some(LeaseHandoff::wait(&paths, std::process::id()).unwrap());
        let mut job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Installing,
            message: String::new(),
            log_path: root.path().join("worker.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let result = Err(invalid("the original startup failure"));
        // A failed status write must leave the reservation intact.
        assert!(
            record_handoff_failure(
                &root.path().join("missing"),
                &mut job,
                &result,
                &mut handoff,
                "",
                "",
            )
            .is_err()
        );
        assert!(handoff.is_some());
        assert!(HostLease::is_held(&paths).unwrap());
        record_handoff_failure(root.path(), &mut job, &result, &mut handoff, "", "").unwrap();
        let persisted: UpdateJob =
            serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap()).unwrap();
        assert_eq!(persisted.phase, UpdatePhase::Failed);
        assert_eq!(persisted.message, "the original startup failure");
        assert!(handoff.is_none());
        assert!(!HostLease::is_held(&paths).unwrap());
    }
    #[cfg(feature = "host")]
    #[test]
    fn transferred_lease_skips_source_prepare_and_survives_installer_start_failure() {
        use crate::daemon::{HostLease, RuntimePaths, lease_handoff::LeaseHandoff};
        const MODE: &str = "CAGENTS_TEST_HEADLESS_RECOVERY";
        const TEST: &str = "update::tests::transferred_lease_skips_source_prepare_and_survives_installer_start_failure";
        if std::env::var_os(MODE).is_none() {
            for failure in [
                "reserved-start-failure",
                "installer-success-daemon-exited",
                "shutdown-requested-without-reservation",
                "start-exit-zero-without-service",
                "ready-missing-terminal",
                "retained-listener",
                "prepared-target-handoff",
                "recovery-recording-failure",
                "start-nonzero-with-service",
                "first-start-failed-without-source",
                "reopened-window-starts-daemon",
                "absent-source-with-prepared-checkpoint",
                "successor-mismatch",
            ] {
                // A retained Prepared checkpoint belongs to this one case;
                // later cases must not consume or remove another case's state.
                let isolated = tempfile::tempdir().unwrap();
                let runtime = isolated.path().join("runtime");
                fs::create_dir(&runtime).unwrap();
                fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                    .env(MODE, "worker")
                    .env("CAGENTS_TEST_RECOVERY_FAILURE", failure)
                    .env(
                        "CAGENTS_TEST_HEADLESS_EXEC",
                        std::env::current_exe().unwrap(),
                    )
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .env("XDG_DATA_HOME", isolated.path().join("data"))
                    .env("XDG_CONFIG_HOME", isolated.path().join("config"))
                    .env("XDG_STATE_HOME", isolated.path().join("state"))
                    .env("XDG_CACHE_HOME", isolated.path().join("cache"))
                    .env("HOME", isolated.path())
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{failure}: {output:?}");
            }
            return;
        }
        if std::env::var(MODE).unwrap() == "daemon" {
            // Exercise the real admission barrier while the worker still owns
            // the update fence. Recovery has to inherit that exact authority.
            let _admission = crate::instance::DaemonAdmissionGuard::acquire().unwrap();
            let root = PathBuf::from(std::env::var_os("CAGENTS_TEST_HEADLESS_ROOT").unwrap());
            let paths = RuntimePaths::discover().unwrap();
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&paths.lock)
                .unwrap();
            assert_eq!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            let proof = crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("recovered-fixture"),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: crate::model::HostIncarnation::new("recovered-fixture"),
            };
            fs::write(
                &paths.lock,
                serde_json::to_vec(&serde_json::json!({"pid":std::process::id(),"proof":proof}))
                    .unwrap(),
            )
            .unwrap();
            let socket = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
            socket.set_nonblocking(true).unwrap();
            fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(root.join("daemon-ready"), b"listener").unwrap();
            use crate::terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
                ServerResponse, StartupState, read_frame, write_frame,
            };
            while !root.join("daemon-exit").exists() {
                let (mut stream, _) = match socket.accept() {
                    Ok(c) => c,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                while let Ok(Some(envelope)) = read_frame::<ClientEnvelope>(&mut stream) {
                    let response = match envelope.request {
                        ClientRequest::Connect { .. } => ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                        ClientRequest::ShutdownV1 {
                            version: crate::terminal_ipc::SHUTDOWN_VERSION,
                            operation: crate::terminal_ipc::ShutdownOperation::Preflight,
                        } => ServerResponse::ShutdownPreflight(
                            crate::terminal_ipc::ShutdownPreflight {
                                capability: crate::terminal_ipc::ShutdownCapability {
                                    proof: proof.clone(),
                                    token: "fixture-inventory-fence".into(),
                                    inventory_generation: 0,
                                    live_terminals: 0,
                                    background_subagents: 0,
                                    background_identity: String::new(),
                                    expires_at_unix_ms: i64::MAX,
                                },
                            },
                        ),
                        ClientRequest::StartupState => ServerResponse::StartupState(
                            if std::env::var("CAGENTS_TEST_RECOVERY_FAILURE").unwrap()
                                == "retained-listener"
                            {
                                StartupState::Retained {
                                    detail: "retained test ownership".into(),
                                }
                            } else {
                                StartupState::Ready
                            },
                        ),
                        ClientRequest::ListPage { after: None } => ServerResponse::ListedPage {
                            terminals: vec![],
                            complete: true,
                        },
                        ClientRequest::ChatList => ServerResponse::ChatListed { sessions: vec![] },
                        _ => panic!("unexpected verification request"),
                    };
                    if write_frame(
                        &mut stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(envelope.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Response { response },
                        },
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            }
            crate::daemon::shut_down_test_listener(socket);
            return;
        }
        let guard = crate::instance::UiUpdateGuard::acquire().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover().unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("headless-recovery"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("headless-recovery"),
        };
        drop(HostLease::acquire(&paths, &proof).unwrap());
        let failure = std::env::var("CAGENTS_TEST_RECOVERY_FAILURE").unwrap();
        let mut lease = matches!(
            failure.as_str(),
            "reserved-start-failure" | "prepared-target-handoff"
        )
        .then(|| LeaseHandoff::wait(&paths, std::process::id()).unwrap());
        let installation = Installation {
            prefix: root.path().into(),
            version: "old".into(),
        };
        let bin = root.path().join("versions/old/bin");
        fs::create_dir_all(&bin).unwrap();
        // Model the installer's restored pointer after a pre-start failure.
        // There are no UI windows to start a daemon on the worker's behalf.
        std::os::unix::fs::symlink("versions/old", root.path().join("current")).unwrap();
        let script = r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(__file__).resolve().parents[3]
assert sys.argv[1:] == ['daemon','start']  # No second source prepare.
with (root/'start-attempts').open('a') as f: f.write('attempt\n')
if os.environ['CAGENTS_TEST_RECOVERY_FAILURE']=='start-exit-zero-without-service': sys.exit(0)
assert 'CAGENTS_DAEMON_LEASE_FD' not in os.environ
assert not pathlib.Path('/proc/self/fd/199').exists()
if os.environ['CAGENTS_TEST_RECOVERY_FAILURE']!='recovery-recording-failure':
    expected = 'Installing' if os.environ['CAGENTS_TEST_RECOVERY_FAILURE'] in ('installer-success-daemon-exited', 'reopened-window-starts-daemon', 'successor-mismatch') else 'Failed'
    assert json.loads((root/'status.json').read_text())['phase'] == expected
os.environ['CAGENTS_TEST_HEADLESS_RECOVERY']='daemon'
os.environ['CAGENTS_TEST_HEADLESS_ROOT']=str(root)
child = subprocess.Popen([os.environ['CAGENTS_TEST_HEADLESS_EXEC'], '--exact',
    'update::tests::transferred_lease_skips_source_prepare_and_survives_installer_start_failure',
    '--nocapture', '--test-threads=1'],
    pass_fds=(int(os.environ['CAGENTS_UI_UPDATE_GUARD_FD']),),
    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=None)
while not (root/'daemon-ready').exists():
    if child.poll() is not None: sys.exit(1)
    time.sleep(.005)
if os.environ['CAGENTS_TEST_RECOVERY_FAILURE']=='start-nonzero-with-service': sys.exit(1)
"#;
        fs::write(bin.join("cagents"), script).unwrap();
        fs::set_permissions(bin.join("cagents"), fs::Permissions::from_mode(0o700)).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::write(self.0.join("daemon-exit"), b"exit");
            }
        }
        let _cleanup = Cleanup(root.path().to_owned());
        let mut job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Installing,
            message: String::new(),
            log_path: root.path().join("worker.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let census: recovery::Census=serde_json::from_value(serde_json::json!({"version":1,"transaction":job.id,"source":proof,"terminals":[],"chats":[]})).unwrap();
        let census = if matches!(
            failure.as_str(),
            "ready-missing-terminal" | "prepared-target-handoff"
        ) {
            let mut missing = crate::terminal_ipc::TerminalDescriptor::relaunchable(
                crate::model::TerminalId::new(),
                crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: vec![],
                    working_directory: "/private".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                None,
            );
            if failure == "prepared-target-handoff" {
                missing.session.continuity = crate::model::TerminalContinuity::Live {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: proof.host_incarnation.clone(),
                };
            }
            serde_json::from_value(serde_json::json!({"version":1,"transaction":job.id,"source":proof,"terminals":[missing.session],"chats":[]})).unwrap()
        } else {
            census
        };
        let without_source = matches!(
            failure.as_str(),
            "first-start-failed-without-source"
                | "reopened-window-starts-daemon"
                | "absent-source-with-prepared-checkpoint"
        );
        let census = if without_source {
            serde_json::from_value(serde_json::json!({"version":1,"transaction":job.id,"source":null,"terminals":[],"chats":[]})).unwrap()
        } else {
            census
        };
        census
            .save(&recovery::census_path(root.path(), &job.id))
            .unwrap();
        let context = recovery::Context {
            shutdown_requested: !without_source,
            census: Some(census),
        };
        let prepared = if matches!(
            failure.as_str(),
            "prepared-target-handoff" | "absent-source-with-prepared-checkpoint"
        ) {
            let store = crate::upgrade::handoff::Store::discover().unwrap();
            store
                .save(&crate::upgrade::handoff::Checkpoint::new(
                    proof.clone(),
                    vec![],
                    vec![],
                    vec![],
                ))
                .unwrap();
            Some((store.path.clone(), fs::read(&store.path).unwrap()))
        } else {
            None
        };
        if failure == "prepared-target-handoff" {
            // Model a retry after an older worker left only its private job
            // census. Resolve it through exact request/source bindings, before
            // any empty-source capture can replace the scope.
            let bin = root.path().join("versions/new/bin");
            fs::create_dir_all(&bin).unwrap();
            for executable in ["cagents", "cagentsd", "cagents-ui"] {
                fs::write(bin.join(executable), b"retained matched fixture").unwrap();
            }
            fs::write(
                root.path().join(".cagents-install-root"),
                b"cagents install root\n",
            )
            .unwrap();
            let state = Installation {
                prefix: root.path().into(),
                version: "new".into(),
            }
            .state_dir()
            .unwrap();
            context
                .census
                .as_ref()
                .unwrap()
                .save(&recovery::census_path(&state, &job.id))
                .unwrap();
            let request = private_create(&state.join(format!("{}.request", job.id))).unwrap();
            serde_json::to_writer(
                &request,
                &UpdateRequest::Installed {
                    root: root.path().join("versions/old"),
                },
            )
            .unwrap();
            write_job(&state, &job).unwrap();
            assert!(matches!(
                inspect_pending_recovery(&bin.join("cagents"), None),
                Err(UpdateError::Invalid(_))
            ));
            assert!(
                !crate::upgrade::recovery::PendingRecovery::inspect()
                    .unwrap()
                    .unwrap()
                    .original_census_complete
            );
            request.set_len(0).unwrap();
            use std::io::Seek;
            let mut request = request;
            request.rewind().unwrap();
            serde_json::to_writer(
                request,
                &UpdateRequest::Installed {
                    root: root.path().join("versions/new"),
                },
            )
            .unwrap();
            write_job(&state, &job).unwrap();
            let before = crate::upgrade::recovery::PendingRecovery::inspect()
                .unwrap()
                .unwrap();
            assert!(!before.original_census_complete);
            assert!(matches!(
                inspect_pending_recovery(&bin.join("cagents"), None),
                Err(UpdateError::TargetRecoveryRequired(_))
            ));
            let pending = crate::upgrade::recovery::PendingRecovery::inspect()
                .unwrap()
                .unwrap();
            assert!(pending.original_census_complete);
            let recovered =
                recovery::Census::from_pending(uuid::Uuid::new_v4().to_string(), &pending).unwrap();
            let value = serde_json::to_value(recovered).unwrap();
            assert_eq!(value["source"], serde_json::to_value(&proof).unwrap());
            assert_eq!(value["terminals"].as_array().unwrap().len(), 1);
        }
        if failure == "recovery-recording-failure" {
            fs::create_dir(root.path().join("status.json")).unwrap();
            fs::create_dir(recovery::outcome_path(root.path(), &job.id)).unwrap();
        }
        let mut installer_result = if matches!(
            failure.as_str(),
            "installer-success-daemon-exited"
                | "first-start-failed-without-source"
                | "reopened-window-starts-daemon"
                | "successor-mismatch"
        ) {
            write_job(root.path(), &job).unwrap();
            Ok(())
        } else {
            Err(invalid(failure.clone()))
        };
        let mut survivors = floor::Survivors::default();
        if failure == "successor-mismatch" {
            // The fixture daemon is this test executable, never the
            // selected successor's.
            survivors
                .authorize_successor(&root.path().join("versions/new/bin/cagentsd"), None, None)
                .unwrap();
        }
        let settled = finish_handoff(
            &installation,
            root.path(),
            &mut job,
            &mut installer_result,
            &mut lease,
            &context,
            &mut survivors,
            &guard,
        );
        if matches!(
            failure.as_str(),
            "first-start-failed-without-source" | "reopened-window-starts-daemon"
        ) {
            assert!(settled.is_ok(), "{settled:?}");
            assert!(matches!(settled, Ok(Handoff::NoDaemon)), "{settled:?}");
            assert!(job.message.contains("will start at graphical login"));
            assert!(!root.path().join("start-attempts").exists());
            let receipt: recovery::Receipt = serde_json::from_slice(
                &fs::read(recovery::outcome_path(root.path(), &job.id)).unwrap(),
            )
            .unwrap();
            assert!(matches!(receipt.outcome, recovery::Outcome::NoSource));
            assert!(!receipt.usable());
            let reopened_window = failure == "reopened-window-starts-daemon";
            if reopened_window {
                // Model the daemon start performed by a reopened UI, using
                // the private fixture service without opening a native window.
                let mut reopen = Command::new(bin.join("cagents"));
                reopen.args(["daemon", "start"]);
                guard.inherit_for_install(&mut reopen).unwrap();
                assert!(reopen.status().unwrap().success());
                assert!(HostLease::is_held(&paths).unwrap());
            }
            let final_result = settled
                .and_then(|_| verify_reopened_service(&context, root.path(), &mut job, None))
                .and_then(|handoff| {
                    finish_reopening_windows(
                        root.path(),
                        &mut job,
                        installer_result,
                        handoff,
                        Ok(None),
                    )
                });
            finish_worker(root.path(), &mut job, final_result, &mut survivors).unwrap();
            let persisted: UpdateJob =
                serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap())
                    .unwrap();
            assert_eq!(persisted.phase, UpdatePhase::Complete);
            assert_eq!(
                persisted.message,
                if reopened_window {
                    "Update installed; saved windows reopened"
                } else {
                    NO_DAEMON_INSTALLED
                }
            );
            if reopened_window {
                fs::write(root.path().join("daemon-exit"), b"exit").unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while HostLease::is_held(&paths).unwrap() {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                }
            } else {
                assert!(!HostLease::is_held(&paths).unwrap());
                assert!(!root.path().join("start-attempts").exists());
            }
            return;
        }
        if let Some((path, bytes)) = prepared {
            if failure == "absent-source-with-prepared-checkpoint" {
                assert!(settled.is_err());
                assert!(!job.message.contains("will start at graphical login"));
                assert!(!root.path().join("start-attempts").exists());
                assert_eq!(fs::read(path).unwrap(), bytes);
                return;
            }
            let Err(UpdateError::TargetRecoveryRequired(required)) = settled else {
                panic!("Expected typed target recovery, got {settled:?}");
            };
            let command = required.recovery_command.unwrap();
            assert!(
                command.contains(
                    &root
                        .path()
                        .join("versions/new/bin/cagents")
                        .display()
                        .to_string()
                )
            );
            assert!(command.contains("daemon recover-update --checkpoint"));
            assert!(command.contains("--yes"));
            assert!(
                job.message.starts_with(failure.as_str()),
                "the cause must lead the target-recovery report: {}",
                job.message
            );
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert!(
                lease.is_none(),
                "the explicit recovery command needs a fresh lease"
            );
            drop(
                LeaseHandoff::try_acquire(&paths).unwrap().expect(
                    "the failed worker must release its reservation before target recovery",
                ),
            );
            assert!(!root.path().join("start-attempts").exists());
            // A retry must retain the pending source scope before looking at
            // the empty socket. No fresh empty census or worker is created.
            let retry = root.path().join("retry-census.json");
            assert!(matches!(
                recovery::capture_file_with_candidate(
                    &retry,
                    Some(&root.path().join("versions/new/bin/cagents"))
                ),
                Err(UpdateError::TargetRecoveryRequired(_))
            ));
            assert!(!retry.exists());
            // Retry entrypoints can run the old UI, old CLI, or a temporary
            // installer coordinator. Only the retained request chooses target.
            let old_bin = root.path().join("versions/old/bin");
            fs::create_dir_all(&old_bin).unwrap();
            for executable in ["cagents", "cagentsd", "cagents-ui"] {
                fs::write(old_bin.join(executable), b"old matched fixture").unwrap();
            }
            for caller in [old_bin.join("cagents-ui"), old_bin.join("cagents")] {
                let Err(UpdateError::TargetRecoveryRequired(required)) =
                    recovery::inspect_pending_caller(&caller)
                else {
                    panic!("old caller must resolve the retained job target")
                };
                let command = required.recovery_command.unwrap();
                assert!(command.starts_with(&format!(
                    "'{}' --no-start daemon recover-update",
                    root.path().join("versions/new/bin/cagents").display()
                )));
                assert!(!command.contains("versions/old"));
                assert!(!command.contains("cagents-ui"));
            }
            // A new, non-original census must not rebind the complete capture
            // or turn an actionable recovery into a dead-end mismatch.
            let state = root.path().join(".update");
            let bin = root.path().join("versions/new/bin");
            let pending = crate::upgrade::recovery::PendingRecovery::inspect()
                .unwrap()
                .unwrap();
            let non_original = root.path().join("non-original-census.json");
            let mut different: serde_json::Value =
                serde_json::from_slice(&fs::read(recovery::census_path(&state, &job.id)).unwrap())
                    .unwrap();
            different["transaction"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
            different["terminals"] = serde_json::json!([]);
            let different: recovery::Census = serde_json::from_value(different).unwrap();
            different.save(&non_original).unwrap();
            let Err(UpdateError::TargetRecoveryRequired(required)) =
                inspect_pending_recovery(&bin.join("cagents"), Some(&non_original))
            else {
                panic!("complete pending scope must remain actionable")
            };
            assert!(
                !required
                    .recovery_command
                    .unwrap()
                    .contains("non-original-census")
            );
            let preserved = crate::upgrade::recovery::PendingRecovery::inspect()
                .unwrap()
                .unwrap();
            assert_eq!(preserved.census, pending.census);
            assert_eq!(
                fs::read(crate::upgrade::handoff::Store::discover().unwrap().path).unwrap(),
                bytes
            );
            assert!(matches!(
                // This fixture itself runs outside an installation. The public
                // entry cannot guess a target from its supplied UI path; the
                // managed old UI/CLI routes are exercised above.
                apply_installed_from(&root.path().join("versions/new/bin/cagents-ui"), true),
                Err(UpdateError::Invalid(_))
            ));
            let original = recovery::census_path(root.path(), &job.id);
            assert!(matches!(
                inspect_pending_recovery(
                    &root.path().join("versions/new/bin/cagents"),
                    Some(&original)
                ),
                Err(UpdateError::TargetRecoveryRequired(_))
            ));
            // Failed absence proof must invalidate an earlier NoSource
            // receipt instead of leaving a stale successful discriminator.
            let mut empty: serde_json::Value =
                serde_json::from_slice(&fs::read(&original).unwrap()).unwrap();
            empty["source"] = serde_json::Value::Null;
            empty["terminals"] = serde_json::json!([]);
            let empty_path = root.path().join("empty-census.json");
            let empty_census: recovery::Census = serde_json::from_value(empty).unwrap();
            empty_census.save(&empty_path).unwrap();
            let proof_path = root.path().join("absence-proof.json");
            fs::write(&proof_path, b"stale-no-source-receipt").unwrap();
            fs::set_permissions(&proof_path, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(
                recovery::verify_file(&empty_path, &proof_path, false, false, true, true).is_err()
            );
            let denied: recovery::Receipt =
                serde_json::from_slice(&fs::read(proof_path).unwrap()).unwrap();
            assert!(matches!(
                denied.outcome,
                recovery::Outcome::Unverified { .. }
            ));
            return;
        }
        let expected_failure = matches!(
            failure.as_str(),
            "start-exit-zero-without-service"
                | "ready-missing-terminal"
                | "retained-listener"
                | "recovery-recording-failure"
                | "start-nonzero-with-service"
        );
        assert_eq!(settled.is_err(), expected_failure, "{failure}: {settled:?}");
        if !expected_failure {
            assert!(matches!(settled, Ok(Handoff::Served)), "{settled:?}");
        }
        if failure == "successor-mismatch" {
            // The serving daemon failed the successor check after a clean
            // install. That fails the install, not the handoff: the worker
            // goes on to reopen windows from `current`, then reports it.
            let error = installer_result.as_ref().unwrap_err().to_string();
            assert!(
                error.contains("does not belong to the confirmed installation"),
                "{error}"
            );
            let persisted: UpdateJob =
                serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap())
                    .unwrap();
            assert_eq!(persisted.phase, UpdatePhase::Failed);
            assert!(
                persisted.message.starts_with(&error),
                "{}",
                persisted.message
            );
            let final_result = finish_reopening_windows(
                root.path(),
                &mut job,
                installer_result,
                Handoff::Served,
                Ok(None),
            );
            finish_worker(root.path(), &mut job, final_result, &mut survivors).unwrap_err();
            let persisted: UpdateJob =
                serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap())
                    .unwrap();
            assert_eq!(persisted.phase, UpdatePhase::Failed);
            assert!(
                persisted.message.starts_with(&error),
                "{}",
                persisted.message
            );
        }
        assert_eq!(
            fs::read_to_string(root.path().join("start-attempts")).unwrap(),
            "attempt\n"
        );
        if expected_failure {
            // The cause leads; recovery text follows it rather than replacing it.
            assert!(
                job.message.starts_with(failure.as_str()),
                "{failure}: {}",
                job.message
            );
            if failure == "recovery-recording-failure" {
                assert!(root.path().join("daemon-ready").exists());
                assert!(HostLease::is_held(&paths).unwrap());
                assert!(job.message.contains("could not record recovery"));
            } else if failure == "start-nonzero-with-service" {
                assert!(root.path().join("daemon-ready").exists());
                assert!(job.message.contains("inventory was verified"));
                assert!(job.message.contains("startup command reported failure"));
            } else {
                assert!(job.message.contains("unverified"));
            }
            assert!(job.message.contains("--no-start daemon status"));
            fs::write(root.path().join("daemon-exit"), b"exit").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while HostLease::is_held(&paths).unwrap() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        assert!(lease.is_none());
        assert!(root.path().join("daemon-ready").exists());
        assert!(std::os::unix::net::UnixStream::connect(&paths.socket).is_ok());
        assert!(
            HostLease::is_held(&paths).unwrap(),
            "worker must recover without windows"
        );
        fs::write(root.path().join("daemon-exit"), b"exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while HostLease::is_held(&paths).unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn handoff_reports_an_exited_source_with_a_leaked_lock_description() {
        use crate::daemon::{RuntimePaths, lease_handoff::HandoffSource};
        use std::io::BufRead;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("daemon");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = RuntimePaths {
            socket: directory.join(crate::terminal_ipc::SOCKET_FILE_NAME),
            lock: directory.join("cagentsd.lock"),
            directory,
        };
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("leaked-handoff"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("leaked-handoff"),
        };
        let mut daemon = Command::new("python3").args(["-c", r#"
import fcntl, json, os, subprocess, sys
lock = open(sys.argv[1], 'w+')
os.chmod(sys.argv[1], 0o600)
fcntl.flock(lock, fcntl.LOCK_EX)
json.dump({'pid':os.getpid(), 'proof':json.loads(sys.argv[2])}, lock)
lock.flush()
# Model an accidentally inherited copy after the serving process exits.
leak = subprocess.Popen(['/bin/sleep', '30'], pass_fds=(lock.fileno(),),
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print(leak.pid, flush=True)
sys.stdin.buffer.read(1)
"#]).arg(&paths.lock).arg(serde_json::to_string(&proof).unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut ready = String::new();
        io::BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let leaked_pid: u32 = ready.trim().parse().unwrap();
        let source = HandoffSource::pin(&paths, &proof).unwrap();
        // Pin the deliberate leak before allowing the original daemon to exit.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, leaked_pid, 0) };
        assert!(fd >= 0);
        struct Leak(File);
        impl Drop for Leak {
            fn drop(&mut self) {
                // SAFETY: the exact fixture process is pinned, never looked up again.
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
        let leak = Leak(unsafe { File::from_raw_fd(fd as i32) });
        daemon.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(daemon.wait().unwrap().success());
        let state = root.path().to_owned();
        let mut job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Installing,
            message: String::new(),
            log_path: state.join("worker.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let (send, recv) = std::sync::mpsc::sync_channel(1);
        let monitor = thread::spawn(move || {
            let _ = send.send(wait_daemon_stopped(paths, source, &state, &mut job));
        });
        let result = recv.recv_timeout(Duration::from_secs(10));
        drop(leak); // Also releases a broken monitor before joining its thread.
        monitor.join().unwrap();
        let error = result
            .expect("a dead source must not wait forever")
            .err()
            .expect("a leaked lock is not a completed handoff")
            .to_string();
        assert_eq!(error, handoff::SERVICE_NOT_STOPPED);
    }
    #[test]
    fn first_terminal_failure_preserves_the_exact_stop_report() {
        let directory = tempfile::tempdir().unwrap();
        let mut job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Installing,
            message: String::new(),
            log_path: directory.path().join("worker.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let report = concat!(
            "\nThe terminal service accepted the stop.\n",
            "Ended or stopped:\n",
            "  build · alpha: screen and directory kept; press Restart in the pane\n",
            "  review · beta: press Resume in the pane\n",
        );
        let mut handoff = None;
        record_handoff_failure(
            directory.path(),
            &mut job,
            &Err(invalid("The installer failed")),
            &mut handoff,
            report,
            "",
        )
        .unwrap();

        let first_failure: UpdateJob =
            serde_json::from_slice(&fs::read(directory.path().join("status.json")).unwrap())
                .unwrap();
        assert_eq!(first_failure.phase, UpdatePhase::Failed);
        assert!(first_failure.message.starts_with("The installer failed"));
        for line in report.lines().filter(|line| !line.is_empty()) {
            assert_eq!(
                first_failure.message.matches(line).count(),
                1,
                "{}",
                first_failure.message
            );
        }
    }

    #[test]
    fn a_refusal_is_recorded_failed_with_its_reasons_for_every_reader() {
        let refusal = impact::tests::blocked_preview().refusal().unwrap();
        let mut job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "9.8.7".into(),
            phase: UpdatePhase::AwaitingConfirmation,
            message: "Review the update".into(),
            log_path: "/fake/update-worker.log".into(),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        record_primary_failure(
            &mut job,
            &UpdateError::Refused(Box::new(refusal.clone())),
            "",
            "",
        );
        assert_eq!(job.phase, UpdatePhase::Failed);
        assert_eq!(job.refusal.as_ref(), Some(&refusal));
        // Readers that predate `refusal` find every reason in the message.
        assert_eq!(job.message, refusal.message());
        let mut record = serde_json::to_value(&job).unwrap();
        assert_eq!(
            serde_json::from_value::<UpdateJob>(record.clone())
                .unwrap()
                .refusal,
            Some(refusal)
        );
        // A record written before the field still reads.
        record.as_object_mut().unwrap().remove("refusal");
        assert_eq!(
            serde_json::from_value::<UpdateJob>(record).unwrap().refusal,
            None
        );
    }

    /// Answering n at a blocked review tells the person what the worker
    /// records: nothing was installed, and what held it back.
    #[test]
    fn declining_a_blocked_review_in_the_cli_reports_the_refusal() {
        struct Terminal {
            job: UpdateJob,
            decided: Vec<UpdateChoice>,
            output: Vec<u8>,
        }
        impl Write for Terminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl TerminalReview for Terminal {
            fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
                Ok(Some(self.job.clone()))
            }
            fn decide(
                &mut self,
                _: &str,
                _: &str,
                choice: UpdateChoice,
            ) -> Result<(), UpdateError> {
                self.decided.push(choice);
                Ok(())
            }
            fn cancel(&mut self, _: &str) -> Result<bool, UpdateError> {
                panic!("declining the review must not cancel the whole update");
            }
            fn read_line(&mut self, answer: &mut String) -> io::Result<usize> {
                answer.push_str("n\n");
                Ok(answer.len())
            }
            fn is_terminal(&self) -> bool {
                true
            }
            fn interrupted(&self) -> bool {
                false
            }
            fn worker_running(&mut self) -> Result<bool, UpdateError> {
                Ok(true)
            }
            fn pause(&mut self) {}
        }
        let preview = impact::tests::blocked_preview();
        let job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: preview.candidate.version.clone(),
            phase: UpdatePhase::AwaitingConfirmation,
            message: "Review the update".into(),
            log_path: "/fake/update-worker.log".into(),
            confirmation: Some(confirmation::Confirmation {
                nonce: "review".into(),
                expires_at_unix_ms: u64::MAX,
                preview,
                stop: None,
                terminal_command: None,
            }),
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let mut terminal = Terminal {
            job: job.clone(),
            decided: Vec::new(),
            output: Vec::new(),
        };
        let error = drive_terminal_review_with(job, false, false, &mut terminal).unwrap_err();
        assert!(matches!(terminal.decided[..], [UpdateChoice::Cancel]));
        assert!(
            matches!(&error, UpdateError::Refused(refusal) if !refusal.sessions.is_empty()),
            "{error:?}"
        );
        // One reason per line, where the record keeps them on one.
        let UpdateError::Refused(refusal) = &error else {
            unreachable!()
        };
        assert_eq!(
            error.to_string(),
            format!(
                "Update not installed: {}.\n{}",
                refusal.summary(),
                refusal
                    .details()
                    .iter()
                    .map(|detail| format!("  {detail}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        );
        assert_eq!(error.record_message(), refusal.message());
    }

    #[test]
    fn terminal_driver_waits_for_worker_recovery_before_returning_failure() {
        struct Terminal {
            job: UpdateJob,
            recovery_finished: bool,
            pauses: usize,
            worker_checks: usize,
            output: Vec<u8>,
        }
        impl Write for Terminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.output.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl TerminalReview for Terminal {
            fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
                Ok(Some(self.job.clone()))
            }
            fn decide(&mut self, _: &str, _: &str, _: UpdateChoice) -> Result<(), UpdateError> {
                panic!("recovery must not ask for another approval");
            }
            fn cancel(&mut self, _: &str) -> Result<bool, UpdateError> {
                panic!("recovery must not submit a new cancellation");
            }
            fn read_line(&mut self, _: &mut String) -> io::Result<usize> {
                panic!("recovery must not ask another question");
            }
            fn is_terminal(&self) -> bool {
                true
            }
            fn interrupted(&self) -> bool {
                false
            }
            fn worker_running(&mut self) -> Result<bool, UpdateError> {
                self.worker_checks += 1;
                Ok(!self.recovery_finished)
            }
            fn pause(&mut self) {
                self.pauses += 1;
                assert_eq!(self.pauses, 1, "the settled failure must return promptly");
                self.recovery_finished = true;
                self.job.message.push_str(
                    "\nThe terminal service could not restart. Run `cagents daemon start` to recover your saved panes.",
                );
            }
        }
        let job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Failed,
            message: concat!(
                "The installer failed\nEnded or stopped:\n",
                "  build · alpha: screen and directory kept; press Restart in the pane",
            )
            .into(),
            log_path: "/fake/update-worker.log".into(),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let mut terminal = Terminal {
            job: job.clone(),
            recovery_finished: false,
            pauses: 0,
            worker_checks: 0,
            output: Vec::new(),
        };
        let error = drive_terminal_review_with(job, false, false, &mut terminal)
            .unwrap_err()
            .to_string();
        assert_eq!(terminal.pauses, 1);
        assert_eq!(terminal.worker_checks, 2);
        assert!(
            error.contains("The terminal service could not restart"),
            "{error}"
        );
        assert!(
            error.contains("build · alpha: screen and directory kept"),
            "{error}"
        );
        assert!(
            terminal.output.is_empty(),
            "settled recovery needs no new prompts"
        );
    }
}

/// A failure an older worker recorded against the build a window runs.
#[cfg(test)]
mod reconciliation_tests {
    use super::*;
    use std::time::SystemTime;

    fn proof() -> crate::model::HostSessionProof {
        crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("target"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("target"),
        }
    }

    fn evidence(
        worker: Worker,
        receipt: recovery::Reading,
        recorded: SystemTime,
    ) -> TargetEvidence {
        TargetEvidence {
            worker,
            receipt,
            recorded,
        }
    }

    /// A window opened while the older worker's verifier runs reads its
    /// placeholder. It waits, then settles on the receipt the worker leaves.
    #[test]
    fn a_pending_placeholder_settles_on_the_verifiers_receipt() {
        let start = SystemTime::now();
        let pending = recovery::Reading::Pending { since: start };
        let at = |seconds| start + Duration::from_secs(seconds);
        for worker in [Worker::Running, Worker::Unknown] {
            assert_eq!(
                judge_target_failure(&evidence(worker, pending.clone(), start), at(5)),
                TargetFailure::Finishing,
                "{worker:?}"
            );
        }
        // The worker finished and replaced the placeholder with its verdict.
        assert_eq!(
            judge_target_failure(
                &evidence(Worker::Finished, recovery::Reading::Usable(proof()), start),
                at(6)
            ),
            TargetFailure::Reached(proof())
        );
        assert_eq!(
            judge_target_failure(
                &evidence(Worker::Finished, recovery::Reading::Unusable, start),
                at(6)
            ),
            TargetFailure::Unproven
        );
        // A finished worker that left the placeholder ran no verifier to
        // its end; nothing proved the service.
        assert_eq!(
            judge_target_failure(&evidence(Worker::Finished, pending.clone(), start), at(1)),
            TargetFailure::Unproven
        );
    }

    #[test]
    fn a_pending_placeholder_times_out_into_the_failure() {
        let start = SystemTime::now();
        let pending = recovery::Reading::Pending { since: start };
        for worker in [Worker::Running, Worker::Unknown] {
            assert_eq!(
                judge_target_failure(
                    &evidence(worker, pending.clone(), start),
                    start + PENDING_RECEIPT_PATIENCE
                ),
                TargetFailure::Unproven,
                "{worker:?}"
            );
        }
        // A clock that went back cannot keep a window waiting.
        assert_eq!(
            judge_target_failure(
                &evidence(
                    Worker::Running,
                    recovery::Reading::Pending {
                        since: start + Duration::from_secs(3600)
                    },
                    start
                ),
                start
            ),
            TargetFailure::Unproven
        );
    }

    /// Between recording the failure and its last verification the worker
    /// reopens windows; the earlier receipt is not its final word.
    #[test]
    fn a_running_worker_is_waited_for_within_its_bound() {
        let recorded = SystemTime::now();
        for receipt in [
            recovery::Reading::Usable(proof()),
            recovery::Reading::Unusable,
        ] {
            assert_eq!(
                judge_target_failure(
                    &evidence(Worker::Running, receipt.clone(), recorded),
                    recorded + Duration::from_secs(60)
                ),
                TargetFailure::Finishing
            );
            // A worker that holds the lease past its bound is stuck, not
            // done: its failure stands, whatever its receipt says.
            assert_eq!(
                judge_target_failure(
                    &evidence(Worker::Running, receipt, recorded),
                    recorded + FINISHING_WORKER_PATIENCE
                ),
                TargetFailure::Unproven
            );
        }
        assert_eq!(
            judge_target_failure(
                &evidence(
                    Worker::Unknown,
                    recovery::Reading::Usable(proof()),
                    recorded
                ),
                recorded
            ),
            TargetFailure::Reached(proof()),
            "only a held lease or a placeholder says the worker is still busy"
        );
    }

    /// The connection refusal 0.2.3 records, as it formats it.
    const CONNECTION_REFUSAL: &str = "could not configure the daemon connection: The running cagents service uses protocol 202; live updates support only protocol 196 (0.2.2) and protocol 198. This source check did not request preparation or shutdown, or create an update checkpoint. Choose a newer release for the cold install. To cold-install safely: save your work, finish or close every hosted session, and quit every cagents window. Using the running build's matching CLI, run `cagents --no-start daemon stop --require-empty` (it refuses while sessions remain), then verify `cagents --no-start daemon status` reports stopped. Run the downloaded release's `sh /path/to/install.sh` without --restart-daemon, then reopen cagents.";

    const HANDSHAKE_REFUSAL: &str =
        "The confirmed successor does not serve this update's matched protocol";

    /// What 0.2.3 appends to a recorded failure, in its order: the recovery
    /// and prepared reports, the verified service once it restarted it, then
    /// the paused report and what still runs.
    const RECORD_REPORTS: &str = "\nThe terminal service accepted the stop.\n2 background tasks stopped; they do not resume automatically.\nEnded or stopped:\n  build: ended with the update\n\nCurrent turns interrupted:\n  review: current turn interrupted; resume it from its tab\n1 background task stopped; it does not resume automatically.\n\nInterruption was not confirmed:\n  notes: check its current turn in the running daemon\n";
    const WORKER_REPORTS: &str = "\nPaused after update recovery:\n  review: open its tab to continue\n\nStill running, not attached:\nnpm run dev (pid 4242)\nStill running; attachment could not be checked:\ncargo watch: rebuilding";

    fn failed_with(message: &str) -> UpdateJob {
        UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "0.2.4".into(),
            phase: UpdatePhase::Failed,
            message: message.into(),
            log_path: "/nonexistent/update.log".into(),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        }
    }

    #[test]
    fn only_a_protocol_checking_workers_failure_is_reconciled() {
        for refusal in [HANDSHAKE_REFUSAL, CONNECTION_REFUSAL] {
            let mut job = failed_with(refusal);
            assert_eq!(
                protocol_refusal_reports(&job).as_deref(),
                Some(""),
                "{refusal}"
            );
            job.recorded_by = Some("0.2.4".into());
            assert!(!protocol_check_could_have_failed_it(&job), "{refusal}");
        }
        // Failures with a cause of their own stand, from a worker of any
        // release: windows that did not reopen, an installer that failed
        // after committing, a helper that ended early, a foreign service, or
        // a refusal that is not where the worker's record opens.
        for message in [
            "The daemon is ready and its original inventory was verified, but some saved windows did not reopen: no display. Run `cagents ui` to reopen the saved workspace.",
            "installer failed (exit status: 1)",
            "Update helper exited before completion",
            "The terminal service does not belong to the confirmed installation; restart it from the installed release.",
            "could not configure the daemon connection: connection refused",
            "installer failed (exit status: 1) The confirmed successor does not serve this update's matched protocol",
        ] {
            assert!(
                !protocol_check_could_have_failed_it(&failed_with(message)),
                "{message}"
            );
        }
        // 0.2.3 appends a later failure to the refusal after a space; it
        // stands, wherever among the reports it lands.
        let could_not_record = " The daemon is Ready and its original inventory was verified, but the update could not record recovery: No space left on device (os error 28). Run `cagents --no-start daemon status` and `cagents --no-start terminal list`; repair the update-state directory permissions or free disk space before retrying.";
        let start_failed = " The daemon is Ready and its original inventory was verified, but its startup command reported failure: exit status: 1. Run `cagents --no-start daemon status` and inspect the update log before retrying.";
        for refusal in [HANDSHAKE_REFUSAL, CONNECTION_REFUSAL] {
            for message in [
                format!("{refusal}{start_failed}"),
                format!("{refusal}{RECORD_REPORTS}{start_failed}"),
                format!("{refusal}{RECORD_REPORTS}{SERVICE_VERIFIED}{could_not_record}"),
                format!("{refusal}{SERVICE_VERIFIED}{could_not_record}{WORKER_REPORTS}"),
                format!("{refusal}: protocol 202"),
                format!("{refusal}\nThe update stopped."),
            ] {
                assert!(
                    !protocol_check_could_have_failed_it(&failed_with(&message)),
                    "{message}"
                );
            }
        }
        // What 0.2.3 workers record is read without the field.
        let recorded: UpdateJob = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(), "version": "0.2.4", "phase": "Failed", "message": "",
            "log_path": "/nonexistent/update.log",
        }))
        .unwrap();
        assert_eq!(recorded.recorded_by, None);
    }

    /// Settling keeps what the worker reported after its refusal: the
    /// sessions it ended, interrupted, paused or left running.
    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn a_settled_update_keeps_the_workers_reports() {
        for (refusal, verified) in [
            (HANDSHAKE_REFUSAL, ""),
            (CONNECTION_REFUSAL, SERVICE_VERIFIED),
        ] {
            for (record, kept) in [
                (
                    format!("{refusal}{RECORD_REPORTS}{verified}{WORKER_REPORTS}"),
                    format!("{RECORD_REPORTS}{WORKER_REPORTS}"),
                ),
                (format!("{refusal}{verified}"), String::new()),
                (
                    format!("{refusal}{verified}{WORKER_REPORTS}"),
                    WORKER_REPORTS.to_owned(),
                ),
            ] {
                let job = failed_with(&record);
                assert_eq!(
                    protocol_refusal_reports(&job).as_ref(),
                    Some(&kept),
                    "{record}"
                );
                let completed = completed_presentation(job);
                assert_eq!(completed.phase, UpdatePhase::Complete);
                assert_eq!(
                    completed.message,
                    format!("Update installed; cagents 0.2.4 verified its terminal service{kept}")
                );
            }
        }
    }

    #[cfg(feature = "ui")]
    mod settled {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        const JOB: &str = "467204f6-b633-4600-b92d-afd4a95547f8";

        struct State {
            _directory: tempfile::TempDir,
            path: PathBuf,
            fixture: PathBuf,
        }

        impl State {
            fn new() -> Self {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join(".update");
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                let state = Self {
                    _directory: directory,
                    path,
                    fixture: Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/update-e8bbb84f2-reached-target"),
                };
                state.write(
                    recovery::census_path(&state.path, JOB),
                    &fs::read(state.fixture.join("census.json")).unwrap(),
                );
                state
            }
            fn write(&self, path: PathBuf, bytes: &[u8]) {
                fs::write(&path, bytes).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            fn receipt(&self, name: &str) {
                self.write(
                    recovery::outcome_path(&self.path, JOB),
                    &fs::read(self.fixture.join(name)).unwrap(),
                );
            }
            fn recorded(&self) -> UpdateJob {
                serde_json::from_slice(&fs::read(self.path.join("status.json")).unwrap()).unwrap()
            }
            fn serving(&self) -> crate::model::HostSessionProof {
                let recovery::Reading::Usable(proof) =
                    recovery::read_receipt(&self.path, JOB).unwrap()
                else {
                    panic!("the fixture receipt is usable");
                };
                proof
            }
        }

        fn failed() -> UpdateJob {
            UpdateJob {
                id: JOB.into(),
                version: "0.2.4".into(),
                phase: UpdatePhase::Failed,
                message: "The confirmed successor does not serve this update's matched protocol"
                    .into(),
                log_path: "/nonexistent/update.log".into(),
                confirmation: None,
                refusal: None,
                in_flight: Vec::new(),
                recorded_by: None,
            }
        }

        /// The old worker's lease and placeholder, then its usable receipt.
        #[test]
        fn the_placeholder_then_the_usable_receipt_settle_the_job_complete() {
            let state = State::new();
            let job = failed();
            write_job(&state.path, &job).unwrap();
            let worker = acquire_lock(&state.path).unwrap();
            state.receipt("pending.json");
            let read = target_evidence(&state.path, JOB).unwrap();
            assert_eq!(read.worker, Worker::Running);
            assert!(matches!(read.receipt, recovery::Reading::Pending { .. }));
            assert_eq!(
                judge_target_failure(&read, SystemTime::now()),
                TargetFailure::Finishing
            );
            let shown = present_for_ui(job.clone(), |_| Ok(Reconciled::Finishing)).unwrap();
            assert!(!shown.phase.is_terminal(), "the window keeps polling");

            state.receipt("recovery.json");
            drop(worker);
            let read = target_evidence(&state.path, JOB).unwrap();
            assert_eq!(read.worker, Worker::Finished);
            let TargetFailure::Reached(proof) = judge_target_failure(&read, SystemTime::now())
            else {
                panic!("the usable receipt of a finished worker decides");
            };
            let settled = present_for_ui(job.clone(), |_| {
                Ok(Reconciled::Completed {
                    proof: proof.clone(),
                    state: state.path.clone(),
                })
            })
            .unwrap();
            assert_eq!(settled.phase, UpdatePhase::Complete);
            let recorded = state.recorded();
            assert_eq!(recorded.phase, UpdatePhase::Complete);
            assert_eq!(recorded.message, settled.message);
            assert!(recorded.refusal.is_none());
            assert!(state.path.join(format!("{JOB}.complete")).is_file());
            let history: serde_json::Value = serde_json::from_slice(
                &fs::read(state.path.join(format!("{JOB}.settled.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(history["recorded"]["phase"], "Failed");
            assert_eq!(history["recorded"]["message"], job.message.as_str());

            // Settled: the daemon that proved it may restart, the receipt
            // may no longer match; the record no longer depends on either.
            state.receipt("pending.json");
            let shown =
                present_for_ui(recorded, |_| panic!("a completed job is not re-judged")).unwrap();
            assert_eq!(shown.phase, UpdatePhase::Complete);
        }

        #[test]
        fn an_unproven_failure_stays_recorded_and_shown() {
            let state = State::new();
            let job = failed();
            write_job(&state.path, &job).unwrap();
            state.receipt("recovery.json");
            let proof = state.serving();
            // The proof did not hold: the daemon could not be confirmed.
            let shown = present_for_ui(job.clone(), |_| Err(invalid("no daemon"))).unwrap();
            assert_eq!(shown.phase, UpdatePhase::Failed);
            assert_eq!(shown.message, job.message);
            assert_eq!(state.recorded().phase, UpdatePhase::Failed);

            // A worker still holding the lease records its own verdict; a
            // window judged before it took the lease keeps asking.
            let worker = acquire_lock(&state.path).unwrap();
            assert!(!settle_as_completed(&state.path, &job, &proof).unwrap());
            let completed = || {
                Ok(Reconciled::Completed {
                    proof: proof.clone(),
                    state: state.path.clone(),
                })
            };
            let shown = present_for_ui(job.clone(), |_| completed()).unwrap();
            assert!(!shown.phase.is_terminal(), "{shown:?}");
            assert_eq!(state.recorded().phase, UpdatePhase::Failed);
            drop(worker);

            // A settlement that cannot be recorded is not shown.
            let gone = state.path.join("gone");
            let shown = present_for_ui(job.clone(), |_| {
                Ok(Reconciled::Completed {
                    proof: proof.clone(),
                    state: gone.clone(),
                })
            })
            .unwrap();
            assert_eq!(shown.phase, UpdatePhase::Failed);
            assert_eq!(shown.message, job.message);

            // A receipt that changed since the judgement proves nothing.
            state.receipt("pending.json");
            assert!(!settle_as_completed(&state.path, &job, &proof).unwrap());
            assert_eq!(state.recorded().phase, UpdatePhase::Failed);

            // Nor does a job rewritten since.
            state.receipt("recovery.json");
            let mut later = job.clone();
            later.message.push_str(" Retried.");
            write_job(&state.path, &later).unwrap();
            assert!(!settle_as_completed(&state.path, &job, &proof).unwrap());
            assert_eq!(state.recorded().message, later.message);
            assert!(!state.path.join(format!("{JOB}.complete")).exists());
        }
    }

    /// The target's daemon settles an older worker's false failure once that
    /// worker finishes, from its own receipt, without a window.
    #[cfg(feature = "host")]
    #[test]
    fn the_targets_daemon_settles_an_older_workers_failure() {
        use std::os::unix::fs::PermissionsExt;
        const JOB: &str = "467204f6-b633-4600-b92d-afd4a95547f8";
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().canonicalize().unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            b"cagents install root\n",
        )
        .unwrap();
        let bin = prefix.join("versions/0.2.4/bin");
        fs::create_dir_all(&bin).unwrap();
        for executable in ["cagents", "cagents-ui", "cagentsd"] {
            fs::write(bin.join(executable), b"fixture").unwrap();
        }
        std::os::unix::fs::symlink("versions/0.2.4", prefix.join("current")).unwrap();
        let layout = locate(&bin.join("cagentsd")).unwrap();
        let state = prefix.join(".update");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/update-e8bbb84f2-reached-target");
        let write = |path: PathBuf, name: &str| {
            fs::write(&path, fs::read(fixture.join(name)).unwrap()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        };
        write(recovery::census_path(&state, JOB), "census.json");
        write(recovery::outcome_path(&state, JOB), "pending.json");
        let mut job = UpdateJob {
            id: JOB.into(),
            version: "0.2.4".into(),
            phase: UpdatePhase::Installing,
            message: "Checkpointing sessions and switching the installation".into(),
            log_path: state.join(format!("{JOB}.log")),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        write_job(&state, &job).unwrap();
        let recorded = || -> UpdateJob {
            serde_json::from_slice(&fs::read(state.join("status.json")).unwrap()).unwrap()
        };
        write(recovery::outcome_path(&state, JOB), "recovery.json");
        let serving = match recovery::read_receipt(&state, JOB).unwrap() {
            recovery::Reading::Usable(serving) => serving,
            other => panic!("{other:?}"),
        };
        write(recovery::outcome_path(&state, JOB), "pending.json");

        // The old worker installed this daemon and is still running.
        let worker = acquire_lock(&state).unwrap();
        let step = || successor_settle_step(&layout, &state, &serving).unwrap();
        assert_eq!(step(), SettleStep::Wait);
        job.phase = UpdatePhase::Failed;
        job.message =
            "The confirmed successor does not serve this update's matched protocol".into();
        write_job(&state, &job).unwrap();
        assert_eq!(step(), SettleStep::Wait);
        drop(worker);

        // It left its placeholder: nothing proved the service.
        assert_eq!(step(), SettleStep::Done);
        assert_eq!(recorded().phase, UpdatePhase::Failed);
        // Its receipt names a daemon that has since restarted.
        write(recovery::outcome_path(&state, JOB), "recovery.json");
        let mut restarted = serving.clone();
        restarted.host_incarnation = crate::model::HostIncarnation::new("restarted");
        assert_eq!(
            successor_settle_step(&layout, &state, &restarted).unwrap(),
            SettleStep::Done
        );
        assert_eq!(recorded().phase, UpdatePhase::Failed);
        // A worker that records its release reports its own verdict.
        job.recorded_by = Some("0.2.4".into());
        write_job(&state, &job).unwrap();
        assert_eq!(step(), SettleStep::Done);
        assert_eq!(recorded().phase, UpdatePhase::Failed);
        job.recorded_by = None;
        write_job(&state, &job).unwrap();

        // Its receipt names this daemon: the update completed.
        assert_eq!(step(), SettleStep::Settled(JOB.into()));
        let settled = recorded();
        assert_eq!(settled.phase, UpdatePhase::Complete);
        assert_eq!(settled.version, "0.2.4");
        assert!(settled.refusal.is_none());
        assert!(
            !serde_json::to_value(&settled)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("refusal")
        );
        let history: serde_json::Value =
            serde_json::from_slice(&fs::read(state.join(format!("{JOB}.settled.json"))).unwrap())
                .unwrap();
        assert_eq!(history["recorded"]["message"], job.message.as_str());
        assert!(state.join(format!("{JOB}.complete")).is_file());
        // Settled for good: a later daemon finds nothing to do.
        assert_eq!(
            successor_settle_step(&layout, &state, &restarted).unwrap(),
            SettleStep::Done
        );
        assert_eq!(recorded().phase, UpdatePhase::Complete);
    }
}
