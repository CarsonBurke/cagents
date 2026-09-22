use super::*;
use crate::{control::ControlClient, terminal::ipc::TerminalCloseResult};

#[derive(Debug, Clone)]
pub(super) struct CloseIntent {
    pub card: CardId,
    pub terminal: TerminalId,
    pub proof: HostSessionProof,
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

fn target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal && session.continuity.live_proof().as_ref() == Some(&intent.proof))
}

fn exited_target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal && matches!(session.continuity,
            TerminalContinuity::Exited { .. } | TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }))
}

impl Cagents {
    pub(super) fn begin_card_close(&mut self, card: CardId) -> Task<Message> {
        if self.terminal_close.is_some() {
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
            self.core.set_keyboard_nav(false);
            if self.terminal_close.as_ref().unwrap().detail.is_some() {
                return close_focus(0);
            }
            return self.start_terminal_close(!self.settings.confirm_terminal_close);
        }
        self.request_card_removal(card)
    }

    pub(super) fn pending_terminal_spawn(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Loading))
            && terminal_continuity(self.card_state(card))
                .is_some_and(|continuity| continuity.live_proof().is_none())
    }

    pub(super) fn request_card_removal(&mut self, card: CardId) -> Task<Message> {
        match self.workspace.request_close(card) {
            Ok(handoff) => workspace_widget::focus_handoff_then(
                handoff,
                widget::Id::new(TOOLBAR_FOCUS_ID),
                |next| self.card_keyboard_focus(next),
                Message::CommitClose(card),
            )
            .map(cosmic::Action::App),
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                Task::none()
            }
        }
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
        if !target_matches(intent, self.card_state(intent.card)) {
            if exited_target_matches(intent, self.card_state(intent.card)) {
                let intent = self.terminal_close.take().unwrap();
                self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                self.closed_terminal_cards.insert(intent.card);
                return self.request_card_removal(intent.card);
            }
            self.terminal_close.as_mut().unwrap().detail =
                Some("Terminal identity or daemon proof changed. Nothing was closed.".into());
            return close_focus(0);
        }
        if !self.terminal_close_attempts.insert(intent.terminal) {
            self.terminal_close.as_mut().unwrap().detail = Some(
                "A close was already attempted. The card is retained until its outcome is known."
                    .into(),
            );
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
                self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                self.terminal_close = None;
                self.closed_terminal_cards.insert(card);
                self.request_card_removal(card)
            }
            CloseOutcome::Active if identity_matches => {
                // The owner positively declined to close: a later explicit Close
                // may issue a fresh, independently fenced request.
                self.terminal_close_attempts.remove(&terminal);
                let intent = self.terminal_close.as_mut().unwrap();
                intent.pending = false;
                intent.active = true;
                intent.focus = 1;
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
        if !intent.active && intent.detail.is_none() {
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
                "This terminal has a running program or background jobs. Closing stops them."
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

fn revalidate_and_close(intent: &CloseIntent, allow_active: bool) -> CloseOutcome {
    let run = || -> Result<CloseOutcome, String> {
        let path = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
        let mut client = ControlClient::connect(&path).map_err(|error| error.to_string())?;
        if client.proof() != &intent.proof || !intent.proof.is_complete() {
            return Ok(CloseOutcome::Stale);
        }
        let terminals = client.list().map_err(|error| error.to_string())?;
        let Some(descriptor) = terminals
            .iter()
            .find(|descriptor| descriptor.session.id == intent.terminal)
        else {
            return Ok(CloseOutcome::Stale);
        };
        match &descriptor.session.continuity {
            TerminalContinuity::Exited { .. }
            | TerminalContinuity::RelaunchAvailable {
                last_exit: Some(_), ..
            } => return Ok(CloseOutcome::Closed),
            continuity if continuity.live_proof().as_ref() == Some(&intent.proof) => {}
            _ => return Ok(CloseOutcome::Stale),
        }
        Ok(
            match client
                .close_terminal_checked(intent.terminal, allow_active)
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
