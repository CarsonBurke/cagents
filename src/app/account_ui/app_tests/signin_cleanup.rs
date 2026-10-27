use super::*;

#[test]
fn ended_offstrip_signin_waits_for_daemon_release_before_local_retirement() {
    let (mut app, card, original) = offstrip_close_fixture();
    app.retire_ended_terminal(card, 0, None, false);
    drop(app.update(Message::TerminalStarted {
        card,
        terminal: original.id,
        result: Err("fixture start already ended".into()),
    }));
    assert!(!app.terminal_start_in_flight(card));
    assert!(ended_terminal(app.card_state(card)).is_some());
    assert!(!app.workspace.order().any(|current| current == card));
    let key = app.account_ui.logins[0].key;
    let task = app.update(Message::AccountUi(Action::CancelLogin(key)));
    // Deliberately do not poll an unbound daemon operation. The synchronous
    // reducer may start a cleanup task, but cannot forget an unreleased PTY.
    assert!(
        app.card_state(card)
            .is_some_and(|state| state.existence.is_open()),
        "completed offstrip sign-in needs a daemon release receipt before retirement"
    );
    assert!(app.durable.card_scopes.contains_key(&card));
    assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Waiting);
    drop(task);
}
