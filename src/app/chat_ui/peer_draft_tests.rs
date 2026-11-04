//! A window already showing a shared draft when another window sends its first
//! message has to end up attached to that conversation, not left on a draft
//! that offers to start it again.
use super::*;
use crate::workspace_model::views::{Pane, Scope, View};

struct Peer {
    app: Cagents,
    _directory: tempfile::TempDir,
    channel: crate::model::ChannelId,
    card: CardId,
    draft: chat::Session,
}

fn draft_session(directory: &std::path::Path) -> chat::Session {
    chat::Session {
        id: TerminalId::new(),
        proof: None,
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: vec![],
                working_directory: directory.to_owned(),
                unset_environment: Default::default(),
                environment: Default::default(),
            },
        },
    }
}

fn started(session: &chat::Session) -> chat::Session {
    let mut started = session.clone();
    started.launch.session_id = Some("provider-conversation".into());
    assert!(!started.is_unstarted());
    started
}

fn project(
    app: &mut Cagents,
    channel: crate::model::ChannelId,
    card: CardId,
    kind: CardKind,
) -> Task<Message> {
    let view = app
        .workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(channel))
        .unwrap();
    view.revision += 1;
    view.panes = vec![Pane {
        id: card,
        title: "Shared".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: None,
        kind,
    }];
    app.reconcile_shared_views()
}

fn draft_runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) if runtime.is_draft() => runtime,
        _ => panic!("expected a draft runtime"),
    }
}

/// A window showing a draft that another window published.
fn peer() -> Peer {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1280.0);
    let directory = tempfile::tempdir().unwrap();
    let channel = crate::model::ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: channel,
            project_id: crate::model::ProjectId::new(),
            title: String::new(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![],
            focused: None,
        });
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.sequence = 1;
    app.workspace_model.views.push(View {
        revision: 1,
        ..View::empty(channel)
    });
    app.acknowledge_view_edit(Scope::Channel(channel), None);
    let card = CardId::new();
    let draft = draft_session(directory.path());
    drop(project(
        &mut app,
        channel,
        card,
        CardKind::Chat(draft.clone()),
    ));
    draft_runtime(&app, card);
    Peer {
        app,
        _directory: directory,
        channel,
        card,
        draft,
    }
}

fn write(app: &mut Cagents, card: CardId, text: &str) {
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("expected a chat runtime");
    };
    runtime.replace_draft(text_editor::Content::with_text(text));
}

#[test]
fn a_draft_attaches_when_another_window_starts_its_conversation() {
    let mut peer = peer();
    write(&mut peer.app, peer.card, "not sent yet");
    let accepted = started(&peer.draft);

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));

    assert!(
        peer.app.chat_start_matches(peer.card, &accepted),
        "the window must connect to the running conversation"
    );
    assert!(matches!(
        peer.app.cards.get(&peer.card),
        Some(CardRuntime::Loading)
    ));
    assert!(peer.app.chat_ui.retiring.is_empty());
    assert!(peer.app.shared_views.attached.contains(&peer.card));
    assert_eq!(
        peer.app.chat_ui.drafts.get(&peer.card).map(String::as_str),
        Some("not sent yet"),
        "what this window wrote is for the attached conversation's composer"
    );
}

/// The trap in attaching in place: a failed start of a draft runtime counts as
/// this window's own first send, which retires what it started.
#[test]
fn failing_to_attach_never_retires_the_other_windows_conversation() {
    let mut peer = peer();
    let accepted = started(&peer.draft);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted),
    ));
    let target = match peer.app.chat_ui.phases.get(&peer.card) {
        Some(Phase::Starting(attempt)) => attempt.target,
        _ => panic!("expected an attachment under way"),
    };

    drop(peer.app.handle_chat_ui(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            "the daemon went away".to_string(),
        )))))),
    )));

    assert!(peer.app.chat_ui.retiring.is_empty());
    assert!(matches!(
        peer.app.cards.get(&peer.card),
        Some(CardRuntime::Failed(_))
    ));
    assert!(peer.app.card_state(peer.card).unwrap().existence.is_open());
}

#[test]
fn what_this_window_wrote_survives_the_other_windows_send() {
    let mut peer = peer();
    write(&mut peer.app, peer.card, "not sent yet");
    // Sending gives the shared draft a new identity before it is accepted.
    let candidate = draft_session(&peer.draft.launch.recipe.working_directory);

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(candidate.clone()),
    ));
    assert_eq!(
        draft_runtime(&peer.app, peer.card).unsent_text(),
        "not sent yet"
    );

    let accepted = started(&candidate);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));
    assert!(peer.app.chat_start_matches(peer.card, &accepted));
    assert_eq!(
        peer.app.chat_ui.drafts.get(&peer.card).map(String::as_str),
        Some("not sent yet")
    );
}

#[test]
fn a_window_starting_the_draft_itself_keeps_its_own_start() {
    let mut peer = peer();
    let own = peer.draft.clone();
    let target = draft_runtime(&peer.app, peer.card).target;
    peer.app.chat_ui.phases.insert(
        peer.card,
        Phase::Starting(Attempt {
            target,
            intent: chat::Intent::Start,
            session: own.clone(),
            recovery: None,
            upgrade_committed: false,
            upgrade_admission: None,
        }),
    );

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(started(&own)),
    ));

    assert!(peer.app.chat_is_draft(peer.card));
    assert!(peer.app.chat_start_matches(peer.card, &own));
}
