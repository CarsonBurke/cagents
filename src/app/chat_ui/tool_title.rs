//! What a tool call's row says it did, for a call the transcript keeps as a
//! plain tool call rather than a command, a file change or a web search.
//!
//! Such a row used to read "Called" followed by the tool's internal name for
//! every call, with "Input available" beneath it, so a plan update, a skill
//! and an MCP request all looked alike and none said what happened. A tool a
//! provider documents is named here by what it does, from the part of its
//! input a reader would recognise; any other tool keeps its name, made
//! readable, and its row's preview offers the one short target its input
//! names.

use std::borrow::Cow;

use crate::chat::ToolStatus;

/// Inputs above this are not parsed to title a row. Titles are recomputed as
/// rows are grouped and drawn, and the fields they read are near the start of
/// a small object; a large input is a document, shown in the call's detail.
const TITLE_INPUT_LIMIT: usize = 16 * 1024;

/// The longest value a row shows as what a call acted on.
const TARGET_LIMIT: usize = 120;

/// A row's verb and what follows it.
#[derive(Debug, PartialEq)]
pub(super) struct Title<'a> {
    pub(super) verb: &'static str,
    pub(super) label: Cow<'a, str>,
    /// Whether the label already says what the call acted on, so a preview
    /// of the input's target would repeat it.
    pub(super) names_target: bool,
}

fn parsed(input: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    if input.len() > TITLE_INPUT_LIMIT || input.trim().is_empty() {
        return None;
    }
    match serde_json::from_str(input).ok()? {
        serde_json::Value::Object(object) => Some(object),
        _ => None,
    }
}

/// A field's value when it is short enough to stand on a row: one line,
/// at most `TARGET_LIMIT` characters.
pub(super) fn short(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= TARGET_LIMIT && !value.contains(['\n', '\r']))
        .then_some(value)
}

/// The first line of a value, bounded for a row.
fn headline(value: &str) -> String {
    let line = value.trim().lines().next().unwrap_or_default().trim();
    let mut chars = line.chars();
    let mut shown: String = chars.by_ref().take(TARGET_LIMIT).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

/// `mcp__linear__get_issue` and Codex's `linear / get_issue` as a reader
/// would say them: "Linear: get issue".
fn integration(name: &str) -> Option<String> {
    let (server, tool) = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .or_else(|| name.split_once(" / "))?;
    // Claude names a claude.ai connector by its account scope first.
    let server = server.strip_prefix("claude_ai_").unwrap_or(server);
    let words = |text: &str| text.replace(['_', '-'], " ").trim().to_owned();
    let mut server = words(server);
    if let Some(first) = server.get(..1) {
        let upper = first.to_uppercase();
        server.replace_range(..1, &upper);
    }
    let tool = words(tool);
    Some(match (server.is_empty(), tool.is_empty()) {
        (false, false) => format!("{server}: {tool}"),
        (true, _) => tool,
        (_, true) => server,
    })
}

/// How a plan's items stand, as "2 of 5 done".
fn plan_progress(input: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let items = input.get("todos")?.as_array()?;
    let done = items
        .iter()
        .filter(|item| item["status"].as_str() == Some("completed"))
        .count();
    Some(match items.len() {
        0 => "cleared".to_owned(),
        total => format!("{done} of {total} done"),
    })
}

/// The row a plain tool call is shown as.
pub(super) fn title<'a>(name: &'a str, input: &'a str, status: ToolStatus) -> Title<'a> {
    let object = parsed(input);
    let field = |key: &str| {
        object
            .as_ref()
            .and_then(|object| object.get(key))
            .and_then(serde_json::Value::as_str)
            .and_then(short)
            .map(|value| Cow::Owned(value.to_owned()))
    };
    let named = |verb: &'static str, label: Option<Cow<'a, str>>| Title {
        verb,
        label: label.unwrap_or_default(),
        names_target: true,
    };
    // A call that did not go through says what it tried; its status says
    // what became of it.
    let settled = !matches!(status, ToolStatus::Running | ToolStatus::Succeeded);
    match name {
        "Skill" => named("Used skill", field("skill")),
        "WebFetch" => named("Fetched", field("url")),
        "WebSearch" | "web_search" => named("Searched the web for", field("query")),
        "ToolSearch" => match field("query") {
            Some(query) => named("Looked up tools for", Some(query)),
            None => named("Looked up tools", None),
        },
        "TodoWrite" => named(
            "Updated the plan",
            object.as_ref().and_then(plan_progress).map(Cow::Owned),
        ),
        "TaskCreate" => named("Added a task", field("subject")),
        "TaskUpdate" => named("Updated a task", field("subject")),
        "TaskList" => named("Listed tasks", None),
        "TaskGet" => named("Read a task", None),
        "NotebookEdit" => named(
            if settled { "Tried to edit" } else { "Edited" },
            field("notebook_path"),
        ),
        // A failed edit is shown as its call, carrying why it failed.
        "Edit" | "MultiEdit" => named(
            if settled { "Tried to edit" } else { "Edited" },
            field("file_path"),
        ),
        "Write" => named(
            if settled { "Tried to write" } else { "Wrote" },
            field("file_path"),
        ),
        "Read" => named("Read", field("file_path")),
        "BashOutput" | "TaskOutput" => named("Checked a background task", None),
        "KillShell" | "KillBash" | "TaskStop" => named("Stopped a background task", None),
        "Monitor" => named(
            "Watched",
            field("description").or_else(|| {
                object
                    .as_ref()
                    .and_then(|object| object.get("command"))
                    .and_then(serde_json::Value::as_str)
                    .map(|command| Cow::Owned(headline(command)))
            }),
        ),
        "AskUserQuestion" => named(
            "Asked",
            object
                .as_ref()
                .and_then(|object| object.get("questions"))
                .and_then(|questions| questions[0]["question"].as_str())
                .map(|question| Cow::Owned(headline(question))),
        ),
        "ExitPlanMode" => named("Proposed a plan", None),
        "EnterPlanMode" => named("Entered plan mode", None),
        "SendMessage" => named("Messaged", field("to")),
        "Agent" | "Task" => named("Started a subagent", field("description")),
        // Claude's lifecycle row for a task it reported before its call: its
        // input is the task's own description, in plain words.
        "Subagent task" => named(
            "Ran a subagent",
            (!input.trim().is_empty()).then(|| Cow::Owned(headline(input))),
        ),
        _ => match integration(name) {
            Some(label) => Title {
                verb: "Used",
                label: Cow::Owned(label),
                names_target: false,
            },
            None => Title {
                verb: "",
                label: Cow::Borrowed(name),
                names_target: false,
            },
        },
    }
}

/// The short target an unfamiliar tool's input names, shown under its name.
/// A tool's arguments are not evidence of what it does, so only a field that
/// names a target is shown; anything else stays in the call's detail.
pub(super) fn target(input: &str) -> String {
    let Some(object) = parsed(input) else {
        return String::new();
    };
    [
        "file_path",
        "path",
        "url",
        "query",
        "pattern",
        "skill",
        "subject",
    ]
    .into_iter()
    .find_map(|key| object.get(key)?.as_str().and_then(short))
    .unwrap_or_default()
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, input: serde_json::Value, status: ToolStatus) -> String {
        let input = input.to_string();
        let title = title(name, &input, status);
        [title.verb, title.label.as_ref()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Each documented tool's row says what was done, never "Called" and
    /// never the tool's internal name.
    #[test]
    fn documented_tools_say_what_they_did() {
        let done = ToolStatus::Succeeded;
        for (name, input, expected) in [
            (
                "Skill",
                json!({"skill":"writing-copy","args":"long instructions"}),
                "Used skill writing-copy",
            ),
            (
                "WebFetch",
                json!({"url":"https://example.com/a","prompt":"Summarize"}),
                "Fetched https://example.com/a",
            ),
            (
                "WebSearch",
                json!({"query":"iced hover widget"}),
                "Searched the web for iced hover widget",
            ),
            (
                "ToolSearch",
                json!({"query":"select:Read"}),
                "Looked up tools for select:Read",
            ),
            (
                "TodoWrite",
                json!({"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"},{"content":"c","status":"pending"}]}),
                "Updated the plan 1 of 3 done",
            ),
            (
                "TaskCreate",
                json!({"subject":"Fix the rows","description":"…"}),
                "Added a task Fix the rows",
            ),
            (
                "NotebookEdit",
                json!({"notebook_path":"analysis.ipynb","new_source":"x"}),
                "Edited analysis.ipynb",
            ),
            (
                "TaskOutput",
                json!({"task_id":"b71bdxnta"}),
                "Checked a background task",
            ),
            (
                "KillShell",
                json!({"shell_id":"b71bdxnta"}),
                "Stopped a background task",
            ),
            (
                "Monitor",
                json!({"command":"tail -f log\n| grep x","description":"Watch the log"}),
                "Watched Watch the log",
            ),
            (
                "AskUserQuestion",
                json!({"questions":[{"question":"Which layout?","options":[]}]}),
                "Asked Which layout?",
            ),
            (
                "ExitPlanMode",
                json!({"plan":"# Plan\n…"}),
                "Proposed a plan",
            ),
            (
                "SendMessage",
                json!({"to":"researcher","message":"start"}),
                "Messaged researcher",
            ),
            (
                "mcp__linear__get_issue",
                json!({"id":"ENG-1"}),
                "Used Linear: get issue",
            ),
            (
                "mcp__claude_ai_Claude_Docs__batch",
                json!({}),
                "Used Claude Docs: batch",
            ),
            (
                "linear / list_issues",
                json!(null),
                "Used Linear: list issues",
            ),
            ("fixture", json!({}), "fixture"),
        ] {
            let shown = row(name, input, done);
            assert_eq!(shown, expected, "{name}");
            assert!(!shown.contains("Called"), "{name}");
        }
    }

    /// A refused or failed edit is shown as its call: it says what it tried,
    /// not that it edited the file.
    #[test]
    fn an_edit_that_did_not_happen_says_what_it_tried() {
        let input = json!({"file_path":"src/lib.rs","old_string":"a","new_string":"b"});
        for status in [ToolStatus::Failed, ToolStatus::Denied, ToolStatus::Stopped] {
            assert_eq!(
                row("Edit", input.clone(), status),
                "Tried to edit src/lib.rs"
            );
        }
        assert_eq!(
            row(
                "Write",
                json!({"file_path":"a.md","content":"x"}),
                ToolStatus::Denied
            ),
            "Tried to write a.md"
        );
    }

    /// Claude's own lifecycle row carries the task's description as plain
    /// text, which is what the row names.
    #[test]
    fn a_reported_subagent_task_is_named_by_its_description() {
        let title = title(
            "Subagent task",
            "Survey the tool rows\nand report",
            ToolStatus::Running,
        );
        assert_eq!(
            (title.verb, title.label.as_ref()),
            ("Ran a subagent", "Survey the tool rows")
        );
    }

    #[test]
    fn an_unfamiliar_tool_previews_only_a_short_named_target() {
        for (input, expected) in [
            (
                json!({"path":"src/app.rs","prompt":"framing"}).to_string(),
                "src/app.rs",
            ),
            (
                json!({"prompt":"framing","body":"document"}).to_string(),
                "",
            ),
            (json!({"query":"first\nsecond"}).to_string(), ""),
            (r#"{"prompt":"Internal framing"#.to_owned(), ""),
            ("plain prose".to_owned(), ""),
        ] {
            assert_eq!(target(&input), expected, "{input}");
        }
    }
}
