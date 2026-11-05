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

fn attach_started(peer: &mut Peer) -> chat::Session {
    let accepted = started(&peer.draft);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));
    assert!(peer.app.chat_start_matches(peer.card, &accepted));
    accepted
}

fn attempt_target(peer: &Peer) -> Target {
    match peer.app.chat_ui.phases.get(&peer.card) {
        Some(Phase::Starting(attempt)) => attempt.target,
        _ => panic!("expected an attachment under way"),
    }
}

/// The other window's start failed or was cancelled while this one was still
/// attaching: it returns to the draft, and the late answer changes nothing.
#[test]
fn a_start_given_up_elsewhere_returns_an_attaching_window_to_the_draft() {
    let mut peer = peer();
    write(&mut peer.app, peer.card, "not sent yet");
    attach_started(&mut peer);
    let target = attempt_target(&peer);

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(peer.draft.clone()),
    ));

    let runtime = draft_runtime(&peer.app, peer.card);
    assert_eq!(runtime.session, peer.draft);
    assert_eq!(runtime.unsent_text(), "not sent yet");
    assert!(!peer.app.chat_start_pending(peer.card));

    drop(peer.app.handle_chat_ui(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            "the conversation ended".to_string(),
        )))))),
    )));
    assert!(peer.app.chat_ui.retiring.is_empty());
    assert_eq!(draft_runtime(&peer.app, peer.card).session, peer.draft);
}

/// An attachment that already failed is not left on the failure when the
/// window it was attaching to goes back to its draft.
#[test]
fn a_start_given_up_elsewhere_replaces_a_failed_attachment_with_the_draft() {
    let mut peer = peer();
    attach_started(&mut peer);
    let target = attempt_target(&peer);
    drop(peer.app.handle_chat_ui(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            "the daemon went away".to_string(),
        )))))),
    )));
    assert!(matches!(
        peer.app.cards.get(&peer.card),
        Some(CardRuntime::Failed(_))
    ));

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(peer.draft.clone()),
    ));

    assert_eq!(draft_runtime(&peer.app, peer.card).session, peer.draft);
    assert!(peer.app.chat_ui.retiring.is_empty());
}

/// Only a card this window released follows the shared pane back. A window's
/// own attachment is not taken down by an unstarted descriptor of the same id.
#[test]
fn a_chat_this_window_did_not_release_ignores_a_returned_draft() {
    let mut peer = peer();
    assert!(
        !peer
            .app
            .release_chat_returned_to_draft(peer.card, &peer.draft)
    );
    attach_started(&mut peer);
    peer.app.forget_peer_started(peer.card);
    assert!(
        !peer
            .app
            .release_chat_returned_to_draft(peer.card, &peer.draft)
    );
    assert!(peer.app.chat_start_pending(peer.card));
}

#[test]
fn a_file_choice_holds_the_draft_until_it_ends_and_then_attaches() {
    let mut peer = peer();
    let ticket = {
        let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get_mut(&peer.card) else {
            panic!("expected a chat runtime");
        };
        let ticket = runtime.begin_attachment_insert();
        runtime.attachment_chooser_pending = Some(ticket.id);
        ticket
    };
    let target = draft_runtime(&peer.app, peer.card).target;
    let accepted = started(&peer.draft);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));
    assert!(peer.app.chat_is_draft(peer.card));

    drop(
        peer.app
            .handle_chat_ui(Action::Attached(target, ticket, Ok(None))),
    );

    assert!(peer.app.chat_start_matches(peer.card, &accepted));
    assert!(peer.app.chat_ui.retiring.is_empty());
}

#[test]
fn a_paste_holds_the_draft_until_it_ends_and_then_attaches() {
    let mut peer = peer();
    let ticket = {
        let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get_mut(&peer.card) else {
            panic!("expected a chat runtime");
        };
        runtime.begin_attachment_insert()
    };
    let target = draft_runtime(&peer.app, peer.card).target;
    let accepted = started(&peer.draft);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));
    assert!(peer.app.chat_is_draft(peer.card));

    drop(peer.app.handle_chat_ui(Action::PastedText(
        target,
        ticket,
        Some("pasted".to_string()),
    )));

    assert!(peer.app.chat_start_matches(peer.card, &accepted));
    assert!(
        peer.app
            .chat_ui
            .drafts
            .get(&peer.card)
            .is_some_and(|text| text.contains("pasted")),
        "the finished paste goes with the rest of the draft"
    );
}

/// A window whose own save lost the race has no pending start any more, and
/// its Send would otherwise do nothing until the next model update. What it
/// tried to send is still its writing.
#[test]
fn losing_a_start_to_another_window_attaches_once_the_save_has_answered() {
    let mut peer = peer();
    write(&mut peer.app, peer.card, "my own first message");
    let target = draft_runtime(&peer.app, peer.card).target;
    peer.app
        .chat_ui
        .phases
        .insert(peer.card, Phase::SavingDraft { target });
    let accepted = started(&peer.draft);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));
    assert!(
        peer.app.chat_is_draft(peer.card),
        "a save under way keeps its draft"
    );

    peer.app.chat_ui.phases.remove(&peer.card);
    drop(peer.app.adopt_stored_draft_session(peer.card));

    assert!(peer.app.chat_start_matches(peer.card, &accepted));
    assert_eq!(
        peer.app.chat_ui.drafts.get(&peer.card).map(String::as_str),
        Some("my own first message")
    );
}

fn session_update(peer: &mut Peer, publish: chat::Session, current: chat::Session) -> uuid::Uuid {
    let pane = |kind| Pane {
        id: peer.card,
        title: "Shared".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: None,
        kind,
    };
    let change = crate::workspace_model::views::Change::Update {
        before: pane(CardKind::Chat(peer.draft.clone())),
        pane: pane(CardKind::Chat(publish)),
    };
    let operation = uuid::Uuid::new_v4();
    let scope = Scope::Channel(peer.channel);
    peer.app
        .shared_views
        .pending
        .push((operation, scope.clone(), change.clone()));
    let mut model = peer.app.workspace_model.clone();
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap();
    view.revision += 1;
    view.panes = vec![pane(CardKind::Chat(current))];
    model.sequence += 1;
    drop(peer.app.refuse_conflicting_view_update(
        scope,
        operation,
        change,
        model,
        "The workspace view changed".into(),
    ));
    operation
}

fn warned_of_refused_update(peer: &Peer) -> bool {
    peer.app
        .notices
        .recent()
        .any(|entry| entry.notice.message.contains("session update was refused"))
}

/// Two windows attached to one conversation learn its provider session at the
/// same time. The one that publishes second has lost nothing.
#[test]
fn losing_a_race_to_the_same_session_update_is_not_worth_a_warning() {
    let mut peer = peer();
    let accepted = started(&peer.draft);
    let operation = session_update(&mut peer, accepted.clone(), accepted);

    assert!(!warned_of_refused_update(&peer));
    assert!(
        !peer
            .app
            .shared_views
            .pending
            .iter()
            .any(|(id, _, _)| *id == operation)
    );
}

#[test]
fn losing_a_race_to_a_different_session_update_still_warns() {
    let mut peer = peer();
    let accepted = started(&peer.draft);
    let mut other = accepted.clone();
    other.launch.session_id = Some("another-provider-conversation".into());

    session_update(&mut peer, accepted, other);

    assert!(warned_of_refused_update(&peer));
}

/// What this window wrote reaches the composer of the conversation it attaches
/// to, not only the stash it waits in.
#[test]
fn what_this_window_wrote_is_in_the_composer_once_attached() {
    let mut peer = peer();
    write(&mut peer.app, peer.card, "not sent yet");
    attach_started(&mut peer);
    let target = attempt_target(&peer);

    let handle = super::tests::fixture_handle(&peer.app, peer.card, Some("provider-conversation"));
    drop(peer.app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    ))));

    let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get(&peer.card) else {
        panic!("expected the attached conversation");
    };
    assert!(!runtime.is_draft());
    assert_eq!(runtime.unsent_text(), "not sent yet");
    assert!(!peer.app.chat_ui.drafts.contains_key(&peer.card));
}
