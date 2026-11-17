//! Closing a chat's card leaves its conversation in history, so the reader
//! can open it again, as a new card. What they had written but not sent goes
//! with it: the composer's text and attachments, a first message or
//! follow-ups held for the next turn, and what the provider was still
//! holding when the close stopped it. The card that reopens the
//! conversation takes all of it back into its composer.
//!
//! The window keeps this with its workspace, so it survives a restart, for
//! the newest [`ClosedChatDraft::LIMIT`] closed chats.
use super::*;
use crate::model::ClosedChatDraft;

impl Cagents {
    /// What closing `card` now would keep for its conversation to reopen
    /// with. Empty when it is not a chat or has nothing unsent.
    pub(in crate::app) fn closed_chat_draft(&self, card: CardId) -> ClosedChatDraft {
        if !matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Chat(_))
        ) {
            return ClosedChatDraft::default();
        }
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        let (text, attachments) = match runtime {
            Some(runtime) => runtime.kept_draft(),
            // A card without a runtime holds its draft where the last one left it.
            None => (
                self.chat_ui.drafts.get(&card).cloned().unwrap_or_default(),
                self.chat_ui
                    .draft_attachments
                    .get(&card)
                    .cloned()
                    .unwrap_or_default(),
            ),
        };
        ClosedChatDraft {
            text,
            attachments,
            waiting: self.chat_waiting_to_save(card).messages,
            closed_at_unix_ms: 0,
        }
    }

    /// Keeps what `card`'s chat had not sent, as its card closes and before
    /// its runtime is dropped. A close the reader confirmed would discard
    /// the message keeps nothing.
    pub(in crate::app) fn keep_closed_chat_draft(&mut self, card: CardId) {
        let discard = self.chat_ui.discard_on_close.remove(&card);
        let draft = if discard {
            ClosedChatDraft::default()
        } else {
            ClosedChatDraft {
                closed_at_unix_ms: unix_millis(),
                ..self.closed_chat_draft(card)
            }
        };
        if draft.is_empty() {
            if self.durable.closed_chat_drafts.contains_key(&card) {
                Arc::make_mut(&mut self.durable)
                    .closed_chat_drafts
                    .remove(&card);
            }
            return;
        }
        let drafts = &mut Arc::make_mut(&mut self.durable).closed_chat_drafts;
        drafts.insert(card, draft);
        ClosedChatDraft::retain_newest(drafts);
    }

    /// Takes back the draft kept when `closed` closed, to be moved with
    /// [`Self::move_chat_draft`] into the card reopening its conversation.
    /// Whether there was one.
    pub(in crate::app) fn reclaim_closed_chat_draft(&mut self, closed: CardId) -> bool {
        if !self.durable.closed_chat_drafts.contains_key(&closed) {
            return false;
        }
        let Some(draft) = Arc::make_mut(&mut self.durable)
            .closed_chat_drafts
            .remove(&closed)
        else {
            return false;
        };
        if !draft.text.is_empty() {
            self.chat_ui.drafts.insert(closed, draft.text);
        }
        if !draft.attachments.is_empty() {
            self.chat_ui
                .draft_attachments
                .insert(closed, draft.attachments);
        }
        if !draft.waiting.is_empty() {
            self.chat_ui.reopened_waiting.insert(closed, draft.waiting);
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
        if let Some(waiting) = self.chat_ui.reopened_waiting.remove(&source) {
            self.chat_ui.reopened_waiting.insert(card, waiting);
        }
    }

    /// Lets a close the reader confirmed discard `card`'s unsent message.
    pub(in crate::app) fn discard_draft_on_close(&mut self, card: CardId, discard: bool) {
        if discard {
            self.chat_ui.discard_on_close.insert(card);
        } else {
            self.chat_ui.discard_on_close.remove(&card);
        }
    }
}

impl Runtime {
    /// What closing this conversation's card would keep of what the reader
    /// has not sent. A first message the provider already took is in the
    /// conversation, so it is not kept again.
    fn kept_draft(&self) -> (String, Vec<Attachment>) {
        let delivered = self
            .handle()
            .is_some_and(|handle| handle.has_accepted_user_input());
        self.unsent_with_held(self.queued.as_ref().filter(|_| !delivered))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        answer_start, chat_runtime, chat_sending_its_first_message, fixture_handle, live_chat_in,
        write,
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
        if let Some(Phase::Closing {
            target,
            restart: None,
        }) = app.chat_ui.phases.get(&card)
        {
            let target = *target;
            let source = app.chat_return_session(card).unwrap();
            drop(app.update(Message::ChatUi(Action::RetainedCloseFinished(
                target,
                source,
                Ok(()),
            ))));
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
            text.contains("What you haven't sent yet comes back when you reopen it."),
            "{text}"
        );
        close(&mut app, card);

        let kept = app.durable.closed_chat_drafts[&card].clone();
        assert_eq!(kept.text, "held for the next turn\nstill writing");
        assert_eq!(
            kept.waiting,
            vec![queued("q-1", "queued with the provider")]
        );
        assert!(kept.closed_at_unix_ms > 0);
        assert_eq!(
            app.persistent_workspace().closed_chat_drafts[&card],
            kept,
            "it is saved with the workspace"
        );

        drop(app.resume_saved_conversation(saved(&app, card)));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert!(app.durable.closed_chat_drafts.is_empty(), "taken back once");
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

        drop(app.resume_saved_conversation(saved(&app, card)));
        let reopened = app.workspace.focused().unwrap();
        let target = app.chat_ui.attempt(reopened).unwrap().target;
        let handle = fixture_handle(&app, reopened, Some("kept"));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items = vec![Arc::new(chat::Item {
            id: "q-1".into(),
            role: chat::Role::User,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: "it got there".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        })];
        handle.publish_snapshot_for_test(snapshot);
        answer_start(&mut app, target, Ok(handle));
        let runtime = chat_runtime(&app, reopened);
        assert!(runtime.composer.is_empty());
        assert_eq!(runtime.restored_waiting, None);
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
            app.durable.closed_chat_drafts[&card].text,
            "an idea for later"
        );

        drop(app.reopen_closed_pane(card));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert_eq!(
            chat_runtime(&app, reopened).composer.text().trim_end(),
            "an idea for later"
        );
        assert!(app.durable.closed_chat_drafts.is_empty());

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

    /// Answering "Discard" to a question about losing the first message
    /// discards it: nothing is kept to come back.
    #[test]
    fn a_confirmed_discard_keeps_nothing() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = chat_sending_its_first_message(&mut app);
        drop(app.update(Message::CloseCard(card)));
        assert!(app.chat_close_asked(card));
        let text = dialog_text(&app);
        assert!(text.contains("first message is still on its way"), "{text}");
        assert!(!text.contains("comes back"), "{text}");
        close(&mut app, card);
        assert!(!app.durable.closed_chat_drafts.contains_key(&card));
    }

    /// A Cancel leaves no discard behind for a later close to act on.
    #[test]
    fn a_cancelled_discard_does_not_outlive_its_question() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = chat_sending_its_first_message(&mut app);
        drop(app.update(Message::CloseCard(card)));
        drop(app.update(Message::CancelChatClose));
        assert!(!app.chat_ui.discard_on_close.contains(&card));
        app.discard_draft_on_close(card, true);
        // A new close asks again, and starts from keeping.
        drop(app.update(Message::CloseCard(card)));
        assert!(!app.chat_ui.discard_on_close.contains(&card));
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
                .closed_chat_drafts
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
    fn a_window_keeps_drafts_for_its_newest_closed_chats() {
        let mut drafts = (0..ClosedChatDraft::LIMIT + 3)
            .map(|index| {
                (
                    CardId::new(),
                    ClosedChatDraft {
                        text: format!("draft {index}"),
                        closed_at_unix_ms: index as i64,
                        ..ClosedChatDraft::default()
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        drafts.insert(CardId::new(), ClosedChatDraft::default());
        ClosedChatDraft::retain_newest(&mut drafts);
        assert_eq!(drafts.len(), ClosedChatDraft::LIMIT);
        assert!(
            drafts
                .values()
                .all(|draft| draft.closed_at_unix_ms >= 3 && !draft.is_empty())
        );
    }
}
