//! Exercise the whole stop path against a separate, authenticated lease owner.
//! Barriers keep each drain phase live until the test explicitly releases it.
use super::*;
use crate::daemon::{HostLease, RuntimePaths};
use std::{fs, io::BufRead, os::unix::net::UnixListener, process::Child};

const PEER_ROOT: &str = "CAGENTS_TEST_STOP_PEER_ROOT";
const PEER_TEST: &str = "control::daemon_stop_tests::shutdown_peer";

#[test]
fn shutdown_peer() {
    let Some(root) = std::env::var_os(PEER_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let paths = paths(&root);
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("stop-fixture"),
        boot_id: crate::model::BootId::new(
            fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim(),
        ),
        host_incarnation: crate::model::HostIncarnation::new("stop-fixture"),
    };
    let lease = HostLease::acquire(&paths, &proof).unwrap();
    let listener = UnixListener::bind(&paths.socket).unwrap();
    fs::write(root.join("ready"), b"").unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
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
    let stop: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
    assert_eq!(stop.proof, Some(proof.clone()));
    assert!(matches!(
        stop.request,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Stop {
                require_empty: true,
                expected_generation: None
            },
        }
    ));
    let outcome = if root.join("reject").exists() {
        ShutdownOutcome::EmptyRequired { current_count: 1 }
    } else {
        ShutdownOutcome::Accepted {
            live_terminals_before: 0,
            termination_requested: true,
        }
    };
    write_frame(
        &mut stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(stop.request_id),
            proof,
            message: ServerMessage::Response {
                response: ServerResponse::ShutdownOutcome(outcome),
            },
        },
    )
    .unwrap();
    drop(stream);
    fs::write(root.join("accepted"), b"").unwrap();
    let mut commands = io::stdin().lock().lines();
    assert_eq!(commands.next().unwrap().unwrap(), "close-listener");
    drop(listener);
    fs::remove_file(&paths.socket).unwrap();
    fs::write(root.join("listener-closed"), b"").unwrap();
    assert_eq!(commands.next().unwrap().unwrap(), "release-lease");
    drop(lease);
    fs::write(root.join("lease-released"), b"").unwrap();
    assert_eq!(commands.next().unwrap().unwrap(), "exit");
}

fn paths(root: &Path) -> RuntimePaths {
    RuntimePaths {
        directory: root.join("runtime"),
        socket: root.join("runtime/cagentsd.sock"),
        lock: root.join("runtime/cagentsd.lock"),
    }
}

struct Peer {
    child: Child,
    root: tempfile::TempDir,
}

impl Peer {
    fn start(reject: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        if reject {
            fs::write(root.path().join("reject"), b"").unwrap();
        }
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", PEER_TEST, "--nocapture"])
            .env(PEER_ROOT, root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut peer = Self { child, root };
        peer.wait_for("ready");
        peer
    }

    fn socket(&self) -> PathBuf {
        paths(self.root.path()).socket
    }

    fn wait_for(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.root.path().join(marker).exists() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "peer exited before {marker}"
            );
            assert!(Instant::now() < deadline, "peer did not reach {marker}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn advance(&mut self, command: &str, marker: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
        self.wait_for(marker);
    }

    fn exit(&mut self) {
        writeln!(self.child.stdin.as_mut().unwrap(), "exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "peer did not exit");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn stop_waits_through_listener_drain_lease_release_and_process_exit() {
    let mut peer = Peer::start(false);
    let socket = peer.socket();
    let (sent, received) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || {
        sent.send(stop_daemon_at(
            &socket,
            true,
            None,
            None,
            false,
            Duration::from_secs(5),
        ))
        .unwrap();
    });
    peer.wait_for("accepted");
    assert!(
        received.recv_timeout(Duration::from_millis(30)).is_err(),
        "stop returned at acceptance"
    );
    assert!(HostLease::is_held(&paths(peer.root.path())).unwrap());
    peer.advance("close-listener", "listener-closed");
    assert!(
        received.recv_timeout(Duration::from_millis(30)).is_err(),
        "stop returned when the socket closed"
    );
    assert!(HostLease::is_held(&paths(peer.root.path())).unwrap());
    peer.advance("release-lease", "lease-released");
    assert!(!HostLease::is_held(&paths(peer.root.path())).unwrap());
    assert!(
        received.recv_timeout(Duration::from_millis(30)).is_err(),
        "stop returned before process exit"
    );
    peer.exit();
    let output = received
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    waiter.join().unwrap();
    assert_eq!(output.data["stopped"], true);
    assert_eq!(output.data["outcome"], "accepted");
    assert!(output.human.starts_with("Daemon stopped:"));
}

#[test]
fn stop_timeout_reports_accepted_shutdown_without_claiming_completion() {
    let mut peer = Peer::start(false);
    let started = Instant::now();
    let error = stop_daemon_at(
        &peer.socket(),
        true,
        None,
        None,
        false,
        Duration::from_millis(50),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind, "shutdown_timeout");
    assert_eq!(error.details.as_ref().unwrap()["outcome"], "accepted");
    assert_eq!(error.details.as_ref().unwrap()["stopped"], false);
    assert!(error.message.contains("may still be draining"));
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        peer.child.try_wait().unwrap().is_none(),
        "timeout must not kill the daemon"
    );
}

#[test]
fn no_wait_preserves_acceptance_without_claiming_completion() {
    let mut peer = Peer::start(false);
    let output = stop_daemon_at(&peer.socket(), true, None, None, true, Duration::ZERO).unwrap();
    assert_eq!(output.data["outcome"], "accepted");
    assert!(output.data.get("stopped").is_none());
    assert!(
        output
            .human
            .contains("termination has not been confirmed complete")
    );
    assert!(peer.child.try_wait().unwrap().is_none());
}

#[test]
fn rejected_stop_returns_without_waiting_for_process_exit() {
    let mut peer = Peer::start(true);
    let error = stop_daemon_at(&peer.socket(), true, None, None, false, Duration::ZERO)
        .err()
        .unwrap();
    assert_eq!(error.kind, "shutdown_rejected");
    assert_eq!(error.details.as_ref().unwrap()["outcome"], "empty_required");
    assert!(peer.child.try_wait().unwrap().is_none());
}
