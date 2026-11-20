//! One input, one projection: a shortcut or palette command that acts by
//! way of another message dispatches that message within its own update,
//! held to the same gates, rather than running a whole update of its own.
use super::terminal_output_tests::projections;
use super::*;
use crate::workspace::FocusDirection;
use keyboard::key::{Code, Named};
use workspace_widget::GlobalShortcut as Command;

/// Two terminal panes in different worktrees, the first focused, with what
/// they derive projected.
fn two_pane_app() -> (Cagents, CardId, CardId) {
    let (mut app, _) = tests::alt_tab_test_app();
    let first = app.durable.cards[0].id;
    let second = app.durable.cards[1].id;
    Arc::make_mut(&mut app.durable).focused = Some(first);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.cards = app
        .durable
        .cards
        .iter()
        .map(|card| (card.id, CardRuntime::Loading))
        .collect();
    drop(app.project_workspace(true));
    assert_eq!(app.workspace.focused(), Some(first));
    (app, first, second)
}

/// A shortcut pressed where the workspace surface takes it, as it takes
/// every global chord before the pane under it sees the key.
fn press(key: keyboard::Key, code: Code, modifiers: keyboard::Modifiers) -> Message {
    Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        },
        captured: true,
    }
}

fn alt_digit(digit: &str, code: Code) -> Message {
    press(
        keyboard::Key::Character(digit.into()),
        code,
        keyboard::Modifiers::ALT,
    )
}

fn ctrl_named(named: Named, code: Code) -> Message {
    press(keyboard::Key::Named(named), code, keyboard::Modifiers::CTRL)
}

fn escape(captured: bool) -> Message {
    Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Escape),
            modified_key: keyboard::Key::Named(Named::Escape),
            physical_key: keyboard::key::Physical::Code(Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        },
        captured,
    }
}

fn repository_app() -> (Cagents, CardId) {
    let (mut app, card, _) = two_pane_app();
    let runtime = RepositoryRuntime::dirty_fixture(card);
    app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    drop(app.project_workspace(true));
    (app, card)
}

/// Updates `app` with `message`, and answers how many times that projected
/// the whole workspace.
fn projections_of(app: &mut Cagents, message: Message) -> usize {
    let before = projections();
    drop(app.update(message));
    projections() - before
}

/// Switching panes by keyboard, the reader's most frequent shortcut,
/// projects the workspace once per key, and leaves nothing stale.
#[test]
fn a_pane_switching_shortcut_projects_the_workspace_once() {
    let (mut app, first, second) = two_pane_app();
    for (key, focused) in [
        (alt_digit("2", Code::Digit2), second),
        (alt_digit("1", Code::Digit1), first),
        (ctrl_named(Named::PageDown, Code::PageDown), second),
        (ctrl_named(Named::PageUp, Code::PageUp), first),
    ] {
        let label = format!("{key:?}");
        assert_eq!(projections_of(&mut app, key), 1, "{label}");
        assert_eq!(app.workspace.focused(), Some(focused), "{label}");
        projection_oracle::assert_projected(&mut app);
    }
}

// A shortcut that forwarded its message as a task would still project once
// here, as `projections_of` never runs the task; what each test below pins is
// that the effect happens within the key's own update.

#[test]
fn escape_closes_find_within_its_own_update() {
    let (mut app, card) = repository_app();
    drop(app.update(Message::Repository(RepositoryAction::OpenFind { card })));
    assert_eq!(app.focused_repository_card_with_find(), Some(card));
    assert_eq!(projections_of(&mut app, escape(false)), 1);
    assert_eq!(app.focused_repository_card_with_find(), None);
    projection_oracle::assert_projected(&mut app);
}

#[test]
fn escape_cancels_a_confirmation_within_its_own_update() {
    let cases: [(&str, fn() -> Cagents, fn(&Cagents) -> bool); 3] = [
        (
            "discarding changes",
            || {
                let mut app = tests::context_inspector_test_app(false);
                drop(app.update(Message::Sidebar(SidebarAction::RequestDiscard {
                    key: tests::discard_key(),
                    request: tests::discard_request(false),
                })));
                app
            },
            |app| app.sidebar_state.pending_discard().is_some(),
        ),
        (
            "closing an edited file",
            || {
                let (mut app, card) = tests::editing_repository_app();
                drop(app.update(Message::CloseCard(card)));
                app
            },
            |app| app.dirty_repository_close.is_some(),
        ),
        (
            "closing a window with work in it",
            || {
                let mut app = tests::context_inspector_test_app(false);
                app.transient_work_close = Some(window::Id::unique());
                app
            },
            |app| app.transient_work_close.is_some(),
        ),
    ];
    for (confirmation, open, is_open) in cases {
        let mut app = open();
        assert!(is_open(&app), "{confirmation}");
        assert_eq!(projections_of(&mut app, escape(false)), 1, "{confirmation}");
        assert!(!is_open(&app), "{confirmation}");
        projection_oracle::assert_projected(&mut app);
    }
}

#[test]
fn saving_by_shortcut_answers_within_its_own_update() {
    let (mut app, card) = repository_app();
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
        if runtime.save_error().is_none())
    );
    let save = press(
        keyboard::Key::Character("s".into()),
        Code::KeyS,
        keyboard::Modifiers::CTRL,
    );
    assert_eq!(projections_of(&mut app, save), 1);
    // This fixture has no original read root, so the save is refused at
    // once and writes no file.
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
        if runtime.save_error() == Some("The original read root is unavailable; reopen this file before saving."))
    );
    projection_oracle::assert_projected(&mut app);
}

/// Every command that stands for another message, chosen from the palette,
/// projects the workspace once, however the message it stands for answers.
#[test]
fn a_command_standing_for_another_message_projects_the_workspace_once() {
    for command in [
        Command::FocusPane2,
        Command::FocusAdjacent(FocusDirection::Right),
        Command::FocusAdjacent(FocusDirection::Left),
        Command::CyclePaneWidth,
        Command::ToggleInspector,
        Command::FocusProjectSearch,
        Command::OpenSettings,
        Command::CloseFocusedCard,
    ] {
        let (mut app, _, _) = two_pane_app();
        assert_eq!(
            projections_of(&mut app, Message::RunCommand(command)),
            1,
            "{command:?}"
        );
        projection_oracle::assert_projected(&mut app);
    }
}

/// A focus move made by a shortcut reconciles the sidebar's row selection
/// as the same move made by a click does: the one projection the shortcut
/// owes is the focus move's.
#[test]
fn a_focus_shortcut_owes_the_focus_moves_row_reconciliation() {
    let (mut app, _, _) = two_pane_app();
    let before = projections();
    // As a handler would, within its update.
    app.dispatching = 1;
    drop(app.dispatch(Message::RunCommand(Command::FocusPane2)));
    app.dispatching = 0;
    assert_eq!(projections(), before, "a dispatch projects nothing itself");
    let owed = app
        .owed_projection
        .take()
        .expect("the command owes a projection");
    assert_eq!(owed.projection, Projection::Full);
    assert!(owed.reconcile_row_selection);
}

/// A shortcut is held to the gates of the message it stands for, which its
/// own key event or command does not pass through: a focus move waits out
/// a save in the focused editor and an update being prepared.
#[test]
fn a_shortcut_is_held_to_the_gates_of_the_message_it_stands_for() {
    let (mut app, first, _) = two_pane_app();
    // The first pane becomes an editor whose save is in flight.
    let state = Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|state| state.id == first)
        .unwrap();
    state.kind = CardKind::Repository(crate::model::RepositoryView {
        project: ProjectId::new(),
        repository: "/repo/.git".into(),
        worktree: "/repo".into(),
        selected_file: None,
    });
    app.cards.insert(
        first,
        CardRuntime::Repository(Box::new(
            crate::repository_widget::RepositoryRuntime::post_persist_failure_fixture(first).0,
        )),
    );
    drop(app.project_workspace(true));
    assert!(app.focused_repository_is_saving());
    for message in [
        alt_digit("2", Code::Digit2),
        ctrl_named(Named::PageDown, Code::PageDown),
        Message::RunCommand(Command::FocusPane2),
        Message::RunCommand(Command::ToggleInspector),
    ] {
        let label = format!("{message:?}");
        drop(app.update(message));
        assert_eq!(app.workspace.focused(), Some(first), "{label}");
        assert!(!app.pane_inspectors.is_open(first), "{label}");
    }

    // A command is not itself refused while an update is prepared, but the
    // focus move it stands for is.
    let (mut app, first, _) = two_pane_app();
    app.update_prepared = true;
    assert!(!app.workspace_inputs_blocked());
    drop(app.update(Message::RunCommand(Command::FocusPane2)));
    assert_eq!(app.workspace.focused(), Some(first));
    app.update_prepared = false;
    drop(app.update(Message::RunCommand(Command::FocusPane2)));
    assert_ne!(app.workspace.focused(), Some(first));
}

/// Switching between panes that are all running only moves which of them
/// are on screen, which no projection reads: the sweep that follows the
/// switch projects nothing, and the oracle confirms nothing was left stale.
#[test]
fn a_sweep_that_only_moves_exposures_projects_nothing() {
    let (mut app, first, second) = two_pane_app();
    drop(app.workspace.resize_viewport(400.0));
    for card in [first, second] {
        app.register_presence(card, Exposure::Offscreen, Residency::Hot);
    }
    drop(app.project_workspace(true));
    assert_eq!(projections_of(&mut app, Message::LifecycleSweep), 0);
    assert_eq!(app.exposure(first), Exposure::Visible);
    projection_oracle::assert_projected(&mut app);
}

/// The timed sweep projects what its reconcile changed, as the sweep after a
/// pane switch does, and a timer with no sweep to run projects nothing.
#[test]
fn an_elapsed_sweep_projects_only_what_it_changed() {
    let (mut app, first, second) = two_pane_app();
    drop(app.workspace.resize_viewport(400.0));
    for card in [first, second] {
        app.register_presence(card, Exposure::Offscreen, Residency::Hot);
    }
    drop(app.project_workspace(true));
    let now = Instant::now();

    // Superseded by a later arming.
    app.lifecycle_sweep_generation = 2;
    app.lifecycle_sweep_deadline = Some(now);
    assert_eq!(
        projections_of(&mut app, Message::LifecycleSweepElapsed(1)),
        0
    );
    assert_eq!(app.exposure(first), Exposure::Offscreen, "nothing swept");

    // Woke before its deadline, so it waits again.
    let later = now + Duration::from_secs(60);
    app.lifecycle_sweep_deadline = Some(later);
    assert_eq!(
        projections_of(&mut app, Message::LifecycleSweepElapsed(2)),
        0
    );
    assert_eq!(app.lifecycle_sweep_generation, 3, "re-armed");
    assert_eq!(app.lifecycle_sweep_deadline, Some(later));
    assert_eq!(app.exposure(first), Exposure::Offscreen, "nothing swept");

    // Due, and its reconcile only moves exposures.
    app.lifecycle_sweep_deadline = Some(now);
    assert_eq!(
        projections_of(&mut app, Message::LifecycleSweepElapsed(3)),
        0
    );
    assert_ne!(app.lifecycle_sweep_deadline, Some(now), "the sweep ran");
    assert_eq!(app.exposure(first), Exposure::Visible);
    projection_oracle::assert_projected(&mut app);
}

/// A helper that dispatches, driven where no update is running to project
/// what it owes, runs a whole update instead and leaves nothing owed.
#[test]
fn a_dispatch_outside_an_update_projects_as_an_update() {
    let (mut app, _, second) = two_pane_app();
    let before = projections();
    drop(app.dispatch(Message::FocusCard(second)));
    assert_eq!(projections() - before, 1);
    assert_eq!(app.owed_projection, None);
    assert_eq!(app.workspace.focused(), Some(second));
    projection_oracle::assert_projected(&mut app);
}

/// Bringing a card up is read by projections, so a sweep that hydrates one
/// still projects the workspace, once.
#[test]
fn a_sweep_that_brings_a_card_up_projects_once() {
    let (mut app, first, second) = two_pane_app();
    drop(app.workspace.resize_viewport(400.0));
    app.register_presence(first, Exposure::Offscreen, Residency::Cold);
    app.register_presence(second, Exposure::Offscreen, Residency::Hot);
    drop(app.project_workspace(true));
    assert_eq!(projections_of(&mut app, Message::LifecycleSweep), 1);
    assert!(!matches!(app.residency(first), Residency::Cold));
    projection_oracle::assert_projected(&mut app);
}

/// What two dispatched messages owe together is never narrower than what
/// either owes alone.
#[test]
fn owed_projections_merge_to_what_both_need() {
    let card = CardId::new();
    let other = CardId::new();
    let owed = |projection, camera_only, reconcile_row_selection| OwedProjection {
        projection,
        camera_only,
        reconcile_row_selection,
        model_update: false,
    };
    let scoped = |card| Projection::ScopedChrome(vec![card]);
    for (first, second, merged) in [
        (
            owed(Projection::Unchanged, false, false),
            owed(Projection::Chrome, false, true),
            owed(Projection::Chrome, false, true),
        ),
        (
            owed(scoped(card), false, false),
            owed(scoped(card), false, false),
            owed(scoped(card), false, false),
        ),
        (
            owed(scoped(card), false, false),
            owed(scoped(other), false, false),
            owed(Projection::Full, false, false),
        ),
        (
            owed(Projection::Chrome, false, false),
            owed(Projection::TranscriptProbes, false, false),
            owed(Projection::Full, false, false),
        ),
        // Moving the camera twice, or once beside nothing, still only
        // moves the camera.
        (
            owed(Projection::Full, true, false),
            owed(Projection::Unchanged, false, false),
            owed(Projection::Full, true, false),
        ),
        (
            owed(Projection::Full, true, false),
            owed(Projection::Full, true, false),
            owed(Projection::Full, true, false),
        ),
        // The camera beside anything else owes the whole projection.
        (
            owed(Projection::Full, true, false),
            owed(Projection::Chrome, false, false),
            owed(Projection::Full, false, false),
        ),
        (
            owed(Projection::Full, true, true),
            owed(Projection::Full, false, false),
            owed(Projection::Full, false, true),
        ),
    ] {
        let label = format!("{first:?} + {second:?}");
        assert_eq!(first.clone().merge(second.clone()), merged, "{label}");
        assert_eq!(second.merge(first), merged, "{label} reversed");
    }
}

/// A message a handler dispatches within its update counts for attention as
/// the update's own message does: one that may change the projects and
/// worktrees attention covers drops a pending notification activation, so
/// the activation is tried again against the new scope.
#[test]
fn a_nested_message_that_changes_attention_scope_resets_the_activation() {
    for (message, changes_scope) in [
        (Message::GitUi(git_ui::Action::Changed), true),
        (Message::RunCommand(Command::FocusPane2), false),
    ] {
        let (mut app, _, _) = two_pane_app();
        app.notification_ui
            .attempt_activation_for_test(&app.durable);
        // As a handler would, within its update, which then finishes.
        app.dispatching = 1;
        let task = app.dispatch(message);
        app.dispatching = 0;
        drop(app.finish_update(
            task,
            UpdateEntry {
                notice_control: None,
                timed: false,
                settled_projection: Task::none(),
                oracle_message: None,
            },
        ));
        assert_eq!(
            !app.notification_ui.activation_attempted_for_test(),
            changes_scope,
            "changes scope: {changes_scope}"
        );
        assert!(
            !app.attention_scope_changed,
            "the update consumed the change"
        );
    }
}

/// Admission can run a whole repository reconciliation update before the
/// message's handler. That update must not answer for the message's scope
/// change: an activation retried within it still sees the old scope.
#[test]
fn an_update_run_during_admission_leaves_the_message_its_scope_change() {
    let (mut app, _, _) = two_pane_app();
    app.pending_repository_reconciliation
        .push(PathBuf::from("/nonexistent/repository"));
    app.dispatching = 1;
    let task = app.dispatch(Message::GitUi(git_ui::Action::Changed));
    app.dispatching = 0;
    assert!(
        app.pending_repository_reconciliation.is_empty(),
        "admission reconciled first"
    );
    assert!(
        app.attention_scope_changed,
        "the reconciliation's update kept the message's change"
    );
    // As an activation retried during the reconciliation would.
    app.notification_ui
        .attempt_activation_for_test(&app.durable);
    drop(app.finish_update(
        task,
        UpdateEntry {
            notice_control: None,
            timed: false,
            settled_projection: Task::none(),
            oracle_message: None,
        },
    ));
    assert!(!app.notification_ui.activation_attempted_for_test());
}
