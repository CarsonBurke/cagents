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

fn pause(owner: &Automation) -> Action {
    Action::Pause {
        id: owner.id,
        revision: owner.revision,
    }
}

#[derive(Debug, PartialEq)]
struct PauseCustody {
    confirmation: Option<(u64, Automation, Request, &'static str)>,
    sequence: u64,
    manual_requests: std::collections::BTreeMap<Uuid, (u64, Uuid)>,
    pending: Option<(u64, bool)>,
    selected: Option<Automation>,
    history: Vec<Firing>,
    history_status: String,
    history_next: Option<HistoryCursor>,
    history_newer: bool,
    error: Option<String>,
    notice: Option<String>,
    run: Option<RunDetail>,
    draft: bool,
    all_history: bool,
}

fn pause_custody(state: &State) -> PauseCustody {
    PauseCustody {
        confirmation: state.confirmation.as_ref().map(|consent| {
            (
                consent.generation,
                consent.automation.clone(),
                consent.request.clone(),
                consent.title,
            )
        }),
        sequence: state.sequence,
        manual_requests: state.manual_requests.clone(),
        pending: state.pending.map(|(sequence, kind)| {
            let Pending::History { append } = kind else {
                panic!("fixture snapshots only settled work or its full history-read payload")
            };
            (sequence, append)
        }),
        selected: state.selected.clone(),
        history: state.history.clone(),
        history_status: format!("{:?}", state.history_status),
        history_next: state.history_next.clone(),
        history_newer: state.history_newer,
        error: state.error.clone(),
        notice: state.notice.clone(),
        run: state.run.clone(),
        draft: state.has_draft(),
        all_history: state.all_history,
    }
}

fn pause_receive(state: &mut State, sequence: u64, response: Response) -> Option<Effect> {
    state.update(Action::Received {
        sequence,
        result: Arc::new(Ok(response)),
    })
}

fn settled_uncertain_manual_retry() -> (State, Automation, u64, Request) {
    use crate::automations::{FiringState, HistoryPage, Trigger};
    let mut owner = automation();
    owner.enabled = true;
    let mut state = State::default();
    select_control_definition(&mut state, &owner);
    assert!(state.update(run_now(&owner)).is_none());
    let old_generation = state.confirmation.as_ref().unwrap().generation;
    let (sequence, original) = request(state.update(Action::Confirm(old_generation)));
    let key = match &original {
        Request::RunNow {
            id,
            execution_revision,
            request,
            confirmed,
        } => {
            assert_eq!(
                (*id, *execution_revision),
                (owner.id, owner.execution_revision)
            );
            assert!(*confirmed && !request.is_nil());
            *request
        }
        _ => panic!("actual confirmed manual request"),
    };
    assert!(
        state
            .update(Action::Received {
                sequence,
                result: Arc::new(Err("fixture: original manual reply lost after send".into())),
            })
            .is_none()
    );
    assert!(state.error.is_some() && state.pending.is_none());
    let (sequence, show) = request(state.update(Action::Tick));
    assert_eq!(show, Request::Show { id: owner.id });
    let (sequence, history) = request(pause_receive(
        &mut state,
        sequence,
        Response::Automation(owner.clone()),
    ));
    assert_eq!(
        history,
        Request::History {
            id: Some(owner.id),
            before: None,
            limit: PAGE,
        }
    );
    let mut settled = history_firing(100, FiringState::Succeeded);
    settled.automation = owner.id;
    settled.revision = owner.execution_revision;
    settled.trigger = Trigger::Manual { request: key };
    settled.started_ms = Some(100);
    settled.finished_ms = Some(101);
    assert!(
        pause_receive(
            &mut state,
            sequence,
            Response::History(HistoryPage {
                firings: vec![settled.clone()],
                next: None,
            }),
        )
        .is_none()
    );
    assert_eq!(state.history, vec![settled]);
    assert_eq!(state.history_status, HistoryStatus::Loaded);
    assert_eq!(
        state.manual_requests.get(&owner.id),
        Some(&(owner.execution_revision, key))
    );
    assert!(state.pending.is_none() && state.confirmation.is_none());
    (state, owner, old_generation, original)
}

fn assert_retry_consent_survives_pause(
    state: &mut State,
    owner: &Automation,
    old_generation: u64,
    original: &Request,
    retry: Action,
    queued_pause: Action,
) {
    assert!(state.update(retry).is_none());
    let consent = state.confirmation.as_ref().unwrap();
    let generation = consent.generation;
    assert_ne!(generation, old_generation);
    assert_eq!(&consent.automation, owner);
    assert_eq!(&consent.request, original);
    assert_eq!(consent.title, "Run this automation now?");
    assert!(state.pending.is_none());
    let custody = pause_custody(state);
    assert!(state.update(queued_pause).is_none());
    assert_eq!(pause_custody(state), custody);
    assert!(state.update(Action::Confirm(old_generation)).is_none());
    assert_eq!(
        pause_custody(state),
        custody,
        "old consent cannot consume the retry"
    );
    let (sequence, actual) = request(state.update(Action::Confirm(generation)));
    assert_eq!(
        &actual, original,
        "confirmation emits the exact original K/e request"
    );
    assert!(matches!(state.pending, Some((current, Pending::Mutate)) if current == sequence));
    assert!(state.confirmation.is_none());
    assert_eq!(state.manual_requests, custody.manual_requests);
    assert_eq!(state.history, custody.history);
}

#[test]
fn same_subject_pause_cannot_consume_uncertain_manual_retry_consent() {
    let (mut state, owner, old_generation, original) = settled_uncertain_manual_retry();
    assert_retry_consent_survives_pause(
        &mut state,
        &owner,
        old_generation,
        &original,
        run_now(&owner),
        pause(&owner),
    );
}

#[test]
fn two_complete_widget_clicks_in_one_batch_preserve_retry_consent() {
    use cosmic::iced::{Event, Point, Rectangle, Size, mouse, runtime::user_interface::Cache};
    let (mut state, owner, old_generation, original) = settled_uncertain_manual_retry();
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cancel_frame(&state, Cache::new(), &mut renderer);
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let point = |label| {
        let button = announced_button(&nodes, label);
        assert!(
            !button.node().is_disabled(),
            "actual {label} control is enabled"
        );
        let bounds = button.node().bounds().expect("actual button geometry");
        let bounds = Rectangle::new(
            Point::new(bounds.x0 as f32, bounds.y0 as f32),
            Size::new(
                (bounds.x1 - bounds.x0) as f32,
                (bounds.y1 - bounds.y0) as f32,
            ),
        );
        assert!(bounds.width > 0.0 && bounds.height > 0.0);
        assert!(Rectangle::with_size(Size::new(900.0, 1100.0)).contains(bounds.center()));
        bounds.center()
    };
    let retry = point("Retry run…");
    let pause_point = point("Pause");
    assert_ne!(retry, pause_point);
    let mut messages = Vec::new();
    let _ = ui.update(
        &[
            Event::Mouse(mouse::Event::CursorMoved { position: retry }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            Event::Mouse(mouse::Event::CursorMoved {
                position: pause_point,
            }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ],
        mouse::Cursor::Available(pause_point),
        &mut renderer,
        &mut cosmic::iced::advanced::clipboard::Null,
        &mut messages,
    );
    let _ = ui.into_cache();
    assert_eq!(
        messages.len(),
        2,
        "two actual widget releases: {messages:?}"
    );
    assert!(matches!(&messages[0], Action::RunNow { id, revision }
        if (*id, *revision) == (owner.id, owner.revision)));
    assert!(matches!(&messages[1], Action::Pause { id, revision }
        if (*id, *revision) == (owner.id, owner.revision)));
    let mut queued = messages.into_iter();
    assert_retry_consent_survives_pause(
        &mut state,
        &owner,
        old_generation,
        &original,
        queued.next().unwrap(),
        queued.next().unwrap(),
    );
}

#[test]
fn pause_rejects_hidden_stale_disabled_and_foreground_busy_controls() {
    use crate::automations::HistoryPage;
    for mode in [
        "draft", "run", "history", "id", "revision", "disabled", "busy",
    ] {
        let mut owner = automation();
        owner.enabled = true;
        let mut state = State::default();
        select_control_definition(&mut state, &owner);
        let retained = pause(&owner);
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
                assert!(detail.run.is_none());
                assert!(pause_receive(&mut state, sequence, Response::Detail(detail)).is_none());
                assert!(state.run.is_some());
            }
            "history" => {
                let (sequence, query) = request(state.update(Action::AllHistory));
                assert!(matches!(query, Request::History { id: None, .. }));
                assert!(
                    pause_receive(
                        &mut state,
                        sequence,
                        Response::History(HistoryPage {
                            firings: Vec::new(),
                            next: None,
                        })
                    )
                    .is_none()
                );
                assert!(state.all_history && state.selected.is_none());
            }
            "busy" => {
                let (sequence, query) = request(state.update(Action::History));
                assert!(matches!(query, Request::History { id: Some(id), .. } if id == owner.id));
                assert!(
                    matches!(state.pending, Some((current, Pending::History { append: false })) if current == sequence)
                );
            }
            _ => {
                let mut successor = owner.clone();
                match mode {
                    "id" => successor.id = Uuid::new_v4(),
                    "revision" => successor.revision += 1,
                    "disabled" => successor.enabled = false,
                    _ => unreachable!(),
                }
                select_control_definition(&mut state, &successor);
            }
        }
        let custody = pause_custody(&state);
        assert!(state.update(retained).is_none(), "{mode}");
        assert_eq!(pause_custody(&state), custody, "{mode}");
    }
}

#[test]
fn enabled_pause_stays_available_and_explicit_cancel_allows_fresh_pause() {
    for mode in ["enabled", "blocked", "active", "both", "metadata"] {
        let mut owner = automation();
        owner.enabled = true;
        if matches!(mode, "blocked" | "both") {
            owner.blocked = Some("fixture: launch unavailable".into());
        }
        if matches!(mode, "active" | "both") {
            owner.active_firing = Some(Uuid::new_v4());
        }
        let mut state = State::default();
        select_control_definition(&mut state, &owner);
        let retained = pause(&owner);
        if mode == "metadata" {
            let mut refreshed = owner.clone();
            refreshed.updated_ms += 1;
            let (sequence, query) = request(state.update(Action::Tick));
            assert_eq!(query, Request::Show { id: owner.id });
            finish_control_definition_read(&mut state, sequence, &refreshed);
            assert_eq!(state.selected.as_ref(), Some(&refreshed));
        }
        assert_pause_request(&mut state, retained, &owner);
    }
    let (mut state, owner, _, original) = settled_uncertain_manual_retry();
    assert!(state.update(run_now(&owner)).is_none());
    assert_eq!(&state.confirmation.as_ref().unwrap().request, &original);
    let receipts = state.manual_requests.clone();
    assert!(state.update(Action::CancelConfirmation).is_none());
    assert!(state.confirmation.is_none() && state.pending.is_none());
    assert_eq!(state.manual_requests, receipts);
    assert_pause_request(&mut state, pause(&owner), &owner);
    assert_eq!(state.manual_requests, receipts);
}
