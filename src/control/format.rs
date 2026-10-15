//! One plain table style for interactive CLI output.
use serde::Serialize;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

pub(super) fn table(headers: &[&str], rows: Vec<Vec<String>>, empty: &str) -> String {
    if rows.is_empty() { return empty.into(); }
    let clean = |text: &str| text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>();
    let rows = rows.into_iter().map(|row| row.into_iter().map(|value| clean(&value)).collect::<Vec<_>>()).collect::<Vec<_>>();
    let widths = headers.iter().enumerate().map(|(index, title)| rows.iter().filter_map(|row| row.get(index))
        .map(|value| value.width()).max().unwrap_or_default().max(title.width())).collect::<Vec<_>>();
    let line = |row: &[String]| row.iter().enumerate().map(|(index, value)| {
        if index + 1 == headers.len() { value.clone() } else {
            format!("{value}{}", " ".repeat(widths.get(index).copied().unwrap_or_default().saturating_sub(value.width()) + 2))
        }
    }).collect::<String>();
    std::iter::once(line(&headers.iter().map(|v| (*v).into()).collect::<Vec<_>>()))
        .chain(rows.iter().map(|row| line(row))).collect::<Vec<_>>().join("\n")
}

pub(crate) fn label(value: impl Serialize) -> String {
    match serde_json::to_value(value).unwrap_or(Value::Null) {
        Value::String(value) => snake(&value).replace('_', " "),
        Value::Null => "unknown".into(),
        _ => "unknown".into(),
    }
}

fn snake(text: &str) -> String {
    let mut result = String::new();
    for (index, c) in text.chars().enumerate() {
        if c.is_uppercase() && index > 0 { result.push('_'); }
        result.extend(c.to_lowercase());
    }
    result
}

pub(crate) fn relative_time(at: u64) -> String {
    relative_at(at, crate::chat::now_ms().unwrap_or_default())
}
fn relative_at(at: u64, now: u64) -> String {
    let future = at > now;
    let seconds = at.abs_diff(now) / 1000;
    let (amount, unit) = match seconds { 0..=59 => (seconds, "sec"), 60..=3599 => (seconds / 60, "min"), 3600..=86399 => (seconds / 3600, "hr"), _ => (seconds / 86400, "days") };
    let unit = if unit == "days" && amount == 1 { "day" } else { unit };
    if future { format!("in {amount} {unit}") } else { format!("{amount} {unit} ago") }
}

// Normalize the CLI JSON boundary without changing saved data or binary IPC.
// Provider payloads, prompt/result text and user labels remain opaque.
pub(super) fn normalize(value: &mut Value) {
    fn walk(value: &mut Value, field: &str) {
        match value {
            Value::Array(values) => for value in values { walk(value, field); },
            Value::Object(values) => {
                // These fields contain externally tagged application enums. A
                // user's environment and provider arguments are opaque maps.
                if values.len() == 1 && matches!(field, "content" | "origin" | "session" | "placement" | "view_scope" | "kind" | "scope" | "owner") {
                    let key = values.keys().next().unwrap().clone();
                    if matches!(key.as_str(), "Text" | "Command" | "Tool" | "Changes" | "Image" | "WebSearch" | "Delivery" | "Child" | "Automation" | "Chat" | "Terminal" | "Channel" | "Worktree" | "Renamed" | "Session" | "Containing" | "Project") {
                        let value = values.remove(&key).unwrap();
                        values.insert(snake(&key), value);
                    }
                }
                for (key, value) in values {
                    if !matches!(key.as_str(), "environment" | "unset_environment" | "arguments") { walk(value, key); }
                }
            },
            Value::String(text) if matches!(field, "provider" | "surface" | "state" | "run_state" | "role" | "relation" | "phase" | "status" | "origin" | "action" | "kind" | "scope" | "owner" | "source") => {
                if matches!(text.as_str(), "Omp" | "Claude" | "Codex" | "Chat" | "Terminal" | "Unknown" | "Idle" | "Working" | "Accepted" | "Running" | "Waiting" | "Succeeded" | "Failed" | "Cancelled" | "Interrupted" | "User" | "Assistant" | "System" | "Myself" | "Parent" | "Child" | "Peer" | "Final" | "Commentary" | "Denied" | "Stopped" | "Run" | "Read" | "Search" | "List" | "Added" | "Deleted" | "Updated" | "Report" | "Message" | "Notice" | "All" | "Ok" | "Warning" | "Limited" | "NeedsLogin" | "Unavailable" | "Live" | "Probe" | "Cache" | "Recorded") { *text = snake(text); }
            }
            _ => {}
        }
    }
    walk(value, "");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_snapshot_includes_headers_alignment_and_empty_state() {
        assert_eq!(table(&["ID", "STATE"], vec![vec!["a".into(), "running".into()],vec!["abc".into(), "exited(0)".into()]], "No terminals"), "ID   STATE\na    running\nabc  exited(0)");
        assert_eq!(table(&["ID"], vec![], "No terminals"), "No terminals");
        assert_eq!(relative_at(0, 180_000), "3 min ago");
        assert_eq!(table(&["NAME", "STATE"], vec![vec!["界".into(), "idle".into()], vec!["wide".into(), "idle".into()]], "No sessions"), "NAME  STATE\n界    idle\nwide  idle");
    }
    #[test]
    fn cli_enum_normalization_preserves_free_form_text() {
        let mut value = serde_json::json!({"provider":"Claude", "spec":{"surface":"Chat"},"run":{"state":"Succeeded","result":{"text":"Claude"},"environment":{"provider":"Claude","state":"Running"}}});
        normalize(&mut value);
        assert_eq!(value["provider"], "claude"); assert_eq!(value["spec"]["surface"], "chat");
        assert_eq!(value["run"]["state"], "succeeded"); assert_eq!(value["run"]["result"]["text"], "Claude");
        assert_eq!(value["run"]["environment"]["provider"], "Claude");
        let mut transcript = serde_json::json!({"content":{"Text":{"text":"Claude", "phase":"Final", "thinking":false}}, "role":"Assistant"});
        normalize(&mut transcript);
        assert_eq!(transcript["content"]["text"]["phase"], "final");
        assert_eq!(transcript["role"], "assistant");
        let content: crate::chat::ItemContent = serde_json::from_value(transcript["content"].clone()).unwrap();
        assert!(matches!(content, crate::chat::ItemContent::Text { phase: crate::chat::Phase::Final, .. }));
        for kind in [crate::chat::DeliveryKind::Report, crate::chat::DeliveryKind::Message, crate::chat::DeliveryKind::Notice] {
            let content = crate::chat::ItemContent::Delivery { kind, from: "reviewer".into(), sender: "fixture".into(), summary: "Report".into(), body: "Report".into() };
            let mut page = serde_json::json!({"content": content});
            normalize(&mut page);
            assert_eq!(page["content"]["delivery"]["kind"], label(kind));
            assert_eq!(page["content"]["delivery"]["body"], "Report");
            assert_eq!(serde_json::from_value::<crate::chat::ItemContent>(page["content"].clone()).unwrap(), content);
        }
        for provider in ["Claude", "claude"] {
            assert_eq!(serde_json::from_value::<crate::agent::Provider>(serde_json::json!(provider)).unwrap(), crate::agent::Provider::Claude);
        }
    }
}
