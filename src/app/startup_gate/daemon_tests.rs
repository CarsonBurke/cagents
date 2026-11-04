//! What the daemon says about its startup recovery decides how long a reopened
//! window waits on panes it cannot settle by itself.
use super::*;

const DETAIL: &str = "Upgrade restore retained original ownership: terminal dimensions or scrollback are outside supported bounds";

/// An opened, handshaking window whose saved terminals are all still attaching,
/// after the gate's first check has recorded them.
fn waiting(panes: usize) -> (Cagents, Vec<CardId>) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut cards = Vec::new();
    for _ in 0..panes {
        let card = crate::app::tests::terminal_card_state();
        app.cards.insert(card.id, CardRuntime::Loading);
        cards.push(card.id);
        Arc::make_mut(&mut app.durable).cards.push(card);
    }
    app.startup_gate = State::fixture_opened();
    assert!(!app.startup_stalled());
    assert_eq!(app.startup_gate.waiting_on, cards);
    (app, cards)
}

/// Moves the stall clock back instead of sleeping through it.
fn no_progress_for(app: &mut Cagents, by: Duration) {
    let gate = &mut app.startup_gate;
    gate.progressed = gate.progressed.map(|at| at - by);
}

#[test]
fn a_retained_daemon_is_reported_at_once_with_its_reason() {
    let (mut app, _) = waiting(2);
    drop(app.startup_state_inspected(Ok(StartupState::Retained {
        detail: DETAIL.into(),
    })));
    assert!(app.startup_gate.reported);
    let notice = app.status.as_ref().expect("the window explains itself");
    assert_eq!(notice.message, RETAINED_NOTICE);
    assert_eq!(notice.details.as_deref(), Some(DETAIL));
}

#[test]
fn the_launcher_report_is_one_bounded_line_that_keeps_the_window_open() {
    let raw = format!("restore failed\n\t{}", "\u{591a}\u{5b57} ".repeat(4000));
    let reason = retained_reason(&raw);
    assert!(reason.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX));
    assert!(reason.contains("restore failed"));
    assert!(reason.ends_with("The update has not been verified."));
    assert!(reason.len() < 4096 - 128, "{}", reason.len());
    assert!(!reason.contains(['\r', '\n', '\t']));
}

#[test]
fn a_pane_refused_while_the_daemon_restores_is_not_counted_against_it() {
    let (mut app, cards) = waiting(1);
    let terminal = match &app.card_state(cards[0]).unwrap().kind {
        CardKind::Terminal(session) => session.id,
        _ => unreachable!(),
    };
    app.startup_attach_failed(cards[0], terminal, "restoring".into());
    app.startup_gate.retries.expire_window();
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 1 })));
    assert_eq!(app.startup_pane_check(), Ok(false));

    drop(app.startup_state_inspected(Ok(StartupState::Ready)));
    assert_eq!(app.startup_pane_check(), Ok(false));
    app.startup_attach_failed(cards[0], terminal, "refused for good".into());
    app.startup_gate.retries.expire_window();
    assert!(
        app.startup_pane_check()
            .unwrap_err()
            .contains("refused for good")
    );
}

#[test]
fn panes_that_stop_changing_on_a_ready_daemon_are_reported() {
    let (mut app, _) = waiting(2);
    no_progress_for(&mut app, NO_PROGRESS - Duration::from_secs(1));
    assert!(!app.startup_stalled());
    no_progress_for(&mut app, Duration::from_secs(1));
    assert!(app.startup_stalled());
}

#[test]
fn a_pane_settling_is_progress() {
    let (mut app, cards) = waiting(2);
    no_progress_for(&mut app, NO_PROGRESS);
    app.cards.remove(&cards[0]);
    Arc::make_mut(&mut app.durable)
        .cards
        .retain(|card| card.id != cards[0]);
    assert!(!app.startup_stalled());
    assert_eq!(app.startup_gate.waiting_on, &cards[1..]);
}

#[test]
fn only_time_the_daemon_reports_as_ready_counts_toward_a_stall() {
    let (mut app, _) = waiting(1);
    no_progress_for(&mut app, NO_PROGRESS);
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 1 })));
    assert!(!app.startup_stalled());

    no_progress_for(&mut app, NO_PROGRESS);
    assert!(!app.startup_stalled(), "the daemon is still restoring");
    drop(app.startup_state_inspected(Ok(StartupState::Ready)));
    assert!(!app.startup_stalled(), "its finishing is progress");

    no_progress_for(&mut app, NO_PROGRESS);
    drop(app.startup_state_inspected(Err("connection refused".into())));
    assert!(
        !app.startup_stalled(),
        "without an answer only the budget applies"
    );
}

#[test]
fn a_settled_window_waits_briefly_for_the_daemons_first_answer() {
    let mut gate = State {
        opened: Some(Instant::now()),
        ..State::default()
    };
    assert!(!gate.daemon_heard());
    gate.opened = gate.opened.map(|at| at - FIRST_ANSWER_GRACE);
    assert!(gate.daemon_heard(), "a hung daemon cannot hold the window");

    let (mut app, _) = waiting(0);
    app.startup_gate.daemon = None;
    assert!(!app.startup_gate.daemon_heard());
    drop(app.startup_state_inspected(Err("connection refused".into())));
    assert!(app.startup_gate.daemon_heard(), "a remote window goes on");
}

#[test]
fn an_answer_after_the_gate_reported_changes_nothing() {
    let (mut app, _) = waiting(1);
    app.startup_gate.reported = true;
    drop(app.startup_state_inspected(Ok(StartupState::Retained {
        detail: DETAIL.into(),
    })));
    assert!(app.status.is_none());
}

#[test]
fn only_one_inspection_runs_at_a_time() {
    let (mut app, _) = waiting(1);
    drop(app.inspect_daemon_startup());
    assert!(app.startup_gate.inspecting);
    let first = app.startup_gate.inspected;
    app.startup_gate.inspected = first.map(|at| at - INSPECT_EVERY);
    let moved = app.startup_gate.inspected;
    drop(app.inspect_daemon_startup());
    assert_eq!(app.startup_gate.inspected, moved, "one is still running");

    drop(app.startup_state_inspected(Ok(StartupState::Ready)));
    drop(app.inspect_daemon_startup());
    assert!(app.startup_gate.inspecting);
    assert_ne!(app.startup_gate.inspected, moved);
}
