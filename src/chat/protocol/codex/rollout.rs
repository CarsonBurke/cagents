//! A Codex conversation read from its saved rollout, without an app-server.
//!
//! The rollout's `response_item` messages are the conversation as the model
//! saw it, including history a forked or resumed thread replays before its
//! first turn. Codex's own harness context travels among the user messages
//! and is left out. Tool calls come from the richer `item_completed` turn
//! items, which carry exit codes and edited paths; rollouts written before
//! Codex recorded turn items fall back to the raw calls.

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
/// Tool output kept per call. A transcript clips it far shorter unless asked
/// for whole output, and this bounds what a long session holds in memory.
const TOOL_OUTPUT: usize = 64 * 1024;
/// Openings of the user-role messages Codex itself injects: project
/// instructions, environment, skill bodies and app-page context.
const HARNESS_CONTEXT: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<user_instructions>",
    "<skill>",
    "<turn_aborted>",
    "<subagent_notification>",
    "<external_codex_apps_open_page>",
];

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

/// Parses rollout lines in one pass. A line Codex is still writing, or one
/// this version does not understand, is skipped rather than failing the
/// conversation.
pub(crate) fn parse(reader: impl Read) -> Vec<Item> {
    let mut items = Vec::new();
    // Raw tool calls, kept with their place in `items` until the rollout
    // proves to have no turn items to supersede them.
    let mut raw_tools = Vec::new();
    let mut turn_items = false;
    for (line, text) in BufReader::new(reader).split(b'\n').enumerate() {
        let Ok(text) = text else { break };
        let Ok(record) = serde_json::from_slice::<Value>(&text) else {
            continue;
        };
        let at = record["timestamp"]
            .as_str()
            .and_then(|time| time.parse::<jiff::Timestamp>().ok())
            .and_then(|time| u64::try_from(time.as_millisecond()).ok());
        let payload = &record["payload"];
        let line_id = || format!("line-{line}");
        let item = |id: String, (role, content): (Role, ItemContent)| Item {
            id,
            role,
            content,
            started_at: None,
            at,
        };
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("response_item"), Some("message")) => {
                if let Some(entry) = message(payload) {
                    let id = payload["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map_or_else(line_id, str::to_owned);
                    items.push(item(id, entry));
                }
            }
            (Some("response_item"), Some("agent_message")) => {
                if let Some(entry) = agent_message(payload) {
                    let id = payload["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map_or_else(line_id, str::to_owned);
                    items.push(item(id, entry));
                }
            }
            (Some("response_item"), Some("function_call" | "custom_tool_call")) if !turn_items => {
                let call = (
                    Role::Assistant,
                    ItemContent::Tool {
                        name: payload["name"].as_str().unwrap_or("tool").into(),
                        input: value_text(payload.get("arguments").unwrap_or(&payload["input"])),
                        output: String::new(),
                        status: ToolStatus::Succeeded,
                    },
                );
                raw_tools.push((items.len(), item(line_id(), call)));
            }
            (Some("event_msg"), Some("item_completed")) => {
                turn_items = true;
                if let Some(entry) = turn_item(&payload["item"]) {
                    let id = payload["item"]["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map_or_else(line_id, str::to_owned);
                    items.push(item(id, entry));
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => items.push(item(
                line_id(),
                (
                    Role::System,
                    notice(
                        "Turn interrupted",
                        payload["reason"].as_str().unwrap_or_default(),
                    ),
                ),
            )),
            (Some("event_msg"), Some("error")) => items.push(item(
                line_id(),
                (
                    Role::System,
                    notice(
                        "Codex error",
                        payload["message"].as_str().unwrap_or_default(),
                    ),
                ),
            )),
            // Modern summaries are encrypted for the model; older ones are text.
            (Some("compacted"), _) => items.push(item(
                line_id(),
                (
                    Role::System,
                    notice(
                        "Context compacted",
                        payload["message"].as_str().unwrap_or_default(),
                    ),
                ),
            )),
            _ => {}
        }
    }
    if !turn_items && !raw_tools.is_empty() {
        let mut merged = Vec::with_capacity(items.len() + raw_tools.len());
        let mut raw_tools = raw_tools.into_iter().peekable();
        for (index, item) in items.into_iter().enumerate() {
            while let Some((_, tool)) = raw_tools.next_if(|(at, _)| *at == index) {
                merged.push(tool);
            }
            merged.push(item);
        }
        merged.extend(raw_tools.map(|(_, tool)| tool));
        items = merged;
    }
    items
}

/// A person's or the agent's message. Developer instructions and the
/// context Codex injects as user messages are not part of the conversation.
fn message(payload: &Value) -> Option<(Role, ItemContent)> {
    let role = match payload["role"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let text = content_text(&payload["content"]);
    if role == Role::User
        && HARNESS_CONTEXT
            .iter()
            .any(|opening| text.trim_start().starts_with(opening))
    {
        return None;
    }
    if text.trim().is_empty() {
        return None;
    }
    Some((
        role,
        ItemContent::Text {
            text,
            thinking: false,
            phase: if payload["phase"] == "commentary" {
                Phase::Commentary
            } else {
                Phase::Final
            },
        },
    ))
}

/// A message between a Codex thread and its own subagents. Its text opens
/// with a header naming the kind and sender; a payload Codex encrypted for
/// the model has no readable body.
fn agent_message(payload: &Value) -> Option<(Role, ItemContent)> {
    let text = content_text(&payload["content"]);
    let (header, body) = text.split_once("Payload:\n").unwrap_or(("", text.as_str()));
    let kind = if header.contains("Message Type: FINAL_ANSWER") {
        DeliveryKind::Report
    } else {
        DeliveryKind::Message
    };
    let author = payload["author"].as_str().unwrap_or_default();
    let from = author.rsplit('/').next().unwrap_or(author);
    Some((
        Role::User,
        ItemContent::Delivery {
            kind,
            from: if from.is_empty() { "a subagent" } else { from }.into(),
            sender: String::new(),
            summary: String::new(),
            body: body.trim().to_owned(),
        },
    ))
}

fn turn_item(item: &Value) -> Option<(Role, ItemContent)> {
    let status = || match item["status"].as_str() {
        Some("failed") => ToolStatus::Failed,
        Some("declined") => ToolStatus::Denied,
        Some("in_progress" | "inProgress") => ToolStatus::Running,
        _ => ToolStatus::Succeeded,
    };
    let content = match item["type"].as_str()? {
        // Messages are read from the response items, which also hold the
        // history a forked thread replays.
        "UserMessage" | "AgentMessage" => return None,
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
            output: bounded(item["aggregated_output"].as_str().unwrap_or_default()),
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
            output: bounded(&content_text(&item["result"]["content"])),
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
        // The `compacted` record that accompanies it carries any summary.
        "ContextCompaction" => return None,
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

fn notice(summary: &str, body: &str) -> ItemContent {
    ItemContent::Delivery {
        kind: DeliveryKind::Notice,
        from: String::new(),
        sender: String::new(),
        summary: summary.into(),
        body: body.into(),
    }
}

/// Text parts of a content array, whatever their casing. An image is marked,
/// unless the text just before it already labels it, as `[Image 1]` does.
fn content_text(content: &Value) -> String {
    let Some(parts) = content.as_array() else {
        return content.as_str().unwrap_or_default().to_owned();
    };
    let mut texts: Vec<String> = Vec::new();
    let mut labelled = false;
    for part in parts {
        match part["type"].as_str() {
            Some("Text" | "text" | "input_text" | "output_text") => {
                let Some(text) = part["text"].as_str() else {
                    continue;
                };
                labelled = is_image_label(text);
                texts.push(text.to_owned());
            }
            Some("Image" | "image" | "LocalImage" | "localImage" | "input_image") => {
                if !std::mem::take(&mut labelled) {
                    texts.push("[image]".into());
                }
            }
            _ => labelled = false,
        }
    }
    texts.join("\n")
}

fn is_image_label(text: &str) -> bool {
    text.strip_prefix("[Image ")
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn bounded(text: &str) -> String {
    let mut text = text.to_owned();
    crate::agent_execution::truncate(&mut text, TOOL_OUTPUT);
    text
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

    fn message(role: &str, text: &str) -> Value {
        json!({"timestamp":"2026-11-05T19:31:01.500Z","type":"response_item","payload":{"type":"message","role":role,"content":[{"type":if role == "assistant" {"output_text"} else {"input_text"},"text":text}]}})
    }

    #[test]
    fn messages_come_from_response_items_and_tools_from_turn_items() {
        let rollout = lines(&[
            json!({"type":"session_meta","payload":{"id":"thread"}}),
            message("developer", "<permissions instructions>"),
            message("user", "# AGENTS.md instructions for /repo"),
            message("user", "<environment_context>\n  <cwd>/repo</cwd>"),
            // A forked thread replays its parent's turns before its own.
            message("user", "Replayed request"),
            json!({"type":"response_item","payload":{"type":"message","id":"msg_r","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Replayed answer"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
            message("user", "Fix the login test"),
            completed(
                json!({"type":"UserMessage","id":"u1","content":[{"type":"Text","text":"Fix the login test"}]}),
            ),
            json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"text":"secret plan"}]}}),
            completed(json!({"type":"Reasoning","id":"r1","summary_text":["secret plan"]})),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"text(await tools.exec_command({cmd:`cargo test`}))"}}),
            completed(
                json!({"type":"CommandExecution","id":"c1","command":["/usr/bin/bash","-lc","cargo test login"],"cwd":"file:///repo","status":"completed","exit_code":1,"aggregated_output":"1 failed"}),
            ),
            completed(
                json!({"type":"FileChange","id":"f1","changes":{"/repo/src/auth.rs":{"type":"update","unified_diff":"-a\n+b"}},"status":"completed"}),
            ),
            json!({"type":"response_item","payload":{"type":"message","id":"msg_c","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Checking the test"}]}}),
            completed(
                json!({"type":"AgentMessage","id":"a1","content":[{"type":"Text","text":"Checking the test"}],"phase":"commentary"}),
            ),
            json!({"type":"response_item","payload":{"type":"agent_message","id":"amsg_1","author":"/root/review","recipient":"/root","content":[{"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/review\nPayload:\nLooks right."}]}}),
            completed(json!({"type":"ContextCompaction","id":"k1"})),
            json!({"type":"compacted","payload":{"message":"","replacement_history":[]}}),
            message("user", "[Image 1]"),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"See this"},{"type":"input_text","text":"[Image 1]"},{"type":"input_image","image_url":"data:"}]}}),
            json!({"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}),
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_comp".into(),
        ]);
        let items = parse(rollout.as_bytes());
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "line-4", "msg_r", "line-7", "c1", "f1", "msg_c", "amsg_1", "line-18", "line-19",
                "line-20", "line-21"
            ]
        );
        let text = |index: usize| match &items[index].content {
            ItemContent::Text { text, .. } => text.as_str(),
            other => panic!("{other:?}"),
        };
        assert_eq!((items[0].role, text(0)), (Role::User, "Replayed request"));
        assert_eq!(items[0].at, Some(1_793_907_061_500));
        assert_eq!(
            (items[1].role, text(1)),
            (Role::Assistant, "Replayed answer")
        );
        assert_eq!(text(2), "Fix the login test");
        assert!(matches!(
            &items[3].content,
            ItemContent::Command { command, exit_code: Some(1), working_directory, .. }
                if command == "cargo test login" && working_directory == "/repo"
        ));
        assert!(
            matches!(&items[4].content, ItemContent::Changes { changes, .. } if changes[0].path == "/repo/src/auth.rs")
        );
        assert!(matches!(
            &items[5].content,
            ItemContent::Text {
                phase: Phase::Commentary,
                ..
            }
        ));
        assert!(matches!(
            &items[6].content,
            ItemContent::Delivery { kind: DeliveryKind::Report, from, body, .. }
                if from == "review" && body == "Looks right."
        ));
        assert!(
            matches!(&items[7].content, ItemContent::Delivery { summary, .. } if summary == "Context compacted")
        );
        assert_eq!(text(8), "[Image 1]", "a lone label is the person's text");
        assert_eq!(
            text(9),
            "See this\n[Image 1]",
            "a labelled image is not marked twice"
        );
        assert!(
            matches!(&items[10].content, ItemContent::Delivery { summary, .. } if summary == "Turn interrupted")
        );
        let serialized = serde_json::to_string(&items).unwrap();
        for hidden in [
            "AGENTS.md",
            "permissions",
            "environment_context",
            "secret plan",
            "exec_command",
        ] {
            assert!(!serialized.contains(hidden), "{hidden} leaked");
        }
    }

    #[test]
    fn rollouts_without_turn_items_keep_their_raw_tool_calls_in_place() {
        let rollout = lines(&[
            message("user", "<environment_context>"),
            message("user", "Deploy it"),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Deploy it"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}"}}),
            message("assistant", "Deployed."),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"Deployed."}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}"}}),
            json!({"type":"compacted","payload":{"message":"Summary so far"}}),
        ]);
        let items = parse(rollout.as_bytes());
        assert_eq!(items.len(), 5, "{items:#?}");
        assert!(matches!(&items[0].content, ItemContent::Text { text, .. } if text == "Deploy it"));
        assert!(matches!(&items[1].content, ItemContent::Tool { name, .. } if name == "shell"));
        assert!(matches!(&items[2].content, ItemContent::Text { text, .. } if text == "Deployed."));
        assert!(matches!(&items[3].content, ItemContent::Tool { .. }));
        assert!(
            matches!(&items[4].content, ItemContent::Delivery { body, .. } if body == "Summary so far")
        );
    }
}
