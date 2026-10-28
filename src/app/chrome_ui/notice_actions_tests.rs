#[derive(Clone, Copy, Debug)]
struct NoticeScrollViewport {
    bounds: iced::Rectangle,
    content: iced::Rectangle,
    translation: iced::Vector,
}

struct NoticeOverlayProbe {
    mounted: Vec<(String, iced::Rectangle)>,
    focused: Vec<String>,
    scrolls: Vec<NoticeScrollViewport>,
}

fn notice_overlay_controls(
    app: &Cagents,
    tree: &mut iced::advanced::widget::Tree,
    renderer: &cosmic::Renderer,
    task: Task<Message>,
) -> (Vec<(String, iced::Rectangle)>, Vec<String>) {
    let probe = notice_overlay_probe(app, tree, renderer, task);
    (probe.mounted, probe.focused)
}

/// Mount the real notices overlay, not only its header anchor. COSMIC stores
/// popover buttons in an overlay tree, so base-widget operations miss them.
fn notice_overlay_probe(
    app: &Cagents,
    tree: &mut iced::advanced::widget::Tree,
    renderer: &cosmic::Renderer,
    task: Task<Message>,
) -> NoticeOverlayProbe {
    use iced::advanced::{
        Layout, layout,
        widget::{
            Operation,
            operation::{Focusable, Outcome},
        },
    };
    use iced::{Rectangle, Size, futures::StreamExt};
    struct Controls {
        mounted: Vec<(String, Rectangle)>,
        focused: Vec<String>,
        scrolls: Vec<NoticeScrollViewport>,
    }
    impl Operation for Controls {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            _: Option<&widget::Id>,
            bounds: Rectangle,
            content: Rectangle,
            translation: iced::Vector,
            _: &mut dyn iced::advanced::widget::operation::Scrollable,
        ) {
            // This fixture mounts one actual popover body scroll. Collect it
            // before and after the production stable ID exists, so the causal
            // failure is invisible keyboard focus, not a missing new ID.
            self.scrolls.push(NoticeScrollViewport {
                bounds,
                content,
                translation,
            });
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            state: &mut dyn Focusable,
        ) {
            if let Some(id) = id {
                self.mounted.push((id.to_string(), bounds));
                if state.is_focused() {
                    self.focused.push(id.to_string());
                }
            }
        }
    }
    let mut element = notice_header(app);
    tree.diff(element.as_widget_mut());
    let viewport = Rectangle::with_size(Size::new(1000.0, 1600.0));
    let node = element.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let mut overlay = element
        .as_widget_mut()
        .overlay(
            tree,
            Layout::new(&node),
            renderer,
            &viewport,
            iced::Vector::ZERO,
        )
        .expect("the actual open notices popover is mounted");
    let node = overlay.as_overlay_mut().layout(renderer, viewport.size());
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let Some(mut stream) = iced::runtime::task::into_stream(task) else {
            return;
        };
        while let Ok(Some(action)) =
            tokio::time::timeout(Duration::from_millis(250), stream.next()).await
        {
            if let iced::runtime::Action::Widget(operation) = action {
                let mut next = Some(operation);
                while let Some(mut operation) = next.take() {
                    overlay.as_overlay_mut().operate(
                        Layout::new(&node),
                        renderer,
                        operation.as_mut(),
                    );
                    if let Outcome::Chain(chained) = operation.finish() {
                        next = Some(chained);
                    }
                }
            }
        }
    });
    executor.shutdown_background();
    let mut controls = Controls {
        mounted: Vec::new(),
        focused: Vec::new(),
        scrolls: Vec::new(),
    };
    overlay
        .as_overlay_mut()
        .operate(Layout::new(&node), renderer, &mut controls);
    NoticeOverlayProbe {
        mounted: controls.mounted,
        focused: controls.focused,
        scrolls: controls.scrolls,
    }
}

#[test]
fn notices_tab_traversal_reaches_the_mounted_open_details_and_dismiss_controls_in_order() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let mut extra_older = Vec::new();
    for index in 0..3 {
        app.notices
            .record(TransientNotice::warning(format!("Older warning {index}")));
        extra_older.push(app.notices.recent().next().unwrap().id);
    }
    app.notices
        .record(TransientNotice::warning("An unrelated warning"));
    let older = app.notices.recent().next().unwrap().id;
    app.notices.record(
        TransientNotice::error("Could not attach this terminal")
            .with_key(NoticeKey::ChatAttach(card))
            .with_details(
                (0..80)
                    .map(|line| format!("Exact saved-chat diagnostic line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
    );
    let id = app.notices.recent().next().unwrap().id;
    let open = format!("chrome.notices.open.{id}");
    let details = format!("chrome.notices.details.{id}");
    let mut expected = vec![
        NOTICES_DISMISS_ALL_ID.to_owned(),
        NOTICES_CLOSE_ID.to_owned(),
        open,
        details,
        notice_dismiss_id(id),
        notice_dismiss_id(older),
    ];
    expected.extend(extra_older.into_iter().rev().map(notice_dismiss_id));
    let task = app.update_chrome(Action::ToggleNotices);
    drop(app.update(Message::ToggleNoticeDetails(id)));
    let renderer = super::super::tests::headless_renderer();
    let mut tree = iced::advanced::widget::Tree::new(notice_header(&app).as_widget());
    let probe = notice_overlay_probe(&app, &mut tree, &renderer, task);
    assert_eq!(
        probe
            .mounted
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        app.popup_focus_ring(Popup::Notices)
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(probe.focused, [NOTICES_CLOSE_ID]);
    let [scroll] = probe.scrolls.as_slice() else {
        panic!("the actual notices overlay has one scrollable body");
    };
    assert!(scroll.content.height > scroll.bounds.height + 100.0);
    let mut previous_offset = scroll.translation.y;
    let mut saw_scrolled_body = false;
    for backwards in [false, true] {
        let tab = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: if backwards {
                keyboard::Modifiers::SHIFT
            } else {
                keyboard::Modifiers::empty()
            },
            text: None,
            repeat: false,
        };
        let mut index = 1;
        for _ in 0..expected.len() {
            index = if backwards {
                (index + expected.len() - 1) % expected.len()
            } else {
                (index + 1) % expected.len()
            };
            let task = app
                .chrome_key_event(&tab)
                .expect("the open popover owns Tab");
            let probe = notice_overlay_probe(&app, &mut tree, &renderer, task);
            assert_eq!(probe.focused, [expected[index].clone()]);
            let [scroll] = probe.scrolls.as_slice() else {
                panic!("the actual notices overlay has one scrollable body");
            };
            let raw = probe
                .mounted
                .iter()
                .find(|(id, _)| id == &expected[index])
                .unwrap()
                .1;
            if index < 2 {
                // Header actions are outside the body. Visiting them must not
                // discard the reader's place in the long diagnostic/history.
                assert!(
                    (scroll.translation.y - previous_offset).abs() < 0.1,
                    "{backwards}: header focus changed reader offset {previous_offset} to {}",
                    scroll.translation.y
                );
                assert!(raw.y >= 0.0 && raw.y + raw.height <= 1600.0);
            } else {
                let visible = iced::Rectangle {
                    x: raw.x - scroll.translation.x,
                    y: raw.y - scroll.translation.y,
                    ..raw
                };
                assert!(
                    visible.x >= scroll.bounds.x - 0.5
                        && visible.x + visible.width <= scroll.bounds.x + scroll.bounds.width + 0.5
                        && visible.y >= scroll.bounds.y - 0.5
                        && visible.y + visible.height
                            <= scroll.bounds.y + scroll.bounds.height + 0.5,
                    "{} is focused but outside the actual body viewport: raw={raw:?}, visible={visible:?}, scroll={scroll:?}",
                    expected[index]
                );
                saw_scrolled_body |= scroll.translation.y > 1.0;
            }
            previous_offset = scroll.translation.y;
        }
    }
    assert!(
        saw_scrolled_body,
        "keyboard traversal must reveal controls below the long diagnostic"
    );
}

#[test]
fn notice_details_keep_full_reason_and_expansion_only_for_the_same_report_identity() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let reason = "First detail\nSecond detail\nThird detail\nFourth detail\nFifth detail\nLast exact diagnostic λ";
    let notice = TransientNotice::error("Could not attach terminal")
        .with_key(NoticeKey::ChatAttach(card))
        .with_details(reason);
    app.notices.record(notice.clone());
    let id = app.notices.recent().next().unwrap().id;
    drop(app.update(Message::ToggleNoticeDetails(id)));
    assert!(!app.notices.recent().next().unwrap().details_open);
    drop(app.update_chrome(Action::ToggleNotices));
    drop(app.update(Message::ToggleNoticeDetails(id)));
    assert!(app.notices.recent().next().unwrap().details_open);
    app.notices
        .record(TransientNotice::warning("Interleaved warning"));
    app.notices.record(notice);
    let repeated = app.notices.recent().next().unwrap();
    assert_eq!(repeated.id, id);
    assert_eq!(repeated.repeats, 2);
    assert!(repeated.details_open);
    assert_eq!(repeated.notice.details.as_deref(), Some(reason));
    assert!(!app.notices.recent().last().unwrap().details_open);
    app.notices.record(
        TransientNotice::error("A different attach outcome")
            .with_key(NoticeKey::ChatAttach(card))
            .with_details("New exact reason"),
    );
    let changed = app.notices.recent().next().unwrap().id;
    assert_ne!(changed, id);
    assert!(!app.notices.recent().next().unwrap().details_open);
    drop(app.update(Message::ToggleNoticeDetails(id)));
    drop(app.update(Message::DismissNotice(id)));
    assert_eq!(app.notices.recent().next().unwrap().id, changed);
    assert!(!app.notices.recent().next().unwrap().details_open);
    app.settings_reset_confirm = true;
    drop(app.update(Message::ToggleNoticeDetails(changed)));
    assert!(!app.notices.recent().next().unwrap().details_open);
    app.settings_reset_confirm = false;
    drop(app.update_chrome(Action::Close));
    drop(app.update(Message::ToggleNoticeDetails(changed)));
    assert!(!app.notices.recent().next().unwrap().details_open);
    drop(app.update_chrome(Action::ToggleNotices));
    drop(app.update(Message::ToggleNoticeDetails(changed)));
    assert!(app.notices.recent().next().unwrap().details_open);
    drop(app.update(Message::ToggleNoticeDetails(changed)));
    assert!(!app.notices.recent().next().unwrap().details_open);
}

#[test]
fn expanding_notice_details_draws_beyond_the_four_line_preview_and_can_collapse_again() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.notices
        .record(TransientNotice::warning("Notice below the long diagnostic"));
    let below = app.notices.recent().next().unwrap().id;
    let reason = (0..20)
        .map(|line| format!("Exact diagnostic line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.notices.record(
        TransientNotice::error("Could not attach terminal")
            .with_key(NoticeKey::ChatAttach(card))
            .with_details(reason.clone()),
    );
    let id = app.notices.recent().next().unwrap().id;
    drop(app.update_chrome(Action::ToggleNotices));
    let renderer = super::super::tests::headless_renderer();
    let mut tree = iced::advanced::widget::Tree::new(notice_header(&app).as_widget());
    let (collapsed, _) = notice_overlay_controls(&app, &mut tree, &renderer, Task::none());
    let below_id = notice_dismiss_id(below);
    let collapsed_y = collapsed
        .iter()
        .find(|(id, _)| id == &below_id)
        .unwrap()
        .1
        .y;
    drop(app.update(Message::ToggleNoticeDetails(id)));
    let (expanded, _) = notice_overlay_controls(&app, &mut tree, &renderer, Task::none());
    let expanded_y = expanded.iter().find(|(id, _)| id == &below_id).unwrap().1.y;
    assert!(
        expanded_y > collapsed_y + 100.0,
        "the full twenty-line reason must extend beyond the four-line preview"
    );
    assert_eq!(
        app.notices
            .recent()
            .next()
            .unwrap()
            .notice
            .details
            .as_deref(),
        Some(reason.as_str())
    );
    drop(app.update(Message::ToggleNoticeDetails(id)));
    let (collapsed_again, _) = notice_overlay_controls(&app, &mut tree, &renderer, Task::none());
    let collapsed_again_y = collapsed_again
        .iter()
        .find(|(id, _)| id == &below_id)
        .unwrap()
        .1
        .y;
    assert!((collapsed_again_y - collapsed_y).abs() < 0.1);
}
