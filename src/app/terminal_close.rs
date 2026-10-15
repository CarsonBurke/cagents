use super::*;
use crate::{control::ControlClient, terminal::ipc::TerminalCloseResult};

#[derive(Debug, Clone)]
pub(super) struct CapturedTerminal {
    source: TerminalSession,
    execution_instance: uuid::Uuid,
    proof: HostSessionProof,
}

/// Per-pane bindings survive cold/completed runtime transitions. Inventory
/// can corroborate an adoption, never choose a successor for old consent.
#[derive(Default)]
pub(super) struct Bindings {
    bound: HashMap<CardId, CapturedTerminal>,
    adoptions: HashMap<CardId, (uuid::Uuid, HostSessionProof, uuid::Uuid, HostSessionProof)>,
    removals: HashMap<CardId, CapturedTerminal>,
}

#[derive(Debug, Clone)]
pub(super) struct CloseIntent {
    pub card: CardId,
    pub terminal: TerminalId,
    pub proof: HostSessionProof,
    completed_source: Option<TerminalSession>,
    captured: Option<CapturedTerminal>,
    pub generation: u64,
    pub restore_card: Option<CardId>,
    pub pending: bool,
    pub active: bool,
    pub dont_ask_again: bool,
    pub detail: Option<String>,
    focus: usize,
    restore_keyboard_nav: bool,
}

#[derive(Debug, Clone)]
pub enum CloseOutcome {
    Closed,
    Active,
    Stale,
    Unreachable(String),
}

#[derive(Debug, Clone)]
pub enum CompletedCloseOutcome {
    Closed,
    ClosedWorker {
        execution_instance: uuid::Uuid,
        proof: HostSessionProof,
    },
    /// Presentation dismissal only; no execution/retirement RPC was issued.
    Dismissed {
        expected_worker: Option<(uuid::Uuid, HostSessionProof)>,
    },
    Active(HostSessionProof),
    ActiveWorker {
        source: TerminalSession,
        execution_instance: uuid::Uuid,
        proof: HostSessionProof,
    },
}

#[derive(Debug, Clone)]
pub(super) struct QueuedCardClose {
    card: CardId,
    unasked: bool,
    kind: Option<CardKind>,
    captured: Option<CapturedTerminal>,
}

fn same_captured(expected: Option<&CapturedTerminal>, current: Option<&CapturedTerminal>) -> bool {
    match (expected, current) {
        (None, None) => true,
        (Some(expected), Some(current)) => {
            expected.execution_instance == current.execution_instance
                && expected.proof == current.proof
                && expected.source.id == current.source.id
                && expected.source.launch == current.source.launch
        }
        _ => false,
    }
}

fn target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    intent.completed_source.is_none()
        && matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal && session.continuity.live_proof().as_ref() == Some(&intent.proof)
            && intent.captured.as_ref().is_none_or(|captured| captured.source.id == session.id && captured.source.launch == session.launch))
}

fn exited_target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal
            && intent.captured.as_ref().is_none_or(|captured| captured.source.id == session.id && captured.source.launch == session.launch)
            && intent.completed_source.as_ref().is_none_or(|source| source == session)
            && matches!(session.continuity,
            TerminalContinuity::Exited { .. } | TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }))
}

impl Cagents {
    pub(super) fn capture_terminal_close_binding(&mut self, card: CardId, spawned: bool) {
        // A successful explicit spawn invalidates old authority even when the
        // new attachment has not published authenticated metadata yet.
        if spawned {
            self.terminal_close_bindings.bound.remove(&card);
            self.terminal_close_bindings.adoptions.remove(&card);
        }
        let Some(CardRuntime::Terminal(surface)) = self.cards.get(&card) else {
            return;
        };
        let Some(binding) = surface.worker_binding() else {
            return;
        };
        let Some(proof) = surface.host_proof().filter(HostSessionProof::is_complete) else {
            return;
        };
        self.record_terminal_close_binding(card, binding, proof, spawned);
    }

    fn record_terminal_close_binding(
        &mut self,
        card: CardId,
        binding: crate::terminal::HostedTerminalBinding,
        proof: HostSessionProof,
        spawned: bool,
    ) {
        if binding.execution_instance.is_nil() || !proof.is_complete() {
            return;
        }
        let Some(CardKind::Terminal(saved)) = self.card_state(card).map(|state| &state.kind) else {
            return;
        };
        if saved.id != binding.source.id || saved.launch != binding.source.launch {
            return;
        }
        if saved
            .continuity
            .live_proof()
            .is_some_and(|saved| saved != proof)
            || binding
                .source
                .continuity
                .live_proof()
                .is_some_and(|issued| issued != proof)
        {
            return;
        }
        let known = self.terminal_close_bindings.bound.get(&card);
        if !spawned && known.is_none() && ended_terminal(self.card_state(card)).is_some() {
            return;
        }
        if let Some(previous) = known {
            if previous.execution_instance == binding.execution_instance && previous.proof == proof
            {
                return;
            }
            let adopted = self
                .terminal_close_bindings
                .adoptions
                .get(&card)
                .is_some_and(|(before, before_proof, after, after_proof)| {
                    *before == previous.execution_instance
                        && *before_proof == previous.proof
                        && *after == binding.execution_instance
                        && *after_proof == proof
                });
            if !spawned && previous.source.id == binding.source.id && !adopted {
                return;
            }
        }
        self.terminal_close_bindings.bound.insert(
            card,
            CapturedTerminal {
                source: binding.source,
                execution_instance: binding.execution_instance,
                proof,
            },
        );
        self.terminal_close_bindings.adoptions.remove(&card);
    }

    pub(super) fn corroborate_terminal_close_adoption(
        &mut self,
        descriptor: &crate::terminal::ipc::TerminalDescriptor,
    ) {
        if descriptor.execution_instance.is_nil() {
            return;
        }
        for (card, previous) in &self.terminal_close_bindings.bound {
            if previous.source.id == descriptor.session.id
                && descriptor
                    .session
                    .continuity
                    .live_proof()
                    .is_some_and(|proof| proof != previous.proof)
                && descriptor.upgrade_sources.iter().any(|source| {
                    *source == previous.source
                        || crate::agent_execution::HostedSession::Terminal(source.clone())
                            .same_process(&crate::agent_execution::HostedSession::Terminal(
                                previous.source.clone(),
                            ))
                })
            {
                self.terminal_close_bindings.adoptions.insert(
                    *card,
                    (
                        previous.execution_instance,
                        previous.proof.clone(),
                        descriptor.execution_instance,
                        descriptor
                            .session
                            .continuity
                            .live_proof()
                            .expect("complete adoption proof checked"),
                    ),
                );
            }
        }
    }

    pub(super) fn forget_terminal_close_binding(&mut self, card: CardId) {
        self.terminal_close_bindings.bound.remove(&card);
        self.terminal_close_bindings.adoptions.remove(&card);
        self.terminal_close_bindings.removals.remove(&card);
    }

    fn terminal_close_binding_current(&self, intent: &CloseIntent) -> bool {
        intent.captured.as_ref().is_none_or(|captured| {
            self.terminal_close_bindings
                .bound
                .get(&intent.card)
                .is_some_and(|current| {
                    current.execution_instance == captured.execution_instance
                        && current.proof == captured.proof
                        && current.source.id == captured.source.id
                        && current.source.launch == captured.source.launch
                })
        })
    }

    fn queued_card_close(&mut self, card: CardId, unasked: bool) -> QueuedCardClose {
        self.capture_terminal_close_binding(card, false);
        QueuedCardClose {
            card,
            unasked,
            kind: self.card_state(card).map(|state| state.kind.clone()),
            captured: self.terminal_close_bindings.bound.get(&card).cloned(),
        }
    }

    pub(super) fn close_cards_in_order(&mut self, cards: Vec<CardId>) -> Task<Message> {
        self.project_recovery_ui.closing_panes = true;
        let selected = cards
            .into_iter()
            .map(|card| self.queued_card_close(card, false))
            .collect::<Vec<_>>();
        self.queued_card_closes.extend(selected);
        self.next_queued_card_close()
    }

    pub(super) fn next_queued_card_close(&mut self) -> Task<Message> {
        while let Some(queued) = self.queued_card_closes.pop_front() {
            if self
                .card_state(queued.card)
                .is_some_and(|state| state.existence.is_open())
            {
                return cosmic::task::message(Message::CloseQueuedCard(queued));
            }
        }
        self.project_recovery_ui.closing_panes = false;
        Task::none()
    }

    pub(super) fn resolve_queued_card_close(&mut self, queued: QueuedCardClose) -> Option<Message> {
        self.capture_terminal_close_binding(queued.card, false);
        let current = self
            .card_state(queued.card)
            .filter(|state| state.existence.is_open());
        let kind_matches = match (queued.kind.as_ref(), current.map(|state| &state.kind)) {
            (Some(CardKind::Terminal(expected)), Some(CardKind::Terminal(current))) => {
                expected.id == current.id
                    && expected.launch == current.launch
                    && current.continuity.live_proof().is_none_or(|proof| {
                        Some(proof)
                            == queued
                                .captured
                                .as_ref()
                                .map(|binding| binding.proof.clone())
                                .or_else(|| expected.continuity.live_proof())
                    })
            }
            (Some(expected), Some(current)) => expected == current,
            _ => false,
        };
        let binding_matches = same_captured(
            queued.captured.as_ref(),
            self.terminal_close_bindings.bound.get(&queued.card),
        );
        if !kind_matches
            || !binding_matches
            || self.terminal_start_in_flight(queued.card)
            || self.chat_start_pending(queued.card)
        {
            self.queued_card_closes.clear();
            self.project_recovery_ui.closing_panes = false;
            self.status = Some(TransientNotice::warning(
                "A selected pane changed while Close panes was waiting. Its new session was preserved; select Close again.",
            ));
            return None;
        }
        Some(if queued.unasked {
            Message::CloseCardUnasked(queued.card)
        } else {
            Message::CloseCard(queued.card)
        })
    }

    pub(super) fn terminal_close_dialog_visible(&self) -> bool {
        self.terminal_close
            .as_ref()
            .is_some_and(|intent| intent.active || intent.detail.is_some())
    }

    /// Gives a visible close dialog the keyboard traversal to restore when it
    /// closes, in place of the one it recorded while a popup had suppressed
    /// it. Returns whether a dialog took it.
    pub(super) fn adopt_terminal_close_keyboard_nav(&mut self, restore: bool) -> bool {
        match self.terminal_close.as_mut() {
            Some(intent) if intent.active || intent.detail.is_some() => {
                intent.restore_keyboard_nav = restore;
                true
            }
            _ => false,
        }
    }

    pub(super) fn terminal_close_pending(&self, card: CardId) -> bool {
        self.terminal_close
            .as_ref()
            .is_some_and(|intent| intent.card == card && intent.pending)
            || ended_terminal(self.card_state(card))
                .is_some_and(|session| self.terminal_close_attempts.contains(&session.id))
    }

    pub(super) fn begin_card_close(&mut self, card: CardId) -> Task<Message> {
        self.close_card_as(card, !self.settings.confirm_terminal_close, false)
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
        let emptied = self.channels_emptied_by(&[card]);
        self.retire_lsp_card(card);
        self.sidebar_state.clear_agent_activity(card);
        self.sidebar_state.acknowledge_agent_update(card);
        self.cards.remove(&card);
        self.forget_terminal_close_binding(card);
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
        let emptied = emptied
            .into_iter()
            .filter(|&channel| !self.channel_has_open_card(channel))
            .collect();
        self.persist();
        Task::batch([self.delete_channels(emptied), self.sync_chrome()])
    }

    fn close_card_as(&mut self, card: CardId, allow_active: bool, unasked: bool) -> Task<Message> {
        if self.terminal_start_in_flight(card)
            || (matches!(
                self.card_state(card).map(|state| &state.kind),
                Some(CardKind::Terminal(_))
            ) && self.chat_start_pending(card))
        {
            self.status = Some(TransientNotice::warning(
                "This pane still has a session handoff or startup in progress. Wait for its identity before closing it.",
            ));
            if self.project_recovery_ui.closing_panes {
                self.queued_card_closes.clear();
                self.project_recovery_ui.closing_panes = false;
            }
            return Task::none();
        }
        self.capture_terminal_close_binding(card, false);
        let captured = self.terminal_close_bindings.bound.get(&card).cloned();
        if let Some(session) = ended_terminal(self.card_state(card)).cloned() {
            if !self.terminal_close_attempts.insert(session.id) {
                return Task::none();
            }
            return cosmic::task::future(async move {
                let source = session.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let socket = crate::terminal::ensure_daemon_ready()
                        .map_err(|error| error.to_string())?;
                    let mut client =
                        ControlClient::connect(&socket).map_err(|error| error.to_string())?;
                    close_completed_with_client(
                        &mut client,
                        &source,
                        captured.as_ref(),
                        allow_active,
                    )
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
        if let Some(intent) = &self.terminal_close {
            // A silent close is still in flight. Preserve subsequent close
            // clicks until that card is settled, instead of dropping them.
            if !intent.active && intent.detail.is_none() && intent.card != card {
                if let Some(queued) = self
                    .queued_card_closes
                    .iter_mut()
                    .find(|queued| queued.card == card)
                {
                    queued.unasked |= unasked;
                } else {
                    let queued = self.queued_card_close(card, unasked);
                    self.queued_card_closes.push_back(queued);
                }
            }
            return Task::none();
        }
        if self.pending_terminal_spawn(card) {
            self.status = Some(TransientNotice::warning(
                "Terminal is still starting. The card is kept until the daemon supplies its identity; then close it again.",
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
            self.terminal_close = Some(CloseIntent {
                card,
                terminal: session.id,
                proof,
                completed_source: None,
                captured,
                generation: 0,
                restore_card: self.workspace.focused(),
                pending: false,
                active: false,
                dont_ask_again: false,
                detail: self.uncertain_terminal_proofs.contains_key(&card).then(|| {
                    "Daemon unreachable; the card is retained. Reconnect before closing.".into()
                }),
                focus: 0,
                restore_keyboard_nav: self.core.keyboard_nav(),
            });
            if self.terminal_close.as_ref().unwrap().detail.is_some() {
                self.core.set_keyboard_nav(false);
                return close_focus(0);
            }
            return self.start_terminal_close(allow_active);
        }
        self.request_card_removal(card)
    }

    pub(super) fn completed_terminal_closed(
        &mut self,
        card: CardId,
        session: TerminalSession,
        result: Result<CompletedCloseOutcome, String>,
    ) -> Task<Message> {
        self.terminal_close_attempts.remove(&session.id);
        if !self.card_state(card).is_some_and(|state| {
            state.existence.is_open()
                && matches!(&state.kind, CardKind::Terminal(current) if current == &session)
        }) {
            return Task::none();
        }
        let mut captured = self.terminal_close_bindings.bound.get(&card).cloned();
        let result = match result {
            Ok(CompletedCloseOutcome::ClosedWorker {
                execution_instance,
                proof,
            }) => {
                if captured.as_ref().is_none_or(|current| {
                    current.execution_instance != execution_instance || current.proof != proof
                }) {
                    return Task::none();
                }
                Ok(CompletedCloseOutcome::Closed)
            }
            Ok(CompletedCloseOutcome::ActiveWorker {
                source,
                execution_instance,
                proof,
            }) => {
                if captured.as_ref().is_none_or(|current| {
                    current.execution_instance != execution_instance || current.proof != proof
                }) {
                    return Task::none();
                }
                captured = Some(CapturedTerminal {
                    source,
                    execution_instance,
                    proof: proof.clone(),
                });
                Ok(CompletedCloseOutcome::Active(proof))
            }
            result => result,
        };
        match result {
            Ok(CompletedCloseOutcome::Dismissed { expected_worker }) => {
                if captured
                    .as_ref()
                    .map(|current| (current.execution_instance, current.proof.clone()))
                    != expected_worker
                {
                    return Task::none();
                }
                // A List absence is not an atomic execution or shared-pane generation fence.
                // Keep its shared membership and history rather than hiding a successor.
                self.status = Some(TransientNotice::warning(
                    "No resident terminal was found. Its saved pane was kept because absence cannot authorize shared session removal. Refresh its connection, or use guarded project removal after inspecting remaining sessions.",
                ));
                Task::none()
            }
            Ok(CompletedCloseOutcome::Closed) => {
                if let Some(captured) = captured {
                    self.terminal_close_bindings.removals.insert(card, captured);
                }
                self.closed_terminal_cards.insert(card);
                self.request_card_removal(card)
            }
            Ok(CompletedCloseOutcome::Active(proof)) => {
                if self.terminal_close.is_some() {
                    let queued = self.queued_card_close(card, false);
                    self.queued_card_closes.push_back(queued);
                    return Task::none();
                }
                self.terminal_close = Some(CloseIntent {
                    card,
                    terminal: session.id,
                    proof,
                    completed_source: Some(session),
                    captured,
                    generation: 0,
                    restore_card: self.workspace.focused(),
                    restore_keyboard_nav: self.core.keyboard_nav(),
                    pending: false,
                    active: true,
                    dont_ask_again: false,
                    detail: None,
                    focus: 1,
                });
                self.core.set_keyboard_nav(false);
                close_focus(1)
            }
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "Could not release the completed terminal: {error}. Its card is retained; retry Close."
                )));
                Task::none()
            }
            Ok(
                CompletedCloseOutcome::ClosedWorker { .. }
                | CompletedCloseOutcome::ActiveWorker { .. },
            ) => unreachable!("bound outcomes normalized above"),
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
            Ok(handoff) => self.card_removal(handoff),
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                Task::none()
            }
        }
    }

    /// Moves focus off a card whose removal the workspace has reserved, then
    /// commits that removal.
    pub(super) fn card_removal(&self, handoff: crate::workspace::FocusHandoff) -> Task<Message> {
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
        if let Some(expected) = self.terminal_close_bindings.removals.get(&id) {
            let matches=same_captured(Some(expected),self.terminal_close_bindings.bound.get(&id))
                && self.card_state(id).is_some_and(|state| matches!(&state.kind,CardKind::Terminal(source) if source.id==expected.source.id && source.launch==expected.source.launch))
                && !self.terminal_start_in_flight(id) && !self.chat_start_pending(id);
            if !matches {
                let _ = self.workspace.cancel_close(id);
                self.closed_terminal_cards.remove(&id);
                self.terminal_close_bindings.removals.remove(&id);
                self.status = Some(TransientNotice::warning(
                    "The pane changed before its close was committed. Its new session was preserved.",
                ));
                return None;
            }
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
        let emptied = self.channels_emptied_by(&[id]);
        if let Some(card) = closed_descriptor {
            self.recently_closed
                .push((card, self.active_workspace_destination()));
            if self.recently_closed.len() > 32 {
                self.recently_closed.remove(0);
            }
        }
        self.retire_lsp_card(id);
        self.sidebar_state.clear_agent_activity(id);
        self.sidebar_state.acknowledge_agent_update(id);
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
            }
        }
        self.lifecycle.remove(id);
        self.forget_terminal_close_binding(id);
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
        let closed = self.apply_viewport(update, true, true);
        // A chat returned to a terminal is replaced only now that it
        // is closed, so no state ever holds both.
        let replacement = replacement
            .map(|(launch, slot, succession)| self.open_chat_replacement(launch, slot, succession))
            .unwrap_or_else(Task::none);
        // The replacement joins the chat's channel, which is then not
        // empty after all.
        let emptied = emptied
            .into_iter()
            .filter(|&channel| !self.channel_has_open_card(channel))
            .collect();
        let reopen = match utility {
            Some(kind) if self.reopen_utility.as_ref() == Some(&kind) => {
                self.reopen_utility = None;
                self.open_utility_card(kind)
            }
            _ => Task::none(),
        };
        let focus = if !replacing
            && handoff
                .is_some_and(|handoff| handoff.target != crate::workspace::FocusTarget::Unchanged)
        {
            iced::widget::operation::focus(
                self.workspace
                    .focused()
                    .map(|card| self.card_keyboard_focus(card))
                    .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID)),
            )
            .map(cosmic::Action::App)
        } else {
            Task::none()
        };
        Some(
            Task::batch([
                closed,
                replacement,
                settings,
                self.delete_channels(emptied),
                reopen,
                focus,
            ])
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
        if self.project_recovery_ui.closing_panes {
            self.queued_card_closes.clear();
            self.project_recovery_ui.closing_panes = false;
        }
        // Dismissing a report ends the attempt it reported on. The fence
        // exists so one intent cannot submit two concurrent requests and so
        // no automatic path retries a request whose fate is unknown; it is
        // not a permanent refusal to ever close this terminal again. A
        // later explicit close still uses its retained source and worker UUID;
        // an old intent never authorizes a replacement under the same ID.
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
        iced::widget::operation::focus(
            card.map(|card| self.card_keyboard_focus(card))
                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID)),
        )
        .map(cosmic::Action::App)
        .chain(self.next_queued_card_close())
    }

    pub(super) fn confirm_terminal_close(&mut self) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if intent.pending || !intent.active || intent.detail.is_some() {
            return Task::none();
        }
        // The opt-out is committed only by Close, never by toggling or Cancel.
        // Do not persist a stale target's dialog either.
        let save = if (target_matches(intent, self.card_state(intent.card))
            || exited_target_matches(intent, self.card_state(intent.card)))
            && self.terminal_close_binding_current(intent)
            && intent.dont_ask_again
        {
            self.apply_setting_input(SettingsField::ConfirmTerminalClose, "false".into())
        } else {
            Task::none()
        };
        Task::batch([save, self.start_terminal_close(true)])
    }

    fn start_terminal_close(&mut self, allow_active: bool) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if intent.pending || intent.detail.is_some() {
            return Task::none();
        }
        if !self.terminal_close_binding_current(intent)
            || (!target_matches(intent, self.card_state(intent.card))
                && !exited_target_matches(intent, self.card_state(intent.card)))
        {
            self.terminal_close.as_mut().unwrap().detail =
                Some("Terminal identity or daemon proof changed. Nothing was closed.".into());
            self.core.set_keyboard_nav(false);
            return close_focus(0);
        }
        if !self.terminal_close_attempts.insert(intent.terminal) {
            self.terminal_close.as_mut().unwrap().detail = Some(
                "A close was already attempted. The card is retained until its outcome is known."
                    .into(),
            );
            self.core.set_keyboard_nav(false);
            return close_focus(0);
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
        let identity_matches = self.terminal_close_binding_current(intent)
            && (target_matches(intent, self.card_state(card))
                || exited_target_matches(intent, self.card_state(card)));
        match outcome {
            CloseOutcome::Closed if identity_matches => {
                if intent.active || intent.detail.is_some() {
                    self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                }
                if let Some(captured) = intent.captured.clone() {
                    self.terminal_close_bindings.removals.insert(card, captured);
                }
                self.terminal_close = None;
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
                intent.focus = 1;
                self.core.set_keyboard_nav(false);
                close_focus(1)
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
                intent.focus = 0;
                intent.detail = Some(match outcome {
                    CloseOutcome::Closed | CloseOutcome::Active | CloseOutcome::Stale => {
                        "Terminal identity or daemon proof changed. The card was kept.".into()
                    }
                    CloseOutcome::Unreachable(error) => format!(
                        "Close unconfirmed: {error}. The card was kept; no automatic retry will be sent."
                    ),
                });
                self.core.set_keyboard_nav(false);
                close_focus(0)
            }
        }
    }

    pub(super) fn cycle_terminal_close_focus(&mut self, backwards: bool) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_mut() else {
            return Task::none();
        };
        if intent.pending {
            return Task::none();
        }
        intent.focus = if intent.detail.is_some() || !intent.active {
            0
        } else {
            (intent.focus + if backwards { 2 } else { 1 }) % 3
        };
        close_focus(intent.focus)
    }

    pub(super) fn terminal_close_dialog(&self) -> Option<Element<'_, Message>> {
        let intent = self.terminal_close.as_ref()?;
        // Idle shells never show an active-process confirmation, even briefly.
        if !self.terminal_close_dialog_visible() {
            return None;
        }
        let mut rows: Vec<Element<'_, Message>> = vec![
            crate::design::title(if intent.detail.is_some() {
                "Terminal kept open"
            } else {
                "Close active processes?"
            })
            .into(),
            crate::design::body(intent.detail.as_deref().unwrap_or(if intent.pending {
                "Checking terminal identity and closing…"
            } else {
                "This terminal or its paired conversation still owns running processes. Closing stops them."
            }))
            .into(),
        ];
        if intent.active && intent.detail.is_none() {
            rows.push(
                widget::button::custom(
                    crate::ui_controls::field::checkbox(intent.dont_ask_again)
                        .label("Don't ask me again")
                        .on_toggle_maybe(
                            (!intent.pending).then_some(Message::TerminalCloseDontAskChanged),
                        ),
                )
                .class(cosmic::theme::Button::Text)
                .id(widget::Id::new("dialog.terminal-close.dont-ask"))
                .on_press_maybe(
                    (!intent.pending)
                        .then_some(Message::TerminalCloseDontAskChanged(!intent.dont_ask_again)),
                )
                .into(),
            );
        }
        // A detail is an outcome the dialog is reporting, not a question it is
        // asking: it offers one way out rather than a refusal beside a
        // destructive button that can never be pressed. The dismissal keeps
        // the cancel id so focus traversal stays one rule for both shapes.
        let buttons = if intent.detail.is_some() {
            vec![
                crate::ui_controls::button::primary("Dismiss")
                    .id(widget::Id::new("dialog.terminal-close.cancel"))
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose))
                    .into(),
            ]
        } else {
            vec![
                crate::ui_controls::button::secondary("Cancel")
                    .id(widget::Id::new("dialog.terminal-close.cancel"))
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose))
                    .into(),
                crate::ui_controls::button::destructive("Close")
                    .id(widget::Id::new("dialog.terminal-close.confirm"))
                    .on_press_maybe(
                        (!intent.pending && intent.active).then_some(Message::ConfirmTerminalClose),
                    )
                    .into(),
            ]
        };
        rows.push(widget::row::with_children(buttons).spacing(space::S).into());
        Some(super::dialog_ui::surface(
            widget::column::with_children(rows).spacing(space::M),
            560.0,
        ))
    }
}

fn close_focus(index: usize) -> Task<Message> {
    iced::widget::operation::focus(widget::Id::new(match index {
        1 => "dialog.terminal-close.confirm",
        2 => "dialog.terminal-close.dont-ask",
        _ => "dialog.terminal-close.cancel",
    }))
    .map(cosmic::Action::App)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    fn bound_fixture() -> (Cagents, CardId, CapturedTerminal) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("captured-worker"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("original"),
        };
        let CardKind::Terminal(source) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!()
        };
        source.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let captured = CapturedTerminal {
            source: source.clone(),
            execution_instance: uuid::Uuid::new_v4(),
            proof,
        };
        app.terminal_close_bindings
            .bound
            .insert(card, captured.clone());
        (app, card, captured)
    }

    fn descriptor_for(captured: &CapturedTerminal) -> crate::terminal::ipc::TerminalDescriptor {
        let mut descriptor = crate::terminal::ipc::TerminalDescriptor::relaunchable(
            captured.source.id,
            captured.source.launch.clone(),
            None,
        );
        descriptor.session = captured.source.clone();
        descriptor.execution_instance = captured.execution_instance;
        descriptor
    }

    #[test]
    fn completed_close_wire_never_uses_legacy_release_or_successor_worker() {
        use crate::terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        };
        for (resident, successor, known) in [
            (true, false, true),
            (true, true, true),
            (false, false, true),
            (true, false, false),
            (false, false, false),
        ] {
            let (_app, _card, captured) = bound_fixture();
            let mut descriptor = descriptor_for(&captured);
            descriptor.session.continuity = TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            };
            if successor {
                descriptor.execution_instance = uuid::Uuid::new_v4();
            }
            let expected = descriptor.clone();
            let (reader, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
            let proof = captured.proof.clone();
            let expected_worker = captured.execution_instance;
            let handle = std::thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let connect: ClientEnvelope = read_frame(&mut server).unwrap().unwrap();
                assert!(matches!(connect.request, ClientRequest::Connect { .. }));
                let reply = |stream: &mut std::os::unix::net::UnixStream, id, response| {
                    write_frame(
                        stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(id),
                            proof: proof.clone(),
                            message: ServerMessage::Response { response },
                        },
                    )
                    .unwrap()
                };
                reply(
                    &mut server,
                    connect.request_id,
                    ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                );
                let list: ClientEnvelope = read_frame(&mut server).unwrap().unwrap();
                assert!(matches!(list.request, ClientRequest::List));
                reply(
                    &mut server,
                    list.request_id,
                    ServerResponse::Listed {
                        terminals: if resident { vec![descriptor] } else { vec![] },
                    },
                );
                if resident && !successor && known {
                    let request: ClientEnvelope = read_frame(&mut server).unwrap().unwrap();
                    assert!(
                        matches!(&request.request,ClientRequest::CloseTerminalSourceChecked {source,execution_instance,allow_active:true} if *execution_instance==expected_worker && *source==expected.session)
                    );
                    reply(
                        &mut server,
                        request.request_id,
                        ServerResponse::TerminalCloseChecked {
                            terminal_id: expected.session.id,
                            outcome: TerminalCloseResult::Closed,
                        },
                    );
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut server).unwrap().is_none(),
                    "no legacy release or successor-directed close may follow"
                );
            });
            let mut client = ControlClient::from_stream(reader).unwrap();
            let outcome = close_completed_with_client(
                &mut client,
                &captured.source,
                known.then_some(&captured),
                true,
            );
            if resident && !successor && known {
                assert!(
                    matches!(outcome,Ok(CompletedCloseOutcome::ClosedWorker {execution_instance,..}) if execution_instance==captured.execution_instance)
                );
            } else if !resident {
                assert!(matches!(
                    outcome,
                    Ok(CompletedCloseOutcome::Dismissed { .. })
                ));
            } else {
                assert!(outcome.is_err());
            }
            drop(client);
            handle.join().unwrap();
        }
    }

    #[test]
    fn close_consent_keeps_original_worker_when_identical_source_is_replaced_before_confirmation() {
        let (mut app, card, captured) = bound_fixture();
        let _ = app.begin_card_close(card);
        let intent = app.terminal_close.clone().unwrap();
        let _ = app.terminal_close_finished(
            card,
            intent.terminal,
            intent.proof.clone(),
            intent.generation,
            CloseOutcome::Active,
        );
        let mut replacement = captured.clone();
        replacement.execution_instance = uuid::Uuid::new_v4();
        app.terminal_close_bindings.bound.insert(card, replacement);
        assert_eq!(
            app.terminal_close
                .as_ref()
                .unwrap()
                .captured
                .as_ref()
                .unwrap()
                .execution_instance,
            captured.execution_instance
        );
        let _ = app.start_terminal_close(true);
        assert!(!app.terminal_close.as_ref().unwrap().pending);
        assert!(app.terminal_close.as_ref().unwrap().detail.is_some());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.closed_terminal_cards.contains(&card));
    }

    #[test]
    fn source_checked_close_fences_replacement_and_allows_natural_completion_of_same_worker() {
        let (_app, _card, captured) = bound_fixture();
        let mut descriptor = descriptor_for(&captured);
        assert!(captured_matches_descriptor(&captured, &descriptor));
        descriptor.session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        assert!(captured_matches_descriptor(&captured, &descriptor));
        descriptor.execution_instance = uuid::Uuid::new_v4();
        assert!(
            !captured_matches_descriptor(&captured, &descriptor),
            "same ID and launch do not identify the worker"
        );
    }

    #[test]
    fn completed_worker_receipt_or_absence_dismissal_cannot_remove_a_new_bound_pane() {
        for outcome in [
            CompletedCloseOutcome::ClosedWorker {
                execution_instance: uuid::Uuid::nil(),
                proof: bound_fixture().2.proof,
            },
            CompletedCloseOutcome::Dismissed {
                expected_worker: None,
            },
        ] {
            let (mut app, card, captured) = bound_fixture();
            let CardKind::Terminal(source) = &mut app.card_state_mut(card).unwrap().kind else {
                panic!()
            };
            source.continuity = TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            };
            let ended = source.clone();
            let outcome = match outcome {
                CompletedCloseOutcome::ClosedWorker { .. } => CompletedCloseOutcome::ClosedWorker {
                    execution_instance: captured.execution_instance,
                    proof: captured.proof.clone(),
                },
                outcome => outcome,
            };
            let mut replacement = captured;
            replacement.execution_instance = uuid::Uuid::new_v4();
            app.terminal_close_bindings.bound.insert(card, replacement);
            let _ = app.completed_terminal_closed(card, ended, Ok(outcome));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(!app.closed_terminal_cards.contains(&card));
        }
    }

    #[test]
    fn queued_close_keeps_initial_worker_and_rejects_replacement_before_dispatch() {
        for dequeue_first in [false, true] {
            let (mut app, card, captured) = bound_fixture();
            let mut queued = app.queued_card_close(card, true);
            if dequeue_first {
                app.queued_card_closes.push_back(queued);
                let Message::CloseQueuedCard(target) =
                    super::super::tests::task_app_message(app.next_queued_card_close())
                else {
                    panic!()
                };
                queued = target;
            }
            let mut replacement = captured.clone();
            replacement.execution_instance = uuid::Uuid::new_v4();
            app.terminal_close_bindings.bound.insert(card, replacement);
            assert!(app.resolve_queued_card_close(queued).is_none());
            assert!(app.terminal_close.is_none());
            assert!(app.card_state(card).unwrap().existence.is_open());
        }
    }

    #[test]
    fn queued_close_allows_same_worker_natural_completion_but_not_new_legacy_binding() {
        let (mut app, card, captured) = bound_fixture();
        let queued = app.queued_card_close(card, true);
        let CardKind::Terminal(source) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!()
        };
        source.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        assert!(
            matches!(app.resolve_queued_card_close(queued),Some(Message::CloseCardUnasked(candidate)) if candidate==card)
        );
        app.forget_terminal_close_binding(card);
        let legacy = app.queued_card_close(card, true);
        app.terminal_close_bindings.bound.insert(card, captured);
        assert!(app.resolve_queued_card_close(legacy).is_none());
    }

    #[test]
    fn stale_worker_confirmation_does_not_persist_opt_out() {
        let (mut app, card, captured) = bound_fixture();
        app.settings.confirm_terminal_close = true;
        let _ = app.begin_card_close(card);
        let intent = app.terminal_close.clone().unwrap();
        let _ = app.terminal_close_finished(
            card,
            intent.terminal,
            intent.proof.clone(),
            intent.generation,
            CloseOutcome::Active,
        );
        app.terminal_close.as_mut().unwrap().dont_ask_again = true;
        let mut replacement = captured;
        replacement.execution_instance = uuid::Uuid::new_v4();
        app.terminal_close_bindings.bound.insert(card, replacement);
        let _ = app.confirm_terminal_close();
        assert!(app.settings.confirm_terminal_close);
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.terminal_close.as_ref().unwrap().pending);
    }

    #[test]
    fn explicit_spawn_without_confirmed_metadata_invalidates_old_close_authority() {
        let (mut app, card, captured) = bound_fixture();
        let _ = app.begin_card_close(card);
        let intent = app.terminal_close.as_ref().unwrap().clone();
        app.terminal_close_bindings.removals.insert(card, captured);
        app.cards.insert(card, CardRuntime::Loading);
        app.capture_terminal_close_binding(card, true);
        assert!(!app.terminal_close_bindings.bound.contains_key(&card));
        assert!(!app.terminal_close_binding_current(&intent));
        assert!(
            app.terminal_close_bindings.removals.contains_key(&card),
            "old commit must still fail closed until the replacement is bound"
        );
    }

    #[test]
    fn acknowledged_old_worker_cannot_commit_after_pane_adoption() {
        let (mut app, card, captured) = bound_fixture();
        app.terminal_close_bindings
            .removals
            .insert(card, captured.clone());
        app.closed_terminal_cards.insert(card);
        let _ = app.request_card_removal(card);
        let mut replacement = captured;
        replacement.proof.host_incarnation = HostIncarnation::new("new-daemon");
        app.terminal_close_bindings.bound.insert(card, replacement);
        assert!(app.commit_card_removal(card, None).is_none());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.card(card).is_some());
        assert!(!app.closed_terminal_cards.contains(&card));
        assert!(app.workspace.pending_close().is_none());
    }

    #[test]
    fn absent_completed_worker_does_not_remove_shared_pane_or_history() {
        let (mut app, card, captured) = bound_fixture();
        let CardKind::Terminal(source) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!()
        };
        source.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let ended = source.clone();
        let _ = app.completed_terminal_closed(
            card,
            ended,
            Ok(CompletedCloseOutcome::Dismissed {
                expected_worker: Some((captured.execution_instance, captured.proof)),
            }),
        );
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.card(card).is_some());
        assert!(!app.closed_terminal_cards.contains(&card));
        assert!(app.workspace.pending_close().is_none());
        assert!(app.recently_closed.is_empty());
    }

    #[test]
    fn authenticated_adoption_inventory_only_corroborates_and_does_not_refresh_old_consent() {
        let (mut app, card, captured) = bound_fixture();
        let mut adopted = descriptor_for(&captured);
        adopted.execution_instance = uuid::Uuid::new_v4();
        let proof = HostSessionProof {
            host_incarnation: HostIncarnation::new("adopted"),
            ..captured.proof.clone()
        };
        adopted.session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        };
        adopted.upgrade_sources = vec![captured.source.clone()];
        app.corroborate_terminal_close_adoption(&adopted);
        assert_eq!(
            app.terminal_close_bindings.bound[&card].execution_instance,
            captured.execution_instance
        );
        assert_eq!(
            app.terminal_close_bindings.adoptions[&card].0,
            captured.execution_instance
        );
        assert_eq!(
            app.terminal_close_bindings.adoptions[&card].2,
            adopted.execution_instance
        );
        app.forget_terminal_close_binding(card);
        assert!(app.terminal_close_bindings.bound.is_empty());
        assert!(app.terminal_close_bindings.adoptions.is_empty());
    }

    #[test]
    fn same_worker_new_proof_adoption_advances_new_consent_but_preserves_pending_intent() {
        let (mut app, card, captured) = bound_fixture();
        let _ = app.begin_card_close(card);
        let intent = app.terminal_close.as_ref().unwrap().clone();
        let mut adopted = descriptor_for(&captured);
        let proof = HostSessionProof {
            host_incarnation: HostIncarnation::new("adopted"),
            ..captured.proof.clone()
        };
        adopted.session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        adopted.upgrade_sources = vec![captured.source.clone()];
        app.corroborate_terminal_close_adoption(&adopted);
        app.card_state_mut(card).unwrap().kind = CardKind::Terminal(adopted.session.clone());
        app.record_terminal_close_binding(
            card,
            crate::terminal::HostedTerminalBinding {
                source: adopted.session,
                execution_instance: adopted.execution_instance,
            },
            proof.clone(),
            false,
        );
        assert_eq!(app.terminal_close_bindings.bound[&card].proof, proof);
        assert_eq!(
            app.terminal_close
                .as_ref()
                .unwrap()
                .captured
                .as_ref()
                .unwrap()
                .proof,
            captured.proof
        );
        assert!(
            !app.terminal_close_binding_current(&intent),
            "old consent must not follow a new daemon proof"
        );
    }

    #[test]
    fn completed_close_waits_for_receipt_and_ignores_a_replaced_card() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.card_state_mut(card).unwrap().residency = Residency::Hot;
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
        app.card_state_mut(card).unwrap().residency = Residency::Hot;
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
        let _ = app.update(Message::CompletedTerminalClosed {
            card,
            session: source.clone(),
            result: Ok(CompletedCloseOutcome::Active(proof.clone())),
        });
        assert!(app.terminal_close_dialog().is_some());
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
        app.card_state_mut(card).unwrap().residency = Residency::Hot;
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
        app.card_state_mut(card).unwrap().residency = Residency::Hot;
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

        let login_state = super::super::tests::terminal_card_state();
        let login = login_state.id;
        Arc::make_mut(&mut app.durable).cards.push(login_state);
        let _ = app.begin_card_close(login);
        let _ = app.close_card_unasked(login);

        assert!(
            matches!(
            app.queued_card_closes.front(),
            Some(QueuedCardClose {card,unasked:true,..}) if *card == login),
            "a later explicit cancellation upgrades a queued ordinary close"
        );
        app.terminal_close = None;
        assert!(matches!(
            super::super::tests::task_app_message(app.next_queued_card_close()),
            Message::CloseQueuedCard(queued) if queued.card == login && queued.unasked
        ));
    }
}

fn close_completed_with_client(
    client: &mut ControlClient,
    source: &TerminalSession,
    captured: Option<&CapturedTerminal>,
    allow_active: bool,
) -> Result<CompletedCloseOutcome, String> {
    let Some(descriptor) = client
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|descriptor| descriptor.session.id == source.id)
    else {
        // Presentation only. A successor may appear after List, so do not
        // issue the legacy source-only release or any other teardown RPC.
        return Ok(CompletedCloseOutcome::Dismissed {
            expected_worker: captured
                .map(|captured| (captured.execution_instance, captured.proof.clone())),
        });
    };
    let captured=captured.ok_or("This saved pane has no verified worker identity. Close the original session from a current attached pane or the CLI, then retry. Its current resident was preserved.")?;
    if client.proof() != &captured.proof || !captured_matches_descriptor(captured, &descriptor) {
        return Err("The terminal worker changed; its successor was preserved".into());
    }
    Ok(
        match client
            .close_terminal_source_checked(
                descriptor.session,
                captured.execution_instance,
                allow_active,
            )
            .map_err(|error| error.to_string())?
        {
            TerminalCloseResult::Closed => CompletedCloseOutcome::ClosedWorker {
                execution_instance: captured.execution_instance,
                proof: captured.proof.clone(),
            },
            TerminalCloseResult::Active => CompletedCloseOutcome::ActiveWorker {
                source: captured.source.clone(),
                execution_instance: captured.execution_instance,
                proof: captured.proof.clone(),
            },
            TerminalCloseResult::Unknown(reason) => return Err(reason),
        },
    )
}

fn captured_matches_descriptor(
    captured: &CapturedTerminal,
    descriptor: &crate::terminal::ipc::TerminalDescriptor,
) -> bool {
    captured.proof.is_complete()
        && !captured.execution_instance.is_nil()
        && captured.execution_instance == descriptor.execution_instance
        && captured.source.id == descriptor.session.id
        && captured.source.launch == descriptor.session.launch
        && descriptor
            .session
            .continuity
            .live_proof()
            .is_none_or(|proof| proof == captured.proof)
}

fn revalidate_and_close(intent: &CloseIntent, allow_active: bool) -> CloseOutcome {
    let Some(captured) = &intent.captured else {
        return CloseOutcome::Unreachable("This pane has no verified terminal worker identity. Focus its original attached pane and retry; its sessions were preserved.".into());
    };
    let run = || -> Result<CloseOutcome, String> {
        let path = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
        let mut client = ControlClient::connect(&path).map_err(|error| error.to_string())?;
        if client.proof() != &captured.proof || !captured.proof.is_complete() {
            return Ok(CloseOutcome::Stale);
        }
        let Some(descriptor) = client
            .list()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|descriptor| descriptor.session.id == captured.source.id)
        else {
            return Ok(CloseOutcome::Stale);
        };
        close_captured_with_client(&mut client, captured, descriptor, allow_active)
    };
    run().unwrap_or_else(CloseOutcome::Unreachable)
}

fn close_captured_with_client(
    client: &mut ControlClient,
    captured: &CapturedTerminal,
    descriptor: crate::terminal::ipc::TerminalDescriptor,
    allow_active: bool,
) -> Result<CloseOutcome, String> {
    if !captured_matches_descriptor(captured, &descriptor) {
        return Ok(CloseOutcome::Stale);
    }
    // The current continuity/exit fields can advance within this exact worker.
    // Its UUID and immutable launch remain the captured consent authority.
    Ok(
        match client
            .close_terminal_source_checked(
                descriptor.session,
                captured.execution_instance,
                allow_active,
            )
            .map_err(|error| error.to_string())?
        {
            TerminalCloseResult::Closed => CloseOutcome::Closed,
            TerminalCloseResult::Active => CloseOutcome::Active,
            TerminalCloseResult::Unknown(reason) => CloseOutcome::Unreachable(reason),
        },
    )
}

/// A one-shot operation for callers with their own scoped consent. Generic
/// pane confirmation uses its pre-captured binding through revalidate_and_close.
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
        let Some(descriptor) = client
            .list()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|descriptor| descriptor.session.id == terminal)
        else {
            return Ok(CloseOutcome::Stale);
        };
        if descriptor.execution_instance.is_nil() {
            return Ok(CloseOutcome::Unreachable(
                "The terminal has no verified worker identity; it was preserved".into(),
            ));
        }
        let captured = CapturedTerminal {
            source: descriptor.session.clone(),
            execution_instance: descriptor.execution_instance,
            proof: proof.clone(),
        };
        close_captured_with_client(&mut client, &captured, descriptor, allow_active)
    };
    run().unwrap_or_else(CloseOutcome::Unreachable)
}
