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
}

/// What pressing one of a stopped card's buttons does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Press {
    /// Reconnect, or resume a conversation whose provider stopped here.
    Reconnect,
    /// Start a provider on a saved conversation the card is reading.
    ResumeSaved,
    /// Leave the conversation that did not load and start one beside it.
    StartFresh,
}

impl Press {
    pub(super) fn action(self, target: Target) -> Action {
        match self {
            Self::Reconnect => Action::Reconnect(target),
            Self::ResumeSaved => Action::ResumeSaved(target),
            Self::StartFresh => Action::StartFresh(target.card),
        }
    }
}

impl Runtime {
    pub(super) fn stopped(&self, transitioning: bool, resuming: bool) -> Stopped {
        if self.start_retry_blocked {
            return Stopped {
                guidance: String::new(),
                primary: None,
                secondary: None,
            };
        }
        let snapshot = &self.snapshot;
        let provider = self.session.launch.provider.label();
        let unresumable = self.offers_fresh_conversation();
        let saved_history = self.has_connected_saved_history(snapshot);
        let enabled = !transitioning
            && self.pending.is_none()
            && self.attachment_chooser_pending.is_none()
            && self.pastes_pending == 0
            && (!saved_history || !snapshot.history_loading);
        if saved_history {
            return Stopped {
                guidance: snapshot
                    .read_only_reason
                    .clone()
                    .unwrap_or_else(|| "Saved conversation history is read-only.".into()),
                primary: Some(("Resume conversation", Press::ResumeSaved, enabled)),
                secondary: None,
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
                        "{provider} stopped before the saved conversation loaded. Try again, or start a new conversation beside it: the old one stays on disk, but this card stops pointing at it."
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
            };
        }
        if unresumable {
            return Stopped {
                guidance: "The saved conversation did not load. Try again, or start a new conversation beside it: the old one stays on disk, but this card stops pointing at it.".into(),
                primary: Some(("Try again", Press::Reconnect, enabled)),
                secondary: Some((
                    "Start a new conversation",
                    Press::StartFresh,
                    enabled,
                )),
            };
        }
        if self.disconnected || !self.connected() {
            return Stopped {
                guidance: format!(
                    "Lost the connection to this chat. {provider} may still be working; your draft is kept."
                ),
                primary: Some(("Reconnect", Press::Reconnect, enabled)),
                secondary: None,
            };
        }
        if self.session.owns_codex_listener() {
            return Stopped {
                guidance: format!(
                    "This chat lost its link to the {provider} session, which may still be running in its terminal. Your draft is kept."
                ),
                primary: Some(("Reconnect", Press::Reconnect, enabled)),
                secondary: None,
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
        }
    }

    pub(super) fn stopped_controls(
        &self,
        transitioning: bool,
        resuming: bool,
    ) -> Element<'_, Message> {
        let Stopped {
            guidance,
            primary,
            secondary,
        } = self.stopped(transitioning, resuming);
        let mut controls = Vec::new();
        if let Some((label, pressed, enabled)) = primary {
            controls.push(
                crate::ui_controls::button::primary(label)
                    .on_press_maybe(enabled.then(|| action(pressed.action(self.target))))
                    .into(),
            );
        }
        if let Some((label, pressed, enabled)) = secondary {
            controls.push(
                crate::ui_controls::button::secondary(label)
                    .on_press_maybe(enabled.then(|| action(pressed.action(self.target))))
                    .into(),
            );
        }
        let mut rows = Vec::new();
        if !guidance.is_empty() {
            rows.push(design::caption(guidance).width(Length::Fill).into());
        }
        if !controls.is_empty() {
            rows.push(
                widget::row::with_children(controls)
                    .spacing(step::GAP_S)
                    .wrap()
                    .into(),
            );
        }
        widget::column::with_children(rows).spacing(space::S).into()
    }
}
