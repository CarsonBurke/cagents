use super::*;
use crate::{control::ControlClient, terminal::ipc::TerminalCloseResult};

#[derive(Debug, Clone)]
pub(super) struct CloseIntent {
    pub card: CardId,
    pub terminal: TerminalId,
    pub proof: HostSessionProof,
    completed_source: Option<TerminalSession>,
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
    Active(HostSessionProof),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueuedCardClose {
    card: CardId,
    unasked: bool,
}

fn target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    intent.completed_source.is_none()
        && matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal && session.continuity.live_proof().as_ref() == Some(&intent.proof))
}

fn exited_target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal
            && intent.completed_source.as_ref().is_none_or(|source| source == session)
            && matches!(session.continuity,
            TerminalContinuity::Exited { .. } | TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }))
}

impl Cagents {
    pub(super) fn next_queued_card_close(&mut self) -> Task<Message> {
        while let Some(QueuedCardClose { card, unasked }) = self.queued_card_closes.pop_front() {
            if self
                .card_state(card)
                .is_some_and(|state| state.existence.is_open())
            {
                return cosmic::task::message(if unasked {
                    Message::CloseCardUnasked(card)
                } else {
                    Message::CloseCard(card)
                });
            }
        }
        Task::none()
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
                    self.queued_card_closes
                        .push_back(QueuedCardClose { card, unasked });
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
        match result {
            Ok(CompletedCloseOutcome::Closed) => {
                self.closed_terminal_cards.insert(card);
                self.request_card_removal(card)
            }
            Ok(CompletedCloseOutcome::Active(proof)) => {
                if self.terminal_close.is_some() {
                    self.queued_card_closes.push_back(QueuedCardClose {
                        card,
                        unasked: false,
                    });
                    return Task::none();
                }
                self.terminal_close = Some(CloseIntent {
                    card,
                    terminal: session.id,
                    proof,
                    completed_source: Some(session),
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
        replacement: Option<(LaunchRecipe, (usize, usize))>,
    ) -> Option<Task<Message>> {
        let handoff = self.workspace.pending_close();
        let replacing = replacement.is_some();
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
            .map(|(launch, slot)| self.open_chat_replacement(launch, slot))
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
        if !target_matches(intent, self.card_state(intent.card))
            && !exited_target_matches(intent, self.card_state(intent.card))
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
        let identity_matches = target_matches(intent, self.card_state(card))
            || exited_target_matches(intent, self.card_state(card));
        match outcome {
            CloseOutcome::Closed if identity_matches => {
                if intent.active || intent.detail.is_some() {
                    self.core.set_keyboard_nav(intent.restore_keyboard_nav);
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
            widget::text::title3(if intent.detail.is_some() {
                "Terminal kept open"
            } else {
                "Close active processes?"
            })
            .into(),
            widget::text::body(intent.detail.as_deref().unwrap_or(if intent.pending {
                "Checking terminal identity and closing…"
            } else {
                "This terminal or its paired conversation still owns running processes. Closing stops them."
            }))
            .into(),
        ];
        if intent.active && intent.detail.is_none() {
            rows.push(
                widget::button::custom(
                    widget::checkbox(intent.dont_ask_again)
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
                widget::button::suggested("Dismiss")
                    .id(widget::Id::new("dialog.terminal-close.cancel"))
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose))
                    .into(),
            ]
        } else {
            vec![
                widget::button::standard("Cancel")
                    .id(widget::Id::new("dialog.terminal-close.cancel"))
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose))
                    .into(),
                widget::button::destructive("Close")
                    .id(widget::Id::new("dialog.terminal-close.confirm"))
                    .on_press_maybe(
                        (!intent.pending && intent.active).then_some(Message::ConfirmTerminalClose),
                    )
                    .into(),
            ]
        };
        rows.push(widget::row::with_children(buttons).spacing(8).into());
        Some(super::dialog_ui::surface(
            widget::column::with_children(rows).spacing(12),
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

        assert_eq!(
            app.queued_card_closes.front().copied(),
            Some(QueuedCardClose {
                card: login,
                unasked: true,
            }),
            "a later explicit cancellation upgrades a queued ordinary close"
        );
        app.terminal_close = None;
        assert!(matches!(
            super::super::tests::task_app_message(app.next_queued_card_close()),
            Message::CloseCardUnasked(card) if card == login
        ));
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
    close_hosted_terminal(intent.terminal, &intent.proof, allow_active)
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
