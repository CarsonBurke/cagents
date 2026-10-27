#![cfg(feature = "host")]

#[path = "support/precheck_receipt_guardian.rs"]
mod guardian;

use guardian::{Custody, Identity};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const CONTROL: &str = "CAGENTS_PRECHECK_TEST_CONTROL";
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    ReceiptDeadline,
    CompletionDeadline,
    OwnerExited(String),
    ReceiptWhileDescendantPresent(Identity),
    Io(String),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

struct Worker {
    directory: PathBuf,
    child: Child,
    deadline: Instant,
}

impl Worker {
    fn start(custody: &Custody, command: &str, ignored: bool) -> Self {
        // Publication, marker observation and owner exit share this original
        // budget. Receipt arrival never renews it.
        let deadline = Instant::now() + Duration::from_secs(10);
        let firing = uuid::Uuid::new_v4();
        let directory = custody.root().join(firing.to_string());
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(directory.join("request"), serde_json::to_vec(&serde_json::json!({
            "firing": firing, "revision": 1, "directory": directory,
            "target_identity": {"device": fs::metadata(&directory).unwrap().dev(), "inode": fs::metadata(&directory).unwrap().ino()},
            "precheck": {"command": command, "timeout_seconds": 1}
        })).unwrap()).unwrap();
        fs::set_permissions(directory.join("request"), fs::Permissions::from_mode(0o600)).unwrap();
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("lease"))
            .unwrap();
        let fd = lease.as_raw_fd();
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
        command
            .arg("--automation-precheck-supervisor")
            .arg(&directory)
            .arg(fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(directory.join("owner.stderr")).unwrap());
        unsafe {
            command.pre_exec(move || {
                if ignored && libc::signal(libc::SIGCHLD, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        custody.register(&child, &directory).unwrap();
        // Only the real supervisor now owns the inherited lease/deadline.
        drop(lease);
        Self {
            directory,
            child,
            deadline,
        }
    }

    fn wait_file(&mut self, name: &str) -> Result<PathBuf, Failure> {
        let path = self.directory.join(name);
        loop {
            if Instant::now() >= self.deadline {
                return Err(Failure::ReceiptDeadline);
            }
            if path.try_exists()? {
                if Instant::now() >= self.deadline {
                    return Err(Failure::ReceiptDeadline);
                }
                return Ok(path);
            }
            if let Some(status) = self.child.try_wait()? {
                // Publication can win between the first path check and exit
                // observation. Recheck without renewing the launch budget.
                if Instant::now() >= self.deadline {
                    return Err(Failure::ReceiptDeadline);
                }
                if path.try_exists()? {
                    if Instant::now() >= self.deadline {
                        return Err(Failure::ReceiptDeadline);
                    }
                    return Ok(path);
                }
                return Err(Failure::OwnerExited(format!(
                    "{status}: {}",
                    guardian::diagnostic(&self.directory.join("owner.stderr"))
                )));
            }
            std::thread::sleep(POLL);
        }
    }

    fn descendant(&mut self) -> Identity {
        Identity::from_file(&self.wait_file("escaped.identity").unwrap()).unwrap()
    }

    fn observe_receipt(
        &mut self,
        descendant: Option<Identity>,
    ) -> Result<serde_json::Value, Failure> {
        let path = self.wait_file("receipt")?;
        let receipt = serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| Failure::Io(error.to_string()))?;
        // This is the consumption boundary: do not wait for the supervisor to
        // repair a prematurely published receipt before checking original work.
        if let Some(identity) = descendant
            && identity.present()?
        {
            return Err(Failure::ReceiptWhileDescendantPresent(identity));
        }
        if Instant::now() >= self.deadline {
            return Err(Failure::ReceiptDeadline);
        }
        Ok(receipt)
    }

    fn finish_owner(&mut self, deadline: Instant) -> Result<(), Failure> {
        loop {
            if Instant::now() >= deadline {
                return Err(Failure::CompletionDeadline);
            }
            if let Some(status) = self.child.try_wait()? {
                if Instant::now() >= deadline {
                    return Err(Failure::CompletionDeadline);
                }
                return if status.success() {
                    Ok(())
                } else {
                    Err(Failure::OwnerExited(format!(
                        "{status}: {}",
                        guardian::diagnostic(&self.directory.join("owner.stderr"))
                    )))
                };
            }
            std::thread::sleep(POLL);
        }
    }

    fn receipt(&mut self, descendant: Option<Identity>) -> Result<serde_json::Value, Failure> {
        let receipt = self.observe_receipt(descendant)?;
        self.finish_owner(self.deadline)?;
        Ok(receipt)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Cancellation is a request, not settlement. After body unwinding the
        // dedicated guardian owns rescue/reaping and retains uncertain roots.
        let _ = fs::write(self.directory.join("cancel"), []);
    }
}

fn escaped(tail: &str) -> String {
    // The leaf writes its own kernel identity before exec. Observations never
    // use a marker PID as signalling authority, and PID reuse is harmless.
    format!(
        r#"setsid /bin/sh -c 'IFS= read -r stat < /proc/$$/stat; stat=${{stat##*) }}; set -- $stat; shift 19; printf "%s %s\n" "$$" "$1" > escaped.identity.next; mv escaped.identity.next escaped.identity; exec sleep 5' >/dev/null 2>&1 & while ! test -s escaped.identity; do sleep 0.01; done; {tail}"#
    )
}

fn transfer_control(command: &mut Command, remote: &UnixStream) {
    let fd = remote.as_raw_fd();
    command.env(CONTROL, fd.to_string());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn send_identity(stream: &mut UnixStream, identity: Identity) {
    stream.write_all(&identity.pid.to_le_bytes()).unwrap();
    stream.write_all(&identity.start.to_le_bytes()).unwrap();
}

fn receive_identity(stream: &mut UnixStream) -> Identity {
    let mut pid = [0; 4];
    let mut start = [0; 8];
    stream.read_exact(&mut pid).unwrap();
    stream.read_exact(&mut start).unwrap();
    Identity {
        pid: i32::from_le_bytes(pid),
        start: u64::from_le_bytes(start),
    }
}

fn publish_receipt(directory: &Path) {
    fs::write(directory.join("receipt.next"), br#"{"outcome":"Passed"}"#).unwrap();
    fs::rename(directory.join("receipt.next"), directory.join("receipt")).unwrap();
}

fn await_release(control: &mut UnixStream) {
    let mut token = [0];
    // EOF on assertion unwinding also releases the private actor. Diagnostics
    // and success never depend on draining a descendant-held output pipe.
    match control.read(&mut token) {
        Ok(0) => {}
        Ok(1) => assert_eq!(token, [b'R']),
        other => panic!("Private actor release failed: {other:?}"),
    }
}

fn actor(stage: &str, directory: &Path) {
    let fd: i32 = std::env::var(CONTROL).unwrap().parse().unwrap();
    assert!(fd > 2);
    let mut control = unsafe { UnixStream::from_raw_fd(fd) };
    // transfer_control clears this only for the intended exec. A nested actor
    // receives its own explicitly transferred endpoint, never this owner's.
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );
    control
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    control
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    match stage {
        "leaf" => {
            send_identity(
                &mut control,
                Identity::read(std::process::id() as i32).unwrap(),
            );
            await_release(&mut control);
        }
        "premature" | "rescue" => {
            let (mut leaf_control, remote) = UnixStream::pair().unwrap();
            leaf_control
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut command = guardian::actor_command("leaf", directory);
            transfer_control(&mut command, &remote);
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut leaf = command.spawn().unwrap();
            drop(remote);
            let identity = receive_identity(&mut leaf_control);
            fs::write(
                directory.join("escaped.identity.next"),
                format!("{} {}\n", identity.pid, identity.start),
            )
            .unwrap();
            fs::rename(
                directory.join("escaped.identity.next"),
                directory.join("escaped.identity"),
            )
            .unwrap();
            publish_receipt(directory);
            control.write_all(b"P").unwrap();
            if stage == "rescue" {
                // Deliberately ignore cancellation/control EOF. Only guardian
                // rescue can stop this held setsid leaf; the owner then reaps
                // it before parking so descendants-first ordering is provable.
                loop {
                    if let Some(status) = leaf.try_wait().unwrap() {
                        assert_eq!(status.signal(), Some(libc::SIGKILL));
                        fs::write(
                            directory.join("leaf-reaped"),
                            format!("{} {} SIGKILL\n", identity.pid, identity.start),
                        )
                        .unwrap();
                        loop {
                            std::thread::park_timeout(Duration::from_secs(60));
                        }
                    }
                    std::thread::sleep(POLL);
                }
            }
            await_release(&mut control);
            leaf_control.write_all(b"R").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = leaf.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                assert!(Instant::now() < deadline, "Controlled leaf did not settle");
                std::thread::sleep(POLL);
            }
        }
        "stall" => {
            publish_receipt(directory);
            control.write_all(b"S").unwrap();
            await_release(&mut control);
        }
        _ => panic!("Unknown private actor stage: {stage}"),
    }
}

struct FakeOwner {
    worker: Worker,
    control: UnixStream,
}

impl FakeOwner {
    fn start(custody: &Custody, kind: &str) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let directory = custody.root().join(kind);
        fs::create_dir(&directory).unwrap();
        let (mut control, remote) = UnixStream::pair().unwrap();
        let mut command = guardian::actor_command(kind, &directory);
        transfer_control(&mut command, &remote);
        let child = command
            .stderr(File::create(directory.join("owner.stderr")).unwrap())
            .spawn()
            .unwrap();
        custody.register(&child, &directory).unwrap();
        drop(remote);
        control
            .set_read_timeout(Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .expect("Owner launch exhausted observation budget"),
            ))
            .unwrap();
        let mut ready = [0];
        control.read_exact(&mut ready).unwrap();
        assert_eq!(ready[0], if kind == "stall" { b'S' } else { b'P' });
        Self {
            worker: Worker {
                directory,
                child,
                deadline,
            },
            control,
        }
    }

    fn release_and_settle(&mut self) {
        self.control.write_all(b"R").unwrap();
        // Separate cleanup grace cannot turn the original completion failure
        // into success. Explicitly settle before asserting the negative oracle.
        self.worker
            .finish_owner(Instant::now() + Duration::from_secs(5))
            .unwrap();
    }
}

#[test]
fn successful_shell_still_reaps_detached_descendants_before_receipt() {
    guardian::isolated(
        "successful_shell_still_reaps_detached_descendants_before_receipt",
        actor,
        |custody| {
            let mut worker = Worker::start(custody, &escaped("exit 0"), false);
            let descendant = worker.descendant();
            let receipt = worker.receipt(Some(descendant)).unwrap();
            assert_eq!(receipt["outcome"], "Passed");
        },
    );
}

#[test]
fn inherited_ignored_sigchld_is_reset_before_owning_descendants() {
    guardian::isolated(
        "inherited_ignored_sigchld_is_reset_before_owning_descendants",
        actor,
        |custody| {
            let mut worker = Worker::start(custody, &escaped("exit 0"), true);
            let descendant = worker.descendant();
            let receipt = worker.receipt(Some(descendant)).unwrap();
            assert_eq!(receipt["outcome"], "Passed");
        },
    );
}

#[test]
fn worker_deadline_survives_controller_disconnect_and_reaps_detached_process() {
    guardian::isolated(
        "worker_deadline_survives_controller_disconnect_and_reaps_detached_process",
        actor,
        |custody| {
            let mut worker = Worker::start(custody, &escaped("sleep 5"), false);
            let descendant = worker.descendant();
            let receipt = worker.receipt(Some(descendant)).unwrap();
            assert_eq!(receipt["outcome"]["Stopped"]["state"], "failed");
        },
    );
}

#[test]
fn cancellation_receipt_follows_descendant_cleanup() {
    guardian::isolated(
        "cancellation_receipt_follows_descendant_cleanup",
        actor,
        |custody| {
            let mut worker = Worker::start(custody, &escaped("sleep 5"), false);
            let descendant = worker.descendant();
            File::create(worker.directory.join("cancel")).unwrap();
            let receipt = worker.receipt(Some(descendant)).unwrap();
            assert_eq!(receipt["outcome"]["Stopped"]["state"], "cancelled");
        },
    );
}

#[test]
fn receipt_controls_reject_live_descendants_and_bound_owner_completion() {
    guardian::isolated(
        "receipt_controls_reject_live_descendants_and_bound_owner_completion",
        actor,
        |custody| {
            let mut premature = FakeOwner::start(custody, "premature");
            let descendant = premature.worker.descendant();
            let observed = premature.worker.receipt(Some(descendant));
            // The leaf stays held until AFTER observation returns. Eventual exit
            // or a sleep-based overlap cannot satisfy this causal negative control.
            premature.release_and_settle();
            assert!(!descendant.present().unwrap());
            assert_eq!(
                observed,
                Err(Failure::ReceiptWhileDescendantPresent(descendant))
            );

            let mut stalled = FakeOwner::start(custody, "stall");
            let observed = stalled.worker.receipt(None);
            stalled.release_and_settle();
            assert_eq!(observed, Err(Failure::CompletionDeadline));
        },
    );
}

#[test]
fn guardian_rescues_the_exact_family_after_receipt_observation_failure() {
    guardian::isolated_rescue_control(
        "guardian_rescues_the_exact_family_after_receipt_observation_failure",
        actor,
        |custody| {
            let mut owner = FakeOwner::start(custody, "rescue");
            let descendant = owner.worker.descendant();
            let original_owner = Identity::read(owner.worker.child.id() as i32).unwrap();
            match owner.worker.receipt(Some(descendant)) {
                Err(Failure::ReceiptWhileDescendantPresent(identity)) if identity == descendant => {
                    guardian::fail_after_receipt_observation(original_owner, descendant);
                }
                other => panic!("Rescue control rejected for the wrong reason: {other:?}"),
            }
        },
    );
}
