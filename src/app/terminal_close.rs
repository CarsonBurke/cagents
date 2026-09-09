use super::*;
use crate::control::ControlClient;

#[derive(Debug, Clone)]
pub(super) struct CloseIntent {
    pub card: CardId,
    pub terminal: TerminalId,
    pub proof: HostSessionProof,
    pub restore_card: Option<CardId>,
    pub pending: bool,
    pub detail: Option<String>,
    confirm_focused: bool,
    restore_keyboard_nav: bool,
}

#[derive(Debug, Clone)]
pub enum CloseOutcome {
    Closed,
    Stale,
    Unreachable(String),
}

fn target_matches(intent: &CloseIntent, state: Option<&CardState>) -> bool {
    matches!(state, Some(CardState { kind: CardKind::Terminal(session), .. })
        if session.id == intent.terminal && session.continuity.live_proof().as_ref() == Some(&intent.proof))
}

impl Cagents {
    pub(super) fn begin_card_close(&mut self, card: CardId) -> Task<Message> {
        if self.terminal_close.is_some() {
            return Task::none();
        }
        if self.pending_terminal_spawn(card) {
            self.status = Some(TransientNotice::warning(
                "Terminal is still starting. The card is kept until the daemon supplies its identity; then use Terminate terminal to close it.",
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
                card, terminal: session.id, proof,
                restore_card: self.workspace.focused(), pending: false,
                detail: self.uncertain_terminal_proofs.contains_key(&card).then(|| "Daemon unreachable; the card is retained. Reconnect before requesting termination.".into()),
                confirm_focused: false,
                restore_keyboard_nav: self.core.keyboard_nav(),
            });
            self.core.set_keyboard_nav(false);
            return iced::widget::operation::focus(widget::Id::new("dialog.terminal-close.cancel"))
                .map(cosmic::Action::App);
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
        self.core.set_keyboard_nav(intent.restore_keyboard_nav);
        let card = resolve_restore_card(intent.restore_card, Some(intent.card), |card| {
            self.workspace.order().any(|id| id == card)
        });
        if let Some(card) = card {
            let _ = self.workspace.focus(card);
        }
        iced::widget::operation::focus(
            card.map(workspace_widget::card_focus_id)
                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID)),
        )
        .map(cosmic::Action::App)
    }

    pub(super) fn confirm_terminal_close(&mut self) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if intent.pending || intent.detail.is_some() {
            return Task::none();
        }
        let intent = intent.clone();
        if !target_matches(&intent, self.card_state(intent.card)) {
            // An exit while the dialog was open permits removing only that same terminal.
            if matches!(self.card_state(intent.card), Some(CardState { kind: CardKind::Terminal(session), .. })
                if session.id == intent.terminal && matches!(session.continuity, TerminalContinuity::Exited { .. } | TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }))
            {
                self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                self.terminal_close = None;
                self.closed_terminal_cards.insert(intent.card);
                return self.request_card_removal(intent.card);
            }
            self.terminal_close.as_mut().unwrap().detail = Some(
                "Stale target: terminal identity or daemon proof changed. Nothing was terminated."
                    .into(),
            );
            return Task::none();
        }
        if !self.terminal_close_attempts.insert(intent.terminal) {
            self.terminal_close.as_mut().unwrap().detail = Some("A termination request was already attempted for this terminal. The card is retained until its outcome is known.".into());
            return Task::none();
        }
        self.terminal_close.as_mut().unwrap().pending = true;
        cosmic::task::future(async move {
            let card = intent.card;
            let terminal = intent.terminal;
            let proof = intent.proof.clone();
            let outcome = tokio::task::spawn_blocking(move || revalidate_and_close(&intent))
                .await
                .unwrap_or_else(|error| {
                    CloseOutcome::Unreachable(format!("Close worker failed: {error}"))
                });
            Message::TerminalCloseFinished {
                card,
                terminal,
                proof,
                outcome,
            }
        })
    }

    pub(super) fn terminal_close_finished(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        proof: HostSessionProof,
        outcome: CloseOutcome,
    ) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_ref() else {
            return Task::none();
        };
        if !intent.pending
            || intent.card != card
            || intent.terminal != terminal
            || intent.proof != proof
        {
            return Task::none();
        }
        let identity_matches = matches!(self.card_state(card), Some(CardState { kind: CardKind::Terminal(session), .. })
            if session.id == terminal && (session.continuity.live_proof().as_ref() == Some(&proof)
                || matches!(session.continuity, TerminalContinuity::Exited { .. } | TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. })));
        match outcome {
            CloseOutcome::Closed if identity_matches => {
                self.core.set_keyboard_nav(intent.restore_keyboard_nav);
                self.terminal_close = None;
                self.closed_terminal_cards.insert(card);
                self.request_card_removal(card)
            }
            outcome => {
                let intent = self.terminal_close.as_mut().unwrap();
                intent.pending = false;
                intent.detail = Some(match outcome {
                    CloseOutcome::Closed | CloseOutcome::Stale => "Stale target: terminal identity or daemon proof changed. The card was kept.".into(),
                    CloseOutcome::Unreachable(error) => format!("Daemon unreachable or close unconfirmed: {error}. The card was kept; no automatic retry will be sent."),
                });
                Task::none()
            }
        }
    }

    pub(super) fn cycle_terminal_close_focus(&mut self) -> Task<Message> {
        let Some(intent) = self.terminal_close.as_mut() else {
            return Task::none();
        };
        intent.confirm_focused =
            !intent.confirm_focused && !intent.pending && intent.detail.is_none();
        iced::widget::operation::focus(widget::Id::new(if intent.confirm_focused {
            "dialog.terminal-close.confirm"
        } else {
            "dialog.terminal-close.cancel"
        }))
        .map(cosmic::Action::App)
    }

    pub(super) fn terminal_close_dialog(&self) -> Option<Element<'_, Message>> {
        let intent = self.terminal_close.as_ref()?;
        Some(widget::container(widget::column::with_children(vec![
            widget::text::title3("Terminate terminal?").into(),
            widget::text::body("This closes the terminal and terminates its running process. Cancel keeps the terminal card.").into(),
            widget::text::body(intent.detail.as_deref().unwrap_or(if intent.pending { "Revalidating the daemon and requesting termination…" } else { "The terminal identity and daemon proof will be checked again before termination." })).into(),
            widget::row::with_children(vec![
                widget::button::standard(if intent.detail.is_some() { "Keep card" } else { "Cancel" })
                    .id(widget::Id::new("dialog.terminal-close.cancel"))
                    .on_press_maybe((!intent.pending).then_some(Message::CancelTerminalClose)).into(),
                widget::button::destructive("Terminate terminal")
                    .id(widget::Id::new("dialog.terminal-close.confirm"))
                    .on_press_maybe((!intent.pending && intent.detail.is_none()).then_some(Message::ConfirmTerminalClose)).into(),
            ]).spacing(8).into(),
        ]).spacing(12)).class(cosmic::theme::Container::Dialog(false)).padding(24).max_width(560).into())
    }
}

fn revalidate_and_close(intent: &CloseIntent) -> CloseOutcome {
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
        client
            .close_terminal(intent.terminal)
            .map_err(|error| error.to_string())?;
        Ok(CloseOutcome::Closed)
    };
    run().unwrap_or_else(CloseOutcome::Unreachable)
}
