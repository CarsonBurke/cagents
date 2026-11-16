//! What a card offers once its agent is no longer reachable: the provider
//! stopped, or the card lost its daemon connection.
//!
//! Each state says what happened in the reader's terms and names the one
//! action that answers it on a labeled button. A state with no action that
//! would work here says what to do instead, rather than offering a control
//! that would only bring the same state back.

use super::*;

/// The recovery a stopped card offers, and the words around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stopped {
    pub(super) guidance: String,
    /// The labeled primary action, with whether it can be pressed now.
    pub(super) primary: Option<(&'static str, Press, bool)>,
    /// A second, lesser choice beside it.
    pub(super) secondary: Option<(&'static str, Press, bool)>,
    /// Every label the primary action can take while this card stays in its
    /// state, so pressing it does not resize it under the pointer.
    pub(super) primary_labels: &'static [&'static str],
}

/// What pressing one of a stopped card's buttons does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Press {
    /// Reconnect, or resume a conversation whose provider stopped here.
    Reconnect,
    /// Start a provider on a saved conversation the card is reading.
    ResumeSaved,
    /// Start a new conversation in this card after checked retirement.
    StartFresh,
    Close,
}

impl Press {
    pub(super) fn message(self, target: Target) -> Message {
        match self {
            Self::Reconnect => action(Action::Reconnect(target)),
            Self::ResumeSaved => action(Action::ResumeSaved(target)),
            Self::StartFresh => action(Action::StartFresh(target.card)),
            Self::Close => Message::CloseCard(target.card),
        }
    }
}

impl Runtime {
    pub(super) fn stopped(&self, transitioning: bool, resuming: bool) -> Stopped {
        self.stopped_with_offer(transitioning, resuming, self.offers_fresh_conversation())
    }

    fn stopped_with_offer(
        &self,
        transitioning: bool,
        resuming: bool,
        fresh_available: bool,
    ) -> Stopped {
        let snapshot = &self.snapshot;
        let provider = self.session.launch.provider.label();
        let unresumable = fresh_available;
        let saved_history = self.has_connected_saved_history(snapshot);
        let enabled = !transitioning
            && self.pending.is_none()
            && self.attachment_chooser_pending.is_none()
            && self.pastes_pending == 0
            && (!saved_history || !snapshot.history_loading);
        if let Some(reopen) = self.start_blocked {
            return Stopped {
                guidance: if self.is_draft() {
                    blocked_recovery::DRAFT_GUIDANCE.to_owned()
                } else {
                    reopen.guidance(unresumable)
                },
                primary: Some(if unresumable {
                    ("Start a new conversation", Press::StartFresh, enabled)
                } else {
                    ("Close", Press::Close, !transitioning)
                }),
                secondary: unresumable.then_some(("Close", Press::Close, !transitioning)),
                primary_labels: &[],
            };
        }
        if saved_history {
            return Stopped {
                guidance: snapshot
                    .read_only_reason
                    .clone()
                    .unwrap_or_else(|| "Saved conversation history is read-only.".into()),
                primary: Some(("Resume conversation", Press::ResumeSaved, enabled)),
                secondary: None,
                primary_labels: &[],
            };
        }
        if self.resumes_in_place() {
            // The provider's own report, if it left one, already stands above
            // this line; this says what is kept and what resuming does.
            return Stopped {
                guidance: if resuming {
                    format!("Starting {provider} again on this conversation…")
                } else if self.resume_failed && !unresumable {
                    format!(
                        "{provider} did not start again. The conversation and your draft are kept."
                    )
                } else if unresumable {
                    format!(
                        "{provider} stopped before the saved conversation loaded. {}",
                        blocked_recovery::FRESH_EXPLANATION
                    )
                } else {
                    format!("{provider} stopped. The conversation and your draft are kept.")
                },
                primary: Some((
                    if resuming {
                        "Resuming…"
                    } else if unresumable || self.resume_failed {
                        "Try again"
                    } else {
                        "Resume conversation"
                    },
                    Press::Reconnect,
                    enabled,
                )),
                secondary: (unresumable && !resuming).then_some((
                    "Start a new conversation",
                    Press::StartFresh,
                    enabled,
                )),
                primary_labels: &["Resume conversation", "Resuming…", "Try again"],
            };
        }
        if unresumable {
            return Stopped {
                guidance: format!(
                    "The saved conversation did not load. {}",
                    blocked_recovery::FRESH_EXPLANATION
                ),
                primary: Some(("Try again", Press::Reconnect, enabled)),
                secondary: Some(("Start a new conversation", Press::StartFresh, enabled)),
                primary_labels: &[],
            };
        }
        if self.disconnected || !self.connected() {
            return Stopped {
                guidance: format!(
                    "Lost the connection to this chat. {provider} may still be working; your draft is kept."
                ),
                primary: Some(("Reconnect", Press::Reconnect, enabled)),
                secondary: None,
                primary_labels: &[],
            };
        }
        if self.session.owns_codex_listener() {
            return Stopped {
                guidance: format!(
                    "This chat lost its link to the {provider} session, which may still be running in its terminal. Your draft is kept."
                ),
                primary: Some(("Reconnect", Press::Reconnect, enabled)),
                secondary: None,
                primary_labels: &[],
            };
        }
        // Attaching again would only bring back this same stopped actor, and
        // nothing here can start its provider on the conversation yet.
        Stopped {
            guidance: format!(
                "{provider} stopped, and this card can't start it again. To continue, copy your draft, close this card, then reopen the conversation with Resume conversation in the new tab (+) menu."
            ),
            primary: None,
            secondary: None,
            primary_labels: &[],
        }
    }

    /// Whether the first recovery button is styled as the next step. Not
    /// while a restart is the way back: every button gives that up.
    pub(super) fn emphasizes_recovery(&self) -> bool {
        !self
            .start_blocked
            .is_some_and(blocked_recovery::Reopen::waits)
    }

    pub(super) fn stopped_controls(
        &self,
        transitioning: bool,
        resuming: bool,
        fresh_available: bool,
    ) -> Element<'_, Message> {
        let Stopped {
            guidance,
            primary,
            secondary,
            primary_labels,
        } = self.stopped_with_offer(transitioning, resuming, fresh_available);
        let mut controls = Vec::new();
        let emphasize = self.emphasizes_recovery();
        if let Some((label, pressed, enabled)) = primary {
            controls.push(
                if emphasize {
                    crate::ui_controls::button::primary(label)
                } else {
                    crate::ui_controls::button::secondary(label)
                }
                .keep_room_for(primary_labels.iter().copied())
                .on_press_maybe(enabled.then(|| pressed.message(self.target)))
                .into(),
            );
        }
        if let Some((label, pressed, enabled)) = secondary {
            controls.push(
                crate::ui_controls::button::secondary(label)
                    .on_press_maybe(enabled.then(|| pressed.message(self.target)))
                    .into(),
            );
        }
        let mut rows = Vec::new();
        if !guidance.is_empty() {
            rows.push(design::caption(guidance).width(Length::Fill).into());
        }
        if !controls.is_empty() {
            rows.push(
                widget::flex_row(controls)
                    .spacing(step::GAP_S as u16)
                    .into(),
            );
        }
        widget::column::with_children(rows).spacing(space::S).into()
    }
}
