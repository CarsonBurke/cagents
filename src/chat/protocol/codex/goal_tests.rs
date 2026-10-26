//! Real adapter request/response fixtures. No app-server or provider is started.
use super::*;
use crate::chat::goal::{Operation, ReadState, Snapshot, Status, Support};

fn adapter() -> Codex {
    Codex {
        thread: Some("thread-exact".into()),
        cwd: PathBuf::from("/tmp"),
        expected_home: std::fs::canonicalize("/tmp").unwrap(),
        ready: true,
        attached: true,
        direct_input: true,
        ..Codex::default()
    }
}

fn raw_goal(thread: &str, objective: &str, status: &str) -> Value {
    json!({"threadId":thread,"objective":objective,"status":status,
        "tokenBudget":null,"tokensUsed":17,"timeUsedSeconds":4,
        "createdAt":50,"updatedAt":2})
}

fn snapshot(adapter: &mut Codex) -> Snapshot {
    adapter.goal_state().unwrap().snapshot()
}

fn published(step: &Step) -> &Snapshot {
    step.events
        .iter()
        .find_map(|event| match event {
            Event::Goal(snapshot) => Some(snapshot),
            _ => None,
        })
        .expect("actual adapter must publish its goal observation")
}

fn command(adapter: &mut Codex, operation: Operation) -> Step {
    let observed = snapshot(adapter);
    adapter
        .command(Command::Goal {
            thread_id: observed.thread_id,
            revision: observed.revision,
            operation,
        })
        .unwrap()
}

fn attach(adapter: &mut Codex) -> Step {
    let initialize = adapter.initialize().unwrap();
    let initialized = adapter
        .receive(json!({"id":initialize.writes[0]["id"],
        "result":{"codexHome":adapter.expected_home,"userAgent":"fixture"}}))
        .unwrap();
    let resume = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/resume")
        .unwrap();
    assert_eq!(resume["params"]["threadId"], "thread-exact");
    let attached = adapter
        .receive(json!({"id":resume["id"],"result":{
        "thread":{"id":"thread-exact","canAcceptDirectInput":true,"status":{"type":"idle"}},
        "initialTurnsPage":{"data":[],"nextCursor":null}}}))
        .unwrap();
    let skills = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    adapter
        .receive(json!({"id":skills["id"],"result":{"data":[]}}))
        .unwrap();
    assert!(adapter.ready);
    attached
}

#[test]
fn native_goal_commands_correlate_exact_thread_and_publish_only_actual_responses() {
    let mut adapter = adapter();
    for (operation, method, reply) in [
        (Operation::Get, "thread/goal/get", json!({"goal":null})),
        (
            Operation::SetObjective("Finish 🦀 safely".into()),
            "thread/goal/set",
            json!({"goal":raw_goal("thread-exact","Finish 🦀 safely","active")}),
        ),
        (
            Operation::Pause,
            "thread/goal/set",
            json!({"goal":raw_goal("thread-exact","Finish 🦀 safely","paused")}),
        ),
        (
            Operation::Resume,
            "thread/goal/set",
            json!({"goal":raw_goal("thread-exact","Finish 🦀 safely","complete")}),
        ),
        (
            Operation::Clear,
            "thread/goal/clear",
            json!({"cleared":true}),
        ),
    ] {
        let before = snapshot(&mut adapter);
        let issued = command(&mut adapter, operation.clone());
        assert_eq!(issued.writes.len(), 1);
        let frame = &issued.writes[0];
        assert_eq!(frame["method"], method);
        assert_eq!(frame["params"]["threadId"], "thread-exact");
        assert!(frame["params"].get("tokenBudget").is_none());
        if !matches!(operation, Operation::SetObjective(_)) {
            assert!(frame["params"].get("objective").is_none());
        }
        assert_eq!(
            published(&issued).read,
            before.read,
            "dispatch is not confirmation"
        );
        assert!(published(&issued).pending);
        assert!(command_busy(&mut adapter));
        let received = adapter
            .receive(json!({"id":frame["id"],"result":reply}))
            .unwrap();
        let actual = published(&received);
        assert!(!actual.pending);
        assert!(!actual.uncertain);
        assert_eq!(actual.support, Support::Supported);
        if operation == Operation::Resume {
            assert!(
                matches!(&actual.read, ReadState::Known(Some(goal)) if goal.status == Status::Complete)
            );
        }
        assert!(
            received.writes.is_empty(),
            "no automatic turn, goal retry or prompt"
        );
    }
    assert_eq!(snapshot(&mut adapter).read, ReadState::Known(None));
}

fn command_busy(adapter: &mut Codex) -> bool {
    let current = snapshot(adapter);
    adapter
        .command(Command::Goal {
            thread_id: current.thread_id,
            revision: current.revision,
            operation: Operation::Get,
        })
        .is_err()
}

#[test]
fn notification_wins_over_early_reply_without_releasing_physical_slot_or_newer_request() {
    let mut adapter = adapter();
    let issued = command(&mut adapter, Operation::SetObjective("original".into()));
    let observed = adapter
        .receive(json!({"method":"thread/goal/updated","params":{
        "threadId":"thread-exact","goal":raw_goal("thread-exact","newer","blocked")}}))
        .unwrap();
    assert!(published(&observed).pending);
    assert!(command_busy(&mut adapter));
    let released = adapter
        .receive(json!({"id":issued.writes[0]["id"],"result":{
        "goal":raw_goal("thread-exact","original","active")}}))
        .unwrap();
    assert!(
        matches!(&published(&released).read, ReadState::Known(Some(goal)) if goal.objective == "newer")
    );
    assert!(!published(&released).pending);
    let next = command(&mut adapter, Operation::Clear);
    assert!(
        adapter
            .receive(json!({"id":issued.writes[0]["id"],"result":{"goal":null}}))
            .is_err()
    );
    assert!(
        snapshot(&mut adapter).pending,
        "duplicate old reply cannot release successor"
    );
    let cleared = adapter
        .receive(json!({"method":"thread/goal/cleared","params":{"threadId":"thread-exact"}}))
        .unwrap();
    assert_eq!(published(&cleared).read, ReadState::Known(None));
    assert!(published(&cleared).pending);
    let stale = adapter
        .receive(json!({"id":next.writes[0]["id"],"result":{"cleared":false}}))
        .unwrap();
    assert_eq!(published(&stale).read, ReadState::Known(None));
    assert!(!published(&stale).uncertain);
    assert!(!published(&stale).pending);
}

#[test]
fn queued_controls_require_current_observation_even_after_response_only_replacement() {
    let mut adapter = adapter();
    let get = command(&mut adapter, Operation::Get);
    adapter.receive(json!({"id":get.writes[0]["id"],"result":{"goal":raw_goal("thread-exact","A","paused")}})).unwrap();
    let old = snapshot(&mut adapter);
    let set = command(&mut adapter, Operation::SetObjective("B".into()));
    adapter.receive(json!({"id":set.writes[0]["id"],"result":{"goal":raw_goal("thread-exact","B","active")}})).unwrap();
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: old.thread_id,
                revision: old.revision,
                operation: Operation::Clear
            })
            .is_err()
    );
    let current = snapshot(&mut adapter);
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: "foreign".into(),
                revision: current.revision,
                operation: Operation::Clear
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), current);
    assert_eq!(
        command(&mut adapter, Operation::Clear).writes[0]["method"],
        "thread/goal/clear"
    );
}

#[test]
fn malformed_current_observations_invalidate_cache_but_foreign_threads_do_not() {
    let mut adapter = adapter();
    let read = command(&mut adapter, Operation::Get);
    adapter.receive(json!({"id":read.writes[0]["id"],"result":{"goal":raw_goal("thread-exact","A","active")}})).unwrap();
    let before = snapshot(&mut adapter);
    let foreign = adapter
        .receive(json!({"method":"thread/goal/updated","params":{
        "threadId":"other","goal":raw_goal("other","foreign","complete")}}))
        .unwrap();
    assert!(foreign.events.is_empty());
    assert_eq!(snapshot(&mut adapter), before);
    let pending = command(&mut adapter, Operation::Get);
    let invalid = adapter
        .receive(json!({"method":"thread/goal/updated","params":{
        "threadId":"thread-exact","goal":raw_goal("wrong-inner","foreign","complete")}}))
        .unwrap();
    assert_eq!(published(&invalid).read, ReadState::Unknown);
    assert!(published(&invalid).pending);
    assert!(
        invalid
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { fatal: false, .. }))
    );
    let late = adapter
        .receive(json!({"id":pending.writes[0]["id"],"result":{"goal":null}}))
        .unwrap();
    assert_eq!(published(&late).read, ReadState::Unknown);
    assert!(!published(&late).pending);
    let retry = command(&mut adapter, Operation::Get);
    let malformed = adapter
        .receive(json!({"id":retry.writes[0]["id"],"result":{
        "goal":raw_goal("other","foreign","complete")}}))
        .unwrap();
    assert!(published(&malformed).uncertain);
    assert_eq!(published(&malformed).read, ReadState::Unknown);
    assert!(!published(&malformed).pending);
    assert!(malformed.writes.is_empty());
}

#[test]
fn attach_and_reconnect_read_optional_goal_without_blocking_unsupported_chat() {
    let mut adapter = adapter();
    let attached = attach(&mut adapter);
    let read = attached
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/goal/get")
        .unwrap();
    assert!(published(&attached).pending);
    assert!(attached.writes.iter().all(|frame| !matches!(
        frame["method"].as_str(),
        Some("turn/start" | "thread/goal/set" | "thread/goal/clear")
    )));
    let unavailable = adapter
        .receive(json!({"id":read["id"],"error":{"code":-32601,"message":"method not found"}}))
        .unwrap();
    assert_eq!(published(&unavailable).support, Support::Unsupported);
    assert!(!published(&unavailable).uncertain);
    assert!(
        unavailable
            .events
            .iter()
            .all(|event| !matches!(event, Event::Error { fatal: true, .. }))
    );
    assert!(adapter.ready);
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: "thread-exact".into(),
                revision: published(&unavailable).revision,
                operation: Operation::SetObjective("No fallback".into())
            })
            .is_err()
    );
    let prompt = adapter
        .command(Command::Prompt {
            text: "ordinary chat".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    assert_eq!(prompt.writes[0]["method"], "turn/start");
    // A fresh handshake may be a newer app-server. Check, never replay mutations.
    let reattached = attach(&mut adapter);
    assert_eq!(
        reattached
            .writes
            .iter()
            .filter(|frame| frame["method"] == "thread/goal/get")
            .count(),
        1
    );
    assert!(reattached.writes.iter().all(|frame| !matches!(
        frame["method"].as_str(),
        Some("thread/goal/set" | "thread/goal/clear")
    )));
}

#[test]
fn reconnect_keeps_unresolved_goal_slot_and_never_replays_a_mutation() {
    let mut adapter = adapter();
    let issued = command(&mut adapter, Operation::Pause);
    let attached = attach(&mut adapter);
    assert!(published(&attached).pending);
    assert_eq!(published(&attached).read, ReadState::Unknown);
    assert!(attached.writes.iter().all(|frame| {
        !frame["method"]
            .as_str()
            .unwrap()
            .starts_with("thread/goal/")
    }));
    assert!(command_busy(&mut adapter));
    let resolved = adapter
        .receive(json!({"id":issued.writes[0]["id"],"error":{"code":-32000,"message":"unknown"}}))
        .unwrap();
    assert!(!published(&resolved).pending);
    assert_eq!(published(&resolved).read, ReadState::Unknown);
    assert!(resolved.writes.is_empty());
    assert_eq!(
        command(&mut adapter, Operation::Get).writes[0]["method"],
        "thread/goal/get"
    );
}

#[test]
fn failed_request_admission_and_invalid_direct_objective_do_not_reserve_a_slot() {
    let mut adapter = adapter();
    let before = snapshot(&mut adapter);
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::SetObjective("🦀".repeat(4_001))
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), before);
    for id in 0..MAX_PENDING {
        adapter.pending.insert(id.to_string(), Pending::Other);
    }
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::Get
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), before);
    adapter.pending.clear();
    adapter.next_id = u64::MAX;
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::Get
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), before);
}
