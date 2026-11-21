//! A chat's first Send in a channel whose shared view is not admitted yet
//! waits for that view, and the reconcile that admits it starts it (#2697).
use super::*;
use crate::app::chat_ui::{Action, Target};
use crate::app::workspace_model_sync::ModelUpdate;

struct Window {
    app: Cagents,
    scope: Scope,
    card: CardId,
    target: Target,
    rx: TestModelRequests,
    _storage: tempfile::TempDir,
}

/// An unpublished default chat in an admitted channel, with a session writer
/// so its first Send can be checkpointed.
fn window() -> Window {
    let (mut app, channel, card, target, rx) = unpublished_default_chat();
    let storage = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        storage.path().join("state.sqlite3"),
        crate::model::BootId::new("unadmitted-first-send"),
    )
    .unwrap();
    app.session = Some(writer);
    Window {
        app,
        scope: Scope::Channel(channel),
        card,
        target,
        rx,
        _storage: storage,
    }
}

/// A composed chat in a channel whose seed is in flight.
fn seeding_channel() -> Window {
    let mut window = window();
    let scope = window.scope.clone();
    window.app.workspace_model.views.clear();
    window.app.shared_views.initialized.remove(&scope);
    window.app.shared_views.baseline.remove(&scope);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.seeding.contains(&scope));
    compose_default_chat(&mut window.app, window.target);
    drop(window.app.reconcile_shared_views());
    drain(&window.rx);
    window
}

/// A composed chat in a window whose daemon has yet to answer whether it has
/// a shared registry at all.
fn connecting_window() -> Window {
    let mut window = window();
    window.app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    window.app.workspace_model.views.clear();
    compose_default_chat(&mut window.app, window.target);
    drop(window.app.reconcile_shared_views());
    drain(&window.rx);
    window
}

fn drain(rx: &TestModelRequests) {
    while rx.try_recv().is_ok() {}
}

fn inserts(rx: &TestModelRequests, card: CardId) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|edit| {
            matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)
        })
        .count()
}

/// Runs `save`, a first Send's checkpoint, and hands its receipt back,
/// returning the provider work the receipt queued (dropped unpolled).
fn complete(window: &mut Window, save: Task<Message>) -> Task<Message> {
    assert!(save.units() > 0, "the first Send saves its draft first");
    let completion = first_start_save_completion(save);
    window.app.handle_chat_ui(completion)
}

fn send(window: &mut Window) -> Task<Message> {
    let save = window
        .app
        .handle_chat_ui(Action::Submit(window.target, crate::chat::PromptMode::Send));
    complete(window, save)
}

/// The message waits on the transcript: nothing asked the daemon to start,
/// the draft's source is fixed, and the composer is clear.
fn assert_held(window: &Window, start: Task<Message>) {
    assert_eq!(
        start.units(),
        0,
        "a provider must not start for a pane its channel has not recorded"
    );
    assert!(window.app.first_send_held(window.card));
    assert!(window.app.chat_start_pending(window.card));
    assert!(window.app.chat_start_attempt(window.card).is_none());
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("")
    );
}

/// The reconcile that admitted the scope started exactly one attempt, and no
/// later pass starts another.
fn assert_started_once(window: &mut Window) {
    let attempt = window
        .app
        .chat_start_attempt(window.card)
        .expect("the admitting reconcile starts the held message");
    assert!(!window.app.first_send_held(window.card));
    for _ in 0..2 {
        drop(window.app.reconcile_shared_views());
        assert_eq!(window.app.chat_start_attempt(window.card), Some(attempt));
    }
}

/// The registry's receipt for the scope's in-flight seed.
fn seed_applied(window: &mut Window, panes: Vec<Pane>, retired: Vec<CardId>) {
    let scope = window.scope.clone();
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let mut view = View::empty(scope.clone());
    view.revision = 1;
    view.panes = panes;
    view.retired.extend(retired);
    model.views.retain(|view| view.scope != scope);
    model.views.push(view);
    let attempt = window.app.shared_views.seed_attempts[&scope];
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedApplied {
                scope,
                attempt,
                model,
            }),
    );
}

#[test]
fn first_send_in_a_seeding_channel_waits_and_starts_once_the_seed_is_admitted() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    assert_eq!(inserts(&window.rx, window.card), 0);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.first_send_held(window.card));
    assert_eq!(inserts(&window.rx, window.card), 0);

    seed_applied(&mut window, Vec::new(), Vec::new());
    // Published by Insert, not migrated by the seed, before its start.
    assert_eq!(inserts(&window.rx, window.card), 1);
    assert_started_once(&mut window);
    assert_eq!(inserts(&window.rx, window.card), 0);
}

#[test]
fn first_send_in_a_blocked_channel_waits_until_the_block_clears() {
    let mut window = window();
    compose_default_chat(&mut window.app, window.target);
    // A terminal here whose canonical pane is listed by a view this window
    // cannot project holds this channel back until that view goes. The
    // previous pass already found it held back.
    window.app.shared_views.blocked.insert(window.scope.clone());
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(window.app.insert_terminal_card(session, None));
    let alias = window.app.workspace.focused().unwrap();
    assert_ne!(alias, window.card);
    let mut canonical = Pane::from_card(window.app.card_state(alias).unwrap(), None).unwrap();
    canonical.id = CardId::new();
    let mut elsewhere = View::empty(ChannelId::new());
    elsewhere.panes.push(canonical);
    window.app.workspace_model.views.push(elsewhere);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.blocked.contains(&window.scope));
    assert_eq!(inserts(&window.rx, window.card), 0);

    let start = send(&mut window);
    assert_held(&window, start);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.first_send_held(window.card));
    assert_eq!(inserts(&window.rx, window.card), 0);

    window
        .app
        .workspace_model
        .views
        .retain(|view| view.scope == window.scope);
    drop(window.app.reconcile_shared_views());
    assert!(!window.app.shared_views.blocked.contains(&window.scope));
    // Published, not kept as a local-only pane its provider could start in.
    assert!(!window.app.durable.local_only_cards.contains(&window.card));
    assert_eq!(inserts(&window.rx, window.card), 1);
    assert_started_once(&mut window);
}

#[test]
fn a_refused_channel_returns_the_message_and_retry_sends_it_once_admitted() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);

    let scope = window.scope.clone();
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedRefused {
                scope: scope.clone(),
                attempt: window.app.shared_views.seed_attempts[&scope],
                error: "The registry could not store these panes".into(),
            }),
    );
    assert!(!window.app.first_send_held(window.card));
    assert!(!window.app.chat_start_pending(window.card));
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
    let error = window.app.chat_error(window.card).unwrap();
    assert!(error.starts_with("This channel couldn't load"), "{error}");
    assert!(!error.contains("published"), "{error}");

    // A Send while the channel is still refused says so at once.
    let start = send(&mut window);
    assert_eq!(start.units(), 0);
    assert!(!window.app.first_send_held(window.card));
    assert!(window.app.chat_error(window.card).is_some());

    drain(&window.rx);
    let save = window
        .app
        .handle_chat_ui(Action::RetryRefusedView(window.target));
    assert!(window.app.shared_views.seeding.contains(&scope));
    let start = complete(&mut window, save);
    assert_held(&window, start);
    assert!(window.app.chat_error(window.card).is_none());

    let pane = Pane::from_card(window.app.card_state(window.card).unwrap(), None).unwrap();
    seed_applied(&mut window, vec![pane], Vec::new());
    assert_started_once(&mut window);
}

#[test]
fn a_pane_retired_elsewhere_while_it_waits_starts_nothing_and_keeps_the_message() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);

    let card = window.card;
    seed_applied(&mut window, Vec::new(), vec![card]);
    assert!(window.app.chat_start_attempt(window.card).is_none());
    assert!(!window.app.first_send_held(window.card));
    // Returned to its composer, whether as a refused start or as a pane
    // closed elsewhere that keeps its unsent message.
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
}

#[test]
fn taking_a_held_message_back_cancels_its_start() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    drop(
        window
            .app
            .handle_chat_ui(Action::CancelQueued(window.target)),
    );
    assert!(!window.app.first_send_held(window.card));
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
    seed_applied(&mut window, Vec::new(), Vec::new());
    assert!(window.app.chat_start_attempt(window.card).is_none());
}

#[test]
fn deleting_a_channel_whose_first_message_waits_deletes_it_and_starts_nothing() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    let Scope::Channel(channel) = window.scope.clone() else {
        unreachable!("the window's scope is its channel")
    };
    // A held message has started nothing, so deleting its channel goes on to
    // close the channel's chat, as for any draft, rather than waiting for a
    // start that only the channel's admission could release.
    drop(window.app.delete_channel(channel));
    assert!(
        window.app.channel_ui.closing_sessions(channel),
        "deleting the channel does not wait on the held message"
    );
    // Until its chat closes with it, a channel being deleted admits nothing.
    seed_applied(&mut window, Vec::new(), Vec::new());
    assert!(window.app.chat_start_attempt(window.card).is_none());
}

#[test]
fn a_first_message_sent_while_connecting_starts_once_the_daemon_has_no_registry() {
    let mut window = connecting_window();
    let start = send(&mut window);
    assert_held(&window, start);
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Unavailable(
                "this daemon has no shared workspace".into(),
            )),
    );
    assert_started_once(&mut window);
}

#[test]
fn a_first_message_sent_while_connecting_waits_for_the_registry_it_joins() {
    let mut window = connecting_window();
    let start = send(&mut window);
    assert_held(&window, start);
    let mut model = window.app.workspace_model.clone();
    model.identity = uuid::Uuid::new_v4();
    model.sequence += 1;
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Connected(model)),
    );
    // Joining seeds the channel, chat and all, in that registry; the message
    // waits for the seed's receipt rather than starting before the registry
    // has the pane.
    assert!(window.app.shared_views.seeding.contains(&window.scope));
    assert!(window.app.first_send_held(window.card));
    assert!(window.app.chat_start_attempt(window.card).is_none());
    let seeded = std::iter::from_fn(|| window.rx.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::SeedView { scope, panes, .. } if scope == window.scope => Some(panes),
            _ => None,
        })
        .expect("joining seeds the channel");
    assert!(seeded.iter().any(|pane| pane.id == window.card));

    seed_applied(&mut window, seeded, Vec::new());
    assert_started_once(&mut window);
}

#[test]
fn retry_loads_its_own_channel_while_another_channel_still_retries() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    let scope = window.scope.clone();
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedRefused {
                scope: scope.clone(),
                attempt: window.app.shared_views.seed_attempts[&scope],
                error: "The registry could not store these panes".into(),
            }),
    );
    assert!(window.app.chat_error(window.card).is_some());
    // Another channel's retry still awaits its receipt, which keeps the
    // notice's Retry for every refused channel unavailable meanwhile.
    let other = Scope::Channel(ChannelId::new());
    window.app.shared_views.seed_failures.insert(
        other.clone(),
        "The registry could not store these panes".into(),
    );
    window.app.shared_views.seeding.insert(other);
    assert!(!window.app.can_retry_view_seeds());

    drain(&window.rx);
    let save = window
        .app
        .handle_chat_ui(Action::RetryRefusedView(window.target));
    assert!(window.app.shared_views.seeding.contains(&scope));
    let start = complete(&mut window, save);
    assert_held(&window, start);
    let pane = Pane::from_card(window.app.card_state(window.card).unwrap(), None).unwrap();
    seed_applied(&mut window, vec![pane], Vec::new());
    assert_started_once(&mut window);
}

#[test]
fn a_held_message_waits_out_the_removal_of_its_worktree() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    let deleting = window.app.project_ui.begin_deleting(PathBuf::from("/repo"));
    assert!(window.app.deleting_card_reason(window.card).is_some());
    // The channel loads while the chat's worktree is being deleted: nothing
    // starts in a directory being emptied.
    seed_applied(&mut window, Vec::new(), Vec::new());
    assert!(window.app.first_send_held(window.card));
    assert!(window.app.chat_start_attempt(window.card).is_none());

    // The deletion fails, so the worktree stays and the message starts,
    // without waiting for an unrelated change to the shared workspace.
    let capture = window.app.capture_worktree_removal(&deleting);
    drop(window.app.update(Message::ProjectUi(
        crate::app::project_ui::Action::Deleted {
            target: deleting,
            capture,
            result: Err("the worktree has changes".into()),
        },
    )));
    assert!(window.app.deleting_card_reason(window.card).is_none());
    assert_started_once(&mut window);
}

#[test]
fn a_refusal_settled_by_the_workspace_notice_takes_its_retry_off_the_pane() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    let scope = window.scope.clone();
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedRefused {
                scope: scope.clone(),
                attempt: window.app.shared_views.seed_attempts[&scope],
                error: "The registry could not store these panes".into(),
            }),
    );
    assert!(window.app.chat_error(window.card).is_some());

    // The workspace notice's Retry loads every refused channel.
    window.app.retry_view_seeds();
    assert!(window.app.shared_views.seeding.contains(&scope));
    assert!(
        window.app.chat_error(window.card).is_some(),
        "the refusal stands until the new seed's receipt"
    );
    let pane = Pane::from_card(window.app.card_state(window.card).unwrap(), None).unwrap();
    seed_applied(&mut window, vec![pane], Vec::new());
    assert!(window.app.chat_error(window.card).is_none());
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message"),
        "the message waits in the composer for the user to send"
    );
    // No Retry is left to press.
    let retry = window
        .app
        .handle_chat_ui(Action::RetryRefusedView(window.target));
    assert_eq!(retry.units(), 0);
    assert!(window.app.chat_start_attempt(window.card).is_none());
}

#[test]
fn a_held_message_follows_the_conversation_another_window_started() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);

    // Another window, whose registry had the channel, sent its own first
    // message from the same shared draft: its own new conversation.
    let mut pane = Pane::from_card(window.app.card_state(window.card).unwrap(), None).unwrap();
    let CardKind::Chat(session) = &mut pane.kind else {
        unreachable!("the pane is a chat")
    };
    session.id = TerminalId::new();
    session.launch.session_id = Some("peer-conversation".into());
    assert!(!session.is_unstarted());
    let accepted = session.clone();
    seed_applied(&mut window, vec![pane], Vec::new());

    assert!(!window.app.first_send_held(window.card));
    assert!(
        window.app.chat_start_matches(window.card, &accepted),
        "the window connects to the running conversation, starting none"
    );
    assert_eq!(
        window.app.held_chat_draft(window.card),
        Some("first message"),
        "the message waits in that conversation's composer"
    );
    // No later pass starts the held message after all.
    drop(window.app.reconcile_shared_views());
    assert!(window.app.chat_start_matches(window.card, &accepted));
}

#[test]
fn a_held_message_never_starts_outside_the_channel_it_was_sent_in() {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    // While the message waits, its pane comes to be listed by another
    // channel, one already loaded.
    let Scope::Channel(channel) = window.scope.clone() else {
        unreachable!("the window's scope is its channel")
    };
    let other = ChannelId::new();
    let durable = Arc::make_mut(&mut window.app.durable);
    let mut moved = durable
        .channels
        .iter()
        .find(|candidate| candidate.id == channel)
        .unwrap()
        .clone();
    for candidate in &mut durable.channels {
        candidate.cards.retain(|card| *card != window.card);
    }
    moved.id = other;
    moved.cards = vec![window.card];
    durable.channels.push(moved);
    window.app.workspace_model.views.push(View::empty(other));
    window
        .app
        .shared_views
        .initialized
        .insert(Scope::Channel(other));
    drop(window.app.reconcile_shared_views());

    assert!(!window.app.first_send_held(window.card));
    // The pane is that channel's draft now; its message is not sent there
    // unasked.
    assert!(window.app.chat_start_attempt(window.card).is_none());
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
}

/// A message the registry refused for a channel still seeding.
fn refused_channel() -> Window {
    let mut window = seeding_channel();
    let start = send(&mut window);
    assert_held(&window, start);
    let scope = window.scope.clone();
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedRefused {
                scope: scope.clone(),
                attempt: window.app.shared_views.seed_attempts[&scope],
                error: "The registry could not store these panes".into(),
            }),
    );
    assert!(window.app.chat_error(window.card).is_some());
    window
}

#[test]
fn a_refused_pane_moved_to_another_channel_loses_the_refusal() {
    let mut window = refused_channel();
    let Scope::Channel(channel) = window.scope.clone() else {
        unreachable!("the window's scope is its channel")
    };
    let other = ChannelId::new();
    let durable = Arc::make_mut(&mut window.app.durable);
    let mut moved = durable
        .channels
        .iter()
        .find(|candidate| candidate.id == channel)
        .unwrap()
        .clone();
    for candidate in &mut durable.channels {
        candidate.cards.retain(|card| *card != window.card);
    }
    moved.id = other;
    moved.cards = vec![window.card];
    durable.channels.push(moved);
    window.app.workspace_model.views.push(View::empty(other));
    window
        .app
        .shared_views
        .initialized
        .insert(Scope::Channel(other));
    drop(window.app.reconcile_shared_views());

    assert!(window.app.chat_error(window.card).is_none());
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
    // No Retry is left: pressing it does nothing, neither offering the
    // channel the pane left nor sending the message.
    let retry = window
        .app
        .handle_chat_ui(Action::RetryRefusedView(window.target));
    assert_eq!(retry.units(), 0);
    assert!(!window.app.shared_views.seeding.contains(&window.scope));
    assert!(!window.app.first_send_held(window.card));
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
}

#[test]
fn retry_while_its_worktree_is_removed_keeps_the_refusal() {
    let mut window = refused_channel();
    let refusal = window.app.chat_error(window.card).unwrap().to_owned();
    window.app.project_ui.begin_deleting(PathBuf::from("/repo"));
    let retry = window
        .app
        .handle_chat_ui(Action::RetryRefusedView(window.target));
    assert_eq!(retry.units(), 0);
    assert!(!window.app.shared_views.seeding.contains(&window.scope));
    assert_eq!(window.app.chat_error(window.card), Some(refusal.as_str()));
    assert_eq!(
        window.app.chat_composer_text(window.card).as_deref(),
        Some("first message")
    );
}
