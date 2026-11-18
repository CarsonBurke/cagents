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
pub(super) const UNNAMED_AGENT_HEADLINE: &str = "This agent's conversation can't be reopened.";
pub(super) const UNNAMED_AGENT_GUIDANCE: &str = "The agent stays open until you close this tab. Closing ends the agent and discards any unsent message.";
pub(super) const CLOSE_GUIDANCE: &str = "Details say why. Closing this tab discards any unsent message; the conversation itself isn't deleted.";

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
}

impl Reopen {
    /// The permanent refusals; `None` for every recovery that has an action.
    pub(in crate::app) fn of(recovery: chat::Recovery) -> Option<Self> {
        match recovery {
            chat::Recovery::Blocked => Some(Self::Unproven),
            chat::Recovery::AfterRestart => Some(Self::AfterRestart),
            chat::Recovery::Account
            | chat::Recovery::Retry
            | chat::Recovery::Busy
            | chat::Recovery::Unresponsive
            | chat::Recovery::Unreached
            | chat::Recovery::Capacity
            | chat::Recovery::Fresh => None,
        }
    }

    pub(in crate::app) fn headline(self) -> &'static str {
        match self {
            Self::Unproven => HEADLINE,
            Self::AfterRestart => RESTART_HEADLINE,
            Self::UnnamedAgent => UNNAMED_AGENT_HEADLINE,
        }
    }

    /// The caption under a saved conversation's refusal, with the
    /// explanation of Start a new conversation when this card offers it.
    pub(super) fn guidance(self, fresh: bool) -> String {
        match (self, fresh) {
            (Self::Unproven, true) => FRESH_EXPLANATION.to_owned(),
            (Self::Unproven, false) => CLOSE_GUIDANCE.to_owned(),
            (Self::AfterRestart, true) => [RESTART_GUIDANCE, RESTART_FRESH_TRADEOFF].join(" "),
            (Self::AfterRestart, false) => [RESTART_GUIDANCE, RESTART_CLOSE_TRADEOFF].join(" "),
            (Self::UnnamedAgent, _) => UNNAMED_AGENT_GUIDANCE.to_owned(),
        }
    }

    /// Whether doing nothing is the way back. No button is then emphasized
    /// as the next step, since each of them gives that way up.
    pub(in crate::app) fn waits(self) -> bool {
        self == Self::AfterRestart
    }
}

/// A start refused before a chat runtime exists, kept with the exact source
/// it refused so a replacement never inherits its diagnostics.
#[derive(Clone, Debug)]
pub(in crate::app) struct BlockedStart {
    pub(in crate::app) source: chat::Session,
    pub(in crate::app) reopen: Reopen,
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
            blocked
                .reopen
                .guidance(self.chat_offers_fresh_conversation(card))
        })
    }

    pub(super) fn begin_fresh_conversation(&mut self, card: CardId) -> Task<Message> {
        if !self.chat_offers_fresh_conversation(card) {
            return Task::none();
        }
        // The draft Fresh keeps must be complete: its button is disabled
        // until a pending send, paste or attachment settles.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
            && (runtime.pending.is_some()
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
                target, source, requested, result,
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
