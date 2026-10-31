//! Value-only, read-only peer chat projection. Nothing in this module grants
//! access to a source: the host must authenticate and fence every page, and the
//! cache owner must advance its private cursor across omitted items as well.
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::{chat, model::TerminalId};

pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_PAGE_BYTES: usize = 128 * 1024;
pub const MAX_PARTS: usize = 16;
pub const MAX_SCANNED_ITEMS: usize = 128;
pub const MAX_CURSOR_BYTES: usize = 4096;
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_ITEM_ID_BYTES: usize = 512;
pub const MAX_TOOL_NAME_BYTES: usize = 256;
/// Reserved inside MAX_PAGE_BYTES for the matched IPC/CLI response envelope.
/// The writer must also enforce its actual complete-frame bound.
pub const ENVELOPE_BYTES: usize = 1024;
// JSON-escaped identity (512 * 6), name (256 * 6), fixed fields and punctuation.
const PART_RESERVE_BYTES: usize = 6144;

/// An opaque continuation comparison value, never a capability. Its server
/// binding to caller/target/Home/epochs/cache must be checked on every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Cursor(String);

impl Cursor {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty()
            || value.len() > MAX_CURSOR_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        {
            return Err("Invalid peer chat cursor".into());
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Cursor {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Cursor> for String {
    fn from(value: Cursor) -> Self {
        value.0
    }
}

/// Neither value promises uncached provider history was searched or loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Coverage {
    /// The matching cache; whether earlier provider history exists is unknown.
    Cached,
    /// The cache is known to omit an earlier portion of this conversation.
    Partial,
}

/// Deliberately excludes raw tool input, command/recipe/cwd, image paths,
/// attachments, request actions and unknown system/reasoning content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    UserText,
    AssistantText {
        phase: chat::Phase,
    },
    ToolOutput {
        name: String,
        status: chat::ToolStatus,
    },
    CommandOutput {
        status: chat::ToolStatus,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub item_id: String,
    /// Byte offset in the allowlisted text/output field, not in serialized Item.
    pub offset: u64,
    pub text: String,
    /// True at the end of the currently cached field; streaming may append later.
    pub complete: bool,
    pub kind: Kind,
    pub started_at: Option<u64>,
    pub at: Option<u64>,
}

impl Part {
    /// Copies only a bounded UTF-8 prefix from a borrowed, already typed item.
    /// None means omitted/exhausted, not end-of-cache. The scanner still needs
    /// to advance its own source-bound cursor and count the examined item.
    pub fn project(
        item: &chat::Item,
        offset: u64,
        max_text_bytes: usize,
    ) -> Result<Option<Self>, String> {
        use chat::{ItemContent, Role};
        let (text, kind): (&str, Kind) = match (&item.role, &item.content) {
            (
                Role::User,
                ItemContent::Text {
                    text,
                    thinking: false,
                    ..
                },
            ) => (text, Kind::UserText),
            (
                Role::Assistant,
                ItemContent::Text {
                    text,
                    thinking: false,
                    phase,
                },
            ) => (text, Kind::AssistantText { phase: *phase }),
            (
                Role::User | Role::Assistant,
                ItemContent::Tool {
                    name,
                    output,
                    status,
                    ..
                },
            ) => {
                if name.len() > MAX_TOOL_NAME_BYTES || name.chars().any(char::is_control) {
                    return Err("Peer chat tool name exceeds its metadata bound".into());
                }
                (
                    output,
                    Kind::ToolOutput {
                        name: name.clone(),
                        status: *status,
                    },
                )
            }
            (
                Role::User | Role::Assistant,
                ItemContent::Command {
                    output,
                    status,
                    exit_code,
                    ..
                },
            ) => (
                output,
                Kind::CommandOutput {
                    status: *status,
                    exit_code: *exit_code,
                },
            ),
            _ => return Ok(None),
        };
        if item.id.is_empty() || item.id.len() > MAX_ITEM_ID_BYTES {
            return Err("Peer chat item identity exceeds its metadata bound".into());
        }
        let start = usize::try_from(offset).map_err(|_| "Invalid peer chat text offset")?;
        if start > text.len() || !text.is_char_boundary(start) {
            return Err("Invalid peer chat text offset".into());
        }
        if start == text.len() && !text.is_empty() {
            return Ok(None);
        }
        let mut end = start
            .saturating_add(max_text_bytes.min(MAX_TEXT_BYTES))
            .min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start && start < text.len() {
            return Err("Peer chat page has no room for the next UTF-8 character".into());
        }
        Ok(Some(Self {
            item_id: item.id.clone(),
            offset,
            text: text[start..end].into(),
            complete: end == text.len(),
            kind,
            started_at: item.started_at,
            at: item.at,
        }))
    }

    fn validate(&self) -> Result<(), String> {
        if self.item_id.is_empty()
            || self.item_id.len() > MAX_ITEM_ID_BYTES
            || self.text.len() > MAX_TEXT_BYTES
            || self.offset.checked_add(self.text.len() as u64).is_none()
            || (self.text.is_empty() && !self.complete)
        {
            return Err("Invalid peer chat part bounds".into());
        }
        if let Kind::ToolOutput { name, .. } = &self.kind
            && (name.len() > MAX_TOOL_NAME_BYTES || name.chars().any(char::is_control))
        {
            return Err("Invalid peer chat tool name".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub schema_version: u16,
    pub session: TerminalId,
    pub parts: Vec<Part>,
    pub next_cursor: Option<Cursor>,
    /// End of this admitted cache view, not provider completion.
    pub at_end: bool,
    pub coverage: Coverage,
}

impl Page {
    /// A private staging page. Set its final continuation/end state only after
    /// the cache owner's scan, then validate before returning/publishing it.
    pub fn new(session: TerminalId, coverage: Coverage) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session,
            parts: Vec::new(),
            next_cursor: None,
            at_end: false,
            coverage,
        }
    }

    /// Conservative chunk allowance: JSON can expand one byte to six bytes.
    /// Reserve worst-case cursor and bounded per-part metadata before copying.
    pub fn remaining_text_bytes(&self) -> Result<usize, String> {
        if self.parts.len() >= MAX_PARTS {
            return Ok(0);
        }
        let used = self.encoded_bytes()?;
        Ok(MAX_PAGE_BYTES
            .saturating_sub(ENVELOPE_BYTES + MAX_CURSOR_BYTES + PART_RESERVE_BYTES + used)
            .saturating_div(6)
            .min(MAX_TEXT_BYTES))
    }

    /// False means keep this same source position for the next page. This
    /// method never discards a part or changes the source scanner's cursor.
    pub fn try_push(&mut self, part: Part) -> Result<bool, String> {
        part.validate()?;
        if self.parts.len() >= MAX_PARTS {
            return Ok(false);
        }
        self.parts.push(part);
        let fits = self
            .encoded_bytes()
            .is_ok_and(|used| used + ENVELOPE_BYTES + MAX_CURSOR_BYTES < MAX_PAGE_BYTES);
        if !fits {
            self.parts.pop();
        }
        Ok(fits)
    }

    /// Final value/budget barrier only; not an authorization or source fence.
    pub fn validate_for(&self, session: TerminalId) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION || self.session != session {
            return Err("Peer chat page does not match the requested session/schema".into());
        }
        if self.parts.len() > MAX_PARTS || self.at_end == self.next_cursor.is_some() {
            return Err("Invalid peer chat page continuation".into());
        }
        for part in &self.parts {
            part.validate()?;
        }
        if self.encoded_bytes()? + ENVELOPE_BYTES > MAX_PAGE_BYTES {
            return Err("Peer chat page exceeds its encoded byte budget".into());
        }
        Ok(())
    }

    /// Counts without allocating a serialized body; stops at the hard cap.
    fn encoded_bytes(&self) -> Result<usize, String> {
        let mut json = Counter(0);
        serde_json::to_writer(&mut json, self)
            .map_err(|_| "Peer chat page exceeds its encoded byte budget")?;
        let mut wire = Counter(0);
        postcard::to_io(self, &mut wire)
            .map_err(|_| "Peer chat page exceeds its encoded byte budget")?;
        Ok(json.0.max(wire.0))
    }
}

struct Counter(usize);

impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|count| *count <= MAX_PAGE_BYTES)
            .ok_or_else(|| io::Error::other("peer chat page byte budget"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: chat::ItemContent) -> chat::Item {
        chat::Item {
            id: "item-1".into(),
            role: chat::Role::Assistant,
            content,
            started_at: Some(1),
            at: Some(2),
        }
    }

    fn text(value: &str) -> chat::Item {
        item(chat::ItemContent::Text {
            text: value.into(),
            thinking: false,
            phase: chat::Phase::Commentary,
        })
    }

    #[test]
    fn allowlist_preserves_visible_output_and_excludes_private_fields() {
        let tool = item(chat::ItemContent::Tool {
            name: "Read".into(),
            input: "PRIVATE_INPUT".into(),
            output: "visible tool".into(),
            status: chat::ToolStatus::Succeeded,
        });
        let command = item(chat::ItemContent::Command {
            label: "PRIVATE_LABEL".into(),
            command: "PRIVATE_ARGV".into(),
            working_directory: "PRIVATE_CWD".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Failed,
            exit_code: Some(3),
            duration_ms: Some(10),
            output: "visible command".into(),
        });
        let mut page = Page::new(TerminalId::new(), Coverage::Cached);
        for value in [&tool, &command, &text("visible assistant")] {
            assert!(
                page.try_push(Part::project(value, 0, 1024).unwrap().unwrap())
                    .unwrap()
            );
        }
        page.at_end = true;
        page.validate_for(page.session).unwrap();
        let json = serde_json::to_string(&page).unwrap();
        assert!(
            json.contains("visible tool")
                && json.contains("visible command")
                && json.contains("visible assistant")
        );
        for secret in [
            "PRIVATE_INPUT",
            "PRIVATE_LABEL",
            "PRIVATE_ARGV",
            "PRIVATE_CWD",
        ] {
            assert!(!json.contains(secret), "{secret}");
        }
        assert!(matches!(
            page.parts[1].kind,
            Kind::CommandOutput {
                exit_code: Some(3),
                status: chat::ToolStatus::Failed
            }
        ));
        let mut user = text("visible user");
        user.role = chat::Role::User;
        assert!(matches!(
            Part::project(&user, 0, 1024).unwrap().unwrap().kind,
            Kind::UserText
        ));
        for omitted in [
            item(chat::ItemContent::Text {
                text: "PRIVATE_THINKING".into(),
                thinking: true,
                phase: chat::Phase::Final,
            }),
            chat::Item {
                role: chat::Role::System,
                ..text("PRIVATE_SYSTEM")
            },
            item(chat::ItemContent::Image {
                path: "PRIVATE_IMAGE".into(),
                description: "image".into(),
                reference: None,
            }),
            item(chat::ItemContent::Delivery {
                kind: chat::DeliveryKind::Message,
                from: "private".into(),
                sender: "PRIVATE_AUTHORITY".into(),
                summary: "summary".into(),
                body: "body".into(),
            }),
        ] {
            assert_eq!(Part::project(&omitted, 0, 1024).unwrap(), None);
        }
    }

    #[test]
    fn utf8_chunks_reconstruct_exact_field_and_reject_nonprogress_offsets() {
        let value = text("A🦀é\nZ");
        let mut offset = 0;
        let mut reconstructed = String::new();
        while let Some(part) = Part::project(&value, offset, 4).unwrap() {
            assert!(!part.text.is_empty());
            offset += part.text.len() as u64;
            reconstructed.push_str(&part.text);
            if part.complete {
                break;
            }
        }
        assert_eq!(reconstructed, "A🦀é\nZ");
        assert!(Part::project(&value, 2, 8).is_err());
        assert!(Part::project(&value, u64::MAX, 8).is_err());
        assert!(Part::project(&value, 1, 3).is_err());
        assert!(Part::project(&value, 0, 0).is_err());
        assert_eq!(Part::project(&value, offset, 4).unwrap(), None);
    }

    #[test]
    fn empty_tool_status_is_visible_but_unfinished_empty_parts_are_invalid() {
        let value = item(chat::ItemContent::Tool {
            name: "Read".into(),
            input: "private".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        });
        let mut part = Part::project(&value, 0, 0).unwrap().unwrap();
        assert!(part.complete && part.text.is_empty());
        part.validate().unwrap();
        part.complete = false;
        assert!(part.validate().is_err());
    }

    #[test]
    fn page_budget_counts_json_escaping_metadata_cursor_and_part_limit() {
        let value = text(&"\u{0001}".repeat(MAX_PAGE_BYTES));
        let mut page = Page::new(TerminalId::new(), Coverage::Partial);
        let mut offset = 0;
        loop {
            let allowance = page.remaining_text_bytes().unwrap();
            if allowance == 0 {
                break;
            }
            let part = Part::project(&value, offset, allowance).unwrap().unwrap();
            offset += part.text.len() as u64;
            assert!(page.try_push(part).unwrap());
        }
        assert!(offset > 0 && offset < MAX_PAGE_BYTES as u64);
        page.next_cursor = Some(Cursor::parse(&"a".repeat(MAX_CURSOR_BYTES)).unwrap());
        page.validate_for(page.session).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() + ENVELOPE_BYTES <= MAX_PAGE_BYTES);
        assert!(postcard::to_stdvec(&page).unwrap().len() + ENVELOPE_BYTES <= MAX_PAGE_BYTES);
        let mut over = page.clone();
        over.parts = vec![
            Part::project(&text(&"\u{0001}".repeat(MAX_TEXT_BYTES)), 0, MAX_TEXT_BYTES)
                .unwrap()
                .unwrap(),
        ];
        assert!(over.validate_for(over.session).is_err());
        let mut refused = Page::new(page.session, Coverage::Cached);
        let before = refused.clone();
        assert!(!refused.try_push(over.parts[0].clone()).unwrap());
        assert_eq!(
            refused, before,
            "encoded-budget refusal must retain source position"
        );
        let mut small = Page::new(page.session, Coverage::Cached);
        for _ in 0..MAX_PARTS {
            assert!(
                small
                    .try_push(Part::project(&text("a"), 0, 1).unwrap().unwrap())
                    .unwrap()
            );
        }
        assert!(
            !small
                .try_push(Part::project(&text("b"), 0, 1).unwrap().unwrap())
                .unwrap()
        );
        assert_eq!(small.parts.len(), MAX_PARTS);
    }

    #[test]
    fn invalid_cursor_metadata_target_and_continuation_fail_closed() {
        for invalid in ["", "a/b", "../path", "a b", "a\n", "🦀"] {
            assert!(Cursor::parse(invalid).is_err());
            assert!(
                serde_json::from_str::<Cursor>(&serde_json::to_string(invalid).unwrap()).is_err()
            );
            assert!(
                postcard::from_bytes::<Cursor>(&postcard::to_stdvec(&invalid).unwrap()).is_err()
            );
        }
        assert!(Cursor::parse(&"a".repeat(MAX_CURSOR_BYTES + 1)).is_err());
        let cursor = Cursor::parse("v1:abc_DEF-09.xyz").unwrap();
        assert_eq!(
            serde_json::from_str::<Cursor>(&serde_json::to_string(&cursor).unwrap()).unwrap(),
            cursor
        );
        let mut page = Page::new(TerminalId::new(), Coverage::Cached);
        assert!(page.validate_for(page.session).is_err());
        page.next_cursor = Some(cursor);
        page.validate_for(page.session).unwrap();
        assert!(page.validate_for(TerminalId::new()).is_err());
        page.at_end = true;
        assert!(page.validate_for(page.session).is_err());
        let mut huge = text("safe");
        huge.id = "x".repeat(MAX_ITEM_ID_BYTES + 1);
        assert!(Part::project(&huge, 0, 8).is_err());
        let huge_name = item(chat::ItemContent::Tool {
            name: "x".repeat(MAX_TOOL_NAME_BYTES + 1),
            input: String::new(),
            output: "safe".into(),
            status: chat::ToolStatus::Succeeded,
        });
        assert!(Part::project(&huge_name, 0, 8).is_err());
    }
}
