//! Cached shared descriptors can present saved text, but cannot start providers.
use super::*;

impl Runtime {
    pub(super) fn awaiting_admission(&self) -> bool {
        matches!(self.connection, Connection::AwaitingAdmission)
    }

    pub(super) fn pending_admission_view(&self) -> Element<'_, Message> {
        let text = self.unsent_text();
        let copy = widget::button::custom(design::body("Copy saved draft"))
            .class(crate::ui_controls::control_style(false, None))
            .padding([space::S as u16, space::M as u16])
            .on_press_maybe(
                (!text.is_empty()).then(|| Message::ChatUi(Action::CopyLiteral(text.clone()))),
            );
        let header = widget::flex_row(vec![design::heading("Saved draft").into(), copy.into()])
            .align_items(Alignment::Center)
            .spacing(space::M as u16);
        let draft = widget::container(view::body(text))
            .width(Length::Fill)
            .padding(space::M)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_M; 4],
                false,
            ));
        let content = widget::column::with_children(vec![
            header.into(),
            design::body("This saved pane has not been reopened. Your unsent text is kept here; you can copy it while the workspace reconnects, or open the conversation from history.").into(),
            widget::scrollable(draft).height(Length::Fill).into(),
        ])
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
        // Deliberately no provider connection, draft skill discovery, retirement,
        // or lifecycle job: this is only a local view of saved bytes.
        self.cards
            .insert(card, CardRuntime::Chat(Box::new(runtime)));
    }

    pub(in crate::app) fn admit_shared_chat(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
    ) -> (Task<Message>, Option<CardId>) {
        let Some(CardRuntime::Chat(mut runtime)) = self.cards.remove(&pane.id) else {
            return (Task::none(), None);
        };
        assert!(runtime.awaiting_admission());
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
        if !same_source && !runtime.unsent_text().is_empty() {
            let retained = CardId::new();
            let mut state = self
                .card_state(pane.id)
                .expect("cached pane exists")
                .clone();
            state.id = retained;
            state.residency = Residency::Hot;
            let local_card = state.clone();
            runtime.target = self.chat_ui.next_target(retained);
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
                presentation = self.apply_viewport(update, false, false);
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
        let task = match &pane.kind {
            CardKind::Chat(session) if session.is_unstarted() => {
                if let Some(CardRuntime::Chat(runtime)) = self.cards.remove(&pane.id) {
                    self.chat_ui.drafts.insert(pane.id, runtime.unsent_text());
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
