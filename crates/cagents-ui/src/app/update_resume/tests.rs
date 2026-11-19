use super::*;
use crate::agent::Provider;
use crate::app::chat_ui::tests::{held, live_chat_for_provider, sent, submit, write};
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

/// Takes `job` in, as a window's status poll does, with `handled` as the
/// shared record's answer.
fn offer(app: &mut Cagents, job: &UpdateJob, handled: &[TerminalId]) {
    drop(app.offer_update_resume(job));
    drop(app.update_update_resume(Action::Loaded(job.id.clone(), Ok(handled.to_vec()))));
}

fn handled(app: &Cagents) -> Vec<TerminalId> {
    app.update_resume.handled.clone().unwrap()
}

/// A window that records the offer with the others.
fn shared(app: &mut Cagents) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    app.update_resume.store = Some(Arc::new(UpdateResumeStore::at(
        directory.path().join("update-resume.sqlite3"),
    )));
    directory
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
        &[],
    );
    assert!(offered(&app).is_empty());
    offer(&mut app, &job(UpdatePhase::Complete, in_flight), &[]);
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
    offer(&mut app, &update, &[]);
    drop(app.update_update_resume(Action::Resume(id)));
    let message = sent(&app, card).expect("the resumed conversation is sent the message");
    assert!(
        message.contains("- subagent \"Review the diff\"\n") && !message.contains("toolu_agent"),
        "{message}"
    );
    assert!(
        message.contains("carry on from where you left off"),
        "{message}"
    );
    assert_eq!(sent(&app, other), None, "only the one resumed is asked");
    assert_eq!(held(&app, other), None);
    assert_eq!(offered(&app), vec![session_id(&app, other)]);
    // The same update polled again does not offer it twice.
    drop(app.offer_update_resume(&update));
    assert_eq!(offered(&app), vec![session_id(&app, other)]);
    // Nor does a window opened later, which reads the shared record.
    assert_eq!(handled(&app), vec![id]);
    app.update_resume = State::default();
    offer(&mut app, &update, &[id]);
    assert_eq!(offered(&app), vec![session_id(&app, other)]);
    // A later update offers afresh.
    let mut later = update.clone();
    later.id = "update-2".into();
    offer(&mut app, &later, &[]);
    assert_eq!(offered(&app).len(), 2);
}

/// One conversation can show in two windows. Each asks the shared record
/// before sending, and only the first to take it off the offer resumes it.
#[test]
fn a_conversation_resumed_in_another_window_is_not_resumed_again() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let _record = shared(&mut app);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("both-windows"));
    let id = session_id(&app, card);
    let update = job(
        UpdatePhase::Complete,
        vec![outcome(id, Fate::Interrupted, |work| work.turn = true)],
    );
    offer(&mut app, &update, &[]);

    drop(app.update_update_resume(Action::Resume(id)));
    assert!(offered(&app).is_empty(), "the row goes at once");
    assert_eq!(
        sent(&app, card),
        None,
        "nothing goes before the record answers"
    );
    let Some(resume) = taken_request(&mut app, id) else {
        panic!("the resume waits on the record");
    };
    // The other window took it first.
    drop(app.update_update_resume(Action::Taken(
        update.id.clone(),
        vec![resume.clone()],
        Ok(Taken {
            first: Vec::new(),
            handled: vec![id],
        }),
    )));
    assert_eq!(sent(&app, card), None);
    assert_eq!(held(&app, card), None);
    assert!(offered(&app).is_empty());
    assert!(app.update_resume.taking.is_empty());
    let notice = app.status.as_ref().map(|notice| notice.message.clone());
    assert!(
        notice.as_deref().is_some_and(
            |notice| notice.ends_with(" was already resumed or dismissed in another window.")
        ),
        "{notice:?}"
    );

    // Asked again with the record saying this window is first, it is sent.
    app.update_resume = State::default();
    let _record = shared(&mut app);
    offer(&mut app, &update, &[]);
    drop(app.update_update_resume(Action::Resume(id)));
    let resume = taken_request(&mut app, id).unwrap();
    drop(app.update_update_resume(Action::Taken(
        update.id.clone(),
        vec![resume],
        Ok(Taken {
            first: vec![id],
            handled: vec![id],
        }),
    )));
    assert!(sent(&app, card).is_some_and(|message| message.contains("carry on")));
    assert_eq!(handled(&app), vec![id]);
}

/// The request a Resume made of the shared record, as it would come back.
fn taken_request(app: &mut Cagents, id: TerminalId) -> Option<Resume> {
    app.update_resume.taking.contains(&id).then(|| {
        let outcome = app
            .update_resume
            .offers
            .iter()
            .find(|outcome| outcome.work.id == id)
            .unwrap()
            .clone();
        Resume {
            id,
            prompt: continue_prompt(&outcome.work),
            label: app.offer_label(&outcome),
        }
    })
}

/// A conversation that stopped taking messages while the record was asked
/// goes back on offer, here and for the other windows.
#[test]
fn a_resume_refused_after_the_record_answered_goes_back_on_offer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let _record = shared(&mut app);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("refused-late"));
    let id = session_id(&app, card);
    let update = job(
        UpdatePhase::Complete,
        vec![outcome(id, Fate::Interrupted, |work| work.turn = true)],
    );
    offer(&mut app, &update, &[]);
    drop(app.update_update_resume(Action::Resume(id)));
    let resume = taken_request(&mut app, id).unwrap();
    // Its conversation went away while the record was asked.
    app.cards.remove(&card);
    drop(app.update_update_resume(Action::Taken(
        update.id.clone(),
        vec![resume],
        Ok(Taken {
            first: vec![id],
            handled: vec![id],
        }),
    )));
    assert!(app.status.is_some(), "the notice says it was not resumed");
    assert_eq!(handled(&app), Vec::<TerminalId>::new());
    assert_eq!(
        app.update_resume
            .offers
            .iter()
            .map(|outcome| outcome.work.id)
            .collect::<Vec<_>>(),
        vec![id]
    );
}

#[test]
fn the_message_names_tasks_as_their_provider_did_and_counts_the_unnamed() {
    let mut work = InFlight {
        id: TerminalId::new(),
        turn: true,
        ..InFlight::default()
    };
    work.tasks.push(subagent("toolu_named", "Review the diff"));
    work.tasks.push(subagent("toolu_unnamed_1", ""));
    work.tasks.push(subagent("toolu_unnamed_2", " "));
    work.tasks.push(InFlightTask {
        id: "bash_7".into(),
        kind: TaskKind::Command,
        name: String::new(),
        waiting: false,
    });
    let message = continue_prompt(&work);
    assert!(
        !message.contains("toolu_") && !message.contains("bash_7"),
        "{message}"
    );
    assert!(
        message.contains(
            "- your turn in progress\n- subagent \"Review the diff\"\n- 2 subagents\n- 1 background command\n"
        ),
        "{message}"
    );
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
    offer(&mut app, &job(UpdatePhase::Complete, in_flight), &[]);
    drop(app.update_update_resume(Action::ResumeAll));
    for card in cards {
        assert!(sent(&app, card).is_some());
    }
    assert!(offered(&app).is_empty());
}

#[test]
fn a_dismissed_offer_stays_dismissed_for_that_update() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("dismissed"));
    let id = session_id(&app, card);
    let elsewhere = TerminalId::new();
    let update = job(
        UpdatePhase::Failed,
        vec![
            outcome(id, Fate::Stopped, |work| work.turn = true),
            outcome(elsewhere, Fate::Stopped, |work| work.turn = true),
        ],
    );
    offer(&mut app, &update, &[]);
    assert_eq!(offered(&app), vec![id]);
    drop(app.update_update_resume(Action::Dismiss));
    assert!(offered(&app).is_empty());
    assert_eq!(sent(&app, card), None, "dismissing sends nothing");
    assert_eq!(held(&app, card), None);
    // Only what this window showed is dismissed: the window holding the
    // other conversation still offers it.
    assert_eq!(handled(&app), vec![id]);
    app.update_resume = State::default();
    offer(&mut app, &update, &[id]);
    assert!(offered(&app).is_empty());
    assert_eq!(
        app.update_resume
            .offers
            .iter()
            .map(|outcome| outcome.work.id)
            .collect::<Vec<_>>(),
        vec![elsewhere]
    );
}

#[test]
fn writing_to_a_conversation_takes_it_off_the_offer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("by-hand"));
    let id = session_id(&app, card);
    let update = job(
        UpdatePhase::Complete,
        vec![outcome(id, Fate::Interrupted, |work| work.turn = true)],
    );
    offer(&mut app, &update, &[]);
    assert_eq!(offered(&app), vec![id]);
    write(&mut app, card, "Where were we?");
    submit(&mut app, card);
    assert_eq!(sent(&app, card).as_deref(), Some("Where were we?"));
    assert!(offered(&app).is_empty(), "their own message carries it on");
    assert_eq!(handled(&app), vec![id]);
}

#[test]
fn an_unreadable_record_still_makes_the_offer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let _record = shared(&mut app);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("unreadable"));
    let update = job(
        UpdatePhase::Complete,
        vec![outcome(session_id(&app, card), Fate::Ended, |work| {
            work.queued.push("queued-1".into());
        })],
    );
    drop(app.offer_update_resume(&update));
    assert!(
        offered(&app).is_empty(),
        "nothing is offered before it is read"
    );
    drop(app.update_update_resume(Action::Loaded(
        update.id.clone(),
        Err("database is locked".into()),
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
        }),
        "Stopped a reply in progress, 1 subagent and 1 background task"
    );
    assert_eq!(
        work(|work| {
            work.tasks.push(subagent("a", "A"));
            work.unlisted = 3;
        }),
        "Stopped 1 subagent and 3 other items"
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

#[test]
fn the_notice_names_every_conversation_that_could_not_be_resumed() {
    let labels = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        refused_notice(&labels(&["login"])),
        "login can't take a message right now, so it wasn't resumed."
    );
    assert_eq!(
        refused_notice(&labels(&["login", "ci", "notes"])),
        "login, ci and notes can't take a message right now, so they weren't resumed."
    );
}
