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
                ItemContent::Text { text, thinking: false, phase } => {
                    if self.final_response && *phase != Phase::Final { return; }
                    self.id = bounded(&item.id, 1024);
                    self.text = bounded(text, RETAINED_BYTES);
                    self.final_response = *phase == Phase::Final;
                }
                ItemContent::Command { label, status, .. } => {
                    let label = presentation::preview(label);
                    self.fallback = format!("{}{}", tool_verb(*status, "Command"), if label.is_empty() { String::new() } else { format!(": {label}") });
                }
                ItemContent::Tool { name, status, .. } => {
                    self.fallback = format!("{}: {}", tool_verb(*status, "Tool"), presentation::title(name));
                }
                _ => {}
            },
            Event::TextDelta { id, role: Role::Assistant, thinking: false, delta } => {
                if id != &self.id {
                    if self.final_response { return; }
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
    format!("{noun} {}", match status {
        ToolStatus::Running => "activity",
        ToolStatus::Succeeded => "completed",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied => "denied",
        ToolStatus::Stopped => "stopped",
    })
}

fn bounded(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) { end -= 1; }
    text[..end].to_owned()
}

impl Snapshot {
    pub(crate) fn notification_content(&self, kind: Kind) -> Content {
        let mut content = Content {
            conversation: presentation::title(self.title.as_deref().unwrap_or_default()),
            preview: String::new(), request: None,
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
                if content.preview.is_empty() { content.preview = presentation::preview(&request.title); }
            }
        } else if matches!(kind, Kind::Failed | Kind::Interrupted) {
            content.preview = presentation::preview(self.error.as_deref().unwrap_or_default());
        }
        if !matches!(kind, Kind::Failed | Kind::Interrupted) {
            if content.preview.is_empty() { content.preview = presentation::preview(&self.notification_preview.text); }
            if content.preview.is_empty() { content.preview = self.notification_preview.fallback.clone(); }
        }
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
        state.notification_preview.observe(&Event::TurnStarted);
        state.notification_preview.observe(&Event::Item(super::super::Item {
            id: "answer".into(), role: Role::Assistant, at: None, started_at: None,
            content: ItemContent::Text { text: "**Done.**\n```sh\nsecret tool output\n```\nAll tests passed.".into(), thinking: false, phase: Phase::Final },
        }));
        state.items = vec![Arc::new(super::super::Item {
            id: "old".into(), role: Role::Assistant, at: None, started_at: None,
            content: ItemContent::Text { text: "Old response must not notify".into(), thinking: false, phase: Phase::Final },
        })];
        assert_eq!(state.notification_content(Kind::Succeeded).preview, "Done. All tests passed.");
        state.notification_preview.observe(&Event::TurnStarted);
        assert!(state.notification_content(Kind::Succeeded).preview.is_empty());
    }

    #[test]
    fn streaming_preview_is_bounded_before_retention() {
        let mut state = Snapshot::default();
        for _ in 0..100 {
            state.notification_preview.observe(&Event::TextDelta { id: "answer".into(), role: Role::Assistant, thinking: false, delta: "🦀".repeat(10000) });
        }
        assert!(state.notification_preview.text.len() <= RETAINED_BYTES);
        assert!(state.notification_content(Kind::Succeeded).preview.chars().count() <= presentation::PREVIEW_LIMIT);
    }
}
