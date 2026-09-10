//! Explicit, self-managed release updates. `gh` owns authentication; discovery
//! never writes installation state. The detached worker alone may run an installer.
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
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
    ClosingWindows,
    Installing,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateJob {
    pub id: String,
    pub version: String,
    pub phase: UpdatePhase,
    pub message: String,
    pub log_path: PathBuf,
}

#[derive(Deserialize)]
struct GitHubRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
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
        stable_version(version)?;
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
            || prefix.join("current").canonicalize()? != version_root
            || !bin.join("cagents").is_file()
            || !bin.join("cagents-ui").is_file()
            || !bin.join("cagentsd").is_file()
        {
            return Err(unsupported_install());
        }
        // Source builds copied into an old release tree must not impersonate it.
        if version != env!("CARGO_PKG_VERSION") {
            return Err(invalid(
                "installed directory and running binary versions differ; reinstall manually",
            ));
        }
        let prefix_meta = fs::metadata(prefix)?;
        if prefix_meta.uid() != unsafe { libc::geteuid() } || prefix_meta.mode() & 0o022 != 0 {
            return Err(invalid("installation root is not privately owned"));
        }
        Ok(Self {
            prefix: prefix.to_owned(),
            version: version.to_owned(),
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
    let current_version = stable_version(current)?;
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
pub fn apply(release: &Release) -> Result<UpdateJob, UpdateError> {
    let installation = Installation::detect()?;
    if stable_version(&release.version)? <= stable_version(&installation.version)? {
        return Err(invalid(
            "selected release is not newer than the running installation",
        ));
    }
    let state = installation.state_dir()?;
    let lock = acquire_lock(&state)?;
    let id = uuid::Uuid::new_v4().to_string();
    let job = UpdateJob {
        id: id.clone(),
        version: release.version.clone(),
        phase: UpdatePhase::Starting,
        message: "Starting verified update".into(),
        log_path: state.join(format!("{id}.log")),
    };
    let log = private_create(&job.log_path)?;
    write_job(&state, &job)?;
    let request = private_create(&state.join(format!("{id}.request")))?;
    serde_json::to_writer(&request, release)?;
    request.sync_all()?;
    let mut command = Command::new(installation.prefix.join("current/bin/cagents"));
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
    let installation = Installation::detect()?;
    let state = installation.prefix.join(".update");
    if !state.exists() {
        return Ok(None);
    }
    let state = installation.state_dir()?;
    let mut job: UpdateJob = match read_private(&state.join("status.json"), SMALL_ASSET_LIMIT) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !matches!(job.phase, UpdatePhase::Complete | UpdatePhase::Failed) {
        match acquire_lock(&state) {
            Ok(_lock) => {
                // A worker may have completed (or a later job may have run)
                // between the initial snapshot and acquiring this lease.
                job = serde_json::from_slice(&read_private(
                    &state.join("status.json"),
                    SMALL_ASSET_LIMIT,
                )?)?;
                if !matches!(job.phase, UpdatePhase::Complete | UpdatePhase::Failed) {
                    job.phase = UpdatePhase::Failed;
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

/// Hidden CLI entrypoint. Only a worker spawned by `apply` has the inherited
/// lease. Re-fetches the selected release from the fixed repository before use.
pub fn run_worker(job_id: &str) -> Result<(), UpdateError> {
    if uuid::Uuid::parse_str(job_id)
        .ok()
        .is_none_or(|id| id.to_string() != job_id)
    {
        return Err(invalid("invalid update job identifier"));
    }
    let installation = Installation::detect()?;
    let state = installation.state_dir()?;
    let _lock = inherited_lock(&state)?;
    let mut job: UpdateJob = serde_json::from_slice(&read_private(
        &state.join("status.json"),
        SMALL_ASSET_LIMIT,
    )?)?;
    if job.id != job_id || job.phase != UpdatePhase::Starting {
        return Err(invalid("update job is no longer pending"));
    }
    let result = perform_update(&installation, &state, &mut job);
    job.phase = if result.is_ok() {
        UpdatePhase::Complete
    } else {
        UpdatePhase::Failed
    };
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
    let selected: Release = serde_json::from_slice(&read_private(
        &state.join(format!("{}.request", job.id)),
        SMALL_ASSET_LIMIT,
    )?)?;
    let cancellation = CancellationToken::new();
    let endpoint = format!("repos/{REPOSITORY}/releases/{}", selected.id);
    let release = applicable_release(serde_json::from_slice(&metadata(
        &endpoint,
        &cancellation,
    )?)?)?;
    if release != selected
        || stable_version(&release.version)? <= stable_version(&installation.version)?
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
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing all application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    let windows = guard.prepare_and_close_windows(Duration::from_secs(45))?;
    let install_result = (|| {
        set_phase(
            state,
            job,
            UpdatePhase::Installing,
            "Checkpointing sessions and switching the installation",
        )?;
        run_installer(installation, &release, &installer, &archive, &checksums)
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

fn run_installer(
    installation: &Installation,
    release: &Release,
    installer: &File,
    archive: &File,
    checksums: &File,
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
        .arg("--restart-daemon")
        .env("CAGENTS_ARCHIVE", format!("/proc/self/fd/{}", files[1]))
        .env("CAGENTS_CHECKSUMS", format!("/proc/self/fd/{}", files[2]))
        .env("CAGENTS_REPOSITORY", REPOSITORY)
        .env_remove("ENV")
        .env_remove("BASH_ENV")
        .stdin(Stdio::null());
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
    let mut file = anonymous_file()?;
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
        .env_remove("GH_DEBUG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().map_err(|error| {
        invalid(format!(
            "cannot start GitHub CLI; install and authenticate gh: {error}"
        ))
    })?;
    let mut pipe = child.stdout.take().expect("piped stdout");
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
                return Err(UpdateError::Timeout("GitHub request"));
            }
            match pipe.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(length) => {
                    total += length as u64;
                    if total > limit {
                        return Err(invalid("GitHub response exceeds its size limit"));
                    }
                    file.write_all(&buffer[..length])?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
            if eof && let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(invalid(format!(
                        "GitHub request failed ({status}); check gh authentication and connectivity"
                    )));
                }
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        seal_file(&file)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: this child created its own process group. Kill descendants too
        // so cancellation/timeouts cannot strand a credential helper or pipe.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
    }
    result?;
    Ok(file)
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
    use super::*;
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
