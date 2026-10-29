use super::*;
use std::sync::Arc;

fn controls(owner: &Automation) -> [Action; 3] {
    [enable(owner), run_now(owner), remove(owner)]
}

fn request(effect: Option<Effect>) -> (u64, Request) {
    let Some(Effect::Request { sequence, request }) = effect else {
        panic!("an actual reducer request is required")
    };
    (sequence, request)
}

#[test]
fn queued_definition_actions_cannot_consume_successor_work_or_retarget_consent() {
    let a = automation();
    let mut b = a.clone();
    b.id = Uuid::new_v4();
    let mut revised = a.clone();
    revised.revision += 1;
    for successor in [&b, &revised] {
        for (old, fresh) in controls(&a).into_iter().zip(controls(successor)) {
            let mut state = State::default();
            select_control_definition(&mut state, &a);
            select_control_definition(&mut state, successor);

            // Establish an actual uncertain manual receipt before replaying
            // the old control. Effects are inspected, never executed.
            assert!(state.update(run_now(successor)).is_none());
            let generation = state.confirmation.as_ref().unwrap().generation;
            let (sequence, original) = request(state.update(Action::Confirm(generation)));
            assert!(
                state
                    .update(Action::Received {
                        sequence,
                        result: Arc::new(Err("fixture: manual reply lost after send".into())),
                    })
                    .is_none()
            );
            let receipts = state.manual_requests.clone();
            let error = state.error.clone();
            let sequence = state.sequence;
            assert!(state.update(old.clone()).is_none());
            assert!(state.pending.is_none());
            assert!(state.confirmation.is_none());
            assert_eq!(state.selected.as_ref(), Some(successor));
            assert_eq!(state.sequence, sequence);
            assert_eq!(state.manual_requests, receipts);
            assert_eq!(state.error, error);

            let (sequence, query) = request(state.update(Action::Tick));
            assert_eq!(query, Request::Show { id: successor.id });
            assert!(state.update(old.clone()).is_none());
            assert!(matches!(state.pending,
                Some((current, Pending::RefreshAutomation)) if current == sequence));
            assert_eq!(state.sequence, sequence);
            assert_eq!(state.manual_requests, receipts);
            finish_control_definition_read(&mut state, sequence, successor);

            assert!(state.update(fresh.clone()).is_none());
            let confirmation = state.confirmation.as_ref().unwrap();
            let generation = confirmation.generation;
            let confirmed = confirmation.request.clone();
            assert_eq!(&confirmation.automation, successor);
            // Neither a foreign callback nor a duplicate callback from the
            // now-hidden definition controls can replace this consent.
            for action in [old, fresh] {
                assert!(state.update(action).is_none());
                let confirmation = state.confirmation.as_ref().unwrap();
                assert_eq!(confirmation.generation, generation);
                assert_eq!(confirmation.request, confirmed);
                assert_eq!(&confirmation.automation, successor);
                assert_eq!(state.sequence, generation);
                assert_eq!(state.manual_requests, receipts);
            }
            let (sequence, actual) = request(state.update(Action::Confirm(generation)));
            assert_eq!(actual, confirmed);
            if matches!(actual, Request::RunNow { .. }) {
                assert_eq!(actual, original, "retry keeps the exact original request");
            }
            assert!(matches!(state.pending,
                Some((current, Pending::Mutate)) if current == sequence));
            assert_eq!(state.manual_requests, receipts);
        }
    }
}

#[test]
fn retained_definition_actions_require_their_visible_available_control() {
    let owner = automation();
    for (mode, actions) in [
        ("draft", controls(&owner).to_vec()),
        ("run", controls(&owner).to_vec()),
        ("history", controls(&owner).to_vec()),
        ("blocked", controls(&owner).to_vec()),
        ("active", vec![run_now(&owner), remove(&owner)]),
        ("enabled", vec![enable(&owner)]),
    ] {
        let mut state = State::default();
        select_control_definition(&mut state, &owner);
        match mode {
            "draft" => {
                assert!(state.update(Action::Edit).is_none());
                assert!(state.has_draft());
            }
            "run" => {
                let detail = run_of(&owner);
                let (sequence, query) = request(state.update(Action::Run(detail.firing.id)));
                assert_eq!(
                    query,
                    Request::ShowFiring {
                        id: detail.firing.id
                    }
                );
                // A run with no backing run record produces no navigation
                // effect, while the definition remains selected underneath.
                assert!(detail.run.is_none());
                assert!(
                    state
                        .update(Action::Received {
                            sequence,
                            result: Arc::new(Ok(Response::Detail(detail))),
                        })
                        .is_none()
                );
                assert!(state.run.is_some());
            }
            "history" => {
                let (sequence, query) = request(state.update(Action::AllHistory));
                assert!(matches!(query, Request::History { id: None, .. }));
                assert!(
                    state
                        .update(Action::Received {
                            sequence,
                            result: Arc::new(Ok(Response::History(
                                crate::automations::HistoryPage {
                                    firings: Vec::new(),
                                    next: None,
                                }
                            ))),
                        })
                        .is_none()
                );
                assert!(state.all_history);
            }
            _ => {
                let mut updated = owner.clone();
                match mode {
                    "blocked" => updated.blocked = Some("fixture: launch unavailable".into()),
                    "active" => updated.active_firing = Some(Uuid::new_v4()),
                    "enabled" => updated.enabled = true,
                    _ => unreachable!(),
                }
                let (sequence, _) = request(state.update(Action::Tick));
                finish_control_definition_read(&mut state, sequence, &updated);
            }
        }
        let selected = state.selected.clone();
        let run = state.run.clone();
        let draft = state.has_draft();
        let all_history = state.all_history;
        let sequence = state.sequence;
        for action in actions {
            assert!(state.update(action).is_none(), "{mode}");
            assert!(state.confirmation.is_none(), "{mode}");
            assert!(state.pending.is_none(), "{mode}");
            assert_eq!(state.sequence, sequence, "{mode}");
            assert_eq!(state.selected, selected, "{mode}");
            assert_eq!(state.run, run, "{mode}");
            assert_eq!(state.has_draft(), draft, "{mode}");
            assert_eq!(state.all_history, all_history, "{mode}");
            assert!(state.manual_requests.is_empty(), "{mode}");
        }
    }
}

#[test]
fn same_definition_metadata_refresh_keeps_captured_controls_valid() {
    let owner = automation();
    let mut refreshed = owner.clone();
    refreshed.updated_ms += 1;
    for action in controls(&owner) {
        let mut state = State::default();
        select_control_definition(&mut state, &owner);
        let (sequence, _) = request(state.update(Action::Tick));
        finish_control_definition_read(&mut state, sequence, &refreshed);
        assert!(state.update(action).is_none());
        let confirmation = state.confirmation.as_ref().unwrap();
        assert_eq!(confirmation.automation, refreshed);
        let generation = confirmation.generation;
        let expected = confirmation.request.clone();
        let (_, actual) = request(state.update(Action::Confirm(generation)));
        assert_eq!(actual, expected);
    }
}
