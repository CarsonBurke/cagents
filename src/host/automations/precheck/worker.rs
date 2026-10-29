//! One bounded, private precheck owner. The supervisor has no runtime or waiter
//! threads; only it reaps descendants. A second helper runs the existing bounded
//! executor, whose waiter threads must never race the supervisor's PID checks.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Outcome;
use crate::{
    automations::{FiringState, MAX_PRECHECK_SECONDS, Precheck},
    process::{CancellationToken, SystemProcessExecutor},
};

const SUPERVISOR: &str = "--automation-precheck-supervisor";
const EXECUTOR: &str = "--automation-precheck-executor";
const MAX_RECORD: u64 = 128 * 1024;
const CLEANUP_WINDOW: Duration = Duration::from_secs(2);
const RETIREMENT_VERSION: u32 = 1;
const MAX_RETIREMENT_RECORD: u64 = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retirement {
    version: u32,
    firing: Uuid,
    revision: u64,
    device: u64,
    inode: u64,
}

fn retirement_name(firing: Uuid) -> String {
    format!("{firing}.retired")
}

fn read_retirement(root: &Path, firing: Uuid) -> io::Result<Option<(Retirement, File)>> {
    crate::secure_file::reject_symlink_components(root)?;
    crate::secure_file::validate_owned_directory(root)?;
    if root.metadata()?.mode() & 0o077 != 0 {
        return Err(io::Error::other("Precheck retirement root is not private"));
    }
    let mut file = match private_file(&root.join(retirement_name(firing))) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_RETIREMENT_RECORD {
        return Err(io::Error::other(
            "Precheck retirement record exceeds its limit",
        ));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_RETIREMENT_RECORD + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RETIREMENT_RECORD as usize {
        return Err(io::Error::other(
            "Precheck retirement record exceeds its limit",
        ));
    }
    let record: Retirement = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if record.version != RETIREMENT_VERSION || record.firing != firing {
        return Err(io::Error::other(
            "Precheck retirement identity does not match",
        ));
    }
    Ok(Some((record, file)))
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::host::automations) struct Request {
    pub firing: Uuid,
    pub revision: u64,
    pub directory: PathBuf,
    pub target_identity: crate::automations::DirectoryIdentity,
    pub precheck: Precheck,
}

#[derive(Debug, Serialize, Deserialize)]
struct Started {
    firing: Uuid,
    revision: u64,
    boot: crate::model::BootId,
    pid: u32,
    start: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    firing: Uuid,
    revision: u64,
    outcome: Outcome,
}

pub(in crate::host::automations) enum Status {
    Running,
    Complete(Outcome),
    /// An unlocked lease without a receipt cannot prove that descendants died.
    CleanupUnconfirmed,
}

pub(in crate::host::automations) struct Worker {
    directory: PathBuf,
    firing: Uuid,
    revision: u64,
    child: Option<Child>,
    retirement: Option<Retirement>,
    retirement_unlinked: bool,
}

impl Worker {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn retirement_is_unlinked(&self) -> bool {
        self.retirement_unlinked
    }

    pub fn retirement_revision(root: &Path, firing: Uuid) -> io::Result<Option<u64>> {
        read_retirement(root, firing).map(|record| record.map(|(record, _)| record.revision))
    }

    fn original_directory(&self) -> io::Result<Option<fs::Metadata>> {
        match self.directory.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => Ok(Some(metadata)),
            Ok(_) => Err(io::Error::other(
                "Precheck retirement directory was replaced",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn launch(root: &Path, request: &Request) -> io::Result<Self> {
        validate_request(request)?;
        crate::secure_file::prepare_private_directory(root)?;
        match root
            .join(retirement_name(request.firing))
            .symlink_metadata()
        {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "This precheck still has durable retirement custody",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let directory = root.join(request.firing.to_string());
        // Never reuse a firing's files, even if a previous launch died before
        // its first receipt. Retrying an arbitrary precheck could charge twice.
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
            .create(&directory)?;
        create_record(&directory, "request", request)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("lease"))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        lease.sync_all()?;
        File::open(&directory)?.sync_all()?;
        let fd = lease.as_raw_fd();
        #[cfg(not(test))]
        let mut command = Command::new(std::env::current_exe()?);
        #[cfg(test)]
        let mut command = Command::new(test_helper_executable()?);
        command
            .args([SUPERVISOR])
            .arg(&directory)
            .arg(fd.to_string())
            .env_remove(crate::agent_execution::AUTHORITY_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // The supervisor and executor exec before launching the user's shell;
        // neither can recover the daemon's process-local saved limit afterward.
        crate::host::descriptor_limit::inherit(&mut command);
        unsafe {
            command.pre_exec(move || {
                // The dedicated owner keeps this lease across exec. It marks
                // it CLOEXEC again before creating its executor or shell.
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = match command.spawn() {
            Ok(child) => Some(child),
            Err(error) => {
                // spawn reports exec failure before any helper could launch
                // the user's command. This is a proved safe terminal result.
                create_record(
                    &directory,
                    "receipt",
                    &Receipt {
                        firing: request.firing,
                        revision: request.revision,
                        outcome: failed(format!("Precheck helper could not start: {error}")),
                    },
                )?;
                None
            }
        };
        drop(lease);
        Ok(Self {
            directory,
            firing: request.firing,
            revision: request.revision,
            child,
            retirement: None,
            retirement_unlinked: false,
        })
    }

    pub fn recover(root: &Path, firing: Uuid, revision: u64) -> Self {
        Self {
            directory: root.join(firing.to_string()),
            firing,
            revision,
            child: None,
            retirement: None,
            retirement_unlinked: false,
        }
    }

    pub fn cancel(&self) -> io::Result<()> {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join("cancel"))
        {
            Ok(file) => {
                file.sync_all()?;
                File::open(&self.directory)?.sync_all()
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub fn owns_child_for_test(&self) -> bool {
        self.child.is_some()
    }

    pub fn reaped(&mut self) -> io::Result<bool> {
        if let Some(child) = &mut self.child
            && child.try_wait()?.is_some()
        {
            self.child.take();
        }
        Ok(self.child.is_none())
    }

    /// The caller has proved in SQLite that this exact firing released its
    /// slot. Its durable result now replaces the on-disk receipt. Never remove
    /// an owner's directory while its process or inherited lease is live.
    pub fn retire(&mut self) -> io::Result<bool> {
        if !self.reaped()? {
            return Ok(false);
        }
        let root = self
            .directory
            .parent()
            .ok_or_else(|| io::Error::other("Precheck retirement has no owning root"))?;
        let retirement = read_retirement(root, self.firing)?;
        if retirement.as_ref().is_some_and(|(record, _)| {
            record.revision != self.revision
                || self
                    .retirement
                    .as_ref()
                    .is_some_and(|expected| expected != record)
        }) {
            return Err(io::Error::other(
                "Precheck retirement revision does not match",
            ));
        }
        let directory_identity = self.original_directory()?;
        if directory_identity.is_none() {
            if retirement.is_none() && self.retirement_unlinked {
                // This exact live owner already unlinked its receipt after
                // ACK; only the failed final parent fsync remains to retry.
                File::open(root)?.sync_all()?;
                return Ok(true);
            }
            let Some((record, file)) = retirement else {
                return Err(io::Error::other(
                    "Missing precheck directory has no retirement proof",
                ));
            };
            file.sync_all()?;
            File::open(root)?.sync_all()?;
            self.retirement = Some(record);
            return Ok(true);
        }
        crate::secure_file::reject_symlink_components(&self.directory)?;
        crate::secure_file::validate_owned_directory(&self.directory)?;
        let identity = directory_identity.expect("original directory was checked");
        if identity.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "Precheck retirement directory is not private",
            ));
        }
        if retirement.as_ref().is_some_and(|(record, _)| {
            (record.device, record.inode) != (identity.dev(), identity.ino())
        }) {
            return Err(io::Error::other(
                "Precheck retirement directory was replaced",
            ));
        }
        let lease = match private_file(&self.directory.join("lease")) {
            Ok(lease) => Some(lease),
            Err(error) if error.kind() == io::ErrorKind::NotFound && retirement.is_some() => None,
            Err(error) => return Err(error),
        };
        if lease.as_ref().is_some_and(|lease| unsafe {
            libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0
        }) {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(false)
            } else {
                Err(error)
            };
        }
        // Only our bounded, flat proof files are reclaimable. An unexpected
        // directory or symlink is never traversed as part of housekeeping.
        let entries = fs::read_dir(&self.directory)?
            .take(9)
            .collect::<io::Result<Vec<_>>>()?;
        if entries.len() > 8 {
            return Err(io::Error::other("Unexpected precheck file count"));
        }
        for entry in &entries {
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::other("Unexpected precheck file"))?;
            let known = matches!(name, "request" | "started" | "receipt" | "cancel" | "lease")
                || [".request-", ".started-", ".receipt-"]
                    .iter()
                    .any(|prefix| {
                        name.strip_prefix(prefix)
                            .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok())
                    });
            if !known {
                return Err(io::Error::other("Unexpected precheck file"));
            }
            private_file(&entry.path())?;
        }
        let expected = Retirement {
            version: RETIREMENT_VERSION,
            firing: self.firing,
            revision: self.revision,
            device: identity.dev(),
            inode: identity.ino(),
        };
        if retirement.is_none() {
            create_record(root, &retirement_name(self.firing), &expected)?;
        }
        // Publication may previously have reached rename but failed parent
        // fsync. Validate and sync that same receipt before any proof unlink.
        let (record, file) = read_retirement(root, self.firing)?
            .ok_or_else(|| io::Error::other("Precheck retirement proof disappeared"))?;
        if record != expected {
            return Err(io::Error::other(
                "Precheck retirement identity does not match",
            ));
        }
        file.sync_all()?;
        File::open(root)?.sync_all()?;
        let current = self.directory.symlink_metadata()?;
        if !current.is_dir() || (current.dev(), current.ino()) != (identity.dev(), identity.ino()) {
            return Err(io::Error::other(
                "Precheck retirement directory was replaced",
            ));
        }
        for entry in entries {
            if entry.file_name() != "lease" {
                fs::remove_file(entry.path())?;
            }
        }
        if lease.is_some() {
            fs::remove_file(self.directory.join("lease"))?;
        }
        fs::remove_dir(&self.directory)?;
        if let Some(parent) = self.directory.parent() {
            File::open(parent)?.sync_all()?;
        }
        self.retirement = Some(expected);
        Ok(true)
    }

    /// Only after the database ACK committed (or its firing was already
    /// compacted). Absence alone never authorizes producing that ACK.
    pub fn forget_retirement(&mut self) -> io::Result<()> {
        if !self.reaped()? || self.original_directory()?.is_some() {
            return Err(io::Error::other("Precheck retirement is not complete"));
        }
        let root = self
            .directory
            .parent()
            .ok_or_else(|| io::Error::other("Precheck retirement has no owning root"))?;
        let expected = self
            .retirement
            .as_ref()
            .ok_or_else(|| io::Error::other("Precheck retirement has no verified receipt"))?;
        let Some((record, file)) = read_retirement(root, self.firing)? else {
            // A prior unlink may have succeeded before its parent fsync failed.
            return File::open(root)?.sync_all();
        };
        if record.revision != self.revision || &record != expected {
            return Err(io::Error::other(
                "Precheck retirement revision does not match",
            ));
        }
        let held = file.metadata()?;
        let path = root.join(retirement_name(self.firing));
        let named = path.symlink_metadata()?;
        if (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(io::Error::other("Precheck retirement proof was replaced"));
        }
        fs::remove_file(path)?;
        self.retirement_unlinked = true;
        File::open(root)?.sync_all()
    }

    pub fn status(&mut self) -> io::Result<Status> {
        self.reaped()?;
        match read_record::<Receipt>(&self.directory.join("receipt")) {
            Ok(receipt) => {
                if receipt.firing != self.firing || receipt.revision != self.revision {
                    return Err(io::Error::other(
                        "Precheck receipt belongs to another firing",
                    ));
                }
                return Ok(Status::Complete(receipt.outcome));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let lease = private_file(&self.directory.join("lease"))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            // The owner can publish between the first read and releasing its
            // lease. Check again while excluding a still-starting owner.
            if let Ok(receipt) = read_record::<Receipt>(&self.directory.join("receipt")) {
                if receipt.firing == self.firing && receipt.revision == self.revision {
                    return Ok(Status::Complete(receipt.outcome));
                }
            }
            if let Ok(started) = read_record::<Started>(&self.directory.join("started")) {
                if started.firing != self.firing || started.revision != self.revision {
                    return Err(io::Error::other(
                        "Precheck start proof belongs to another firing",
                    ));
                }
                if started.boot != crate::daemon::read_boot_id().map_err(io::Error::other)? {
                    // A different kernel boot proves old descendants extinct,
                    // but says nothing about command effects before shutdown.
                    return Ok(Status::Complete(Outcome::Stopped {
                        state: FiringState::Interrupted, skip: None,
                        diagnostic: "The system restarted before the precheck result was recorded; its effects are uncertain and it was not repeated".into(),
                    }));
                }
            }
            return Ok(Status::CleanupUnconfirmed);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(Status::Running)
        } else {
            Err(error)
        }
    }
}

/// Called by cagentsd before logging, runtimes or any background thread exists.
pub(crate) fn entry() -> Option<Result<(), String>> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next()?;
    if mode != SUPERVISOR && mode != EXECUTOR {
        return None;
    }
    Some((|| {
        let directory = PathBuf::from(args.next().ok_or("Missing precheck directory")?);
        crate::secure_file::reject_symlink_components(&directory).map_err(|e| e.to_string())?;
        crate::secure_file::validate_owned_directory(&directory).map_err(|e| e.to_string())?;
        if fs::metadata(&directory).map_err(|e| e.to_string())?.mode() & 0o077 != 0 {
            return Err("Precheck directory is not private".into());
        }
        let request: Request =
            read_record(&directory.join("request")).map_err(|e| e.to_string())?;
        validate_request(&request).map_err(|e| e.to_string())?;
        if directory.file_name().and_then(|s| s.to_str())
            != Some(request.firing.to_string().as_str())
        {
            return Err("Precheck directory does not match its firing".into());
        }
        if mode == EXECUTOR {
            if args.next().is_some() {
                return Err("Unexpected precheck arguments".into());
            }
            let outcome = (|| -> io::Result<Outcome> {
                let held_directory = crate::host::automations::target::verify(
                    &request.directory,
                    &request.target_identity,
                )?;
                // This dedicated helper has no other threads and executes one
                // command. Retain the exact checked object through shell spawn;
                // replacing the old pathname cannot redirect the authorized work.
                if unsafe { libc::fchdir(held_directory.as_raw_fd()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(super::run(
                    &SystemProcessExecutor,
                    &request.precheck,
                    Path::new("."),
                    &CancellationToken::default(),
                ))
            })()
            .unwrap_or_else(|error| {
                failed(format!(
                    "Precheck target could not be opened safely: {error}"
                ))
            });
            serde_json::to_writer(io::stdout().lock(), &outcome).map_err(|e| e.to_string())
        } else {
            let fd = args
                .next()
                .and_then(|value| value.to_str().and_then(|s| s.parse::<i32>().ok()))
                .filter(|fd| *fd >= 3)
                .ok_or("Missing precheck owner lease")?;
            if args.next().is_some() {
                return Err("Unexpected precheck arguments".into());
            }
            // SAFETY: this exact inherited descriptor is transferred to this
            // owner by launch; metadata is verified before using its authority.
            let lease = unsafe { File::from_raw_fd(fd) };
            let expected = private_file(&directory.join("lease"))
                .map_err(|e| e.to_string())?
                .metadata()
                .map_err(|e| e.to_string())?;
            let actual = lease.metadata().map_err(|e| e.to_string())?;
            if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
                return Err("Invalid precheck lease descriptor".into());
            }
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("Precheck already has another owner".into());
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            supervisor(&directory, &request, lease).map_err(|e| e.to_string())
        }
    })())
}

fn supervisor(directory: &Path, request: &Request, _lease: File) -> io::Result<()> {
    // SIG_IGN survives exec and automatically reaps children. That would
    // invalidate the unreaped-PID invariant used by OwnedChildren, so restore
    // normal waitable children before this dedicated owner launches anything.
    if unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let pid = std::process::id();
    let stat = fs::read(format!("/proc/{pid}/stat"))?;
    let (_, start) = crate::model::agent_recovery::process_identity(&stat)
        .ok_or_else(|| io::Error::other("Invalid precheck process identity"))?;
    create_record(
        directory,
        "started",
        &Started {
            firing: request.firing,
            revision: request.revision,
            boot: crate::daemon::read_boot_id().map_err(io::Error::other)?,
            pid,
            start,
        },
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(EXECUTOR)
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return create_record(
                directory,
                "receipt",
                &Receipt {
                    firing: request.firing,
                    revision: request.revision,
                    outcome: failed(format!("Precheck executor could not start: {error}")),
                },
            );
        }
    };
    let mut output = child.stdout.take().expect("piped executor output");
    let mut children = crate::daemon::owned_children::OwnedChildren::new(child);
    let result = (|| -> io::Result<Outcome> {
        if unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now()
            + Duration::from_secs(u64::from(request.precheck.timeout_seconds))
            + Duration::from_secs(2);
        let mut bytes = Vec::new();
        let outcome = loop {
            if directory.join("cancel").try_exists()? {
                break Outcome::Stopped {
                    state: FiringState::Cancelled,
                    skip: None,
                    diagnostic: "Precheck cancelled".into(),
                };
            }
            if Instant::now() >= deadline {
                break failed("Precheck exceeded its deadline".into());
            }
            let mut buffer = [0u8; 4096];
            match output.read(&mut buffer) {
                Ok(0) => {
                    break serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                        failed("Precheck executor ended without a valid result".into())
                    });
                }
                Ok(length) => {
                    if bytes.len() + length > MAX_RECORD as usize {
                        break failed("Precheck result exceeded its limit".into());
                    }
                    bytes.extend_from_slice(&buffer[..length]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => break failed(format!("Could not read precheck result: {error}")),
            }
        };
        Ok(outcome)
    })();
    let mut outcome =
        result.unwrap_or_else(|error| failed(format!("Precheck supervision failed: {error}")));
    // This helper has no waiter threads and retains the unreaped group leader.
    // The shared owner also finds descendants that created another session.
    loop {
        match children.terminate(CLEANUP_WINDOW) {
            Ok(true) => break,
            Ok(false) | Err(_) => {
                outcome = failed("Precheck cleanup exceeded its deadline; all descendants were subsequently stopped".into());
                // Do not publish a receipt or drop ownership on uncertainty.
                // The daemon remains blocked until this owner proves ECHILD.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    create_record(
        directory,
        "receipt",
        &Receipt {
            firing: request.firing,
            revision: request.revision,
            outcome,
        },
    )
}

#[cfg(test)]
fn test_helper_executable() -> io::Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER").ok_or_else(|| {
            io::Error::other("Precheck Worker tests require the same-head cagentsd helper")
        })?,
    );
    let metadata = path.metadata()?;
    if !path.is_absolute() || !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(io::Error::other(
            "Precheck Worker test helper must be an absolute executable file",
        ));
    }
    Ok(path)
}

fn failed(diagnostic: String) -> Outcome {
    Outcome::Stopped {
        state: FiringState::Failed,
        skip: None,
        diagnostic,
    }
}

fn validate_request(request: &Request) -> io::Result<()> {
    if request.precheck.command.is_empty()
        || request.precheck.command.len() > crate::automations::MAX_PRECHECK_BYTES
        || request.precheck.command.contains('\0')
        || !(1..=MAX_PRECHECK_SECONDS).contains(&request.precheck.timeout_seconds)
        || !request.directory.is_absolute()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid precheck request",
        ));
    }
    Ok(())
}

fn private_file(path: &Path) -> io::Result<File> {
    crate::secure_file::reject_symlink_components(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > MAX_RECORD
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unsafe precheck file",
        ));
    }
    Ok(file)
}

fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file = private_file(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD as usize {
        return Err(io::Error::other("Precheck record exceeds its limit"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn create_record(directory: &Path, name: &str, record: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    if bytes.len() > MAX_RECORD as usize {
        return Err(io::Error::other("Precheck record exceeds its limit"));
    }
    let temporary = directory.join(format!(".{name}-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        // Atomic no-replace rename preserves one-link private-file identity
        // throughout publication, including a concurrent restart reader.
        let root = crate::fs_safety::Root::open(directory)?;
        crate::fs_safety::rename_new_within(
            &root,
            temporary.file_name().expect("temporary leaf"),
            name.as_ref(),
        )
        .map_err(io::Error::other)?;
        File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_publication_is_private_atomic_and_cannot_replace_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let firing = Uuid::new_v4();
        let receipt = Receipt {
            firing,
            revision: 7,
            outcome: Outcome::Passed,
        };
        create_record(directory.path(), "receipt", &receipt).unwrap();
        let file = private_file(&directory.path().join("receipt")).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(file.metadata().unwrap().nlink(), 1);
        assert!(
            create_record(
                directory.path(),
                "receipt",
                &Receipt {
                    firing,
                    revision: 8,
                    outcome: failed("replacement".into()),
                }
            )
            .is_err()
        );
        let restored: Receipt = read_record(&directory.path().join("receipt")).unwrap();
        assert_eq!(restored.revision, 7);
        assert_eq!(restored.outcome, Outcome::Passed);
    }

    #[test]
    fn unlocked_worker_without_receipt_is_never_inferred_successful() {
        let root = tempfile::tempdir().unwrap();
        crate::secure_file::prepare_private_directory(root.path()).unwrap();
        assert_eq!(root.path().metadata().unwrap().mode() & 0o777, 0o700);
        let firing = Uuid::new_v4();
        let directory = root.path().join(firing.to_string());
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("lease"))
            .unwrap();
        let mut worker = Worker::recover(root.path(), firing, 1);
        assert!(matches!(
            worker.status().unwrap(),
            Status::CleanupUnconfirmed
        ));
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(matches!(worker.status().unwrap(), Status::Running));
        create_record(
            &directory,
            "receipt",
            &Receipt {
                firing,
                revision: 1,
                outcome: Outcome::Passed,
            },
        )
        .unwrap();
        assert!(matches!(
            worker.status().unwrap(),
            Status::Complete(Outcome::Passed)
        ));
        assert!(
            !worker.retire().unwrap(),
            "receipt publication is not owner exit"
        );
        assert!(directory.join("receipt").exists());
        drop(lease);
        assert!(worker.retire().unwrap());
        assert!(
            !directory.exists(),
            "settled proof files are reclaimed after owner exit"
        );
        assert_eq!(
            Worker::retirement_revision(root.path(), firing).unwrap(),
            Some(1)
        );
        worker.forget_retirement().unwrap();
        assert_eq!(
            Worker::retirement_revision(root.path(), firing).unwrap(),
            None
        );
    }

    fn retirement_fixture() -> (tempfile::TempDir, Worker, Retirement) {
        let root = tempfile::tempdir().unwrap();
        crate::secure_file::prepare_private_directory(root.path()).unwrap();
        assert_eq!(root.path().metadata().unwrap().mode() & 0o777, 0o700);
        let firing = Uuid::new_v4();
        let directory = root.path().join(firing.to_string());
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("lease"))
            .unwrap();
        create_record(
            &directory,
            "receipt",
            &Receipt {
                firing,
                revision: 1,
                outcome: Outcome::Passed,
            },
        )
        .unwrap();
        let identity = directory.metadata().unwrap();
        let retirement = Retirement {
            version: RETIREMENT_VERSION,
            firing,
            revision: 1,
            device: identity.dev(),
            inode: identity.ino(),
        };
        let worker = Worker::recover(root.path(), firing, 1);
        (root, worker, retirement)
    }

    #[test]
    fn durable_retirement_replays_full_and_partial_unlink_without_replacing_evidence() {
        for partial in [false, true] {
            let (root, mut worker, retirement) = retirement_fixture();
            let marker = root.path().join(retirement_name(retirement.firing));
            if partial {
                // A previously published receipt survives a crash after the
                // final lease unlink but before removing the same directory.
                create_record(
                    root.path(),
                    &retirement_name(retirement.firing),
                    &retirement,
                )
                .unwrap();
                fs::remove_file(worker.directory.join("receipt")).unwrap();
                fs::remove_file(worker.directory.join("lease")).unwrap();
            } else {
                let lease = private_file(&worker.directory.join("lease")).unwrap();
                assert_eq!(
                    unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                assert!(!worker.retire().unwrap());
                assert!(!marker.exists(), "live lease cannot mint retirement proof");
                drop(lease);
                assert!(worker.retire().unwrap());
            }
            let original = fs::read(&marker).unwrap();
            assert!(worker.retire().unwrap());
            assert!(!worker.directory.exists());
            assert_eq!(fs::read(&marker).unwrap(), original);
            let target = root.path().metadata().unwrap();
            let request = Request {
                firing: retirement.firing,
                revision: 1,
                directory: root.path().into(),
                target_identity: crate::automations::DirectoryIdentity {
                    device: target.dev(),
                    inode: target.ino(),
                },
                precheck: Precheck {
                    command: "exit 7".into(),
                    timeout_seconds: 5,
                },
            };
            assert_eq!(
                Worker::launch(root.path(), &request).err().unwrap().kind(),
                io::ErrorKind::AlreadyExists
            );
            let mut wrong = Worker::recover(root.path(), retirement.firing, 2);
            assert!(wrong.retire().is_err());
            assert!(marker.exists());
            let mut recovered = Worker::recover(root.path(), retirement.firing, 1);
            assert!(recovered.retire().unwrap());
            std::os::unix::fs::symlink(root.path().join("missing-target"), &recovered.directory)
                .unwrap();
            assert!(
                recovered.retire().is_err(),
                "dangling replacement is not absence"
            );
            assert!(recovered.forget_retirement().is_err());
            assert!(
                marker.exists(),
                "final cleanup erased a dangling replacement's custody"
            );
            fs::remove_file(&recovered.directory).unwrap();
            let mut replacement = retirement.clone();
            replacement.inode += 1;
            fs::remove_file(&marker).unwrap();
            create_record(
                root.path(),
                &retirement_name(retirement.firing),
                &replacement,
            )
            .unwrap();
            assert!(recovered.forget_retirement().is_err());
            assert!(
                marker.exists(),
                "same-ID replacement retirement proof was removed"
            );
            fs::remove_file(&marker).unwrap();
            create_record(
                root.path(),
                &retirement_name(retirement.firing),
                &retirement,
            )
            .unwrap();
            recovered.forget_retirement().unwrap();
            assert!(!marker.exists());
            assert!(
                recovered.retire().unwrap(),
                "exact post-ACK fsync retry remains possible"
            );
        }
    }

    #[test]
    fn retirement_refuses_missing_legacy_and_replaced_directories() {
        let root = tempfile::tempdir().unwrap();
        crate::secure_file::prepare_private_directory(root.path()).unwrap();
        assert_eq!(root.path().metadata().unwrap().mode() & 0o777, 0o700);
        let firing = Uuid::new_v4();
        let mut missing = Worker::recover(root.path(), firing, 1);
        assert!(missing.retire().is_err());
        crate::secure_file::prepare_private_directory(&missing.directory).unwrap();
        assert!(
            missing.retire().is_err(),
            "an empty legacy directory is not cleanup proof"
        );
        assert!(missing.directory.exists());
        assert!(!root.path().join(retirement_name(firing)).exists());

        let (root, mut worker, retirement) = retirement_fixture();
        create_record(
            root.path(),
            &retirement_name(retirement.firing),
            &retirement,
        )
        .unwrap();
        let original = root.path().join("original-directory");
        fs::rename(&worker.directory, &original).unwrap();
        crate::secure_file::prepare_private_directory(&worker.directory).unwrap();
        fs::write(
            worker.directory.join("replacement"),
            b"preserve replacement",
        )
        .unwrap();
        assert!(
            worker
                .retire()
                .unwrap_err()
                .to_string()
                .contains("replaced")
        );
        assert_eq!(
            fs::read(worker.directory.join("replacement")).unwrap(),
            b"preserve replacement"
        );
        assert!(original.join("receipt").exists());
        assert!(
            root.path()
                .join(retirement_name(retirement.firing))
                .exists()
        );

        let (root, mut worker, retirement) = retirement_fixture();
        create_record(
            root.path(),
            &retirement_name(retirement.firing),
            &retirement,
        )
        .unwrap();
        let original = root.path().join("original-directory");
        fs::rename(&worker.directory, &original).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing-target"), &worker.directory).unwrap();
        assert!(worker.retire().is_err());
        assert!(
            worker
                .directory
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(original.join("receipt").exists());
        assert!(
            root.path()
                .join(retirement_name(retirement.firing))
                .exists()
        );
    }

    #[test]
    fn retirement_receipts_reject_foreign_unsafe_malformed_and_oversized_records() {
        use std::os::unix::fs::PermissionsExt;
        for case in 0..9 {
            let (root, mut worker, mut retirement) = retirement_fixture();
            let marker = root.path().join(retirement_name(retirement.firing));
            match case {
                0 => retirement.version += 1,
                1 => retirement.firing = Uuid::new_v4(),
                2 => retirement.revision += 1,
                3 => retirement.inode += 1,
                _ => {}
            }
            match case {
                4 => fs::write(&marker, b"malformed private receipt").unwrap(),
                5 => fs::write(&marker, vec![b'x'; MAX_RETIREMENT_RECORD as usize + 1]).unwrap(),
                _ => create_record(
                    root.path(),
                    marker.file_name().unwrap().to_str().unwrap(),
                    &retirement,
                )
                .unwrap(),
            }
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
            if case == 6 {
                fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
            } else if case == 7 {
                let target = root.path().join("saved-receipt");
                fs::rename(&marker, &target).unwrap();
                std::os::unix::fs::symlink(&target, &marker).unwrap();
            } else if case == 8 {
                fs::hard_link(&marker, root.path().join("extra-link")).unwrap();
            }
            assert!(worker.retire().is_err(), "unsafe receipt case {case}");
            assert!(worker.directory.join("lease").exists());
            assert!(worker.directory.join("receipt").exists());
            assert!(marker.symlink_metadata().is_ok());
        }
    }

    #[test]
    fn recovery_rejects_a_receipt_for_another_revision_or_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let firing = Uuid::new_v4();
        let directory = root.path().join(firing.to_string());
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        create_record(
            &directory,
            "receipt",
            &Receipt {
                firing,
                revision: 3,
                outcome: Outcome::Passed,
            },
        )
        .unwrap();
        assert!(Worker::recover(root.path(), firing, 2).status().is_err());
        std::os::unix::fs::symlink(directory.join("receipt"), directory.join("alias")).unwrap();
        assert!(read_record::<Receipt>(&directory.join("alias")).is_err());
    }

    #[test]
    fn another_kernel_boot_proves_cleanup_but_never_a_successful_precheck() {
        let root = tempfile::tempdir().unwrap();
        let firing = Uuid::new_v4();
        let directory = root.path().join(firing.to_string());
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        create_record(&directory, "lease", &()).unwrap();
        create_record(
            &directory,
            "started",
            &Started {
                firing,
                revision: 1,
                boot: crate::model::BootId::new("previous-kernel-boot"),
                pid: std::process::id(),
                start: 1,
            },
        )
        .unwrap();
        assert!(matches!(
            Worker::recover(root.path(), firing, 1).status().unwrap(),
            Status::Complete(Outcome::Stopped {
                state: FiringState::Interrupted,
                ..
            })
        ));
        assert!(Worker::recover(root.path(), firing, 2).status().is_err());
    }
}
