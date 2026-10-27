use super::*;
use crate::chat::{Role, ToolStatus};

fn item(id: &str, role: Role, text: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role,
        at: None,
        started_at: None,
        content: ItemContent::Text {
            text: text.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    }
}

fn provider_error(id: &str, provider: Provider, text: &str) -> chat::Item {
    let mut error = item(id, Role::System, text);
    assert!(chat::failure::mark_provider_error(provider, &mut error));
    error
}

fn child(id: &str, status: ToolStatus, waiting: bool) -> chat::Subagent {
    chat::Subagent {
        id: id.into(),
        kind: "reviewer".into(),
        description: format!("{id} — implementation detail with a very long descriptive name ")
            .repeat(8),
        model: None,
        status,
        waiting,
        detail: String::new(),
        background: true,
        items: Vec::new(),
    }
}

fn quota(reset: &str, request: &str) -> String {
    format!(
        "You've hit your limit · resets {reset} (America/Los_Angeles)\nAPI Error: 429\nrequest-id: {request}\nmodel: disposable-fixture"
    )
}

#[test]
fn equivalent_provider_failures_and_replay_have_one_projection_and_keep_original_sources() {
    let a = quota("1:30pm", "first");
    let b = quota("1:30pm", "second");
    let (mut runtime, handle) = frame_cost::live_runtime(&[
        item("prompt", Role::User, "Review this change"),
        provider_error("a", Provider::Claude, &a),
        provider_error("b", Provider::Claude, &b),
    ]);
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.error = Some(format!("Claude: rate_limit\n{b}"));
    handle.publish_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    let key = runtime.failure_ui.incidents[0].key.clone();
    let full = runtime.failure_details(&key).unwrap();
    assert!(full.contains(&a));
    assert!(full.contains(&b));
    assert_eq!(
        runtime.snapshot.items.len(),
        3,
        "projection must not delete provider history"
    );
    let before = runtime.failure_ui.revision;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    assert_eq!(runtime.failure_ui.revision, before);
}

#[test]
fn different_resets_and_separate_user_exchanges_keep_distinct_history() {
    let early = quota("1:30pm", "early");
    let later = quota("2:30pm", "later");
    let (runtime, _) = frame_cost::live_runtime(&[
        item("p1", Role::User, "First request"),
        provider_error("e1", Provider::Claude, &early),
        provider_error("e2", Provider::Claude, &later),
        item("p2", Role::User, "A later request"),
        provider_error("e3", Provider::Claude, &early),
    ]);
    assert_eq!(runtime.failure_ui.incidents.len(), 3);
    let keys: HashSet<_> = runtime
        .failure_ui
        .incidents
        .iter()
        .map(|incident| &incident.key)
        .collect();
    assert_eq!(keys.len(), 3);
}

#[test]
fn tool_service_errors_and_quoted_provider_errors_do_not_become_account_failures() {
    let mut service = item("service", Role::Assistant, "");
    service.content = ItemContent::Tool {
        name: "Bash".into(),
        input: "run-service".into(),
        output: "service quota exceeded; authentication failed".into(),
        status: ToolStatus::Failed,
    };
    let quoted = quota("1:30pm", "quoted");
    let (runtime, _) = frame_cost::live_runtime(&[
        service,
        item("user", Role::User, &quoted),
        item("answer", Role::Assistant, &quoted),
        item("system-quotation", Role::System, &quoted),
    ]);
    assert!(runtime.failure_ui.incidents.is_empty());
    assert!(!runtime.cache["service"].diagnostic);
    assert!(!runtime.cache["user"].diagnostic);
    assert!(!runtime.cache["answer"].diagnostic);
    assert!(!runtime.cache["system-quotation"].diagnostic);
}

#[test]
fn failed_child_detail_without_items_is_complete_copyable_and_separate_from_other_children() {
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    let mut failed = child("failed", ToolStatus::Failed, false);
    failed.detail = format!(
        "Connection reset by peer\n{}\nrequest-id: child-A",
        "diagnostic ".repeat(300)
    );
    let mut other = child("other", ToolStatus::Failed, false);
    other.detail = "request-id: child-B\nA different tool was refused".into();
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![failed.clone(), other.clone()];
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    for expected in [&failed, &other] {
        let incident = runtime
            .failure_ui
            .incidents
            .iter()
            .find(|incident| incident.child_scope.as_deref() == Some(expected.id.as_str()))
            .unwrap();
        assert_eq!(
            runtime.failure_details(&incident.key).unwrap(),
            expected.detail
        );
    }
    assert!(
        runtime.error.is_none(),
        "a child's detail is not a parent failure"
    );
}

#[test]
fn stable_child_error_sources_keep_distinct_incidents_and_exact_replay_is_idempotent() {
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    let first_id = "11111111-1111-4111-8111-111111111111:0";
    let second_id = "22222222-2222-4222-8222-222222222222:0";
    let first_raw = quota("1:30pm", "first-child-request");
    let second_raw = quota("1:30pm", "second-child-request");
    let mut agent = child("worker", ToolStatus::Failed, false);
    agent.items = vec![
        Arc::new(provider_error(first_id, Provider::Claude, &first_raw)),
        Arc::new(provider_error(second_id, Provider::Claude, &second_raw)),
    ];
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![agent];
    handle.publish_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    let mut keys = Vec::new();
    for (id, expected) in [(first_id, &first_raw), (second_id, &second_raw)] {
        let (incident, first) = runtime.failure_ui.child_item("worker", id).unwrap();
        assert!(
            first,
            "each distinct stable source owns its visible diagnostic"
        );
        assert_eq!(
            runtime.failure_details(&incident.key).as_deref(),
            Some(expected.as_str())
        );
        keys.push(incident.key.clone());
    }
    assert_ne!(keys[0], keys[1]);
    let revision = runtime.failure_ui.revision;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    assert_eq!(runtime.failure_ui.revision, revision);
    assert_eq!(runtime.snapshot.subagents[0].items.len(), 2);
    for (id, key) in [(first_id, &keys[0]), (second_id, &keys[1])] {
        assert_eq!(
            &runtime.failure_ui.child_item("worker", id).unwrap().0.key,
            key
        );
    }
}

#[test]
fn one_proven_claude_error_carrier_groups_raw_blocks_but_preserves_distinct_causes() {
    for selected_child in [false, true] {
        for unknown_first in [false, true] {
            let (mut runtime, handle) = frame_cost::live_runtime(&[]);
            let cause = quota("1:30pm", "multi-block-cause");
            let raw = "HTTP 429\nRequest body: {\"model\":\"disposable-fixture\",\"input\":\"original request\"}\nrequest-id: unclassified-raw-block";
            let carrier = "33333333-3333-4333-8333-333333333333";
            let cause_id = format!("{carrier}:{}", usize::from(unknown_first));
            let raw_id = format!("{carrier}:{}", usize::from(!unknown_first));
            let cause_item = Arc::new(provider_error(&cause_id, Provider::Claude, &cause));
            let raw_item = Arc::new(provider_error(&raw_id, Provider::Claude, raw));
            let (items, expected_copy) = if unknown_first {
                (
                    vec![raw_item.clone(), cause_item.clone()],
                    format!("{raw}\n\n---\n\n{cause}"),
                )
            } else {
                (
                    vec![cause_item.clone(), raw_item.clone()],
                    format!("{cause}\n\n---\n\n{raw}"),
                )
            };
            let first_id = items[0].id.clone();
            let mut snapshot = runtime.snapshot.as_ref().clone();
            snapshot.generation += 1;
            if selected_child {
                let mut agent = child("carrier-child", ToolStatus::Failed, false);
                agent.items = items;
                snapshot.subagents = vec![agent];
                runtime.selected_subagent = Some("carrier-child".into());
            } else {
                snapshot.items = items;
            }
            handle.publish_snapshot_for_test(snapshot.clone());
            runtime.refresh();
            assert_eq!(
                runtime.failure_ui.incidents.len(),
                1,
                "child={selected_child}, raw first={unknown_first}"
            );
            let incident = &runtime.failure_ui.incidents[0];
            assert_eq!(
                incident.kind,
                Some(chat::failure::FailureKind::AccountLimit)
            );
            assert!(incident.summary.contains("1:30pm"));
            let key = incident.key.clone();
            for id in [&cause_id, &raw_id] {
                if selected_child {
                    let (found, first) =
                        runtime.failure_ui.child_item("carrier-child", id).unwrap();
                    assert_eq!(found.key, key);
                    assert_eq!(first, id == &first_id);
                } else {
                    let found = runtime.failure_ui.item(id).unwrap();
                    assert_eq!(found.key, key);
                    assert_eq!(found.first_item.as_deref(), Some(first_id.as_str()));
                }
            }
            assert_eq!(
                runtime.failure_details(&key).as_deref(),
                Some(expected_copy.as_str())
            );
            assert_eq!(
                chat::failure::provider_error_text(Provider::Claude, &cause_item),
                Some(cause.as_str())
            );
            assert_eq!(
                chat::failure::provider_error_text(Provider::Claude, &raw_item),
                Some(raw)
            );
            let revision = runtime.failure_ui.revision;
            snapshot.generation += 1;
            handle.publish_snapshot_for_test(snapshot.clone());
            runtime.refresh();
            assert_eq!(runtime.failure_ui.incidents.len(), 1);
            assert_eq!(runtime.failure_ui.revision, revision);
            assert_eq!(
                runtime.failure_details(&key).as_deref(),
                Some(expected_copy.as_str())
            );

            let distinct =
                "Connection reset by peer\nrequest-id: distinct-classified-cause-in-same-carrier";
            let distinct_id = format!("{carrier}:2");
            let distinct_item = Arc::new(provider_error(&distinct_id, Provider::Claude, distinct));
            if selected_child {
                snapshot.subagents[0].items.push(distinct_item);
            } else {
                snapshot.items.push(distinct_item);
            }
            snapshot.generation += 1;
            handle.publish_snapshot_for_test(snapshot);
            runtime.refresh();
            assert_eq!(
                runtime.failure_ui.incidents.len(),
                2,
                "a different classified cause cannot disappear into the quota incident"
            );
            let other = runtime
                .failure_ui
                .incidents
                .iter()
                .find(|incident| incident.kind == Some(chat::failure::FailureKind::Connection))
                .unwrap();
            assert_ne!(other.key, key);
            assert_eq!(
                runtime.failure_details(&other.key).as_deref(),
                Some(distinct)
            );
            assert_eq!(
                runtime.failure_details(&key).as_deref(),
                Some(expected_copy.as_str())
            );
        }
    }
}

#[test]
fn large_diagnostic_preview_and_find_are_bounded_while_full_copy_keeps_exact_source() {
    let needle = "ISSUE1506_SUFFIX_needle_λ";
    let raw = format!(
        "{}\n{}\n{needle}\n",
        quota("1:30pm", "full-copy"),
        "technical-body ".repeat(160_000)
    );
    assert!(raw.len() >= 2 * 1024 * 1024);
    let (mut runtime, handle) =
        frame_cost::live_runtime(&[provider_error("error", Provider::Claude, &raw)]);
    let cached = &runtime.cache["error"];
    assert!(cached.diagnostic);
    assert!(cached.markdown.items().is_empty());
    assert!(cached.blocks.is_empty());
    let revision = cached.revision;
    let original = runtime.snapshot.items[0].clone();
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.detail = "Unrelated status update".into();
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert!(Arc::ptr_eq(&original, &runtime.snapshot.items[0]));
    assert_eq!(runtime.cache["error"].revision, revision);
    let key = runtime.failure_ui.incidents[0].key.clone();
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw.as_str()));
    assert_eq!(
        chat::failure::provider_error_text(Provider::Claude, &runtime.snapshot.items[0]),
        Some(raw.as_str())
    );
    assert_eq!(
        copy_text(CopyPart::Text, &runtime.snapshot.items[0].content).unwrap(),
        format!("Provider request failed\n\n{raw}")
    );
    assert!(
        runtime.failure_preview(&key).is_none(),
        "collapsed diagnostics have no prepared preview"
    );
    runtime.cache_failure_preview(&key);
    let (preview, omitted) = runtime.failure_preview(&key).unwrap();
    assert!(preview.len() <= 8192 && omitted);
    assert!(raw.starts_with(preview));
    assert!(!preview.contains(needle));
    runtime.find = Some(needle.into());
    assert_eq!(
        runtime.reveal_failure_for_find("error").as_deref(),
        Some("error")
    );
    let (preview, omitted) = runtime.failure_preview(&key).unwrap();
    assert!(preview.len() <= 8192 && omitted);
    assert!(
        preview.contains(needle),
        "Find must reveal the actual suffix in its bounded preview"
    );
    assert!(runtime.failure_ui.expanded.contains(&key));
    assert!(runtime.failure_ui.find_expanded.contains(&key));
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw.as_str()));
    runtime.restore_failure_find();
    assert!(!runtime.failure_ui.expanded.contains(&key));
    assert!(runtime.failure_preview(&key).is_none());
}

#[test]
fn manually_expanded_details_survive_find_query_changes_and_close_without_new_publication() {
    let needle = "ISSUE1506_MANUAL_DETAILS_SUFFIX_λ";
    let raw = format!(
        "{}\n{}\n{needle}\n",
        quota("1:30pm", "manual-open"),
        "prefix ".repeat(10_000)
    );
    let (mut runtime, _) =
        frame_cost::live_runtime(&[provider_error("manual-error", Provider::Claude, &raw)]);
    let generation = runtime.snapshot.generation;
    let original = runtime.snapshot.items[0].clone();
    let key = runtime.failure_ui.incidents[0].key.clone();
    runtime.failure_ui.expanded.insert(key.clone());
    runtime.cache_failure_preview(&key);
    assert!(raw.starts_with(runtime.failure_preview(&key).unwrap().0));

    // The same Runtime calls made by Find: restore the previous disclosure,
    // change the query and refresh, then reveal the matched diagnostic.
    runtime.restore_find_disclosures();
    runtime.find = Some(needle.into());
    runtime.refresh_find();
    assert_eq!(
        runtime.reveal_failure_for_find("manual-error").as_deref(),
        Some("manual-error")
    );
    let (preview, omitted) = runtime.failure_preview(&key).unwrap();
    assert!(preview.len() <= 8192 && omitted && preview.contains(needle));
    assert!(
        !runtime.failure_ui.find_expanded.contains(&key),
        "Find must preserve the reader's already open panel"
    );

    runtime.restore_find_disclosures();
    runtime.find = Some("UNRELATED_QUERY_NOT_PRESENT_IN_THE_DIAGNOSTIC".into());
    runtime.refresh_find();
    assert!(runtime.find_matches.is_empty());
    assert!(runtime.failure_ui.expanded.contains(&key));
    let (preview, omitted) = runtime.failure_preview(&key).unwrap();
    assert!(preview.len() <= 8192 && omitted);
    assert!(raw.starts_with(preview));
    assert!(
        !preview.contains(needle),
        "an unrelated Find must not keep the old suffix window"
    );

    // CloseFind restores temporary disclosure, clears Find and refreshes the
    // remaining manually expanded preview without waiting for provider data.
    runtime.restore_find_disclosures();
    runtime.find = None;
    runtime.refresh_failure_previews();
    assert!(runtime.failure_ui.expanded.contains(&key));
    let (preview, omitted) = runtime.failure_preview(&key).unwrap();
    assert!(!preview.is_empty() && preview.len() <= 8192 && omitted);
    assert!(raw.starts_with(preview));
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw.as_str()));
    assert_eq!(runtime.snapshot.generation, generation);
    assert!(Arc::ptr_eq(&original, &runtime.snapshot.items[0]));
}

#[test]
fn one_working_child_remains_in_the_footer_while_the_parent_waits_or_is_blocked() {
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.subagents = vec![child("only-worker", ToolStatus::Running, false)];
    let mut publish = |snapshot: &mut chat::Snapshot| {
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot.clone());
        runtime.refresh();
        runtime.conversation_status(false)
    };
    assert_eq!(
        publish(&mut snapshot),
        (Some(Icon::AgentWorking), "Subagents working".into())
    );

    snapshot.waiting = true;
    let parent_and_child = (
        Some(Icon::AgentWorking),
        "Waiting for you · 1 working".to_owned(),
    );
    assert_eq!(publish(&mut snapshot), parent_and_child);
    snapshot.detail = "Background command completed".into();
    assert_eq!(publish(&mut snapshot), parent_and_child);

    // A provider question can be present without setting the parent scalar.
    snapshot.waiting = false;
    snapshot.requests.push(chat::Request {
        kind: chat::RequestKind::Question,
        id: "parent-question".into(),
        title: "Choose the next step".into(),
        description: "The child is independently still working".into(),
        fields: Vec::new(),
        actions: Vec::new(),
    });
    assert_eq!(publish(&mut snapshot), parent_and_child);

    snapshot.error = Some(quota("1:30pm", "single-child-parent"));
    assert_eq!(
        publish(&mut snapshot),
        (
            Some(Icon::AgentWorking),
            "Account limit reached · 1 working".into(),
        )
    );
    snapshot.subagents[0].waiting = true;
    assert_eq!(
        publish(&mut snapshot),
        (
            Some(Icon::Warning),
            "Account limit reached · 1 waiting".into(),
        )
    );
    snapshot.subagents[0].status = ToolStatus::Succeeded;
    snapshot.subagents[0].waiting = false;
    assert_eq!(
        publish(&mut snapshot),
        (Some(Icon::Warning), "Account limit reached".into())
    );
    snapshot.error = None;
    for status in [
        ToolStatus::Succeeded,
        ToolStatus::Failed,
        ToolStatus::Stopped,
    ] {
        snapshot.subagents[0].status = status;
        assert_eq!(
            publish(&mut snapshot),
            (Some(Icon::AgentWaiting), "Waiting for you".into())
        );
    }
    snapshot.subagents[0].status = ToolStatus::Running;
    snapshot.subagents[0].waiting = true;
    assert_eq!(
        publish(&mut snapshot),
        (Some(Icon::AgentWaiting), "Waiting for you".into())
    );
    snapshot.requests.clear();
    assert_eq!(
        publish(&mut snapshot),
        (Some(Icon::AgentWaiting), "Subagents waiting".into())
    );
}

#[test]
fn mixed_blocked_waiting_and_working_children_do_not_hide_real_work_or_clear_parent_failure() {
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.error = Some(quota("1:30pm", "parent"));
    snapshot.subagents = vec![
        child("running", ToolStatus::Running, false),
        child("waiting", ToolStatus::Running, true),
        child("failed", ToolStatus::Failed, false),
    ];
    handle.publish_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    let (icon, status) = runtime.conversation_status(false);
    assert_eq!(icon, Some(Icon::AgentWorking));
    for text in [
        "Account limit reached",
        "1 working",
        "1 waiting",
        "1 failed",
    ] {
        assert!(status.contains(text), "{status}");
    }
    snapshot.generation += 1;
    snapshot.detail = "Background command completed".into();
    handle.publish_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    assert_eq!(runtime.conversation_status(false).1, status);
    snapshot.generation += 1;
    snapshot.subagents[0].status = ToolStatus::Succeeded;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    let (icon, status) = runtime.conversation_status(false);
    assert_ne!(icon, Some(Icon::AgentWorking));
    assert!(status.contains("Account limit reached"));
    assert!(!status.contains("working"));
}

#[test]
fn generic_local_error_does_not_mask_an_active_provider_failure_in_the_footer() {
    let local =
        "The image insertion point changed while loading. Attach the image again at the new caret.";
    let local_capacity = "overloaded_error: provider temporarily at capacity";
    for (provider_error, label) in [
        (quota("1:30pm", "snapshot-quota"), "Account limit reached"),
        (
            "authentication_error: invalid credentials".into(),
            "Sign-in required",
        ),
        ("connection reset by peer".into(), "Connection interrupted"),
    ] {
        let (mut runtime, handle) = frame_cost::live_runtime(&[]);
        let mut snapshot = runtime.snapshot.as_ref().clone();
        snapshot.generation += 1;
        snapshot.error = Some(provider_error.clone());
        snapshot.subagents = vec![child("independent-worker", ToolStatus::Running, false)];
        runtime.error = Some(local.into());
        handle.publish_snapshot_for_test(snapshot.clone());
        runtime.refresh();
        assert_eq!(
            runtime.conversation_status(false),
            (Some(Icon::AgentWorking), format!("{label} · 1 working"))
        );
        let details: Vec<_> = runtime
            .failure_ui
            .incidents
            .iter()
            .filter_map(|incident| runtime.failure_details(&incident.key))
            .collect();
        assert!(details.iter().any(|raw| raw == local));
        assert!(details.iter().any(|raw| raw == &provider_error));

        // A recognized local failure keeps its existing precedence. The
        // independent provider incident and exact original remain available.
        runtime.error = Some(local_capacity.into());
        runtime.reconcile_failure_ui();
        assert_eq!(
            runtime.conversation_status(false),
            (
                Some(Icon::AgentWorking),
                "Provider at capacity · 1 working".into(),
            )
        );
        assert!(runtime.failure_ui.incidents.iter().any(|incident| {
            runtime.failure_details(&incident.key).as_deref() == Some(provider_error.as_str())
        }));
        runtime.error = None;
        runtime.reconcile_failure_ui();
        assert_eq!(
            runtime.conversation_status(false),
            (Some(Icon::AgentWorking), format!("{label} · 1 working"))
        );

        // Clearing provider state must not promote a historical failure into
        // the active footer, whether a generic local error remains or not.
        runtime.error = Some(local.into());
        snapshot.generation += 1;
        snapshot.error = None;
        handle.publish_snapshot_for_test(snapshot.clone());
        runtime.refresh();
        assert_eq!(
            runtime.conversation_status(false),
            (Some(Icon::AgentWorking), "Subagents working".into())
        );
        runtime.error = None;
        runtime.reconcile_failure_ui();
        snapshot.generation += 1;
        snapshot.subagents[0].status = ToolStatus::Succeeded;
        handle.publish_snapshot_for_test(snapshot);
        runtime.refresh();
        assert_eq!(runtime.conversation_status(false), (None, "Ready".into()));
    }
}

#[test]
fn local_error_set_change_and_clear_reconcile_without_a_snapshot_publication() {
    let (mut runtime, _) = frame_cost::live_runtime(&[]);
    assert!(runtime.failure_ui.incidents.is_empty());
    let generation = runtime.snapshot.generation;
    let first = quota("1:30pm", "local-first");
    runtime.error = Some(first.clone());
    assert!(runtime.reconcile_failure_ui());
    assert_eq!(runtime.snapshot.generation, generation);
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    let key = runtime.failure_ui.incidents[0].key.clone();
    assert_eq!(
        runtime.failure_details(&key).as_deref(),
        Some(first.as_str())
    );
    assert!(
        !runtime.reconcile_failure_ui(),
        "an unchanged local error must not invalidate every frame"
    );
    let changed = quota("2:30pm", "local-changed");
    runtime.error = Some(changed.clone());
    assert!(runtime.reconcile_failure_ui());
    assert_eq!(runtime.snapshot.generation, generation);
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    let changed_key = runtime.failure_ui.incidents[0].key.clone();
    assert_ne!(key, changed_key);
    assert!(runtime.failure_details(&key).is_none());
    assert_eq!(
        runtime.failure_details(&changed_key).as_deref(),
        Some(changed.as_str())
    );
    runtime.error = None;
    assert!(runtime.reconcile_failure_ui());
    assert!(runtime.failure_ui.incidents.is_empty());
    assert!(runtime.failure_details(&changed_key).is_none());
    assert!(!runtime.reconcile_failure_ui());
}

#[test]
fn historical_auth_diagnostics_survive_without_reactivating_recovery_after_new_work() {
    let raw = "Your access token could not be refreshed. Please sign in again.\nrequest-id: historical-auth";
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    runtime.session.launch.provider = Provider::Codex;
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.items = vec![
        Arc::new(item("p1", Role::User, "Earlier request")),
        Arc::new(provider_error("auth-old", Provider::Codex, raw)),
        Arc::new(item("p2", Role::User, "A later request after recovery")),
        Arc::new(item("answer", Role::Assistant, "Retained successful work")),
    ];
    handle.publish_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    let old = runtime.failure_ui.item("auth-old").unwrap();
    let old_key = old.key.clone();
    assert_eq!(old.kind, Some(chat::failure::FailureKind::Authentication));
    assert!(!old.active_recovery);
    assert!(runtime.auth_error().is_none());
    assert_eq!(runtime.failure_details(&old_key).as_deref(), Some(raw));
    snapshot.generation += 1;
    snapshot.error = Some(raw.into());
    handle.publish_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    assert!(runtime.auth_error().is_some());
    assert!(!runtime.failure_ui.item("auth-old").unwrap().active_recovery);
    assert_eq!(
        runtime
            .failure_ui
            .incidents
            .iter()
            .filter(|incident| incident.active_recovery)
            .count(),
        1
    );
    snapshot.generation += 1;
    snapshot.error = None;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert!(runtime.auth_error().is_none());
    assert!(
        runtime
            .failure_ui
            .incidents
            .iter()
            .all(|incident| !incident.active_recovery)
    );
    assert_eq!(runtime.failure_details(&old_key).as_deref(), Some(raw));
}

#[test]
fn waiting_child_quota_detail_without_items_is_projected_and_does_not_hide_other_work() {
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    let raw = quota("1:30pm", "waiting-child-only");
    let mut waiting = child("blocked-child", ToolStatus::Running, true);
    waiting.detail = raw.clone();
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![waiting, child("active-child", ToolStatus::Running, false)];
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    let incident = &runtime.failure_ui.incidents[0];
    assert_eq!(
        incident.kind,
        Some(chat::failure::FailureKind::AccountLimit)
    );
    assert!(runtime.incident_has_child_detail(incident, "blocked-child"));
    assert_eq!(
        runtime.failure_details(&incident.key).as_deref(),
        Some(raw.as_str())
    );
    assert!(runtime.snapshot.subagents[0].items.is_empty());
    let (icon, status) = runtime.conversation_status(false);
    assert_eq!(icon, Some(Icon::AgentWorking));
    assert!(status.contains("1 working"), "{status}");
    assert!(status.contains("1 waiting"), "{status}");
    assert!(runtime.error.is_none());
}

#[test]
fn child_picker_filters_full_names_and_keeps_order_during_status_changes() {
    let mut children = vec![
        child("finished", ToolStatus::Succeeded, false),
        child("working", ToolStatus::Running, false),
        child("failed", ToolStatus::Failed, false),
    ];
    let mut picker = child_navigation::Picker::new(&children);
    let order: Vec<_> = picker
        .rows(&children)
        .iter()
        .map(|child| child.id.clone())
        .collect();
    children[1].status = ToolStatus::Failed;
    assert_eq!(
        picker
            .rows(&children)
            .iter()
            .map(|child| child.id.clone())
            .collect::<Vec<_>>(),
        order
    );
    picker.query = "working".into();
    assert_eq!(picker.rows(&children).len(), 1);
    picker.query.clear();
    picker.filter = child_navigation::Filter::Attention;
    assert_eq!(picker.rows(&children).len(), 2);
    picker.filter = child_navigation::Filter::Working;
    assert!(picker.rows(&children).is_empty());
}

#[test]
fn narrow_child_navigation_stays_bounded_with_a_thousand_long_names() {
    use iced::advanced::{layout, widget::Tree};
    fn nodes(tree: &Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }
    let (mut runtime, handle) = frame_cost::live_runtime(&[]);
    let mut snapshot = runtime.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.subagents = (0..1000)
        .map(|n| {
            child(
                &format!("worker-{n}"),
                if n == 999 {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Running
                },
                false,
            )
        })
        .collect();
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    let renderer = crate::app::tests::headless_renderer();
    for width in [260.0, 360.0, 900.0] {
        for open in [false, true] {
            runtime.child_picker =
                open.then(|| child_navigation::Picker::new(&runtime.snapshot.subagents));
            for height in [180.0, 600.0] {
                // The navbar contains only the trigger. Exercise the actual
                // in-pane chooser, including its bounded page of choices.
                let mut element = if open {
                    runtime.child_picker_view().expect("open chooser")
                } else {
                    runtime.child_navigation(width)
                };
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, height)),
                );
                assert!(
                    node.size().width <= width && node.size().height <= height,
                    "pane {width}×{height}: {:?}",
                    node.size()
                );
                assert!(
                    nodes(&tree) < 2000,
                    "only a bounded page of child controls may be mounted"
                );
                let accessible = element.as_widget().a11y_nodes(
                    iced::advanced::Layout::new(&node),
                    &tree,
                    iced::mouse::Cursor::Unavailable,
                );
                let choices: Vec<_> = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .filter(|node| {
                        node.node().role() == iced_accessibility::accesskit::Role::Button
                    })
                    .filter_map(|node| node.node().label())
                    .filter(|label| {
                        label
                            .rsplit_once(" · ")
                            .is_some_and(|(_, id)| id.starts_with("worker-"))
                    })
                    .collect();
                assert_eq!(choices.len(), if open { 24 } else { 0 });
                if open {
                    let mut actual = choices.into_iter().map(str::to_owned).collect::<Vec<_>>();
                    let mut expected = runtime
                        .child_picker
                        .as_ref()
                        .unwrap()
                        .rows(&runtime.snapshot.subagents)
                        .into_iter()
                        .take(24)
                        .map(|child| {
                            format!(
                                "{} · {} · {}",
                                Runtime::native_child_name(child),
                                Runtime::native_child_status(child),
                                child.id
                            )
                        })
                        .collect::<Vec<_>>();
                    actual.sort();
                    expected.sort();
                    assert_eq!(
                        actual, expected,
                        "AX choices keep complete names and exact child identities"
                    );
                }
            }
        }
    }
}

#[test]
fn child_chooser_actions_preserve_reader_and_cover_transcript_until_closed() {
    use iced::runtime::user_interface::{Cache, UserInterface};
    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match app.cards.get(&target.card) {
            Some(CardRuntime::Chat(runtime)) => runtime,
            _ => panic!("fixture chat remains mounted"),
        }
    }
    fn labels(app: &Cagents, target: Target, cache: Cache) -> (Vec<String>, Cache) {
        let live = runtime(app, target);
        let mut renderer = crate::app::tests::headless_renderer();
        let ui = UserInterface::build(
            live.view(
                None,
                false,
                true,
                &app.settings.keyboard_shortcuts,
                360.0,
                Place {
                    name: "chooser-fixture".into(),
                    branch: None,
                    channel: None,
                },
                true,
                None,
                Vec::new(),
            ),
            iced::Size::new(360.0, 720.0),
            cache,
            &mut renderer,
        );
        let accessible = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let labels = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect();
        (labels, ui.into_cache())
    }
    let (mut live, handle) = frame_cost::live_runtime(&[]);
    let mut snapshot = live.snapshot.as_ref().clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    snapshot.has_later = true;
    let mut selected = child("chosen-worker", ToolStatus::Running, false);
    selected.items.push(Arc::new(item(
        "child-answer",
        Role::Assistant,
        "Retain this child's reading position.",
    )));
    selected.items.push(Arc::new(provider_error(
        "chooser-quota",
        Provider::Claude,
        &quota("1:30pm", "chooser-visible-diagnostics"),
    )));
    snapshot.subagents = vec![selected, child("other-worker", ToolStatus::Failed, false)];
    handle.publish_snapshot_for_test(snapshot);
    live.refresh();
    let mut app = crate::app::tests::context_inspector_test_app(true);
    app.context_inspector_open = false;
    let mut target = live.target;
    target.card = app.workspace.focused().unwrap();
    live.target = target;
    app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(live)));
    let main_reader = Scroll::new(321.0, 4000.0, 600.0);
    let child_reader = Scroll::new(173.0, 3000.0, 600.0);
    drop(app.handle_chat_ui(Action::Scrolled(target, None, main_reader, true)));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("chosen-worker".into()))));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        Some("chosen-worker".into()),
        child_reader,
        true,
    )));
    let publication = runtime(&app, target).snapshot.clone();
    let (before, mut cache) = labels(&app, target, Cache::new());
    // Diagnostic controls are ordinary buttons; prose Copy is hover-only.
    let copy_labels: Vec<_> = before
        .iter()
        .filter(|label| label.starts_with("Copy original diagnostics for "))
        .collect();
    assert_eq!(copy_labels.len(), 1, "{before:?}");
    let diagnostic_copy = copy_labels[0].to_string();
    for action in [
        Action::SubagentPicker(target, true),
        Action::SubagentQuery(target, "other-worker".into()),
        Action::SubagentQuery(target, "chosen-worker".into()),
        Action::SubagentPicker(target, false),
    ] {
        drop(app.handle_chat_ui(action));
        let live = runtime(&app, target);
        assert_eq!(live.selected_subagent.as_deref(), Some("chosen-worker"));
        assert_eq!(live.scroll, main_reader);
        assert_eq!(live.active_scroll(), child_reader);
        assert!(Arc::ptr_eq(&live.snapshot, &publication));
        assert!(
            live.pending.is_none(),
            "chooser actions must not submit or interrupt work"
        );
        assert!(live.interrupt_armed.is_none());
        assert!(live.page_request.is_none());
        assert!(
            live.deferred_page.is_none(),
            "chooser search must not request Latest/history"
        );
        let (current, next_cache) = labels(&app, target, cache);
        cache = next_cache;
        if let Some(picker) = &live.child_picker {
            assert_eq!(live.focus_control(), "chat-subagent-search");
            assert!(
                current
                    .iter()
                    .any(|label| label == "Close subagent chooser — Escape")
            );
            assert!(
                !current.iter().any(|label| label == &diagnostic_copy),
                "covered transcript actions must be absent from AX"
            );
            let expected: Vec<_> = picker
                .rows(&live.snapshot.subagents)
                .into_iter()
                .map(|child| {
                    format!(
                        "{} · {} · {}",
                        Runtime::native_child_name(child),
                        Runtime::native_child_status(child),
                        child.id
                    )
                })
                .collect();
            let actual: Vec<_> = current
                .iter()
                .filter(|label| {
                    label.ends_with(" · chosen-worker") || label.ends_with(" · other-worker")
                })
                .cloned()
                .collect();
            assert_eq!(actual, expected);
        } else {
            assert!(current.iter().any(|label| label == &diagnostic_copy));
        }
    }
    // A publication can remove the final child while the chooser is open.
    // Its own Close remains reachable even though the navbar trigger vanishes.
    drop(app.handle_chat_ui(Action::SubagentPicker(target, true)));
    let mut snapshot = publication.as_ref().clone();
    snapshot.generation += 1;
    snapshot.subagents.clear();
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let (empty, _) = labels(&app, target, cache);
    assert!(
        empty
            .iter()
            .any(|label| label == "Close subagent chooser — Escape"),
        "{empty:?}"
    );
    assert!(
        !empty
            .iter()
            .any(|label| label.starts_with("Choose subagent:"))
    );
    drop(app.handle_chat_ui(Action::SubagentPicker(target, false)));
    assert!(runtime(&app, target).child_picker.is_none());
    assert_eq!(runtime(&app, target).focus_control(), "chat-composer");
}

#[test]
fn retained_unknown_carrier_block_does_not_keep_an_obsolete_classified_sibling() {
    for selected_child in [false, true] {
        for remove_cause in [false, true] {
            let (mut runtime, handle) = frame_cost::live_runtime(&[]);
            let carrier = "44444444-4444-4444-8444-444444444444";
            let raw_id = format!("{carrier}:0");
            let cause_id = format!("{carrier}:1");
            let raw = "HTTP 429\nOriginal request body: {\"input\":\"disposable raw data\"}";
            let old_cause = quota("1:30pm", "obsolete-cause");
            let raw_item = Arc::new(provider_error(&raw_id, Provider::Claude, raw));
            let cause_item = Arc::new(provider_error(&cause_id, Provider::Claude, &old_cause));
            let mut snapshot = runtime.snapshot.as_ref().clone();
            snapshot.generation += 1;
            let items = vec![raw_item.clone(), cause_item];
            if selected_child {
                let mut agent = child("cache-child", ToolStatus::Failed, false);
                agent.items = items;
                snapshot.subagents = vec![agent];
                runtime.selected_subagent = Some("cache-child".into());
            } else {
                snapshot.items = items;
            }
            handle.publish_snapshot_for_test(snapshot.clone());
            runtime.refresh();
            assert_eq!(runtime.failure_ui.incidents.len(), 1);
            assert_eq!(
                runtime.failure_ui.incidents[0].kind,
                Some(chat::failure::FailureKind::AccountLimit)
            );
            let obsolete_key = runtime.failure_ui.incidents[0].key.clone();
            let new_cause = "Connection reset by peer\nrequest-id: replacement-cause";
            let mut current_items = vec![raw_item.clone()];
            if !remove_cause {
                current_items.push(Arc::new(provider_error(
                    &cause_id,
                    Provider::Claude,
                    new_cause,
                )));
            }
            if selected_child {
                snapshot.subagents[0].items = current_items;
            } else {
                snapshot.items = current_items;
            }
            snapshot.generation += 1;
            handle.publish_snapshot_for_test(snapshot);
            runtime.refresh();
            let retained = if selected_child {
                &runtime.snapshot.subagents[0].items[0]
            } else {
                &runtime.snapshot.items[0]
            };
            assert!(Arc::ptr_eq(retained, &raw_item));
            assert_eq!(runtime.failure_ui.incidents.len(), 1);
            let current = &runtime.failure_ui.incidents[0];
            assert_ne!(current.key, obsolete_key);
            assert_eq!(
                current.kind,
                (!remove_cause).then_some(chat::failure::FailureKind::Connection)
            );
            assert!(runtime.failure_details(&obsolete_key).is_none());
            let expected = if remove_cause {
                raw.to_owned()
            } else {
                format!("{raw}\n\n---\n\n{new_cause}")
            };
            assert_eq!(
                runtime.failure_details(&current.key).as_deref(),
                Some(expected.as_str())
            );
            assert!(
                !runtime
                    .failure_details(&current.key)
                    .unwrap()
                    .contains(&old_cause)
            );
        }
    }
}

#[test]
fn expanded_diagnostic_preview_and_copy_follow_changed_sources_with_the_same_key_and_query() {
    // Item Arc replacement and mutable Snapshot/child detail sources all need
    // fresh preview bytes even when semantic reset, item ID and Find stay put.
    for source in ["item", "snapshot", "child-detail"] {
        let (mut runtime, handle) = frame_cost::live_runtime(&[]);
        let raw_before = format!("{}\nOLD_VIEW_λ", quota("1:30pm", "before"));
        let raw_after = format!("{}\nNEW_VIEW_λ", quota("1:30pm", "after"));
        let id = "55555555-5555-4555-8555-555555555555:0";
        let before_item = Arc::new(provider_error(id, Provider::Claude, &raw_before));
        let mut snapshot = runtime.snapshot.as_ref().clone();
        snapshot.generation += 1;
        match source {
            "item" => snapshot.items = vec![before_item.clone()],
            "snapshot" => snapshot.error = Some(raw_before.clone()),
            _ => {
                let mut agent = child("preview-child", ToolStatus::Running, true);
                agent.detail = raw_before.clone();
                snapshot.subagents = vec![agent];
                runtime.selected_subagent = Some("preview-child".into());
            }
        }
        handle.publish_snapshot_for_test(snapshot.clone());
        runtime.refresh();
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        let key = runtime.failure_ui.incidents[0].key.clone();
        runtime.find = Some("VIEW_".into());
        runtime.failure_ui.expanded.insert(key.clone());
        runtime.cache_failure_preview(&key);
        assert_eq!(runtime.failure_preview(&key).unwrap().0, raw_before);
        assert_eq!(
            runtime.failure_details(&key).as_deref(),
            Some(raw_before.as_str())
        );
        let revision = runtime.failure_ui.revision;
        match source {
            "item" => {
                snapshot.items = vec![Arc::new(provider_error(id, Provider::Claude, &raw_after))]
            }
            "snapshot" => snapshot.error = Some(raw_after.clone()),
            _ => snapshot.subagents[0].detail = raw_after.clone(),
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        runtime.refresh();
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(runtime.failure_ui.incidents[0].key, key);
        assert_ne!(
            runtime.failure_ui.revision, revision,
            "changed {source} bytes must invalidate their preview"
        );
        assert!(runtime.failure_ui.expanded.contains(&key));
        assert_eq!(runtime.find.as_deref(), Some("VIEW_"));
        let (preview, omitted) = runtime.failure_preview(&key).unwrap();
        assert!(preview.len() <= 8192);
        assert!(
            !omitted,
            "these small originals must all fit in the preview"
        );
        let expected_copy = if source == "snapshot" {
            // refresh archives each distinct provider error via note(). The
            // current source is new, while both original diagnostics remain.
            assert_eq!(runtime.snapshot.error.as_deref(), Some(raw_after.as_str()));
            assert!(runtime.failures.iter().any(|(_, text)| text == &raw_before));
            assert!(runtime.failures.iter().any(|(_, text)| text == &raw_after));
            assert!(preview.contains(&raw_before));
            assert!(
                preview.contains(&raw_after),
                "the current error must not be hidden by an older Find match"
            );
            format!("{raw_before}\n\n---\n\n{raw_after}")
        } else {
            assert_eq!(preview, raw_after);
            assert!(!preview.contains("OLD_VIEW_λ"));
            raw_after.clone()
        };
        assert_eq!(
            runtime.failure_details(&key).as_deref(),
            Some(expected_copy.as_str())
        );
        if source == "item" {
            assert!(!Arc::ptr_eq(&before_item, &runtime.snapshot.items[0]));
            assert_eq!(
                chat::failure::provider_error_text(Provider::Claude, &before_item),
                Some(raw_before.as_str())
            );
            assert_eq!(
                copy_text(CopyPart::Text, &runtime.snapshot.items[0].content).unwrap(),
                format!("Provider request failed\n\n{raw_after}")
            );
        }
    }
}

fn task_report(id: &str, summary: String, body: String) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: Role::User,
        at: None,
        started_at: None,
        content: ItemContent::Delivery {
            kind: chat::DeliveryKind::Report,
            from: String::new(),
            sender: "task-report-sender".into(),
            summary,
            body,
        },
    }
}

#[derive(Default)]
struct ReportSurface {
    text: Vec<String>,
    accessible: Vec<String>,
}

fn report_surface(runtime: &Runtime, id: &str) -> ReportSurface {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    impl Operation for ReportSurface {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
            self.text.push(text.to_owned());
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                self.text.push(prose.plain.to_string());
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let item = runtime.active_item(id).expect("current report source");
    let mut element = runtime.delivery_view(
        item,
        &runtime.presentation_cache()[id],
        design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE),
    );
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(360.0, 12000.0)),
    );
    let mut result = ReportSurface::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut result);
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    result.accessible = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .flat_map(|node| [node.node().label(), node.node().value()])
        .flatten()
        .map(str::to_owned)
        .collect();
    result
}

#[test]
fn task_reports_hide_short_and_huge_bodies_without_losing_outcome_or_exact_copy() {
    use iced::futures::StreamExt;
    let long_name =
        "Review implementation and ownership with a deliberately long task name ".repeat(8);
    for (outcome, body) in [
        (
            " finished",
            "SHORT_REPORT_BODY quota exceeded is quoted task output, not a parent failure."
                .to_owned(),
        ),
        (
            " failed",
            format!(
                "HUGE_RAW_REPORT_BODY\n{}\nHUGE_REPORT_END",
                "{\"request\":\"quota exceeded\",\"raw\":true}\n".repeat(60_000)
            ),
        ),
        (
            " was stopped",
            "STOPPED_REPORT_BODY with raw diagnostics".to_owned(),
        ),
    ] {
        let heading = format!("Agent \"{long_name}\"{outcome}");
        if outcome == " failed" {
            assert!(
                body.len() >= 2 * 1024 * 1024,
                "exercise a genuinely huge raw report"
            );
        }
        let report = task_report("task-report", heading.clone(), body.clone());
        let (runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&report));
        let target = runtime.target;
        let original = runtime.snapshot.items[0].clone();
        assert!(
            runtime.failure_ui.incidents.is_empty(),
            "report prose is not a provider error carrier"
        );
        let cached = &runtime.cache["task-report"];
        assert!(cached.markdown.items().is_empty());
        assert!(cached.blocks.is_empty());
        assert!(!cached.source.provisional());
        assert_eq!(cached.source.retained_tail_capacity(), 0);
        let surface = report_surface(&runtime, "task-report");
        assert!(
            surface
                .accessible
                .contains(&format!("Show report details: {heading}"))
        );
        assert!(
            surface
                .accessible
                .contains(&format!("Copy original report: {heading}"))
        );
        assert!(!surface.text.iter().any(|text| text == &heading));
        assert!(
            surface.text.iter().any(|text| text.ends_with(outcome)
                && text.contains('…')
                && text.chars().count() <= 73 + outcome.chars().count()),
            "compact task identity must preserve {outcome}: {:?}",
            surface.text
        );
        let marker = body.lines().next().unwrap();
        assert!(
            !surface
                .text
                .iter()
                .chain(&surface.accessible)
                .any(|text| text.contains(marker)),
            "even short report bodies stay behind Details"
        );
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        let copy = app.handle_chat_ui(Action::Copy(target, "task-report".into(), CopyPart::Text));
        let copied = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut effects =
                    iced::runtime::task::into_stream(copy).expect("actual Copy action");
                tokio::time::timeout(Duration::from_secs(1), async {
                    while let Some(effect) = effects.next().await {
                        if let iced::runtime::Action::Clipboard(
                            iced::runtime::clipboard::Action::Write { contents, .. },
                        ) = effect
                        {
                            return contents;
                        }
                    }
                    panic!("Copy report must publish original bytes");
                })
                .await
                .expect("bounded clipboard effect")
            });
        assert_eq!(copied, format!("{heading}\n\n{body}"));
        let live = app.chat_runtime_mut(target).unwrap();
        assert!(Arc::ptr_eq(&original, &live.snapshot.items[0]));
        assert_eq!(live.snapshot.items[0].as_ref(), &report);
        assert!(
            live.cache["task-report"].markdown.items().is_empty(),
            "Copy must not parse hidden Markdown"
        );
    }
}

#[test]
fn report_details_and_late_find_prepare_original_markdown_and_restore_disclosure() {
    let needle = "REPORT_LATE_FIND_NEEDLE";
    let body = format!(
        "# Full Markdown report\n\n{}\n```rust\nlet report_value = 7;\n```\n\n{needle}\n",
        "A report paragraph with quoted quota exceeded diagnostic prose.\n\n".repeat(48)
    );
    let heading = "Agent \"Detailed review\" finished";
    for selected_child in [false, true] {
        let report = task_report("disclosed-report", heading.into(), body.clone());
        let (mut runtime, handle) = frame_cost::live_runtime(if selected_child {
            &[]
        } else {
            std::slice::from_ref(&report)
        });
        if selected_child {
            let mut snapshot = runtime.snapshot.as_ref().clone();
            snapshot.generation += 1;
            let mut agent = child("report-child", ToolStatus::Succeeded, false);
            agent.items = vec![Arc::new(report.clone())];
            snapshot.subagents = vec![agent];
            handle.publish_snapshot_for_test(snapshot);
            runtime.refresh();
        }
        let target = runtime.target;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        if selected_child {
            drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("report-child".into()))));
        }
        let original = app
            .chat_runtime_mut(target)
            .unwrap()
            .active_item("disclosed-report")
            .unwrap()
            .clone();
        for temporary_find in [false, true] {
            if temporary_find {
                drop(find_navigation::find_for_test(
                    &mut app,
                    target,
                    needle.into(),
                ));
            } else {
                drop(app.handle_chat_ui(Action::Toggle(target, "disclosed-report".into())));
            }
            let live = app.chat_runtime_mut(target).unwrap();
            preparation::drain_fixture(live);
            assert!(
                live.item_expanded("disclosed-report"),
                "child={selected_child}, Find={temporary_find}"
            );
            let cached = &live.presentation_cache()["disclosed-report"];
            assert!(
                !cached.markdown.items().is_empty(),
                "explicit disclosure must prepare original Markdown"
            );
            assert!(
                !cached.blocks.is_empty(),
                "the original fenced code block is prepared too"
            );
            assert!(!cached.source.provisional());
            let surface = report_surface(live, "disclosed-report");
            assert!(surface.text.iter().any(|text| text == heading));
            assert!(
                surface.text.iter().any(|text| text.contains(needle)),
                "the late body match must actually be rendered"
            );
            assert!(
                surface
                    .accessible
                    .contains(&format!("Hide report details: {heading}"))
            );
            assert!(live.failure_ui.incidents.is_empty());
            if temporary_find {
                assert!(!live.find_matches.is_empty());
                drop(app.handle_chat_ui(Action::CloseFind(target)));
            } else {
                drop(app.handle_chat_ui(Action::Toggle(target, "disclosed-report".into())));
            }
            let live = app.chat_runtime_mut(target).unwrap();
            assert!(!live.item_expanded("disclosed-report"));
            let cached = &live.presentation_cache()["disclosed-report"];
            assert!(
                cached.markdown.items().is_empty(),
                "collapse releases parsed report Markdown"
            );
            assert!(cached.blocks.is_empty());
            assert!(!cached.source.provisional());
            assert_eq!(cached.source.retained_tail_capacity(), 0);
            let collapsed = report_surface(live, "disclosed-report");
            assert!(
                !collapsed
                    .text
                    .iter()
                    .chain(&collapsed.accessible)
                    .any(|text| text.contains(needle))
            );
            assert!(Arc::ptr_eq(
                &original,
                live.active_item("disclosed-report").unwrap()
            ));
            assert_eq!(
                copy_text(CopyPart::Text, &original.content),
                Some(format!("{heading}\n\n{body}"))
            );
        }
        // The full opaque sender is indexed even when its suffix is absent
        // from the compact/full heading and the report body.
        let sender_suffix = "report-sender";
        assert!(!heading.contains(sender_suffix) && !body.contains(sender_suffix));
        drop(find_navigation::find_for_test(
            &mut app,
            target,
            sender_suffix.into(),
        ));
        let live = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(live);
        assert!(!live.find_matches.is_empty());
        assert!(
            live.item_expanded("disclosed-report"),
            "metadata Find must reveal the matching report"
        );
        let metadata = report_surface(live, "disclosed-report");
        assert!(
            metadata
                .text
                .iter()
                .any(|text| text == "Sender: task-report-sender"),
            "the indexed opaque sender suffix must actually be visible"
        );
        assert_eq!(
            copy_text(CopyPart::Text, &original.content),
            Some(format!("{heading}\n\n{body}"))
        );
        drop(app.handle_chat_ui(Action::CloseFind(target)));
        let live = app.chat_runtime_mut(target).unwrap();
        assert!(!live.item_expanded("disclosed-report"));
        assert!(
            live.presentation_cache()["disclosed-report"]
                .markdown
                .items()
                .is_empty()
        );
        assert!(
            !report_surface(live, "disclosed-report")
                .text
                .iter()
                .any(|text| text.contains(sender_suffix))
        );

        // Search restores its own temporary disclosure, never a reader's
        // explicit Details choice, even when no publication arrives.
        drop(app.handle_chat_ui(Action::Toggle(target, "disclosed-report".into())));
        drop(find_navigation::find_for_test(
            &mut app,
            target,
            needle.into(),
        ));
        drop(app.handle_chat_ui(Action::CloseFind(target)));
        let live = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(live);
        assert!(live.item_expanded("disclosed-report"));
        assert!(
            !live.presentation_cache()["disclosed-report"]
                .markdown
                .items()
                .is_empty()
        );
        assert!(Arc::ptr_eq(
            &original,
            live.active_item("disclosed-report").unwrap()
        ));
    }
}

#[test]
fn report_async_disclosure_closes_without_resurrection_from_queued_preparation() {
    let report = task_report(
        "queued-report",
        "Agent review finished".into(),
        format!(
            "# Full report\n\n{}\n```rust\nlet ready = true;\n```",
            "body paragraph\n\n".repeat(512)
        ),
    );
    let (mut runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&report));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    {
        let live = app.chat_runtime_mut(target).unwrap();
        assert!(!live.preparation.contains("queued-report"));
        assert!(
            live.presentation_cache()["queued-report"]
                .markdown
                .items()
                .is_empty()
        );
    }
    drop(app.handle_chat_ui(Action::Toggle(target, "queued-report".into())));
    {
        let live = app.chat_runtime_mut(target).unwrap();
        assert!(live.item_expanded("queued-report"));
        assert!(live.preparation.contains("queued-report"));
        let cached = &live.presentation_cache()["queued-report"];
        assert!(cached.preparing && cached.placeholder);
        assert!(
            cached.markdown.items().is_empty(),
            "opening only admits work; UI must not parse it"
        );
    }
    drop(app.handle_chat_ui(Action::Toggle(target, "queued-report".into())));
    {
        let live = app.chat_runtime_mut(target).unwrap();
        assert!(!live.item_expanded("queued-report"));
        // Complete the already admitted original job after the reader closed
        // it. The actual Ready handler must discard this now-unwanted body.
        preparation::drain_fixture(live);
        let cached = &live.presentation_cache()["queued-report"];
        assert!(cached.markdown.items().is_empty());
        assert!(cached.blocks.is_empty());
        assert_eq!(cached.report_expanded, Some(false));
        assert!(!live.preparation.contains("queued-report"));
    }
    drop(app.handle_chat_ui(Action::Toggle(target, "queued-report".into())));
    let live = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(live);
    let cached = &live.presentation_cache()["queued-report"];
    assert!(!cached.placeholder && !cached.markdown.items().is_empty());
    assert!(!cached.blocks.is_empty());
    assert_eq!(cached.report_expanded, Some(true));
    assert_eq!(
        copy_text(CopyPart::Text, &report.content),
        copy_text(
            CopyPart::Text,
            &live.active_item("queued-report").unwrap().content
        )
    );
}

#[test]
fn busy_report_ready_and_collapse_release_actual_worker_canonical_and_palette() {
    for selected_child in [false, true] {
        let report = task_report(
            "busy-report",
            "Completed delivery while another operation is working".into(),
            format!(
                "{}\n```rust\nlet actual = true;\n```",
                "Report body\n\n".repeat(512)
            ),
        );
        let (mut runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&report));
        let target = runtime.target;
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.busy = !selected_child;
        if selected_child {
            let mut agent = child("running-report-owner", ToolStatus::Running, false);
            agent.items = vec![Arc::new(report.clone())];
            snapshot.subagents = vec![agent];
        }
        handle.publish_snapshot_for_test(snapshot);
        runtime.refresh();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        if selected_child {
            drop(app.handle_chat_ui(Action::SelectSubagent(
                target,
                Some("running-report-owner".into()),
            )));
        }
        drop(app.handle_chat_ui(Action::Toggle(target, "busy-report".into())));
        let palette = {
            let live = app.chat_runtime_mut(target).unwrap();
            preparation::drain_fixture(live);
            assert!(live.item_expanded("busy-report"));
            let cached = &live.presentation_cache()["busy-report"];
            assert!(!cached.placeholder && !cached.markdown.items().is_empty());
            let palette = Arc::downgrade(cached.blocks.values().next().unwrap().spans(false));
            assert!(palette.upgrade().is_some());
            assert!(
                !live.preparation.contains("busy-report"),
                "a finite Report Ready removes the Shared/canonical owner even while busy"
            );
            palette
        };
        drop(app.handle_chat_ui(Action::Toggle(target, "busy-report".into())));
        preparation::retirement_barrier();
        let live = app.chat_runtime_mut(target).unwrap();
        assert!(!live.item_expanded("busy-report"));
        assert!(
            live.presentation_cache()["busy-report"]
                .markdown
                .items()
                .is_empty()
        );
        assert!(live.presentation_cache()["busy-report"].blocks.is_empty());
        assert!(!live.preparation.contains("busy-report"));
        assert!(
            palette.upgrade().is_none(),
            "actual shared syntax spans must be released"
        );
        assert_eq!(
            copy_text(CopyPart::Text, &report.content),
            copy_text(
                CopyPart::Text,
                &live.active_item("busy-report").unwrap().content
            ),
        );
        // A closed same-ID replacement remains unparsed until disclosure;
        // reopening must admit the new original, never the retired canonical.
        let replacement = task_report(
            "busy-report",
            "Replacement delivery".into(),
            "NEW_REPORT_SOURCE_AFTER_CLOSE".into(),
        );
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        if selected_child {
            snapshot.subagents[0].items = vec![Arc::new(replacement.clone())];
        } else {
            snapshot.items = vec![Arc::new(replacement.clone())];
        }
        handle.publish_snapshot_for_test(snapshot);
        live.refresh();
        assert!(
            live.presentation_cache()["busy-report"]
                .markdown
                .items()
                .is_empty()
        );
        assert!(!live.preparation.contains("busy-report"));
        drop(app.handle_chat_ui(Action::Toggle(target, "busy-report".into())));
        let live = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(live);
        assert!(!live.preparation.contains("busy-report"));
        assert!(
            report_surface(live, "busy-report")
                .text
                .iter()
                .any(|text| text.contains("NEW_REPORT_SOURCE_AFTER_CLOSE"))
        );
        assert_eq!(
            copy_text(
                CopyPart::Text,
                &live.active_item("busy-report").unwrap().content
            ),
            copy_text(CopyPart::Text, &replacement.content)
        );
    }
}

#[test]
fn diagnostic_preview_widget_and_catalog_share_bounded_text_but_copy_keeps_full_original() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    use iced::futures::StreamExt;
    let raw = format!(
        "{}\n{}\nFULL_ORIGINAL_END",
        quota("1:30pm", "bounded-selection"),
        "raw diagnostic ".repeat(8192)
    );
    let (runtime, _handle) =
        frame_cost::live_runtime(&[provider_error("bounded-error", Provider::Claude, &raw)]);
    let target = runtime.target;
    let key = runtime.failure_ui.incidents[0].key.clone();
    assert!(runtime.selection_document().0.is_empty());
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    drop(app.handle_chat_ui(Action::ToggleFailure(target, key.clone())));
    let preview_owner = {
        let live = app.chat_runtime_mut(target).unwrap();
        let preview = live.failure_preview(&key).unwrap().0.to_owned();
        assert_eq!(preview.len(), 8192);
        assert!(!preview.contains("FULL_ORIGINAL_END"));
        let (catalog, bookmark) = live.selection_document();
        assert_eq!(catalog.search_blocks().len(), 1);
        let identity = failure::diagnostic_preview_identity(&key);
        let text = catalog
            .text(identity)
            .expect("actual visible preview catalog entry");
        assert_eq!(text.as_ref(), preview);
        let weak = Arc::downgrade(&text);
        let (start, end) = catalog.endpoints().unwrap();
        assert_eq!(catalog.copy(start, end).as_deref(), Some(preview.as_str()));
        let incident = &live.failure_ui.incidents[0];
        let mut element = selection::transcript_virtualized(
            live.failure_view(incident),
            target,
            None,
            catalog,
            bookmark,
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(480.0, 900.0)),
        );
        #[derive(Default)]
        struct PreviewNodes(Vec<(u64, String)>);
        impl Operation for PreviewNodes {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
                    && let Some(logical) = &prose.logical
                {
                    self.0.push((logical.identity, prose.plain.to_string()));
                }
            }
        }
        let mut actual = PreviewNodes::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        assert_eq!(actual.0, vec![(identity, preview)]);
        weak
    };
    let task = app.handle_chat_ui(Action::CopyFailure(target, key.clone()));
    let copied = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let mut effects =
                iced::runtime::task::into_stream(task).expect("actual CopyFailure action");
            tokio::time::timeout(Duration::from_secs(1), async {
                while let Some(effect) = effects.next().await {
                    if let iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    ) = effect
                    {
                        return contents;
                    }
                }
                panic!("CopyFailure must publish complete originals");
            })
            .await
            .expect("bounded clipboard action")
        });
    assert_eq!(copied, raw);
    drop(app.handle_chat_ui(Action::ToggleFailure(target, key)));
    let live = app.chat_runtime_mut(target).unwrap();
    assert!(live.selection_document().0.is_empty());
    preparation::retirement_barrier();
    assert!(
        preview_owner.upgrade().is_none(),
        "closed preview must leave no catalog plaintext owner"
    );
}

#[test]
fn hidden_diagnostic_metadata_releases_exact_source_and_restores_disclosure_on_rehydrate() {
    let raw = format!(
        "{}\n{}",
        quota("1:30pm", "hidden-metadata"),
        "original-diagnostic ".repeat(8192)
    );
    let (mut runtime, handle) =
        frame_cost::live_runtime(&[provider_error("hidden-error", Provider::Claude, &raw)]);
    let original = runtime.snapshot.items[0].clone();
    let weak = Arc::downgrade(&original);
    let key = runtime.failure_ui.incidents[0].key.clone();
    runtime.failure_ui.expanded.insert(key.clone());
    runtime.cache_failure_preview(&key);
    assert!(runtime.failure_preview(&key).is_some());
    drop(original);
    runtime.set_visible(false).unwrap();
    preparation::retirement_barrier();
    assert!(runtime.failure_ui.incidents.is_empty());
    assert!(runtime.failure_preview(&key).is_none());
    assert!(runtime.failure_ui.expanded.contains(&key));
    assert!(
        weak.upgrade().is_none(),
        "diagnostic metadata must not retain the hidden original item"
    );
    runtime.set_visible(true).unwrap();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![Arc::new(provider_error(
        "hidden-error",
        Provider::Claude,
        &raw,
    ))];
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert_eq!(runtime.failure_ui.incidents[0].key, key);
    assert!(runtime.failure_ui.expanded.contains(&key));
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw.as_str()));
    assert!(runtime.failure_preview(&key).is_some());
}
