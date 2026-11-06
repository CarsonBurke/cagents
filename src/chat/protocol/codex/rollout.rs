//! A Codex conversation read from its saved rollout, without an app-server.
//!
//! Codex records each completed turn item as an `item_completed` event in the
//! thread's rollout. Those items, unlike the raw model input next to them,
//! hold only what the person and the agent actually exchanged: harness
//! instructions and environment context never appear among them. Rollouts
//! written before Codex recorded turn items still carry the user and agent
//! messages as events of their own.

use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
};

use serde_json::Value;

use crate::chat::{
    ChangeKind, CommandAction, DeliveryKind, FileChange, Item, ItemContent, Phase, Role, ToolStatus,
};

/// Rollouts are append-only JSON lines; a conversation this large is refused
/// rather than read into memory.
const MAX_ROLLOUT: u64 = 256 * 1024 * 1024;

/// Every item in the rollout at `path`, oldest first.
pub(crate) fn read(path: &Path) -> Result<Vec<Item>, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    crate::secure_file::reject_symlink_components(path).map_err(|error| error.to_string())?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("Cannot open the Codex rollout: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ROLLOUT {
        return Err("The Codex rollout must be a regular file smaller than 256 MiB".into());
    }
    Ok(parse(file.take(MAX_ROLLOUT)))
}

/// Parses rollout lines. A line Codex is still writing, or one this version
/// does not understand, is skipped rather than failing the conversation.
pub(crate) fn parse(reader: impl Read) -> Vec<Item> {
    let mut records = Vec::new();
    let mut turn_items = false;
    for (line, text) in BufReader::new(reader).split(b'\n').enumerate() {
        let Ok(text) = text else { break };
        let Ok(record) = serde_json::from_slice::<Value>(&text) else {
            continue;
        };
        turn_items |=
            record["type"] == "event_msg" && record["payload"]["type"] == "item_completed";
        records.push((line, record));
    }
    let mut items = Vec::new();
    for (line, record) in &records {
        let at = record["timestamp"]
            .as_str()
            .and_then(|time| time.parse::<jiff::Timestamp>().ok())
            .and_then(|time| u64::try_from(time.as_millisecond()).ok());
        let payload = &record["payload"];
        let item = match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("item_completed")) => turn_item(&payload["item"]),
            (Some("event_msg"), Some("turn_aborted")) => Some((
                Role::System,
                notice(
                    "Turn interrupted",
                    payload["reason"].as_str().unwrap_or_default(),
                ),
            )),
            (Some("event_msg"), Some("error")) => Some((
                Role::System,
                notice(
                    "Codex error",
                    payload["message"].as_str().unwrap_or_default(),
                ),
            )),
            // Rollouts from before turn items carry their messages as events.
            (Some("event_msg"), Some("user_message")) if !turn_items => {
                text(payload["message"].as_str().unwrap_or_default())
                    .map(|content| (Role::User, content))
            }
            (Some("event_msg"), Some("agent_message")) if !turn_items => {
                text(payload["message"].as_str().unwrap_or_default())
                    .map(|content| (Role::Assistant, content))
            }
            (Some("response_item"), Some("function_call" | "custom_tool_call")) if !turn_items => {
                Some((
                    Role::Assistant,
                    ItemContent::Tool {
                        name: payload["name"].as_str().unwrap_or("tool").into(),
                        input: value_text(payload.get("arguments").unwrap_or(&payload["input"])),
                        output: String::new(),
                        status: ToolStatus::Succeeded,
                    },
                ))
            }
            (Some("compacted"), _) if !turn_items => Some((
                Role::System,
                notice(
                    "Context compacted",
                    payload["message"].as_str().unwrap_or_default(),
                ),
            )),
            _ => None,
        };
        let Some((role, content)) = item else {
            continue;
        };
        let id = payload["item"]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map_or_else(|| format!("line-{line}"), str::to_owned);
        items.push(Item {
            started_at: None,
            id,
            role,
            content,
            at,
        });
    }
    items
}

fn turn_item(item: &Value) -> Option<(Role, ItemContent)> {
    let status = || match item["status"].as_str() {
        Some("failed") => ToolStatus::Failed,
        Some("declined") => ToolStatus::Denied,
        Some("in_progress" | "inProgress") => ToolStatus::Running,
        _ => ToolStatus::Succeeded,
    };
    let content = match item["type"].as_str()? {
        "UserMessage" => return text(&content_text(&item["content"])).map(|c| (Role::User, c)),
        "AgentMessage" => {
            let text = content_text(&item["content"]);
            if text.trim().is_empty() {
                return None;
            }
            return Some((
                Role::Assistant,
                ItemContent::Text {
                    text,
                    thinking: false,
                    phase: if item["phase"] == "commentary" {
                        Phase::Commentary
                    } else {
                        Phase::Final
                    },
                },
            ));
        }
        "Reasoning" => return None,
        "CommandExecution" => ItemContent::Command {
            label: String::new(),
            command: command_line(&item["command"]),
            working_directory: item["cwd"]
                .as_str()
                .map(|cwd| cwd.strip_prefix("file://").unwrap_or(cwd).to_owned())
                .unwrap_or_default(),
            action: CommandAction::Run,
            status: status(),
            exit_code: item["exit_code"]
                .as_i64()
                .and_then(|code| i32::try_from(code).ok()),
            duration_ms: None,
            output: item["aggregated_output"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        },
        "FileChange" => ItemContent::Changes {
            changes: item["changes"]
                .as_object()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|(path, change)| FileChange {
                            path: path.clone(),
                            kind: match change["type"].as_str() {
                                Some("add") => ChangeKind::Added,
                                Some("delete") => ChangeKind::Deleted,
                                _ => ChangeKind::Updated,
                            },
                            // A compact transcript names the paths; the diff
                            // stays in the provider's history.
                            diff: String::new(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            status: status(),
        },
        "McpToolCall" => ItemContent::Tool {
            name: item["tool"].as_str().unwrap_or("tool").into(),
            input: value_text(&item["arguments"]),
            output: content_text(&item["result"]["content"]),
            status: if item["result"]["isError"] == true {
                ToolStatus::Failed
            } else {
                status()
            },
        },
        "CollabAgentToolCall" => ItemContent::Tool {
            name: "Agent".into(),
            input: item["tool"].as_str().unwrap_or_default().into(),
            output: String::new(),
            status: status(),
        },
        "SubAgentActivity" if item["kind"] == "started" => ItemContent::Tool {
            name: "Agent".into(),
            input: item["agent_path"].as_str().unwrap_or_default().into(),
            output: String::new(),
            status: ToolStatus::Succeeded,
        },
        "SubAgentActivity" => return None,
        "WebSearch" => ItemContent::WebSearch {
            query: item["query"].as_str().unwrap_or_default().into(),
            results: Vec::new(),
        },
        "ImageView" => ItemContent::Image {
            path: item["path"].as_str().unwrap_or_default().into(),
            description: String::new(),
            reference: None,
        },
        "ContextCompaction" => return Some((Role::System, notice("Context compacted", ""))),
        "Extension" => ItemContent::Tool {
            name: item["kind"].as_str().unwrap_or("extension").into(),
            input: String::new(),
            output: String::new(),
            status: status(),
        },
        // An item this version does not know still shows that something ran.
        other => ItemContent::Tool {
            name: other.into(),
            input: String::new(),
            output: String::new(),
            status: status(),
        },
    };
    Some((Role::Assistant, content))
}

fn text(text: &str) -> Option<ItemContent> {
    (!text.trim().is_empty()).then(|| ItemContent::Text {
        text: text.into(),
        thinking: false,
        phase: Phase::Final,
    })
}

fn notice(summary: &str, body: &str) -> ItemContent {
    ItemContent::Delivery {
        kind: DeliveryKind::Notice,
        from: String::new(),
        sender: String::new(),
        summary: summary.into(),
        body: body.into(),
    }
}

/// Text parts of a content array, whatever their casing; images are marked.
fn content_text(content: &Value) -> String {
    let Some(parts) = content.as_array() else {
        return content.as_str().unwrap_or_default().to_owned();
    };
    parts
        .iter()
        .filter_map(|part| match part["type"].as_str() {
            Some("Text" | "text" | "input_text" | "output_text") => {
                part["text"].as_str().map(str::to_owned)
            }
            Some("Image" | "image" | "LocalImage" | "localImage" | "input_image") => {
                Some("[image]".into())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The script a shell wrapper runs, rather than the wrapper itself.
fn command_line(command: &Value) -> String {
    let Some(argv) = command.as_array() else {
        return command.as_str().unwrap_or_default().to_owned();
    };
    let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
    match argv.as_slice() {
        [_, flag, script] if matches!(*flag, "-lc" | "-c") => (*script).to_owned(),
        _ => argv.join(" "),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(records: &[Value]) -> String {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn completed(item: Value) -> Value {
        json!({"timestamp":"2026-11-05T19:31:01.500Z","type":"event_msg","payload":{"type":"item_completed","item":item}})
    }

    #[test]
    fn turn_items_hold_the_conversation_without_harness_context() {
        let rollout = lines(&[
            json!({"type":"session_meta","payload":{"id":"thread"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}),
            completed(
                json!({"type":"UserMessage","id":"u1","content":[{"type":"Text","text":"Fix the login test"}]}),
            ),
            completed(json!({"type":"Reasoning","id":"r1","summary_text":["secret plan"]})),
            completed(
                json!({"type":"CommandExecution","id":"c1","command":["/usr/bin/bash","-lc","cargo test login"],"cwd":"file:///repo","status":"completed","exit_code":1,"aggregated_output":"1 failed"}),
            ),
            completed(
                json!({"type":"FileChange","id":"f1","changes":{"/repo/src/auth.rs":{"type":"update","unified_diff":"-a\n+b"}},"status":"completed"}),
            ),
            completed(json!({"type":"ContextCompaction","id":"k1"})),
            completed(
                json!({"type":"AgentMessage","id":"a1","content":[{"type":"Text","text":"Checking the test"}],"phase":"commentary"}),
            ),
            completed(
                json!({"type":"AgentMessage","id":"a2","content":[{"type":"Text","text":"Fixed."}],"phase":"final_answer"}),
            ),
            json!({"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}),
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_comp".into(),
        ]);
        let items = parse(rollout.as_bytes());
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["u1", "c1", "f1", "k1", "a1", "a2", "line-10"]);
        assert!(
            matches!(&items[0].content, ItemContent::Text { text, .. } if text == "Fix the login test")
        );
        assert_eq!(items[0].role, Role::User);
        assert_eq!(items[0].at, Some(1_793_907_061_500));
        assert!(matches!(
            &items[1].content,
            ItemContent::Command { command, exit_code: Some(1), working_directory, .. }
                if command == "cargo test login" && working_directory == "/repo"
        ));
        assert!(
            matches!(&items[2].content, ItemContent::Changes { changes, .. } if changes[0].path == "/repo/src/auth.rs")
        );
        assert!(
            matches!(&items[3].content, ItemContent::Delivery { summary, .. } if summary == "Context compacted")
        );
        assert!(matches!(
            &items[4].content,
            ItemContent::Text {
                phase: Phase::Commentary,
                ..
            }
        ));
        assert!(matches!(
            &items[5].content,
            ItemContent::Text {
                phase: Phase::Final,
                ..
            }
        ));
        let serialized = serde_json::to_string(&items).unwrap();
        for hidden in ["AGENTS.md", "permissions", "secret plan"] {
            assert!(!serialized.contains(hidden), "{hidden} leaked");
        }
    }

    #[test]
    fn rollouts_without_turn_items_read_their_message_events() {
        let rollout = lines(&[
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>"}]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Deploy it"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"Deployed."}}),
            json!({"type":"compacted","payload":{"message":"Summary so far"}}),
        ]);
        let items = parse(rollout.as_bytes());
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[0].content, ItemContent::Text { text, .. } if text == "Deploy it"));
        assert!(matches!(&items[1].content, ItemContent::Tool { name, .. } if name == "shell"));
        assert_eq!(items[2].role, Role::Assistant);
        assert!(
            matches!(&items[3].content, ItemContent::Delivery { body, .. } if body == "Summary so far")
        );
        assert!(items.iter().all(|item| item.id.starts_with("line-")));
    }
}
