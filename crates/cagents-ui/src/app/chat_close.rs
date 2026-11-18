//! Closing a pane closes it in every window and stops what it runs, so a chat
//! that is still working asks first, as a terminal running a process does. The
//! window may not be the one that started it, or know how far it has got.
//! The conversation stays in history, and this window keeps anything it had
//! not sent for when it reopens (see `chat_ui::kept_draft`), so this is a
//! routine question: Enter answers it, and the person may choose not to be
//! asked again. Closing a chat whose first message hasn't arrived discards
//! that message, which nothing brings back, so those questions are always
//! asked and open on their dismissal: one whose message is still on its way,
//! and one whose message couldn't start and is kept unsent in the composer.

use super::*;

const CANCEL: &str = "dialog.chat-close.cancel";
const CONFIRM: &str = "dialog.chat-close.confirm";
const QUESTION: settings::Confirmation = settings::Confirmation::StopWorkingChat;
const _: () = assert!(matches!(
    dialog_ui::Question::StopWorkingChat.skippable(),
    Some(QUESTION)
));
/// How much of a first message can stand in for a conversation's name.
const FIRST_MESSAGE_NAME_CHARS: usize = 60;

#[derive(Debug, Clone)]
pub(super) struct ChatCloseConfirm {
    card: CardId,
    session: TerminalId,
    /// False when this window cannot tell, as while it is still attaching.
    certain: bool,
    /// Decided when the question opens, so the answer it starts on doesn't
    /// change under the person reading it.
    question: dialog_ui::Question,
    /// Whether closing keeps anything unsent for a reopened conversation,
    /// which the question then says.
    keeps_unsent: bool,
    restore_card: Option<CardId>,
    restore_keyboard_nav: bool,
}

impl Cagents {
    /// A close the reader asked for, from anywhere a chat pane can be closed.
    pub(super) fn request_chat_close(&mut self, card: CardId) -> Task<Message> {
        self.reset_shared_removal_for_new_close(card);
        // A discard confirmed for an earlier close that didn't go ahead
        // answers nothing about this one.
        self.forget_close_loss(card);
        self.continue_chat_close(card)
    }

    pub(super) fn continue_chat_close(&mut self, card: CardId) -> Task<Message> {
        // Closing a fenced pane stops nothing, so there is nothing to ask.
        if let Some(closed) = self.close_fenced_card(card) {
            return closed;
        }
        if !self.prepare_shared_removal(card) {
            return Task::none();
        }
        self.chat_close_question(card)
            .unwrap_or_else(|| self.begin_chat_close(card))
    }

    /// The task holding this close behind the question, if it must wait for an
    /// answer. Another close arriving during a question waits its turn.
    pub(super) fn chat_close_question(&mut self, card: CardId) -> Option<Task<Message>> {
        if self.chat_close_asked(card) {
            return Some(Task::none());
        }
        // A question asked beside a close under way could end up under that
        // close's own dialog. A chat with nothing to ask doesn't have to wait.
        if self.chat_close_dialog_visible()
            || (self.chat_close_would_ask(card).is_some() && self.another_close_under_way(card))
        {
            self.queue_card_close(card, false);
            return Some(Task::none());
        }
        self.ask_before_chat_close(card)
    }

    /// What a close of this card would ask, about which conversation, and
    /// whether this window is sure it is working. A close that loses a first
    /// message is asked about whatever the stop-question setting says: that
    /// setting governs stopping work, not losing text.
    fn chat_close_would_ask(
        &self,
        card: CardId,
    ) -> Option<(dialog_ui::Question, TerminalId, bool)> {
        if let Some(session) = self.blocked_draft_discard(card) {
            return Some((dialog_ui::Question::DiscardUnsentMessage, session, true));
        }
        let (session, certain) = self.chat_working_session(card)?;
        if self.chat_first_message_on_its_way(card).is_some() {
            Some((
                dialog_ui::Question::StopSendingFirstMessage,
                session,
                certain,
            ))
        } else {
            self.asks(QUESTION)
                .then_some((dialog_ui::Question::StopWorkingChat, session, certain))
        }
    }

    pub(super) fn ask_before_chat_close(&mut self, card: CardId) -> Option<Task<Message>> {
        let (question, session, certain) = self.chat_close_would_ask(card)?;
        if !self.prepare_shared_removal(card) {
            return Some(Task::none());
        }
        let restore_keyboard_nav = self.core.keyboard_nav();
        let keeps_unsent = self.close_keeps_unsent(card, question);
        self.chat_close_confirm = Some(ChatCloseConfirm {
            card,
            session,
            certain,
            question,
            keeps_unsent,
            restore_card: self.workspace.focused(),
            restore_keyboard_nav,
        });
        self.core.set_keyboard_nav(false);
        Some(self.open_question(question, widget::Id::new(CANCEL), widget::Id::new(CONFIRM)))
    }

    /// Whether the conversation asked about is still the one on the card. One
    /// closed or replaced meanwhile leaves nothing to answer.
    fn chat_close_target_current(&self, intent: &ChatCloseConfirm) -> bool {
        self.card_state(intent.card).is_some_and(|state| {
            state.existence.is_open()
                && (intent.question != dialog_ui::Question::DiscardUnsentMessage
                    || self.blocked_draft_discard(intent.card) == Some(intent.session))
                && matches!(&state.kind, CardKind::Chat(session) if session.id == intent.session)
        })
    }

    pub(super) fn chat_close_asked(&self, card: CardId) -> bool {
        self.chat_close_confirm
            .as_ref()
            .is_some_and(|intent| intent.card == card)
    }

    pub(super) fn chat_close_dialog_visible(&self) -> bool {
        self.chat_close_confirm
            .as_ref()
            .is_some_and(|intent| self.chat_close_target_current(intent))
    }

    /// A popup that opened or closed around the question recorded its own
    /// keyboard state; the question restores the reader's.
    pub(super) fn adopt_chat_close_keyboard_nav(&mut self, restore: bool) -> bool {
        match self.chat_close_confirm.as_mut() {
            Some(intent) => {
                intent.restore_keyboard_nav = restore;
                true
            }
            None => false,
        }
    }

    /// Ends the question once its conversation has gone, so no modal state
    /// outlives the dialog it belonged to, and lets closes waiting on it go on.
    pub(super) fn settle_chat_close_confirm(&mut self) -> Option<Task<Message>> {
        if self.chat_close_confirm.is_none() || self.chat_close_dialog_visible() {
            return None;
        }
        drop(self.finish_chat_close_question());
        self.dismiss_confirmation(QUESTION);
        Some(self.next_queued_card_close())
    }

    fn finish_chat_close_question(&mut self) -> Option<ChatCloseConfirm> {
        let intent = self.chat_close_confirm.take()?;
        self.core.set_keyboard_nav(intent.restore_keyboard_nav);
        Some(intent)
    }

    pub(super) fn cancel_chat_close(&mut self) -> Task<Message> {
        let Some(intent) = self.finish_chat_close_question() else {
            return Task::none();
        };
        self.dismiss_confirmation(QUESTION);
        self.shared_views.removals.remove(&intent.card);
        let card = intent
            .restore_card
            .filter(|card| self.workspace.order().any(|id| id == *card))
            .unwrap_or(intent.card);
        let _ = self.workspace.focus(card);
        self.focus_pane_keyboard(card)
    }

    pub(super) fn confirm_chat_close(&mut self) -> Task<Message> {
        let current = self.chat_close_dialog_visible();
        let Some(intent) = self.finish_chat_close_question() else {
            return Task::none();
        };
        if current {
            // Only a close that goes ahead keeps a tick in "Don't ask again";
            // one whose conversation changed meanwhile answered nothing.
            // A question about losing a message has no "Don't ask again".
            let remember = if let Some(confirmation) = intent.question.skippable() {
                self.commit_confirmation(confirmation)
            } else {
                self.dismiss_confirmation(QUESTION);
                Task::none()
            };
            // A message the reader chose to discard is not kept for a
            // reopened conversation; the rest of what it had not sent is.
            self.confirm_close_loss(intent.card, intent.session, intent.question);
            Task::batch([remember, self.begin_chat_close(intent.card)])
        } else {
            self.dismiss_confirmation(QUESTION);
            Task::none()
        }
    }

    pub(super) fn chat_close_dialog(&self) -> Element<'_, Message> {
        let intent = self.chat_close_confirm.as_ref();
        // Several closes can ask in turn, so each says which conversation it means.
        // A conversation still sending its first message has no name of its
        // own yet; that message is what the reader knows it by.
        let first_message =
            intent.and_then(|intent| self.chat_first_message_on_its_way(intent.card));
        let name = first_message
            .as_deref()
            .and_then(|message| message.lines().next())
            .map(
                |line| match line.char_indices().nth(FIRST_MESSAGE_NAME_CHARS) {
                    Some((end, _)) => format!("{}\u{2026}", &line[..end]),
                    None => line.to_owned(),
                },
            )
            .or_else(|| {
                intent
                    .and_then(|intent| self.card_chrome.get(&intent.card))
                    .map(|chrome| chrome.title.to_string())
            });
        let question = intent.map_or(dialog_ui::Question::StopWorkingChat, |intent| {
            intent.question
        });
        let discard_draft = question == dialog_ui::Question::DiscardUnsentMessage;
        let title = if discard_draft {
            "Discard this message?".to_owned()
        } else {
            name.map_or_else(
                || "Stop this conversation?".to_owned(),
                |name| format!("Stop \u{201c}{name}\u{201d}?"),
            )
        };
        let body = if discard_draft {
            "Closing this tab discards your unsent message."
        } else if first_message.is_some() {
            "Its first message is still on its way. Closing it stops the conversation, discards that message and closes it in every window."
        } else if intent.is_some_and(|intent| intent.certain) {
            "It is still working. Closing it stops it and closes it in every window."
        } else {
            "It may still be working. Closing it stops it and closes it in every window."
        };
        // What isn't lost, the reader should know where to find.
        let kept = intent
            .filter(|intent| intent.keeps_unsent)
            .and(match question {
                dialog_ui::Question::StopWorkingChat => {
                    Some("Unsent and queued messages come back when you reopen this chat in this window.")
                }
                dialog_ui::Question::StopSendingFirstMessage => Some(
                    "Anything else unsent comes back when you reopen this chat in this window.",
                ),
                _ => None,
            });
        dialog_ui::overlay(
            dialog_ui::confirmation_surface(
                question,
                widget::column::with_children(
                    [
                        Some(crate::design::title(title).into()),
                        Some(crate::design::body(body).into()),
                        kept.map(|kept| crate::design::body(kept).into()),
                        Some(match question.skippable() {
                            Some(confirmation) => self.dont_ask_again_control(confirmation, true),
                            None => iced::widget::Space::new().into(),
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
                )
                .spacing(space::M),
                dialog_ui::confirmation_actions(
                    crate::ui_controls::button::secondary(if discard_draft {
                        "Cancel"
                    } else {
                        "Keep working"
                    })
                    .id(widget::Id::new(CANCEL))
                    .on_press(Message::CancelChatClose),
                    crate::ui_controls::button::destructive(if discard_draft {
                        "Discard"
                    } else {
                        "Stop and close"
                    })
                    .id(widget::Id::new(CONFIRM))
                    .on_press(Message::ConfirmChatClose),
                ),
                560.0,
            ),
            Some(Message::CancelChatClose),
            1.0,
        )
    }
}
