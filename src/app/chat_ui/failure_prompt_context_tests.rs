//! Actual Runtime refresh and collector regressions using published fixtures.
use super::*;

fn text(id: &str, role: chat::Role, body: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role,
        at: None,
        started_at: None,
        content: ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    }
}
fn image(id: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: chat::Role::User,
        at: None,
        started_at: None,
        content: ItemContent::Image {
            path: String::new(),
            description: "Synthetic image".into(),
            reference: None,
        },
    }
}
fn part(provider: Provider, carrier: &str, index: usize) -> String {
    match provider {
        Provider::Codex => format!("{carrier}/input/{index}"),
        Provider::Claude | Provider::Omp => format!("{carrier}:{index}"),
    }
}
fn live_fixture(provider: Provider, items: Vec<chat::Item>) -> (Runtime, Arc<ClientHandle>) {
    let session = fresh_chat_session(
        provider,
        TerminalId::new(),
        "/project".into(),
        &Account::system(provider),
        false,
    )
    .unwrap();
    let snapshot = Snapshot {
        ready: true,
        items: items.into_iter().map(Arc::new).collect(),
        ..Snapshot::default()
    };
    let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
    let runtime = Runtime::new(
        Target {
            card: CardId::new(),
            generation: 1,
        },
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    (runtime, handle)
}
fn publish_error(runtime: &mut Runtime, handle: &ClientHandle, error: &str) {
    let mut next = (*handle.snapshot()).clone();
    next.generation += 1;
    next.error = Some(error.into());
    handle.publish_snapshot_for_test(next);
    runtime.refresh();
}

#[test]
fn eight_part_user_prompt_archived_and_current_failure_share_one_message_scope() {
    let raw = "Failed to authenticate: OAuth session expired and could not be refreshed";
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let carrier = "11111111-1111-4111-8111-111111111111";
        let mut items = vec![text(
            &part(provider, carrier, 0),
            chat::Role::User,
            "Inspect these synthetic images",
        )];
        items.extend((1..=8).map(|index| image(&part(provider, carrier, index))));
        let (mut runtime, handle) = live_fixture(provider, items);
        assert_eq!(runtime.turns.len(), 1);
        assert_eq!(runtime.turns[0].prompt.len(), 9);
        let originals = runtime.snapshot.items.clone();
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(
            runtime.failures.len(),
            1,
            "the actual refresh must retain the original note"
        );
        assert_eq!(
            runtime.failures[0].0.as_deref(),
            Some(part(provider, carrier, 0).as_str())
        );
        assert_eq!(
            runtime.failure_ui.incidents.len(),
            1,
            "one decoder message, not one failure per content part: {provider:?}"
        );
        let key = runtime.failure_ui.incidents[0].key.clone();
        assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw));
        assert_eq!(runtime.snapshot.error.as_deref(), Some(raw));
        assert!(
            originals
                .iter()
                .zip(&runtime.snapshot.items)
                .all(|(a, b)| Arc::ptr_eq(a, b)),
            "projection must preserve the original transcript/IDs"
        );
        let revision = runtime.failure_ui.revision;
        runtime.refresh();
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(
            runtime.failure_ui.revision, revision,
            "unchanged replay is idempotent"
        );
    }
}

#[test]
fn paged_image_only_prompt_keeps_its_original_note_carrier_without_first_part() {
    let provider = Provider::Claude;
    let carrier = "22222222-2222-4222-8222-222222222222";
    let first = part(provider, carrier, 0);
    let last = part(provider, carrier, 8);
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![text(&first, chat::Role::User, "Prompt"), image(&last)],
    );
    let raw = "Opaque provider failure\nexact original details";
    publish_error(&mut runtime, &handle, raw);
    let key = runtime.failure_ui.incidents[0].key.clone();
    let mut page = (*handle.snapshot()).clone();
    page.generation += 1;
    page.has_earlier = true;
    page.items.retain(|item| item.id == last);
    handle.publish_snapshot_for_test(page);
    runtime.refresh();
    assert!(!runtime.snapshot.items.iter().any(|item| item.id == first));
    assert_eq!(runtime.failures[0].0.as_deref(), Some(first.as_str()));
    assert_eq!(runtime.failure_ui.incidents.len(), 1);
    assert_eq!(runtime.failure_ui.incidents[0].key, key);
    assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw));
    runtime.failure_ui.expanded.insert(key.clone());
    runtime.cache_failure_preview(&key);
    let (catalog, _) = runtime.selection_document();
    assert_eq!(
        catalog.text(diagnostic_preview_identity(&key)).as_deref(),
        Some(raw),
        "the production visible transcript catalog must retain the sole notice after paging"
    );
    assert_eq!(
        runtime.failure_ui.incidents[0].anchor.as_deref(),
        Some(last.as_str())
    );
    assert_eq!(
        runtime.failures[0].0.as_deref(),
        Some(first.as_str()),
        "display projection must not rewrite the original note"
    );

    // An unrelated visible prompt provides no authority to move this old
    // notice to the latest turn, even if the diagnostic text is identical.
    let mut unrelated = (*handle.snapshot()).clone();
    unrelated.generation += 1;
    unrelated.error = None;
    unrelated.items = vec![Arc::new(text(
        "another-message:0",
        chat::Role::User,
        "Unrelated prompt",
    ))];
    handle.publish_snapshot_for_test(unrelated);
    runtime.refresh();
    assert_eq!(runtime.failures[0].0.as_deref(), Some(first.as_str()));
    let historical_key = runtime.failure_ui.incidents[0].key.clone();
    runtime.failure_ui.expanded.insert(historical_key.clone());
    runtime.cache_failure_preview(&historical_key);
    let (catalog, _) = runtime.selection_document();
    assert!(
        catalog
            .text(diagnostic_preview_identity(&historical_key))
            .is_none(),
        "an absent carrier must not project its notice into an unrelated visible turn"
    );
}

#[test]
fn distinct_prompt_carriers_keep_identical_errors_and_exact_original_sources() {
    let provider = Provider::Claude;
    let raw = "Opaque provider failure\nidentical complete details";
    let a = "33333333-3333-4333-8333-333333333333";
    let b = "44444444-4444-4444-8444-444444444444";
    let error = |id: &str| {
        let mut item = text(id, chat::Role::System, raw);
        assert!(chat::failure::mark_provider_error(provider, &mut item));
        item
    };
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text(&part(provider, a, 0), chat::Role::User, "First"),
            image(&part(provider, a, 1)),
            error("error-a:0"),
            text(&part(provider, b, 0), chat::Role::User, "Second"),
            image(&part(provider, b, 1)),
            error("error-b:0"),
        ],
    );
    publish_error(&mut runtime, &handle, raw);
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    let keys: HashSet<_> = runtime
        .failure_ui
        .incidents
        .iter()
        .map(|incident| &incident.key)
        .collect();
    assert_eq!(keys.len(), 2);
    for incident in &runtime.failure_ui.incidents {
        assert_eq!(runtime.failure_details(&incident.key).as_deref(), Some(raw));
    }
    assert_eq!(runtime.snapshot.items.len(), 6);

    // Normalizing the multipart parent must not absorb simultaneous children
    // whose separate scoped provider carriers report the exact same body.
    let child = |id: &str| chat::Subagent {
        id: id.into(),
        kind: "reviewer".into(),
        description: "Synthetic child failure".into(),
        model: None,
        status: chat::ToolStatus::Failed,
        waiting: false,
        detail: raw.into(),
        background: true,
        items: vec![Arc::new(error("child-error:0"))],
    };
    let original_parent = runtime.snapshot.items.clone();
    let mut children = (*handle.snapshot()).clone();
    children.generation += 1;
    for id in ["child-a", "child-b"] {
        let mut parent_call = text(id, chat::Role::Assistant, "");
        parent_call.content = ItemContent::Tool {
            name: "Agent".into(),
            input: "Synthetic reviewer".into(),
            output: String::new(),
            status: chat::ToolStatus::Failed,
        };
        children.items.push(Arc::new(parent_call));
    }
    children.subagents = vec![child("child-a"), child("child-b")];
    handle.publish_snapshot_for_test(children);
    runtime.refresh();
    assert_eq!(
        runtime.failure_ui.incidents.len(),
        4,
        "two parent message scopes and two distinct child scopes must all survive"
    );
    assert_eq!(
        runtime
            .failure_ui
            .incidents
            .iter()
            .filter(|incident| incident.child_scope.is_none())
            .count(),
        2
    );
    for id in ["child-a", "child-b"] {
        let incidents: Vec<_> = runtime
            .failure_ui
            .incidents
            .iter()
            .filter(|incident| incident.child_scope.as_deref() == Some(id))
            .collect();
        assert_eq!(
            incidents.len(),
            1,
            "each child's exact item/detail echo joins only that child"
        );
        let incident = incidents[0];
        assert_eq!(
            incident.sources,
            vec![
                Source::ChildItem(id.into(), "child-error:0".into()),
                Source::ChildDetail(id.into()),
            ],
            "both exact source identities must survive, not merely their equal body"
        );
        let (from_item, first) = runtime.failure_ui.child_item(id, "child-error:0").unwrap();
        assert_eq!(from_item.key, incident.key);
        assert!(first);
        assert!(runtime.incident_has_child_detail(incident, id));
        assert!(!runtime.incident_has_child_detail(
            incident,
            if id == "child-a" {
                "child-b"
            } else {
                "child-a"
            }
        ));
        for source in &incident.sources {
            assert_eq!(runtime.failure_source(source), Some(raw));
        }
        // The real parent call ID exists after the multipart B prompt. Bind
        // the child to that normalized parent context, exercising the lookup
        // rather than the unrelated child:<id> fallback.
        let expected_context = format!("message\0{b}:child:{id}:error:child-error");
        let expected = State::metadata(provider, raw, false, &expected_context).unwrap();
        assert_eq!(incident.key, expected.key);
        assert_eq!(runtime.failure_details(&incident.key).as_deref(), Some(raw));
    }
    let keys: HashSet<_> = runtime
        .failure_ui
        .incidents
        .iter()
        .map(|incident| &incident.key)
        .collect();
    assert_eq!(keys.len(), 4);
    assert!(
        original_parent
            .iter()
            .zip(&runtime.snapshot.items)
            .all(|(a, b)| Arc::ptr_eq(a, b))
    );
}

#[test]
fn matching_first_lines_and_unknown_suffixes_do_not_establish_a_shared_cause() {
    let provider = Provider::Claude;
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text("unproven:01", chat::Role::User, "Prompt"),
            image("unproven:2"),
        ],
    );
    publish_error(&mut runtime, &handle, "Opaque provider failure");
    assert_eq!(
        runtime.failure_ui.incidents.len(),
        2,
        "noncanonical01 must not be borrowed as message-carrier proof"
    );

    let body_a = "Failed to authenticate\nrequest-id: first";
    let body_b = "Failed to authenticate\nrequest-id: second";
    let mut a = text("cause-a:0", chat::Role::System, body_a);
    let mut b = text("cause-b:0", chat::Role::System, body_b);
    assert!(chat::failure::mark_provider_error(provider, &mut a));
    assert!(chat::failure::mark_provider_error(provider, &mut b));
    let (runtime, _) = live_fixture(
        provider,
        vec![
            text("prompt:0", chat::Role::User, "Prompt"),
            image("prompt:1"),
            a,
            b,
        ],
    );
    assert_eq!(
        runtime.failure_ui.incidents.len(),
        2,
        "equal summaries retain distinct full diagnostics"
    );
}

#[test]
fn opaque_item_id_does_not_alias_a_canonical_message_carrier() {
    let provider = Provider::Claude;
    let raw = "Opaque provider failure";
    let error = |id: &str| {
        let mut item = text(id, chat::Role::System, raw);
        assert!(chat::failure::mark_provider_error(provider, &mut item));
        item
    };
    let (runtime, _) = live_fixture(
        provider,
        vec![
            text("m", chat::Role::User, "Legacy opaque item"),
            error("error-legacy:0"),
            text("m:0", chat::Role::User, "Canonical message"),
            error("error-message:0"),
        ],
    );
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    assert_ne!(
        runtime.failure_ui.incidents[0].key,
        runtime.failure_ui.incidents[1].key
    );
}

#[test]
fn adjacent_distinct_message_carriers_do_not_borrow_the_shared_visual_turn_key() {
    let provider = Provider::Claude;
    let a = "55555555-5555-4555-8555-555555555555";
    let b = "66666666-6666-4666-8666-666666666666";
    let first = part(provider, a, 0);
    let second = part(provider, b, 0);
    let (mut runtime, _) = live_fixture(
        provider,
        vec![
            text(&first, chat::Role::User, "First independent submission"),
            image(&part(provider, a, 1)),
            text(&second, chat::Role::User, "Second independent submission"),
            image(&part(provider, b, 1)),
        ],
    );
    assert_eq!(
        runtime.turns.len(),
        1,
        "presentation intentionally groups adjacent prompts"
    );
    assert_eq!(runtime.turns[0].key, first);
    // Explicit retained-note inputs test collector identity independently of
    // visual grouping. This does not claim Runtime.note records both anchors
    // in a coalesced first-prompt publication; that ordering is a separate gap.
    let raw = "Opaque provider failure\nidentical original diagnostic";
    runtime.failures = vec![(Some(first), raw.into()), (Some(second), raw.into())];
    runtime.refresh_failure_ui();
    assert_eq!(runtime.failure_ui.incidents.len(), 2);
    assert_ne!(
        runtime.failure_ui.incidents[0].key,
        runtime.failure_ui.incidents[1].key
    );
    for incident in &runtime.failure_ui.incidents {
        assert_eq!(runtime.failure_details(&incident.key).as_deref(), Some(raw));
    }
}

#[test]
fn refresh_keeps_adjacent_opaque_notes_visible_without_aliasing_their_scopes() {
    let raw = "Opaque provider failure\ncomplete original diagnostic";
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text("legacy-first", chat::Role::User, "First submission"),
                text("legacy-second", chat::Role::User, "Second submission"),
            ],
        );
        assert_eq!(runtime.turns.len(), 1);
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(runtime.failures[0].0.as_deref(), Some("legacy-second"));
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("legacy-first")
        );
        let first_key = runtime.failure_ui.incidents[0].key.clone();
        runtime.failure_ui.expanded.insert(first_key.clone());
        runtime.cache_failure_preview(&first_key);
        assert_eq!(
            runtime
                .selection_document()
                .0
                .text(diagnostic_preview_identity(&first_key))
                .as_deref(),
            Some(raw),
            "the production transcript catalog must expose the opaque notice"
        );

        let mut next = (*handle.snapshot()).clone();
        next.generation += 1;
        next.error = None;
        next.items.push(Arc::new(text(
            "legacy-third",
            chat::Role::User,
            "Third submission",
        )));
        handle.publish_snapshot_for_test(next);
        runtime.refresh();
        assert_eq!(runtime.failures[0].0.as_deref(), Some("legacy-second"));
        assert_eq!(
            runtime
                .selection_document()
                .0
                .text(diagnostic_preview_identity(&first_key))
                .as_deref(),
            Some(raw),
            "clearing the current error must retain the visible captured note"
        );
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failures[1].0.as_deref(), Some("legacy-third"));
        assert_eq!(runtime.failure_ui.incidents.len(), 2);
        assert_ne!(
            runtime.failure_ui.incidents[0].key,
            runtime.failure_ui.incidents[1].key
        );
        for incident in &runtime.failure_ui.incidents {
            assert_eq!(incident.anchor.as_deref(), Some("legacy-first"));
            assert_eq!(runtime.failure_details(&incident.key).as_deref(), Some(raw));
        }
        let keys = runtime
            .failure_ui
            .incidents
            .iter()
            .map(|incident| incident.key.clone())
            .collect::<Vec<_>>();
        for key in &keys {
            runtime.failure_ui.expanded.insert(key.clone());
            runtime.cache_failure_preview(key);
        }
        let (catalog, _) = runtime.selection_document();
        for key in &keys {
            assert_eq!(
                catalog.text(diagnostic_preview_identity(key)).as_deref(),
                Some(raw)
            );
        }

        // An absent opaque item has no same-message proof. Do not move its
        // historical note to a different prompt merely sharing the old Turn.
        let mut next = (*handle.snapshot()).clone();
        next.generation += 1;
        next.error = None;
        next.items.retain(|item| item.id != "legacy-second");
        handle.publish_snapshot_for_test(next);
        runtime.refresh();
        assert_eq!(runtime.failures[0].0.as_deref(), Some("legacy-second"));
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("legacy-second")
        );
        assert_eq!(
            runtime.failure_ui.incidents[1].anchor.as_deref(),
            Some("legacy-first")
        );
        let historical_key = runtime.failure_ui.incidents[0].key.clone();
        runtime.failure_ui.expanded.insert(historical_key.clone());
        runtime.cache_failure_preview(&historical_key);
        let (catalog, _) = runtime.selection_document();
        assert!(
            catalog
                .text(diagnostic_preview_identity(&historical_key))
                .is_none()
        );
        assert_eq!(
            catalog
                .text(diagnostic_preview_identity(&keys[1]))
                .as_deref(),
            Some(raw)
        );
    }
}

#[test]
fn refresh_attributes_adjacent_message_error_to_the_incoming_prompt() {
    let raw = "Opaque provider failure\nexact original diagnostic";
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let a = "77777777-7777-4777-8777-777777777777";
        let b = "88888888-8888-4888-8888-888888888888";
        // The incoming publication can contain both prompts and the failure.
        let (mut runtime, handle) = live_fixture(provider, vec![]);
        let mut next = (*handle.snapshot()).clone();
        next.generation += 1;
        next.items = vec![
            Arc::new(text(&part(provider, a, 0), chat::Role::User, "First")),
            Arc::new(image(&part(provider, a, 1))),
            Arc::new(text(&part(provider, b, 0), chat::Role::User, "Second")),
            Arc::new(image(&part(provider, b, 1))),
        ];
        next.error = Some(raw.into());
        handle.publish_snapshot_for_test(next);
        runtime.refresh();
        assert_eq!(runtime.turns.len(), 1);
        assert_eq!(runtime.turns[0].key, part(provider, a, 0));
        assert_eq!(runtime.failures.len(), 1);
        assert_eq!(
            runtime.failures[0].0.as_deref(),
            Some(part(provider, b, 0).as_str())
        );
        assert_eq!(
            runtime.failure_ui.incidents.len(),
            1,
            "note and current error must share B's message: {provider:?}"
        );
        let incident = &runtime.failure_ui.incidents[0];
        assert!(incident.sources.contains(&Source::Note(0)));
        assert!(incident.sources.contains(&Source::Snapshot));
        assert_eq!(
            incident.anchor.as_deref(),
            Some(part(provider, a, 0).as_str()),
            "the shared turn remains the display destination"
        );
        assert_eq!(runtime.failure_details(&incident.key).as_deref(), Some(raw));

        // A later, distinct prompt can report the same complete diagnostic.
        let c = "99999999-9999-4999-8999-999999999999";
        let mut next = (*handle.snapshot()).clone();
        next.generation += 1;
        next.error = None;
        next.items.push(Arc::new(text(
            &part(provider, c, 0),
            chat::Role::User,
            "Third",
        )));
        next.items.push(Arc::new(image(&part(provider, c, 1))));
        handle.publish_snapshot_for_test(next);
        runtime.refresh();
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failure_ui.incidents.len(), 2);
        assert_ne!(
            runtime.failure_ui.incidents[0].key,
            runtime.failure_ui.incidents[1].key
        );
        for incident in &runtime.failure_ui.incidents {
            assert_eq!(runtime.failure_details(&incident.key).as_deref(), Some(raw));
        }
    }
}

#[test]
fn paged_adjacent_auth_recovery_removes_only_the_current_message_warning() {
    let provider = Provider::Claude;
    let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let raw = "Failed to authenticate: OAuth session expired and could not be refreshed";
    let (mut runtime, handle) = live_fixture(
        provider,
        vec![
            text(&part(provider, a, 0), chat::Role::User, "First"),
            image(&part(provider, a, 1)),
        ],
    );
    publish_error(&mut runtime, &handle, raw);
    let first_note = runtime.failures[0].clone();
    let mut next = (*handle.snapshot()).clone();
    next.generation += 1;
    next.items.push(Arc::new(text(
        &part(provider, b, 0),
        chat::Role::User,
        "Second",
    )));
    next.items.push(Arc::new(image(&part(provider, b, 1))));
    // A changed current error on the same visual turn must not replace A.
    next.error = Some(chat::sign_in_verification_failure(
        raw,
        "Synthetic verification failure",
    ));
    handle.publish_snapshot_for_test(next);
    runtime.refresh();
    assert_eq!(runtime.turns.len(), 1);
    assert_eq!(runtime.failures.len(), 2);
    assert_eq!(runtime.failures[0], first_note);
    assert!(!runtime.active_auth_notice(&runtime.failures[0].0, &runtime.failures[0].1));
    assert!(runtime.active_auth_notice(&runtime.failures[1].0, &runtime.failures[1].1));
    let current_note = runtime.failures[1].clone();

    let mut page = (*handle.snapshot()).clone();
    page.generation += 1;
    page.has_earlier = true;
    page.items.retain(|item| item.id == part(provider, b, 1));
    handle.publish_snapshot_for_test(page);
    runtime.refresh();
    assert!(runtime.active_auth_notice(&runtime.failures[1].0, &runtime.failures[1].1));
    runtime.note(current_note.1.clone());
    assert_eq!(
        runtime.failures.len(),
        2,
        "paging must not create another auth scope"
    );
    assert_eq!(runtime.failures[0], first_note);

    let mut recovered = (*handle.snapshot()).clone();
    recovered.generation += 1;
    recovered.error = None;
    recovered.ready = true;
    recovered.busy = false;
    handle.publish_snapshot_for_test(recovered);
    runtime.refresh();
    assert_eq!(
        runtime.failures,
        vec![first_note],
        "idle recovery must remove B only, even after its first part was paged out"
    );
}
