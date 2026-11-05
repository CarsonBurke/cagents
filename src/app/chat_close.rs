//! Closing a pane closes it in every window and stops what it runs, so a chat
//! that is still working asks first, as a terminal running a process does. The
//! window may not be the one that started it, or know how far it has got.

use super::*;

const CANCEL: &str = "dialog.chat-close.cancel";
const CONFIRM: &str = "dialog.chat-close.confirm";

#[derive(Debug, Clone)]
pub(super) struct ChatCloseConfirm {
    card: CardId,
    session: TerminalId,
    focus_confirm: bool,
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
        if self.chat_close_dialog_visible() {
            self.queued_card_closes.push_back(terminal_close::QueuedCardClose {
                card,
                unasked: false,
            });
            return Some(Task::none());
        }
        self.ask_before_chat_close(card)
    }

    fn ask_before_chat_close(&mut self, card: CardId) -> Option<Task<Message>> {
        let session = self.chat_working_session(card)?;
        let restore_keyboard_nav = self.core.keyboard_nav();
        self.chat_close_confirm = Some(ChatCloseConfirm {
            card,
            session,
            focus_confirm: false,
            restore_keyboard_nav,
        });
        self.core.set_keyboard_nav(false);
        Some(chat_close_focus(false))
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

    /// Ends the question once its conversation has gone, so no modal state
    /// outlives the dialog it belonged to.
    pub(super) fn settle_chat_close_confirm(&mut self) {
        if self.chat_close_confirm.is_some() && !self.chat_close_dialog_visible() {
            drop(self.finish_chat_close_question());
        }
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
        iced::widget::operation::focus(self.card_keyboard_focus(intent.card))
            .map(cosmic::Action::App)
            .chain(self.next_queued_card_close())
    }

    pub(super) fn confirm_chat_close(&mut self) -> Task<Message> {
        let current = self.chat_close_dialog_visible();
        let Some(intent) = self.finish_chat_close_question() else {
            return Task::none();
        };
        let close = if current {
            self.begin_chat_close(intent.card)
        } else {
            Task::none()
        };
        close.chain(self.next_queued_card_close())
    }

    pub(super) fn cycle_chat_close_focus(&mut self) -> Task<Message> {
        let Some(intent) = self.chat_close_confirm.as_mut() else {
            return Task::none();
        };
        intent.focus_confirm = !intent.focus_confirm;
        chat_close_focus(intent.focus_confirm)
    }

    pub(super) fn chat_close_dialog(&self) -> Element<'_, Message> {
        dialog_ui::overlay(
            dialog_ui::surface(
                widget::column::with_children(vec![
                    crate::design::title("Stop this conversation?").into(),
                    crate::design::body(
                        "It is still working. Closing it stops it and closes it in every window.",
                    )
                    .into(),
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

fn chat_close_focus(confirm: bool) -> Task<Message> {
    iced::widget::operation::focus(widget::Id::new(if confirm { CONFIRM } else { CANCEL }))
        .map(cosmic::Action::App)
}
