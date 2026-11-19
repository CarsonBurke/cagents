use super::*;
use crate::agent::Provider;
use crate::app::chat_ui::tests::{live_chat_for_provider, outgoing};
use crate::update::UpdatePhase;
use crate::update::in_flight::{Fate, Task as InFlightTask};

fn session_id(app: &Cagents, card: CardId) -> TerminalId {
    match app.card_state(card).map(|state| &state.kind) {
        Some(CardKind::Chat(session)) => session.id,
        _ => panic!("the card holds a chat"),
    }
}

fn outcome(id: TerminalId, fate: Fate, work: impl FnOnce(&mut InFlight)) -> Outcome {
    let mut recorded = InFlight {
        id,
        ..InFlight::default()
    };
    work(&mut recorded);
    Outcome {
        work: recorded,
        label: "Recorded label".into(),
        fate,
    }
}

fn job(phase: UpdatePhase, in_flight: Vec<Outcome>) -> UpdateJob {
    UpdateJob {
        id: "update-1".into(),
        version: "1.2.3".into(),
        phase,
        message: String::new(),
        log_path: PathBuf::new(),
        confirmation: None,
        refusal: None,
        in_flight,
    }
}

/// Takes `job` in, as a window's status poll does, with `record` as the
/// shared state's answer.
fn offer(app: &mut Cagents, job: &UpdateJob, record: Option<&Record>) {
    drop(app.offer_update_resume(job));
    let saved = record.map(|record| serde_json::to_string(record).unwrap());
    drop(app.update_update_resume(Action::Loaded(job.id.clone(), Ok(saved))));
}

fn offered(app: &Cagents) -> Vec<TerminalId> {
    app.offered_here().map(|outcome| outcome.work.id).collect()
}

fn subagent(id: &str, name: &str) -> InFlightTask {
    InFlightTask {
        id: id.into(),
        kind: TaskKind::Subagent,
        name: name.into(),
        waiting: false,
    }
}

#[test]
fn only_work_the_update_stopped_is_offered_and_only_here() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let restarted = live_chat_for_provider(&mut app, Provider::Codex, Some("restarted"));
    let continued = live_chat_for_provider(&mut app, Provider::Claude, Some("continued"));
    let interrupted = live_chat_for_provider(&mut app, Provider::Claude, Some("interrupted"));
    let elsewhere = TerminalId::new();
    let in_flight = vec![
        outcome(session_id(&app, restarted), Fate::Restarted, |work| {
            work.tasks.push(subagent("child", "Worker"));
        }),
        outcome(session_id(&app, continued), Fate::Continued, |work| {
            work.turn = true;
        }),
        outcome(session_id(&app, interrupted), Fate::Interrupted, |work| {
            work.turn = true;
        }),
        outcome(elsewhere, Fate::Stopped, |work| work.turn = true),
    ];
    // A review still running has nothing to offer yet.
    offer(
        &mut app,
        &job(UpdatePhase::Installing, in_flight.clone()),
        None,
    );
    assert!(offered(&app).is_empty());
    offer(&mut app, &job(UpdatePhase::Complete, in_flight), None);
    assert_eq!(
        offered(&app),
        vec![session_id(&app, restarted), session_id(&app, interrupted)],
        "work that survived, and a conversation another window holds, are not offered here"
    );
}

#[test]
fn resuming_asks_the_conversation_to_carry_on_once_per_update() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("resumed"));
    let other = live_chat_for_provider(&mut app, Provider::Claude, Some("other"));
    let id = session_id(&app, card);
    let update = job(
        UpdatePhase::Complete,
        vec![
            outcome(id, Fate::Restarted, |work| {
                work.turn = true;
                work.tasks.push(subagent("toolu_agent", "Review the diff"));
            }),
            outcome(session_id(&app, other), Fate::Interrupted, |work| {
                work.turn = true;
            }),
        ],
    );
    offer(&mut app, &update, None);
    drop(app.update_update_resume(Action::Resume(id)));
    let sent = outgoing(&app, card).expect("the resumed conversation is asked to carry on");
    assert!(
        sent.contains("subagent \"Review the diff\" (toolu_agent)"),
        "{sent}"
    );
    assert!(sent.contains("carry on from where you left off"), "{sent}");
    assert_eq!(outgoing(&app, other), None, "only the one resumed is asked");
    assert_eq!(offered(&app), vec![session_id(&app, other)]);
    // The same update polled again does not offer it twice.
    drop(app.offer_update_resume(&update));
    assert_eq!(offered(&app), vec![session_id(&app, other)]);
    // Nor does a window opened later, which reads the shared record.
    let record = app.update_resume.record.clone().unwrap();
    assert_eq!(record.handled, vec![id]);
    app.update_resume = State::default();
    offer(&mut app, &update, Some(&record));
    assert_eq!(offered(&app), vec![session_id(&app, other)]);
    // A later update offers afresh.
    let mut later = update.clone();
    later.id = "update-2".into();
    offer(&mut app, &later, Some(&record));
    assert_eq!(offered(&app).len(), 2);
}

#[test]
fn resume_all_asks_every_conversation_listed() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let cards: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| live_chat_for_provider(&mut app, Provider::Claude, Some(name)))
        .collect();
    let in_flight = cards
        .iter()
        .map(|card| {
            outcome(session_id(&app, *card), Fate::Interrupted, |work| {
                work.turn = true
            })
        })
        .collect();
    offer(&mut app, &job(UpdatePhase::Complete, in_flight), None);
    drop(app.update_update_resume(Action::ResumeAll));
    for card in cards {
        assert!(outgoing(&app, card).is_some());
    }
    assert!(offered(&app).is_empty());
}

#[test]
fn a_dismissed_offer_stays_dismissed_for_that_update() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("dismissed"));
    let update = job(
        UpdatePhase::Failed,
        vec![outcome(session_id(&app, card), Fate::Stopped, |work| {
            work.turn = true;
        })],
    );
    offer(&mut app, &update, None);
    assert_eq!(offered(&app).len(), 1);
    drop(app.update_update_resume(Action::Dismiss));
    assert!(offered(&app).is_empty());
    assert_eq!(outgoing(&app, card), None, "dismissing sends nothing");
    let record = app.update_resume.record.clone().unwrap();
    assert!(record.dismissed);
    app.update_resume = State::default();
    offer(&mut app, &update, Some(&record));
    assert!(offered(&app).is_empty());
}

#[test]
fn an_unreadable_record_still_makes_the_offer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("unreadable"));
    let update = job(
        UpdatePhase::Complete,
        vec![outcome(session_id(&app, card), Fate::Ended, |work| {
            work.queued.push("queued-1".into());
        })],
    );
    drop(app.offer_update_resume(&update));
    drop(app.update_update_resume(Action::Loaded(
        update.id.clone(),
        Ok(Some("{not json".into())),
    )));
    assert_eq!(offered(&app).len(), 1);
}

#[test]
fn rows_say_what_stopped_in_plain_words() {
    let id = TerminalId::new();
    let work = |edit: fn(&mut InFlight)| {
        let mut work = InFlight {
            id,
            ..InFlight::default()
        };
        edit(&mut work);
        summary(&work)
    };
    assert_eq!(work(|work| work.turn = true), "Stopped a reply in progress");
    assert_eq!(
        work(|work| {
            work.turn = true;
            work.tasks.push(subagent("a", "A"));
            work.tasks.push(InFlightTask {
                id: "bash-1".into(),
                kind: TaskKind::Command,
                name: "cargo test".into(),
                waiting: false,
            });
            work.unlisted = 1;
        }),
        "Stopped a reply in progress, 1 subagent and 2 background tasks"
    );
    assert_eq!(
        work(|work| {
            work.tasks.push(subagent("a", "A"));
            work.tasks.push(subagent("b", "B"));
        }),
        "Stopped 2 subagents"
    );
    assert_eq!(
        work(|work| work.queued.push("q".into())),
        "1 queued message wasn't delivered"
    );
    assert_eq!(
        work(|work| {
            work.turn = true;
            work.queued.push("q".into());
            work.queued.push("r".into());
        }),
        "Stopped a reply in progress; 2 queued messages weren't delivered"
    );
}
