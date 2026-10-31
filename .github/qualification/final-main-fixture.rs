//! Synthetic, private final-main integration fixture. No actor/provider runs.
//! Public StateStore and the real workspace-model wire are the only producers.
use cagents::{
    account::{self, Account},
    agent::Provider,
    agent_execution::{Agent, Authority, HostedSession, Origin, Spec, Surface, Target},
    chat::{Launch, Session},
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
    os::unix::{
        fs::MetadataExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const A: &str = "00000000-0000-4000-8000-000000002018";
const B: &str = "00000000-0000-4000-8000-000000002019";
const CHANNEL: &str = "00000000-0000-4000-8000-000000002020";
const PROJECT: &str = "00000000-0000-4000-8000-000000002021";
const BAD_DRAFT: &str = "Rejected sibling keeps its saved café draft.\nNever submit or start it.";
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 80, 153, 22, 253, 31, 0, 4,
    12, 2, 21, 73, 175, 151, 59, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
static EVENTS: Mutex<()> = Mutex::new(());

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: "synthetic-integration-host".into(),
        boot_id: cagents::session::current_boot_id().expect("kernel boot"),
        host_incarnation: "synthetic-current-incarnation".into(),
    }
}
fn event(root: &Path, mut value: serde_json::Value) {
    // Connections and the peer watcher share one receipt lock; formatting a
    // Value directly into separate append descriptors could interleave lines.
    let _guard = EVENTS.lock().unwrap();
    value.as_object_mut().unwrap().insert(
        "unix_ns".into(),
        serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ),
    );
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("fixture.jsonl"))
        .unwrap();
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    output.write_all(&line).unwrap();
}
fn refused() -> ServerMessage {
    ServerMessage::Error(ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        "Request outside the private read-only draft fixture",
        false,
    ))
}
fn private_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let info = fs::symlink_metadata(path)?;
    if !info.is_dir()
        || info.file_type().is_symlink()
        || info.mode() & 0o7777 != 0o700
        || fs::canonicalize(path)? != path
    {
        return Err("Expected a canonical private owned directory".into());
    }
    // Bind owner to this process without depending on an extra libc crate.
    let process_owner = fs::metadata("/proc/self")?.uid();
    if info.uid() != process_owner {
        return Err("Directory owner differs from the fixture caller".into());
    }
    Ok(())
}
fn sources(root: &Path) -> (Session, Session, Session) {
    let recipe = LaunchRecipe {
        program: "synthetic-offline-claude-never-execute".into(),
        arguments: vec!["--permission-mode".into(), "default".into()],
        working_directory: root.join("project"),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let initial = Session {
        id: "00000000-0000-4000-8000-000000002022".parse().unwrap(),
        launch: Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut chosen = initial.clone();
    chosen.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        root.join("peer-home").to_string_lossy().into(),
    );
    chosen
        .launch
        .recipe
        .environment
        .insert("SYNTHETIC_RECIPE_DIMENSION".into(), "peer-B".into());
    chosen.launch.recipe.unset_environment = ["SYNTHETIC_UNUSED_VALUE".into()].into();
    chosen.launch.recipe.working_directory = root.join("project/peer-directory");
    // Permission, provider, model and effort are constant. Account/home/cwd are
    // synthetic local fixture metadata, never real login or caller changes.
    let mut rejected = initial.clone();
    rejected.id = "00000000-0000-4000-8000-000000002023".parse().unwrap();
    rejected.launch.session_id = Some(rejected.id.to_string());
    rejected.proof = Some(proof());
    (initial, chosen, rejected)
}
fn bad_attachment(root: &Path) -> cagents::chat::Attachment {
    cagents::chat::Attachment {
        path: root.join("rejected-saved.png"),
        reference: Some(cagents::chat::ImageReference {
            id: "00000000-0000-4000-8000-000000002024".parse().unwrap(),
            number: 1,
            device: "00000000-0000-4000-8000-000000002025".parse().unwrap(),
            name: "Synthetic unavailable sibling image.png".into(),
            sha256: "6aa8d4c0d15e7b9c7a02ddb0699e41448ef178b2f18e83597b673c559bef04cb".into(),
            extension: "png".into(),
        }),
    }
}
struct State {
    model: WorkspaceModel,
    watcher_started: bool,
    chosen: Session,
}

fn edit(root: &Path, state: &mut State, edit: ModelEdit, initial: &Session) -> Result<(), String> {
    let id: CardId = A.parse().unwrap();
    let bad: CardId = B.parse().unwrap();
    match edit {
        ModelEdit::OpenProject {
            repository,
            name,
            kind,
        } if repository == root.join("project")
            && kind == ProjectKind::Folder
            && name == "project" => {}
        ModelEdit::SeedView {
            scope: Scope::Worktree { repository, path },
            panes,
            closed,
        } if repository == root.join("project")
            && path == repository
            && panes.is_empty()
            && closed.is_empty() =>
        {
            event(root, serde_json::json!({"event":"seed-empty-worktree"}));
        }
        ModelEdit::SeedView {
            scope,
            panes,
            closed,
        } if scope == Scope::Channel(CHANNEL.parse().unwrap()) => {
            if panes.len() != 1
                || panes[0].id != id
                || panes[0].kind != CardKind::Chat(initial.clone())
                || panes[0].scope.is_some()
                || panes[0].pending_terminal_start
                || !closed.is_empty()
            {
                return Err("Seed must contain only exact unstarted A, never rejected B".into());
            }
            panes[0].validate()?;
            event(
                root,
                serde_json::json!({"event":"seed-A-only","scope":scope,"panes":panes,"closed":closed}),
            );
        }
        ModelEdit::ChangeView {
            scope,
            expected_revision,
            change,
            ..
        } if scope == Scope::Channel(CHANNEL.parse().unwrap()) => {
            let Change::Update { before, pane } = &change else {
                return Err("Only presentation edits are allowed".into());
            };
            let current = state
                .model
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .ok_or("Missing view")?;
            let source = current
                .panes
                .iter()
                .find(|pane| pane.id == id)
                .ok_or("Missing A")?;
            if before.id != id
                || pane.id != id
                || before.id == bad
                || before.kind != source.kind
                || pane.kind != source.kind
                || before.scope.is_some()
                || pane.scope.is_some()
                || before.pending_terminal_start
                || pane.pending_terminal_start
            {
                return Err("A local edit changed exact source or attempted B publication".into());
            }
            if current.revision != expected_revision {
                return Err("Stale view revision".into());
            }
            if current.apply(&change)? {
                current.revision += 1;
            }
            event(
                root,
                serde_json::json!({"event":"presentation-A-only","change":change}),
            );
        }
        _ => return Err("Workspace mutation outside the exact fixture bootstrap".into()),
    }
    state.model.sequence += 1;
    Ok(())
}
fn client(mut stream: UnixStream, root: PathBuf, state: Arc<Mutex<State>>, initial: Session) {
    let writer = Arc::new(Mutex::new(stream.try_clone().unwrap()));
    let mut connected = false;
    while let Ok(Some(envelope)) = terminal_ipc::read_frame::<ClientEnvelope>(&mut stream) {
        let watching = matches!(&envelope.request, ClientRequest::WatchWorkspaceModel);
        let seed_scope = match &envelope.request {
            ClientRequest::EditWorkspaceModel {
                edit:
                    ModelEdit::SeedView {
                        scope,
                        panes,
                        closed,
                    },
            } => {
                event(
                    &root,
                    serde_json::json!({"event":"seed-request","request_id":envelope.request_id,
                    "scope":scope,"panes":panes,"closed":closed}),
                );
                Some(scope.clone())
            }
            _ => None,
        };
        if envelope.version != terminal_ipc::PROTOCOL_VERSION
            || (!matches!(&envelope.request, ClientRequest::Connect { .. })
                && (!connected || envelope.proof != Some(proof())))
        {
            event(
                &root,
                serde_json::json!({"event":"UNEXPECTED-envelope","request":format!("{:?}", envelope.request)}),
            );
            break;
        }
        let response = match envelope.request {
            ClientRequest::Connect { expected_host, .. }
                if expected_host.as_ref().is_none_or(|value| value == &proof()) =>
            {
                connected = true;
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof(),
                        resumed: vec![],
                    },
                }
            }
            ClientRequest::WatchWorkspaceModel | ClientRequest::ReadWorkspaceModel => {
                let model = state.lock().unwrap().model.clone();
                event(
                    &root,
                    serde_json::json!({"event":"full-model","model":model}),
                );
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel { model },
                }
            }
            ClientRequest::EditWorkspaceModel { edit: request } => {
                let mut state = state.lock().unwrap();
                match edit(&root, &mut state, request, &initial) {
                    Ok(()) => ServerMessage::Response {
                        response: ServerResponse::WorkspaceModel {
                            model: state.model.clone(),
                        },
                    },
                    Err(reason) => {
                        event(
                            &root,
                            serde_json::json!({"event":"UNEXPECTED-edit","reason":reason}),
                        );
                        refused()
                    }
                }
            }
            ClientRequest::List => ServerMessage::Response {
                response: ServerResponse::Listed { terminals: vec![] },
            },
            ClientRequest::ChatList => ServerMessage::Response {
                response: ServerResponse::ChatListed { sessions: vec![] },
            },
            ClientRequest::WatchTerminals => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            ClientRequest::WatchNotifications { .. } | ClientRequest::ReadNotifications => {
                ServerMessage::Response {
                    response: ServerResponse::Notifications {
                        snapshot: Default::default(),
                    },
                }
            }
            ClientRequest::NotificationPresence { .. }
            | ClientRequest::NotificationPreferencesChanged { .. } => ServerMessage::Response {
                response: ServerResponse::Accepted,
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
            other => {
                // Includes Authenticate, every ChatStart/Attach/Prepare/Close,
                // PTY/spawn/relaunch and every unlisted tracking mutation.
                event(
                    &root,
                    serde_json::json!({"event":"UNEXPECTED-request","request":format!("{other:?}")}),
                );
                refused()
            }
        };
        let acknowledged = match &response {
            ServerMessage::Response {
                response: ServerResponse::WorkspaceModel { model },
            } => Some(model.clone()),
            _ => None,
        };
        if terminal_ipc::write_frame(
            &mut *writer.lock().unwrap(),
            &ServerEnvelope {
                version: terminal_ipc::PROTOCOL_VERSION,
                request_id: Some(envelope.request_id),
                proof: proof(),
                message: response,
            },
        )
        .is_err()
        {
            return;
        }
        if let (Some(scope), Some(model)) = (seed_scope, acknowledged) {
            event(
                &root,
                serde_json::json!({"event":"seed-ack","scope":scope,"request_id":envelope.request_id,"full_model":model}),
            );
        }
        if watching {
            let first = {
                let mut state = state.lock().unwrap();
                let first = !state.watcher_started;
                state.watcher_started = true;
                first
            };
            if first {
                let (root, state, writer) = (root.clone(), state.clone(), writer.clone());
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
                    while !root.join("peer-update").exists() {
                        if std::time::Instant::now() >= deadline {
                            event(
                                &root,
                                serde_json::json!({"event":"UNEXPECTED-peer-deadline"}),
                            );
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    let marker = fs::symlink_metadata(root.join("peer-update")).unwrap();
                    if !marker.is_file()
                        || marker.file_type().is_symlink()
                        || marker.nlink() != 1
                        || marker.len() != 0
                    {
                        event(&root, serde_json::json!({"event":"UNEXPECTED-peer-marker"}));
                        return;
                    }
                    let mut state = state.lock().unwrap();
                    let chosen = state.chosen.clone();
                    let view = &mut state.model.views[0];
                    let before = view
                        .panes
                        .iter()
                        .find(|pane| pane.id.to_string() == A)
                        .unwrap()
                        .clone();
                    let mut pane = before.clone();
                    pane.kind = CardKind::Chat(chosen);
                    let change = Change::Update { before, pane };
                    let edit = ModelEdit::ChangeView {
                        scope: view.scope.clone(),
                        expected_revision: view.revision,
                        operation: OperationId::new().as_uuid(),
                        change: change.clone(),
                    };
                    view.apply(&change).unwrap();
                    view.revision += 1;
                    state.model.sequence += 1;
                    event(
                        &root,
                        serde_json::json!({"event":"peer-source-witness","edit":edit,"full_model":state.model}),
                    );
                    let result = terminal_ipc::write_frame(
                        &mut *writer.lock().unwrap(),
                        &ServerEnvelope {
                            version: terminal_ipc::PROTOCOL_VERSION,
                            request_id: None,
                            proof: proof(),
                            message: ServerMessage::WorkspaceModelEdited {
                                sequence: state.model.sequence,
                                edit,
                            },
                        },
                    );
                    event(
                        &root,
                        serde_json::json!({"event":if result.is_ok() {"peer-delivered"} else {"UNEXPECTED-peer-write"}}),
                    );
                });
            }
        }
    }
}
fn inspect(root: &Path, database: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // The driver first makes a real SQLite backup; opening normal StateStore
    // may normalize only that separate clone, never the live writer database.
    if database.parent() != Some(root.join("inspection").as_path())
        || fs::canonicalize(database)? != database
        || fs::symlink_metadata(database)?.file_type().is_symlink()
    {
        return Err("Inspection requires its exact private backup".into());
    }
    let store = StateStore::open(database, cagents::session::current_boot_id()?)?;
    let workspace_id: WorkspaceId = fs::read_to_string(root.join("workspace-id"))?.parse()?;
    let saved = store
        .load_workspace(workspace_id, None)?
        .ok_or("Workspace missing")?;
    let (_, chosen, rejected) = sources(root);
    let a: CardId = A.parse()?;
    let b: CardId = B.parse()?;
    let owner = saved
        .channels
        .iter()
        .find(|entry| entry.id.to_string() == CHANNEL)
        .ok_or("Channel missing")?;
    if saved.cards.len() != 2
        || owner.cards != vec![a, b]
        || owner.focused != Some(a)
        || saved.focused != Some(a)
        || saved.active_channel != Some(owner.id)
        || !owner.shared
        || saved.quarantined_panes != HashMap::from([(b, CardKind::Chat(rejected.clone()))])
        || saved
            .cards
            .iter()
            .find(|card| card.id == a)
            .is_none_or(|card| !card.existence.is_open() || card.kind != CardKind::Chat(chosen))
        || saved
            .cards
            .iter()
            .find(|card| card.id == b)
            .is_none_or(|card| !card.existence.is_open() || card.kind != CardKind::Chat(rejected))
        || saved.chat_drafts.get(&b).map(String::as_str) != Some(BAD_DRAFT)
        || saved.chat_draft_attachments.get(&b) != Some(&vec![bad_attachment(root)])
        || !saved.local_only_cards.is_empty()
    {
        return Err("Source, scope, focus, recipe or rejected sibling custody changed".into());
    }
    let images = saved
        .chat_draft_attachments
        .get(&a)
        .ok_or("Real clipboard image missing")?;
    if images.len() != 1 {
        return Err("Expected exactly one actual clipboard image".into());
    }
    let image = &images[0];
    let reference = image
        .reference
        .as_ref()
        .ok_or("Managed image receipt missing")?;
    if reference.name != "Pasted image"
        || reference.number != 1
        || reference.extension != "png"
        || cagents::chat::attachment_assets::attachment(reference)? != *image
        || cagents::chat::attachment_assets::read(reference, PNG.len())? != PNG
        || fs::read(root.join("rejected-saved.png"))? != PNG
        || cagents::chat::attachment_assets::validate(&bad_attachment(root)).is_ok()
    {
        return Err("Image identity, bytes or rejection evidence changed".into());
    }
    println!(
        "{}",
        serde_json::json!({"event":"typed-inspection","workspace":saved,
        "actual_image":image,"unavailable_sibling_asset_valid":false})
    );
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_some()
        || Authority::from_environment() != Authority::default()
    {
        return Err(
            "Genuine User parent required; inherited caller authority is not changed".into(),
        );
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 1 && !(args.len() == 3 && args[1] == "--inspect") {
        return Err("Pass private root [--inspect backup]".into());
    }
    let root = PathBuf::from(&args[0]);
    private_directory(&root)?;
    for (key, suffix) in [
        ("HOME", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("TMPDIR", "tmp"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ] {
        let value = PathBuf::from(std::env::var_os(key).ok_or("Private environment missing")?);
        if value != root.join(suffix) {
            return Err("Private XDG binding changed".into());
        }
        private_directory(&value)?;
    }
    if args.len() == 3 {
        return inspect(&root, &PathBuf::from(&args[2]));
    }
    for path in [
        "project",
        "project/peer-directory",
        "peer-home",
        "chooser-data",
    ] {
        fs::create_dir_all(root.join(path))?;
    }
    fs::write(
        root.join("chooser-data/synthetic-file.txt"),
        "Synthetic chooser data; never select or submit.\n",
    )?;
    fs::write(root.join("offered.png"), PNG)?;
    fs::write(root.join("rejected-saved.png"), PNG)?;
    let store = account::registry::Store::installation().ok_or("Account config unavailable")?;
    let peer_key = "00000000-0000-4000-8000-000000002026".parse()?;
    let peer_home = fs::canonicalize(root.join("peer-home"))?;
    let (_, linked) = store.update(|registry| {
        let linked = registry.link(Provider::Claude, peer_key, peer_home.clone(), None, None, 1)?;
        registry.set_label(
            &Account::linked(Provider::Claude, linked, peer_home),
            Some("Synthetic peer account".into()),
        );
        registry.set_label(
            &Account::system(Provider::Claude),
            Some("Synthetic initial account".into()),
        );
        Ok::<_, String>(linked)
    })?;
    linked.map_err(std::io::Error::other)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    account::cache::Cache::installation()
        .ok_or("Catalog path unavailable")?
        .store_catalog(
            Provider::Claude,
            account::cache::Catalog {
                observed_at: now,
                models: vec![
                    cagents::chat::Model {
                        id: "fixture-model-a".into(),
                        label: "Fixture model A".into(),
                        reasoning: vec!["low".into()],
                    },
                    cagents::chat::Model {
                        id: "fixture-model-b".into(),
                        label: "Fixture model B".into(),
                        reasoning: vec!["low".into()],
                    },
                ],
            },
        )?;
    let (initial, chosen, rejected) = sources(&root);
    assert!(initial.is_unstarted() && chosen.is_unstarted());
    let cards = [
        (A, "Idle image draft", initial.clone()),
        (B, "Unavailable saved sibling", rejected.clone()),
    ]
    .map(|(id, title, session)| CardState {
        id: id.parse().unwrap(),
        generation: 0,
        title: title.into(),
        width_fraction: 0.5,
        kind: CardKind::Chat(session),
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Cold,
    });
    let channel: ChannelId = CHANNEL.parse()?;
    let project: ProjectId = PROJECT.parse()?;
    let workspace = WorkspaceState {
        cards: cards.to_vec(),
        focused: Some(A.parse()?),
        active_channel: Some(channel),
        project_order: vec![project],
        workspace_model_identity: Some("00000000-0000-4000-8000-000000002027".parse()?),
        channels: vec![ProjectChannel {
            id: channel,
            project_id: project,
            title: "Draft peer fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![A.parse()?, B.parse()?],
            focused: Some(A.parse()?),
        }],
        chat_drafts: HashMap::from([(B.parse()?, BAD_DRAFT.into())]),
        chat_draft_attachments: HashMap::from([(B.parse()?, vec![bad_attachment(&root)])]),
        ..Default::default()
    };
    let database = SessionRuntime::default_database_path()?;
    if database.exists() {
        return Err("Refusing existing fixture state".into());
    }
    fs::write(
        root.join("database-path"),
        database.to_string_lossy().as_bytes(),
    )?;
    fs::write(root.join("workspace-id"), workspace.id.to_string())?;
    let store = StateStore::open(database, cagents::session::current_boot_id()?)?;
    store.save_project(&ProjectState {
        id: project,
        generation: 0,
        name: "project".into(),
        repository: root.join("project"),
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
    let mut wrong_issuer = rejected.clone();
    wrong_issuer.proof.as_mut().unwrap().host_incarnation = "synthetic-wrong-issuer".into();
    let model = WorkspaceModel {
        identity: workspace.workspace_model_identity.unwrap(),
        sequence: 10,
        projects: vec![ModelProject {
            repository: root.join("project"),
            name: "project".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
        channels: vec![ModelChannel {
            id: channel,
            repository: root.join("project"),
            title: "Draft peer fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        }],
        views: vec![
            View {
                scope: Scope::Channel(channel),
                revision: 7,
                retired: vec![],
                panes: cards
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
                    .collect(),
            },
            View::empty(Scope::Worktree {
                repository: root.join("project"),
                path: root.join("project"),
            }),
        ],
        agents: vec![Agent {
            id: AgentId::from_uuid(B.parse::<CardId>()?.as_uuid()),
            name: Some("Synthetic rejected owner".into()),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: root.join("project"),
                    directory: root.join("project"),
                    placement: Some(Scope::Channel(channel)),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Scope::Channel(channel),
            session: Some(HostedSession::Chat(wrong_issuer)),
            console: None,
            created_ms: 0,
            closed: false,
        }],
        ..Default::default()
    };
    event(
        &root,
        serde_json::json!({"event":"source","initial":initial,"chosen":chosen,"rejected":rejected,"connection_proof":proof(),"full_model":model}),
    );
    let state = Arc::new(Mutex::new(State {
        model,
        watcher_started: false,
        chosen,
    }));
    let socket = terminal_ipc::daemon_socket_path()?;
    fs::create_dir_all(socket.parent().ok_or("Socket parent")?)?;
    let listener = UnixListener::bind(socket)?;
    event(&root, serde_json::json!({"event":"ready"}));
    for stream in listener.incoming() {
        let (root, state, initial) = (root.clone(), state.clone(), initial.clone());
        std::thread::spawn(move || client(stream.unwrap(), root, state, initial));
    }
    Ok(())
}
