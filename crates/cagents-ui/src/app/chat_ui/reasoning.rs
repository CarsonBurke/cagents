//! Plain one-line summaries, prepared with the message rather than per frame.

use crate::chat::ItemContent;
use pulldown_cmark::{Event, Options, Parser};

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Preview {
    pub(super) first: String,
    pub(super) latest: String,
}

impl Preview {
    pub(super) fn of(content: &ItemContent) -> Option<Self> {
        match content {
            ItemContent::Text {
                text,
                thinking: true,
                ..
            } => Some(Self::new(text)),
            _ => None,
        }
    }

    pub(super) fn new(text: &str) -> Self {
        // Claude uses this canonical placeholder for redacted_thinking blocks.
        if text.trim() == "[Redacted thinking]" {
            return Self::default();
        }
        let first = text.lines().find_map(plain_line).unwrap_or_default();
        let latest = text.lines().rev().find_map(plain_line).unwrap_or_default();
        Self { first, latest }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.first.capacity() + self.latest.capacity()
    }
}

// More than any visible row can show; expanded prose keeps the full source.
const PREVIEW_CHARS: usize = 1024;

fn plain_line(line: &str) -> Option<String> {
    if line.chars().all(|character| {
        character.is_whitespace() || matches!(character, '*' | '_' | '#' | '>' | '-' | '`')
    }) {
        return None;
    }
    let mut plain = String::new();
    let mut remaining = PREVIEW_CHARS;
    for event in Parser::new_ext(line.trim(), Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                for character in text.chars().take(remaining) {
                    plain.push(character);
                    remaining -= 1;
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                plain.push(' ');
                remaining -= 1;
            }
            _ => {}
        }
        if remaining == 0 {
            break;
        }
    }
    let plain = plain.trim();
    (!plain.is_empty()).then(|| plain.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_choose_readable_first_and_latest_lines_without_markdown() {
        let preview = Preview::new(
            "\n# **Checking permissions**\n\n> - Retry the `probe` with [new settings](https://example.test).\n\n",
        );
        assert_eq!(preview.first, "Checking permissions");
        assert_eq!(preview.latest, "Retry the probe with new settings.");
        assert_eq!(Preview::new("***\n\n**A thought**\n___").first, "A thought");
        assert_eq!(
            Preview::new("Inspect Option<String>").first,
            "Inspect Option<String>"
        );
        assert_eq!(
            Preview::new("Keep snake_case and 2 * 3").first,
            "Keep snake_case and 2 * 3"
        );
        assert_eq!(
            Preview::new("  ~~Old~~ **new** &amp; useful  ").latest,
            "Old new & useful"
        );
    }

    #[test]
    fn a_long_single_line_has_bounded_preview_storage_without_ellipsis() {
        let source = format!("**{}**", "思".repeat(100_000));
        let preview = Preview::new(&source);
        assert_eq!(preview.first, "思".repeat(PREVIEW_CHARS));
        assert_eq!(preview.latest, preview.first);
        assert!(preview.retained_bytes() <= 2 * PREVIEW_CHARS * 4);
    }

    #[test]
    fn empty_and_redacted_reasoning_has_no_disclosure_text() {
        for source in [
            "",
            "\n  \t\r\n",
            "** **",
            "[Redacted thinking]",
            "\n[Redacted thinking]\n",
        ] {
            assert_eq!(Preview::new(source), Preview::default(), "{source:?}");
        }
    }
}
