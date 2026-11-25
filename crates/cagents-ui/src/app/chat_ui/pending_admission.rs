//! Cached shared descriptors can present saved text, but cannot start providers.
use super::*;

/// Whether `a` and `b` name one conversation: the same chat and provider
/// conversation. The proof, continuity and listener are the daemon's record
/// of what hosts it, which it reissues when it relaunches the conversation,
/// as after a reboot; they don't change whose draft the pane holds.
pub(in crate::app) fn same_conversation(a: &chat::Session, b: &chat::Session) -> bool {
    a.id == b.id
        && a.launch.provider == b.launch.provider
        && a.launch.session_id == b.launch.session_id
}

/// Why a saved pane shows only its saved draft.
#[derive(Clone, Copy)]
pub(super) enum PendingPane {
    /// It waits for its view, which `held` says more about when the view
    /// itself is held back.
    Waiting { held: Option<&'static str> },
    /// Its saved descriptor failed its agent's proof, so it stays as saved
    /// rather than waiting to reconnect.
    Fenced,
    /// A copy of an unsent draft its pane kept when the pane's conversation
    /// changed. It never reconnects.
    Copy,
}

pub(in crate::app) const RETAINED_COPY_TITLE: &str = "A copy of your unsent message";
pub(in crate::app) const RETAINED_COPY_EXPLANATION: &str = "This conversation changed before you sent this, so cagents kept your text here. The conversation continues in its own pane. Copy what you still need, then close this copy. Closing it doesn't stop any agent.";

impl Runtime {
    pub(super) fn awaiting_admission(&self) -> bool {
        matches!(self.connection, Connection::AwaitingAdmission)
    }

    /// Whether the unsent draft holds anything worth keeping: text other
    /// than whitespace, or an attachment.
    pub(super) fn has_unsent_words_or_attachments(&self) -> bool {
        !self.unsent_attachments().is_empty() || !self.unsent_text().trim().is_empty()
    }

    pub(super) fn pending_admission_view(
        &self,
        trailing_clearance: f32,
        pending: PendingPane,
    ) -> Element<'_, Message> {
        let fenced = matches!(pending, PendingPane::Fenced);
        let card = self.target.card;
        let text = self.unsent_text();
        // Files and images wait with the text; their names say they were kept.
        let attached = self
            .unsent_attachments()
            .iter()
            .map(Attachment::label)
            .collect::<Vec<_>>();
        let mut children = Vec::new();
        // Why the pane waits leads: an update restore the daemon is still
        // running or holds, when that is the reason, above the draft it kept.
        if let PendingPane::Waiting { held } = pending {
            let (title, explanation) = self.update_wait.admission(held);
            if let Some(title) = title {
                children.push(design::title(title).into());
            }
            children.push(design::body(explanation).into());
        }
        if matches!(pending, PendingPane::Copy) {
            // A copy waits for nothing: it says what it is, and closes.
            children.push(design::title(RETAINED_COPY_TITLE).into());
            children.push(design::body(RETAINED_COPY_EXPLANATION).into());
            children.push(
                crate::ui_controls::button::primary("Close copy")
                    .id(card_control_id(card, "retained-copy.close"))
                    .name("Close copy")
                    .on_press(Message::CloseCard(card))
                    .into(),
            );
        }
        if fenced {
            // The same heading, explanation and Close as a fenced pane with no
            // chat runtime, which is drawn as an unavailable pane.
            children.push(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(Icon::Warning, design::STATUS_ICON_SIZE),
                    design::title("Agent connection unavailable").into(),
                ])
                .align_y(Alignment::Center)
                .spacing(space::S)
                .into(),
            );
            children.push(design::body(super::super::shared_views::FENCED_PANE_EXPLANATION).into());
            children.push(
                crate::ui_controls::button::primary("Close")
                    .id(card_control_id(card, "failed.close"))
                    .name("Close Agent")
                    .on_press(Message::CloseCard(card))
                    .into(),
            );
        }
        // A fenced pane may have nothing unsent; an empty saved draft would
        // only look like something to copy.
        if !fenced || !text.is_empty() || !attached.is_empty() {
            let copy = widget::button::custom(design::body("Copy saved draft"))
                .class(crate::ui_controls::control_style(false, None))
                .padding([space::S as u16, space::M as u16])
                .on_press_maybe(
                    (!text.is_empty()).then(|| Message::ChatUi(Action::CopyLiteral(text.clone()))),
                );
            children.push(
                widget::flex_row(vec![design::heading("Saved draft").into(), copy.into()])
                    .align_items(Alignment::Center)
                    .spacing(space::M as u16)
                    .into(),
            );
            // A long draft scrolls inside its card, so it fades into the card's
            // own fill at the card's edges. The vertical inset scrolls with the
            // text; the side inset keeps the fade clear of the rounded corners.
            // The card hugs a short draft; the space it may grow into is laid
            // out last, so the attachment caption below it keeps its room.
            let card = widget::container(crate::interaction::overflow_fade_y(
                widget::scrollable(
                    widget::container(view::body(text))
                        .width(Length::Fill)
                        .padding(design::with_vertical_rail(iced::Padding::from([
                            space::M,
                            0.0,
                        ]))),
                ),
                design::sunken,
            ))
            .width(Length::Fill)
            .padding([0.0, space::M])
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_M; 4],
                false,
            ));
            children.push(
                widget::container(card)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
            );
            if !attached.is_empty() {
                children.push(design::caption(format!("Attached: {}", attached.join(", "))).into());
            }
        }
        let content = widget::column::with_children(children)
            .spacing(space::M)
            .width(Length::Fill);
        widget::container(widget::container(content).max_width(design::READABLE_MEASURE))
            .padding(iced::Padding::new(space::L).right(space::L.max(trailing_clearance)))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

impl Cagents {
    pub(in crate::app) fn chat_awaiting_admission(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.awaiting_admission())
    }

    /// A copy of an unsent draft kept beside the pane that holds its
    /// conversation now: this window's own, and never admitted, so nothing
    /// waits for it. A kept pane whose view retired it has no such pane
    /// beside it, and stays what it is.
    pub(in crate::app) fn is_retained_chat_copy(&self, card: CardId) -> bool {
        self.durable.local_only_cards.contains(&card)
            && self
                .durable
                .pending_chat_admissions
                .get(&card)
                .is_some_and(|source| self.shared_pane_holds_chat(card, source.id))
    }

    /// Whether a shared pane other than `card` holds chat `chat`.
    fn shared_pane_holds_chat(&self, card: CardId, chat: TerminalId) -> bool {
        self.durable.cards.iter().any(|other| {
            other.id != card
                && other.existence.is_open()
                && !self.durable.local_only_cards.contains(&other.id)
                && matches!(&other.kind, CardKind::Chat(session) if session.id == chat)
        })
    }

    pub(super) fn pending_pane(&self, card: CardId) -> PendingPane {
        let duplicate = || {
            matches!(
                self.card_state(card).map(|state| &state.kind),
                Some(CardKind::Chat(session)) if self.shared_pane_holds_chat(card, session.id)
            )
        };
        if self.shared_views.quarantined.contains(&card) && !duplicate() {
            PendingPane::Fenced
        } else if self.is_retained_chat_copy(card) || self.shared_views.quarantined.contains(&card)
        {
            // A fenced alias of a pane that holds its conversation is a copy
            // of it in all but name.
            PendingPane::Copy
        } else {
            PendingPane::Waiting {
                held: self.channel_pane_view_hold(card),
            }
        }
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
        self.restore_saved_draft(card, &mut runtime);
        runtime.update_wait = self.chat_ui.update_recovery.wait;
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
        // Exactly what the workspace snapshot saves for this card later.
        let (draft, attachments) = runtime.persistent_draft();
        let payload = crate::persistence::RetainedChatCopy {
            workspace: self.durable.id,
            source_card,
            source: runtime.session.clone(),
            copy: self.card_state(copy).expect("retained card exists").clone(),
            draft,
            attachments,
            draft_model: runtime.draft_model().or_else(|| {
                self.chat_ui
                    .draft_models
                    .get(&copy)
                    .filter(|model| model.applies_to(&runtime.session))
                    .cloned()
            }),
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
        if self
            .chat_ui
            .attempt(card)
            .is_none_or(|attempt| attempt.upgrade_admission.is_none())
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
        // The card's own pane may already be retired or replaced, so its
        // Content can't be assumed to belong there: it is kept as a copy.
        self.admit_shared_chat_inner(&pane, false, false)
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
        self.admit_shared_chat_inner(pane, true, true)
    }

    /// Keeps the unproved source of a pane its view still lists, without
    /// attaching it.
    pub(in crate::app) fn retain_unresolved_shared_upgrade(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
    ) -> (Task<Message>, Option<CardId>) {
        self.admit_shared_chat_inner(pane, false, true)
    }

    /// `listed`: `pane` is what its view lists now, so a draft of the same
    /// conversation stays on it rather than in a copy.
    fn admit_shared_chat_inner(
        &mut self,
        pane: &crate::workspace_model::views::Pane,
        resolve_upgrade: bool,
        listed: bool,
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
        // A descriptor the daemon reissued for the same conversation, as
        // when it relaunched it after a reboot, still owns this draft.
        let same_conversation = listed
            && !same_source
            && matches!(&pane.kind, CardKind::Chat(session) if same_conversation(session, &runtime.session));
        // The copy can only be read and copied, so only text and attachments
        // earn one. A model choice alone stays bound to its own draft and is
        // not offered to the replacement, and whitespace is not text.
        if !same_source && !same_conversation && runtime.has_unsent_words_or_attachments() {
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
            let local_card = state.clone();
            runtime.retarget(self.chat_ui.next_target(retained));
            if let Some(model) = self.chat_ui.draft_models.remove(&pane.id) {
                self.chat_ui.draft_models.insert(retained, model);
            }
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
            self.set_presence(retained, self.exposure(pane.id), Residency::Hot);
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
                presentation = Task::batch([
                    presentation,
                    self.apply_viewport(update, false, DurableChange::None),
                ]);
            }
        } else if same_source {
            // Keep the Content object (including cursor/selection) through the
            // actual Ready handler, which replaces only the connection.
            runtime.connection = Connection::Connecting;
            self.cards.insert(pane.id, CardRuntime::Chat(runtime));
        } else if same_conversation {
            // Only what the reader typed carries over: the runtime that opens
            // for the daemon's descriptor restores it from the saved draft.
            // Nothing the old runtime held in flight is replayed.
            self.cards.insert(pane.id, CardRuntime::Chat(runtime));
            self.stash_unsent_chat(pane.id);
            self.cards.remove(&pane.id);
        }
        if let (Some(CardKind::Chat(saved)), CardKind::Chat(canonical)) = (
            self.card_state(pane.id).map(|card| card.kind.clone()),
            &pane.kind,
        ) {
            self.adopt_shared_chat_identity(pane.id, &saved, canonical);
        }
        if let Some(card) = self.card_state_mut(pane.id) {
            card.kind = pane.kind.clone();
        }
        if !resolve_upgrade {
            // Failed exact lineage is not permission to restart either actor.
            // Keep the source copy inert and leave the canonical pane's retry
            // explicit instead of falling through to ordinary resume/start.
            let kept_here = self
                .chat_ui
                .drafts
                .get(&pane.id)
                .is_some_and(|draft| !draft.trim().is_empty())
                || self.chat_ui.draft_attachments.contains_key(&pane.id);
            let notice = if saved_copy.is_some() {
                "Could not reconnect this conversation. Your draft is in the pane beside it. Retry to reconnect."
            } else if kept_here {
                "Could not reconnect this conversation. Retry to reconnect; your unsent draft comes back with it."
            } else {
                "Could not reconnect this conversation. Retry to reconnect."
            };
            self.cards
                .insert(pane.id, CardRuntime::Failed(notice.into()));
            return (presentation, saved_copy);
        }
        let task = match &pane.kind {
            CardKind::Chat(session) if session.is_unstarted() => {
                self.stash_unsent_chat(pane.id);
                self.cards.remove(&pane.id);
                self.open_chat(pane.id, session.clone(), chat::Intent::Restore)
            }
            CardKind::Chat(session) => {
                self.cards.entry(pane.id).or_insert(CardRuntime::Loading);
                // A conversation still stopping after its pane closed, or
                // whose stop was refused, attaches as any pane a view shows
                // it on does: after the stop, or to what still runs.
                self.attach_shared_chat(pane.id, session.clone())
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

    /// The file a person chose stays with the draft another window's start
    /// set aside, and in the inert copy kept beside the replacement (#2202).
    #[test]
    fn admission_keeps_an_unsent_file_beside_the_draft_images() {
        chat::attachment_assets::with_test_image(|image| {
            let file = chat::Attachment::file("/project/notes.txt");
            let mut app = crate::app::tests::context_inspector_test_app(false);
            drop(app.new_native_chat(Provider::Claude, Some("/project".into())));
            let card = app.workspace.focused().unwrap();
            let mut source = match app.cards.get(&card).unwrap() {
                CardRuntime::Chat(runtime) => runtime.session.clone(),
                _ => panic!("native draft expected"),
            };
            source.launch.session_id = Some("old-file-thread".into());
            app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
            app.chat_ui
                .draft_attachments
                .insert(card, vec![image.clone(), file.clone()]);
            app.await_shared_chat_admission(card, source.clone());
            let CardRuntime::Chat(runtime) = app.cards.get(&card).unwrap() else {
                panic!("saved draft expected")
            };
            assert_eq!(runtime.unsent_attachments(), [image.clone(), file.clone()]);

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
            let retained = retained.expect("a file-carrying draft receives an inert source copy");
            let saved = app.persistent_workspace();
            assert_eq!(saved.chat_draft_attachments[&retained], [image, file]);
        });
    }
}
