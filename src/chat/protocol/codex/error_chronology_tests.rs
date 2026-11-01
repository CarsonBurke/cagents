use super::{tests::adapter, *};
use crate::agent::Provider;

#[test]
fn delayed_standalone_items_and_retry_receipts_keep_the_originating_turn_position() {
    for method in ["item/started", "item/completed"] {
        let raw = "Provider is temporarily at capacity";
        let start = |turn: &str, text: &str| {
            json!({"method":"turn/started","params":{"threadId":"thread-exact",
            "turn":{"id":turn,"status":"inProgress","items":[{"id":"prompt","type":"userMessage",
                "content":[{"type":"text","text":text}]}]}}})
        };
        let retry = json!({"method":"error","params":{"threadId":"thread-exact","turnId":"old-a",
            "willRetry":true,"error":{"message":raw}}});
        let (_, snapshot) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
            Box::new(adapter()),
            vec![
                json!({"method":"turn/started","params":{"threadId":"thread-exact",
                    "turn":{"id":"fixture-settle","status":"inProgress","items":[]}}}),
                json!({"method":"turn/completed","params":{"threadId":"thread-exact",
                    "turn":{"id":"fixture-settle","status":"completed","items":[]}}}),
                start("old-a", "First request"),
                start("new-b", "Continue"),
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                    "turnId":"new-b","itemId":"answer","delta":"Recovered "}}),
                json!({"method":method,"params":{"threadId":"thread-exact","turnId":"old-a",
                    "item":{"id":"answer","type":"agentMessage","text":"Partial answer"}}}),
                retry.clone(),
                retry,
                json!({"method":"error","params":{"threadId":"thread-exact","turnId":"old-a",
                    "willRetry":false,"error":{"message":raw}}}),
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                    "turnId":"new-b","itemId":"answer","delta":"answer"}}),
            ],
            false,
        );
        assert_eq!(snapshot.items.len(), 7, "{method}");
        assert_eq!(snapshot.items[0].id, "old-a/prompt");
        assert_eq!(snapshot.items[1].id, "old-a/answer");
        assert!(snapshot.items[2].id.starts_with("codex-notice:"));
        assert!(snapshot.items[3].id.starts_with("codex-notice:"));
        assert_ne!(
            snapshot.items[2].id, snapshot.items[3].id,
            "retry receipts retain distinct UUID identities"
        );
        assert_eq!(snapshot.items[4].id, turn_error_item("old-a", raw).id);
        assert_eq!(snapshot.items[5].id, "new-b/prompt");
        assert_eq!(snapshot.items[6].id, "new-b/answer");
        for index in 2..5 {
            assert_eq!(
                chat::failure::provider_error_text(Provider::Codex, &snapshot.items[index]),
                Some(raw)
            );
        }
        assert!(snapshot.busy);
        assert_eq!(snapshot.error_generation, 0);
        assert!(snapshot.error.is_none());
        assert!(matches!(&snapshot.items[6].content,
            ItemContent::Text { text, .. } if text == "Recovered answer"));
    }
}

#[test]
fn partial_overlap_history_preserves_the_last_known_item_for_a_later_error() {
    let mut adapter = adapter();
    let mut request = Step::default();
    adapter.history(&mut request, None).unwrap();
    let raw = "Provider is temporarily at capacity";
    let user = |text: &str| json!({"id":"prompt","type":"userMessage","content":[{"type":"text","text":text}]});
    let (_, snapshot) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(adapter),
        vec![
            json!({"method":"turn/started","params":{"threadId":"thread-exact",
                "turn":{"id":"fixture-settle","status":"inProgress","items":[]}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-exact",
                "turn":{"id":"fixture-settle","status":"completed","items":[]}}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                "turnId":"old-a","itemId":"answer","delta":"Partial answer"}}),
            json!({"method":"turn/started","params":{"threadId":"thread-exact",
                "turn":{"id":"new-b","status":"inProgress","items":[user("Continue")]}}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                "turnId":"new-b","itemId":"answer","delta":"Recovered "}}),
            json!({"id":request.writes[0]["id"],"result":{"data":[
                {"id":"new-b","status":"inProgress","items":[user("Continue"),
                    {"id":"answer","type":"agentMessage","text":"Recovered "}]},
                {"id":"old-a","status":"failed","items":[user("First request"),
                    {"id":"answer","type":"agentMessage","text":"Partial answer"}]}
            ],"nextCursor":null}}),
            json!({"method":"error","params":{"threadId":"thread-exact","turnId":"old-a",
                "willRetry":false,"error":{"message":raw}}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                "turnId":"new-b","itemId":"answer","delta":"answer"}}),
        ],
        false,
    );
    let error_id = turn_error_item("old-a", raw).id;
    assert_eq!(
        snapshot
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        [
            "old-a/prompt",
            "old-a/answer",
            &error_id,
            "new-b/prompt",
            "new-b/answer"
        ]
    );
    assert_eq!(snapshot.error_generation, 0);
    assert!(snapshot.busy);
}

#[test]
fn first_delayed_terminal_error_is_inserted_before_the_new_turn_without_history() {
    let raw = "Provider is temporarily at capacity\nretained original diagnostics";
    let user = |text: &str| json!({"id":"prompt","type":"userMessage","content":[{"type":"text","text":text}]});
    let start = |id: &str, items: Value| {
        json!({"method":"turn/started","params":{
            "threadId":"thread-exact","turn":{"id":id,"status":"inProgress","items":items}
        }})
    };
    for (completion, early, unseen) in [
        (false, false, false),
        (true, false, false),
        (true, false, true),
        (false, true, false),
        (false, false, true),
    ] {
        let late_failure = if completion {
            json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{
                "id":"old-a","status":"failed","error":{"message":raw},"items":[
                    user("First request"), {"id":"answer","type":"agentMessage","text":"Partial answer"}
                ]
            }}})
        } else {
            json!({"method":"error","params":{"threadId":"thread-exact","turnId":"old-a",
                "willRetry":false,"error":{"message":raw}}})
        };
        // The actor fixture requires one outcome callback. This empty
        // completed turn contributes no conversation items or errors.
        let mut frames = vec![
            start("fixture-settle", json!([])),
            json!({"method":"turn/completed","params":{"threadId":"thread-exact",
                "turn":{"id":"fixture-settle","status":"completed","items":[]}}}),
        ];
        let partial = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
            "turnId":"old-a","itemId":"answer","delta":"Partial answer"}});
        if early {
            frames.push(partial.clone());
        }
        frames.push(start("old-a", json!([user("First request")])));
        if !early && !unseen {
            frames.push(partial.clone());
        }
        frames.extend([
            start("new-b", json!([user("Continue")])),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                    "turnId":"new-b","itemId":"answer","delta":"Recovered "}}),
        ]);
        if !completion && unseen {
            frames.push(partial);
        }
        frames.extend([
            // An older item completing cannot rewind A's last item proof.
            json!({"method":"item/completed","params":{"threadId":"thread-exact",
                    "turnId":"old-a","item":user("First request")}}),
            late_failure,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                    "turnId":"new-b","itemId":"answer","delta":"answer"}}),
        ]);
        let (_, snapshot) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
            Box::new(adapter()),
            frames,
            false,
        );
        let error_id = turn_error_item("old-a", raw).id;
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [
                "old-a/prompt",
                "old-a/answer",
                &error_id,
                "new-b/prompt",
                "new-b/answer"
            ],
            "first late error must precede B before any history: completion={completion}, early={early}, unseen={unseen}"
        );
        assert_eq!(
            chat::failure::provider_error_text(Provider::Codex, &snapshot.items[2]),
            Some(raw)
        );
        assert!(snapshot.busy);
        assert!(snapshot.error.is_none());
        assert_eq!(
            snapshot.error_generation, 0,
            "an old turn cannot issue a scalar error into B"
        );
        assert!(snapshot.error_item.is_none());
        assert!(matches!(&snapshot.items[4].content,
            ItemContent::Text { text, .. } if text == "Recovered answer"));
    }
}

#[test]
fn terminal_error_placement_retains_exact_anchors_and_bounds_unproved_order() {
    let mut adapter = adapter();
    adapter.observe_turn_item("turn", "turn/prompt", false);
    adapter.observe_turn_item("turn", "turn/answer", false);
    adapter.observe_turn_item("turn", "turn/prompt", false);
    let mut step = Step::default();
    adapter.terminal_error("turn", "at capacity".into(), &mut step, true);
    let [
        Event::ItemAfter { item, after },
        Event::Error { .. },
        Event::ErrorSource { item_id },
    ] = step.events.as_slice()
    else {
        panic!("{:?}", step.events);
    };
    assert_eq!(after, "turn/answer");
    assert_eq!(item_id, &item.id);

    for index in 0..MAX_TURN_ANCHORS {
        let turn = format!("later-{index}");
        adapter.observe_turn_item(&turn, &format!("{turn}/prompt"), false);
    }
    assert_eq!(adapter.turn_anchors.len(), MAX_TURN_ANCHORS);
    let mut unproved = Step::default();
    adapter.terminal_error("turn", "different diagnostics".into(), &mut unproved, false);
    assert!(
        matches!(unproved.events.as_slice(), [Event::Item(_)]),
        "evicted turn evidence cannot borrow another turn's anchor"
    );

    let mut anchor = TurnAnchor::default();
    for index in 0..MAX_TURN_ANCHOR_ITEMS {
        anchor.observe(&index.to_string(), false);
    }
    anchor.observe("unknown-after-budget", false);
    assert!(anchor.last.is_none());
    assert_eq!(anchor.seen.len(), MAX_TURN_ANCHOR_ITEMS);
    anchor.observe("provider-ordered-last-item", true);
    assert_eq!(anchor.last.as_deref(), Some("provider-ordered-last-item"));
    anchor.observe("0", false);
    assert_eq!(anchor.last.as_deref(), Some("provider-ordered-last-item"));
}

#[test]
fn authoritative_missing_middle_content_precedes_its_streamed_successor_and_later_retry() {
    let raw = "Provider is temporarily at capacity";
    let prompt =
        json!({"id":"p","type":"userMessage","content":[{"type":"text","text":"Continue"}]});
    let answer = json!({"id":"a","type":"agentMessage","text":"Retained answer"});
    for missing_middle in [true, false] {
        let mut frames = vec![
            json!({"method":"turn/started","params":{"threadId":"thread-exact",
            "turn":{"id":"turn","status":"inProgress","items":[prompt]}}}),
        ];
        if missing_middle {
            frames.push(
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact",
                "turnId":"turn","itemId":"a","delta":"Retained answer"}}),
            );
        }
        frames.push(
            json!({"method":"error","params":{"threadId":"thread-exact","turnId":"turn",
            "willRetry":true,"error":{"message":raw}}}),
        );
        let items = if missing_middle {
            json!([prompt, {"id":"r","type":"reasoning","summary":["Earlier reasoning"]}, answer])
        } else {
            json!([prompt, answer])
        };
        frames.push(
            json!({"method":"turn/completed","params":{"threadId":"thread-exact",
            "turn":{"id":"turn","status":"completed","items":items}}}),
        );
        let (_, snapshot) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
            Box::new(adapter()),
            frames,
            false,
        );
        let retry_index = if missing_middle { 3 } else { 1 };
        let retry = &snapshot.items[retry_index];
        assert!(retry.id.starts_with("codex-notice:"));
        assert_eq!(
            chat::failure::provider_error_text(Provider::Codex, retry),
            Some(raw)
        );
        let expected = if missing_middle {
            vec!["turn/p", "turn/r/summary/0", "turn/a", retry.id.as_str()]
        } else {
            vec!["turn/p", retry.id.as_str(), "turn/a"]
        };
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(snapshot.error_generation, 0);
        assert!(snapshot.error.is_none());
    }
}

#[test]
fn newly_completed_output_keeps_an_earlier_retry_receipt_before_it() {
    let mut adapter = adapter();
    adapter.observe_turn_item("turn", "turn/prompt", false);
    adapter.observe_turn_item("turn", "codex-notice:retry-receipt", false);
    let item = Item {
        id: "turn/answer".into(),
        role: Role::Assistant,
        content: ItemContent::Text {
            text: "Recovered answer".into(),
            thinking: false,
            phase: Phase::Final,
        },
        started_at: None,
        at: None,
    };
    let mut step = Step::default();
    adapter.snapshot_item("turn", item, Some("turn/prompt"), &mut step);
    assert!(
        matches!(step.events.as_slice(), [Event::ItemAfter { after, .. }]
        if after == "codex-notice:retry-receipt")
    );
    assert_eq!(
        adapter.turn_anchors.back().unwrap().last.as_deref(),
        Some("turn/answer")
    );
}

#[test]
fn first_delayed_child_completion_preserves_child_turn_order_without_history() {
    let mut parent = adapter();
    let mut child = adapter();
    child.thread = Some("child".into());
    child.is_child = true;
    parent.children.insert("child".into(), Box::new(child));
    let raw = "Provider is temporarily at capacity";
    let user = |text: &str| json!({"id":"prompt","type":"userMessage","content":[{"type":"text","text":text}]});
    let child_start = |turn: &str, prompt: &str| {
        json!({"method":"turn/started","params":{
            "threadId":"child","turn":{"id":turn,"status":"inProgress","items":[user(prompt)]}
        }})
    };
    let (_, snapshot) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(parent),
        vec![
            json!({"method":"turn/started","params":{"threadId":"thread-exact",
                "turn":{"id":"fixture-settle","status":"inProgress","items":[]}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-exact",
                "turn":{"id":"fixture-settle","status":"completed","items":[]}}}),
            child_start("old-a", "First child request"),
            child_start("new-b", "Continue child work"),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"child",
                "turnId":"new-b","itemId":"answer","delta":"Recovered child "}}),
            json!({"method":"turn/completed","params":{"threadId":"child","turn":{
                "id":"old-a","status":"failed","error":{"message":raw},"items":[
                    user("First child request"), {"id":"answer","type":"agentMessage","text":"Partial child answer"}
                ]
            }}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"child",
                "turnId":"new-b","itemId":"answer","delta":"answer"}}),
        ],
        false,
    );
    assert!(
        snapshot.items.is_empty(),
        "child diagnostics never enter the parent transcript"
    );
    assert!(snapshot.error.is_none());
    let child = snapshot
        .subagents
        .iter()
        .find(|child| child.id == "child")
        .unwrap();
    let error_id = turn_error_item("old-a", raw).id;
    assert_eq!(
        child
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        [
            "old-a/prompt",
            "old-a/answer",
            &error_id,
            "new-b/prompt",
            "new-b/answer"
        ]
    );
    assert_eq!(child.status, Some(ToolStatus::Running));
    assert_eq!(
        chat::failure::provider_error_text(Provider::Codex, &child.items[2]),
        Some(raw)
    );
    assert!(matches!(&child.items[4].content,
        ItemContent::Text { text, .. } if text == "Recovered child answer"));
}

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
            let [Event::Item(item)] = step.events.as_slice() else {
                panic!(
                    "an old scoped failure retains only its own history: {:?}",
                    step.events
                );
            };
            assert_eq!(item, &turn_error_item("earlier-turn", raw));
            assert_eq!(
                chat::failure::provider_error_text(Provider::Codex, item),
                Some(raw)
            );
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
