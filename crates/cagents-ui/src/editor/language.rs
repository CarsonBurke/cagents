//! What a text buffer reads as when its name does not say.
//!
//! A file's name decides first: an extension, a `README`, or a dotfile names
//! its language and nothing here second-guesses it. A buffer whose name says
//! nothing, such as an untitled note or a `NOTES` file, is read from its
//! content instead, and the reader can settle the question by choosing.
//!
//! Inference reads a bounded prefix, so it costs the same for an 8 MiB file
//! as for a note, and callers run it when the content changes, never while
//! drawing.

use std::path::Path;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag};

/// A language the reader can choose for a buffer whose name does not decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DocumentLanguage {
    Markdown,
    PlainText,
}

impl DocumentLanguage {
    pub fn is_markdown(self) -> bool {
        self == Self::Markdown
    }
}

/// Whether `path` is Markdown by its name alone, or `None` when the name says
/// nothing and the content decides.
///
/// A dotfile with no further extension (`.env`, `.gitignore`, `.bashrc`)
/// has none as far as [`Path::extension`] is concerned, but its name is its
/// type. One that has one, such as `.notes.md`, is typed by it.
///
/// A name need not be UTF-8 for its extension to decide.
pub fn markdown_by_name(path: &Path) -> Option<bool> {
    let name = path.file_name()?;
    if name.eq_ignore_ascii_case("README") {
        return Some(true);
    }
    if let Some(extension) = path.extension() {
        return Some(
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown"),
        );
    }
    name.as_encoded_bytes().starts_with(b".").then_some(false)
}

/// How much of a buffer inference reads: enough for any reply or note to
/// show what it is, bounded so a large file costs no more than a small one.
pub const INFERENCE_PREFIX_BYTES: usize = 64 * 1024;

/// The text inference reads: a buffer's first whole lines, up to
/// [`INFERENCE_PREFIX_BYTES`], each ended with `\n`.
#[derive(Debug, Default)]
pub struct InferencePrefix(String);

impl InferencePrefix {
    /// Adds the next line, answering whether there is room for another.
    ///
    /// A first line longer than the bound is cut at it rather than skipped,
    /// so a one-line buffer is still read, and still costs no more than the
    /// bound.
    pub fn push_line(&mut self, line: &str) -> bool {
        if self.0.is_empty() {
            let end = line.floor_char_boundary(INFERENCE_PREFIX_BYTES - 1);
            self.0.push_str(&line[..end]);
        } else if self.0.len() + line.len() + 1 > INFERENCE_PREFIX_BYTES {
            return false;
        } else {
            self.0.push_str(line);
        }
        self.0.push('\n');
        self.0.len() < INFERENCE_PREFIX_BYTES
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The [`InferencePrefix`] of `lines`.
pub fn inference_prefix<'a>(lines: impl IntoIterator<Item = &'a str>) -> InferencePrefix {
    let mut prefix = InferencePrefix::default();
    for line in lines {
        if !prefix.push_line(line) {
            break;
        }
    }
    prefix
}

/// Whether `text` reads as Markdown, given whether the buffer read as
/// Markdown before.
///
/// The two thresholds differ so the answer does not flip while someone
/// types: a half-typed `**bold` must not drop a reply back to plain text,
/// and one `# comment` must not turn a plain note into a heading. Plain text
/// becomes Markdown on clear evidence (two different kinds of construct, or
/// one that only Markdown has: a fence, a table, a link, a section heading),
/// and Markdown stays Markdown until no construct is left at all. A buffer
/// with nothing in it is no evidence either way and keeps what it was.
pub fn infer_markdown(text: &str, was_markdown: bool) -> bool {
    if text.trim().is_empty() {
        return was_markdown;
    }
    // A script names its interpreter on its first line, and its `#`
    // comments parse as headings.
    if text.starts_with("#!") {
        return false;
    }
    let evidence = Evidence::of(text);
    if was_markdown {
        // A construct still being typed (an unclosed `**` or backtick) is
        // not one yet, but its marks are evidence enough to hold on to.
        evidence.constructs > 0 || has_markup_marks(text)
    } else {
        evidence.distinctive || evidence.kinds >= 2
    }
}

/// Whether `text` holds the punctuation Markdown is written with, closed
/// or not.
fn has_markup_marks(text: &str) -> bool {
    text.contains(['`', '*', '_', '~', '[', '|'])
        || text
            .lines()
            .any(|line| matches!(line.trim_start().chars().next(), Some('#' | '>' | '-')))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Evidence {
    constructs: u32,
    /// How many different kinds of construct were seen.
    kinds: u32,
    /// Whether a construct that plain prose and code comments do not produce
    /// by accident was seen.
    distinctive: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Heading,
    List,
    Emphasis,
    InlineCode,
    Quote,
    Fence,
    Table,
    Link,
}

impl Evidence {
    fn of(text: &str) -> Self {
        let mut seen = [false; 8];
        let mut evidence = Self::default();
        let mut note = |kind: Kind, distinctive: bool| {
            let seen = &mut seen[kind as usize];
            if !*seen {
                *seen = true;
                evidence.kinds += 1;
            }
            evidence.constructs = evidence.constructs.saturating_add(1);
            evidence.distinctive |= distinctive;
        };
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for event in Parser::new_ext(text, options) {
            match event {
                // `# word` is also every shell and config comment; `## word`
                // rarely is.
                Event::Start(Tag::Heading { level, .. }) => {
                    note(Kind::Heading, level != HeadingLevel::H1);
                }
                Event::Start(Tag::Item) => note(Kind::List, false),
                Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => {
                    note(Kind::Emphasis, false);
                }
                Event::Code(_) => note(Kind::InlineCode, false),
                Event::Start(Tag::BlockQuote(_)) => note(Kind::Quote, false),
                // An indented block is what any indented text parses as.
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => note(Kind::Fence, true),
                Event::Start(Tag::Table(_)) => note(Kind::Table, true),
                // A bare address is plain text's link too.
                Event::Start(Tag::Link { link_type, .. })
                    if !matches!(link_type, LinkType::Autolink | LinkType::Email) =>
                {
                    note(Kind::Link, true);
                }
                _ => {}
            }
        }
        evidence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_REPLY: &str = "\
I looked into the failing build.

## What happened

The **cache key** ignored `Cargo.lock`, so:

1. CI restored a stale target directory.
2. The linker picked up an old object.

```rust
let key = hash(lockfile);
```

See [the workflow](https://example.com/ci.yml) for details.
";

    #[test]
    fn a_name_decides_before_the_content() {
        for (name, markdown) in [
            ("notes.md", Some(true)),
            ("NOTES.MARKDOWN", Some(true)),
            ("README", Some(true)),
            ("readme", Some(true)),
            ("main.rs", Some(false)),
            ("notes.txt", Some(false)),
            (".env", Some(false)),
            (".gitignore", Some(false)),
            // A dotfile's extension still types it.
            (".notes.md", Some(true)),
            (".release.MARKDOWN", Some(true)),
            (".bashrc.local", Some(false)),
            ("Untitled", None),
            ("Untitled-2", None),
            ("LICENSE", None),
            ("Makefile", None),
        ] {
            assert_eq!(markdown_by_name(Path::new(name)), markdown, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_still_decides_by_its_extension() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        for (name, markdown) in [
            (&b"note\xff.md"[..], Some(true)),
            (b"note\xff.txt", Some(false)),
            (b".\xffrc", Some(false)),
            (b"note\xff", None),
        ] {
            assert_eq!(
                markdown_by_name(Path::new(OsStr::from_bytes(name))),
                markdown,
                "{name:?}"
            );
        }
    }

    #[test]
    fn markdown_replies_read_as_markdown() {
        for text in [
            AGENT_REPLY,
            "## Summary\n\nDone.\n",
            "```\ncargo test\n```\n",
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            "Read [the guide](docs/guide.md).\n",
            "- **First** item\n- Second item\n",
            "# Plan\n\n- one\n- two\n",
            "Use `cargo fmt` and then *commit*.\n",
        ] {
            assert!(infer_markdown(text, false), "{text:?}");
        }
    }

    #[test]
    fn plain_text_stays_plain() {
        for text in [
            "Buy milk\nCall the dentist at 3pm\n",
            "Dear team,\n\nThe release moved to Friday. Thanks!\n",
            // A file of comments: headings to CommonMark, not to a reader.
            "# Build settings\nCC=clang\n# Output\nOUT=target\n",
            "#!/bin/sh\n## Deploy\nset -e\n- not a list\n",
            "Logs at https://example.com/logs and mail ops@example.com\n",
            "    indented text\n    is not a code fence\n",
            "- a lone list\n- of things\n- to do\n",
            "2 * 3 * 4 = 24\n",
            "",
        ] {
            assert!(!infer_markdown(text, false), "{text:?}");
        }
    }

    #[test]
    fn markdown_holds_until_the_content_clearly_is_not() {
        // A reply mid-edit: the bold is not closed yet and the fence is open.
        assert!(infer_markdown("## Plan\n\nThe **ca", true));
        assert!(infer_markdown("Run `make", true));
        // The same text would not have made plain text Markdown.
        assert!(!infer_markdown("# Plan\n\nThe **ca", false));
        // Emptied, or between paste and typing, the buffer keeps what it was.
        assert!(infer_markdown("", true));
        assert!(infer_markdown("\n  \n", true));
        assert!(!infer_markdown("", false));
        // Replaced by prose, it reads as plain text again.
        assert!(!infer_markdown("Just a sentence now.\n", true));
        assert!(!infer_markdown(
            "#!/usr/bin/env python\nprint('hi')\n",
            true
        ));
    }

    #[test]
    fn typing_a_reply_line_by_line_never_flips_back() {
        let mut markdown = false;
        let mut flips = 0;
        for end in 1..=AGENT_REPLY.len() {
            let Some(prefix) = AGENT_REPLY.get(..end) else {
                continue;
            };
            let next = infer_markdown(prefix, markdown);
            if next != markdown {
                flips += 1;
            }
            markdown = next;
        }
        assert!(markdown);
        assert_eq!(flips, 1, "plain until the evidence arrives, then Markdown");
    }

    #[test]
    fn the_prefix_is_bounded_by_whole_lines() {
        let line = "x".repeat(1000);
        let prefix = inference_prefix(std::iter::repeat_n(line.as_str(), 1000));
        let prefix = prefix.as_str();
        assert!(prefix.len() <= INFERENCE_PREFIX_BYTES);
        assert!(prefix.len() > INFERENCE_PREFIX_BYTES - 1001);
        assert!(prefix.lines().all(|read| read == line));
        // One line longer than the bound is read up to it, rather than
        // whole or not at all, and never split inside a character.
        let long = "y".repeat(INFERENCE_PREFIX_BYTES * 2);
        let read = inference_prefix([long.as_str(), "next"]);
        assert_eq!(read.as_str().len(), INFERENCE_PREFIX_BYTES);
        assert!(read.as_str().trim_end().bytes().all(|byte| byte == b'y'));
        let wide = "λ".repeat(INFERENCE_PREFIX_BYTES);
        let read = inference_prefix([wide.as_str()]);
        assert!(read.as_str().len() <= INFERENCE_PREFIX_BYTES);
        assert!(
            read.as_str()
                .trim_end()
                .chars()
                .all(|character| character == 'λ')
        );
    }
}
