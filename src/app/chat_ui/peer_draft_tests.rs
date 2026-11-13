//! A window already showing a shared draft when another window sends its first
//! message has to end up attached to that conversation, not left on a draft
//! that offers to start it again.
use super::*;
use crate::workspace_model::views::{Change, Pane, Scope, View};

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
    app.workspace_model
        .channels
        .push(crate::workspace_model::ModelChannel {
            id: channel,
            repository: std::path::PathBuf::from("/repo/.git"),
            title: String::new(),
            title_source: Default::default(),
            ordinal: 1,
        });
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
            resume_committed: false,
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

/// A window that finished attaching, with its own session update still
/// waiting for the host's answer.
fn attached_with_update_pending(peer: &mut Peer) -> (uuid::Uuid, Change) {
    write(&mut peer.app, peer.card, "not sent yet");
    attach_started(peer);
    let target = attempt_target(peer);
    let handle = super::tests::fixture_handle(&peer.app, peer.card, Some("provider-conversation"));
    drop(peer.app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    ))));
    assert!(!peer.app.chat_is_draft(peer.card));
    let (operation, _, change) = peer.app.shared_views.pending.first().unwrap().clone();
    (operation, change)
}

fn assert_back_on_the_draft(peer: &Peer) {
    let runtime = draft_runtime(&peer.app, peer.card);
    assert_eq!(runtime.session, peer.draft);
    assert_eq!(runtime.unsent_text(), "not sent yet");
    assert!(peer.app.chat_ui.retiring.is_empty());
}

/// The host took this window's update before the other window gave the start
/// up: the attachment is dropped, not retired.
#[test]
fn a_start_given_up_after_attaching_returns_the_window_to_the_draft() {
    let mut peer = peer();
    let (operation, _) = attached_with_update_pending(&mut peer);
    let kind = peer.app.card_state(peer.card).unwrap().kind.clone();
    drop(project(&mut peer.app, peer.channel, peer.card, kind));
    peer.app
        .acknowledge_view_edit(Scope::Channel(peer.channel), Some(operation));
    assert!(peer.app.shared_views.pending.is_empty());

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(peer.draft.clone()),
    ));

    assert_back_on_the_draft(&peer);
}

/// The other window gave the start up first, so the host refuses this window's
/// update. The refusal must not leave it attached to the abandoned start.
#[test]
fn a_start_given_up_before_the_update_lands_returns_the_window_to_the_draft() {
    let mut peer = peer();
    let (operation, change) = attached_with_update_pending(&mut peer);
    let mut model = peer.app.workspace_model.clone();
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(peer.channel))
        .unwrap();
    view.revision += 1;
    view.panes[0].kind = CardKind::Chat(peer.draft.clone());
    model.sequence += 1;

    drop(peer.app.refuse_conflicting_view_update(
        Scope::Channel(peer.channel),
        operation,
        change,
        model,
        "The workspace view changed".into(),
    ));

    assert_back_on_the_draft(&peer);
}

/// A connection that arrives after the other window gave the start up is
/// dropped: it neither replaces the draft nor retires anything.
#[test]
fn a_late_connection_after_a_start_given_up_elsewhere_changes_nothing() {
    let mut peer = peer();
    attach_started(&mut peer);
    let target = attempt_target(&peer);
    let handle = super::tests::fixture_handle(&peer.app, peer.card, Some("provider-conversation"));

    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(peer.draft.clone()),
    ));
    drop(peer.app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    ))));

    assert_eq!(draft_runtime(&peer.app, peer.card).session, peer.draft);
    assert!(peer.app.chat_ui.retiring.is_empty());
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
    session_update_under_title(peer, publish, current, "Shared")
}

fn session_update_under_title(
    peer: &mut Peer,
    publish: chat::Session,
    current: chat::Session,
    current_title: &str,
) -> uuid::Uuid {
    let pane = |kind| Pane {
        id: peer.card,
        title: "Shared".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: None,
        kind,
    };
    let change = Change::Update {
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
    view.panes = vec![Pane {
        title: current_title.into(),
        ..pane(CardKind::Chat(current))
    }];
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

/// The first send names the conversation while each window's session update is
/// on its way. The update never changed the name, so losing it loses nothing.
#[test]
fn losing_a_race_after_the_conversation_was_named_stays_quiet() {
    let mut peer = peer();
    let accepted = started(&peer.draft);

    session_update_under_title(
        &mut peer,
        accepted.clone(),
        accepted,
        "Summarize the release checklist",
    );

    assert!(!warned_of_refused_update(&peer));
}

/// A refused edit that changed no session can be a lost name or width, so it
/// is not mistaken for a session update another window already made.
#[test]
fn losing_a_race_with_no_session_change_still_warns() {
    let mut peer = peer();
    let unchanged = peer.draft.clone();

    session_update(&mut peer, unchanged.clone(), unchanged);

    assert!(warned_of_refused_update(&peer));
}

/// A window that attached to a chat another window is running.
fn working_chat(peer: &mut Peer) -> uuid::Uuid {
    let (operation, _) = attached_with_update_pending(peer);
    publish_activity(peer, |snapshot| snapshot.busy = true);
    operation
}

/// What the provider reports reaches the handle whether or not the pane is on
/// screen. The runtime's own copy is left alone, as it is for a hidden pane.
fn publish_activity(peer: &Peer, change: impl FnOnce(&mut Snapshot)) {
    let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get(&peer.card) else {
        panic!("expected a chat runtime");
    };
    let handle = runtime.handle().unwrap();
    let mut snapshot = (*handle.snapshot()).clone();
    change(&mut snapshot);
    handle.publish_snapshot_for_test(snapshot);
}

fn card_open(peer: &Peer) -> bool {
    peer.app
        .card_state(peer.card)
        .is_some_and(|state| state.existence.is_open())
        && peer.app.workspace.pending_close().is_none()
        && !peer.app.chat_close_owned(peer.card)
}

/// Closing a pane ends it for every window, so one that is working asks first,
/// and keeping it changes nothing.
#[test]
fn closing_a_working_chat_asks_and_keeping_it_changes_nothing() {
    let mut peer = peer();
    working_chat(&mut peer);

    drop(peer.app.update(Message::CloseCard(peer.card)));
    assert!(peer.app.chat_close_dialog_visible());
    assert!(card_open(&peer));

    drop(peer.app.update(Message::CancelChatClose));
    assert!(!peer.app.chat_close_dialog_visible());
    assert!(card_open(&peer));
}

#[test]
fn stopping_a_working_chat_closes_it_once_confirmed() {
    let mut peer = peer();
    working_chat(&mut peer);
    drop(peer.app.update(Message::CloseCard(peer.card)));
    assert!(peer.app.chat_close_dialog_visible());

    drop(peer.app.update(Message::ConfirmChatClose));

    assert!(!peer.app.chat_close_dialog_visible());
    assert!(!card_open(&peer));
}

/// The window may still be attaching, in which case it cannot know the chat is
/// idle.
#[test]
fn closing_a_chat_this_window_is_attaching_to_asks() {
    let mut peer = peer();
    attach_started(&mut peer);

    drop(peer.app.update(Message::CloseCard(peer.card)));

    assert!(peer.app.chat_close_dialog_visible());
    assert!(card_open(&peer));
}

#[test]
fn a_close_with_nothing_working_does_not_ask() {
    let mut draft = peer();
    drop(draft.app.update(Message::CloseCard(draft.card)));
    assert!(!draft.app.chat_close_dialog_visible());

    let mut idle = peer();
    attached_with_update_pending(&mut idle);
    drop(idle.app.update(Message::CloseCard(idle.card)));
    assert!(!idle.app.chat_close_dialog_visible());
}

/// Delegated work outlives the turn that started it, and closing stops it too.
#[test]
fn closing_a_chat_with_a_running_background_agent_asks() {
    let mut peer = peer();
    attached_with_update_pending(&mut peer);
    publish_activity(&peer, |snapshot| {
        snapshot.subagents.push(chat::Subagent {
            id: "agent".into(),
            kind: "general".into(),
            description: "Background work".into(),
            status: chat::ToolStatus::Running,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        });
    });

    drop(peer.app.update(Message::CloseCard(peer.card)));

    assert!(peer.app.chat_close_dialog_visible());
}

/// The copy on the runtime is only refreshed on screen, so a turn that ended
/// while the pane was hidden still looks busy there.
#[test]
fn a_chat_that_finished_while_hidden_closes_without_asking() {
    let mut peer = peer();
    attached_with_update_pending(&mut peer);
    let CardRuntime::Chat(runtime) = peer.app.cards.get_mut(&peer.card).unwrap() else {
        panic!("expected a chat runtime");
    };
    Arc::make_mut(&mut runtime.snapshot).busy = true;

    drop(peer.app.update(Message::CloseCard(peer.card)));

    assert!(!peer.app.chat_close_dialog_visible());
}

/// Closing while the first message is on its way would drop the prompt, at
/// every step of the send: saving it, starting the provider, and waiting for
/// the provider to come up, which the provider reports as doing nothing.
#[test]
fn closing_at_any_step_of_this_windows_first_send_asks_and_keeps_the_message() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = super::tests::draft_chat_for_provider(&mut app, Provider::Claude);
    write(&mut app, card, "hello");

    super::tests::submit(&mut app, card);
    assert!(app.chat_start_pending(card), "the provider is starting");
    let asks_and_keeps = |app: &mut Cagents| {
        drop(app.update(Message::CloseCard(card)));
        assert!(app.chat_close_dialog_visible());
        drop(app.update(Message::CancelChatClose));
        assert!(!app.chat_close_dialog_visible());
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("the chat stays");
        };
        assert!(runtime.queued.is_some(), "and so does its first message");
    };
    asks_and_keeps(&mut app);

    let target = super::tests::chat_runtime_attempt(&app, card);
    let handle = super::tests::fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot {
        ready: false,
        ..Snapshot::default()
    });
    super::tests::answer_start(&mut app, target, Ok(handle));
    assert!(!app.chat_start_pending(card), "attached, not yet up");
    asks_and_keeps(&mut app);
    let session = app.chat_working_session(card).unwrap();
    assert!(session.1, "this window knows what it sent");
}

/// A window that came to the conversation from elsewhere has no message of its
/// own to go by; a provider still coming up reports idle and has not been.
#[test]
fn closing_a_conversation_still_coming_up_elsewhere_asks_without_claiming_it_works() {
    let mut peer = peer();
    attached_with_update_pending(&mut peer);
    publish_activity(&peer, |snapshot| snapshot.ready = false);

    assert!(!peer.app.chat_working_session(peer.card).unwrap().1);
    drop(peer.app.update(Message::CloseCard(peer.card)));
    assert!(peer.app.chat_close_dialog_visible());

    drop(peer.app.update(Message::CancelChatClose));
    assert!(card_open(&peer));
}

/// Closes held behind a question go on once its conversation is gone, and a
/// second close of any kind waits rather than opening a dialog under it.
#[test]
fn closes_waiting_on_a_question_go_on_when_it_ends_elsewhere() {
    let mut peer = peer();
    let operation = working_chat(&mut peer);
    let kind = peer.app.card_state(peer.card).unwrap().kind.clone();
    drop(project(&mut peer.app, peer.channel, peer.card, kind));
    peer.app
        .acknowledge_view_edit(Scope::Channel(peer.channel), Some(operation));
    drop(peer.app.update(Message::CloseCard(peer.card)));

    let waiting = CardId::new();
    drop(peer.app.begin_card_close(waiting));
    assert!(peer.app.terminal_close.is_none());
    assert!(peer.app.pane_close_owned(waiting));

    let replacement = draft_session(peer._directory.path());
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(replacement),
    ));
    drop(peer.app.update(Message::CancelChatClose));

    assert!(peer.app.queued_card_closes.is_empty());
    assert!(!peer.app.pane_close_owned(waiting));
}

/// An answer that arrives just as its question ends must not answer the next
/// question, which the reader has not seen.
#[test]
fn an_answer_to_a_question_that_ended_does_not_answer_the_next() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut working = |thread| {
        let card = super::tests::live_chat_for_provider(&mut app, Provider::Claude, Some(thread));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("expected a chat runtime");
        };
        let handle = runtime.handle().unwrap();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.busy = true;
        handle.publish_snapshot_for_test(snapshot);
        card
    };
    let asked = working("asked-thread");
    let next = working("next-thread");

    drop(app.update(Message::CloseCard(asked)));
    assert!(app.chat_close_asked(asked));
    drop(app.begin_card_close(next));
    assert_eq!(app.queued_card_closes.len(), 1);
    let Some(CardKind::Chat(session)) = app.card_state(asked).map(|state| state.kind.clone())
    else {
        panic!("expected a chat");
    };
    app.replace_chat_conversation(
        asked,
        chat::Session {
            id: TerminalId::new(),
            ..session
        },
    );

    drop(app.update(Message::ConfirmChatClose));

    assert!(app.chat_close_asked(next));
    assert!(!app.chat_close_owned(next));
    assert!(
        app.card_state(next)
            .is_some_and(|state| state.existence.is_open())
    );
}

/// A window attached to a conversation another window is starting holds its
/// message rather than sending it to a provider that would refuse it.
#[test]
fn a_message_written_while_the_provider_comes_up_waits_in_the_composer() {
    let mut peer = peer();
    attached_with_update_pending(&mut peer);
    let target = {
        let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get_mut(&peer.card) else {
            panic!("expected a chat runtime");
        };
        Arc::make_mut(&mut runtime.snapshot).ready = false;
        runtime.replace_draft(text_editor::Content::with_text("from this window"));
        runtime.target
    };

    drop(
        peer.app
            .update(Message::ChatUi(Action::Submit(target, PromptMode::Send))),
    );

    let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get(&peer.card) else {
        panic!("expected a chat runtime");
    };
    assert!(runtime.pending.is_none());
    assert!(runtime.error.is_none());
    assert_eq!(runtime.unsent_text(), "from this window");
}

/// A provider coming up and in error is not going to work on anything.
#[test]
fn a_provider_that_failed_coming_up_closes_without_asking() {
    let mut peer = peer();
    attached_with_update_pending(&mut peer);
    publish_activity(&peer, |snapshot| {
        snapshot.ready = false;
        snapshot.error = Some("the provider could not start".into());
    });

    assert!(peer.app.chat_working_session(peer.card).is_none());
}

/// A popup that closes under the question hands it the reader's keyboard state.
#[test]
fn answering_restores_the_keyboard_state_a_popup_handed_over() {
    let mut peer = peer();
    working_chat(&mut peer);
    peer.app.core.set_keyboard_nav(false);
    drop(peer.app.update(Message::CloseCard(peer.card)));

    assert!(peer.app.adopt_chat_close_keyboard_nav(true));
    drop(peer.app.update(Message::CancelChatClose));

    assert!(peer.app.core.keyboard_nav());
}

/// A question about a conversation that has since been closed or replaced has
/// no answer left, and must not hold the window's keyboard behind it.
#[test]
fn the_question_ends_when_the_other_window_replaces_the_conversation() {
    let mut peer = peer();
    let operation = working_chat(&mut peer);
    let kind = peer.app.card_state(peer.card).unwrap().kind.clone();
    drop(project(&mut peer.app, peer.channel, peer.card, kind));
    peer.app
        .acknowledge_view_edit(Scope::Channel(peer.channel), Some(operation));
    drop(peer.app.update(Message::CloseCard(peer.card)));
    assert!(peer.app.chat_close_dialog_visible());

    let replacement = draft_session(peer._directory.path());
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(replacement),
    ));

    assert!(!peer.app.chat_close_dialog_visible());

    // An answer already on its way cannot close what replaced the conversation.
    drop(peer.app.update(Message::ConfirmChatClose));
    assert!(!peer.app.chat_close_asked(peer.card));
    assert!(card_open(&peer));
}

/// A question asked beside a close under way could end up under that close's
/// own dialog, so it waits for it. With nothing to ask, the chat needn't wait.
#[test]
fn a_working_chat_asks_only_after_the_close_ahead_of_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut chat = |thread, busy| {
        let card = super::tests::live_chat_for_provider(&mut app, Provider::Claude, Some(thread));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("expected a chat runtime");
        };
        let handle = runtime.handle().unwrap();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.ready = true;
        snapshot.busy = busy;
        handle.publish_snapshot_for_test(snapshot);
        card
    };
    let ahead = chat("ahead-thread", false);
    let working = chat("working-thread", true);
    let idle = chat("idle-thread", false);
    app.chrome_ui.closing_group.push(ahead);

    drop(app.update(Message::CloseCard(working)));
    assert!(!app.chat_close_dialog_visible());
    assert!(app.pane_close_owned(working));
    assert_eq!(app.queued_card_closes.len(), 1);

    drop(app.update(Message::CloseCard(idle)));
    assert_eq!(app.queued_card_closes.len(), 1);
    assert!(!app.chat_close_asked(idle));

    app.chrome_ui.closing_group.clear();
    drop(app.next_queued_card_close());
    assert!(app.chat_close_asked(working));
    assert!(!app.chat_close_owned(working));
}

/// History means this chat has lost its provider rather than yet to get one.
/// Holding the message there would be a dead composer with no explanation.
#[test]
fn a_chat_with_history_that_lost_its_provider_does_not_hold_a_message() {
    let mut peer = peer();
    attached_with_update_pending(&mut peer);
    let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get_mut(&peer.card) else {
        panic!("expected a chat runtime");
    };
    Arc::make_mut(&mut runtime.snapshot).ready = false;
    assert!(runtime.provider_coming_up());

    Arc::make_mut(&mut runtime.snapshot).items = vec![
        chat::Item {
            id: "earlier".into(),
            role: chat::Role::Assistant,
            content: chat::ItemContent::Text {
                text: "An earlier reply".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            started_at: None,
            at: None,
        }
        .into(),
    ];

    assert!(!runtime.provider_coming_up());
}

/// Esc goes through update like the dialog's buttons, so the close waiting
/// behind the question gets its turn straight away. It answers the question
/// even when the composer the question opened over captured it.
#[test]
fn escape_on_the_question_lets_the_next_close_ask() {
    for captured in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut working = |thread| {
            let card =
                super::tests::live_chat_for_provider(&mut app, Provider::Claude, Some(thread));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("expected a chat runtime");
            };
            let handle = runtime.handle().unwrap();
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.busy = true;
            handle.publish_snapshot_for_test(snapshot);
            card
        };
        let asked = working("asked-thread");
        let next = working("next-thread");
        drop(app.update(Message::CloseCard(asked)));
        drop(app.begin_card_close(next));
        assert!(app.chat_close_asked(asked));

        let task = app.update(Message::KeyEvent {
            event: crate::app::dialog_ui::tests::key_press(iced::keyboard::key::Named::Escape),
            captured,
        });
        let cancel = crate::app::tests::task_app_message(task);
        assert!(
            matches!(cancel, Message::CancelChatClose),
            "captured={captured}"
        );
        drop(app.update(cancel));

        assert!(app.chat_close_asked(next), "captured={captured}");
        assert!(!app.chat_close_owned(asked), "captured={captured}");
    }
}

/// Gives `peer` a store of its own holding the draft it shows, as each
/// window's own sessions database does.
fn with_own_store(peer: &mut Peer) -> Arc<crate::persistence::StateStore> {
    let (writer, _) = crate::session::SessionRuntime::open(
        peer._directory.path().join("window-b.sqlite3"),
        crate::model::BootId::new("peer-own-store"),
    )
    .unwrap();
    writer.snapshot(peer.app.persistent_workspace());
    writer.flush(std::time::Duration::from_secs(5)).unwrap();
    let store = writer.operation_store();
    peer.app.session = Some(writer);
    store
}

/// Saves what `peer` shows and returns the chat its own store then holds.
fn saved_chat(
    peer: &Cagents,
    card: CardId,
    store: &crate::persistence::StateStore,
) -> chat::Session {
    let writer = peer.session.as_ref().unwrap();
    writer
        .flush(std::time::Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("this window's store refused the pane: {error}"));
    assert_eq!(writer.last_error(), None);
    match store.load_card(card, None).unwrap().map(|card| card.kind) {
        Some(CardKind::Chat(session)) => session,
        other => panic!("expected a saved chat, found {other:?}"),
    }
}

fn retained_claude(session: &chat::Session) -> chat::Session {
    let mut retained = session.clone();
    retained.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    retained.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    retained.proof = Some(crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    });
    assert!(retained.has_valid_native_continuity());
    retained
}

/// The other window's Send saves the draft under a new identity, which its
/// own store records. This window only follows the shared view, and its own
/// store would refuse every later write of the pane unless it takes that
/// conversation too (#2176). So would a resume in the other window, which
/// starts a successor conversation (#2402), and a start given up there, which
/// returns the same conversation to a draft that no longer retains its
/// provider (#2349).
#[test]
fn this_windows_store_follows_the_conversation_another_window_moves_the_pane_to() {
    let mut peer = peer();
    let store = with_own_store(&mut peer);
    let mut candidate = peer.draft.clone();
    candidate.id = TerminalId::new();
    let accepted = retained_claude(&candidate);

    for (step, kind) in [
        ("first send", candidate.clone()),
        ("started", accepted.clone()),
        ("returned to the draft", candidate.clone()),
        ("started again", accepted.clone()),
        ("resumed", {
            let mut successor = retained_claude(&accepted);
            successor.id = TerminalId::new();
            successor
        }),
    ] {
        drop(project(
            &mut peer.app,
            peer.channel,
            peer.card,
            CardKind::Chat(kind.clone()),
        ));
        assert_eq!(saved_chat(&peer.app, peer.card, &store), kind, "{step}");
    }
}

/// Both windows send the shared draft's first message at once. This window's
/// own send has queued its conversation for its store, which holds it once
/// written, when the other window's start wins the shared view. The store
/// follows the winner whether or not its own write landed first, rather than
/// refusing every later write of the pane.
#[test]
fn this_windows_store_follows_the_other_windows_start_while_its_own_send_saves() {
    for own_send_written in [false, true] {
        let mut peer = peer();
        let store = with_own_store(&mut peer);
        write(&mut peer.app, peer.card, "my own first message");
        let target = draft_runtime(&peer.app, peer.card).target;
        drop(peer.app.start_chat_draft(target));
        let own = peer
            .app
            .draft_start_candidate(peer.card)
            .expect("this window's send is saving")
            .clone();
        if own_send_written {
            assert_eq!(saved_chat(&peer.app, peer.card, &store), own);
        }

        let mut theirs = peer.draft.clone();
        theirs.id = TerminalId::new();
        let theirs = retained_claude(&theirs);
        drop(project(
            &mut peer.app,
            peer.channel,
            peer.card,
            CardKind::Chat(theirs.clone()),
        ));
        assert_eq!(
            saved_chat(&peer.app, peer.card, &store),
            theirs,
            "own send written first: {own_send_written}"
        );
    }
}

/// Unsent text in this window that only repeats the message the other window
/// sent goes once the transcript shows that message; anything else stays.
#[test]
fn a_copy_of_the_first_message_leaves_the_composer_and_other_writing_stays() {
    const SENT: &str = "Message from window A";
    for (text, kept) in [
        (SENT, ""),
        ("  Message from window A\n", ""),
        ("Notes typed in window B", "Notes typed in window B"),
    ] {
        let mut peer = peer();
        write(&mut peer.app, peer.card, text);
        let mut candidate = peer.draft.clone();
        candidate.id = TerminalId::new();
        drop(project(
            &mut peer.app,
            peer.channel,
            peer.card,
            CardKind::Chat(started(&candidate)),
        ));
        let target = attempt_target(&peer);
        let handle =
            super::tests::fixture_handle(&peer.app, peer.card, Some("provider-conversation"));
        drop(peer.app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        ))));
        let unsent = |peer: &Peer| match peer.app.cards.get(&peer.card) {
            Some(CardRuntime::Chat(runtime)) if !runtime.is_draft() => runtime.unsent_text(),
            _ => panic!("expected the attached conversation"),
        };
        assert_eq!(
            unsent(&peer),
            text,
            "{text:?}: kept until the transcript shows"
        );

        super::show_chat_snapshot_for_test(
            &mut peer.app,
            peer.card,
            Snapshot {
                session_id: Some("provider-conversation".into()),
                items: vec![Arc::new(chat::Item {
                    started_at: None,
                    id: "first".into(),
                    role: chat::Role::User,
                    content: chat::ItemContent::Text {
                        text: SENT.into(),
                        thinking: false,
                        phase: Default::default(),
                    },
                    at: None,
                })],
                ..Snapshot::default()
            },
        );
        assert_eq!(unsent(&peer), kept, "{text:?}");
    }
}

/// A file choice under way when the other window's first send arrives under
/// a new identity keeps the draft until it ends, and the chosen file comes
/// along to the attached conversation.
#[test]
fn a_file_chosen_while_another_window_sends_comes_along() {
    const FILE: &str = "/repo/notes.txt";
    let mut peer = peer();
    write(&mut peer.app, peer.card, "notes");
    let ticket = {
        let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get_mut(&peer.card) else {
            panic!("expected a chat runtime");
        };
        let ticket = runtime.begin_attachment_insert();
        runtime.attachment_chooser_pending = Some(ticket.id);
        ticket
    };
    let target = draft_runtime(&peer.app, peer.card).target;
    let mut candidate = peer.draft.clone();
    candidate.id = TerminalId::new();
    let accepted = started(&candidate);
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(accepted.clone()),
    ));
    assert!(
        peer.app.chat_is_draft(peer.card),
        "the chooser holds the draft"
    );

    let _admission = super::attachment_draft::test_admission();
    drop(peer.app.handle_chat_ui(Action::Attached(
        target,
        ticket.clone(),
        Ok(Some(vec![std::path::PathBuf::from(FILE)])),
    )));
    assert!(
        peer.app.chat_is_draft(peer.card),
        "the file is still loading"
    );
    drop(peer.app.handle_chat_ui(Action::AttachmentsPrepared(
        target,
        ticket,
        Ok(vec![Attachment::file(FILE)]),
    )));
    assert!(peer.app.chat_start_matches(peer.card, &accepted));
    assert!(peer.app.chat_ui.retiring.is_empty());

    let handle = super::tests::fixture_handle(&peer.app, peer.card, Some("provider-conversation"));
    drop(peer.app.update(Message::ChatUi(Action::Ready(
        attempt_target(&peer),
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    ))));
    let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get(&peer.card) else {
        panic!("expected the attached conversation");
    };
    assert!(!runtime.is_draft());
    assert_eq!(runtime.unsent_text(), "notes");
    assert_eq!(
        runtime
            .attachments
            .iter()
            .map(|attachment| attachment.path.as_path())
            .collect::<Vec<_>>(),
        [std::path::Path::new(FILE)]
    );
}

/// This window's own first message, still queued behind its provider's start
/// when the other window's start replaces the pane, stays this window's
/// writing: its text and its attached file are both kept for the composer
/// that opens next.
#[test]
fn a_file_queued_behind_this_windows_own_start_comes_along() {
    const FILE: &str = "/repo/queued.txt";
    let mut peer = peer();
    with_own_store(&mut peer);
    write(&mut peer.app, peer.card, "my own first message");
    {
        let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get_mut(&peer.card) else {
            panic!("expected a chat runtime");
        };
        runtime.replace_attachments(vec![Attachment::file(FILE)]);
    }
    let target = draft_runtime(&peer.app, peer.card).target;
    let save = peer.app.start_chat_draft(target);
    let (target, attempt, result) = super::first_start_checkpoint_tests::checkpoint(save);
    assert!(result.is_ok());
    drop(
        peer.app
            .handle_chat_ui(Action::DraftStartCheckpointed(target, attempt, result)),
    );
    match peer.app.cards.get(&peer.card) {
        Some(CardRuntime::Chat(runtime)) => assert!(
            runtime.queued.as_ref().is_some_and(|queued| queued
                .attachments
                .iter()
                .any(|attachment| attachment.path == std::path::Path::new(FILE))),
            "the file waits with the queued first message"
        ),
        _ => panic!("expected a chat runtime"),
    }

    // The daemon answers this window's publish of its own conversation, which
    // lost to the other window's, before the view shows the winner.
    let published = peer
        .app
        .shared_views
        .pending
        .iter()
        .map(|(operation, _, _)| *operation)
        .collect::<Vec<_>>();
    for operation in published {
        peer.app
            .acknowledge_view_edit(Scope::Channel(peer.channel), Some(operation));
    }
    let mut theirs = peer.draft.clone();
    theirs.id = TerminalId::new();
    drop(project(
        &mut peer.app,
        peer.channel,
        peer.card,
        CardKind::Chat(started(&theirs)),
    ));
    assert_eq!(
        peer.app.chat_ui.drafts.get(&peer.card).map(String::as_str),
        Some("my own first message")
    );
    assert_eq!(
        peer.app.chat_ui.draft_attachments[&peer.card]
            .iter()
            .map(|attachment| attachment.path.as_path())
            .collect::<Vec<_>>(),
        [std::path::Path::new(FILE)]
    );
}
