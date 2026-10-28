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

fn copy_action(runtime: &Runtime) -> Action {
    let item = runtime.active_item("provider-context").unwrap();
    let mut footer = runtime.message_copy(item);
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(footer.as_widget());
    let size = iced::Size::new(520.0, 100.0);
    let node = footer.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, size),
    );
    let accessible =
        footer
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    let bounds = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .find(|node| node.node().label() == Some("Copy this message"))
        .expect("the context footer retains an accessible Copy action")
        .node()
        .bounds()
        .unwrap();
    let cursor = iced::mouse::Cursor::Available(iced::Point::new(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    ));
    let mut messages = Vec::new();
    for event in [
        iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
        iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
    ] {
        footer.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(event),
            Layout::new(&node),
            cursor,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut iced::advanced::Shell::new(&mut messages),
            &iced::Rectangle::with_size(size),
        );
    }
    assert_eq!(messages.len(), 1);
    let Message::ChatUi(action) = messages.pop().unwrap() else {
        panic!("context Copy must dispatch a chat action");
    };
    action
}

fn clipboard(task: Task<Message>, expected: Option<&str>) {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), async {
            if let Some(mut effects) = iced::runtime::task::into_stream(task) {
                while let Some(effect) = effects.next().await {
                    if let iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    ) = effect
                    {
                        assert_eq!(Some(contents.as_str()), expected);
                        return;
                    }
                }
            }
            assert!(
                expected.is_none(),
                "canonical Copy must write the full body"
            );
        })
        .await
        .expect("context clipboard work must settle");
    });
    executor.shutdown_timeout(Duration::from_secs(1));
}

#[test]
fn provider_context_copy_footer_uses_canonical_scope_and_recovers_after_failure() {
    let _clipboard_context = text_copy::isolated_context();
    for full_body in [
        "short original context λ\r\n".to_owned(),
        format!(
            "FIRST_CONTEXT\n{}LAST_CONTEXT λ\r\n",
            "opaque context\n".repeat(240_000)
        ),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = crate::app::chat_ui::tests::history_fixture(&mut app);
        app.workspace.focus(card).unwrap();
        let parent_source = chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: chat::TextCopyScope::Conversation,
            token: 7,
            revision: 1,
            bytes: full_body.len() as u64,
        };
        let child_body = "distinct child context λ\r\n";
        let child_source = chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: chat::TextCopyScope::Subagent {
                id: "native-child".into(),
            },
            bytes: child_body.len() as u64,
            ..parent_source.clone()
        };
        // UI publications contain a resident fragment only. Canonical page
        // assembly is exercised by the real Store test; completion fixtures
        // here exercise the actual footer and asynchronous clipboard guards.
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items = vec![Arc::new(context("resident parent fragment"))];
        snapshot.subagents = vec![chat::Subagent {
            items: vec![Arc::new(context(child_body))],
            ..super::tests::native_child_fixture()
        }];
        snapshot.text_copy_sources.conversation = vec![Some(parent_source.clone())];
        snapshot
            .text_copy_sources
            .children
            .insert("native-child".into(), vec![Some(child_source.clone())]);
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        for (scope, source, body) in [
            (None, parent_source.clone(), full_body.as_str()),
            (Some("native-child".to_owned()), child_source, child_body),
        ] {
            drop(app.handle_chat_ui(Action::SelectSubagent(target, scope)));
            let action = copy_action(app.chat_runtime_mut(target).unwrap());
            assert!(
                matches!(&action, Action::CopyMessage(actual, id, Some(captured))
                if *actual == target && id == "provider-context" && captured == &source)
            );
            clipboard(
                app.handle_chat_ui(Action::CopyMessage(target, "provider-context".into(), None)),
                None,
            );
            assert!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("unavailable")
            );
            let request = text_copy::Request::for_test(handle.clone(), source.clone());
            clipboard(
                app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "provider-context".into(),
                    request,
                    Err("canonical context read failed".into()),
                )),
                None,
            );
            assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());

            let stale = text_copy::Request::for_test(handle.clone(), source.clone());
            let mut next = (*handle.snapshot()).clone();
            match &source.scope {
                chat::TextCopyScope::Conversation => {
                    next.text_copy_sources.conversation_revision += 1
                }
                chat::TextCopyScope::Subagent { id } => {
                    next.text_copy_sources.children.get_mut(id).unwrap()[0]
                        .as_mut()
                        .unwrap()
                        .revision += 1
                }
            }
            next.generation += 1;
            handle.publish_snapshot_for_test(next);
            app.chat_runtime_mut(target).unwrap().refresh();
            clipboard(
                app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "provider-context".into(),
                    stale,
                    Ok(body.into()),
                )),
                None,
            );
            let Action::CopyMessage(_, _, Some(current)) =
                copy_action(app.chat_runtime_mut(target).unwrap())
            else {
                panic!("fresh context Copy must retain canonical authority");
            };
            let request = text_copy::Request::for_test(handle.clone(), current);
            clipboard(
                app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "provider-context".into(),
                    request,
                    Ok(body.into()),
                )),
                Some(body),
            );
            assert!(app.chat_runtime_mut(target).unwrap().copied.is_some());
            app.chat_runtime_mut(target).unwrap().copied = None;
        }
    }
}

#[test]
fn provider_context_closed_widget_and_catalog_preserve_literal_body() {
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
