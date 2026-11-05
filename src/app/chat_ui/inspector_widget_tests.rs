// Full libcosmic frames exercise the runtime's named-state extraction and the
// Inspector's actual modal wrapper. The fixture replaces provider I/O only;
// redraw, FlowRequest, shaping completion, retained diff and layout are real.
mod inspector_widget {
    use super::*;
    use iced::advanced::{
        Layout, Renderer as _, Shell, layout, mouse, renderer,
        widget::{Operation, Tree, operation, tree::NAMED},
    };
    use iced::futures::{FutureExt, StreamExt};
    use iced::{Rectangle, Size, Vector};

    const DRAFT: &str = "Keep this exact unsent Inspector draft.";

    #[derive(Clone, Debug)]
    struct Glyph {
        identity: u64,
        byte: usize,
        text: String,
        bounds: Rectangle,
        viewport: Rectangle,
    }

    #[derive(Default)]
    struct Reader {
        transcript: Option<widget::Id>,
        workspace: Option<widget::Id>,
        composer: Option<widget::Id>,
        workspace_viewport: Option<Rectangle>,
        transcript_viewport: Option<Rectangle>,
        move_bottom: Option<f32>,
        translation: Vector,
        pending: Option<(Vector, Rectangle)>,
        clips: Vec<Rectangle>,
        wanted: Option<Glyph>,
        glyph: Option<Glyph>,
        top: Option<f32>,
        travel: f32,
        composer_mounted: bool,
        composer_bounds: Option<Rectangle>,
        details_mounted: bool,
    }

    impl Operation for Reader {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            let old = self.translation;
            let pushed = if let Some((translation, viewport)) = self.pending.take() {
                self.clips.push(Rectangle {
                    x: viewport.x - old.x,
                    y: viewport.y - old.y,
                    ..viewport
                });
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
            id: Option<&widget::Id>,
            viewport: Rectangle,
            content: Rectangle,
            translation: Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            if self.workspace.is_some() && id == self.workspace.as_ref() {
                self.workspace_viewport = Some(Rectangle {
                    x: viewport.x - self.translation.x,
                    y: viewport.y - self.translation.y,
                    ..viewport
                });
            }
            if id == self.transcript.as_ref() {
                self.transcript_viewport = Some(Rectangle {
                    x: viewport.x - self.translation.x,
                    y: viewport.y - self.translation.y,
                    ..viewport
                });
                self.top = Some(translation.y);
                self.travel = (content.height - viewport.height).max(0.0);
                if let Some(bottom) = self.move_bottom.take() {
                    state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(bottom),
                    });
                }
            }
            self.pending = Some((translation, viewport));
        }

        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            bounds: Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            let Some(prose) = state.downcast_ref::<selectable_prose::State>() else {
                return;
            };
            self.details_mounted |= prose.plain.contains("Provider is at capacity");
            let Some(logical) = &prose.logical else {
                return;
            };
            let Some(viewport) = self.transcript_viewport else {
                return;
            };
            let byte = if let Some(wanted) = &self.wanted {
                if wanted.identity != logical.identity {
                    return;
                }
                wanted.byte
            } else {
                if !prose.plain.starts_with("Reading checkpoint ") {
                    return;
                }
                if bounds.height <= 0.0 {
                    return;
                }
                let y = (viewport.y + 72.0 - bounds.y + self.translation.y)
                    .clamp(0.0, (bounds.height - 1.0).max(0.0));
                logical.start + prose.hit(iced::Point::new(bounds.width / 2.0, y))
            };
            let Some(local) = prose.logical_byte_bounds(logical.identity, byte) else {
                return;
            };
            let glyph = Rectangle {
                x: bounds.x + local.x - self.translation.x,
                y: bounds.y + local.y - self.translation.y,
                ..local
            };
            if self
                .clips
                .iter()
                .all(|clip| glyph.intersection(clip).is_some())
                && (self.wanted.is_some()
                    || self.glyph.as_ref().is_none_or(|current| {
                        (glyph.y - viewport.y - 72.0).abs()
                            < (current.bounds.y - current.viewport.y - 72.0).abs()
                    }))
            {
                self.glyph = Some(Glyph {
                    identity: logical.identity,
                    byte,
                    text: prose.plain.chars().take(65).collect(),
                    bounds: glyph,
                    viewport,
                });
            }
        }

        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            _: &mut dyn operation::Focusable,
        ) {
            self.composer_mounted |= self.composer.is_some() && id == self.composer.as_ref();
            if self.composer.is_some() && id == self.composer.as_ref() {
                self.composer_bounds = Some(Rectangle {
                    x: bounds.x - self.translation.x,
                    y: bounds.y - self.translation.y,
                    ..bounds
                });
            }
        }
    }

    struct Frames {
        renderer: cosmic::Renderer,
        tree: Tree,
        executor: tokio::runtime::Runtime,
        shaped: usize,
        drive_widget_tasks: bool,
        widget_tasks: Vec<Box<dyn Operation>>,
        widget_operations: usize,
        workspace_sample: Option<workspace_widget::ViewportSample>,
        workspace_samples: usize,
        clock: Instant,
    }

    impl Frames {
        fn new() -> Self {
            Self {
                renderer: crate::app::tests::headless_renderer(),
                tree: Tree::empty(),
                executor: tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
                shaped: 0,
                drive_widget_tasks: false,
                widget_tasks: vec![],
                widget_operations: 0,
                workspace_sample: None,
                workspace_samples: 0,
                clock: Instant::now(),
            }
        }

        fn with_widget_tasks() -> Self {
            Self {
                drive_widget_tasks: true,
                ..Self::new()
            }
        }

        fn complete_flow(&mut self, app: &mut Cagents, task: Task<Message>) -> Message {
            self.executor.block_on(async {
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
                        task = app.update(available.next().await.expect("Flow availability"));
                    }
                    panic!("actual Inspector Flow request did not complete");
                })
                .await
                .expect("bounded actual Inspector Flow completion")
            })
        }

        /// Drive the real camera/frame reducers and their immediate native
        /// effects. Provider start, resume and retirement tasks are replaced
        /// by explicit fixture completions before entering this frame loop.
        fn send(&mut self, app: &mut Cagents, message: Message) {
            assert!(self.drive_widget_tasks);
            let mut messages = std::collections::VecDeque::from([message]);
            for _ in 0..256 {
                let Some(message) = messages.pop_front() else {
                    return;
                };
                if let Message::Viewport {
                    event: WorkspaceViewportEvent::Scrolled(sample),
                    ..
                } = &message
                {
                    self.workspace_sample = Some(*sample);
                    self.workspace_samples += 1;
                }
                let flow = matches!(&message, Message::ChatUi(Action::FlowRequest(..)));
                let task = app.update(message);
                if flow {
                    let ready = self.complete_flow(app, task);
                    self.shaped += 1;
                    messages.push_back(ready);
                    continue;
                }
                let effects = self.executor.block_on(async {
                    let mut effects = vec![];
                    if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                        while let Some(Some(effect)) = stream.next().now_or_never() {
                            effects.push(effect);
                        }
                    }
                    effects
                });
                for effect in effects {
                    match effect {
                        iced::runtime::Action::Widget(operation) => {
                            self.widget_tasks.push(operation)
                        }
                        iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                            messages.push_back(message);
                        }
                        _ => {}
                    }
                }
            }
            panic!("actual camera callbacks did not quiesce");
        }

        fn frame(&mut self, app: &mut Cagents, reader: &mut Reader) -> bool {
            if self.drive_widget_tasks
                && (app.workspace.is_animating() || app.context_inspector_transition.is_some())
            {
                self.clock = self.clock.max(Instant::now()) + Duration::from_millis(16);
                self.send(app, Message::Frame(self.clock));
            }
            let bounds = Size::new(app.window_width, app.window_height);
            let mut element = cosmic::ApplicationExt::view_main(app);
            NAMED.with(|named| *named.borrow_mut() = self.tree.take_all_named());
            self.tree.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut self.tree);
            let mut node = element.as_widget_mut().layout(
                &mut self.tree,
                &self.renderer,
                &layout::Limits::new(Size::ZERO, bounds),
            );
            NAMED.with(|named| named.borrow_mut().clear());
            let widgets_applied = !self.widget_tasks.is_empty();
            for operation in std::mem::take(&mut self.widget_tasks) {
                let mut current = Some(operation);
                while let Some(mut operation) = current.take() {
                    element.as_widget_mut().operate(
                        &mut self.tree,
                        Layout::new(&node),
                        &self.renderer,
                        operation.as_mut(),
                    );
                    self.widget_operations += 1;
                    if let operation::Outcome::Chain(next) = operation.finish() {
                        current = Some(next);
                    }
                }
                node = element.as_widget_mut().layout(
                    &mut self.tree,
                    &self.renderer,
                    &layout::Limits::new(Size::ZERO, bounds),
                );
            }
            element.as_widget_mut().operate(
                &mut self.tree,
                Layout::new(&node),
                &self.renderer,
                reader,
            );
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut self.tree,
                &iced::Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &self.renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &Rectangle::with_size(bounds),
            );
            let invalid = shell.is_layout_invalid();
            self.renderer.reset(Rectangle::with_size(bounds));
            element.as_widget().draw(
                &self.tree,
                &mut self.renderer,
                &cosmic::theme::active(),
                &renderer::Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &Rectangle::with_size(bounds),
            );
            drop(element);
            let mut shaped = false;
            let mut updated = false;
            for message in messages {
                let cosmic::Action::App(message) = message else {
                    continue;
                };
                updated = true;
                if self.drive_widget_tasks {
                    self.send(app, message);
                    continue;
                }
                let request = matches!(&message, Message::ChatUi(Action::FlowRequest(..)));
                let task = app.update(message);
                if !request {
                    drop(task);
                    continue;
                }
                shaped = true;
                let ready = self.complete_flow(app, task);
                self.shaped += 1;
                drop(app.update(ready));
            }
            invalid
                || shaped
                || updated
                || widgets_applied
                || !self.widget_tasks.is_empty()
                || (self.drive_widget_tasks
                    && (app.workspace.is_animating() || app.context_inspector_transition.is_some()))
        }

        fn settle(&mut self, app: &mut Cagents, target: Target, wanted: Option<&Glyph>) -> Reader {
            for _ in 0..64 {
                let mut reader = Reader {
                    transcript: Some(card_control_id(target.card, "chat-transcript")),
                    workspace: self
                        .drive_widget_tasks
                        .then(|| app.workspace_scroll_id.clone()),
                    composer: Some(card_control_id(target.card, "chat-composer")),
                    wanted: wanted.cloned(),
                    ..Reader::default()
                };
                if !self.frame(app, &mut reader) {
                    return reader;
                }
            }
            panic!("actual Inspector redraw, shaping and layout did not settle");
        }
    }

    fn history_items() -> Vec<Arc<chat::Item>> {
        let mut items = Vec::new();
        for turn in 0..40 {
            items.push(Arc::new(chat::Item {
                id: format!("inspector-prompt-{turn}"),
                role: chat::Role::User,
                content: text(&format!("Review reading checkpoint {turn}.")),
                at: None,
                started_at: None,
            }));
            items.push(presentation_text(
                &format!("inspector-answer-{turn}"),
                &(0..8)
                    .map(|paragraph| {
                        format!(
                            "Reading checkpoint {turn:03}/{paragraph:02}: {}\n\n",
                            // Exceed prose::paragraph's 2048-byte Flow threshold.
                            "The copper bridge crosses the river beside the old station. "
                                .repeat(48)
                        )
                    })
                    .collect::<String>(),
            ));
        }
        items.push(Arc::new(chat::Item {
            id: "inspector-image-2".into(),
            role: chat::Role::User,
            content: ItemContent::Image {
                path: "/inspector-fixture/image-2.png".into(),
                description: "Image 2".into(),
                reference: Some(chat::ImageReference {
                    id: uuid::Uuid::new_v4(),
                    number: 2,
                    device: uuid::Uuid::new_v4(),
                    name: "image-2.png".into(),
                    sha256: "0".repeat(64),
                    extension: "png".into(),
                }),
            },
            at: None,
            started_at: None,
        }));
        items
    }

    fn fixture() -> (Cagents, CardId, Target, String) {
        let mut app = inspector_paint_app();
        drop(app.update(Message::WindowResized(1280.0, 800.0)));
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        show_chat_snapshot_for_test(
            &mut app,
            card,
            Snapshot {
                generation: 17,
                ready: true,
                session_id: Some(KNOWN_THREAD.into()),
                items: history_items(),
                error: Some("Provider is at capacity. Please try again later.".into()),
                ..Snapshot::default()
            },
        );
        // Image decoding is the fixture boundary. The actual image widget and
        // Image 2 identity participate in every native frame.
        let runtime = app.chat_runtime_mut(target).unwrap();
        let image = runtime.cache.get_mut("inspector-image-2").unwrap();
        image.image_source = None;
        image.image = Some(iced::widget::image::Handle::from_rgba(
            2,
            2,
            vec![80, 120, 160, 255].repeat(4),
        ));
        let failure = runtime.failure_ui.incidents.last().unwrap().key.clone();
        drop(app.update(Message::ChatUi(Action::ToggleFailure(
            target,
            failure.clone(),
        ))));
        write(&mut app, card, DRAFT);
        (app, card, target, failure)
    }

    fn row_anchor(app: &Cagents, card: CardId) -> (String, f32) {
        let (scroll, viewport, rows) = chat_runtime(app, card)
            .scroll_memory
            .inspector_fixture_geometry(None)
            .expect("actual native transcript layout reported geometry");
        let top = scroll.from_top();
        let (id, bounds) = rows
            .iter()
            .filter(|(id, bounds)| {
                id.starts_with("virtual-chunk-")
                    && bounds.y + bounds.height > top
                    && bounds.y < top + viewport.height
            })
            .min_by(|(_, a), (_, b)| a.y.total_cmp(&b.y))
            .or_else(|| {
                rows.iter().find(|(_, bounds)| {
                    bounds.y + bounds.height > top && bounds.y < top + viewport.height
                })
            })
            .expect("a visible stable reader row exists");
        (id.clone(), bounds.y - top)
    }

    fn assert_row_anchor(app: &Cagents, card: CardId, before: &(String, f32), stage: &str) {
        let (scroll, viewport, rows) = chat_runtime(app, card)
            .scroll_memory
            .inspector_fixture_geometry(None)
            .expect("actual native transcript layout reported geometry");
        let top = scroll.from_top();
        let bounds = rows
            .iter()
            .find_map(|(id, bounds)| (id == &before.0).then_some(*bounds))
            .unwrap_or_else(|| panic!("{stage}: reading row {} was evicted", before.0));
        assert!(
            bounds.y + bounds.height > top && bounds.y < top + viewport.height,
            "{stage}: original reader row {} left the viewport: top {top}, row {bounds:?}",
            before.0,
        );
        assert!(
            (bounds.y - top - before.1).abs() <= 28.0,
            "{stage}: reader row {} moved from {} to {} pixels within the viewport",
            before.0,
            before.1,
            bounds.y - top,
        );
    }

    fn assert_tail(app: &Cagents, card: CardId, stage: &str) {
        let (scroll, viewport, rows) = chat_runtime(app, card)
            .scroll_memory
            .inspector_fixture_geometry(None)
            .expect("actual native transcript layout reported geometry");
        assert!(
            ((scroll.content() - scroll.window_height()).max(0.0) - scroll.from_top()).abs() <= 1.0,
            "{stage}: tail reader left the actual native end: {scroll:?}"
        );
        let image = rows
            .iter()
            .find_map(|(id, bounds)| (id == "inspector-image-2").then_some(*bounds))
            .expect("the actual Image 2 row has measured geometry");
        assert!(
            image.y + image.height > scroll.from_top()
                && image.y < scroll.from_top() + viewport.height,
            "{stage}: Image 2 left the native reading window: {image:?}, {scroll:?}",
        );
    }

    fn ready_reopened_middle_fixture(
        frames: &mut Frames,
    ) -> (tempfile::TempDir, Cagents, [CardId; 3], Target) {
        let directory = tempfile::tempdir().unwrap();
        let mut app = inspector_paint_app();
        app.settings.default_native_pane_fraction = 1.0;
        drop(app.update(Message::WindowResized(1280.0, 800.0)));
        let mut original = vec![];
        for (index, session) in [
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            KNOWN_THREAD,
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        ]
        .into_iter()
        .enumerate()
        {
            if index == 0 {
                drop(app.new_native_chat(Provider::Codex, Some(directory.path().into())));
            } else {
                drop(app.update(Message::RunCommand(
                    workspace_widget::GlobalShortcut::NewThread,
                )));
            }
            let card = app.workspace.focused().unwrap();
            write(&mut app, card, "Open this fixture conversation.");
            submit(&mut app, card);
            let target = chat_runtime_attempt(&app, card);
            let handle = fixture_handle(&app, card, Some(session));
            answer_start(&mut app, target, Ok(handle));
            settle(&mut app, card, Ok(()));
            show_chat_snapshot_for_test(
                &mut app,
                card,
                Snapshot {
                    generation: 1,
                    ready: true,
                    session_id: Some(session.into()),
                    items: vec![presentation_text(
                        &format!("neighbor-{index}"),
                        "This neighboring conversation stays open.",
                    )],
                    ..Snapshot::default()
                },
            );
            original.push(card);
        }
        let [left, source, right] = original[..] else {
            unreachable!();
        };
        frames.send(&mut app, Message::FocusCard(source));
        let source_target = chat_runtime(&app, source).target;
        frames.settle(&mut app, source_target, None);

        // The UI close reducer and its exact retained-owner completion remove
        // the original card. No provider or daemon retirement task is polled.
        close_chat(&mut app, source);
        frames.send(&mut app, Message::FocusCard(left));
        let left_target = chat_runtime(&app, left).target;
        frames.settle(&mut app, left_target, None);
        drop(app.update(Message::RunCommand(
            workspace_widget::GlobalShortcut::ReopenPane,
        )));
        let primary = app.workspace.focused().unwrap();
        assert_ne!(primary, source, "reopen creates a new native card");
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            [left, primary, right]
        );
        let ready = ready_message_for_pending_switch(
            &app,
            primary,
            Snapshot {
                generation: 17,
                ready: true,
                session_id: Some(KNOWN_THREAD.into()),
                items: history_items(),
                ..Snapshot::default()
            },
        );
        // Replace only resume transport with the production Ready reducer.
        // Camera/frame/widget effects below run against the actual app Tree.
        drop(app.update(ready));
        let target = chat_runtime(&app, primary).target;
        frames.settle(&mut app, target, None);
        // The reopened card may initially be outside the old native viewport.
        // Publish its resident history after the real camera exposes it, just
        // as the visible client receives the provider's current snapshot.
        show_chat_snapshot_for_test(
            &mut app,
            primary,
            Snapshot {
                generation: 18,
                ready: true,
                session_id: Some(KNOWN_THREAD.into()),
                items: history_items(),
                ..Snapshot::default()
            },
        );
        write(&mut app, primary, DRAFT);
        assert!(chat_runtime(&app, primary).snapshot.ready);
        assert_eq!(app.workspace.focused_index(), Some(1));
        for card in [left, primary, right] {
            assert_eq!(app.card_state(card).unwrap().width_fraction, 1.0);
        }
        (directory, app, [left, primary, right], target)
    }

    fn assert_primary_native_camera(app: &Cagents, frames: &Frames, primary: CardId, stage: &str) {
        let sample = frames
            .workspace_sample
            .expect("the actual native strip publishes its measured scroll sample");
        let card = app.workspace.presentation_geometry().card(primary).unwrap();
        assert_eq!(app.workspace.focused(), Some(primary));
        assert_eq!(app.workspace.focused_index(), Some(1));
        assert!(!app.workspace.is_animating(), "{stage}: the camera settled");
        assert!(
            sample.offset > 1.0,
            "{stage}: the active middle card needs native scrolling"
        );
        assert_eq!(sample.width, app.workspace.viewport_width());
        assert!(
            (sample.offset - app.workspace.camera().offset()).abs() <= 0.5,
            "{stage}: measured native offset {} disagrees with model camera {}",
            sample.offset,
            app.workspace.camera().offset(),
        );
        assert!(
            card.start - sample.offset >= -1.0 && card.end - sample.offset <= sample.width + 1.0,
            "{stage}: active primary is clipped in the actual native window: card {card:?}, sample {sample:?}",
        );
    }

    fn assert_primary_controls_visible(reader: &Reader, stage: &str) {
        let workspace = reader
            .workspace_viewport
            .expect("actual native workspace viewport");
        let transcript = reader
            .transcript_viewport
            .expect("actual primary transcript viewport");
        let composer = reader.composer_bounds.expect("actual primary draft editor");
        for (name, bounds) in [("transcript", transcript), ("composer", composer)] {
            assert!(
                bounds.x >= workspace.x - 1.0
                    && bounds.x + bounds.width <= workspace.x + workspace.width + 1.0
                    && bounds.y >= workspace.y - 1.0
                    && bounds.y + bounds.height <= workspace.y + workspace.height + 1.0,
                "{stage}: primary {name} is clipped: {bounds:?}, workspace {workspace:?}",
            );
        }
    }

    #[test]
    fn inspector_widget_keeps_reopened_ready_middle_card_in_the_native_viewport() {
        let mut frames = Frames::with_widget_tasks();
        let (_directory, mut app, cards, target) = ready_reopened_middle_fixture(&mut frames);
        let primary = cards[1];
        frames.settle(&mut app, target, None);
        let mut movement = Reader {
            transcript: Some(card_control_id(primary, "chat-transcript")),
            move_bottom: Some(1450.0),
            ..Reader::default()
        };
        frames.frame(&mut app, &mut movement);
        let before = frames.settle(&mut app, target, None);
        assert_primary_native_camera(&app, &frames, primary, "initial docked Inspector");
        assert_primary_controls_visible(&before, "initial docked Inspector");
        let initial_width = before.transcript_viewport.unwrap().width;
        let glyph = before
            .glyph
            .expect("the Ready primary has actual shaped reading glyphs");
        let anchor = row_anchor(&app, primary);
        assert!(
            frames.shaped > 0,
            "the Ready reading point uses actual Flow completions"
        );
        let operations_before = frames.widget_operations;
        let samples_before = frames.workspace_samples;

        frames.send(&mut app, Message::WindowResized(960.0, 800.0));
        assert!(app.context_inspector_is_overlay());
        let obscured = frames.settle(&mut app, target, Some(&glyph));
        assert!(
            obscured.top.is_none(),
            "generic modal operations stay foreground only"
        );
        assert!(obscured.composer_bounds.is_none());
        assert_primary_native_camera(&app, &frames, primary, "settled narrow Inspector overlay");
        assert_row_anchor(
            &app,
            primary,
            &anchor,
            "settled narrow Ready Inspector overlay",
        );
        assert!(frames.widget_operations > operations_before);
        assert!(frames.workspace_samples > samples_before);
        assert_eq!(chat_runtime(&app, primary).composer.text(), DRAFT);

        frames.send(&mut app, Message::ContextInspector(InspectorAction::Close));
        for stage in [
            "collapsed narrow Ready Inspector",
            "redocked wide Ready Inspector",
        ] {
            if stage == "redocked wide Ready Inspector" {
                frames.send(&mut app, Message::ToggleContextInspector);
                frames.send(&mut app, Message::WindowResized(1280.0, 800.0));
            }
            let after = frames.settle(&mut app, target, Some(&glyph));
            assert!(app.context_inspector_transition.is_none());
            assert_primary_native_camera(&app, &frames, primary, stage);
            assert_primary_controls_visible(&after, stage);
            assert_row_anchor(&app, primary, &anchor, stage);
            let actual = after.glyph.as_ref().unwrap_or_else(|| {
                panic!("{stage}: primary reading glyph disappeared: {}", glyph.text)
            });
            assert!(
                ((actual.bounds.y - actual.viewport.y) - (glyph.bounds.y - glyph.viewport.y)).abs()
                    <= 56.0,
                "{stage}: primary reading glyph moved from {glyph:?} to {actual:?}",
            );
            assert_eq!(chat_runtime(&app, primary).composer.text(), DRAFT);
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), cards);
            if stage == "redocked wide Ready Inspector" {
                assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
                assert_eq!(after.transcript_viewport.unwrap().width, initial_width);
            }
        }
    }

    #[test]
    fn inspector_widget_preserves_real_reader_through_overlay_collapse_and_redock() {
        for bottom in [0.0, 1450.0] {
            let (mut app, card, target, failure) = fixture();
            let mut frames = Frames::new();
            frames.settle(&mut app, target, None);
            let mut move_reader = Reader {
                transcript: Some(card_control_id(card, "chat-transcript")),
                move_bottom: Some(bottom),
                ..Reader::default()
            };
            frames.frame(&mut app, &mut move_reader);
            let before = frames.settle(&mut app, target, None);
            let initial_width = before
                .transcript_viewport
                .expect("the initial reader has a measured viewport")
                .width;
            assert!(before.travel > 1500.0, "fixture has a real long transcript");
            assert!(
                before.composer_mounted,
                "the actual draft editor is mounted"
            );
            let glyph = if bottom > 0.0 {
                Some(
                    before
                        .glyph
                        .expect("natural reading paragraph has real shaped glyphs"),
                )
            } else {
                assert!(
                    before.details_mounted,
                    "expanded capacity Details has mounted prose"
                );
                None
            };
            let anchor = row_anchor(&app, card);
            // The tail can contain only Image 2 and static diagnostic text.
            // The natural paragraph case must exercise asynchronous Flow.
            if bottom > 0.0 {
                assert!(frames.shaped > 0, "actual redraw completed Flow shaping");
            }

            drop(app.update(Message::WindowResized(960.0, 800.0)));
            assert!(app.context_inspector_is_overlay());
            let mut obscured = Reader::default();
            frames.frame(&mut app, &mut obscured);
            assert!(
                obscured.top.is_none(),
                "modal operations remain foreground only"
            );
            if bottom > 0.0 {
                assert_row_anchor(&app, card, &anchor, "immediate narrow overlay");
            } else {
                assert_tail(&app, card, "immediate narrow overlay");
            }
            drop(
                app.advance_context_inspector_transition(
                    Instant::now() + DIALOG_TRANSITION_DURATION,
                ),
            );
            frames.settle(&mut app, target, None);
            if bottom > 0.0 {
                assert_row_anchor(&app, card, &anchor, "settled narrow overlay");
            } else {
                assert_tail(&app, card, "settled narrow overlay");
            }

            inspector_paint_close(&mut app);
            for stage in ["collapsed narrow Inspector", "redocked wide Inspector"] {
                if stage == "redocked wide Inspector" {
                    drop(app.update(Message::ToggleContextInspector));
                    drop(app.update(Message::WindowResized(1280.0, 800.0)));
                    assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
                    drop(app.update(Message::Frame(Instant::now() + DIALOG_TRANSITION_DURATION)));
                    assert!(app.context_inspector_transition.is_none());
                    assert_eq!(app.context_inspector_progress(), 1.0);
                }
                let after = frames.settle(&mut app, target, glyph.as_ref());
                if stage == "redocked wide Inspector" {
                    assert_eq!(
                        after.transcript_viewport.unwrap().width,
                        initial_width,
                        "settled redock must restore the original reader width"
                    );
                }
                assert!(
                    after.composer_mounted,
                    "{stage}: the actual draft editor is mounted"
                );
                if let Some(glyph) = &glyph {
                    assert_row_anchor(&app, card, &anchor, stage);
                    let actual = after.glyph.as_ref().unwrap_or_else(|| {
                        panic!("{stage}: natural paragraph disappeared: {}", glyph.text)
                    });
                    assert!(
                        ((actual.bounds.y - actual.viewport.y)
                            - (glyph.bounds.y - glyph.viewport.y))
                            .abs()
                            <= 56.0,
                        "{stage}: natural reading glyph moved from {glyph:?} to {actual:?}"
                    );
                } else {
                    assert_tail(&app, card, stage);
                    assert!(
                        after.details_mounted,
                        "{stage}: expanded capacity Details has mounted prose"
                    );
                }
                let runtime = chat_runtime(&app, card);
                assert_eq!(runtime.composer.text(), DRAFT);
                assert!(runtime.failure_ui.expanded.contains(&failure));
            }
        }
    }
}
