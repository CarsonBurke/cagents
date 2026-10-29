//! Private, synthetic native UI fixture. This is not production authority evidence.
//! Compile against the matched browserless development library; run only from
//! a genuine User process with private HOME/XDG paths. It never starts a provider.
use cagents::{
    agent::Provider,
    agent_execution::{Agent, Authority, HostedSession, Origin, Spec, Surface, Target},
    chat::{Launch, Session, Snapshot},
    model::*,
    persistence::{SnapshotBase, StateStore},
    session::SessionRuntime,
    terminal_ipc::{
        self, ClientEnvelope, ClientRequest, ProtocolError, ProtocolErrorCode, ServerEnvelope,
        ServerMessage, ServerResponse,
    },
    workspace_model::{
        ModelChannel, ModelEdit, ModelProject, WorkspaceModel,
        views::{Change, Pane, Scope, View},
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{Arc, Mutex},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: "synthetic-held-host".into(),
        boot_id: cagents::session::current_boot_id().expect("native fixture kernel boot"),
        host_incarnation: "synthetic-current-incarnation".into(),
    }
}
fn error(message: &str) -> ServerMessage {
    ServerMessage::Error(ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        message,
        false,
    ))
}
fn record(root: &std::path::Path, event: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("receipts.txt"))
        .expect("private receipt writer");
    writeln!(file, "{event}").expect("private receipt commit");
}
fn snapshot(session: &Session) -> Snapshot {
    Snapshot {
        ready: true,
        session_id: session.launch.session_id.clone(),
        title: (std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1"))
            .then(|| "Hidden conversation recovered".into()),
        items: if std::env::var("HELD_FIXTURE_PAYLOAD").as_deref() == Ok("long") {
            (0..200).map(|index| Arc::new(cagents::chat::Item {
                id: format!("synthetic-history-{index}"),
                role: if index % 2 == 0 { cagents::chat::Role::User } else { cagents::chat::Role::Assistant },
                content: cagents::chat::ItemContent::Text {
                    text: format!("Synthetic conversation entry {index}.\n{}", "Review the saved task, preserve its exact source and continue the healthy work.\n".repeat(5)),
                    thinking: false,
                    phase: cagents::chat::Phase::Final,
                },
                at: None,
                started_at: None,
            })).collect()
        } else { vec![] },
        ..Default::default()
    }
}
struct FixtureState {
    model: WorkspaceModel,
    repaired: bool,
    performance_started: bool,
}
fn client(
    mut stream: UnixStream,
    state: Arc<Mutex<FixtureState>>,
    root: PathBuf,
    healthy: Session,
    held: Session,
) {
    let writer = Arc::new(Mutex::new(stream.try_clone().expect("private fixture writer")));
    while let Ok(Some(request)) = terminal_ipc::read_frame::<ClientEnvelope>(&mut stream) {
        let watching = matches!(&request.request, ClientRequest::WatchWorkspaceModel);
        let explicit_read = matches!(&request.request, ClientRequest::ReadWorkspaceModel);
        if explicit_read {
            record(&root, "explicit-read-started");
            if root.join("error-check").exists() { record(&root, "scripted-read-disconnect"); return; }
            if root.join("hold-check").exists() {
                record(&root, "held-read-started");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while root.join("hold-check").exists() {
                    if std::time::Instant::now() >= deadline {
                        record(&root, "UNEXPECTED-held-read-deadline");
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        let response = match request.request {
            ClientRequest::Connect { expected_host, .. } => {
                if expected_host
                    .as_ref()
                    .is_some_and(|expected| expected != &proof())
                {
                    error("Synthetic fixture proof mismatch")
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof(),
                            resumed: vec![],
                        },
                    }
                }
            }
            ClientRequest::Authenticate { .. } => {
                error("This native fixture does not accept delegated callers")
            }
            ClientRequest::WatchWorkspaceModel | ClientRequest::ReadWorkspaceModel => {
                let mut fixture = state.lock().unwrap();
                if explicit_read && root.join("repair-evidence").exists() && !fixture.repaired {
                    fixture.repaired = true;
                    fixture.model.sequence += 1;
                    let bad = &mut fixture.model.agents[1];
                    bad.closed = false;
                    bad.session = Some(HostedSession::Chat(held.clone()));
                    record(&root, "synthetic-exact-source-repaired");
                }
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel {
                        model: fixture.model.clone(),
                    },
                }
            }
            ClientRequest::EditWorkspaceModel { edit } => {
                let mut fixture = state.lock().unwrap();
                let repaired = fixture.repaired;
                let model = &mut fixture.model;
                let result: Result<(), String> = (|| {
                    match edit {
                        ModelEdit::SeedView {
                            scope,
                            panes,
                            closed,
                        } => {
                            let bad_id = CardId::from_uuid(model.agents[1].id.as_uuid());
                            if closed.contains(&bad_id) {
                                record(&root, "UNEXPECTED-bad-shared-close");
                                return Err("Quarantined close was published".into());
                            }
                            for pane in &panes {
                                pane.validate()?;
                                if pane.id == bad_id && !repaired {
                                    record(&root, "UNEXPECTED-bad-shared-insert");
                                    return Err("Unverified synthetic pane was published".into());
                                }
                            }
                            let view = model
                                .views
                                .iter_mut()
                                .find(|view| view.scope == scope)
                                .ok_or("Unknown synthetic scope")?;
                            for id in closed {
                                view.apply(&Change::Remove { id })?;
                            }
                            for pane in panes {
                                view.apply(&Change::Insert { pane, before: None })?;
                            }
                            record(&root, "seed-applied");
                        }
                        ModelEdit::ChangeView { scope, change, .. } => {
                            let bad_id = CardId::from_uuid(model.agents[1].id.as_uuid());
                            let changed = match &change {
                                Change::Insert { pane, .. }
                                | Change::InsertIfEmpty { pane }
                                | Change::Update { pane, .. } => pane.id,
                                Change::Remove { id }
                                | Change::Move { id, .. }
                                | Change::Transfer { id, .. } => *id,
                            };
                            if changed == bad_id && !repaired {
                                record(&root, "UNEXPECTED-bad-shared-mutation");
                                return Err("Quarantined mutation was published".into());
                            }
                            model
                                .views
                                .iter_mut()
                                .find(|view| view.scope == scope)
                                .ok_or("Unknown synthetic scope")?
                                .apply(&change)?;
                        }
                        #[cfg(held_product_after)]
                        ModelEdit::SuggestConversationTitle { id, card, expected_revision, title, source }
                            if std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1")
                                && id == model.channels[0].id
                                && card == CardId::from_uuid(model.agents[0].id.as_uuid())
                                && model.views.iter().any(|view| view.scope == Scope::Channel(id)
                                    && view.revision == expected_revision && view.primary_chat() == Some(card))
                                && title == "Hidden conversation recovered"
                                && source == cagents::model::ChannelTitleSource::Provider =>
                        {
                            model.channels[0].title = title;
                            model.channels[0].title_source = source;
                            record(&root, "hidden-healthy-ready-title-published");
                        }
                        _ => return Err("Mutation outside this native fixture".into()),
                    }
                    model.sequence += 1;
                    Ok(())
                })();
                match result {
                    Ok(()) => ServerMessage::Response {
                        response: ServerResponse::WorkspaceModel {
                            model: model.clone(),
                        },
                    },
                    Err(_) => error("Synthetic fixture refused an inadmissible workspace edit"),
                }
            }
            ClientRequest::List | ClientRequest::WatchTerminals => ServerMessage::Response {
                response: ServerResponse::Listed { terminals: vec![] },
            },
            ClientRequest::ChatList => ServerMessage::Response {
                response: ServerResponse::ChatListed {
                    sessions: vec![healthy.clone(), held.clone()],
                },
            },
            ClientRequest::ChatAttach { session } => {
                let repaired = state.lock().unwrap().repaired;
                if session == healthy || (repaired && session == held) {
                    if session == healthy && std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1") {
                        record(&root, "healthy-attach-held-before-ready");
                        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                        while !root.join("release-hidden-healthy").exists() {
                            if std::time::Instant::now() >= deadline {
                                record(&root, "UNEXPECTED-hidden-attach-deadline");
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                    }
                    record(
                        &root,
                        if session == healthy {
                            "healthy-attach-requested"
                        } else {
                            "repaired-held-attach-requested"
                        },
                    );
                    ServerMessage::Response {
                        response: ServerResponse::ChatAttached {
                            snapshot: snapshot(&session),
                            session,
                        },
                    }
                } else {
                    record(&root, "UNEXPECTED-unverified-attach");
                    error("Synthetic pane is unverified")
                }
            }
            ClientRequest::ChatRead { chat_id, .. } => ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                },
            },
            ClientRequest::ChatVisibility { .. }
            | ClientRequest::ChatAcknowledge { .. }
            | ClientRequest::ChatDetach { .. } => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            ClientRequest::WatchNotifications { .. } => ServerMessage::Response {
                response: ServerResponse::Notifications {
                    snapshot: Default::default(),
                },
            },
            ClientRequest::WatchWork => ServerMessage::Response {
                response: ServerResponse::Work(cagents::work::Response::Watching {
                    revision: 1,
                    board: Box::new(cagents::work::Board {
                        generated_at_unix_ms: 0,
                        stale_after_days: 7,
                        projects: vec![],
                        items: vec![],
                        linear_unavailable: None,
                    }),
                }),
            },
            ClientRequest::Attach { .. } => {
                record(&root, "UNEXPECTED-terminal-attach");
                error("This fixture never attaches a terminal")
            }
            ClientRequest::ChatStart { .. }
            | ClientRequest::Spawn { .. }
            | ClientRequest::ChatCommand { .. } => {
                record(&root, "UNEXPECTED-provider-effect");
                error("This fixture never starts or sends to a provider")
            }
            ClientRequest::ChatClose { .. }
            | ClientRequest::ChatCloseRetained { .. }
            | ClientRequest::ShutdownTerminal { .. } => {
                record(&root, "UNEXPECTED-runtime-close");
                error("This fixture never closes a provider")
            }
            _ => error("Request outside this synthetic native fixture"),
        };
        if terminal_ipc::write_frame(
            &mut *writer.lock().unwrap(),
            &ServerEnvelope {
                version: terminal_ipc::PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof(),
                message: response,
            },
        )
        .is_err()
        {
            break;
        }
        if explicit_read { record(&root, "explicit-read-replied"); }
        let start_performance = if watching && std::env::var("HELD_FIXTURE_PAYLOAD").as_deref() == Ok("long") {
            let mut fixture = state.lock().unwrap();
            let first = !fixture.performance_started;
            fixture.performance_started = true;
            first
        } else { false };
        if start_performance {
            let writer = writer.clone();
            let state = state.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                while !root.join("performance-stream").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let started = std::time::Instant::now();
                for index in 0..150 {
                    let mut writer = writer.lock().unwrap();
                    let mut fixture = state.lock().unwrap();
                    let edit = ModelEdit::RetitleChannel {
                        id: fixture.model.channels[0].id,
                        title: format!("Saved recovery fixture {}", index % 2),
                    };
                    fixture.model.channels[0].title = format!("Saved recovery fixture {}", index % 2);
                    fixture.model.sequence += 1;
                    let sequence = fixture.model.sequence;
                    let message = ServerEnvelope {
                        version: terminal_ipc::PROTOCOL_VERSION,
                        request_id: None,
                        proof: proof(),
                        message: ServerMessage::WorkspaceModelEdited { sequence, edit },
                    };
                    drop(fixture);
                    if terminal_ipc::write_frame(&mut *writer, &message).is_err() { return; }
                    drop(writer);
                    let sent = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                    record(&root, &format!("performance-update-{index}-{sequence}-unix-ns-{sent}-elapsed-ns-{}", started.elapsed().as_nanos()));
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                record(&root, "performance-stream-complete");
            });
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if Authority::from_environment() != Authority::default() {
        return Err(
            "Genuine User launch required; inherited caller authority must remain unchanged".into(),
        );
    }
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Private fixture root required")?,
    );
    if !root.is_absolute() {
        return Err("Private fixture root must be absolute".into());
    }
    if fs::symlink_metadata(&root)?.file_type().is_symlink() {
        return Err("Refusing a symbolic-link fixture root".into());
    }
    for key in [
        "HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "TMPDIR",
    ] {
        if !PathBuf::from(std::env::var_os(key).ok_or("Private fixture environment required")?)
            .starts_with(&root)
        {
            return Err("Refusing nonprivate fixture environment".into());
        }
    }
    let mode = std::env::args().nth(2).unwrap_or_else(|| "stale".into());
    if !matches!(mode.as_str(), "stale" | "closed" | "closed-history") {
        return Err("Unknown native fixture mode".into());
    }
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("Private runtime required")?);
    if runtime != root.join("runtime")
        || fs::symlink_metadata(&runtime)?.file_type().is_symlink()
    {
        return Err("Refusing nonprivate native runtime".into());
    }
    let repository = root
        .parent()
        .ok_or("Fixture parent missing")?
        .join("project");
    fs::create_dir_all(&repository)?;
    let project: ProjectId = "00000000-0000-4000-8000-000000001922".parse()?;
    let channel: ChannelId = "00000000-0000-4000-8000-000000002922".parse()?;
    let healthy_id: CardId = "00000000-0000-4000-8000-000000003922".parse()?;
    let bad_id: CardId = "00000000-0000-4000-8000-000000004922".parse()?;
    let session = |id: &str, title: &str| -> Session {
        Session {
            id: id.parse().unwrap(),
            launch: Launch {
                provider: Provider::Claude,
                session_id: Some(id.into()),
                recipe: LaunchRecipe {
                    program: "synthetic-offline-provider".into(),
                    arguments: vec![title.into()],
                    working_directory: repository.clone(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    };
    let healthy = session("00000000-0000-4000-8000-000000005922", "healthy");
    let held = session("00000000-0000-4000-8000-000000006922", "held");
    let mut cards = [
        (healthy_id, "Healthy saved pane", healthy.clone()),
        (bad_id, "Unavailable saved pane", held.clone()),
    ]
    .map(|(id, title, session)| CardState {
        id,
        generation: 0,
        title: title.into(),
        width_fraction: 0.5,
        kind: CardKind::Chat(session),
        existence: if mode == "closed-history" && id == bad_id {
            Existence::Closed {
                tombstone: Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 0,
                    reason: Some("synthetic closed history".into()),
                },
            }
        } else {
            Existence::Open
        },
        exposure: Exposure::Visible,
        residency: Residency::Cold,
    });
    let terminal_surface = std::env::var("HELD_FIXTURE_SURFACE").as_deref() == Ok("terminal");
    if terminal_surface {
        let issued = proof();
        cards[1].kind = CardKind::Terminal(TerminalSession {
            id: held.id,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: held.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: issued.host_session_id,
                boot_id: issued.boot_id,
                host_incarnation: issued.host_incarnation,
            },
        });
    }
    let payload = std::env::var("HELD_FIXTURE_PAYLOAD").unwrap_or_else(|_| "short".into());
    let draft = match payload.as_str() {
        "empty" => String::new(),
        "long" => "Saved Unicode draft: café 🌿 日本語. Keep every line.\n".repeat(500),
        _ => "Keep this saved draft intact.\nIt belongs to the unavailable pane.".into(),
    };
    fs::write(root.join("expected-draft.txt"), &draft)?;
    let attachment = root.parent().ok_or("Fixture parent missing")?.join("synthetic-saved-attachment.txt");
    fs::write(&attachment, "Synthetic saved attachment. No private content.\n")?;
    fs::write(root.join("expected-attachment.txt"), attachment.to_string_lossy().as_bytes())?;
    if std::env::args().nth(3).as_deref() == Some("--inspect") {
        let database = SessionRuntime::default_database_path()?;
        let store = StateStore::open(&database, cagents::session::current_boot_id()?)?;
        let workspace_id: WorkspaceId = fs::read_to_string(root.join("workspace-id"))?.trim().parse()?;
        let saved = store.load_workspace(workspace_id, None)?.ok_or("Saved workspace missing")?;
        if std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1") {
            let active: ChannelId = "00000000-0000-4000-8000-000000008922".parse()?;
            let settings: CardId = "00000000-0000-4000-8000-000000009922".parse()?;
            if saved.active_channel != Some(active) || saved.focused != Some(settings)
                || saved.channels.iter().find(|channel| channel.id == active).and_then(|channel| channel.focused) != Some(settings)
            { return Err("Hidden recovery changed the active saved scope or focus".into()); }
        }
        let bad = saved.cards.iter().find(|card| card.id == bad_id).ok_or("Saved rejected pane missing")?;
        if bad.kind != cards[1].kind { return Err("Saved rejected source changed".into()); }
        let should_be_closed = std::env::args().nth(4).as_deref() == Some("closed");
        if bad.existence.is_open() == should_be_closed { return Err("Saved pane existence changed unexpectedly".into()); }
        if saved.chat_drafts.get(&bad_id).map(String::as_str).unwrap_or("") != draft { return Err("Saved draft changed".into()); }
        let expected = if payload == "empty" { vec![] } else { vec![cagents::chat::Attachment::file(attachment)] };
        if saved.chat_draft_attachments.get(&bad_id).cloned().unwrap_or_default() != expected { return Err("Saved attachments changed".into()); }
        if root.join("healthy-draft-expected").exists() && saved.chat_drafts.get(&healthy_id).map(String::as_str) != Some(fs::read_to_string(root.join("healthy-draft-expected"))?.as_str()) { return Err("Healthy sibling edited draft did not persist".into()); }
        println!("typed-saved-source-draft-attachments-and-existence-verified");
        return Ok(());
    }
    let hidden_mode = std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1");
    let held_channel_title_source = if hidden_mode {
        cagents::model::ChannelTitleSource::Prompt
    } else {
        Default::default()
    };
    let mut workspace = WorkspaceState {
        project_order: vec![project],
        cards: cards.to_vec(),
        focused: Some(healthy_id),
        active_channel: Some(channel),
        workspace_model_identity: Some("00000000-0000-4000-8000-000000007922".parse()?),
        chat_drafts: HashMap::from([(
            bad_id,
            draft,
        )]),
        chat_draft_attachments: if payload == "empty" { HashMap::new() } else { HashMap::from([(bad_id, vec![cagents::chat::Attachment::file(attachment)])]) },
        ..Default::default()
    };
    fs::write(root.join("workspace-id"), workspace.id.to_string())?;
    workspace.channels.push(ProjectChannel {
        id: channel,
        project_id: project,
        title: "Saved recovery fixture".into(),
        title_source: held_channel_title_source,
        ordinal: 1,
        shared: true,
        cards: workspace
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| card.id)
            .collect(),
        focused: Some(healthy_id),
    });
    let active_channel: ChannelId = "00000000-0000-4000-8000-000000008922".parse()?;
    if hidden_mode {
        let settings: CardId = "00000000-0000-4000-8000-000000009922".parse()?;
        workspace.cards.push(CardState {
            id: settings,
            generation: 0,
            title: "Active settings".into(),
            width_fraction: 1.0,
            kind: CardKind::Settings,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Cold,
        });
        workspace.channels.push(ProjectChannel {
            id: active_channel,
            project_id: project,
            title: "Active settings fixture".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: true,
            cards: vec![settings],
            focused: Some(settings),
        });
        workspace.active_channel = Some(active_channel);
        workspace.focused = Some(settings);
    }
    let database = SessionRuntime::default_database_path()?;
    if database.exists() {
        return Err(
            "Refusing to overwrite an existing native fixture; use a fresh private root".into(),
        );
    }
    fs::write(root.join("database-path"), database.to_string_lossy().as_bytes())?;
    let store = StateStore::open(&database, cagents::session::current_boot_id()?)?;
    store.save_project(&ProjectState {
        id: project,
        generation: 0,
        name: "Recovery fixture".into(),
        repository: repository.clone(),
        kind: ProjectKind::Folder,
        existence: Existence::Open,
    })?;
    store.save_workspace(
        &workspace,
        SnapshotBase {
            workspace: 0,
            cards: &BTreeMap::new(),
        },
    )?;
    drop(store);
    let panes = cards
        .iter()
        .map(|card| Pane {
            id: card.id,
            title: card.title.clone(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: card.width_fraction,
            kind: card.kind.clone(),
            scope: None,
        })
        .collect();
    let mut stale_owner = held.clone();
    stale_owner.proof.as_mut().unwrap().host_incarnation = "synthetic-stale-incarnation".into();
    let mut agents =
        [(healthy_id, healthy.clone()), (bad_id, stale_owner)].map(|(id, session)| Agent {
            id: AgentId::from_uuid(id.as_uuid()),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: repository.clone(),
                    directory: repository.clone(),
                    placement: Some(Scope::Channel(channel)),
                },
                surface: Surface::Chat,
                model: Some("synthetic".into()),
                permission_mode: None,
                reasoning_effort: Some("low".into()),
            },
            placement: Scope::Channel(channel),
            session: Some(HostedSession::Chat(session)),
            console: None,
            created_ms: 0,
            closed: id == bad_id && mode != "stale",
        });
    if terminal_surface {
        let CardKind::Terminal(terminal) = &cards[1].kind else { unreachable!() };
        let mut stale = terminal.clone();
        if let TerminalContinuity::DetachedSameBoot { host_incarnation, .. } = &mut stale.continuity { *host_incarnation = "synthetic-stale-incarnation".into(); }
        agents[1].spec.surface = Surface::Terminal;
        agents[1].session = Some(HostedSession::Terminal(stale.clone()));
        agents[1].console = Some(stale);
    }
    let mut agents = agents.to_vec();
    if payload == "long" {
        for index in 0..30 {
            let mut agent = agents[0].clone();
            agent.id = format!("00000000-0000-4000-8000-{:012x}", 0x10000 + index).parse()?;
            agent.origin = Origin::Automation { id: agent.id.as_uuid() };
            agent.name = Some(format!("Synthetic idle peer {index}"));
            let mut session = healthy.clone();
            session.id = format!("00000000-0000-4000-8000-{:012x}", 0x20000 + index).parse()?;
            session.launch.session_id = Some(session.id.to_string());
            agent.session = Some(HostedSession::Chat(session));
            agents.push(agent);
        }
    }
    let state = Arc::new(Mutex::new(FixtureState {
        repaired: false,
        performance_started: false,
        model: WorkspaceModel {
            identity: workspace.workspace_model_identity.unwrap(),
            sequence: 1,
            projects: vec![ModelProject {
                repository: repository.clone(),
                name: "Recovery fixture".into(),
                kind: ProjectKind::Folder,
                ordinal: 1,
            }],
            channels: vec![ModelChannel {
                id: channel,
                repository,
                title: "Saved recovery fixture".into(),
                title_source: held_channel_title_source,
                ordinal: 1,
            }],
            views: vec![View {
                scope: Scope::Channel(channel),
                revision: 1,
                panes,
                retired: vec![],
            }],
            agents,
            ..Default::default()
        },
    }));
    if hidden_mode {
        let mut fixture = state.lock().unwrap();
        let repository = fixture.model.projects[0].repository.clone();
        fixture.model.channels.push(ModelChannel {
            id: active_channel,
            repository,
            title: "Active settings fixture".into(),
            title_source: Default::default(),
            ordinal: 0,
        });
        fixture.model.views.push(View::empty(Scope::Channel(active_channel)));
    }
    let socket = terminal_ipc::daemon_socket_path()?;
    fs::create_dir_all(socket.parent().ok_or("Socket parent missing")?)?;
    let listener = UnixListener::bind(&socket)?;
    record(&root, "fixture-ready");
    for stream in listener.incoming() {
        let stream = stream?;
        let state = state.clone();
        let root = root.clone();
        let healthy = healthy.clone();
        let held = held.clone();
        std::thread::spawn(move || client(stream, state, root, healthy, held));
    }
    Ok(())
}
