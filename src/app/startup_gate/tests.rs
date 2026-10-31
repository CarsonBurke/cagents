use super::*;

const DETAIL: &str = "Upgrade restore retained original ownership: terminal dimensions or scrollback are outside supported bounds";

/// An opened, handshaking window whose saved terminals are all still
/// attaching, after the gate's first check has recorded them.
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
    drop(app.verify_startup_gate());
    assert_eq!(
        app.startup_gate
            .pending
            .as_ref()
            .map(|pending| &pending.panes),
        Some(&cards)
    );
    (app, cards)
}

/// Moves the gate's clock back instead of sleeping through its bounds.
fn elapse(app: &mut Cagents, by: Duration) {
    let gate = &mut app.startup_gate;
    let back = |at: Instant| at.checked_sub(by).unwrap();
    gate.opened = gate.opened.map(back);
    gate.progressed = gate.progressed.map(back);
    gate.inspected = gate.inspected.map(back);
}

fn reported(app: &Cagents) -> Option<&str> {
    app.startup_gate
        .reported
        .then(|| app.status.as_ref().unwrap().message.as_str())
}

#[test]
fn retained_daemon_recovery_reports_its_reason_at_once_and_keeps_the_panes() {
    let (mut app, cards) = waiting(3);
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 2 })));
    drop(app.verify_startup_gate());
    assert_eq!(
        reported(&app),
        None,
        "a restore in progress is not a failure"
    );

    drop(app.startup_state_inspected(Ok(StartupState::Retained {
        detail: DETAIL.into(),
    })));
    let reason = reported(&app).expect("retained recovery must answer the launcher");
    // Launchers keep the window open only for this error form.
    assert!(
        reason.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX),
        "{reason}"
    );
    assert!(reason.contains(DETAIL), "{reason}");
    assert!(reason.contains("this window remains available"), "{reason}");
    for card in cards {
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    }
    // The gate answers once; later checks and answers change nothing.
    let reason = reason.to_owned();
    drop(app.verify_startup_gate());
    drop(app.startup_state_inspected(Ok(StartupState::Ready)));
    assert_eq!(reported(&app), Some(reason.as_str()));
}

#[test]
fn retained_reason_fits_one_launcher_handshake_line() {
    let detail = "Restore failed: ünïcödé\nsecond line ".repeat(500);
    let line = format!(
        "{}{}",
        instance::UI_LAUNCH_ERROR_PREFIX,
        retained_reason(&detail)
    );
    // The launcher refuses a response longer than 4 KiB, which would close
    // the recovery window it should keep.
    assert!(line.len() < 4096, "{}", line.len());
    assert!(!line.contains('\n'));
    assert!(line.contains("Restore failed: ünïcödé second line"));
}

#[test]
fn panes_without_progress_fail_before_the_inventory_budget_once_the_daemon_is_ready() {
    // Three panes give a 120 second inventory budget.
    let (mut app, cards) = waiting(3);
    drop(app.startup_state_inspected(Ok(StartupState::Ready)));
    elapse(&mut app, NO_PROGRESS - Duration::from_secs(1));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    elapse(&mut app, Duration::from_secs(2));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("a stalled gate must answer the launcher");
    assert!(
        reason.starts_with(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX),
        "{reason}"
    );
    assert!(reason.contains("made no progress"), "{reason}");
    assert!(reason.contains(&cards[0].to_string()), "{reason}");
}

#[test]
fn settling_panes_or_a_restoring_daemon_keep_the_gate_waiting() {
    // Ten panes give a 330 second inventory budget.
    let (mut app, cards) = waiting(10);
    drop(app.startup_state_inspected(Ok(StartupState::Ready)));

    // One pane settling is progress, even though it failed.
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    app.cards.insert(
        cards[0],
        CardRuntime::Failed("Fixture attach failed".into()),
    );
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    // The panes cannot move while the daemon is still restoring...
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 3 })));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);
    // ...and its finishing is progress in its own right.
    drop(app.startup_state_inspected(Ok(StartupState::Ready)));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    // A daemon that cannot answer is not evidence of a stall.
    elapse(&mut app, NO_PROGRESS + Duration::from_secs(1));
    drop(app.startup_state_inspected(Err("daemon socket connection deadline elapsed".into())));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);
}

#[test]
fn the_inventory_budget_still_caps_a_daemon_that_keeps_restoring() {
    let (mut app, _) = waiting(1);
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 1 })));
    elapse(&mut app, Duration::from_secs(59));
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 1 })));
    drop(app.verify_startup_gate());
    assert_eq!(reported(&app), None);

    elapse(&mut app, Duration::from_secs(1));
    drop(app.startup_state_inspected(Ok(StartupState::Restoring { awaiting: 1 })));
    drop(app.verify_startup_gate());
    let reason = reported(&app).expect("the absolute cap still applies");
    assert!(reason.contains("still reconnecting"), "{reason}");
}
