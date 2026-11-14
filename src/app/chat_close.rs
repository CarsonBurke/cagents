//! Closing a pane closes it in every window and stops what it runs, so a chat
//! that is still working asks first, as a terminal running a process does. The
//! window may not be the one that started it, or know how far it has got.
//! The conversation stays in history, so this is a routine question: Enter
//! answers it, and the person may choose not to be asked again.

use super::*;

const CANCEL: &str = "dialog.chat-close.cancel";
const CONFIRM: &str = "dialog.chat-close.confirm";
const QUESTION: settings::Confirmation = settings::Confirmation::StopWorkingChat;
/// How much of a first message can stand in for a conversation's name.
const FIRST_MESSAGE_NAME_CHARS: usize = 60;

#[derive(Debug, Clone)]
pub(super) struct ChatCloseConfirm {
    card: CardId,
    session: TerminalId,
    /// False when this window cannot tell, as while it is still attaching.
    certain: bool,
    restore_card: Option<CardId>,
    restore_keyboard_nav: bool,
}

impl Cagents {
    /// A close the reader asked for, from anywhere a chat pane can be closed.
    pub(super) fn request_chat_close(&mut self, card: CardId) -> Task<Message> {
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

    /// The working conversation a close of this card would ask about, unless
    /// the person chose not to be asked.
    fn chat_close_would_ask(&self, card: CardId) -> Option<(TerminalId, bool)> {
        self.asks(QUESTION)
            .then(|| self.chat_working_session(card))
            .flatten()
    }

    pub(super) fn ask_before_chat_close(&mut self, card: CardId) -> Option<Task<Message>> {
        let (session, certain) = self.chat_close_would_ask(card)?;
        let restore_keyboard_nav = self.core.keyboard_nav();
        self.chat_close_confirm = Some(ChatCloseConfirm {
            card,
            session,
            certain,
            restore_card: self.workspace.focused(),
            restore_keyboard_nav,
        });
        self.core.set_keyboard_nav(false);
        Some(self.open_confirmation(QUESTION, widget::Id::new(CANCEL), widget::Id::new(CONFIRM)))
    }

    /// Whether the conversation asked about is still the one on the card. One
    /// closed or replaced meanwhile leaves nothing to answer.
    fn chat_close_target_current(&self, intent: &ChatCloseConfirm) -> bool {
        self.card_state(intent.card).is_some_and(|state| {
            state.existence.is_open()
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
            let remember = self.commit_confirmation(QUESTION);
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
        let title = name.map_or_else(
            || "Stop this conversation?".to_owned(),
            |name| format!("Stop \u{201c}{name}\u{201d}?"),
        );
        let body = if first_message.is_some() {
            "Its first message is still on its way. Closing it stops the conversation, discards that message and closes it in every window."
        } else if intent.is_some_and(|intent| intent.certain) {
            "It is still working. Closing it stops it and closes it in every window."
        } else {
            "It may still be working. Closing it stops it and closes it in every window."
        };
        dialog_ui::overlay(
            dialog_ui::confirmation_surface(
                widget::column::with_children(vec![
                    crate::design::title(title).into(),
                    crate::design::body(body).into(),
                    self.dont_ask_again_control(QUESTION, true),
                    dialog_ui::confirmation_actions(
                        crate::ui_controls::button::secondary("Keep working")
                            .id(widget::Id::new(CANCEL))
                            .on_press(Message::CancelChatClose),
                        crate::ui_controls::button::destructive("Stop and close")
                            .id(widget::Id::new(CONFIRM))
                            .on_press(Message::ConfirmChatClose),
                    ),
                ])
                .spacing(space::M),
                560.0,
            ),
            Some(Message::CancelChatClose),
            1.0,
        )
    }
}
