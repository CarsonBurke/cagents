//! A compact, read-only page of another session's conversation.
//!
//! The daemon reads the conversation from the session's own provider history
//! and condenses it here, so an agent picking up a peer's work receives the
//! messages that matter without thinking text, tool output or provider
//! framing. Reading confers no control, exactly like listing a session.

use serde::{Deserialize, Serialize};

use crate::chat::{self, DeliveryKind, Item, ItemContent, Role, ToolStatus};

/// Messages a page holds unless the caller asks for more or fewer.
pub const DEFAULT_LIMIT: u16 = 10;
pub const MAX_LIMIT: u16 = 200;
/// A user or assistant message, or a delivery, longer than this is clipped
/// unless the caller asks for full text.
pub const MESSAGE_BYTES: usize = 2_000;
/// Provider context, such as a compaction summary or loaded instructions, is
/// usually long and rarely what the reader came for.
pub const NOTICE_BYTES: usize = 600;
/// One line describing a tool call.
pub const TOOL_SUMMARY_BYTES: usize = 160;
/// A tool's output, when the caller asks for tools.
pub const TOOL_OUTPUT_BYTES: usize = 600;
/// What one reply carries at most, whatever the options. Older entries
/// beyond it remain readable from the page's `earlier` cursor.
pub const MAX_PAGE_BYTES: usize = 512 * 1024;
/// Edited paths a compact tool summary names before counting the rest.
const EDITED_PATHS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    /// The session, named as `cagents send` names a recipient: at least four
    /// characters of its session ID, lineage or agent ID, or `parent`.
    pub session: String,
    /// An entry ID from an earlier page's `earlier`; the page ends just before it.
    pub before: Option<String>,
    /// Messages to return; tool calls between them come along.
    pub limit: u16,
    /// List each tool call with its output instead of summarizing a run of them.
    pub tools: bool,
    /// Never clip a message, notice or tool output; the page budget still applies.
    pub full: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// The session as `cagents sessions --channel` lists it.
    pub session: super::directory::Member,
    /// Oldest first, ending at the newest entry or just before `before`.
    pub entries: Vec<Entry>,
    /// Pass as `before` to read the entries preceding this page. Absent when
    /// the page begins at the start of the provider's saved history.
    pub earlier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The provider item's stable ID, usable as a `before` cursor.
    pub id: String,
    /// Unix milliseconds when the daemon saw this entry, if it did; replayed
    /// provider history has no time.
    pub at: Option<u64>,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    /// What the person at the conversation sent.
    User(Text),
    Assistant {
        text: Text,
        /// Progress narration rather than the answer the turn was asked for.
        commentary: bool,
    },
    /// A message, report or notice delivered to the conversation by another
    /// session, a subagent or the provider itself. The user wrote none of these.
    Delivery {
        kind: DeliveryKind,
        /// The sender's name, or a short ID when it has none.
        from: String,
        summary: String,
        body: Text,
    },
    /// One tool call, listed when the caller asks for tools.
    Tool(Tool),
    /// A run of consecutive tool calls, summarized.
    Tools(ToolRun),
}

/// Text, possibly clipped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
    /// Bytes removed from the end; zero when the text is complete.
    pub omitted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    /// The provider's tool name, or Run, Read, Search, List, Edit or WebSearch
    /// for the calls cagents recognizes.
    pub name: String,
    /// What it acted on: a command, path, pattern, query or description.
    pub summary: String,
    pub status: ToolStatus,
    pub output: Option<Text>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRun {
    /// How many calls each tool received, most used first.
    pub calls: Vec<ToolCount>,
    pub failed: u32,
    /// Paths the run edited, newest first, at most eight.
    pub edited: Vec<String>,
    /// Edited paths beyond those listed.
    pub more_edited: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCount {
    pub name: String,
    pub count: u32,
}

impl Text {
    fn clipped(text: &str, limit: Option<usize>) -> Self {
        let text = text.trim_end();
        let Some(limit) = limit.filter(|limit| text.len() > *limit) else {
            return Self {
                text: text.into(),
                omitted: 0,
            };
        };
        let mut kept = text.to_owned();
        super::truncate(&mut kept, limit);
        // A clipped line ending mid-word reads as a typo; end on a line or a
        // word when one is close.
        let floor = kept.len() * 3 / 4;
        if let Some(end) = kept.rfind('\n').or_else(|| kept.rfind(' '))
            && end >= floor
        {
            kept.truncate(end);
        }
        let kept = kept.trim_end().to_owned();
        Self {
            omitted: text.len() - kept.len(),
            text: kept,
        }
    }

    fn bytes(&self) -> usize {
        self.text.len()
    }
}

impl Content {
    /// Messages count against a page's limit; tools and notices come along.
    pub fn is_message(&self) -> bool {
        match self {
            Self::User(_) | Self::Assistant { .. } => true,
            Self::Delivery { kind, .. } => *kind != DeliveryKind::Notice,
            Self::Tool(_) | Self::Tools(_) => false,
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::User(text) | Self::Assistant { text, .. } => text.bytes(),
            Self::Delivery {
                from,
                summary,
                body,
                ..
            } => from.len() + summary.len() + body.bytes(),
            Self::Tool(tool) => {
                tool.name.len() + tool.summary.len() + tool.output.as_ref().map_or(0, Text::bytes)
            }
            Self::Tools(run) => {
                run.calls
                    .iter()
                    .map(|call| call.name.len() + 8)
                    .sum::<usize>()
                    + run.edited.iter().map(String::len).sum::<usize>()
            }
        }
    }
}

/// A tool call condensed to its name, one line of what it acted on, and its output.
fn tool(content: &ItemContent) -> Option<(Tool, Vec<String>)> {
    let (name, summary, status, output, edited) = match content {
        ItemContent::Command {
            label,
            command,
            action,
            status,
            exit_code,
            output,
            ..
        } => {
            let name = match action {
                chat::CommandAction::Run => "Run",
                chat::CommandAction::Read => "Read",
                chat::CommandAction::Search => "Search",
                chat::CommandAction::List => "List",
            };
            // A command is more exact than the provider's description of it.
            let summary = if command.trim().is_empty() {
                label
            } else {
                command
            };
            let status = match exit_code {
                Some(code) if *code != 0 && *status == ToolStatus::Succeeded => ToolStatus::Failed,
                _ => *status,
            };
            (
                name.to_owned(),
                summary.clone(),
                status,
                output.as_str(),
                Vec::new(),
            )
        }
        ItemContent::Changes { changes, status } => {
            let paths: Vec<String> = changes.iter().map(|change| change.path.clone()).collect();
            ("Edit".to_owned(), paths.join(", "), *status, "", paths)
        }
        ItemContent::WebSearch { query, .. } => (
            "WebSearch".to_owned(),
            query.clone(),
            ToolStatus::Succeeded,
            "",
            Vec::new(),
        ),
        ItemContent::Tool {
            name,
            input,
            output,
            status,
        } => (
            name.clone(),
            input_summary(input),
            *status,
            output.as_str(),
            Vec::new(),
        ),
        ItemContent::Image {
            description, path, ..
        } => (
            "Image".to_owned(),
            if description.is_empty() {
                path.clone()
            } else {
                description.clone()
            },
            ToolStatus::Succeeded,
            "",
            Vec::new(),
        ),
        ItemContent::Text { .. } | ItemContent::Delivery { .. } => return None,
    };
    Some((
        Tool {
            name,
            summary: one_line(&summary, TOOL_SUMMARY_BYTES),
            status,
            output: (!output.trim().is_empty()).then(|| Text {
                text: output.to_owned(),
                omitted: 0,
            }),
        },
        edited,
    ))
}

/// Tool input is usually a JSON object; its most descriptive field says
/// what the call did better than the whole object.
fn input_summary(input: &str) -> String {
    const FIELDS: &[&str] = &[
        "description",
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "skill",
        "subject",
        "prompt",
    ];
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str(input) else {
        return input.to_owned();
    };
    FIELDS
        .iter()
        .find_map(|field| {
            object
                .get(*field)?
                .as_str()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| object.values().find_map(|value| value.as_str()))
        .map_or_else(String::new, str::to_owned)
}

fn one_line(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped = Text::clipped(&line, Some(limit));
    if clipped.omitted > 0 {
        format!("{}…", clipped.text)
    } else {
        clipped.text
    }
}

/// Builds one page from items offered newest first.
pub struct Builder {
    before: Option<String>,
    limit: usize,
    tools: bool,
    full: bool,
    /// Newest first until `finish`.
    entries: Vec<Entry>,
    messages: usize,
    bytes: usize,
    /// The ID of the oldest item this page covers.
    oldest: Option<String>,
    full_page: bool,
}

impl Builder {
    pub fn new(query: &Query) -> Self {
        Self {
            before: query.before.clone(),
            limit: usize::from(query.limit.clamp(1, MAX_LIMIT)),
            tools: query.tools,
            full: query.full,
            entries: Vec::new(),
            messages: 0,
            bytes: 0,
            oldest: None,
            full_page: false,
        }
    }

    /// Whether the cursor has been reached, so items now belong on the page.
    pub fn found_cursor(&self) -> bool {
        self.before.is_none()
    }

    pub fn is_full(&self) -> bool {
        self.full_page
    }

    /// Offer the next older item. Returns false once the page is full; the
    /// item that did not fit belongs to the next page.
    pub fn push(&mut self, item: &Item) -> bool {
        if self.full_page {
            return false;
        }
        if let Some(cursor) = &self.before {
            if item.id == *cursor {
                // Until an older item joins the page, the cursor itself
                // still marks where the next page begins.
                self.oldest = self.before.take();
            }
            return true;
        }
        let Some(content) = self.condense(item) else {
            // Thinking and empty text are covered without an entry.
            self.oldest = Some(item.id.clone());
            return true;
        };
        let message = content.is_message();
        if message && self.messages == self.limit {
            self.full_page = true;
            return false;
        }
        let bytes = content.bytes();
        if self.bytes + bytes > MAX_PAGE_BYTES && !self.entries.is_empty() {
            self.full_page = true;
            return false;
        }
        let content = if self.entries.is_empty() && bytes > MAX_PAGE_BYTES {
            clip_to(content, MAX_PAGE_BYTES)
        } else {
            content
        };
        // A run of tool calls folds into the newer run it continues.
        if let (
            Content::Tools(run),
            Some(Entry {
                content: Content::Tools(newer),
                id,
                at,
            }),
        ) = (&content, self.entries.last_mut())
        {
            let before = Content::Tools(newer.clone()).bytes();
            merge(newer, run);
            *id = item.id.clone();
            *at = item.started_at.or(item.at).or(*at);
            self.bytes = self.bytes - before + Content::Tools(newer.clone()).bytes();
        } else {
            self.bytes += content.bytes();
            self.entries.push(Entry {
                id: item.id.clone(),
                at: item.started_at.or(item.at),
                content,
            });
        }
        self.messages += usize::from(message);
        self.oldest = Some(item.id.clone());
        true
    }

    fn condense(&self, item: &Item) -> Option<Content> {
        let limit = |bytes| (!self.full).then_some(bytes);
        match &item.content {
            ItemContent::Text {
                text,
                thinking: false,
                phase,
            } if !text.trim().is_empty() => Some(match item.role {
                Role::User => Content::User(Text::clipped(text, limit(MESSAGE_BYTES))),
                Role::Assistant => Content::Assistant {
                    text: Text::clipped(text, limit(MESSAGE_BYTES)),
                    commentary: *phase == chat::Phase::Commentary,
                },
                Role::System => Content::Delivery {
                    kind: DeliveryKind::Notice,
                    from: String::new(),
                    summary: String::new(),
                    body: Text::clipped(text, limit(NOTICE_BYTES)),
                },
            }),
            ItemContent::Text { .. } => None,
            ItemContent::Delivery {
                kind,
                from,
                sender,
                summary,
                body,
            } => Some(Content::Delivery {
                kind: *kind,
                from: if from.is_empty() && !sender.is_empty() {
                    chat::delivery_sender(from, sender)
                } else {
                    from.clone()
                },
                summary: one_line(summary, TOOL_SUMMARY_BYTES),
                body: Text::clipped(
                    body,
                    limit(if *kind == DeliveryKind::Notice {
                        NOTICE_BYTES
                    } else {
                        MESSAGE_BYTES
                    }),
                ),
            }),
            content => {
                let (mut tool, edited) = tool(content)?;
                if self.tools {
                    tool.output = tool
                        .output
                        .map(|output| Text::clipped(&output.text, limit(TOOL_OUTPUT_BYTES)));
                    Some(Content::Tool(tool))
                } else {
                    let mut run = ToolRun {
                        calls: vec![ToolCount {
                            name: tool.name,
                            count: 1,
                        }],
                        failed: u32::from(matches!(
                            tool.status,
                            ToolStatus::Failed | ToolStatus::Denied
                        )),
                        ..ToolRun::default()
                    };
                    add_edited(&mut run, edited);
                    Some(Content::Tools(run))
                }
            }
        }
    }

    /// The finished page, oldest first. `exhausted` says the history held
    /// nothing older than the last item offered.
    pub fn finish(mut self, exhausted: bool) -> Result<(Vec<Entry>, Option<String>), String> {
        if let Some(cursor) = self.before {
            return Err(format!(
                "Entry {cursor} is no longer in this conversation's history; read the newest page again"
            ));
        }
        for entry in &mut self.entries {
            if let Content::Tools(run) = &mut entry.content {
                run.calls
                    .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
            }
        }
        self.entries.reverse();
        let earlier = if self.full_page || !exhausted {
            self.oldest
        } else {
            None
        };
        Ok((self.entries, earlier))
    }
}

fn merge(newer: &mut ToolRun, older: &ToolRun) {
    for call in &older.calls {
        match newer.calls.iter_mut().find(|known| known.name == call.name) {
            Some(known) => known.count += call.count,
            None => newer.calls.push(call.clone()),
        }
    }
    newer.failed += older.failed;
    add_edited(newer, older.edited.clone());
    newer.more_edited += older.more_edited;
}

fn add_edited(run: &mut ToolRun, paths: Vec<String>) {
    for path in paths {
        if run.edited.contains(&path) {
            continue;
        }
        if run.edited.len() < EDITED_PATHS {
            run.edited.push(path);
        } else {
            run.more_edited += 1;
        }
    }
}

/// The first entry of a page always fits, however large it is.
fn clip_to(content: Content, bytes: usize) -> Content {
    let clip = |text: Text| {
        let mut clipped = Text::clipped(&text.text, Some(bytes / 2));
        clipped.omitted += text.omitted;
        clipped
    };
    match content {
        Content::User(text) => Content::User(clip(text)),
        Content::Assistant { text, commentary } => Content::Assistant {
            text: clip(text),
            commentary,
        },
        Content::Delivery {
            kind,
            from,
            summary,
            body,
        } => Content::Delivery {
            kind,
            from,
            summary,
            body: clip(body),
        },
        Content::Tool(mut tool) => {
            tool.output = tool.output.map(clip);
            Content::Tool(tool)
        }
        Content::Tools(run) => Content::Tools(run),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, role: Role, content: ItemContent) -> Item {
        Item {
            started_at: None,
            id: id.into(),
            role,
            content,
            at: None,
        }
    }

    fn text(id: &str, role: Role, text: &str) -> Item {
        item(
            id,
            role,
            ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        )
    }

    fn command(id: &str, command: &str) -> Item {
        item(
            id,
            Role::Assistant,
            ItemContent::Command {
                label: "Run the tests".into(),
                command: command.into(),
                working_directory: "/repo".into(),
                action: chat::CommandAction::Run,
                status: ToolStatus::Succeeded,
                exit_code: Some(0),
                duration_ms: None,
                output: "ok\n".repeat(400),
            },
        )
    }

    fn edit(id: &str, path: &str) -> Item {
        item(
            id,
            Role::Assistant,
            ItemContent::Changes {
                changes: vec![chat::FileChange {
                    path: path.into(),
                    kind: chat::ChangeKind::Updated,
                    diff: "-a\n+b\n".into(),
                }],
                status: ToolStatus::Succeeded,
            },
        )
    }

    fn query(limit: u16) -> Query {
        Query {
            session: "abcd".into(),
            before: None,
            limit,
            tools: false,
            full: false,
        }
    }

    /// Items as history supplies them: oldest first, offered newest first.
    fn build(query: &Query, items: &[Item]) -> (Vec<Entry>, Option<String>) {
        let mut builder = Builder::new(query);
        let mut exhausted = true;
        for item in items.iter().rev() {
            if !builder.push(item) {
                exhausted = false;
                break;
            }
        }
        builder.finish(exhausted).unwrap()
    }

    fn conversation() -> Vec<Item> {
        vec![
            text("u1", Role::User, "Fix the login test"),
            item(
                "think",
                Role::Assistant,
                ItemContent::Text {
                    text: "private reasoning".into(),
                    thinking: true,
                    phase: chat::Phase::Final,
                },
            ),
            command("c1", "cargo test login"),
            edit("e1", "src/auth.rs"),
            command("c2", "cargo test login"),
            edit("e2", "src/auth.rs"),
            edit("e3", "tests/login.rs"),
            text("a1", Role::Assistant, "Fixed: the token expired early."),
            text("u2", Role::User, "Open a PR"),
            text("a2", Role::Assistant, "Opened #12."),
        ]
    }

    #[test]
    fn compact_page_keeps_messages_and_folds_tool_runs_without_thinking_or_output() {
        let (entries, earlier) = build(&query(10), &conversation());
        assert_eq!(earlier, None, "the whole conversation fit");
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, ["u1", "c1", "a1", "u2", "a2"]);
        let Content::Tools(run) = &entries[1].content else {
            panic!("{:?}", entries[1]);
        };
        assert_eq!(
            run.calls,
            [
                ToolCount {
                    name: "Edit".into(),
                    count: 3
                },
                ToolCount {
                    name: "Run".into(),
                    count: 2
                }
            ]
        );
        assert_eq!(run.edited, ["tests/login.rs", "src/auth.rs"]);
        let serialized = serde_json::to_string(&entries).unwrap();
        assert!(!serialized.contains("private reasoning"));
        assert!(!serialized.contains("ok\\nok"), "tool output stays out");
    }

    #[test]
    fn limit_counts_messages_and_the_cursor_reads_the_preceding_page() {
        let items = conversation();
        let (entries, earlier) = build(&query(2), &items);
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, ["u2", "a2"]);
        assert_eq!(earlier.as_deref(), Some("u2"));

        let mut previous = query(2);
        previous.before = earlier;
        let (entries, earlier) = build(&previous, &items);
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(
            ids,
            ["u1", "c1", "a1"],
            "tools between two messages come along"
        );
        assert_eq!(earlier, None, "the first message starts the conversation");

        let mut missing = query(2);
        missing.before = Some("rewritten".into());
        let mut builder = Builder::new(&missing);
        for item in items.iter().rev() {
            builder.push(item);
        }
        assert!(builder.finish(true).unwrap_err().contains("rewritten"));
    }

    #[test]
    fn a_page_ending_in_unread_history_offers_a_cursor() {
        let items = conversation();
        let mut builder = Builder::new(&query(10));
        // Only the newest provider page has been offered so far.
        for item in items[7..].iter().rev() {
            assert!(builder.push(item));
        }
        let (entries, earlier) = builder.finish(false).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(earlier.as_deref(), Some("a1"));
    }

    #[test]
    fn tools_option_lists_each_call_with_clipped_output() {
        let mut query = query(10);
        query.tools = true;
        let (entries, _) = build(&query, &conversation());
        let Content::Tool(tool) = &entries[1].content else {
            panic!("{:?}", entries[1]);
        };
        assert_eq!(tool.name, "Run");
        assert_eq!(tool.summary, "cargo test login");
        let output = tool.output.as_ref().unwrap();
        assert!(output.text.len() <= TOOL_OUTPUT_BYTES);
        assert_eq!(
            output.text.len() + output.omitted,
            "ok\n".repeat(400).trim_end().len()
        );
        assert_eq!(entries.len(), 9);
    }

    #[test]
    fn long_text_is_clipped_unless_full_and_notices_are_clipped_harder() {
        let long = "word ".repeat(1_000);
        let items = vec![
            item(
                "summary",
                Role::System,
                ItemContent::Delivery {
                    kind: DeliveryKind::Notice,
                    from: String::new(),
                    sender: String::new(),
                    summary: "Claude context".into(),
                    body: long.clone(),
                },
            ),
            text("a", Role::Assistant, &long),
        ];
        let (entries, _) = build(&query(10), &items);
        let Content::Delivery { body, .. } = &entries[0].content else {
            panic!()
        };
        assert!(body.text.len() <= NOTICE_BYTES && body.omitted > 0);
        assert!(!body.text.ends_with(' '));
        let Content::Assistant { text, .. } = &entries[1].content else {
            panic!()
        };
        assert!(text.text.len() <= MESSAGE_BYTES);
        assert_eq!(text.text.len() + text.omitted, long.trim_end().len());
        assert!(
            !entries[0].content.is_message(),
            "notices do not use up the limit"
        );

        let mut full = query(10);
        full.full = true;
        let (entries, _) = build(&full, &items);
        let Content::Assistant { text, .. } = &entries[1].content else {
            panic!()
        };
        assert_eq!(text.omitted, 0);
    }

    #[test]
    fn tool_input_summary_prefers_descriptive_fields() {
        assert_eq!(
            input_summary(
                r#"{"subagent_type":"Explore","description":"Find callers","prompt":"long"}"#
            ),
            "Find callers"
        );
        assert_eq!(input_summary(r#"{"other":"value"}"#), "value");
        assert_eq!(input_summary("raw input"), "raw input");
        assert_eq!(one_line("a\n  b\tc", 100), "a b c");
        assert!(one_line(&"x".repeat(400), TOOL_SUMMARY_BYTES).ends_with('…'));
    }

    #[test]
    fn the_page_budget_bounds_full_text_and_still_returns_the_newest_entry() {
        let huge = "y".repeat(MAX_PAGE_BYTES * 2);
        let items = vec![
            text("older", Role::Assistant, &huge),
            text("old", Role::User, "earlier"),
            text("big", Role::Assistant, &huge),
        ];
        let mut query = query(10);
        query.full = true;
        let (entries, earlier) = build(&query, &items);
        // The newest entry is clipped to leave room for what precedes it; an
        // older one that no longer fits waits for the next page.
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["old", "big"]
        );
        let Content::Assistant { text, .. } = &entries[1].content else {
            panic!()
        };
        assert!(text.text.len() <= MAX_PAGE_BYTES / 2 && text.omitted > 0);
        assert_eq!(earlier.as_deref(), Some("old"));
    }
}
