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
/// are named members of the input, read once per change to the call (the
/// title is kept with the row's presentation, never formed while drawing)
/// and without keeping the rest, so the limit only stops a pathological
/// input: a file being written is well within it and keeps its path.
const TITLE_INPUT_LIMIT: usize = 8 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    /// How many times this thread has read a call's input for its title.
    pub(super) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The longest value a row shows as what a call acted on.
const TARGET_LIMIT: usize = 120;

/// A row's verb and what follows it, formed once each time the call changes
/// and kept with the row, so drawing and regrouping never read the input.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Title {
    pub(super) verb: &'static str,
    pub(super) label: String,
    /// The short target an unfamiliar tool's input names, shown under its
    /// name. Empty when the label already says what the call acted on.
    pub(super) target: String,
}

impl Title {
    /// A call's row as its name alone, for a call whose input has not been
    /// read: nothing here parses it.
    pub(super) fn unread(name: &str) -> Self {
        Self {
            label: readable(name),
            ..Self::default()
        }
    }

    /// The verb and label as one line, as a group's summary says it.
    pub(super) fn line(&self) -> String {
        match (self.verb.is_empty(), self.label.is_empty()) {
            (true, _) => self.label.clone(),
            (false, true) => self.verb.to_owned(),
            (false, false) => format!("{} {}", self.verb, self.label),
        }
    }
}

/// The members of a tool's input that a row can be titled from. Anything
/// else, such as the content of a file being written, is skipped unread. A
/// member of an unexpected type is passed over on its own, so a tool whose
/// `query` is a list still keeps the `path` beside it.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct Fields<'a> {
    #[serde(borrow, deserialize_with = "text")]
    skill: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    url: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    query: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    subject: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    file_path: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    notebook_path: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    path: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    pattern: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    description: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    command: Option<Cow<'a, str>>,
    #[serde(borrow, deserialize_with = "text")]
    to: Option<Cow<'a, str>>,
    #[serde(deserialize_with = "lenient")]
    todos: Option<Vec<Todo>>,
    #[serde(borrow, deserialize_with = "lenient")]
    questions: Option<Vec<Question<'a>>>,
}

/// A member that is a string, or nothing when it is any other value. The
/// value is read in place, so a string without escapes is borrowed and
/// anything else is skipped without being kept.
fn text<'de: 'a, 'a, D>(deserializer: D) -> Result<Option<Cow<'a, str>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
    struct Text;
    impl<'de> Visitor<'de> for Text {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("any value")
        }
        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(value)))
        }
        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(value.to_owned())))
        }
        fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(value)))
        }
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }
    deserializer.deserialize_any(Text)
}

/// A member read as `T`, or nothing when it has another shape. Unlike
/// [`text`], this holds the member while trying it, so it is kept to the
/// small lists a title reads.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged, bound = "T: serde::Deserialize<'de>")]
    enum Member<T> {
        Read(T),
        Other(serde::de::IgnoredAny),
    }
    Ok(
        match <Member<T> as serde::Deserialize>::deserialize(deserializer)? {
            Member::Read(value) => Some(value),
            Member::Other(_) => None,
        },
    )
}

#[derive(serde::Deserialize)]
struct Todo {
    #[serde(default, deserialize_with = "lenient")]
    status: Option<String>,
}

#[derive(serde::Deserialize)]
struct Question<'a> {
    #[serde(borrow, default, deserialize_with = "text")]
    question: Option<Cow<'a, str>>,
}

/// The input's named members, when it is a whole object of a size worth
/// reading. An input still streaming is an unclosed prefix, which is not
/// scanned only to fail at its end.
fn fields(input: &str) -> Option<Fields<'_>> {
    let input = input.trim();
    if input.len() > TITLE_INPUT_LIMIT || !input.starts_with('{') || !input.ends_with('}') {
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

/// Words in integration names that are not written the way their
/// identifiers are.
const INTEGRATION_WORDS: &[(&str, &str)] = &[
    ("github", "GitHub"),
    ("gitlab", "GitLab"),
    ("openai", "OpenAI"),
    ("hubspot", "HubSpot"),
    ("youtube", "YouTube"),
    ("posthog", "PostHog"),
    ("devtools", "DevTools"),
    ("api", "API"),
];

/// `mcp__linear__get_issue` and Codex's `linear / get_issue` as a reader
/// would say them: "Linear: get issue". An integration's name is a proper
/// name, so each of its words is capitalised; the action keeps its words
/// as they are.
fn integration(name: &str) -> Option<String> {
    let (server, tool) = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .or_else(|| name.split_once(" / "))?;
    // Claude names a claude.ai connector by its account scope first.
    let server = server.strip_prefix("claude_ai_").unwrap_or(server);
    let words = |text: &str| -> Vec<String> {
        text.split(['_', '-', ' '])
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let server = words(server)
        .into_iter()
        .map(|word| {
            if let Some((_, written)) = INTEGRATION_WORDS
                .iter()
                .find(|(id, _)| word.eq_ignore_ascii_case(id))
            {
                (*written).to_owned()
            } else if word.chars().all(|c| !c.is_uppercase()) {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let tool = words(tool).join(" ");
    Some(match (server.as_str(), tool.is_empty()) {
        // Codex's app connectors without an app named: the namespace is
        // an implementation name, and the action already says the app.
        ("Codex Apps" | "", _) => {
            let mut chars = tool.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
        (_, true) => server,
        (_, false) => format!("{server}: {tool}"),
    })
}

/// A tool's name as a reader would say it: an integration's as "Linear:
/// get issue", an identifier's (`apply_patch`) with its words spaced.
fn readable(name: &str) -> String {
    integration(name).unwrap_or_else(|| {
        if name.contains(char::is_whitespace) {
            name.to_owned()
        } else {
            name.replace('_', " ")
        }
    })
}

/// A name given as a sentence-case phrase, as Codex names "Image
/// generation", continues the verb before it: "Used image generation". A
/// name whose first word is not a plain capitalized word, an acronym or an
/// integration's "Linear: get issue", keeps its letters.
fn after_verb(label: String) -> String {
    let first = label.split_whitespace().next().unwrap_or_default();
    let mut letters = first.chars();
    let phrase = label.contains(char::is_whitespace)
        && !label.contains(':')
        && letters.next().is_some_and(char::is_uppercase)
        && letters.clone().next().is_some()
        && letters.all(char::is_lowercase);
    if !phrase {
        return label;
    }
    let mut chars = label.chars();
    chars
        .next()
        .map(|initial| initial.to_lowercase().chain(chars).collect())
        .unwrap_or(label)
}

/// The duration of a pause as Codex names it, "Wait 5s" or "Wait 1m 5s".
fn codex_wait(name: &str) -> Option<&str> {
    let duration = name.strip_prefix("Wait ")?;
    duration
        .split(' ')
        .all(|part| {
            part.strip_suffix(['s', 'm'])
                .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
        })
        .then_some(duration)
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
/// and its status says what became of it.
pub(super) struct Verb {
    doing: &'static str,
    done: &'static str,
    tried: &'static str,
}

impl Verb {
    pub(super) fn said(&self, status: ToolStatus) -> &'static str {
        match status {
            // An agent waiting on its own background work has not finished.
            ToolStatus::Running | ToolStatus::Waiting => self.doing,
            ToolStatus::Succeeded => self.done,
            ToolStatus::Failed | ToolStatus::Denied | ToolStatus::Stopped | ToolStatus::Ended => {
                self.tried
            }
        }
    }
}

pub(super) const fn verb(doing: &'static str, done: &'static str, tried: &'static str) -> Verb {
    Verb { doing, done, tried }
}

/// What a monitor watches. Its description usually names it ("PR 12 CI
/// checks"); one written as an instruction ("Watch the log") loses the
/// verb the row already says.
fn watched(description: &str) -> &str {
    for lead in ["watching ", "watch ", "monitoring ", "monitor "] {
        if description
            .get(..lead.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(lead))
        {
            return description[lead.len()..].trim_start();
        }
    }
    description
}

/// A tool search's query without its operators: `+slack send` requires
/// "slack" in a tool's name, which a reader reads as "slack send".
fn search_words(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| word.trim_start_matches('+'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl Title {
    /// The row a plain tool call is shown as, read from its input. This
    /// parses the input, so it is formed once when the call changes.
    pub(super) fn of(name: &str, input: &str, status: ToolStatus) -> Self {
        #[cfg(test)]
        READS.with(|reads| reads.set(reads.get() + 1));
        let parsed = fields(input);
        let structured = parsed.is_some();
        let fields = parsed.unwrap_or_default();
        let owned =
            |value: &Option<Cow<'_, str>>| value.as_deref().and_then(short).map(str::to_owned);
        // A verb that leads into what it acted on ("Searched the web for")
        // stands alone when the input does not say what that was.
        let named = |verb: Verb, label: Option<String>| {
            let said = verb.said(status);
            match label.filter(|label| !label.is_empty()) {
                Some(label) => Title {
                    verb: said,
                    label,
                    target: String::new(),
                },
                None => Title {
                    verb: said.strip_suffix(" for").unwrap_or(said),
                    ..Title::default()
                },
            }
        };
        // Codex reports its own pauses by how long they last, as "Wait 5s".
        if let Some(duration) = codex_wait(name) {
            return named(
                verb("Waiting", "Waited", "Tried to wait"),
                Some(duration.to_owned()),
            );
        }
        match name {
            // Codex replacing a long conversation's context with a summary.
            "Compact context" => named(
                verb(
                    "Compacting the context",
                    "Compacted the context",
                    "Tried to compact the context",
                ),
                None,
            ),
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
            // `select:Read,Edit` loads the tools it names rather than
            // searching, and names them as the tools' own identifiers.
            "ToolSearch" => match fields.query.as_deref().map(str::trim) {
                Some(query) if query.starts_with("select:") => {
                    let names: Vec<String> = query["select:".len()..]
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(|name| integration(name).unwrap_or_else(|| name.to_owned()))
                        .collect();
                    named(
                        if names.len() == 1 {
                            verb("Loading tool", "Loaded tool", "Tried to load tool")
                        } else {
                            verb("Loading tools", "Loaded tools", "Tried to load tools")
                        },
                        short(&names.join(", ")).map(str::to_owned),
                    )
                }
                query => match query.map(search_words).as_deref().and_then(short) {
                    Some(words) => named(
                        verb(
                            "Looking up tools for",
                            "Looked up tools for",
                            "Tried to look up tools for",
                        ),
                        Some(words.to_owned()),
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
                fields.todos.as_deref().map(plan_progress),
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
            "Grep" => named(
                verb("Searching for", "Searched for", "Tried to search for"),
                owned(&fields.pattern),
            ),
            "Glob" => named(
                verb("Listing", "Listed", "Tried to list"),
                owned(&fields.pattern),
            ),
            "Bash" => named(
                verb("Running", "Ran", "Tried to run"),
                fields.command.as_deref().map(headline),
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
            "Monitor" => named(
                verb("Watching", "Watched", "Tried to watch"),
                fields
                    .description
                    .as_deref()
                    .and_then(short)
                    .map(|description| watched(description).to_owned())
                    .or_else(|| fields.command.as_deref().map(headline)),
            ),
            "AskUserQuestion" => named(
                verb("Asking", "Asked", "Tried to ask"),
                fields
                    .questions
                    .as_ref()
                    .and_then(|questions| questions.first()?.question.as_deref())
                    .map(headline),
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
            // Claude's lifecycle row for a task it reported before its call:
            // its input is the task's own description, in plain words. A
            // subagent that failed or was stopped still ran; the row's status
            // says how it ended.
            "Subagent task" => named(
                verb("Running a subagent", "Ran a subagent", "Ran a subagent"),
                (!input.trim().is_empty()).then(|| headline(input)),
            ),
            // Any other tool is named, made readable, and its preview offers
            // the one short target its input names.
            _ => Title {
                verb: verb("Using", "Used", "Tried to use").said(status),
                // An integration's name is a proper name; only a plain
                // phrase continues the verb.
                label: integration(name).unwrap_or_else(|| after_verb(readable(name))),
                target: if structured {
                    target(&fields)
                } else {
                    plain(input)
                },
            },
        }
    }
}

/// The short target an unfamiliar tool's input names, shown under its name.
/// A tool's arguments are not evidence of what it does, so only a field that
/// names a target is shown; anything else stays in the call's detail.
/// An input given as plain text rather than named fields, such as an image
/// generator's prompt, when it is short enough to stand on a row as the
/// named fields must be. Freeform text over several lines, such as a patch
/// or a script, is detail for the open row; an empty or null input, or
/// structured input that is not read (still streaming in, or too large),
/// says nothing.
fn plain(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() || text == "null" || text.starts_with(['{', '[']) {
        return String::new();
    }
    if text.starts_with('"') {
        // A string short enough for a row is at most six bytes a character
        // once escaped, so a longer one is not decoded only to be dropped.
        if text.len() > TARGET_LIMIT * 6 + 2 {
            return String::new();
        }
        return match serde_json::from_str::<String>(text) {
            Ok(decoded) => short(&decoded).unwrap_or_default().to_owned(),
            Err(_) => String::new(),
        };
    }
    short(text).unwrap_or_default().to_owned()
}

fn target(fields: &Fields<'_>) -> String {
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
        Title::of(name, &input.to_string(), status).line()
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
                "Watched the log",
            ),
            (
                "Monitor",
                json!({"command":"gh pr checks 12","description":"PR 12 CI checks"}),
                "Watched PR 12 CI checks",
            ),
            (
                "ToolSearch",
                json!({"query":"select:NotebookEdit"}),
                "Loaded tool NotebookEdit",
            ),
            (
                "ToolSearch",
                json!({"query":"select:mcp__claude_ai_Linear__save_issue"}),
                "Loaded tool Linear: save issue",
            ),
            (
                "ToolSearch",
                json!({"query":"+slack send"}),
                "Looked up tools for slack send",
            ),
            (
                "mcp__chrome-devtools__take_screenshot",
                json!({}),
                "Used Chrome DevTools: take screenshot",
            ),
            (
                "mcp__sequential_thinking__think",
                json!({}),
                "Used Sequential Thinking: think",
            ),
            (
                "codex_apps / linear_get_issue",
                json!({}),
                "Used Linear get issue",
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
            ("fixture", json!({}), "Used fixture"),
            ("apply_patch", json!({}), "Used apply patch"),
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
        let titled =
            |status| Title::of("Subagent task", "Survey the tool rows\nand report", status).line();
        assert_eq!(
            titled(ToolStatus::Running),
            "Running a subagent Survey the tool rows"
        );
        // A subagent that failed or was stopped did run; the row's status
        // word says how it ended, so the verb does not claim it only tried.
        for status in [
            ToolStatus::Succeeded,
            ToolStatus::Failed,
            ToolStatus::Stopped,
        ] {
            assert_eq!(titled(status), "Ran a subagent Survey the tool rows");
        }
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
            ("plain prose".to_owned(), "plain prose"),
            ("*** Begin Patch\n*** Update File: a.rs".to_owned(), ""),
        ] {
            let title = Title::of("fixture", &input, ToolStatus::Succeeded);
            assert_eq!(title.target, expected, "{input}");
        }
        // A tool the row names by what it did previews nothing more.
        let skill = Title::of(
            "Skill",
            r#"{"skill":"writing-copy"}"#,
            ToolStatus::Succeeded,
        );
        assert_eq!(skill.target, "");
    }

    /// A call whose input is still streaming is not scanned to its end
    /// only to fail there: an unclosed prefix is not read at all.
    #[test]
    fn an_unclosed_input_is_not_read() {
        let partial = format!(r#"{{"file_path":"a.md","content":"{}"#, "x".repeat(64));
        assert!(fields(&partial).is_none());
        assert_eq!(
            Title::of("Write", &partial, ToolStatus::Running).line(),
            "Writing"
        );
    }

    /// A tool whose input is one short line of plain text, such as an image
    /// generator given a prompt, shows it beneath its name; freeform text
    /// over several lines, and an empty or null input, show nothing rather
    /// than a placeholder.
    #[test]
    fn a_plain_text_input_is_shown_and_an_empty_one_is_not() {
        // As Codex reports an image generation: a named phrase with its
        // prompt as plain text.
        let image = Title::of(
            "Image generation",
            "A fox reading tool rows",
            ToolStatus::Running,
        );
        assert_eq!(image.line(), "Using image generation");
        assert_eq!(image.target, "A fox reading tool rows");
        assert_eq!(Title::unread("Image generation").label, "Image generation");
        for kept in ["GPU Profile", "Linear: get issue", "A"] {
            assert_eq!(after_verb(kept.to_owned()), kept);
        }
        let target = |input: &str| Title::of("image_generation", input, ToolStatus::Succeeded);
        let title = target("A fox reading tool rows");
        assert_eq!(title.line(), "Used image generation");
        assert_eq!(title.target, "A fox reading tool rows");
        assert_eq!(target("\"A fox in the rain\"").target, "A fox in the rain");
        for freeform in ["\"A fox\\nin the rain\"", "First line\nsecond line"] {
            assert_eq!(target(freeform).target, "", "{freeform:?}");
        }
        assert_eq!(target(&"word ".repeat(200)).target, "");
        for nothing in ["", "  ", "null", "{}", "[]", "{\"partial\": \"x"] {
            assert_eq!(target(nothing).target, "", "{nothing:?}");
        }
    }

    /// A member of an unexpected type is passed over on its own: the rest of
    /// the input still titles the row.
    #[test]
    fn an_unexpected_member_leaves_the_others_readable() {
        let done = ToolStatus::Succeeded;
        let title = Title::of(
            "find_symbol",
            &json!({"path":"src/app.rs","query":["x","y"],"limit":5}).to_string(),
            done,
        );
        assert_eq!(title.target, "src/app.rs");
        assert_eq!(
            row(
                "Read",
                json!({"file_path":"src/lib.rs","offset":{"line":3}}),
                done
            ),
            "Read src/lib.rs"
        );
        assert_eq!(
            row(
                "TodoWrite",
                json!({"todos":[{"status":"completed"},{"status":7}]}),
                done
            ),
            "Updated the plan 1 of 2 done"
        );
        assert_eq!(
            row("AskUserQuestion", json!({"questions":"Which one?"}), done),
            "Asked"
        );
        assert_eq!(
            row("Skill", json!({"skill":"tab\tescaped","args":null}), done),
            "Used skill tab\tescaped"
        );
    }

    /// A verb that leads into its target stands alone when the input names
    /// none, rather than ending on "for".
    #[test]
    fn a_missing_target_leaves_a_complete_phrase() {
        for (name, status, expected) in [
            ("WebSearch", ToolStatus::Succeeded, "Searched the web"),
            ("WebSearch", ToolStatus::Running, "Searching the web"),
            ("WebSearch", ToolStatus::Failed, "Tried to search the web"),
            ("Grep", ToolStatus::Succeeded, "Searched"),
            ("ToolSearch", ToolStatus::Succeeded, "Looked up tools"),
        ] {
            for input in [json!({}), json!({"query":""}), json!(null)] {
                assert_eq!(row(name, input.clone(), status), expected, "{name} {input}");
            }
        }
    }

    /// Codex's own pauses and context compaction read as what happened, not
    /// as a tool that was used.
    #[test]
    fn codex_pauses_and_compaction_say_what_happened() {
        for (name, status, expected) in [
            ("Wait 5s", ToolStatus::Running, "Waiting 5s"),
            ("Wait 5s", ToolStatus::Succeeded, "Waited 5s"),
            ("Wait 1m 5s", ToolStatus::Succeeded, "Waited 1m 5s"),
            ("Wait 5s", ToolStatus::Stopped, "Tried to wait 5s"),
            (
                "Compact context",
                ToolStatus::Running,
                "Compacting the context",
            ),
            (
                "Compact context",
                ToolStatus::Succeeded,
                "Compacted the context",
            ),
            (
                "Compact context",
                ToolStatus::Failed,
                "Tried to compact the context",
            ),
            // Not a duration, so not a pause Codex reported.
            (
                "Wait for review",
                ToolStatus::Succeeded,
                "Used wait for review",
            ),
            ("Wait 5", ToolStatus::Succeeded, "Used wait 5"),
        ] {
            assert_eq!(Title::of(name, "", status).line(), expected, "{name}");
        }
    }
}
