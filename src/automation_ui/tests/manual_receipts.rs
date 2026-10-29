use super::*;
use crate::automations::{FiringState, HistoryPage, Trigger};
use std::sync::Arc;

fn request(effect: Option<Effect>) -> (u64, Request) {
    let Some(Effect::Request { sequence, request }) = effect else {
        panic!("an actual reducer request is required")
    };
    (sequence, request)
}

fn receive(state: &mut State, sequence: u64, response: Response) -> Option<Effect> {
    state.update(Action::Received {
        sequence,
        result: Arc::new(Ok(response)),
    })
}

fn selected_empty() -> (State, Automation) {
    let selected = automation();
    let mut state = State::default();
    let (sequence, show) = request(state.update(Action::Select(selected.id)));
    assert_eq!(show, Request::Show { id: selected.id });
    let (sequence, history) = request(receive(
        &mut state,
        sequence,
        Response::Automation(selected.clone()),
    ));
    assert_eq!(
        history,
        Request::History {
            id: Some(selected.id),
            before: None,
            limit: PAGE
        }
    );
    assert!(
        receive(
            &mut state,
            sequence,
            Response::History(HistoryPage {
                firings: Vec::new(),
                next: None,
            })
        )
        .is_none()
    );
    assert_eq!(state.history_status, HistoryStatus::Loaded);
    (state, selected)
}

fn confirmed_run(state: &mut State, selected: &Automation) -> (u64, Request, Uuid) {
    assert!(
        state
            .update(Action::RunNow {
                id: selected.id,
                revision: selected.revision,
            })
            .is_none()
    );
    let confirmation = state.confirmation.as_ref().expect("public confirmation");
    assert_eq!(confirmation.automation, *selected);
    let generation = confirmation.generation;
    let (sequence, run) = request(state.update(Action::Confirm(generation)));
    let Request::RunNow {
        id,
        execution_revision,
        request: key,
        confirmed,
    } = &run
    else {
        panic!("exact manual run request")
    };
    assert_eq!(*id, selected.id);
    assert_eq!(*execution_revision, selected.execution_revision);
    assert!(*confirmed);
    (sequence, run.clone(), *key)
}

fn lost_manual(state: &mut State, selected: &Automation) -> (u64, Request, Uuid) {
    let (sequence, original, key) = confirmed_run(state, selected);
    assert!(
        state
            .update(Action::Received {
                sequence,
                result: Arc::new(Err("fixture: original RunNow reply lost after send".into())),
            })
            .is_none()
    );
    assert!(state.pending.is_none());
    assert_eq!(
        state.manual_requests.get(&selected.id),
        Some(&(selected.execution_revision, key))
    );
    (sequence, original, key)
}

fn poll_history(state: &mut State, selected: &Automation, page: HistoryPage) {
    let (sequence, show) = request(state.update(Action::Tick));
    assert_eq!(show, Request::Show { id: selected.id });
    let (sequence, query) = request(receive(
        state,
        sequence,
        Response::Automation(selected.clone()),
    ));
    assert_eq!(
        query,
        Request::History {
            id: Some(selected.id),
            before: None,
            limit: PAGE
        }
    );
    assert!(receive(state, sequence, Response::History(page)).is_none());
    assert!(state.pending.is_none());
    assert_eq!(state.history_status, HistoryStatus::Loaded);
}

fn firing(selected: &Automation, id: u128, created_ms: u64, key: Uuid) -> Firing {
    let mut value = history_firing(created_ms, FiringState::Succeeded);
    value.id = Uuid::from_u128(id);
    value.automation = selected.id;
    value.revision = selected.execution_revision;
    value.trigger = Trigger::Manual { request: key };
    value.started_ms = Some(created_ms);
    value.finished_ms = Some(created_ms + 1);
    value
}

#[test]
fn polled_manual_retry_receipt_reconciles_one_row_without_moving_newer_history() {
    let (mut state, selected) = selected_empty();
    let (_, original, key) = lost_manual(&mut state, &selected);
    let original_firing = firing(&selected, 100, 100, key);
    let mut newer = firing(&selected, 200, 200, Uuid::new_v4());
    newer.state = FiringState::Failed;
    poll_history(
        &mut state,
        &selected,
        HistoryPage {
            firings: vec![newer.clone(), original_firing.clone()],
            next: None,
        },
    );
    assert_eq!(
        state.manual_requests.get(&selected.id),
        Some(&(selected.execution_revision, key))
    );
    let cursor = state.history_next.clone();
    let history_newer = state.history_newer;
    let (sequence, retry, retry_key) = confirmed_run(&mut state, &selected);
    assert_eq!(
        retry, original,
        "an uncertain retry must keep the exact original request"
    );
    assert_eq!(retry_key, key);
    assert!(
        receive(
            &mut state,
            sequence,
            Response::Firing(original_firing.clone())
        )
        .is_none()
    );
    assert_eq!(
        state.history.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![newer.id, original_firing.id],
        "an idempotent receipt must reconcile the polled firing once without moving it ahead of newer history"
    );
    assert_eq!(state.history, vec![newer, original_firing]);
    assert_eq!(state.history_next, cursor);
    assert_eq!(state.history_newer, history_newer);
    assert!(!state.manual_requests.contains_key(&selected.id));
    assert!(state.pending.is_none());
    assert_eq!(
        state.rows[0].last_state,
        Some(FiringState::Failed),
        "older receipt preserves latest outcome"
    );
}

#[test]
fn absent_manual_retry_receipt_uses_backend_creation_order_and_keeps_page_cursor() {
    for (created_ms, id, expected_index) in [
        (1100, 50, 0),
        (975, 5000, 25),
        (975, 10, 26),
        (900, 50, PAGE),
    ] {
        let (mut state, selected) = selected_empty();
        let (_, original, key) = lost_manual(&mut state, &selected);
        let page: Vec<_> = (0..PAGE)
            .map(|index| {
                let created = 1000 - index as u64;
                firing(&selected, created as u128, created, Uuid::new_v4())
            })
            .collect();
        let cursor = Some(history_cursor(history_key(page.last().unwrap())));
        poll_history(
            &mut state,
            &selected,
            HistoryPage {
                firings: page.clone(),
                next: cursor.clone(),
            },
        );
        let history_newer = state.history_newer;
        let receipt = firing(&selected, id, created_ms, key);
        assert!(!state.history.iter().any(|row| row.id == receipt.id));
        let (sequence, retry, _) = confirmed_run(&mut state, &selected);
        assert_eq!(retry, original);
        assert!(receive(&mut state, sequence, Response::Firing(receipt.clone())).is_none());
        let mut expected = page;
        expected.insert(expected_index, receipt);
        assert_eq!(
            state.history, expected,
            "an absent explicit receipt must preserve descending creation time and UUID order"
        );
        assert_eq!(
            state.history_next, cursor,
            "an explicit receipt cannot rewrite pagination custody"
        );
        assert_eq!(state.history_newer, history_newer);
        assert_eq!(
            state.history.len(),
            PAGE + 1,
            "retain the existing explicit-receipt window policy"
        );
        assert!(!state.manual_requests.contains_key(&selected.id));
    }
}

#[test]
fn current_manual_receipt_updates_release_metadata_and_ignores_superseded_reads() {
    let (mut state, selected) = selected_empty();
    let (old_sequence, original, key) = lost_manual(&mut state, &selected);
    let mut held = firing(&selected, 100, 100, key);
    held.release_pending = true;
    let mut owning = selected.clone();
    owning.active_firing = Some(held.id);
    poll_history(
        &mut state,
        &owning,
        HistoryPage {
            firings: vec![held.clone()],
            next: None,
        },
    );
    assert!(state.history[0].release_pending);

    // The backend's finish_automation_release clears both fields. Show lands
    // first, while the old loaded row remains until its background History.
    let (sequence, show) = request(state.update(Action::Tick));
    assert_eq!(show, Request::Show { id: selected.id });
    let (superseded, query) = request(receive(
        &mut state,
        sequence,
        Response::Automation(selected.clone()),
    ));
    assert!(
        matches!(query, Request::History { id: Some(id), before: None, .. } if id == selected.id)
    );
    assert!(state.pending.unwrap().1.background_read());
    let (sequence, retry, _) = confirmed_run(&mut state, &selected);
    assert_eq!(retry, original);
    assert!(
        receive(
            &mut state,
            superseded,
            Response::History(HistoryPage {
                firings: vec![held.clone()],
                next: None,
            })
        )
        .is_none()
    );
    assert!(matches!(state.pending, Some((current, Pending::Mutate)) if current == sequence));
    let mut released = held.clone();
    released.release_pending = false;
    assert!(receive(&mut state, sequence, Response::Firing(released.clone())).is_none());
    assert_eq!(
        state.history,
        vec![released.clone()],
        "the current receipt must refresh the exact loaded firing's mutable release metadata"
    );
    assert!(!state.manual_requests.contains_key(&selected.id));
    let notice = state.notice.clone();
    for stale in [old_sequence, superseded] {
        assert!(receive(&mut state, stale, Response::Firing(held.clone())).is_none());
        assert_eq!(state.history, vec![released.clone()]);
        assert_eq!(state.notice, notice);
        assert!(state.pending.is_none());
    }
}
