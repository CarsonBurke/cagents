//! A closing pane leaves immediately. The daemon stops the provider and keeps
//! its result even if the window leaves once its handoff is accepted. A
//! refusal leaves the conversation running and offers a way back to it. Its
//! next close waits on its pane so a second refusal can show what it can end.
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
use crate::{
    control::{ControlClient, ControlError},
    terminal::ipc::ChatCloseOutcome,
};
use uuid::Uuid;

/// A conversation stopping in the background after its pane closed.
#[derive(Debug)]
pub(in crate::app) struct Closing {
    /// The pane that closed.
    card: CardId,
    /// The conversation as its pane closed.
    session: chat::Session,
    receipt: Option<Uuid>,
    pub(super) awaiting_acceptance: bool,
    /// Panes that hold it again while it stops: reopens of it, and the
    /// closed pane itself when a shared view kept it. Each waits in its
    /// place, unattached, until the stop settles.
    reopens: Vec<CardId>,
}

#[derive(Debug, Clone)]
pub enum CloseHandoff {
    AlreadyStopped,
    Accepted,
    Refused(String),
    Uncertain,
}

pub(super) enum SharedChatClose {
    Refused(TerminalId),
    Pending(Target),
    RemoveReturned,
    Saved,
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
pub(in crate::app) fn shares_conversation(a: &chat::Session, b: &chat::Session) -> bool {
    a.id == b.id
        || (a.launch.provider == b.launch.provider
            && a.launch.session_id.is_some()
            && a.launch.session_id == b.launch.session_id)
}

/// A saved pane removal must match the conversation, even when another
/// window reuses the same chat ID for a replacement or a new draft.
pub(super) fn returned_pane_source_matches(
    source: &chat::Session,
    current: &chat::Session,
) -> bool {
    if current.is_unstarted() || source.launch.provider != current.launch.provider {
        return false;
    }
    match (&source.launch.session_id, &current.launch.session_id) {
        (Some(source), Some(current)) => source == current,
        _ => source.id == current.id,
    }
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
        session: chat::Session,
        title: String,
    ) -> Task<Message> {
        let target = self.chat_ui.next_target(card);
        let receipt = Uuid::new_v4();
        self.chat_ui.background_closes.insert(
            target,
            Closing {
                card,
                session: session.clone(),
                receipt: Some(receipt),
                awaiting_acceptance: true,
                reopens: Vec::new(),
            },
        );
        // Save the receipt before handing off. Even if the acknowledgement
        // is lost, the next window reads this receipt without issuing a stop.
        let stopping = &mut Arc::make_mut(&mut self.durable).stopping_chats;
        stopping.retain(|stopping| stopping.session.id != session.id);
        stopping.push(StoppingChat {
            session: session.clone(),
            card,
            title,
            close_receipt: Some(receipt),
            refused: None,
            dismissed: false,
            stopped: false,
        });
        self.persist();
        self.hand_off_close(target, receipt, session, false)
    }

    fn hand_off_close(
        &self,
        target: Target,
        receipt: Uuid,
        session: chat::Session,
        at_launch: bool,
    ) -> Task<Message> {
        let flush = self.session.as_ref().map(|writer| writer.request_flush());
        cosmic::task::future(async move {
            let handoff = close_observer(move || {
                // Once accepted, even a process crash must leave the next
                // window enough information to read the daemon's receipt.
                if let Some(flush) = flush {
                    flush?
                        .recv_timeout(Duration::from_secs(20))
                        .map_err(|error| error.to_string())??;
                }
                if at_launch {
                    crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
                }
                let socket = crate::terminal::ipc::daemon_socket_path()
                    .map_err(|error| error.to_string())?;
                let Some(mut client) =
                    crate::app::channel_ui::connect_for_chat_close(&socket, &session)?
                else {
                    return Ok(CloseHandoff::AlreadyStopped);
                };
                Ok(match client.begin_chat_close(receipt, session) {
                    Ok(()) => CloseHandoff::Accepted,
                    Err(error @ ControlError::Remote { .. }) => {
                        CloseHandoff::Refused(error.reason())
                    }
                    Err(_) => CloseHandoff::Uncertain,
                })
            })
            .await
            .unwrap_or_else(CloseHandoff::Refused);
            Message::ChatUi(Action::ChatCloseAccepted(target, handoff))
        })
    }

    /// Whether the pane `card`, restoring `session`, waits for
    /// [`Self::resume_stopping_chats`] instead of attaching: its conversation
    /// is one the last window closed without hearing that it stopped, or
    /// whose stop was refused, or `card` is the closed pane a shared view put
    /// back after it stopped, which is still to go.
    pub(in crate::app) fn awaits_stop(&self, card: CardId, session: &chat::Session) -> bool {
        self.pending_pane_removal(card, session)
            || self.durable.stopping_chats.iter().any(|stopping| {
                shares_conversation(&stopping.session, session)
                    && !self.heard_stopped(stopping.session.id)
                    && (!stopping.stopped || stopping.card == card)
            })
    }

    /// Whether this window heard the conversation `id` stop. Its stop stays
    /// saved only while the closed pane a shared view kept is still to go.
    fn heard_stopped(&self, id: TerminalId) -> bool {
        self.chat_ui
            .stopped_closes
            .values()
            .any(|stopped| stopped.id == id)
    }

    /// Reads outstanding receipts and puts back refusals already presented
    /// by the last window. A pending pre-receipt save is submitted once. A pane
    /// that reopened one before that window closed waits for the answer, as
    /// it did then, or goes back to the conversation still running. A closed
    /// pane a shared view put back after its conversation stopped goes, with
    /// nothing asked again.
    pub(in crate::app) fn resume_stopping_chats(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        let pending_panes = self.durable.pending_chat_pane_removals.clone();
        for (&card, session) in &pending_panes {
            let task = if self
                .chat_return_session(card)
                .is_some_and(|current| !returned_pane_source_matches(session, &current))
            {
                self.keep_reused_return(card)
            } else {
                self.take_away_returned_close(card, session)
            };
            tasks.push(task);
        }
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
                // Once stopped, only the closed pane itself is held.
                .filter(|card| {
                    (!stopping.stopped || *card == stopping.card)
                        && !pending_panes.contains_key(card)
                })
                .collect();
            // A refused stop's conversation still runs: a pane the view holds
            // goes back to it once admitted, as `attach_shared_chat` does.
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
            let mut reopens = reopened;
            // The closed pane itself, restored from its view and showing its
            // saved text until the view admits it, is held with the stop too.
            if !pending_panes.contains_key(&stopping.card)
                && self.chat_awaiting_admission(stopping.card)
                && self
                    .chat_return_session(stopping.card)
                    .is_some_and(|session| shares_conversation(&session, &stopping.session))
            {
                reopens.push(stopping.card);
            }
            // The closed pane itself, kept by a shared view, stays out of
            // sight from the start, as it did in the window that closed it.
            if reopens.contains(&stopping.card) {
                self.chat_ui.hidden_returns.insert(stopping.card);
            }
            let submit = !stopping.stopped && stopping.close_receipt.is_none();
            let receipt = if submit {
                let receipt = Uuid::new_v4();
                if let Some(saved) = Arc::make_mut(&mut self.durable)
                    .stopping_chats
                    .iter_mut()
                    .find(|saved| saved.session.id == id)
                {
                    saved.close_receipt = Some(receipt);
                }
                self.persist();
                Some(receipt)
            } else {
                stopping.close_receipt
            };
            self.chat_ui.background_closes.insert(
                target,
                Closing {
                    card: stopping.card,
                    session: stopping.session.clone(),
                    receipt,
                    awaiting_acceptance: submit,
                    reopens,
                },
            );
            tasks.push(match receipt {
                Some(receipt) if submit => {
                    self.hand_off_close(target, receipt, stopping.session, true)
                }
                Some(receipt) => read_close(target, receipt, true, Duration::ZERO),
                None => self.background_close_finished(target, Ok(())),
            });
        }
        tasks.push(self.hide_returned_closes());
        Task::batch(tasks)
    }

    pub(super) fn background_close_accepted(
        &mut self,
        target: Target,
        handoff: CloseHandoff,
    ) -> Task<Message> {
        let Some(closing) = self.chat_ui.background_closes.get_mut(&target) else {
            return Task::none();
        };
        closing.awaiting_acceptance = false;
        let receipt = closing.receipt.expect("submitted close has a receipt");
        match handoff {
            CloseHandoff::AlreadyStopped => return self.background_close_finished(target, Ok(())),
            CloseHandoff::Refused(reason) => {
                return self.background_close_finished(target, Err(reason));
            }
            CloseHandoff::Accepted | CloseHandoff::Uncertain => {}
        }
        if self.exit.is_some() {
            return self.finish_close_handoffs_for_exit();
        }
        read_close(target, receipt, false, Duration::ZERO)
    }

    fn finish_close_handoffs_for_exit(&mut self) -> Task<Message> {
        if !self.chat_ui.close_handoffs_pending() {
            return self.advance_exit(|exit| exit.chats_closed = true);
        }
        Task::none()
    }

    pub(super) fn background_close_outcome(
        &mut self,
        target: Target,
        receipt: Uuid,
        outcome: Result<ChatCloseOutcome, String>,
    ) -> Task<Message> {
        if !self
            .chat_ui
            .background_closes
            .get(&target)
            .is_some_and(|closing| closing.receipt == Some(receipt))
        {
            return Task::none();
        }

        let result = match outcome {
            Ok(ChatCloseOutcome::Finished(result)) => result,
            Ok(ChatCloseOutcome::Missing) => {
                return self.background_close_finished(
                    target,
                    Err("The daemon has no receipt confirming it accepted this stop.".into()),
                );
            }
            Ok(ChatCloseOutcome::Pending) => {
                return if self.exit.is_none() {
                    read_close(target, receipt, false, Duration::ZERO)
                } else {
                    Task::none()
                };
            }
            Err(error) => {
                tracing::warn!(%error, "could not read the outcome of a closed chat");
                return if self.exit.is_none() {
                    read_close(target, receipt, true, Duration::from_secs(5))
                } else {
                    Task::none()
                };
            }
        };
        let apply = self.background_close_finished(target, result);
        let Some(flush) = self
            .session
            .as_ref()
            .and_then(|writer| writer.request_flush().ok())
        else {
            return apply;
        };
        // An accepted close's projection must survive a window exit before the
        // daemon can forget the receipt. A failed flush leaves it readable.
        let acknowledge = iced::Task::future(async move {
            let result = close_observer(move || {
                flush
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|error| error.to_string())??;
                connect_close_daemon(false)?
                    .acknowledge_chat_close(receipt)
                    .map_err(|error| error.reason())
            })
            .await;
            if let Err(error) = result {
                tracing::warn!(%error, "could not acknowledge the saved chat close outcome");
            }
        })
        .discard();
        Task::batch([apply, acknowledge])
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
        // A shared view put the closed pane back while its conversation
        // stopped. Its stop stays saved until the pane is gone, so a window
        // that closes first still takes the pane away at its next launch,
        // rather than restoring a closed chat.
        let returned = self.returned_with(&closing);
        let fenced_gives_way =
            result.is_err() && returned && self.shared_views.quarantined.contains(&closing.card);
        let durable = Arc::make_mut(&mut self.durable);
        if let Some(stopping) = durable
            .stopping_chats
            .iter_mut()
            .find(|saved| saved.session.id == id)
        {
            stopping.close_receipt = None;
        }
        let refused = match result {
            Ok(()) => {
                self.chat_ui.close_refused.remove(&id);
                self.chat_ui
                    .stopped_closes
                    .insert(closing.card, closing.session.clone());
                if returned {
                    durable
                        .stopping_chats
                        .iter_mut()
                        .filter(|stopping| stopping.session.id == id)
                        .for_each(|stopping| stopping.stopped = true);
                } else {
                    durable
                        .stopping_chats
                        .retain(|stopping| stopping.session.id != id);
                }
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
        if fenced_gives_way {
            durable
                .pending_chat_pane_removals
                .insert(closing.card, closing.session.clone());
        }
        self.persist();
        if self.exit.is_some() {
            // Nothing reopens on the way out. A refusal heard now is saved,
            // and reported when the window next opens.
            return self.finish_close_handoffs_for_exit();
        }
        let mut tasks = Vec::new();
        // The closed pane itself first: a refused stop puts it back on what
        // still runs, which a reopen of the conversation then goes to.
        let mut reopens = closing.reopens;
        reopens.sort_by_key(|card| *card != closing.card);
        // A closed pane its view fences never attaches, so a refused stop
        // does not bring it back on what still runs: it goes by identity
        // after any reopen of the conversation has gone back to that, taking
        // what the closed pane had not sent while it is still out of sight.
        // Without one, the refusal's notice stays to reopen it.
        for card in reopens {
            // A closed pane kept out of sight goes as it is, or comes back
            // into sight with the conversation it attaches to.
            let goes =
                card == closing.card && ((refused.is_none() && returned) || fenced_gives_way);
            if !goes {
                tasks.push(self.show_returned_close(card));
            }
            if fenced_gives_way && card == closing.card {
                continue;
            }
            // One its view fences is never admitted, so never attaches: the
            // reopen goes back to what still runs instead.
            if refused.is_some()
                && returned
                && card != closing.card
                && !self.shared_views.quarantined.contains(&closing.card)
                && self.reopen_awaits_stop(card)
                && let Some(session) = self.chat_return_session(closing.card)
            {
                tasks.push(self.supersede_reopening(card, closing.card, &session));
                continue;
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
            // The closed pane itself, restored at launch from its view and
            // showing saved text until the view admits it. Only an admitted
            // pane's removal reaches the view, so one whose conversation
            // stopped stays out of sight, its stop saved, until admission
            // takes it away (see `attach_shared_chat`). Otherwise admission
            // puts it back on what still runs, as any pane a view shows is.
            if card == closing.card && self.chat_awaiting_admission(card) {
                if goes && !self.awaits_admission_to_go(card) {
                    tasks.push(self.take_away_returned_close(card, &closing.session));
                }
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
                // kept it, goes as it was asked to now that nothing runs. A
                // pane outside the strip shown now goes once its strip is.
                None if card == closing.card => {
                    self.take_away_returned_close(card, &closing.session)
                }
                None => match self.chat_return_session(card) {
                    Some(session) => self.open_chat(card, session, chat::Intent::Restore),
                    None => Task::none(),
                },
            });
        }
        if fenced_gives_way {
            tasks.push(self.take_away_returned_close(closing.card, &closing.session));
        }
        Task::batch(tasks)
    }

    /// Removes the closed pane `card` a shared view put back from the strip,
    /// asking the daemon nothing. One its view fences leaves the view by
    /// identity, as a fenced pane's close does.
    fn remove_returned_pane(&mut self, card: CardId) -> Option<Task<Message>> {
        let fenced = self.shared_views.quarantined.contains(&card);
        // Recorded first: committing the removal publishes it.
        if fenced {
            self.shared_views.fenced_closes.insert(card);
        }
        let task = self.remove_chat_card(card, None);
        if fenced
            && self
                .card_state(card)
                .is_none_or(|state| state.existence.is_open())
        {
            self.shared_views.fenced_closes.remove(&card);
        }
        task
    }

    /// Whether a shared view put the pane `closing` closed back, still
    /// holding the conversation being stopped and waiting, unattached, for
    /// the stop to settle.
    fn returned_with(&self, closing: &Closing) -> bool {
        closing.reopens.contains(&closing.card)
            && (self.awaits_attachment(closing.card) || self.chat_awaiting_admission(closing.card))
            && self
                .chat_return_session(closing.card)
                .is_some_and(|session| shares_conversation(&session, &closing.session))
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

    /// Whether the closed chat pane `card` goes through the stop of a
    /// conversation it started: a view that brings it back is handled here,
    /// out of sight, until that stop settles and its removal follows, or on
    /// what still runs when the daemon refused to stop it. An unstarted
    /// draft has nothing to stop, so nothing here waits for it.
    pub(in crate::app) fn chat_close_returns_through_stop(&self, card: CardId) -> bool {
        let started = |session: &chat::Session| !session.is_unstarted();
        self.chat_ui.stopped_closes.get(&card).is_some_and(started)
            || self
                .chat_ui
                .background_closes
                .values()
                .any(|closing| closing.card == card && started(&closing.session))
            || self
                .durable
                .stopping_chats
                .iter()
                .any(|stopping| stopping.card == card && started(&stopping.session))
            || ((self.chat_ui.hidden_returns.contains(&card)
                || self.chat_ui.returns_to_remove.contains(&card))
                && self.card_state(card).is_some_and(
                    |state| matches!(&state.kind, CardKind::Chat(session) if started(session)),
                ))
    }

    /// Whether this window still owes a stopped pane's removal, even if the
    /// scope cannot project the returned pane yet. Match the registry's
    /// conversation, since it may have put a successor in the same card.
    pub(in crate::app) fn chat_close_can_retry_removal(
        &self,
        card: CardId,
        session: &chat::Session,
    ) -> bool {
        let matches =
            |closed: &chat::Session| !closed.is_unstarted() && shares_conversation(closed, session);
        self.pending_pane_removal(card, session)
            || self.chat_ui.stopped_closes.get(&card).is_some_and(matches)
            || self
                .chat_ui
                .background_closes
                .values()
                .any(|closing| closing.card == card && matches(&closing.session))
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

    /// Whether the close lifecycle owns attachment of a pane the view admits.
    pub(super) fn shared_chat_close(
        &self,
        card: CardId,
        session: &chat::Session,
    ) -> Option<SharedChatClose> {
        if let Some(source) = self.durable.pending_chat_pane_removals.get(&card) {
            return returned_pane_source_matches(source, session)
                .then_some(SharedChatClose::RemoveReturned);
        }
        if let Some(id) = self.refused_close_of(session) {
            return Some(SharedChatClose::Refused(id));
        }
        if let Some((target, _)) = self
            .chat_ui
            .background_closes
            .iter()
            .find(|(_, closing)| shares_conversation(&closing.session, session))
        {
            return Some(SharedChatClose::Pending(*target));
        }
        if self
            .chat_ui
            .stopped_closes
            .get(&card)
            .is_some_and(|stopped| shares_conversation(stopped, session))
        {
            return Some(SharedChatClose::RemoveReturned);
        }
        self.awaits_stop(card, session)
            .then_some(SharedChatClose::Saved)
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
        let close = self.shared_chat_close(card, &session);
        self.attach_shared_chat_with_close(card, session, close)
    }

    pub(super) fn attach_shared_chat_with_close(
        &mut self,
        card: CardId,
        session: chat::Session,
        close: Option<SharedChatClose>,
    ) -> Task<Message> {
        match close {
            Some(SharedChatClose::Refused(id)) => self.restore_refused_in_place(card, id),
            Some(SharedChatClose::Pending(target)) => {
                let closing = self
                    .chat_ui
                    .background_closes
                    .get_mut(&target)
                    .expect("classified background close is still pending");
                if !closing.reopens.contains(&card) {
                    closing.reopens.push(card);
                }
                if closing.card == card {
                    self.chat_ui.hidden_returns.insert(card);
                }
                self.hold_returned_unsent(card);
                Task::none()
            }
            Some(SharedChatClose::RemoveReturned) => {
                // The view never heard that this pane closed. Take it away
                // now or once its view places it in the strip.
                self.hold_returned_unsent(card);
                self.take_away_returned_close(card, &session)
            }
            Some(SharedChatClose::Saved) | None => {
                // A pane that was to go now holds another conversation.
                let kept = if self.chat_ui.stopped_closes.contains_key(&card)
                    || self.durable.pending_chat_pane_removals.contains_key(&card)
                {
                    self.keep_reused_return(card)
                } else {
                    Task::none()
                };
                if close.is_some() {
                    // Startup has yet to ask again about the saved stop.
                    self.hold_returned_unsent(card);
                    if self.stopping_close_of(card).is_some() {
                        self.chat_ui.hidden_returns.insert(card);
                        return Task::batch([kept, self.hide_returned_closes()]);
                    }
                    return kept;
                }
                Task::batch([kept, self.open_chat(card, session, chat::Intent::Restore)])
            }
        }
    }

    /// Removes returned panes once their strip is ready.
    pub(in crate::app) fn remove_returned_chats(&mut self) -> Task<Message> {
        let queued = &self.chat_ui.returns_to_remove;
        if queued.is_empty() {
            return Task::none();
        }
        let fenced_offstrip = queued.iter().copied().filter(|card| {
            self.workspace.card(*card).is_none()
                && self.shared_views.quarantined.contains(card)
                && self.durable.pending_chat_pane_removals.contains_key(card)
        });
        let ready = self
            .workspace
            .order()
            .filter(|card| queued.contains(card))
            .chain(fenced_offstrip)
            .collect::<Vec<_>>();
        Task::batch(
            ready
                .into_iter()
                .map(|card| self.take_away_queued_return(card))
                .collect::<Vec<_>>(),
        )
    }

    /// Removes returned panes that were waiting for the strip to be free.
    pub(in crate::app) fn take_away_returns_after_removal(&mut self) -> Task<Message> {
        if !self.chat_ui.returns_await_removal || self.workspace.pending_close().is_some() {
            return Task::none();
        }
        self.chat_ui.returns_await_removal = false;
        self.remove_returned_chats()
    }

    /// Takes away again a closed pane a shared view brought back after its
    /// conversation stopped, once its strip is shown, keeping it out of sight
    /// meanwhile. Its stop stays saved until then, so a window that closes
    /// first takes it away at its next launch rather than restoring it.
    fn remove_when_placed(&mut self, card: CardId, session: &chat::Session) {
        // A pane already gone has nothing left to take away.
        if !self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open())
        {
            return;
        }
        self.chat_ui.returns_to_remove.insert(card);
        self.chat_ui.hidden_returns.insert(card);
        if self.pending_pane_removal(card, session) {
            return;
        }
        if let Some(stopping) = self
            .durable
            .stopping_chats
            .iter()
            .position(|stopping| stopping.session.id == session.id)
        {
            if !self.durable.stopping_chats[stopping].stopped {
                Arc::make_mut(&mut self.durable).stopping_chats[stopping].stopped = true;
                self.persist();
            }
            return;
        }
        let title = self
            .card_state(card)
            .map(|state| self.card_display_title(state).to_owned())
            .unwrap_or_default();
        Arc::make_mut(&mut self.durable)
            .stopping_chats
            .push(StoppingChat {
                session: session.clone(),
                card,
                title,
                close_receipt: None,
                refused: None,
                dismissed: false,
                stopped: true,
            });
        self.persist();
    }

    /// Removes a returned pane after its view projection finishes. A fenced
    /// pane can also close by identity from outside the strip.
    fn take_away_returned_close(&mut self, card: CardId, session: &chat::Session) -> Task<Message> {
        if !self.shared_views.applying
            && (self.workspace.order().any(|placed| placed == card)
                || (self.shared_views.quarantined.contains(&card)
                    && self.pending_pane_removal(card, session)))
        {
            self.take_away_queued_return(card)
        } else {
            self.remove_when_placed(card, session);
            Task::none()
        }
    }

    /// Takes away a returned pane without asking to stop its conversation.
    /// If the strip is busy, the pane stays out of sight until it can close.
    fn take_away_queued_return(&mut self, card: CardId) -> Task<Message> {
        let open = |app: &Self| {
            app.card_state(card)
                .is_some_and(|state| state.existence.is_open())
        };
        // Another window put a different conversation in the pane since: it
        // stays, and so does what runs it.
        let holds_return = self.chat_return_session(card).is_some_and(|current| {
            let recorded = self.pending_pane_removal(card, &current);
            if recorded {
                return self
                    .workspace_model
                    .views
                    .iter()
                    .flat_map(|view| &view.panes)
                    .find(|pane| pane.id == card)
                    .is_none_or(|pane| {
                        matches!(&pane.kind, CardKind::Chat(session)
                        if self.pending_pane_removal(card, session))
                    });
            }
            recorded
                || self
                    .chat_ui
                    .stopped_closes
                    .get(&card)
                    .is_some_and(|stopped| shares_conversation(stopped, &current))
        });
        if open(self) && !holds_return {
            return self.keep_reused_return(card);
        }
        // Only an admitted pane's removal reaches its view, so one still
        // awaiting admission waits for it out of sight.
        if self.awaits_admission_to_go(card) {
            self.chat_ui.returns_to_remove.insert(card);
            self.chat_ui.hidden_returns.insert(card);
            return Task::none();
        }
        // While another pane's removal holds the strip, it waits for that to
        // finish rather than queue a close, which would ask the daemon again
        // to stop what has stopped, and could reach a conversation reopened
        // since.
        let placed = self.workspace.order().any(|placed| placed == card);
        if open(self)
            && placed
            && self
                .workspace
                .pending_close()
                .is_some_and(|pending| pending.unmounting != card)
        {
            self.chat_ui.returns_to_remove.insert(card);
            self.chat_ui.hidden_returns.insert(card);
            self.chat_ui.returns_await_removal = true;
            return Task::none();
        }
        let task = if !open(self) {
            None
        } else if placed {
            self.remove_returned_pane(card)
        } else if self.shared_views.quarantined.contains(&card) {
            self.keep_unsent_on_close(card);
            self.shared_views.fenced_closes.insert(card);
            let task = self.retire_offstrip_card(card);
            if open(self) {
                self.shared_views.fenced_closes.remove(&card);
            } else {
                self.chat_discard(card);
            }
            Some(task)
        } else {
            None
        };
        if !open(self) {
            // The returned pane's removal has committed. If its view still
            // holds it, keep the local tombstone after this one retry.
            self.forget_pending_pane_removal(card);
            self.chat_ui.stopped_closes.remove(&card);
            self.chat_ui.returns_to_remove.remove(&card);
            self.chat_ui.hidden_returns.remove(&card);
            // Nothing is left for its view to admit.
            if self.durable.pending_chat_admissions.contains_key(&card) {
                Arc::make_mut(&mut self.durable)
                    .pending_chat_admissions
                    .remove(&card);
                self.persist();
            }
        } else {
            self.chat_ui.returns_to_remove.insert(card);
            self.chat_ui.hidden_returns.insert(card);
        }
        task.unwrap_or_else(Task::none)
    }

    /// Holds `card` unattached, with what it had not sent: what its runtime
    /// shows, and, for the closed pane itself, what its close kept, so a
    /// save while it is back and its next close both keep it.
    fn hold_returned_unsent(&mut self, card: CardId) {
        self.stash_unsent_chat(card);
        self.cards.remove(&card);
        self.reclaim_kept_draft(card);
    }

    /// Keeps a pane that was to go because another window has put a
    /// different conversation in it, back in sight.
    fn keep_reused_return(&mut self, card: CardId) -> Task<Message> {
        // Forgotten while it is still known to have stopped.
        self.forget_stop_of_removed(card);
        self.forget_pending_pane_removal(card);
        self.chat_ui.returns_to_remove.remove(&card);
        self.chat_ui.stopped_closes.remove(&card);
        self.show_returned_close(card)
    }

    /// Admission is already replacing the pane. Let that path save the
    /// removal together with the replacement and its draft.
    pub(super) fn release_reused_pane_removal(&mut self, card: CardId) -> Task<Message> {
        Arc::make_mut(&mut self.durable)
            .pending_chat_pane_removals
            .remove(&card);
        self.chat_ui.returns_to_remove.remove(&card);
        self.show_returned_close(card)
    }

    /// Forgets the stop saved for the closed pane `card` once the pane is
    /// gone, its conversation having stopped. Until then the next launch
    /// would take the pane away.
    pub(in crate::app) fn forget_stop_of_removed(&mut self, card: CardId) {
        let settled = |app: &Self, stopping: &StoppingChat| {
            stopping.card == card
                && stopping.refused.is_none()
                && app.heard_stopped(stopping.session.id)
        };
        if !self
            .durable
            .stopping_chats
            .iter()
            .any(|stopping| settled(self, stopping))
        {
            return;
        }
        let kept = self
            .durable
            .stopping_chats
            .iter()
            .filter(|stopping| !settled(self, stopping))
            .cloned()
            .collect();
        Arc::make_mut(&mut self.durable).stopping_chats = kept;
        self.persist();
    }

    /// Forgets each stop saved only for a closed pane that has since gone by
    /// some other route, as another window closing it, or its worktree's
    /// removal: nothing is left for the next launch to take away.
    fn forget_stops_of_gone_returns(&mut self) {
        let gone = |app: &Self, card: CardId| {
            !app.card_state(card)
                .is_some_and(|state| state.existence.is_open())
        };
        let cards = self
            .chat_ui
            .returns_to_remove
            .iter()
            .copied()
            .filter(|card| gone(self, *card))
            .collect::<Vec<_>>();
        for card in &cards {
            self.chat_ui.returns_to_remove.remove(card);
        }
        let settled = self
            .durable
            .stopping_chats
            .iter()
            .filter(|stopping| {
                stopping.refused.is_none()
                    && self.heard_stopped(stopping.session.id)
                    && gone(self, stopping.card)
            })
            .map(|stopping| stopping.card)
            .collect::<Vec<_>>();
        for card in settled {
            self.forget_stop_of_removed(card);
        }
        let gone_panes = self
            .durable
            .pending_chat_pane_removals
            .keys()
            .copied()
            .filter(|card| gone(self, *card))
            .collect::<Vec<_>>();
        for card in gone_panes {
            self.forget_pending_pane_removal(card);
        }
    }

    fn forget_pending_pane_removal(&mut self, card: CardId) {
        if self.durable.pending_chat_pane_removals.contains_key(&card) {
            Arc::make_mut(&mut self.durable)
                .pending_chat_pane_removals
                .remove(&card);
            self.persist();
        }
    }

    fn pending_pane_removal(&self, card: CardId, session: &chat::Session) -> bool {
        self.durable
            .pending_chat_pane_removals
            .get(&card)
            .is_some_and(|source| returned_pane_source_matches(source, session))
    }

    /// The saved stop of the pane while it still holds the same conversation.
    fn stopping_close_of(&self, card: CardId) -> Option<&StoppingChat> {
        let current = self.chat_return_session(card)?;
        self.durable.stopping_chats.iter().find(|stopping| {
            stopping.card == card
                && stopping.refused.is_none()
                && shares_conversation(&stopping.session, &current)
        })
    }

    /// Whether the closed pane `card` waits for its view to admit it before
    /// its removal can reach the view. A fenced pane's removal reaches it by
    /// identity, so that one does not wait.
    fn awaits_admission_to_go(&self, card: CardId) -> bool {
        self.chat_awaiting_admission(card) && !self.shared_views.quarantined.contains(&card)
    }

    /// Keeps out of sight each closed pane a shared view put back while its
    /// conversation stops, once the strip holds it, and brings back into
    /// sight one its close no longer holds.
    pub(in crate::app) fn hide_returned_closes(&mut self) -> Task<Message> {
        self.forget_stops_of_gone_returns();
        // A pane waiting for admission to go that its view fences instead
        // is never admitted: it goes now, by identity.
        let fenced = self
            .chat_ui
            .hidden_returns
            .iter()
            .filter(|card| self.shared_views.quarantined.contains(card))
            .filter_map(|card| {
                let stopped = self.chat_ui.stopped_closes.get(card);
                let saved = self.durable.pending_chat_pane_removals.get(card);
                Some((*card, stopped.or(saved)?.clone()))
            })
            .filter(|(card, _)| self.chat_awaiting_admission(*card))
            .collect::<Vec<_>>();
        let mut fenced_tasks = fenced
            .into_iter()
            .map(|(card, session)| self.take_away_returned_close(card, &session))
            .collect::<Vec<_>>();
        let held = self
            .chat_ui
            .background_closes
            .values()
            .filter(|closing| closing.reopens.contains(&closing.card))
            .map(|closing| closing.card)
            .chain(self.chat_ui.returns_to_remove.iter().copied())
            .chain(self.chat_ui.hidden_returns.iter().copied().filter(|card| {
                (self.chat_awaiting_admission(*card)
                    && self.chat_ui.stopped_closes.contains_key(card))
                    || self.stopping_close_of(*card).is_some()
                    || self.durable.pending_chat_pane_removals.contains_key(card)
            }))
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
        tasks.append(&mut fenced_tasks);
        Task::batch(tasks)
    }

    /// The closed panes kept out of sight while their conversation stops.
    pub(in crate::app) fn returned_closes(&self) -> Vec<CardId> {
        self.chat_ui.hidden_returns.iter().copied().collect()
    }

    /// Whether `card` is a closed pane kept out of sight while its
    /// conversation stops or its removal waits.
    pub(in crate::app) fn returned_close_is_hidden(&self, card: CardId) -> bool {
        self.chat_ui.hidden_returns.contains(&card)
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
        // A pane its view holds goes back to the conversation once admitted,
        // which takes the record then: Reopen goes to that pane. A pane still
        // due to close cannot take the reopen, even after its fence lifts.
        let awaiting = self
            .durable
            .stopping_chats
            .iter()
            .find(|stopping| stopping.session.id == id && stopping.refused.is_some())
            .and_then(|failure| {
                self.durable.cards.iter().find(|state| {
                    state.existence.is_open()
                        && self.chat_awaiting_admission(state.id)
                        && !self.shared_views.quarantined.contains(&state.id)
                        && matches!(&state.kind, CardKind::Chat(session)
                            if returned_pane_source_matches(&failure.session, session)
                                && !self.pending_pane_removal(state.id, session))
                })
            })
            .map(|state| state.id);
        if let Some(card) = awaiting {
            return cosmic::task::message(Message::FocusCard(card));
        }
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

fn connect_close_daemon(ensure_ready: bool) -> Result<ControlClient, String> {
    if ensure_ready {
        crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
    }
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    ControlClient::connect(&socket).map_err(|error| error.to_string())
}

fn read_close(target: Target, receipt: Uuid, ensure_ready: bool, delay: Duration) -> Task<Message> {
    cosmic::task::future(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let outcome = close_observer(move || {
            connect_close_daemon(ensure_ready)?
                .read_chat_close(receipt)
                .map_err(|error| error.reason())
        })
        .await;
        Message::ChatUi(Action::ChatCloseOutcome(target, receipt, outcome))
    })
}

/// A pending read or flush must never keep the window's async runtime alive.
async fn close_observer<T: Send + 'static>(
    observe: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (reply, received) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("chat-close-receipt".into())
        .spawn(move || {
            let _ = reply.send(observe());
        })
        .map_err(|error| error.to_string())?;
    received.await.map_err(|error| error.to_string())?
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
