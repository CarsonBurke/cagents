use super::*;

const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";

/// Exercise the catalog used by transcript selection, after the ordinary
/// asynchronous preparation jobs have installed their results. This is source
/// projection evidence, not native input, scrolling, or provider validation.
fn catalog(runtime: &mut Runtime) -> Vec<(u64, Arc<str>)> {
    preparation::drain_fixture(runtime);
    let keys: Vec<_> = runtime
        .failure_ui
        .incidents
        .iter()
        .map(|incident| incident.key.clone())
        .collect();
    for key in keys {
        runtime.failure_ui.expanded.insert(key.clone());
        runtime.cache_failure_preview(&key);
    }
    runtime.selection_document().0.search_blocks()
}

fn position(blocks: &[(u64, Arc<str>)], identity: u64) -> usize {
    blocks
        .iter()
        .position(|(found, _)| *found == identity)
        .unwrap_or_else(|| panic!("missing catalog identity {identity}: {blocks:?}"))
}

fn prompt_position(blocks: &[(u64, Arc<str>)], id: &str) -> usize {
    position(blocks, selection::logical_identity(id, &[2]))
}

fn failure_position(blocks: &[(u64, Arc<str>)], key: &str) -> usize {
    position(blocks, diagnostic_preview_identity(key))
}

#[test]
fn scalar_capacity_precedes_dot_during_streaming_and_rerender() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let prompt = part(provider, "capacity-prompt", 0);
        let dot = part(provider, "resume-dot", 0);
        let (mut runtime, handle) =
            live_fixture(provider, vec![text(&prompt, chat::Role::User, "Continue")]);
        publish_error(&mut runtime, &handle, CAPACITY);
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        let original = runtime.failures[0].clone();
        let key = runtime.failure_ui.incidents[0].key.clone();
        assert_eq!(runtime.failure_details(&key).as_deref(), Some(CAPACITY));

        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.turn_generation += 1;
            next.busy = true;
            next.items.push(Arc::new(text(&dot, chat::Role::User, ".")));
            next.items
                .push(Arc::new(text("streamed-reply", chat::Role::Assistant, "")));
        });
        for body in ["Resuming", "Resuming the requested work."] {
            publish(&mut runtime, &handle, |next| {
                *Arc::make_mut(next.items.last_mut().unwrap()) =
                    text("streamed-reply", chat::Role::Assistant, body);
            });
            assert_eq!(runtime.failures, vec![original.clone()]);
            assert_eq!(runtime.failure_ui.incidents.len(), 1);
            assert_eq!(runtime.failure_ui.incidents[0].key, key);
            let blocks = catalog(&mut runtime);
            assert!(prompt_position(&blocks, &prompt) < failure_position(&blocks, &key));
            assert!(failure_position(&blocks, &key) < prompt_position(&blocks, &dot));
            assert_eq!(runtime.failure_details(&key).as_deref(), Some(CAPACITY));
        }
        publish(&mut runtime, &handle, |next| {
            next.items.extend([
                Arc::new(tool("resumed-command")),
                Arc::new(text(
                    "after-command",
                    chat::Role::Assistant,
                    "The resumed command finished.",
                )),
            ]);
            next.busy = false;
        });
        // A metadata publication exercises the cached grouping path too.
        publish(&mut runtime, &handle, |next| next.detail = "Ready".into());
        let blocks = catalog(&mut runtime);
        assert!(failure_position(&blocks, &key) < prompt_position(&blocks, &dot));
        assert_eq!(runtime.failure_ui.incidents[0].key, key);
        assert!(runtime.failure_ui.expanded.contains(&key));
    }
}

#[test]
fn typed_capacity_keeps_its_event_key_through_streaming_and_fresh_history() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let prompt = part(provider, "typed-prompt", 0);
        let dot = part(provider, "typed-resume", 0);
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text(&prompt, chat::Role::User, "Continue"),
                provider_error(provider, "capacity-event:0", CAPACITY),
            ],
        );
        publish_error(&mut runtime, &handle, CAPACITY);
        assert_eq!(
            runtime.failure_ui.incidents.len(),
            1,
            "item and scalar are one event"
        );
        let key = runtime.failure_ui.incidents[0].key.clone();
        assert!(
            runtime.failure_ui.incidents[0]
                .sources
                .contains(&Source::Snapshot)
        );
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items.extend([
                Arc::new(text(&dot, chat::Role::User, ".")),
                Arc::new(text("typed-stream", chat::Role::Assistant, "Resuming")),
            ]);
        });
        let blocks = catalog(&mut runtime);
        assert!(failure_position(&blocks, &key) < prompt_position(&blocks, &dot));
        publish(&mut runtime, &handle, |next| {
            *Arc::make_mut(next.items.last_mut().unwrap()) = text(
                "typed-stream",
                chat::Role::Assistant,
                "Resuming with additional streamed text.",
            );
            next.items.push(Arc::new(tool("typed-resumed-command")));
        });
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(runtime.failure_ui.incidents[0].key, key);
        let saved = runtime
            .snapshot
            .items
            .iter()
            .map(|item| (**item).clone())
            .collect();
        let (mut reopened, _) = live_fixture(provider, saved);
        assert_eq!(reopened.failure_ui.incidents.len(), 1);
        assert_eq!(reopened.failure_ui.incidents[0].key, key);
        assert_eq!(reopened.failure_details(&key).as_deref(), Some(CAPACITY));
        let blocks = catalog(&mut reopened);
        assert!(failure_position(&blocks, &key) < prompt_position(&blocks, &dot));
    }
}

#[test]
fn scalar_same_text_after_more_work_and_a_new_prompt_is_three_occurrences() {
    let provider = Provider::Claude;
    let first = part(provider, "first-prompt", 0);
    let second = part(provider, "second-prompt", 0);
    let (mut runtime, handle) =
        live_fixture(provider, vec![text(&first, chat::Role::User, "Inspect")]);
    publish_error(&mut runtime, &handle, CAPACITY);
    let first_key = runtime.failure_ui.incidents[0].key.clone();
    let first_note = runtime.failures[0].clone();
    publish(&mut runtime, &handle, |next| {
        next.error = None;
        next.items.push(Arc::new(text(
            "recovered-work",
            chat::Role::Assistant,
            "A genuinely resumed response.",
        )));
    });
    publish_error(&mut runtime, &handle, CAPACITY);
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    assert_eq!(runtime.failures[0], first_note);
    assert_eq!(runtime.failure_ui.incidents[0].key, first_key);
    let second_key = runtime.failure_ui.incidents[1].key.clone();
    assert_ne!(first_key, second_key);
    publish(&mut runtime, &handle, |next| {
        next.error = None;
        next.items
            .push(Arc::new(text(&second, chat::Role::User, "Try once more")));
    });
    publish_error(&mut runtime, &handle, CAPACITY);
    assert_eq!(runtime.failure_ui.incidents.len(), 3);
    let third_key = runtime.failure_ui.incidents[2].key.clone();
    assert_ne!(third_key, first_key);
    assert_ne!(third_key, second_key);
    let blocks = catalog(&mut runtime);
    assert!(failure_position(&blocks, &first_key) < failure_position(&blocks, &second_key));
    assert!(failure_position(&blocks, &second_key) < prompt_position(&blocks, &second));
    assert!(prompt_position(&blocks, &second) < failure_position(&blocks, &third_key));
    for key in [&first_key, &second_key, &third_key] {
        assert_eq!(runtime.failure_details(key).as_deref(), Some(CAPACITY));
    }
}

#[test]
fn typed_same_text_on_one_prompt_stays_distinct_with_an_unchanged_scalar() {
    let provider = Provider::Claude;
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Inspect"),
            provider_error(provider, "earlier-capacity:0", CAPACITY),
        ],
    );
    publish_error(&mut runtime, &handle, CAPACITY);
    let earlier_key = runtime.failure_ui.incidents[0].key.clone();
    // A new typed occurrence remains observable even when the scalar state
    // publication never exposed its intermediate clear.
    publish(&mut runtime, &handle, |next| {
        next.items.extend([
            Arc::new(text("recovery", chat::Role::Assistant, "Resumed work.")),
            Arc::new(provider_error(provider, "later-capacity:0", CAPACITY)),
        ]);
    });
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    assert_eq!(runtime.failure_ui.incidents[0].key, earlier_key);
    let later_key = runtime.failure_ui.incidents[1].key.clone();
    assert_ne!(earlier_key, later_key);
    let blocks = catalog(&mut runtime);
    assert!(failure_position(&blocks, &earlier_key) < failure_position(&blocks, &later_key));
    assert_eq!(
        runtime.failure_details(&earlier_key).as_deref(),
        Some(CAPACITY)
    );
    assert_eq!(
        runtime.failure_details(&later_key).as_deref(),
        Some(CAPACITY)
    );
}

#[test]
fn coalesced_new_turn_same_scalar_text_records_a_later_occurrence() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let first = part(provider, "coalesced-first", 0);
        let second = part(provider, "coalesced-second", 0);
        let (mut runtime, handle) =
            live_fixture(provider, vec![text(&first, chat::Role::User, "Inspect")]);
        publish_error(&mut runtime, &handle, CAPACITY);
        let original = runtime.failures[0].clone();
        let original_key = runtime.failure_ui.incidents[0].key.clone();
        publish(&mut runtime, &handle, |next| {
            // Production recover_history_error clears the preceding scalar
            // on Event::TurnStarted. If that clear and the next same-text
            // failure coalesce, turn_generation still proves the boundary.
            next.turn_generation += 1;
            next.items.push(Arc::new(text(
                &second,
                chat::Role::User,
                "Try the request again",
            )));
            next.error = Some(CAPACITY.into());
        });
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failures[0], original);
        assert_eq!(runtime.failure_ui.incidents.len(), 2);
        assert_eq!(runtime.failure_ui.incidents[0].key, original_key);
        let later_key = runtime.failure_ui.incidents[1].key.clone();
        assert_ne!(original_key, later_key);
        let blocks = catalog(&mut runtime);
        assert!(failure_position(&blocks, &original_key) < prompt_position(&blocks, &second));
        assert!(prompt_position(&blocks, &second) < failure_position(&blocks, &later_key));
    }
}

#[test]
fn replayed_multipart_provider_carrier_keeps_one_notice_and_all_originals() {
    let provider = Provider::Claude;
    let cause = "Claude provider is at capacity. Please retry later.";
    let details = "request-id: original-carrier\nOriginal diagnostic details are retained.";
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Inspect"),
            provider_error(provider, "one-carrier:0", cause),
            provider_error(provider, "one-carrier:1", details),
        ],
    );
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    let key = runtime.failure_ui.incidents[0].key.clone();
    let originals = runtime.failure_details(&key).unwrap();
    assert!(originals.contains(cause));
    assert!(originals.contains(details));
    catalog(&mut runtime);
    publish(&mut runtime, &handle, |next| {
        // Provider replay replaces the same IDs in place; it is not a new
        // diagnostic receipt and must retain the expanded notice identity.
        next.items[1] = Arc::new(provider_error(provider, "one-carrier:0", cause));
        next.items[2] = Arc::new(provider_error(provider, "one-carrier:1", details));
        next.items
            .push(Arc::new(text("after-replay", chat::Role::User, "Continue")));
    });
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    assert_eq!(runtime.failure_ui.incidents[0].key, key);
    assert!(runtime.failure_ui.expanded.contains(&key));
    assert_eq!(
        runtime.failure_details(&key).as_deref(),
        Some(originals.as_str())
    );
    let blocks = catalog(&mut runtime);
    assert!(failure_position(&blocks, &key) < prompt_position(&blocks, "after-replay"));
}

#[test]
fn claude_scalar_envelope_joins_its_proven_multipart_diagnostic() {
    let provider = Provider::Claude;
    let cause = "Claude provider is at capacity. Please retry later.";
    let details = "request-id: original-carrier\nOriginal diagnostic details are retained.";
    let scalar = format!("Claude: overloaded_error\n{cause}\n{details}");
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Inspect"),
            provider_error(provider, "wrapped-carrier:0", cause),
            provider_error(provider, "wrapped-carrier:1", details),
        ],
    );
    publish_error(&mut runtime, &handle, &scalar);
    assert_eq!(
        runtime.failure_ui.incidents.len(),
        1,
        "one provider envelope has one visible incident"
    );
    let key = runtime.failure_ui.incidents[0].key.clone();
    let originals = runtime.failure_details(&key).unwrap();
    assert!(originals.contains(cause));
    assert!(originals.contains(details));
    assert!(originals.contains(&scalar));
    let blocks = catalog(&mut runtime);
    assert_eq!(
        blocks
            .iter()
            .filter(|(id, _)| *id == diagnostic_preview_identity(&key))
            .count(),
        1
    );
}

#[test]
fn typed_diagnostic_closes_before_autonomous_resumed_prose() {
    let provider = Provider::Claude;
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Inspect"),
            provider_error(provider, "capacity-event:0", CAPACITY),
        ],
    );
    let key = runtime.failure_ui.incidents[0].key.clone();
    publish(&mut runtime, &handle, |next| {
        // Recovery can arrive without another user prompt. A diagnostic is
        // still a boundary; ordinary commentary must not join its old group.
        next.items.push(Arc::new(text(
            "autonomous-recovery",
            chat::Role::Assistant,
            "Autonomous resumed response.",
        )));
    });
    let blocks = catalog(&mut runtime);
    let recovery = blocks
        .iter()
        .position(|(_, body)| body.contains("Autonomous resumed response."))
        .expect("the resumed response is selectable");
    assert!(failure_position(&blocks, &key) < recovery);
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(CAPACITY));
}

#[test]
fn paging_out_a_proven_item_does_not_change_its_retained_note_identity() {
    let provider = Provider::Claude;
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Inspect"),
            provider_error(provider, "capacity-event:0", CAPACITY),
        ],
    );
    publish_error(&mut runtime, &handle, CAPACITY);
    let key = runtime.failure_ui.incidents[0].key.clone();
    catalog(&mut runtime);
    publish(&mut runtime, &handle, |next| {
        next.has_earlier = true;
        // Keep the owning prompt visible, so the retained notice still has
        // a proven destination even while its explicit carrier is absent.
        next.items.retain(|item| item.id == "prompt:0");
    });
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    assert_eq!(runtime.failure_ui.incidents[0].key, key);
    assert!(runtime.failure_ui.expanded.contains(&key));
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(CAPACITY));
    let blocks = catalog(&mut runtime);
    assert!(prompt_position(&blocks, "prompt:0") < failure_position(&blocks, &key));
}

#[test]
fn opaque_event_id_never_aliases_a_proven_claude_carrier() {
    let provider = Provider::Claude;
    let (runtime, _) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Inspect"),
            provider_error(provider, "opaque", CAPACITY),
            text("recovery", chat::Role::Assistant, "Resumed work."),
            provider_error(provider, "opaque:0", CAPACITY),
        ],
    );
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    assert_ne!(
        runtime.failure_ui.incidents[0].key,
        runtime.failure_ui.incidents[1].key
    );
}
