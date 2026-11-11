//! Explicit, self-managed release updates. `gh` owns authentication; discovery
//! never writes installation state. The detached worker alone may run an installer.
mod cancellation;
mod candidate;
mod confirmation;
mod floor;
mod floor_display;
mod floor_prediction;
mod handoff;
mod installed;
mod pending_recovery;
pub(crate) mod recovery;
#[cfg(all(feature = "ui", feature = "development"))]
mod review_fixture;
pub use recovery::TargetRecoveryRequired;
pub(crate) mod source;
pub use confirmation::{Choice as UpdateChoice, Confirmation as UpdateConfirmation, decide};
pub mod impact;
mod review;
pub mod source_inspection;
pub use review::render_review;
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
    TargetRecoveryRequired(#[from] recovery::TargetRecoveryRequired),
    #[error("Update cancelled")]
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
    ReleaseFromTerminal(Release),
    Installed { root: PathBuf },
    InstalledFromTerminal { root: PathBuf },
}

pub fn apply(release: &Release) -> Result<UpdateJob, UpdateError> {
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
    let candidate = locate(executable)?;
    if candidate.prefix != layout.prefix
        || executable.canonicalize()?
            != candidate
                .version_root
                .join("bin/cagents-ui")
                .canonicalize()?
    {
        return Err(invalid(
            "the installed candidate changed; inspect the installation again",
        ));
    }
    // The selected build owns the review and worker before it becomes current.
    let candidate_root = candidate.version_root;
    installation.version = installed_entry_worker(&layout, &candidate_root)?;
    let version = candidate_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(unsupported_install)?
        .to_owned();
    start_job(
        &installation,
        &version,
        &if terminal {
            UpdateRequest::InstalledFromTerminal {
                root: candidate_root,
            }
        } else {
            UpdateRequest::Installed {
                root: candidate_root,
            }
        },
    )
}

fn installed_entry_worker(window: &InstallLayout, candidate: &Path) -> Result<String, UpdateError> {
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
        candidate,
        owner.as_ref(),
        source.protocol,
        source.proof.as_ref(),
    )
}

/// Return the selected candidate's version after source admission and the no-op
/// check. The candidate updater rechecks the source and its installed owner.
fn installed_candidate_version(
    window: &InstallLayout,
    candidate: &Path,
    daemon: Option<&InstallLayout>,
    source_protocol: u16,
    source_proof: Option<&crate::model::HostSessionProof>,
) -> Result<String, UpdateError> {
    if !source::is_qualified_source(source_protocol) {
        return Err(source::unsupported_source(source_protocol));
    }
    let replaces_daemon = source_proof.is_some()
        && (source_protocol != crate::terminal_ipc::PROTOCOL_VERSION
            || daemon.is_some_and(|owner| owner.version_root != candidate));
    if candidate == window.version_root
        && !replaces_daemon
        && served_version_root(window)? == candidate
    {
        return Err(invalid("the installed application is already running"));
    }
    candidate
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
/// used by the application. The explicitly invoked build supplies the worker.
pub fn upgrade_installed(yes: bool, interrupt: bool) -> Result<UpdateJob, UpdateError> {
    let _interrupt = cancellation::Interrupt::install()?;
    // An interrupted preparation has its own immutable source census. Do not
    // route this retry through old `current` or construct a fresh empty source.
    recovery::inspect_pending_current()?;
    let layout = locate(&std::env::current_exe()?)?;
    // An explicit versions/V CLI applies V without publishing it first.
    let candidate = layout.version_root.join("bin/cagents-ui");
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
    let tag = format!("v{parsed}");
    let selected = applicable_release(serde_json::from_slice(&metadata(
        &format!("repos/{REPOSITORY}/releases/tags/{tag}"),
        &CancellationToken::for_terminal(),
    )?)?)?;
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
                        "Waiting for these conversations to finish their turns:\n  {}\nCtrl-C cancels the update; nothing has been ended or stopped.",
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
                        return Err(UpdateError::Cancelled);
                    }
                    submitted.insert(confirmation.nonce.clone());
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
                            let _ =
                                terminal.decide(&job.id, &confirmation.nonce, UpdateChoice::Cancel);
                            return Err(error.into());
                        }
                    }
                };
                match choice {
                    OrdinaryChoice::Cancel => {
                        terminal.decide(&job.id, &confirmation.nonce, choice.into())?;
                        return Err(UpdateError::Cancelled);
                    }
                    OrdinaryChoice::InterruptNow => {
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

/// Keep historical diagnostics available to the CLI without showing a resolved
/// installation failure as a warning in every subsequently installed window.
#[cfg(feature = "ui")]
pub(crate) fn status_for_ui() -> Result<Option<UpdateJob>, UpdateError> {
    #[cfg(feature = "development")]
    if let Some(root) = std::env::var_os("CAGENTS_EVIDENCE_FIXTURE")
        && let Some(job) = review_fixture::read(Path::new(&root))?
    {
        return Ok(Some(job));
    }
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
    let settled_failure = job.phase == UpdatePhase::Failed && result.is_err();
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
        Err(_) if settled_failure => job.message.clone(),
        Err(error) => error.to_string(),
    };
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
    if let Err(error) = write_job(&state, &job) {
        if result.is_ok() {
            return Err(error);
        }
        // Preserve a typed recovery request even when its human-readable job
        // status cannot be recorded. The caller still owns handoff custody.
        eprintln!("Could not record update result: {error}");
    }
    result
}

fn perform_update(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    survivors: &mut floor::Survivors,
) -> Result<(), UpdateError> {
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
    let confirmed = confirmation::wait(state, job, |policy| {
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
    let install_result = (|| {
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
        survivors.authorize_successor(&target_daemon, Some(&daemon_digest), handoff.as_ref())?;
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
        if confirmed.replaces_daemon {
            survivors.verify_successor(&target_daemon)?;
        }
        Ok(())
    })();
    let recovery_result = finish_handoff(
        installation,
        state,
        job,
        &install_result,
        handoff,
        &recovery,
        survivors,
        &guard,
    );
    // Do not let a recovery UI autostart the frozen previous executable while
    // ownership or Prepared recovery is unresolved.
    recovery_result?;
    let reopen_result = guard
        .relaunch_closed_windows(&installation.prefix.join("current/bin/cagents-ui"))
        .map_err(UpdateError::from);
    verify_reopened_service(&recovery, state, job)?;
    if let Err(error) = reopen_result {
        return report_recovery_failure(
            state,
            job,
            invalid(format!(
                "The daemon is ready and its original inventory was verified, but some saved windows did not reopen: {error}. Run `cagents ui` to reopen the saved workspace."
            )),
        );
    }
    install_result
}

fn verify_reopened_service(
    recovery: &recovery::Context,
    state: &Path,
    job: &mut UpdateJob,
) -> Result<(), UpdateError> {
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
    if receipt.usable() || matches!(receipt.outcome, recovery::Outcome::NoSource) {
        Ok(())
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
        eprintln!("{error}");
        job.phase = if matches!(error, UpdateError::Cancelled) {
            UpdatePhase::Cancelled
        } else {
            UpdatePhase::Failed
        };
        job.confirmation = None;
        job.message = error.to_string();
        if !job.message.contains("Ended or stopped:") {
            job.message.push_str(recovery_report);
        }
        if !job.message.contains("Current turns interrupted:") {
            job.message.push_str(prepared_report);
        }
        write_job(state, job)?;
        drop(handoff.take());
    }
    Ok(())
}

/// Socket evidence is separate from the lease metadata left by an old source.
enum CleanupDaemon {
    Ready,
    Absent,
    Unknown(String),
}

fn cleanup_daemon(paths: &crate::daemon::RuntimePaths, owns_handoff: bool) -> CleanupDaemon {
    let source = match source::Client::connect(&paths.socket) {
        Ok(source) => source,
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            // A handoff owns the old daemon's lease while its metadata can
            // still name an unreaped, exited source. That is a reservation,
            // not proof of a serving daemon. Other holders remain uncertain.
            return if owns_handoff {
                CleanupDaemon::Absent
            } else {
                match crate::daemon::HostLease::is_held(paths) {
                    Ok(false) => CleanupDaemon::Absent,
                    Ok(true) => CleanupDaemon::Unknown(
                        "the daemon lease is held without an answering service".into(),
                    ),
                    Err(error) => CleanupDaemon::Unknown(error.to_string()),
                }
            };
        }
        Err(error) => return CleanupDaemon::Unknown(error.to_string()),
    };
    let source::Client::Current { mut client, peer } = source else {
        return CleanupDaemon::Unknown("the answering service uses an older protocol".into());
    };
    if client.protocol() != crate::terminal_ipc::PROTOCOL_VERSION {
        return CleanupDaemon::Unknown("the answering service uses a different protocol".into());
    }
    let verified = (|| {
        peer.verify(client.proof())?;
        let state = client
            .startup_state()
            .map_err(|error| invalid(error.to_string()))?;
        peer.verify(client.proof())?;
        if state != crate::terminal_ipc::StartupState::Ready {
            return Err(invalid(
                "the answering service has not finished restoring its panes",
            ));
        }
        Ok(())
    })();
    match verified {
        Ok(()) => CleanupDaemon::Ready,
        Err(error) => CleanupDaemon::Unknown(error.to_string()),
    }
}

/// Settle the reservation before reopening clients. The worker, including a
/// headless update with no windows, owns restarting the selected installation.
fn finish_handoff(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    result: &Result<(), UpdateError>,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    recovery: &recovery::Context,
    survivors: &mut floor::Survivors,
    guard: &crate::instance::UiUpdateGuard,
) -> Result<(), UpdateError> {
    let needed =
        recovery.shutdown_requested || handoff.is_some() || survivors.needs_daemon_recovery();
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
        return Ok(());
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
            job.message = "The application files are installed. No daemon was serving before the update, and none is running now; the daemon will start at graphical login.".into();
            recording_errors.extend(write_job(state, job).err().map(|e| e.to_string()));
        }
        return finish_recording_recovery(state, job, recording_errors);
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
        return finish_recording_recovery(state, job, recording_errors);
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
                UpdateError::TargetRecoveryRequired(required),
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
        job.message.push_str(
            " The terminal service is ready and its original terminal inventory was verified.",
        );
        recording_errors.extend(write_job(state, job).err().map(|e| e.to_string()));
        finish_recording_recovery(state, job, recording_errors)
    } else {
        if let Err(error) = verified.save(&outcome_path) {
            eprintln!("Could not save recovery proof: {error}");
        }
        report_recovery_failure(state, job, verified.failure())
    }
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
            census.verify()
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

fn report_recovery_failure(
    state: &Path,
    job: &mut UpdateJob,
    error: UpdateError,
) -> Result<(), UpdateError> {
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
    bounded_command_output(command, operation, limit, timeout, cancellation, None)
}

const MAINTENANCE_STDERR_LIMIT: usize = 16 * 1024;

/// Only matched, internal store-maintenance commands opt into diagnostics.
/// The update worker's stderr is its private job log; never log Command's
/// Debug representation, which includes environment overrides.
fn bounded_maintenance_command(
    command: &mut Command,
    operation: &'static str,
    limit: u64,
    timeout: Duration,
    cancellation: &CancellationToken,
    log: &mut dyn Write,
) -> Result<File, UpdateError> {
    let argv: Vec<_> = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let timestamp = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    };
    let _ = writeln!(
        log,
        "{}",
        serde_json::json!({
            "phase": "maintenance-start", "at_unix_ms": timestamp(),
            "argv": argv, "stderr_limit_bytes": MAINTENANCE_STDERR_LIMIT,
        })
    );
    let mut stderr = MaintenanceStderr::default();
    let result = bounded_command_output(
        command,
        operation,
        limit,
        timeout,
        cancellation,
        Some(&mut stderr),
    );
    let _ = writeln!(
        log,
        "{}",
        serde_json::json!({
            "phase": "maintenance-end", "at_unix_ms": timestamp(), "argv": argv,
            "status": stderr.status.map(|status| status.to_string()),
            "outcome": result.as_ref().map(|_| "success".to_owned()).unwrap_or_else(|error| error.to_string()),
            "stderr_limit_bytes": MAINTENANCE_STDERR_LIMIT,
            "stderr_truncated": stderr.truncated,
            "stderr": String::from_utf8_lossy(&stderr.bytes),
        })
    );
    result
}

#[derive(Default)]
struct MaintenanceStderr {
    bytes: Vec<u8>,
    truncated: bool,
    eof: bool,
    status: Option<std::process::ExitStatus>,
}

impl MaintenanceStderr {
    /// Drain beyond the retained prefix too, so a verbose child cannot block
    /// on stderr while we wait for stdout or its exit status.
    fn read(&mut self, pipe: &mut std::process::ChildStderr) -> io::Result<bool> {
        let mut buffer = [0u8; 64 * 1024];
        match pipe.read(&mut buffer) {
            Ok(0) => {
                self.eof = true;
                Ok(false)
            }
            Ok(length) => {
                let retained = length.min(MAINTENANCE_STDERR_LIMIT - self.bytes.len());
                self.bytes.extend_from_slice(&buffer[..retained]);
                self.truncated |= retained != length;
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn first_line(&self) -> Option<String> {
        let raw = String::from_utf8_lossy(&self.bytes);
        let mut characters = raw.chars();
        let mut text = String::new();
        while let Some(character) = characters.next() {
            match character {
                // Discard complete ANSI sequences, including their payload,
                // so a color reset or terminal title is not the error line.
                '\u{1b}' => match characters.next() {
                    Some('[') => {
                        for character in characters.by_ref() {
                            if ('@'..='~').contains(&character) {
                                break;
                            }
                        }
                    }
                    Some(']' | 'P' | '_' | '^') => {
                        let mut escape = false;
                        for character in characters.by_ref() {
                            if character == '\u{7}' || (escape && character == '\\') {
                                break;
                            }
                            escape = character == '\u{1b}';
                        }
                    }
                    Some(' '..='/') => {
                        for character in characters.by_ref() {
                            if ('0'..='~').contains(&character) {
                                break;
                            }
                        }
                    }
                    _ => (),
                },
                '\n' => text.push('\n'),
                '\t' => text.push(' '),
                character
                    if !character.is_control()
                        && !matches!(character,
                    '\u{061C}' | '\u{200E}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') =>
                {
                    text.push(character)
                }
                _ => (),
            }
        }
        text.lines().find_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| line.chars().take(512).collect())
        })
    }
}

fn bounded_command_output(
    command: &mut Command,
    operation: &'static str,
    limit: u64,
    timeout: Duration,
    cancellation: &CancellationToken,
    mut stderr: Option<&mut MaintenanceStderr>,
) -> Result<File, UpdateError> {
    if cancellation.cancelled() {
        return Err(UpdateError::Cancelled);
    }
    let mut file = anonymous_file()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if stderr.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .process_group(0);
    let child = command
        .spawn()
        .map_err(|error| invalid(format!("cannot start {operation}: {error}")))?;
    let mut child = StreamingChild::new(child)?;
    let mut pipe = child.child.stdout.take().expect("piped stdout");
    let mut error_pipe = child.child.stderr.take();
    let mut nonblocking = false;
    let result = (|| {
        for fd in
            std::iter::once(pipe.as_raw_fd()).chain(error_pipe.as_ref().map(AsRawFd::as_raw_fd))
        {
            // SAFETY: flags are changed on the owned live pipe descriptors only.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        nonblocking = true;
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
            // Give both streams one bounded read each per iteration. A busy
            // stdout must not starve stderr (or timeout/cancellation checks).
            let mut progress = false;
            if let (Some(stderr), Some(pipe)) = (stderr.as_deref_mut(), error_pipe.as_mut()) {
                progress |= stderr.read(pipe)?;
            }
            match pipe.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(length) => {
                    total += length as u64;
                    if total > limit {
                        return Err(invalid(format!("{operation} exceeds its size limit")));
                    }
                    file.write_all(&buffer[..length])?;
                    progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
            if eof
                && stderr.as_ref().is_none_or(|stderr| stderr.eof)
                && let Some(status) = child.observed_exit_status()?
            {
                if let Some(stderr) = stderr.as_deref_mut() {
                    stderr.status = Some(status);
                }
                if !status.success() {
                    let detail = stderr
                        .as_ref()
                        .and_then(|stderr| stderr.first_line())
                        .map(|line| format!(": {line}"))
                        .unwrap_or_default();
                    return Err(invalid(format!("{operation} failed ({status}){detail}")));
                }
                break;
            }
            if !progress {
                thread::sleep(Duration::from_millis(20));
            }
        }
        seal_file(&file)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(())
    })();
    // Keep the original leader unreaped through failed status and sealing/seek
    // errors, since each still needs original-group cleanup. Success preserves
    // the existing policy of leaving independently running descendants alone.
    let cleanup = child.finish(result.is_err());
    if let Some(stderr) = stderr {
        stderr.status = child.final_status.or(stderr.status);
        // Preserve pending diagnostics on timeout/cancellation/size failure.
        // One nonblocking read is enough to fill the retained prefix, and is
        // bounded even if an independently running descendant keeps writing.
        if nonblocking && let Some(pipe) = error_pipe.as_mut() {
            let _ = stderr.read(pipe);
        }
    }
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
    final_status: Option<std::process::ExitStatus>,
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
            final_status: None,
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
                let status = status.ok_or_else(|| {
                    io::Error::other("streaming child final reap returned no status")
                })?;
                self.final_status = Some(status);
                Ok(status)
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
        let release = applicable_release(release("9.8.7")).unwrap();
        let job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: release.version.clone(),
            phase: UpdatePhase::AwaitingConfirmation,
            message: String::new(),
            log_path: directory.path().join("worker.log"),
            confirmation: None,
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
        let release = applicable_release(release("9.8.7")).unwrap();
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

    fn maintenance_records(log: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(log)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn maintenance_diagnostics_keep_failure_and_private_bounded_log() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("job.log");
        let mut log = private_create(&log_path).unwrap();
        let error = bounded_maintenance_command(
            Command::new("/bin/sh")
                .args(["-c", "printf '\\n\\007\\nstore lease refused\\033\\r\\nsecond detail\\n' >&2; exit 7", "daemon", "store-schema"])
                .env("CAGENTS_TEST_SECRET", "must-not-appear-in-diagnostics"),
            "Preparing terminal service recovery", 100, Duration::from_secs(2),
            &CancellationToken::new(), &mut log,
        ).unwrap_err();
        assert!(
            error.to_string().ends_with(": store lease refused"),
            "{error}"
        );
        assert!(!error.to_string().contains(char::is_control));
        assert_eq!(fs::metadata(&log_path).unwrap().mode() & 0o777, 0o600);
        let bytes = fs::read(log_path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("must-not-appear"));
        let records = maintenance_records(&bytes);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["phase"], "maintenance-start");
        assert_eq!(records[1]["phase"], "maintenance-end");
        assert_eq!(records[0]["argv"], records[1]["argv"]);
        assert_eq!(records[0]["argv"][4], "store-schema");
        assert!(
            records[0]["at_unix_ms"].as_u64().unwrap()
                <= records[1]["at_unix_ms"].as_u64().unwrap()
        );
        assert!(records[1]["status"].as_str().unwrap().contains('7'));
        assert!(
            records[1]["stderr"]
                .as_str()
                .unwrap()
                .contains("second detail")
        );
        assert_eq!(records[1]["stderr_truncated"], false);
        let quiet = bounded_command(
            Command::new("/bin/sh").args(["-c", "echo authentication-detail >&2; exit 7"]),
            "GitHub request",
            100,
            Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(!quiet.to_string().contains("authentication-detail"));
    }

    #[test]
    fn maintenance_diagnostics_drain_oversized_dual_streams_without_deadlock() {
        let mut log = Vec::new();
        let mut output = bounded_maintenance_command(
            Command::new("/usr/bin/python3").args([
                "-c",
                r#"import os, threading
def emit(fd, byte):
 for _ in range(256): os.write(fd, byte * 4096)
a=threading.Thread(target=emit,args=(1,b'o'))
a.start()
emit(2,b'e')
a.join()
"#,
            ]),
            "maintenance fixture",
            2 * 1024 * 1024,
            Duration::from_secs(5),
            &CancellationToken::new(),
            &mut log,
        )
        .unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![b'o'; 1024 * 1024]);
        assert!(output.write_all(b"sealed").is_err());
        let records = maintenance_records(&log);
        assert_eq!(
            records[1]["stderr"].as_str().unwrap(),
            "e".repeat(MAINTENANCE_STDERR_LIMIT)
        );
        assert_eq!(records[1]["stderr_limit_bytes"], MAINTENANCE_STDERR_LIMIT);
        assert_eq!(records[1]["stderr_truncated"], true);
        assert_eq!(records[1]["outcome"], "success");
    }

    #[test]
    fn maintenance_diagnostics_preserve_timeout_and_cancellation_cleanup() {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let ready = directory.path().join("ready");
            let cancellation = CancellationToken::new();
            let signal = cancellation.clone();
            let marker = ready.clone();
            let canceller = cancel.then(|| {
                thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !marker.exists() && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                    signal.cancel();
                })
            });
            let mut log = Vec::new();
            let started = Instant::now();
            let result = bounded_maintenance_command(
                Command::new("/bin/sh")
                    .args([
                        "-c",
                        "printf 'waiting for store\\n' >&2; echo $$ > \"$1\"; exec sleep 30",
                        "fixture",
                    ])
                    .arg(&ready),
                "maintenance fixture",
                100,
                if cancel {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(500)
                },
                &cancellation,
                &mut log,
            );
            if let Some(canceller) = canceller {
                canceller.join().unwrap();
            }
            if cancel {
                assert!(matches!(result, Err(UpdateError::Cancelled)), "{result:?}");
            } else {
                assert!(
                    matches!(result, Err(UpdateError::Timeout("maintenance fixture"))),
                    "{result:?}"
                );
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            let pid: libc::pid_t = fs::read_to_string(ready).unwrap().trim().parse().unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            let records = maintenance_records(&log);
            assert_eq!(records[1]["stderr"], "waiting for store\n");
            assert!(records[1]["status"].as_str().unwrap().contains("signal"));
        }
    }

    #[test]
    fn maintenance_diagnostics_bound_failure_line_and_ignore_display_controls() {
        let stderr = MaintenanceStderr {
            bytes: format!("\n\t\u{7}\u{1b}[0m\n\u{1b}]0;private title\u{7}\n\u{1b}[31mrefused\u{1b}[0m\u{202e} {}", "界".repeat(1000)).into_bytes(),
            ..MaintenanceStderr::default()
        };
        let line = stderr.first_line().unwrap();
        assert!(line.starts_with("refused "));
        assert_eq!(line.chars().count(), 512);
        assert!(!line.contains('\u{202e}'));
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
        // Deliberately not a current, candidate-line or protocol77 envelope. Entry routing
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
        let directory = tempfile::tempdir().unwrap();
        install_tree(directory.path(), &["old", "new", "third"], "new");
        let layout = |version: &str| {
            locate(
                &directory
                    .path()
                    .join("versions")
                    .join(version)
                    .join("bin/cagents"),
            )
            .unwrap()
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
        // Even with no running daemon, invoking a staged build selects it;
        // the current link remains the old build until the worker succeeds.
        install_tree(directory.path(), &[], "old");
        assert_eq!(
            installed_candidate_version(
                &new,
                &new.version_root,
                None,
                crate::terminal_ipc::PROTOCOL_VERSION,
                None
            )
            .unwrap(),
            "new"
        );
        assert_eq!(served_version_root(&new).unwrap(), old.version_root);
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
    pub(super) fn install_tree(root: &Path, versions: &[&str], served: &str) {
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
                "absent-source-with-prepared-checkpoint",
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
                        ClientRequest::List => ServerResponse::Listed { terminals: vec![] },
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
    expected = 'Installing' if os.environ['CAGENTS_TEST_RECOVERY_FAILURE']=='installer-success-daemon-exited' else 'Failed'
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
            "first-start-failed-without-source" | "absent-source-with-prepared-checkpoint"
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
        let installer_result = if matches!(
            failure.as_str(),
            "installer-success-daemon-exited" | "first-start-failed-without-source"
        ) {
            write_job(root.path(), &job).unwrap();
            Ok(())
        } else {
            Err(invalid(failure.clone()))
        };
        let settled = finish_handoff(
            &installation,
            root.path(),
            &mut job,
            &installer_result,
            &mut lease,
            &context,
            &mut floor::Survivors::default(),
            &guard,
        );
        if failure == "first-start-failed-without-source" {
            assert!(settled.is_ok(), "{settled:?}");
            assert!(job.message.contains("will start at graphical login"));
            assert!(!root.path().join("start-attempts").exists());
            let receipt: recovery::Receipt = serde_json::from_slice(
                &fs::read(recovery::outcome_path(root.path(), &job.id)).unwrap(),
            )
            .unwrap();
            assert!(matches!(receipt.outcome, recovery::Outcome::NoSource));
            assert!(!receipt.usable());
            verify_reopened_service(&context, root.path(), &mut job).unwrap();
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
        assert_eq!(
            fs::read_to_string(root.path().join("start-attempts")).unwrap(),
            "attempt\n"
        );
        if expected_failure {
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
