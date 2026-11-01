//! A bounded live-turn excerpt, independent of transcript paging.
use super::{Event, ItemContent, Phase, Role, Snapshot, ToolStatus};
use crate::notifications::{Content, Kind, RequestType, presentation};

const RETAINED_BYTES: usize = 8192;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    topic: String,
    id: String,
    text: String,
    final_response: bool,
    fallback: String,
    keep_at_start: bool,
}

impl Preview {
    /// A decoder has just proved this buffered context belongs to the start
    /// in the same step. Consume exactly that reset, then resume normal turn
    /// boundaries; a later acknowledgement must not restore this older copy.
    #[cfg(feature = "host")]
    pub(super) fn at_observed_start(mut self) -> Self {
        self.keep_at_start = true;
        self
    }

    pub(super) fn observe(&mut self, event: &Event) {
        match event {
            Event::TurnStarted if std::mem::take(&mut self.keep_at_start) => {}
            Event::TurnStarted => {
                // Claude emits its actual user item before TurnStarted in the
                // accepted Send step. Preserve that message label while the
                // response excerpt resets; the next live user item replaces it.
                let topic = std::mem::take(&mut self.topic);
                *self = Self {
                    topic,
                    ..Default::default()
                };
            }
            Event::Reset { .. } => *self = Self::default(),
            Event::Item(item) | Event::ItemAfter { item, .. } | Event::ItemBefore { item, .. }
                if item.role == Role::User =>
            {
                if let ItemContent::Text {
                    text,
                    thinking: false,
                    ..
                } = &item.content
                {
                    // A provider may have no conversation title. Use only its
                    // current live message as a short opt-in label; this is
                    // turn context, not a persisted canonical channel name.
                    // Never page history or read request answers/defaults.
                    self.topic = presentation::title(&presentation::preview(text));
                }
            }
            Event::Item(item) | Event::ItemAfter { item, .. } | Event::ItemBefore { item, .. }
                if item.role == Role::Assistant =>
            {
                match &item.content {
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
                }
            }
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

impl Preview {
    fn conversation(&self, provider_title: Option<&str>) -> String {
        let title = presentation::title(provider_title.unwrap_or_default());
        if title.is_empty() {
            self.topic.clone()
        } else {
            title
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
    session_id: Option<String>,
    conversation: String,
    error: String,
    preview: Preview,
}
#[cfg(feature = "host")]
impl FrameContext {
    pub fn capture(state: &Snapshot) -> Self {
        Self {
            session_id: state.session_id.clone(),
            conversation: presentation::title(state.title.as_deref().unwrap_or_default()),
            error: bounded(state.error.as_deref().unwrap_or_default(), RETAINED_BYTES),
            preview: state.notification_preview.as_ref().clone(),
        }
    }
    fn content(&self, kind: Kind) -> Content {
        Content {
            conversation: self.preview.conversation(Some(&self.conversation)),
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
                    self.session_id = None;
                    self.conversation.clear();
                }
            } else {
                self.preview.observe(event);
            }
            match event {
                Event::Session { id, title } => {
                    // Mirror Snapshot's naming boundary: identity-only updates
                    // preserve this conversation's provider title, while an
                    // actual session change cannot carry the old name forward.
                    if self.session_id.as_deref() != Some(id.as_str()) {
                        self.conversation =
                            presentation::title(title.as_deref().unwrap_or_default());
                    } else if let Some(title) = title {
                        self.conversation = presentation::title(title);
                    }
                    self.session_id = Some(id.clone());
                }
                Event::Error { message, .. } => self.error = bounded(message, RETAINED_BYTES),
                Event::TurnFinished { outcome, .. } if contexts < CONTEXTS => {
                    pending.push(*outcome);
                    contexts += 1;
                }
                _ => {}
            }
            // A fatal/cache error cannot discard a final outcome later in
            // this decoded frame. The Actor publishes the same immutable
            // context before returning the original error.
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

/// Public prompts are provider-neutral here: Claude puts its question in the
/// field label and Codex puts it in the description. Retain both, without
/// copying field defaults or answers, under the same bounded Markdown scan.
fn public_question_preview(request: &super::Request) -> Option<String> {
    if request.kind != super::RequestKind::Question {
        return None;
    }
    // Some providers put the question in the request message and repeat its
    // heading as the field label (OMP); those fields contain input help, not
    // a replacement for that message. Keep the existing meaningful message
    // first in that shape. Claude/Codex have distinct public question labels.
    let message_first = request
        .fields
        .iter()
        .filter(|field| !field.secret)
        .all(|field| field.label.is_empty() || field.label == request.title);
    let mut text = if message_first {
        bounded(&request.description, RETAINED_BYTES)
    } else {
        String::new()
    };
    for field in request.fields.iter().filter(|field| !field.secret) {
        for part in [field.label.as_str(), field.description.as_str()] {
            if part.is_empty() || text.len() == RETAINED_BYTES {
                continue;
            }
            if !text.is_empty() {
                let separator = bounded(" · ", RETAINED_BYTES - text.len());
                text.push_str(&separator);
            }
            text.push_str(&bounded(part, RETAINED_BYTES - text.len()));
        }
        if text.len() == RETAINED_BYTES {
            break;
        }
    }
    let preview = presentation::preview(&text);
    (!preview.is_empty()).then_some(preview)
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
        // A provider can replace a question or its options under the same RPC ID.
        // This fingerprint is internal, never a preview or serialized evidence;
        // secret prompt changes still need fresh attention. Answer/default
        // values never define a new notice or enter this digest.
        for field in &request.fields {
            digest.update([0]);
            for text in [
                field.id.as_str(),
                field.label.as_str(),
                field.description.as_str(),
            ] {
                digest.update(text.len().to_le_bytes());
                digest.update(text.as_bytes());
            }
            digest.update([u8::from(field.multiple), u8::from(field.secret)]);
            digest.update(field.choices.len().to_le_bytes());
            for choice in &field.choices {
                for text in [
                    choice.id.as_str(),
                    choice.label.as_str(),
                    choice.description.as_str(),
                ] {
                    digest.update(text.len().to_le_bytes());
                    digest.update(text.as_bytes());
                }
            }
        }
        Some(format!("{:x}", digest.finalize()))
    }

    pub(crate) fn notification_content(&self, kind: Kind) -> Content {
        let mut content = Content {
            conversation: self
                .notification_preview
                .conversation(self.title.as_deref()),
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
                // Public question prompts are useful; field initial/answer
                // values and all secret fields stay out of the excerpt.
                content.preview = public_question_preview(request)
                    .unwrap_or_else(|| presentation::preview(&request.description));
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
    fn notification_question_context_uses_public_metadata_without_answer_values() {
        use crate::chat::{Choice, Field, Request, RequestKind};
        let mut snapshot = Snapshot {
            requests: vec![Request {
                kind: RequestKind::Question,
                id: "question".into(),
                title: "Provider questions".into(),
                description: "Provider is waiting for your answers.".into(),
                fields: vec![
                    Field {
                        id: "public".into(),
                        label: "Which `config_file.rs` strategy?".into(),
                        description: "Migration".into(),
                        choices: vec![Choice {
                            id: "keep".into(),
                            label: "Keep".into(),
                            description: "Keep existing".into(),
                        }],
                        multiple: false,
                        secret: false,
                        initial: "PRIVATE_ANSWER".into(),
                    },
                    Field {
                        id: "secret".into(),
                        label: "PRIVATE_LABEL".into(),
                        description: "PRIVATE_DESCRIPTION".into(),
                        choices: vec![],
                        multiple: false,
                        secret: true,
                        initial: "PRIVATE_SECRET".into(),
                    },
                ],
                actions: vec![],
            }],
            ..Default::default()
        };
        let content = snapshot.notification_content(Kind::Waiting);
        assert!(content.preview.contains("Which config_file.rs strategy?"));
        assert!(!content.preview.contains("PRIVATE"));
        let original = snapshot.waiting_fingerprint();
        snapshot.requests[0].fields[0].initial = "EDITED_ANSWER".into();
        snapshot.requests[0].fields[1].initial = "EDITED_SECRET".into();
        assert_eq!(snapshot.waiting_fingerprint(), original);
        snapshot.requests[0].fields[1].label = "EDITED_SECRET_LABEL".into();
        let changed_secret_prompt = snapshot.waiting_fingerprint();
        assert_ne!(changed_secret_prompt, original);
        assert!(
            !snapshot
                .notification_content(Kind::Waiting)
                .preview
                .contains("SECRET_LABEL")
        );
        snapshot.requests[0].fields[0].choices[0].label = "Keep revised option".into();
        assert_ne!(snapshot.waiting_fingerprint(), changed_secret_prompt);
        snapshot.requests[0].fields[0].label = "λ".repeat(RETAINED_BYTES * 2);
        let content = snapshot.notification_content(Kind::Waiting);
        assert!(content.preview.chars().count() <= 240);
        assert!(!content.preview.chars().any(char::is_control));
    }

    #[test]
    fn notification_topic_fallback_is_bounded_and_fenced_to_live_messages() {
        let mut state = Snapshot::default();
        let message = |text: &str| {
            Event::Item(super::super::Item {
                id: "current-user".into(),
                role: Role::User,
                at: None,
                started_at: None,
                content: ItemContent::Text {
                    text: text.into(),
                    thinking: false,
                    phase: Phase::Final,
                },
            })
        };
        Arc::make_mut(&mut state.notification_preview).observe(&message("Repair `config_file.rs`"));
        assert_eq!(
            state.notification_content(Kind::Waiting).conversation,
            "Repair config_file.rs"
        );
        state.title = Some("Provider conversation name".into());
        assert_eq!(
            state.notification_content(Kind::Succeeded).conversation,
            "Provider conversation name"
        );
        state.title = None;
        Arc::make_mut(&mut state.notification_preview).observe(&Event::TurnStarted);
        assert_eq!(
            state.notification_content(Kind::Waiting).conversation,
            "Repair config_file.rs"
        );
        Arc::make_mut(&mut state.notification_preview).observe(&message("Review the next change"));
        assert_eq!(
            state.notification_content(Kind::Waiting).conversation,
            "Review the next change"
        );
        Arc::make_mut(&mut state.notification_preview).observe(&message(&"🦀".repeat(100_000)));
        assert!(
            state
                .notification_content(Kind::Succeeded)
                .conversation
                .chars()
                .count()
                <= presentation::TITLE_LIMIT
        );
        Arc::make_mut(&mut state.notification_preview).observe(&Event::Reset {
            reason: "Fixture new conversation".into(),
        });
        assert!(
            state
                .notification_content(Kind::Succeeded)
                .conversation
                .is_empty()
        );
    }

    #[test]
    fn positioned_live_items_keep_notification_topic_and_response_context() {
        let mut state = Snapshot::default();
        let item = |id: &str, role, body: &str| super::super::Item {
            id: id.into(),
            role,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Phase::Final,
            },
        };
        let preview = Arc::make_mut(&mut state.notification_preview);
        preview.observe(&Event::TextDelta {
            id: "early-answer".into(),
            role: Role::Assistant,
            thinking: false,
            delta: "Checking".into(),
        });
        preview.observe(&Event::ItemBefore {
            item: item("prompt", Role::User, "Check the saved state"),
            before: "early-answer".into(),
        });
        preview.observe(&Event::ItemAfter {
            item: item("answer", Role::Assistant, "Recovered response"),
            after: "prompt".into(),
        });
        let content = state.notification_content(Kind::Succeeded);
        assert_eq!(content.conversation, "Check the saved state");
        assert_eq!(content.preview, "Recovered response");
    }

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
    fn immutable_frame_context_preserves_final_excerpt_across_fatal_error_ordering() {
        let final_item = Event::Item(super::super::Item {
            id: "answer".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: "Exact final response".into(),
                thinking: false,
                phase: Phase::Final,
            },
        });
        let finished = Event::TurnFinished {
            key: Some("turn".into()),
            outcome: Kind::Succeeded,
        };
        let fatal = Event::Error {
            message: "Actual provider failure".into(),
            fatal: true,
        };
        for events in [
            vec![fatal.clone(), final_item.clone(), finished.clone()],
            vec![finished, fatal, final_item],
        ] {
            let contexts = FrameContext::capture(&Snapshot::default()).project(&events);
            assert_eq!(contexts.len(), 1);
            assert_eq!(contexts[0].preview, "Exact final response");
        }
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
