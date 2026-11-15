//! Reads a provider's own transcript file, line by line, for what points at
//! pull requests. The provider writes it as the conversation goes, so one
//! reader covers history from before the daemon started, conversations whose
//! helper predates this reader, and subagents, without the provider adapter
//! or its Store taking part. Only the fields that name commands, tool calls
//! and prose are read; everything else in a line is ignored.

use crate::pull_request_signals::{self, Reference, Relation, Signal};
use serde_json::Value;
use std::collections::HashMap;

#[cfg(test)]
mod tests;

/// Calls still waiting for their results. A transcript whose calls never
/// complete cannot grow this without bound.
const PENDING: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Claude,
    Codex,
}

/// One signal, and whether a subagent rather than the conversation itself
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Found {
    pub signal: Signal,
    pub by_subagent: bool,
}

#[derive(Debug, Clone)]
enum Call {
    Shell {
        script: String,
    },
    /// A delegated agent: its result is its report, read as prose.
    Agent,
    Tool {
        name: String,
        input: String,
    },
}

/// One transcript file's reading state between lines.
#[derive(Debug)]
pub(super) struct Reader {
    format: Format,
    /// Every line of this file is a subagent's.
    subagent: bool,
    pending: HashMap<String, Call>,
}

impl Reader {
    pub fn new(format: Format, subagent: bool) -> Self {
        Self {
            format,
            subagent,
            pending: HashMap::new(),
        }
    }

    pub fn line(&mut self, line: &[u8], out: &mut Vec<Found>) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        match self.format {
            Format::Claude => self.claude(&value, out),
            Format::Codex => self.codex(&value, out),
        }
    }

    fn remember(&mut self, id: &str, call: Call) {
        if id.is_empty() {
            return;
        }
        if self.pending.len() >= PENDING {
            // Calls this old will not be answered; their results would only
            // have refined a signal, never been the only one.
            self.pending.clear();
        }
        self.pending.insert(id.to_owned(), call);
    }

    fn settle(&mut self, id: &str, output: &str, succeeded: Option<bool>) -> Vec<Signal> {
        match self.pending.remove(id) {
            Some(Call::Shell { script }) => {
                pull_request_signals::command(&script, output, succeeded)
            }
            Some(Call::Agent) => pull_request_signals::prose(output),
            Some(Call::Tool { name, input }) => pull_request_signals::tool(&name, &input, output),
            None => Vec::new(),
        }
    }

    fn claude(&mut self, value: &Value, out: &mut Vec<Found>) {
        // Harness context, skill bodies and compaction summaries are not
        // anything the user or an agent said.
        if value["isMeta"] == true || value["isCompactSummary"] == true {
            return;
        }
        let by_subagent = self.subagent || value["isSidechain"] == true;
        let mut signals = Vec::new();
        let content = &value["message"]["content"];
        match value["type"].as_str() {
            Some("user") => {
                if let Some(text) = content.as_str() {
                    signals.extend(prose(text));
                }
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => signals.extend(prose(block["text"].as_str().unwrap_or(""))),
                        Some("tool_result") => {
                            let output = block_text(&block["content"]);
                            let succeeded = Some(block["is_error"] != true);
                            let id = block["tool_use_id"].as_str().unwrap_or("");
                            signals.extend(self.settle(id, &output, succeeded));
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => signals.extend(prose(block["text"].as_str().unwrap_or(""))),
                        Some("tool_use") => {
                            let id = block["id"].as_str().unwrap_or("");
                            let input = &block["input"];
                            match block["name"].as_str().unwrap_or("") {
                                "Bash" => {
                                    if let Some(script) = input["command"].as_str() {
                                        self.remember(
                                            id,
                                            Call::Shell {
                                                script: script.to_owned(),
                                            },
                                        );
                                    }
                                }
                                "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                                    let path = input["file_path"]
                                        .as_str()
                                        .or_else(|| input["notebook_path"].as_str());
                                    signals.extend(path.and_then(edited));
                                }
                                "Agent" | "Task" => {
                                    // What it was asked to do names pull
                                    // requests the conversation pointed it at.
                                    signals.extend(prose(input["prompt"].as_str().unwrap_or("")));
                                    self.remember(id, Call::Agent);
                                }
                                // Reads and searches name files, not work.
                                "Read" | "Grep" | "Glob" | "LS" | "TodoWrite" | "WebSearch" => {}
                                name => self.remember(
                                    id,
                                    Call::Tool {
                                        name: name.to_owned(),
                                        input: input.to_string(),
                                    },
                                ),
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        out.extend(signals.into_iter().map(|signal| Found {
            signal,
            by_subagent,
        }));
    }

    fn codex(&mut self, value: &Value, out: &mut Vec<Found>) {
        if value["type"] != "response_item" {
            return;
        }
        let payload = &value["payload"];
        let mut signals = Vec::new();
        match payload["type"].as_str() {
            Some("message") => {
                let role = payload["role"].as_str();
                if matches!(role, Some("user" | "assistant")) {
                    for block in payload["content"].as_array().into_iter().flatten() {
                        let text = block["text"].as_str().unwrap_or("");
                        // Codex records its environment and instruction files
                        // as user messages.
                        if role == Some("user")
                            && (text.trim_start().starts_with('<')
                                || text.starts_with("# AGENTS.md"))
                        {
                            continue;
                        }
                        signals.extend(prose(text));
                    }
                }
            }
            Some("function_call") => {
                let id = payload["call_id"].as_str().unwrap_or("");
                let name = payload["name"].as_str().unwrap_or("");
                let arguments = payload["arguments"].as_str().unwrap_or("");
                let parsed = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
                match name {
                    "apply_patch" => {
                        signals.extend(patch_files(parsed["input"].as_str().unwrap_or("")));
                    }
                    _ => match codex_script(&parsed) {
                        Some(script) => self.remember(id, Call::Shell { script }),
                        None => self.remember(
                            id,
                            Call::Tool {
                                name: name.to_owned(),
                                input: arguments.to_owned(),
                            },
                        ),
                    },
                }
            }
            Some("local_shell_call") => {
                let id = payload["call_id"].as_str().unwrap_or("");
                if let Some(script) = codex_script(&payload["action"]) {
                    self.remember(id, Call::Shell { script });
                }
            }
            Some("custom_tool_call") if payload["name"] == "apply_patch" => {
                signals.extend(patch_files(payload["input"].as_str().unwrap_or("")));
            }
            Some(
                "function_call_output" | "local_shell_call_output" | "custom_tool_call_output",
            ) => {
                let id = payload["call_id"].as_str().unwrap_or("");
                let (output, succeeded) = codex_output(&payload["output"]);
                signals.extend(self.settle(id, &output, succeeded));
            }
            _ => {}
        }
        out.extend(signals.into_iter().map(|signal| Found {
            signal,
            by_subagent: self.subagent,
        }));
    }
}

/// Prose with harness framing removed: slash-command wrappers carry no one's
/// words, and system reminders are the harness's.
fn prose(text: &str) -> Vec<Signal> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("<command-")
        || trimmed.starts_with("<local-command")
        || trimmed.starts_with("<task-notification>")
    {
        return Vec::new();
    }
    if !text.contains("<system-reminder>") {
        return pull_request_signals::prose(text);
    }
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        kept.push_str(&rest[..start]);
        rest = match rest[start..].find("</system-reminder>") {
            Some(end) => &rest[start + end + "</system-reminder>".len()..],
            None => "",
        };
    }
    kept.push_str(rest);
    pull_request_signals::prose(&kept)
}

fn block_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A file edited at `path` points at whatever branch its directory has
/// checked out.
fn edited(path: &str) -> Option<Signal> {
    let directory = std::path::Path::new(path).parent()?.to_str()?;
    Some(Signal {
        reference: Reference::CurrentBranch {
            directory: (!directory.is_empty()).then(|| directory.to_owned()),
        },
        relation: Relation::Edited,
    })
}

fn patch_files(patch: &str) -> Vec<Signal> {
    let mut signals = patch
        .lines()
        .filter_map(|line| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .filter_map(|path| edited(path.trim()))
        .collect::<Vec<_>>();
    signals.sort_by(|a, b| a.reference.cmp(&b.reference));
    signals.dedup();
    signals
}

/// The shell text of a Codex command call, in the directory it ran in.
fn codex_script(arguments: &Value) -> Option<String> {
    let script = match &arguments["command"] {
        Value::String(script) => script.clone(),
        Value::Array(words) => {
            let words = words
                .iter()
                .map(|word| word.as_str().unwrap_or(""))
                .collect::<Vec<_>>();
            match words.as_slice() {
                [shell, flag, script, ..]
                    if matches!(
                        shell.rsplit('/').next(),
                        Some("bash" | "sh" | "zsh" | "dash")
                    ) && flag.starts_with('-')
                        && flag.ends_with('c') =>
                {
                    (*script).to_owned()
                }
                words => words
                    .iter()
                    .map(|word| shell_quote(word))
                    .collect::<Vec<_>>()
                    .join(" "),
            }
        }
        _ => arguments["cmd"].as_str()?.to_owned(),
    };
    Some(
        match arguments["workdir"]
            .as_str()
            .or_else(|| arguments["working_directory"].as_str())
        {
            Some(directory) => format!("cd {} && {script}", shell_quote(directory)),
            None => script,
        },
    )
}

fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=@+,%".contains(&byte))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// A Codex call's output text and whether it succeeded: structured as
/// `{"output", "metadata": {"exit_code"}}`, or text led by `Exit code: N`.
fn codex_output(output: &Value) -> (String, Option<bool>) {
    let text = match output {
        Value::String(text) => text.clone(),
        Value::Object(_) => output["content"].as_str().unwrap_or("").to_owned(),
        _ => String::new(),
    };
    if let Ok(structured) = serde_json::from_str::<Value>(&text)
        && let Some(body) = structured["output"].as_str()
    {
        let succeeded = structured["metadata"]["exit_code"]
            .as_i64()
            .map(|code| code == 0);
        return (body.to_owned(), succeeded);
    }
    let code = text.lines().take(3).find_map(|line| {
        line.strip_prefix("Exit code: ")
            .or_else(|| line.strip_prefix("Process exited with code "))
            .and_then(|code| code.trim().parse::<i64>().ok())
    });
    (text, code.map(|code| code == 0))
}
