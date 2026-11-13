use super::daemon_recovery::KeptPane;
use super::*;
use crate::{
    control::ControlClient, terminal::ipc::TerminalCloseResult, upgrade::UnreadableProcess,
};

#[derive(Debug, Clone)]
pub(super) struct CloseIntent {
    pub card: CardId,
    pub terminal: TerminalId,
    pub proof: HostSessionProof,
    completed_source: Option<TerminalSession>,
    /// The pane as Close found it. Only this exact state may be retired on a
    /// proof that its terminal is gone.
    saved_source: Option<TerminalSession>,
    /// The connection to this terminal was lost, so no close may be sent to
    /// it: Close may only ask whether the terminal is proven gone.
    probe_only: bool,
    pub generation: u64,
    pub restore_card: Option<CardId>,
    pub pending: bool,
    pub active: bool,
    pub detail: Option<String>,
    /// Processes Close couldn't read, which kept the pane: the person may
    /// close it anyway and leave them running.
    unreadable: Vec<UnreadableProcess>,
    /// Processes the person chose to leave running. Close passes over these
    /// and signals nothing.
    leave: Vec<UnreadableProcess>,
    restore_keyboard_nav: bool,
}

impl CloseIntent {
    /// Whether the dialog is on screen: asking about active processes,
    /// reporting why the pane was kept, or closing beside processes left
    /// running at the person's word.
    fn shows_dialog(&self) -> bool {
        self.active || self.detail.is_some() || !self.leave.is_empty()
    }

    /// The report can offer to close beside what it names.
    fn offers_leaving(&self) -> bool {
        self.detail.is_some() && !self.unreadable.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum CloseOutcome {
    Closed,
    Active,
    Stale,
    /// The owning daemon answered and the terminal is proven gone. Nothing was
    /// stopped: there is nothing left to stop.
    NotHosted,
    /// Not proven gone, for the reason given. Nothing was sent to the terminal.
    Kept(KeptPane),
    Unreachable(String),
}

#[derive(Debug, Clone)]
pub enum CompletedCloseOutcome {
    Closed,
    Active(HostSessionProof),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueuedCardClose {
    card: CardId,
    unasked: bool,
}

#[cfg(test)]
impl CloseIntent {
    pub(super) fn is_probe_only(&self) -> bool {
        self.probe_only
    }
}

fn target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    intent.completed_source.is_none()
        && matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal && session.continuity.live_proof().as_ref() == Some(&intent.proof))
}

fn terminal_session(state: Option<&CardState>) -> Option<&TerminalSession> {
    match state {
        Some(CardState {
            kind: CardKind::Terminal(session),
            ..
        }) => Some(session),
        _ => None,
    }
}

fn exited_target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal
            && intent.completed_source.as_ref().is_none_or(|source| source == session)
            && matches!(session.continuity,
            TerminalContinuity::Exited { .. } | TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }))
}

impl Cagents {
    /// Runs after every update, because not every way a close ends reports back:
    /// a retained chat's close can fail, or a pane can be refused at commit, with
    /// nothing left to tell the queue. Without this the workspace's only removal
    /// stays taken and the panes behind it stay disabled on "Checking…" until
    /// the next launch.
    pub(super) fn settle_close_queue(&mut self) -> Task<Message> {
        if let Some(card) = self
            .workspace
            .pending_close()
            .map(|pending| pending.unmounting)
            && self.close_commit_delivered == Some(card)
            && !self.removal_owner_running(card)
        {
            self.abandon_removal(card);
        }
        if self.queued_card_closes.is_empty() {
            Task::none()
        } else {
            self.next_queued_card_close()
        }
    }

    /// Gives up removing `card`, which stays open and can be closed again.
    pub(super) fn abandon_removal(&mut self, card: CardId) {
        if self
            .workspace
            .pending_close()
            .is_some_and(|pending| pending.unmounting == card)
        {
            let _ = self.workspace.cancel_close(card);
        }
        if self.close_commit_delivered == Some(card) {
            self.close_commit_delivered = None;
        }
        self.closed_terminal_cards.remove(&card);
        self.proven_gone_cards.remove(&card);
    }

    /// Whether something is still working towards removing `card` once its
    /// reservation has reached the commit.
    fn removal_owner_running(&self, card: CardId) -> bool {
        self.terminal_close
            .as_ref()
            .is_some_and(|intent| intent.card == card)
            || self.completed_release == Some(card)
            || self.chat_close_owned(card)
    }

    /// Every pane whose close the others have to wait for, each held until that
    /// pane settles. Whether a pane is owned, and whether the queue has to wait,
    /// are both read from this list, so they can't disagree.
    ///
    /// A chat's own close isn't here. It takes as long as its daemon takes to
    /// answer and needs nothing from another pane until its removal at the end,
    /// which `remove_chat_card` queues if another pane is holding it then.
    fn closes_under_way(&self) -> impl Iterator<Item = CardId> + '_ {
        let reserved = self
            .workspace
            .pending_close()
            .map(|pending| pending.unmounting);
        let asking = self.terminal_close.as_ref().map(|intent| intent.card);
        let dirty = self
            .dirty_repository_close
            .iter()
            .flat_map(|close| std::iter::once(close.card).chain(close.remaining.iter().copied()));
        reserved
            .into_iter()
            .chain(asking)
            .chain(self.completed_release)
            .chain(dirty)
            .chain(self.chrome_ui.closing_group.iter().copied())
    }

    /// Whether a close of a pane other than `card` has to finish first. The
    /// workspace removes one pane at a time, so a second started beside it would
    /// be refused at the end of its own close.
    pub(super) fn another_close_under_way(&self, card: CardId) -> bool {
        self.closes_under_way().any(|running| running != card)
    }

    /// Whether the next queued pane has to wait: for a close under way, or for
    /// whatever would drop the message that starts it. The update tail tries
    /// again once none of them hold.
    fn close_queue_held(&self) -> bool {
        self.closes_under_way().next().is_some()
            || self.update_prepared
            || self.modal_is_open()
            || self.chrome_ui.is_open()
    }

    pub(super) fn queue_card_close(&mut self, card: CardId, unasked: bool) {
        if let Some(queued) = self
            .queued_card_closes
            .iter_mut()
            .find(|queued| queued.card == card)
        {
            queued.unasked |= unasked;
        } else {
            self.queued_card_closes
                .push_back(QueuedCardClose { card, unasked });
        }
    }

    pub(super) fn next_queued_card_close(&mut self) -> Task<Message> {
        // Starting a pane runs a whole update, which comes back here. This loop
        // already moves on from whatever that update settles.
        if self.close_queue_advancing {
            return Task::none();
        }
        self.close_queue_advancing = true;
        let mut tasks = Vec::new();
        let mut kept = 0;
        while !self.close_queue_held() {
            let Some(QueuedCardClose { card, unasked }) = self.queued_card_closes.pop_front()
            else {
                break;
            };
            let open = |app: &Self| {
                app.card_state(card)
                    .is_some_and(|state| state.existence.is_open())
            };
            if !open(self) {
                continue;
            }
            // Each nested update logs and clears the status, so one set before
            // the queue moved is held back for the caller to report.
            let earlier_notice = self.status.take();
            // Acquire the next close's ownership before returning a task.
            // A delayed CardId-only message would leave Retry free to
            // replace its terminal between dequeue and dispatch.
            tasks.push(self.update(if unasked {
                Message::CloseCardUnasked(card)
            } else {
                Message::CloseCard(card)
            }));
            if self.status.is_none() {
                self.status = earlier_notice;
            }
            // Started, it holds the queue until it settles. Otherwise it was
            // turned away, and nothing else would tell the reader.
            if !self.close_queue_held() && open(self) && !self.pane_close_owned(card) {
                kept += 1;
            }
        }
        self.close_queue_advancing = false;
        // Each turned-away pane's own reason, if it has one, is already a notice.
        if kept > 0 {
            let notice = TransientNotice::warning(if kept == 1 {
                "A pane was kept. Close it again."
            } else {
                "Some panes were kept. Close them again."
            });
            if let Some(earlier) = self.status.replace(notice) {
                self.report_notice(earlier);
            }
        }
        Task::batch(tasks)
    }

    /// Close was taken for this pane and hasn't settled, with no dialog to say
    /// so. An answering daemon takes moments, or about five seconds behind a
    /// recovery. One that doesn't answer holds this until its 40 second timeout.
    pub(super) fn terminal_close_in_progress(&self, card: CardId) -> bool {
        self.terminal_close.as_ref().is_some_and(|intent| {
            intent.card == card && intent.pending && !intent.active && intent.detail.is_none()
        }) || self.completed_release == Some(card)
            || self
                .queued_card_closes
                .iter()
                .any(|queued| queued.card == card)
    }

    pub(super) fn terminal_close_dialog_visible(&self) -> bool {
        self.terminal_close
            .as_ref()
            .is_some_and(CloseIntent::shows_dialog)
    }

    /// Gives a visible close dialog the keyboard traversal to restore when it
    /// closes, in place of the one it recorded while a popup had suppressed
    /// it. Returns whether a dialog took it.
    pub(super) fn adopt_terminal_close_keyboard_nav(&mut self, restore: bool) -> bool {
        match self.terminal_close.as_mut() {
            Some(intent) if intent.shows_dialog() => {
                intent.restore_keyboard_nav = restore;
                true
            }
            _ => false,
        }
    }

    /// An accepted removal owns its pane until cancellation or commit, even
    /// while its asynchronous cleanup has no visible dialog.
    pub(super) fn pane_close_owned(&self, card: CardId) -> bool {
        self.closes_under_way().any(|running| running == card)
            || self.chat_close_owned(card)
            || self
                .queued_card_closes
                .iter()
                .any(|queued| queued.card == card)
            || self.chat_close_asked(card)
    }

    /// The daemon has been asked to close this pane's terminal and hasn't said
    /// how that went.
    pub(super) fn terminal_close_pending(&self, card: CardId) -> bool {
        self.terminal_close
            .as_ref()
            .is_some_and(|intent| intent.card == card && intent.pending)
            || self.completed_release == Some(card)
    }

    pub(super) fn begin_card_close(&mut self, card: CardId) -> Task<Message> {
        self.close_card_as(card, !self.asks(QUESTION), false)
    }

    /// Closes `card` without asking whether to stop what its terminal is
    /// running, for a close the reader already asked for in so many words -
    /// cancelling a sign-in, say. The daemon's identity checks still apply.
    pub(super) fn close_card_unasked(&mut self, card: CardId) -> Task<Message> {
        self.close_card_as(card, true, true)
    }

    /// Closes `card` from outside the strip on screen, which has no widget
    /// for it to hand focus from. It only retires the card: whatever the
    /// card ran must already have ended or been closed through the daemon,
    /// as a worktree's deletion retires the cards it held.
    pub(super) fn retire_offstrip_card(&mut self, card: CardId) -> Task<Message> {
        if self.workspace.order().any(|id| id == card)
            || self
                .card_state(card)
                .is_none_or(|state| !state.existence.is_open())
        {
            return Task::none();
        }
        // Read before anything persists the scope, which prunes the closed
        // card from its channel's membership.
        let emptied = self.close_emptied_channels(&[card]);
        self.retire_lsp_card(card);
        self.sidebar_state.forget_card_attention(card);
        self.cards.remove(&card);
        self.lifecycle.remove(card);
        Arc::make_mut(&mut self.durable).card_scopes.remove(&card);
        if let Some(state) = self.card_state_mut(card) {
            state.existence = Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: unix_millis(),
                    reason: Some("closed by user".into()),
                },
            };
        }
        self.persist();
        Task::batch([emptied, self.sync_chrome()])
    }

    fn close_card_as(&mut self, card: CardId, allow_active: bool, unasked: bool) -> Task<Message> {
        // Any pane waits behind a chat's question, so no second dialog opens
        // underneath it and takes its keys.
        if self.chat_close_asked(card) {
            return Task::none();
        }
        if self.chat_close_dialog_visible() {
            self.queue_card_close(card, unasked);
            return Task::none();
        }
        if self.pane_transfer_pending(card) {
            self.status = Some(TransientNotice::warning(
                "This pane is moving to a channel. Wait for the move to finish before closing it.",
            ));
            return Task::none();
        }
        if let Some(reason) = self.review_checkout_blocker(card) {
            self.status = Some(TransientNotice::warning(reason));
            return Task::none();
        }
        // Wait for the close under way, rather than have it refuse this pane at
        // the end of its own close.
        if self.another_close_under_way(card) {
            self.queue_card_close(card, unasked);
            return Task::none();
        }
        // This pane's own close is already running or asking, or the daemon has
        // answered and only its removal is left.
        if self.terminal_close.is_some()
            || self.completed_release == Some(card)
            || self.closed_terminal_cards.contains(&card)
        {
            return Task::none();
        }
        if let Some(session) = self.ended_terminal(card).cloned() {
            self.completed_release = Some(card);
            return cosmic::task::future(async move {
                let source = session.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let socket = crate::terminal::ensure_daemon_ready()
                        .map_err(|error| error.to_string())?;
                    let mut client =
                        ControlClient::connect(&socket).map_err(|error| error.to_string())?;
                    match client
                        .release_completed_terminal(source, allow_active)
                        .map_err(|error| error.to_string())?
                    {
                        TerminalCloseResult::Closed => Ok(CompletedCloseOutcome::Closed),
                        TerminalCloseResult::Active => {
                            Ok(CompletedCloseOutcome::Active(client.proof().clone()))
                        }
                        TerminalCloseResult::Unknown(error) => Err(error),
                    }
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                Message::CompletedTerminalClosed {
                    card,
                    session,
                    result,
                }
            });
        }
        if self.pending_terminal_spawn(card) {
            self.status = Some(TransientNotice::warning(
                "This terminal is still starting, so the pane was kept. Close it again once it has started.",
            ));
            return Task::none();
        }
        if let Some(CardState {
            kind: CardKind::Terminal(session),
            ..
        }) = self.card_state(card)
            && let Some(proof) = session
                .continuity
                .live_proof()
                .or_else(|| self.uncertain_terminal_proofs.get(&card).cloned())
        {
            let uncertain = self.uncertain_terminal_proofs.contains_key(&card);
            let probe_only = uncertain && session.continuity.live_proof().is_some();
            self.terminal_close = Some(CloseIntent {
                card,
                terminal: session.id,
                proof,
                completed_source: None,
                saved_source: Some(session.clone()),
                probe_only,
                generation: 0,
                restore_card: self.workspace.focused(),
                pending: false,
                active: false,
                unreadable: Vec::new(),
                leave: Vec::new(),
                detail: (uncertain && !probe_only).then(|| {
                    "Cagents lost contact with its daemon, so this terminal may still be running. The pane was kept. Use Retry or Reconnect on the pane if it offers them, then close it again.".into()
                }),
                restore_keyboard_nav: self.core.keyboard_nav(),
            });
            if self.terminal_close.as_ref().unwrap().detail.is_some() {
                self.core.set_keyboard_nav(false);
                return close_focus();
            }
            return self.start_terminal_close(allow_active);
        }
        if !unasked && let Some(asked) = self.ask_before_chat_close(card) {
            return asked;
        }
        self.request_card_removal(card)
    }

    pub(super) fn completed_terminal_closed(
        &mut self,
        card: CardId,
        session: TerminalSession,
        result: Result<CompletedCloseOutcome, String>,
    ) -> Task<Message> {
        if self.completed_release == Some(card) {
            self.completed_release = None;
        }
        if !self.card_state(card).is_some_and(|state| {
            state.existence.is_open()
                && matches!(&state.kind, CardKind::Terminal(current) if current == &session)
        }) {
            return self.next_queued_card_close();
        }
        match result {
            Ok(CompletedCloseOutcome::Closed) => {
                self.closed_terminal_cards.insert(card);
                self.request_card_removal(card)
            }
            Ok(CompletedCloseOutcome::Active(proof)) => {
                self.terminal_close = Some(CloseIntent {
                    card,
                    terminal: session.id,
                    proof,
                    completed_source: Some(session),
                    saved_source: None,
                    probe_only: false,
                    generation: 0,
                    restore_card: self.workspace.focused(),
                    restore_keyboard_nav: self.core.keyboard_nav(),
                    pending: false,
                    active: true,
                    detail: None,
                    unreadable: Vec::new(),
                    leave: Vec::new(),
                });
                self.core.set_keyboard_nav(false);
                self.open_confirmation(QUESTION, cancel_id(), confirm_id())
            }
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "Could not release the completed terminal: {error}. The pane was kept. Try Close again."
                )));
                self.next_queued_card_close()
            }
        }
    }

    pub(super) fn pending_terminal_spawn(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Loading))
            && self.terminal_start_in_flight(card)
            && terminal_continuity(self.card_state(card))
                .is_some_and(|continuity| continuity.live_proof().is_none())
    }

    pub(super) fn request_card_removal(&mut self, card: CardId) -> Task<Message> {
        let reservation = match self.workspace.pending_close() {
            Some(handoff) if handoff.unmounting == card => Ok(handoff),
            _ => self.workspace.request_close(card),
        };
        match reservation {
            Ok(handoff) => {
                // A fresh commit is on its way to this reservation.
                self.close_commit_delivered = None;
                self.card_removal(handoff)
            }
            Err(error) => {
                self.abandon_removal(card);
                tracing::warn!(%error, "pane removal was refused");
                self.status = Some(TransientNotice::error(
                    "This pane couldn't be removed right now, so it was kept. Close it again.",
                ));
                // Nothing will settle this close, so the queue moves on now. It
                // waits by itself for a removal still on its way.
                self.next_queued_card_close()
            }
        }
    }

    /// Moves focus off a card whose removal the workspace has reserved, then
    /// commits that removal.
    pub(super) fn card_removal(
        &mut self,
        handoff: crate::workspace::FocusHandoff,
    ) -> Task<Message> {
        // A neighbour that is a chat still starting has no input to take the
        // keyboard yet; it is owed the keyboard once the input is drawn.
        if let crate::workspace::FocusTarget::Card(next) = handoff.target {
            self.keyboard_owed = self.owes_chat_input(next).then_some(next);
        }
        if !self.workspace_surfaces_visible() {
            // The accepted close still commits, but its hidden pane cannot
            // take native focus from Work or the contextual inspector.
            return cosmic::task::message(Message::CommitClose(handoff.unmounting));
        }
        workspace_widget::focus_handoff_then(
            handoff,
            widget::Id::new(TOOLBAR_FOCUS_ID),
            |next| self.card_keyboard_focus(next),
            Message::CommitClose(handoff.unmounting),
        )
        .map(cosmic::Action::App)
    }

    /// Commits the workspace reservation before retiring any runtime or saved
    /// identity. A failed or stale reservation leaves the card untouched.
    pub(super) fn commit_card_removal(
        &mut self,
        id: CardId,
        replacement: Option<(
            LaunchRecipe,
            (usize, usize),
            Option<(TerminalId, TerminalId)>,
        )>,
    ) -> Option<Task<Message>> {
        if let Some(reason) = self.review_checkout_blocker(id) {
            if self
                .workspace
                .pending_close()
                .is_some_and(|pending| pending.unmounting == id)
            {
                let _ = self.workspace.cancel_close(id);
            }
            self.status = Some(TransientNotice::warning(reason));
            return None;
        }
        let handoff = self.workspace.pending_close();
        let replacing = replacement.is_some();
        let closed_descriptor = (!replacing).then(|| self.card_state(id).cloned()).flatten();
        let update = match self.workspace.commit_close(id) {
            Ok(update) => update,
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                return None;
            }
        };
        // Read before anything persists the scope, which prunes the
        // closed card from its channel's membership.
        let emptied = if replacing {
            Task::none()
        } else {
            self.close_emptied_channels(&[id])
        };
        // Nothing of a terminal proven gone is left to bring back. Reopening
        // could only run its saved command again, unasked.
        if let Some(card) = closed_descriptor.filter(|_| !self.proven_gone_cards.remove(&id)) {
            self.recently_closed
                .push((card, self.active_workspace_destination()));
            if self.recently_closed.len() > 32 {
                self.recently_closed.remove(0);
            }
        }
        self.retire_lsp_card(id);
        self.sidebar_state.forget_card_attention(id);
        self.chat_discard(id);
        // A draft still waiting out its debounce is the last thing
        // typed into the pane; closing it is not discarding it.
        let settings = if matches!(self.cards.get(&id), Some(CardRuntime::Settings)) {
            self.flush_settings_drafts()
        } else {
            Task::none()
        };
        let utility = self
            .card_state(id)
            .filter(|card| card.kind.is_utility())
            .map(|card| card.kind.clone());
        if let Some(runtime) = self.cards.remove(&id) {
            match runtime {
                CardRuntime::Terminal(_handle) => {
                    // Explicit confirmation already obtained the daemon acknowledgement.
                    // Dropping the surface only detaches; never send a second shutdown.
                }
                #[cfg(feature = "integrated-browser")]
                CardRuntime::Browser(browser) => {
                    self.begin_browser_teardown(browser.handle);
                }
                CardRuntime::Loading
                | CardRuntime::Failed(_)
                | CardRuntime::Completed
                | CardRuntime::Repository(_)
                | CardRuntime::Chat(_)
                | CardRuntime::PullRequests(_)
                | CardRuntime::Settings
                | CardRuntime::Accounts
                | CardRuntime::Automations => {}
                // A reopened board starts from the daemon's, not this one.
                CardRuntime::Work => self.work_ui.reset(),
            }
        }
        self.lifecycle.remove(id);
        if let Some(CardKind::Terminal(session)) = self.card_state(id).map(|card| &card.kind) {
            let terminal = session.id;
            self.retained_outputs.remove(&terminal);
        }
        // A closed card is never reopened, so its filing is spent.
        Arc::make_mut(&mut self.durable).card_scopes.remove(&id);
        if let Some(card) = self.card_state_mut(id) {
            card.existence = Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: unix_millis(),
                    reason: Some("closed by user".into()),
                },
            };
        }
        let closed = self.apply_viewport(update, true, DurableChange::Content);
        // A chat returned to a terminal is replaced only now that it
        // is closed, so no state ever holds both.
        let replacement = replacement
            .map(|(launch, slot, succession)| self.open_chat_replacement(launch, slot, succession))
            .unwrap_or_else(Task::none);
        let reopen = match utility {
            Some(kind) if self.reopen_utility.as_ref() == Some(&kind) => {
                self.reopen_utility = None;
                self.open_utility_card(kind)
            }
            _ => Task::none(),
        };
        let focus = if self.workspace_surfaces_visible()
            && !replacing
            && handoff
                .is_some_and(|handoff| handoff.target != crate::workspace::FocusTarget::Unchanged)
        {
            match self.workspace.focused() {
                Some(card) => self.focus_pane_keyboard(card),
                None => iced::widget::operation::focus(widget::Id::new(TOOLBAR_FOCUS_ID))
                    .map(cosmic::Action::App),
            }
        } else {
            Task::none()
        };
        Some(
            Task::batch([closed, replacement, settings, emptied, reopen, focus])
                .chain(self.next_queued_card_close()),
        )
    }

    pub(super) fn cancel_terminal_close(&mut self) -> Task<Message> {
        // A close already submitted to the owner cannot be recalled safely.
        if self
            .terminal_close
            .as_ref()
            .is_some_and(|intent| intent.pending)
        {
            return Task::none();
        }
        let Some(intent) = self.terminal_close.take() else {
            return Task::none();
        };
        self.dismiss_confirmation(QUESTION);
        // Dismissing a report ends the attempt it reported on. The fence
        // exists so one intent cannot submit two concurrent requests and so
        // no automatic path retries a request whose fate is unknown; it is
        // not a permanent refusal to ever close this terminal again. A
        // terminal id is a fresh UUID and the request carries the owner's
        // session proof, so a later explicit close cannot reach a different
        // terminal, and an owner that no longer knows this one answers
        // `Stale` rather than killing something else.
        if intent.detail.is_some() {
            self.terminal_close_attempts.remove(&intent.terminal);
        }
        self.core.set_keyboard_nav(intent.restore_keyboard_nav);
        let card = resolve_restore_card(intent.restore_card, Some(intent.card), |card| {
            self.workspace.order().any(|id| id == card)
        });
        if let Some(card) = card {
            let _ = self.workspace.focus(card);
        }
        match card {
            Some(card) => self.focus_pane_keyboard(card),
            None => iced::widget::operation::focus(widget::Id::new(TOOLBAR_FOCUS_ID))
                .map(cosmic::Action::App),
        }
        .chain(self.next_queued_card_close())
    }

    pub(super) fn confirm_terminal_close(&mut self) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if !intent.pending && intent.offers_leaving() {
            return self.close_leaving_unreadable();
        }
        if intent.pending || !intent.active || intent.detail.is_some() {
            return Task::none();
        }
        // The opt-out is committed only by Close, never by toggling or Cancel.
        // Do not persist a stale target's dialog either.
        let save = if target_matches(intent, self.card_state(intent.card))
            || exited_target_matches(intent, self.card_state(intent.card))
        {
            self.commit_confirmation(QUESTION)
        } else {
            self.dismiss_confirmation(QUESTION);
            Task::none()
        };
        Task::batch([save, self.start_terminal_close(true)])
    }

    /// "Close anyway" on a pane kept only by processes Close couldn't read.
    /// Close proves everything else again, and passes over exactly the
    /// processes the dialog named. It sends nothing to them, or to anything.
    fn close_leaving_unreadable(&mut self) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_mut() else {
            return Task::none();
        };
        let named = std::mem::take(&mut intent.unreadable);
        intent.leave.extend(named);
        intent.detail = None;
        // An active terminal still asks, as any Close does.
        self.start_terminal_close(false)
    }

    fn start_terminal_close(&mut self, allow_active: bool) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if intent.pending || intent.detail.is_some() {
            return Task::none();
        }
        if !target_matches(intent, self.card_state(intent.card))
            && !exited_target_matches(intent, self.card_state(intent.card))
        {
            self.terminal_close.as_mut().unwrap().detail =
                Some("This terminal changed before it could close, so nothing was closed. Try closing the pane again.".into());
            self.core.set_keyboard_nav(false);
            return close_focus();
        }
        if !self.terminal_close_attempts.insert(intent.terminal) {
            self.terminal_close.as_mut().unwrap().detail = Some(
                "A close was already attempted, so the pane was kept until Cagents knows how it ended."
                    .into(),
            );
            self.core.set_keyboard_nav(false);
            return close_focus();
        }
        self.terminal_close_generation = self.terminal_close_generation.wrapping_add(1);
        let intent = self.terminal_close.as_mut().unwrap();
        // Exit events may arrive before this request completes. Only the PTY
        // owner's kill receipt identifies an intentional exit; pending intent
        // alone must never suppress a concurrent crash or a rejected close.
        intent.pending = true;
        intent.generation = self.terminal_close_generation;
        let intent = intent.clone();
        cosmic::task::future(async move {
            let card = intent.card;
            let terminal = intent.terminal;
            let proof = intent.proof.clone();
            let generation = intent.generation;
            let outcome =
                tokio::task::spawn_blocking(move || revalidate_and_close(&intent, allow_active))
                    .await
                    .unwrap_or_else(|error| {
                        CloseOutcome::Unreachable(format!("Close worker failed: {error}"))
                    });
            Message::TerminalCloseFinished {
                card,
                terminal,
                proof,
                generation,
                outcome,
            }
        })
    }

    pub(super) fn terminal_close_finished(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        proof: HostSessionProof,
        generation: u64,
        outcome: CloseOutcome,
    ) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if !intent.pending
            || intent.card != card
            || intent.terminal != terminal
            || intent.proof != proof
            || intent.generation != generation
        {
            return Task::none();
        }
        let identity_matches = target_matches(intent, self.card_state(card))
            || exited_target_matches(intent, self.card_state(card));
        let still_saved = intent
            .saved_source
            .as_ref()
            .is_some_and(|saved| terminal_session(self.card_state(card)) == Some(saved));
        let starting = self.terminal_start_in_flight(card);
        match outcome {
            CloseOutcome::NotHosted if identity_matches && still_saved && !starting => {
                if intent.shows_dialog() {
                    self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                }
                self.terminal_close = None;
                // No request was sent, so nothing is outstanding for the fence.
                self.terminal_close_attempts.remove(&terminal);
                self.closed_terminal_cards.insert(card);
                self.proven_gone_cards.insert(card);
                self.request_card_removal(card)
            }
            CloseOutcome::Closed if identity_matches => {
                if intent.shows_dialog() {
                    self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                }
                self.terminal_close = None;
                // Answered, so nothing is outstanding for the fence to hold. A
                // pane the workspace then won't remove can be closed again.
                self.terminal_close_attempts.remove(&terminal);
                self.closed_terminal_cards.insert(card);
                self.request_card_removal(card)
            }
            CloseOutcome::Active if identity_matches => {
                // The owner positively declined to close: a later explicit Close
                // may issue a fresh, independently fenced request.
                self.terminal_close_attempts.remove(&terminal);
                let intent = self.terminal_close.as_mut().unwrap();
                intent.restore_card = self.workspace.focused();
                intent.restore_keyboard_nav = self.core.keyboard_nav();
                intent.pending = false;
                intent.active = true;
                self.core.set_keyboard_nav(false);
                self.open_confirmation(QUESTION, cancel_id(), confirm_id())
            }
            outcome => {
                // `Stale` and a mismatched identity both mean the request was
                // never applied to the card's terminal, so nothing is left
                // outstanding for the fence to hold.
                if !matches!(outcome, CloseOutcome::Unreachable(_)) {
                    self.terminal_close_attempts.remove(&terminal);
                }
                let intent = self.terminal_close.as_mut().unwrap();
                intent.restore_card = self.workspace.focused();
                intent.restore_keyboard_nav = self.core.keyboard_nav();
                intent.pending = false;
                intent.unreadable = match &outcome {
                    CloseOutcome::Kept(kept) => kept.unreadable.clone(),
                    _ => Vec::new(),
                };
                intent.detail = Some(match outcome {
                    CloseOutcome::NotHosted if starting => {
                        "This terminal is starting, so the pane was kept. Close it again when it finishes.".into()
                    }
                    CloseOutcome::Closed
                    | CloseOutcome::Active
                    | CloseOutcome::Stale
                    | CloseOutcome::NotHosted => {
                        "This terminal changed before it could close, so the pane was kept. Try closing it again.".into()
                    }
                    CloseOutcome::Kept(kept) => kept.reason,
                    CloseOutcome::Unreachable(error) => {
                        tracing::warn!(%error, "terminal close was not confirmed");
                        "Cagents couldn't confirm this terminal closed, so it may still be running. The pane was kept and Cagents won't retry on its own. Try Close again in a moment. If this keeps happening, leave the pane in place.".into()
                    }
                });
                self.core.set_keyboard_nav(false);
                close_focus()
            }
        }
    }

    pub(super) fn terminal_close_dialog(&self) -> Option<Element<'_, Message>> {
        let intent = self.terminal_close.as_ref()?;
        // Idle shells never show an active-process confirmation, even briefly.
        if !self.terminal_close_dialog_visible() {
            return None;
        }
        let mut rows: Vec<Element<'_, Message>> = vec![
            crate::design::title(if intent.offers_leaving() {
                leave_running_title(&intent.unreadable)
            } else if intent.detail.is_some() {
                "Pane kept".into()
            } else if intent.active || intent.leave.is_empty() {
                // Close anyway can meet a terminal the daemon hosts after all,
                // which asks about its active processes as any Close does.
                "Close active processes?".into()
            } else {
                "Closing pane".into()
            })
            .into(),
            // Panes can share a title's first words, and a group close asks
            // about each in turn, so this says exactly which one is meant.
            crate::design::caption(
                self.card_chrome
                    .get(&intent.card)
                    .map(|chrome| chrome.title.as_str())
                    .or_else(|| {
                        self.card_state(intent.card)
                            .map(|card| self.card_display_title(card))
                    })
                    .unwrap_or("Terminal")
                    .to_owned(),
            )
            .into(),
            crate::design::body(intent.detail.as_deref().unwrap_or(if intent.pending {
                "Checking terminal identity and closing…"
            } else {
                "This terminal or its paired conversation still owns running processes. Closing stops them."
            }))
            .into(),
        ];
        if intent.active && intent.detail.is_none() {
            rows.push(self.dont_ask_again_control(QUESTION, !intent.pending));
        }
        // A detail is an outcome the dialog is reporting, not a question it is
        // asking: it offers one way out rather than a refusal beside a
        // destructive button that can never be pressed. The dismissal keeps
        // the cancel id so focus traversal stays one rule for both shapes.
        // Keeping the pane stays the focused, first choice; closing beside
        // processes Close couldn't rule out is the deliberate one.
        let buttons = if intent.offers_leaving() {
            vec![super::dialog_ui::confirmation_actions(
                crate::ui_controls::button::secondary("Keep pane")
                    .id(cancel_id())
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose)),
                crate::ui_controls::button::destructive("Close anyway")
                    .id(confirm_id())
                    .on_press_maybe((!intent.pending).then_some(Message::ConfirmTerminalClose)),
            )]
        } else if intent.detail.is_some() {
            vec![
                crate::ui_controls::button::primary("Dismiss")
                    .id(cancel_id())
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose))
                    .into(),
            ]
        } else {
            vec![super::dialog_ui::confirmation_actions(
                crate::ui_controls::button::secondary("Cancel")
                    .id(cancel_id())
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose)),
                crate::ui_controls::button::destructive("Close")
                    .id(confirm_id())
                    .on_press_maybe(
                        (!intent.pending && intent.active).then_some(Message::ConfirmTerminalClose),
                    ),
            )]
        };
        rows.push(widget::row::with_children(buttons).spacing(space::S).into());
        Some(super::dialog_ui::confirmation_surface(
            widget::column::with_children(rows).spacing(space::M),
            560.0,
        ))
    }
}

/// The question Close anyway answers: "Close pane and leave ssh-agent
/// running?", or "…leave 3 processes running?".
fn leave_running_title(processes: &[UnreadableProcess]) -> String {
    match processes {
        [process] => format!(
            "Close pane and leave {} running?",
            process.name.as_deref().unwrap_or("a process")
        ),
        _ => format!(
            "Close pane and leave {} processes running?",
            processes.len()
        ),
    }
}

/// Closing a terminal ends its processes, but the pane can be opened again:
/// a routine question, unlike a report or Close anyway, which keep focus on
/// keeping the pane.
const QUESTION: settings::Confirmation = settings::Confirmation::CloseActiveTerminal;

fn cancel_id() -> widget::Id {
    widget::Id::new("dialog.terminal-close.cancel")
}

fn confirm_id() -> widget::Id {
    widget::Id::new("dialog.terminal-close.confirm")
}

/// A report, or Close anyway, opens on keeping the pane.
fn close_focus() -> Task<Message> {
    iced::widget::operation::focus(cancel_id()).map(cosmic::Action::App)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    /// The dialog asks the question its destructive button answers.
    #[test]
    fn leaving_processes_running_is_asked_by_name() {
        let agent = UnreadableProcess::fixture(1, 10, "ssh-agent");
        assert_eq!(
            leave_running_title(std::slice::from_ref(&agent)),
            "Close pane and leave ssh-agent running?"
        );
        assert_eq!(
            leave_running_title(&[agent, UnreadableProcess::fixture(2, 10, "sshd-session")]),
            "Close pane and leave 2 processes running?"
        );
    }

    fn routing_intent(probe_only: bool, saved: Option<TerminalSession>) -> CloseIntent {
        CloseIntent {
            card: CardId::new(),
            terminal: TerminalId::new(),
            proof: HostSessionProof {
                host_session_id: HostSessionId::new("host"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
            completed_source: None,
            saved_source: saved,
            probe_only,
            generation: 0,
            restore_card: None,
            pending: true,
            active: false,
            detail: None,
            unreadable: Vec::new(),
            leave: Vec::new(),
            restore_keyboard_nav: false,
        }
    }

    #[test]
    fn close_asks_for_proof_only_after_the_daemon_declines_the_saved_identity() {
        let app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let session = terminal_source(&app, card);
        let saved = routing_intent(false, Some(session.clone()));
        let no_proof = |_: &TerminalSession, _: &[UnreadableProcess]| -> Result<(), KeptPane> {
            panic!("the daemon answered, so no proof is needed")
        };
        assert!(matches!(
            close_or_prove_gone(&saved, || CloseOutcome::Closed, no_proof),
            CloseOutcome::Closed
        ));
        assert!(matches!(
            close_or_prove_gone(&saved, || CloseOutcome::Active, no_proof),
            CloseOutcome::Active
        ));
        assert!(matches!(
            close_or_prove_gone(&saved, || CloseOutcome::Unreachable("x".into()), no_proof),
            CloseOutcome::Unreachable(_)
        ));
        assert!(matches!(
            close_or_prove_gone(&saved, || CloseOutcome::Stale, |_, _| Ok(())),
            CloseOutcome::NotHosted
        ));
        assert!(matches!(
            close_or_prove_gone(&saved, || CloseOutcome::Stale, |_, _| Err("why".into())),
            CloseOutcome::Kept(kept) if kept.reason == "why"
        ));
        assert!(matches!(
            close_or_prove_gone(
                &routing_intent(false, None),
                || CloseOutcome::Stale,
                no_proof
            ),
            CloseOutcome::Stale
        ));
    }

    #[test]
    fn a_card_with_a_lost_connection_never_reaches_the_close_request() {
        let app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let session = terminal_source(&app, card);
        let probe = routing_intent(true, Some(session));
        let never = || -> CloseOutcome { panic!("a probe must not send a close") };
        assert!(matches!(
            close_or_prove_gone(&probe, never, |_, _| Ok(())),
            CloseOutcome::NotHosted
        ));
        assert!(matches!(
            close_or_prove_gone(&probe, never, |_, _| Err("still there".into())),
            CloseOutcome::Kept(_)
        ));
    }

    fn channel_close_fixture() -> (Cagents, CardId, crate::channel_work::Home) {
        use crate::workspace_model::{
            ModelChannel,
            views::{Pane, Scope, View},
        };
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let home = crate::channel_work::Home {
            registry: uuid::Uuid::new_v4(),
            channel: crate::model::ChannelId::new(),
            creation_ordinal: 4,
        };
        let project = ProjectId::new();
        app.active_workspace = Some("/tmp".into());
        app.workspace_model.identity = home.registry;
        app.workspace_model.channels.push(ModelChannel {
            id: home.channel,
            repository: "/tmp".into(),
            title: "Retained Home".into(),
            title_source: Default::default(),
            ordinal: home.creation_ordinal,
        });
        app.workspace_model.views.push(View {
            scope: Scope::Channel(home.channel),
            revision: 7,
            panes: vec![Pane::from_card(app.card_state(card).unwrap(), None).unwrap()],
            retired: vec![],
        });
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = Some(home.channel);
        durable.channels.push(crate::model::ProjectChannel {
            id: home.channel,
            project_id: project,
            title: "Retained Home".into(),
            title_source: Default::default(),
            ordinal: home.creation_ordinal,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
        (app, card, home)
    }

    fn assert_deferred_home_close(
        app: &mut Cagents,
        card: CardId,
        home: crate::channel_work::Home,
    ) {
        use crate::workspace_model::ModelEdit;
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(
            app.durable
                .channels
                .iter()
                .any(|channel| channel.id == home.channel)
        );
        assert!(
            !app.channel_ui.is_deleting(home.channel),
            "closing a pane is not explicit Home deletion"
        );
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == home.channel)
                .unwrap()
                .cards
                .is_empty(),
            "persist prunes closed membership after the original Home was captured"
        );
        assert!(app.channel_ui.retire_candidates.contains(&home));
        let (send, received) = async_channel::unbounded();
        app.workspace_model_tx = send;
        app.retire_settled_empty_channels();
        assert!(
            received.try_recv().is_err(),
            "the old occupied canonical view is insufficient"
        );
        app.workspace_model.views[0].panes.clear();
        app.workspace_model.views[0].revision = 8;
        app.retire_settled_empty_channels();
        assert_eq!(
            received.try_recv().unwrap(),
            ModelEdit::RetireEmptyChannel {
                home,
                expected_view_revision: 8,
            }
        );
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn final_pane_commit_defers_home_retirement_to_canonical_work_and_view_proof() {
        let (mut app, card, home) = channel_close_fixture();
        app.workspace.request_close(card).unwrap();
        drop(app.commit_card_removal(card, None).unwrap());
        assert_deferred_home_close(&mut app, card, home);
    }

    #[test]
    fn final_offstrip_pane_close_defers_home_retirement_to_canonical_work_and_view_proof() {
        let (mut app, card, home) = channel_close_fixture();
        app.workspace = Cagents::rebuild_workspace(&WorkspaceState::default());
        drop(app.retire_offstrip_card(card));
        assert_deferred_home_close(&mut app, card, home);
    }

    fn retry_close_fixture() -> (Cagents, CardId) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.set_residency(card, Residency::Hot);
        let state = app.card_state_mut(card).unwrap();
        let CardKind::Terminal(session) = &mut state.kind else {
            unreachable!();
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        (app, card)
    }

    fn terminal_source(app: &Cagents, card: CardId) -> TerminalSession {
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            unreachable!();
        };
        session.clone()
    }

    fn queued_terminal(app: &mut Cagents, like: CardId) -> CardId {
        let mut state = super::super::tests::terminal_card_state();
        let card = state.id;
        let mut session = terminal_source(app, like);
        session.id = TerminalId::new();
        state.kind = CardKind::Terminal(session);
        Arc::make_mut(&mut app.durable).cards.push(state);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        card
    }

    /// Queued panes show that their close is being checked, so one that is
    /// turned away can't be allowed to hold up the rest.
    #[test]
    fn a_pane_that_refuses_its_turn_does_not_strand_the_queue() {
        let (mut app, first) = retry_close_fixture();
        let moving = queued_terminal(&mut app, first);
        let next = queued_terminal(&mut app, first);
        app.mark_pane_moving(moving);
        app.queued_card_closes.extend([
            QueuedCardClose {
                card: moving,
                unasked: true,
            },
            QueuedCardClose {
                card: next,
                unasked: true,
            },
        ]);
        drop(app.next_queued_card_close());
        assert!(!app.terminal_close_in_progress(moving));
        assert!(!app.pane_close_owned(moving));
        assert!(app.pane_close_owned(next), "the next pane got its turn");
        assert!(app.queued_card_closes.is_empty());
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.starts_with("A pane was kept"))
        );
    }

    /// Not every owner reports back: a retained chat's close can fail, or find
    /// its session changed, with nothing to tell the queue.
    #[test]
    fn a_turn_abandoned_without_settling_still_advances_the_queue() {
        let (mut app, first) = retry_close_fixture();
        let turn = queued_terminal(&mut app, first);
        let behind = queued_terminal(&mut app, first);
        app.queued_card_closes.extend([
            QueuedCardClose {
                card: turn,
                unasked: true,
            },
            QueuedCardClose {
                card: behind,
                unasked: true,
            },
        ]);
        drop(app.next_queued_card_close());
        assert!(app.terminal_close_pending(turn));

        drop(app.update(Message::DismissStatus));
        assert_eq!(app.queued_card_closes.len(), 1, "its owner still has it");

        app.terminal_close = None;
        drop(app.update(Message::DismissStatus));
        assert!(app.queued_card_closes.is_empty());
        assert!(app.terminal_close_pending(behind));
    }

    /// The exit report can reach the app before the daemon's answer does. By the
    /// time the removal commits the pane is gone, and nothing about it may still
    /// count as a close the queue has to wait for.
    #[test]
    fn a_live_pane_closed_on_its_turn_hands_the_queue_on_when_its_exit_won_the_race() {
        let (mut app, first) = retry_close_fixture();
        let turn = queued_terminal(&mut app, first);
        let behind = queued_terminal(&mut app, first);
        app.queued_card_closes.extend([
            QueuedCardClose {
                card: turn,
                unasked: true,
            },
            QueuedCardClose {
                card: behind,
                unasked: true,
            },
        ]);
        drop(app.next_queued_card_close());
        let intent = app.terminal_close.clone().expect("the turn is running");
        assert_eq!(intent.card, turn);

        app.retire_ended_terminal(turn, 0, None, true);
        drop(app.update(Message::TerminalCloseFinished {
            card: turn,
            terminal: intent.terminal,
            proof: intent.proof.clone(),
            generation: intent.generation,
            outcome: CloseOutcome::Closed,
        }));
        drop(app.update(Message::CommitClose(turn)));

        assert!(!app.card_state(turn).unwrap().existence.is_open());
        assert!(!app.pane_close_owned(turn));
        assert!(
            !app.terminal_close_attempts.contains(&intent.terminal),
            "a settled close leaves no fence behind"
        );
        assert_eq!(
            app.terminal_close.as_ref().map(|intent| intent.card),
            Some(behind),
            "the pane behind it gets its turn"
        );
    }

    /// Between the daemon's answer and the commit there is nothing left to ask.
    /// A second request could only come back about a pane already gone.
    #[test]
    fn a_second_close_while_the_removal_commits_asks_the_daemon_nothing() {
        let (mut app, first) = retry_close_fixture();
        drop(app.update(Message::CloseCard(first)));
        let intent = app.terminal_close.clone().expect("the close is running");
        drop(app.update(Message::TerminalCloseFinished {
            card: first,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Closed,
        }));
        assert!(app.terminal_close.is_none());
        drop(app.update(Message::CloseCard(first)));
        assert!(app.terminal_close.is_none(), "no second request");
        drop(app.update(Message::CommitClose(first)));
        assert!(!app.card_state(first).unwrap().existence.is_open());
    }

    /// Pressing Close again on a pane whose close is running starts nothing, and
    /// leaves that close as it was.
    #[test]
    fn a_second_close_while_its_own_close_runs_starts_nothing() {
        let (mut app, card) = retry_close_fixture();
        drop(app.update(Message::CloseCard(card)));
        let running = app.terminal_close.clone().expect("the close is running");
        assert_eq!(app.close_card_as(card, false, false).units(), 0);
        let after = app.terminal_close.clone().expect("the close still runs");
        assert_eq!(after.generation, running.generation);
        assert_eq!(after.pending, running.pending);
    }

    /// The same for an ended terminal, which is released rather than closed.
    #[test]
    fn a_second_close_while_its_own_release_runs_starts_nothing() {
        let (mut app, card) = retry_close_fixture();
        app.retire_ended_terminal(card, 0, None, false);
        app.cards.insert(card, CardRuntime::Completed);
        assert!(app.close_card_as(card, false, false).units() > 0);
        assert_eq!(app.completed_release, Some(card));
        assert_eq!(app.close_card_as(card, false, false).units(), 0);
    }

    /// A pane the daemon has closed but the workspace wouldn't remove is still
    /// there, so its Close and Retry have to keep working.
    #[test]
    fn a_pane_refused_after_its_terminal_closed_can_be_closed_again() {
        let (mut app, first) = retry_close_fixture();
        let holder = queued_terminal(&mut app, first);
        drop(app.update(Message::CloseCard(first)));
        let intent = app.terminal_close.clone().expect("the close is running");
        let _ = app.workspace.request_close(holder).unwrap();

        app.retire_ended_terminal(first, 0, None, true);
        drop(app.update(Message::TerminalCloseFinished {
            card: first,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Closed,
        }));
        assert!(app.card_state(first).unwrap().existence.is_open());
        let _ = app.workspace.cancel_close(holder);

        assert!(!app.pane_close_owned(first));
        drop(app.update(Message::CloseCard(first)));
        assert!(
            app.pane_close_owned(first),
            "Close runs again instead of doing nothing"
        );
    }

    /// Each takes the workspace's only removal once the daemon has answered, so
    /// two asked at once would have one refused.
    #[test]
    fn ended_panes_closed_together_each_wait_their_turn() {
        let (mut app, first) = retry_close_fixture();
        let second = queued_terminal(&mut app, first);
        for card in [first, second] {
            app.retire_ended_terminal(card, 0, None, true);
        }
        app.chrome_ui.closing_group = vec![first, second];
        drop(app.close_pane_group());
        assert_eq!(app.completed_release, Some(first));
        assert!(app.terminal_close_in_progress(second));

        let session = terminal_source(&app, first);
        drop(app.update(Message::CompletedTerminalClosed {
            card: first,
            session,
            result: Ok(CompletedCloseOutcome::Closed),
        }));
        drop(app.update(Message::CommitClose(first)));
        assert!(!app.card_state(first).unwrap().existence.is_open());
        assert_eq!(app.completed_release, Some(second));
        assert!(app.status.is_none(), "neither pane was refused");
    }

    /// The message that starts a pane would be dropped behind a dialog, and the
    /// pane reported as kept. The queue waits for the dialog instead.
    #[test]
    fn an_open_dialog_holds_the_queue_instead_of_turning_panes_away() {
        let (mut app, first) = retry_close_fixture();
        let behind = queued_terminal(&mut app, first);
        app.queued_card_closes.push_back(QueuedCardClose {
            card: behind,
            unasked: false,
        });
        app.device_ui.open = true;
        drop(app.next_queued_card_close());
        assert_eq!(app.queued_card_closes.len(), 1);
        assert!(app.status.is_none());

        app.device_ui.open = false;
        drop(app.update(Message::DismissStatus));
        assert_eq!(
            app.terminal_close.as_ref().map(|intent| intent.card),
            Some(behind)
        );
    }

    /// An update drops the message that starts the next pane, so the pane stays
    /// queued until the update lets go.
    #[test]
    fn an_update_holds_the_queue_until_it_lets_go() {
        let (mut app, first) = retry_close_fixture();
        let behind = queued_terminal(&mut app, first);
        app.queue_card_close(behind, false);
        app.update_prepared = true;
        drop(app.next_queued_card_close());
        assert_eq!(app.queued_card_closes.len(), 1);
        assert!(app.terminal_close.is_none());

        app.update_prepared = false;
        drop(app.next_queued_card_close());
        assert_eq!(
            app.terminal_close.as_ref().map(|intent| intent.card),
            Some(behind)
        );
        assert!(app.status.is_none());
    }

    /// A pane behind a discard dialog or in a group is spoken for, so Retry can't
    /// replace its terminal and another pane's close has to wait.
    #[test]
    fn a_pane_waiting_in_a_dirty_close_or_a_group_is_owned() {
        let (mut app, first) = retry_close_fixture();
        let behind = queued_terminal(&mut app, first);
        let bystander = queued_terminal(&mut app, first);
        app.cards.insert(
            first,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(first))),
        );
        drop(app.update(Message::CloseCard(first)));
        app.dirty_repository_close
            .as_mut()
            .unwrap()
            .remaining
            .push_back(behind);
        assert!(app.pane_close_owned(first));
        assert!(app.pane_close_owned(behind));
        assert!(!app.pane_close_owned(bystander));
        assert!(app.another_close_under_way(bystander));

        app.dirty_repository_close = None;
        assert!(!app.pane_close_owned(behind));
        assert!(!app.another_close_under_way(bystander));

        app.chrome_ui.closing_group = vec![behind];
        assert!(app.pane_close_owned(behind));
        assert!(!app.another_close_under_way(behind));
        assert!(!app.pane_close_owned(bystander));
        assert!(app.another_close_under_way(bystander));
    }

    /// The commit reached a pane that's still closing, so nothing has been lost
    /// until that close finishes without another one.
    #[test]
    fn a_delivered_commit_is_given_up_only_once_nothing_is_closing_the_pane() {
        let (mut app, first) = retry_close_fixture();
        app.workspace.request_close(first).unwrap();
        app.close_commit_delivered = Some(first);
        app.completed_release = Some(first);
        drop(app.settle_close_queue());
        assert!(app.workspace.pending_close().is_some());

        app.completed_release = None;
        drop(app.settle_close_queue());
        assert!(app.workspace.pending_close().is_none());
        assert!(app.close_commit_delivered.is_none());
    }

    /// A close finishing after its first commit sends another, which the first
    /// one's record mustn't cancel while it's on its way.
    #[test]
    fn a_fresh_commit_is_not_mistaken_for_the_delivered_one() {
        let (mut app, first) = retry_close_fixture();
        app.workspace.request_close(first).unwrap();
        app.close_commit_delivered = Some(first);
        drop(app.request_card_removal(first));
        drop(app.settle_close_queue());
        assert!(app.workspace.pending_close().is_some());
    }

    /// Input stops while an update is being prepared, and a dropped commit is
    /// never sent again, so its reservation would block every later Close.
    #[test]
    fn a_removal_dropped_for_an_update_is_given_up() {
        let (mut app, first) = retry_close_fixture();
        app.workspace.request_close(first).unwrap();
        app.closed_terminal_cards.insert(first);
        app.update_prepared = true;
        drop(app.update(Message::CommitClose(first)));
        assert!(app.workspace.pending_close().is_none());
        assert!(app.card_state(first).unwrap().existence.is_open());
        assert!(!app.closed_terminal_cards.contains(&first));
        assert!(
            app.status.is_some(),
            "the pane stays, so the reader is told"
        );
    }

    /// Discarding an edit is the close the reader asked for. A pane already
    /// queued can't take its place while a message is on its way.
    #[test]
    fn a_discarded_edit_closes_ahead_of_the_panes_already_queued() {
        let (mut app, first) = retry_close_fixture();
        let behind = queued_terminal(&mut app, first);
        app.cards.insert(
            first,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(first))),
        );
        app.queued_card_closes.push_back(QueuedCardClose {
            card: behind,
            unasked: true,
        });
        drop(app.update(Message::CloseCard(first)));
        assert!(app.dirty_repository_close.is_some());
        drop(app.update(Message::DiscardAndCloseRepository(first)));
        assert!(app.dirty_repository_close.is_none());
        assert_eq!(
            app.terminal_close.as_ref().map(|intent| intent.card),
            Some(first),
            "the discarded pane's close starts at once"
        );
        assert_eq!(
            app.queued_card_closes.front().map(|queued| queued.card),
            Some(behind)
        );
    }

    #[test]
    fn panes_turned_away_together_are_reported_together() {
        let (mut app, first) = retry_close_fixture();
        let moving = [
            queued_terminal(&mut app, first),
            queued_terminal(&mut app, first),
        ];
        for card in moving {
            app.mark_pane_moving(card);
            app.queued_card_closes.push_back(QueuedCardClose {
                card,
                unasked: true,
            });
        }
        drop(app.next_queued_card_close());
        assert!(app.queued_card_closes.is_empty());
        assert!(
            moving
                .iter()
                .all(|card| !app.terminal_close_in_progress(*card))
        );
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.starts_with("Some panes")),
            "the rest would go without a word about them"
        );
    }

    #[test]
    fn panes_turned_away_before_one_that_closes_are_still_reported() {
        let (mut app, first) = retry_close_fixture();
        let moving = [
            queued_terminal(&mut app, first),
            queued_terminal(&mut app, first),
        ];
        let next = queued_terminal(&mut app, first);
        for card in moving {
            app.mark_pane_moving(card);
            app.queued_card_closes.push_back(QueuedCardClose {
                card,
                unasked: true,
            });
        }
        app.queued_card_closes.push_back(QueuedCardClose {
            card: next,
            unasked: true,
        });
        drop(app.next_queued_card_close());
        assert!(app.pane_close_owned(next));
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.starts_with("Some panes"))
        );
    }

    /// Another pane settling behind a refusal mustn't use up the group: the
    /// dialog blocks workspace messages, so each pane would be dropped unasked.
    #[test]
    fn a_visible_dialog_holds_the_queue_until_it_is_dismissed() {
        let (mut app, first) = retry_close_fixture();
        let next = queued_terminal(&mut app, first);
        drop(app.update(Message::CloseCardUnasked(first)));
        let intent = app.terminal_close.as_ref().unwrap().clone();
        drop(app.update(Message::TerminalCloseFinished {
            card: first,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Kept("The pane was kept.".into()),
        }));
        app.queued_card_closes.push_back(QueuedCardClose {
            card: next,
            unasked: true,
        });
        drop(app.next_queued_card_close());
        assert_eq!(app.queued_card_closes.len(), 1);
        assert!(app.status.is_none());
        drop(app.update(Message::CancelTerminalClose));
        assert!(app.queued_card_closes.is_empty());
        assert!(app.pane_close_owned(next));
    }

    #[test]
    fn a_close_with_nothing_to_settle_still_advances_the_queue() {
        let (mut app, first) = retry_close_fixture();
        let next = queued_terminal(&mut app, first);
        let unknown = CardId::new();
        app.closed_terminal_cards.insert(unknown);
        app.queued_card_closes.push_back(QueuedCardClose {
            card: next,
            unasked: true,
        });
        drop(app.request_card_removal(unknown));
        assert!(!app.closed_terminal_cards.contains(&unknown));
        assert!(app.status.is_some());
        assert!(app.pane_close_owned(next), "the next pane got its turn");
        assert!(app.queued_card_closes.is_empty());

        // A completed terminal's answer that arrives after its pane changed.
        let (mut app, first) = retry_close_fixture();
        let next = queued_terminal(&mut app, first);
        let mut replaced = terminal_source(&app, first);
        replaced.id = TerminalId::new();
        app.queued_card_closes.push_back(QueuedCardClose {
            card: next,
            unasked: true,
        });
        drop(app.completed_terminal_closed(
            first,
            replaced.clone(),
            Ok(CompletedCloseOutcome::Closed),
        ));
        assert!(app.queued_card_closes.is_empty(), "the queue moved on");
        assert!(app.pane_close_owned(next));

        // A release the daemon couldn't complete keeps its pane, and the rest go on.
        let (mut app, first) = retry_close_fixture();
        let next = queued_terminal(&mut app, first);
        let current = terminal_source(&app, first);
        app.queued_card_closes.push_back(QueuedCardClose {
            card: next,
            unasked: true,
        });
        drop(app.completed_terminal_closed(first, current, Err("daemon went away".into())));
        assert!(app.card_state(first).unwrap().existence.is_open());
        assert!(app.status.is_some());
        assert!(app.queued_card_closes.is_empty(), "the queue moved on");
        assert!(app.pane_close_owned(next));
    }

    #[test]
    fn a_removal_refused_at_commit_still_advances_the_queue() {
        let (mut app, first) = retry_close_fixture();
        let next = queued_terminal(&mut app, first);
        app.closed_terminal_cards.insert(first);
        app.queued_card_closes.push_back(QueuedCardClose {
            card: next,
            unasked: true,
        });
        // Nothing reserved this removal, so the workspace turns it down.
        drop(app.update(Message::CommitClose(first)));
        assert!(app.card_state(first).unwrap().existence.is_open());
        assert!(!app.closed_terminal_cards.contains(&first));
        assert!(app.queued_card_closes.is_empty(), "the queue moved on");
        assert!(app.pane_close_owned(next));
    }

    /// The dialog's popover keeps widget operations from the strip behind it, so
    /// moving focus there would highlight one tab over another pane's contents.
    /// The dialog names the pane instead, and the keyboard goes back on Dismiss.
    #[test]
    fn a_refusal_leaves_the_view_where_it_is() {
        let (mut app, first) = retry_close_fixture();
        let other = queued_terminal(&mut app, first);
        let _ = app.workspace.focus(first);
        app.workspace.take_widget_focus_request();
        app.set_residency(other, Residency::Hot);
        let state = app.card_state_mut(other).unwrap();
        let CardKind::Terminal(session) = &mut state.kind else {
            unreachable!();
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        drop(app.update(Message::CloseCard(other)));
        assert_eq!(app.workspace.focused(), Some(first), "still only checking");
        let intent = app.terminal_close.as_ref().unwrap().clone();
        assert_eq!(intent.card, other);
        drop(app.update(Message::TerminalCloseFinished {
            card: other,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Kept("The pane was kept.".into()),
        }));
        assert!(app.terminal_close_dialog_visible());
        assert_eq!(app.workspace.focused(), Some(first));
        assert_eq!(
            app.workspace.take_widget_focus_request(),
            None,
            "the next frame would take the keyboard from the dialog"
        );
        drop(app.update(Message::CancelTerminalClose));
        assert_eq!(app.workspace.focused(), Some(first));
    }

    #[test]
    fn queued_close_keeps_its_original_terminal_when_completion_offers_retry() {
        let (mut app, first) = retry_close_fixture();
        let mut second = super::super::tests::terminal_card_state();
        let queued = second.id;
        second.kind = CardKind::Terminal(terminal_source(&app, first));
        let CardKind::Terminal(session) = &mut second.kind else {
            unreachable!();
        };
        session.id = TerminalId::new();
        Arc::make_mut(&mut app.durable).cards.push(second);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.settings.confirm_terminal_close = true;

        // Tasks stay unpolled: the request is owned, but no daemon is used.
        drop(app.update(Message::CloseCardUnasked(first)));
        assert!(app.terminal_close_pending(first));
        assert!(!app.terminal_close_dialog_visible());
        drop(app.update(Message::CloseCardUnasked(queued)));
        let queued_intent = QueuedCardClose {
            card: queued,
            unasked: true,
        };
        assert_eq!(app.queued_card_closes.front(), Some(&queued_intent));
        // Both clicks were taken, and neither pane has a dialog to say so.
        assert!(app.terminal_close_in_progress(first));
        assert!(app.terminal_close_in_progress(queued));

        // This is the same completion transition used by TerminalWake.
        app.retire_ended_terminal(queued, 0, None, false);
        app.cards.insert(queued, CardRuntime::Completed);
        let source = terminal_source(&app, queued);
        let residency = app.residency(queued).clone();
        assert!(app.ended_terminal(queued).is_some());
        assert!(app.pane_close_owned(queued));
        assert!(!app.terminal_start_in_flight(queued));
        drop(app.update(Message::RetryCard(queued)));
        assert_eq!(terminal_source(&app, queued), source);
        assert_eq!(*app.residency(queued), residency);
        assert!(matches!(
            app.cards.get(&queued),
            Some(CardRuntime::Completed)
        ));
        assert!(!app.terminal_start_in_flight(queued));
        assert_eq!(app.queued_card_closes.front(), Some(&queued_intent));
        assert!(app.terminal_close_pending(first));

        // A different completed pane still permits an explicit restart.
        let unrelated = super::super::tests::terminal_card_state();
        let other = unrelated.id;
        Arc::make_mut(&mut app.durable).cards.push(unrelated);
        app.retire_ended_terminal(other, 0, None, false);
        let old_other = terminal_source(&app, other).id;
        drop(app.update(Message::RetryCard(other)));
        assert_ne!(terminal_source(&app, other).id, old_other);
        assert!(app.terminal_start_in_flight(other));
        assert_eq!(terminal_source(&app, queued), source);
    }

    #[test]
    fn completed_release_owns_retry_until_its_exact_receipt() {
        let (mut app, card) = retry_close_fixture();
        app.retire_ended_terminal(card, 0, None, false);
        app.cards.insert(card, CardRuntime::Completed);
        let source = terminal_source(&app, card);
        drop(app.update(Message::CloseCard(card)));
        assert!(app.terminal_close_pending(card));
        assert!(app.pane_close_owned(card));
        assert!(app.terminal_close.is_none());
        drop(app.update(Message::CancelTerminalClose));
        drop(app.update(Message::RetryCard(card)));
        assert_eq!(terminal_source(&app, card), source);
        assert!(!app.terminal_start_in_flight(card));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
        assert!(app.terminal_close_pending(card));

        // A reported failure retains the original and releases this attempt.
        drop(app.update(Message::CompletedTerminalClosed {
            card,
            session: source.clone(),
            result: Err("fixture release unavailable".into()),
        }));
        assert!(!app.pane_close_owned(card));
        assert_eq!(terminal_source(&app, card), source);
        drop(app.update(Message::RetryCard(card)));
        let replacement = terminal_source(&app, card);
        assert_ne!(replacement.id, source.id);
        assert_eq!(replacement.predecessor, Some(source.id));
        assert_eq!(replacement.launch, source.launch);
        assert!(app.terminal_start_in_flight(card));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    }

    #[test]
    fn completed_close_confirmation_cancel_allows_a_fresh_explicit_retry() {
        let (mut app, card) = retry_close_fixture();
        let proof = terminal_source(&app, card).continuity.live_proof().unwrap();
        app.retire_ended_terminal(card, 0, None, false);
        app.cards.insert(card, CardRuntime::Completed);
        let source = terminal_source(&app, card);
        drop(app.update(Message::CloseCard(card)));
        drop(app.update(Message::CompletedTerminalClosed {
            card,
            session: source.clone(),
            result: Ok(CompletedCloseOutcome::Active(proof)),
        }));
        assert!(app.terminal_close_dialog_visible());
        assert!(app.pane_close_owned(card));
        assert!(!app.terminal_close_pending(card));
        drop(app.update(Message::RetryCard(card)));
        assert_eq!(terminal_source(&app, card), source);
        assert!(!app.terminal_start_in_flight(card));

        drop(app.update(Message::CancelTerminalClose));
        assert!(!app.pane_close_owned(card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        drop(app.update(Message::RetryCard(card)));
        let replacement = terminal_source(&app, card);
        assert_ne!(replacement.id, source.id);
        assert_eq!(replacement.predecessor, Some(source.id));
        assert_eq!(replacement.launch, source.launch);
        assert!(app.terminal_start_in_flight(card));
    }

    #[test]
    fn cancelling_one_close_acquires_the_next_before_a_delayed_retry() {
        let (mut app, first) = retry_close_fixture();
        let mut second = super::super::tests::terminal_card_state();
        let queued = second.id;
        let CardKind::Terminal(session) = &mut second.kind else {
            unreachable!();
        };
        session.continuity = terminal_source(&app, first).continuity;
        Arc::make_mut(&mut app.durable).cards.push(second);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.settings.confirm_terminal_close = true;
        drop(app.update(Message::CloseCardUnasked(first)));
        drop(app.update(Message::CloseCardUnasked(queued)));
        assert_eq!(app.queued_card_closes.front().unwrap().card, queued);
        app.retire_ended_terminal(queued, 0, None, false);
        app.cards.insert(queued, CardRuntime::Completed);
        let source = terminal_source(&app, queued);

        // The first owner declines, allowing its normal confirmation/Cancel.
        let intent = app.terminal_close.as_ref().unwrap().clone();
        drop(app.update(Message::TerminalCloseFinished {
            card: first,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Active,
        }));
        assert!(app.terminal_close_dialog_visible());
        drop(app.update(Message::CancelTerminalClose));
        assert!(app.queued_card_closes.is_empty());
        assert!(app.terminal_close.is_none());
        assert!(app.terminal_close_pending(queued));
        assert!(app.pane_close_owned(queued));

        // The Cancel task is unpolled, so any other input can arrive before
        // its async release runs. Ownership must already fence this Retry.
        drop(app.update(Message::RetryCard(queued)));
        assert_eq!(terminal_source(&app, queued), source);
        assert!(!app.terminal_start_in_flight(queued));
        assert!(matches!(
            app.cards.get(&queued),
            Some(CardRuntime::Completed)
        ));
        drop(app.update(Message::CompletedTerminalClosed {
            card: queued,
            session: source.clone(),
            result: Ok(CompletedCloseOutcome::Closed),
        }));
        assert!(app.closed_terminal_cards.contains(&queued));
        assert_eq!(terminal_source(&app, queued), source);
        assert!(!app.terminal_start_in_flight(queued));
        drop(app.update(Message::CommitClose(queued)));
        assert!(!app.card_state(queued).unwrap().existence.is_open());
        assert!(app.card_state(first).unwrap().existence.is_open());
    }

    #[test]
    fn completed_close_waits_for_receipt_and_ignores_a_replaced_card() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.set_residency(card, Residency::Hot);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!();
        };
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(7),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        let source = session.clone();
        let _pending = app.begin_card_close(card);
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.terminal_close_pending(card));
        let _ = app.update(Message::CompletedTerminalClosed {
            card,
            session: source.clone(),
            result: Err("holder unavailable".into()),
        });
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.terminal_close_pending(card));
        let _pending = app.begin_card_close(card);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!();
        };
        session.id = TerminalId::new();
        let _ = app.update(Message::CompletedTerminalClosed {
            card,
            session: source,
            result: Ok(CompletedCloseOutcome::Closed),
        });
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.closed_terminal_cards.contains(&card));
    }

    #[test]
    fn completed_terminal_with_live_conversation_uses_fenced_close_confirmation() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.set_residency(card, Residency::Hot);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let source = session.clone();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("paired"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let _ = app.begin_card_close(card);
        let opening = app.update(Message::CompletedTerminalClosed {
            card,
            session: source.clone(),
            result: Ok(CompletedCloseOutcome::Active(proof.clone())),
        });
        // A routine question opens on its answer, so Enter closes the pane.
        assert_eq!(
            super::super::tests::focused_element(app.terminal_close_dialog().unwrap(), opening),
            Some(confirm_id())
        );
        assert!(matches!(
            super::super::dialog_ui::tests::enter_control(
                app.terminal_close_dialog().unwrap(),
                confirm_id()
            ),
            Message::ConfirmTerminalClose
        ));
        assert!(app.card_state(card).unwrap().existence.is_open());
        let _ = app.update(Message::ConfirmTerminalClose);
        let intent = app.terminal_close.as_ref().unwrap().clone();
        assert!(intent.pending);
        assert_eq!(intent.completed_source, Some(source));
        assert_eq!(intent.proof, proof);
        let _ = app.update(Message::TerminalCloseFinished {
            card,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Closed,
        });
        assert!(app.closed_terminal_cards.contains(&card));
    }

    #[test]
    fn close_dialog_over_a_popup_restores_the_readers_keyboard_nav() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.set_residency(card, Residency::Hot);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let source = session.clone();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("paired"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        app.notices.record(TransientNotice::warning("first"));
        app.core.set_keyboard_nav(true);
        let _ = app.update(Message::ChromeUi(chrome_ui::Action::ToggleNotices));
        assert!(app.chrome_ui.is_open());
        assert!(!app.core.keyboard_nav());

        // The completed close's confirmation arrives while the popup still
        // holds the shell's traversal aside.
        let _ = app.begin_card_close(card);
        let _ = app.update(Message::CompletedTerminalClosed {
            card,
            session: source,
            result: Ok(CompletedCloseOutcome::Active(proof)),
        });
        assert!(app.terminal_close_dialog().is_some());
        assert!(
            !app.core.keyboard_nav(),
            "the dialog owns Tab while it is open"
        );

        let _ = app.update(Message::CancelTerminalClose);
        assert!(app.terminal_close.is_none());
        assert!(!app.chrome_ui.is_open());
        assert!(
            app.core.keyboard_nav(),
            "the reader's traversal survives the popup and the dialog"
        );
    }

    #[test]
    fn completed_confirmation_cannot_close_a_relaunched_card() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.set_residency(card, Residency::Hot);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        let original_continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("paired"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let proof = original_continuity.live_proof().unwrap();
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let source = session.clone();
        let _ = app.begin_card_close(card);
        let _ = app.update(Message::CompletedTerminalClosed {
            card,
            session: source,
            result: Ok(CompletedCloseOutcome::Active(proof)),
        });
        let _ = app.update(Message::ConfirmTerminalClose);
        let intent = app.terminal_close.as_ref().unwrap().clone();
        assert!(intent.pending);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = original_continuity;
        let _ = app.update(Message::TerminalCloseFinished {
            card,
            terminal: intent.terminal,
            proof: intent.proof,
            generation: intent.generation,
            outcome: CloseOutcome::Closed,
        });
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.closed_terminal_cards.contains(&card));
        assert!(app.terminal_close.as_ref().unwrap().detail.is_some());
    }

    #[test]
    fn an_invisible_pending_close_does_not_block_the_whole_workspace() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!();
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        app.core.set_keyboard_nav(true);
        let _close = app.begin_card_close(card);

        assert!(
            app.core.keyboard_nav(),
            "a hidden close keeps keyboard navigation"
        );
        assert!(app.terminal_close_pending(card));
        assert!(!app.modal_is_open());
        assert!(app.dialog().is_none());

        app.settings_reset_confirm = true;
        assert!(
            app.dialog().is_some(),
            "a hidden close cannot mask another dialog"
        );
        app.settings_reset_confirm = false;

        let intent = app.terminal_close.as_ref().unwrap();
        let (terminal, proof, generation) =
            (intent.terminal, intent.proof.clone(), intent.generation);
        app.core.set_keyboard_nav(false);
        let _dialog =
            app.terminal_close_finished(card, terminal, proof, generation, CloseOutcome::Active);
        assert!(app.modal_is_open());
        assert!(app.dialog().is_some());
        let _cancel = app.cancel_terminal_close();
        assert!(
            !app.core.keyboard_nav(),
            "dismissal keeps the reader's newer choice"
        );
    }

    #[test]
    fn queued_login_cancellation_keeps_its_unasked_intent() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let first = app.workspace.focused().unwrap();
        let CardKind::Terminal(session) = &mut app.card_state_mut(first).unwrap().kind else {
            unreachable!();
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        app.settings.confirm_terminal_close = true;
        let _ = app.close_card_unasked(first);
        assert!(app.terminal_close_pending(first));

        let mut login_state = super::super::tests::terminal_card_state();
        let login = login_state.id;
        let CardKind::Terminal(session) = &mut login_state.kind else {
            unreachable!();
        };
        session.continuity = terminal_source(&app, first).continuity;
        let login_terminal = session.id;
        Arc::make_mut(&mut app.durable).cards.push(login_state);
        let _ = app.begin_card_close(login);
        let _ = app.close_card_unasked(login);

        assert_eq!(
            app.queued_card_closes.front().copied(),
            Some(QueuedCardClose {
                card: login,
                unasked: true,
            }),
            "a later explicit cancellation upgrades a queued ordinary close"
        );
        app.terminal_close = None;
        drop(app.next_queued_card_close());
        assert!(app.queued_card_closes.is_empty());
        let next = app.terminal_close.as_ref().unwrap();
        assert_eq!((next.card, next.terminal), (login, login_terminal));
        assert_eq!(
            next.proof,
            terminal_source(&app, login)
                .continuity
                .live_proof()
                .unwrap()
        );
        assert!(next.pending);
        assert!(!next.active);
    }
}

fn revalidate_and_close(intent: &CloseIntent, allow_active: bool) -> CloseOutcome {
    if let Some(source) = &intent.completed_source {
        let run = || -> Result<CloseOutcome, String> {
            let path =
                crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
            let mut client = ControlClient::connect(&path).map_err(|error| error.to_string())?;
            if client.proof() != &intent.proof || !intent.proof.is_complete() {
                return Ok(CloseOutcome::Stale);
            }
            client
                .release_completed_terminal(source.clone(), allow_active)
                .map_err(|error| error.to_string())
                .map(|outcome| match outcome {
                    TerminalCloseResult::Closed => CloseOutcome::Closed,
                    TerminalCloseResult::Active => CloseOutcome::Active,
                    TerminalCloseResult::Unknown(error) => CloseOutcome::Unreachable(error),
                })
        };
        return run().unwrap_or_else(CloseOutcome::Unreachable);
    }
    close_or_prove_gone(
        intent,
        || close_hosted_terminal(intent.terminal, &intent.proof, allow_active),
        super::daemon_recovery::prove_not_hosted,
    )
}

/// `close` is the only route that can stop anything, so a card whose connection
/// was lost never reaches it. Its `Stale` means the daemon doesn't host the saved
/// identity. That is what a dead terminal looks like, and also what an unknown
/// one looks like, so only a positive proof may retire the pane.
fn close_or_prove_gone(
    intent: &CloseIntent,
    close: impl FnOnce() -> CloseOutcome,
    prove: impl FnOnce(&TerminalSession, &[UnreadableProcess]) -> Result<(), KeptPane>,
) -> CloseOutcome {
    if !intent.probe_only {
        match close() {
            CloseOutcome::Stale => {}
            outcome => return outcome,
        }
    }
    let Some(saved) = &intent.saved_source else {
        return CloseOutcome::Stale;
    };
    match prove(saved, &intent.leave) {
        Ok(()) => CloseOutcome::NotHosted,
        Err(kept) => CloseOutcome::Kept(kept),
    }
}

/// Asks the daemon to close `terminal`, provided the daemon is still the one
/// `proof` names and still hosts it as that session. A terminal that has
/// already ended counts as closed; one that is gone, or hosted under another
/// proof, is `Stale` and left alone.
pub(super) fn close_hosted_terminal(
    terminal: TerminalId,
    proof: &HostSessionProof,
    allow_active: bool,
) -> CloseOutcome {
    let run = || -> Result<CloseOutcome, String> {
        let path = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
        let mut client = ControlClient::connect(&path).map_err(|error| error.to_string())?;
        if client.proof() != proof || !proof.is_complete() {
            return Ok(CloseOutcome::Stale);
        }
        let terminals = client.list().map_err(|error| error.to_string())?;
        let Some(descriptor) = terminals
            .iter()
            .find(|descriptor| descriptor.session.id == terminal)
        else {
            return Ok(CloseOutcome::Stale);
        };
        match &descriptor.session.continuity {
            TerminalContinuity::Exited { .. }
            | TerminalContinuity::RelaunchAvailable {
                last_exit: Some(_), ..
            } => {
                return client
                    .release_completed_terminal(descriptor.session.clone(), allow_active)
                    .map_err(|error| error.to_string())
                    .map(|outcome| match outcome {
                        TerminalCloseResult::Closed => CloseOutcome::Closed,
                        TerminalCloseResult::Active => CloseOutcome::Active,
                        TerminalCloseResult::Unknown(error) => CloseOutcome::Unreachable(error),
                    });
            }
            continuity if continuity.live_proof().as_ref() == Some(proof) => {}
            _ => return Ok(CloseOutcome::Stale),
        }
        Ok(
            match client
                .close_terminal_checked(terminal, allow_active)
                .map_err(|error| error.to_string())?
            {
                TerminalCloseResult::Closed => CloseOutcome::Closed,
                TerminalCloseResult::Active => CloseOutcome::Active,
                TerminalCloseResult::Unknown(detail) => CloseOutcome::Unreachable(detail),
            },
        )
    };
    run().unwrap_or_else(CloseOutcome::Unreachable)
}
