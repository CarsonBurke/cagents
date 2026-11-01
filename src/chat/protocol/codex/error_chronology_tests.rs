use super::{tests::adapter, *};
use crate::agent::Provider;

/// These tests feed real provider frames through Codex, the actor and its
/// private transcript store. Provider I/O is replaced by the existing local
/// actor fixture; item ordering and live/history dedup use production code.
#[test]
fn terminal_error_incidents_keep_turn_order_through_streaming_dedup_and_history_reload() {
    let raw = "Codex is temporarily at capacity\noriginal request diagnostics";
    let user = |id: &str, text: &str| json!({"id":id,"type":"userMessage","content":[{"type":"text","text":text}]});
    let answer = |id: &str, text: &str| json!({"id":id,"type":"agentMessage","text":text});
    let turn_a = json!({"id":"failed-a","status":"failed","error":{"message":raw},
        "items":[user("prompt", "First request"), answer("answer", "Partial answer")]});
    let turn_b = json!({"id":"resumed-b","status":"completed","items":[
        user("prompt", "Continue after capacity clears"), answer("answer", "Recovered answer")]});
    let turn_c = json!({"id":"failed-c","status":"failed","error":{"message":raw},
        "items":[user("prompt", "Another request")]});
    let started = |turn: &str, items: Value| {
        json!({"method":"turn/started","params":{"threadId":"thread-exact",
            "turn":{"id":turn,"status":"inProgress","items":items}}})
    };
    let completed = |turn: &Value| json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":turn}});
    let failed = |turn: &str| {
        json!({"method":"error","params":{"threadId":"thread-exact","turnId":turn,
            "willRetry":false,"error":{"message":raw}}})
    };
    let mut adapter = adapter();
    let mut history = Step::default();
    adapter.history(&mut history, None).unwrap();
    let page = json!({"data":[turn_c,turn_b,turn_a],"nextCursor":null});
    let frames = vec![
        started("failed-a", turn_a["items"].clone()),
        failed("failed-a"),
        failed("failed-a"),
        completed(&turn_a),
        started(
            "resumed-b",
            json!([user("prompt", "Continue after capacity clears")]),
        ),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                "turnId":"resumed-b","itemId":"answer","delta":"Recovered "}}),
        failed("failed-a"),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                "turnId":"resumed-b","itemId":"answer","delta":"answer"}}),
        completed(&turn_b),
        started("failed-c", turn_c["items"].clone()),
        failed("failed-c"),
        completed(&turn_c),
        json!({"id":history.writes[0]["id"],"result":page}),
    ];
    let failure_a = turn_error_item("failed-a", raw).id;
    let failure_c = turn_error_item("failed-c", raw).id;
    assert_ne!(
        failure_a, failure_c,
        "same text in later turns is a new incident"
    );
    let expected = vec![
        "failed-a/prompt",
        "failed-a/answer",
        &failure_a,
        "resumed-b/prompt",
        "resumed-b/answer",
        "failed-c/prompt",
        &failure_c,
    ];
    let ids = |snapshot: &chat::Snapshot| {
        snapshot
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>()
    };

    // Inspect the actual mid-stream snapshot before any history can repair
    // ordering. The delayed duplicate of A must neither move its incident past
    // B nor set B's scalar error; B's two deltas remain one live answer.
    let (_, in_progress) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(self::adapter()),
        frames[..8].to_vec(),
        false,
    );
    assert_eq!(ids(&in_progress), expected[..5]);
    assert!(in_progress.busy);
    assert!(in_progress.error.is_none());
    assert!(matches!(&in_progress.items[4].content,
        ItemContent::Text { text, .. } if text == "Recovered answer"));

    let (_, streamed) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(adapter),
        frames,
        false,
    );
    assert_eq!(ids(&streamed), expected);
    assert_eq!(streamed.error.as_deref(), Some(raw));
    assert!(
        matches!(&streamed.items[4].content, ItemContent::Text { text, .. } if text == "Recovered answer")
    );
    for index in [2, 6] {
        assert_eq!(
            chat::failure::provider_error_text(Provider::Codex, &streamed.items[index]),
            Some(raw)
        );
    }

    // A new adapter/store has no locally retained error note. Full provider
    // history must restore each failure before the next turn on its own.
    let mut reloaded_adapter = self::adapter();
    let mut history = Step::default();
    reloaded_adapter.history(&mut history, None).unwrap();
    let (_, reloaded) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(reloaded_adapter),
        vec![
            json!({"id":history.writes[0]["id"],"result":page}),
            started("fixture-settle", json!([])),
            completed(&json!({"id":"fixture-settle","status":"completed","items":[]})),
        ],
        false,
    );
    assert_eq!(ids(&reloaded), expected);
    assert!(
        reloaded.error.is_none(),
        "historical failures do not fail the active turn"
    );
    for index in [2, 6] {
        assert_eq!(
            chat::failure::provider_error_text(Provider::Codex, &reloaded.items[index]),
            Some(raw)
        );
    }
}

#[test]
fn terminal_error_identity_uses_explicit_turn_scope_and_retains_raw_diagnostics() {
    let mut adapter = adapter();
    adapter.turn = Some("current-turn-is-not-error-authority".into());
    adapter.notification_turn = adapter.turn.clone();
    let raw = "Provider is at capacity\nfull diagnostics 🦀";
    for turn in [Value::Null, json!(""), json!("earlier-turn")] {
        let step = adapter
            .receive(json!({"method":"error","params":{
                "threadId":"thread-exact","turnId":turn,"willRetry":false,"error":{"message":raw}
            }}))
            .unwrap();
        if turn.as_str().is_some_and(|id| !id.is_empty()) {
            assert!(
                step.suppress_current_notification,
                "an old turn's failure cannot fail the new turn"
            );
            let [
                Event::Item(item),
                Event::Error {
                    message,
                    fatal: false,
                },
            ] = step.events.as_slice()
            else {
                panic!(
                    "scoped failure retains history and active error semantics: {:?}",
                    step.events
                );
            };
            assert_eq!(item, &turn_error_item("earlier-turn", raw));
            assert_eq!(
                chat::failure::provider_error_text(Provider::Codex, item),
                Some(raw)
            );
            assert_eq!(message, raw);
        } else {
            assert!(
                matches!(step.events.as_slice(), [Event::Error { message, fatal: false }] if message == raw)
            );
        }
    }
    // Delimiters in provider turn IDs and diagnostics cannot alias one another.
    assert_ne!(
        turn_error_item("a", "b/c").id,
        turn_error_item("a/b", "c").id
    );
    assert_ne!(turn_error_item("a", "bc").id, turn_error_item("ab", "c").id);
}

#[test]
fn completion_and_history_share_diagnostic_identity_without_a_message_field() {
    let mut adapter = adapter();
    let turn = json!({"id":"failed","status":"failed","items":[],
        "error":{"code":"capacity_error","additionalDetails":"original diagnostics"}});
    let completion = adapter
        .receive(json!({"method":"turn/completed","params":{
            "threadId":"thread-exact","turn":turn
        }}))
        .unwrap();
    let item = completion
        .events
        .iter()
        .find_map(|event| match event {
            Event::Item(item) => Some(item),
            _ => None,
        })
        .expect("failed completion records the provider diagnostic");
    let mut history = Step::default();
    adapter
        .page(
            &json!({"data":[turn],"nextCursor":null}),
            &mut history,
            false,
        )
        .unwrap();
    let [Event::LatestHistory { items, .. }, Event::BranchPoints(_)] = history.events.as_slice()
    else {
        panic!("{:?}", history.events);
    };
    assert_eq!(items, &vec![item.clone()]);
    let raw = turn["error"].to_string();
    assert_eq!(
        chat::failure::provider_error_text(Provider::Codex, item),
        Some(raw.as_str())
    );
}

#[test]
fn child_terminal_error_retains_position_in_legacy_embedded_history() {
    let mut parent = adapter();
    let mut discovered = Step::default();
    parent
        .discover_child("child", &json!({}), &mut discovered)
        .unwrap();
    let raw = "Provider is temporarily at capacity";
    let resumed = parent
        .receive(json!({"id":discovered.writes[0]["id"],"result":{
            "thread":{"id":"child","status":{"type":"idle"},"turns":[
                {"id":"failed","status":"failed","error":{"message":raw},"items":[
                    {"id":"answer","type":"agentMessage","text":"Partial child answer"}]},
                {"id":"recovered","status":"completed","items":[
                    {"id":"answer","type":"agentMessage","text":"Recovered child answer"}]}
            ]}
        }}))
        .unwrap();
    let items = resumed
        .events
        .iter()
        .filter_map(|event| match event {
            Event::SubagentItem { id, item } if id == "child" => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].id, "failed/answer");
    assert_eq!(items[1], &turn_error_item("failed", raw));
    assert_eq!(items[2].id, "recovered/answer");
    assert!(
        !resumed
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "historical errors do not replace the active child/parent error state"
    );
}
