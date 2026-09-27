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
}

impl Worker {
    pub fn launch(root: &Path, request: &Request) -> io::Result<Self> {
        validate_request(request)?;
        crate::secure_file::prepare_private_directory(root)?;
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
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([SUPERVISOR])
            .arg(&directory)
            .arg(fd.to_string())
            .env_remove(crate::agent_execution::AUTHORITY_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
        })
    }

    pub fn recover(root: &Path, firing: Uuid, revision: u64) -> Self {
        Self {
            directory: root.join(firing.to_string()),
            firing,
            revision,
            child: None,
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

    pub fn reaped(&mut self) -> io::Result<bool> {
        if let Some(child) = &mut self.child
            && child.try_wait()?.is_some()
        {
            self.child.take();
        }
        Ok(self.child.is_none())
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
