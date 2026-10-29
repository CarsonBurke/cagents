//! Direct GUI/library-style public request, without CLI policy setup. Only
//! private synthetic framed peers run; no Host permission or daemon-start proof.
use super::{Operation, Reply, request};
use crate::{
    agent_execution::Authority,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    os::unix::{
        ffi::OsStringExt,
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const ROLE: &str = "CAGENTS_TEST_DEVICE_NO_START_ENTRY_ROLE";
const POLICY: &str = "CAGENTS_TEST_DEVICE_NO_START_ENTRY_POLICY";
const ROOT: &str = "CAGENTS_TEST_DEVICE_NO_START_ENTRY_ROOT";
const TOKEN: &str = "synthetic-device-entry-original-scope";
const REFUSAL: &str = "Private device peer refused the original scope";
fn root() -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT).unwrap())
}
fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("device-entry-fixture"),
        boot_id: BootId::new("device-entry-boot"),
        host_incarnation: HostIncarnation::new("device-entry-incarnation"),
    }
}
fn response(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof(),
            message,
        },
    )
    .unwrap();
}
fn accepted(peer: &mut UnixStream, request: &ClientEnvelope) {
    response(
        peer,
        request,
        ServerMessage::Response {
            response: ServerResponse::Accepted,
        },
    );
}
fn home() -> Reply {
    Reply::Home {
        shell_error: None,
        path: "/synthetic-selected-device/home".into(),
        shell: "/bin/sh".into(),
        environment: Default::default(),
    }
}
struct Peer(Option<JoinHandle<(UnixListener, usize)>>);
impl Peer {
    fn new(authority: Authority) -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        assert!(socket.starts_with(root()));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut readiness = 0;
            let (mut peer, auth) = loop {
                let mut peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "public device entry did not connect"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("private device entry listener: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                assert!(hello.proof.is_none());
                response(
                    &mut peer,
                    &hello,
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof(),
                            resumed: vec![],
                        },
                    },
                );
                if let Some(auth) = read_frame::<ClientEnvelope>(&mut peer).unwrap() {
                    break (peer, auth);
                }
                // Service the old genuine readiness probe all the way through.
                // Diagnose its policy only after real authentication/business.
                readiness += 1;
                assert!(readiness <= 1, "no repeated readiness or fallback");
            };
            assert_eq!(auth.proof.as_ref(), Some(&proof()));
            assert!(
                matches!(&auth.request, ClientRequest::Authenticate { authority: actual } if actual == &authority),
                "public request retains the exact original User or scoped authority"
            );
            // Synthetic acceptance only permits reaching the real operation;
            // the fixture issues no Host capability or real mutation permission.
            accepted(&mut peer, &auth);
            let business: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(business.proof.as_ref(), Some(&proof()));
            assert_eq!(
                business.request,
                ClientRequest::DeviceOperation {
                    operation: Operation::Home
                }
            );
            if authority.token().is_some() {
                response(
                    &mut peer,
                    &business,
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::PermissionDenied,
                        REFUSAL,
                        false,
                    )),
                );
            } else {
                response(
                    &mut peer,
                    &business,
                    ServerMessage::Response {
                        response: ServerResponse::DeviceOperation { reply: home() },
                    },
                );
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no retry, additional operation, reauthentication or User fallback"
            );
            (listener, readiness)
        })))
    }
    fn finished(mut self) -> usize {
        let (listener, readiness) = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        readiness
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}
fn exercise(role: &str, policy: &str) {
    assert!(!crate::device::is_remote());
    let expected = match role {
        "user" => Authority::default(),
        "present" => Authority::session(TOKEN.into()),
        "empty" | "non-unicode" => Authority::session(String::new()),
        _ => unreachable!(),
    };
    assert_eq!(Authority::from_environment(), expected);
    // This fresh exact-selector child never calls run_from or StartPolicy.
    // NO_START's raw atomic therefore remains its static false construction
    // value by source invariant. Do not query daemon_autostart_disabled here:
    // its public policy may itself include the inherited environment after fix.
    let peer = Peer::new(expected.clone());
    let result = request(Operation::Home);
    if expected.token().is_some() {
        assert!(result.unwrap_err().contains(REFUSAL));
    } else {
        assert_eq!(result.unwrap(), home());
    }
    let readiness = peer.finished();
    // Real public authentication/business already completed before this oracle.
    // No resolver counters or the private request_as seam replace this entry.
    let inherited_no_start = matches!(policy, "1" | "TRUE" | "yes");
    assert_eq!(
        readiness,
        usize::from(expected.token().is_none() && !inherited_no_start),
        "direct entry must honor inherited no-start without CLI setup"
    );
}
fn child(selector: &str, role: &str, policy: &str) {
    let private = tempfile::Builder::new()
        .prefix("dv-entry-")
        .tempdir()
        .unwrap();
    let base = private.path().canonicalize().unwrap();
    for leaf in [
        "home", "runtime", "config", "data", "cache", "state", "tmp", "bin",
    ] {
        let path = base.join(leaf);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = base.join("stdout");
    let stderr = base.join("stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .args(["--exact", selector, "--nocapture", "--test-threads=1"])
        .current_dir(&base)
        .env(ROLE, role)
        .env(POLICY, policy)
        .env(ROOT, &base)
        .env("HOME", base.join("home"))
        .env("PATH", base.join("bin"))
        .env("XDG_RUNTIME_DIR", base.join("runtime"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_DATA_HOME", base.join("data"))
        .env("XDG_CACHE_HOME", base.join("cache"))
        .env("XDG_STATE_HOME", base.join("state"))
        .env("TMPDIR", base.join("tmp"))
        .env("TMP", base.join("tmp"))
        .env("TEMP", base.join("tmp"))
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    if policy != "absent" {
        command.env("CAGENTS_NO_START", policy);
    }
    if role != "user" {
        command.env(
            crate::agent_execution::AUTHORITY_ENV,
            match role {
                "present" => OsString::from(TOKEN),
                "empty" => OsString::new(),
                "non-unicode" => OsString::from_vec(vec![0xff]),
                _ => unreachable!(),
            },
        );
    }
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                self.0.kill().unwrap();
            }
            self.0.wait().unwrap();
        }
    }
    let mut process = OwnedChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(35);
    let status = loop {
        for path in [&stdout, &stderr] {
            assert!(fs::metadata(path).unwrap().len() <= 1024 * 1024);
        }
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "private device entry child exceeded bound"
        );
        thread::sleep(Duration::from_millis(5));
    };
    for path in [&stdout, &stderr] {
        assert!(fs::metadata(path).unwrap().len() <= 1024 * 1024);
    }
    let stdout = fs::read_to_string(stdout).unwrap();
    let stderr = fs::read_to_string(stderr).unwrap();
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
    assert!(status.success(), "{role}/{policy}: {stdout}\n{stderr}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
}
#[test]
fn public_device_request_honors_inherited_no_start_without_cli_policy_setup() {
    if let Some(role) = std::env::var_os(ROLE) {
        exercise(role.to_str().unwrap(), &std::env::var(POLICY).unwrap());
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for policy in ["absent", "1", "TRUE", "yes", "0", "false", "invalid"] {
        child(&selector, "user", policy);
    }
    for role in ["present", "empty", "non-unicode"] {
        child(&selector, role, "TRUE");
    }
}
