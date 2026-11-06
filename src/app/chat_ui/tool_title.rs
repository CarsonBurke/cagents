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

/// Inputs above this are not read to title a row. The fields a title reads
/// are named members of the input, read once per change to the call and
/// without keeping the rest, so the limit only stops a pathological input:
/// a file being written is well within it and keeps its path.
const TITLE_INPUT_LIMIT: usize = 8 * 1024 * 1024;

/// The longest value a row shows as what a call acted on.
const TARGET_LIMIT: usize = 120;

/// A row's verb and what follows it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Title<'a> {
    pub(super) verb: &'static str,
    pub(super) label: Cow<'a, str>,
    /// Whether the label already says what the call acted on, so a preview
    /// of the input's target would repeat it.
    pub(super) names_target: bool,
}

/// The members of a tool's input that a row can be titled from. Anything
/// else, such as the content of a file being written, is skipped unread.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct Fields<'a> {
    #[serde(borrow)]
    skill: Option<Cow<'a, str>>,
    #[serde(borrow)]
    url: Option<Cow<'a, str>>,
    #[serde(borrow)]
    query: Option<Cow<'a, str>>,
    #[serde(borrow)]
    subject: Option<Cow<'a, str>>,
    #[serde(borrow)]
    file_path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    notebook_path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pattern: Option<Cow<'a, str>>,
    #[serde(borrow)]
    description: Option<Cow<'a, str>>,
    #[serde(borrow)]
    command: Option<Cow<'a, str>>,
    #[serde(borrow)]
    to: Option<Cow<'a, str>>,
    todos: Option<Vec<Todo>>,
    #[serde(borrow)]
    questions: Option<Vec<Question<'a>>>,
}

#[derive(serde::Deserialize)]
struct Todo {
    #[serde(default)]
    status: Option<String>,
}

#[derive(serde::Deserialize)]
struct Question<'a> {
    #[serde(borrow, default)]
    question: Option<Cow<'a, str>>,
}

/// The input's named members, when it is an object of a size worth reading.
fn fields(input: &str) -> Option<Fields<'_>> {
    if input.len() > TITLE_INPUT_LIMIT || !input.trim_start().starts_with('{') {
        return None;
    }
    serde_json::from_str(input).ok()
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

/// Integrations whose names are not written the way their identifiers are.
const INTEGRATION_NAMES: &[(&str, &str)] = &[
    ("github", "GitHub"),
    ("gitlab", "GitLab"),
    ("openai", "OpenAI"),
    ("hubspot", "HubSpot"),
    ("youtube", "YouTube"),
    ("posthog", "PostHog"),
];

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
    if let Some((_, written)) = INTEGRATION_NAMES
        .iter()
        .find(|(id, _)| server.eq_ignore_ascii_case(id))
    {
        server = (*written).to_owned();
    } else if let Some(first) = server.get(..1) {
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
fn plan_progress(todos: &[Todo]) -> String {
    let done = todos
        .iter()
        .filter(|todo| todo.status.as_deref() == Some("completed"))
        .count();
    match todos.len() {
        0 => "cleared".to_owned(),
        total => format!("{done} of {total} done"),
    }
}

/// What a call does, said as it stands: under way, done, or tried and not
/// done. A call that failed, was refused or was stopped says what it tried,
/// and its status glyph says what became of it.
struct Verb {
    doing: &'static str,
    done: &'static str,
    tried: &'static str,
}

impl Verb {
    fn said(&self, status: ToolStatus) -> &'static str {
        match status {
            ToolStatus::Running => self.doing,
            ToolStatus::Succeeded => self.done,
            ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped => self.tried,
        }
    }
}

const fn verb(doing: &'static str, done: &'static str, tried: &'static str) -> Verb {
    Verb { doing, done, tried }
}

/// The row a plain tool call is shown as.
pub(super) fn title<'a>(name: &'a str, input: &'a str, status: ToolStatus) -> Title<'a> {
    let fields = fields(input).unwrap_or_default();
    let owned = |value: &Option<Cow<'_, str>>| {
        value
            .as_deref()
            .and_then(short)
            .map(|value| Cow::Owned(value.to_owned()))
    };
    let named = |verb: Verb, label: Option<Cow<'a, str>>| Title {
        verb: verb.said(status),
        label: label.unwrap_or_default(),
        names_target: true,
    };
    match name {
        "Skill" => named(
            verb("Using skill", "Used skill", "Tried to use skill"),
            owned(&fields.skill),
        ),
        "WebFetch" => named(
            verb("Fetching", "Fetched", "Tried to fetch"),
            owned(&fields.url),
        ),
        "WebSearch" | "web_search" => named(
            verb(
                "Searching the web for",
                "Searched the web for",
                "Tried to search the web for",
            ),
            owned(&fields.query),
        ),
        // `select:Read,Edit` loads the tools it names rather than searching.
        "ToolSearch" => match fields.query.as_deref().map(str::trim) {
            Some(query) if query.starts_with("select:") => named(
                verb("Loading tools", "Loaded tools", "Tried to load tools"),
                short(&query["select:".len()..].replace(',', ", "))
                    .map(|names| Cow::Owned(names.to_owned())),
            ),
            _ => match owned(&fields.query) {
                Some(query) => named(
                    verb(
                        "Looking up tools for",
                        "Looked up tools for",
                        "Tried to look up tools for",
                    ),
                    Some(query),
                ),
                None => named(
                    verb(
                        "Looking up tools",
                        "Looked up tools",
                        "Tried to look up tools",
                    ),
                    None,
                ),
            },
        },
        "TodoWrite" => named(
            verb(
                "Updating the plan",
                "Updated the plan",
                "Tried to update the plan",
            ),
            fields.todos.as_deref().map(plan_progress).map(Cow::Owned),
        ),
        "TaskCreate" => named(
            verb("Adding a task", "Added a task", "Tried to add a task"),
            owned(&fields.subject),
        ),
        "TaskUpdate" => named(
            verb(
                "Updating a task",
                "Updated a task",
                "Tried to update a task",
            ),
            owned(&fields.subject),
        ),
        "TaskList" => named(
            verb("Listing tasks", "Listed tasks", "Tried to list tasks"),
            None,
        ),
        "TaskGet" => named(
            verb("Reading a task", "Read a task", "Tried to read a task"),
            None,
        ),
        "NotebookEdit" => named(
            verb("Editing", "Edited", "Tried to edit"),
            owned(&fields.notebook_path),
        ),
        "Edit" | "MultiEdit" => named(
            verb("Editing", "Edited", "Tried to edit"),
            owned(&fields.file_path),
        ),
        "Write" => named(
            verb("Writing", "Wrote", "Tried to write"),
            owned(&fields.file_path),
        ),
        "Read" => named(
            verb("Reading", "Read", "Tried to read"),
            owned(&fields.file_path),
        ),
        "BashOutput" | "TaskOutput" => named(
            verb(
                "Checking a background task",
                "Checked a background task",
                "Tried to check a background task",
            ),
            None,
        ),
        "KillShell" | "KillBash" | "TaskStop" => named(
            verb(
                "Stopping a background task",
                "Stopped a background task",
                "Tried to stop a background task",
            ),
            None,
        ),
        // A monitor's description already says what it watches, in the
        // imperative ("Watch the build log"), so it stands alone; a bare
        // command is introduced by the verb.
        "Monitor" => match owned(&fields.description) {
            Some(description) => Title {
                verb: "",
                label: description,
                names_target: true,
            },
            None => named(
                verb("Watching", "Watched", "Tried to watch"),
                fields
                    .command
                    .as_deref()
                    .map(|command| Cow::Owned(headline(command))),
            ),
        },
        "AskUserQuestion" => named(
            verb("Asking", "Asked", "Tried to ask"),
            fields
                .questions
                .as_ref()
                .and_then(|questions| questions.first()?.question.as_deref())
                .map(|question| Cow::Owned(headline(question))),
        ),
        "ExitPlanMode" => named(
            verb(
                "Proposing a plan",
                "Proposed a plan",
                "Tried to propose a plan",
            ),
            None,
        ),
        "EnterPlanMode" => named(
            verb(
                "Entering plan mode",
                "Entered plan mode",
                "Tried to enter plan mode",
            ),
            None,
        ),
        "SendMessage" => named(
            verb("Messaging", "Messaged", "Tried to message"),
            owned(&fields.to),
        ),
        "Agent" | "Task" => named(
            verb(
                "Starting a subagent",
                "Started a subagent",
                "Tried to start a subagent",
            ),
            owned(&fields.description),
        ),
        // Claude's lifecycle row for a task it reported before its call: its
        // input is the task's own description, in plain words. A subagent
        // that failed or was stopped still ran; its glyph says how it ended.
        "Subagent task" => named(
            verb("Running a subagent", "Ran a subagent", "Ran a subagent"),
            (!input.trim().is_empty()).then(|| Cow::Owned(headline(input))),
        ),
        _ => match integration(name) {
            Some(label) => Title {
                verb: verb("Using", "Used", "Tried to use").said(status),
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
    let Some(fields) = fields(input) else {
        return String::new();
    };
    [
        &fields.file_path,
        &fields.path,
        &fields.url,
        &fields.query,
        &fields.pattern,
        &fields.skill,
        &fields.subject,
    ]
    .into_iter()
    .find_map(|value| value.as_deref().and_then(short))
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
                json!({"query":"select:Read,Edit"}),
                "Loaded tools Read, Edit",
            ),
            (
                "ToolSearch",
                json!({"query":"notebook jupyter"}),
                "Looked up tools for notebook jupyter",
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
                "Watch the log",
            ),
            (
                "Monitor",
                json!({"command":"tail -f log\n| grep x"}),
                "Watched tail -f log",
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
                "mcp__github__create_pull_request",
                json!({}),
                "Used GitHub: create pull request",
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
        let titled = |status| {
            let title = title("Subagent task", "Survey the tool rows\nand report", status);
            format!("{} {}", title.verb, title.label)
        };
        assert_eq!(
            titled(ToolStatus::Running),
            "Running a subagent Survey the tool rows"
        );
        assert_eq!(
            titled(ToolStatus::Succeeded),
            "Ran a subagent Survey the tool rows"
        );
    }

    /// A call still under way says so; it does not claim to be done.
    #[test]
    fn a_running_call_says_what_it_is_doing() {
        let running = ToolStatus::Running;
        for (name, input, expected) in [
            (
                "Read",
                json!({"file_path":"src/lib.rs"}),
                "Reading src/lib.rs",
            ),
            (
                "Edit",
                json!({"file_path":"src/lib.rs"}),
                "Editing src/lib.rs",
            ),
            (
                "Write",
                json!({"file_path":"a.md","content":"x"}),
                "Writing a.md",
            ),
            (
                "WebFetch",
                json!({"url":"https://example.com"}),
                "Fetching https://example.com",
            ),
            (
                "Skill",
                json!({"skill":"writing-copy"}),
                "Using skill writing-copy",
            ),
            (
                "Agent",
                json!({"description":"Survey"}),
                "Starting a subagent Survey",
            ),
            (
                "mcp__linear__get_issue",
                json!({}),
                "Using Linear: get issue",
            ),
        ] {
            assert_eq!(row(name, input, running), expected, "{name}");
        }
        assert_eq!(
            row("mcp__linear__get_issue", json!({}), ToolStatus::Failed),
            "Tried to use Linear: get issue"
        );
    }

    /// A large file being written keeps its path on the row: the content
    /// is skipped, not a reason to give up on the title.
    #[test]
    fn a_large_write_keeps_its_path() {
        let content = "x".repeat(1024 * 1024);
        assert_eq!(
            row(
                "Write",
                json!({"content":content,"file_path":"big.txt"}),
                ToolStatus::Denied
            ),
            "Tried to write big.txt"
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
