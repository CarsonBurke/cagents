//! Cached shared descriptors can present saved text, but cannot start providers.
use super::*;

impl Runtime {
    pub(super) fn awaiting_admission(&self) -> bool {
        matches!(self.connection, Connection::AwaitingAdmission)
    }

    pub(in crate::app) fn pending_admission_guidance(&self) -> &'static str {
        if !self.unsent_attachments().is_empty() {
            "This saved pane has not been reopened. Reconnect the workspace to reopen this pane with its saved attachments. Copying text or opening conversation history will not include these attachments."
        } else if self.session.launch.session_id.is_some() {
            "This saved pane has not been reopened. Reconnect the workspace to reopen it, or open the conversation from history. Saved text stays in this pane."
        } else {
            "This saved pane has not been reopened. Reconnect the workspace to reopen it. You can copy saved text into a new conversation."
        }
    }

    pub(super) fn pending_admission_view(&self) -> Element<'_, Message> {
        let text = self.unsent_text();
        let mut actions = vec![design::heading("Saved draft").into()];
        if !text.is_empty() {
            actions.push(
                widget::button::custom(design::body("Copy saved text"))
                    .class(crate::ui_controls::control_style(false, None))
                    .padding([space::S as u16, space::M as u16])
                    .on_press(Message::ChatUi(Action::CopyLiteral(text.clone())))
                    .into(),
            );
        }
        let header = widget::flex_row(actions)
            .align_items(Alignment::Center)
            .spacing(space::M as u16);
        let mut content = vec![
            header.into(),
            design::body(self.pending_admission_guidance()).into(),
        ];
        if !self.unsent_attachments().is_empty() {
            content.push(design::body("Saved attachments stay in this pane.").into());
        }
        if !text.is_empty() {
            let draft = widget::container(view::body(text))
                .width(Length::Fill)
                .padding(space::M)
                .class(design::surface(
                    Some(design::sunken),
                    [design::RADIUS_M; 4],
                    false,
                ));
            content.push(widget::scrollable(draft).height(Length::Fill).into());
        }
        let content = widget::column::with_children(content)
            .spacing(space::M)
            .width(Length::Fill);
        widget::container(widget::container(content).max_width(design::READABLE_MEASURE))
            .padding(space::L)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

impl Cagents {
    pub(in crate::app) fn chat_awaiting_admission(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.awaiting_admission())
    }

    pub(in crate::app) fn await_shared_chat_admission(
        &mut self,
        card: CardId,
        session: chat::Session,
    ) {
        Arc::make_mut(&mut self.durable)
            .pending_chat_admissions
            .insert(card, session.clone());
        let target = self.chat_ui.next_target(card);
        let visible = self.workspace.visibility(card) == Some(VisibilityClass::Visible);
        let mut runtime = Runtime::new(
            target,
            session,
            Connection::AwaitingAdmission,
            visible,
            self.settings.editor_syntax_theme,
        );
        if let Some(text) = self.chat_ui.drafts.remove(&card) {
            runtime.replace_draft(text_editor::Content::with_text(&text));
        }
        if let Some(images) = self.chat_ui.draft_attachments.remove(&card) {
            runtime.replace_attachments(
                images
                    .into_iter()
                    .filter(|image| image.reference.is_some())
                    .collect(),
            );
        }
        // Deliberately no provider connection, draft skill discovery, retirement,
        // or lifecycle job: this is only a local view of saved bytes.
        self.cards
            .insert(card, CardRuntime::Chat(Box::new(runtime)));
    }

    fn checkpoint_retained_chat_copy(&self, source_card: CardId, copy: CardId) -> Task<Message> {
        let Some(writer) = &self.session else {
            return Task::none();
        };
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&copy) else {
            return Task::none();
        };
        let payload = crate::persistence::RetainedChatCopy {
            workspace: self.durable.id,
            source_card,
            source: runtime.session.clone(),
            copy: self.card_state(copy).expect("retained card exists").clone(),
            draft: runtime.unsent_text(),
            attachments: runtime
                .unsent_attachments()
                .iter()
                .filter(|image| image.reference.is_some())
                .cloned()
                .collect(),
            scope: self.durable.card_scopes.get(&copy).cloned(),
            channel: self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&copy))
                .map(|channel| channel.id),
        };
        let receipt = writer.preserve_chat_copy(payload);
        cosmic::task::future(async move {
            let result = match receipt {
                Ok(receipt) => tokio::task::spawn_blocking(move || {
                    receipt
                        .recv()
                        .map_err(|error| format!("Session writer disconnected: {error}"))?
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result),
                Err(error) => Err(error),
            };
            Message::ChatUi(Action::RetainedCopyCommitted(copy, result))
        })
    }

    /// A newer projection cannot reinterpret an in-flight source's Content.
    /// Strict upgrade connections only attach, so forgetting their generation
    /// detaches a late handle rather than retiring a daemon-owned actor.
    pub(in crate::app) fn preserve_pending_shared_upgrade(
        &mut self,
        card: CardId,
    ) -> (Task<Message>, Option<CardId>) {
        if !self
            .chat_ui
            .attempt(card)
            .is_some_and(|attempt| attempt.upgrade_admission.is_some())
        {
            return (Task::none(), None);
        }
        self.chat_ui.phases.remove(&card);
        self.retain_shared_upgrade_source(card)
    }

    pub(in crate::app) fn retain_shared_upgrade_source(
        &mut self,
        card: CardId,
    ) -> (Task<Message>, Option<CardId>) {
        let Some(pane) = self
            .card_state(card)
            .filter(|state| state.existence.is_open())
            .and_then(|state| {
                crate::workspace_model::views::Pane::from_card(
                    state,
                    self.durable.card_scopes.get(&card).cloned(),
                )
            })
        else {
            return (Task::none(), None);
        };
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return (Task::none(), None);
        };
        runtime.connection = Connection::AwaitingAdmission;
        self.retain_unresolved_shared_upgrade(&pane)
    }

    pub(in crate::app) fn shared_upgrade_target_changed(
        &self,
        pane: &crate::workspace_model::views::Pane,
    ) -> bool {
        self.chat_ui
            .attempt(pane.id)
            .and_then(|attempt| attempt.upgrade_admission.as_ref())
            .is_some_and(
                |canonical| !matches!(&pane.kind, CardKind::Chat(session) if session == canonical),
            )
    }

    /// An authoritative upgrade can arrive before or during an ordinary
    /// reconnect. Keep the live Content owner, but admit the canonical target
    /// through the same exact lineage and durable checkpoint as a cached pane.
    pub(in crate::app) fn prepare_shared_chat_upgrade(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
    ) {
        let CardKind::Chat(canonical) = &pane.kind else {
            return;
        };
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&pane.id) else {
            return;
        };
        if runtime.awaiting_admission()
            || !chat::client::UpgradeRecovery::is_candidate(&runtime.session, canonical)
            || self.chat_ui.phases.get(&pane.id).is_some_and(|phase| {
                !matches!(phase, Phase::Starting(attempt)
                    if attempt.session == runtime.session
                        && attempt.recovery.is_none()
                        && attempt.upgrade_admission.is_none())
            })
        {
            return;
        }
        let source = runtime.session.clone();
        // This only supersedes an ordinary attachment to this exact source.
        // Its late Ready detaches; it must not retire the shared actor or send
        // the unsent Content. Returning/closing/account recovery keep ownership.
        self.chat_ui.phases.remove(&pane.id);
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&pane.id) {
            runtime.connection = Connection::AwaitingAdmission;
        }
        Arc::make_mut(&mut self.durable)
            .pending_chat_admissions
            .insert(pane.id, source);
    }

    pub(in crate::app) fn admit_shared_chat(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
    ) -> (Task<Message>, Option<CardId>) {
        self.admit_shared_chat_inner(pane, true)
    }

    pub(in crate::app) fn retain_unresolved_shared_upgrade(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
    ) -> (Task<Message>, Option<CardId>) {
        self.admit_shared_chat_inner(pane, false)
    }

    fn admit_shared_chat_inner(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
        resolve_upgrade: bool,
    ) -> (Task<Message>, Option<CardId>) {
        let Some(CardRuntime::Chat(mut runtime)) = self.cards.remove(&pane.id) else {
            return (Task::none(), None);
        };
        assert!(runtime.awaiting_admission());
        if resolve_upgrade
            && let CardKind::Chat(canonical) = &pane.kind
            && chat::client::UpgradeRecovery::is_candidate(&runtime.session, canonical)
        {
            // Similar descriptors are only candidates. Resolve the old source
            // on the authenticated connection and attach this exact target;
            // the existing Ready path waits for the durable upgrade receipt.
            let source = runtime.session.clone();
            runtime.connection = Connection::Connecting;
            self.cards.insert(pane.id, CardRuntime::Chat(runtime));
            if let Some(card) = self.card_state_mut(pane.id) {
                card.kind = pane.kind.clone();
            }
            return (
                self.start_shared_chat_upgrade(pane.id, source, canonical.clone()),
                None,
            );
        }
        Arc::make_mut(&mut self.durable)
            .pending_chat_admissions
            .remove(&pane.id);
        let mut saved_copy = None;
        let mut presentation = Task::none();
        // Card IDs are presentation identity, not proof that a replacement
        // conversation owns the old draft. Keep an incompatible source beside
        // the current pane, with its original descriptor and content intact.
        let same_source =
            matches!(&pane.kind, CardKind::Chat(session) if session == &runtime.session);
        if !same_source
            && (!runtime.unsent_text().is_empty() || !runtime.unsent_attachments().is_empty())
        {
            let retained = CardId::new();
            let mut state = self
                .card_state(pane.id)
                .expect("cached pane exists")
                .clone();
            state.id = retained;
            // The shared descriptor may already have advanced while a
            // strict upgrade attachment was in flight. The copy still owns
            // the runtime's original source, never that newer descriptor.
            state.kind = CardKind::Chat(runtime.session.clone());
            state.residency = Residency::Hot;
            let local_card = state.clone();
            runtime.retarget(self.chat_ui.next_target(retained));
            let durable = Arc::make_mut(&mut self.durable);
            if let Some(scope) = durable.card_scopes.get(&pane.id).cloned() {
                durable.card_scopes.insert(retained, scope);
            }
            for channel in &mut durable.channels {
                if let Some(index) = channel.cards.iter().position(|id| *id == pane.id) {
                    channel.cards.insert(index + 1, retained);
                }
            }
            durable
                .pending_chat_admissions
                .insert(retained, runtime.session.clone());
            durable.cards.push(state);
            self.retain_local_pane(retained);
            self.cards.insert(retained, CardRuntime::Chat(runtime));
            presentation = self.checkpoint_retained_chat_copy(pane.id, retained);
            saved_copy = Some(retained);
            let position = self.workspace.order().position(|id| id == pane.id);
            if let Some(index) = position {
                let card = WorkspaceCard::new(
                    retained,
                    local_card.title,
                    CardWidth::Fraction(local_card.width_fraction),
                )
                .expect("saved pane is valid");
                let update = self
                    .workspace
                    .add_card(card, index + 1, false)
                    .expect("saved draft has a fresh card identity");
                presentation =
                    Task::batch([presentation, self.apply_viewport(update, false, false)]);
            }
        } else if same_source {
            // Keep the Content object (including cursor/selection) through the
            // actual Ready handler, which replaces only the connection.
            runtime.connection = Connection::Connecting;
            self.cards.insert(pane.id, CardRuntime::Chat(runtime));
        }
        if let Some(card) = self.card_state_mut(pane.id) {
            card.kind = pane.kind.clone();
        }
        if !resolve_upgrade {
            // Failed exact lineage is not permission to restart either actor.
            // Keep the source copy inert and leave the canonical pane's retry
            // explicit instead of falling through to ordinary resume/start.
            let notice = if saved_copy.is_some() {
                "Could not reconnect this conversation. Your draft is in the pane beside it. Retry to reconnect."
            } else {
                "Could not reconnect this conversation. Retry to reconnect."
            };
            self.cards
                .insert(pane.id, CardRuntime::Failed(notice.into()));
            return (presentation, saved_copy);
        }
        let task = match &pane.kind {
            CardKind::Chat(session) if session.is_unstarted() => {
                if let Some(CardRuntime::Chat(runtime)) = self.cards.remove(&pane.id) {
                    self.chat_ui.drafts.insert(pane.id, runtime.unsent_text());
                    let images = runtime
                        .unsent_attachments()
                        .iter()
                        .filter(|image| image.reference.is_some())
                        .cloned()
                        .collect::<Vec<_>>();
                    if !images.is_empty() {
                        self.chat_ui.draft_attachments.insert(pane.id, images);
                    }
                }
                self.open_chat(pane.id, session.clone(), chat::Intent::Restore)
            }
            CardKind::Chat(session) => {
                self.cards.entry(pane.id).or_insert(CardRuntime::Loading);
                self.open_chat(pane.id, session.clone(), chat::Intent::Restore)
            }
            _ => Task::none(),
        };
        (Task::batch([task, presentation]), saved_copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_and_click_saved_panel(
        runtime: &Runtime,
        renderer: &cosmic::Renderer,
        width: f32,
    ) -> (Vec<String>, usize, Vec<Message>) {
        use iced::advanced::{
            Layout, Shell, clipboard, layout, mouse,
            widget::{Operation, Tree},
        };
        use iced::{Event, Rectangle, Size};

        #[derive(Default)]
        struct Panel {
            texts: Vec<String>,
            copy: Vec<Rectangle>,
        }
        impl Operation for Panel {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: Rectangle, text: &str) {
                self.texts.push(text.to_owned());
                if text == "Copy saved text" {
                    self.copy.push(bounds);
                }
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                _: Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                    self.texts.push(prose.plain.to_string());
                }
            }
        }
        let mut element = runtime.pending_admission_view();
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(width, 650.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut panel = Panel::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), renderer, &mut panel);
        let mut messages = Vec::new();
        for bounds in &panel.copy {
            let point = bounds.center();
            for event in [
                mouse::Event::CursorMoved { position: point },
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    mouse::Cursor::Available(point),
                    renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
        }
        (panel.texts, panel.copy.len(), messages)
    }

    #[test]
    fn saved_attachment_panel_copies_only_nonempty_text_and_keeps_its_images() {
        chat::attachment_assets::with_test_image(|image| {
            let renderer = crate::app::tests::headless_renderer();
            for named in [false, true] {
                for text in ["", "  saved text 🦀\n"] {
                    let mut app = crate::app::tests::context_inspector_test_app(false);
                    drop(app.new_native_chat(Provider::Claude, Some("/project".into())));
                    let card = app.workspace.focused().unwrap();
                    let mut source = app.chat_return_session(card).unwrap();
                    if named {
                        source.launch.session_id = Some("saved-image-panel-fixture".into());
                    }
                    app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
                    app.chat_ui = State::with_draft_attachments(
                        HashMap::from([(card, text.into())]),
                        HashMap::from([(card, vec![image.clone()])]),
                    );
                    app.await_shared_chat_admission(card, source.clone());
                    for width in [320.0, 900.0] {
                        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                            panic!("saved draft runtime expected");
                        };
                        let (texts, copy_controls, messages) =
                            render_and_click_saved_panel(runtime, &renderer, width);
                        assert!(
                            texts
                                .iter()
                                .any(|text| text == "Saved attachments stay in this pane.")
                        );
                        assert!(texts.iter().any(|text| {
                            text.contains("Reconnect the workspace")
                                && text.contains("history will not include these attachments")
                        }));
                        assert!(!texts.iter().any(|text| text == "Copy saved draft"));
                        if text.is_empty() {
                            assert_eq!(copy_controls, 0, "image-only content has no empty Copy");
                            assert!(messages.is_empty());
                        } else {
                            assert_eq!(copy_controls, 1);
                            assert_eq!(messages.len(), 1);
                            assert!(matches!(
                                &messages[0],
                                Message::ChatUi(Action::CopyLiteral(copied)) if copied == text
                            ));
                        }
                        for message in messages {
                            // The returned clipboard Task is deliberately not
                            // polled; this is renderer/action contract coverage.
                            drop(app.update(message));
                        }
                        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                            panic!("saved draft runtime expected");
                        };
                        assert_eq!(runtime.session, source);
                        assert_eq!(runtime.unsent_text(), text);
                        assert_eq!(runtime.unsent_attachments(), [image.clone()]);
                        assert!(runtime.awaiting_admission());
                        assert!(!app.chat_start_pending(card));
                        let saved = app.persistent_workspace();
                        assert_eq!(saved.chat_draft_attachments[&card], [image.clone()]);
                        assert_eq!(saved.pending_chat_admissions[&card], source);
                    }
                }
            }
        });
    }

    #[test]
    fn admission_keeps_image_only_draft_and_rehomes_its_exact_source_metadata() {
        chat::attachment_assets::with_test_image(|image| {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            drop(app.new_native_chat(Provider::Claude, Some("/project".into())));
            let card = app.workspace.focused().unwrap();
            let mut source = match app.cards.get(&card).unwrap() {
                CardRuntime::Chat(runtime) => runtime.session.clone(),
                _ => panic!("native draft expected"),
            };
            source.launch.session_id = Some("old-image-thread".into());
            app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
            app.chat_ui
                .draft_attachments
                .insert(card, vec![image.clone()]);
            app.await_shared_chat_admission(card, source.clone());
            let CardRuntime::Chat(runtime) = app.cards.get(&card).unwrap() else {
                panic!("saved draft expected")
            };
            assert!(runtime.awaiting_admission());
            assert!(runtime.unsent_text().is_empty());
            assert_eq!(runtime.unsent_attachments(), [image.clone()]);
            assert!(!app.chat_ui.draft_attachments.contains_key(&card));

            let mut replacement = app.card_state(card).unwrap().clone();
            let mut canonical = source.clone();
            canonical.id = TerminalId::new();
            canonical.launch.session_id = Some("different-thread".into());
            replacement.kind = CardKind::Chat(canonical);
            let pane = crate::workspace_model::views::Pane::from_card(
                &replacement,
                Some("/project".into()),
            )
            .unwrap();
            let (task, retained) = app.retain_unresolved_shared_upgrade(&pane);
            drop(task);
            let retained = retained.expect("image-only draft must receive an inert source copy");
            let CardRuntime::Chat(runtime) = app.cards.get(&retained).unwrap() else {
                panic!("retained source expected")
            };
            assert_eq!(runtime.session, source);
            assert_eq!(runtime.unsent_attachments(), [image.clone()]);
            let saved = app.persistent_workspace();
            assert_eq!(saved.chat_draft_attachments[&retained], [image]);
            assert!(saved.pending_chat_admissions.contains_key(&retained));
            assert!(
                matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))),
                "review-only admission must not start the replacement provider"
            );
        });
    }
}
