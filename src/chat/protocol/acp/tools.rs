//! ACP tool calls as transcript steps.
//!
//! An agent reports a call with `tool_call` and then revises it with sparse
//! `tool_call_update`s, each naming only the fields that changed, and a
//! permission request carries one more such update. The adapter keeps each
//! call's merged state so that every revision can be shown whole, and so
//! that an update that arrives before the call it revises, or a late copy of
//! the call itself, cannot take a finished step back to running.
use crate::chat::{ChangeKind, CommandAction, FileChange, ItemContent, ToolStatus};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// The same bound the OMP and Codex adapters put on the calls they keep, so
/// a long session does not carry the arguments of every call it ever made.
const MAX_CALLS: usize = 128;
/// A step's output is shown whole up to this; past it, its start is kept
/// with a note, since every revision of a call resends all of it.
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_DIFFS: usize = 64;

#[derive(Debug, Clone, Default)]
struct Call {
    title: String,
    kind: String,
    status: Option<ToolStatus>,
    /// Text the call produced, from its `content` blocks.
    output: Option<String>,
    diffs: Option<Vec<FileChange>>,
    locations: Vec<String>,
    input: Value,
    raw_output: Value,
}

#[derive(Default)]
pub(super) struct Calls {
    calls: HashMap<String, Call>,
    order: VecDeque<String>,
}

impl Calls {
    /// Merges a `tool_call`, a `tool_call_update` or a permission request's
    /// `toolCall` into what is known of the call, and returns the step to
    /// show for it.
    pub fn merge(&mut self, update: &Value) -> Option<(String, ItemContent)> {
        let id = update["toolCallId"].as_str().filter(|id| !id.is_empty())?;
        if !self.calls.contains_key(id) {
            if self.order.len() == MAX_CALLS
                && let Some(oldest) = self.order.pop_front()
            {
                self.calls.remove(&oldest);
            }
            self.order.push_back(id.to_owned());
        }
        let call = self.calls.entry(id.to_owned()).or_default();
        if let Some(title) = update["title"].as_str() {
            call.title = title.to_owned();
        }
        if let Some(kind) = update["kind"].as_str() {
            call.kind = kind.to_owned();
        }
        if let Some(status) = update["status"].as_str().and_then(status) {
            // A finished call stays finished: a revision delivered out of
            // order, or a late copy of the original call, says less.
            if call.status.is_none_or(ToolStatus::is_live) {
                call.status = Some(status);
            }
        }
        if let Some(content) = update["content"].as_array() {
            let (output, diffs) = contents(content);
            call.output = Some(output);
            call.diffs = (!diffs.is_empty()).then_some(diffs);
        }
        if let Some(locations) = update["locations"].as_array() {
            call.locations = locations
                .iter()
                .filter_map(|location| location["path"].as_str())
                .take(MAX_DIFFS)
                .map(str::to_owned)
                .collect();
        }
        if !update["rawInput"].is_null() {
            call.input = update["rawInput"].clone();
        }
        if !update["rawOutput"].is_null() {
            call.raw_output = update["rawOutput"].clone();
        }
        Some((format!("tool:{id}"), call.content()))
    }

    /// Ends every call still running, as a turn's end does: whatever it was
    /// doing stopped with the turn.
    pub fn stop_running(&mut self) -> Vec<(String, ItemContent)> {
        let mut stopped = Vec::new();
        for id in &self.order {
            let Some(call) = self.calls.get_mut(id) else {
                continue;
            };
            if call.status.is_none_or(ToolStatus::is_live) {
                call.status = Some(ToolStatus::Stopped);
                stopped.push((format!("tool:{id}"), call.content()));
            }
        }
        stopped
    }

    /// What the reader is asked to approve: the command, or the files, or
    /// failing those the agent's own title.
    pub fn summary(&self, id: &str) -> String {
        let Some(call) = self.calls.get(id) else {
            return String::new();
        };
        if let Some(diffs) = &call.diffs {
            return diffs
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>()
                .join("\n");
        }
        if call.kind == "execute"
            && let Some(command) = command_line(&call.input)
        {
            return command;
        }
        call.locations.join("\n")
    }

    pub fn title(&self, id: &str) -> Option<&str> {
        self.calls
            .get(id)
            .map(|call| call.title.as_str())
            .filter(|title| !title.is_empty())
    }
}

impl Call {
    fn content(&self) -> ItemContent {
        let status = self.status.unwrap_or(ToolStatus::Running);
        if let Some(diffs) = &self.diffs {
            return ItemContent::Changes {
                changes: diffs.clone(),
                status,
            };
        }
        let output = self
            .output
            .clone()
            .filter(|output| !output.is_empty())
            .unwrap_or_else(|| raw_output(&self.raw_output));
        let action = match self.kind.as_str() {
            "execute" => Some(CommandAction::Run),
            "read" => Some(CommandAction::Read),
            "search" => Some(CommandAction::Search),
            _ => None,
        };
        match action {
            Some(action) => {
                let command = match action {
                    CommandAction::Run => command_line(&self.input),
                    _ => None,
                }
                .unwrap_or_else(|| self.title.clone());
                let label = match action {
                    // A read is labelled by what it read.
                    CommandAction::Read => self
                        .locations
                        .first()
                        .cloned()
                        .unwrap_or_else(|| self.title.clone()),
                    // The agent's title for a command is its own summary of
                    // it, worth showing only when it says something the
                    // command line does not.
                    _ if self.title == command => String::new(),
                    _ => self.title.clone(),
                };
                ItemContent::Command {
                    label,
                    command,
                    working_directory: self.input["cwd"].as_str().unwrap_or_default().to_owned(),
                    action,
                    status,
                    exit_code: exit_code(&self.raw_output),
                    duration_ms: None,
                    output: match action {
                        CommandAction::Run => super::super::terminal_output::display(output),
                        _ => output,
                    },
                }
            }
            None => ItemContent::Tool {
                name: if self.title.is_empty() {
                    "Tool".to_owned()
                } else {
                    self.title.clone()
                },
                input: if self.input.is_null() {
                    String::new()
                } else {
                    serde_json::to_string_pretty(&self.input).unwrap_or_default()
                },
                output,
                status,
            },
        }
    }
}

fn status(status: &str) -> Option<ToolStatus> {
    match status {
        "pending" | "in_progress" => Some(ToolStatus::Running),
        "completed" => Some(ToolStatus::Succeeded),
        "failed" => Some(ToolStatus::Failed),
        _ => None,
    }
}

/// A tool call's `content`: its text output and the file changes it made.
/// A `terminal` block names a terminal the client would have had to create,
/// which cagents never advertises, so it has nothing to show.
fn contents(blocks: &[Value]) -> (String, Vec<FileChange>) {
    let mut output = String::new();
    let mut diffs = Vec::new();
    // Every diff of a call shares the bound on its output.
    let mut budget = MAX_OUTPUT;
    for block in blocks {
        match block["type"].as_str() {
            Some("content") => {
                if let Some(text) = block_text(&block["content"]) {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(text);
                }
            }
            Some("diff") if diffs.len() < MAX_DIFFS => {
                if let Some(mut change) = diff(block) {
                    let shown = change.diff.len().min(budget);
                    if shown < change.diff.len() {
                        change.diff = bounded_to(change.diff, shown);
                    }
                    budget -= shown;
                    diffs.push(change);
                }
            }
            _ => {}
        }
    }
    (bounded(output), diffs)
}

/// The text a content block shows, for the kinds that carry any.
pub(super) fn block_text(block: &Value) -> Option<&str> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str(),
        "resource" => block["resource"]["text"].as_str(),
        "resource_link" => block["uri"].as_str(),
        _ => None,
    }
}

fn diff(block: &Value) -> Option<FileChange> {
    let path = block["path"].as_str().filter(|path| !path.is_empty())?;
    let after = block["newText"].as_str()?;
    let before = block["oldText"].as_str();
    let kind = match before {
        None => ChangeKind::Added,
        Some(_) if after.is_empty() => ChangeKind::Deleted,
        Some(_) => ChangeKind::Updated,
    };
    Some(FileChange {
        lines: crate::chat::LineOrigin::File,
        path: path.to_owned(),
        kind,
        diff: super::super::claude_edits::unified(before.unwrap_or_default(), after, ""),
    })
}

fn command_line(input: &Value) -> Option<String> {
    match &input["command"] {
        Value::String(command) => Some(command.clone()),
        Value::Array(argv) => {
            let words = argv.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            let mut line = words.join(" ");
            if let Some(arguments) = input["args"].as_array() {
                for argument in arguments.iter().filter_map(Value::as_str) {
                    line.push(' ');
                    line.push_str(argument);
                }
            }
            (!line.is_empty()).then_some(line)
        }
        _ => None,
    }
}

fn raw_output(raw: &Value) -> String {
    let text = match raw {
        Value::Null => return String::new(),
        Value::String(text) => text.clone(),
        _ => ["output", "stdout", "content", "result"]
            .iter()
            .find_map(|key| raw[*key].as_str().map(str::to_owned))
            .unwrap_or_else(|| serde_json::to_string_pretty(raw).unwrap_or_default()),
    };
    bounded(text)
}

fn exit_code(raw: &Value) -> Option<i32> {
    ["exitCode", "exit_code"]
        .iter()
        .find_map(|key| raw[*key].as_i64())
        .and_then(|code| i32::try_from(code).ok())
}

fn bounded(text: String) -> String {
    bounded_to(text, MAX_OUTPUT)
}

/// `text`, cut to its first `limit` bytes with a note when it is longer.
fn bounded_to(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[Output truncated]");
    }
    text
}
