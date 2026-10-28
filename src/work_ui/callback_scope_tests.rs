// Included inside the owning State tests. Every refresh origin comes from an
// actual admitted Effect, never a fabricated ticket or a field replacement.
fn admitted_refresh(state: &mut State) -> RefreshTicket {
    let Some(Effect::Refresh { ticket }) = state.update(Action::Refresh) else {
        panic!("an idle pane must admit the user's refresh")
    };
    ticket
}

fn delivered_refresh(state: &mut State, ticket: RefreshTicket, result: Result<(), String>) {
    let action = Action::Refreshed { ticket, result };
    assert!(
        action.background(),
        "issued work must settle behind a modal"
    );
    assert!(
        state.update(action).is_none(),
        "a reply cannot retry backend work"
    );
}

#[test]
fn late_refresh_cannot_settle_a_reopened_board_or_its_current_refresh() {
    for old_failed in [false, true] {
        for current_failed in [false, true] {
            let mut state = State::default();
            connected(&mut state, 90, board(vec![item("old-pane", "cagents")]));
            let old_owner = state.watch_owner();
            let first = admitted_refresh(&mut state);
            assert_eq!(state.refreshing, Some(first.clone()));
            // Closing the Work card calls this real reset. Reopening installs
            // its own first board before the second explicit refresh is sent.
            state.reset();
            assert_ne!(state.watch_owner(), old_owner);
            assert!(state.board.is_none());
            let fresh = board(vec![item("reopened-pane", "cagents")]);
            connected(&mut state, 1, fresh.clone());
            let second = admitted_refresh(&mut state);
            assert_ne!(second, first);
            let late = if old_failed {
                Err("old pane refresh failed".into())
            } else {
                Ok(())
            };
            delivered_refresh(&mut state, first.clone(), late);
            assert_eq!(
                state.refreshing,
                Some(second.clone()),
                "F1 must not clear F2's busy state"
            );
            assert!(
                state.error.is_none(),
                "F1 failure does not belong to the reopened pane"
            );
            assert_eq!(state.board.as_deref(), Some(fresh.as_ref()));
            assert_eq!(
                state.visible().map(Item::label).collect::<Vec<_>>(),
                ["reopened-pane"]
            );
            delivered_refresh(
                &mut state,
                second,
                if current_failed {
                    Err("current pane refresh failed".into())
                } else {
                    Ok(())
                },
            );
            assert!(
                state.refreshing.is_none(),
                "the matching F2 reply still settles"
            );
            assert_eq!(
                state.error.as_deref(),
                current_failed.then_some("current pane refresh failed")
            );
            delivered_refresh(&mut state, first, Err("even later old pane error".into()));
            assert!(state.refreshing.is_none());
            assert_eq!(
                state.error.as_deref(),
                current_failed.then_some("current pane refresh failed")
            );
            assert_eq!(state.board.as_deref(), Some(fresh.as_ref()));
        }
    }
}

#[test]
fn refresh_operation_identity_preserves_coalescing_and_matching_completion_in_one_pane() {
    let mut state = State::default();
    connected(&mut state, 1, board(vec![item("current", "cagents")]));
    let owner = state.watch_owner();
    let first = admitted_refresh(&mut state);
    assert!(
        state.update(Action::Refresh).is_none(),
        "pending refresh coalesces locally"
    );
    assert_eq!(state.refreshing, Some(first.clone()));
    delivered_refresh(&mut state, first.clone(), Err("first current error".into()));
    assert!(state.refreshing.is_none());
    assert_eq!(state.error.as_deref(), Some("first current error"));
    let second = admitted_refresh(&mut state);
    assert_eq!(state.watch_owner(), owner);
    assert_ne!(
        second, first,
        "same pane is not the same admitted operation"
    );
    assert!(
        state.error.is_none(),
        "a fresh explicit attempt clears its earlier error"
    );
    for result in [Ok(()), Err("duplicate first error".into())] {
        delivered_refresh(&mut state, first.clone(), result);
        assert_eq!(state.refreshing, Some(second.clone()));
        assert!(state.error.is_none());
    }
    delivered_refresh(&mut state, second.clone(), Ok(()));
    assert!(state.refreshing.is_none());
    assert!(state.error.is_none());
    delivered_refresh(&mut state, second, Err("duplicate completed reply".into()));
    assert!(
        state.error.is_none(),
        "an already settled operation cannot report a new failure"
    );
}

#[test]
fn old_watch_board_and_loss_cannot_reopen_or_replace_a_new_pane_or_retire_its_pin() {
    let mut state = State::default();
    connected(&mut state, 30, board(vec![item("old", "cagents")]));
    let old = state.watch_owner();
    state.reset();
    let current = state.watch_owner();
    assert_ne!(old, current);
    let retired = board(vec![item("retired-watch", "cagents")]);
    for watch in [
        Watch::Board {
            revision: 10_000,
            board: retired.clone(),
            first: true,
        },
        Watch::Lost("retired watch disconnected".into()),
    ] {
        assert!(
            state
                .update(Action::Watched { owner: old, watch })
                .is_none()
        );
        assert!(
            state.board.is_none(),
            "a queued old first board cannot reopen a reset pane"
        );
        assert!(state.lost.is_none());
    }
    let fresh = board(vec![item("fresh", "cagents")]);
    connected(&mut state, 1, fresh.clone());
    let card = CardRef::of(&fresh.items[0]);
    let pending_pin = pin(&mut state, &card, Some(Column::Active));
    let pin_lifetime = state.pins.lifetime;
    let optimistic = state.board.clone();
    let pending_refresh = admitted_refresh(&mut state);
    for watch in [
        Watch::Board {
            revision: 10_000,
            board: retired.clone(),
            first: false,
        },
        Watch::Board {
            revision: 1,
            board: retired,
            first: true,
        },
        Watch::Lost("late old watch loss".into()),
    ] {
        assert!(
            state
                .update(Action::Watched { owner: old, watch })
                .is_none()
        );
        assert_eq!(state.board, optimistic);
        assert!(state.lost.is_none());
        assert_eq!(state.pins.lifetime, pin_lifetime);
        assert_eq!(state.pins.latest.get(&card), Some(&pending_pin.operation));
        assert_eq!(state.refreshing, Some(pending_refresh.clone()));
    }
    // Rejected old observer events cannot cancel the current pin's callback.
    complete_pin(&mut state, pending_pin, Err("current pin refusal".into()));
    assert_eq!(state.item(&card).unwrap().column, Column::InProgress);
    assert!(
        state
            .pins
            .error
            .as_deref()
            .unwrap()
            .contains("current pin refusal")
    );
    delivered_refresh(&mut state, pending_refresh, Ok(()));
    assert!(state.refreshing.is_none());
    // Current owned watch callbacks still accept connection loss and a fresh
    // first board whose daemon revision starts over.
    assert!(
        state
            .update(Action::Watched {
                owner: current,
                watch: Watch::Lost("current connection lost".into())
            })
            .is_none()
    );
    assert_eq!(state.lost.as_deref(), Some("current connection lost"));
    let reconnected = board(vec![item("current-reconnect", "cagents")]);
    connected(&mut state, 1, reconnected.clone());
    assert_eq!(
        state.watch_owner(),
        current,
        "reconnecting does not replace this pane's owner"
    );
    assert_eq!(state.board.as_deref(), Some(reconnected.as_ref()));
    assert!(state.lost.is_none());
}
