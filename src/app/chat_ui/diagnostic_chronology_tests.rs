use super::*;

const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";
const FIRST: &str = "11111111-1111-4111-8111-111111111111";
const NEXT: &str = "22222222-2222-4222-8222-222222222222";

fn part(provider: Provider, message: &str, index: usize) -> String {
    match provider {
        Provider::Codex => format!("{message}/input/{index}"),
        Provider::Claude | Provider::Omp => format!("{message}:{index}"),
    }
}

fn text(id: impl Into<String>, role: chat::Role, body: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role,
        at: None,
        started_at: None,
        content: ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: chat::Phase::Commentary,
        },
    }
}

fn image(id: String) -> chat::Item {
    chat::Item {
        id,
        role: chat::Role::User,
        at: None,
        started_at: None,
        content: ItemContent::Image {
            path: String::new(),
            description: "Synthetic attachment".into(),
            reference: None,
        },
    }
}

fn fixture(provider: Provider, items: Vec<chat::Item>) -> (Runtime, Arc<ClientHandle>) {
    let session = fresh_chat_session(
        provider,
        TerminalId::new(),
        "/project".into(),
        &Account::system(provider),
        false,
    )
    .unwrap();
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            items: items.into_iter().map(Arc::new).collect(),
            ..Snapshot::default()
        },
    ));
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

fn publish(runtime: &mut Runtime, handle: &ClientHandle, edit: impl FnOnce(&mut Snapshot)) {
    let mut next = (*handle.snapshot()).clone();
    next.generation += 1;
    edit(&mut next);
    handle.publish_snapshot_for_test(next);
    runtime.refresh();
}

fn notice_before_prompt(runtime: &mut Runtime, key: &str, prompt: &str) {
    preparation::drain_fixture(runtime);
    runtime.failure_ui.expanded.insert(key.into());
    runtime.cache_failure_preview(key);
    let (catalog, _) = runtime.selection_document();
    let notice = catalog
        .position(failure::diagnostic_preview_identity(key))
        .expect("expanded notice remains selectable");
    let prompt = catalog
        .position(selection::logical_identity(prompt, &[2]))
        .expect("later user message remains selectable");
    assert!(
        notice < prompt,
        "earlier notice must precede the later prompt"
    );
}

#[test]
fn early_capacity_note_stays_before_the_later_prompt_and_streaming_reply() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let first = part(provider, FIRST, 0);
        let next = part(provider, NEXT, 0);
        let (mut runtime, handle) =
            fixture(provider, vec![text(first, chat::Role::User, "Investigate")]);
        runtime.replace_draft(text_editor::Content::with_text("Unsent draft"));
        publish(&mut runtime, &handle, |page| {
            page.error = Some(CAPACITY.into())
        });
        let original = runtime.failures[0].clone();
        let key = runtime.failure_ui.incidents[0].key.clone();

        publish(&mut runtime, &handle, |page| {
            page.error = None;
            page.busy = true;
            page.turn_generation += 1;
            page.items.extend([
                Arc::new(text(&next, chat::Role::User, ".")),
                Arc::new(text("reply", chat::Role::Assistant, "Continuing")),
            ]);
        });
        assert!(runtime.snapshot.busy);
        notice_before_prompt(&mut runtime, &key, &next);
        publish(&mut runtime, &handle, |page| {
            page.items[2] = Arc::new(text(
                "reply",
                chat::Role::Assistant,
                "Continuing the investigation.",
            ));
        });
        assert!(runtime.snapshot.busy);
        runtime.refresh();
        preparation::drain_fixture(&mut runtime);
        notice_before_prompt(&mut runtime, &key, &next);
        publish(&mut runtime, &handle, |page| page.busy = false);
        notice_before_prompt(&mut runtime, &key, &next);
        assert_eq!(runtime.failures, vec![original]);
        assert_eq!(runtime.composer.text(), "Unsent draft");
        assert_eq!(runtime.failure_details(&key).as_deref(), Some(CAPACITY));
    }
}

#[test]
fn the_same_error_after_later_work_retains_both_attempts_in_order() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let first = part(provider, FIRST, 0);
        let next = part(provider, NEXT, 0);
        let (mut runtime, handle) =
            fixture(provider, vec![text(first, chat::Role::User, "Investigate")]);
        publish(&mut runtime, &handle, |page| {
            page.error = Some(CAPACITY.into())
        });
        let first_note = runtime.failures[0].clone();
        let first_key = runtime.failure_ui.incidents[0].key.clone();
        publish(&mut runtime, &handle, |page| {
            page.error = None;
            page.turn_generation += 1;
            page.items.extend([
                Arc::new(text(&next, chat::Role::User, "Try again")),
                Arc::new(text("reply", chat::Role::Assistant, "Checking again.")),
            ]);
        });
        publish(&mut runtime, &handle, |page| {
            page.error = Some(CAPACITY.into())
        });
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failures[0], first_note);
        assert_eq!(runtime.failures[1].message.as_deref(), Some(next.as_str()));
        assert_eq!(runtime.failure_ui.incidents.len(), 2);
        let second_key = runtime.failure_ui.incidents[1].key.clone();
        assert_ne!(first_key, second_key);
        notice_before_prompt(&mut runtime, &first_key, &next);
        runtime.failure_ui.expanded.insert(second_key.clone());
        runtime.cache_failure_preview(&second_key);
        let (catalog, _) = runtime.selection_document();
        assert!(
            catalog
                .position(selection::logical_identity(&next, &[2]))
                .unwrap()
                < catalog
                    .position(failure::diagnostic_preview_identity(&second_key))
                    .unwrap()
        );
        assert_eq!(
            runtime.failure_details(&first_key).as_deref(),
            Some(CAPACITY)
        );
        assert_eq!(
            runtime.failure_details(&second_key).as_deref(),
            Some(CAPACITY)
        );
    }
}

#[test]
fn failed_prompt_parts_still_group_together_before_the_next_message() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let (mut runtime, handle) = fixture(
            provider,
            vec![text(part(provider, FIRST, 0), chat::Role::User, "Inspect")],
        );
        publish(&mut runtime, &handle, |page| {
            page.error = Some(CAPACITY.into())
        });
        publish(&mut runtime, &handle, |page| {
            page.items
                .extend((1..=2).map(|index| Arc::new(image(part(provider, FIRST, index)))));
        });
        assert_eq!(runtime.turns.len(), 1, "one multipart prompt");
        assert_eq!(runtime.turns[0].prompt.len(), 3);
        let key = runtime.failure_ui.incidents[0].key.clone();
        let next = part(provider, NEXT, 0);
        publish(&mut runtime, &handle, |page| {
            page.error = None;
            page.items.extend([
                Arc::new(text(&next, chat::Role::User, "Next image")),
                Arc::new(image(part(provider, NEXT, 1))),
            ]);
        });
        assert_eq!(runtime.turns.len(), 2);
        assert_eq!(runtime.turns[0].prompt.len(), 3);
        assert_eq!(runtime.turns[1].prompt.len(), 2);
        notice_before_prompt(&mut runtime, &key, &next);
    }
}

#[test]
fn a_paged_prompt_part_keeps_the_notice_before_a_new_message() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let remaining = part(provider, FIRST, 1);
        let next = part(provider, NEXT, 0);
        let (mut runtime, handle) = fixture(
            provider,
            vec![
                text(part(provider, FIRST, 0), chat::Role::User, "Inspect"),
                image(remaining.clone()),
                image(part(provider, FIRST, 2)),
            ],
        );
        publish(&mut runtime, &handle, |page| {
            page.error = Some(CAPACITY.into())
        });
        let key = runtime.failure_ui.incidents[0].key.clone();
        publish(&mut runtime, &handle, |page| {
            page.has_earlier = true;
            page.error = None;
            page.items.retain(|item| item.id == remaining);
            page.items
                .push(Arc::new(text(&next, chat::Role::User, "Next")));
        });
        assert_eq!(runtime.turns.len(), 2);
        notice_before_prompt(&mut runtime, &key, &next);
    }
}

#[test]
fn a_provider_diagnostic_already_keeps_later_prompts_separate() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let mut error = text("provider-error:0", chat::Role::System, CAPACITY);
        assert!(chat::failure::mark_provider_error(provider, &mut error));
        let next = part(provider, NEXT, 0);
        let (mut runtime, handle) = fixture(
            provider,
            vec![
                text(part(provider, FIRST, 0), chat::Role::User, "Inspect"),
                error,
            ],
        );
        let key = runtime.failure_ui.incidents[0].key.clone();
        publish(&mut runtime, &handle, |page| {
            page.items.extend([
                Arc::new(text(&next, chat::Role::User, "Next")),
                Arc::new(text("reply", chat::Role::Assistant, "Continuing.")),
            ]);
        });
        notice_before_prompt(&mut runtime, &key, &next);
    }
}
