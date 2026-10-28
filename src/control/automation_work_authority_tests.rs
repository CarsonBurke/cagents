//! Public CLI entry over private framed peers. Synthetic credentials establish
//! transport fidelity, not production Host authorization or provider behavior.
use super::*;
use crate::{
    automations::Request as AutomationRequest,
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::ProtocolError,
    work::{Board, Column, Request as WorkRequest, Response as WorkResponse, Scope},
};
use std::{
    cell::Cell,
    fs::{self, File},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt, net::UnixListener},
    thread::JoinHandle,
};

const ROLE: &str = "CAGENTS_TEST_AUTOMATION_WORK_ROLE";
const ROOT: &str = "CAGENTS_TEST_AUTOMATION_WORK_ROOT";
const SSH_MARKER: &str = "CAGENTS_TEST_AUTOMATION_WORK_SSH_MARKER";
const TOKEN: &str = "synthetic-automation-work-original-scope";
const REFUSAL: &str = "Private automation/Work business request refused";

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("automation-work-fixture"),
        boot_id: BootId::new("automation-work-fixture"),
        host_incarnation: HostIncarnation::new("automation-work-fixture"),
    }
}

fn reply(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
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

fn next(peer: &mut UnixStream) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(request.proof.as_ref(), Some(&proof()));
    request
}

#[derive(Clone, Copy)]
enum Business {
    Proposal,
    Pin,
    List,
}

struct Peer(Option<JoinHandle<(UnixListener, usize, usize)>>);
impl Peer {
    fn new(
        business: Business,
        proposal: uuid::Uuid,
        project: PathBuf,
        authority: Authority,
    ) -> Self {
        let socket = daemon_socket_path().unwrap();
        assert!(socket.starts_with(PathBuf::from(std::env::var_os(ROOT).unwrap())));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(6);
            let mut probes = 0;
            let mut raw_probes = 0;
            let (mut peer, mut request) = loop {
                let mut peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "CLI did not reach its private peer"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("private listener: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let Some(hello) = read_frame::<ClientEnvelope>(&mut peer).unwrap() else {
                    // User --no-start checks endpoint existence without a
                    // framed handshake. Accept it before business dispatch.
                    raw_probes += 1;
                    assert!(raw_probes <= 1, "no repeated existence probe");
                    continue;
                };
                assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                assert!(hello.proof.is_none());
                reply(
                    &mut peer,
                    &hello,
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof(),
                            resumed: vec![],
                        },
                    },
                );
                if let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap() {
                    assert_eq!(request.proof.as_ref(), Some(&proof()));
                    break (peer, request);
                }
                // Accept the old readiness probe so the before regression
                // fails on routing counts after valid business behavior.
                probes += 1;
                assert!(probes <= 1, "no repeated readiness or fallback");
            };
            if matches!(business, Business::Pin) && authority.token().is_some() {
                assert!(matches!(&request.request,
                    ClientRequest::Authenticate { authority: actual } if actual == &authority));
                reply(
                    &mut peer,
                    &request,
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                );
                request = next(&mut peer);
            }
            match business {
                Business::Proposal => assert!(matches!(&request.request,
                    ClientRequest::Automation { authority: actual, operation: AutomationRequest::Proposal { id } }
                        if actual == &authority && *id == proposal)),
                Business::Pin => assert!(matches!(&request.request,
                    ClientRequest::Work(WorkRequest::Pin { scope: Scope::Project(actual), item, column: Some(Column::Active) })
                        if actual == &project && item == "topic")),
                Business::List => assert!(matches!(
                    &request.request,
                    ClientRequest::Work(WorkRequest::Board {
                        scope: Scope::All,
                        fresh: false
                    })
                )),
            }
            if matches!(business, Business::List) {
                reply(
                    &mut peer,
                    &request,
                    ServerMessage::Response {
                        response: ServerResponse::Work(WorkResponse::Board(Board {
                            generated_at_unix_ms: 1,
                            stale_after_days: 7,
                            projects: vec![],
                            items: vec![],
                            linear_unavailable: None,
                        })),
                    },
                );
            } else {
                reply(
                    &mut peer,
                    &request,
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::PermissionDenied,
                        REFUSAL,
                        false,
                    )),
                );
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no extra request, Authenticate on a global read, or User retry"
            );
            (listener, probes, raw_probes)
        })))
    }

    fn finished(mut self) -> (usize, usize) {
        let (listener, probes, raw_probes) = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no fallback connection after the business reply"
        );
        (probes, raw_probes)
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

fn arguments(business: Business, proposal: uuid::Uuid, project: &Path) -> Vec<OsString> {
    let mut arguments = vec![OsString::from("cagents"), OsString::from("--json")];
    match business {
        Business::Proposal => arguments.extend([
            "automation".into(),
            "proposal".into(),
            proposal.to_string().into(),
        ]),
        Business::Pin => arguments.extend([
            "work".into(),
            "pin".into(),
            "topic".into(),
            "active".into(),
            "--project".into(),
            project.as_os_str().to_owned(),
        ]),
        Business::List => arguments.extend(["work".into(), "list".into()]),
    }
    arguments
}

fn exercise_local(role: &str) {
    let private = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let project = private.join("project");
    fs::create_dir(&project).unwrap();
    let authority = Authority::from_environment();
    let scoped = authority.token().is_some();
    let no_start = role == "user-no-start";
    for business in [Business::Proposal, Business::Pin, Business::List] {
        let proposal = uuid::Uuid::new_v4();
        let peer = Peer::new(business, proposal, project.clone(), authority.clone());
        let staged = Cell::new(0);
        let mut args = arguments(business, proposal, &project);
        if no_start {
            args.insert(1, "--no-start".into());
        }
        let result = run_from_with_staged_lookup(args, |_, recover| {
            staged.set(staged.get() + 1);
            assert_eq!(recover, !no_start);
            Ok(None)
        });
        assert_eq!(
            result,
            if matches!(business, Business::List) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        );
        let (probes, raw_probes) = peer.finished();
        assert_eq!(
            staged.get(),
            usize::from(!scoped),
            "scoped services must bypass staged owner lookup/recovery after reaching business dispatch"
        );
        assert_eq!(
            probes,
            usize::from(!scoped && !no_start),
            "scoped services must use the existing endpoint; ordinary User readiness stays intact"
        );
        assert_eq!(
            raw_probes,
            usize::from(!scoped && no_start),
            "User no-start keeps its endpoint probe; scoped services connect directly"
        );
    }
}

fn exercise_remote() {
    let private = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let devices = crate::device::configuration_path().unwrap();
    fs::create_dir_all(devices.parent().unwrap()).unwrap();
    fs::write(devices, br#"{"devices":[{"name":"fixture-remote","host":"scope-fixture.invalid","executable":"cagents"}]}"#).unwrap();
    for (index, business) in [Business::Proposal, Business::Pin, Business::List]
        .into_iter()
        .enumerate()
    {
        let mut args = arguments(business, uuid::Uuid::new_v4(), &private.join("project"));
        // Device selection is immutable for this process. Later commands use
        // the already selected device rather than failing a second selection.
        if index == 0 {
            args.splice(1..1, ["--device".into(), "fixture-remote".into()]);
        }
        let staged = Cell::new(0);
        let result = run_from_with_staged_lookup(args, |_, _| {
            staged.set(staged.get() + 1);
            Ok(None)
        });
        assert_eq!(result, ExitCode::FAILURE);
        assert!(crate::device::is_remote());
        assert_eq!(crate::device::selected().unwrap().name, "fixture-remote");
        assert_eq!(staged.get(), 0);
        assert!(
            !Path::new(&std::env::var_os(SSH_MARKER).unwrap()).exists(),
            "scoped remote service must refuse before device preparation/SSH"
        );
    }
}

#[test]
fn public_automation_and_work_keep_captured_scope_before_routing_and_readiness() {
    if let Some(role) = std::env::var_os(ROLE) {
        let role = role.to_str().unwrap();
        if role.starts_with("remote-") {
            exercise_remote();
        } else {
            exercise_local(role);
        }
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in [
        "user",
        "user-no-start",
        "present",
        "empty",
        "non-unicode",
        "remote-present",
        "remote-empty",
        "remote-non-unicode",
    ] {
        let private = tempfile::Builder::new().prefix("aw-").tempdir().unwrap();
        let private_root = private.path().canonicalize().unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache", "state", "bin"] {
            let path = private_root.join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let marker = private_root.join("ssh-called");
        let ssh = private_root.join("bin/ssh");
        fs::write(&ssh, b"#!/bin/sh\nprintf 'unexpected ssh\\n' >> \"$CAGENTS_TEST_AUTOMATION_WORK_SSH_MARKER\"\nexit 1\n").unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = private_root.join("stdout");
        let stderr = private_root.join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(&private_root)
            .env(ROLE, role)
            .env(ROOT, &private_root)
            .env(SSH_MARKER, &marker)
            .env("PATH", private_root.join("bin"))
            .env("HOME", private_root.join("home"))
            .env("XDG_RUNTIME_DIR", private_root.join("runtime"))
            .env("XDG_CONFIG_HOME", private_root.join("config"))
            .env("XDG_DATA_HOME", private_root.join("data"))
            .env("XDG_CACHE_HOME", private_root.join("cache"))
            .env("XDG_STATE_HOME", private_root.join("state"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        let scope_role = role.strip_prefix("remote-").unwrap_or(role);
        if !scope_role.starts_with("user") {
            command.env(
                crate::agent_execution::AUTHORITY_ENV,
                match scope_role {
                    "present" => OsString::from(TOKEN),
                    "empty" => OsString::new(),
                    "non-unicode" => OsString::from_vec(vec![0xff]),
                    _ => unreachable!(),
                },
            );
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(35);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private CLI child exceeded its bound: {role}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
        assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
        let stdout = fs::read_to_string(stdout).unwrap();
        let stderr = fs::read_to_string(stderr).unwrap();
        assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        assert!(status.success(), "{role}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
        let errors = stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        assert_eq!(
            errors.len(),
            if role.starts_with("remote-") { 3 } else { 2 }
        );
        for error in errors {
            assert_eq!(error["ok"], false);
            if role.starts_with("remote-") {
                assert_eq!(error["error"]["kind"], "permission_denied");
            } else {
                assert_eq!(error["error"]["kind"], "protocol");
                assert_eq!(
                    error["error"]["details"]["code"],
                    serde_json::to_value(ProtocolErrorCode::PermissionDenied).unwrap()
                );
                assert!(
                    error["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains(REFUSAL)
                );
            }
        }
        assert!(!marker.exists());
    }
}
