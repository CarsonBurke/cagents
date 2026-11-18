//! Recovery from a permanent refusal, without retrying the refused source.
use super::*;

pub(in crate::app) const HEADLINE: &str = "This conversation can't be reopened safely.";
pub(in crate::app) const RESTART_HEADLINE: &str =
    "This conversation can't be reopened until you restart this computer.";
pub(super) const DRAFT_HEADLINE: &str = "This conversation couldn't start.";
pub(super) const DRAFT_GUIDANCE: &str = "Your message is kept. Copy it into a new conversation, or see Details for why this one couldn't start.";
pub(super) const FRESH_EXPLANATION: &str = "Starts a new conversation in this tab, keeping any unsent message; closing the tab discards it. The old conversation isn't deleted, but this tab stops pointing at it.";
pub(super) const RESTART_GUIDANCE: &str = "Leave this tab open and restart this computer when convenient. The next time cagents starts, it reopens the conversation here, with any unsent message.";
pub(super) const RESTART_FRESH_TRADEOFF: &str = "Starting a new conversation keeps the message; closing the tab discards it. Either way, the old conversation won't reopen here, though it isn't deleted.";
pub(super) const RESTART_CLOSE_TRADEOFF: &str = "Closing the tab discards the message, and the conversation won't reopen, though it isn't deleted.";
pub(super) const UNNAMED_AGENT_HEADLINE: &str = "This agent's conversation can't be reopened";
pub(super) const UNNAMED_AGENT_BODY: &str = "cagents lost track of this conversation when its background service stopped. Nothing new was started.";
/// [`UNNAMED_AGENT_BODY`] once the daemon has found the agent's processes
/// still running; nothing else may claim it runs.
pub(super) const UNNAMED_AGENT_RUNNING_BODY: &str = "cagents lost track of this conversation when its background service stopped. The agent is still running. Nothing new was started.";
pub(super) const UNNAMED_AGENT_CLOSE: &str = "Close";
pub(super) const UNNAMED_AGENT_RUNNING_CLOSE: &str = "Close and end agent";
pub(super) const UNNAMED_AGENT_STILL_RUNS: &str =
    "The agent is still running, so this tab stays open. Try again.";
pub(super) const UNNAMED_AGENT_TERMINAL: &str =
    "This agent's conversation can't be reopened, so there is nothing to open in a terminal.";
pub(super) const UNNAMED_AGENT_UNSENT: &str = "Closing also discards your unsent message:";
pub(super) const UNNAMED_AGENT_UNSENT_ATTACHMENTS: &str =
    "Closing also discards your unsent message and its attachments.";
pub(super) const UNSENDABLE_UNTIL_RESTART: Unsendable = Unsendable {
    placeholder: "Messages can be sent once this conversation reopens",
    reason: "available once this conversation reopens",
};
pub(super) const UNSENDABLE_UNTIL_REOPENED: Unsendable = Unsendable {
    placeholder: "Reopen this conversation to send a message",
    reason: "reopen this conversation first",
};
pub(super) const UNSENDABLE_UNTIL_FRESH: Unsendable = Unsendable {
    placeholder: "Start a new conversation to send a message",
    reason: "start a new conversation first",
};
pub(super) const UNSENDABLE: Unsendable = Unsendable {
    placeholder: "This conversation can't take new messages",
    reason: "this conversation can't take new messages",
};

/// Why a refused conversation's composer sends nothing: in its empty field,
/// and after the name of its disabled Send control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Unsendable {
    pub(super) placeholder: &'static str,
    pub(super) reason: &'static str,
}
pub(super) const CLOSE_GUIDANCE: &str = "Details say why. Closing this tab discards any unsent message; the conversation itself isn't deleted.";
pub(in crate::app) const UPDATE_HEADLINE: &str =
    "This conversation hasn't reopened since the update.";
pub(super) const UPDATE_DRAFT_HEADLINE: &str = "This conversation didn't start during the update.";
pub(in crate::app) const STOPPED_HEADLINE: &str = "This conversation stopped after the update.";
pub(super) const STOPPED_GUIDANCE: &str = "It couldn't restart after the update, so cagents stopped it. Nothing was resent. Reopen conversation starts it again in this tab; Details say why it stopped.";
pub(in crate::app) const REOPEN_LABEL: &str = "Reopen conversation";

/// When a permanently refused saved conversation can come back, as far as
/// the daemon's typed refusal says. Nothing here promises Resume conversation
/// in the new tab (+) menu: it asks the daemon to prepare the same saved
/// conversation, or starts the same saved launch, so it meets the same
/// refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Reopen {
    /// No later step is known to reopen it; the reason is under Details.
    Unproven,
    /// A new boot proves the conversation's earlier process gone, and this
    /// card's own restore then prepares it again.
    AfterRestart,
    /// An open agent's conversation whose provider never named it: nothing
    /// can reopen it, and starting over here would give up the agent's card,
    /// so Close, which ends the agent, is the only way on.
    UnnamedAgent,
    /// The daemon is still restoring what an update carried over, or holds
    /// that restore for recovery. Nothing is wrong with this conversation:
    /// the card reopens it, or lets its message be sent, once the daemon says
    /// its startup recovery has finished (see `update_recovery`).
    UpdateRecovery,
    /// The update's restore stopped this conversation after its restart
    /// failed. Restoring it again only attaches, which meets the same
    /// refusal; the reader's Reopen conversation starts it again.
    UpdateStopped,
}

impl Reopen {
    /// The permanent refusals; `None` for every recovery that has an action.
    pub(in crate::app) fn of(recovery: chat::Recovery) -> Option<Self> {
        match recovery {
            chat::Recovery::Blocked => Some(Self::Unproven),
            chat::Recovery::AfterRestart => Some(Self::AfterRestart),
            chat::Recovery::UpdateRecovery => Some(Self::UpdateRecovery),
            chat::Recovery::UpdateStopped => Some(Self::UpdateStopped),
            chat::Recovery::Account
            | chat::Recovery::Retry
            | chat::Recovery::Busy
            | chat::Recovery::Unresponsive
            | chat::Recovery::Unreached
            | chat::Recovery::NotAdmitted
            | chat::Recovery::Capacity
            | chat::Recovery::Fresh => None,
        }
    }

    pub(in crate::app) fn headline(self) -> &'static str {
        match self {
            Self::Unproven => HEADLINE,
            Self::AfterRestart => RESTART_HEADLINE,
            Self::UnnamedAgent => UNNAMED_AGENT_HEADLINE,
            Self::UpdateRecovery => UPDATE_HEADLINE,
            Self::UpdateStopped => STOPPED_HEADLINE,
        }
    }

    /// The headline over a first message this refusal kept.
    pub(super) fn draft_headline(self) -> &'static str {
        match self {
            Self::Unproven | Self::AfterRestart | Self::UpdateStopped => DRAFT_HEADLINE,
            Self::UpdateRecovery => UPDATE_DRAFT_HEADLINE,
            Self::UnnamedAgent => UNNAMED_AGENT_HEADLINE,
        }
    }

    /// The caption under a refusal: for a saved conversation, with the
    /// explanation of Start a new conversation when this card offers it,
    /// and for a first message (`draft`), what became of it. `wait` is what
    /// the daemon last said about its update restore.
    pub(super) fn guidance(self, fresh: bool, draft: bool, wait: UpdateWait) -> String {
        match (self, fresh) {
            (Self::UpdateRecovery, _) => wait.guidance(draft).to_owned(),
            (Self::UnnamedAgent, _) => UNNAMED_AGENT_BODY.to_owned(),
            _ if draft => DRAFT_GUIDANCE.to_owned(),
            (Self::UpdateStopped, _) => STOPPED_GUIDANCE.to_owned(),
            (Self::Unproven, true) => FRESH_EXPLANATION.to_owned(),
            (Self::Unproven, false) => CLOSE_GUIDANCE.to_owned(),
            (Self::AfterRestart, true) => [RESTART_GUIDANCE, RESTART_FRESH_TRADEOFF].join(" "),
            (Self::AfterRestart, false) => [RESTART_GUIDANCE, RESTART_CLOSE_TRADEOFF].join(" "),
        }
    }

    /// Why the composer of a conversation refused this way sends nothing.
    /// `fresh`: the card offers Start a new conversation, which takes the
    /// draft along.
    pub(super) fn unsendable(self, fresh: bool) -> Unsendable {
        match (self, fresh) {
            // Both come back by themselves: after a restart, or once the
            // update's restore finishes.
            (Self::AfterRestart | Self::UpdateRecovery, _) => UNSENDABLE_UNTIL_RESTART,
            (Self::UpdateStopped, _) => UNSENDABLE_UNTIL_REOPENED,
            (Self::Unproven, true) => UNSENDABLE_UNTIL_FRESH,
            // Its pane shows no message box, only why it can't reopen and
            // Close; this names the case should a draft ever be drawn.
            (Self::Unproven, false) | (Self::UnnamedAgent, _) => UNSENDABLE,
        }
    }

    /// Whether doing nothing is the way back. No button is then emphasized
    /// as the next step, since each of them gives that way up.
    pub(in crate::app) fn waits(self) -> bool {
        matches!(self, Self::AfterRestart | Self::UpdateRecovery)
    }

    /// Whether this card may give up its conversation for a new one. A
    /// conversation waiting on the update comes back by itself; a new one
    /// in its place would only lose this tab's way back to it.
    pub(super) fn offers_fresh(self) -> bool {
        !matches!(self, Self::UpdateRecovery | Self::UpdateStopped)
    }

    /// Whether the reader's Reopen conversation is the way back: a start of
    /// the saved conversation, which the refused restore would not make.
    pub(in crate::app) fn reopens(self) -> bool {
        self == Self::UpdateStopped
    }
}

/// A start refused before a chat runtime exists, kept with the exact source
/// it refused so a replacement never inherits its diagnostics.
#[derive(Clone, Debug)]
pub(in crate::app) struct BlockedStart {
    pub(in crate::app) source: chat::Session,
    pub(in crate::app) reopen: Reopen,
    /// For [`Reopen::UnnamedAgent`], whether its agent is known to run.
    pub(in crate::app) liveness: Liveness,
}

/// What the daemon last proved still runs for an unreachable agent's
/// conversation. Nothing is known until it answers, and then only what it
/// proved, so the card never claims an agent runs on the strength of its
/// open record alone.
#[derive(Clone, Debug, Default)]
pub(in crate::app) struct Liveness {
    /// The processes proved to be the conversation's, which "Close and end
    /// agent" ends: exactly those the card claimed, never one found since.
    pub(in crate::app) running: Vec<crate::terminal::ipc::ProcessStart>,
    /// Ending them is under way.
    pub(in crate::app) ending: bool,
    /// Why ending them failed, until the next attempt.
    pub(in crate::app) end_error: Option<String>,
}

/// What an unreachable agent's card says, and what its Close is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct UnnamedAgentCopy {
    pub(in crate::app) body: &'static str,
    pub(in crate::app) close: &'static str,
    pub(in crate::app) close_name: &'static str,
}

impl UnnamedAgentCopy {
    fn of(running: bool) -> Self {
        if running {
            Self {
                body: UNNAMED_AGENT_RUNNING_BODY,
                close: UNNAMED_AGENT_RUNNING_CLOSE,
                close_name: "Close this tab and end its agent",
            }
        } else {
            Self {
                body: UNNAMED_AGENT_BODY,
                close: UNNAMED_AGENT_CLOSE,
                close_name: "Close this tab",
            }
        }
    }
}

impl Cagents {
    pub(in crate::app) fn blocked_draft_discard(&self, card: CardId) -> Option<TerminalId> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        // Every blocked surface says the draft is kept, so closing the card
        // asks before discarding it. A close that discards it keeps nothing
        // for a reopened card (see `kept_draft`).
        (runtime.start_blocked.is_some()
            && (!runtime.composer.text().trim().is_empty() || !runtime.attachments.is_empty()))
        .then_some(runtime.session.id)
    }

    /// Failed-card diagnostics are authorized by the same exact source as the
    /// refusal. A stale Details or Copy press cannot expose a replacement.
    pub(in crate::app) fn blocked_failure(&self, card: CardId) -> Option<&BlockedStart> {
        self.chat_ui.blocked_starts.get(&card).filter(|blocked| {
            matches!(self.cards.get(&card), Some(CardRuntime::Failed(_)))
                && self.card_state(card).is_some_and(|state| {
                    matches!(&state.kind, CardKind::Chat(current) if *current == blocked.source)
                })
        })
    }

    pub(in crate::app) fn blocked_failure_source(&self, card: CardId) -> Option<&chat::Session> {
        self.blocked_failure(card).map(|blocked| &blocked.source)
    }

    pub(in crate::app) fn blocked_failure_diagnostics(&self, card: CardId) -> Element<'_, Message> {
        let Some(source) = self.blocked_failure_source(card) else {
            return iced::widget::Space::new().into();
        };
        let open = self.chat_ui.blocked_details.get(&card) == Some(source);
        let mut rows = vec![
            widget::row::with_children(vec![
                crate::ui_controls::button::quiet(if open { "Hide details" } else { "Details" })
                    .on_press(action(Action::ToggleBlockedDetails(card, source.clone())))
                    .into(),
                crate::ui_controls::button::quiet("Copy details")
                    .on_press(action(Action::CopyBlockedDetails(card, source.clone())))
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        ];
        if open && let Some(CardRuntime::Failed(failure)) = self.cards.get(&card) {
            rows.push(
                widget::container(widget::scrollable(widget::selectable_text(
                    failure.message(),
                )))
                .max_height(240)
                .into(),
            );
        }
        widget::column::with_children(rows).spacing(space::S).into()
    }

    pub(in crate::app) fn blocked_failure_guidance(&self, card: CardId) -> Option<String> {
        self.blocked_failure(card).map(|blocked| {
            blocked.reopen.guidance(
                self.chat_offers_fresh_conversation(card),
                false,
                self.chat_ui.update_recovery.wait,
            )
        })
    }

    /// The open agent's conversation this card can't reach again
    /// ([`Reopen::UnnamedAgent`]), while the card still names it, whether the
    /// card kept its chat runtime or only the failure.
    pub(in crate::app) fn unnamed_agent(&self, card: CardId) -> Option<&BlockedStart> {
        let blocked = self
            .chat_ui
            .blocked_starts
            .get(&card)
            .filter(|blocked| blocked.reopen == Reopen::UnnamedAgent)?;
        let current = matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Chat(current)) if *current == blocked.source
        );
        let shown = match self.cards.get(&card) {
            Some(CardRuntime::Failed(_)) => true,
            Some(CardRuntime::Chat(runtime)) => runtime.start_blocked == Some(Reopen::UnnamedAgent),
            _ => false,
        };
        (current && shown).then_some(blocked)
    }

    /// The refusal whose Details a press may show or copy: a press for a
    /// conversation the card no longer names reveals nothing.
    pub(in crate::app) fn blocked_details_source(&self, card: CardId) -> Option<&chat::Session> {
        self.blocked_failure(card)
            .or_else(|| self.unnamed_agent(card))
            .map(|blocked| &blocked.source)
    }

    pub(in crate::app) fn blocked_details_text(&self, card: CardId) -> Option<&str> {
        self.blocked_details_source(card)?;
        match self.cards.get(&card)? {
            CardRuntime::Failed(failure) => Some(failure.message()),
            CardRuntime::Chat(runtime) => runtime.error.as_deref(),
            _ => None,
        }
    }

    /// What the unreachable agent's card says, which claims the agent still
    /// runs only once the daemon has proved it.
    pub(in crate::app) fn unnamed_agent_copy(&self, card: CardId) -> Option<UnnamedAgentCopy> {
        Some(UnnamedAgentCopy::of(
            !self.unnamed_agent(card)?.liveness.running.is_empty(),
        ))
    }

    /// Asks the daemon whether the unreachable agent's processes still run,
    /// which only it can see, so the card can say so (see
    /// [`BlockedStart::running`]). Asked when the start is refused and again
    /// each time the card is shown.
    pub(in crate::app) fn ask_unnamed_agent_running(
        &self,
        card: CardId,
        session: chat::Session,
    ) -> Task<Message> {
        cosmic::task::future(async move {
            let asked = session.clone();
            let census = tokio::task::spawn_blocking(move || {
                super::close_refusal::lost_processes(asked, Vec::new())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            let running = match census {
                Ok(processes) => proved(&processes),
                // Unknown, so the card claims nothing.
                Err(error) => {
                    tracing::debug!(%error, "could not learn whether an unreachable agent still runs");
                    Vec::new()
                }
            };
            Message::ChatUi(Action::UnnamedAgentRunning(card, session, running))
        })
    }

    /// Records the daemon's answer for the conversation it was asked about;
    /// one the card no longer names changes nothing, nor does one that
    /// arrives while its processes are being ended.
    pub(super) fn record_unnamed_agent_running(
        &mut self,
        card: CardId,
        session: &chat::Session,
        running: Vec<crate::terminal::ipc::ProcessStart>,
    ) {
        if let Some(liveness) = self.unnamed_agent_liveness(card, session)
            && !liveness.ending
        {
            liveness.running = running;
        }
    }

    fn unnamed_agent_liveness(
        &mut self,
        card: CardId,
        session: &chat::Session,
    ) -> Option<&mut Liveness> {
        self.chat_ui
            .blocked_starts
            .get_mut(&card)
            .filter(|blocked| blocked.reopen == Reopen::UnnamedAgent && blocked.source == *session)
            .map(|blocked| &mut blocked.liveness)
    }

    /// "Close and end agent", once any question its close asks is answered:
    /// ends the processes the card said still run, then closes it, so the
    /// label is true. `None` when it claimed none, and the close goes on.
    pub(super) fn end_unnamed_agent_before_close(&mut self, card: CardId) -> Option<Task<Message>> {
        let session = self.unnamed_agent(card)?.source.clone();
        let liveness = self.unnamed_agent_liveness(card, &session)?;
        if liveness.ending {
            return Some(Task::none());
        }
        if liveness.running.is_empty() {
            return None;
        }
        liveness.ending = true;
        liveness.end_error = None;
        let chosen = liveness.running.clone();
        Some(cosmic::task::future(async move {
            let asked = session.clone();
            let result = tokio::task::spawn_blocking(move || {
                super::close_refusal::lost_processes(asked, chosen)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ChatUi(Action::UnnamedAgentEnded(card, session, result))
        }))
    }

    /// Closes the card once what it ended is gone; otherwise says why and
    /// what still runs, for another try.
    pub(super) fn unnamed_agent_ended(
        &mut self,
        card: CardId,
        session: &chat::Session,
        result: Result<Vec<crate::terminal::ipc::LostProcess>, String>,
    ) -> Task<Message> {
        if self
            .unnamed_agent(card)
            .is_none_or(|blocked| blocked.source != *session)
        {
            return Task::none();
        }
        let Some(liveness) = self.unnamed_agent_liveness(card, session) else {
            return Task::none();
        };
        liveness.ending = false;
        match result {
            Ok(remaining) => {
                liveness.running = proved(&remaining);
                if liveness.running.is_empty() {
                    // Its questions were answered before the agent was ended.
                    return self.begin_chat_close(card);
                }
                liveness.end_error = Some(UNNAMED_AGENT_STILL_RUNS.to_owned());
            }
            Err(error) => liveness.end_error = Some(error),
        }
        // The card stays, so nothing holds its removal for another try.
        self.abandon_removal(card);
        Task::none()
    }

    /// An agent's card whose conversation can't be reached again is done:
    /// one centred state says so and offers only the Close that ends the
    /// agent. No composer stands under it, since nothing typed could be sent.
    pub(in crate::app) fn unnamed_agent_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let source = &self.unnamed_agent(card)?.source;
        let copy = self.unnamed_agent_copy(card)?;
        let liveness = &self.unnamed_agent(card)?.liveness;
        let closing =
            self.terminal_close_in_progress(card) || self.chat_close_owned(card) || liveness.ending;
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::row::with_children(vec![
                widget::icon(Icon::Warning.handle())
                    .size(design::STATUS_ICON_SIZE)
                    .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                        iced::widget::svg::Style {
                            color: Some(design::warning_color(theme)),
                        }
                    })))
                    .into(),
                design::title(UNNAMED_AGENT_HEADLINE).into(),
            ])
            .align_y(Alignment::Center)
            .spacing(space::S)
            .into(),
            widget::selectable_text(copy.body)
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ];
        // Closing discards what the card kept unsent, so it is shown, and can
        // be copied, before the button that discards it.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
            let text = runtime.unsent_text();
            if !text.trim().is_empty() {
                rows.push(
                    widget::column::with_children(vec![
                        design::caption(UNNAMED_AGENT_UNSENT).into(),
                        widget::container(widget::scrollable(
                            widget::selectable_text(text.clone())
                                .wrapping(iced::widget::text::Wrapping::Word)
                                .width(Length::Fill),
                        ))
                        .class(design::surface(
                            Some(design::sunken),
                            [design::RADIUS_M; 4],
                            false,
                        ))
                        .padding([space::S, space::M])
                        .max_height(160)
                        .width(Length::Fill)
                        .into(),
                        crate::ui_controls::button::quiet("Copy message")
                            .on_press(action(Action::CopyBlockedDraft(card, source.clone())))
                            .into(),
                    ])
                    .spacing(space::S)
                    .into(),
                );
            } else if !runtime.attachments.is_empty() {
                rows.push(design::caption(UNNAMED_AGENT_UNSENT_ATTACHMENTS).into());
            }
        }
        rows.push(
            crate::ui_controls::button::primary(copy.close)
                .instead(
                    if liveness.ending {
                        "Ending agent…"
                    } else {
                        "Checking…"
                    },
                    closing,
                )
                .id(crate::app::card_control_id(card, "failed.close"))
                .name(copy.close_name)
                .on_press_maybe((!closing).then_some(Message::CloseCard(card)))
                .into(),
        );
        if let Some(error) = &liveness.end_error {
            rows.push(
                widget::selectable_text(error.clone())
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            );
        }
        // A Close the daemon refused says why under the button that asked,
        // with what still holds it and the way to end that.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
            && let Some(refusal) = runtime.close_refusal_view()
        {
            rows.push(refusal);
        }
        let open = self.chat_ui.blocked_details.get(&card) == Some(source);
        let mut details = vec![disclosure::row(
            if open { "Hide details" } else { "Details" }.to_owned(),
            format!(
                "{} technical details for why this conversation can't be reopened",
                if open { "Hide" } else { "Show" }
            ),
            Icon::Info,
            None,
            None,
            Some(open),
            Some(Action::ToggleBlockedDetails(card, source.clone())),
        )];
        if open {
            if let Some(text) = self.blocked_details_text(card) {
                details.push(
                    widget::selectable_text(text.to_owned())
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .width(Length::Fill)
                        .into(),
                );
            }
            details.push(
                crate::ui_controls::button::quiet("Copy details")
                    .on_press(action(Action::CopyBlockedDetails(card, source.clone())))
                    .into(),
            );
        }
        rows.push(
            widget::column::with_children(details)
                .spacing(space::S)
                .into(),
        );
        Some(
            widget::container(
                widget::container(widget::column::with_children(rows).spacing(space::L))
                    .class(cosmic::theme::Container::Secondary)
                    .max_width(520)
                    .padding(space::XL),
            )
            .center(Length::Fill)
            .into(),
        )
    }

    /// Reopens a conversation the update stopped. Restoring it only meets
    /// the daemon's refusal again, so this is an explicit start of the saved
    /// conversation, as Resume conversation and opening it from the sidebar
    /// make, which also lifts a startup restore's attach-only rule for this
    /// pane. The provider starts on the conversation with nothing sent.
    pub(super) fn reopen_stopped(&mut self, card: CardId) -> Task<Message> {
        if self.chat_start_pending(card) {
            return Task::none();
        }
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
            if !runtime.start_blocked.is_some_and(Reopen::reopens)
                || !runtime.commands.is_idle()
                || runtime.attachment_chooser_pending.is_some()
                || runtime.pastes_pending > 0
            {
                return Task::none();
            }
            runtime.release_start_block();
            let session = runtime.return_session();
            return self.start_chat(card, session, chat::Intent::Start, None, false);
        }
        if !self
            .blocked_failure(card)
            .is_some_and(|blocked| blocked.reopen.reopens())
        {
            return Task::none();
        }
        self.chat_ui.blocked_starts.remove(&card);
        self.retry_card_as(card, Some(chat::Intent::Start))
    }

    pub(super) fn begin_fresh_conversation(&mut self, card: CardId) -> Task<Message> {
        if !self.chat_offers_fresh_conversation(card) {
            return Task::none();
        }
        // The draft Fresh keeps must be complete: its button is disabled
        // until a pending send, paste or attachment settles.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
            && (!runtime.commands.is_idle()
                || runtime.attachment_chooser_pending.is_some()
                || runtime.pastes_pending > 0)
        {
            return Task::none();
        }
        let Some(CardKind::Chat(requested)) = self.card_state(card).map(|state| &state.kind) else {
            return Task::none();
        };
        let requested = requested.clone();
        // The runtime may have learned the provider identity since the saved
        // descriptor was published. Close that exact owner, and separately
        // guard the canonical descriptor against replacement during cleanup.
        let Some(source) = self.chat_return_session(card) else {
            return Task::none();
        };
        let Some(replacement) = source.fresh_start() else {
            return Task::none();
        };
        self.clear_close_refusal(card);
        let target = self.chat_ui.next_target(card);
        self.chat_ui.phases.insert(
            card,
            Phase::Closing {
                target,
                restart: Some(replacement),
            },
        );
        cosmic::task::future(async move {
            let closing = source.clone();
            let result =
                tokio::task::spawn_blocking(move || close_fresh_conversation(None, Some(closing)))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            action(Action::FreshCloseFinished(
                target,
                Box::new(source),
                Box::new(requested),
                result,
            ))
        })
    }

    pub(super) fn finish_fresh_conversation(
        &mut self,
        target: Target,
        source: chat::Session,
        requested: chat::Session,
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.chat_ui.abandoned.remove(&target).is_some() {
            return match result {
                Ok(()) => Task::none(),
                Err(_) => self.end_chat(target.card, None, Some(source), close_fresh_conversation),
            };
        }
        let Some(Phase::Closing {
            restart: Some(replacement),
            ..
        }) = self.chat_ui.finish(target, |phase| {
            matches!(
                phase,
                Phase::Closing {
                    restart: Some(_),
                    ..
                }
            )
        })
        else {
            return Task::none();
        };
        if self.card_state(target.card).is_none_or(|state| {
            !state.existence.is_open()
                || !matches!(&state.kind, CardKind::Chat(current) if current == &requested)
        }) || self.chat_return_session(target.card).as_ref() != Some(&source)
        {
            return Task::none();
        }
        if let Err(error) = result {
            return self.refuse_chat_close(target.card, source, error);
        }
        if self.refuse_for_deleting_card(target.card) {
            if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                runtime.disconnected = true;
                runtime.error = Some("Restart cancelled: the worktree was being deleted.".into());
            }
            self.observe_agent_activity_for_card(target.card, None);
            return Task::none();
        }
        // Only checked retirement commits the new pointer. Stashing the
        // composer here preserves unsent text and attachments without sending.
        self.chat_forget(target.card);
        self.cards.insert(target.card, CardRuntime::Loading);
        self.replace_chat_conversation(target.card, replacement.clone());
        self.start_chat(target.card, replacement, chat::Intent::Start, None, false)
    }
}

/// The processes of `found` proved to be the conversation's.
fn proved(found: &[crate::terminal::ipc::LostProcess]) -> Vec<crate::terminal::ipc::ProcessStart> {
    found
        .iter()
        .filter(|process| process.proved)
        .map(|process| process.process)
        .collect()
}
