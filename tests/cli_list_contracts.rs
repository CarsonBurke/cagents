#![cfg(target_os = "linux")]
//! Real CLI formatting against bounded private framed peers, without providers.
use cagents::{
    account::info::Report,
    agent::Provider,
    agent_execution::{self, Agent, Operation, Origin, Placement, Spec, State, Surface, Target},
    automations,
    model::{BootId, ChannelId, HostIncarnation, HostSessionId, HostSessionProof},
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    },
    workspace_model::{ModelChannel, WorkspaceModel},
};
use std::{
    fs, io,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const ACCOUNT_DETAIL_SENTINEL: &str = "fixture-account-detail-must-stay-out-of-listings-4f58c12e";

#[derive(Clone, Copy)]
enum Listing {
    Accounts,
    Agents,
    Channels,
    Automations,
    History,
    Terminals,
    TerminalShow,
    Sessions,
}

impl Listing {
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Accounts => &["account", "list"],
            Self::Agents => &["agent", "list"],
            Self::Channels => &["channel", "list"],
            Self::Automations => &["automation", "list"],
            Self::History => &["automation", "history"],
            Self::Terminals => &["terminal", "list"],
            Self::TerminalShow => &["terminal", "show", "00000000-0000-4000-8000-000000000001"],
            Self::Sessions => &["sessions"],
        }
    }

    fn reply(self, request: ClientRequest, populated: bool) -> ServerResponse {
        match (self, request) {
            (
                Self::Sessions,
                ClientRequest::Agent {
                    operation: Operation::Directory(scope),
                    ..
                },
            ) => {
                assert!(
                    matches!(scope, agent_execution::directory::Scope::Containing(path) if path.is_absolute())
                );
                let sessions = if populated {
                    let id = "00000000-0000-4000-8000-000000000001".parse().unwrap();
                    vec![agent_execution::directory::Entry {
                        id,
                        surface: agent_execution::directory::Surface::Chat,
                        provider: Some(Provider::Claude),
                        title: Some("Fixture session".into()),
                        channel: None,
                        view_scope: None,
                        channel_repository: None,
                        state: cagents::agent::ActivityState::Working,
                        directory: Some("/fixture".into()),
                        worktree: Some("/fixture".into()),
                        project: Some("/fixture/.git".into()),
                        project_name: Some("Fixture".into()),
                        delegation: None,
                        relation: agent_execution::directory::Relation::Peer,
                        lineage: agent_execution::Lineage(id),
                    }]
                } else {
                    vec![]
                };
                ServerResponse::Agent(agent_execution::Reply::Directory(
                    agent_execution::directory::Directory {
                        caller: None,
                        scope: agent_execution::directory::Scope::All,
                        sessions,
                    },
                ))
            }
            (
                Self::Accounts,
                ClientRequest::Agent {
                    operation: Operation::AccountInfo(query),
                    ..
                },
            ) => {
                assert!(!query.refresh);
                assert_eq!(query.provider, None);
                let accounts = if populated {
                    vec![serde_json::from_value(serde_json::json!({
                        "id": "claude/system", "provider": "Claude", "label": "Fixture account",
                        "registered_identity": null, "observed_identity": null, "identity_conflict": false,
                        "selected_by_caller": true, "default_for_new_chats": true, "archived": false,
                        "usage": null, "remaining": [], "credits": null,
                        "credits_remaining_fraction": null, "credits_remaining": null,
                        "age_ms": null, "stale": true, "detail": ACCOUNT_DETAIL_SENTINEL
                    })).unwrap()]
                } else {
                    vec![]
                };
                ServerResponse::Agent(agent_execution::Reply::AccountInfo(Report {
                    sampled_at: 123,
                    caller: None,
                    accounts,
                    diagnostics: vec![],
                }))
            }
            (
                Self::Agents,
                ClientRequest::Agent {
                    operation: Operation::List,
                    ..
                },
            ) => {
                let entries = if populated {
                    vec![agent_execution::Summary {
                        agent: Agent {
                            id: "abcdef00-0000-4000-8000-000000000001".parse().unwrap(),
                            name: Some("Fixture reviewer".into()),
                            origin: Origin::User,
                            spec: Spec {
                                provider: Provider::Claude,
                                account: None,
                                target: Target {
                                    repository: "/fixture/.git".into(),
                                    directory: "/fixture".into(),
                                    placement: None,
                                },
                                surface: Surface::Chat,
                                model: None,
                                permission_mode: None,
                                reasoning_effort: None,
                            },
                            placement: Placement::Channel(ChannelId::new()),
                            session: None,
                            console: None,
                            created_ms: 0,
                            closed: false,
                        },
                        run: "12345678-0000-4000-8000-000000000001".parse().unwrap(),
                        state: State::Waiting,
                    }]
                } else {
                    vec![]
                };
                ServerResponse::Agent(agent_execution::Reply::Listed(entries))
            }
            (Self::Channels, ClientRequest::ReadWorkspaceModel) => {
                let channels = if populated {
                    vec![ModelChannel {
                        id: ChannelId::new(),
                        repository: "/fixture/.git".into(),
                        title: "Fixture channel".into(),
                        title_source: Default::default(),
                        ordinal: 1,
                    }]
                } else {
                    vec![]
                };
                ServerResponse::WorkspaceModel {
                    model: WorkspaceModel {
                        identity: Uuid::new_v4(),
                        channels,
                        ..Default::default()
                    },
                }
            }
            (
                Self::Automations,
                ClientRequest::Automation {
                    operation: automations::Request::List { .. },
                    ..
                },
            ) => {
                let entries = if populated {
                    vec![automations::Summary {
                        id: Uuid::from_u128(1),
                        revision: 1,
                        name: "Fixture schedule".into(),
                        provider: Provider::Claude,
                        directory: "/fixture".into(),
                        enabled: true,
                        next: None,
                        active_firing: None,
                        last_state: None,
                        blocked: None,
                    }]
                } else {
                    vec![]
                };
                ServerResponse::Automation(automations::Response::Listed(entries))
            }
            (
                Self::History,
                ClientRequest::Automation {
                    operation: automations::Request::History { .. },
                    ..
                },
            ) => {
                let firings = if populated {
                    vec![automations::Firing {
                        id: Uuid::from_u128(1),
                        automation: Uuid::from_u128(2),
                        name: "Fixture history".into(),
                        revision: 1,
                        trigger: automations::Trigger::Manual {
                            request: Uuid::from_u128(3),
                        },
                        state: automations::FiringState::Skipped,
                        release_pending: false,
                        cancel_requested: false,
                        created_ms: 0,
                        started_ms: None,
                        finished_ms: Some(1),
                        agent: None,
                        run: None,
                        skip: Some(automations::SkipReason::Capacity),
                        diagnostic: None,
                    }]
                } else {
                    vec![]
                };
                ServerResponse::Automation(automations::Response::History(
                    automations::HistoryPage {
                        firings,
                        next: None,
                    },
                ))
            }
            (Self::Terminals | Self::TerminalShow, ClientRequest::List) => {
                let terminals = if populated {
                    let mut terminal = cagents::terminal::ipc::TerminalDescriptor::relaunchable(
                        "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                        cagents::model::LaunchRecipe {
                            program: "/bin/sh".into(),
                            arguments: vec!["-l".into()],
                            working_directory: "/fixture".into(),
                            environment: std::collections::BTreeMap::from([(
                                "FIXTURE_VALUE".into(),
                                "show only".into(),
                            )]),
                            unset_environment: Default::default(),
                        },
                        None,
                    );
                    terminal.title = Some("Fixture terminal".into());
                    terminal.session.resume_hint = Some("fixture recovery metadata".into());
                    terminal.session.continuity =
                        cagents::model::TerminalContinuity::DetachedSameBoot {
                            host_session_id: HostSessionId::new("list-peer"),
                            boot_id: BootId::new("private-boot"),
                            host_incarnation: HostIncarnation::new("private-incarnation"),
                        };
                    vec![terminal]
                } else {
                    vec![]
                };
                ServerResponse::Listed { terminals }
            }
            _ => panic!("CLI sent a request outside its expected read contract"),
        }
    }
}

fn output(listing: Listing, populated: bool, json: bool) -> String {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let service = runtime.join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir(&service).unwrap();
    fs::set_permissions(&service, fs::Permissions::from_mode(0o700)).unwrap();
    let listener =
        UnixListener::bind(service.join(cagents::terminal::ipc::SOCKET_FILE_NAME)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let peer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("list-peer"),
            boot_id: BootId::new("private-boot"),
            host_incarnation: HostIncarnation::new("private-incarnation"),
        };
        for _ in 0..4 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("bounded private accept: {error}"),
                }
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero());
            stream
                .set_read_timeout(Some(remaining.min(Duration::from_secs(3))))
                .unwrap();
            stream
                .set_write_timeout(Some(remaining.min(Duration::from_secs(3))))
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
                            resumed: vec![],
                        },
                    },
                },
            )
            .unwrap();
            // Existing-daemon readiness may connect and handshake before the
            // command itself. A probe must close without asking for effects.
            let Some(request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() else {
                continue;
            };
            assert_eq!(request.proof.as_ref(), Some(&proof));
            let response = listing.reply(request.request, populated);
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response { response },
                },
            )
            .unwrap();
            assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
            return;
        }
        panic!("CLI never issued its expected metadata read");
    });
    let executable = Path::new(env!("CARGO_BIN_EXE_cagents"));
    let mut command = CommandSpec::new(executable)
        .args(["--device", "local"])
        .args(listing.arguments().iter().copied())
        .env("HOME", root.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .timeout(Duration::from_secs(10))
        .output_limits(64 * 1024, 64 * 1024);
    if json {
        command = command.arg("--json");
    }
    let result = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let settled = peer.join();
    settled.unwrap();
    let result = result.unwrap().require_success(executable).unwrap();
    String::from_utf8(result.stdout).unwrap()
}

#[derive(Clone, Copy)]
enum AcceptedWait {
    CodedError,
    Disconnect,
    WrongStatus,
    Running,
}

fn accepted_wait_output(
    spawn: bool,
    supplied_key: Option<&str>,
    json: bool,
    behavior: AcceptedWait,
) -> (cagents::process::ProcessOutput, serde_json::Value) {
    use agent_execution::{Accepted, Reply, ResultSummary, Run};
    use cagents::terminal::ipc::{ProtocolError, ProtocolErrorCode};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let service = runtime.join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir_all(&service).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&service, fs::Permissions::from_mode(0o700)).unwrap();
    let listener =
        UnixListener::bind(service.join(cagents::terminal::ipc::SOCKET_FILE_NAME)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let agent_id = "abcdef00-0000-4000-8000-000000000001".parse().unwrap();
    let run_id = "12345678-0000-4000-8000-000000000001".parse().unwrap();
    let accepted = Accepted {
        agent: Agent {
            id: agent_id,
            name: Some("accepted-fixture".into()),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: root.path().join(".git"),
                    directory: root.path().into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Channel(ChannelId::new()),
            session: None,
            console: None,
            created_ms: 123,
            closed: false,
        },
        run: Run {
            id: run_id,
            agent: agent_id,
            state: State::Accepted,
            revision: 7,
            accepted_ms: 456,
            started_ms: None,
            finished_ms: None,
            provider_key: None,
            result: ResultSummary::default(),
        },
        created: supplied_key.is_none(),
    };
    let done = Arc::new(AtomicBool::new(false));
    let peer_done = done.clone();
    let peer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("accepted-wait-peer"),
            boot_id: BootId::new("private-boot"),
            host_incarnation: HostIncarnation::new("private-incarnation"),
        };
        let mut submits = 0;
        let mut run_reads = 0;
        let mut receipt = None;
        loop {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if peer_done.load(Ordering::Acquire) {
                        assert_eq!(submits, 1, "post-accept failure must never resubmit");
                        assert!(run_reads > 0);
                        return receipt.expect("original acceptance");
                    }
                    assert!(Instant::now() < deadline, "bounded private accept");
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("private accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            let respond = |stream: &mut _, request_id, message| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request_id),
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
            };
            respond(
                &mut stream,
                connect.request_id,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            while let Some(request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() {
                assert_eq!(request.proof.as_ref(), Some(&proof));
                let ClientRequest::Agent { operation, .. } = request.request else {
                    panic!("request outside private agent contract");
                };
                let reply = match operation {
                    Operation::Directory(_) => {
                        Reply::Directory(agent_execution::directory::Directory {
                            caller: None,
                            scope: agent_execution::directory::Scope::Project(
                                accepted.agent.spec.target.repository.clone(),
                            ),
                            sessions: vec![],
                        })
                    }
                    Operation::List => Reply::Listed(vec![]),
                    Operation::Inbox(_) | Operation::PeekInbox(_) => Reply::Inbox(vec![]),
                    Operation::Submit(submit) => {
                        submits += 1;
                        assert_eq!(submits, 1, "no duplicate submission after Accepted");
                        match submit.destination {
                            agent_execution::Destination::New(_) => assert!(spawn),
                            agent_execution::Destination::Existing(id) => {
                                assert!(!spawn);
                                assert_eq!(id, accepted.agent.id);
                            }
                        }
                        receipt = Some(serde_json::json!({
                            "agent": accepted.agent,
                            "run": accepted.run,
                            "created": accepted.created,
                            "idempotency_key": submit.idempotency_key,
                            "device": "local",
                        }));
                        Reply::Accepted(accepted.clone())
                    }
                    Operation::Run(id) => {
                        assert_eq!(id, accepted.run.id);
                        run_reads += 1;
                        match behavior {
                            AcceptedWait::Disconnect => break,
                            AcceptedWait::CodedError => {
                                respond(
                                    &mut stream,
                                    request.request_id,
                                    ServerMessage::Error(ProtocolError::new(
                                        ProtocolErrorCode::InvalidRequest,
                                        "fixture run observation failed",
                                        true,
                                    )),
                                );
                                continue;
                            }
                            AcceptedWait::WrongStatus => Reply::Listed(vec![]),
                            AcceptedWait::Running => {
                                let mut run = accepted.run.clone();
                                run.state = State::Running;
                                Reply::Status {
                                    agent: accepted.agent.clone(),
                                    run,
                                }
                            }
                        }
                    }
                    _ => panic!("operation outside private accepted-wait contract"),
                };
                respond(
                    &mut stream,
                    request.request_id,
                    ServerMessage::Response {
                        response: ServerResponse::Agent(reply),
                    },
                );
            }
        }
    });
    let executable = Path::new(env!("CARGO_BIN_EXE_cagents"));
    let mut command = CommandSpec::new(executable).args(["--device", "local", "agent"]);
    if spawn {
        command = command.args(["spawn", "--provider", "claude"]);
    } else {
        command = command.args(["send", &agent_id.to_string()]);
    }
    command = command
        .args(["--wait", "--timeout", "0.15"])
        .arg("private accepted-wait fixture")
        .cwd(root.path())
        .env("HOME", root.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .timeout(Duration::from_secs(6))
        .output_limits(64 * 1024, 64 * 1024);
    if let Some(key) = supplied_key {
        command = command.args(["--idempotency-key", key]);
    }
    if json {
        command = command.arg("--json");
    }
    let result = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    done.store(true, Ordering::Release);
    let receipt = peer.join().unwrap();
    (result.unwrap(), receipt)
}

#[test]
fn accepted_wait_errors_retain_original_receipts_without_resubmission() {
    for spawn in [false, true] {
        for supplied_key in [
            None,
            Some("same key 'quoted' \"double\" $(literal); `data`"),
        ] {
            for behavior in [AcceptedWait::CodedError, AcceptedWait::Disconnect] {
                let (output, acceptance) =
                    accepted_wait_output(spawn, supplied_key, true, behavior);
                assert_eq!(output.exit_code, Some(1));
                assert!(output.stdout.is_empty());
                let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(error["ok"], false);
                assert_eq!(error["error"]["details"]["acceptance"], acceptance);
                let expected_kind = if matches!(behavior, AcceptedWait::CodedError) {
                    assert_eq!(error["error"]["details"]["code"], "invalid_request");
                    assert_eq!(error["error"]["details"]["retryable"], true);
                    "protocol"
                } else {
                    "connection"
                };
                assert_eq!(error["error"]["kind"], expected_kind);
            }
        }
    }
}

#[test]
fn accepted_final_status_errors_retain_original_receipts_without_resubmission() {
    for spawn in [false, true] {
        for supplied_key in [None, Some("same supplied key")] {
            let (output, acceptance) =
                accepted_wait_output(spawn, supplied_key, true, AcceptedWait::WrongStatus);
            assert_eq!(output.exit_code, Some(1));
            assert!(output.stdout.is_empty());
            let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["ok"], false);
            assert_eq!(error["error"]["kind"], "agent.status");
            assert_eq!(error["error"]["details"]["acceptance"], acceptance);
        }
    }
}

#[test]
fn accepted_wait_recovery_is_readable_and_normal_timeout_stays_distinct() {
    for spawn in [false, true] {
        let key = "same key 'quoted' \"double\" $(literal); `data`";
        let (output, receipt) =
            accepted_wait_output(spawn, Some(key), false, AcceptedWait::CodedError);
        assert_eq!(output.exit_code, Some(1));
        assert!(output.stdout.is_empty());
        let human = String::from_utf8(output.stderr).unwrap();
        for field in ["agent", "run"] {
            assert!(
                human.contains(receipt[field]["id"].as_str().unwrap()),
                "{human}"
            );
        }
        assert!(
            human.contains(&format!("idempotency key {key:?}")),
            "{human}"
        );
        assert!(
            human.contains(&format!(
                "cagents --device=local agent wait {}",
                receipt["run"]["id"].as_str().unwrap()
            )),
            "{human}"
        );
        let (output, receipt) = accepted_wait_output(spawn, None, true, AcceptedWait::Running);
        assert_eq!(output.exit_code, Some(3));
        assert!(output.stderr.is_empty());
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["data"]["ended_by"], "timeout");
        assert_eq!(response["data"]["timed_out"], true);
        assert_eq!(
            response["data"]["idempotency_key"],
            receipt["idempotency_key"]
        );
        assert_eq!(response["data"]["run"]["state"], "Running");
    }
}

#[test]
fn standalone_lists_show_headers_or_explicit_empty_states_without_debug_values() {
    for (listing, header, empty, content) in [
        (
            Listing::Accounts,
            "ACCOUNT",
            "No accounts",
            "Fixture account",
        ),
        (
            Listing::Agents,
            "AGENT",
            "No open agents",
            "Waiting for input",
        ),
        (
            Listing::Channels,
            "CHANNEL",
            "No channels",
            "Fixture channel",
        ),
        (
            Listing::Automations,
            "AUTOMATION",
            "No automations",
            "Fixture schedule",
        ),
        (
            Listing::History,
            "FIRING",
            "No automation history",
            "At capacity",
        ),
        (
            Listing::Terminals,
            "TERMINAL",
            "No running terminals",
            "Fixture terminal",
        ),
        (
            Listing::Sessions,
            "SESSION",
            "No live sessions",
            "Fixture session",
        ),
    ] {
        assert_eq!(output(listing, false, false).trim(), empty);
        let human = output(listing, true, false);
        assert!(human.starts_with(header), "{human}");
        assert!(human.contains(content), "{human}");
        assert!(
            !human.contains(ACCOUNT_DETAIL_SENTINEL),
            "account detail leaked into public listing: {human}"
        );
        for raw in ["Some(", "None", "{ ", "created_ms", "at_unix_ms"] {
            assert!(!human.contains(raw), "raw value {raw}: {human}");
        }
    }
}

#[test]
fn standalone_account_list_json_is_metadata_only_and_lowercase() {
    let rendered = output(Listing::Accounts, true, true);
    assert!(
        !rendered.contains(ACCOUNT_DETAIL_SENTINEL),
        "account detail leaked into public JSON: {rendered}"
    );
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["command"], "account.list");
    let row = &value["data"]["accounts"][0];
    assert_eq!(row["provider"], "claude");
    assert_eq!(row["name"], "Fixture account");
    assert_eq!(row["default"], true);
    assert_eq!(row["current"], true);
    for removed in ["usage", "environment", "launch", "recipe", "detail"] {
        assert!(row.get(removed).is_none(), "unexpected field {removed}");
    }
}

#[test]
fn standalone_terminal_list_omits_recipes_and_show_preserves_them() {
    let listed: serde_json::Value =
        serde_json::from_str(&output(Listing::Terminals, true, true)).unwrap();
    let row = &listed["data"]["terminals"][0];
    assert_eq!(row["state"], "running");
    assert_eq!(row["title"], "Fixture terminal");
    for field in ["session", "launch", "environment", "recipe", "resume_hint"] {
        assert!(row.get(field).is_none());
    }
    let shown: serde_json::Value =
        serde_json::from_str(&output(Listing::TerminalShow, true, true)).unwrap();
    assert_eq!(
        shown["data"]["terminal"]["session"]["launch"]["environment"]["FIXTURE_VALUE"],
        "show only"
    );
    assert_eq!(
        shown["data"]["terminal"]["session"]["resume_hint"],
        "fixture recovery metadata"
    );
}

#[test]
fn unconfirmed_account_removal_is_rejected_before_device_preparation() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime-must-not-be-created");
    let command = CommandSpec::new(Path::new(env!("CARGO_BIN_EXE_cagents")))
        .args([
            "--json",
            "--device",
            "not-a-configured-device",
            "account",
            "remove",
            "claude/00000000-0000-4000-8000-000000000001",
        ])
        .env("HOME", root.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .timeout(Duration::from_secs(5))
        .output_limits(64 * 1024, 64 * 1024);
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap();
    assert_eq!(output.exit_code, Some(2), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["kind"], "usage", "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--yes")
    );
    assert!(output.stdout.is_empty());
    assert!(!runtime.exists());
}
