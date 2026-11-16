//! Recovery from a permanent refusal, without retrying the refused source.
use super::*;

pub(in crate::app) const HEADLINE: &str = "This conversation can't be reopened safely.";
pub(super) const DRAFT_HEADLINE: &str = "This conversation couldn't start.";
pub(super) const DRAFT_GUIDANCE: &str = "Your message is kept. Copy it into a new conversation, or see Details for why this one couldn't start.";
pub(super) const FRESH_EXPLANATION: &str = "Starts a new conversation in this card. The old one stays on disk, but this card stops pointing at it. Your draft is kept.";
const REOPEN_GUIDANCE: &str = "Close this card, then use Resume conversation in the new tab (+) menu to reopen the conversation.";

pub(super) fn guidance(fresh: bool) -> String {
    if fresh {
        [FRESH_EXPLANATION, REOPEN_GUIDANCE].join(" ")
    } else {
        REOPEN_GUIDANCE.to_owned()
    }
}

impl Cagents {
    pub(in crate::app) fn blocked_draft_discard(&self, card: CardId) -> Option<TerminalId> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        // Every blocked surface says the draft is kept, and nothing restores
        // it once its card closes: a reopened conversation gets a new card.
        (runtime.start_retry_blocked
            && (!runtime.composer.text().trim().is_empty() || !runtime.attachments.is_empty()))
        .then_some(runtime.session.id)
    }

    /// Failed-card diagnostics are authorized by the same exact source as the
    /// refusal. A stale Details or Copy press cannot expose a replacement.
    pub(in crate::app) fn blocked_failure_source(&self, card: CardId) -> Option<&chat::Session> {
        self.chat_ui.blocked_starts.get(&card).filter(|source| {
            matches!(self.cards.get(&card), Some(CardRuntime::Failed(_)))
                && self.card_state(card).is_some_and(
                    |state| matches!(&state.kind, CardKind::Chat(current) if current == *source),
                )
        })
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
        self.blocked_failure_source(card)
            .map(|_| guidance(self.chat_offers_fresh_conversation(card)))
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
