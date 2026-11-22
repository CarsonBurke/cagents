//! Closing a chat's card leaves its conversation in history, so the reader
//! can open it again, as a new card. What they had written but not sent goes
//! with it: the composer's text and attachments, a first message or
//! follow-ups held for the next turn, and what the provider was still
//! holding when the close stopped it. The card that reopens the
//! conversation takes all of it back into its composer, less anything the
//! conversation shows arrived.
//!
//! The window keeps this with its workspace, so it survives a restart, for
//! the newest [`KeptChatDraft::LIMIT`] closed chats, for as long as
//! [`KeptChatDraft::RETENTION_MS`]. Only what the reader confirmed a close
//! discards is not kept.
use super::*;
use crate::model::{KeptChatDraft, SentAfter};

/// What the reader confirmed a close discards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ConfirmedLoss {
    /// A draft whose conversation couldn't start: the composer is the
    /// message, so nothing is kept.
    Draft,
    /// The first message, still on its way. Anything else written since is
    /// kept.
    FirstMessage,
}

impl ConfirmedLoss {
    /// What answering `question` with its destructive choice discards.
    pub(in crate::app) fn of(question: dialog_ui::Question) -> Option<Self> {
        match question {
            dialog_ui::Question::DiscardUnsentMessage => Some(Self::Draft),
            dialog_ui::Question::StopSendingFirstMessage => Some(Self::FirstMessage),
            _ => None,
        }
    }
}

impl Cagents {
    /// What closing `card` now would keep for its conversation to reopen
    /// with, once `loss` is discarded. Empty when it is not a chat or has
    /// nothing else unsent.
    pub(in crate::app) fn draft_to_keep(
        &self,
        card: CardId,
        loss: Option<ConfirmedLoss>,
    ) -> KeptChatDraft {
        if loss == Some(ConfirmedLoss::Draft)
            || !matches!(
                self.card_state(card).map(|state| &state.kind),
                Some(CardKind::Chat(_))
            )
        {
            return KeptChatDraft::default();
        }
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        let (text, attachments, sending) = match runtime {
            Some(runtime) => runtime.kept_draft(loss != Some(ConfirmedLoss::FirstMessage)),
            // A card without a runtime holds its draft where the last one left it.
            None => (
                self.chat_ui.drafts.get(&card).cloned().unwrap_or_default(),
                self.chat_ui
                    .draft_attachments
                    .get(&card)
                    .cloned()
                    .unwrap_or_default(),
                self.chat_ui.sending_on_close.get(&card).cloned(),
            ),
        };
        KeptChatDraft {
            text,
            attachments,
            waiting: self.chat_waiting_to_save(card).messages,
            sending: sending.is_some(),
            sent_after: sending.unwrap_or_default(),
            closed_at_unix_ms: 0,
        }
    }

    /// Keeps what `card`'s chat had not sent, as its card closes and before
    /// its runtime is dropped, less what the reader confirmed this close
    /// discards.
    pub(in crate::app) fn keep_unsent_on_close(&mut self, card: CardId) {
        let session = match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(session)) => Some(session.id),
            _ => None,
        };
        // A confirmation answers for the conversation it asked about, and
        // for a blocked draft only while that draft is still blocked.
        let loss = self
            .chat_ui
            .confirmed_losses
            .remove(&card)
            .filter(|(asked, _)| Some(*asked) == session)
            .map(|(_, loss)| loss)
            .filter(|loss| {
                *loss != ConfirmedLoss::Draft || self.blocked_draft_discard(card) == session
            });
        let draft = KeptChatDraft {
            closed_at_unix_ms: unix_millis(),
            ..self.draft_to_keep(card, loss)
        };
        if draft.is_empty() {
            if self.durable.kept_chat_drafts.contains_key(&card) {
                Arc::make_mut(&mut self.durable)
                    .kept_chat_drafts
                    .remove(&card);
            }
            return;
        }
        let now = draft.closed_at_unix_ms;
        let drafts = &mut Arc::make_mut(&mut self.durable).kept_chat_drafts;
        drafts.insert(card, draft);
        KeptChatDraft::retain_recent(drafts, now);
    }

    /// Takes back the draft kept when `closed` closed, to be moved with
    /// [`Self::move_chat_draft`] into the card reopening its conversation.
    /// Whether there was one.
    pub(in crate::app) fn reclaim_kept_draft(&mut self, closed: CardId) -> bool {
        if !self.durable.kept_chat_drafts.contains_key(&closed) {
            // A closed pane a shared view put back already holds what its
            // close kept, where a card without a runtime does.
            return self.returned_close_is_hidden(closed)
                && !self.cards.contains_key(&closed)
                && (self.chat_ui.drafts.contains_key(&closed)
                    || self.chat_ui.draft_attachments.contains_key(&closed)
                    || self.chat_ui.waiting.contains_key(&closed));
        }
        let Some(draft) = Arc::make_mut(&mut self.durable)
            .kept_chat_drafts
            .remove(&closed)
        else {
            return false;
        };
        if unix_millis().saturating_sub(draft.closed_at_unix_ms) > KeptChatDraft::RETENTION_MS {
            return false;
        }
        if !draft.text.is_empty() {
            self.chat_ui.drafts.insert(closed, draft.text);
            if draft.sending {
                self.chat_ui
                    .sending_on_close
                    .insert(closed, draft.sent_after);
            }
        }
        if !draft.attachments.is_empty() {
            self.chat_ui
                .draft_attachments
                .insert(closed, draft.attachments);
        }
        if !draft.waiting.is_empty() {
            // Saved with the reopened card like any queue, until it connects
            // and can tell what the conversation already has.
            self.chat_ui.waiting.insert(
                closed,
                SavedQueue {
                    host: None,
                    messages: draft.waiting,
                    stopped: true,
                },
            );
        }
        true
    }

    /// Gives `card` the unsent draft `source` holds without a runtime.
    pub(in crate::app) fn move_chat_draft(&mut self, source: CardId, card: CardId) {
        if let Some(attachments) = self.chat_ui.draft_attachments.remove(&source) {
            self.chat_ui.draft_attachments.insert(card, attachments);
        }
        if let Some(draft) = self.chat_ui.drafts.remove(&source) {
            self.chat_ui.drafts.insert(card, draft);
        }
        if let Some(after) = self.chat_ui.sending_on_close.remove(&source) {
            self.chat_ui.sending_on_close.insert(card, after);
        }
        // Only messages no provider holds go with the draft; another
        // card's queue stays its own.
        if self
            .chat_ui
            .waiting
            .get(&source)
            .is_some_and(|queue| queue.stopped)
            && let Some(queue) = self.chat_ui.waiting.remove(&source)
        {
            self.chat_ui.waiting.insert(card, queue);
        }
    }

    /// Records what the reader confirmed closing `card` discards, for the
    /// conversation `session` they were asked about, replacing any earlier
    /// answer.
    pub(in crate::app) fn confirm_close_loss(
        &mut self,
        card: CardId,
        session: TerminalId,
        question: dialog_ui::Question,
    ) {
        match ConfirmedLoss::of(question) {
            Some(loss) => {
                self.chat_ui.confirmed_losses.insert(card, (session, loss));
            }
            None => {
                self.chat_ui.confirmed_losses.remove(&card);
            }
        }
    }

    /// Whether answering `question` by closing `card` keeps anything for its
    /// conversation to reopen with.
    pub(in crate::app) fn close_keeps_unsent(
        &self,
        card: CardId,
        question: dialog_ui::Question,
    ) -> bool {
        !self
            .draft_to_keep(card, ConfirmedLoss::of(question))
            .is_empty()
    }

    /// Forgets what an earlier close of `card` was confirmed to discard.
    pub(in crate::app) fn forget_close_loss(&mut self, card: CardId) {
        self.chat_ui.confirmed_losses.remove(&card);
    }
}

impl Runtime {
    /// What closing this conversation's card would keep of what the reader
    /// has not sent, with what they had said before it where that begins
    /// with the message still being sent. A first message the provider already
    /// took is in the conversation, so it is not kept again, nor one
    /// `with_first` excludes.
    fn kept_draft(&self, with_first: bool) -> (String, Vec<Attachment>, Option<SentAfter>) {
        let delivered = self
            .handle()
            .is_some_and(|handle| handle.has_accepted_user_input());
        let (text, attachments) =
            self.unsent_with_held(self.queued.as_ref().filter(|_| with_first && !delivered));
        // The first message still on its way leads what is kept, ahead of
        // anything written after it, which stays however it went.
        let sending = self
            .commands
            .iter()
            .find_map(|pending| Some((pending.sent.as_deref()?, &pending.sent_after)))
            .filter(|(sent, _)| attachments.is_empty() && leading(&text, sent).is_some())
            .map(|(_, after)| after.clone());
        (text, attachments, sending)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        answer_start, chat_runtime, chat_sending_its_first_message, fixture_handle, live_chat_in,
        submit, write,
    };
    use super::*;
    use crate::persistence::SavedConversation;

    fn dialog_text(app: &Cagents) -> String {
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
        let mut element = app.chat_close_dialog();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1200.0, 860.0)),
        );
        let mut text = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut text);
        text.0.join("\n")
    }

    /// Closes `card` as its close button does, answering any question with
    /// its confirmation, and checks the card is gone.
    fn close(app: &mut Cagents, card: CardId) {
        drop(app.update(Message::CloseCard(card)));
        if app.chat_close_asked(card) {
            drop(app.update(Message::ConfirmChatClose));
        }
        finish_close(app, card);
    }

    /// Lets the close of `card` under way finish, and checks the card is gone.
    fn finish_close(app: &mut Cagents, card: CardId) {
        if let Some(Phase::Closing {
            target,
            restart: None,
        }) = app.chat_ui.phases.get(&card)
        {
            let target = *target;
            let source = app.chat_return_session(card).unwrap();
            drop(app.update(Message::ChatUi(Action::HeldCloseFinished(
                target,
                source,
                Ok(()),
            ))));
        }
        // A close running in the background, which the daemon now confirms.
        let stopping = app
            .chat_ui
            .background_closes
            .keys()
            .copied()
            .filter(|target| target.card == card)
            .collect::<Vec<_>>();
        for target in stopping {
            drop(app.update(Message::ChatUi(Action::Retired(target, Ok(())))));
        }
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(!app.card_state(card).unwrap().existence.is_open());
    }

    fn queued(id: &str, text: &str) -> chat::QueuedMessage {
        chat::QueuedMessage {
            id: id.into(),
            text: text.into(),
            editable: true,
            delivery: Default::default(),
            attachments: Vec::new(),
        }
    }

    fn user_said(id: &str, text: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::User,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        })
    }

    /// Reopens `card`'s conversation from history, connected to a provider
    /// whose transcript holds `items`, and returns the new card.
    fn reopen_with(app: &mut Cagents, card: CardId, items: Vec<Arc<chat::Item>>) -> CardId {
        reopen_with_page(app, card, items, false)
    }

    /// [`reopen_with`], with `has_earlier` saying the transcript's earlier
    /// messages are not loaded.
    fn reopen_with_page(
        app: &mut Cagents,
        card: CardId,
        items: Vec<Arc<chat::Item>>,
        has_earlier: bool,
    ) -> CardId {
        let reopened = begin_reopen(app, card);
        let target = app.chat_ui.attempt(reopened).unwrap().target;
        let handle = fixture_handle(app, reopened, Some("kept"));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items = items;
        snapshot.has_earlier = has_earlier;
        handle.publish_snapshot_for_test(snapshot.clone());
        answer_start(app, target, Ok(handle));
        // The transcript, shown as the focused card it opens as.
        super::super::show_chat_snapshot_for_test(app, reopened, snapshot);
        reopened
    }

    /// Opens `card`'s conversation from history in a new card, still connecting.
    fn begin_reopen(app: &mut Cagents, card: CardId) -> CardId {
        drop(app.resume_saved_conversation(saved(app, card)));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        reopened
    }

    /// The conversation `card` held, as history lists it once its card closed.
    fn saved(app: &Cagents, card: CardId) -> SavedConversation {
        let CardKind::Chat(mut session) = app.card_state(card).unwrap().kind.clone() else {
            panic!("a chat");
        };
        // Resumed directly, without a daemon to prepare it.
        session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        SavedConversation {
            card,
            title: "Kept".into(),
            session,
            open: false,
            updated_at_unix_ms: 0,
        }
    }

    /// Someone closes a working chat with a draft written, a follow-up held
    /// for the next turn and a message the provider queued. The routine
    /// question says they come back; reopening the conversation from history
    /// brings every one of them back to its composer, in the order written,
    /// and says which the provider may have had.
    #[test]
    fn a_working_chat_closed_with_unsent_messages_reopens_with_them() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = live_chat_in(&mut app, Provider::Claude, project.path(), Some("kept"));
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.busy = true;
        snapshot.queued_messages = vec![queued("q-1", "queued with the provider")];
        handle.publish_snapshot_for_test(snapshot);
        if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
            runtime.followups.push(Followup {
                id: 0,
                text: "held for the next turn".into(),
                preview: "held for the next turn".into(),
                attachments: Vec::new(),
            });
        }
        write(&mut app, card, "still writing");

        drop(app.update(Message::CloseCard(card)));
        assert!(app.chat_close_asked(card));
        let text = dialog_text(&app);
        assert!(text.contains("It is still working"), "{text}");
        assert!(
            text.contains(
                "Unsent and queued messages come back when you reopen this chat in this window."
            ),
            "{text}"
        );
        close(&mut app, card);

        let kept = app.durable.kept_chat_drafts[&card].clone();
        assert_eq!(kept.text, "held for the next turn\nstill writing");
        assert_eq!(
            kept.waiting,
            vec![queued("q-1", "queued with the provider")]
        );
        assert!(kept.closed_at_unix_ms > 0);
        assert_eq!(
            app.persistent_workspace().kept_chat_drafts[&card],
            kept,
            "it is saved with the workspace"
        );

        drop(app.resume_saved_conversation(saved(&app, card)));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert!(app.durable.kept_chat_drafts.is_empty(), "taken back once");
        let target = app.chat_ui.attempt(reopened).unwrap().target;
        let handle = fixture_handle(&app, reopened, Some("kept"));
        answer_start(&mut app, target, Ok(handle));
        let runtime = chat_runtime(&app, reopened);
        assert_eq!(
            runtime.composer.text().trim_end(),
            "queued with the provider\nheld for the next turn\nstill writing"
        );
        assert_eq!(
            runtime.restored_waiting,
            Some(RestoredWaiting {
                count: 1,
                whole: true
            }),
            "the composer says the agent may have had the queued message"
        );
    }

    /// A message the reopened conversation's history shows arrived is not
    /// offered again.
    #[test]
    fn a_queued_message_that_arrived_is_not_brought_back() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = live_chat_in(&mut app, Provider::Claude, project.path(), Some("kept"));
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.busy = true;
        snapshot.queued_messages = vec![queued("q-1", "it got there")];
        handle.publish_snapshot_for_test(snapshot);
        close(&mut app, card);

        let reopened = reopen_with(&mut app, card, vec![user_said("q-1", "it got there")]);
        let runtime = chat_runtime(&app, reopened);
        assert!(runtime.composer.is_empty());
        assert_eq!(runtime.restored_waiting, None);
    }

    /// The provider's queued messages wait with the reopened card, saved with
    /// it, until it connects; closing it again before then keeps them again.
    #[test]
    fn queued_messages_survive_a_reopened_card_that_never_connected() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = live_chat_in(&mut app, Provider::Claude, project.path(), Some("kept"));
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.busy = true;
        snapshot.queued_messages = vec![queued("q-1", "not yet read")];
        handle.publish_snapshot_for_test(snapshot);
        close(&mut app, card);

        let reopened = begin_reopen(&mut app, card);
        let saved = app.persistent_workspace().chat_waiting_messages[&reopened].clone();
        assert!(saved.stopped, "no provider holds them: {saved:?}");
        assert_eq!(saved.messages, vec![queued("q-1", "not yet read")]);

        close(&mut app, reopened);
        assert_eq!(
            app.durable.kept_chat_drafts[&reopened].waiting,
            vec![queued("q-1", "not yet read")]
        );
    }

    /// A message still being sent as the chat closed comes back, unless the
    /// reopened conversation shows it arrived.
    #[test]
    fn a_message_still_being_sent_comes_back_only_if_it_did_not_arrive() {
        for arrived in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let project = tempfile::tempdir().unwrap();
            let card = live_chat_in(&mut app, Provider::Claude, project.path(), Some("kept"));
            write(&mut app, card, "run the migration");
            submit(&mut app, card);
            assert!(
                chat_runtime(&app, card).commands.in_flight().is_some(),
                "still sending"
            );
            close(&mut app, card);
            let kept = &app.durable.kept_chat_drafts[&card];
            assert_eq!(kept.text, "run the migration");
            assert!(kept.sending);

            let items = if arrived {
                vec![
                    user_said("u-1", "an earlier message"),
                    user_said("u-2", "run the migration"),
                ]
            } else {
                vec![user_said("u-1", "an earlier message")]
            };
            let reopened = reopen_with(&mut app, card, items);
            assert_eq!(
                chat_runtime(&app, reopened).composer.text().trim_end(),
                if arrived { "" } else { "run the migration" },
                "arrived {arrived}"
            );
        }
    }

    /// What was written after a message still being sent stays when the
    /// chat reopens, whether or not that message arrived; only the message
    /// itself is dropped once the conversation shows it.
    #[test]
    fn what_was_written_after_a_message_still_being_sent_stays() {
        for arrived in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let project = tempfile::tempdir().unwrap();
            let card = live_chat_in(&mut app, Provider::Claude, project.path(), Some("kept"));
            write(&mut app, card, "run the migration");
            submit(&mut app, card);
            write(&mut app, card, "then the seed");
            close(&mut app, card);
            let kept = &app.durable.kept_chat_drafts[&card];
            assert_eq!(kept.text, "run the migration\nthen the seed");
            assert!(kept.sending);

            let mut items = vec![user_said("u-1", "an earlier message")];
            if arrived {
                items.push(user_said("u-2", "run the migration"));
            }
            let reopened = reopen_with(&mut app, card, items);
            assert_eq!(
                chat_runtime(&app, reopened).composer.text().trim_end(),
                if arrived {
                    "then the seed"
                } else {
                    "run the migration\nthen the seed"
                },
                "arrived {arrived}"
            );
        }
    }

    /// Closes a chat in `project` showing `items`, as `shown` further
    /// describes, while it is still sending `continue`.
    fn close_sending_continue(
        project: &Path,
        items: Vec<Arc<chat::Item>>,
        shown: impl FnOnce(&mut chat::Snapshot),
    ) -> (Cagents, CardId) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_in(&mut app, Provider::Claude, project, Some("kept"));
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items = items;
        shown(&mut snapshot);
        handle.publish_snapshot_for_test(snapshot.clone());
        super::super::show_chat_snapshot_for_test(&mut app, card, snapshot);
        write(&mut app, card, "continue");
        if chat_runtime(&app, card).snapshot.busy {
            // Queued behind the agent's work, as Send does not interrupt it.
            let target = chat_runtime(&app, card).target;
            drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Queue))));
        } else {
            submit(&mut app, card);
        }
        assert!(
            chat_runtime(&app, card).commands.in_flight().is_some(),
            "still sending"
        );
        close(&mut app, card);
        assert!(app.durable.kept_chat_drafts[&card].sending);
        (app, card)
    }

    /// Someone sends `continue` again and closes the chat while that second
    /// one is still being sent. The first `continue` in the reopened
    /// conversation does not count as its arrival; only a later one does.
    #[test]
    fn a_repeated_message_still_being_sent_comes_back_unless_it_arrived() {
        for arrived in [false, true] {
            let project = tempfile::tempdir().unwrap();
            let (mut app, card) =
                close_sending_continue(project.path(), vec![user_said("u-1", "continue")], |_| {});
            assert_eq!(
                app.durable.kept_chat_drafts[&card].sent_after,
                SentAfter::Same("u-1".into())
            );
            let mut items = vec![user_said("u-1", "continue")];
            if arrived {
                items.push(user_said("u-2", "continue"));
            }
            let reopened = reopen_with(&mut app, card, items);
            assert_eq!(
                chat_runtime(&app, reopened).composer.text().trim_end(),
                if arrived { "" } else { "continue" },
                "arrived {arrived}"
            );
        }
    }

    /// Where the reopened conversation can't show that a `continue` came
    /// after the one before the repeat, the repeat stays in the composer
    /// rather than being lost: that one is out of view or under another id,
    /// or the latest messages were out of view when it was sent.
    #[test]
    fn a_repeated_message_that_cannot_be_told_apart_comes_back() {
        let repeated = || {
            vec![
                user_said("item-1", "continue"),
                user_said("item-2", "continue"),
            ]
        };
        let project = tempfile::tempdir().unwrap();
        // Under other ids, as a provider that numbers replayed items gives.
        let (mut app, card) =
            close_sending_continue(project.path(), vec![user_said("u-1", "continue")], |_| {});
        let reopened = reopen_with(&mut app, card, repeated());
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "continue"
        );

        // Sent while reading an earlier page.
        let (mut app, card) = close_sending_continue(
            project.path(),
            vec![user_said("u-1", "continue")],
            |shown| {
                shown.has_later = true;
            },
        );
        assert_eq!(
            app.durable.kept_chat_drafts[&card].sent_after,
            SentAfter::Unseen
        );
        let reopened = reopen_with(&mut app, card, repeated());
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "continue"
        );

        // Sent with no message from the reader in view: a page of the
        // agent's work, its earlier messages not loaded.
        let (mut app, card) = close_sending_continue(project.path(), Vec::new(), |shown| {
            shown.has_earlier = true;
        });
        assert_eq!(
            app.durable.kept_chat_drafts[&card].sent_after,
            SentAfter::Unseen
        );
        let reopened = reopen_with(&mut app, card, repeated());
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "continue"
        );

        // Reopened on a page that starts after the message before it.
        let (mut app, card) =
            close_sending_continue(project.path(), vec![user_said("u-1", "continue")], |_| {});
        let reopened = reopen_with_page(&mut app, card, vec![user_said("u-2", "continue")], true);
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "continue"
        );
    }

    /// The agent is busy, so the first `continue` waits in the provider's
    /// queue as the second is sent, and the chat closes while that one is
    /// still being sent. The first arriving later does not count as the
    /// second's arrival.
    #[test]
    fn a_message_repeating_a_queued_one_comes_back() {
        let project = tempfile::tempdir().unwrap();
        let (mut app, card) =
            close_sending_continue(project.path(), vec![user_said("u-1", "start")], |shown| {
                shown.busy = true;
                shown.capabilities.queue = true;
                shown.queued_messages = vec![queued("q-1", "continue")];
            });
        assert_eq!(
            app.durable.kept_chat_drafts[&card].sent_after,
            SentAfter::Unseen
        );
        let reopened = reopen_with(
            &mut app,
            card,
            vec![user_said("u-1", "start"), user_said("q-1", "continue")],
        );
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "continue"
        );
    }

    /// A different message sent after `continue` still settles by the latest
    /// message alone, whatever its id.
    #[test]
    fn a_new_message_still_being_sent_settles_by_the_latest_message() {
        let project = tempfile::tempdir().unwrap();
        let (mut app, card) =
            close_sending_continue(project.path(), vec![user_said("u-1", "go on")], |_| {});
        assert_eq!(
            app.durable.kept_chat_drafts[&card].sent_after,
            SentAfter::Different
        );
        let reopened = reopen_with(
            &mut app,
            card,
            vec![
                user_said("item-1", "go on"),
                user_said("item-2", "continue"),
            ],
        );
        assert!(chat_runtime(&app, reopened).composer.is_empty());
    }

    /// A draft kept before what the reader said last was recorded settles
    /// by the latest message, as it did then.
    #[test]
    fn a_kept_draft_saved_without_a_position_settles_as_before() {
        let saved: KeptChatDraft =
            serde_json::from_str(r#"{"text":"continue","sending":true,"closed_at_unix_ms":1}"#)
                .unwrap();
        assert_eq!(saved.sent_after, SentAfter::Different);
    }

    /// Reopening the latest closed pane brings a never-started chat back with
    /// what was written in it, while a clone of an open chat starts empty.
    #[test]
    fn a_reopened_draft_comes_back_and_a_clone_starts_empty() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = super::super::tests::draft_chat_in(&mut app, Provider::Codex, project.path());
        write(&mut app, card, "an idea for later");
        close(&mut app, card);
        assert_eq!(
            app.durable.kept_chat_drafts[&card].text,
            "an idea for later"
        );

        drop(app.reopen_closed_pane(card));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "an idea for later"
        );
        assert!(app.durable.kept_chat_drafts.is_empty());

        let source = app.card_state(reopened).unwrap().clone();
        drop(app.duplicate_pane(source));
        let clone = app.workspace.focused().unwrap();
        assert_ne!(clone, reopened);
        assert!(chat_runtime(&app, clone).composer.is_empty());
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "an idea for later"
        );
    }

    /// Confirming the close of a chat still sending its first message
    /// discards that message, and only that: nothing else is kept when
    /// nothing else was written, and what was written since is.
    #[test]
    fn a_confirmed_first_message_discard_keeps_the_rest() {
        for written in [None, Some("and check the logs")] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (card, target) = super::super::tests::starting_chat_in(
                &mut app,
                Provider::Claude,
                Path::new("/workspace/chat-fixture"),
            );
            // Connected, so the composer takes more while the first message
            // is still on its way.
            let handle = fixture_handle(&app, card, Some("sending"));
            answer_start(&mut app, target, Ok(handle));
            assert!(app.chat_first_message_on_its_way(card).is_some());
            if let Some(written) = written {
                write(&mut app, card, written);
            }
            drop(app.update(Message::CloseCard(card)));
            assert!(app.chat_close_asked(card));
            let text = dialog_text(&app);
            assert!(text.contains("first message is still on its way"), "{text}");
            assert_eq!(
                text.contains(
                    "Anything else unsent comes back when you reopen this chat in this window."
                ),
                written.is_some(),
                "{text}"
            );
            close(&mut app, card);
            assert_eq!(
                app.durable
                    .kept_chat_drafts
                    .get(&card)
                    .map(|kept| kept.text.as_str()),
                written
            );
        }
    }

    /// A discard confirmed for one conversation, or by a close that didn't
    /// go ahead, answers nothing about a later close.
    #[test]
    fn a_confirmed_discard_does_not_outlive_its_question() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = chat_sending_its_first_message(&mut app);
        drop(app.update(Message::CloseCard(card)));
        drop(app.update(Message::CancelChatClose));
        assert!(!app.chat_ui.confirmed_losses.contains_key(&card));
        let session = app.chat_return_session(card).unwrap().id;
        app.confirm_close_loss(card, session, dialog_ui::Question::StopSendingFirstMessage);
        // A new close asks again, and starts from keeping.
        drop(app.update(Message::CloseCard(card)));
        assert!(app.chat_close_asked(card));
        assert!(!app.chat_ui.confirmed_losses.contains_key(&card));
        drop(app.update(Message::CancelChatClose));

        // An answer about another conversation, left by a close that was
        // refused, doesn't discard this one's draft.
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let live = live_chat_in(&mut app, Provider::Claude, project.path(), Some("kept"));
        write(&mut app, live, "keep me");
        app.confirm_close_loss(live, session, dialog_ui::Question::DiscardUnsentMessage);
        drop(app.begin_chat_close(live));
        finish_close(&mut app, live);
        assert_eq!(app.durable.kept_chat_drafts[&live].text, "keep me");
    }

    /// The edge case #2944's review left open: a first message still being
    /// sent while the chat's provider exits, or its connection drops, closes
    /// without a question. That state is reachable. What the provider never
    /// took is kept, so nothing is lost; what it took is in the conversation.
    #[test]
    fn a_first_message_its_stopped_provider_never_took_is_kept() {
        for (disconnect, delivered) in [(false, false), (true, false), (true, true)] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (card, target) = super::super::tests::starting_chat_in(
                &mut app,
                Provider::Claude,
                Path::new("/workspace/chat-fixture"),
            );
            let handle = fixture_handle(&app, card, Some("stopped"));
            let mut snapshot = (*handle.snapshot()).clone();
            if delivered {
                snapshot.items = vec![Arc::new(chat::Item {
                    id: "user-1".into(),
                    role: chat::Role::User,
                    at: None,
                    started_at: None,
                    content: ItemContent::Text {
                        text: "hello".into(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                })];
            }
            let handle = Arc::new(ClientHandle::fixture(handle.session(), snapshot));
            answer_start(&mut app, target, Ok(handle.clone()));
            assert!(app.chat_first_message_on_its_way(card).is_some());
            let mut stopped = (*handle.snapshot()).clone();
            if disconnect {
                if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
                    runtime.disconnected = true;
                }
            } else {
                stopped.exited = true;
                handle.publish_snapshot_for_test(stopped);
            }
            assert_eq!(
                app.chat_working_session(card),
                None,
                "disconnected {disconnect}: the close asks nothing"
            );
            drop(app.update(Message::CloseCard(card)));
            assert!(!app.chat_close_asked(card));
            close(&mut app, card);
            let kept = app
                .durable
                .kept_chat_drafts
                .get(&card)
                .map(|kept| kept.text.as_str());
            assert_eq!(
                kept,
                (!delivered).then_some("hello"),
                "disconnected {disconnect}, delivered {delivered}"
            );
        }
    }

    #[test]
    fn a_window_keeps_drafts_for_its_newest_recently_closed_chats() {
        let now = KeptChatDraft::RETENTION_MS * 2;
        let draft = |closed_at_unix_ms| KeptChatDraft {
            text: "draft".into(),
            closed_at_unix_ms,
            ..KeptChatDraft::default()
        };
        let mut drafts = (0..KeptChatDraft::LIMIT + 3)
            .map(|index| (CardId::new(), draft(now - index as i64)))
            .collect::<HashMap<_, _>>();
        drafts.insert(CardId::new(), KeptChatDraft::default());
        drafts.insert(CardId::new(), draft(now - KeptChatDraft::RETENTION_MS - 1));
        KeptChatDraft::retain_recent(&mut drafts, now);
        assert_eq!(drafts.len(), KeptChatDraft::LIMIT);
        assert!(drafts.values().all(|draft| {
            draft.closed_at_unix_ms > now - KeptChatDraft::LIMIT as i64 && !draft.is_empty()
        }));
    }
}
