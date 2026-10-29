//! Cached shared descriptors can present saved text, but cannot start providers.
use super::*;

impl Runtime {
    pub(in crate::app) fn awaiting_admission(&self) -> bool {
        matches!(self.connection, Connection::AwaitingAdmission)
    }

    pub(super) fn pending_admission_view(&self) -> Element<'_, Message> {
        self.pending_admission_view_with_failure(None, false)
    }

    pub(super) fn pending_admission_view_with_failure(
        &self,
        failure: Option<&crate::app::shared_views::SharedChatAdmissionFailure>,
        retry_enabled: bool,
    ) -> Element<'_, Message> {
        self.saved_conversation_view(failure, retry_enabled, false)
    }

    /// The saved descriptor failed its agent's proof, so this pane stays as
    /// saved rather than waiting to connect, and can be closed in this window.
    pub(super) fn fenced_admission_view(&self) -> Element<'_, Message> {
        self.saved_conversation_view(None, false, true)
    }

    fn saved_conversation_view(
        &self,
        failure: Option<&crate::app::shared_views::SharedChatAdmissionFailure>,
        retry_enabled: bool,
        fenced: bool,
    ) -> Element<'_, Message> {
        let text = self.unsent_text();
        let attachments = self.unsent_attachments();
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::heading("Saved conversation").into(),
            design::caption("Not connected")
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
            design::body(if fenced {
                crate::app::shared_views::FENCED_PANE_EXPLANATION
            } else {
                "This conversation is not connected. Saved content remains available here."
            })
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .width(Length::Fill)
            .into(),
        ];
        if fenced {
            rows.push(
                widget::button::custom(design::body("Close here"))
                    .name("Close here")
                    .id(card_control_id(self.target.card, "saved.fenced.close"))
                    .class(crate::ui_controls::control_style(false, None))
                    .padding([space::S as u16, space::M as u16])
                    .on_press(Message::CloseCard(self.target.card))
                    .into(),
            );
        }
        if let Some(failure) = failure {
            // Admission owns the sanitized public message and opaque authority.
            // Presentation never inspects a session proof or creates a retry.
            rows.push(
                design::body(failure.message().to_owned())
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill)
                    .into(),
            );
            if let Some(retry) = failure.retry() {
                rows.push(crate::ui_controls::owned(
                    (retry.clone(), retry_enabled),
                    widget::button::custom(design::body("Retry workspace connection"))
                        .name("Retry workspace connection")
                        .id(card_control_id(self.target.card, "saved.admission.retry"))
                        .class(crate::ui_controls::control_style(false, None))
                        .padding([space::S as u16, space::M as u16])
                        .on_press_maybe(
                            retry_enabled.then_some(Message::RetrySharedChatAdmission(retry)),
                        ),
                ));
            }
        }
        if text.is_empty() && attachments.is_empty() {
            rows.push(design::body("No unsent draft.").into());
        } else {
            rows.push(design::heading("Unsent draft").into());
            if !text.is_empty() {
                rows.push(
                    widget::button::custom(design::body("Copy saved draft"))
                        .name("Copy saved draft")
                        .class(crate::ui_controls::control_style(false, None))
                        .padding([space::S as u16, space::M as u16])
                        .on_press(Message::ChatUi(Action::CopySavedDraft(text.clone())))
                        .into(),
                );
                rows.push(
                    widget::container(view::body(text))
                        .width(Length::Fill)
                        .padding(space::M)
                        .class(design::surface(
                            Some(design::sunken),
                            [design::RADIUS_M; 4],
                            false,
                        ))
                        .into(),
                );
            }
            if !attachments.is_empty() {
                rows.push(design::caption(format!("Attachments ({})", attachments.len())).into());
                for attachment in attachments {
                    // Identity and numbering come from the preserved receipt.
                    // This is a text inventory: never open paths or decode images.
                    let label = match &attachment.reference {
                        Some(reference) if !reference.name.is_empty() => {
                            format!("{} · {}", reference.label(), reference.name)
                        }
                        _ => attachment.label(),
                    };
                    rows.push(
                        widget::row::with_children(vec![
                            crate::ui_controls::passive_icon(
                                Icon::Attachment,
                                design::ROW_ICON_SIZE,
                            ),
                            design::body(label)
                                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                                .width(Length::Fill)
                                .into(),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(space::S)
                        .into(),
                    );
                }
            }
        }
        let content = widget::column::with_children(vec![
            widget::scrollable(widget::column::with_children(rows).spacing(space::M))
                .height(Length::Fill)
                .into(),
        ])
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
