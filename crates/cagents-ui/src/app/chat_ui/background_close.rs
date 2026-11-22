//! Closing a chat takes its pane away at once. Stopping the conversation, which
//! the daemon does by asking its provider to end and proving it gone, belongs
//! to a job the pane does not wait for: the job holds what the pane closed and
//! answers for it when the daemon does.
//!
//! The window saves each close until the daemon has answered it, so a close
//! the window did not stay to hear is asked again on its next launch, and a
//! refusal is still reported then. A close the daemon refuses leaves the
//! conversation running with no pane, so the refusal is reported with a way
//! back to it. Its next close is the reader's attempt to deal with what held
//! it, and so waits on its pane, where a second refusal is shown beside
//! whatever it can end.
//!
//! A conversation reopened while it is stopping gets its pane at once, which
//! waits to attach until the stop settles, so no pane starts or attaches to a
//! provider that is being ended.
//!
//! A shared view can put the closed pane itself back while its conversation
//! stops: the registry refuses a removal it judges against a conversation
//! still running, which the stop then ends. That pane closed, so it stays out
//! of sight, and no one is told to close it again: once the stop settles it
//! goes, or comes back if the stop was refused.
use super::*;
use crate::model::StoppingChat;
use crate::persistence::SavedConversation;

/// A conversation stopping in the background after its pane closed.
#[derive(Debug)]
pub(in crate::app) struct Closing {
    /// The pane that closed.
    card: CardId,
    /// The conversation as its pane closed.
    session: chat::Session,
    /// Panes that hold it again while it stops: reopens of it, and the
    /// closed pane itself when a shared view kept it. Each waits in its
    /// place, unattached, until the stop settles.
    reopens: Vec<CardId>,
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
    /// Saves what the provider has confirmed about the conversation of the
    /// closed pane `card`, so reopening it resumes that conversation rather
    /// than the one the card was saved with. The pane has already left its
    /// views, which hold what it was saved with, so only the closed card
    /// changes.
    pub(super) fn install_closing_identity(&mut self, card: CardId, session: &chat::Session) {
        let confirms = |state: &CardState| {
            state.id == card
                && matches!(&state.kind, CardKind::Chat(saved)
                    if saved != session && only_confirms(saved, session))
        };
        // What reopening the pane restores, recorded as it closed.
        if let Some((closed, _)) = self
            .recently_closed
            .iter_mut()
            .rev()
            .find(|(state, _)| confirms(state))
        {
            closed.kind = CardKind::Chat(session.clone());
        }
        if let Some(state) = self.card_state_mut(card)
            && confirms(state)
        {
            state.kind = CardKind::Chat(session.clone());
            self.persist();
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
                card,
                session: session.clone(),
                reopens: Vec::new(),
            },
        );
        // Saved before anything is sent, so a window that closes before the
        // daemon answers still asks again, and reports, on its next launch.
        let stopping = &mut Arc::make_mut(&mut self.durable).stopping_chats;
        stopping.retain(|stopping| stopping.session.id != session.id);
        stopping.push(StoppingChat {
            session: session.clone(),
            card,
            title,
            refused: None,
            dismissed: false,
        });
        self.persist();
        ask_to_stop(target, handle, session, false)
    }

    /// Whether `session` is a conversation the last window closed without
    /// hearing that it stopped, or whose stop was refused. A pane restoring
    /// it waits for [`Self::resume_stopping_chats`] instead of attaching.
    pub(in crate::app) fn awaits_stop(&self, session: &chat::Session) -> bool {
        self.durable
            .stopping_chats
            .iter()
            .any(|stopping| shares_conversation(&stopping.session, session))
    }

    /// Asks again to stop each chat the last window closed without hearing
    /// the daemon's answer, and puts back the refusals it had heard. A pane
    /// that reopened one before that window closed waits for the answer, as
    /// it did then, or goes back to the conversation still running.
    pub(in crate::app) fn resume_stopping_chats(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        for stopping in self.durable.stopping_chats.clone() {
            let id = stopping.session.id;
            let reopened: Vec<_> = self
                .durable
                .cards
                .iter()
                .filter(|state| {
                    state.existence.is_open()
                        && matches!(&state.kind, CardKind::Chat(session)
                            if shares_conversation(&stopping.session, session))
                })
                .map(|state| state.id)
                .filter(|card| self.awaits_attachment(*card))
                .collect();
            if stopping.refused.is_some() {
                self.chat_ui.close_refused.insert(id);
                let mut reopened = reopened.into_iter();
                if let Some(card) = reopened.next() {
                    tasks.push(self.attach_to_refused_close(card, id));
                }
                for card in reopened {
                    if let Some(session) = self.chat_return_session(card) {
                        tasks.push(self.open_chat(card, session, chat::Intent::Restore));
                    }
                }
                continue;
            }
            let asked = self
                .chat_ui
                .background_closes
                .values()
                .any(|closing| closing.session.id == id);
            if asked {
                continue;
            }
            let target = self.chat_ui.next_target(stopping.card);
            let reopens = reopened;
            self.chat_ui.background_closes.insert(
                target,
                Closing {
                    card: stopping.card,
                    session: stopping.session.clone(),
                    reopens,
                },
            );
            tasks.push(ask_to_stop(target, None, stopping.session, true));
        }
        Task::batch(tasks)
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
        let durable = Arc::make_mut(&mut self.durable);
        let refused = match result {
            Ok(()) => {
                self.chat_ui.close_refused.remove(&id);
                self.chat_ui
                    .stopped_closes
                    .insert(closing.card, closing.session.clone());
                durable
                    .stopping_chats
                    .retain(|stopping| stopping.session.id != id);
                None
            }
            Err(reason) => {
                tracing::warn!(%reason, "the daemon refused a closed chat's close");
                self.chat_ui.close_refused.insert(id);
                if let Some(stopping) = durable
                    .stopping_chats
                    .iter_mut()
                    .find(|stopping| stopping.session.id == id)
                {
                    stopping.refused = Some(reason.clone());
                }
                Some(reason)
            }
        };
        self.persist();
        if self.exit.is_some() {
            // Nothing reopens on the way out. A refusal heard now is saved,
            // and reported when the window next opens.
            if self.chat_ui.background_closes.is_empty() {
                return self.advance_exit(|exit| exit.chats_closed = true);
            }
            return Task::none();
        }
        let mut tasks = Vec::new();
        for card in closing.reopens {
            // A closed pane kept out of sight goes as it is, or comes back
            // into sight with the conversation it attaches to.
            let goes = refused.is_none()
                && card == closing.card
                && self.awaits_attachment(card)
                && self
                    .chat_return_session(card)
                    .is_some_and(|session| shares_conversation(&session, &closing.session));
            if goes {
                self.chat_ui.hidden_returns.remove(&card);
            } else {
                tasks.push(self.show_returned_close(card));
            }
            if self.reopen_awaits_stop(card) {
                // A refused stop leaves the conversation running: the reopen
                // goes back to it, its notice going with that.
                let still_running = refused.as_ref().map(|_| {
                    self.take_refusal_to_show(id)
                        .unwrap_or_else(|| closing.session.clone())
                });
                tasks.push(self.reopen_once_stopped(card, still_running));
                continue;
            }
            if !self.awaits_attachment(card) {
                continue;
            }
            // Another window put a different conversation in the pane while
            // it waited. This stop is nothing to that one, which attaches as
            // any pane a view shows does.
            if let Some(session) = self.chat_return_session(card)
                && !shares_conversation(&session, &closing.session)
            {
                tasks.push(self.open_chat(card, session, chat::Intent::Restore));
                continue;
            }
            tasks.push(match &refused {
                // The closed pane itself, back because a shared view kept it,
                // holds what that view knows of the conversation now.
                Some(_) if card == closing.card => self.restore_refused_in_place(card, id),
                // The conversation is still running, so the pane goes back
                // to it, and says why it could not be stopped.
                Some(_) => self.attach_to_refused_close(card, id),
                // The closed pane itself, back only because a shared view
                // kept it, goes as it was asked to now that nothing runs.
                None if card == closing.card => {
                    self.remove_chat_card(card, None).unwrap_or_else(Task::none)
                }
                None => match self.chat_return_session(card) {
                    Some(session) => self.open_chat(card, session, chat::Intent::Restore),
                    None => Task::none(),
                },
            });
        }
        Task::batch(tasks)
    }

    /// Whether `card` is an open chat pane that has not yet attached to
    /// anything, as a reopen held behind a stopping conversation is.
    fn awaits_attachment(&self, card: CardId) -> bool {
        !self.cards.contains_key(&card)
            && self.card_state(card).is_some_and(|state| {
                state.existence.is_open() && matches!(state.kind, CardKind::Chat(_))
            })
    }

    /// Reopens a conversation that is still stopping: its pane is put in
    /// place now and goes on with the reopen once the stop settles. `None`
    /// when the conversation is not stopping.
    pub(in crate::app) fn reopen_while_closing(
        &mut self,
        entry: &SavedConversation,
    ) -> Option<Task<Message>> {
        let target = self
            .chat_ui
            .background_closes
            .iter()
            .find(|(_, closing)| shares_conversation(&closing.session, &entry.session))
            .map(|(target, _)| *target)?;
        let (card, placed) = self.reopen_after_stop(entry.clone());
        if let Some(closing) = self.chat_ui.background_closes.get_mut(&target) {
            closing.reopens.push(card);
        }
        Some(placed)
    }

    /// Whether `card` is a reopen waiting, unattached, for its conversation to
    /// stop. Nothing else may attach it meanwhile.
    pub(in crate::app) fn holds_reopen(&self, card: CardId) -> bool {
        self.reopen_awaits_stop(card)
            || self
                .chat_ui
                .background_closes
                .values()
                .any(|closing| closing.reopens.contains(&card))
    }

    /// Gives up the reopen held in `card`, which is closing.
    pub(super) fn release_held_reopen(&mut self, card: CardId) {
        for closing in self.chat_ui.background_closes.values_mut() {
            closing.reopens.retain(|held| *held != card);
        }
    }

    /// Whether `session`'s conversation is stopping after its pane closed, or
    /// its stop was refused.
    pub(in crate::app) fn has_background_close(&self, session: &chat::Session) -> bool {
        self.refused_close_of(session).is_some()
            || self
                .chat_ui
                .background_closes
                .values()
                .any(|closing| shares_conversation(&closing.session, session))
    }

    /// Attaches a chat pane a shared view put in the strip. One whose
    /// conversation is still stopping waits, unattached, for the stop to
    /// settle, as a reopen does; one whose stop was refused goes back to the
    /// conversation still running, and says why. A closed pane comes back so
    /// when the view refuses its removal.
    pub(in crate::app) fn attach_shared_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
    ) -> Task<Message> {
        if let Some(id) = self.refused_close_of(&session) {
            return self.restore_refused_in_place(card, id);
        }
        let target = self
            .chat_ui
            .background_closes
            .iter()
            .find(|(_, closing)| shares_conversation(&closing.session, &session))
            .map(|(target, _)| *target);
        let Some(target) = target else {
            // The view never heard that this pane closed, as when the window
            // closed before the removal reached it. The conversation has
            // stopped, so the pane goes again.
            // Another window may have put a different conversation in it
            // since, which attaches as any pane a view shows does.
            if self
                .chat_ui
                .stopped_closes
                .get(&card)
                .is_some_and(|stopped| shares_conversation(stopped, &session))
            {
                if self.workspace.order().any(|placed| placed == card) {
                    return self.remove_chat_card(card, None).unwrap_or_else(Task::none);
                }
                // The view is still putting the pane in the strip, which
                // removes it once it is there.
                self.cards.remove(&card);
                self.chat_ui.stopped_returns.insert(card);
                return Task::none();
            }
            // The last window closed before hearing the stop: the launch asks
            // again, and the pane waits for that answer, unattached (see
            // `resume_stopping_chats`).
            if self.awaits_stop(&session) {
                self.stash_unsent_chat(card);
                self.cards.remove(&card);
                return Task::none();
            }
            return self.open_chat(card, session, chat::Intent::Restore);
        };
        if let Some(closing) = self.chat_ui.background_closes.get_mut(&target) {
            if !closing.reopens.contains(&card) {
                closing.reopens.push(card);
            }
            if closing.card == card {
                self.chat_ui.hidden_returns.insert(card);
            }
        }
        // It waits unattached, as a held reopen does, keeping what it had
        // not sent.
        self.stash_unsent_chat(card);
        self.cards.remove(&card);
        Task::none()
    }

    /// Takes away again each closed pane a shared view brought back after its
    /// conversation stopped, now that the strip shows it.
    pub(in crate::app) fn remove_stopped_returns(&mut self) -> Task<Message> {
        let placed = self
            .workspace
            .order()
            .filter(|card| self.chat_ui.stopped_returns.contains(card))
            .collect::<Vec<_>>();
        let mut tasks = Vec::new();
        for card in placed {
            self.chat_ui.stopped_returns.remove(&card);
            let open = self
                .card_state(card)
                .is_some_and(|state| state.existence.is_open());
            if open && let Some(task) = self.remove_chat_card(card, None) {
                tasks.push(task);
            }
        }
        Task::batch(tasks)
    }

    /// Keeps out of sight each closed pane a shared view put back while its
    /// conversation stops, once the strip holds it, and brings back into
    /// sight one its close no longer holds.
    pub(in crate::app) fn hide_returned_closes(&mut self) -> Task<Message> {
        let held = self
            .chat_ui
            .background_closes
            .values()
            .filter(|closing| closing.reopens.contains(&closing.card))
            .map(|closing| closing.card)
            .collect::<HashSet<_>>();
        let released = self
            .chat_ui
            .hidden_returns
            .iter()
            .copied()
            .filter(|card| {
                !held.contains(card)
                    || !self
                        .card_state(*card)
                        .is_some_and(|state| state.existence.is_open())
            })
            .collect::<Vec<_>>();
        let mut tasks = released
            .into_iter()
            .map(|card| self.show_returned_close(card))
            .collect::<Vec<_>>();
        let shown = self
            .workspace
            .shown()
            .filter(|card| self.chat_ui.hidden_returns.contains(card))
            .collect::<Vec<_>>();
        for card in shown {
            if let Ok(update) = self.workspace.hide_card(card) {
                tasks.push(self.apply_viewport(update, false, DurableChange::None));
            }
        }
        Task::batch(tasks)
    }

    /// The closed panes kept out of sight while their conversation stops.
    pub(in crate::app) fn returned_closes(&self) -> Vec<CardId> {
        self.chat_ui.hidden_returns.iter().copied().collect()
    }

    /// Whether `card` is a closed pane kept out of sight while its
    /// conversation stops.
    pub(in crate::app) fn returned_close_is_hidden(&self, card: CardId) -> bool {
        self.chat_ui.hidden_returns.contains(&card)
    }

    /// Whether `card` closed and its conversation is still stopping, so a
    /// view that keeps it keeps nothing the person is to act on.
    pub(in crate::app) fn closes_in_background(&self, card: CardId) -> bool {
        self.chat_ui
            .background_closes
            .values()
            .any(|closing| closing.card == card)
    }

    /// Brings a closed pane kept out of sight back into it.
    fn show_returned_close(&mut self, card: CardId) -> Task<Message> {
        if !self.chat_ui.hidden_returns.remove(&card) {
            return Task::none();
        }
        match self.workspace.show_card(card) {
            Some(update) => self.apply_viewport(update, false, DurableChange::None),
            None => Task::none(),
        }
    }

    /// Puts a pane that holds the conversation whose stop was refused back on
    /// it, as the pane now has it, saying why it could not be stopped.
    fn restore_refused_in_place(&mut self, card: CardId, id: TerminalId) -> Task<Message> {
        let Some(session) = self.chat_return_session(card) else {
            return Task::none();
        };
        // The pane may hold the conversation under a successor's identity,
        // which is the one it attaches with: the refusal it shows, and the
        // wait its next close keeps, follow that identity.
        if let Some((_, reason)) = self.take_close_failure(id) {
            self.chat_ui.refusals_to_show.insert(session.id, reason);
            self.chat_ui.close_refused.insert(session.id);
        }
        self.open_chat(card, session, chat::Intent::Restore)
    }

    /// Takes a refused close out of the window's notices to show on the pane
    /// that goes back to its conversation, returning that conversation.
    fn take_refusal_to_show(&mut self, id: TerminalId) -> Option<chat::Session> {
        let (failure, reason) = self.take_close_failure(id)?;
        self.chat_ui.refusals_to_show.insert(id, reason);
        Some(failure.session)
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

    /// The refused close of the conversation `session` reopens, if any.
    pub(in crate::app) fn refused_close_of(&self, session: &chat::Session) -> Option<TerminalId> {
        self.durable
            .stopping_chats
            .iter()
            .find(|stopping| {
                stopping.refused.is_some() && shares_conversation(&stopping.session, session)
            })
            .map(|stopping| stopping.session.id)
    }

    /// Takes a refused close out of the window's notices, returning it.
    fn take_close_failure(&mut self, id: TerminalId) -> Option<(StoppingChat, String)> {
        let stopping = &mut Arc::make_mut(&mut self.durable).stopping_chats;
        let index = stopping
            .iter()
            .position(|stopping| stopping.session.id == id && stopping.refused.is_some())?;
        let mut failure = stopping.remove(index);
        self.persist();
        let reason = failure.refused.take().unwrap_or_default();
        Some((failure, reason))
    }

    /// Puts a held pane back on the conversation whose stop was refused.
    fn attach_to_refused_close(&mut self, card: CardId, id: TerminalId) -> Task<Message> {
        let Some(session) = self
            .take_refusal_to_show(id)
            .or_else(|| self.chat_return_session(card))
        else {
            return Task::none();
        };
        let _ = self.install_chat_session(card, session.clone());
        self.persist();
        self.open_chat(card, session, chat::Intent::Restore)
    }

    /// Reopens a conversation whose stop was refused, attached to the provider
    /// still running it, on a pane that says why it could not be stopped. A
    /// conversation already open is brought into view instead.
    pub(in crate::app) fn reopen_refused_close(&mut self, id: TerminalId) -> Task<Message> {
        let Some((failure, reason)) = self.take_close_failure(id) else {
            return Task::none();
        };
        if let Some(card) = self.existing_conversation_card(&failure.session, None) {
            return cosmic::task::message(Message::FocusCard(card));
        }
        self.chat_ui.refusals_to_show.insert(id, reason);
        // It comes back where it closed, as it closed, with what it had not
        // sent.
        let session = failure.session.clone();
        let entry = SavedConversation {
            card: failure.card,
            title: failure.title,
            session: failure.session,
            open: false,
            updated_at_unix_ms: 0,
        };
        self.place_resumed_conversation(entry, session, None).1
    }

    /// Takes a refused close out of the notices. Its conversation still
    /// runs, so the record stays: reopening it by any route attaches to what
    /// runs it, and that reopen takes the record.
    pub(super) fn dismiss_close_failure(&mut self, id: TerminalId) {
        let Some(failure) = Arc::make_mut(&mut self.durable)
            .stopping_chats
            .iter_mut()
            .find(|stopping| stopping.session.id == id && stopping.refused.is_some())
        else {
            return;
        };
        if !failure.dismissed {
            failure.dismissed = true;
            self.persist();
        }
    }

    fn close_failures(&self) -> impl Iterator<Item = (&StoppingChat, &str)> {
        self.durable
            .stopping_chats
            .iter()
            .filter(|stopping| !stopping.dismissed)
            .filter_map(|stopping| Some((stopping, stopping.refused.as_deref()?)))
    }

    pub(in crate::app) fn has_close_failure_notices(&self) -> bool {
        self.close_failures().next().is_some()
    }

    /// Each refused close, in the application notices, with the way back to
    /// the conversation it left running.
    pub(in crate::app) fn close_failure_notice_views(&self) -> Vec<Element<'_, Message>> {
        let severity = NoticeSeverity::Error;
        self.close_failures()
            .map(|(failure, reason)| {
                let id = failure.session.id;
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
                        notice_body(close_failure_message(&failure.title)),
                        notice_surface_details(severity, reason),
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

/// Sends the daemon a closed chat's stop, answered by `Action::Retired`. One
/// asked again at launch first brings up the daemon the launch would start
/// anyway, so its absence is not taken for a refusal.
fn ask_to_stop(
    target: Target,
    handle: Option<Arc<ClientHandle>>,
    session: chat::Session,
    at_launch: bool,
) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if at_launch {
                crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
            }
            close_conversation(handle, Some(session))
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        Message::ChatUi(Action::Retired(target, result))
    })
}

/// What a refused close's notice says.
pub(in crate::app) fn close_failure_message(title: &str) -> String {
    let name = if title.is_empty() {
        "A closed chat".to_owned()
    } else {
        format!("“{title}”")
    };
    format!("{name} is still running: its pane closed, but Cagents couldn't stop it.")
}

#[cfg(test)]
impl Cagents {
    /// The close stopping `card`'s conversation in the background, if any.
    pub(in crate::app) fn background_close_for_test(&self, card: CardId) -> Option<Target> {
        self.chat_ui
            .background_closes
            .keys()
            .copied()
            .find(|target| target.card == card)
    }
}
