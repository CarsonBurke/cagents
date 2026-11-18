//! Closing a chat takes its pane away at once. Stopping the conversation, which
//! the daemon does by asking its provider to end and proving it gone, belongs
//! to a job the pane does not wait for: the job holds what the pane closed and
//! answers for it when the daemon does.
//!
//! A close the daemon refuses leaves the conversation running with no pane, so
//! the refusal is reported with a way back to it. Its next close is the
//! reader's attempt to deal with what held it, and so waits on its pane, where
//! a second refusal is shown beside whatever it can end.
//!
//! While a conversation is stopping, a reopen of it waits for the stop to
//! settle, so no pane starts or attaches to a provider that is being ended.
use super::*;
use crate::persistence::SavedConversation;

/// A conversation stopping in the background after its pane closed.
#[derive(Debug)]
pub(in crate::app) struct Closing {
    /// The conversation as its pane closed.
    session: chat::Session,
    /// The card it closed from, which keeps its unsent text.
    card: CardId,
    /// What its pane was called, to name it if the close is refused.
    title: String,
    /// Reopens asked for while it stopped, which go ahead once it settles.
    reopens: Vec<SavedConversation>,
}

/// A close the daemon refused after its pane had gone.
#[derive(Debug, Clone)]
pub(in crate::app) struct Failure {
    session: chat::Session,
    card: CardId,
    title: String,
    reason: String,
}

/// Whether `current` is `closed` with nothing changed but what its provider has
/// confirmed since: the conversation's identity and how its provider is kept.
pub(super) fn only_confirms(closed: &chat::Session, current: &chat::Session) -> bool {
    if closed
        .launch
        .session_id
        .as_ref()
        .is_some_and(|id| current.launch.session_id.as_ref() != Some(id))
    {
        return false;
    }
    let mut learned = closed.clone();
    learned.launch.session_id = current.launch.session_id.clone();
    learned.learn_native_continuity_from(current);
    &learned == current
}

/// Whether two descriptors name one conversation: the same pane identity, or
/// the same provider conversation.
fn shares_conversation(a: &chat::Session, b: &chat::Session) -> bool {
    a.id == b.id
        || (a.launch.provider == b.launch.provider
            && a.launch.session_id.is_some()
            && a.launch.session_id == b.launch.session_id)
}

impl Cagents {
    /// Saves what the provider has confirmed about the conversation in `card`
    /// before it closes, so the closed pane, and anything reopening it,
    /// resume that conversation rather than the one the card was saved with.
    pub(super) fn install_closing_identity(&mut self, card: CardId, session: &chat::Session) {
        let confirms = matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Chat(saved)) if saved != session && only_confirms(saved, session)
        );
        if confirms {
            let _ = self.install_chat_session(card, session.clone());
        }
    }

    /// Stops the conversation a closed pane held, without the pane.
    pub(super) fn close_chat_in_background(
        &mut self,
        card: CardId,
        handle: Option<Arc<ClientHandle>>,
        session: chat::Session,
        title: String,
    ) -> Task<Message> {
        let target = self.chat_ui.next_target(card);
        self.chat_ui.background_closes.insert(
            target,
            Closing {
                session: session.clone(),
                card,
                title,
                reopens: Vec::new(),
            },
        );
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || close_conversation(handle, Some(session)))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            Message::ChatUi(Action::Retired(target, result))
        })
    }

    /// The daemon's answer to a background close.
    pub(super) fn background_close_finished(
        &mut self,
        target: Target,
        result: Result<(), String>,
    ) -> Task<Message> {
        let Some(closing) = self.chat_ui.background_closes.remove(&target) else {
            return Task::none();
        };
        let id = closing.session.id;
        let refused = match result {
            Ok(()) => {
                self.chat_ui.close_refused.remove(&id);
                false
            }
            Err(reason) => {
                tracing::warn!(%reason, "the daemon refused a closed chat's close");
                self.chat_ui.close_refused.insert(id);
                self.chat_ui
                    .close_failures
                    .retain(|failure| failure.session.id != id);
                self.chat_ui.close_failures.push(Failure {
                    session: closing.session,
                    card: closing.card,
                    title: closing.title,
                    reason,
                });
                true
            }
        };
        if self.exit.is_some() {
            // Nothing reopens on the way out; the exit only waited to hear.
            if self.chat_ui.background_closes.is_empty() {
                return self.advance_exit(|exit| exit.chats_closed = true);
            }
            return Task::none();
        }
        if closing.reopens.is_empty() {
            return Task::none();
        }
        if refused {
            // The conversation is still running, so the reopen asked for goes
            // back to it, on a pane that says why it could not be stopped.
            return self.reopen_refused_close(id);
        }
        Task::batch(
            closing
                .reopens
                .into_iter()
                .map(|entry| self.resume_saved_conversation(entry))
                .collect::<Vec<_>>(),
        )
    }

    /// Holds a reopen of a conversation that is still stopping until it has
    /// stopped, or the daemon has refused to stop it. True when held.
    pub(in crate::app) fn defer_reopen_while_closing(&mut self, entry: &SavedConversation) -> bool {
        let Some(closing) = self
            .chat_ui
            .background_closes
            .values_mut()
            .find(|closing| shares_conversation(&closing.session, &entry.session))
        else {
            return false;
        };
        if !closing
            .reopens
            .iter()
            .any(|held| shares_conversation(&held.session, &entry.session))
        {
            closing.reopens.push(entry.clone());
        }
        true
    }

    /// Whether a refused conversation reopening on a new pane should attach to
    /// the provider the daemon kept for it rather than start another.
    pub(super) fn reopens_refused_close(&self, session: &chat::Session) -> bool {
        self.chat_ui.refusals_to_show.contains_key(&session.id)
    }

    /// Shows a refused close on the pane its conversation reopened on.
    pub(super) fn show_refusal_on_reopen(&mut self, card: CardId) -> Task<Message> {
        let Some(session) = self.chat_return_session(card) else {
            return Task::none();
        };
        let Some(reason) = self.chat_ui.refusals_to_show.remove(&session.id) else {
            return Task::none();
        };
        self.refuse_chat_close(card, session, reason)
    }

    pub(super) fn reopen_refused_close(&mut self, id: TerminalId) -> Task<Message> {
        let Some(index) = self
            .chat_ui
            .close_failures
            .iter()
            .position(|failure| failure.session.id == id)
        else {
            return Task::none();
        };
        let failure = self.chat_ui.close_failures.remove(index);
        if let Some(card) = self.existing_conversation_card(&failure.session) {
            return cosmic::task::message(Message::FocusCard(card));
        }
        self.chat_ui.refusals_to_show.insert(id, failure.reason);
        // It comes back as it closed, with what it had not sent.
        self.recently_closed
            .retain(|(card, _)| card.id != failure.card);
        let user_title = self.chrome_ui.user_titles.get(&failure.card).cloned();
        let draft_from = self
            .reclaim_kept_draft(failure.card)
            .then_some(failure.card);
        self.insert_resumed_chat_card(failure.session, None, user_title, draft_from)
    }

    pub(super) fn dismiss_close_failure(&mut self, id: TerminalId) {
        self.chat_ui
            .close_failures
            .retain(|failure| failure.session.id != id);
    }

    pub(in crate::app) fn has_close_failure_notices(&self) -> bool {
        !self.chat_ui.close_failures.is_empty()
    }

    /// Each refused close, in the application notices, with the way back to
    /// the conversation it left running.
    pub(in crate::app) fn close_failure_notice_views(&self) -> Vec<Element<'_, Message>> {
        let severity = NoticeSeverity::Error;
        self.chat_ui
            .close_failures
            .iter()
            .map(|failure| {
                let id = failure.session.id;
                let title = if failure.title.is_empty() {
                    "A closed chat is still running".to_owned()
                } else {
                    format!("“{}” is still running", failure.title)
                };
                let actions = widget::row::with_children(vec![
                    notice_surface_action(
                        severity,
                        "Reopen",
                        "Reopen the conversation that couldn't be stopped",
                        widget::Id::new(format!("chat-close-failure.reopen.{id}")),
                        Message::ChatUi(Action::ReopenRefusedClose(id)),
                    ),
                    notice_surface_action(
                        severity,
                        "Dismiss",
                        "Dismiss application notice",
                        widget::Id::new(format!("chat-close-failure.dismiss.{id}")),
                        Message::ChatUi(Action::DismissCloseFailure(id)),
                    ),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Center);
                notice_surface(
                    severity,
                    widget::column::with_children(vec![
                        notice_body(format!(
                            "{title}: its pane closed, but Cagents couldn't stop it. Reopen it to see what holds it."
                        )),
                        notice_surface_details(severity, &failure.reason),
                    ])
                    .spacing(space::XS)
                    .width(Length::Fill)
                    .into(),
                    Some(actions.into()),
                    false,
                )
            })
            .collect()
    }
}

#[cfg(test)]
impl Cagents {
    /// Records that the daemon refused `card`'s last close, so its next close
    /// waits on its pane, as a reader's retry after a refusal does.
    pub(in crate::app) fn refuse_chat_close_once_for_test(&mut self, card: CardId) {
        let id = self.chat_return_session(card).expect("a chat").id;
        self.chat_ui.close_refused.insert(id);
    }
}
