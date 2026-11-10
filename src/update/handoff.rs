//! Daemon ownership transfer and recovery, independent of window reopening.
use super::*;

const SOURCE_RESUMED: &str = "The terminal service is running again, so the update was stopped. Your windows will reopen with their saved panes; retry the update when you are ready.";
const ANOTHER_SERVICE: &str = "Another cagents service started during the update; nothing was lost. Run `cagents daemon status`, then retry.";
const SERVICE_BUSY: &str = "Another cagents service started during the update and has running sessions. It was kept running; nothing was lost. Run `cagents daemon status`, then retry.";
pub(super) const SERVICE_NOT_STOPPED: &str = "The terminal service could not finish stopping. Your saved panes were kept. Run `cagents daemon status`, then retry.";

pub(super) fn wait_daemon_stopped(
    paths: crate::daemon::RuntimePaths,
    source: crate::daemon::lease_handoff::HandoffSource,
    state: &Path,
    job: &mut UpdateJob,
) -> Result<crate::daemon::lease_handoff::LeaseHandoff, UpdateError> {
    wait_with_observers(
        paths,
        source,
        state,
        job,
        |paths| {
            // A successful authenticated handshake means shutdown was abandoned.
            // A shutting-down daemon rejects Connect with host_shutting_down.
            source_answered(source::Client::connect(&paths.socket))
        },
        stop_empty_contender,
    )
}

fn source_answered(reply: Result<source::Client, crate::control::ControlError>) -> bool {
    use crate::control::ControlError;
    match reply {
        Ok(_) => true,
        Err(ControlError::UnexpectedMessage { actual, .. }) => actual != "host_shutting_down",
        // An explicit rejection is also an answer: the daemon is no longer
        // advertising its shutdown barrier. Leave it running and reopen clients.
        Err(ControlError::Remote { .. } | ControlError::ConnectionRejected { .. }) => true,
        Err(_) => false,
    }
}

fn wait_with_observers(
    paths: crate::daemon::RuntimePaths,
    source: crate::daemon::lease_handoff::HandoffSource,
    state: &Path,
    job: &mut UpdateJob,
    mut serving: impl FnMut(&crate::daemon::RuntimePaths) -> bool,
    mut stop_contender: impl FnMut(&crate::daemon::RuntimePaths, i32) -> Result<(), UpdateError>,
) -> Result<crate::daemon::lease_handoff::LeaseHandoff, UpdateError> {
    use crate::daemon::lease_handoff::{LeaseHandoff, holder_pid};
    use std::sync::mpsc::{self, RecvTimeoutError};
    const WAITING: &str = "Waiting for the terminal service to save its panes";
    let (sender, receiver) = mpsc::sync_channel(1);
    let waiting_paths = paths.clone();
    let pid = source.pid;
    thread::Builder::new()
        .name("daemon-lease-handoff".into())
        .spawn(move || {
            let _ = sender.send(LeaseHandoff::wait(&waiting_paths, pid));
        })?;
    let mut last_progress = Instant::now();
    let mut source_exited_at = None;
    let mut contenders = 0;
    let mut stopping = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => {
                return result.map_err(|error| {
                    eprintln!("Could not acquire daemon descriptor: {error}");
                    invalid(SERVICE_NOT_STOPPED)
                });
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("Daemon descriptor waiter exited unexpectedly");
                return Err(invalid(SERVICE_NOT_STOPPED));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let exited = source.exited()?;
        if !exited && serving(&paths) {
            return Err(invalid(SOURCE_RESUMED));
        }
        // Older clients can win the kernel waiter wakeup. Their own CLI does
        // the inventory/empty check and conditional shutdown in one operation;
        // never signal a numeric PID or assume that a new daemon owns nothing.
        if let Some(holder) = holder_pid(&paths)?
            && holder > 1
            && holder != std::process::id() as i32
            && holder != source.pid as i32
            && Some(holder) != source.locker_pid
            && stopping != Some(holder)
        {
            eprintln!("Daemon handoff contender {holder}, source {pid}");
            contenders += 1;
            if contenders > 3 {
                return Err(invalid(ANOTHER_SERVICE));
            }
            stop_contender(&paths, holder)?;
            stopping = Some(holder);
            // Give this service a bounded opportunity to release the lock.
            source_exited_at = Some(Instant::now());
        }
        if exited
            && source_exited_at.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(5)
        {
            eprintln!(
                "Source {pid} exited without releasing the daemon descriptor; holder {:?}",
                holder_pid(&paths)?
            );
            return Err(invalid(SERVICE_NOT_STOPPED));
        }
        if last_progress.elapsed() >= Duration::from_secs(15) {
            eprintln!("{WAITING}");
            set_phase(state, job, UpdatePhase::Installing, WAITING)?;
            last_progress = Instant::now();
        }
    }
}

fn stop_empty_contender(
    paths: &crate::daemon::RuntimePaths,
    holder: i32,
) -> Result<(), UpdateError> {
    stop_contender_with(paths, holder, || {
        crate::daemon::running_executable_at(paths)
    })
}

fn stop_contender_with(
    paths: &crate::daemon::RuntimePaths,
    holder: i32,
    mut executable: impl FnMut() -> Result<Option<PathBuf>, crate::daemon::PlatformError>,
) -> Result<(), UpdateError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if crate::daemon::lease_handoff::holder_pid(paths)? != Some(holder) {
            return Ok(());
        }
        // A just-started daemon owns the lock before either its metadata or
        // socket is ready. Allow that startup to settle before inspecting it.
        if let Ok(Some(binary)) = executable() {
            let cli = binary.with_file_name(crate::companion::Product::Cli.file_name());
            match empty_inventory(&cli, deadline.saturating_duration_since(Instant::now())) {
                Ok(false) => return Err(invalid(SERVICE_BUSY)),
                Ok(true) => {
                    if crate::daemon::lease_handoff::holder_pid(paths)? != Some(holder) {
                        return Ok(());
                    }
                    if stop_empty_service(&cli, deadline.saturating_duration_since(Instant::now()))
                        .is_ok()
                    {
                        return Ok(());
                    }
                    // Inventory can change after preflight. Read it again;
                    // --require-empty itself never ends those new sessions.
                }
                Err(error) => eprintln!("Contender inventory not ready: {error}"),
            }
        }
        if Instant::now() >= deadline {
            return Err(invalid(ANOTHER_SERVICE));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn empty_inventory(cli: &Path, timeout: Duration) -> Result<bool, UpdateError> {
    let mut command = Command::new(cli);
    command.args(["--json", "daemon", "preflight"]);
    crate::install_guard::sanitize_child_environment(&mut command);
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    let output = bounded_command(
        &mut command,
        "Checking the other terminal service",
        SMALL_ASSET_LIMIT,
        timeout,
        &CancellationToken::new(),
    )?;
    let reply: serde_json::Value = serde_json::from_reader(output)?;
    if reply["ok"] != true || reply["command"] != "daemon.preflight" {
        return Err(invalid(
            "The other service did not provide its session inventory",
        ));
    }
    let live = reply["data"]["capability"]["live_terminals"]
        .as_u64()
        .ok_or_else(|| invalid("The other service did not report its running sessions"))?;
    Ok(live == 0)
}

fn stop_empty_service(cli: &Path, timeout: Duration) -> Result<(), UpdateError> {
    let mut command = Command::new(cli);
    command.args(["--json", "daemon", "stop", "--require-empty"]);
    crate::install_guard::sanitize_child_environment(&mut command);
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    let output = bounded_command(
        &mut command,
        "Checking and stopping the other terminal service",
        SMALL_ASSET_LIMIT,
        timeout,
        &CancellationToken::new(),
    )
    .map_err(|error| {
        eprintln!("Contender empty-service shutdown failed: {error}");
        invalid(SERVICE_BUSY)
    })?;
    let reply: serde_json::Value = serde_json::from_reader(output).map_err(|error| {
        eprintln!("Contender shutdown reply invalid: {error}");
        invalid(ANOTHER_SERVICE)
    })?;
    if reply["ok"] != true
        || reply["command"] != "daemon.stop"
        || !(reply["data"]["state"] == "already_stopped"
            || (reply["data"]["outcome"] == "accepted"
                && reply["data"]["live_terminals_before"] == 0))
    {
        eprintln!("Contender did not confirm an empty shutdown: {reply}");
        return Err(invalid(SERVICE_BUSY));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{RuntimePaths, lease_handoff::HandoffSource};
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;

    struct Service(std::process::Child);
    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn service(root: &Path, paths: &RuntimePaths) -> (Service, HandoffSource) {
        fs::create_dir_all(&paths.directory).unwrap();
        fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o700)).unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("handoff-fixture"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("handoff-fixture"),
        };
        let mut child = Command::new("python3")
            .args([
                "-c",
                r#"
import fcntl, json, os, sys, time
from pathlib import Path
lock = open(sys.argv[1], 'w+')
os.chmod(sys.argv[1], 0o600)
fcntl.flock(lock, fcntl.LOCK_EX)
json.dump({'pid':os.getpid(), 'proof':json.loads(sys.argv[2])}, lock)
lock.flush()
print('ready', flush=True)
while not Path(sys.argv[3]).exists(): time.sleep(.005)
"#,
            ])
            .arg(&paths.lock)
            .arg(serde_json::to_string(&proof).unwrap())
            .arg(root.join("release"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let source = HandoffSource::pin(paths, &proof).unwrap();
        (Service(child), source)
    }

    fn fixture(root: &Path) -> (RuntimePaths, UpdateJob) {
        let directory = root.join("daemon");
        (
            RuntimePaths {
                socket: directory.join(crate::terminal_ipc::SOCKET_FILE_NAME),
                lock: directory.join("cagentsd.lock"),
                directory,
            },
            UpdateJob {
                id: uuid::Uuid::new_v4().to_string(),
                version: "new".into(),
                phase: UpdatePhase::Installing,
                message: String::new(),
                log_path: root.join("worker.log"),
                confirmation: None,
            },
        )
    }

    #[test]
    fn source_that_answers_again_ends_update_without_waiting_for_its_exit() {
        let root = tempfile::tempdir().unwrap();
        let (paths, mut job) = fixture(root.path());
        let (mut source_service, source) = service(root.path(), &paths);
        let result = wait_with_observers(
            paths.clone(),
            source,
            root.path(),
            &mut job,
            |_| true,
            |_, _| panic!("the original daemon must not be stopped"),
        );
        assert_eq!(result.err().unwrap().to_string(), SOURCE_RESUMED);
        assert!(source_service.0.try_wait().unwrap().is_none());
        assert!(crate::daemon::HostLease::is_held(&paths).unwrap());
        // Settle the blocked waiter without leaving a child behind.
        fs::write(root.path().join("release"), b"exit").unwrap();
        assert!(source_service.0.wait().unwrap().success());
    }

    #[test]
    fn empty_rival_is_stopped_and_handoff_retried_but_live_rival_is_preserved() {
        for busy in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (paths, mut job) = fixture(root.path());
            let source_root = root.path().join("source");
            fs::create_dir(&source_root).unwrap();
            let (mut original, source) = service(&source_root, &paths);
            fs::write(source_root.join("release"), b"exit").unwrap();
            assert!(original.0.wait().unwrap().success());
            // Acquire before starting the waiter, making the older-client win
            // deterministic rather than relying on scheduler wakeup ordering.
            let (mut rival, _) = service(root.path(), &paths);
            let cli = root.path().join("cagents");
            fs::write(&cli, format!(r#"#!/usr/bin/python3
import json, pathlib, sys
if sys.argv[1:] == ['--json','daemon','preflight']:
    counter=pathlib.Path({release}+'.attempts')
    attempt=int(counter.read_text())+1 if counter.exists() else 1
    counter.write_text(str(attempt))
    if attempt < 3: sys.exit(7)  # Metadata exists; listener not ready yet.
    print(json.dumps({{'ok':True,'command':'daemon.preflight','data':{{'capability':{{'live_terminals':int({busy})}}}}}}))
    sys.exit(0)
assert sys.argv[1:] == ['--json','daemon','stop','--require-empty']
if {busy}: sys.exit(4)
pathlib.Path({release}).write_text('empty checked; stop')
print(json.dumps({{'ok':True,'command':'daemon.stop','data':{{'outcome':'accepted','live_terminals_before':0,'termination_requested':True}}}}))
"#, busy = if busy { "True" } else { "False" }, release = serde_json::to_string(&root.path().join("release")).unwrap())).unwrap();
            fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
            let result = wait_with_observers(
                paths.clone(),
                source,
                root.path(),
                &mut job,
                |_| false,
                |_, holder| {
                    assert_eq!(holder, rival.0.id() as i32);
                    let mut resolve_attempts = 0;
                    stop_contender_with(&paths, holder, || {
                        resolve_attempts += 1;
                        if resolve_attempts < 3 {
                            return Ok(None); // Lock acquired; metadata not published yet.
                        }
                        Ok(Some(cli.with_file_name("cagentsd")))
                    })
                },
            );
            assert_eq!(
                fs::read_to_string(root.path().join("release.attempts")).unwrap(),
                "3"
            );
            if busy {
                assert_eq!(result.err().unwrap().to_string(), SERVICE_BUSY);
                assert!(rival.0.try_wait().unwrap().is_none());
                assert!(!root.path().join("release").exists());
            } else {
                let lease = result.unwrap();
                assert!(rival.0.wait().unwrap().success());
                assert!(crate::daemon::HostLease::is_held(&paths).unwrap());
                drop(lease);
            }
        }
    }

    #[test]
    fn resumed_source_handshake_ends_wait_with_original_service_still_running() {
        use crate::terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        };
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let (paths, mut job) = fixture(root.path());
        fs::create_dir(&paths.directory).unwrap();
        fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o700)).unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("resumed-source"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new("resumed-source"),
        };
        let lock = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&paths.lock)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        serde_json::to_writer(
            &lock,
            &serde_json::json!({
                "pid": std::process::id(), "proof": &proof,
            }),
        )
        .unwrap();
        let source = HandoffSource::pin(&paths, &proof).unwrap();
        assert!(!source.exited().unwrap());
        let listener = UnixListener::bind(&paths.socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            // Source discovery and its fresh authenticated handshake are
            // separate connections, both handled by this still-running owner.
            for _ in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "source probe never connected");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("source accept: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
                assert!(matches!(connect.request, ClientRequest::Connect { .. }));
                write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(connect.request_id),
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
            }
            // Prevent a concurrent fork from keeping this fixture listener live.
            crate::daemon::shut_down_test_listener(listener);
        });
        let result = wait_daemon_stopped(paths.clone(), source, root.path(), &mut job);
        server.join().unwrap();
        assert_eq!(result.err().unwrap().to_string(), SOURCE_RESUMED);
        assert!(crate::daemon::HostLease::is_held(&paths).unwrap());
        // The source is this test process: it kept its exact lease throughout
        // the cancelled update and was never signalled or asked to stop.
        assert_eq!(
            unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC
        );
        // Release explicitly so copies inherited by concurrent test forks do
        // not leave the off-thread flock waiter behind.
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    }

    #[test]
    fn explicit_source_answers_end_wait_but_shutdown_and_absence_keep_waiting() {
        use crate::control::ControlError;
        use crate::terminal_ipc::ProtocolErrorCode;

        for error in [
            ControlError::Remote {
                code: ProtocolErrorCode::InvalidRequest,
                message: "Shutdown was abandoned".into(),
                retryable: false,
            },
            ControlError::ConnectionRejected {
                code: ProtocolErrorCode::InvalidRequest,
                message: "Please reconnect".into(),
                retryable: true,
            },
            ControlError::UnexpectedMessage {
                expected: "the connect response",
                actual: "terminal_created",
            },
        ] {
            assert!(source_answered(Err(error)));
        }
        for error in [
            ControlError::UnexpectedMessage {
                expected: "the connect response",
                actual: "host_shutting_down",
            },
            ControlError::ConnectionClosed,
            ControlError::Connect(io::Error::from(io::ErrorKind::NotFound)),
            ControlError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused)),
        ] {
            assert!(!source_answered(Err(error)));
        }
    }
}
