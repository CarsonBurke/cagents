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
    Work,
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
            // The noun alone lists, as `work ls` does.
            Self::Work => &["work"],
        }
    }

    fn reply(self, request: ClientRequest, populated: bool) -> ServerResponse {
        match (self, request) {
            (
                Self::Sessions,
                ClientRequest::Agent {
                    operation:
                        Operation::Context {
                            scope,
                            standing: true,
                        },
                    ..
                },
            ) => {
                assert!(
                    matches!(scope, agent_execution::directory::Scope::Containing(path) if path.is_absolute())
                );
                let sessions = if populated {
                    let id = "00000000-0000-4000-8000-000000000001".parse().unwrap();
                    vec![agent_execution::directory::Member {
                        session: agent_execution::directory::Entry {
                            id,
                            surface: agent_execution::directory::Surface::Chat,
                            provider: Some(Provider::Claude),
                            title: Some("Fixture session".into()),
                            channel: None,
                            view_scope: None,
                            channel_repository: None,
                            lineage_channel: None,
                            state: cagents::agent::ActivityState::Working,
                            directory: Some("/fixture".into()),
                            worktree: Some("/fixture".into()),
                            project: Some("/fixture/.git".into()),
                            project_name: Some("Fixture".into()),
                            delegation: None,
                            relation: agent_execution::directory::Relation::Peer,
                            lineage: agent_execution::Lineage(id),
                        },
                        exited: false,
                        failure: None,
                        active_at: None,
                        limit: None,
                        account: None,
                    }]
                } else {
                    vec![]
                };
                ServerResponse::Agent(agent_execution::Reply::Context(
                    agent_execution::directory::Context {
                        sessions: agent_execution::directory::Sessions {
                            caller: None,
                            scope: agent_execution::directory::Scope::All,
                            sessions,
                        },
                        lineage_channel: None,
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
                            // The daemon's record holds the console's whole
                            // recipe, environment included.
                            console: Some(
                                cagents::terminal::ipc::TerminalDescriptor::relaunchable(
                                    "00000000-0000-4000-8000-000000000002".parse().unwrap(),
                                    cagents::model::LaunchRecipe {
                                        program: "/fixture/cagents".into(),
                                        arguments: vec!["agent".into(), "console".into()],
                                        working_directory: "/fixture".into(),
                                        environment: std::collections::BTreeMap::from([(
                                            "PROVIDER_TOKEN".into(),
                                            ENVIRONMENT_SENTINEL.into(),
                                        )]),
                                        unset_environment: Default::default(),
                                    },
                                    None,
                                )
                                .session,
                            ),
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
                        skipped_occurrences: None,
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
            (Self::Terminals | Self::TerminalShow, ClientRequest::ListPage { after: None }) => {
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
                ServerResponse::ListedPage {
                    terminals,
                    complete: true,
                }
            }
            // The board is scoped to the open Git project containing the
            // current directory, which this fixture's projects do not.
            (
                Self::Work | Self::Agents,
                ClientRequest::ReadWorkspaceMetadata {
                    query: cagents::workspace_model::metadata::Query::Projects,
                },
            ) => ServerResponse::WorkspaceMetadata(
                cagents::workspace_model::metadata::Reply::Projects {
                    identity: Uuid::from_u128(1),
                    projects: vec![cagents::workspace_model::ModelProject {
                        repository: "/fixture/.git".into(),
                        name: "Fixture".into(),
                        kind: cagents::model::ProjectKind::Git,
                        ordinal: 0,
                    }],
                },
            ),
            (
                Self::Work,
                ClientRequest::Work(cagents::work::Request::Board {
                    scope: cagents::work::Scope::All,
                    fresh: false,
                }),
            ) => {
                let items = if populated {
                    serde_json::json!([{
                        "project": "/fixture/.git",
                        "project_name": "Fixture",
                        "key": {"branch": "fixture/branch"},
                        "branch": "fixture/branch",
                        "worktree": {
                            "path": "/fixture/.worktrees/branch",
                            "primary": false,
                            "locked": false,
                            "created_at_unix_ms": null
                        },
                        "head": null,
                        "comparison": {"base": "origin/main", "ahead": 2, "behind": 0},
                        "dirty": {"paths": 0, "conflicted": 0, "newest_modified_at_unix_ms": null},
                        "pull_request": null,
                        "linear": null,
                        "sessions": [],
                        "last_activity": null,
                        "error": null,
                        "column": "in_progress",
                        "derived": "in_progress",
                        "pinned": false
                    }])
                } else {
                    serde_json::json!([])
                };
                let board = serde_json::from_value(serde_json::json!({
                    "generated_at_unix_ms": 0,
                    "stale_after_days": 7,
                    "projects": [{
                        "repository": "/fixture/.git",
                        "name": "Fixture",
                        "root": "/fixture",
                        "base": "origin/main",
                        "scanned_at_unix_ms": 0,
                        "scan_error": null,
                        "forge": "not_git_hub"
                    }],
                    "items": items,
                    "linear_unavailable": null
                }))
                .unwrap();
                ServerResponse::Work(cagents::work::Response::Board(board))
            }
            _ => panic!("CLI sent a request outside its expected read contract"),
        }
    }
}

fn output(listing: Listing, populated: bool, json: bool) -> String {
    output_with(listing, populated, json, &[])
}

/// [`output`], with `extra` arguments after the listing's own.
fn output_with(listing: Listing, populated: bool, json: bool, extra: &[&str]) -> String {
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
            let Some(mut request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() else {
                continue;
            };
            loop {
                assert_eq!(request.proof.as_ref(), Some(&proof));
                // A read that only chooses what a Work listing covers
                // precedes it on the same connection, and a populated agent
                // listing follows with one that names its projects.
                let preliminary = match listing {
                    Listing::Work => {
                        matches!(request.request, ClientRequest::ReadWorkspaceMetadata { .. })
                    }
                    Listing::Agents => {
                        populated
                            && matches!(
                                request.request,
                                ClientRequest::Agent {
                                    operation: Operation::List,
                                    ..
                                }
                            )
                    }
                    _ => false,
                };
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
                let next = read_frame::<ClientEnvelope>(&mut stream).unwrap();
                match next {
                    Some(next) if preliminary => request = next,
                    next => {
                        assert!(next.is_none());
                        return;
                    }
                }
            }
        }
        panic!("CLI never issued its expected metadata read");
    });
    let executable = Path::new(env!("CARGO_BIN_EXE_cagents"));
    let mut command = CommandSpec::new(executable)
        .args(["--device", "local"])
        .args(listing.arguments().iter().copied())
        .args(extra.iter().copied())
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
                if matches!(&request.request, ClientRequest::ReadWorkspaceModel) {
                    respond(
                        &mut stream,
                        request.request_id,
                        ServerMessage::Response {
                            response: ServerResponse::WorkspaceModel {
                                model: WorkspaceModel {
                                    channels: vec![ModelChannel {
                                        id: accepted.agent.channel().unwrap(),
                                        repository: accepted.agent.spec.target.repository.clone(),
                                        title: "Fixture accepted agents".into(),
                                        title_source: Default::default(),
                                        ordinal: 0,
                                    }],
                                    ..Default::default()
                                },
                            },
                        },
                    );
                    continue;
                }
                let ClientRequest::Agent { operation, .. } = request.request else {
                    panic!("request outside private agent contract");
                };
                let reply = match operation {
                    Operation::Context {
                        standing: false, ..
                    } => Reply::Context(agent_execution::directory::Context {
                        sessions: agent_execution::directory::Sessions {
                            caller: None,
                            scope: agent_execution::directory::Scope::Project(
                                accepted.agent.spec.target.repository.clone(),
                            ),
                            sessions: vec![],
                        },
                        lineage_channel: None,
                    }),
                    // The agent's usage limit and account, read beside its run.
                    Operation::Sessions(scope) => {
                        Reply::Sessions(agent_execution::directory::Sessions {
                            caller: None,
                            scope,
                            sessions: vec![],
                        })
                    }
                    Operation::List => Reply::Listed(vec![]),
                    Operation::Inbox(_) | Operation::PeekInbox(_) => Reply::Inbox(vec![]),
                    // A spawn submits; a follow-up asks for a prompt, which
                    // this agent takes now.
                    Operation::Submit(submit)
                    | Operation::Prompt {
                        request: submit, ..
                    } => {
                        submits += 1;
                        assert_eq!(submits, 1, "no duplicate submission after Accepted");
                        let prompted = match submit.destination {
                            agent_execution::Destination::New(_) => {
                                assert!(spawn);
                                false
                            }
                            agent_execution::Destination::Existing(id) => {
                                assert!(!spawn);
                                assert_eq!(id, accepted.agent.id);
                                true
                            }
                        };
                        receipt = Some(serde_json::json!({
                            "agent": accepted.agent,
                            "run": accepted.run,
                            "created": accepted.created,
                            "idempotency_key": submit.idempotency_key,
                            "device": "local",
                        }));
                        if prompted {
                            Reply::Prompted {
                                accepted: accepted.clone(),
                                start: agent_execution::Start::Sent { resumed: false },
                            }
                        } else {
                            Reply::Accepted(accepted.clone())
                        }
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
        if spawn {
            let agent: Agent = serde_json::from_value(receipt["agent"].clone()).unwrap();
            assert_eq!(
                response["data"]["placed_in"]["channel"],
                serde_json::json!(agent.channel().unwrap())
            );
            assert_eq!(
                response["data"]["placed_in"]["title"],
                "Fixture accepted agents"
            );
        }
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
        (
            Listing::Work,
            "IN PROGRESS (1)\nCARD ",
            "No cards in project Fixture",
            "fixture/branch",
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
fn agent_list_names_its_project_and_the_next_step() {
    let human = output(Listing::Agents, true, false);
    // The daemon's project name labels the one checkout the agents share,
    // and the waiting agent's handle is its shortest unique prefix.
    assert!(
        human.contains("\n(project Fixture at /fixture)\n"),
        "{human}"
    );
    assert!(!human.contains("PROJECT"), "{human}");
    assert!(
        human.trim_end().ends_with(
            "hint: `cagents agent status abcdef00` shows its latest result; \
             `cagents agent requests abcdef00` lists what it asks"
        ),
        "{human}"
    );
}

#[test]
fn agent_list_fields_select_from_a_compact_row() {
    let value: serde_json::Value = serde_json::from_str(&output_with(
        Listing::Agents,
        true,
        false,
        &["--fields", "name,state,project,provider"],
    ))
    .unwrap();
    assert_eq!(value["command"], "agent.list");
    let agents = value["data"]["agents"].as_array().unwrap();
    assert!(!agents.is_empty());
    for agent in agents {
        let keys: Vec<&str> = agent
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            keys.iter()
                .all(|key| ["name", "state", "project", "provider"].contains(key)),
            "{agent}"
        );
        assert_eq!(agent["project"], "Fixture", "{agent}");
    }
}

#[test]
fn agent_list_json_reports_compact_rows() {
    let value: serde_json::Value =
        serde_json::from_str(&output(Listing::Agents, true, true)).unwrap();
    assert_eq!(value["command"], "agent.list");
    let channel = value["data"]["agents"][0]["channel"].clone();
    assert!(channel.is_string(), "{value}");
    // Each fact once, by ID and name: no launch recipe, no repository beside
    // the project, and nothing unknown, such as a model or account.
    assert_eq!(
        value["data"],
        serde_json::json!({"agents": [{
            "id": "abcdef00-0000-4000-8000-000000000001",
            "name": "Fixture reviewer",
            "provider": "claude",
            "state": "waiting",
            "run": "12345678-0000-4000-8000-000000000001",
            "project": "Fixture",
            "directory": "/fixture",
            "channel": channel,
            "console": "00000000-0000-4000-8000-000000000002",
            "created_at": 0
        }]})
    );
}

#[test]
fn work_alone_lists_the_board_in_its_json_envelope() {
    let human = output(Listing::Work, true, false);
    // One project's cards name it once, with the checkout their worktrees
    // are relative to, and an empty column is no column.
    assert!(
        human.ends_with("\n(project Fixture at /fixture)\n"),
        "{human}"
    );
    assert!(!human.contains("PROJECT"), "{human}");
    assert!(human.contains(".worktrees/branch"), "{human}");
    let value: serde_json::Value =
        serde_json::from_str(&output(Listing::Work, true, true)).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "work.list");
    // Each project is described once and each card is a compact row: the
    // project's base is not repeated, and facts a card lacks are left out.
    assert_eq!(
        value["data"],
        serde_json::json!({
            // The scope says what the board covers: here every project, since
            // none of them contains this directory.
            "scope": "all",
            "projects": [{
                "name": "Fixture",
                "root": "/fixture",
                "base": "origin/main",
                "forge": "not_github"
            }],
            "stale_after_days": 7,
            "cards": [{
                "card": "fixture/branch",
                "project": "Fixture",
                "column": "in_progress",
                "worktree": "/fixture/.worktrees/branch",
                "ahead": 2
            }]
        })
    );
    // Naming fields asks for JSON and keeps only them.
    let selected: serde_json::Value = serde_json::from_str(&output_with(
        Listing::Work,
        true,
        false,
        &["--fields", "column,card"],
    ))
    .unwrap();
    assert_eq!(
        selected["data"],
        serde_json::json!({"cards": [{"column": "in_progress", "card": "fixture/branch"}]})
    );
}

#[test]
fn session_listings_report_compact_rows_and_selected_fields() {
    let value: serde_json::Value =
        serde_json::from_str(&output(Listing::Sessions, true, true)).unwrap();
    assert_eq!(value["command"], "sessions");
    // The checkout is the directory, the project its name, and nothing
    // unknown or default is spelled out.
    assert_eq!(
        value["data"],
        serde_json::json!({
            "scope": "all",
            "sessions": [{
                "id": "00000000-0000-4000-8000-000000000001",
                "provider": "claude",
                "state": "working",
                "title": "Fixture session",
                "project": "Fixture",
                "directory": "/fixture"
            }]
        })
    );
    let selected: serde_json::Value = serde_json::from_str(&output_with(
        Listing::Sessions,
        true,
        false,
        &["--fields", "id,state"],
    ))
    .unwrap();
    assert_eq!(
        selected["data"],
        serde_json::json!({"sessions": [{
            "id": "00000000-0000-4000-8000-000000000001",
            "state": "working"
        }]})
    );
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

/// A launch environment value in the fixture daemon's agent records.
const ENVIRONMENT_SENTINEL: &str = "ENVIRONMENT_CREDENTIAL_SENTINEL";

#[test]
fn agent_list_json_prints_no_launch_recipe() {
    // `agent status` prints the record, recipes without their environments.
    for arguments in [&[][..], &["--fields", "id,console,session"]] {
        let rendered = output_with(Listing::Agents, true, true, arguments);
        assert!(!rendered.contains(ENVIRONMENT_SENTINEL), "{rendered}");
        assert!(!rendered.contains("/fixture/cagents"), "{rendered}");
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
    let rendered = output(Listing::TerminalShow, true, true);
    // Show keeps the recipe but never its environment's values.
    assert!(!rendered.contains("show only"), "{rendered}");
    let shown: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let launch = &shown["data"]["terminal"]["session"]["launch"];
    assert_eq!(launch["program"], "/bin/sh");
    assert!(launch.get("environment").is_none(), "{launch}");
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

#[test]
fn an_unknown_field_is_a_json_usage_error_naming_the_fields() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime-must-not-be-created");
    let command = CommandSpec::new(Path::new(env!("CARGO_BIN_EXE_cagents")))
        .args(["ls", "--fields=id,bogus"])
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
    // `--fields` asks for JSON, its errors included.
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["kind"], "usage", "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("bogus") && message.contains("state"),
        "{message}"
    );
    assert!(output.stdout.is_empty());
    assert!(!runtime.exists());
}
