use super::*;
use iced::advanced::{
    Layout, layout,
    widget::{Operation, Tree},
};

fn context(body: &str) -> chat::Item {
    chat::Item {
        id: "provider-context".into(),
        role: chat::Role::System,
        content: ItemContent::Delivery {
            kind: chat::DeliveryKind::Notice,
            from: String::new(),
            sender: String::new(),
            summary: "Claude context".into(),
            body: body.into(),
        },
        started_at: None,
        at: None,
    }
}

fn rendered(runtime: &mut Runtime, width: f32, expanded: bool) {
    preparation::drain_fixture(runtime);
    let item = runtime.active_item("provider-context").unwrap();
    let cached = &runtime.presentation_cache()[&item.id];
    assert_eq!(turn::slot_of(item), turn::Slot::Commentary);
    assert_eq!(cached.preview.text, "Claude context");
    assert!(cached.preview.tail.is_empty());
    let mut element = runtime.step_view(
        item,
        cached,
        design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
    );
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 10_000.0));
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    cached.flow_cache.complete_fixture();
    node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    assert!(
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .any(
                |node| node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some("Claude context")
            )
    );
    #[derive(Default)]
    struct Text(Vec<String>);
    impl Operation for Text {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(text) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push(text.plain.to_string());
            }
        }
    }
    let mut text = Text::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut text);
    let ItemContent::Delivery { body, .. } = &item.content else {
        unreachable!()
    };
    if expanded {
        assert_eq!(
            text.0.join(""),
            *body,
            "the disclosure mounts the complete literal source"
        );
    } else {
        assert!(
            text.0.is_empty(),
            "the closed row mounts no context body or prefix preview"
        );
    }
}

#[test]
fn provider_context_closed_widget_catalog_and_copy_preserve_literal_body() {
    use iced::futures::StreamExt;
    for body in [
        "Base directory for this skill: /fixture\nshort instructions".to_owned(),
        format!(
            "Base directory for this skill: /fixture\n{}END_CONTEXT",
            "literal *markdown* λ\n".repeat(64)
        ),
    ] {
        let item = context(&body);
        let mut user_notice = item.clone();
        user_notice.id = "user-notice-control".into();
        user_notice.role = chat::Role::User;
        let (runtime, _handle) = prepared_runtime_for_test(&[item.clone(), user_notice]);
        let target = runtime.target;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        let identity = selection::logical_identity(&item.id, &[2]);
        for expanded in [false, true, false] {
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(runtime.item_expanded(&item.id), expanded);
            for width in [320.0, 900.0] {
                rendered(runtime, width, expanded);
            }
            let (catalog, _) = runtime.selection_document();
            if expanded {
                assert_eq!(catalog.text(identity).as_deref(), Some(body.as_str()));
            } else {
                assert!(
                    catalog.text(identity).is_none(),
                    "Ctrl+A must not include a closed context body"
                );
            }
            assert!(
                catalog
                    .text(selection::logical_identity("user-notice-control", &[2]))
                    .is_some(),
                "existing User notice previews stay selectable"
            );
            drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
        }
        drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
        assert!(
            !app.chat_runtime_mut(target)
                .unwrap()
                .item_expanded(&item.id)
        );
        let task = app.handle_chat_ui(Action::Copy(target, item.id.clone(), CopyPart::Text));
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let copied = executor.block_on(async {
            let mut effects = iced::runtime::task::into_stream(task).unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(effect) = effects.next().await {
                    if let iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    ) = effect
                    {
                        return contents;
                    }
                }
                panic!("Copy must issue an actual clipboard write");
            })
            .await
            .unwrap()
        });
        assert_eq!(copied, format!("Claude context\n\n{body}"));
        executor.shutdown_background();
    }
}

#[test]
fn provider_context_find_reveals_selected_scope_and_restores_disclosure() {
    let parent = context("Base directory for this skill: /fixture\nparent-context-needle");
    let child = context("Conversation summary\nchild-context-needle");
    let (runtime, handle) = prepared_runtime_for_test(&[parent]);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![chat::Subagent {
        items: vec![Arc::new(child)],
        ..super::tests::native_child_fixture()
    }];
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    for (scope, needle) in [
        (None, "parent-context-needle"),
        (Some("native-child".to_owned()), "child-context-needle"),
    ] {
        drop(app.handle_chat_ui(Action::SelectSubagent(target, scope)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(runtime);
        assert!(!runtime.item_expanded("provider-context"));
        drop(find_navigation::find_for_test(
            &mut app,
            target,
            needle.into(),
        ));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(
            runtime.item_expanded("provider-context"),
            "Find must reveal the actual matching context"
        );
        assert_eq!(
            runtime.find_navigation.current_hit().unwrap().0,
            "provider-context"
        );
        rendered(runtime, 520.0, true);
        let (catalog, _) = runtime.selection_document();
        assert!(
            catalog
                .text(selection::logical_identity("provider-context", &[2]))
                .unwrap()
                .contains(needle)
        );
        drop(app.handle_chat_ui(Action::CloseFind(target)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(!runtime.item_expanded("provider-context"));
        rendered(runtime, 520.0, false);
    }
}
