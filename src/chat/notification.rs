//! A bounded live-turn excerpt, independent of transcript paging.
use super::{Event, ItemContent, Phase, Role, Snapshot, ToolStatus};
use crate::notifications::{Content, Kind, RequestType, presentation};

const RETAINED_BYTES: usize = 8192;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    id: String,
    text: String,
    final_response: bool,
    fallback: String,
}

impl Preview {
    pub(super) fn observe(&mut self, event: &Event) {
        match event {
            Event::TurnStarted | Event::Reset { .. } => *self = Self::default(),
            Event::Item(item) if item.role == Role::Assistant => match &item.content {
                ItemContent::Text {
                    text,
                    thinking: false,
                    phase,
                } => {
                    if self.final_response && *phase != Phase::Final {
                        return;
                    }
                    self.id = bounded(&item.id, 1024);
                    self.text = bounded(text, RETAINED_BYTES);
                    self.final_response = *phase == Phase::Final;
                }
                ItemContent::Command { label, status, .. } => {
                    let label = presentation::preview(label);
                    self.fallback = format!(
                        "{}{}",
                        tool_verb(*status, "Command"),
                        if label.is_empty() {
                            String::new()
                        } else {
                            format!(": {label}")
                        }
                    );
                }
                ItemContent::Tool { name, status, .. } => {
                    self.fallback = format!(
                        "{}: {}",
                        tool_verb(*status, "Tool"),
                        presentation::title(name)
                    );
                }
                _ => {}
            },
            Event::TextDelta {
                id,
                role: Role::Assistant,
                thinking: false,
                delta,
            } => {
                if id != &self.id {
                    if self.final_response {
                        return;
                    }
                    self.id = bounded(id, 1024);
                    self.text.clear();
                }
                let available = RETAINED_BYTES.saturating_sub(self.text.len());
                self.text.push_str(&bounded(delta, available));
            }
            _ => {}
        }
    }
}

fn tool_verb(status: ToolStatus, noun: &str) -> String {
    format!(
        "{noun} {}",
        match status {
            ToolStatus::Running => "activity",
            ToolStatus::Succeeded => "completed",
            ToolStatus::Failed => "failed",
            ToolStatus::Denied => "denied",
            ToolStatus::Stopped => "stopped",
        }
    )
}

fn bounded(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Bounded source metadata captured under the model lock; projection and
/// formatting run after releasing it, independently of transcript storage.
#[cfg(feature = "host")]
pub(crate) struct FrameContext {
    conversation: String,
    error: String,
    preview: Preview,
}
#[cfg(feature = "host")]
impl FrameContext {
    pub fn capture(state: &Snapshot) -> Self {
        Self {
            conversation: presentation::title(state.title.as_deref().unwrap_or_default()),
            error: bounded(state.error.as_deref().unwrap_or_default(), RETAINED_BYTES),
            preview: state.notification_preview.as_ref().clone(),
        }
    }
    fn content(&self, kind: Kind) -> Content {
        Content {
            conversation: self.conversation.clone(),
            preview: response_preview(&self.preview, Some(&self.error), kind),
            request: None,
        }
    }
    /// Keep the authoritative event order; enrich only the first64 outcomes of
    /// a pathological frame, with remaining evidence still metadata-only.
    pub fn project(mut self, events: &[Event]) -> std::collections::VecDeque<Content> {
        const CONTEXTS: usize = 64;
        fn flush(
            basis: &FrameContext,
            pending: &mut Vec<Kind>,
            out: &mut std::collections::VecDeque<Content>,
        ) {
            out.extend(pending.drain(..).map(|kind| basis.content(kind)));
        }
        let mut pending = Vec::new();
        let mut out = std::collections::VecDeque::new();
        let mut contexts = 0;
        for event in events {
            if matches!(event, Event::TurnStarted | Event::Reset { .. }) {
                flush(&self, &mut pending, &mut out);
                self.preview.observe(event);
                self.error.clear();
                if matches!(event, Event::Reset { .. }) {
                    self.conversation.clear();
                }
            } else {
                self.preview.observe(event);
            }
            match event {
                Event::Session {
                    title: Some(title), ..
                } => self.conversation = presentation::title(title),
                Event::Error { message, .. } => self.error = bounded(message, RETAINED_BYTES),
                Event::TurnFinished { outcome, .. } if contexts < CONTEXTS => {
                    pending.push(*outcome);
                    contexts += 1;
                }
                _ => {}
            }
            if matches!(event, Event::Error { fatal: true, .. }) {
                break;
            }
        }
        flush(&self, &mut pending, &mut out);
        out
    }
}

fn response_preview(preview: &Preview, error: Option<&str>, kind: Kind) -> String {
    if matches!(kind, Kind::Failed | Kind::Interrupted) {
        presentation::preview(error.unwrap_or_default())
    } else {
        let text = presentation::preview(&preview.text);
        presentation::plain_preview(if text.is_empty() {
            &preview.fallback
        } else {
            &text
        })
    }
}

impl Snapshot {
    /// Stream the actionable request metadata into a fixed-size fingerprint.
    /// Do not truncate identity, clone its payload, or include secret answers
    /// and unrelated streaming/output changes while waiting.
    pub(crate) fn waiting_fingerprint(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        let request = self.requests.first()?;
        let mut digest = Sha256::new();
        for text in [
            request.id.as_str(),
            request.title.as_str(),
            request.description.as_str(),
        ] {
            digest.update(text.len().to_le_bytes());
            digest.update(text.as_bytes());
        }
        digest.update([match request.kind {
            super::RequestKind::Approval => 0,
            super::RequestKind::Question => 1,
            super::RequestKind::Consent => 2,
        }]);
        Some(format!("{:x}", digest.finalize()))
    }

    pub(crate) fn notification_content(&self, kind: Kind) -> Content {
        let mut content = Content {
            conversation: presentation::title(self.title.as_deref().unwrap_or_default()),
            preview: String::new(),
            request: None,
        };
        if kind == Kind::Waiting {
            if let Some(request) = self.requests.first() {
                content.request = Some(match request.kind {
                    super::RequestKind::Approval => RequestType::Approval,
                    super::RequestKind::Question => RequestType::Question,
                    super::RequestKind::Consent => RequestType::Consent,
                });
                // Never include secret field values or command arguments.
                content.preview = presentation::preview(&request.description);
                if content.preview.is_empty() {
                    content.preview = presentation::preview(&request.title);
                }
            }
        }
        if content.preview.is_empty() {
            content.preview =
                response_preview(&self.notification_preview, self.error.as_deref(), kind);
        }
        content.preview = presentation::plain_preview(&content.preview);
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn live_preview_survives_history_paging_and_turn_reset_clears_it() {
        let mut state = Snapshot::default();
        Arc::make_mut(&mut state.notification_preview).observe(&Event::TurnStarted);
        Arc::make_mut(&mut state.notification_preview).observe(&Event::Item(super::super::Item {
            id: "answer".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: "**Done.**\n```sh\nsecret tool output\n```\nAll tests passed.".into(),
                thinking: false,
                phase: Phase::Final,
            },
        }));
        state.items = vec![Arc::new(super::super::Item {
            id: "old".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: "Old response must not notify".into(),
                thinking: false,
                phase: Phase::Final,
            },
        })];
        assert_eq!(
            state.notification_content(Kind::Succeeded).preview,
            "Done. All tests passed."
        );
        Arc::make_mut(&mut state.notification_preview).observe(&Event::TurnStarted);
        assert!(
            state
                .notification_content(Kind::Succeeded)
                .preview
                .is_empty()
        );
    }

    #[test]
    fn streaming_preview_is_bounded_before_retention() {
        let mut state = Snapshot::default();
        for _ in 0..100 {
            Arc::make_mut(&mut state.notification_preview).observe(&Event::TextDelta {
                id: "answer".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "🦀".repeat(10000),
            });
        }
        assert!(state.notification_preview.text.len() <= RETAINED_BYTES);
        assert!(
            state
                .notification_content(Kind::Succeeded)
                .preview
                .chars()
                .count()
                <= presentation::PREVIEW_LIMIT
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn immutable_frame_context_stops_at_turn_boundaries_and_bounds_rich_copies() {
        let state = Snapshot::default();
        let text = |id: &str, text: &str| {
            Event::Item(super::super::Item {
                id: id.into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::Text {
                    text: text.into(),
                    thinking: false,
                    phase: Phase::Final,
                },
            })
        };
        let mut events = vec![
            Event::TurnFinished {
                key: Some("old".into()),
                outcome: Kind::Succeeded,
            },
            text("old-answer", "Old `notification_preview` answer"),
            Event::TurnStarted,
            text("new-answer", "New answer"),
            Event::TurnFinished {
                key: Some("new".into()),
                outcome: Kind::Succeeded,
            },
        ];
        let contexts = FrameContext::capture(&state).project(&events);
        assert_eq!(contexts[0].preview, "Old notification_preview answer");
        assert_eq!(contexts[1].preview, "New answer");
        assert!(
            state.notification_preview.text.is_empty(),
            "preprojection must not mutate the resident transcript/model"
        );
        events.extend((0..1000).map(|_| Event::TurnFinished {
            key: None,
            outcome: Kind::Succeeded,
        }));
        assert_eq!(
            FrameContext::capture(&state).project(&events).len(),
            64,
            "extra evidence remains metadata-only with bounded context storage"
        );
    }

    #[test]
    fn producer_to_desktop_preserves_plain_identifiers_and_code_punctuation() {
        for (raw, expected) in [
            (
                "Updated `notification_preview`.",
                "Updated notification_preview.",
            ),
            (
                "Updated config_file.rs and calculated 2 * 3.",
                "Updated config_file.rs and calculated 2 * 3.",
            ),
            ("```rust\nlet x = *ptr;\n```", "let x = *ptr;"),
        ] {
            let mut state = Snapshot::default();
            Arc::make_mut(&mut state.notification_preview).observe(&Event::Item(
                super::super::Item {
                    id: "answer".into(),
                    role: Role::Assistant,
                    at: None,
                    started_at: None,
                    content: ItemContent::Text {
                        text: raw.into(),
                        thinking: false,
                        phase: Phase::Final,
                    },
                },
            ));
            let mut event = crate::notifications::Event::session(
                crate::model::TerminalId::new(),
                crate::agent::Provider::Codex,
                "/repo".into(),
                crate::notifications::Evidence {
                    key: "turn".into(),
                    kind: Kind::Succeeded,
                },
            );
            event.content = Some(state.notification_content(Kind::Succeeded));
            assert!(presentation::message(&event, true).body.ends_with(expected));
        }
    }
}
