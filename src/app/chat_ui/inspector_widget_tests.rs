// Full libcosmic frames exercise the runtime's named-state extraction and the
// Inspector's actual modal wrapper. The fixture replaces provider I/O only;
// redraw, FlowRequest, shaping completion, retained diff and layout are real.
mod inspector_widget {
    use super::*;
    use iced::advanced::{
        Layout, Renderer as _, Shell, layout, mouse, renderer,
        widget::{Operation, Tree, operation, tree::NAMED},
    };
    use iced::futures::StreamExt;
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
        composer: Option<widget::Id>,
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
            _: Rectangle,
            _: &mut dyn operation::Focusable,
        ) {
            self.composer_mounted |= self.composer.is_some() && id == self.composer.as_ref();
        }
    }

    struct Frames {
        renderer: cosmic::Renderer,
        tree: Tree,
        executor: tokio::runtime::Runtime,
        shaped: usize,
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
            }
        }

        fn frame(&mut self, app: &mut Cagents, reader: &mut Reader) -> bool {
            let bounds = Size::new(app.window_width, app.window_height);
            let mut element = cosmic::ApplicationExt::view_main(app);
            NAMED.with(|named| *named.borrow_mut() = self.tree.take_all_named());
            self.tree.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut self.tree);
            let node = element.as_widget_mut().layout(
                &mut self.tree,
                &self.renderer,
                &layout::Limits::new(Size::ZERO, bounds),
            );
            NAMED.with(|named| named.borrow_mut().clear());
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
                let request = matches!(&message, Message::ChatUi(Action::FlowRequest(..)));
                let task = app.update(message);
                if !request {
                    drop(task);
                    continue;
                }
                shaped = true;
                let ready = self.executor.block_on(async {
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
                });
                self.shaped += 1;
                drop(app.update(ready));
            }
            invalid || shaped || updated
        }

        fn settle(&mut self, app: &mut Cagents, target: Target, wanted: Option<&Glyph>) -> Reader {
            for _ in 0..64 {
                let mut reader = Reader {
                    transcript: Some(card_control_id(target.card, "chat-transcript")),
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

    fn fixture() -> (Cagents, CardId, Target, String) {
        let mut app = inspector_paint_app();
        drop(app.update(Message::WindowResized(1280.0, 800.0)));
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
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
        show_chat_snapshot_for_test(
            &mut app,
            card,
            Snapshot {
                generation: 17,
                ready: true,
                session_id: Some(KNOWN_THREAD.into()),
                items,
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
                }
                let after = frames.settle(&mut app, target, glyph.as_ref());
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
