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
    os::unix::{fs::MetadataExt, net::{UnixListener, UnixStream}},
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
const COMPOSITION_USER: &str = "Actual user request: keep my saved work while I open another channel.";
const COMPOSITION_READY: &str = "Composition transcript is ready.";
const COMPOSITION_CONTEXT: &str = "<system-reminder>\r\nSYNTHETIC_CONTEXT_BEGIN\nThis is synthetic provider context, not a user instruction.\nKeep literal **Markdown**, <tags>, café and 日本語 unchanged.\nThe saved task and its attachment belong to their original pane.\nSYNTHETIC_CONTEXT_END  \r\n</system-reminder>";
// Offered by the owned X11 clipboard, then staged by the real ImageSaved path.
// This is separate from the deliberately unavailable saved attachment below.
const MANAGED_PNG: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 80, 153, 22, 253, 31, 0, 4, 12, 2, 21, 73, 175, 151, 59, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130];

fn managed_image(root: &std::path::Path) -> Result<cagents::chat::Attachment, Box<dyn std::error::Error>> {
    let path = root.join("image-only-attachment.txt");
    let directory = fs::symlink_metadata(root)?;
    let metadata = fs::symlink_metadata(&path)?;
    if fs::canonicalize(root)? != root || !directory.is_dir() || directory.mode() & 0o7777 != 0o700
        || !metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1
        || metadata.uid() != directory.uid() || metadata.mode() & 0o7777 != 0o600 || metadata.len() > 4096 {
        return Err("Managed image observation is not a private fresh file".into());
    }
    let raw = fs::read_to_string(&path)?;
    let fields = raw.strip_suffix('\n').ok_or("Image observation must end in one newline")?.split('\n').collect::<Vec<_>>();
    if fields.len() != 7 || fields.iter().any(|field| field.is_empty() || field.chars().any(char::is_control)) {
        return Err("Managed image observation fields changed".into());
    }
    let reference = cagents::chat::ImageReference {
        id: fields[1].parse()?, number: fields[2].parse()?, device: fields[3].parse()?,
        name: fields[4].into(), sha256: fields[5].into(), extension: fields[6].into(),
    };
    reference.validate()?;
    if reference.id.to_string() != fields[1] || reference.number.to_string() != fields[2]
        || reference.device.to_string() != fields[3] || reference.number != 1
        || reference.name != "Pasted image" || reference.extension != "png" {
        return Err("Managed image observation is not the first real PNG paste".into());
    }
    let image = cagents::chat::Attachment { path: fields[0].into(), reference: Some(reference) };
    if !image.path.starts_with(root.join("data"))
        || fs::canonicalize(&image.path)?.as_os_str() != std::ffi::OsStr::new(fields[0]) {
        return Err("Managed image observation escaped private asset storage".into());
    }
    cagents::chat::attachment_assets::validate(&image)?;
    let reference = image.reference.as_ref().unwrap();
    if cagents::chat::attachment_assets::attachment(reference)? != image
        || cagents::chat::attachment_assets::read(reference, MANAGED_PNG.len())? != MANAGED_PNG {
        return Err("Managed image registry, exact reference or PNG bytes changed".into());
    }
    Ok(image)
}
fn composition_copy_source() -> cagents::chat::TextCopySource {
    cagents::chat::TextCopySource {
        cache: "00000000-0000-4000-8000-000000012922".parse().unwrap(),
        scope: cagents::chat::TextCopyScope::Conversation,
        token: 1, revision: 1, bytes: COMPOSITION_CONTEXT.len() as u64,
    }
}
fn snapshot(session: &Session) -> Snapshot {
    if std::env::var("HELD_FIXTURE_IMAGE_ONLY").as_deref() == Ok("1") {
        return Snapshot {
            ready: true, generation: 1, session_id: session.launch.session_id.clone(),
            finished_at: Some(1_700_000_000_000),
            capabilities: cagents::chat::Capabilities { attachments: true, ..Default::default() },
            items: vec![Arc::new(cagents::chat::Item {
                id: "image-only-ready".into(), role: cagents::chat::Role::Assistant,
                content: cagents::chat::ItemContent::Text {
                    text: "Verified conversation ready for image-only retirement.".into(),
                    thinking: false, phase: cagents::chat::Phase::Final,
                }, at: Some(1_700_000_000_000), started_at: None,
            })],
            ..Default::default()
        };
    }
    if std::env::var("HELD_FIXTURE_COMPOSITION").as_deref() == Ok("1") {
        let text = |id: &str, role, body: &str| Arc::new(cagents::chat::Item {
            id: id.into(), role,
            content: cagents::chat::ItemContent::Text {
                text: body.into(), thinking: false, phase: cagents::chat::Phase::Final,
            }, at: None, started_at: None,
        });
        return Snapshot {
            ready: true, generation: 1, session_id: session.launch.session_id.clone(),
            items: vec![
                text("composition-user", cagents::chat::Role::User, COMPOSITION_USER),
                Arc::new(cagents::chat::Item {
                    id: "composition-context".into(), role: cagents::chat::Role::System,
                    content: cagents::chat::ItemContent::Delivery {
                        kind: cagents::chat::DeliveryKind::Notice, from: String::new(),
                        sender: String::new(), summary: "Claude context".into(),
                        body: COMPOSITION_CONTEXT.into(),
                    }, at: None, started_at: None,
                }),
                text("composition-ready", cagents::chat::Role::Assistant, COMPOSITION_READY),
            ],
            text_copy_sources: cagents::chat::TextCopySources {
                conversation_revision: 1,
                conversation: vec![None, Some(composition_copy_source()), None],
                ..Default::default()
            },
            ..Default::default()
        };
    }
    Snapshot {
        ready: true,
        session_id: session.launch.session_id.clone(),
        title: (std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1"))
            .then(|| "Hidden conversation recovered".into()),
        items: if std::env::var("HELD_FIXTURE_PEER").as_deref() == Ok("1") {
            vec![Arc::new(cagents::chat::Item {
                id: "peer-removal-ready".into(), role: cagents::chat::Role::Assistant,
                content: cagents::chat::ItemContent::Text {
                    text: "Verified conversation ready for peer removal.".into(), thinking: false,
                    phase: cagents::chat::Phase::Final,
                }, at: None, started_at: None,
            })]
        } else if session.launch.recipe.arguments.first().map(String::as_str) == Some("ordinary-retained-source") {
            vec![Arc::new(cagents::chat::Item {
                id: "ordinary-history-ready".into(), role: cagents::chat::Role::Assistant,
                content: cagents::chat::ItemContent::Text {
                    text: "Ordinary History recovery is ready.".into(), thinking: false,
                    phase: cagents::chat::Phase::Final,
                }, at: None, started_at: None,
            })]
        } else if std::env::var("HELD_FIXTURE_PAYLOAD").as_deref() == Ok("long") {
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
fn ordinary_source(healthy: &Session) -> Session {
    let mut source = healthy.clone();
    source.id = "00000000-0000-4000-8000-000000010922".parse().unwrap();
    source.launch.session_id = Some(source.id.to_string());
    source.launch.recipe.arguments = vec!["ordinary-retained-source".into()];
    source
}
fn ordinary_card() -> CardId {
    "00000000-0000-4000-8000-000000011922".parse().unwrap()
}
const ORDINARY_DRAFT: &str = "Keep the ordinary retained draft intact.\nOriginal café 🌿 text.";
struct FixtureState {
    model: WorkspaceModel,
    repaired: bool,
    performance_started: bool,
    ordinary_started: bool,
    peer_removal_started: bool,
    composition_channel: Option<ChannelId>,
    image_removal_started: bool,
}

fn composition_edit(root: &std::path::Path, fixture: &mut FixtureState, edit: ModelEdit, healthy: &Session) -> ServerMessage {
    let FixtureState { model, composition_channel, .. } = fixture;
    let original = Scope::Channel(model.channels[0].id);
    let healthy_id = CardId::from_uuid(model.agents[0].id.as_uuid());
    let result: Result<(), String> = (|| {
        match edit {
            ModelEdit::OpenProject { repository, name, kind }
                if repository == model.projects[0].repository && kind == ProjectKind::Folder
                    && (name == "project" || name == model.projects[0].name) => {
                model.projects[0].name = name;
            }
            ModelEdit::CreateChannel { id, repository, title, title_source }
                if root.join("composition-create-allowed").exists()
                    && composition_channel.is_none()
                    && repository == model.projects[0].repository && title.is_empty()
                    && title_source == ChannelTitleSource::Manual
                    && !model.channels.iter().any(|channel| channel.id == id) => {
                let ordinal = model.channels.iter().map(|channel| channel.ordinal).max().unwrap_or(0) + 1;
                model.channels.push(ModelChannel { id, repository, title, title_source, ordinal });
                model.views.push(View::empty(Scope::Channel(id)));
                *composition_channel = Some(id);
                fs::write(root.join("composition-channel-id"), id.to_string()).map_err(|error| error.to_string())?;
            }
            ModelEdit::SeedView { scope, panes, closed } if closed.is_empty() => {
                let private_worktree = Scope::Worktree {
                    repository: healthy.launch.recipe.working_directory.clone(),
                    path: healthy.launch.recipe.working_directory.clone(),
                };
                let new_scope = (*composition_channel).map(Scope::Channel);
                let view = model.views.iter_mut().find(|view| view.scope == scope).ok_or("Unknown composition scope")?;
                if scope == original {
                    for pane in panes {
                        pane.validate()?;
                        if pane.id != healthy_id || pane.kind != CardKind::Chat(healthy.clone())
                            || pane.scope.is_some() || pane.pending_terminal_start
                            || !view.panes.iter().any(|saved| saved.id == pane.id && saved.kind == pane.kind) {
                            return Err("Composition seed cannot publish an unverified source".into());
                        }
                    }
                } else if (scope != private_worktree && Some(scope) != new_scope)
                    || !panes.is_empty() || !view.panes.is_empty() || !view.retired.is_empty() {
                    return Err("Composition bootstrap requires an exact empty private scope".into());
                }
            }
            ModelEdit::ChangeView { scope, expected_revision, change: Change::Update { before, pane }, .. }
                if scope == original && before.id == healthy_id && pane.id == healthy_id
                    && before.kind == CardKind::Chat(healthy.clone()) && pane.kind == before.kind
                    && before.scope.is_none() && pane.scope.is_none()
                    && !before.pending_terminal_start && !pane.pending_terminal_start => {
                let view = model.views.iter_mut().find(|view| view.scope == scope).ok_or("Missing composition scope")?;
                if view.revision != expected_revision { return Err("Stale composition presentation update".into()); }
                if view.apply(&Change::Update { before, pane })? { view.revision += 1; }
            }
            _ => return Err("Mutation outside composition bootstrap".into()),
        }
        model.sequence += 1;
        Ok(())
    })();
    match result {
        Ok(()) => ServerMessage::Response { response: ServerResponse::WorkspaceModel { model: model.clone() } },
        Err(reason) => {
            record(root, &format!("UNEXPECTED-composition-model-mutation {reason}"));
            error("Composition fixture refused a workspace mutation")
        }
    }
}

fn peer_edit(root: &std::path::Path, model: &mut WorkspaceModel, edit: ModelEdit, healthy: &Session) -> ServerMessage {
    let id = CardId::from_uuid(model.agents[0].id.as_uuid());
    let expected_scope = Scope::Channel(model.channels[0].id);
    let result: Result<(), String> = (|| {
        match edit {
            ModelEdit::OpenProject { repository, name, kind }
                if repository == model.projects[0].repository && kind == ProjectKind::Folder
                    && (name == "project" || name == model.projects[0].name) => {
                model.projects[0].name = name;
            }
            ModelEdit::SeedView { scope: Scope::Worktree { repository, path }, panes, closed }
                if repository == model.projects[0].repository && path == repository
                    && panes.is_empty() && closed.is_empty() => {
                let scope = Scope::Worktree { repository, path };
                let mut views = model.views.iter().filter(|view| view.scope == scope);
                if !views.next().is_some_and(|view| view.panes.is_empty() && view.retired.is_empty())
                    || views.next().is_some() {
                    return Err("Private Worktree bootstrap changed".into());
                }
                record(root, "peer-empty-private-worktree-seed-checked");
            }
            ModelEdit::SeedView { scope, panes, closed }
                if scope == expected_scope && closed.is_empty() => {
                let view = model.views.iter_mut().find(|view| view.scope == scope).ok_or("Missing peer scope")?;
                if view.retired.contains(&id) && !panes.is_empty() { return Err("Seed after peer retirement".into()); }
                for pane in panes {
                    pane.validate()?;
                    if pane.id != id || pane.kind != CardKind::Chat(healthy.clone())
                        || pane.scope.is_some() || pane.pending_terminal_start {
                        return Err("Unexpected peer seed source".into());
                    }
                    if !view.panes.iter().any(|current| current.id == id && current.kind == pane.kind) {
                        return Err("Peer seed cannot introduce a source".into());
                    }
                }
                record(root, "peer-existing-seed-checked");
            }
            ModelEdit::ChangeView { scope, expected_revision, change: Change::Update { before, pane }, .. }
                if scope == expected_scope && before.id == id && pane.id == id
                    && before.kind == CardKind::Chat(healthy.clone()) && pane.kind == before.kind
                    && before.scope.is_none() && pane.scope.is_none()
                    && !before.pending_terminal_start && !pane.pending_terminal_start => {
                let view = model.views.iter_mut().find(|view| view.scope == scope).ok_or("Missing peer scope")?;
                if view.retired.contains(&id) || view.revision != expected_revision {
                    return Err("Peer update after retirement or stale revision".into());
                }
                if view.apply(&Change::Update { before, pane })? { view.revision += 1; }
                record(root, "peer-presentation-update-checked");
            }
            _ => return Err("Mutation outside peer-removal bootstrap".into()),
        }
        model.sequence += 1;
        Ok(())
    })();
    match result {
        Ok(()) => ServerMessage::Response { response: ServerResponse::WorkspaceModel { model: model.clone() } },
        Err(reason) => {
            record(root, &format!("UNEXPECTED-peer-model-mutation {reason}"));
            error("Peer-removal fixture refused a workspace mutation")
        }
    }
}

fn image_edit(root: &std::path::Path, model: &mut WorkspaceModel, edit: ModelEdit, healthy: &Session) -> ServerMessage {
    let id = CardId::from_uuid(model.agents[0].id.as_uuid());
    let scope = Scope::Channel(model.channels[0].id);
    let result: Result<(), String> = (|| {
        match edit {
            ModelEdit::OpenProject { repository, name, kind }
                if repository == model.projects[0].repository && kind == ProjectKind::Folder
                    && (name == "project" || name == model.projects[0].name) => { model.projects[0].name = name; }
            ModelEdit::SeedView { scope: Scope::Worktree { repository, path }, panes, closed }
                if repository == model.projects[0].repository && path == repository
                    && panes.is_empty() && closed.is_empty()
                    && model.views.iter().filter(|view| view.scope == Scope::Worktree { repository: repository.clone(), path: path.clone() })
                        .count() == 1
                    && model.views.iter().any(|view| view.scope == Scope::Worktree { repository: repository.clone(), path: path.clone() }
                        && view.panes.is_empty() && view.retired.is_empty()) => {}
            ModelEdit::SeedView { scope: supplied, panes, closed } if supplied == scope && closed.is_empty() => {
                let view = model.views.iter().find(|view| view.scope == scope).ok_or("Missing image scope")?;
                for pane in panes {
                    if pane.id != id || pane.kind != CardKind::Chat(healthy.clone())
                        || pane.scope.is_some() || pane.pending_terminal_start
                        || view.retired.contains(&id)
                        || !view.panes.iter().any(|current| current.id == id && current.kind == pane.kind) {
                        return Err("Image seed introduced, retired or unverified a source".into());
                    }
                    pane.validate()?;
                }
                record(root, "image-existing-healthy-seed-checked");
            }
            ModelEdit::ChangeView { scope: supplied, expected_revision, change: Change::Update { before, pane }, .. }
                if supplied == scope && before.id == id && pane.id == id
                    && before.kind == CardKind::Chat(healthy.clone()) && pane.kind == before.kind
                    && before.scope.is_none() && pane.scope.is_none()
                    && !before.pending_terminal_start && !pane.pending_terminal_start => {
                let view = model.views.iter_mut().find(|view| view.scope == scope).ok_or("Missing image scope")?;
                if view.retired.contains(&id) || view.revision != expected_revision {
                    return Err("Image presentation update was stale or republished a retired source".into());
                }
                if view.apply(&Change::Update { before, pane })? { view.revision += 1; }
                record(root, "image-existing-healthy-presentation-checked");
            }
            _ => return Err("Mutation outside exact image-only bootstrap".into()),
        }
        model.sequence += 1;
        Ok(())
    })();
    match result {
        Ok(()) => ServerMessage::Response { response: ServerResponse::WorkspaceModel { model: model.clone() } },
        Err(reason) => {
            record(root, &format!("UNEXPECTED-image-model-mutation {reason:?}"));
            error("Image-only fixture refused a workspace mutation")
        }
    }
}

fn client(
    mut stream: UnixStream,
    state: Arc<Mutex<FixtureState>>,
    root: PathBuf,
    healthy: Session,
    held: Session,
) {
    let peer_mode = std::env::var("HELD_FIXTURE_PEER").as_deref() == Ok("1");
    let composition_mode = std::env::var("HELD_FIXTURE_COMPOSITION").as_deref() == Ok("1");
    let image_mode = std::env::var("HELD_FIXTURE_IMAGE_ONLY").as_deref() == Ok("1");
    let writer = Arc::new(Mutex::new(stream.try_clone().expect("private fixture writer")));
    let mut admitted: Option<Session> = None;
    while let Ok(Some(request)) = terminal_ipc::read_frame::<ClientEnvelope>(&mut stream) {
        let visible_chat = match &request.request {
            ClientRequest::ChatVisibility { chat_id, visible: true }
                if admitted.as_ref().is_some_and(|session| session.id == *chat_id) => Some(*chat_id),
            _ => None,
        };
        match &request.request {
            ClientRequest::ChatAttach { .. } => record(&root, "effect-request-chat-attach"),
            ClientRequest::ChatStart { .. } => record(&root, "effect-request-chat-start"),
            ClientRequest::ChatPrepareResume { .. } => record(&root, "effect-request-chat-prepare"),
            ClientRequest::ChatRecoverAccount { .. } | ClientRequest::ChatStartSelected { .. }
            | ClientRequest::ChatReturnTerminal { .. } => record(&root, "effect-request-other-provider-mutation"),
            ClientRequest::ChatClose { .. } | ClientRequest::ChatCloseRetained { .. } => record(&root, "effect-request-chat-close"),
            _ => {}
        }
        let watching = matches!(&request.request, ClientRequest::WatchWorkspaceModel);
        let composition_create = composition_mode && matches!(&request.request,
            ClientRequest::EditWorkspaceModel { edit: ModelEdit::CreateChannel { .. } });
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
            ClientRequest::ChatStart { .. } | ClientRequest::ChatPrepareResume { .. }
            | ClientRequest::ChatRecoverAccount { .. } | ClientRequest::ChatStartSelected { .. }
            | ClientRequest::ChatReturnTerminal { .. } | ClientRequest::ChatClose { .. }
            | ClientRequest::ChatCloseRetained { .. } | ClientRequest::ChatCommitTerminal { .. }
                if peer_mode || composition_mode || image_mode => {
                record(&root, if peer_mode { "UNEXPECTED-peer-provider-mutation" } else if image_mode { "UNEXPECTED-image-provider-mutation" } else { "UNEXPECTED-composition-provider-mutation" });
                error(if peer_mode { "Peer removal grants no provider mutation authority" } else { "Composition grants no provider mutation authority" })
            }
            ClientRequest::WatchWorkspaceModel | ClientRequest::ReadWorkspaceModel => {
                let mut fixture = state.lock().unwrap();
                if image_mode && root.join("repair-evidence").exists() {
                    record(&root, "UNEXPECTED-image-repair-authority");
                    return;
                }
                if image_mode && explicit_read {
                    // Give each real guarded recheck a distinct, observable
                    // full-model/save barrier without changing pane authority.
                    fixture.model.sequence += 1;
                    fixture.model.channels[0].title = format!("Image custody check {}", fixture.model.sequence);
                    fixture.model.channels[0].title_source = ChannelTitleSource::Manual;
                    record(&root, &format!("image-full-read-model sequence={} channel={:?} views={:?} issuers={:?}",
                        fixture.model.sequence, fixture.model.channels[0], fixture.model.views, fixture.model.agents));
                    let json = match serde_json::to_string(&fixture.model) {
                        Ok(json) => json,
                        Err(reason) => {
                            record(&root, &format!("UNEXPECTED-image-model-json {reason:?}"));
                            return;
                        }
                    };
                    record(&root, &format!("image-full-read-json sequence={} json={json}", fixture.model.sequence));
                }
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
            ClientRequest::EditWorkspaceModel { edit } if peer_mode => {
                peer_edit(&root, &mut state.lock().unwrap().model, edit, &healthy)
            }
            ClientRequest::EditWorkspaceModel { edit } if composition_mode => {
                composition_edit(&root, &mut state.lock().unwrap(), edit, &healthy)
            }
            ClientRequest::EditWorkspaceModel { edit } if image_mode => {
                image_edit(&root, &mut state.lock().unwrap().model, edit, &healthy)
            }
            ClientRequest::EditWorkspaceModel { edit } => {
                let mut fixture = state.lock().unwrap();
                let repaired = fixture.repaired;
                let model = &mut fixture.model;
                let edit_summary = match &edit {
                    ModelEdit::SeedView { scope, panes, closed } => format!(
                        "SeedView scope={scope:?} panes={:?} closed={closed:?}",
                        panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
                    ),
                    ModelEdit::ChangeView { scope, change, .. } => {
                        let affected = match change {
                            Change::Insert { pane, .. }
                            | Change::InsertIfEmpty { pane }
                            | Change::Update { pane, .. } => pane.id,
                            Change::Remove { id }
                            | Change::Move { id, .. }
                            | Change::Transfer { id, .. } => *id,
                        };
                        format!("ChangeView scope={scope:?} affected={affected:?}")
                    }
                    ModelEdit::OpenProject { repository, .. } => format!("OpenProject repository={repository:?}"),
                    #[cfg(held_product_after)]
                    ModelEdit::SuggestConversationTitle { id, card, .. } => format!("SuggestConversationTitle channel={id:?} card={card:?}"),
                    other => format!("Other {:?}", std::mem::discriminant(other)),
                };
                let result: Result<(), String> = (|| {
                    match edit {
                        ModelEdit::OpenProject { repository, name, kind }
                            if repository == model.projects[0].repository
                                && kind == ProjectKind::Folder
                                && (name == "project" || name == model.projects[0].name) =>
                        {
                            model.projects[0].name = name;
                            record(&root, "private-project-open-accepted");
                        }
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
                                if pane.id == bad_id && !repaired {
                                    record(&root, "UNEXPECTED-bad-shared-insert");
                                    return Err("Unverified synthetic pane was published".into());
                                }
                                pane.validate()?;
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
                        other => {
                            record(&root, &format!("UNEXPECTED-unsupported-model-edit-{:?}", std::mem::discriminant(&other)));
                            return Err("Mutation outside this native fixture".into());
                        }
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
                    Err(reason) => {
                        record(&root, &format!("UNEXPECTED-model-edit-refused {edit_summary} reason={reason:?}"));
                        error("Synthetic fixture refused an inadmissible workspace edit")
                    }
                }
            }
            ClientRequest::WatchTerminals => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            ClientRequest::List => ServerMessage::Response {
                response: ServerResponse::Listed { terminals: vec![] },
            },
            ClientRequest::ChatList => ServerMessage::Response {
                response: ServerResponse::ChatListed {
                    sessions: if peer_mode { vec![healthy.clone()] } else { vec![healthy.clone(), held.clone()] },
                },
            },
            ClientRequest::ChatAttach { session } => {
                let repaired = state.lock().unwrap().repaired;
                let retired = if peer_mode || image_mode {
                    let fixture = state.lock().unwrap();
                    fixture.model.views[0].retired.contains(&CardId::from_uuid(fixture.model.agents[0].id.as_uuid()))
                } else { false };
                if retired {
                    record(&root, "UNEXPECTED-peer-reattach-after-removal");
                    error("A retired pane cannot attach again")
                } else if session == healthy || (repaired && session == held) {
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
                    admitted = Some(session.clone());
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
            ClientRequest::ChatRead { chat_id, after_generation } => {
                if let Some(session) = admitted.as_ref().filter(|session| session.id == chat_id) {
                    let snapshot = snapshot(session);
                    ServerMessage::Response { response: ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: after_generation.is_none_or(|generation| generation < snapshot.generation).then_some(snapshot),
                    }}
                } else {
                    record(&root, "UNEXPECTED-unadmitted-chat-read");
                    error("Read requires an exact previously admitted synthetic actor")
                }
            }
            ClientRequest::ChatCommand { chat_id, command: cagents::chat::Command::ReadTextCopy { source, offset } }
                if composition_mode && chat_id == healthy.id
                    && admitted.as_ref() == Some(&healthy)
                    && source == composition_copy_source() && offset == 0 => {
                ServerMessage::Response { response: ServerResponse::ChatTextCopy {
                    chat_id, page: cagents::chat::TextCopyPage {
                        source, offset, text: COMPOSITION_CONTEXT.into(), done: true,
                    },
                }}
            }
            ClientRequest::ChatVisibility { chat_id, .. } => {
                if admitted.as_ref().is_some_and(|session| session.id == chat_id) {
                    ServerMessage::Response { response: ServerResponse::Accepted }
                } else {
                    record(&root, "UNEXPECTED-unadmitted-chat-visibility");
                    error("Visibility requires an exact previously admitted synthetic actor")
                }
            }
            ClientRequest::ChatAcknowledge { .. }
            | ClientRequest::ChatDetach { .. } => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            ClientRequest::Automation {
                authority,
                operation: cagents::automations::Request::PendingProposals { repository, after: None, limit: 1 },
            } if authority == Authority::default() && repository == healthy.launch.recipe.working_directory => {
                record(&root, "readonly-private-project-pending-proposals-replied");
                ServerMessage::Response { response: ServerResponse::Automation(cagents::automations::Response::Proposals(vec![])) }
            }
            ClientRequest::NotificationPresence { .. }
            | ClientRequest::NotificationPreferencesChanged { .. }
            | ClientRequest::AcknowledgeNotification { .. } => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            ClientRequest::WatchNotifications { .. } | ClientRequest::ReadNotifications => ServerMessage::Response {
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
            ClientRequest::ChatStart { mut session, source, terminate_source, .. }
                if std::env::var("HELD_FIXTURE_ORDINARY").as_deref() == Ok("1")
                    && session.launch == ordinary_source(&healthy).launch
                    && session.id != ordinary_source(&healthy).id
                    && session.id != healthy.id && session.id != held.id
                    && session.proof.is_none()
                    && session.terminal.is_none()
                    && session.owned_codex_listener.is_none()
                    && session.native_continuity == ordinary_source(&healthy).native_continuity
                    && source.is_none() && !terminate_source =>
            {
                let mut fixture = state.lock().unwrap();
                if fixture.ordinary_started {
                    record(&root, "UNEXPECTED-duplicate-ordinary-start");
                    return;
                }
                fixture.ordinary_started = true;
                // One exact synthetic protocol response, with no process or
                // provider launch. Every other provider effect remains fatal.
                session.proof = Some(proof());
                admitted = Some(session.clone());
                fs::write(root.join("ordinary-recovered-chat-id"), session.id.to_string()).unwrap();
                record(&root, "ordinary-exact-history-start-accepted");
                ServerMessage::Response { response: ServerResponse::ChatAttached {
                    snapshot: snapshot(&session), session,
                }}
            }
            ClientRequest::ChatCommitTerminal { terminal_id }
                if terminal_id == held.id && {
                    let fixture = state.lock().unwrap();
                    fixture.model.agents.get(1).is_some_and(|agent|
                        agent.closed && agent.spec.surface == Surface::Terminal
                        && matches!(agent.session.as_ref(), Some(HostedSession::Terminal(session)) if session.id == terminal_id))
                } =>
            {
                // Production accepts this durability acknowledgement even when
                // no handoff receipt exists. It does not admit or act on an actor.
                record(&root, "terminal-handoff-ack-idempotent-no-receipt");
                ServerMessage::Response { response: ServerResponse::Accepted }
            }
            ClientRequest::ChatStart { .. }
            | ClientRequest::ChatRecoverAccount { .. }
            | ClientRequest::ChatStartSelected { .. }
            | ClientRequest::ChatReturnTerminal { .. }
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
            other => {
                let kind = match &other {
                    ClientRequest::Automation { operation, .. } => format!("Automation-{:?}", std::mem::discriminant(operation)),
                    _ => format!("{:?}", std::mem::discriminant(&other)),
                };
                record(&root, &format!("UNEXPECTED-unsupported-request-{kind}"));
                error("Request outside this synthetic native fixture")
            }
        };
        let full_model_delivered = matches!(&response, ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { .. },
        });
        let composition_copy_delivered = composition_mode && matches!(&response,
            ServerMessage::Response { response: ServerResponse::ChatTextCopy { .. } });
        let peer_model_delivered = peer_mode && explicit_read && matches!(&response,
            ServerMessage::Response { response: ServerResponse::WorkspaceModel { model } }
                if model.channels[0].title == "Peer removal received"
                    && model.views[0].retired.contains(&CardId::from_uuid(model.agents[0].id.as_uuid())));
        let image_read_delivered = if image_mode && explicit_read {
            match &response {
                ServerMessage::Response { response: ServerResponse::WorkspaceModel { model } } => Some(format!(
                    "image-read-delivered sequence={} revision={} retired={}", model.sequence, model.views[0].revision,
                    model.views[0].retired.contains(&CardId::from_uuid(model.agents[0].id.as_uuid())),
                )),
                _ => None,
            }
        } else { None };
        let healthy_long_snapshot = match &response {
            ServerMessage::Response {
                response: ServerResponse::ChatAttached { session, snapshot },
            } if session == &healthy
                && std::env::var("HELD_FIXTURE_PAYLOAD").as_deref() == Ok("long") =>
            {
                assert!(snapshot.ready);
                assert_eq!(snapshot.session_id, healthy.launch.session_id);
                assert_eq!(snapshot.items.len(), 200);
                assert_eq!(snapshot.items.first().unwrap().id, "synthetic-history-0");
                assert_eq!(snapshot.items.last().unwrap().id, "synthetic-history-199");
                Some(format!(
                    "healthy-long-snapshot-delivered items={} first={} last={}",
                    snapshot.items.len(), snapshot.items.first().unwrap().id,
                    snapshot.items.last().unwrap().id,
                ))
            }
            _ => None,
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
        if full_model_delivered { record(&root, "full-model-delivered"); }
        if composition_create && full_model_delivered { record(&root, "composition-ranked-response-delivered"); }
        if composition_copy_delivered {
            record(&root, "composition-context-copy-delivered");
        }
        if peer_model_delivered { record(&root, "peer-removal-model-delivered"); }
        if let Some(receipt) = image_read_delivered { record(&root, &receipt); }
        if let Some(receipt) = healthy_long_snapshot { record(&root, &receipt); }
        if let Some(chat_id) = visible_chat {
            if terminal_ipc::write_frame(&mut *writer.lock().unwrap(), &ServerEnvelope {
                version: terminal_ipc::PROTOCOL_VERSION, request_id: None,
                proof: proof(), message: ServerMessage::ChatChanged { chat_id },
            }).is_err() { break; }
        }
        if explicit_read { record(&root, "explicit-read-replied"); }
        let start_image = if watching && image_mode {
            let mut fixture = state.lock().unwrap();
            let first = !fixture.image_removal_started;
            fixture.image_removal_started = true;
            first
        } else { false };
        if start_image {
            let writer = writer.clone();
            let state = state.clone();
            let root = root.clone();
            let healthy = healthy.clone();
            let held = held.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                while !root.join("image-remove").exists() {
                    if std::time::Instant::now() >= deadline { record(&root, "UNEXPECTED-image-trigger-deadline"); return; }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let image = match managed_image(&root) {
                    Ok(image) => image,
                    Err(reason) => { record(&root, &format!("UNEXPECTED-image-pre-remove-asset {reason:?}")); return; }
                };
                record(&root, &format!("image-pre-remove-asset-validated id={} sha256={}",
                    image.reference.as_ref().unwrap().id, image.reference.as_ref().unwrap().sha256));
                let mut writer = writer.lock().unwrap();
                let mut fixture = state.lock().unwrap();
                let id = CardId::from_uuid(fixture.model.agents[0].id.as_uuid());
                let bad_id = CardId::from_uuid(fixture.model.agents[1].id.as_uuid());
                let mut issuer = healthy.clone();
                issuer.launch.session_id = None;
                let mut stale = held.clone();
                stale.proof.as_mut().unwrap().host_incarnation = "synthetic-stale-incarnation".into();
                if fixture.model.agents.len() != 2 || fixture.model.agents[0].closed
                    || fixture.model.agents[0].session != Some(HostedSession::Chat(issuer))
                    || fixture.model.agents[1].closed
                    || fixture.model.agents[1].session != Some(HostedSession::Chat(stale)) {
                    record(&root, "UNEXPECTED-image-issuer-or-held-source-changed"); return;
                }
                let view = &mut fixture.model.views[0];
                if view.panes.len() != 2 || view.retired.contains(&id)
                    || !view.panes.iter().any(|pane| pane.id == id && pane.kind == CardKind::Chat(healthy.clone())
                        && pane.scope.is_none() && !pane.pending_terminal_start)
                    || !view.panes.iter().any(|pane| pane.id == bad_id && pane.kind == CardKind::Chat(held.clone())
                        && pane.scope.is_none() && !pane.pending_terminal_start) {
                    record(&root, "UNEXPECTED-image-canonical-source-changed"); return;
                }
                let expected_revision = view.revision;
                let scope = view.scope.clone();
                record(&root, &format!("image-pre-remove-canonical-witness scope={scope:?} revision={expected_revision} panes={:?}", view.panes));
                view.apply(&Change::Remove { id }).expect("exact image-only peer removal");
                view.revision += 1;
                fixture.model.sequence += 1;
                fixture.model.channels[0].title = "Image retirement received".into();
                fixture.model.channels[0].title_source = ChannelTitleSource::Manual;
                let message = ServerEnvelope {
                    version: terminal_ipc::PROTOCOL_VERSION, request_id: None, proof: proof(),
                    message: ServerMessage::WorkspaceModelEdited {
                        sequence: fixture.model.sequence,
                        edit: ModelEdit::ChangeView { scope, expected_revision, operation: OperationId::new().as_uuid(), change: Change::Remove { id } },
                    },
                };
                drop(fixture);
                if terminal_ipc::write_frame(&mut *writer, &message).is_err() {
                    record(&root, "UNEXPECTED-image-event-write-failed"); return;
                }
                record(&root, &format!("image-removal-event-delivered expected_revision={expected_revision}"));
            });
        }
        let start_peer = if watching && peer_mode {
            let mut fixture = state.lock().unwrap();
            let first = !fixture.peer_removal_started;
            fixture.peer_removal_started = true;
            first
        } else { false };
        if start_peer {
            let writer = writer.clone();
            let state = state.clone();
            let root = root.clone();
            let healthy = healthy.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !root.join("peer-remove").exists() {
                    if std::time::Instant::now() >= deadline { record(&root, "UNEXPECTED-peer-trigger-deadline"); return; }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let mut writer = writer.lock().unwrap();
                let mut fixture = state.lock().unwrap();
                let agent = fixture.model.agents[0].clone();
                let mut expected_issuer = healthy.clone();
                expected_issuer.launch.session_id = None;
                if agent.closed || agent.session != Some(HostedSession::Chat(expected_issuer)) {
                    record(&root, "UNEXPECTED-peer-issuer-changed"); return;
                }
                let id = CardId::from_uuid(agent.id.as_uuid());
                let view = &mut fixture.model.views[0];
                if view.panes.len() != 1 || view.panes[0].id != id || view.panes[0].kind != CardKind::Chat(healthy) {
                    record(&root, "UNEXPECTED-peer-canonical-source-changed"); return;
                }
                let expected_revision = view.revision;
                let scope = view.scope.clone();
                view.apply(&Change::Remove { id }).expect("exact private peer removal");
                view.revision += 1;
                fixture.model.sequence += 1;
                fixture.model.channels[0].title = "Peer removal received".into();
                fixture.model.channels[0].title_source = ChannelTitleSource::Manual;
                let message = ServerEnvelope {
                    version: terminal_ipc::PROTOCOL_VERSION, request_id: None, proof: proof(),
                    message: ServerMessage::WorkspaceModelEdited {
                        sequence: fixture.model.sequence,
                        edit: ModelEdit::ChangeView { scope, expected_revision, operation: OperationId::new().as_uuid(), change: Change::Remove { id } },
                    },
                };
                drop(fixture);
                if terminal_ipc::write_frame(&mut *writer, &message).is_err() {
                    record(&root, "UNEXPECTED-peer-event-write-failed"); return;
                }
                record(&root, "peer-removal-event-delivered");
            });
        }
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
fn inspection_database(root: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if std::env::args().nth(5).as_deref() != Some("--inspection-database")
        || std::env::args().nth(7).is_some() {
        return Err("Typed inspection requires its explicit private snapshot".into());
    }
    let database = PathBuf::from(std::env::args().nth(6).ok_or("Inspection snapshot missing")?);
    if ![root.join("inspection/typed.sqlite3"), root.join("inspection/refusal.sqlite3")].contains(&database)
        || fs::canonicalize(root)? != root || fs::canonicalize(&database)? != database {
        return Err("Refusing an inspection path outside the exact private snapshots".into());
    }
    let root_metadata = fs::symlink_metadata(root)?;
    let directory = fs::symlink_metadata(root.join("inspection"))?;
    let snapshot = fs::symlink_metadata(&database)?;
    let original_path = SessionRuntime::default_database_path()?;
    if PathBuf::from(fs::read_to_string(root.join("database-path"))?) != original_path
        || !original_path.starts_with(root) || fs::canonicalize(&original_path)? != original_path {
        return Err("Original fixture database binding changed".into());
    }
    let original = fs::symlink_metadata(&original_path)?;
    if !root_metadata.is_dir() || root_metadata.mode() & 0o7777 != 0o700
        || !directory.is_dir() || directory.file_type().is_symlink()
        || directory.mode() & 0o7777 != 0o700 || directory.uid() != root_metadata.uid()
        || !snapshot.is_file() || snapshot.file_type().is_symlink()
        || snapshot.mode() & 0o7777 != 0o600 || snapshot.uid() != root_metadata.uid() || snapshot.nlink() != 1
        || !original.is_file() || original.file_type().is_symlink()
        || original.mode() & 0o7777 != 0o600 || original.uid() != root_metadata.uid() || original.nlink() != 1
        || database == original_path || (snapshot.dev(), snapshot.ino()) == (original.dev(), original.ino()) {
        return Err("Inspection requires distinct private snapshot and original files".into());
    }
    println!("inspection-snapshot-path-verified snapshot={} original={}", database.display(), original_path.display());
    Ok(database)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if Authority::from_environment() != Authority::default() {
        return Err(
            "Genuine User launch required; inherited caller authority must remain unchanged".into(),
        );
    }
    let invocation = std::env::args().skip(1).collect::<Vec<_>>();
    let serving = invocation.len() == 2;
    let inspecting = invocation.len() == 6 && invocation[2] == "--inspect"
        && matches!(invocation[3].as_str(), "open" | "closed")
        && invocation[4] == "--inspection-database";
    if !serving && !inspecting {
        return Err("Use root/mode or root/mode/--inspect/open|closed/--inspection-database/path".into());
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
    if !matches!(mode.as_str(), "stale" | "closed" | "closed-history" | "peer-removal" | "composition" | "image-only") {
        return Err("Unknown native fixture mode".into());
    }
    let peer_mode = mode == "peer-removal";
    let composition_mode = mode == "composition";
    let image_mode = mode == "image-only";
    if image_mode != (std::env::var("HELD_FIXTURE_IMAGE_ONLY").as_deref() == Ok("1")) {
        return Err("Image-only mode and private environment disagree".into());
    }
    if composition_mode != (std::env::var("HELD_FIXTURE_COMPOSITION").as_deref() == Ok("1")) {
        return Err("Composition mode and private environment disagree".into());
    }
    if peer_mode != (std::env::var("HELD_FIXTURE_PEER").as_deref() == Ok("1")) {
        return Err("Peer-removal mode and private environment disagree".into());
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
    if peer_mode {
        cards[1].kind = CardKind::Settings;
        cards[1].title = "Active settings".into();
    }
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
    // A saved image carries immutable reference metadata. Legacy path-only
    // provider files are not persisted draft images. This synthetic reference
    // is deliberately unavailable in the private asset registry: its path is
    // a location hint, never authority to resolve or publish an image.
    const IMAGE_BYTES: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 80, 153, 22, 253, 31, 0, 4, 12, 2, 21, 73, 175, 151, 59, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130];
    let attachment = cagents::chat::Attachment {
        path: root.parent().ok_or("Fixture parent missing")?.join("synthetic-saved-attachment.png"),
        reference: Some(cagents::chat::ImageReference {
            id: "00000000-0000-4000-8000-000000001923".parse()?,
            number: 1,
            device: "00000000-0000-4000-8000-000000002923".parse()?,
            name: "Synthetic saved attachment.png".into(),
            sha256: "6aa8d4c0d15e7b9c7a02ddb0699e41448ef178b2f18e83597b673c559bef04cb".into(),
            extension: "png".into(),
        }),
    };
    attachment.reference.as_ref().unwrap().validate()?;
    if std::env::args().nth(3).as_deref() == Some("--inspect") {
        let database = inspection_database(&root)?;
        let store = StateStore::open(&database, cagents::session::current_boot_id()?)?;
        let workspace_id: WorkspaceId = fs::read_to_string(root.join("workspace-id"))?.trim().parse()?;
        let saved = store.load_workspace(workspace_id, None)?.ok_or("Saved workspace missing")?;
        #[cfg(held_product_after)]
        if image_mode {
            let image = managed_image(&root)?;
            let owner = saved.channels.iter().find(|owner| owner.id == channel).ok_or("Image channel missing")?;
            let source = saved.cards.iter().find(|card| card.id == healthy_id).ok_or("Image source history missing")?;
            let bad = saved.cards.iter().find(|card| card.id == bad_id).ok_or("Image rejected sibling missing")?;
            if source.kind != CardKind::Chat(healthy.clone()) || bad.kind != CardKind::Chat(held.clone())
                || !bad.existence.is_open() || saved.active_channel != Some(channel)
                || saved.cards.len() != 2 || saved.channels.len() != 1
                || saved.quarantined_panes != HashMap::from([(bad_id, CardKind::Chat(held.clone()))])
                || saved.pending_chat_admissions.get(&bad_id) != Some(&held)
                || saved.pending_chat_admissions.len() != 1
                || saved.pending_chat_admissions.contains_key(&healthy_id)
                || !saved.chat_drafts.is_empty()
                || saved.chat_draft_attachments.contains_key(&bad_id)
                || saved.local_only_cards.contains(&bad_id)
                || owner.project_id != project || !owner.shared {
                return Err("Image-only source, proof, empty text, sibling denial, scope or pending custody changed".into());
            }
            #[cfg(held_image_before)]
            {
                if !matches!(&source.existence, Existence::Closed { .. }) || !saved.local_only_cards.is_empty()
                    || owner.cards != vec![bad_id] || !saved.chat_draft_attachments.is_empty() {
                    return Err("BeforeImage must positively show exact retired image association loss".into());
                }
                println!("typed-image-before-existing-association-loss: exact-source-closed no-local-custody image-association-absent valid-asset-bytes-retained sibling-only-quarantine");
            }
            #[cfg(not(held_image_before))]
            {
                if !source.existence.is_open() || !saved.local_only_cards.contains(&healthy_id)
                    || saved.local_only_cards.len() != 1 || saved.chat_draft_attachments.len() != 1
                    || owner.cards != vec![healthy_id, bad_id]
                    || saved.chat_draft_attachments.get(&healthy_id) != Some(&vec![image]) {
                    return Err("After image-only retirement lost exact open local source or managed image".into());
                }
                println!("typed-image-after-open-local-source-exact-managed-image-and-sibling-only-quarantine-preserved");
            }
            return Ok(());
        }
        #[cfg(held_composition_main_before)]
        if composition_mode {
            let created: ChannelId = fs::read_to_string(root.join("composition-channel-id"))?.parse()?;
            let owner = saved.channels.iter().find(|entry| entry.id == channel).ok_or("Original main comparison channel missing")?;
            let new = saved.channels.iter().find(|entry| entry.id == created).ok_or("New main comparison channel missing")?;
            let healthy_card = saved.cards.iter().find(|card| card.id == healthy_id).ok_or("Main comparison healthy source missing")?;
            let bad = saved.cards.iter().find(|card| card.id == bad_id).ok_or("Main comparison held source missing")?;
            if new.project_id != project || !new.shared || new.ordinal <= owner.ordinal
                || saved.active_channel != Some(channel) || saved.focused != Some(healthy_id)
                || !owner.cards.contains(&healthy_id) || !owner.cards.contains(&bad_id)
                || new.cards.contains(&healthy_id) || new.cards.contains(&bad_id)
                || healthy_card.kind != CardKind::Chat(healthy.clone()) || !healthy_card.existence.is_open()
                || bad.kind != cards[1].kind || !bad.existence.is_open()
                || saved.pending_chat_admissions.get(&healthy_id) != Some(&healthy)
                || saved.pending_chat_admissions.get(&bad_id) != Some(&held)
                || saved.chat_drafts.get(&healthy_id).is_some_and(|text| !text.is_empty())
                || saved.chat_draft_attachments.get(&healthy_id).is_some_and(|images| !images.is_empty())
                || saved.chat_drafts.get(&bad_id).map(String::as_str) != Some(draft.as_str())
                || saved.chat_draft_attachments.get(&bad_id) != Some(&vec![attachment.clone()]) {
                return Err("Main comparison changed exact blocked sources, saved payload, pending intent, rank or navigation".into());
            }
            if fs::read(&attachment.path)? != IMAGE_BYTES
                || cagents::chat::attachment_assets::validate(&attachment).is_ok() {
                return Err("Main comparison changed saved image bytes or acquired image authority".into());
            }
            println!("typed-main-before-blocked-sources-both-pending-intents-saved-payload-rank-and-return-verified");
            return Ok(());
        }
        if composition_mode {
            let created: ChannelId = fs::read_to_string(root.join("composition-channel-id"))?.parse()?;
            let owner = saved.channels.iter().find(|entry| entry.id == channel).ok_or("Original composition channel missing")?;
            let new = saved.channels.iter().find(|entry| entry.id == created).ok_or("New composition channel missing")?;
            let healthy_card = saved.cards.iter().find(|card| card.id == healthy_id).ok_or("Composition healthy source missing")?;
            // This saved shared-channel fixture creates both Awaiting sources
            // before its first model reply. Denial retains the held source;
            // successful healthy admission must clear its own marker.
            if new.project_id != project || !new.shared || new.ordinal <= owner.ordinal
                || saved.active_channel != Some(channel) || saved.focused != Some(healthy_id)
                || !owner.cards.contains(&healthy_id) || !owner.cards.contains(&bad_id)
                || new.cards.contains(&healthy_id) || new.cards.contains(&bad_id)
                || saved.pending_chat_admissions.contains_key(&healthy_id)
                || saved.pending_chat_admissions.get(&bad_id) != Some(&held)
                || saved.chat_draft_attachments.get(&healthy_id).is_some_and(|images| !images.is_empty())
                || healthy_card.kind != CardKind::Chat(healthy.clone()) || !healthy_card.existence.is_open() {
                return Err("Composition rank, scope, membership, focus or healthy source changed".into());
            }
            #[cfg(held_product_after)]
            if !saved.quarantined_panes.contains_key(&bad_id) || saved.quarantined_panes.contains_key(&healthy_id) {
                return Err("Composition changed exact held-pane quarantine".into());
            }
            println!("typed-composition-channel-rank-scope-focus-and-healthy-source-verified");
        }
        if peer_mode {
            let channel_state = saved.channels.iter().find(|owner| owner.id == channel).ok_or("Peer channel missing")?;
            let removed = saved.cards.iter().find(|card| card.id == healthy_id).ok_or("Peer retired history missing")?;
            let settings = saved.cards.iter().find(|card| card.id == bad_id).ok_or("Peer Settings survivor missing")?;
            if removed.existence.is_open() || removed.kind != CardKind::Chat(healthy.clone())
                || channel_state.cards.contains(&healthy_id) || channel_state.cards != vec![bad_id]
                || !settings.existence.is_open() || settings.kind != CardKind::Settings
                || saved.active_channel != Some(channel) || saved.focused != Some(bad_id)
                || channel_state.focused != Some(bad_id)
                || saved.chat_drafts.get(&healthy_id).is_some_and(|draft| !draft.is_empty())
                || saved.chat_draft_attachments.get(&healthy_id).is_some_and(|images| !images.is_empty()) {
                return Err("Peer removal changed source, membership, survivor or saved focus".into());
            }
            #[cfg(held_product_after)]
            if saved.quarantined_panes.contains_key(&healthy_id) {
                return Err("Verified peer retirement manufactured quarantine".into());
            }
            println!("typed-peer-retirement-source-history-membership-and-focus-verified");
            return Ok(());
        }
        if std::env::var("HELD_FIXTURE_ORDINARY").as_deref() == Ok("1") {
            let id = ordinary_card();
            let original = ordinary_source(&healthy);
            let card = saved.cards.iter().find(|card| card.id == id).ok_or("Ordinary closed copy missing")?;
            if card.existence.is_open() || card.kind != CardKind::Chat(original.clone())
                || !saved.local_only_cards.contains(&id)
                || saved.pending_chat_admissions.contains_key(&id)
            { return Err("Ordinary closed source, existence, local custody or admission changed".into()); }
            #[cfg(held_product_after)]
            {
                if saved.chat_drafts.get(&id).map(String::as_str) != Some(ORDINARY_DRAFT)
                    || saved.chat_draft_attachments.get(&id) != Some(&vec![attachment.clone()])
                { return Err("Ordinary closed saved draft or attachments changed".into()); }
                if saved.quarantined_panes.contains_key(&id) {
                    return Err("Ordinary closed copy acquired false denial evidence".into());
                }
                println!("ordinary-closed-source-custody-and-full-saved-payload-preserved");
            }
            #[cfg(not(held_product_after))]
            {
                // c727's persistent_workspace filters saved payloads to open
                // cards. Record this existing loss as the Before behavior;
                // all identity/custody checks above still apply to both arms.
                if saved.chat_drafts.contains_key(&id)
                    || saved.chat_draft_attachments.contains_key(&id)
                { return Err("Baseline closed-payload observation changed".into()); }
                println!("baseline-existing-closed-payload-loss: source-and-custody-preserved draft-absent attachments-absent");
            }
            let recovered = fs::read_to_string(root.join("ordinary-recovered-chat-id"))?;
            let recovered = saved.cards.iter().find(|card| matches!(&card.kind, CardKind::Chat(session) if session.id.to_string() == recovered)).ok_or("Recovered ordinary conversation missing")?;
            if !recovered.existence.is_open()
                || saved.chat_drafts.get(&recovered.id).map(String::as_str) != Some("Fresh work after ordinary History recovery.")
                || saved.chat_draft_attachments.get(&recovered.id).is_some_and(|attachments| !attachments.is_empty())
            { return Err("Recovered ordinary conversation did not keep fresh work".into()); }
        }
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
        let expected = if payload == "empty" { vec![] } else { vec![attachment.clone()] };
        if saved.chat_draft_attachments.get(&bad_id).cloned().unwrap_or_default() != expected { return Err("Saved attachments changed".into()); }
        if root.join("healthy-draft-expected").exists() && saved.chat_drafts.get(&healthy_id).map(String::as_str) != Some(fs::read_to_string(root.join("healthy-draft-expected"))?.as_str()) { return Err("Healthy sibling edited draft did not persist".into()); }
        if fs::read(&attachment.path)? != IMAGE_BYTES { return Err("Saved attachment bytes changed".into()); }
        if cagents::chat::attachment_assets::validate(&attachment).is_ok() { return Err("Unavailable image reference unexpectedly gained asset authority".into()); }
        println!("typed-saved-source-draft-attachments-bytes-and-existence-verified");
        return Ok(());
    }
    fs::write(root.join("expected-draft.txt"), &draft)?;
    if image_mode {
        if payload != "empty" || terminal_surface || peer_mode || composition_mode
            || std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1")
            || std::env::var("HELD_FIXTURE_ORDINARY").as_deref() == Ok("1") {
            return Err("Image-only requires its separate empty mixed Chat fixture".into());
        }
        fs::write(root.join("image-only-offered.png"), MANAGED_PNG)?;
    }
    if composition_mode {
        if payload != "short" || terminal_surface
            || std::env::var("HELD_FIXTURE_HIDDEN").as_deref() == Ok("1")
            || std::env::var("HELD_FIXTURE_ORDINARY").as_deref() == Ok("1") {
            return Err("Composition requires its separate short Chat fixture".into());
        }
        fs::write(root.join("composition-context.txt"), COMPOSITION_CONTEXT)?;
    }
    fs::write(&attachment.path, IMAGE_BYTES)?;
    fs::write(root.join("expected-attachment.txt"), attachment.path.to_string_lossy().as_bytes())?;
    if cagents::chat::attachment_assets::validate(&attachment).is_ok() { return Err("Synthetic unavailable image fixture acquired asset authority".into()); }
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
        chat_draft_attachments: if payload == "empty" { HashMap::new() } else { HashMap::from([(bad_id, vec![attachment.clone()])]) },
        ..Default::default()
    };
    if image_mode { workspace.chat_drafts.clear(); }
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
    let ordinary_mode = std::env::var("HELD_FIXTURE_ORDINARY").as_deref() == Ok("1");
    if ordinary_mode {
        let source = ordinary_source(&healthy);
        let id = ordinary_card();
        workspace.cards.push(CardState {
            id, generation: 0, title: "Ordinary retained conversation".into(), width_fraction: 1.0,
            kind: CardKind::Chat(source.clone()),
            existence: Existence::Closed { tombstone: Tombstone {
                operation_id: None, closed_at_unix_ms: 0, reason: Some("ordinary retained copy closed by user".into()),
            }}, exposure: Exposure::Offscreen, residency: Residency::Cold,
        });
        workspace.local_only_cards.insert(id);
        workspace.chat_drafts.insert(id, ORDINARY_DRAFT.into());
        workspace.chat_draft_attachments.insert(id, vec![attachment.clone()]);
    }
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
        .filter(|card| !peer_mode || !card.kind.is_utility())
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
            closed: id == bad_id && mode != "stale" && !composition_mode && !image_mode,
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
    if peer_mode {
        agents.truncate(1);
        let Some(HostedSession::Chat(issued)) = &mut agents[0].session else { unreachable!() };
        issued.launch.session_id = None;
    }
    if image_mode {
        let Some(HostedSession::Chat(issued)) = &mut agents[0].session else { unreachable!() };
        issued.launch.session_id = None;
    }
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
        ordinary_started: false,
        peer_removal_started: false,
        composition_channel: None,
        image_removal_started: false,
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
                title: "Saved recovery model received".into(),
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
    {
        let mut fixture = state.lock().unwrap();
        let repository = fixture.model.projects[0].repository.clone();
        fixture.model.views.push(View::empty(Scope::Worktree { repository: repository.clone(), path: repository }));
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
