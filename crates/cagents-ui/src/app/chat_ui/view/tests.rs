use super::*;

/// The composer's scrolled draft, its fade, caret and selection stop on
/// the first whole device row inside its border, at the composer's own
/// inset, border and reach, wherever it lands and at any scale (#2824).
/// Reaching the whole inset would take in the antialiased border's rows.
#[test]
fn composer_text_reach_stops_inside_its_border() {
    let padding = composer_editor_padding();
    let reach = (COMPOSER_TEXT_REACH, COMPOSER_TEXT_REACH);
    for scale in [0.8, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
        for top in [0.0, 573.73, 580.4, 580.5, 611.15, 777.95] {
            let bounds = iced::Rectangle {
                x: 326.3,
                y: top,
                width: 720.0,
                height: 12.0 * COMPOSER_LINE_HEIGHT + 2.0 * COMPOSER_INSET_Y,
            };
            let (drawn, depth) = crate::ui_controls::field::padding_fade_geometry(
                bounds, padding, reach, reach, scale,
            );
            let context = format!("scale {scale}, top {top}: {drawn:?}");
            let border_top = ((bounds.y + design::BORDER_WIDTH) * scale).ceil();
            let border_bottom = ((bounds.y + bounds.height - design::BORDER_WIDTH) * scale).floor();
            let drawn_top = (drawn.y * scale).round();
            let drawn_bottom = ((drawn.y + drawn.height) * scale).round();
            assert!(drawn_top >= border_top, "{context}");
            assert!(drawn_bottom <= border_bottom, "{context}");
            // The band is not given up: it reaches the first row inside.
            assert!(drawn_top - border_top <= 1.0, "{context}");
            assert!(border_bottom - drawn_bottom <= 1.0, "{context}");
            assert!(depth.0 > 0.0 && depth.1 > 0.0, "{context}");
        }
    }
}

#[test]
fn saved_history_composer_offers_attachment_in_its_actions_without_enabling_send() {
    use iced::advanced::{
        Layout, Shell, clipboard, layout, mouse,
        widget::{Operation, Tree, operation::Focusable},
    };
    struct Bounds {
        id: widget::Id,
        rect: Option<iced::Rectangle>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn Focusable,
        ) {
            if id == Some(&self.id) {
                self.rect = Some(bounds);
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    for transitioning in [false, true] {
        let (mut runtime, _) = frame_cost::live_runtime(&[]);
        runtime.session.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        runtime.session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        runtime.session.proof = Some(crate::model::HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        });
        let snapshot = Snapshot {
            session_id: runtime.session.launch.session_id.clone(),
            exited: true,
            read_only_reason: Some("Working directory no longer exists".into()),
            ..Default::default()
        };
        runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
            runtime.session.clone(),
            snapshot.clone(),
        )));
        runtime.snapshot = Arc::new(snapshot);
        assert!(runtime.has_connected_saved_history(&runtime.snapshot));
        assert!(!runtime.snapshot.capabilities.attachments);
        let target = runtime.target;
        // Attaching is one of the `+` menu's choices, and opening the
        // menu starts keyboard focus on it.
        runtime.composer_actions = true;
        assert_eq!(
            runtime.composer_action_rows(transitioning),
            [(ComposerRow::Attach, !transitioning)]
        );
        let mut element = runtime.view(
            None,
            transitioning,
            true,
            &shortcuts,
            600.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        );
        let mut tree = Tree::new(element.as_widget());
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 800.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        let mut overlay = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                iced::Vector::ZERO,
            )
            .expect("the open actions menu draws over the transcript");
        let menu = overlay.as_overlay_mut().layout(&renderer, viewport.size());
        let mut bounds = Bounds {
            id: card_control_id(target.card, "chat-attach"),
            rect: None,
        };
        overlay
            .as_overlay_mut()
            .operate(Layout::new(&menu), &renderer, &mut bounds);
        let rect = bounds
            .rect
            .expect("the actions menu offers its attachment control");
        assert!(rect.width > 0.0 && rect.height > 0.0 && rect.y < viewport.height);
        let cursor = mouse::Cursor::Available(iced::Point::new(rect.center_x(), rect.center_y()));
        let mut messages = vec![];
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            let mut shell = Shell::new(&mut messages);
            overlay.as_overlay_mut().update(
                &iced::Event::Mouse(event),
                Layout::new(&menu),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
            );
        }
        if transitioning {
            assert!(messages.is_empty(), "switching disables draft controls");
        } else {
            // The press admits exactly one chooser, without editing or
            // submitting the draft beneath the menu.
            assert!(
                matches!(messages.as_slice(), [
                    Message::ChatUi(Action::Attach(found)),
                ] if *found == target),
                "attachment control emitted unexpected messages: {messages:?}; bounds={rect:?}"
            );
        }
    }
}

#[test]
fn review_artifact_button_retains_exact_uri_and_rendered_origin_without_answering() {
    use iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
    use iced::{Event, Point, Rectangle, Size};
    for (question, button_label, expected_uri) in [
        (
            "Compare tree-real-*.png and [Expanded](docs/tree%20expanded.png).",
            "Open artifact: Expanded",
            "docs/tree%20expanded.png",
        ),
        (
            "Compare tree-real-*.png and [Manifest](Cargo.toml:42).",
            "Open artifact: Manifest",
            "Cargo.toml:42",
        ),
        (
            "Compare tree-real-*.png and [Source](main.rs:42:7).",
            "Open artifact: Source",
            "main.rs:42:7",
        ),
        (
            "Compare tree-real-*.png and [Unicode source](src/résumé.rs:9:2).",
            "Open artifact: Unicode source",
            "src/résumé.rs:9:2",
        ),
        (
            "Compare tree-real-*.png and [Source revision](file:///producing/worktree/main.rs?revision=7:3#L12:3).",
            "Open artifact: Source revision",
            "file:///producing/worktree/main.rs?revision=7:3#L12:3",
        ),
        (
            "Compare tree-real-*.png and [Filtered source](FILE:/producing/worktree/main.rs?filter=*?[one]{a,b}#section[one]).",
            "Open artifact: Filtered source",
            "FILE:/producing/worktree/main.rs?filter=*?[one]{a,b}#section[one]",
        ),
        (
            "Compare tree-real-*.png and [Literal source](file:///producing/worktree/literal%2A%3F%5Bone%5D.rs?revision=7:3#L12:3).",
            "Open artifact: Literal source",
            "file:///producing/worktree/literal%2A%3F%5Bone%5D.rs?revision=7:3#L12:3",
        ),
    ] {
        let request = chat::Request {
            kind: chat::RequestKind::Question,
            id: "review".into(),
            title: "Review".into(),
            description: String::new(),
            fields: vec![chat::Field {
                id: question.into(),
                label: question.into(),
                description: "Screenshots".into(),
                choices: vec![],
                multiple: false,
                secret: false,
                initial: String::new(),
            }],
            actions: vec![],
        };
        let answers = Answers::new();
        let target = Target {
            card: CardId::new(),
            generation: 17,
        };
        let document = Some(PathBuf::from("/producing/worktree/.chat"));
        let mut element =
            request_view(target, &request, &answers, true, RequestSurface::Transcript)
                .map(move |message| capture_document_link(message, target, &document));
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(520.0, 1000.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let buttons: Vec<_> = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node
                        .node()
                        .label()
                        .is_some_and(|label| label.starts_with("Open artifact:"))
            })
            .collect();
        assert_eq!(
            buttons.len(),
            1,
            "the bare pattern never becomes a file action"
        );
        assert_eq!(buttons[0].node().label(), Some(button_label));
        let bounds = buttons[0].node().bounds().unwrap();
        let cursor = mouse::Cursor::Available(Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        ));
        let mut messages = Vec::new();
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::OpenDocumentFrom(found, Some(origin), uri))]
            if *found == target && origin == Path::new("/producing/worktree/.chat") && uri == expected_uri)
        );
        assert_eq!(
            request.fields[0].id, question,
            "the provider answer key stays literal"
        );
        assert!(
            answers.is_empty(),
            "opening an artifact must not answer the question"
        );
    }
}

#[test]
fn recorded_message_timestamp_format_uses_message_metadata() {
    let at = 1_700_000_000_000;
    let local = chat::local_time((at / 1000) as i64).unwrap();
    let same_day = chat::local_time((at / 1000 + 60) as i64).unwrap();
    let full = format!(
        "{:04}-{:02}-{:02} {}",
        local.year + 1900,
        local.month + 1,
        local.day,
        local.clock()
    );
    let stamp = recorded_message_stamp(at, Some(&same_day)).unwrap();
    assert_eq!(stamp, (at, local.clock(), full.clone()));
    let other_day = chat::local_time((at / 1000 + 172_800) as i64).unwrap();
    assert_eq!(
        recorded_message_stamp(at, Some(&other_day)),
        Some((at, full.clone(), full.clone()))
    );
    assert_eq!(
        recorded_message_stamp(at, None),
        Some((at, full.clone(), full))
    );
}

#[test]
fn recorded_timestamp_changes_refresh_retained_footer_without_heartbeat_churn() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let at = 1_700_000_000_000;
    let item = chat::Item {
        id: "dated-retained-answer".into(),
        role: chat::Role::Assistant,
        at: Some(at),
        started_at: Some(at - 120_000),
        content: ItemContent::Text {
            text: "Finished answer".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    };
    let (mut runtime, handle) = frame_cost::live_runtime(&[item]);
    let mut before = runtime.turn_fingerprint(&runtime.turns[0], false);
    let revision = runtime.cache["dated-retained-answer"].revision;
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(runtime.find_turn_view_for_test().as_widget());
    let inspect = |runtime: &Runtime, tree: &mut Tree| {
        let mut element = runtime.find_turn_view_for_test();
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1000.0)),
        );
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut texts);
        texts.0
    };
    let initial = runtime.cache["dated-retained-answer"]
        .stamp
        .as_ref()
        .unwrap()
        .1
        .clone();
    assert!(inspect(&runtime, &mut tree).contains(&initial));
    let mut previous_label = Some(initial);
    for (recorded, changes_label) in [
        (Some(at + 1), false),
        (Some(at + 60_000), true),
        (None, true),
    ] {
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        Arc::make_mut(&mut snapshot.items[0]).at = recorded;
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        preparation::drain_fixture(&mut runtime);
        assert_eq!(
            runtime.cache["dated-retained-answer"].revision, revision,
            "unchanged message body keeps prepared cache"
        );
        let after = runtime.turn_fingerprint(&runtime.turns[0], false);
        assert_eq!(
            after != before,
            changes_label,
            "retained footer tracks its visible time, not subminute observations"
        );
        before = after;
        let texts = inspect(&runtime, &mut tree);
        let label = runtime.cache["dated-retained-answer"]
            .stamp
            .as_ref()
            .map(|stamp| stamp.1.clone());
        if let Some(label) = &label {
            assert!(
                texts.contains(label),
                "same retained Tree draws current recorded label"
            );
        }
        if let Some(previous) = &previous_label
            && label.as_ref() != Some(previous)
        {
            assert!(
                !texts.contains(previous),
                "same retained Tree withdraws obsolete recorded time"
            );
        }
        previous_label = label;
    }
}

#[test]
fn recorded_timestamps_reach_parent_child_and_rebuilt_message_copy_footers() {
    use iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<(String, iced::Rectangle)>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds));
        }
    }
    let at = 1_700_000_000_000;
    let parent = chat::Item {
        id: "dated-prompt".into(),
        role: chat::Role::User,
        at: Some(at),
        started_at: Some(at - 120_000),
        content: ItemContent::Text {
            text: "Recorded prompt".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    };
    let (mut runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&parent));
    let child = chat::Item {
        // Opaque message IDs can collide across transcripts.
        id: parent.id.clone(),
        role: chat::Role::Assistant,
        at: Some(at + 120_000),
        content: ItemContent::Text {
            text: "Recorded child answer".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        ..parent.clone()
    };
    let mut snapshot = (*handle.snapshot()).clone();
    // Publish the permission metadata already accepted by this Runtime;
    // otherwise its no-thread draft fallback must clone each fresh Arc.
    snapshot.permission_mode = runtime.snapshot.permission_mode.clone();
    snapshot.generation += 1;
    let child_id = native_child_fixture().id;
    snapshot.subagents.push(chat::Subagent {
        items: vec![Arc::new(child.clone())],
        ..native_child_fixture()
    });
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    let renderer = crate::app::tests::headless_renderer();
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    for (selected, item) in [(None, &parent), (Some(child_id), &child)] {
        let child_selected = selected.is_some();
        let before_selection = app.chat_runtime_mut(target).unwrap().snapshot.clone();
        drop(app.handle_chat_ui(Action::SelectSubagent(target, selected)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(runtime);
        assert!(Arc::ptr_eq(&before_selection, &runtime.snapshot));
        assert_eq!(runtime.cache[&parent.id].stamp.as_ref().unwrap().0, at);
        let expected = runtime.presentation_cache()[&item.id]
            .stamp
            .clone()
            .expect("parent and child retain recorded time");
        assert_eq!(expected.0, item.at.unwrap());
        let before = runtime.snapshot.clone();
        // Actual disclosure actions rebuild the cache without any new
        // provider publication. The recorded label stays available.
        drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(Arc::ptr_eq(&before, &runtime.snapshot));
        assert_eq!(
            runtime.presentation_cache()[&item.id].stamp.as_ref(),
            Some(&expected)
        );
        let mut footer = runtime.message_copy(item);
        let mut tree = Tree::new(footer.as_widget());
        let node = footer.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 100.0)),
        );
        let mut texts = Texts::default();
        footer
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        let (_, stamp_bounds) = texts
            .0
            .iter()
            .find(|(text, _)| text == &expected.1)
            .expect("real footer contains recorded label");
        let accessible = footer.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let copy = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some("Copy this message")
            })
            .expect("actual Copy action remains accessible");
        let bounds = copy.node().bounds().unwrap();
        assert!(
            stamp_bounds.x >= bounds.x1 as f32,
            "timestamp follows Copy in the same footer"
        );
        let cursor = iced::Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 100.0));
        let mut messages = Vec::new();
        for button in [
            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
        ] {
            footer.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(button),
                Layout::new(&node),
                iced::mouse::Cursor::Available(cursor),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0], Message::ChatUi(Action::Copy(target, id, CopyPart::Text) | Action::CopyMessage(target, id, _)) if *target == runtime.target && id == &item.id)
        );
        drop(footer);
        if child_selected {
            let revision = app.chat_runtime_mut(target).unwrap().child_cache[&item.id].revision;
            let mut previous_label = expected.1;
            for recorded in [Some(at + 180_000), None] {
                let mut snapshot = (*handle.snapshot()).clone();
                snapshot.generation += 1;
                Arc::make_mut(&mut snapshot.subagents[0].items[0]).at = recorded;
                handle.publish_delta_snapshot_for_test(snapshot);
                let runtime = app.chat_runtime_mut(target).unwrap();
                runtime.refresh();
                preparation::drain_fixture(runtime);
                assert_eq!(
                    runtime.child_cache[&item.id].revision, revision,
                    "clock-only publication retains prepared child body"
                );
                assert_eq!(
                    runtime.cache[&parent.id].stamp.as_ref().unwrap().0,
                    at,
                    "same child ID cannot replace the parent timestamp"
                );
                let current = &runtime.snapshot.subagents[0].items[0];
                let label = runtime.child_cache[&item.id]
                    .stamp
                    .as_ref()
                    .map(|stamp| stamp.1.clone());
                let mut footer = runtime.message_copy(current);
                tree.diff(footer.as_widget_mut());
                let node = footer.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 100.0)),
                );
                let mut texts = Texts::default();
                footer.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut texts,
                );
                if let Some(label) = &label {
                    assert!(
                        texts.0.iter().any(|(text, _)| text == label),
                        "same native footer Tree draws the changed child timestamp"
                    );
                }
                assert!(
                    !texts.0.iter().any(|(text, _)| text == &previous_label),
                    "same native footer Tree withdraws the obsolete timestamp"
                );
                previous_label = label.unwrap_or_default();
            }
        }
    }
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    Arc::make_mut(&mut snapshot.items[0]).at = None;
    Arc::make_mut(&mut snapshot.subagents[0].items[0]).at = None;
    handle.publish_delta_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    assert!(runtime.cache[&parent.id].stamp.is_none());
    assert!(
        runtime.child_cache[&child.id].stamp.is_none(),
        "missing recorded time never falls back to the current clock"
    );
}

#[test]
fn recorded_selected_child_actions_preserve_disclosure_of_colliding_parent() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                self.0.push(prose.plain.to_string());
            }
        }
    }
    let parent = chat::Item {
        id: "shared-thought".into(),
        role: chat::Role::Assistant,
        at: Some(1_700_000_000_000),
        started_at: Some(1_700_000_000_000),
        content: ItemContent::Text {
            text: "Parent preview\nDistinct parent reasoning".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
    };
    let child = chat::Item {
        content: ItemContent::Text {
            text: "Child preview\nDistinct child reasoning".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        ..parent.clone()
    };
    let (mut runtime, handle) = frame_cost::live_runtime(&[parent]);
    let mut snapshot = (*handle.snapshot()).clone();
    // Publish the permission metadata already accepted by this Runtime;
    // otherwise its no-thread draft fallback must clone each fresh Arc.
    snapshot.permission_mode = runtime.snapshot.permission_mode.clone();
    snapshot.generation += 1;
    snapshot.subagents.push(chat::Subagent {
        status: chat::ToolStatus::Succeeded,
        items: vec![Arc::new(child)],
        ..native_child_fixture()
    });
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let renderer = crate::app::tests::headless_renderer();
    let inspect = |runtime: &Runtime| {
        let item = runtime.active_item("shared-thought").unwrap();
        let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
        let mut element = runtime.prose_view(
            0,
            item,
            &runtime.presentation_cache()[&item.id],
            turn::Slot::Thinking,
            settings,
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1000.0)),
        );
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        texts.0.join(" ")
    };
    assert!(!inspect(app.chat_runtime_mut(target).unwrap()).contains("Distinct parent reasoning"));
    drop(app.handle_chat_ui(Action::Find(target, "Distinct parent reasoning".into())));
    assert!(inspect(app.chat_runtime_mut(target).unwrap()).contains("Distinct parent reasoning"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    drop(app.handle_chat_ui(Action::Toggle(target, "shared-thought".into())));
    drop(app.handle_chat_ui(Action::Unfold(target, "shared-thought#output".into())));
    assert!(inspect(app.chat_runtime_mut(target).unwrap()).contains("Distinct child reasoning"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(!runtime.item_expanded("shared-thought"));
    assert!(!runtime.part_unfolded("shared-thought#output"));
    assert!(!inspect(runtime).contains("Distinct parent reasoning"));
    drop(app.handle_chat_ui(Action::Toggle(target, "shared-thought".into())));
    drop(app.handle_chat_ui(Action::Unfold(target, "shared-thought#output".into())));
    assert!(inspect(app.chat_runtime_mut(target).unwrap()).contains("Distinct parent reasoning"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.item_expanded("shared-thought"));
    assert!(runtime.part_unfolded("shared-thought#output"));
    assert!(inspect(runtime).contains("Distinct child reasoning"));
    drop(app.handle_chat_ui(Action::Toggle(target, "shared-thought".into())));
    drop(app.handle_chat_ui(Action::Unfold(target, "shared-thought#output".into())));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.item_expanded("shared-thought"));
    assert!(runtime.part_unfolded("shared-thought#output"));
    assert!(inspect(runtime).contains("Distinct parent reasoning"));
}

#[test]
fn recorded_tool_timestamp_reserves_footer_space_at_narrow_and_wide_widths() {
    use iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<(String, iced::Rectangle)>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds));
        }
    }
    let item = chat::Item {
        id: "dated-tool-command".into(),
        role: chat::Role::Assistant,
        at: Some(1_700_000_000_000),
        started_at: None,
        content: ItemContent::Command {
            label: "Run focused tests".into(),
            command: "cargo test --lib recorded_timestamp".into(),
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(1000),
            output: "all tests passed".into(),
        },
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    runtime.expanded.edit().insert(item.id.clone());
    let stamp = runtime.cache[&item.id]
        .stamp
        .as_ref()
        .expect("recorded command time is prepared")
        .1
        .clone();
    let renderer = crate::app::tests::headless_renderer();
    for width in [160.0, 240.0, 600.0] {
        let mut element = runtime.step_view(
            &item,
            &runtime.cache[&item.id],
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
        );
        let mut tree = Tree::new(element.as_widget());
        let limits =
            layout::Limits::new(iced::Size::new(width, 0.0), iced::Size::new(width, 1000.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let layout = Layout::new(&node);
        let header = layout.children().next().unwrap().bounds();
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, layout, &renderer, &mut texts);
        let stamp_bounds = texts
            .0
            .iter()
            .find(|(text, _)| text == &stamp)
            .map(|(_, bounds)| bounds)
            .expect("expanded detail retains the recorded time");
        let accessible =
            element
                .as_widget()
                .a11y_nodes(layout, &tree, iced::mouse::Cursor::Unavailable);
        let copy = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some("Copy this message")
            })
            .unwrap()
            .node()
            .bounds()
            .unwrap();
        assert!(
            copy.y0 >= f64::from(header.y + header.height),
            "Copy has reserved footer space below the header"
        );
        assert!(
            stamp_bounds.y >= header.y + header.height,
            "recorded time does not cover the command or duration"
        );
        assert!(stamp_bounds.x >= copy.x1 as f32);
        assert!(stamp_bounds.x + stamp_bounds.width <= width + 0.1);
        assert!(copy.x0 >= 0.0 && copy.x1 <= f64::from(width) + 0.1);
        assert!(stamp_bounds.y + stamp_bounds.height <= node.size().height + 0.1);
        let cursor = iced::Point::new(
            ((copy.x0 + copy.x1) / 2.0) as f32,
            ((copy.y0 + copy.y1) / 2.0) as f32,
        );
        let viewport = iced::Rectangle::with_size(iced::Size::new(width, 1000.0));
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: cursor }),
            layout,
            iced::mouse::Cursor::Available(cursor),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        for button in [
            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(button),
                layout,
                iced::mouse::Cursor::Available(cursor),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0], Message::ChatUi(Action::Copy(target, id, CopyPart::Text) | Action::CopyMessage(target, id, _)) if *target == runtime.target && id == &item.id)
        );
        let after = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(
            node.size(),
            after.size(),
            "hover/copy does not change the reserved row height"
        );
    }
}

pub(super) fn native_child_fixture() -> chat::Subagent {
    chat::Subagent {
        id: "native-child".into(),
        kind: "Scout".into(),
        description: "Inspect repository".into(),
        status: chat::ToolStatus::Running,
        model: None,
        waiting: false,
        detail: String::new(),
        background: true,
        items: Vec::new(),
    }
}

#[test]
fn native_cost_estimates_are_labeled_and_partial_coverage_is_explicit() {
    let (label, tooltip) = cost_estimate_label(crate::chat::pricing::CostEstimate {
        microusd: 25_000,
        partial: false,
    });
    assert_eq!(label, "$0.03 est.");
    assert!(tooltip.contains("ChatGPT plan usage is not billed per token"));
    let (label, tooltip) = cost_estimate_label(crate::chat::pricing::CostEstimate {
        microusd: 1_000,
        partial: true,
    });
    assert_eq!(label, "<$0.01 est. · observed");
    assert!(tooltip.contains("partial estimate"));
}

#[test]
fn fast_mode_explains_preference_and_cooldown_without_protocol_terms() {
    let preference = crate::chat::FastMode {
        state: crate::chat::FastModeState::Off,
        disabled_reason: Some("sdk_opt_in_required".into()),
    };
    assert!(preference.changeable());
    assert!(fast_mode_tooltip(&preference).starts_with("Enable faster responses"));
    assert!(!fast_mode_tooltip(&preference).contains("sdk"));
    let cooldown = crate::chat::FastMode {
        state: crate::chat::FastModeState::Cooldown,
        disabled_reason: None,
    };
    assert!(cooldown.enabled());
    assert!(fast_mode_tooltip(&cooldown).contains("temporarily cooling down"));
}

#[test]
fn assistant_activity_uses_action_footer_as_separation_without_hover_reflow() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    struct Probe {
        summary: String,
        bounds: HashMap<String, iced::Rectangle>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            text: &str,
        ) {
            if text == self.summary {
                self.bounds.insert("activity".into(), bounds);
            }
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                for key in [
                    "Before activity.",
                    "After activity.",
                    "New user exchange.\nA second line.",
                ] {
                    if state.plain.as_ref() == key {
                        self.bounds.insert(key.into(), bounds);
                    }
                }
            }
        }
    }
    let text = |id: &str, role, text: &str| chat::Item {
        id: id.into(),
        role,
        content: ItemContent::Text {
            text: text.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    };
    let items = vec![
        text("before", chat::Role::Assistant, "Before activity."),
        chat::Item {
            id: "tool".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Command {
                label: "Check fixture".into(),
                command: "true".into(),
                working_directory: "/project".into(),
                action: chat::CommandAction::Run,
                status: chat::ToolStatus::Succeeded,
                exit_code: Some(0),
                duration_ms: None,
                output: "visible when expanded".into(),
            },
            at: None,
            started_at: None,
        },
        text("after", chat::Role::Assistant, "After activity."),
        text(
            "new-prompt",
            chat::Role::User,
            "New user exchange.\nA second line.",
        ),
    ];
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    for width in [360.0, 780.0] {
        let (runtime, _handle) = frame_cost::live_runtime(&items);
        let summary = runtime
            .turns
            .iter()
            .find(|turn| !turn.steps.is_empty())
            .unwrap()
            .summary
            .clone();
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            width,
            0.0,
            Place { branch: None },
            true,
            None,
            Vec::new(),
            true,
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 1000.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut probe = Probe {
            summary: summary.clone(),
            bounds: Default::default(),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        let before = probe.bounds["Before activity."];
        let activity = probe.bounds["activity"];
        let after = probe.bounds["After activity."];
        let next_prompt = probe.bounds["New user exchange.\nA second line."];
        let before_gap = activity.y - (before.y + before.height);
        let after_gap = after.y - (activity.y + activity.height);
        let exchange_gap = next_prompt.y - (after.y + after.height);
        assert!(
            (0.0..=40.0).contains(&before_gap),
            "idle activity gap {before_gap} at width {width}"
        );
        assert!(
            (8.0..=24.0).contains(&after_gap),
            "activity-to-answer gap {after_gap} at width {width}"
        );
        assert!(
            exchange_gap >= before_gap && exchange_gap <= 44.0,
            "new exchange gap {exchange_gap} at width {width}"
        );
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(iced::mouse::Event::CursorMoved {
                position: before.center(),
            }),
            Layout::new(&node),
            iced::mouse::Cursor::Available(before.center()),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut iced::advanced::Shell::new(&mut messages),
            &iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(width, 1000.0)),
        );
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut hovered = Probe {
            summary,
            bounds: Default::default(),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut hovered);
        assert_eq!(
            hovered.bounds, probe.bounds,
            "hover controls must not move neighboring messages"
        );
    }
}

#[test]
fn unstarted_conversation_footer_is_a_neutral_draft_not_a_pending_connection() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let session = chat::Session {
            id: crate::model::TerminalId::new(),
            launch: chat::Launch {
                provider,
                recipe: crate::model::LaunchRecipe::shell(PathBuf::from("/unused")).unwrap(),
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        let mut draft = Runtime::draft(
            target,
            session.clone(),
            Snapshot::default(),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        assert!(draft.is_draft());
        assert!(draft.handle().is_none());
        assert!(draft.queued.is_none());
        assert!(!draft.snapshot.ready);
        assert_eq!(draft.conversation_status(false), (None, "Draft".into()));

        draft
            .composer
            .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new("Unsent local draft".into()),
            )));
        assert_eq!(draft.composer.text(), "Unsent local draft");
        assert!(draft.handle().is_none());
        assert!(draft.queued.is_none());
        assert_eq!(draft.conversation_status(false), (None, "Draft".into()));
        assert_eq!(draft.conversation_status(true).1, "Switching session");
        draft.error = Some("Local attachment could not be read".into());
        assert_eq!(
            draft.conversation_status(false),
            (Some(Icon::Warning), "Error".into())
        );

        let connecting = Runtime::new(
            target,
            session,
            Connection::Connecting,
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        assert!(!connecting.is_draft());
        assert!(connecting.handle().is_none());
        assert_eq!(
            connecting.conversation_status(false),
            (Some(Icon::AgentWorking), "Connecting".into())
        );
    }
    let (live, _handle) = frame_cost::live_runtime(&[]);
    assert!(!live.is_draft());
    assert!(live.handle().is_some());
    assert_eq!(live.conversation_status(false), (None, "Ready".into()));
}

#[test]
fn conversation_footer_requires_completion_evidence_and_tracks_delegated_waiting() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    assert_eq!(runtime.conversation_status(false).1, "Ready");
    Arc::make_mut(&mut runtime.snapshot).ready = false;
    assert_eq!(runtime.conversation_status(false).1, "Connecting");
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.ready = true;
    snapshot.finished_at = Some(42);
    assert_eq!(runtime.conversation_status(false).1, "Finished");
    Arc::make_mut(&mut runtime.snapshot).detail = "Turn interrupted".into();
    assert_eq!(runtime.conversation_status(false).1, "Interrupted");
    Arc::make_mut(&mut runtime.snapshot).detail = "Turn failed".into();
    assert_eq!(runtime.conversation_status(false).1, "Failed");
    Arc::make_mut(&mut runtime.snapshot).detail.clear();
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    assert_ne!(
        runtime.conversation_status(false).1,
        "Finished",
        "old completion cannot mask a new turn"
    );
    Arc::make_mut(&mut runtime.snapshot).error = Some("Earlier history unavailable".into());
    assert_eq!(
        runtime.conversation_status(false).0,
        Some(Icon::AgentWorking)
    );
    Arc::make_mut(&mut runtime.snapshot).waiting = true;
    assert_eq!(runtime.conversation_status(false).1, "Waiting for you");
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.waiting = false;
    snapshot.busy = false;
    assert_eq!(runtime.conversation_status(false).1, "Finished with error");
    Arc::make_mut(&mut runtime.snapshot).error = None;
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.subagents.push(native_child_fixture());
    assert_eq!(runtime.conversation_status(false).1, "1 agent working");
    Arc::make_mut(&mut runtime.snapshot).subagents[0].waiting = true;
    assert_eq!(runtime.conversation_status(false).1, "1 agent waiting");
    Arc::make_mut(&mut runtime.snapshot).subagents[0].status = chat::ToolStatus::Succeeded;
    assert_eq!(
        runtime.conversation_status(false).1,
        "Finished",
        "terminal children cannot retain waiting state"
    );
    runtime.disconnected = true;
    assert_eq!(runtime.conversation_status(false).1, "Disconnected");
}

#[test]
fn a_stopped_command_is_a_notice_rather_than_the_conversation_state() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let notice = format!(
        "{} systemd-oomd ended it; the agent is still running.",
        chat::failure::COMMAND_OUT_OF_MEMORY
    );
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = true;
    snapshot.detail = "Running command".into();
    let working = runtime.conversation_status(false);
    Arc::make_mut(&mut runtime.snapshot).error = Some(notice);
    assert_eq!(
        runtime.conversation_status(false),
        working,
        "the agent carries on, so its status still says what it is doing"
    );
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = false;
    snapshot.detail.clear();
    snapshot.finished_at = Some(42);
    assert_eq!(runtime.conversation_status(false).1, "Finished");
    Arc::make_mut(&mut runtime.snapshot).finished_at = None;
    assert_eq!(runtime.conversation_status(false).1, "Ready");
    // The agent itself stopping is still the conversation's state.
    Arc::make_mut(&mut runtime.snapshot).error = Some(format!(
        "{} systemd-oomd ended it.",
        chat::failure::PROVIDER_OUT_OF_MEMORY
    ));
    assert_eq!(
        runtime.conversation_status(false),
        (Some(Icon::Warning), "Stopped: out of memory".into())
    );
}

#[test]
fn agent_status_counts_only_active_states_and_identifies_the_current_failure() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    for (index, status) in [
        chat::ToolStatus::Running,
        chat::ToolStatus::Running,
        chat::ToolStatus::Waiting,
        chat::ToolStatus::Failed,
        chat::ToolStatus::Stopped,
        chat::ToolStatus::Ended,
        chat::ToolStatus::Succeeded,
    ]
    .into_iter()
    .enumerate()
    {
        let mut child = native_child_fixture();
        child.id = format!("child-{index}");
        child.status = status;
        child.waiting = index == 1;
        child.description = "Check restart safety".into();
        snapshot.subagents.push(child);
    }
    assert_eq!(
        runtime.conversation_status(false).1,
        "1 agent working · 1 waiting · 1 waiting on background work · 1 failed: Check restart safety"
    );
    assert_eq!(
        runtime.conversation_status_with_failures(false, false).1,
        "1 agent working · 1 waiting · 1 waiting on background work"
    );
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot
        .subagents
        .retain(|agent| agent.status == chat::ToolStatus::Failed);
    assert_eq!(
        runtime.timeline_activity(false, None),
        Some((Icon::Warning, "1 failed: Check restart safety".into()))
    );
}

#[test]
fn status_failure_button_opens_the_exact_transcript_and_multiple_failures_open_a_filtered_chooser()
{
    use iced::advanced::clipboard;
    use iced::runtime::user_interface::{Cache, UserInterface};
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let mut app = crate::app::tests::context_inspector_test_app(true);
    runtime.target.card = app.workspace.focused().unwrap();
    let target = runtime.target;
    let mut failed = native_child_fixture();
    failed.status = chat::ToolStatus::Failed;
    failed.description = "Check restart safety".into();
    failed.detail = "The soak test crashed".into();
    Arc::make_mut(&mut runtime.snapshot).subagents.push(failed);
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = UserInterface::build(
        activity_row(
            Some(Icon::Warning),
            String::new(),
            runtime.agent_failure_control(),
        ),
        iced::Size::new(800.0, 36.0),
        Cache::new(),
        &mut renderer,
    );
    let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
    let bounds = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| node.node().label() == Some("Open failed agent: Check restart safety"))
        .and_then(|node| node.node().bounds())
        .expect("an actionable named failure");
    let pointer = iced::Point::new(
        (bounds.x0 + bounds.x1) as f32 / 2.0,
        (bounds.y0 + bounds.y1) as f32 / 2.0,
    );
    let mut messages = Vec::new();
    ui.update(
        &[
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: pointer }),
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
        ],
        iced::mouse::Cursor::Available(pointer),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    drop(ui);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let select = messages
        .into_iter()
        .find_map(|message| match message {
            Message::ChatUi(action @ Action::SelectSubagent(..)) => Some(action),
            _ => None,
        })
        .expect("failure click selects a transcript");
    drop(app.handle_chat_ui(select));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert_eq!(runtime.selected_subagent.as_deref(), Some("native-child"));
    let mut second = runtime.snapshot.subagents[0].clone();
    second.id = "second-failure".into();
    Arc::make_mut(&mut runtime.snapshot).subagents.push(second);
    drop(app.handle_chat_ui(Action::SubagentFailures(target)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    let picker = runtime.child_picker.as_ref().unwrap();
    assert_eq!(picker.filter, child_navigation::Filter::Failed);
    assert_eq!(
        picker
            .rows(&runtime.snapshot.subagents, &runtime.child_ordinals)
            .len(),
        2
    );
}

#[test]
fn long_activity_remains_one_line_in_a_narrow_pane() {
    use iced::advanced::{layout, widget::Tree};
    let renderer = crate::app::tests::headless_renderer();
    let mut row = activity_row(
        Some(Icon::AgentWorking),
        "7 agents working\n1 waiting on background work · ".repeat(20),
        None,
    );
    let mut tree = Tree::new(row.as_widget());
    let node = row.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(240.0, 200.0)),
    );
    assert_eq!(node.size().width, 240.0);
    assert!(
        node.size().height < 30.0,
        "status wrapped: {:?}",
        node.size()
    );
}

#[test]
fn contentless_thinking_draws_no_row_and_leaves_the_working_indicator_in_place() {
    let item = |id: &str, role, content| chat::Item {
        id: id.into(),
        role,
        content,
        started_at: Some(1_000),
        at: Some(2_000),
    };
    let command = |id: &str, status| {
        item(
            id,
            chat::Role::Assistant,
            ItemContent::Command {
                label: String::new(),
                command: "cargo test".into(),
                working_directory: "/project".into(),
                action: chat::CommandAction::Run,
                status,
                exit_code: None,
                duration_ms: None,
                output: String::new(),
            },
        )
    };
    // What Claude sends for most thinking: a signature and no text.
    let thought = item(
        "thought",
        chat::Role::Assistant,
        ItemContent::Text {
            text: String::new(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
    );
    let prompt = item(
        "prompt",
        chat::Role::User,
        ItemContent::Text {
            text: "Run the tests".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    );
    let regroup = |runtime: &mut Runtime| {
        runtime.turns = turn::group(
            &runtime.snapshot.items,
            &runtime.asked_after,
            crate::app::chat_ui::Presentations(&runtime.cache),
        )
        .into();
    };
    let (mut runtime, _handle) =
        frame_cost::live_runtime(&[prompt, command("c1", chat::ToolStatus::Succeeded), thought]);
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    regroup(&mut runtime);
    let placed = |runtime: &Runtime| {
        runtime
            .turns
            .iter()
            .flat_map(|turn| turn.steps.iter().chain(&turn.prose))
            .map(|placed| placed.index)
            .collect::<Vec<_>>()
    };
    // While the agent thinks with nothing to show, the thought draws no row
    // and nothing in the transcript claims the work, so the closing line
    // keeps its glyph and says the agent is thinking.
    assert_eq!(placed(&runtime), [1], "the thought has no row");
    assert!(!runtime.work_shown(None));
    let (glyph, label) = runtime
        .timeline_activity(false, None)
        .expect("the lead is working");
    assert_eq!(
        runtime.closing_line(glyph, label, false, None),
        (Some(Icon::AgentWorking), "Thinking".to_owned())
    );

    // The next command joins the one before the thought: one group.
    Arc::make_mut(&mut runtime.snapshot)
        .items
        .push(Arc::new(command("c2", chat::ToolStatus::Running)));
    regroup(&mut runtime);
    assert_eq!(runtime.turns.len(), 1);
    assert_eq!(
        runtime.turns[0]
            .steps
            .iter()
            .map(|step| step.index)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!(placed(&runtime), [1, 3]);
    assert_eq!(
        runtime.strip_status(&runtime.turns[0]),
        Some(chat::ToolStatus::Running)
    );
}

#[test]
fn a_working_turn_shows_one_activity_indicator_on_its_newest_live_row() {
    use iced::advanced::{layout, widget::Tree};
    let item = |id: &str, role, content| chat::Item {
        id: id.into(),
        role,
        content,
        started_at: Some(1_000),
        at: Some(2_000),
    };
    let command = |id: &str, status| {
        item(
            id,
            chat::Role::Assistant,
            ItemContent::Command {
                label: String::new(),
                command: "cargo test".into(),
                working_directory: "/project".into(),
                action: chat::CommandAction::Run,
                status,
                exit_code: None,
                duration_ms: None,
                output: String::new(),
            },
        )
    };
    // A thought with words to show is a row of its own; one with nothing to
    // read is not (see the test below).
    let thought = item(
        "thought",
        chat::Role::Assistant,
        ItemContent::Text {
            text: "**Checking the results**".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
    );
    let prompt = item(
        "prompt",
        chat::Role::User,
        ItemContent::Text {
            text: "Run the tests".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    );
    let regroup = |runtime: &mut Runtime| {
        runtime.turns = turn::group(
            &runtime.snapshot.items,
            &runtime.asked_after,
            crate::app::chat_ui::Presentations(&runtime.cache),
        )
        .into();
    };
    let running = command("c1", chat::ToolStatus::Running);
    let (mut runtime, _handle) =
        frame_cost::live_runtime(&[prompt.clone(), running.clone(), thought.clone()]);
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    regroup(&mut runtime);
    let closing = |runtime: &Runtime| {
        let (glyph, label) = runtime
            .timeline_activity(false, None)
            .expect("the lead is working");
        runtime.closing_line(glyph, label, false, None)
    };
    let blank = (None, " ".to_owned());
    let turn = runtime.turns.last().unwrap();
    // A command still running while the agent thinks: the thought is the
    // newest work, so the strip keeps its glyph and the closing line
    // keeps its place empty.
    assert_eq!(turn.glyph(), Some(chat::ToolStatus::Running));
    assert!(turn.summary.ends_with(" · working"));
    assert_eq!(runtime.strip_status(turn), None);
    assert!(runtime.work_shown(None));
    assert_eq!(closing(&runtime), blank);
    assert_eq!(
        runtime.step_status(&runtime.snapshot.items[1]),
        None,
        "a step inside a strip leaves the indicator to the strip"
    );

    // The emptied closing line still tells assistive technology what
    // the agent is doing.
    {
        use iced::advanced::Layout;
        let renderer = crate::app::tests::headless_renderer();
        let mut element = runtime.closing_row(Icon::AgentWorking, "Thinking".into(), None, None);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 200.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node.node().label() == Some("Thinking"))
        );
    }

    // A closing line that says more than the row keeps its words.
    Arc::make_mut(&mut runtime.snapshot).subagents = vec![native_child_fixture()];
    assert_eq!(closing(&runtime), (None, "1 agent working".to_owned()));
    Arc::make_mut(&mut runtime.snapshot).subagents.clear();

    // The command left running in the background while the agent goes
    // on to newer steps: one group per stretch of tools, and only the
    // newest work carries the indicator, whichever group holds it.
    let next = command("c2", chat::ToolStatus::Running);
    Arc::make_mut(&mut runtime.snapshot)
        .items
        .push(Arc::new(next.clone()));
    regroup(&mut runtime);
    assert_eq!(runtime.turns.len(), 2);
    let (older, newer) = (&runtime.turns[0], &runtime.turns[1]);
    assert_eq!(older.glyph(), Some(chat::ToolStatus::Running));
    assert_eq!(runtime.strip_status(older), None);
    assert_eq!(runtime.strip_status(newer), Some(chat::ToolStatus::Running));
    assert_eq!(closing(&runtime), blank);

    // Between steps nothing in the transcript is live, so the closing
    // line says what the agent is doing, and no strip spins beside it.
    Arc::make_mut(&mut runtime.snapshot).items[3] =
        Arc::new(command("c2", chat::ToolStatus::Succeeded));
    regroup(&mut runtime);
    assert!(!runtime.work_shown(None));
    assert!(
        runtime
            .turns
            .iter()
            .all(|turn| runtime.strip_status(turn) != Some(chat::ToolStatus::Running))
    );
    assert_eq!(
        closing(&runtime),
        (Some(Icon::AgentWorking), "Working".to_owned())
    );

    // Once the lead is idle, the background command's group is the work
    // still under way.
    Arc::make_mut(&mut runtime.snapshot).busy = false;
    assert_eq!(
        runtime.strip_status(&runtime.turns[0]),
        Some(chat::ToolStatus::Running)
    );
    assert_eq!(
        runtime.strip_status(&runtime.turns[1]),
        Some(chat::ToolStatus::Succeeded)
    );

    // Once subagents work after the lead has finished, the closing line
    // spins for them and the background command's group keeps its glyph.
    Arc::make_mut(&mut runtime.snapshot).subagents = vec![native_child_fixture()];
    assert!(runtime.closing_spins());
    assert_eq!(runtime.strip_status(&runtime.turns[0]), None);
    Arc::make_mut(&mut runtime.snapshot).subagents.clear();

    // A running coordination card states its run in words, so while the
    // lead waits on it the closing line carries the indicator.
    {
        let spawn = item(
            "spawn",
            chat::Role::Assistant,
            ItemContent::Tool {
                name: "Agent".into(),
                input: serde_json::json!({
                    "subagent_type": "reviewer",
                    "description": "Review the change",
                    "prompt": "Review the change"
                })
                .to_string(),
                output: String::new(),
                status: chat::ToolStatus::Running,
            },
        );
        let (mut waiting, handle) = frame_cost::live_runtime(&[prompt.clone(), spawn.clone()]);
        Arc::make_mut(&mut waiting.snapshot).busy = true;
        regroup(&mut waiting);
        assert!(
            waiting.cache["spawn"]
                .coordination
                .as_deref()
                .is_some_and(Coordinated::is_card)
        );
        assert!(!waiting.work_shown(None));
        let (glyph, label) = waiting.timeline_activity(false, None).unwrap();
        assert_eq!(
            waiting.closing_line(glyph, label, false, None).0,
            Some(Icon::AgentWorking)
        );

        // The same holds in a subagent's own transcript, whose cards are
        // drawn from its own cache.
        let agent = chat::Subagent {
            items: vec![Arc::new(chat::Item {
                id: "child-spawn".into(),
                ..spawn
            })],
            status: chat::ToolStatus::Running,
            ..native_child_fixture()
        };
        Arc::make_mut(&mut waiting.snapshot)
            .subagents
            .push(agent.clone());
        waiting.selected_subagent = Some(agent.id.clone());
        let mut published = (*waiting.snapshot).clone();
        published.generation += 1;
        handle.publish_delta_snapshot_for_test(published);
        waiting.refresh();
        let snapshot = waiting.snapshot.clone();
        waiting.prepare_child_cache(&snapshot);
        preparation::drain_fixture(&mut waiting);
        assert!(
            waiting.child_cache["child-spawn"]
                .coordination
                .as_deref()
                .is_some_and(Coordinated::is_card)
        );
        assert!(!waiting.work_shown(Some(&agent)));
        assert_eq!(
            waiting
                .closing_line(Icon::AgentWorking, "Working".into(), false, Some(&agent))
                .0,
            Some(Icon::AgentWorking)
        );
    }

    // In a subagent's own transcript steps stand alone, and only the
    // newest one carries the indicator.
    let mut agent = chat::Subagent {
        id: "child".into(),
        items: vec![Arc::new(running.clone())],
        ..native_child_fixture()
    };
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(agent.clone());
    runtime.selected_subagent = Some("child".into());
    assert_eq!(
        runtime.step_status(&running),
        Some(chat::ToolStatus::Running)
    );
    assert!(runtime.work_shown(Some(&agent)));
    agent.items.push(Arc::new(thought.clone()));
    Arc::make_mut(&mut runtime.snapshot).subagents[0] = agent.clone();
    assert_eq!(runtime.step_status(&running), None);
    assert!(runtime.work_shown(Some(&agent)));
    assert_eq!(
        runtime.closing_line(Icon::AgentWorking, "Thinking".into(), false, Some(&agent)),
        blank
    );

    // The empty closing line is the activity row's height.
    let renderer = crate::app::tests::headless_renderer();
    let height = |mut element: Element<'_, Message>| {
        let mut tree = Tree::new(element.as_widget());
        element
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 200.0)),
            )
            .size()
            .height
    };
    assert_eq!(
        height(activity_row(None, " ".into(), None)),
        height(activity_row(
            Some(Icon::AgentWorking),
            "Thinking".into(),
            None
        ))
    );
}

#[test]
fn timeline_activity_follows_real_work_and_settles_into_the_finished_stamp() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let activity = |runtime: &Runtime| runtime.timeline_activity(false, None);
    // Nothing runs, so nothing is said.
    assert_eq!(activity(&runtime), None);
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.ready = true;
    snapshot.busy = true;
    snapshot.detail = "Reviewing changes".into();
    let items = runtime.snapshot.items.len();
    assert_eq!(
        activity(&runtime),
        Some((Icon::AgentWorking, "Reviewing changes".into()))
    );
    // Subagents are counted beside the parent's own work, and the row is
    // derived, never appended to the conversation.
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.subagents = vec![native_child_fixture(), native_child_fixture()];
    snapshot.subagents[1].id = "second-child".into();
    assert_eq!(
        activity(&runtime),
        Some((
            Icon::AgentWorking,
            "Reviewing changes · 2 agents working".into()
        ))
    );
    assert_eq!(runtime.snapshot.items.len(), items);
    Arc::make_mut(&mut runtime.snapshot).busy = false;
    assert_eq!(
        activity(&runtime),
        Some((Icon::AgentWorking, "2 agents working".into()))
    );
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.subagents[0].waiting = true;
    snapshot.subagents[1].status = chat::ToolStatus::Succeeded;
    assert_eq!(
        activity(&runtime),
        Some((Icon::AgentWaiting, "1 agent waiting".into()))
    );
    Arc::make_mut(&mut runtime.snapshot).waiting = true;
    assert_eq!(
        activity(&runtime).map(|(glyph, _)| glyph),
        Some(Icon::AgentWaiting)
    );

    // The selected subagent's own work closes its transcript.
    let mut child = native_child_fixture();
    child.detail = "Reading the parser".into();
    assert_eq!(
        runtime.timeline_activity(false, Some(&child)),
        Some((Icon::AgentWorking, "Reading the parser".into()))
    );
    child.status = chat::ToolStatus::Succeeded;
    assert_eq!(runtime.timeline_activity(false, Some(&child)), None);

    // Where the transcript already says what is happening, or the end of
    // the conversation is not shown, there is no row.
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.waiting = false;
    snapshot.busy = true;
    assert!(activity(&runtime).is_some());
    assert_eq!(runtime.timeline_activity(true, None), None);
    Arc::make_mut(&mut runtime.snapshot).has_later = true;
    assert_eq!(activity(&runtime), None);
    Arc::make_mut(&mut runtime.snapshot).has_later = false;
    runtime.disconnected = true;
    assert_eq!(activity(&runtime), None);
    runtime.disconnected = false;

    // Once work ends the row goes, and the stamp says how the turn ended.
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = false;
    snapshot.subagents.clear();
    snapshot.finished_at = Some(42);
    snapshot.detail = "Turn interrupted".into();
    assert_eq!(activity(&runtime), None);
    assert_eq!(runtime.finished_label(), "Interrupted");
    Arc::make_mut(&mut runtime.snapshot).detail.clear();
    assert_eq!(runtime.finished_label(), "Finished");
    Arc::make_mut(&mut runtime.snapshot).error = Some("Provider failed".into());
    assert_eq!(runtime.finished_label(), "Finished with error");
    // Once stamped, how the turn ended stays put through later notices
    // and errors that are not about it.
    runtime.finished_stamp = Some(FinishedStamp {
        at: 42,
        clock: "12:00".into(),
        full: "2026-11-05 12:00".into(),
        outcome: "Interrupted",
    });
    Arc::make_mut(&mut runtime.snapshot).detail = "Reconnected".into();
    runtime.error = Some("Could not change the model".into());
    assert_eq!(runtime.finished_label(), "Interrupted");
    // Choosing a model is not a message on its way: it neither reads as
    // sending nor takes the stamp away. A message does.
    runtime.error = None;
    let model = Command::SetModel {
        model: "haiku".into(),
        reasoning: None,
    };
    let pending = commands::PendingCommand::new(1, &model);
    let snapshot = runtime.snapshot.clone();
    assert!(runtime.commands.push(pending, model, &snapshot).is_some());
    assert!(!runtime.sending());
    assert_eq!(activity(&runtime), None);
    runtime.commands = commands::Commands::default();
    let message = Command::Prompt {
        text: "next".into(),
        attachments: Vec::new(),
        mode: PromptMode::Queue,
    };
    let pending = commands::PendingCommand::new(1, &message);
    assert!(runtime.commands.push(pending, message, &snapshot).is_some());
    assert!(runtime.sending());
    assert_eq!(
        activity(&runtime),
        Some((Icon::AgentWorking, "Sending".into()))
    );
}

#[test]
fn conversation_tab_marks_only_the_main_conversations_own_state() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let state = |runtime: &Runtime| runtime.parent_tab_state().map(|(glyph, _)| glyph);
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.ready = true;
    snapshot.subagents = vec![native_child_fixture()];
    // A subagent working or waiting is said on its own tab, not here.
    assert_eq!(state(&runtime), None);
    Arc::make_mut(&mut runtime.snapshot).subagents[0].waiting = true;
    assert_eq!(state(&runtime), None);
    // The main conversation's own work, question and failure are.
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    assert_eq!(state(&runtime), Some(Icon::AgentWorking));
    Arc::make_mut(&mut runtime.snapshot).waiting = true;
    assert_eq!(state(&runtime), Some(Icon::AgentWaiting));
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = false;
    snapshot.waiting = false;
    snapshot.subagents.clear();
    snapshot.finished_at = Some(42);
    // A turn that ended well needs no mark; one that failed does.
    assert_eq!(state(&runtime), None);
    Arc::make_mut(&mut runtime.snapshot).detail = "Turn interrupted".into();
    assert_eq!(state(&runtime), None);
    Arc::make_mut(&mut runtime.snapshot).detail = "Turn failed".into();
    assert_eq!(runtime.parent_tab_state(), Some((Icon::Warning, "Failed")));
}

#[test]
fn an_agent_on_its_background_work_is_reported_without_asking_for_the_user() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let mut child = native_child_fixture();
    child.status = chat::ToolStatus::Waiting;
    assert_eq!(
        Runtime::native_child_status(&child),
        "Waiting on background work"
    );
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.subagents.push(child);
    snapshot.finished_at = Some(42);
    let (glyph, label) = runtime.conversation_status(false);
    assert!(
        label.contains("1 agent waiting on background work"),
        "{label}"
    );
    assert!(!label.contains("Waiting for you"), "{label}");
    assert!(!label.contains("Finished"), "{label}");
    // Neither working, asking, nor done.
    assert_eq!(glyph, None);
    // The row outlasts the card scrolling away, and the same quiet glyph as
    // the card says nothing is asked of the reader.
    assert_eq!(
        runtime.timeline_activity(false, None),
        Some((
            Icon::AgentBackground,
            "1 agent waiting on background work".into()
        ))
    );
    let child = runtime.snapshot.subagents[0].clone();
    assert_eq!(
        runtime.timeline_activity(false, Some(&child)),
        Some((Icon::AgentBackground, "Waiting on background work".into()))
    );
}

#[test]
fn native_child_identity_uses_its_reported_model_and_waiting_state() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    Arc::make_mut(&mut runtime.snapshot).selected_model = Some("parent-model".into());
    let mut child = native_child_fixture();
    let unknown = runtime.native_child_identity(&child);
    assert!(unknown.contains("Claude native subagent"));
    assert!(unknown.contains("Model not reported"));
    assert!(!unknown.contains("parent-model"));
    child.model = Some("child-model".into());
    child.waiting = true;
    child.detail = "Waiting for your approval".into();
    let known = runtime.native_child_identity(&child);
    assert!(known.contains("child-model"));
    assert!(known.contains("Waiting for you"));
    assert!(known.contains("Waiting for your approval"));
    // A provider's raw error payload is summarized, not quoted.
    child.detail =
        r#"{"message":"You've hit your limit · resets 1:30pm","type":"rate_limit_error"}"#.into();
    let limited = runtime.native_child_identity(&child);
    assert!(limited.contains("Claude usage limit reached · resets 1:30pm"));
    assert!(!limited.contains("rate_limit_error"));
    // A finished child's own report is quoted, never read as a failure.
    child.waiting = false;
    child.status = chat::ToolStatus::Succeeded;
    child.detail = "psql gave connection refused until postgres started".into();
    let report = runtime.native_child_identity(&child);
    assert!(report.contains("psql gave connection refused"));
    assert!(!report.contains("connection was interrupted"));
    child.status = chat::ToolStatus::Succeeded;
    assert_eq!(Runtime::native_child_status(&child), "Finished");
}

#[test]
fn cold_child_find_acceptance_changes_retained_revision_without_another_query_edit() {
    use iced::advanced::{layout, widget::Tree};
    use iced::futures::StreamExt;
    let item = |id: &str, text: String| {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text,
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        })
    };
    let (runtime, handle) = frame_cost::live_runtime(&[]);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![chat::Subagent {
        items: vec![
            item("short", "child needle".into()),
            Arc::new(chat::Item {
                id: "other".into(),
                role: chat::Role::Assistant,
                content: ItemContent::Tool {
                    name: "Read".into(),
                    input: "{}".into(),
                    output: format!("{}needle", "unrelated\n".repeat(5000)),
                    status: chat::ToolStatus::Succeeded,
                },
                at: None,
                started_at: None,
            }),
        ],
        ..native_child_fixture()
    }];
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
    let mut task = app.resume_current_find(target, false);
    // The admitted raw snapshot predates reader preparation. Its first
    // accepted Pattern cannot authorize uncapping a later ready reader.
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    let before = app
        .chat_runtime_mut(target)
        .unwrap()
        .child_find_revision()
        .unwrap();
    assert_eq!(
        before.1, 0,
        "raw worker has not published the shared Pattern"
    );
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let mut tree = Tree::empty();
    let render = |app: &mut Cagents, tree: &mut Tree| {
        let runtime = app.chat_runtime_mut(target).unwrap();
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            600.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        );
        tree.diff(&mut element);
        element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 800.0)),
        );
    };
    render(&mut app, &mut tree);
    let built = virtualized::constructed_rows();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for _ in 0..8 {
        let next = executor.block_on(async {
            let mut stream =
                iced::runtime::task::into_stream(task).expect("real pending raw Find task");
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(effect) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        action @ (Action::FindGroups(..)
                        | Action::FindResume(..)
                        | Action::FindAvailable),
                    ))) = effect
                    {
                        return action;
                    }
                }
                panic!("raw Find completion");
            })
            .await
            .unwrap()
        });
        task = app.handle_chat_ui(next);
        if find_navigation::render_revision(target) != 0 {
            break;
        }
    }
    drop(task);
    let after = app
        .chat_runtime_mut(target)
        .unwrap()
        .child_find_revision()
        .unwrap();
    assert_eq!(
        before.0, after.0,
        "completion must not invent another user query edit"
    );
    assert_ne!(
        before, after,
        "accepted Pattern invalidates retained short native child rows"
    );
    assert!(after.1 != 0);
    render(&mut app, &mut tree);
    assert!(
        virtualized::constructed_rows() > built,
        "the existing native Tree rebuilds the changed child row"
    );
    // The first accepted raw snapshot names a closed step. Explicit
    // disclosure creates its actual reader before the next raw scan;
    // laying out the closed row alone does not prepare ToolText.
    assert!(
        app.chat_runtime_mut(target).unwrap().child_cache["other"]
            .tool_output
            .is_none(),
        "the initial raw plan predates actual reader readiness"
    );
    drop(app.handle_chat_ui(Action::Toggle(target, "other".into())));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    assert!(app.chat_runtime_mut(target).unwrap().item_expanded("other"));
    render(&mut app, &mut tree);
    let (reader, text, cache_revision) = {
        let cached = &app.chat_runtime_mut(target).unwrap().child_cache["other"];
        let output = cached.tool_output.as_ref().unwrap();
        (output.find_identity(), output.text.clone(), cached.revision)
    };
    assert!(!find_navigation::tool_uncapped(target, reader, &text));
    // A real manual navigation cancellation leaves the current query and
    // accepted stable groups intact, allowing the coalesced raw refresh.
    app.chat_runtime_mut(target)
        .unwrap()
        .find_navigation
        .cancel();
    let task = app.resume_current_find(target, false);
    let raw_reply = |task| {
        executor.block_on(async {
            let mut stream = iced::runtime::task::into_stream(task)
                .expect("same-query current-reader raw worker");
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(effect) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        action @ Action::FindGroups(..),
                    ))) = effect
                    {
                        return action;
                    }
                }
                panic!("second actual raw worker completion");
            })
            .await
            .unwrap()
        })
    };
    drop(app.handle_chat_ui(raw_reply(task)));
    let current = app.chat_runtime_mut(target).unwrap();
    let reader_phase = current.child_find_revision().unwrap();
    assert_eq!(
        reader_phase.0, after.0,
        "reader readiness does not edit the query"
    );
    assert_ne!(
        reader_phase.1, after.1,
        "new exact uncapping bindings advance the accepted render phase"
    );
    assert_eq!(
        current.child_cache["other"].revision, cache_revision,
        "only accepted reader bindings changed"
    );
    assert!(find_navigation::tool_uncapped(target, reader, &text));
    let built = virtualized::constructed_rows();
    render(&mut app, &mut tree);
    assert!(
        virtualized::constructed_rows() > built,
        "the same retained native Tree rebuilds for the second accepted reader plan"
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents[0]
        .items
        .push(item("tail", "fresh unrelated tail".into()));
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    app.chat_runtime_mut(target)
        .unwrap()
        .find_navigation
        .cancel();
    let task = app.resume_current_find(target, false);
    drop(app.handle_chat_ui(raw_reply(task)));
    assert_eq!(
        find_navigation::render_revision(target),
        reader_phase.1,
        "an actual new raw result for unrelated tail append reuses unchanged exact reader bindings"
    );
}

#[test]
fn selected_child_find_navigation_resolves_real_scoped_widget_anchors() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation::Outcome},
    };
    use iced::futures::StreamExt;
    let item = |id: &str, text: &str| chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: text.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    };
    let (runtime, handle) = frame_cost::live_runtime(&[item("bc", "parent needle")]);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents = [("a", "bc"), ("ab", "c")]
        .into_iter()
        .map(|(child, id)| chat::Subagent {
            id: child.into(),
            items: vec![Arc::new(item(id, "child needle"))],
            ..native_child_fixture()
        })
        .collect();
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let renderer = crate::app::tests::headless_renderer();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let mut previous_anchor = None;
    for (child, item, framed) in [("a", "bc", "1:abc"), ("ab", "c", "2:abc")] {
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some(child.into()))));
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        let task = app.handle_chat_ui(Action::Find(target, "needle".into()));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.find_matches.iter().collect::<Vec<_>>(), vec![item]);
        let expected =
            iced::advanced::widget::Id::new(format!("chat-child-item-{}-{framed}", target.card));
        assert_ne!(
            previous_anchor.as_ref(),
            Some(&expected),
            "opaque child/item boundaries must be unambiguous"
        );
        previous_anchor = Some(expected.clone());
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            600.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 800.0)),
        );
        struct Anchors {
            expected: iced::advanced::widget::Id,
            count: usize,
        }
        impl Operation for Anchors {
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(policy) = state.downcast_mut::<virtualized::PlaceholderPolicy>() {
                    policy.visit = false;
                }
            }
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn container(&mut self, id: Option<&iced::advanced::widget::Id>, _: iced::Rectangle) {
                self.count += usize::from(id == Some(&self.expected));
            }
        }
        let mut anchors = Anchors { expected, count: 0 };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut anchors);
        assert_eq!(
            anchors.count, 1,
            "actual selected child widget owns its Find anchor"
        );
        executor.block_on(async {
            let mut stream = iced::runtime::task::into_stream(task)
                .expect("Find must navigate a visible child match");
            let Some(iced::runtime::Action::Widget(mut operation)) =
                tokio::time::timeout(Duration::from_millis(250), stream.next())
                    .await
                    .unwrap()
            else {
                panic!("Find must produce its scroll widget operation");
            };
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                operation.as_mut(),
            );
            assert!(
                matches!(operation.finish(), Outcome::Chain(_)),
                "Find navigation must resolve the selected child row, not an absent parent turn"
            );
        });
    }
    executor.shutdown_background();
}

#[test]
fn actual_find_actions_mount_deep_logical_matches_and_reject_late_navigation() {
    drive_deep_find(false);
}
#[test]
fn actual_find_current_target_appends_keep_pending_geometry_through_parser_publications() {
    drive_deep_find(true);
}
#[test]
fn actual_find_navigation_keeps_its_thirty_second_expiry() {
    use iced::futures::{FutureExt, StreamExt};
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let (runtime, _handle) = frame_cost::live_runtime(&[]);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let mut stream = iced::runtime::task::into_stream(app.begin_find_navigation(target, None))
        .expect("navigation owns its production deadline");
    executor.block_on(async {
        // Iced tasks yield before polling their futures. Receiving the
        // actual resume proves both batched timers have started.
        let started = tokio::time::Instant::now();
        assert!(matches!(
            stream.next().await,
            Some(iced::runtime::Action::Output(cosmic::Action::App(
                Message::ChatUi(Action::FindResume(..))
            )))
        ));
        tokio::time::advance(started + Duration::from_secs(29) - tokio::time::Instant::now()).await;
        assert!(stream.next().now_or_never().is_none());
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .find_navigation
                .pending()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let Some(iced::runtime::Action::Output(cosmic::Action::App(
            message @ Message::ChatUi(Action::FindExpired(..)),
        ))) = stream.next().await
        else {
            panic!("the production deadline must expire navigation at thirty seconds");
        };
        assert_eq!(
            tokio::time::Instant::now(),
            started + Duration::from_secs(30)
        );
        drop(app.update(message));
        assert!(
            !app.chat_runtime_mut(target)
                .unwrap()
                .find_navigation
                .pending()
        );
    });
    executor.shutdown_background();
}

fn drive_deep_find(append_target: bool) {
    use iced::advanced::{
        Layout, Shell, layout, mouse,
        widget::{Operation, Tree, operation},
    };
    use iced::futures::{FutureExt, StreamExt, stream::BoxStream};
    type Effect = iced::runtime::Action<cosmic::Action<Message>>;
    struct Visible {
        identity: u64,
        byte: usize,
        translation: iced::Vector,
        pending: Option<(iced::Vector, iced::Rectangle)>,
        clips: Vec<iced::Rectangle>,
        found: bool,
        sample: Option<(iced::Rectangle, Vec<iced::Rectangle>)>,
    }
    impl Operation for Visible {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            let old = self.translation;
            let pushed = if let Some((translation, viewport)) = self.pending.take() {
                let viewport = iced::Rectangle {
                    x: viewport.x - self.translation.x,
                    y: viewport.y - self.translation.y,
                    ..viewport
                };
                self.clips.push(viewport);
                self.translation += translation;
                true
            } else {
                false
            };
            visit(self);
            if pushed {
                self.clips.pop();
            }
            self.translation = old;
        }
        fn scrollable(
            &mut self,
            _: Option<&widget::Id>,
            viewport: iced::Rectangle,
            _: iced::Rectangle,
            translation: iced::Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            self.pending = Some((translation, viewport));
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
                && let Some(local) = prose.logical_byte_bounds(self.identity, self.byte)
            {
                let glyph = iced::Rectangle {
                    x: bounds.x + local.x - self.translation.x,
                    y: bounds.y + local.y - self.translation.y,
                    ..local
                };
                self.sample = Some((glyph, self.clips.clone()));
                self.found |= !self.clips.is_empty()
                    && self
                        .clips
                        .iter()
                        .all(|clip| glyph.intersection(clip).is_some());
            }
        }
    }
    fn view<'a>(
        runtime: &'a Runtime,
        shortcuts: &'a crate::settings::KeyboardShortcuts,
    ) -> Element<'a, Message> {
        runtime.view(
            None,
            false,
            true,
            shortcuts,
            600.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        )
    }
    fn push(streams: &mut Vec<BoxStream<'static, Effect>>, task: Task<Message>) {
        if let Some(stream) = iced::runtime::task::into_stream(task) {
            streams.push(stream.boxed());
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    // This fixture holds geometry through 24 real parser publications; use
    // simulated frame time for its functional navigation deadline.
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(append_target)
        .build()
        .unwrap();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let needle = "FW16_UNIQUE_DEEP_NEEDLE";
    let paragraph = format!("{} {needle}", "wide prose café λ ".repeat(16_000));
    let code = format!(
        "```text\n{}{} {needle}\n```",
        "bounded line\n".repeat(2_000),
        "W".repeat(8_000)
    );
    let nested = format!(
        "{}> {needle}",
        (0..300)
            .map(|index| format!("> Nested paragraph {index}\n>\n"))
            .collect::<String>()
    );
    let output = format!(
        "{}{} {needle}",
        "tool line\n".repeat(2_000),
        "Z".repeat(8_000)
    );
    for (child, content, path, byte) in [
        (
            false,
            ItemContent::Text {
                text: paragraph.clone(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            vec![0, 0],
            paragraph.find(needle).unwrap(),
        ),
        (
            true,
            ItemContent::Text {
                text: code.clone(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            vec![0, 0],
            code.find(needle).unwrap() - "```text\n".len(),
        ),
        (
            false,
            ItemContent::Text {
                text: nested,
                thinking: false,
                phase: chat::Phase::Final,
            },
            vec![0, 0, 300],
            0,
        ),
        (
            false,
            ItemContent::Tool {
                name: "FixtureInspect".into(),
                input: "fixture".into(),
                output: output.clone(),
                status: chat::ToolStatus::Succeeded,
            },
            vec![5],
            output.find(needle).unwrap(),
        ),
    ]
    .into_iter()
    .take(if append_target { 1 } else { 4 })
    {
        let item = chat::Item {
            id: "deep-answer".into(),
            role: chat::Role::Assistant,
            content,
            at: None,
            started_at: None,
        };
        let (runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
        let target = runtime.target;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        // Native navigation mutations require an authoritative open card,
        // in addition to its live runtime. Exercise that admission guard.
        let mut card = crate::app::tests::terminal_card_state();
        card.id = target.card;
        card.generation = target.generation;
        card.kind = CardKind::Chat(runtime.session.clone());
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(card);
        durable.focused = Some(target.card);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        if child {
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.subagents = vec![chat::Subagent {
                id: "deep-child".into(),
                items: vec![Arc::new(item.clone())],
                ..native_child_fixture()
            }];
            handle.publish_snapshot_for_test(snapshot);
            app.chat_runtime_mut(target).unwrap().refresh();
            drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("deep-child".into()))));
            preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        }
        let delayed = !append_target && !child && path == [0, 0];
        if append_target {
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.busy = true;
            snapshot.items[0] = Arc::new(item.clone());
            handle.publish_snapshot_for_test(snapshot);
            app.chat_runtime_mut(target).unwrap().refresh();
            preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        }
        let mut streams = Vec::new();
        let mut availability = Box::pin(flow_layout::availability_stream());
        push(
            &mut streams,
            app.handle_chat_ui(Action::Find(target, needle.into())),
        );
        let identity = selection::logical_identity(&item.id, &path);
        // Stall the actual worker boundary for the single large paragraph.
        // Dropping the sender on panic releases the reclaimer immediately.
        let mut release = delayed.then(preparation::hold_retirement);
        let stale_request = delayed.then(|| {
            let cache = flow_layout::Cache::default();
            let entry = cache.entry(
                u64::MAX,
                vec![iced::widget::text::Span::new("old leaf".to_owned())].into(),
            );
            let mut state = entry.borrow_mut();
            state.bind_target(target);
            state.configure(600.0, iced::Pixels(14.0), iced::Font::default());
            state
                .request()
                .expect("real request from an evicted unrelated leaf")
        });
        let mut stalled_locations = 0;
        let mut tree = Tree::empty();
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 500.0));
        let limits = layout::Limits::new(iced::Size::ZERO, viewport.size());
        let deadline =
            executor.block_on(async { tokio::time::Instant::now() }) + Duration::from_secs(30);
        let mut seen = false;
        let mut passes = 0;
        let mut last_geometry = None;
        let mut locations = std::collections::VecDeque::new();
        let mut preparation_jobs = HashSet::new();
        let mut buffered_reply = None;
        let mut target_publications = 0;
        let mut accepted_publications = 0;
        while executor.block_on(async { tokio::time::Instant::now() }) < deadline {
            passes += 1;
            // Materialize the actual admitted parser subscriptions once per
            // recipe, including their real spawn_blocking worker/results.
            for subscription in app
                .chat_runtime_mut(target)
                .unwrap()
                .preparation
                .subscriptions(target)
            {
                for recipe in iced::advanced::subscription::into_recipes(subscription) {
                    let mut hash = iced::advanced::subscription::Hasher::default();
                    recipe.hash(&mut hash);
                    if preparation_jobs.insert(std::hash::Hasher::finish(&hash)) {
                        streams.push(
                            recipe
                                .stream(iced::futures::stream::empty().boxed())
                                .map(|message| {
                                    iced::runtime::Action::Output(cosmic::Action::App(message))
                                })
                                .boxed(),
                        );
                    }
                }
            }
            let mut effects = Vec::new();
            executor.block_on(async {
                let mut index = 0;
                while index < streams.len() {
                    let mut finished = false;
                    for _ in 0..8 {
                        match tokio::time::timeout(Duration::from_millis(1), streams[index].next())
                            .await
                        {
                            Ok(Some(effect)) => effects.push(effect),
                            Ok(None) => {
                                finished = true;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if finished {
                        drop(streams.remove(index));
                    } else {
                        index += 1;
                    }
                }
                // Drive the same coalesced current-state/change stream as
                // the native subscription, after actual permit release.
                if let Some(Some(message)) = availability.next().now_or_never() {
                    effects.push(iced::runtime::Action::Output(cosmic::Action::App(message)));
                }
                // A native redraw is paced by the event loop. Keep this
                // fixture from exhausting a synthetic iteration cap while
                // real full-context shaping is still running off-thread.
                tokio::time::sleep(Duration::from_millis(16)).await;
            });
            let runtime = app.chat_runtime_mut(target).unwrap();
            let mut element = view(runtime, &shortcuts);
            // Match the native runtime: initialize/reconcile the root tag
            // and state before diffing its children or delivering events.
            tree.diff(element.as_widget_mut());
            let mut node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut messages = Vec::new();
            for effect in effects {
                match effect {
                    iced::runtime::Action::Widget(operation) => {
                        let mut current = Some(operation);
                        while let Some(mut operation) = current.take() {
                            element.as_widget_mut().operate(
                                &mut tree,
                                Layout::new(&node),
                                &renderer,
                                operation.as_mut(),
                            );
                            if let operation::Outcome::Chain(next) = operation.finish() {
                                current = Some(next);
                            }
                        }
                        node = element
                            .as_widget_mut()
                            .layout(&mut tree, &renderer, &limits);
                    }
                    iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                        messages.push(message)
                    }
                    _ => {}
                }
            }
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let mut visible = Visible {
                identity,
                byte,
                translation: iced::Vector::ZERO,
                pending: None,
                clips: Vec::new(),
                found: false,
                sample: None,
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut visible);
            let pending = runtime.find_navigation.pending();
            if visible.sample.is_some() {
                last_geometry = visible.sample;
            }
            drop(element);
            for message in messages {
                if append_target
                    && target_publications == 0
                    && buffered_reply.is_none()
                    && matches!(&message, Message::ChatUi(Action::FlowReady(_, _, Some(_))))
                {
                    // Hold an actual produced reply, including its source
                    // and output admission, without blocking parser workers.
                    buffered_reply = Some(message);
                    continue;
                }
                if let Message::ChatUi(Action::FindLocated(_, _, _, location)) = &message {
                    if release.is_some()
                        && location
                            .as_ref()
                            .is_some_and(|location| location.shape_pending.is_some())
                    {
                        stalled_locations += 1;
                    }
                    if locations.len() == 16 {
                        locations.pop_front();
                    }
                    locations.push_back(location.clone());
                }
                push(&mut streams, app.update(message));
            }
            if release.is_some() {
                assert!(
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .find_navigation
                        .correction_count()
                        .is_some_and(|count| count < 16),
                    "delayed current geometry keeps Find pending without spending its correction budget"
                );
                if stalled_locations >= 24 {
                    release.take().unwrap().send(()).unwrap();
                } else {
                    // This exact stale same-card completion is rejected by
                    // source ownership, but still delivers the real handler's
                    // geometry wake. It cannot burn the current Find budget.
                    push(
                        &mut streams,
                        app.handle_chat_ui(Action::FlowReady(
                            target,
                            stale_request.as_ref().unwrap().clone(),
                            None,
                        )),
                    );
                }
            }
            if buffered_reply.is_some() {
                let runtime = app.chat_runtime_mut(target).unwrap();
                assert!(
                    runtime
                        .find_navigation
                        .correction_count()
                        .is_some_and(|count| count < 16),
                    "current target publications preserve the pending Find correction budget"
                );
                let current = runtime.active_item(&item.id).unwrap().clone();
                let accepted = runtime.cache.get(&item.id).is_some_and(|cached| {
                    !cached.placeholder
                        && cached
                            .item
                            .as_ref()
                            .is_some_and(|shown| Arc::ptr_eq(shown, &current))
                });
                if accepted
                    && runtime.find_navigation.current_hit()
                        == Some((item.id.as_str(), identity, byte))
                {
                    let (catalog, _) = runtime.selection_document();
                    let actual = catalog
                        .owned_search_blocks(&HashSet::from([item.id.clone()]))
                        .into_iter()
                        .find(|(_, _, text)| text.contains(needle))
                        .unwrap();
                    assert_eq!((actual.1, actual.2.find(needle)), (identity, Some(byte)));
                    accepted_publications = target_publications;
                    if target_publications == 24 {
                        assert!(
                            actual.2.ends_with(" verified append 24"),
                            "the exact current accepted catalog contains the final source publication"
                        );
                        push(&mut streams, app.update(buffered_reply.take().unwrap()));
                    } else {
                        target_publications += 1;
                        use chat::presentation::{ItemChange, Presentation};
                        let previous = handle.snapshot();
                        let old = previous
                            .items
                            .iter()
                            .find(|source| source.id == item.id)
                            .unwrap();
                        let mut next = (**old).clone();
                        let ItemContent::Text { text, .. } = &mut next.content else {
                            unreachable!();
                        };
                        text.push_str(&format!(" verified append {target_publications}"));
                        let change = ItemChange::between(Some(old), &next);
                        assert!(
                            matches!(change, ItemChange::Append { .. }),
                            "the real presentation validator receives an append delta"
                        );
                        let mut metadata = (*previous).clone();
                        metadata.generation += 1;
                        metadata.items.clear();
                        handle.publish_presentation_for_test(Presentation {
                            base_generation: previous.generation,
                            file_changes_unchanged: false,
                            metadata,
                            order: vec![item.id.clone()],
                            changes: vec![change],
                            children: vec![],
                        });
                        let next = handle.snapshot().items[0].clone();
                        assert!(
                            handle.known_append(old, &next),
                            "actual client lineage authenticates each current-target publication"
                        );
                        push(
                            &mut streams,
                            app.handle_chat_ui(Action::Wake(target, Wake::Changed)),
                        );
                    }
                }
            }
            if append_target {
                // All UI borrows have ended. Acknowledge retired output
                // admission before the next paused-clock tick across this
                // handoff to the process-owned std reclaimer.
                preparation::retirement_barrier();
            }
            if !pending && visible.found && (!append_target || accepted_publications == 24) {
                seen = true;
                break;
            }
        }
        assert!(
            !append_target
                || (target_publications == 24
                    && accepted_publications == 24
                    && buffered_reply.is_none()),
            "more than sixteen real target appends were accepted and reindexed before releasing Flow"
        );
        assert!(
            !delayed || stalled_locations >= 24,
            "actual current geometry survived more than sixteen unrelated completion wakes"
        );
        assert!(
            seen,
            "actual Find action must expose the deep mounted glyph; child={child}, path={path:?}, byte={byte}, passes={passes}, streams={}, navigation={}, last_geometry={last_geometry:?}, locations={locations:?}",
            streams.len(),
            app.chat_runtime_mut(target)
                .unwrap()
                .find_navigation
                .diagnostic(),
        );
        // A collapsed tool exposes its output only after the real accepted
        // Find plan expands it. Verify the same source oracle after that
        // user path and actual mounted glyph, without pre-expanding it.
        let (catalog, _) = app.chat_runtime_mut(target).unwrap().selection_document();
        let actual = catalog
            .owned_search_blocks(&HashSet::from([item.id.clone()]))
            .into_iter()
            .find(|(_, _, text)| text.contains(needle))
            .expect("actual prepared source catalog includes the deep needle");
        assert_eq!(actual.1, identity, "fixture logical path {path:?}");
        assert_eq!(actual.2.find(needle), Some(byte), "fixture logical byte");
        assert_eq!(
            app.chat_runtime_mut(target).unwrap().find_matches.len(),
            1,
            "main matching-group count is preserved"
        );
        for forward in [true, false] {
            // The single match wraps in both directions through the real
            // action and creates a fresh guarded navigation intent.
            drop(app.handle_chat_ui(Action::FindNext(target, forward)));
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(runtime.find_index, 0);
            let intent = runtime.find_navigation.guard();
            assert!(intent.current());
            drop(app.handle_chat_ui(Action::Scrolled(
                target,
                child.then(|| "deep-child".into()),
                Scroll::new(0.0, 10_000.0, 500.0),
                Some(Page::Earlier),
            )));
            assert!(
                !intent.current(),
                "a late queued correction cannot override actual user navigation"
            );
            assert!(
                !app.chat_runtime_mut(target)
                    .unwrap()
                    .find_navigation
                    .pending()
            );
        }
    }
    executor.shutdown_background();
}

#[test]
fn find_reveals_long_prompts_and_restores_disclosures_in_each_transcript() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    let prompt = |suffix: &str| chat::Item {
        id: "same#opaque-id".into(),
        role: chat::Role::User,
        content: ItemContent::Text {
            text: format!("{}{suffix}", "A long prompt line.\n".repeat(128)),
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    };
    let (runtime, handle) = frame_cost::live_runtime(&[prompt("parent-only-suffix")]);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![chat::Subagent {
        items: vec![Arc::new(prompt("child-only-suffix"))],
        ..native_child_fixture()
    }];
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let renderer = crate::app::tests::headless_renderer();
    let rendered = |runtime: &mut Runtime| {
        // A newly selected child has its own actual parser preparation;
        // completing Flow cannot manufacture a missing prompt DTO.
        preparation::drain_fixture(runtime);
        let mut element = runtime.prompt_view(runtime.active_item("same#opaque-id").unwrap());
        let mut tree = Tree::new(element.as_widget());
        let _ = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 8000.0)),
        );
        for cached in runtime.cache.values().chain(runtime.child_cache.values()) {
            cached.flow_cache.complete_fixture();
        }
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 8000.0)),
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
                if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                    self.0.push(prose.plain.to_string());
                }
            }
        }
        let mut text = Text::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut text);
        text.0.join("\n")
    };
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("parent-only-suffix"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only-suffix".into(),
    ));
    assert!(
        rendered(app.chat_runtime_mut(target).unwrap()).contains("parent-only-suffix"),
        "a parent prompt Find match must be visible beyond its collapsed cutoff"
    );
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "absent".into(),
    ));
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("parent-only-suffix"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only-suffix".into(),
    ));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("parent-only-suffix"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("child-only-suffix"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "child-only-suffix".into(),
    ));
    assert!(
        rendered(app.chat_runtime_mut(target).unwrap()).contains("child-only-suffix"),
        "a child prompt Find match must be visible beyond its collapsed cutoff"
    );
    assert!(
        !app.chat_runtime_mut(target)
            .unwrap()
            .expanded
            .contains("same#opaque-id")
    );
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "absent".into(),
    ));
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("child-only-suffix"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "child-only-suffix".into(),
    ));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("child-only-suffix"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "child-only-suffix".into(),
    ));
    drop(app.handle_chat_ui(Action::Toggle(target, "same#opaque-id".into())));
    drop(app.handle_chat_ui(Action::Toggle(target, "same#opaque-id".into())));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    assert!(!rendered(app.chat_runtime_mut(target).unwrap()).contains("parent-only-suffix"));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    assert!(
        rendered(app.chat_runtime_mut(target).unwrap()).contains("child-only-suffix"),
        "explicit child expansion survives search close and transcript switches"
    );
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only-suffix".into(),
    ));
    drop(app.handle_chat_ui(Action::Toggle(target, "same#opaque-id".into())));
    drop(app.handle_chat_ui(Action::Toggle(target, "same#opaque-id".into())));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(
        rendered(app.chat_runtime_mut(target).unwrap()).contains("parent-only-suffix"),
        "explicit parent prompt choice survives Find closure too"
    );
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    assert!(rendered(app.chat_runtime_mut(target).unwrap()).contains("child-only-suffix"));
}

#[test]
fn selected_native_child_renders_identity_and_bounded_selector() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let mut child = native_child_fixture();
    child.model = Some("child-model".into());
    child.waiting = true;
    child.detail = "Waiting for your approval".into();
    runtime.selected_subagent = Some(child.id.clone());
    Arc::make_mut(&mut runtime.snapshot).subagents.push(child);
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let mut element = runtime.view(
        None,
        false,
        true,
        &shortcuts,
        600.0,
        0.0,
        Place { branch: None },
        true,
        None,
        vec![],
        true,
    );
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 800.0)),
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    let labels: Vec<_> = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect();
    assert!(
        labels
            .iter()
            .any(|label| label.contains("Claude native subagent")
                && label.contains("child-model")
                && label.contains("Waiting for you")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("Scout · Waiting · ")
                && label.contains("Task: Inspect repository")),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.contains("Conversation")),
        "{labels:?}"
    );
}

#[test]
fn native_app_reader_survives_cold_source_placeholder_until_actual_preparation_completes() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    struct Reader {
        id: widget::Id,
        move_bottom: Option<f32>,
        top: Option<f32>,
    }
    impl Operation for Reader {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            _: iced::Rectangle,
            translation: iced::Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.id) {
                self.top = Some(translation.y);
                if let Some(bottom) = self.move_bottom.take() {
                    state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(bottom),
                    });
                }
            }
        }
    }
    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match &app.cards[&target.card] {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("fixture chat"),
        }
    }
    fn view(app: &Cagents, target: Target) -> Element<'_, Message> {
        runtime(app, target).view(
            None,
            false,
            true,
            &app.settings.keyboard_shortcuts,
            620.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        )
    }
    let source = (0..80)
        .map(|index| {
            format!(
                "## Section {index:04}\n\nReading paragraph {index:04}: {}\n\n",
                "ordinary text ".repeat(12)
            )
        })
        .collect::<String>();
    let item = chat::Item {
        id: "cold-rich".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: source,
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    };
    let (mut live, handle) = frame_cost::live_runtime(&[item]);
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let mut target = live.target;
    target.card = app.workspace.focused().unwrap();
    live.target = target;
    app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(live)));
    let renderer = crate::app::tests::headless_renderer();
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(620.0, 720.0));
    let scroll = card_control_id(target.card, "chat-transcript");
    let read = |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node| {
        let mut reader = Reader {
            id: scroll.clone(),
            move_bottom: None,
            top: None,
        };
        element
            .as_widget_mut()
            .operate(tree, Layout::new(node), &renderer, &mut reader);
        reader.top.expect("actual native transcript")
    };
    let expected;
    {
        let mut element = view(&app, target);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut movement = Reader {
            id: scroll.clone(),
            move_bottom: Some(700.0),
            top: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut movement);
        expected = read(&mut element, &mut tree, &node);
        assert!(expected > 500.0);
        // No subsequent layout: hide immediately after actual navigation.
    }
    let snapshot = (*runtime(&app, target).snapshot).clone();
    app.chat_runtime_mut(target)
        .unwrap()
        .set_visible(false)
        .unwrap();
    app.chat_runtime_mut(target).unwrap().cool();
    preparation::retirement_barrier();
    assert!(runtime(&app, target).cache.is_empty());
    app.chat_runtime_mut(target)
        .unwrap()
        .set_visible(true)
        .unwrap();
    let mut tree;
    {
        let mut empty_wait = view(&app, target);
        tree = Tree::new(&empty_wait);
        empty_wait
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
    }
    handle.replace_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert!(runtime(&app, target).cache["cold-rich"].placeholder);
    assert!(runtime(&app, target).preparation.contains("cold-rich"));
    {
        let mut placeholder = view(&app, target);
        tree.diff(placeholder.as_widget_mut());
        placeholder
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
    }
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    assert!(!runtime(&app, target).cache["cold-rich"].placeholder);
    let mut ready = view(&app, target);
    tree.diff(ready.as_widget_mut());
    let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let actual = read(&mut ready, &mut tree, &node);
    assert!(
        (actual - expected).abs() <= 1.0,
        "actual App/native Tree restored {actual} instead of {expected}"
    );
}

// Task::batch wraps even zero effects in Some(empty stream). Observe the
// real native task protocol instead of its optional stream representation.
fn native_task_emits_nothing(task: Task<Message>) -> bool {
    use iced::futures::StreamExt;
    let Some(mut stream) = iced::runtime::task::into_stream(task) else {
        return true;
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let empty = executor.block_on(async {
        tokio::time::timeout(Duration::from_millis(250), stream.next())
            .await
            .expect("empty native effect stream must settle")
            .is_none()
    });
    executor.shutdown_background();
    empty
}

#[test]
fn native_paginated_latest_waits_for_newest_source_after_ack_and_preserves_child_and_failure_readers()
 {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    use iced::futures::StreamExt;
    for case in ["success-main", "failure-main", "success-child"] {
        struct Reader {
            id: widget::Id,
            move_bottom: Option<f32>,
            top: Option<f32>,
            travel: Option<f32>,
            tail: bool,
        }
        impl Operation for Reader {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                    self.tail |= prose.plain.contains("LATEST_END_NATIVE_ATTESTATION");
                }
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                viewport: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                state: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.id) {
                    self.top = Some(translation.y);
                    self.travel = Some((content.height - viewport.height).max(0.0));
                    if let Some(bottom) = self.move_bottom.take() {
                        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(bottom),
                        });
                    }
                }
            }
        }
        fn runtime(app: &Cagents, target: Target) -> &Runtime {
            match &app.cards[&target.card] {
                CardRuntime::Chat(runtime) => runtime,
                _ => panic!("fixture chat"),
            }
        }
        fn view(app: &Cagents, target: Target) -> Element<'_, Message> {
            runtime(app, target).view(
                None,
                false,
                true,
                &app.settings.keyboard_shortcuts,
                620.0,
                0.0,
                Place { branch: None },
                true,
                None,
                vec![],
                true,
            )
        }
        let source = (0..80)
            .map(|index| {
                format!(
                    "## Section {index:04}\n\nReading paragraph {index:04}: {}\n\n",
                    "ordinary text ".repeat(12)
                )
            })
            .collect::<String>();
        let item = chat::Item {
            id: "cold-rich".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: source,
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        };
        let (mut live, handle) = frame_cost::live_runtime(&[item]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let mut initial = (*handle.snapshot()).clone();
        initial.generation = 10;
        initial.has_later = true;
        let mut child = (*initial.items[0]).clone();
        child.id = "child-rich".into();
        initial.subagents.push(chat::Subagent {
            id: "selected-child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            model: None,
            waiting: false,
            detail: String::new(),
            background: false,
            items: vec![Arc::new(child)],
        });
        handle.replace_snapshot_for_test(initial);
        app.chat_runtime_mut(target).unwrap().refresh();
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(620.0, 720.0));
        let scroll = card_control_id(target.card, "chat-transcript");
        let read = |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node| {
            let mut reader = Reader {
                id: scroll.clone(),
                move_bottom: None,
                top: None,
                travel: None,
                tail: false,
            };
            element
                .as_widget_mut()
                .operate(tree, Layout::new(node), &renderer, &mut reader);
            (
                reader.top.expect("actual native transcript"),
                reader.travel.unwrap(),
                reader.tail,
            )
        };
        let expected;
        {
            let mut element = view(&app, target);
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut movement = Reader {
                id: scroll.clone(),
                move_bottom: Some(700.0),
                top: None,
                travel: None,
                tail: false,
            };
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut movement,
            );
            expected = read(&mut element, &mut tree, &node).0;
            assert!(expected > 500.0);
            // No subsequent layout: hide immediately after actual navigation.
        }
        let snapshot = (*runtime(&app, target).snapshot).clone();
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(false)
            .unwrap();
        app.chat_runtime_mut(target).unwrap().cool();
        preparation::retirement_barrier();
        assert!(runtime(&app, target).cache.is_empty());
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(true)
            .unwrap();
        let mut tree;
        {
            let mut empty_wait = view(&app, target);
            tree = Tree::new(&empty_wait);
            empty_wait
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        handle.replace_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(runtime(&app, target).cache["cold-rich"].placeholder);
        assert!(runtime(&app, target).preparation.contains("cold-rich"));
        drop(app.handle_chat_ui(Action::Latest(target)));
        let pending = runtime(&app, target)
            .commands
            .in_flight()
            .expect("actual Latest dispatch");
        assert_eq!(pending.page, Some(Page::Latest));
        let pending_id = pending.id;
        assert_eq!(
            runtime(&app, target).scroll_memory.edge(None),
            Some(scroll::Edge::Latest)
        );
        if case == "success-child" {
            drop(app.handle_chat_ui(Action::SelectSubagent(
                target,
                Some("selected-child".into()),
            )));
            preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
            let mut child = view(&app, target);
            tree.diff(child.as_widget_mut());
            let node = child.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut movement = Reader {
                id: scroll.clone(),
                move_bottom: Some(300.0),
                top: None,
                travel: None,
                tail: false,
            };
            child
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut movement);
        }
        let child_before = if case == "success-child" {
            let mut child = view(&app, target);
            tree.diff(child.as_widget_mut());
            let node = child.as_widget_mut().layout(&mut tree, &renderer, &limits);
            Some(read(&mut child, &mut tree, &node))
        } else {
            None
        };
        let completion = app.handle_chat_ui(Action::CommandFinished(
            target,
            pending_id,
            if case == "failure-main" {
                Err("Fixture newest-history request failed".into())
            } else {
                Ok(())
            },
        ));
        if case == "success-main" {
            assert_eq!(
                runtime(&app, target).snapshot.generation,
                10,
                "ACK is not newer history arrival"
            );
            assert!(runtime(&app, target).snapshot.has_later);
            assert_eq!(runtime(&app, target).page_request, Some((Page::Latest, 10)));
            let mut placeholder = view(&app, target);
            tree.diff(placeholder.as_widget_mut());
            let node = placeholder
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let before = read(&mut placeholder, &mut tree, &node);
            assert_eq!((before.0, before.1), (0.0, 0.0));
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            executor.block_on(async {
                let mut stream = iced::runtime::task::into_stream(completion)
                    .expect("real successful PageLatest completion effects");
                let mut native_snap = false;
                while let Ok(Some(effect)) =
                    tokio::time::timeout(Duration::from_millis(250), stream.next()).await
                {
                    if let iced::runtime::Action::Widget(mut operation) = effect {
                        placeholder.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            operation.as_mut(),
                        );
                        native_snap = true;
                    }
                }
                assert!(
                    native_snap,
                    "success uses actual typed native edge operation"
                );
            });
            executor.shutdown_background();
            assert_eq!(
                read(&mut placeholder, &mut tree, &node),
                before,
                "fitting ACK operation cannot establish source arrival"
            );
        } else {
            assert!(
                native_task_emits_nothing(completion),
                "failed or child-selected ACK emits no native scroll"
            );
        }
        if case == "failure-main" {
            assert_eq!(runtime(&app, target).scroll_memory.edge(None), None);
            assert!(runtime(&app, target).page_request.is_none());
            preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
            let mut ready = view(&app, target);
            tree.diff(ready.as_widget_mut());
            let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let actual = read(&mut ready, &mut tree, &node);
            assert!(
                (actual.0 - expected).abs() <= 1.0,
                "failed Latest preserves old native reading anchor"
            );
            assert!(!actual.2);
            continue;
        }
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        {
            let mut old_ready = view(&app, target);
            tree.diff(old_ready.as_widget_mut());
            let node = old_ready
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let old = read(&mut old_ready, &mut tree, &node);
            assert!(
                old.1 > 500.0 && !old.2,
                "actual old-generation Ready is large and has no newest end marker"
            );
            if let Some(before) = child_before {
                assert_eq!(
                    old, before,
                    "old parent ACK/Ready cannot move selected child"
                );
            }
        }
        assert_eq!(
            runtime(&app, target).scroll_memory.edge(None),
            Some(scroll::Edge::Latest),
            "old source Ready cannot fulfill explicit Latest"
        );
        if case == "success-main" {
            let home = app
                .chat_scroll_key(&keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Home),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Home),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                })
                .expect("actual Home dispatch after ACK");
            assert_eq!(
                runtime(&app, target).scroll_memory.edge(None),
                Some(scroll::Edge::Start)
            );
            {
                let mut at_home = view(&app, target);
                tree.diff(at_home.as_widget_mut());
                let node = at_home
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                let executor = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                executor.block_on(async {
                    let mut stream = iced::runtime::task::into_stream(home)
                        .expect("actual Home widget operation");
                    let effect = tokio::time::timeout(Duration::from_millis(250), stream.next())
                        .await
                        .unwrap()
                        .expect("Home effect");
                    let iced::runtime::Action::Widget(mut operation) = effect else {
                        panic!("Home widget operation");
                    };
                    at_home.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        operation.as_mut(),
                    );
                });
                executor.shutdown_background();
                assert_eq!(read(&mut at_home, &mut tree, &node).0, 0.0);
            }
            let dispatched = runtime(&app, target).next_command;
            drop(app.handle_chat_ui(Action::Latest(target)));
            assert_eq!(
                runtime(&app, target).next_command,
                dispatched,
                "post-ACK repeated Latest coalesces the existing request"
            );
            assert!(runtime(&app, target).commands.is_idle());
            assert_eq!(runtime(&app, target).page_request, Some((Page::Latest, 10)));
            assert_eq!(
                runtime(&app, target).scroll_memory.edge(None),
                Some(scroll::Edge::Latest),
                "post-ACK explicit Latest supersedes actual Home even while provider request coalesces"
            );
        }

        let mut latest = (*handle.snapshot()).clone();
        latest.generation = 11;
        latest.has_later = false;
        latest.history_loading = false;
        let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut latest.items[0]).content
        else {
            panic!("text fixture");
        };
        for index in 80..120 {
            text.push_str(&format!(
                "\n\n## Newest section {index:04}\n\n{}\n\n",
                "new authoritative source ".repeat(12)
            ));
        }
        text.push_str("\n\nLATEST_END_NATIVE_ATTESTATION\n");
        handle.replace_snapshot_for_test(latest);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert_eq!(runtime(&app, target).snapshot.generation, 11);
        assert!(!runtime(&app, target).snapshot.has_later);
        assert!(runtime(&app, target).page_request.is_none());
        if case == "success-child" {
            let mut child = view(&app, target);
            tree.diff(child.as_widget_mut());
            let node = child.as_widget_mut().layout(&mut tree, &renderer, &limits);
            assert_eq!(
                read(&mut child, &mut tree, &node),
                child_before.unwrap(),
                "parent ACK/newest source cannot move selected child"
            );
            drop(child);
            drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
        }
        assert!(
            runtime(&app, target).cache["cold-rich"].placeholder,
            "newest source uses real cold preparation"
        );
        {
            let mut pending = view(&app, target);
            tree.diff(pending.as_widget_mut());
            pending
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        assert!(!runtime(&app, target).cache["cold-rich"].placeholder);
        let mut ready = view(&app, target);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let actual = read(&mut ready, &mut tree, &node);
        assert!(
            actual.1 > 500.0 && (actual.0 - actual.1).abs() <= 1.0 && actual.2,
            "actual newest-generation Ready must mount its end marker at native end, got {actual:?}, old reading top {expected}"
        );
    }
}

#[test]
fn native_deferred_latest_fetch_preserves_later_home_and_wheel_across_replacement_source() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    use iced::futures::StreamExt;
    struct Reader {
        id: widget::Id,
        bottom: Option<f32>,
        top: f32,
        travel: f32,
        bounds: iced::Rectangle,
    }
    impl Operation for Reader {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            viewport: iced::Rectangle,
            content: iced::Rectangle,
            translation: iced::Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.id) {
                self.top = translation.y;
                self.travel = (content.height - viewport.height).max(0.0);
                self.bounds = viewport;
                if let Some(bottom) = self.bottom.take() {
                    state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(bottom),
                    });
                }
            }
        }
    }
    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match &app.cards[&target.card] {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("fixture chat"),
        }
    }
    fn view(app: &Cagents, target: Target) -> Element<'_, Message> {
        runtime(app, target).view(
            None,
            false,
            true,
            &app.settings.keyboard_shortcuts,
            620.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        )
    }
    for case in [
        "home",
        "wheel",
        "child",
        "failure",
        "follow",
        "ordinary",
        "coalesced",
        "intermediate",
        "find",
    ] {
        let item = chat::Item {
            id: "old-window-only".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: (0..100)
                    .map(|index| format!("## Old {index}\n\n{}\n\n", "reader source ".repeat(12)))
                    .collect(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        };
        let (mut live, handle) = frame_cost::live_runtime(&[item]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let mut initial = (*handle.snapshot()).clone();
        initial.generation = 10;
        initial.has_later = true;
        let mut child = (*initial.items[0]).clone();
        child.id = "unrelated-child".into();
        initial.subagents.push(chat::Subagent {
            id: "child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            model: None,
            waiting: false,
            detail: String::new(),
            background: false,
            items: vec![Arc::new(child)],
        });
        handle.replace_snapshot_for_test(initial);
        app.chat_runtime_mut(target).unwrap().refresh();
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(620.0, 720.0));
        let read =
            |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node, bottom| {
                let mut reader = Reader {
                    id: card_control_id(target.card, "chat-transcript"),
                    bottom,
                    top: -1.0,
                    travel: -1.0,
                    bounds: iced::Rectangle::default(),
                };
                element
                    .as_widget_mut()
                    .operate(tree, Layout::new(node), &renderer, &mut reader);
                reader
            };
        let mut tree;
        {
            let mut element = view(&app, target);
            tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            read(&mut element, &mut tree, &node, Some(700.0));
            assert!(read(&mut element, &mut tree, &node, None).top > 500.0);
        }
        let unrelated = if matches!(case, "ordinary" | "coalesced") {
            None
        } else {
            drop(app.chat_send(
                target,
                Command::SetPermissionMode {
                    mode: "manual".into(),
                },
                false,
            ));
            Some(runtime(&app, target).commands.in_flight().unwrap().id)
        };
        drop(app.handle_chat_ui(Action::Latest(target)));
        if unrelated.is_some() {
            assert_eq!(
                runtime(&app, target).deferred_page,
                Some(HistoryNavigation::Latest)
            );
        }
        assert_eq!(
            runtime(&app, target).scroll_memory.edge(None),
            Some(scroll::Edge::Latest)
        );
        let mut find_guard = None;
        let expected;
        if case == "find" {
            // Accept a real Find query before certifying its navigation
            // guard; refresh correctly cancels navigation without a query.
            drop(find_navigation::find_for_test(
                &mut app,
                target,
                "Old 50".into(),
            ));
            let (catalog, _) = runtime(&app, target).selection_document();
            let (_, identity, _) = catalog
                .owned_search_blocks(&HashSet::from(["old-window-only".into()]))
                .into_iter()
                .find(|(_, _, text)| text.as_ref() == "Old 50")
                .expect("actual prepared Find heading");
            let intent = runtime(&app, target).find_navigation.guard();
            let epoch = runtime(&app, target).find_navigation.document_epoch();
            let mut element = view(&app, target);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let location = executor.block_on(async {
                let mut stream = iced::runtime::task::into_stream(scroll::locate_logical(
                    target,
                    "old-window-only".into(),
                    identity,
                    0,
                ))
                .unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    while let Some(effect) = stream.next().await {
                        match effect {
                            iced::runtime::Action::Widget(mut operation) => {
                                element.as_widget_mut().operate(
                                    &mut tree,
                                    Layout::new(&node),
                                    &renderer,
                                    operation.as_mut(),
                                );
                                let _ = operation.finish();
                            }
                            iced::runtime::Action::Output(location) => {
                                return location.expect("actual native Find location");
                            }
                            _ => {}
                        }
                    }
                    panic!("actual locator completion");
                })
                .await
                .unwrap()
            });
            executor.block_on(async {
                let task =
                    scroll::apply_find_location(target, location, intent.clone(), Some(epoch));
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                let effect = tokio::time::timeout(Duration::from_millis(250), stream.next())
                    .await
                    .unwrap()
                    .unwrap();
                let iced::runtime::Action::Widget(mut operation) = effect else {
                    panic!("actual Find widget operation")
                };
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
                let _ = operation.finish();
            });
            executor.shutdown_background();
            let after = read(&mut element, &mut tree, &node, None);
            assert!(
                after.top > 500.0 && after.top < after.travel,
                "actual accepted Find owns an interior reader"
            );
            assert!(intent.document_current(epoch));
            find_guard = Some((intent, epoch));
            expected = Some(after.top);
        } else if case == "follow" {
            expected = None;
        } else if case == "wheel" {
            let mut element = view(&app, target);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let before = read(&mut element, &mut tree, &node, None);
            let mut messages = vec![];
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: 3.0 },
                }),
                Layout::new(&node),
                iced::mouse::Cursor::Available(before.bounds.center()),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &before.bounds,
            );
            let after = read(&mut element, &mut tree, &node, None);
            assert!(
                after.top > 0.0 && after.top < before.top,
                "actual wheel moved native reader"
            );
            expected = Some(after.top);
        } else {
            for repetition in 0..if case == "coalesced" { 2 } else { 1 } {
                if repetition == 1 {
                    let count = runtime(&app, target).next_command;
                    drop(app.handle_chat_ui(Action::Latest(target)));
                    assert_eq!(
                        runtime(&app, target).next_command,
                        count,
                        "explicit Latest coalesces accepted fetch"
                    );
                    assert_eq!(
                        runtime(&app, target).scroll_memory.edge(None),
                        Some(scroll::Edge::Latest)
                    );
                }
                let home = app
                    .chat_scroll_key(&keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Home),
                        modified_key: keyboard::Key::Named(keyboard::key::Named::Home),
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    })
                    .expect("actual later Home dispatch");
                let mut element = view(&app, target);
                tree.diff(element.as_widget_mut());
                let node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                let executor = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                executor.block_on(async {
                    let mut stream = iced::runtime::task::into_stream(home).unwrap();
                    let effect = tokio::time::timeout(Duration::from_millis(250), stream.next())
                        .await
                        .unwrap()
                        .unwrap();
                    let iced::runtime::Action::Widget(mut operation) = effect else {
                        panic!("Home widget operation")
                    };
                    element.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        operation.as_mut(),
                    );
                });
                executor.shutdown_background();
                assert_eq!(read(&mut element, &mut tree, &node, None).top, 0.0);
            }
            expected = Some(0.0);
        }
        if let Some(unrelated) = unrelated {
            assert_eq!(
                runtime(&app, target).deferred_page,
                Some(HistoryNavigation::Latest),
                "reader intent cannot cancel the fetch"
            );
            drop(app.handle_chat_ui(Action::CommandFinished(target, unrelated, Ok(()))));
        }
        if let Some((intent, epoch)) = &find_guard {
            assert!(
                intent.document_current(*epoch),
                "resuming the deferred FETCH cannot cancel current Find intent/document"
            );
        }
        let fetch = runtime(&app, target)
            .commands
            .in_flight()
            .expect("deferred FETCH resumed despite later reader intent");
        assert_eq!(fetch.page, Some(Page::Latest));
        let fetch_id = fetch.id;
        assert_eq!(runtime(&app, target).page_request, Some((Page::Latest, 10)));
        if expected.is_some() {
            assert_ne!(
                runtime(&app, target).scroll_memory.edge(None),
                Some(scroll::Edge::Latest)
            );
        }
        let mut child_before = None;
        if case == "child" {
            drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
            preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
            let mut element = view(&app, target);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            read(&mut element, &mut tree, &node, Some(300.0));
            child_before = Some(read(&mut element, &mut tree, &node, None).top);
        }
        drop(app.handle_chat_ui(Action::CommandFinished(
            target,
            fetch_id.wrapping_add(1),
            Ok(()),
        )));
        assert_eq!(
            runtime(&app, target).commands.in_flight().unwrap().id,
            fetch_id,
            "stale ACK cannot consume the actual fetch"
        );
        let ack = app.handle_chat_ui(Action::CommandFinished(
            target,
            fetch_id,
            if case == "failure" {
                Err("fixture latest failed".into())
            } else {
                Ok(())
            },
        ));
        if let Some((intent, epoch)) = &find_guard {
            assert!(
                intent.document_current(*epoch),
                "PageLatest ACK cannot invalidate current accepted Find"
            );
        }
        if expected.is_some() {
            assert!(
                native_task_emits_nothing(ack),
                "resumed fetch cannot issue a new reader snap"
            );
        }
        if case == "failure" {
            let mut element = view(&app, target);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            assert_eq!(
                read(&mut element, &mut tree, &node, None).top,
                expected.unwrap()
            );
            continue;
        }
        if case == "intermediate" {
            let mut intermediate = (*handle.snapshot()).clone();
            intermediate.generation = 11;
            intermediate.has_later = true;
            intermediate.history_loading = false;
            Arc::make_mut(&mut intermediate.items[0]).id = "intermediate-no-old-ids".into();
            handle.replace_snapshot_for_test(intermediate);
            app.chat_runtime_mut(target).unwrap().refresh();
            assert!(
                runtime(&app, target).page_request.is_none(),
                "existing Runtime clears page metadata on intermediate source"
            );
            {
                let mut pending = view(&app, target);
                tree.diff(pending.as_widget_mut());
                pending
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
            }
            preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
            let mut ready = view(&app, target);
            tree.diff(ready.as_widget_mut());
            let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
            assert_eq!(
                read(&mut ready, &mut tree, &node, None).top,
                expected.unwrap(),
                "intermediate replacement also preserves Home"
            );
        }
        let mut latest = (*handle.snapshot()).clone();
        latest.generation = if case == "intermediate" { 12 } else { 11 };
        latest.has_later = false;
        latest.history_loading = false;
        let replacement = Arc::make_mut(&mut latest.items[0]);
        replacement.id = "replacement-with-no-old-ids".into();
        let ItemContent::Text { text, .. } = &mut replacement.content else {
            panic!("text fixture")
        };
        *text = (0..120)
            .map(|index| format!("## New {index}\n\n{}\n\n", "replacement source ".repeat(12)))
            .collect();
        handle.replace_snapshot_for_test(latest);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(runtime(&app, target).page_request.is_none());
        if let Some(before) = child_before {
            let mut element = view(&app, target);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            assert_eq!(
                read(&mut element, &mut tree, &node, None).top,
                before,
                "main replacement token cannot move selected child"
            );
            drop(element);
            drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
        }
        assert!(runtime(&app, target).cache["replacement-with-no-old-ids"].placeholder);
        {
            let mut pending = view(&app, target);
            tree.diff(pending.as_widget_mut());
            pending
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        let mut ready = view(&app, target);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let actual = read(&mut ready, &mut tree, &node, None);
        assert!(actual.travel > 500.0);
        if let Some(expected) = expected {
            assert!(
                (actual.top - expected).abs() <= 1.0,
                "{case}: replacement top {} must preserve later reader {expected}",
                actual.top
            );
        } else {
            assert!(
                (actual.top - actual.travel).abs() <= 1.0,
                "unsuperseded Latest still follows newest source"
            );
        }
    }
}

#[test]
fn native_app_dispatches_home_end_and_latest_while_cold_placeholder_fits() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    use iced::futures::StreamExt;
    for command in ["Home", "End", "Latest"] {
        struct Reader {
            id: widget::Id,
            move_bottom: Option<f32>,
            top: Option<f32>,
            travel: Option<f32>,
        }
        impl Operation for Reader {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                viewport: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                state: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.id) {
                    self.top = Some(translation.y);
                    self.travel = Some((content.height - viewport.height).max(0.0));
                    if let Some(bottom) = self.move_bottom.take() {
                        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(bottom),
                        });
                    }
                }
            }
        }
        fn runtime(app: &Cagents, target: Target) -> &Runtime {
            match &app.cards[&target.card] {
                CardRuntime::Chat(runtime) => runtime,
                _ => panic!("fixture chat"),
            }
        }
        fn view(app: &Cagents, target: Target) -> Element<'_, Message> {
            runtime(app, target).view(
                None,
                false,
                true,
                &app.settings.keyboard_shortcuts,
                620.0,
                0.0,
                Place { branch: None },
                true,
                None,
                vec![],
                true,
            )
        }
        fn dispatch(app: &mut Cagents, target: Target, command: &str) -> Task<Message> {
            if command == "Latest" {
                app.handle_chat_ui(Action::Latest(target))
            } else {
                let key = if command == "Home" {
                    keyboard::key::Named::Home
                } else {
                    keyboard::key::Named::End
                };
                app.chat_scroll_key(&keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key),
                    modified_key: keyboard::Key::Named(key),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                })
                .expect("actual chat keyboard dispatch")
            }
        }
        let source = (0..80)
            .map(|index| {
                format!(
                    "## Section {index:04}\n\nReading paragraph {index:04}: {}\n\n",
                    "ordinary text ".repeat(12)
                )
            })
            .collect::<String>();
        let item = chat::Item {
            id: "cold-rich".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: source,
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        };
        let (mut live, handle) = frame_cost::live_runtime(&[item]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let mut seeded = (*handle.snapshot()).clone();
        let mut child_item = (*seeded.items[0]).clone();
        child_item.id = "child-rich".into();
        seeded.subagents.push(chat::Subagent {
            id: "selected-child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            model: None,
            waiting: false,
            detail: String::new(),
            background: false,
            items: vec![Arc::new(child_item)],
        });
        handle.replace_snapshot_for_test(seeded);
        app.chat_runtime_mut(target).unwrap().refresh();
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(620.0, 720.0));
        let scroll = card_control_id(target.card, "chat-transcript");
        let read = |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node| {
            let mut reader = Reader {
                id: scroll.clone(),
                move_bottom: None,
                top: None,
                travel: None,
            };
            element
                .as_widget_mut()
                .operate(tree, Layout::new(node), &renderer, &mut reader);
            (
                reader.top.expect("actual native transcript"),
                reader.travel.unwrap(),
            )
        };
        let expected;
        {
            let mut element = view(&app, target);
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut movement = Reader {
                id: scroll.clone(),
                move_bottom: Some(700.0),
                top: None,
                travel: None,
            };
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut movement,
            );
            expected = read(&mut element, &mut tree, &node).0;
            assert!(expected > 500.0);
            // No subsequent layout: hide immediately after actual navigation.
        }
        let snapshot = (*runtime(&app, target).snapshot).clone();
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(false)
            .unwrap();
        app.chat_runtime_mut(target).unwrap().cool();
        preparation::retirement_barrier();
        assert!(runtime(&app, target).cache.is_empty());
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(true)
            .unwrap();
        let mut tree;
        {
            let mut empty_wait = view(&app, target);
            tree = Tree::new(&empty_wait);
            empty_wait
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        handle.replace_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(runtime(&app, target).cache["cold-rich"].placeholder);
        assert!(runtime(&app, target).preparation.contains("cold-rich"));
        let stale = Target {
            generation: target.generation + 1,
            ..target
        };
        assert!(
            native_task_emits_nothing(app.handle_chat_ui(Action::Latest(stale))),
            "stale generation cannot emit a card-ID-only scroll operation"
        );
        assert_eq!(runtime(&app, target).scroll_memory.edge(None), None);
        let task = dispatch(&mut app, target, command);
        {
            let mut placeholder = view(&app, target);
            tree.diff(placeholder.as_widget_mut());
            let node = placeholder
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let before = read(&mut placeholder, &mut tree, &node);
            assert_eq!(before, (0.0, 0.0), "actual cold App placeholder fits");
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            executor.block_on(async {
                let mut stream = iced::runtime::task::into_stream(task)
                    .expect("actual keyboard/Latest native operation");
                let action = tokio::time::timeout(Duration::from_millis(250), stream.next())
                    .await
                    .unwrap()
                    .expect("native operation effect");
                let iced::runtime::Action::Widget(mut operation) = action else {
                    panic!("expected actual widget operation");
                };
                placeholder.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
            });
            executor.shutdown_background();
            assert_eq!(
                read(&mut placeholder, &mut tree, &node),
                before,
                "actual fitting snap has unchanged native translation"
            );
        }
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        assert!(!runtime(&app, target).cache["cold-rich"].placeholder);
        let mut ready = view(&app, target);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let actual = read(&mut ready, &mut tree, &node);
        assert!(
            actual.1 > 500.0,
            "actual authoritative Ready remains the large transcript"
        );
        assert!(
            (actual.0 - if command == "Home" { 0.0 } else { actual.1 }).abs() <= 1.0,
            "actual App/native Tree {command} produced {actual:?} instead of the explicit edge (old reading top {expected})"
        );
        drop(ready);
        let held = dispatch(&mut app, target, command);
        drop(app.handle_chat_ui(Action::SelectSubagent(
            target,
            Some("selected-child".into()),
        )));
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        {
            let mut child = view(&app, target);
            tree.diff(child.as_widget_mut());
            let node = child.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut movement = Reader {
                id: scroll.clone(),
                move_bottom: Some(300.0),
                top: None,
                travel: None,
            };
            child
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut movement);
            let before = read(&mut child, &mut tree, &node);
            assert!(before.0 > 500.0 && before.0 < before.1);
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            executor.block_on(async {
                let mut stream =
                    iced::runtime::task::into_stream(held).expect("held actual App edge dispatch");
                let action = tokio::time::timeout(Duration::from_millis(250), stream.next())
                    .await
                    .unwrap()
                    .expect("held widget effect");
                let iced::runtime::Action::Widget(mut operation) = action else {
                    panic!("widget operation");
                };
                child.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
            });
            executor.shutdown_background();
            assert_eq!(
                read(&mut child, &mut tree, &node),
                before,
                "held parent Home/End/Latest cannot affect selected child"
            );
        }
        assert_eq!(
            runtime(&app, target)
                .scroll_memory
                .edge(Some("selected-child")),
            None
        );
        assert_eq!(
            runtime(&app, target).scroll_memory.edge(None),
            Some(if command == "Home" {
                scroll::Edge::Start
            } else if command == "Latest" {
                scroll::Edge::Latest
            } else {
                scroll::Edge::End
            })
        );
        drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        let mut parent = view(&app, target);
        tree.diff(parent.as_widget_mut());
        let node = parent.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let actual = read(&mut parent, &mut tree, &node);
        assert!(actual.1 > 500.0);
        assert!(
            (actual.0 - if command == "Home" { 0.0 } else { actual.1 }).abs() <= 1.0,
            "original selected context retains its own explicit edge"
        );
    }
}

#[test]
fn native_child_picker_selects_both_transcripts_through_real_narrow_chooser_rebuild_with_approval()
{
    use iced::advanced::clipboard;
    use iced::runtime::user_interface::{Cache, UserInterface};
    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match app.cards.get(&target.card) {
            Some(CardRuntime::Chat(runtime)) => runtime,
            _ => panic!("the fixture chat remains mounted"),
        }
    }
    fn view(app: &Cagents, target: Target) -> Element<'_, Message> {
        runtime(app, target).view(
            None,
            false,
            true,
            &app.settings.keyboard_shortcuts,
            360.0,
            0.0,
            Place { branch: None },
            true,
            None,
            Vec::new(),
            true,
        )
    }
    let thought = chat::Item {
        id: "live-thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "Inspecting compact tools and native agent tabs.".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        at: None,
        started_at: None,
    };
    let (mut live, _handle) = frame_cost::live_runtime(&[thought]);
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let mut target = live.target;
    target.card = app.workspace.focused().unwrap();
    live.target = target;
    live.session.launch.provider = Provider::Codex;
    live.expanded.edit().insert("live-thought".into());
    let mut child = native_child_fixture();
    child.waiting = true;
    child.model = Some("gpt-fixture-mini".into());
    child.detail = "Waiting for your approval".into();
    let snapshot = Arc::make_mut(&mut live.snapshot);
    snapshot.busy = true;
    snapshot.subagents.push(child);
    snapshot.requests.push(chat::Request {
        kind: crate::chat::RequestKind::Approval,
        id: "child-approval".into(),
        title: "Child agent · Command approval".into(),
        description: "printf native-fixture".into(),
        fields: Vec::new(),
        actions: vec![
            chat::Choice {
                id: "decline".into(),
                label: "Deny".into(),
                description: String::new(),
            },
            chat::Choice {
                id: "accept".into(),
                label: "Allow once".into(),
                description: String::new(),
            },
        ],
    });
    app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(live)));
    let mut renderer = crate::app::tests::headless_renderer();
    let size = iced::Size::new(360.0, 720.0);
    let mut cache = Cache::new();
    for step in 0..3 {
        let mut ui = UserInterface::build(view(&app, target), size, cache, &mut renderer);
        let accessible = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let bounds = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label().is_some_and(|label| match step {
                        0 => label.starts_with("Search and filter subagents:"),
                        1 => {
                            label.contains("Inspect repository")
                                && label.ends_with(" · native-child")
                        }
                        // The open subagent's transcript does not show the
                        // main conversation's request, so its tab does.
                        _ => label == "Main conversation: Waiting for you",
                    })
            })
            .and_then(|node| node.node().bounds())
            .unwrap_or_else(|| {
                panic!(
                    "chooser step {step}: {:?}",
                    accessible
                        .root()
                        .iter()
                        .chain(accessible.children())
                        .filter_map(|node| node.node().label())
                        .collect::<Vec<_>>()
                )
            });
        if step == 1 {
            assert!(
                bounds.x0 >= 0.0 && bounds.x1 <= f64::from(size.width),
                "actual chooser bounds {bounds:?} must fit the narrow pane"
            );
        }
        let pointer = iced::Point::new(
            (bounds.x0 + bounds.x1) as f32 / 2.0,
            (bounds.y0 + bounds.y1) as f32 / 2.0,
        );
        let mut messages = Vec::new();
        ui.update(
            &[
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: pointer }),
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            ],
            iced::mouse::Cursor::Available(pointer),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::SelectSubagent(..))))
        );
        cache = ui.into_cache();
        // This is the actual UserInterface cache extraction/rebuild path
        // used after the press requests a native redraw.
        let mut ui = UserInterface::build(view(&app, target), size, cache, &mut renderer);
        ui.update(
            &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            ))],
            iced::mouse::Cursor::Available(pointer),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        cache = ui.into_cache();
        let selection = messages
            .into_iter()
            .find_map(|message| match message {
                Message::ChatUi(
                    action @ (Action::SelectSubagent(..) | Action::SubagentPicker(..)),
                ) => Some(action),
                _ => None,
            })
            .expect("native selector and chooser publish actions through the full chat hierarchy");
        drop(app.handle_chat_ui(selection));
        if step == 0 {
            assert!(runtime(&app, target).child_picker.is_some());
            assert!(runtime(&app, target).selected_subagent.is_none());
        } else {
            assert_eq!(
                runtime(&app, target).selected_subagent.as_deref(),
                if step == 1 {
                    Some("native-child")
                } else {
                    None
                }
            );
            assert!(runtime(&app, target).child_picker.is_none());
        }
    }
}

#[test]
fn live_reasoning_disclosure_toggles_through_actions_and_retains_streaming_choice() {
    use iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct ReadText(Vec<(String, iced::Rectangle)>);
    impl Operation for ReadText {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push((prose.plain.to_string(), bounds));
            }
        }
    }
    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match app.cards.get(&target.card) {
            Some(CardRuntime::Chat(runtime)) => runtime,
            _ => panic!("the fixture chat remains mounted"),
        }
    }
    let item = chat::Item {
        id: "live-thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "Secret reasoning body".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        at: None,
        started_at: None,
    };
    let (mut live, handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    Arc::make_mut(&mut live.snapshot).busy = true;
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let mut target = live.target;
    target.card = app.workspace.focused().unwrap();
    live.target = target;
    app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(live)));
    let renderer = crate::app::tests::headless_renderer();
    let view = |app: &Cagents| {
        let runtime = runtime(app, target);
        let item = &runtime.snapshot.items[0];
        runtime.text_view(
            0,
            item,
            &runtime.cache[&item.id],
            turn::Slot::Thinking,
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
        )
    };
    let mut tree = Tree::new(view(&app).as_widget());
    for (step_index, expanded) in [false, true, true, false].into_iter().enumerate() {
        assert_eq!(
            runtime(&app, target).expanded.contains("live-thought"),
            expanded,
            "the actual Toggle action controls the disclosure state"
        );
        let mut element = view(&app);
        tree.diff(element.as_widget_mut());
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 800.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        let mut rendered = ReadText::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut rendered);
        let values: Vec<_> = rendered
            .0
            .into_iter()
            .filter(|(_, bounds)| {
                bounds.width > 0.0
                    && bounds.height > 0.0
                    && bounds.y < viewport.height
                    && bounds.y + bounds.height > 0.0
            })
            .map(|(text, _)| text)
            .collect();
        assert_eq!(
            values
                .iter()
                .any(|value| value.contains("Secret reasoning body")),
            expanded,
            "step={step_index} expanded={expanded} markdown_items={} rendered={values:?}",
            runtime(&app, target).cache["live-thought"]
                .markdown
                .items()
                .len()
        );
        if expanded && !values.iter().any(|value| value.contains("streamed detail")) {
            // Publish a live delta while open, then keep the same widget tree.
            let runtime = app.chat_runtime_mut(target).unwrap();
            let mut snapshot = runtime.snapshot.as_ref().clone();
            let mut item = snapshot.items[0].as_ref().clone();
            if let ItemContent::Text { text, .. } = &mut item.content {
                text.push_str(" streamed detail");
            }
            snapshot.items[0] = Arc::new(item);
            snapshot.generation += 1;
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            preparation::drain_fixture(runtime);
            assert!(runtime.expanded.contains("live-thought"));
            continue;
        }
        if step_index == 3 {
            break;
        }
        let pointer = iced::Point::new(36.0, 16.0);
        let mut messages = Vec::new();
        for event in [
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        let toggle = messages
            .into_iter()
            .find_map(|message| match message {
                Message::ChatUi(Action::Toggle(actual, id)) if actual == target => Some(id),
                _ => None,
            })
            .expect("the native reasoning button emits a disclosure action");
        drop(app.handle_chat_ui(Action::Toggle(target, toggle)));
        assert_eq!(
            runtime(&app, target).expanded.contains("live-thought"),
            !expanded,
            "published Toggle must update the mounted runtime"
        );
    }
}

#[test]
fn reasoning_disclosure_preserves_keyboard_focus_across_retained_toggles() {
    use iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree, operation},
    };
    #[derive(Default)]
    struct FocusDisclosure(bool);
    impl Operation for FocusDisclosure {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if !self.0 {
                state.focus();
                self.0 = true;
            } else {
                state.unfocus();
            }
        }
    }
    #[derive(Default)]
    struct Probe {
        focused: Option<bool>,
        prose: Vec<String>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.focused.get_or_insert_with(|| state.is_focused());
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if bounds.width > 0.0
                && bounds.height > 0.0
                && let Some(prose) = state.downcast_ref::<selectable_prose::State>()
            {
                self.prose.push(prose.plain.to_string());
            }
        }
    }
    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match app.cards.get(&target.card) {
            Some(CardRuntime::Chat(runtime)) => runtime,
            _ => panic!("the fixture chat remains mounted"),
        }
    }
    let item = chat::Item {
        id: "keyboard-thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "Keyboard reasoning body".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        at: None,
        started_at: None,
    };
    let (mut live, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    Arc::make_mut(&mut live.snapshot).busy = true;
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let mut target = live.target;
    target.card = app.workspace.focused().unwrap();
    live.target = target;
    app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(live)));
    let renderer = crate::app::tests::headless_renderer();
    let view = |app: &Cagents| {
        let runtime = runtime(app, target);
        let item = &runtime.snapshot.items[0];
        runtime.text_view(
            0,
            item,
            &runtime.cache[&item.id],
            turn::Slot::Thinking,
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
        )
    };
    let mut tree = Tree::new(view(&app).as_widget());
    let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 800.0));
    for (index, expanded) in [false, true, false].into_iter().enumerate() {
        let mut element = view(&app);
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        if index == 0 {
            // Native keyboard focus is assigned once. The second Enter
            // must work through the same retained tree without refocusing.
            let mut focus = FocusDisclosure::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
            assert!(focus.0, "the mounted disclosure is focusable");
        }
        let mut probe = Probe::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        assert_eq!(
            probe.focused,
            Some(true),
            "disclosure focus survives diff at stage {index}"
        );
        assert_eq!(
            probe
                .prose
                .iter()
                .any(|text| text.contains("Keyboard reasoning body")),
            expanded
        );
        assert_eq!(
            runtime(&app, target).expanded.contains("keyboard-thought"),
            expanded
        );
        if index == 2 {
            break;
        }
        let enter = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
            modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
            physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &enter,
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        let mut toggles = messages.into_iter().filter_map(|message| match message {
            Message::ChatUi(Action::Toggle(actual, id)) if actual == target => Some(id),
            _ => None,
        });
        let id = toggles
            .next()
            .expect("Enter toggles the focused disclosure");
        assert_eq!(id, "keyboard-thought");
        assert!(toggles.next().is_none(), "one key emits one toggle");
        drop(app.handle_chat_ui(Action::Toggle(target, id)));
        assert_eq!(
            runtime(&app, target).expanded.contains("keyboard-thought"),
            !expanded
        );
    }
}

/// A tool call's title is read from its input once, when the call
/// changes. Drawing its row, open or closed, and regrouping the
/// transcript read the kept title, so a large refused write costs
/// nothing per frame and still names its file.
#[test]
fn drawing_and_regrouping_a_tool_row_do_not_read_its_input_again() {
    use iced::advanced::{Layout, layout, widget::Tree};
    use iced_accessibility::accesskit::Role;
    let item = chat::Item {
        id: "write".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Write".into(),
            input: serde_json::json!({
                "content": "x".repeat(2 * 1024 * 1024),
                "file_path": "docs/large-report.md",
            })
            .to_string(),
            output: "The user doesn't want to proceed with this tool use.".into(),
            status: chat::ToolStatus::Denied,
        },
        started_at: None,
        at: None,
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    assert_eq!(
        runtime.turns[0].summary,
        "Tried to write docs/large-report.md · denied"
    );
    let reads = || crate::app::chat_ui::tool_title::READS.with(std::cell::Cell::get);
    let before = reads();
    let name = "Tried to write docs/large-report.md · Denied";
    for open in [false, true, false] {
        if open {
            runtime.expanded.edit().insert(item.id.clone());
        } else {
            runtime.expanded.edit().remove(&item.id);
        }
        let mut element = runtime.step_header(&item, &runtime.cache[&item.id], open);
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(520.0, 1000.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node.node().role() == Role::Button
                    && node.node().label() == Some(name)),
            "the row names the file it tried to write ({name})"
        );
    }
    let turns = turn::group(
        &runtime.snapshot.items,
        &runtime.asked_after,
        crate::app::chat_ui::Presentations(&runtime.cache),
    );
    assert_eq!(turns[0].summary, runtime.turns[0].summary);
    // Opening the row forms its presentation again, from the same call.
    let shown = runtime.snapshot.items[0].clone();
    let previous = runtime.cache.remove(&item.id);
    let reopened = CachedItem::updated(
        &shown.content,
        runtime.syntax_theme,
        previous,
        true,
        false,
        None,
    );
    assert_eq!(
        reopened
            .title_of(&shown)
            .map(|title| title.line())
            .as_deref(),
        Some("Tried to write docs/large-report.md")
    );
    assert_eq!(
        reads(),
        before,
        "the input was read again after it was titled"
    );
}

/// A group of one long shell script keeps its strip on one line in a
/// narrow pane, and its duration stays in view rather than being pushed
/// past the edge by the wrapped command.
#[test]
fn a_one_command_strip_stays_on_one_line_with_its_duration_in_view() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let script = format!(
        "cd ~/.local/share/cagents/accounts/claude/{}/projects/; f=transcript.jsonl; \
             echo \"### USER PROMPTS\"; jq -r 'select(.type==\"user\") | .message' $f | head",
        "0123456789abcdef".repeat(4)
    );
    let item = chat::Item {
        id: "script".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Command {
            label: String::new(),
            command: script,
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(1_200),
            output: String::new(),
        },
        started_at: Some(1_000),
        at: Some(2_200),
    };
    let (runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    let turn = &runtime.turns[0];
    assert!(turn.summary.starts_with("Ran cd ~/.local/share/cagents"));
    let renderer = crate::app::tests::headless_renderer();
    let one_line = {
        let mut short = turn.clone();
        short.summary = "Ran ls".into();
        let mut element = runtime.step_strip(&short, Vec::new);
        let mut tree = Tree::new(element.as_widget());
        element
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 1000.0)),
            )
            .size()
            .height
    };
    for width in [200.0, 320.0, 960.0] {
        let mut element = runtime.step_strip(turn, Vec::new);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 1000.0)),
        );
        assert_eq!(node.size().height, one_line, "one row at width {width}");
        assert_eq!(node.size().width, width, "the whole row is clickable");
        let line = Layout::new(&node)
            .children()
            .next()
            .and_then(|button| button.children().next())
            .expect("the strip's header row");
        let cells: Vec<_> = line.children().map(|cell| cell.bounds()).collect();
        assert_eq!(cells.len(), 4, "kind, preview, elapsed time, chevron");
        let [icon, summary, duration, chevron] = cells.as_slice() else {
            unreachable!();
        };
        assert!(icon.x + icon.width < summary.x);
        assert!(summary.width > 0.0, "the preview keeps visible space");
        assert!(summary.x + summary.width < duration.x);
        assert!(duration.width > 0.0);
        assert!(duration.x + duration.width < chevron.x);
        assert!(chevron.x + chevron.width <= width);
        assert!(
            (width - chevron.x - chevron.width - step::GAP_S).abs() < 1.0,
            "the chevron stays at the trailing control inset"
        );
    }
}

#[test]
fn rendered_exact_tool_targets_keep_verb_prefixed_names() {
    use iced::advanced::{Layout, layout, widget::Tree};
    use iced_accessibility::accesskit::Role;
    for (operation, target, name) in [
        (
            chat::CommandAction::Read,
            "Read λ notes.md",
            "Read Read λ notes.md",
        ),
        (
            chat::CommandAction::Search,
            "Search TODO",
            "Searched Search TODO",
        ),
        (chat::CommandAction::List, "List *.rs", "Listed List *.rs"),
    ] {
        let item = chat::Item {
            id: "exact".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Command {
                label: target.into(),
                command: target.into(),
                working_directory: "/project".into(),
                action: operation,
                status: chat::ToolStatus::Succeeded,
                exit_code: None,
                duration_ms: None,
                output: "result".into(),
            },
            started_at: None,
            at: None,
        };
        let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
        assert_eq!(runtime.cache[&item.id].preview.text, target);
        assert_eq!(runtime.turns[0].summary, name);
        for open in [false, true] {
            if open {
                runtime.expanded.edit().insert(item.id.clone());
            } else {
                runtime.expanded.edit().remove(&item.id);
            }
            let mut element = runtime.step_view(
                &item,
                &runtime.cache[&item.id],
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
            );
            let renderer = crate::app::tests::headless_renderer();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(520.0, 1000.0)),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let nodes: Vec<_> = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .collect();
            assert!(
                nodes
                    .iter()
                    .any(|node| node.node().role() == Role::Button
                        && node.node().label() == Some(name)),
                "row action must name its exact target"
            );
            // RichText has no separate accessible leaf in this pinned
            // widget graph; the mounted row's exact name carries its
            // target, while the preview cache preserves the drawn bytes.
            let row = nodes
                .iter()
                .find(|node| node.node().label() == Some(name))
                .unwrap()
                .id()
                .clone();
            let copy = nodes
                .iter()
                .find(|node| node.node().label() == Some("Copy this message"))
                .map(|node| node.id().clone());
            assert_eq!(
                copy.is_some(),
                open,
                "copy remains available in the expanded detail"
            );
            use iced_accessibility::{
                A11yId,
                accesskit::{Action as A11yAction, ActionRequest, TreeId},
            };
            let event = |id: A11yId, action| {
                let A11yId::Widget(widget) = id.clone() else {
                    panic!("widget identity required")
                };
                iced::Event::A11y(
                    widget,
                    ActionRequest {
                        action,
                        target_tree: TreeId::ROOT,
                        target_node: id.into(),
                        data: None,
                    },
                )
            };
            let mut dispatch = |event: iced::Event| {
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    iced::mouse::Cursor::Unavailable,
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut iced::advanced::Shell::new(&mut messages),
                    &iced::Rectangle::with_size(iced::Size::new(520.0, 1000.0)),
                );
                messages
            };
            let unknown = A11yId::from(iced::advanced::widget::Id::unique());
            assert!(dispatch(event(unknown.clone(), A11yAction::Click)).is_empty());
            assert!(dispatch(event(unknown, A11yAction::Focus)).is_empty());
            let mut mismatched = event(row.clone(), A11yAction::Click);
            if let iced::Event::A11y(_, request) = &mut mismatched {
                request.target_node = copy
                    .clone()
                    .unwrap_or_else(|| A11yId::from(iced::advanced::widget::Id::unique()))
                    .into();
            }
            assert!(dispatch(mismatched).is_empty());
            let mut wrong_tree = event(row.clone(), A11yAction::Click);
            if let iced::Event::A11y(_, request) = &mut wrong_tree {
                request.target_tree = TreeId(uuid::Uuid::new_v4());
            }
            assert!(dispatch(wrong_tree).is_empty());
            let toggled = dispatch(event(row.clone(), A11yAction::Click));
            assert_eq!(toggled.len(), 1);
            assert!(
                matches!(&toggled[0], Message::ChatUi(Action::Toggle(actual, id))
                if *actual == runtime.target && id == &item.id)
            );
            let Some(copy) = copy else {
                continue;
            };
            let copied = dispatch(event(copy.clone(), A11yAction::Click));
            assert_eq!(copied.len(), 1);
            assert!(
                matches!(&copied[0], Message::ChatUi(Action::Copy(actual, id, CopyPart::Text) | Action::CopyMessage(actual, id, _))
                if *actual == runtime.target && id == &item.id)
            );
            assert!(dispatch(event(copy.clone(), A11yAction::Focus)).is_empty());
            // Keep the actual widget state mounted through diff. AT focus
            // makes the hover control keyboard-visible without a pointer.
            tree.diff(element.as_widget_mut());
            let enter = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
                location: iced::keyboard::Location::Standard,
                modifiers: iced::keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut dispatch = |event: iced::Event| {
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    iced::mouse::Cursor::Unavailable,
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut iced::advanced::Shell::new(&mut messages),
                    &iced::Rectangle::with_size(iced::Size::new(520.0, 1000.0)),
                );
                messages
            };
            assert!(dispatch(event(row, A11yAction::Blur)).is_empty());
            let copied = dispatch(enter.clone());
            assert_eq!(copied.len(), 1);
            assert!(
                matches!(&copied[0], Message::ChatUi(Action::Copy(actual, id, CopyPart::Text) | Action::CopyMessage(actual, id, _))
                if *actual == runtime.target && id == &item.id)
            );
            assert!(dispatch(event(copy, A11yAction::Blur)).is_empty());
            assert!(
                dispatch(enter).is_empty(),
                "cleared hidden Copy focus must not consume Enter"
            );
        }
    }
}

#[test]
fn a_stop_marker_is_one_quiet_row_with_nothing_to_open() {
    use iced::advanced::{Layout, layout, widget::Tree};
    use iced_accessibility::accesskit::Role;
    let item = chat::Item {
        at: Some(10),
        ..chat::stopped_notice("turn:stopped".into())
    };
    let (runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    let mut element = runtime.step_view(
        &item,
        &runtime.cache[&item.id],
        design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE),
    );
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(520.0, 1000.0)),
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    let labels: Vec<_> = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| node.node().label().map(|label| (node.node().role(), label)))
        .collect();
    assert!(
        labels.iter().any(|(_, label)| *label == chat::STOPPED),
        "the marker says the turn stopped: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .all(|(role, label)| *role != Role::Button && !label.contains("0s")),
        "a stop has nothing to disclose and no time of its own: {labels:?}"
    );
}

#[test]
fn transcript_viewport_reaches_pane_edges_with_inset_inside_content() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    struct Probe {
        id: widget::Id,
        find: widget::Id,
        viewport: Option<iced::Rectangle>,
        field: Option<iced::Rectangle>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.find) {
                self.field = Some(bounds);
            }
        }
        fn text_input(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn operation::TextInput,
        ) {
            if id == Some(&self.find) {
                self.field = Some(bounds);
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: iced::Rectangle,
            _: iced::Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.id) {
                self.viewport = Some(bounds);
            }
        }
    }
    let items = frame_cost::transcript(1);
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    // Controls around the transcript keep the reading column.
    runtime.find = Some("answer".into());
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let renderer = crate::app::tests::headless_renderer();
    // Narrower and wider than the reading column, as a split or a
    // maximized pane.
    for width in [600.0_f32, 1440.0] {
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            width.min(super::super::layout::READING_COLUMN),
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 700.0)),
        );
        let mut probe = Probe {
            id: card_control_id(runtime.target.card, "chat-transcript"),
            find: card_control_id(runtime.target.card, "chat-find"),
            viewport: None,
            field: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        let bounds = probe.viewport.expect("transcript viewport");
        assert_eq!(bounds.x, 0.0, "width {width}");
        assert_eq!(bounds.width, width, "width {width}");
        let column = width.min(super::super::layout::READING_COLUMN);
        let field = probe.field.expect("find field");
        assert_eq!(
            field.x,
            (width - column) / 2.0 + step::GAP_M,
            "width {width}"
        );
        assert!(
            field.x + field.width <= (width + column) / 2.0 - step::GAP_M,
            "width {width}"
        );
    }
}

#[test]
fn footer_picker_rows_center_single_lines_and_subtitled_blocks() {
    use iced::advanced::{layout, widget::Tree};

    fn leaves(node: &layout::Node, origin: iced::Point, out: &mut Vec<iced::Rectangle>) {
        let bounds = node.bounds() + iced::Vector::new(origin.x, origin.y);
        if node.children().is_empty() {
            out.push(bounds);
        }
        for child in node.children() {
            leaves(child, bounds.position(), out);
        }
    }

    let renderer = crate::app::tests::headless_renderer();
    for (detail, height, text_lines) in [
        (None, design::DENSE_ROW_HEIGHT, 1),
        (Some("model-id"), design::list::TWO_LINE_HEIGHT, 2),
    ] {
        let mut row = picker_choice_row(
            "High".into(),
            detail,
            "High".into(),
            widget::Id::unique(),
            true,
            Message::RunCommand(crate::workspace_widget::GlobalShortcut::ChatModel),
        );
        let mut tree = Tree::new(row.as_widget());
        let node = row.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(280.0, 200.0)),
        );
        assert_eq!(node.size().height, height);
        let mut parts = Vec::new();
        leaves(&node, iced::Point::ORIGIN, &mut parts);
        let icon = parts
            .iter()
            .find(|part| {
                part.width == f32::from(design::COMPACT_ICON_SIZE)
                    && part.height == f32::from(design::COMPACT_ICON_SIZE)
            })
            .expect("selected check mark");
        assert!((icon.center_y() - height / 2.0).abs() <= 0.5);
        let text: Vec<_> = parts
            .iter()
            .filter(|part| part.x >= icon.x + icon.width)
            .collect();
        assert_eq!(text.len(), text_lines);
        assert_eq!(text[0].x - (icon.x + icon.width), step::GAP_S);
        let top = text.iter().map(|part| part.y).fold(f32::INFINITY, f32::min);
        let bottom = text
            .iter()
            .map(|part| part.y + part.height)
            .fold(0.0_f32, f32::max);
        assert!(((top + bottom) / 2.0 - icon.center_y()).abs() <= 0.5);
    }
}

#[test]
fn footer_model_picker_bounds_reasoning_with_and_without_fast_mode() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation::Focusable},
    };
    struct Choices {
        ids: Vec<widget::Id>,
        bounds: Vec<(widget::Id, iced::Rectangle)>,
    }
    impl Operation for Choices {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn Focusable,
        ) {
            if let Some(id) = id.filter(|id| self.ids.contains(id)) {
                self.bounds.push((id.clone(), bounds));
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    for fast in [false, true] {
        for width in [900.0, 1500.0] {
            let (mut runtime, _handle) = furnished_runtime();
            runtime.model_picker = Some(String::new());
            let snapshot = Arc::make_mut(&mut runtime.snapshot);
            snapshot.fast_mode = fast.then_some(chat::FastMode {
                state: chat::FastModeState::Off,
                disabled_reason: Some("preference".into()),
            });
            snapshot.models.push(model("Plain model", "Plain model"));
            let efforts: Vec<_> = std::iter::once(card_control_id(
                runtime.target.card,
                "chat-reasoning-choice-default",
            ))
            .chain((0..3).map(|index| {
                card_control_id(
                    runtime.target.card,
                    &format!("chat-reasoning-choice-{index}"),
                )
            }))
            .collect();
            let selected = card_control_id(
                runtime.target.card,
                &format!(
                    "chat-model-choice-Claude-named-{}",
                    runtime.snapshot.selected_model.as_deref().unwrap()
                ),
            );
            let plain = card_control_id(
                runtime.target.card,
                "chat-model-choice-Claude-named-Plain model",
            );
            let mut menu = runtime.model_search(Vec::new()).expect("open model picker");
            let mut tree = Tree::new(menu.as_widget());
            let node = menu.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 900.0)),
            );
            assert_eq!(node.size().width, design::MODEL_POPUP_WIDTH);
            let mut choices = Choices {
                ids: efforts
                    .iter()
                    .cloned()
                    .chain([selected.clone(), plain.clone()])
                    .collect(),
                bounds: Vec::new(),
            };
            menu.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut choices);
            assert_eq!(
                choices.bounds.len(),
                6,
                "all advertised choices remain mounted"
            );
            let bounds = |id: &widget::Id| {
                choices
                    .bounds
                    .iter()
                    .find(|(found, _)| found == id)
                    .unwrap()
                    .1
            };
            let model = bounds(&selected);
            assert_eq!(model.height, design::list::TWO_LINE_HEIGHT);
            assert_eq!(
                bounds(&plain).height,
                design::DENSE_ROW_HEIGHT,
                "models without subtitles are single-line rows"
            );
            for id in &efforts {
                let effort = bounds(id);
                assert_eq!(effort.height, design::DENSE_ROW_HEIGHT);
                assert_eq!(
                    (effort.x, effort.width),
                    (model.x, model.width),
                    "check columns and trailing highlight edges align"
                );
                assert!(effort.x >= 0.0 && effort.x + effort.width <= design::MODEL_POPUP_WIDTH);
            }
            for pair in efforts.windows(2) {
                let previous = bounds(&pair[0]);
                let next = bounds(&pair[1]);
                assert_eq!(
                    previous.y + previous.height,
                    next.y,
                    "effort rows have the model list's gapless spacing"
                );
            }
        }
    }
}

#[test]
fn narrow_footer_model_picker_contains_model_and_reasoning_choices() {
    use iced::advanced::{
        clipboard,
        widget::{Operation, operation::Focusable},
    };
    use iced::runtime::{UserInterface, user_interface::Cache};

    fn click(
        app: &Cagents,
        target: Target,
        cache: Cache,
        renderer: &mut cosmic::Renderer,
        role: &str,
    ) -> (Cache, Vec<Message>) {
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&target.card) else {
            panic!("chat");
        };
        let view = runtime.view(
            None,
            false,
            true,
            &app.settings.keyboard_shortcuts,
            450.0,
            0.0,
            Place { branch: None },
            true,
            None,
            Vec::new(),
            true,
        );
        let mut ui = UserInterface::build(view, iced::Size::new(450.0, 700.0), cache, renderer);
        struct Bounds {
            id: widget::Id,
            rect: Option<iced::Rectangle>,
            focused: bool,
        }
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn Focusable,
            ) {
                if id == Some(&self.id) {
                    self.rect = Some(bounds);
                    self.focused = state.is_focused();
                }
            }
        }
        let mut bounds = Bounds {
            id: card_control_id(target.card, role),
            rect: None,
            focused: false,
        };
        // Unlike a11y_nodes in pinned COSMIC, operate also traverses the
        // actual nested overlay layouts and their live focusable controls.
        ui.operate(renderer, &mut bounds);
        let rect = bounds
            .rect
            .unwrap_or_else(|| panic!("missing mounted control {role}"));
        let pointer = iced::Point::new(rect.center_x(), rect.center_y());
        let keyboard = role.starts_with("chat-reasoning-choice-");
        let events = if keyboard {
            ui.operate(
                renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                    target.card,
                    "chat-model-search",
                )),
            );
            // Traverse from the search, as Tab does in the native menu.
            // A direct focus of the effort would miss unreachable rows.
            for _ in 0..16 {
                let mut next: Box<dyn Operation> =
                    Box::new(iced::advanced::widget::operation::focusable::focus_next::<()>());
                loop {
                    ui.operate(renderer, next.as_mut());
                    match next.finish() {
                        iced::advanced::widget::operation::Outcome::Chain(chained) => {
                            next = chained
                        }
                        _ => break,
                    }
                }
                ui.operate(renderer, &mut bounds);
                if bounds.focused {
                    break;
                }
            }
            assert!(
                bounds.focused,
                "reasoning must be reachable from the model search with Tab"
            );
            vec![iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
                location: iced::keyboard::Location::Standard,
                modifiers: iced::keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            })]
        } else {
            vec![
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: pointer }),
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
            ]
        };
        let mut messages = Vec::new();
        ui.update(
            &events,
            if keyboard {
                iced::mouse::Cursor::Unavailable
            } else {
                iced::mouse::Cursor::Available(pointer)
            },
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        (ui.into_cache(), messages)
    }
    fn apply(app: &mut Cagents, messages: Vec<Message>) {
        for message in messages {
            drop(app.update(message));
        }
    }

    let (mut live, _handle) = frame_cost::live_runtime(&frame_cost::transcript(1));
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let mut target = live.target;
    target.card = app.workspace.focused().unwrap();
    live.target = target;
    let snapshot = Arc::make_mut(&mut live.snapshot);
    snapshot.capabilities.model_selection = true;
    snapshot.capabilities.reasoning_selection = true;
    snapshot.models = vec![
        chat::Model {
            id: "model-a".into(),
            label: "Fixture model A".into(),
            reasoning: vec!["low".into(), "high".into()],
        },
        model("model-b", "Fixture model B"),
    ];
    snapshot.selected_model = Some("model-a".into());
    snapshot.reasoning = Some("low".into());
    app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(live)));
    let mut renderer = crate::app::tests::headless_renderer();
    let mut cache = Cache::new();
    // Both choices are reached through the model picker in the footer.
    for reasoning in [false, true] {
        let (next, messages) = click(&app, target, cache, &mut renderer, "chat-model-picker");
        cache = next;
        apply(&mut app, messages);
        let label = if !reasoning {
            "chat-model-choice-Claude-named-model-b"
        } else {
            "chat-reasoning-choice-1"
        };
        let (next, messages) = click(&app, target, cache, &mut renderer, label);
        cache = next;
        assert!(
            messages.iter().any(|message| match message {
                Message::ChatUi(Action::Model(actual, id)) => *actual == target && id == "model-b",
                Message::ChatUi(Action::Reasoning(actual, id, effort)) =>
                    *actual == target && id == "model-a" && effort.as_deref() == Some("high"),
                _ => false,
            }),
            "picker must publish its selection"
        );
        drop(app.update(action(Action::ModelPicker(target, false))));
    }
    let (next, messages) = click(&app, target, cache, &mut renderer, "chat-model-picker");
    cache = next;
    apply(&mut app, messages);
    let (next, messages) = click(
        &app,
        target,
        cache,
        &mut renderer,
        "chat-reasoning-choice-default",
    );
    cache = next;
    assert!(messages.iter().any(|message| matches!(message,
        Message::ChatUi(Action::Reasoning(actual, id, None)) if *actual == target && id == "model-a")));
    drop(app.update(action(Action::ModelPicker(target, false))));
    // The keyboard command opens the model popup over its visible anchor.
    drop(app.open_chat_control(crate::workspace_widget::GlobalShortcut::ChatModel));
    let (_, messages) = click(
        &app,
        target,
        cache,
        &mut renderer,
        "chat-model-choice-Claude-named-model-b",
    );
    assert!(messages.iter().any(|message| matches!(message,
        Message::ChatUi(Action::Model(actual, id)) if *actual == target && id == "model-b")));
}

/// Where the controls named `roles`, the footer, and the footer's text
/// land when `runtime` is drawn `width` wide.
struct Laid {
    footer: iced::Rectangle,
    controls: HashMap<String, iced::Rectangle>,
    footer_text: Vec<String>,
    /// Every text the view draws.
    text: Vec<String>,
    /// What assistive technology is told inside the footer.
    footer_names: Vec<String>,
    /// The far edge of whatever the footer draws, text or named glyph.
    footer_drawn_right: f32,
}

fn lay_out(runtime: &Runtime, width: f32, accounts: Option<ChatAccounts>, roles: &[&str]) -> Laid {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation::Focusable},
    };
    struct Probe {
        footer_id: widget::Id,
        footer: Option<iced::Rectangle>,
        ids: Vec<(String, widget::Id)>,
        controls: HashMap<String, iced::Rectangle>,
        text: Vec<(iced::Rectangle, String)>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&self.footer_id) {
                self.footer = Some(bounds);
            }
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn Focusable,
        ) {
            if let Some((role, _)) = self.ids.iter().find(|(_, known)| Some(known) == id) {
                self.controls.insert(role.clone(), bounds);
            }
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
            self.text.push((bounds, text.to_owned()));
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let mut element = runtime.view(
        None,
        false,
        true,
        &shortcuts,
        width,
        0.0,
        Place {
            branch: Some("feature/a-long-branch-name".into()),
        },
        true,
        accounts,
        vec![],
        true,
    );
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 700.0)),
    );
    let mut probe = Probe {
        footer_id: card_control_id(runtime.target.card, "chat-footer"),
        footer: None,
        ids: roles
            .iter()
            .map(|role| {
                (
                    (*role).to_owned(),
                    card_control_id(runtime.target.card, role),
                )
            })
            .collect(),
        controls: HashMap::new(),
        text: Vec::new(),
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
    let footer = probe.footer.expect("footer mounted");
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    let named: Vec<(iced::Rectangle, String)> = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| {
            let node = node.node();
            let bounds = node.bounds()?;
            Some((
                iced::Rectangle::new(
                    iced::Point::new(bounds.x0 as f32, bounds.y0 as f32),
                    iced::Size::new(bounds.width() as f32, bounds.height() as f32),
                ),
                node.label()?.to_owned(),
            ))
        })
        .filter(|(bounds, _)| footer.contains(bounds.center()))
        .collect();
    let all_text = probe.text.iter().map(|(_, text)| text.clone()).collect();
    let text: Vec<(iced::Rectangle, String)> = probe
        .text
        .into_iter()
        .filter(|(bounds, _)| footer.contains(bounds.center()))
        .collect();
    Laid {
        footer,
        text: all_text,
        controls: probe.controls,
        footer_drawn_right: text
            .iter()
            .chain(&named)
            .map(|(bounds, _)| bounds.x + bounds.width)
            .fold(f32::MIN, f32::max),
        footer_text: text.into_iter().map(|(_, text)| text).collect(),
        footer_names: named.into_iter().map(|(_, name)| name).collect(),
    }
}

/// A settled conversation with every footer control a provider offers.
fn furnished_runtime() -> (Runtime, Arc<ClientHandle>) {
    let (mut runtime, handle) = frame_cost::live_runtime(&frame_cost::transcript(1));
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.capabilities.model_selection = true;
    snapshot.capabilities.reasoning_selection = true;
    snapshot.capabilities.steer = true;
    snapshot.capabilities.permission_modes =
        vec!["default".into(), "acceptEdits".into(), "plan".into()];
    snapshot.permission_mode = Some("acceptEdits".into());
    snapshot.models = vec![chat::Model {
        id: "claude-sonnet-4-5".into(),
        label: "Claude Sonnet 4.5".into(),
        reasoning: vec!["low".into(), "medium".into(), "high".into()],
    }];
    snapshot.selected_model = Some("claude-sonnet-4-5".into());
    snapshot.reasoning = Some("medium".into());
    snapshot.active_model = Some(chat::ActiveModel {
        aliases: Vec::new(),
        id: "claude-sonnet-4-5".into(),
        label: "Claude Sonnet 4.5".into(),
        reasoning: Some("medium".into()),
        default_reasoning: Some("medium".into()),
    });
    snapshot.usage.context_limit = 200_000;
    snapshot.usage.context_tokens = 84_000;
    snapshot.usage.cost_microusd = Some(1_234_567);
    snapshot.finished_at = Some(42);
    (runtime, handle)
}

fn fixture_accounts() -> ChatAccounts<'static> {
    ChatAccounts {
        current: "Synthetic account with a long name",
        choices: Default::default(),
    }
}

#[test]
fn composer_offers_send_only_for_a_draft_and_keeps_stop_last() {
    let send = Trailing::Submit {
        mode: PromptMode::Send,
        slot: 0,
    };
    assert_eq!(composer_trailing(&[PromptMode::Send], false, false), []);
    assert_eq!(composer_trailing(&[PromptMode::Send], false, true), [send]);
    let working = [PromptMode::Steer, PromptMode::Queue];
    assert_eq!(composer_trailing(&working, true, false), [Trailing::Stop]);
    assert_eq!(
        composer_trailing(&working, true, true),
        [
            Trailing::Submit {
                mode: PromptMode::Steer,
                slot: 0
            },
            Trailing::Submit {
                mode: PromptMode::Queue,
                slot: 1
            },
            Trailing::Stop,
        ]
    );
}

/// A cancel that also stops background subagents waits for a second
/// press. Stop and the draft keep their place while it waits, so the
/// second press lands where the first did, and the line above the draft
/// says what that press stops.
#[test]
fn a_stop_waiting_for_confirmation_keeps_its_place_and_says_what_it_stops() {
    let roles = ["chat-stop", "chat-composer"];
    let (mut runtime, _handle) = furnished_runtime();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = true;
    snapshot.capabilities.interrupt_stops_background = true;
    for id in ["scout", "planner"] {
        snapshot.subagents.push(chat::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: id.into(),
            kind: String::new(),
            description: String::new(),
            status: chat::ToolStatus::Running,
            background: true,
            items: Vec::new(),
        });
    }
    runtime.composer = text_editor::Content::with_text("Run the tests again");
    for width in [470.0_f32, 900.0] {
        let waiting = |runtime: &mut Runtime, armed: bool| {
            runtime.interrupt_armed = armed.then(Instant::now);
            lay_out(runtime, width, None, &roles)
        };
        let first = waiting(&mut runtime, false);
        let second = waiting(&mut runtime, true);
        assert_eq!(
            second.controls["chat-stop"], first.controls["chat-stop"],
            "Stop keeps its place at {width}"
        );
        assert_eq!(
            second.controls["chat-composer"].width, first.controls["chat-composer"].width,
            "the draft keeps its width at {width}"
        );
        let says = |laid: &Laid| {
            laid.text.iter().any(|text| {
                text == "Press Stop or Esc again to cancel the turn and stop 2 background subagents"
            })
        };
        assert!(!says(&first));
        assert!(says(&second), "{:?}", second.text);
    }
}

/// While a message is only on its way, there is no turn in view: the
/// composer invites a message rather than steering. Stop is offered while
/// the conversation reports working, whether or not its turn has begun.
#[test]
fn a_message_on_its_way_offers_no_turn_to_steer_and_stop_only_while_working() {
    let roles = ["chat-submit-0", "chat-stop", "chat-composer"];
    let (mut runtime, _handle) = furnished_runtime();
    let message = Command::Prompt {
        text: "Look at the logs".into(),
        attachments: Vec::new(),
        mode: PromptMode::Send,
    };
    let mut pending = commands::PendingCommand::new(1, &message);
    pending.outgoing = Some(commands::Outgoing::new(
        "Look at the logs".into(),
        Vec::new(),
        PromptMode::Send,
        &runtime.snapshot,
    ));
    let snapshot = runtime.snapshot.clone();
    assert!(runtime.commands.push(pending, message, &snapshot).is_some());
    assert!(runtime.turn_under_way());
    runtime.composer = text_editor::Content::with_text("Then fix them");
    let laid = lay_out(&runtime, 900.0, None, &roles);
    assert!(laid.controls.contains_key("chat-submit-0"));
    assert!(!laid.controls.contains_key("chat-stop"));
    assert_eq!(
        composer_placeholder(Some(PromptMode::Steer), false),
        "Message"
    );
    assert_eq!(
        composer_placeholder(Some(PromptMode::Steer), true),
        "Steer this turn"
    );
    assert_eq!(
        composer_placeholder(Some(PromptMode::Queue), false),
        "Queue for the next turn"
    );
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    let laid = lay_out(&runtime, 900.0, None, &roles);
    assert!(laid.controls.contains_key("chat-stop"));
}

#[test]
fn send_appears_with_a_draft_left_of_a_stop_that_stays_put() {
    let roles = [
        "chat-submit-0",
        "chat-submit-1",
        "chat-stop",
        "chat-composer",
    ];
    for width in [470.0_f32, 900.0] {
        for busy in [false, true] {
            let (mut runtime, _handle) = furnished_runtime();
            Arc::make_mut(&mut runtime.snapshot).busy = busy;
            let empty = lay_out(&runtime, width, None, &roles);
            assert!(
                !empty.controls.contains_key("chat-submit-0"),
                "an empty draft has no Send at {width} (busy {busy})"
            );
            assert_eq!(empty.controls.contains_key("chat-stop"), busy);
            runtime.composer = text_editor::Content::with_text("Run the tests again");
            let typed = lay_out(&runtime, width, None, &roles);
            let send = typed.controls["chat-submit-0"];
            let editor = typed.controls["chat-composer"];
            assert!(send.x >= editor.x + editor.width, "Send follows the draft");
            if busy {
                let stop = typed.controls["chat-stop"];
                let queue = typed.controls["chat-submit-1"];
                assert!(send.x < queue.x && queue.x < stop.x, "Send leads the group");
                assert_eq!(
                    stop, empty.controls["chat-stop"],
                    "Stop keeps its place as a draft appears at {width}"
                );
            } else {
                assert!(!typed.controls.contains_key("chat-stop"));
            }
        }
    }
    // Attachments are something to send without any text.
    let (mut runtime, _handle) = furnished_runtime();
    runtime
        .attachments
        .push(chat::Attachment::file("/project/screenshot.png"));
    assert!(
        lay_out(&runtime, 470.0, None, &roles)
            .controls
            .contains_key("chat-submit-0")
    );
}

#[test]
fn narrow_footer_keeps_its_controls_on_one_line_without_an_overflow() {
    let roles = ["chat-model-picker", "chat-permission-picker"];
    for width in [280.0_f32, 320.0, 439.0, 440.0, 470.0, 599.0, 600.0, 900.0] {
        let (runtime, _handle) = furnished_runtime();
        let laid = lay_out(&runtime, width, Some(fixture_accounts()), &roles);
        let footer = laid.footer;
        let fit = FooterFit::for_width(width);
        assert!(
            laid.controls["chat-model-picker"].x < laid.controls["chat-permission-picker"].x,
            "model precedes permissions at {width}"
        );
        for role in roles {
            let control = laid
                .controls
                .get(role)
                .unwrap_or_else(|| panic!("{role} is in the footer at {width}"));
            assert!(
                control.x >= footer.x && control.x + control.width <= footer.x + footer.width,
                "{role} stays inside the footer at {width}: {control:?} in {footer:?}"
            );
        }
        let text = laid.footer_text.join(" | ");
        if fit != FooterFit::Wide {
            // The wide footer wraps a long place; a narrow one has no
            // place to wrap and keeps one line.
            assert!(
                footer.height <= design::PRIMARY_CONTROL_SIZE + step::GAP_M + 1.0,
                "footer wrapped at {width}: {footer:?}"
            );
            // The permission mode is cut, never squeezed out.
            assert!(
                laid.controls["chat-permission-picker"].width >= 64.0,
                "permission picker kept at {width}: {:?}",
                laid.controls["chat-permission-picker"]
            );
        }
        assert!(text.contains("Auto-accept edits"), "{text}");
        assert_eq!(text.contains("Synthetic account"), fit == FooterFit::Wide);
        let roomy = fit != FooterFit::Tight;
        assert_eq!(text.contains("Claude Sonnet 4.5"), roomy, "{text}");
        assert_eq!(text.contains("42%"), roomy, "{text}");
        assert_eq!(text.contains("$1.23"), roomy, "{text}");
        // The trailing group holds the footer's far edge at every width,
        // so model and spend never jump sides as the pane narrows.
        let inner_right = footer.x + footer.width - (step::GAP_M + COMPOSER_PADDING);
        assert!(
            (inner_right - laid.footer_drawn_right).abs() <= 1.0,
            "trailing group ends at the footer's edge at {width}: drawn to {} in {footer:?}",
            laid.footer_drawn_right
        );
        // What a narrow pane hides from sight stays in what a screen
        // reader is told.
        let names = laid.footer_names.join(" | ");
        assert_eq!(
            names.contains("Account: Synthetic account with a long name"),
            fit != FooterFit::Wide,
            "{names}"
        );
        let ring = laid
            .footer_names
            .iter()
            .find(|name| name.starts_with("42% of context"))
            .unwrap_or_else(|| panic!("the ring is named at {width}: {names}"));
        // Where the cost is no longer drawn, the ring says it.
        assert_eq!(ring.contains("$1.23"), !roomy, "{ring}");
        assert!(names.contains("$1.23"), "{names}");
        // The conversation's state closes the transcript, never the footer.
        for status in ["Finished", "Ready", "Working", "Responding"] {
            assert!(
                !text.contains(status),
                "{status} in footer at {width}: {text}"
            );
        }
    }
}

#[test]
fn composer_stays_inset_with_and_without_a_conversation_title() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    struct Probe {
        id: widget::Id,
        bounds: Option<iced::Rectangle>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&self.id) {
                self.bounds = Some(bounds);
            }
        }
    }
    let items = frame_cost::transcript(1);
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    for title in [None, Some("Conversation".to_owned())] {
        for width in [280.0, 600.0] {
            let (mut runtime, _handle) = frame_cost::live_runtime(&items);
            Arc::make_mut(&mut runtime.snapshot).title = title.clone();
            let mut element = runtime.view(
                None,
                false,
                true,
                &shortcuts,
                width,
                0.0,
                Place { branch: None },
                true,
                None,
                vec![],
                true,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 700.0)),
            );
            let mut probe = Probe {
                id: card_control_id(runtime.target.card, "chat-composer-surface"),
                bounds: None,
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            let bounds = probe.bounds.expect("composer surface");
            assert_eq!(bounds.x, step::GAP_M);
            assert_eq!(bounds.width, width - 2.0 * step::GAP_M);
        }
    }
}

fn inspector_chat_fixture(width: f32) -> (Cagents, CardId) {
    let mut items = frame_cost::transcript(1);
    items.truncate(1);
    if let ItemContent::Text { text, .. } = &mut items[0].content {
        *text = format!("{}restart safety.", "Review the retry change. ".repeat(12));
    }
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    runtime.target.card = card;
    runtime.composer = text_editor::Content::with_text("retained draft");
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(runtime.session.clone());
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
    drop(app.workspace.resize_viewport(width));
    app.sync_pane_inspectors();
    (app, card)
}

#[test]
fn inspector_toggle_clearance_keeps_transcript_rows_out_of_its_footprint() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation::Focusable},
    };
    struct Bounds {
        toggle: widget::Id,
        row: Option<iced::Rectangle>,
        composer_id: widget::Id,
        composer: Option<iced::Rectangle>,
        button: Option<iced::Rectangle>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&scroll::item_id("0-prompt")) {
                self.row = Some(bounds);
            }
            if id == Some(&self.composer_id) {
                self.composer = Some(bounds);
            }
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn Focusable,
        ) {
            if id == Some(&self.toggle) {
                self.button = Some(bounds);
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    for width in [420.0, 720.0, 1440.0] {
        let (app, card) = inspector_chat_fixture(width);
        let mut element = app.card_view(card, true, false);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 800.0)),
        );
        let mut bounds = Bounds {
            toggle: card_control_id(card, "inspector.toggle"),
            composer_id: card_control_id(card, "chat-composer-surface"),
            row: None,
            composer: None,
            button: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
        let row = bounds.row.expect("rendered first user row");
        let button = bounds.button.expect("floating toggle");
        assert!(
            row.x + row.width + step::GAP_S <= button.x + 0.1,
            "{width}: {row:?} / {button:?}"
        );
        let composer = bounds.composer.expect("composer remains mounted");
        let reading = width.min(super::super::layout::READING_COLUMN);
        assert_eq!(composer.x, (width - reading) / 2.0 + step::GAP_M);
        assert_eq!(composer.width, reading - 2.0 * step::GAP_M);
    }
}

#[test]
fn inspector_open_header_reserves_a_standard_gap_before_its_toggle() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation::Focusable},
    };
    #[derive(Default)]
    struct HeaderBounds {
        panel: Option<iced::Rectangle>,
        toggle: Option<iced::Rectangle>,
        controls: Vec<iced::Rectangle>,
        panel_id: Option<widget::Id>,
        toggle_id: Option<widget::Id>,
    }
    impl Operation for HeaderBounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == self.panel_id.as_ref() {
                self.panel = Some(bounds);
            }
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn Focusable,
        ) {
            if id == self.toggle_id.as_ref() {
                self.toggle = Some(bounds);
            } else {
                self.controls.push(bounds);
            }
        }
    }
    let (mut app, card) = inspector_chat_fixture(1440.0);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    // Without the activity block the toggle ends the first section's header.
    app.settings.inspector_show_activity = false;
    app.open_pane_inspector_now(card);
    let renderer = crate::app::tests::headless_renderer();
    let mut element = app.card_view(card, true, false);
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1440.0, 800.0)),
    );
    let mut bounds = HeaderBounds {
        panel_id: Some(card_control_id(card, "inspector.panel")),
        toggle_id: Some(card_control_id(card, "inspector.toggle")),
        ..Default::default()
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
    let panel = bounds.panel.expect("mounted inspector");
    let toggle = bounds.toggle.expect("header toggle");
    let adjacent = bounds
        .controls
        .iter()
        .filter(|control| {
            control.x >= panel.x
                && control.x < toggle.x
                && control.y < toggle.y + toggle.height
                && control.y + control.height > toggle.y
        })
        .max_by(|a, b| (a.x + a.width).total_cmp(&(b.x + b.width)))
        .expect("an actual adjacent header control, not just its short label");
    assert!(
        toggle.x - adjacent.x - adjacent.width >= step::GAP_S - 0.1,
        "control {adjacent:?} stops a standard gap before toggle {toggle:?}"
    );
}

#[test]
fn inspector_saved_admission_actions_clear_the_floating_toggle() {
    use iced::runtime::user_interface::{Cache, UserInterface};
    for width in [300.0, 420.0, 720.0] {
        let (mut app, card) = inspector_chat_fixture(width);
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("fixture chat")
        };
        runtime.connection = Connection::AwaitingAdmission;
        let mut renderer = crate::app::tests::headless_renderer();
        let ui = UserInterface::build(
            app.card_view(card, true, false),
            iced::Size::new(width, 800.0),
            Cache::new(),
            &mut renderer,
        );
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let all = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .collect::<Vec<_>>();
        let copy = all
            .iter()
            .find(|node| node.node().label() == Some("Copy saved draft"))
            .and_then(|node| node.node().bounds())
            .expect("copy saved draft control");
        let toggle = all
            .iter()
            .find(|node| {
                node.node()
                    .label()
                    .is_some_and(|label| label.starts_with("Toggle inspector"))
            })
            .and_then(|node| node.node().bounds())
            .expect("floating toggle");
        assert!(copy.x1 + f64::from(step::GAP_S) <= toggle.x0 + 0.1);
    }
}

#[test]
fn inspector_overlay_fills_its_pane_and_escape_restores_the_retained_toggle_focus() {
    use iced::advanced::widget::{Operation, operation};
    use iced::futures::StreamExt;
    use iced::runtime::user_interface::{Cache, UserInterface};
    struct Panel {
        id: widget::Id,
        bounds: Option<iced::Rectangle>,
    }
    impl Operation for Panel {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&self.id) {
                self.bounds = Some(bounds);
            }
        }
    }
    fn frame(
        app: &Cagents,
        card: CardId,
        width: f32,
        cache: Cache,
        task: Task<Message>,
        renderer: &mut cosmic::Renderer,
        event: Option<keyboard::Event>,
    ) -> (
        Cache,
        Option<widget::Id>,
        Option<iced::Rectangle>,
        Vec<Message>,
    ) {
        let mut ui = UserInterface::build(
            widget::row::with_children(
                app.workspace
                    .order()
                    .map(|id| app.card_view(id, true, false)),
            )
            .width(Length::Fill)
            .height(Length::Fill),
            iced::Size::new(width, 800.0),
            cache,
            renderer,
        );
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            while let Some(action) = executor.block_on(stream.next()) {
                if let iced::runtime::Action::Widget(mut op) = action {
                    loop {
                        ui.operate(renderer, &mut op);
                        match op.finish() {
                            operation::Outcome::Chain(next) => op = next,
                            _ => break,
                        }
                    }
                }
            }
        }
        let mut messages = Vec::new();
        if let Some(event) = event {
            let _ = ui.update(
                &[iced::Event::Keyboard(event)],
                iced::mouse::Cursor::Unavailable,
                renderer,
                &mut iced::advanced::clipboard::Null,
                &mut messages,
            );
        }
        let focused = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = focused.clone();
        let mut find = operation::map(operation::focusable::find_focused(), move |id| {
            *sink.lock().unwrap() = Some(id)
        });
        ui.operate(renderer, &mut find);
        let _ = find.finish();
        let mut panel = Panel {
            id: card_control_id(card, "inspector.panel"),
            bounds: None,
        };
        ui.operate(renderer, &mut panel);
        let id = focused.lock().unwrap().clone();
        (ui.into_cache(), id, panel.bounds, messages)
    }
    let mut renderer = crate::app::tests::headless_renderer();
    for width in [420.0, 600.0] {
        let (mut app, card) = inspector_chat_fixture(width);
        let (cache, _, _, _) = frame(
            &app,
            card,
            width,
            Cache::new(),
            Task::none(),
            &mut renderer,
            None,
        );
        let opened = app.set_pane_inspector_open(card, true);
        let finished =
            app.advance_pane_inspector_transitions(Instant::now() + DIALOG_TRANSITION_DURATION);
        let (cache, focused, bounds, _) = frame(
            &app,
            card,
            width,
            cache,
            Task::batch([opened, finished]),
            &mut renderer,
            None,
        );
        assert_eq!(focused, Some(card_control_id(card, "inspector.toggle")));
        let bounds = bounds.expect("actual overlay panel");
        assert_eq!(
            (bounds.x, bounds.width),
            (0.0, width),
            "overlay fills the pane"
        );
        let closed = app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: false,
        });
        assert!(!app.pane_inspectors.is_open(card));
        assert!(
            app.pane_content_inputs_blocked(card),
            "closing slide still covers content"
        );
        let (cache, _, _, _) = frame(&app, card, width, cache, closed, &mut renderer, None);
        let finished =
            app.advance_pane_inspector_transitions(Instant::now() + DIALOG_TRANSITION_DURATION);
        let (cache, focused, panel, _) =
            frame(&app, card, width, cache, finished, &mut renderer, None);
        assert!(panel.is_none());
        assert_eq!(
            focused,
            Some(card_control_id(card, "inspector.toggle")),
            "Escape returns focus after the slide"
        );
        let (_, focused, _, _) = frame(&app, card, width, cache, Task::none(), &mut renderer, None);
        assert_eq!(
            focused,
            Some(card_control_id(card, "inspector.toggle")),
            "focus survives the next retained frame"
        );
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("retained chat")
        };
        assert_eq!(runtime.composer.text(), "retained draft");
    }
    // Native focus can enter the left inspector by Tab while workspace
    // focus still names the right pane. Escape must follow native focus.
    for move_focus_during_close in [false, true] {
        let (mut app, left) = inspector_chat_fixture(840.0);
        let right = super::super::tests::draft_chat_for_provider(&mut app, Provider::Claude);
        for card in Arc::make_mut(&mut app.durable).cards.iter_mut() {
            card.width_fraction = 0.5;
        }
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.workspace.resize_viewport(840.0));
        assert_eq!(app.workspace.focused(), Some(right));
        let opened = app.set_pane_inspector_open(left, true);
        let completed =
            app.advance_pane_inspector_transitions(Instant::now() + DIALOG_TRANSITION_DURATION);
        let (cache, focused, _, _) = frame(
            &app,
            left,
            840.0,
            Cache::new(),
            Task::batch([opened, completed]),
            &mut renderer,
            None,
        );
        assert_eq!(focused, Some(card_control_id(left, "inspector.toggle")));
        let escape = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let (cache, _, _, messages) = frame(
            &app,
            left,
            840.0,
            cache,
            Task::none(),
            &mut renderer,
            Some(escape),
        );
        let close = messages
            .into_iter()
            .find(|message| matches!(message, Message::ClosePaneInspector(card) if *card == left))
            .expect("actual focused inspector consumes Escape and names its own pane");
        let closed = app.update(close);
        assert!(!app.pane_inspectors.is_open(left));
        assert_eq!(app.workspace.focused(), Some(left));
        let (cache, _, _, _) = frame(&app, left, 840.0, cache, closed, &mut renderer, None);
        let expected = if move_focus_during_close {
            card_control_id(right, "chat-composer")
        } else {
            card_control_id(left, "inspector.toggle")
        };
        let newer_focus = if move_focus_during_close {
            iced::widget::operation::focus(expected.clone()).map(cosmic::Action::App)
        } else {
            Task::none()
        };
        let (cache, _, _, _) = frame(&app, left, 840.0, cache, newer_focus, &mut renderer, None);
        let finished =
            app.advance_pane_inspector_transitions(Instant::now() + DIALOG_TRANSITION_DURATION);
        let (_, focused, _, _) = frame(&app, left, 840.0, cache, finished, &mut renderer, None);
        assert_eq!(
            focused,
            Some(expected),
            "completion preserves a newer native focus choice"
        );
        drop(app.update(Message::ClosePaneInspector(left)));
        assert!(
            !app.pane_inspectors.is_open(left),
            "repeated Escape cannot reopen it"
        );
    }
}

#[test]
fn reading_column_is_stable_across_transcript_states() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    struct Probe {
        composer: widget::Id,
        transcript: widget::Id,
        editor: Option<iced::Rectangle>,
        viewport: Option<iced::Rectangle>,
        answer: Option<iced::Rectangle>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&self.composer) {
                self.editor = Some(bounds);
            }
            if id == Some(&scroll::item_id("0-answer")) {
                self.answer = Some(bounds);
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: iced::Rectangle,
            _: iced::Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.transcript) {
                self.viewport = Some(bounds);
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    for width in [280.0_f32, 600.0, 956.0, 1440.0] {
        let mut baseline = None;
        for state in 0..7 {
            let mut items = if state == 0 {
                vec![]
            } else {
                frame_cost::transcript(1)
            };
            if (state == 5 || state == 6)
                && let ItemContent::Text { text, .. } = &mut items.last_mut().unwrap().content
            {
                *text = if state == 5 {
                    "w".repeat(3000)
                } else {
                    format!("```rust\n{}\n```", "long_code_token".repeat(1000))
                };
            }
            let (mut runtime, _handle) = frame_cost::live_runtime(&items);
            if state == 2 {
                Arc::make_mut(&mut runtime.snapshot)
                    .requests
                    .push(chat::Request {
                        kind: chat::RequestKind::Approval,
                        id: "approval".into(),
                        title: "Command approval".into(),
                        description: "Run cargo test".into(),
                        fields: vec![],
                        actions: vec![],
                    });
            }
            if state == 3 {
                runtime
                    .opened
                    .edit()
                    .insert(runtime.turns[0].key.clone(), true);
            }
            if state == 4 {
                Arc::make_mut(&mut runtime.snapshot).ready = false;
            }
            let mut element = runtime.view(
                None,
                false,
                true,
                &shortcuts,
                width.min(super::super::layout::READING_COLUMN),
                0.0,
                Place { branch: None },
                true,
                None,
                vec![],
                true,
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 700.0)),
            );
            let mut probe = Probe {
                composer: card_control_id(runtime.target.card, "chat-composer-surface"),
                transcript: card_control_id(runtime.target.card, "chat-transcript"),
                editor: None,
                viewport: None,
                answer: None,
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            let editor = probe.editor.expect("composer");
            let viewport = probe.viewport.expect("transcript");
            if state != 0 {
                let answer = probe.answer.expect("answer row");
                assert_eq!(
                    (answer.x, answer.width),
                    (editor.x, editor.width),
                    "transcript rows and composer share the same column in state {state}"
                );
            }
            let column_width = width.min(super::super::layout::READING_COLUMN);
            let geometry = (viewport.x, viewport.width, editor.x, editor.width);
            // The viewport, and so its scrollbar, reaches the pane edges;
            // the column is centered inside it.
            assert_eq!((viewport.x, viewport.width), (0.0, width), "state {state}");
            assert_eq!(
                editor.x,
                (width - column_width) / 2.0 + step::GAP_M,
                "state {state}"
            );
            assert_eq!(
                editor.width,
                column_width - 2.0 * step::GAP_M,
                "state {state}"
            );
            assert_eq!(*baseline.get_or_insert(geometry), geometry, "state {state}");
            // Nothing of the pane lies between the transcript and the
            // composer: a row scrolling down meets the composer's own
            // edge, never a strip of pane drawn over it.
            if state != 2 && state != 4 {
                assert_eq!(
                    viewport.y + viewport.height,
                    editor.y,
                    "the transcript runs on to the composer in state {state} at {width}"
                );
            }
        }
    }
}

#[test]
fn prompt_notice_and_raw_output_share_transcript_selection() {
    use iced::advanced::{
        layout,
        widget::{Operation, Tree},
    };
    let items = frame_cost::transcript(1);
    let (runtime, _handle) = frame_cost::live_runtime(&items);
    let prompt = runtime
        .snapshot
        .items
        .iter()
        .find(|item| item.role == chat::Role::User)
        .expect("accepted user prompt");
    let expected = match &prompt.content {
        ItemContent::Text { text, .. } => text.clone(),
        _ => panic!("fixture prompt must be text"),
    };
    let output = tool_text::ToolText::update(None, "tool output\n", tool_text::Language::Plain);
    let content = widget::column::with_children(vec![
        runtime.prompt_view(prompt),
        transcript_body("Queued message", runtime.target),
        notice_entry(
            "Provider warning",
            runtime.target,
            notice_identity(None, "Provider warning"),
        ),
        output.view(
            crate::settings::EditorSyntaxTheme::Auto,
            None,
            runtime.target,
            None,
        ),
    ]);
    let mut element = selection::transcript(content, runtime.target, None);
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1200.0));
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    #[derive(Default)]
    struct ReadText(Vec<(String, iced::Rectangle)>);
    impl Operation for ReadText {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                self.0.push((prose.plain.to_string(), bounds));
            }
        }
    }
    let mut text = ReadText::default();
    element.as_widget_mut().operate(
        &mut tree,
        iced::advanced::Layout::new(&node),
        &renderer,
        &mut text,
    );
    assert_eq!(
        text.0
            .iter()
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>(),
        [
            expected.clone(),
            "Queued message".into(),
            "Provider warning".into(),
            "tool output\n".into()
        ]
    );
    #[derive(Default)]
    struct ClipboardText(String);
    impl iced::advanced::Clipboard for ClipboardText {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            Some(self.0.clone())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
            self.0 = contents;
        }
    }
    let first = text.0.first().unwrap().1;
    let last = text.0.last().unwrap().1;
    let start = iced::Point::new(first.x, first.y + 3.0);
    let end = iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0);
    let mut clipboard = ClipboardText::default();
    let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 1200.0));
    for (event, pointer) in [
        (
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            start,
        ),
        (
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
            end,
        ),
        (
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
            end,
        ),
        (
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("c".into()),
                modified_key: keyboard::Key::Character("c".into()),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat: false,
            }),
            end,
        ),
    ] {
        let mut messages = Vec::new();
        let mut shell = iced::advanced::Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &event,
            iced::advanced::Layout::new(&node),
            iced::mouse::Cursor::Available(pointer),
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert!(shell.is_event_captured());
        if shell.is_layout_invalid() {
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
    }
    assert_eq!(
        clipboard.0,
        format!("{expected}\n\nQueued message\n\nProvider warning\n\ntool output\n")
    );
}

#[test]
fn forgetting_a_conversation_releases_catalog_and_selection_contexts() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&frame_cost::transcript(1));
    let (catalog, _) = runtime.selection_document();
    let backing = Arc::downgrade(&catalog);
    assert!(catalog.retained_plain_bytes() > 0);
    drop(catalog);
    runtime.forget_transcript();
    assert!(runtime.selection_document.borrow().is_none());
    assert!(runtime.selection_bookmarks.borrow().is_empty());
    assert!(
        backing.upgrade().is_none(),
        "no orphan logical plaintext owner survives reset"
    );
}

#[test]
fn notice_projection_keeps_exact_visible_newlines() {
    for body in ["short", "short\n", "short\n\n"] {
        assert_eq!(visible_notice_body(body, false), body);
        assert_eq!(visible_notice_body(body, true), body);
    }
    let long = "line\n".repeat(NOTICE_LINES + 1);
    assert_eq!(
        visible_notice_body(&long, false),
        ["line"; NOTICE_LINES].join("\n")
    );
    assert_eq!(visible_notice_body(&long, true), long);
}

#[test]
fn failure_catalog_keeps_visible_turn_order_and_duplicate_messages_distinct() {
    let items: Vec<_> = frame_cost::transcript(2)
        .into_iter()
        .filter(|item| item.role == chat::Role::User || item.id.ends_with("-answer"))
        .collect();
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    assert_eq!(runtime.turns.len(), 2);
    let first = runtime.turns[0].key.clone();
    let second = runtime.turns[1].key.clone();
    let raw = format!(
        "Repeated failure\n{}\nHIDDEN_DIAGNOSTIC_END",
        "original raw bytes ".repeat(8192)
    );
    runtime.failures = [&first, &second]
        .map(|key| failure::Note {
            message: Some(key.clone()),
            after: Some(key.clone()),
            text: raw.clone(),
        })
        .into();
    runtime.error = Some("Current failure".into());
    runtime.refresh_failure_ui();
    let keys: Vec<_> = runtime
        .failure_ui
        .incidents
        .iter()
        .map(|incident| incident.key.clone())
        .collect();
    assert_eq!(
        keys.len(),
        3,
        "different prompt scopes retain distinct incidents"
    );
    let (collapsed, _) = runtime.selection_document();
    assert!(
        !collapsed
            .search_blocks()
            .iter()
            .any(|(_, text)| text.contains("Repeated failure") || text.contains("Current failure"))
    );
    for key in &keys {
        runtime.failure_ui.expanded.edit().insert(key.clone());
        runtime.cache_failure_preview(key);
    }
    let (catalog, _) = runtime.selection_document();
    let blocks = catalog.search_blocks();
    let position = |identity| {
        blocks
            .iter()
            .position(|(candidate, _)| *candidate == identity)
            .unwrap()
    };
    let first_notice = position(failure::diagnostic_preview_identity(&keys[0]));
    let second_notice = position(failure::diagnostic_preview_identity(&keys[1]));
    let second_prompt = &items[runtime.turns[1].prompt[0]];
    assert!(first_notice < position(selection::logical_identity(&second_prompt.id, &[2])));
    assert!(second_notice > first_notice);
    assert_eq!(blocks.last().unwrap().1.as_ref(), "Current failure");
    assert_eq!(blocks[first_notice].1.len(), 8192);
    assert_eq!(blocks[second_notice].1.len(), 8192);
    assert!(
        !blocks
            .iter()
            .any(|(_, text)| text.contains("HIDDEN_DIAGNOSTIC_END"))
    );
    assert_eq!(
        runtime.failure_details(&keys[0]).as_deref(),
        Some(raw.as_str())
    );
    assert_eq!(
        runtime.failure_details(&keys[1]).as_deref(),
        Some(raw.as_str())
    );
    // A same-anchor source replacement must retire the old preview and
    // its catalog identity, rather than keep invisible original bytes.
    runtime.failures[0].text = "Replacement failure at the original first prompt".into();
    runtime.refresh_failure_ui();
    let (replacement, _) = runtime.selection_document();
    assert!(
        replacement
            .text(failure::diagnostic_preview_identity(&keys[0]))
            .is_none()
    );
    assert_eq!(
        replacement
            .text(failure::diagnostic_preview_identity(&keys[1]))
            .unwrap()
            .len(),
        8192
    );
    assert!(
        runtime
            .failure_ui
            .incidents
            .iter()
            .any(|incident| incident.anchor.as_ref() == Some(&first)
                && runtime.failure_details(&incident.key).as_deref()
                    == Some(runtime.failures[0].text.as_str()))
    );
}

fn web_search_selection_fixture() -> (Runtime, Arc<ClientHandle>, chat::Item, String) {
    let query = "search query";
    let title = "Search title";
    let url = "https://example.test/source#L22";
    let snippet = format!(
        "{}WEB_SEARCH_FIND_NEEDLE",
        "literal α é excerpt ".repeat(2000)
    );
    assert!(
        query.len() + title.len() + url.len() + snippet.len() < 64 * 1024,
        "public stored metadata fixture remains below the Store shrink threshold"
    );
    let item = chat::Item {
        id: "web-search".into(),
        role: chat::Role::Assistant,
        content: ItemContent::WebSearch {
            query: query.into(),
            results: vec![chat::SearchResult {
                title: title.into(),
                url: url.into(),
                snippet: snippet.clone(),
            }],
        },
        at: None,
        started_at: None,
    };
    let expected = [query, title, url, snippet.as_str()].join("\n\n");
    let (runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    (runtime, handle, item, expected)
}

fn copy_web_search_native(select_all: bool) {
    use iced::advanced::{
        layout,
        widget::{Operation, Tree},
    };
    let (runtime, _handle, item, expected) = web_search_selection_fixture();
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    let runtime = app.chat_runtime_mut(target).unwrap();
    let key = runtime.turns[0].key.clone();
    runtime.opened.edit().insert(key, true);
    let (catalog, bookmark) = runtime.selection_document();
    let canonical = runtime
        .snapshot
        .items
        .iter()
        .find(|source| source.id == item.id)
        .unwrap();
    let content = runtime.step_detail(
        canonical,
        &runtime.cache[&item.id],
        design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_TRANSCRIPT),
    );
    let mut element = selection::transcript_virtualized(content, target, None, catalog, bookmark);
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 100_000.0));
    let _ = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    runtime.cache[&item.id].flow_cache.complete_fixture();
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    #[derive(Default)]
    struct TextBounds(Vec<(Arc<str>, iced::Rectangle)>);
    impl Operation for TextBounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push((prose.plain.clone(), bounds));
            }
        }
    }
    let mut bounds = TextBounds::default();
    element.as_widget_mut().operate(
        &mut tree,
        iced::advanced::Layout::new(&node),
        &renderer,
        &mut bounds,
    );
    assert_eq!(bounds.0.first().unwrap().0.as_ref(), "search query");
    assert!(
        bounds
            .0
            .last()
            .unwrap()
            .0
            .ends_with("WEB_SEARCH_FIND_NEEDLE")
    );
    #[derive(Default)]
    struct ClipboardText(String);
    impl iced::advanced::Clipboard for ClipboardText {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            Some(self.0.clone())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, text: String) {
            self.0 = text;
        }
    }
    let first = bounds.0.first().unwrap().1;
    let last = bounds.0.last().unwrap().1;
    let start = iced::Point::new(first.x, first.y + 3.0);
    let end = if select_all {
        start
    } else {
        iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0)
    };
    let mut events = vec![(
        iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
        start,
    )];
    if !select_all {
        events.push((
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
            end,
        ));
    }
    events.push((
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        )),
        end,
    ));
    for key in if select_all {
        vec!["a", "c"]
    } else {
        vec!["c"]
    } {
        events.push((
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key.into()),
                modified_key: keyboard::Key::Character(key.into()),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat: false,
            }),
            end,
        ));
    }
    let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 100_000.0));
    let mut clipboard = ClipboardText::default();
    for (event, point) in events {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            iced::advanced::Layout::new(&node),
            iced::mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard,
            &mut iced::advanced::Shell::new(&mut messages),
            &viewport,
        );
    }
    assert_eq!(
        clipboard.0, expected,
        "actual native WebSearch selection copies query/title/URL/snippet in visible source order through the Runtime Catalog"
    );
}

#[test]
fn native_web_search_ctrl_a_copy_keeps_all_visible_literal_fields() {
    copy_web_search_native(true);
}

#[test]
fn native_web_search_drag_copy_keeps_visible_field_boundaries() {
    copy_web_search_native(false);
}

#[test]
fn actual_web_search_find_indexes_visible_result_snippet() {
    use iced::futures::StreamExt;
    let (runtime, _handle, item, _) = web_search_selection_fixture();
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "WEB_SEARCH_FIND_NEEDLE".into(),
    ));
    drop(app.handle_chat_ui(Action::FindNext(target, true)));
    let task = app.resume_current_find(target, false);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = executor.block_on(async {
        let mut stream =
            iced::runtime::task::into_stream(task).expect("actual prepared Catalog Find task");
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(effect) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    action @ Action::FindIndexed(..),
                ))) = effect
                {
                    return action;
                }
            }
            panic!("actual Catalog worker reply");
        })
        .await
        .unwrap()
    });
    drop(app.handle_chat_ui(reply));
    let hit = app
        .chat_runtime_mut(target)
        .unwrap()
        .find_navigation
        .current_hit();
    assert!(
        hit.is_some_and(|(id, _, _)| id == item.id),
        "actual admitted WebSearch Find locates the visible snippet through its prepared Runtime Catalog"
    );
}

#[test]
fn delivery_failure_icon_requires_an_explicit_command_outcome() {
    for heading in [
        "Background command failed",
        "Background command failed (exit 1)",
        "Wait for logs · failed (exit 101)",
        "Wait for logs · failed (exit 0)",
    ] {
        assert!(delivery_command_failed(heading), "{heading}");
    }
    for heading in [
        "Background command finished",
        "Wait for logs · finished",
        "Build failed while testing",
        "Message from tester: failed",
        "casmi GPU jobs finishing",
    ] {
        assert!(!delivery_command_failed(heading), "{heading}");
    }
}

#[test]
fn cli_spawn_step_stays_one_row_and_discloses_its_launch_at_narrow_width() {
    use iced::advanced::{layout, widget::Tree};
    let item = chat::Item {
        id: "spawn-reviewer".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Command {
            label: String::new(),
            command: format!(
                "cagents agent spawn --name reviewer '{}'",
                "Review the build carefully. ".repeat(30)
            ),
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(20),
            output: format!(
                "Accepted agent 1a2b3c4d run 5e6f7a8b\nIn channel \"casmi review\" ({})",
                crate::model::ChannelId::new()
            ),
        },
        started_at: None,
        at: None,
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    let renderer = crate::app::tests::headless_renderer();
    let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_TRANSCRIPT);
    let mut catalog = selection::Catalog::builder();
    runtime.selection_item(&mut catalog, &item, turn::Slot::Card, false);
    assert_eq!(catalog.finish().retained_plain_bytes(), 0);
    for width in [280.0, 900.0] {
        let cached = &runtime.cache[&item.id];
        let coordinated = cached
            .coordination
            .as_deref()
            .expect("recognized CLI spawn");
        let mut closed = runtime.coordination_card(&item, coordinated, None);
        let mut tree = Tree::new(&closed);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 2000.0));
        let node = closed.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let size = node.size();
        let accessible = closed.as_widget().a11y_nodes(
            iced::advanced::Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let buttons: Vec<_> = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node
                        .node()
                        .label()
                        .is_some_and(|label| label.contains("Open reviewer"))
            })
            .collect();
        assert_eq!(
            buttons.len(),
            1,
            "the shared row is the one named way to open this reviewer"
        );
        let bounds = buttons[0].node().bounds().unwrap();
        assert!(
            bounds.x1 - bounds.x0 > width as f64 - 20.0,
            "the whole row is the keyboard target"
        );
        assert!(
            size.width <= width && size.height < 60.0,
            "compact spawn: {size:?}"
        );
    }
    runtime.expanded.edit().insert(item.id.clone());
    let mut catalog = selection::Catalog::builder();
    runtime.selection_item(&mut catalog, &item, turn::Slot::Card, true);
    assert!(catalog.finish().retained_plain_bytes() > 0);
    let cached = &runtime.cache[&item.id];
    let mut open = runtime.coordination_card(
        &item,
        cached.coordination.as_deref().unwrap(),
        Some(runtime.step_detail(&item, cached, settings)),
    );
    let mut tree = Tree::new(&open);
    let size = open
        .as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(280.0, 4000.0)),
        )
        .size();
    assert!(size.height > 60.0, "expanded launch details: {size:?}");
    assert!(
        size.width <= 280.0,
        "expanded launch fits the pane: {size:?}"
    );
}

#[test]
fn report_outcomes_require_a_canonical_task_heading() {
    for (heading, title, outcome) in [
        ("Agent \"Review\" failed", "Agent \"Review\"", "failed"),
        ("Agent \"Review\" finished", "Agent \"Review\"", "finished"),
        (
            "Workflow \"Build\" was stopped",
            "Workflow \"Build\"",
            "was stopped",
        ),
    ] {
        assert_eq!(delivery_report_outcome("", heading), Some((title, outcome)));
        assert_eq!(delivery_report_outcome("Teammate", heading), None);
    }
    for heading in [
        "Build failed while testing",
        "Message from tester: failed",
        "Agent failed to start",
    ] {
        assert_eq!(delivery_report_outcome("", heading), None);
    }
}

#[test]
fn delivery_details_are_closed_at_narrow_width_and_preserve_literal_output() {
    use iced::advanced::{layout, widget::Tree};
    let mut item = frame_cost::transcript(1).remove(0);
    item.id = "background-delivery".into();
    item.content = ItemContent::Delivery {
        kind: chat::DeliveryKind::Notice,
        from: String::new(),
        sender: "command-task".into(),
        summary: format!("{} · finished", "Wait for k-gather logs ".repeat(20)),
        body: "cd /fixture/logs && cat < input.log > output.log".into(),
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(&[item]);
    let item = runtime.snapshot.items[0].clone();
    let renderer = crate::app::tests::headless_renderer();
    let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_TRANSCRIPT);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(280.0, 1000.0));
    let mut closed = runtime.delivery_view(&item, &runtime.cache[&item.id], settings);
    let mut tree = Tree::new(&closed);
    let node = closed.as_widget_mut().layout(&mut tree, &renderer, &limits);
    assert!(node.size().width <= 280.0);
    assert!(
        node.size().height < 60.0,
        "long heading remains one row: {:?}",
        node.size()
    );
    let closed_height = node.size().height;
    let mut catalog = selection::Catalog::builder();
    runtime.selection_item(&mut catalog, &item, turn::Slot::Prose, false);
    assert_eq!(
        catalog.finish().retained_plain_bytes(),
        0,
        "closed details cannot be selected"
    );
    runtime.expanded.edit().insert(item.id.clone());
    let mut open = runtime.delivery_view(&item, &runtime.cache[&item.id], settings);
    let mut tree = Tree::new(&open);
    let node = open.as_widget_mut().layout(&mut tree, &renderer, &limits);
    assert!(
        node.size().height > closed_height,
        "disclosure reveals command"
    );
    let mut catalog = selection::Catalog::builder();
    runtime.selection_item(&mut catalog, &item, turn::Slot::Prose, true);
    assert!(
        catalog.finish().retained_plain_bytes() > 0,
        "open literal output is selectable"
    );
}

#[test]
fn monitor_event_output_remains_visible_and_selectable_without_expanding() {
    use iced::advanced::{layout, widget::Tree};
    let mut item = frame_cost::transcript(1).remove(0);
    item.id = "monitor-delivery".into();
    item.content = ItemContent::Delivery {
        kind: chat::DeliveryKind::Notice,
        from: String::new(),
        sender: "monitor-task".into(),
        summary: "Monitor event: \"casmi GPU jobs finishing\"".into(),
        body: "k-gather-1 done: 1 failed, 3 passed in 28.96s\nx < y && y > z".into(),
    };
    let (runtime, _handle) = frame_cost::live_runtime(&[item]);
    let item = &runtime.snapshot.items[0];
    let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_TRANSCRIPT);
    let mut view = runtime.delivery_view(item, &runtime.cache[&item.id], settings);
    let mut tree = Tree::new(&view);
    let renderer = crate::app::tests::headless_renderer();
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1000.0));
    let node = view.as_widget_mut().layout(&mut tree, &renderer, &limits);
    assert!(
        node.size().height > 40.0,
        "monitor event output stays visible"
    );
    let mut catalog = selection::Catalog::builder();
    runtime.selection_item(&mut catalog, item, turn::Slot::Prose, false);
    assert!(
        catalog.finish().retained_plain_bytes() > 0,
        "visible event has selection geometry"
    );
}

#[test]
fn queued_and_notice_widgets_copy_through_logical_catalog() {
    use iced::advanced::{
        layout,
        widget::{Operation, Tree},
    };
    let items = frame_cost::transcript(1);
    let mut delivery = items[0].clone();
    delivery.id = "delivery-notice".into();
    delivery.content = ItemContent::Delivery {
        kind: chat::DeliveryKind::Notice,
        from: String::new(),
        sender: "notice-task".into(),
        summary: "Provider report".into(),
        body: "Delivered warning".into(),
    };
    let mut sources = items.clone();
    sources.push(delivery);
    let (mut runtime, _handle) = frame_cost::live_runtime(&sources);
    let delivery = runtime.snapshot.items.last().unwrap().clone();
    runtime.expanded.edit().insert(delivery.id.clone());
    let queued = Queued {
        prompt: prompt::LocalPrompt::new("Queued message".into(), vec![]),
        resolved_text: "Queued message".into(),
        model: None,
        draft: runtime.session.clone(),
        chosen: (None, None),
        since: None,
        rechecking: false,
    };
    let source = queued.prompt.source.clone();
    runtime.queued = Some(queued);
    let (run, ready) = preparation::local_reply_for_test(&mut runtime);
    let prepared = runtime
        .preparation
        .accept_local(runtime.target, run, &source, ready)
        .unwrap();
    prepared.install_local(&mut runtime.queued.as_mut().unwrap().prompt.presentation);
    let queued = runtime.queued.as_ref().unwrap();
    let cached = &runtime.cache[&delivery.id];
    let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_TRANSCRIPT);
    let first = notice_identity(Some("turn-a"), "Repeated failure");
    let second = notice_identity(Some("turn-b"), "Repeated failure");
    assert_ne!(first, second);
    let mut builder = selection::Catalog::builder();
    builder.logical(first, Arc::from("Repeated failure"));
    builder.plain("queued-prompt", &[2], Arc::from("Queued message"));
    builder.plain(&delivery.id, &[2], Arc::from("Delivered warning"));
    builder.logical(second, Arc::from("Repeated failure"));
    let content = widget::column::with_children(vec![
        notice_entry("Repeated failure", runtime.target, first),
        runtime.queued_view(queued),
        runtime.delivery_view(&delivery, cached, settings),
        notice_entry("Repeated failure", runtime.target, second),
    ]);
    let mut element = selection::transcript_virtualized(
        content,
        runtime.target,
        None,
        builder.finish(),
        Default::default(),
    );
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1200.0));
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    #[derive(Default)]
    struct ReadText(Vec<(String, iced::Rectangle)>);
    impl Operation for ReadText {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                self.0.push((prose.plain.to_string(), bounds));
            }
        }
    }
    let mut text = ReadText::default();
    element.as_widget_mut().operate(
        &mut tree,
        iced::advanced::Layout::new(&node),
        &renderer,
        &mut text,
    );
    assert_eq!(
        text.0
            .iter()
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>(),
        [
            "Repeated failure",
            "Queued message",
            "Delivered warning",
            "Repeated failure"
        ]
    );
    #[derive(Default)]
    struct ClipboardText(String);
    impl iced::advanced::Clipboard for ClipboardText {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            Some(self.0.clone())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
            self.0 = contents;
        }
    }
    let first = text.0.first().unwrap().1;
    let last = text.0.last().unwrap().1;
    let start = iced::Point::new(first.x, first.y + 3.0);
    let end = iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0);
    let mut clipboard = ClipboardText::default();
    let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 1200.0));
    for (event, pointer) in [
        (
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            start,
        ),
        (
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
            end,
        ),
        (
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
            end,
        ),
        (
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("c".into()),
                modified_key: keyboard::Key::Character("c".into()),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat: false,
            }),
            end,
        ),
    ] {
        let mut messages = Vec::new();
        let mut shell = iced::advanced::Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &event,
            iced::advanced::Layout::new(&node),
            iced::mouse::Cursor::Available(pointer),
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert!(shell.is_event_captured());
        if shell.is_layout_invalid() {
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
    }
    assert_eq!(
        clipboard.0,
        "Repeated failure\n\nQueued message\n\nDelivered warning\n\nRepeated failure"
    );
}

#[test]
fn pointer_copy_across_visual_paragraph_and_code_chunks_adds_no_separators() {
    use iced::advanced::{
        layout,
        widget::{Operation, Tree},
    };
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (source, raw_code) in [
        ("bounded αβ words ".repeat(300), false),
        ("αβ item line\n\n".repeat(300), true),
        ("αβ item line\r\n\r\n".repeat(300), true),
    ] {
        let item = chat::Item {
            id: "copy-source".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: source.clone(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        };
        let (runtime, _handle) = frame_cost::live_runtime(&[item]);
        let target = runtime.target;
        let cached = &runtime.cache["copy-source"];
        let mut builder = selection::Catalog::builder();
        let item = cached.item.as_ref().unwrap().id.clone();
        if raw_code {
            // Code copy retains the raw CRLF and terminal newline rather
            // than testing only a Markdown parser's normalized projection.
            builder.plain(&item, &[0], Arc::from(source.as_str()));
        } else {
            builder.markdown(&item, &[0], cached.markdown.items());
        }
        let catalog = builder.finish();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        let expected = catalog
            .search_blocks()
            .into_iter()
            .map(|(_, text)| text.to_string())
            .collect::<String>();
        let code_source: Arc<str> = Arc::from(source.as_str());
        let reader_source = super::super::reader_source::Source::new(&source);
        let make = |app: &Cagents| {
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&target.card) else {
                panic!("copy fixture retains its live runtime");
            };
            let cached = &runtime.cache["copy-source"];
            let body = if raw_code {
                let code = prose::code_text(
                    code_source.clone(),
                    None,
                    design::TEXT_BODY.into(),
                    iced::Font::MONOSPACE,
                    target,
                    None,
                    Some(selectable_prose::Logical {
                        identity: selection::logical_identity(&item, &[0]),
                        start: 0,
                    }),
                    Some(cached.flow_cache.clone()),
                    Some(reader_source.clone()),
                );
                widget::scrollable(code)
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        iced::widget::scrollable::Scrollbar::new(),
                    ))
                    .width(Length::Fill)
                    .into()
            } else {
                prose::view(
                    cached,
                    design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_TRANSCRIPT),
                    target,
                    None,
                )
            };
            let content = scroll::transcript(body, target, None, vec![], true, vec![]);
            selection::transcript_virtualized(
                content,
                target,
                None,
                catalog.clone(),
                Default::default(),
            )
        };
        let mut element = make(&app);
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 30000.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        // Follow actual Redraw -> FlowRequest -> admitted worker ->
        // FlowReady -> owner revision -> retained diff/layout. Completing
        // an Entry directly cannot invalidate a retained paragraph node.
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 30000.0));
        let mut settled = false;
        for _ in 0..64 {
            let mut messages = Vec::new();
            let mut shell = iced::advanced::Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                iced::advanced::Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            let invalid = shell.is_layout_invalid();
            let mut shaped = false;
            drop(element);
            for message in messages {
                let is_request = matches!(&message, Message::ChatUi(Action::FlowRequest(..)));
                let task = app.update(message);
                if !is_request {
                    drop(task);
                    continue;
                }
                shaped = true;
                let ready = executor.block_on(async {
                    tokio::time::timeout(Duration::from_secs(10), async {
                        let mut task = task;
                        let mut available = Box::pin(flow_layout::availability_stream());
                        for _ in 0..64 {
                            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                                while let Some(effect) = stream.next().await {
                                    if let iced::runtime::Action::Output(cosmic::Action::App(
                                        message @ Message::ChatUi(Action::FlowReady(..)),
                                    )) = effect
                                    {
                                        return message;
                                    }
                                }
                            }
                            // A denied admission waits for the same real
                            // availability notification used by the UI.
                            task = app.update(available.next().await.expect("Flow availability"));
                        }
                        panic!("actual Flow request did not complete");
                    })
                    .await
                    .expect("bounded actual Flow worker completes")
                });
                drop(app.update(ready));
            }
            element = make(&app);
            tree.diff(element.as_widget_mut());
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            if !invalid && !shaped {
                settled = true;
                break;
            }
        }
        assert!(settled, "actual Flow and transcript layouts converge");
        #[derive(Default)]
        struct ReadText(Vec<(String, iced::Rectangle, usize, bool)>);
        impl Operation for ReadText {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                    if prose.empty_line_caret() {
                        assert_eq!(bounds.width, 0.0, "caret adds no horizontal content extent");
                        assert!(bounds.height > 0.0);
                        let logical = prose.logical.as_ref().unwrap();
                        let caret = prose
                            .logical_byte_bounds(logical.identity, logical.start)
                            .expect("empty physical row exposes its logical caret");
                        assert!(caret.width > 0.0 && caret.height > 0.0);
                    }
                    self.0.push((
                        prose.plain.to_string(),
                        bounds,
                        prose.logical.as_ref().map_or(0, |logical| logical.start),
                        prose.empty_line_caret(),
                    ));
                }
            }
        }
        let mut text = ReadText::default();
        element.as_widget_mut().operate(
            &mut tree,
            iced::advanced::Layout::new(&node),
            &renderer,
            &mut text,
        );
        assert!(
            !text.0.is_empty(),
            "current selectable glyph geometry is mounted"
        );
        if raw_code {
            assert_eq!(
                expected, source,
                "raw code oracle preserves every newline byte"
            );
            assert!(
                text.0.iter().filter(|(_, _, _, caret)| *caret).count() > 1,
                "both middle and terminal empty rows are mounted"
            );
            assert!(
                text.0.len() > 2,
                "code selection crosses real physical row widgets"
            );
            let ending = if source.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            for (plain, _, start, caret) in &text.0 {
                if *caret {
                    assert_eq!(
                        plain, ending,
                        "visual blank retains its exact LF/CRLF bytes"
                    );
                    assert_eq!(
                        source.get(*start..*start + plain.len()),
                        Some(plain.as_str()),
                        "blank caret belongs to its current logical source slice"
                    );
                }
            }
            let terminal = text.0.last().unwrap();
            assert!(
                terminal.3,
                "terminal physical blank has real caret geometry"
            );
            assert_eq!(
                terminal.2 + terminal.0.len(),
                source.len(),
                "terminal caret covers the original source endpoint"
            );
        } else {
            assert_eq!(
                text.0.len(),
                1,
                "paragraph preserves one full-context shaped flow"
            );
            assert_eq!(text.0[0].0, expected);
        }
        #[derive(Default)]
        struct ClipboardText(String);
        impl iced::advanced::Clipboard for ClipboardText {
            fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
                Some(self.0.clone())
            }
            fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
                self.0 = contents;
            }
        }
        let first = text.0.first().unwrap().1;
        let last = text.0.last().unwrap().1;
        let start = iced::Point::new(first.x, first.y + 3.0);
        let end = iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0);
        let mut selections = vec![(start, end, expected.as_str())];
        if raw_code {
            let blank = &text.0[1];
            let next = &text.0[2];
            assert!(blank.3 && !next.3);
            assert_eq!(
                next.2,
                blank.2 + blank.0.len(),
                "middle blank and next row are adjacent original source ranges"
            );
            selections.push((
                iced::Point::new(blank.1.x + 5.0, blank.1.y + blank.1.height / 2.0),
                iced::Point::new(next.1.x, next.1.y + 3.0),
                &source[blank.2..next.2],
            ));
        }
        let mut clipboard = ClipboardText::default();
        for (start, end, expected_selection) in selections {
            for (event, pointer) in [
                (
                    iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    )),
                    start,
                ),
                (
                    iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
                    end,
                ),
                (
                    iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Left,
                    )),
                    end,
                ),
                (
                    iced::Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Character("c".into()),
                        modified_key: keyboard::Key::Character("c".into()),
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::CTRL,
                        text: None,
                        repeat: false,
                    }),
                    end,
                ),
            ] {
                let mut messages = Vec::new();
                let mut shell = iced::advanced::Shell::new(&mut messages);
                element.as_widget_mut().update(
                    &mut tree,
                    &event,
                    iced::advanced::Layout::new(&node),
                    iced::mouse::Cursor::Available(pointer),
                    &renderer,
                    &mut clipboard,
                    &mut shell,
                    &viewport,
                );
                assert!(shell.is_event_captured());
                if shell.is_layout_invalid() {
                    node = element
                        .as_widget_mut()
                        .layout(&mut tree, &renderer, &limits);
                }
            }
            assert_eq!(
                clipboard.0, expected_selection,
                "pointer copy preserves exact logical bytes, including empty physical rows"
            );
        }
    }
}

#[test]
fn waiting_queue_is_bounded_and_independent_of_transcript_scroll() {
    let mut runtime = Runtime::draft(
        Target {
            card: CardId::new(),
            generation: 0,
        },
        chat::Session {
            id: crate::model::TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: crate::model::LaunchRecipe::shell(PathBuf::from("/unused")).unwrap(),
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        },
        Snapshot::default(),
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    assert!(runtime.queue_view().is_none());
    runtime.followups.push(super::super::Followup {
        id: 0,
        text: "Local follow-up".into(),
        preview: "Local follow-up".into(),
        attachments: Vec::new(),
    });
    assert!(runtime.queue_view().is_some());
    runtime.selected_subagent = Some("child".into());
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.capabilities.queue_remove = true;
    snapshot.capabilities.queue_start = true;
    snapshot.queued_messages = (0..chat::QueuedMessage::LIMIT)
        .map(|id| chat::QueuedMessage::new(id.to_string(), "Long queued prompt\n".repeat(100)))
        .collect();
    let renderer = crate::app::tests::headless_renderer();
    for width in [320.0, 400.0, 960.0] {
        let mut element = runtime.queue_view().unwrap();
        let mut tree = iced::advanced::widget::Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 1000.0)),
        );
        assert_eq!(node.size().width, width);
        assert!(
            node.size().height < 240.0,
            "queue must leave room for transcript and composer: {:?}",
            node.size()
        );
    }
}

#[test]
fn reasoning_finishes_on_any_later_item_in_its_own_stream() {
    let thought = Arc::new(chat::Item {
        id: "thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "A thought".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        at: None,
        started_at: None,
    });
    assert!(!reasoning_finished(std::slice::from_ref(&thought), true, 0));
    assert!(reasoning_finished(std::slice::from_ref(&thought), false, 0));
    let successors = [
        thought.content.clone(),
        ItemContent::Text {
            text: "Progress".into(),
            thinking: false,
            phase: chat::Phase::Commentary,
        },
        ItemContent::Text {
            text: "Answer".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        ItemContent::Command {
            label: "probe".into(),
            command: "echo probe".into(),
            working_directory: String::new(),
            action: chat::CommandAction::Run,
            output: String::new(),
            status: chat::ToolStatus::Running,
            exit_code: None,
            duration_ms: None,
        },
        ItemContent::Tool {
            name: "Read".into(),
            input: "{}".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
    ];
    for content in successors {
        let next = Arc::new(chat::Item {
            id: "next".into(),
            content,
            ..(*thought).clone()
        });
        let items = vec![thought.clone(), next];
        assert!(reasoning_finished(&items, true, 0));
        assert!(!reasoning_finished(&items, true, 1));
    }
    let user = Arc::new(chat::Item {
        id: "user".into(),
        role: chat::Role::User,
        ..(*thought).clone()
    });
    assert!(reasoning_finished(&[thought.clone(), user], true, 0));
    let child = chat::Subagent {
        items: vec![thought.clone()],
        status: chat::ToolStatus::Running,
        ..native_child_fixture()
    };
    let parent = vec![
        thought.clone(),
        Arc::new(chat::Item {
            id: "parent-command".into(),
            ..(*thought).clone()
        }),
    ];
    assert!(reasoning_finished(&parent, true, 0));
    assert!(!reasoning_finished(&child.items, child.status.is_live(), 0));
    let mut child = child;
    child.items.push(Arc::new(chat::Item {
        id: "child-command".into(),
        content: ItemContent::Tool {
            name: "Read".into(),
            input: "{}".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
        ..(*thought).clone()
    }));
    assert!(reasoning_finished(&child.items, child.status.is_live(), 0));
}

#[test]
fn finished_reasoning_uses_its_own_end_before_successor_or_turn_timing() {
    let mut thought = chat::Item {
        id: "timed-thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: String::new(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        started_at: Some(1_000),
        at: Some(13_000),
    };
    let later = Arc::new(chat::Item {
        id: "delayed-tool".into(),
        started_at: Some(90_000),
        at: Some(100_000),
        ..thought.clone()
    });
    for text in ["", "[Redacted thinking]", "A readable thought"] {
        if let ItemContent::Text { text: body, .. } = &mut thought.content {
            *body = text.into();
        }
        for items in [
            vec![Arc::new(thought.clone())],
            vec![Arc::new(thought.clone()), later.clone()],
        ] {
            assert_eq!(
                reasoning_elapsed(&items, 0, true, Some(200_000)).as_deref(),
                Some("12s")
            );
            assert_eq!(reasoning_elapsed(&items, 0, false, Some(200_000)), None);
        }
    }
    thought.at = thought.started_at;
    assert_eq!(
        reasoning_elapsed(&[Arc::new(thought.clone())], 0, true, None),
        None,
        "under a second has no duration to show"
    );
    thought.at = None;
    let items = vec![Arc::new(thought.clone()), later];
    assert_eq!(
        reasoning_elapsed(&items, 0, true, None).as_deref(),
        Some("1m 29s")
    );
    let items = vec![Arc::new(thought.clone())];
    assert_eq!(
        reasoning_elapsed(&items, 0, true, Some(66_000)).as_deref(),
        Some("1m 5s")
    );
    assert_eq!(reasoning_elapsed(&items, 0, true, None), None);
    thought.started_at = None;
    thought.at = Some(13_000);
    assert_eq!(
        reasoning_elapsed(&[Arc::new(thought.clone())], 0, true, None),
        None,
        "a last-update timestamp alone does not establish a zero-length interval"
    );
}

/// A thought's end is read from its own position in the stream, so a
/// thought in the middle of a turn ends where the next item starts, and only
/// the last item can still be thinking.
#[test]
fn reasoning_reads_its_end_from_its_own_position() {
    let item = |id: &str, started_at, at| {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: "Weighing the options".into(),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
            started_at: Some(started_at),
            at,
        })
    };
    let items = [
        item("first", 1_000, Some(2_000)),
        item("second", 2_000, Some(3_000)),
        item("middle", 10_000, None),
        item("last", 25_000, None),
    ];
    assert!(reasoning_finished(&items, true, 2));
    assert!(!reasoning_finished(&items, true, 3));
    assert_eq!(
        reasoning_elapsed(&items, 2, true, Some(90_000)).as_deref(),
        Some("15s"),
        "the next item's start ends the middle thought, not the turn's end"
    );
    assert_eq!(
        reasoning_elapsed(&items, 3, true, Some(90_000)).as_deref(),
        Some("1m 5s"),
        "only the last item ends with the turn"
    );
    assert_eq!(reasoning_elapsed(&items, 4, true, Some(90_000)), None);
}

#[test]
fn reasoning_own_end_changes_invalidate_parent_and_mounted_child_rows() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let thought = chat::Item {
        id: "metadata-thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "Recorded thought".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        started_at: Some(1_000),
        at: Some(13_000),
    };
    let (mut runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&thought));
    let before = fingerprints(&runtime, true);
    let content_revision = runtime.cache[&thought.id].revision;
    Arc::make_mut(&mut Arc::make_mut(&mut runtime.snapshot).items[0]).at = Some(25_000);
    assert_eq!(runtime.cache[&thought.id].revision, content_revision);
    assert_ne!(
        before,
        fingerprints(&runtime, true),
        "own-end metadata changes the retained parent row"
    );
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(chat::Subagent {
            items: vec![Arc::new(thought.clone())],
            status: chat::ToolStatus::Succeeded,
            ..native_child_fixture()
        });
    runtime.selected_subagent = Some("native-child".into());
    let mut published = (*runtime.snapshot).clone();
    published.generation += 1;
    handle.publish_delta_snapshot_for_test(published);
    runtime.refresh();
    let snapshot = runtime.snapshot.clone();
    runtime.prepare_child_cache(&snapshot);
    preparation::drain_fixture(&mut runtime);
    assert!(runtime.child_cache.contains_key(&thought.id));
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let mut tree = Tree::empty();
    let render = |runtime: &Runtime, tree: &mut Tree| {
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            600.0,
            0.0,
            Place { branch: None },
            true,
            None,
            vec![],
            true,
        );
        tree.diff(&mut element);
        let node = element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 900.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            tree,
            iced::mouse::Cursor::Unavailable,
        );
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let before = render(&runtime, &mut tree);
    assert!(
        before
            .iter()
            .any(|name| name == "Reasoning: Recorded thought · 12s"),
        "{before:?}"
    );
    let revision = runtime.child_cache[&thought.id].revision;
    Arc::make_mut(&mut Arc::make_mut(&mut runtime.snapshot).subagents[0].items[0]).at =
        Some(25_000);
    assert_eq!(runtime.child_cache[&thought.id].revision, revision);
    let after = render(&runtime, &mut tree);
    assert!(
        after
            .iter()
            .any(|name| name == "Reasoning: Recorded thought · 24s"),
        "{after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|name| name == "Reasoning: Recorded thought · 12s")
    );
}

#[test]
fn reasoning_rows_use_the_selected_subagents_completion_and_duration() {
    use iced::advanced::{Layout, layout, widget::Tree};
    use iced_accessibility::accesskit::Role;
    let thought = chat::Item {
        id: "shared-thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "**First thought**\nLatest line".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        started_at: Some(1_000),
        at: Some(13_000),
    };
    let next = chat::Item {
        id: "command".into(),
        content: ItemContent::Tool {
            name: "Read".into(),
            input: "{}".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
        started_at: Some(25_000),
        at: Some(25_000),
        ..thought.clone()
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(&[thought.clone(), next.clone()]);
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = true;
    snapshot.subagents.push(chat::Subagent {
        items: vec![Arc::new(thought.clone())],
        status: chat::ToolStatus::Running,
        ..native_child_fixture()
    });
    runtime.selected_subagent = Some("native-child".into());
    let renderer = crate::app::tests::headless_renderer();
    let label = |runtime: &Runtime| {
        let mut element = runtime.text_view(
            0,
            &thought,
            &runtime.cache["shared-thought"],
            turn::Slot::Thinking,
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 800.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.node().role() == Role::Button)
            .unwrap()
            .node()
            .label()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        label(&runtime),
        "Reasoning: Latest line",
        "parent activity cannot finish a child thought"
    );
    Arc::make_mut(&mut runtime.snapshot).subagents[0]
        .items
        .push(Arc::new(next));
    assert_eq!(
        label(&runtime),
        "Reasoning: First thought · 12s",
        "a child's tool call finishes its previous thought while the child runs"
    );
    let child = &mut Arc::make_mut(&mut runtime.snapshot).subagents[0];
    child.items.pop();
    child.status = chat::ToolStatus::Succeeded;
    assert_eq!(
        label(&runtime),
        "Reasoning: First thought · 12s",
        "parent busy state cannot revive a completed child"
    );
}

#[test]
fn historical_reasoning_pages_do_not_show_live_status_or_borrow_the_latest_turns_end() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let renderer = crate::app::tests::headless_renderer();
    for body in ["**Earlier thought**\nLater line", ""] {
        let thought = chat::Item {
            id: "historical-thought".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: body.into(),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
            started_at: Some(1_000),
            at: Some(13_000),
        };
        let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&thought));
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.has_later = true;
        snapshot.finished_at = Some(99_000);
        for busy in [true, false] {
            Arc::make_mut(&mut runtime.snapshot).busy = busy;
            let mut element = runtime.text_view(
                0,
                &thought,
                &runtime.cache[&thought.id],
                turn::Slot::Thinking,
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 800.0)),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let labels = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter_map(|node| node.node().label())
                .collect::<Vec<_>>();
            let expected = if body.is_empty() {
                "Thought for 12s"
            } else {
                "Reasoning: Earlier thought · 12s"
            };
            assert!(labels.contains(&expected), "older page labels: {labels:?}");
            assert!(
                !labels
                    .iter()
                    .any(|label| label.contains("Thinking") || label.contains("38s"))
            );
        }
        let before = fingerprints(&runtime, true);
        Arc::make_mut(&mut runtime.snapshot).finished_at = Some(123_000);
        assert_eq!(
            before,
            fingerprints(&runtime, true),
            "latest turn timing cannot invalidate historical reasoning"
        );
    }
}

#[test]
fn reasoning_preview_rows_keep_duration_disclosure_and_full_width_at_narrow_sizes() {
    use iced::advanced::{Layout, layout, widget::Tree};
    use iced_accessibility::accesskit::Role;
    let thought = chat::Item {
        id: "thought".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "**First thought**\nLatest line".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        started_at: Some(1_000),
        at: Some(13_000),
    };
    let next = chat::Item {
        id: "command".into(),
        started_at: Some(25_000),
        at: Some(25_000),
        content: ItemContent::Tool {
            name: "Read".into(),
            input: "{}".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
        ..thought.clone()
    };
    let renderer = crate::app::tests::headless_renderer();
    for source in ["**First thought**\nLatest line", "", "[Redacted thinking]"] {
        let mut thought = thought.clone();
        if let ItemContent::Text { text, .. } = &mut thought.content {
            *text = source.into();
        }
        let (mut runtime, _handle) = frame_cost::live_runtime(&[thought.clone(), next.clone()]);
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        if source.is_empty() {
            // A terminal thought may have no successor. Its recorded turn
            // finish is still a real duration, including after cancellation.
            let snapshot = Arc::make_mut(&mut runtime.snapshot);
            snapshot.items.pop();
            snapshot.busy = false;
            snapshot.finished_at = Some(13_000);
        }
        for width in [200.0, 320.0, 960.0] {
            let mut element = runtime.text_view(
                0,
                &thought,
                &runtime.cache["thought"],
                turn::Slot::Thinking,
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 800.0)),
            );
            assert_eq!(node.size().width, width);
            assert!(
                node.size().height < 40.0,
                "collapsed reasoning is a single compact row"
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let buttons = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter(|node| node.node().role() == Role::Button)
                .collect::<Vec<_>>();
            if source.starts_with("**") {
                assert_eq!(buttons.len(), 1);
                assert_eq!(
                    buttons[0].node().label(),
                    Some("Reasoning: First thought · 12s")
                );
            } else {
                assert!(
                    buttons.is_empty(),
                    "empty/redacted reasoning has nothing to expand"
                );
                let labels = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .filter_map(|node| node.node().label())
                    .collect::<Vec<_>>();
                assert!(
                    labels.contains(&"Thought for 12s"),
                    "empty/redacted finished reasoning reads as one phrase: {labels:?}"
                );
                assert!(
                    !labels.contains(&"12s"),
                    "its duration is not repeated apart from it: {labels:?}"
                );
            }
        }
    }
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&thought));
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    let mut element = runtime.text_view(
        0,
        &thought,
        &runtime.cache["thought"],
        turn::Slot::Thinking,
        design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT),
    );
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 800.0)),
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    assert!(
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .any(|node| node.node().role() == Role::Button
                && node.node().label() == Some("Reasoning: Latest line"))
    );
}

#[test]
fn activity_does_not_describe_prompts_completed_commands_or_older_pages_as_live_work() {
    let mut snapshot = Snapshot {
        busy: true,
        ..Snapshot::default()
    };
    snapshot.items.push(Arc::new(chat::Item {
        id: "user".into(),
        role: chat::Role::User,
        content: ItemContent::Text {
            text: "Prompt".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        started_at: None,
        at: None,
    }));
    assert_eq!(activity_label(&snapshot), "Working");
    Arc::make_mut(&mut snapshot.items[0]).role = chat::Role::Assistant;
    assert_eq!(activity_label(&snapshot), "Responding");
    Arc::make_mut(&mut snapshot.items[0]).content = ItemContent::Command {
        label: "Echo".into(),
        command: "echo hi".into(),
        working_directory: String::new(),
        action: chat::CommandAction::Run,
        status: chat::ToolStatus::Succeeded,
        output: "hi".into(),
        exit_code: Some(0),
        duration_ms: None,
    };
    assert_eq!(activity_label(&snapshot), "Working");
    snapshot.has_later = true;
    assert_eq!(activity_label(&snapshot), "Working");
}

#[test]
fn concrete_activity_is_inferred_from_streamed_items() {
    let mut snapshot = Snapshot {
        busy: true,
        detail: "Working".into(),
        ..Snapshot::default()
    };
    snapshot.items.push(
        chat::Item {
            id: "thought".into(),
            role: chat::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: "Planning".into(),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
        }
        .into(),
    );
    assert_eq!(activity_label(&snapshot), "Thinking");
    if let ItemContent::Text { thinking, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content {
        *thinking = false;
    }
    assert_eq!(activity_label(&snapshot), "Responding");
}

/// The fingerprint of every exchange on screen, oldest first.
fn fingerprints(runtime: &Runtime, dark: bool) -> Vec<u64> {
    runtime
        .turns
        .iter()
        .map(|turn| runtime.turn_fingerprint(turn, dark))
        .collect()
}

/// The rows of the exchange at `turn`, each with the value that decides
/// whether it is drawn again.
/// The rows' fingerprints at a fixed time, so that only what the test
/// changed can change them, never a second ticking over between reads.
fn row_fingerprints(runtime: &Runtime, turn: usize) -> Vec<(TurnPart, u64)> {
    runtime.turn_row_fingerprints_at(&runtime.turns[turn], true, Some(60_000))
}

fn message(id: &str, role: chat::Role, text: &str, thinking: bool, at: u64) -> chat::Item {
    chat::Item {
        id: id.into(),
        role,
        content: ItemContent::Text {
            text: text.into(),
            thinking,
            phase: if role == chat::Role::User {
                chat::Phase::Final
            } else {
                chat::Phase::Commentary
            },
        },
        started_at: Some(at),
        at: Some(at),
    }
}

/// Publishes what `change` makes of the chat, with the agent working,
/// and prepares it.
fn publish_working(
    runtime: &mut Runtime,
    handle: &ClientHandle,
    change: impl FnOnce(&mut Snapshot),
) {
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    change(&mut snapshot);
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(runtime);
}

/// A thought is finished by what follows it rather than by any change to
/// the thought itself, so its kept row is drawn again when the answer
/// arrives, while the prompt above it keeps its own.
#[test]
fn a_kept_thought_finishes_when_the_answer_after_it_arrives() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let (mut runtime, handle) = frame_cost::live_runtime(&[
        message("prompt", chat::Role::User, "Why does it fail?", false, 0),
        message(
            "thought",
            chat::Role::Assistant,
            "**First thought**\n\nLatest line",
            true,
            1_000,
        ),
    ]);
    publish_working(&mut runtime, &handle, |_| {});
    let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(
        runtime
            .turn_view(&runtime.turns[0], settings, true)
            .as_widget(),
    );
    let texts = |runtime: &Runtime, tree: &mut Tree| {
        let mut element = runtime.turn_view(&runtime.turns[0], settings, true);
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 2000.0)),
        );
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut texts);
        texts.0
    };
    let working = texts(&runtime, &mut tree);
    assert!(
        working.iter().any(|text| text.contains("Latest line"))
            && !working.iter().any(|text| text.contains("First thought")),
        "{working:?}"
    );
    let before = row_fingerprints(&runtime, 0);
    let revision = runtime.cache["thought"].revision;
    publish_working(&mut runtime, &handle, |snapshot| {
        snapshot.items.push(Arc::new(message(
            "answer",
            chat::Role::Assistant,
            "Because the cursor is stale.",
            false,
            13_000,
        )));
    });
    assert_eq!(
        runtime.cache["thought"].revision, revision,
        "the thought itself is unchanged"
    );
    let after = row_fingerprints(&runtime, 0);
    assert_eq!(after[0], before[0], "the prompt keeps its row");
    assert_ne!(after[1], before[1], "the thought's row is drawn again");
    let finished = texts(&runtime, &mut tree);
    assert!(
        finished.iter().any(|text| text.contains("First thought")),
        "{finished:?}"
    );
}

/// The strip spins only while its newest step is the newest thing the
/// agent did, so its kept row is drawn again when a message follows the
/// step, though no step changed.
#[test]
fn a_kept_strip_stops_spinning_when_a_message_follows_its_step() {
    let (mut runtime, handle) = frame_cost::live_runtime(&[
        message("prompt", chat::Role::User, "Read the parser.", false, 0),
        chat::Item {
            id: "read".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "Read".into(),
                input: r#"{"file_path":"/project/src/parser.rs"}"#.into(),
                output: String::new(),
                status: chat::ToolStatus::Running,
            },
            started_at: Some(1_000),
            at: Some(1_000),
        },
    ]);
    publish_working(&mut runtime, &handle, |_| {});
    let strip = |rows: &[(TurnPart, u64)]| {
        rows.iter()
            .find(|(part, _)| *part == TurnPart::Steps)
            .expect("a step strip")
            .1
    };
    assert_eq!(
        runtime.strip_status(&runtime.turns[0]),
        Some(chat::ToolStatus::Running)
    );
    let before = row_fingerprints(&runtime, 0);
    publish_working(&mut runtime, &handle, |snapshot| {
        snapshot.items.push(Arc::new(message(
            "answer",
            chat::Role::Assistant,
            "Reading it now.",
            false,
            2_000,
        )));
    });
    assert_eq!(runtime.strip_status(&runtime.turns[0]), None);
    let after = row_fingerprints(&runtime, 0);
    assert_eq!(after[0], before[0], "the prompt keeps its row");
    assert_ne!(strip(&after), strip(&before), "the strip is drawn again");
}

/// Find reaches an exchange through its first row, in one place, whether
/// or not that row is mounted: a long exchange read far below its prompt
/// is found all the same.
#[test]
fn an_exchange_is_found_through_its_first_row_mounted_or_not() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    struct Anchors(widget::Id, Vec<iced::Rectangle>);
    impl Operation for Anchors {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            if id == Some(&self.0) {
                self.1.push(bounds);
            }
        }
    }
    let mut items = vec![message(
        "prompt",
        chat::Role::User,
        "Check every parser.",
        false,
        0,
    )];
    items.extend((0..80).map(|step| {
        message(
            &format!("step-{step}"),
            chat::Role::Assistant,
            &format!("Step {step}: checked the cursor handling."),
            false,
            1_000 + step,
        )
    }));
    let (runtime, _handle) = frame_cost::live_runtime(&items);
    assert_eq!(runtime.turns.len(), 1);
    let turn = &runtime.turns[0];
    let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
    let renderer = crate::app::tests::headless_renderer();
    let mut element = runtime.turn_view(turn, settings, true);
    let mut tree = Tree::new(element.as_widget());
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, f32::INFINITY));
    let height = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits)
        .size()
        .height;
    assert!(height > 3_000.0, "the exchange is {height} tall");
    for top in [0.0, height - 300.0] {
        let viewport = virtualized::Viewport {
            top,
            height: 300.0,
            bottom: false,
        };
        let mut element = runtime.turn_view_in(turn, settings, true, viewport);
        let mut tree = Tree::new(element.as_widget());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut anchors = Anchors(runtime.find_anchor(&turn.key), Vec::new());
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut anchors);
        assert_eq!(anchors.1.len(), 1, "read from {top}: {:?}", anchors.1);
        assert_eq!(anchors.1[0].y, 0.0, "read from {top}");
    }
}

/// A lone step that stopped an agent names it on its collapsed strip as
/// its open row does, once the daemon says how Claude addresses it, and
/// the drawn turn is redrawn when it learns that.
#[test]
fn a_stopped_agent_is_named_on_its_strip_once_its_address_is_known() {
    let items = vec![chat::Item {
        id: "toolu_stop".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "TaskStop".into(),
            input: r#"{"task_id":"a1892b93e88999a7d"}"#.into(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        },
        started_at: Some(1000),
        at: Some(1000),
    }];
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(chat::Subagent {
            id: "toolu_relocate".into(),
            kind: "general-purpose".into(),
            description: "Relocate the repo".into(),
            status: chat::ToolStatus::Stopped,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        });
    let turn = runtime.turns[0].clone();
    assert_eq!(runtime.strip_summary(&turn), "Stopped a background task");
    let before = runtime.turn_fingerprint(&turn, true);
    runtime.agent_addresses =
        Arc::new([("a1892b93e88999a7d".to_owned(), "toolu_relocate".to_owned())].into());
    assert_ne!(before, runtime.turn_fingerprint(&turn, true));
    let name = runtime.listed_agent_name("toolu_relocate").unwrap();
    assert_eq!(runtime.strip_summary(&turn), format!("Stopped {name}"));
}

/// A turn whose one step failed an edit to one file names that file on
/// its collapsed strip as its open row does, from the working directory,
/// with the turn's state after the file's name.
#[test]
fn a_lone_file_step_names_its_file_on_the_collapsed_strip() {
    let items = vec![chat::Item {
        id: "toolu_edit".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Changes {
            changes: vec![chat::FileChange {
                path: "/project/src/platform/a.rs".into(),
                kind: chat::ChangeKind::Updated,
                diff: String::new(),
            }],
            status: chat::ToolStatus::Failed,
        },
        started_at: Some(1000),
        at: Some(1000),
    }];
    let (runtime, _handle) = frame_cost::live_runtime(&items);
    let turn = runtime.turns[0].clone();
    assert_eq!(
        runtime.strip_summary(&turn),
        "Tried to edit src/platform/a.rs · failed"
    );
}

#[test]
fn new_turn_activity_keeps_completed_historical_reasoning_retained() {
    let items = frame_cost::transcript(3);
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    let idle = fingerprints(&runtime, true);
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    assert_eq!(
        idle,
        fingerprints(&runtime, true),
        "all fixture reasoning already has a later final answer"
    );
}

#[test]
fn busy_changes_invalidate_only_turns_containing_reasoning() {
    let mut items = frame_cost::transcript(2);
    items.retain(|item| !matches!(item.content, ItemContent::Text { thinking: true, .. }));
    for (id, role, thinking) in [
        ("reasoning-prompt", chat::Role::User, false),
        ("reasoning", chat::Role::Assistant, true),
    ] {
        items.push(chat::Item {
            id: id.into(),
            role,
            content: ItemContent::Text {
                text: id.into(),
                thinking,
                phase: chat::Phase::Final,
            },
            started_at: None,
            at: None,
        });
    }
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    let reasoning = runtime
        .turns
        .iter()
        .map(|turn| {
            turn.prose
                .iter()
                .any(|placed| placed.slot == turn::Slot::Thinking)
        })
        .collect::<Vec<_>>();
    assert_eq!(reasoning.iter().filter(|&&thinking| thinking).count(), 1);
    assert!(
        reasoning.len() >= 3,
        "historical work and the latest reasoning are grouped"
    );
    assert!(runtime.turns.iter().any(|turn| !turn.steps.is_empty()));
    let idle = fingerprints(&runtime, true);
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    let busy = fingerprints(&runtime, true);
    assert_eq!(
        idle.iter()
            .zip(&busy)
            .map(|(idle, busy)| idle != busy)
            .collect::<Vec<_>>(),
        reasoning,
    );
    let thought = runtime
        .snapshot
        .items
        .iter()
        .position(|item| item.id == "reasoning")
        .unwrap();
    assert!(!reasoning_finished(&runtime.snapshot.items, true, thought));
    Arc::make_mut(&mut runtime.snapshot).busy = false;
    assert_eq!(idle, fingerprints(&runtime, true));
    assert!(reasoning_finished(&runtime.snapshot.items, false, thought));

    // Completed historical reasoning keeps its retained presentation when
    // another turn changes the conversation's busy state.
    let (mut history, _handle) = frame_cost::live_runtime(&frame_cost::transcript(1));
    let idle = fingerprints(&history, true);
    Arc::make_mut(&mut history.snapshot).busy = true;
    assert_eq!(idle, fingerprints(&history, true));
}

#[test]
fn copy_feedback_invalidates_only_the_messages_own_turns() {
    let items = (0..3)
        .flat_map(|index| {
            [
                (chat::Role::User, format!("prompt-{index}")),
                (chat::Role::Assistant, format!("answer-{index}")),
            ]
        })
        .map(|(role, id)| chat::Item {
            content: ItemContent::Text {
                text: id.clone(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            id,
            role,
            started_at: None,
            at: None,
        })
        .collect::<Vec<_>>();
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    assert_eq!(runtime.turns.len(), 3);
    let changed = |before: &[u64], after: &[u64]| {
        before
            .iter()
            .zip(after)
            .map(|(before, after)| before != after)
            .collect::<Vec<_>>()
    };
    let original = fingerprints(&runtime, true);
    let copied_at = Instant::now();
    runtime.copied = Some(("answer-0".into(), copied_at));
    let first = fingerprints(&runtime, true);
    assert_eq!(changed(&original, &first), [true, false, false]);

    // Repeating Copy refreshes its expiry but draws the same checkmark.
    runtime.copied = Some(("answer-0".into(), copied_at + Duration::from_millis(500)));
    assert_eq!(first, fingerprints(&runtime, true));

    // Moving feedback removes one checkmark and adds the other.
    runtime.copied = Some(("answer-1".into(), copied_at));
    let second = fingerprints(&runtime, true);
    assert_eq!(changed(&first, &second), [true, true, false]);
    runtime.copied = None;
    let cleared = fingerprints(&runtime, true);
    assert_eq!(changed(&second, &cleared), [false, true, false]);
    assert_eq!(original, cleared);

    // An item outside this transcript cannot change any visible badge.
    runtime.copied = Some(("retired-item".into(), copied_at));
    assert_eq!(original, fingerprints(&runtime, true));
}

/// Publishes `items` to `runtime` as a provider frame would.
fn publish(runtime: &mut Runtime, handle: &ClientHandle, items: &[chat::Item], generation: u64) {
    handle.publish_delta_snapshot_for_test(Snapshot {
        generation,
        ready: true,
        items: items.iter().cloned().map(Arc::new).collect(),
        ..Snapshot::default()
    });
    runtime.refresh();
    preparation::drain_fixture(runtime);
}

#[test]
fn prose_only_refresh_keeps_prompt_strings_and_turn_group_allocations() {
    let mut items = frame_cost::transcript(1);
    let (mut runtime, handle) = frame_cost::live_runtime(&items);
    let prompt = runtime.history[0].as_ptr();
    let turns = runtime.turns.as_ptr();
    let prose = runtime.turns.last().unwrap().prose.as_ptr();
    let ItemContent::Text { text, .. } = &mut items.last_mut().unwrap().content else {
        unreachable!()
    };
    text.push_str(" More streamed text.");
    publish(&mut runtime, &handle, &items, 20);
    assert_eq!(
        runtime.history[0].as_ptr(),
        prompt,
        "unchanged history strings are not cloned"
    );
    assert_eq!(
        runtime.turns.as_ptr(),
        turns,
        "unchanged placement is not regrouped"
    );
    assert_eq!(runtime.turns.last().unwrap().prose.as_ptr(), prose);
}

#[test]
fn refresh_reparses_tail_colliding_rewrites_and_invalidates_retained_turns() {
    let body = |prefix: &str| format!("```text\n{}{}\n```\n", prefix.repeat(80), "z".repeat(80));
    let mut items = vec![chat::Item {
        id: "answer".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: body("a"),
            thinking: false,
            phase: chat::Phase::Final,
        },
        started_at: None,
        at: None,
    }];
    let (mut runtime, handle) = frame_cost::live_runtime(&items);
    let before = fingerprints(&runtime, true);
    let token = runtime.cache["answer"].token;
    let revision = runtime.cache["answer"].revision;
    // A new immutable Arc with identical content keeps its prepared presentation.
    publish(&mut runtime, &handle, &items, 1);
    assert_eq!(revision, runtime.cache["answer"].revision);
    assert_eq!(before, fingerprints(&runtime, true));
    items[0].content = ItemContent::Text {
        text: body("b"),
        thinking: false,
        phase: chat::Phase::Final,
    };
    assert_eq!(token, super::super::ChangeToken::of(&items[0].content));
    publish(&mut runtime, &handle, &items, 2);
    assert_ne!(before, fingerprints(&runtime, true));
    let codes = |runtime: &Runtime| {
        let mut codes = Vec::new();
        super::super::for_each_fence(runtime.cache["answer"].markdown.items(), &mut |_, code| {
            codes.push(code.to_owned());
        });
        codes
    };
    assert_eq!(
        codes(&runtime),
        vec![format!("{}{}\n", "b".repeat(80), "z".repeat(80))]
    );
    // Rewriting an earlier prefix and appending is not an incremental append either.
    items[0].content = ItemContent::Text {
        text: format!("{}\nMore", body("c")),
        thinking: false,
        phase: chat::Phase::Final,
    };
    publish(&mut runtime, &handle, &items, 3);
    assert_eq!(
        codes(&runtime),
        vec![format!("{}{}\n", "c".repeat(80), "z".repeat(80))]
    );
}

#[test]
fn refresh_replaces_expanded_tool_text_after_a_tail_colliding_rewrite() {
    let mut items = vec![chat::Item {
        id: "tool".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "shell".into(),
            input: "ls".into(),
            output: format!("{}{}", "a".repeat(80), "z".repeat(80)),
            status: chat::ToolStatus::Succeeded,
        },
        started_at: None,
        at: None,
    }];
    let (mut runtime, handle) = frame_cost::live_runtime(&items);
    runtime.expanded.edit().insert("tool".into());
    // First update creates the expanded presentation.
    if let ItemContent::Tool { input, .. } = &mut items[0].content {
        *input = "pwd".into();
    }
    publish(&mut runtime, &handle, &items, 1);
    let token = runtime.cache["tool"].token;
    let before = fingerprints(&runtime, true);
    let expected = format!("{}{}", "b".repeat(80), "z".repeat(80));
    if let ItemContent::Tool { output, .. } = &mut items[0].content {
        *output = expected.clone();
    }
    assert_eq!(token, super::super::ChangeToken::of(&items[0].content));
    publish(&mut runtime, &handle, &items, 2);
    assert_eq!(
        runtime.cache["tool"]
            .tool_output
            .as_ref()
            .unwrap()
            .text
            .as_ref(),
        expected
    );
    assert_ne!(before, fingerprints(&runtime, true));
}

#[test]
fn image_handle_changes_invalidate_retained_turns_even_without_a_loading_frame() {
    let items = vec![chat::Item {
        id: "image".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Image {
            path: "/image.png".into(),
            description: "Image".into(),
            reference: None,
        },
        started_at: None,
        at: None,
    }];
    let (mut runtime, _) = frame_cost::live_runtime(&items);
    let cached = runtime.cache.get_mut("image").unwrap();
    cached.image_source = None;
    cached.image = Some(iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4]));
    let before = fingerprints(&runtime, true);
    runtime.cache.get_mut("image").unwrap().image =
        Some(iced::widget::image::Handle::from_rgba(1, 1, vec![255; 4]));
    assert_ne!(before, fingerprints(&runtime, true));
}

#[test]
fn tool_heartbeats_do_not_rebuild_the_turn_within_an_elapsed_second() {
    let mut items = vec![chat::Item {
        id: "tool".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Bash".into(),
            input: "sleep 5".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
        started_at: Some(1000),
        at: Some(1500),
    }];
    let (mut runtime, handle) = frame_cost::live_runtime(&items);
    let fingerprint = runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(2500));
    items[0].at = Some(1800);
    publish(&mut runtime, &handle, &items, 1);
    assert_eq!(
        fingerprint,
        runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(2900))
    );
    assert_ne!(
        fingerprint,
        runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(3000))
    );
    if let ItemContent::Tool { status, .. } = &mut items[0].content {
        *status = chat::ToolStatus::Succeeded;
    }
    items[0].at = Some(3500);
    publish(&mut runtime, &handle, &items, 2);
    assert_ne!(
        fingerprint,
        runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(4000)),
        "a tool's settled status and duration still invalidate its turn",
    );
    assert_eq!(
        runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(4000)),
        runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(60000)),
        "completed durations stay fixed",
    );
}

#[test]
fn quiet_coordination_and_child_tool_rows_advance_only_on_elapsed_seconds() {
    let item = chat::Item {
        id: "quiet-call".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Task".into(),
            input: r#"{"prompt":"Inspect the changes","subagent_type":"Explore"}"#.into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
        started_at: Some(1_000),
        at: Some(1_500),
    };
    let (runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    let turn = &runtime.turns[0];
    assert!(
        turn.steps.is_empty(),
        "coordination cards render among prose"
    );
    assert_eq!(
        runtime.turn_fingerprint_at(turn, true, Some(2_500)),
        runtime.turn_fingerprint_at(turn, true, Some(2_999))
    );
    assert_ne!(
        runtime.turn_fingerprint_at(turn, true, Some(2_500)),
        runtime.turn_fingerprint_at(turn, true, Some(3_000)),
        "a quiet raw-call timer must keep advancing"
    );
    let mut agent = native_child_fixture();
    agent.items = vec![Arc::new(item.clone())];
    let cached = &runtime.cache[&item.id];
    let revision = |item: &chat::Item, now| {
        runtime.child_item_revision(&agent, 0, item, cached, true, Some(now))
    };
    assert_eq!(revision(&item, 2_500), revision(&item, 2_999));
    assert_ne!(revision(&item, 2_500), revision(&item, 3_000));
    let mut waiting = item;
    if let ItemContent::Tool { status, .. } = &mut waiting.content {
        *status = chat::ToolStatus::Waiting;
    }
    assert_eq!(
        revision(&waiting, 3_000),
        revision(&waiting, 90_000),
        "background waiting retains its observed work interval"
    );
}

/// A streaming answer is the only exchange that changes, so it is the
/// only one drawn again; the ones above it keep what they drew.
#[test]
fn only_the_streaming_exchange_is_drawn_again() {
    let mut items = frame_cost::transcript(3);
    let (mut runtime, handle) = frame_cost::live_runtime(&items);
    let before = fingerprints(&runtime, true);
    assert!(before.len() >= 3);
    let last = items.len() - 1;
    let ItemContent::Text { text, .. } = &mut items[last].content else {
        unreachable!()
    };
    text.push_str(" and one more word");
    publish(&mut runtime, &handle, &items, 1);
    let after = fingerprints(&runtime, true);
    assert_eq!(before[..before.len() - 1], after[..after.len() - 1]);
    assert_ne!(before.last(), after.last());
    // A republished snapshot with nothing new changes nothing.
    publish(&mut runtime, &handle, &items, 2);
    assert_eq!(fingerprints(&runtime, true), after);
}

/// Anything a retained exchange draws besides its items is part of what
/// decides whether it is drawn again.
#[test]
fn an_exchange_is_drawn_again_when_its_presentation_changes() {
    let items = frame_cost::transcript(1);
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    let strip = runtime
        .turns
        .iter()
        .position(|turn| !turn.steps.is_empty())
        .expect("a group with steps");
    let fingerprint = |runtime: &Runtime, dark| fingerprints(runtime, dark)[strip];
    let light = fingerprint(&runtime, false);
    assert_ne!(light, fingerprint(&runtime, true), "the appearance");
    let key = runtime.turns[strip].key.clone();
    let open = runtime
        .opened
        .get(&key)
        .copied()
        .unwrap_or(runtime.tool_groups_expanded);
    runtime.opened.edit().insert(key, !open);
    let toggled = fingerprint(&runtime, false);
    assert_ne!(light, toggled, "the strip opening or closing");
    runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::SolarizedDark);
    assert_ne!(toggled, fingerprint(&runtime, false), "the syntax theme");
}

/// Open details own their content, and remain reusable until a rendered
/// input changes. Disclosure itself must change that key.
#[test]
fn an_open_steps_retained_key_changes_once_for_its_disclosure() {
    let items = frame_cost::transcript(1);
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    let strip = runtime
        .turns
        .iter()
        .position(|turn| !turn.steps.is_empty())
        .expect("a group with steps");
    let closed = runtime.turn_fingerprint(&runtime.turns[strip], true);
    let step = runtime.turns[strip].steps[0].index;
    runtime
        .expanded
        .edit()
        .insert(runtime.snapshot.items[step].id.clone());
    let opened = runtime.turn_fingerprint(&runtime.turns[strip], true);
    assert_ne!(closed, opened);
    assert_eq!(
        opened,
        runtime.turn_fingerprint(&runtime.turns[strip], true)
    );
}

/// Every kind of Markdown block an answer can hold is drawn by the
/// transcript's own renderer, including those inside lists and quotes.
#[test]
fn every_markdown_block_is_drawn() {
    let cached = CachedItem::new(
        &ItemContent::Text {
            text: "# Title\n\nA [link](https://example.com) and `code`.\n\n\
                       - one\n- [x] done\n\n3. three\n4. four\n\n> quoted\n>\n> ```sh\n> ls\n> ```\n\n\
                       ---\n\n| a | b |\n|:-:|--:|\n| 1 | 2 |\n\n![alt](image.png)\n\n\
                       ```rust\nfn main() {}\n```\n"
                .into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        crate::settings::EditorSyntaxTheme::Auto,
    );
    assert_eq!(
        cached.blocks.len(),
        2,
        "the quoted fence and the top-level one"
    );
    let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
    let renderer = crate::app::tests::headless_renderer();
    let mut element = prose::view(
        &cached,
        settings,
        Target {
            card: CardId::new(),
            generation: 0,
        },
        None,
    );
    let mut tree = iced::advanced::widget::Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 4_000.0)),
    );
    // Heading, paragraph, both lists, the quote, the rule, the table, the
    // image and the fence, each laid out with room of its own.
    assert_eq!(node.children().len(), 9);
    assert!(
        node.children()
            .iter()
            .all(|block| block.size().width > 0.0 && block.size().height > 0.0)
    );
}

#[test]
fn up_reaches_the_queue_from_an_empty_draft_and_alt_up_from_anywhere() {
    use keyboard::Modifiers;
    use keyboard::key::Named;
    for (named, modifiers, empty, expected) in [
        (Named::ArrowUp, Modifiers::empty(), true, true),
        (Named::ArrowUp, Modifiers::empty(), false, false),
        (Named::ArrowUp, Modifiers::ALT, false, true),
        (Named::ArrowUp, Modifiers::ALT, true, true),
        // Selecting upward and jumping the caret stay the editor's.
        (Named::ArrowUp, Modifiers::SHIFT, true, false),
        (Named::ArrowUp, Modifiers::CTRL, true, false),
        (
            Named::ArrowUp,
            Modifiers::ALT | Modifiers::SHIFT,
            true,
            false,
        ),
        (Named::ArrowDown, Modifiers::ALT, true, false),
    ] {
        assert_eq!(
            queue_key(&key(named, modifiers), empty),
            expected,
            "{named:?} {modifiers:?} empty={empty}"
        );
    }
    let mut unfocused = key(Named::ArrowUp, Modifiers::empty());
    unfocused.status = text_editor::Status::Active;
    assert!(!queue_key(&unfocused, true));
}

fn press(modifiers: keyboard::Modifiers, focused: bool) -> text_editor::KeyPress {
    let key = keyboard::Key::Named(keyboard::key::Named::Enter);
    text_editor::KeyPress {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
        modifiers,
        text: None,
        status: if focused {
            text_editor::Status::Focused { is_hovered: false }
        } else {
            text_editor::Status::Active
        },
    }
}

/// A press of `named`, which is what routing reads; the physical key
/// is not.
fn key(named: keyboard::key::Named, modifiers: keyboard::Modifiers) -> text_editor::KeyPress {
    let key = keyboard::Key::Named(named);
    text_editor::KeyPress {
        modified_key: key.clone(),
        key,
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
        modifiers,
        text: None,
        status: text_editor::Status::Focused { is_hovered: false },
    }
}

/// With the list open the keys a reader picks with go to it, and Enter
/// goes to it only when choosing still changes something; otherwise it
/// sends, or edits, exactly as it would with no list.
#[test]
fn an_open_list_takes_only_the_keys_it_answers_to() {
    use keyboard::key::Named;
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    let list = |enter_chooses| OpenList {
        insertion: "$commits".into(),
        enter_chooses,
    };
    let none = keyboard::Modifiers::empty();
    let send = Some(PromptMode::Send);
    let route = |named, modifiers, open: &OpenList| {
        let press = key(named, modifiers);
        let send = submit_chord(&press, send, None, true);
        completion_action(target, &press, open, send)
    };

    assert!(matches!(
        route(Named::ArrowDown, none, &list(false)),
        Some(Action::CompletionMove(_, true))
    ));
    assert!(matches!(
        route(Named::ArrowUp, none, &list(false)),
        Some(Action::CompletionMove(_, false))
    ));
    assert!(matches!(
        route(Named::Escape, none, &list(false)),
        Some(Action::CompletionDismiss(_))
    ));
    // Tab always chooses, and does nothing else if the row is gone when
    // it lands.
    assert!(matches!(
        route(Named::Tab, none, &list(false)),
        Some(Action::Complete(_, insertion, None)) if insertion == "$commits"
    ));
    // Enter chooses only when the list says it would change the word, and
    // sends as it would have if the word moved on before it landed.
    assert!(matches!(
        route(Named::Enter, none, &list(true)),
        Some(Action::Complete(_, insertion, Some(otherwise)))
            if insertion == "$commits"
                && matches!(*otherwise, Action::Submit(_, PromptMode::Send))
    ));
    assert!(route(Named::Enter, none, &list(false)).is_none());
    // Where Enter breaks the line, that is what it falls back to.
    let press = key(Named::Enter, none);
    assert!(matches!(
        completion_action(target, &press, &list(true), None),
        Some(Action::Complete(_, _, Some(otherwise)))
            if matches!(
                *otherwise,
                Action::Composer(_, text_editor::Action::Edit(text_editor::Edit::Enter))
            )
    ));
    // Chords reach past the list, to sending and to the editor.
    for (named, modifiers) in [
        (Named::Enter, keyboard::Modifiers::COMMAND),
        (Named::Enter, keyboard::Modifiers::SHIFT),
        (Named::Tab, keyboard::Modifiers::SHIFT),
        (Named::ArrowDown, keyboard::Modifiers::ALT),
    ] {
        assert!(
            route(named, modifiers, &list(true)).is_none(),
            "{named:?} with {modifiers:?}"
        );
    }
    // Keys the list does not answer to stay with the editor.
    assert!(route(Named::ArrowLeft, none, &list(true)).is_none());
}

/// The hint says which named skills reach the agent as words, and why.
#[test]
fn the_unloaded_hint_names_the_skills_and_the_rule() {
    assert_eq!(
        unloaded_hint(&["/beta"], chat::SkillReach::Leading).as_deref(),
        Some("/beta goes as plain text: only a skill that starts the message is loaded with it")
    );
    assert_eq!(
        unloaded_hint(&["/skill:b", "/skill:c"], chat::SkillReach::First).as_deref(),
        Some(
            "/skill:b, /skill:c go as plain text: only the first skill a message names is loaded with it"
        )
    );
    assert_eq!(unloaded_hint(&[], chat::SkillReach::Leading), None);
}

/// The composer is the product's primary input: the chords must reach the
/// send controls, and nothing else may submit a half-written message.
#[test]
fn the_modified_chord_submits_under_either_preference() {
    let send = Some(PromptMode::Send);
    let queue = Some(PromptMode::Queue);

    for enter_sends in [true, false] {
        assert_eq!(
            submit_chord(
                &press(keyboard::Modifiers::COMMAND, true),
                send,
                None,
                enter_sends
            ),
            send,
            "Ctrl+Enter must keep sending whichever key the preference favours"
        );
        assert_eq!(
            submit_chord(
                &press(
                    keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                    true
                ),
                send,
                queue,
                enter_sends
            ),
            queue
        );
        // No alternate mode on offer: the chord falls back to editing.
        assert_eq!(
            submit_chord(
                &press(
                    keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                    true
                ),
                send,
                None,
                enter_sends
            ),
            None
        );
        for modifiers in [
            // Shift+Enter is the line break under either preference, so
            // there is one keystroke that always means the same thing.
            keyboard::Modifiers::SHIFT,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::COMMAND | keyboard::Modifiers::ALT,
        ] {
            assert_eq!(
                submit_chord(&press(modifiers, true), send, queue, enter_sends),
                None,
                "{modifiers:?} must edit the draft rather than submit it"
            );
        }
        // The binding is offered every key press in the window.
        assert_eq!(
            submit_chord(
                &press(keyboard::Modifiers::COMMAND, false),
                send,
                queue,
                enter_sends
            ),
            None
        );
        // Nothing to submit: the chord falls back to inserting a line.
        assert_eq!(
            submit_chord(
                &press(keyboard::Modifiers::COMMAND, true),
                None,
                None,
                enter_sends
            ),
            None
        );
    }
}

/// The preference is the whole point of the setting: it has to change what
/// a bare Enter does and nothing else.
#[test]
fn a_bare_enter_submits_only_when_the_preference_asks() {
    let send = Some(PromptMode::Send);
    assert_eq!(
        submit_chord(
            &press(keyboard::Modifiers::default(), true),
            send,
            None,
            true
        ),
        send
    );
    assert_eq!(
        submit_chord(
            &press(keyboard::Modifiers::default(), true),
            send,
            None,
            false
        ),
        None
    );
    // Even when Enter sends, an unfocused composer must not answer for a
    // key press meant for another widget.
    assert_eq!(
        submit_chord(
            &press(keyboard::Modifiers::default(), false),
            send,
            None,
            true
        ),
        None
    );
}

/// Every submit control's tooltip has to name the chord bound to it, or the
/// keyboard path stays invisible.
#[test]
fn submit_tooltips_name_the_chord_the_preference_bound() {
    for (mode, slot, enter_sends, chord) in [
        (PromptMode::Send, 0, true, "Enter"),
        (PromptMode::Send, 0, false, "Ctrl+Enter"),
        (PromptMode::Steer, 0, true, "Enter"),
        (PromptMode::Queue, 0, false, "Ctrl+Enter"),
        (PromptMode::Steer, 1, true, "Ctrl+Shift+Enter"),
        (PromptMode::Queue, 1, false, "Ctrl+Shift+Enter"),
    ] {
        let (_, label) = submit_control(mode, slot, enter_sends);
        assert!(
            label.ends_with(chord),
            "{mode:?} in slot {slot} is bound to {chord} but reads {label:?}"
        );
    }
}

/// A chip the reader cannot read is worse than no chip: the point of it is
/// to confirm which file they just sent.
#[test]
fn an_attachment_is_named_by_something_a_reader_recognises() {
    assert_eq!(
        Runtime::attachment_label("/home/reader/holiday/beach.png", "Image attachment"),
        "beach.png",
        "a file on disk is identified by its name, not the browsing above it"
    );
    assert_eq!(
        Runtime::attachment_label("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg", "Diagram"),
        "Diagram",
        "an inline payload has no name, so the description has to carry it"
    );
    assert_eq!(
        Runtime::attachment_label("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg", ""),
        "Attachment",
        "a payload no provider described still needs a readable label"
    );
}

fn scroll(from_top: f32, content: f32, window: f32) -> Scroll {
    Scroll::new(from_top, content, window)
}

/// History arrives by gesture now, so the two ends of the loaded window
/// are the pagination control: a reader who has reached one has to be
/// recognised as having reached it, and one in the middle must not be.
#[test]
fn reaching_an_end_of_the_transcript_is_what_asks_for_more_of_it() {
    let middle = scroll(500.0, 2_000.0, 400.0);
    assert!(
        !middle.at_top(),
        "the middle of a transcript is not its top"
    );
    assert!(!middle.at_bottom());

    // A wheel notch settles short of an exact end, and that still counts.
    assert!(scroll(PAGE_REACH - 1.0, 2_000.0, 400.0).at_top());
    assert!(scroll(1_600.0 - PAGE_REACH + 1.0, 2_000.0, 400.0).at_bottom());

    // A transcript shorter than its pane cannot be scrolled at all, so
    // both ends are reached: with no gesture available, the page has to
    // arrive without one.
    let short = scroll(0.0, 200.0, 400.0);
    assert!(short.at_top() && short.at_bottom());
}

#[test]
fn scrollable_short_pages_have_separate_pagination_edges() {
    for travel in [0.01, 1.0, 24.0, 48.0, 96.0] {
        let content = 400.0 + travel;
        let top = scroll(0.0, content, 400.0);
        assert!(top.at_top() && !top.at_bottom());
        let bottom = scroll(content - 400.0, content, 400.0);
        assert!(bottom.at_bottom() && !bottom.at_top());
        let middle = scroll((content - 400.0) * 0.5, content, 400.0);
        assert!(!middle.at_top() && !middle.at_bottom());
    }
}

/// A question's own prose and the command or payload it is about read as
/// two different things, so the surface has to be handed them separately
/// - but only where the provider drew the line itself.
#[test]
fn a_question_is_split_only_where_the_provider_drew_a_boundary() {
    assert_eq!(
        question_and_context("Claude wants to run a command.\n\n{\"cmd\":\"rm -rf .\"}\nIn: /tmp"),
        (
            "Claude wants to run a command.",
            "{\"cmd\":\"rm -rf .\"}\nIn: /tmp"
        )
    );
    assert_eq!(
        question_and_context("Codex wants to apply a patch.\n```diff\n-old\n+new\n```"),
        ("Codex wants to apply a patch.", "-old\n+new")
    );
    // The case this rule exists for: a sentence that ran over two lines
    // is one sentence, and setting its second half as an argv would be a
    // lie about what the provider said.
    assert_eq!(
        question_and_context(
            "Claude wants to run a command in a directory\nit has not been given access to."
        ),
        (
            "Claude wants to run a command in a directory\nit has not been given access to.",
            ""
        )
    );
    assert_eq!(
        question_and_context("Codex is waiting for your answer."),
        ("Codex is waiting for your answer.", "")
    );
    // A description that is nothing but material leaves the request's own
    // title as the whole question.
    assert_eq!(
        question_and_context("```\ngit push --force\n```"),
        ("", "git push --force")
    );
}

fn choice(id: &str) -> chat::Choice {
    chat::Choice {
        id: id.into(),
        label: id.into(),
        description: String::new(),
    }
}

/// The order the answers are drawn in is the order the keyboard reaches
/// them, so a permission request has to put refusing first and the widest
/// grant last - and must not hand Enter to anything that grants.
#[test]
fn the_safe_answer_comes_first_and_enter_never_grants() {
    let permission = chat::Request {
        kind: crate::chat::RequestKind::Approval,
        id: "r1".into(),
        title: "Command approval".into(),
        description: String::new(),
        fields: Vec::new(),
        actions: vec![
            choice("acceptForSession"),
            choice("accept"),
            choice("decline"),
        ],
    };
    let mut ordered = permission.actions.clone();
    ordered.sort_by_key(stake);
    assert_eq!(
        ordered
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        vec!["decline", "accept", "acceptForSession"]
    );
    assert_eq!(
        submit_answer(&permission),
        None,
        "a question with something to refuse must not answer to Enter"
    );

    // Ordinary question forms can submit on Enter even when cancellation
    // is available. Their explicit semantic kind prevents granting permission.
    let questions = chat::Request {
        kind: chat::RequestKind::Question,
        fields: vec![chat::Field {
            id: "answer".into(),
            label: "Question".into(),
            description: String::new(),
            choices: vec![],
            multiple: false,
            secret: false,
            initial: String::new(),
        }],
        actions: vec![choice("submit"), choice("cancel")],
        ..permission
    };
    assert_eq!(
        submit_answer(&questions).map(|choice| choice.id.as_str()),
        Some("submit")
    );
    assert_eq!(
        primary_answer(&questions).map(|choice| choice.id.as_str()),
        Some("submit")
    );
    assert_eq!(
        ordered_answers(&questions)
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        vec!["submit", "cancel"]
    );
    let yes_no = chat::Request {
        fields: vec![],
        ..questions.clone()
    };
    assert!(
        submit_answer(&yes_no).is_none(),
        "fieldless confirmation requires an explicit click"
    );
    assert_eq!(
        primary_answer(&yes_no).map(|choice| choice.id.as_str()),
        Some("submit")
    );
    for kind in [chat::RequestKind::Approval, chat::RequestKind::Consent] {
        let explicit = chat::Request {
            kind,
            ..questions.clone()
        };
        assert!(submit_answer(&explicit).is_none());
        assert!(primary_answer(&explicit).is_none());
        assert_eq!(ordered_answers(&explicit)[0].id, "cancel");
    }
    // Claude's transport retains allow/deny while the product is a question.
    let claude = chat::Request {
        actions: vec![choice("allow"), choice("deny")],
        ..questions
    };
    assert_eq!(
        primary_answer(&claude).map(|choice| choice.id.as_str()),
        Some("allow")
    );
    assert_eq!(
        submit_answer(&claude).map(|choice| choice.id.as_str()),
        Some("allow")
    );
    assert_eq!(ordered_answers(&claude)[0].id, "allow");
}

fn model(id: &str, label: &str) -> chat::Model {
    chat::Model {
        id: id.into(),
        label: label.into(),
        reasoning: Vec::new(),
    }
}

fn listed<'a>(groups: &[(Provider, Vec<ModelRow<'a>>)]) -> Vec<(Provider, Vec<&'a str>)> {
    groups
        .iter()
        .map(|(provider, rows)| {
            (
                *provider,
                rows.iter()
                    .map(|row| row.model.map_or("default", |model| model.id.as_str()))
                    .collect(),
            )
        })
        .collect()
}

/// A draft can start on a provider's default model as well as a named
/// one, so the default heads each provider's group; a started
/// conversation can only switch to a model it names.
#[test]
fn model_search_matches_the_visible_default_alias_label() {
    let models = [model("default", "Opus 5.5")];
    assert_eq!(model_hits(&models, "Default (Opus")[0].id, "default");
    let offers = [Offer {
        provider: Provider::Claude,
        models: &models,
        catalog: None,
    }];
    assert_eq!(
        listed(&model_rows(&offers, "", true)),
        [(Provider::Claude, vec!["default"])]
    );
}

#[test]
fn footer_cached_implicit_default_mounts_checked_model_and_effort_choices() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let (mut runtime, _handle) = furnished_runtime();
    runtime.model_picker = Some(String::new());
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.models = vec![chat::Model {
        id: "default".into(),
        label: "Default (Opus 5.5)".into(),
        reasoning: vec!["low".into(), "high".into()],
    }];
    snapshot.selected_model = None;
    snapshot.reasoning = None;
    snapshot.active_model = None;
    let renderer = crate::app::tests::headless_renderer();
    let mut menu = runtime.model_search(Vec::new()).unwrap();
    let mut tree = Tree::new(menu.as_widget());
    let node = menu.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 900.0)),
    );
    let accessible =
        menu.as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    let choices: Vec<_> = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| {
            node.node().label().map(|name| {
                (
                    name.to_owned(),
                    node.node().description().map(str::to_owned),
                )
            })
        })
        .collect();
    for title in ["Default (Opus 5.5)", "Default", "Low", "High"] {
        assert!(
            choices.iter().any(|(name, selected)| name == title
                && selected.as_deref() == title.starts_with("Default").then_some("Selected")),
            "{choices:?}"
        );
    }
}

#[test]
fn footer_effort_labels_and_default_row_use_person_facing_names() {
    let model = chat::Model {
        id: "model".into(),
        label: "Model".into(),
        reasoning: ["low", "medium", "high", "xhigh", "max"]
            .map(str::to_owned)
            .into(),
    };
    assert_eq!(
        reasoning_rows(&model, None),
        vec![
            (None, "Default".into()),
            (Some("low"), "Low".into()),
            (Some("medium"), "Medium".into()),
            (Some("high"), "High".into()),
            (Some("xhigh"), "Extra high".into()),
            (Some("max"), "Max".into()),
        ]
    );
    assert_eq!(
        reasoning_rows(&model, Some("medium"))[0],
        (None, "Default (Medium)".into())
    );
    assert_eq!(effort_label("future-effort"), "future-effort");
}

#[test]
fn footer_staged_choice_tooltip_keeps_running_values_separate() {
    let (mut runtime, _handle) = furnished_runtime();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.models = vec![
        model("default", "Default (Opus 5.5)"),
        model("next", "Next model"),
    ];
    snapshot.selected_model = Some("default".into());
    snapshot.reasoning = None;
    snapshot.active_model = Some(chat::ActiveModel {
        aliases: vec!["default".into(), "opus".into()],
        id: "claude-opus-5-5".into(),
        label: "Opus 5.5".into(),
        reasoning: None,
        default_reasoning: Some("medium".into()),
    });
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)),
        None
    );
    snapshot.models.push(model("opus", "Opus 5.5"));
    snapshot.selected_model = Some("opus".into());
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)),
        None,
        "ordinary aliases also resolve to the running model"
    );
    snapshot.selected_model = None;
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)),
        None
    );
    snapshot.models[0].label = "Default (Another model)".into();
    snapshot
        .active_model
        .as_mut()
        .unwrap()
        .aliases
        .retain(|alias| alias != "default");
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)).as_deref(),
        Some("Default (Another model)")
    );
    snapshot.models[0].label = "Default (Opus 5.5)".into();
    snapshot
        .active_model
        .as_mut()
        .unwrap()
        .aliases
        .push("default".into());
    snapshot.selected_model = Some("default".into());
    snapshot.reasoning = Some("high".into());
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)).as_deref(),
        Some("High effort")
    );
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            false,
            Provider::Claude,
            FooterFit::Wide,
            None
        ),
        "Opus 5.5"
    );
    snapshot.active_model.as_mut().unwrap().reasoning = Some("high".into());
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)),
        None
    );
    snapshot.reasoning = None;
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)).as_deref(),
        Some("Default effort")
    );
    snapshot.selected_model = Some("next".into());
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)).as_deref(),
        Some("Next model · Default effort")
    );
    let catalog = crate::account::cache::Catalog {
        models: snapshot.models.clone(),
        observed_at: 1,
        default_model: None,
        reasoning_defaults: [("next".into(), "high".into())].into(),
    };
    assert_eq!(
        reasoning_default(snapshot, &snapshot.models[1], Some(&catalog)),
        Some("high")
    );
    assert_eq!(reasoning_default(snapshot, &snapshot.models[1], None), None);
    snapshot.models[1].label = "Opus 5.5".into();
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)).as_deref(),
        Some("Opus 5.5 · Default effort"),
        "distinct concrete models can share a display name"
    );
    assert_eq!(
        reasoning_default(snapshot, &snapshot.models[1], Some(&catalog)),
        Some("high")
    );
    snapshot.active_model = None;
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)),
        None
    );
    snapshot.selected_model = Some("default".into());
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            false,
            Provider::Claude,
            FooterFit::Wide,
            None
        ),
        "Default (Opus 5.5)"
    );
    snapshot.reasoning = Some("high".into());
    for provider in [Provider::Claude, Provider::Codex] {
        assert_eq!(
            footer_model_label(
                snapshot,
                ChosenModel::reported(snapshot),
                false,
                provider,
                FooterFit::Wide,
                None
            ),
            "Default (Opus 5.5) · High"
        );
    }
}

#[test]
fn footer_staged_model_uses_reported_aliases_even_when_their_names_differ() {
    let (mut runtime, _handle) = furnished_runtime();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.models = vec![
        model("opusplan", "Opus Plan"),
        model("deployment-current", "Team model"),
    ];
    snapshot.reasoning = None;
    snapshot.active_model = Some(chat::ActiveModel {
        id: "claude-sonnet-5-5".into(),
        label: "Sonnet 5.5".into(),
        aliases: vec!["deployment-current".into(), "opusplan".into()],
        reasoning: None,
        default_reasoning: None,
    });
    for alias in ["opusplan", "deployment-current"] {
        snapshot.selected_model = Some(alias.into());
        assert_eq!(
            staged_model_choice(snapshot, ChosenModel::reported(snapshot)),
            None
        );
    }
    snapshot.active_model.as_mut().unwrap().aliases.clear();
    assert_eq!(
        staged_model_choice(snapshot, ChosenModel::reported(snapshot)).as_deref(),
        Some("Team model"),
        "an unreported alias resolution cannot be inferred from its display name"
    );
}

#[test]
fn footer_model_label_names_active_model_and_keeps_draft_alias() {
    let (mut runtime, _handle) = furnished_runtime();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.models = vec![model("default", "Default (Opus 5.5)")];
    snapshot.selected_model = Some("default".into());
    snapshot.reasoning = None;
    snapshot.active_model = Some(chat::ActiveModel {
        aliases: Vec::new(),
        id: "claude-opus-5-5".into(),
        label: "Opus 5.5".into(),
        reasoning: None,
        default_reasoning: None,
    });
    for fit in [FooterFit::Wide, FooterFit::Narrow, FooterFit::Tight] {
        assert_eq!(
            footer_model_label(
                snapshot,
                ChosenModel::reported(snapshot),
                false,
                Provider::Claude,
                fit,
                None
            ),
            "Opus 5.5"
        );
        assert_eq!(
            footer_model_label(
                snapshot,
                ChosenModel::reported(snapshot),
                true,
                Provider::Claude,
                fit,
                None
            ),
            "Default (Opus 5.5)"
        );
    }
    snapshot.selected_model = None;
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            true,
            Provider::Claude,
            FooterFit::Wide,
            None
        ),
        "Default (Opus 5.5)"
    );
    snapshot.selected_model = Some("next-turn-model".into());
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            false,
            Provider::Claude,
            FooterFit::Wide,
            None
        ),
        "Opus 5.5"
    );
    snapshot.active_model = None;
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            false,
            Provider::Claude,
            FooterFit::Wide,
            None
        ),
        "next-turn-model"
    );
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            true,
            Provider::Claude,
            FooterFit::Wide,
            None
        ),
        "next-turn-model"
    );
}

#[test]
fn draft_default_effort_stays_hidden_while_attachment_choice_is_pending() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, value: &str) {
            self.0.push(value.to_owned());
        }
    }
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let models = vec![chat::Model {
        id: "offline-mini".into(),
        label: "Offline Mini".into(),
        reasoning: vec!["medium".into(), "high".into()],
    }];
    let active = chat::ActiveModel {
        aliases: Vec::new(),
        id: "offline-mini".into(),
        label: "Offline Mini".into(),
        reasoning: None,
        default_reasoning: Some("medium".into()),
    };
    drop(app.observe_listed_models(Provider::Codex, models.clone(), Some(&active)));
    let card = super::super::tests::draft_chat_for_provider(&mut app, Provider::Codex);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("draft");
    };
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.models = models;
    snapshot.selected_model = Some("offline-mini".into());
    snapshot.reasoning = Some("medium".into());
    runtime.attachment_chooser_pending = Some(uuid::Uuid::new_v4());
    assert!(runtime.is_draft());
    assert!(!runtime.choosing());
    let renderer = crate::app::tests::headless_renderer();
    let mut view = app.chat_view(card);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 900.0)),
    );
    let mut texts = Texts(Vec::new());
    view.as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
    assert!(
        texts.0.iter().any(|text| text == "Offline Mini"),
        "{:?}",
        texts.0
    );
    assert!(
        !texts.0.iter().any(|text| text.contains("Medium")),
        "{:?}",
        texts.0
    );
}

#[test]
fn draft_footer_suppresses_catalog_default_but_keeps_high_override() {
    let (mut runtime, _handle) = furnished_runtime();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.models = vec![model("offline-mini", "Offline Mini")];
    snapshot.selected_model = Some("offline-mini".into());
    snapshot.reasoning = Some("medium".into());
    let catalog = crate::account::cache::Catalog {
        models: snapshot.models.clone(),
        observed_at: 1,
        default_model: Some("offline-mini".into()),
        reasoning_defaults: [("offline-mini".into(), "medium".into())].into(),
    };
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            true,
            Provider::Codex,
            FooterFit::Wide,
            catalog.default_effort(snapshot.selected_model.as_deref())
        ),
        "Offline Mini"
    );
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            true,
            Provider::Codex,
            FooterFit::Wide,
            None
        ),
        "Offline Mini · Medium",
        "an unknown default does not erase the explicit choice"
    );
    snapshot.selected_model = None;
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            true,
            Provider::Codex,
            FooterFit::Wide,
            catalog.default_effort(None)
        ),
        "ChatGPT · Default model"
    );
    snapshot.selected_model = Some("offline-mini".into());
    snapshot.reasoning = Some("high".into());
    assert_eq!(
        footer_model_label(
            snapshot,
            ChosenModel::reported(snapshot),
            true,
            Provider::Codex,
            FooterFit::Wide,
            catalog.default_effort(snapshot.selected_model.as_deref())
        ),
        "Offline Mini · High"
    );
}

#[test]
fn footer_model_label_appends_only_nondefault_effort_for_both_providers() {
    let (mut runtime, _handle) = furnished_runtime();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.active_model = Some(chat::ActiveModel {
        aliases: Vec::new(),
        id: "resolved".into(),
        label: "Resolved model".into(),
        reasoning: None,
        default_reasoning: Some("medium".into()),
    });
    // A pending selection must not change what the running turn displays.
    snapshot.reasoning = Some("low".into());
    for provider in [Provider::Claude, Provider::Codex] {
        snapshot.active_model.as_mut().unwrap().reasoning = None;
        assert_eq!(
            footer_model_label(
                snapshot,
                ChosenModel::reported(snapshot),
                false,
                provider,
                FooterFit::Wide,
                None
            ),
            "Resolved model"
        );
        snapshot.active_model.as_mut().unwrap().reasoning = Some("medium".into());
        assert_eq!(
            footer_model_label(
                snapshot,
                ChosenModel::reported(snapshot),
                false,
                provider,
                FooterFit::Wide,
                None
            ),
            "Resolved model"
        );
        snapshot.active_model.as_mut().unwrap().reasoning = Some("high".into());
        assert_eq!(
            footer_model_label(
                snapshot,
                ChosenModel::reported(snapshot),
                false,
                provider,
                FooterFit::Wide,
                None
            ),
            "Resolved model · High"
        );
    }
}

#[test]
fn a_draft_offers_each_providers_default_ahead_of_its_models() {
    let model = |id: &str| chat::Model {
        id: id.into(),
        label: id.into(),
        reasoning: Vec::new(),
    };
    let codex = [model("gpt-a"), model("gpt-b")];
    let offers = [
        Offer {
            provider: Provider::Codex,
            models: &codex,
            catalog: None,
        },
        Offer {
            provider: Provider::Claude,
            models: &[],
            catalog: None,
        },
    ];
    assert_eq!(
        listed(&model_rows(&offers, "", true)),
        [
            (Provider::Codex, vec!["default", "gpt-a", "gpt-b"]),
            (Provider::Claude, vec!["default"]),
        ]
    );
    assert_eq!(
        listed(&model_rows(&offers[..1], "", false)),
        [(Provider::Codex, vec!["gpt-a", "gpt-b"])]
    );
    assert_eq!(
        listed(&model_rows(&offers, "gpt-b", true)),
        [(Provider::Codex, vec!["gpt-b"])]
    );
}

/// A provider is named by the start of a word it is known by, not by
/// any letters its heading happens to contain, and not by one letter.
#[test]
fn a_search_names_a_provider_by_the_start_of_its_names() {
    let omp = [chat::Model {
        id: "kimi".into(),
        label: "Kimi".into(),
        reasoning: Vec::new(),
    }];
    let offers = [Offer {
        provider: Provider::Omp,
        models: &omp,
        catalog: None,
    }];
    for query in ["omp", "Other", "mod"] {
        assert_eq!(
            listed(&model_rows(&offers, query, true)),
            [(Provider::Omp, vec!["default", "kimi"])],
            "{query}"
        );
    }
    for query in ["the", "o", "er"] {
        assert!(model_rows(&offers, query, true).is_empty(), "{query}");
    }
    assert!(names_provider(Provider::Codex, "codex"));
    assert!(names_provider(Provider::Codex, "chat"));
    assert!(!names_provider(Provider::Claude, "laude"));
}

#[test]
fn clicking_a_model_keeps_the_popup_alive_until_selection() {
    use cosmic::iced::advanced::{
        Layout, Shell, clipboard, layout, mouse, renderer::Headless, widget::Tree,
    };
    use cosmic::iced::{Event, Point, Rectangle, Size};
    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(<cosmic::Renderer as Headless>::new(
            iced::Font::DEFAULT,
            iced::Pixels(13.),
            Some("tiny-skia"),
        ))
        .unwrap();
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    let models = vec![model("gpt-selected", "ChatGPT selected")];
    let offers = [Offer {
        provider: Provider::Codex,
        models: &models,
        catalog: None,
    }];
    let mut element = model_list(target, &offers, false, (Provider::Codex, None), "");
    let mut tree = Tree::new(element.as_widget());
    let viewport = Rectangle::new(Point::ORIGIN, Size::new(300., 400.));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let field = Layout::new(&node)
        .children()
        .next()
        .unwrap()
        .bounds()
        .center();
    let row = Layout::new(&node)
        .children()
        .nth(1)
        .unwrap()
        .bounds()
        .center();
    let mut update = |pressed: bool, point: Point| {
        let mut messages = Vec::new();
        let event = Event::Mouse(if pressed {
            mouse::Event::ButtonPressed(mouse::Button::Left)
        } else {
            mouse::Event::ButtonReleased(mouse::Button::Left)
        });
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages
    };
    let _ = update(true, field);
    let _ = update(false, field);
    let pressed = update(true, row);
    assert!(
        !pressed
            .iter()
            .any(|message| matches!(message, Message::ChatUi(Action::ModelPicker(_, false))))
    );
    let released = update(false, row);
    assert!(released.iter().any(|message| matches!(message, Message::ChatUi(Action::Model(selected, model)) if *selected == target && model == "gpt-selected")));
}

/// A reader recalls a model by any part of its identifier, including the
/// provider prefix, and never by its capitalisation.
#[test]
fn the_model_search_matches_any_part_of_an_identifier() {
    let models = vec![
        model("anthropic/claude-sonnet-4-5", "Claude Sonnet 4.5"),
        model("openai/gpt-5-codex", "GPT-5 Codex"),
    ];
    let ids = |query: &str| {
        model_hits(&models, query)
            .into_iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids("SONNET"), vec!["anthropic/claude-sonnet-4-5"]);
    assert_eq!(ids("openai/"), vec!["openai/gpt-5-codex"]);
    assert_eq!(ids("codex"), vec!["openai/gpt-5-codex"]);
    // Typing what the row shows finds the row.
    assert_eq!(ids("Sonnet 4.5"), vec!["anthropic/claude-sonnet-4-5"]);
    assert_eq!(ids("  ").len(), 2, "an empty query lists every model");
    assert!(ids("haiku").is_empty());
}
#[test]
fn file_change_review_fits_narrow_and_wide_panes_and_names_delegated_proposals() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let key = chat::FileChangeKey {
        source: chat::FileChangeSource::Subagent { id: "child".into() },
        path: "src/renamed.rs".into(),
    };
    let mut snapshot = (*runtime.snapshot).clone();
    snapshot.file_changes = Arc::new(chat::FileChangesSummary {
        total_files: 1,
        coverage: chat::FileChangesCoverage {
            has_unloaded_history: true,
            has_imported_history: true,
            has_incomplete_delegated_history: true,
            ..chat::FileChangesCoverage::default()
        },
        files: vec![chat::FileChangeSummary {
            key: key.clone(),
            resolved_path: None,
            ownership_unverified: true,
            revision: 1,
            kind: chat::ChangeKind::Renamed {
                from: "src/old.rs".into(),
            },
            succeeded: 0,
            pending: 1,
            failed: 0,
            additions: None,
            deletions: None,
            imported: 1,
        }],
        ..chat::FileChangesSummary::default()
    });
    runtime.snapshot = Arc::new(snapshot);
    runtime.changes.open = true;
    let renderer = crate::app::tests::headless_renderer();
    for width in [360.0, 670.0, 960.0] {
        runtime.changes.back();
        let mut element = runtime.changes_view(width);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
        );
        assert!(
            node.size().width <= width,
            "review must fit a narrow split pane"
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let labels: Vec<_> = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect();
        for expected in [
            "Files changed",
            "Proposed edit",
            "Subagent: child",
            "pending",
            "from src/old.rs",
            "Close",
        ] {
            assert!(
                labels.iter().any(|label| label.contains(expected)),
                "{width}: missing {expected}: {labels:?}"
            );
        }
        assert!(
            !labels.iter().any(|label| label == "Open file"),
            "unresolved child-relative paths must not route to the parent"
        );
        drop(element);
        runtime.changes.select(key.clone());
        let mut selected = runtime.changes_view(width);
        let mut selected_tree = Tree::new(selected.as_widget());
        let selected_node = selected.as_widget_mut().layout(
            &mut selected_tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
        );
        let selected_accessible = selected.as_widget().a11y_nodes(
            Layout::new(&selected_node),
            &selected_tree,
            iced::mouse::Cursor::Unavailable,
        );
        let nodes: Vec<_> = selected_accessible
            .root()
            .iter()
            .chain(selected_accessible.children())
            .collect();
        if width < 560.0 {
            assert!(
                nodes
                    .iter()
                    .any(|node| node.node().label() == Some("Back to files")),
                "narrow detail view needs a visible return to the file list"
            );
            assert!(
                !nodes.iter().any(|node| node
                    .node()
                    .label()
                    .is_some_and(|label| label.starts_with("View reported edits for"))),
                "narrow panes switch to the detail instead of stacking both scrollers"
            );
        } else {
            let file = nodes
                .iter()
                .find(|node| {
                    node.node()
                        .label()
                        .is_some_and(|label| label.starts_with("View reported edits for"))
                })
                .and_then(|node| node.node().bounds())
                .expect("file list stays mounted beside the detail");
            let detail = nodes
                .iter()
                .find(|node| node.node().label() == Some("The file location was not reported."))
                .and_then(|node| node.node().bounds())
                .expect("selected file detail is mounted");
            assert!(
                file.x1 <= detail.x0,
                "{width}: default-width files and edits must sit side by side"
            );
        }
        drop(selected);
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let mut full = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            width,
            0.0,
            Place { branch: None },
            true,
            None,
            Vec::new(),
            true,
        );
        let mut full_tree = Tree::new(full.as_widget());
        let full_node = full.as_widget_mut().layout(
            &mut full_tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
        );
        let accessible = full.as_widget().a11y_nodes(
            Layout::new(&full_node),
            &full_tree,
            iced::mouse::Cursor::Unavailable,
        );
        assert!(
            !accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node
                    .node()
                    .label()
                    .is_some_and(|name| name.starts_with("Reported edits 1"))),
            "file review must not add a permanent footer action"
        );
        // The draft field stands for the composer: an empty draft
        // offers no Send to look for.
        for label in ["Message", "Close"] {
            let bounds = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .find(|node| {
                    node.node().label().is_some_and(|name| {
                        if label == "Message" {
                            name == label
                        } else {
                            name.starts_with(label)
                        }
                    })
                })
                .and_then(|node| node.node().bounds())
                .unwrap_or_else(|| panic!("{width}: {label} must remain mounted"));
            assert!(
                bounds.y0 >= 0.0 && bounds.y1 <= 600.0 && bounds.y1 > bounds.y0,
                "{width}: {label} must remain visible in a 600px window: {bounds:?}"
            );
        }
    }
}
#[test]
fn narrow_file_review_restores_actual_scroll_on_back_after_the_old_ui_operation_misses() {
    use iced::advanced::{
        Layout, layout,
        widget::{Id, Operation, Tree},
    };
    struct Offset {
        id: Id,
        y: Option<f32>,
    }
    impl Operation for Offset {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            _: iced::Rectangle,
            _: iced::Rectangle,
            translation: iced::Vector,
            _: &mut dyn iced::advanced::widget::operation::scrollable::Scrollable,
        ) {
            if id == Some(&self.id) {
                self.y = Some(translation.y);
            }
        }
    }
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let key = chat::FileChangeKey {
        source: chat::FileChangeSource::Conversation,
        path: "src/file-036.rs".into(),
    };
    let rows = (0..128)
        .map(|index| chat::FileChangeSummary {
            key: chat::FileChangeKey {
                source: chat::FileChangeSource::Conversation,
                path: format!("src/file-{index:03}.rs"),
            },
            resolved_path: Some(format!("/project/src/file-{index:03}.rs")),
            revision: 1,
            kind: chat::ChangeKind::Updated,
            succeeded: 1,
            pending: 0,
            failed: 0,
            additions: Some(1),
            deletions: Some(1),
            imported: 0,
            ownership_unverified: false,
        })
        .collect();
    let summary = chat::FileChangesSummary {
        request_id: 42,
        revision: 1,
        files: rows,
        total_files: 128,
        applied_files: 128,
        ..Default::default()
    };
    Arc::make_mut(&mut runtime.snapshot).file_changes = Arc::new(summary.clone());
    runtime.changes.list = Some(summary);
    runtime.changes.show();
    runtime.changes.list_scroll = 1000.0;
    let original_epoch = runtime.changes.list_epoch;
    let id = card_control_id(runtime.target.card, "chat-changes-files");
    let renderer = crate::app::tests::headless_renderer();
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(360.0, 600.0));
    let mut files = runtime.changes_view(360.0);
    let mut tree = Tree::new(files.as_widget());
    let node = files.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut offset = Offset {
        id: id.clone(),
        y: None,
    };
    files
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut offset);
    assert_eq!(offset.y, Some(1000.0));
    drop(files);
    runtime.changes.select(key);
    runtime
        .changes
        .record_scroll(42, 42, original_epoch, true, 0.0);
    assert_eq!(
        runtime.changes.list_scroll, 1000.0,
        "a stale retiring list callback cannot overwrite the saved offset"
    );
    let mut detail = runtime.changes_view(360.0);
    tree.diff(detail.as_widget_mut());
    let node = detail.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut early = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
        id.clone(),
        iced::widget::scrollable::AbsoluteOffset {
            x: None,
            y: Some(1000.0),
        },
    );
    detail
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut early);
    let mut absent = Offset {
        id: id.clone(),
        y: None,
    };
    detail
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut absent);
    assert_eq!(
        absent.y, None,
        "an operation before list remount cannot restore it"
    );
    drop(detail);
    runtime.changes.back();
    runtime
        .changes
        .record_scroll(42, 42, original_epoch, true, 0.0);
    assert_eq!(
        runtime.changes.list_scroll, 1000.0,
        "late callbacks remain stale after Back makes the list visible again"
    );
    let mut files = runtime.changes_view(360.0);
    tree.diff(files.as_widget_mut());
    let node = files.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut restored = Offset {
        id: id.clone(),
        y: None,
    };
    files
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut restored);
    assert_eq!(
        restored.y,
        Some(1000.0),
        "first remount layout restores the real widget offset without any follow-up task"
    );
    let mut moved = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
        id.clone(),
        iced::widget::scrollable::AbsoluteOffset {
            x: None,
            y: Some(1100.0),
        },
    );
    files
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut moved);
    let node = files.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut unchanged = Offset { id, y: None };
    files
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut unchanged);
    assert_eq!(
        unchanged.y,
        Some(1100.0),
        "later layouts cannot reset newer widget scroll before its callback arrives"
    );
}
#[test]
fn busy_composer_escape_dismisses_actions_before_interrupting() {
    use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};

    let renderer = crate::app::tests::headless_renderer();
    let viewport = iced::Rectangle::with_size(iced::Size::new(620.0, 720.0));
    for (actions_open, fork_open) in [(false, false), (true, false), (true, true)] {
        let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.ready = true;
        snapshot.busy = true;
        snapshot.capabilities.branch = true;
        snapshot.capabilities.compact = true;
        runtime.composer_actions = actions_open;
        // An already-open fork picker can remain visible as a turn starts.
        runtime.branch_picker = fork_open;
        let target = runtime.target;
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            viewport.width,
            0.0,
            Place { branch: None },
            true,
            None,
            Vec::new(),
            true,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                target.card,
                "chat-composer",
            )),
        );
        let escape = iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &escape,
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        if actions_open {
            assert!(messages.iter().any(|message| matches!(
                message,
                Message::ChatUi(Action::ComposerActions(actual, false)) if *actual == target
            )));
            assert!(
                !messages
                    .iter()
                    .any(|message| matches!(message, Message::ChatUi(Action::Interrupt(..))))
            );
        } else {
            assert!(
                messages.iter().any(|message| matches!(
                    message,
                    Message::ChatUi(Action::Interrupt(actual)) if *actual == target
                )),
                "the focused busy composer otherwise handles Escape as interruption"
            );
        }
    }
}

#[test]
fn switching_review_and_fork_popover_keeps_the_composer_visible_at_600px() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let (mut live, _handle) = frame_cost::live_runtime(&[]);
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    live.target.card = card;
    let target = live.target;
    let snapshot = Arc::make_mut(&mut live.snapshot);
    snapshot.ready = true;
    snapshot.capabilities.branch = true;
    Arc::make_mut(&mut snapshot.file_changes).total_files = 1;
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(live.session.clone());
    app.cards.insert(card, CardRuntime::Chat(Box::new(live)));
    let renderer = crate::app::tests::headless_renderer();
    for mode in [
        Action::ChangesPicker(target, true),
        Action::BranchPicker(target, true),
        Action::ChangesPicker(target, true),
    ] {
        drop(app.handle_chat_ui(mode));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("fixture chat");
        };
        assert_ne!(
            runtime.changes.open, runtime.branch_picker,
            "only the selected inline panel is open"
        );
        for width in [360.0, 670.0] {
            let mut element = runtime.view(
                None,
                false,
                true,
                &app.settings.keyboard_shortcuts,
                width,
                0.0,
                Place { branch: None },
                true,
                None,
                Vec::new(),
                true,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let bounds = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .find(|node| node.node().label() == Some("Message"))
                .and_then(|node| node.node().bounds())
                .expect("composer draft field remains mounted");
            assert!(
                bounds.y0 >= 0.0 && bounds.y1 <= 600.0 && bounds.y1 > bounds.y0,
                "{width}: inline panels must not push the composer out of the window: {bounds:?}"
            );
        }
    }
}

/// The composer is one row, `+ | draft | send | stop`, at every width.
/// Send is offered only for a draft, and a busy conversation always
/// offers Stop after it.
#[test]
fn composer_is_one_row_and_offers_stop_while_busy_with_an_empty_draft() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    for width in [280.0_f32, 670.0] {
        for busy in [false, true] {
            for draft in ["", "Fix the test", "first line\nsecond line"] {
                let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
                let snapshot = Arc::make_mut(&mut runtime.snapshot);
                snapshot.busy = busy;
                snapshot.capabilities.attachments = true;
                snapshot.capabilities.compact = true;
                runtime.replace_draft(text_editor::Content::with_text(draft));
                let mut element = runtime.view(
                    None,
                    false,
                    true,
                    &shortcuts,
                    width,
                    0.0,
                    Place { branch: None },
                    true,
                    None,
                    vec![],
                    true,
                );
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 700.0)),
                );
                let accessible = element.as_widget().a11y_nodes(
                    Layout::new(&node),
                    &tree,
                    iced::mouse::Cursor::Unavailable,
                );
                let controls: Vec<_> = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .filter_map(|node| {
                        let label = node.node().label()?;
                        [
                            "Attach files and more actions",
                            "Message",
                            "Send message",
                            "Cancel active turn",
                            "Steer the active turn",
                            "Queue for the next turn",
                        ]
                        .iter()
                        .any(|name| label.starts_with(name))
                        .then(|| (label.to_owned(), node.node().bounds().unwrap()))
                    })
                    .collect();
                let state = format!("{width}px busy={busy} draft={draft:?}");
                let find = |name: &str| {
                    controls
                        .iter()
                        .find(|(label, _)| label.starts_with(name))
                        .map(|(_, bounds)| *bounds)
                };
                let plus = find("Attach files and more actions")
                    .unwrap_or_else(|| panic!("{state}: `+` leads the row: {controls:?}"));
                let editor = controls
                    .iter()
                    .find(|(label, _)| label == "Message")
                    .map(|(_, bounds)| *bounds)
                    .unwrap_or_else(|| panic!("{state}: the draft: {controls:?}"));
                let send = find("Send message")
                    .or_else(|| find("Steer the active turn"))
                    .or_else(|| find("Queue for the next turn"));
                let stop = find("Cancel active turn");
                let trailing = match (busy, draft.is_empty()) {
                    (false, true) => {
                        assert!(stop.is_none(), "{state}: idle has nothing to stop");
                        assert!(
                            send.is_none(),
                            "{state}: an empty draft offers no send: {controls:?}"
                        );
                        None
                    }
                    (false, false) => {
                        assert!(stop.is_none(), "{state}: idle has nothing to stop");
                        Some(send.unwrap_or_else(|| panic!("{state}: Send: {controls:?}")))
                    }
                    (true, true) => {
                        assert!(
                            send.is_none(),
                            "{state}: an empty draft offers no send: {controls:?}"
                        );
                        Some(stop.unwrap_or_else(|| panic!("{state}: Stop: {controls:?}")))
                    }
                    (true, false) => {
                        let stop = stop.unwrap_or_else(|| {
                            panic!("{state}: Stop stays beside a follow-up: {controls:?}")
                        });
                        let send = send.unwrap_or_else(|| {
                            panic!("{state}: the follow-up can be sent: {controls:?}")
                        });
                        assert!(send.x1 <= stop.x0, "{state}: send precedes Stop");
                        Some(send)
                    }
                };
                // One row: `+` before the draft, the trailing controls
                // after it, and all beside the draft's last line.
                assert!(plus.x1 <= editor.x0, "{state}: {plus:?} {editor:?}");
                if let Some(trailing) = trailing {
                    assert!(editor.x1 <= trailing.x0, "{state}: {editor:?} {trailing:?}");
                }
                for control in std::iter::once(plus).chain(trailing) {
                    assert!(
                        control.y0 >= editor.y0 && control.y1 <= editor.y1,
                        "{state}: controls sit inside the draft's row: {control:?} {editor:?}"
                    );
                }
                if !draft.contains('\n') {
                    // A single line reserves no toolbar row beneath it.
                    assert!(
                        editor.y1 - editor.y0
                            <= f64::from(design::PRIMARY_CONTROL_SIZE + 2.0 * space::XS),
                        "{state}: {editor:?}"
                    );
                    let middle = (editor.y0 + editor.y1) / 2.0;
                    assert!(
                        std::iter::once(plus)
                            .chain(trailing)
                            .all(|control| ((control.y0 + control.y1) / 2.0 - middle).abs() < 1.0),
                        "{state}: controls center on a single line"
                    );
                }
            }
        }
    }
}

/// An agent started, a step, a thought and a report are each a line of the
/// timeline by the one rule, so the space between any two of them is the
/// one gap between lines, the same gap a group of steps puts between its
/// rows; only prose and an exchange's opening set themselves apart.
#[test]
fn every_timeline_row_is_a_line_by_the_one_rhythm_rule() {
    let item = |content| chat::Item {
        id: "item".into(),
        role: chat::Role::Assistant,
        content,
        started_at: None,
        at: None,
    };
    let started = item(ItemContent::Tool {
        name: "Agent".into(),
        input: serde_json::json!({
            "subagent_type": "general-purpose",
            "description": "Design",
            "prompt": "Draft a design",
        })
        .to_string(),
        output: String::new(),
        status: chat::ToolStatus::Succeeded,
    });
    let thought = item(ItemContent::Text {
        text: "**Weighing the drafts**".into(),
        thinking: true,
        phase: chat::Phase::Commentary,
    });
    let answer = item(ItemContent::Text {
        text: "Done.".into(),
        thinking: false,
        phase: chat::Phase::Final,
    });
    // A started agent is drawn as a card among the prose.
    assert_eq!(
        Rhythm::of(&started, turn::slot_of(&started), true),
        Rhythm::Line
    );
    assert_eq!(Rhythm::of(&started, turn::Slot::Step, false), Rhythm::Line);
    assert_eq!(
        Rhythm::of(&thought, turn::Slot::Thinking, false),
        Rhythm::Line
    );
    assert_eq!(Rhythm::of(&answer, turn::Slot::Prose, false), Rhythm::Block);
    assert_eq!(Rhythm::Line.gap(Rhythm::Line), step::GAP_XS);
    for above in [Rhythm::Line, Rhythm::Block, Rhythm::Opening] {
        assert_eq!(above.gap(Rhythm::Opening), step::GAP_L);
    }
    assert_eq!(Rhythm::Line.gap(Rhythm::Block), step::GAP_M);
    assert_eq!(Rhythm::Block.gap(Rhythm::Line), step::GAP_M);
    // Before it is measured, a line is taken to be one timeline row, and
    // the gap after it completes the pitch the layout test measures.
    assert_eq!(
        super::row_estimate(&started, Rhythm::Line, 0.0, 780.0) + step::GAP_XS,
        design::LINE_TRANSCRIPT + 3.0 * step::GAP_XS
    );
}

/// The sequence from #3119: four agents started, a thought with nothing to
/// read, a fifth agent, then the reply. Every row of the timeline sits the
/// same distance below the one before it, whether a hidden thought, a shown
/// one, a group boundary or nothing at all falls between them, and so do the
/// reports of agents that finished, though each opens an exchange; the reply
/// sits the same distance below the last row however it was reached; and the
/// rows' words, the agents' names, their states and the reply are set at one
/// size.
#[test]
fn a_timeline_keeps_one_pitch_and_one_size_down_to_the_reply() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Probe {
        texts: Vec<(String, iced::Rectangle)>,
        reply: Option<(iced::Rectangle, f32)>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            text: &str,
        ) {
            self.texts.push((text.to_owned(), bounds));
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(state) = state.downcast_ref::<selectable_prose::State>()
                && state.plain.as_ref() == REPLY
            {
                let (_, size) = state.mounted_typography().expect("the reply is laid out");
                self.reply = Some((bounds, size.0));
            }
        }
    }
    const REPLY: &str = "All five designs are ready.";
    let item = |id: &str, role, content| chat::Item {
        id: id.into(),
        role,
        content,
        started_at: None,
        at: None,
    };
    let text = |id: &str, role, text: &str, thinking| {
        item(
            id,
            role,
            ItemContent::Text {
                text: text.into(),
                thinking,
                phase: if thinking {
                    chat::Phase::Commentary
                } else {
                    chat::Phase::Final
                },
            },
        )
    };
    let started = |id: &str, name: &str| {
        item(
            id,
            chat::Role::Assistant,
            ItemContent::Tool {
                name: "Agent".into(),
                input: serde_json::json!({
                    "subagent_type": name,
                    "description": format!("Design {id}"),
                    "prompt": "Draft a design",
                })
                .to_string(),
                output: String::new(),
                status: chat::ToolStatus::Succeeded,
            },
        )
    };
    let command = item(
        "command",
        chat::Role::Assistant,
        ItemContent::Command {
            label: "Read the fixture".into(),
            command: "cat fixture".into(),
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: None,
            output: String::new(),
        },
    );
    // A second command, so that the open group is one: a lone step is
    // drawn as itself.
    let notes = item(
        "notes",
        chat::Role::Assistant,
        ItemContent::Command {
            label: "Read the notes".into(),
            command: "cat notes".into(),
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: None,
            output: String::new(),
        },
    );
    // What a finished background agent reports, which opens an exchange of
    // its own and is drawn as a row of the timeline.
    let finished = |n: usize| {
        item(
            &format!("report-{n}"),
            chat::Role::User,
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Report,
                from: String::new(),
                sender: String::new(),
                summary: format!("Agent \"Sidebar direction {n}\" finished"),
                body: "The design is drafted.".into(),
            },
        )
    };
    let sequence = |between: Vec<chat::Item>, reports: usize| {
        let mut items = vec![text(
            "prompt",
            chat::Role::User,
            "Draft five designs.",
            false,
        )];
        items.extend((1..=4).map(|n| started(&format!("agent-{n}"), "general-purpose")));
        items.extend(between);
        items.push(started("agent-5", "sol-design2"));
        items.extend((1..=reports).map(finished));
        items.push(text("reply", chat::Role::Assistant, REPLY, false));
        items
    };
    // A thought with nothing to read draws no row; one with a headline does.
    let thought = text("thought", chat::Role::Assistant, "", true);
    let headline = text(
        "headline",
        chat::Role::Assistant,
        "**Weighing the drafts**",
        true,
    );
    let scenarios = [
        (
            "a hidden thought between",
            sequence(vec![thought.clone()], 0),
            false,
            780.0,
        ),
        (
            "a thought between",
            sequence(vec![headline], 0),
            false,
            780.0,
        ),
        ("nothing between", sequence(Vec::new(), 0), false, 780.0),
        (
            "a group boundary",
            sequence(vec![command.clone()], 0),
            false,
            780.0,
        ),
        (
            "an open group",
            sequence(vec![command, notes], 0),
            true,
            780.0,
        ),
        (
            "agents reporting",
            sequence(vec![thought.clone()], 2),
            false,
            780.0,
        ),
        (
            "a narrow pane",
            sequence(vec![thought.clone()], 0),
            false,
            220.0,
        ),
        (
            "agents reporting in a narrow pane",
            sequence(vec![thought], 2),
            false,
            220.0,
        ),
    ];
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let width_at = |mut element: Element<'static, Message>| {
        let mut tree = Tree::new(element.as_widget());
        element
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1000.0, 100.0)),
            )
            .size()
            .width
    };
    let verb = width_at(design::transcript("Started").into());
    assert_ne!(
        verb,
        width_at(design::body("Started").into()),
        "the sizes differ"
    );
    let mut pitches = Vec::new();
    let mut reply_gaps = Vec::new();
    for (scenario, items, open, width) in scenarios {
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        runtime.tool_groups_expanded = open;
        let summary = runtime
            .turns
            .iter()
            .find(|turn| !turn.steps.is_empty())
            .map(|turn| turn.summary.clone());
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            width,
            0.0,
            Place { branch: None },
            true,
            None,
            Vec::new(),
            true,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 1400.0)),
        );
        let mut probe = Probe::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        // Each row of the timeline, by the words that start it.
        let rows: Vec<&(String, iced::Rectangle)> = probe
            .texts
            .iter()
            .filter(|(text, _)| {
                text == "Started"
                    || text.starts_with("Sidebar direction ")
                    || text.starts_with("Thought")
                    || text.starts_with("Thinking")
                    || text.starts_with("Ran cat ")
                    || text.starts_with("Weighing the drafts")
                    || summary.as_deref() == Some(text.as_str())
            })
            .collect();
        let expected =
            5 + usize::from(matches!(
                scenario,
                "a thought between" | "a group boundary" | "an open group"
            )) + 2 * usize::from(open)
                + 2 * usize::from(scenario.starts_with("agents reporting"));
        assert_eq!(rows.len(), expected, "{scenario}: {rows:?}");
        // A report names its agent as a row names any agent, without the
        // provider's "Agent" and quotes, and the name keeps its room: where
        // the pane is too narrow for it and the outcome both, the outcome
        // gives way, and so does a started agent's state.
        let narrow = width < 300.0;
        for (text, bounds) in &probe.texts {
            // Wide, every agent's name is whole; narrow, a report's still
            // is, its outcome having given way.
            if text.starts_with("Sidebar direction ")
                || !narrow && text.starts_with("Design agent-")
            {
                let natural = width_at(design::transcript_name(text.clone()).into());
                assert!(
                    bounds.width + 0.5 >= natural,
                    "{scenario}: {text:?} is cut to {} of {natural}",
                    bounds.width
                );
            }
        }
        let outcomes = |word: &str| probe.texts.iter().filter(|(text, _)| text == word).count();
        if scenario.starts_with("agents reporting") {
            assert_eq!(
                outcomes("finished"),
                if narrow { 0 } else { 2 },
                "{scenario}: the reports' outcome"
            );
        }
        if narrow {
            assert_eq!(
                outcomes("Finished"),
                0,
                "{scenario}: the started agents' state"
            );
        }
        let tops: Vec<f32> = rows.iter().map(|(_, bounds)| bounds.y).collect();
        for pair in tops.windows(2) {
            pitches.push((scenario, pair[1] - pair[0]));
        }
        // Every word in the timeline's rows - verbs, names, previews and
        // states - is set in the transcript's line box.
        let (first, last) = (tops[0], *tops.last().unwrap());
        for (text, bounds) in &probe.texts {
            if bounds.y >= first && bounds.y <= last && bounds.height > 0.0 {
                assert_eq!(
                    bounds.height,
                    design::LINE_TRANSCRIPT,
                    "{scenario}: {text:?} sits in the transcript's line box"
                );
            }
        }
        for (text, bounds) in &rows {
            if text == "Started" {
                assert_eq!(
                    bounds.width, verb,
                    "{scenario}: the verb is at transcript size"
                );
            }
        }
        let (reply, size) = probe.reply.expect("the reply is drawn");
        assert_eq!(
            size,
            design::TEXT_TRANSCRIPT,
            "{scenario}: the reply's size"
        );
        reply_gaps.push((scenario, reply.y - (last + design::LINE_TRANSCRIPT)));
    }
    let pitch = pitches[0].1;
    for (scenario, found) in &pitches {
        assert!(
            (found - pitch).abs() < 0.5,
            "{scenario}: rows {found} apart rather than {pitch}: {pitches:?}"
        );
    }
    assert_eq!(
        pitch,
        design::LINE_TRANSCRIPT + 2.0 * step::GAP_XS + step::GAP_XS,
        "a row's line box, its padding and the gap between rows"
    );
    for (scenario, gap) in &reply_gaps {
        assert_eq!(
            *gap,
            step::GAP_XS + step::GAP_M,
            "{scenario}: the reply sits a block's gap below the last row's padding"
        );
    }
}

/// The text an exchange's rows draw, every row mounted.
fn turn_texts(runtime: &Runtime) -> Vec<String> {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut element = runtime.find_turn_view_for_test();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 2000.0)),
    );
    let mut texts = Texts::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
    texts.0
}

/// A Claude `Bash` call as the adapter stores one.
fn bash(id: &str, description: &str, command: &str, output: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: ItemContent::Command {
            label: description.into(),
            command: command.into(),
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(3_000),
            output: output.into(),
        },
        started_at: Some(1_000),
        at: Some(4_000),
    }
}

/// The compound command from #3110 looked around the configuration
/// directory and then asked another device for its sessions. It is a
/// command, not "Listed live sessions"; the one invocation on its own is.
#[test]
fn a_compound_command_is_prepared_as_the_command_it_is() {
    let compound = "ls ~/.config/cagents/ 2>/dev/null; find ~/.config/cagents -maxdepth 2 -name '*.toml' 2>/dev/null; timeout 20 cagents --device fw16 --no-start sessions --json 2>&1 | head -50";
    let (runtime, _handle) = frame_cost::live_runtime(&[bash(
        "toolu_compound",
        "Check fw16 device availability in cagents",
        compound,
        "[]",
    )]);
    assert!(runtime.cache["toolu_compound"].coordination.is_none());
    let texts = turn_texts(&runtime);
    assert!(
        texts.iter().all(|text| !text.contains("live sessions")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("Ran ls ~/.config/cagents/")),
        "{texts:?}"
    );

    let (runtime, _handle) = frame_cost::live_runtime(&[bash(
        "toolu_sessions",
        "List fw16 sessions",
        "timeout 20 cagents --device fw16 --no-start sessions --json",
        "[]",
    )]);
    assert!(runtime.cache["toolu_sessions"].coordination.is_some());
    let texts = turn_texts(&runtime);
    assert!(
        texts.iter().any(|text| text.contains("live sessions")),
        "{texts:?}"
    );
}

/// A group of one step draws that step once, whether groups open by
/// default or the reader opened this one: no header above it repeating its
/// title and icon (#3110). Two steps keep their summary strip.
#[test]
fn a_group_of_one_step_draws_the_step_alone() {
    let item = bash("toolu_lone", "", "cargo check --workspace", "");
    for open in [false, true] {
        let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
        runtime.tool_groups_expanded = open;
        let texts = turn_texts(&runtime);
        let titles = texts
            .iter()
            .filter(|text| text.contains("cargo check --workspace"))
            .count();
        assert_eq!(titles, 1, "open={open}: {texts:?}");
    }
    let mut second = bash("toolu_second", "", "cargo test -q", "");
    second.started_at = Some(5_000);
    second.at = Some(6_000);
    let (runtime, _handle) = frame_cost::live_runtime(&[item, second]);
    let texts = turn_texts(&runtime);
    assert!(
        texts.iter().any(|text| text.starts_with("Ran 2 commands")),
        "{texts:?}"
    );
}

/// A lone step the reader opened stays open when a second step joins it:
/// the strip the two now share opens over it, and closing that step later
/// folds only the step. Closing a lone step returns its group to the
/// setting.
#[test]
fn a_group_opens_over_the_step_the_reader_opened_before_it_grew() {
    let first = bash("toolu_first", "", "cargo check --workspace", "Finished");
    let (runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&first));
    let target = runtime.target;
    let key = runtime.turns[0].key.clone();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let toggle = |app: &mut Cagents| {
        drop(app.handle_chat_ui(Action::Toggle(target, "toolu_first".into())));
    };

    toggle(&mut app);
    toggle(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(!runtime.item_expanded("toolu_first"));
    assert_eq!(runtime.opened.get(&key), None);

    toggle(&mut app);
    let mut second = bash("toolu_second", "", "cargo test -q", "");
    second.started_at = Some(5_000);
    second.at = Some(6_000);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.permission_mode = runtime.snapshot.permission_mode.clone();
    snapshot.generation += 1;
    snapshot.items.push(Arc::new(second));
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(runtime);
    let turn = runtime.turns[0].clone();
    assert_eq!((turn.key.as_str(), turn.steps.len()), (key.as_str(), 2));
    assert!(runtime.group_open(&turn));
    let texts = turn_texts(runtime);
    // The first step's detail, with its output, is open.
    for shown in ["Ran 2 commands", "Output"] {
        assert!(
            texts.iter().any(|text| text.contains(shown)),
            "{shown}: {texts:?}"
        );
    }

    toggle(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(!runtime.item_expanded("toolu_first"));
    assert!(runtime.group_open(&runtime.turns[0]));
    let texts = turn_texts(runtime);
    assert!(texts.iter().all(|text| text != "Output"), "{texts:?}");
    assert!(
        texts.iter().any(|text| text.contains("cargo test -q")),
        "{texts:?}"
    );
}

/// A coordinating step reads in the past tense, so one still running
/// without the spinner says it is working, as its group's strip did. The
/// spinner moves on to newer work once the agent writes again.
#[test]
fn a_lone_coordinating_step_still_running_says_so() {
    let mut item = bash("toolu_sessions", "", "cagents sessions", "");
    if let ItemContent::Command {
        status,
        exit_code,
        duration_ms,
        ..
    } = &mut item.content
    {
        *status = chat::ToolStatus::Running;
        *exit_code = None;
        *duration_ms = None;
    }
    item.at = None;
    let later = chat::Item {
        id: "later".into(),
        role: chat::Role::Assistant,
        at: Some(5_000),
        started_at: Some(5_000),
        content: ItemContent::Text {
            text: "Meanwhile, the build.".into(),
            thinking: false,
            phase: chat::Phase::Commentary,
        },
    };
    let (mut runtime, handle) = frame_cost::live_runtime(&[item, later]);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.permission_mode = runtime.snapshot.permission_mode.clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    assert!(runtime.cache["toolu_sessions"].coordination.is_some());
    assert_eq!(runtime.strip_status(&runtime.turns[0]), None);
    let texts = turn_texts(&runtime);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("live sessions · working")),
        "{texts:?}"
    );
}
