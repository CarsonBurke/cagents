//! An answer's Markdown as the parser has to be handed it.
//!
//! iced's Markdown parser is written for documents, and three of the choices
//! it makes for a document lose text from an agent's answer:
//!
//! - a `---` or `+++` line with text straight after it opens a front-matter
//!   block, and everything up to the next such line is discarded - which is
//!   every section of an answer that separates its sections with `---`;
//! - whatever reads as an HTML tag is dropped, so `Option<String>` written
//!   outside backticks arrives as `Option`, and `<details>` not at all;
//! - an indented code block comes out as an empty box, with its lines run into
//!   the paragraph that follows.
//!
//! The parser's item types cannot be built from outside it, so it cannot be
//! replaced for these three cases without carrying a copy of the whole widget
//! crate. What reaches it is rewritten instead: a separator is given a fourth
//! dash, which is still a separator and no longer front matter; a `<` that is
//! neither an autolink nor a line break is escaped, which is how Markdown
//! spells a literal one; and an indented block is fenced. Text inside a fence
//! or a code span is never touched, because nothing is interpreted there and
//! an escape would be shown. Only the parser sees the result - copying a
//! message still copies what the agent wrote.
//!
//! An answer arrives a few bytes at a time and is parsed as it grows, so the
//! rewrite is a function of the whole text so far that is computed a line at a
//! time. Nearly always the rewrite of a longer text extends the rewrite of the
//! shorter one, and only the new bytes are handed on. Where it does not - a
//! `<https://…>` is an autolink only once its `>` arrives - the caller is told
//! to parse the answer again, which is rare and always right, rather than have
//! the unfinished end of a line held back from the reader.

/// The fence an indented block is given. Longer than any fence an answer is
/// likely to quote, since the block's own lines could otherwise close it.
const FENCE: &str = "~~~~~~~~";

/// What the parser is to be given after an answer grew.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// More of the same document.
    Append(String),
    /// The whole document again, because what was handed over for the
    /// unfinished last line no longer begins what that line became.
    Restart(String),
}

/// The rewrite of one answer, resumable from the last line it saw whole.
#[derive(Debug, Default)]
pub(super) struct Source {
    blocks: Blocks,
    /// The bytes of the answer that were whole lines, which is all `blocks`
    /// has been told about.
    consumed: usize,
    /// What was handed over for the unfinished line past `consumed`.
    tail: String,
}

impl Source {
    /// Takes `body`, the answer so far, of which everything up to the bytes
    /// seen last time is unchanged.
    pub(super) fn advance(&mut self, body: &str) -> Step {
        let Some(rest) = body.get(self.consumed..) else {
            return self.restart(body);
        };
        let mut text = String::new();
        let mut unfinished = "";
        for line in rest.split_inclusive('\n') {
            if line.ends_with('\n') {
                self.blocks.line(line, true, &mut text);
                self.consumed += line.len();
            } else {
                unfinished = line;
            }
        }
        let mut tail = String::new();
        // The unfinished line is read by a copy: what it turns out to be is
        // only known once it ends.
        self.blocks.clone().line(unfinished, false, &mut tail);
        text.push_str(&tail);
        // The lines that ended begin with the one that was unfinished, so
        // what was handed over for it has to begin what it became.
        if !text.starts_with(&self.tail) {
            return self.restart(body);
        }
        text.drain(..self.tail.len());
        self.tail = tail;
        Step::Append(text)
    }

    fn restart(&mut self, body: &str) -> Step {
        *self = Self::default();
        match self.advance(body) {
            Step::Append(text) | Step::Restart(text) => Step::Restart(text),
        }
    }
}

/// The whole of `body` as the parser is to be given it.
#[cfg(test)]
pub(super) fn rewrite(body: &str) -> String {
    match Source::default().advance(body) {
        Step::Append(text) | Step::Restart(text) => text,
    }
}

/// An open fenced block, which runs until a fence that closes it.
#[derive(Debug, Clone, Copy)]
struct Fence {
    marker: u8,
    length: usize,
    /// The column the fence was opened at. A closing fence may sit up to
    /// three columns further in, and one inside a list item sits at the
    /// item's own indentation.
    indent: usize,
    /// How many block quotes the fence was opened inside. A line quoted less
    /// deeply has left the quote, and the fence with it.
    quote: usize,
}

/// The block structure a line is read against: only as much of Markdown's as
/// decides whether a line is code.
#[derive(Debug, Clone)]
struct Blocks {
    fence: Option<Fence>,
    /// An indented block being fenced, with the blank lines seen since its
    /// last line. They are kept back until the next line says whether they
    /// are inside the block or after it.
    indented: Option<usize>,
    /// Whether a list may still be open. An indented line under a list item
    /// continues the item rather than starting a code block, so this errs
    /// towards staying set: a block that goes unfenced is no worse off than
    /// before, and a list item fenced as code would be.
    list: bool,
    /// Whether the previous line was blank. An indented line can only start a
    /// code block there, since anywhere else it continues a paragraph.
    blank: bool,
}

impl Default for Blocks {
    fn default() -> Self {
        Self {
            fence: None,
            indented: None,
            list: false,
            blank: true,
        }
    }
}

impl Blocks {
    /// Writes `line` as the parser is to see it. An unfinished line is one
    /// more bytes may still be appended to.
    fn line(&mut self, line: &str, complete: bool, out: &mut String) {
        let (text, ending) = match line.strip_suffix('\n') {
            Some(text) => match text.strip_suffix('\r') {
                Some(text) => (text, "\r\n"),
                None => (text, "\n"),
            },
            None => (line, ""),
        };
        let (quote, quoted) = quote_prefix(text);
        if let Some(fence) = self.fence {
            if quote >= fence.quote {
                if complete && fence.closed_by(&text[quoted..]) {
                    self.fence = None;
                    self.blank = false;
                }
                out.push_str(line);
                return;
            }
            self.fence = None;
        }
        // Outside a fence the white space ending a line means nothing until
        // something follows it, and what follows decides what the line is.
        let text = if complete { text } else { text.trim_end() };
        // The quote's own marker may have ended with the space just set aside.
        let quoted = quoted.min(text.len());
        if text.trim().is_empty() {
            if !complete {
                return;
            }
            match &mut self.indented {
                Some(blanks) => *blanks += 1,
                None => out.push_str(line),
            }
            self.blank = true;
            return;
        }
        let indent = indentation(text);
        if let Some(blanks) = self.indented {
            if indent >= 4 {
                out.extend(std::iter::repeat_n('\n', blanks));
                out.push_str(outdented(text));
                out.push_str(ending);
                self.indented = Some(0);
                self.blank = false;
                return;
            }
            out.push_str(FENCE);
            out.push('\n');
            out.extend(std::iter::repeat_n('\n', blanks));
            self.indented = None;
        } else if indent >= 4 && self.blank && !self.list {
            out.push_str(FENCE);
            out.push('\n');
            out.push_str(outdented(text));
            out.push_str(ending);
            self.indented = Some(0);
            self.blank = false;
            return;
        }
        let (item, content) = list_prefix(&text[quoted..]);
        let content = quoted + content;
        let column = content - quoted + indentation(&text[content..]);
        let nested = self.list || item || quote > 0;
        let outermost = quote == 0 && !item && indent < 2;
        if let Some((marker, length)) = fence_opener(text[content..].trim_start())
            && (nested || indent < 4)
        {
            self.fence = Some(Fence {
                marker,
                length,
                indent: column,
                quote,
            });
            self.list = (self.list && !outermost) || item;
            self.blank = false;
            out.push_str(text);
            out.push_str(ending);
            return;
        }
        if item {
            self.list = true;
        } else if outermost && (self.blank || text.starts_with('#')) {
            // Only a line that cannot continue the last item ends the list:
            // one after a blank line, or a heading, at the outer margin.
            self.list = false;
        }
        self.blank = false;
        if let Some(rest) = text.strip_prefix("---")
            && rest.trim().is_empty()
        {
            // Four dashes are the same separator, or the same underline of a
            // heading, and are not the three that open front matter. The
            // fourth waits for the line to end, so nothing already handed
            // over changes.
            out.push_str(if complete { "----" } else { "---" });
            out.push_str(rest);
        } else if let Some(rest) = text.strip_prefix("+++") {
            // Front matter in the other spelling. Pluses mean nothing else at
            // the start of a line, so an escaped one reads the same.
            out.push_str("++\\+");
            inline(rest, complete, out);
        } else {
            inline(text, complete, out);
        }
        out.push_str(ending);
    }
}

impl Fence {
    /// Whether `line`, less the quote markers before it, closes this fence.
    fn closed_by(self, line: &str) -> bool {
        let content = line.trim_start();
        let run = run_length(content.as_bytes(), self.marker);
        run >= self.length
            && content[run..].trim().is_empty()
            && indentation(line) <= self.indent + 3
    }
}

/// The depth of block quote `line` sits in and where its text starts.
fn quote_prefix(line: &str) -> (usize, usize) {
    let bytes = line.as_bytes();
    let (mut depth, mut start) = (0, 0);
    loop {
        let mut at = start;
        while at < bytes.len() && at - start < 3 && bytes[at] == b' ' {
            at += 1;
        }
        if bytes.get(at) != Some(&b'>') {
            return (depth, start);
        }
        depth += 1;
        start = at + 1;
        if bytes.get(start) == Some(&b' ') {
            start += 1;
        }
    }
}

/// Whether `line` opens a list item, and where the item's own text starts.
fn list_prefix(line: &str) -> (bool, usize) {
    let bytes = line.as_bytes();
    let (mut item, mut start) = (false, 0);
    loop {
        let mut at = start;
        while matches!(bytes.get(at), Some(b' ' | b'\t')) {
            at += 1;
        }
        let digits = bytes[at..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let marker = match bytes.get(at) {
            Some(b'-' | b'*' | b'+') => at + 1,
            Some(_) if (1..=9).contains(&digits) => match bytes.get(at + digits) {
                Some(b'.' | b')') => at + digits + 1,
                _ => return (item, start),
            },
            _ => return (item, start),
        };
        match bytes.get(marker) {
            Some(b' ' | b'\t') => start = marker + 1,
            None => start = marker,
            Some(_) => return (item, start),
        }
        item = true;
    }
}

/// The marker and length of the fence `content` opens, if it opens one.
fn fence_opener(content: &str) -> Option<(u8, usize)> {
    let marker = *content.as_bytes().first()?;
    let length = run_length(content.as_bytes(), marker);
    let opens = match marker {
        // A backtick fence's info string cannot hold a backtick: such a line
        // is a paragraph that starts with a code span.
        b'`' => !content[length..].contains('`'),
        b'~' => true,
        _ => false,
    };
    (opens && length >= 3).then_some((marker, length))
}

fn run_length(bytes: &[u8], marker: u8) -> usize {
    bytes.iter().take_while(|byte| **byte == marker).count()
}

/// The column `line`'s text starts at, a tab reaching the next multiple of
/// four.
fn indentation(line: &str) -> usize {
    let mut column = 0;
    for byte in line.bytes() {
        match byte {
            b' ' => column += 1,
            b'\t' => column += 4 - column % 4,
            _ => break,
        }
    }
    column
}

/// `line` less the four columns that made it code.
fn outdented(line: &str) -> &str {
    let mut columns = 0;
    for (index, byte) in line.bytes().enumerate() {
        match byte {
            _ if columns >= 4 => return &line[index..],
            b' ' => columns += 1,
            b'\t' => return &line[index + 1..],
            _ => return &line[index..],
        }
    }
    ""
}

/// Writes a line of prose with every `<` that would start a tag escaped.
///
/// Code spans are matched within the line. One that wraps onto the next line
/// is rare in an answer, and reading it as text costs a visible backslash
/// where reading text as code would cost the tag again.
fn inline(text: &str, complete: bool, out: &mut String) {
    let bytes = text.as_bytes();
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if bytes.get(at + 1).is_some_and(u8::is_ascii_punctuation) => at += 2,
            b'`' => {
                let run = run_length(&bytes[at..], b'`');
                at = match closing_run(bytes, at + run, run) {
                    Some(end) => end,
                    None if complete => at + run,
                    // The span may yet close, and the far likelier reading of
                    // an unfinished line is that it will.
                    None => bytes.len(),
                };
            }
            b'<' => {
                let before = text[..at].trim_end();
                if before.ends_with("](") || before.ends_with("]:") {
                    // A link destination in angle brackets.
                    at += 1;
                } else if let Some(length) = autolink(&bytes[at..]) {
                    at += length;
                } else if let Some(length) = line_break(&bytes[at..]) {
                    // The one tag the parser honours, in the one spelling it
                    // knows.
                    out.push_str(&text[copied..at]);
                    out.push_str("<br>");
                    at += length;
                    copied = at;
                } else {
                    out.push_str(&text[copied..at]);
                    out.push('\\');
                    copied = at;
                    at += 1;
                }
            }
            _ => at += 1,
        }
    }
    out.push_str(&text[copied..]);
}

/// Where the code span opened by a run of `run` backticks ends: just past the
/// next run of exactly that length.
fn closing_run(bytes: &[u8], mut at: usize, run: usize) -> Option<usize> {
    while at < bytes.len() {
        if bytes[at] == b'`' {
            let length = run_length(&bytes[at..], b'`');
            at += length;
            if length == run {
                return Some(at);
            }
        } else {
            at += 1;
        }
    }
    None
}

/// The length of the autolink `bytes` starts with: `<scheme:…>` or
/// `<name@host>`, with no white space inside.
fn autolink(bytes: &[u8]) -> Option<usize> {
    let end = bytes.iter().position(|byte| *byte == b'>')?;
    let inner = &bytes[1..end];
    if inner
        .iter()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control() || *byte == b'<')
    {
        return None;
    }
    let scheme = inner
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
        .count();
    let uri = (2..=32).contains(&scheme)
        && inner[0].is_ascii_alphabetic()
        && inner.get(scheme) == Some(&b':');
    let email = || {
        let at = inner.iter().position(|byte| *byte == b'@')?;
        let (name, host) = (&inner[..at], &inner[at + 1..]);
        let named = !name.is_empty()
            && name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(byte));
        let hosted = !host.is_empty()
            && host
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
        (named && hosted).then_some(())
    };
    (uri || email().is_some()).then_some(end + 1)
}

/// The length of the line break `bytes` starts with: `<br>`, `<br/>` or
/// `<br />`, in either case.
fn line_break(bytes: &[u8]) -> Option<usize> {
    if !bytes.get(1..3)?.eq_ignore_ascii_case(b"br") {
        return None;
    }
    let mut at = 3;
    while bytes.get(at) == Some(&b' ') {
        at += 1;
    }
    if bytes.get(at) == Some(&b'/') {
        at += 1;
    }
    (bytes.get(at) == Some(&b'>')).then_some(at + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the reader loses today, and what the parser is given instead.
    #[test]
    fn text_the_parser_would_drop_is_rewritten_so_it_is_kept() {
        for (source, expected) in [
            // A separator with text straight after it is not front matter.
            (
                "Intro\n\n---\nNext part\n\n---\nLast\n",
                "Intro\n\n----\nNext part\n\n----\nLast\n",
            ),
            ("---\ntitle: x\n---\nbody\n", "----\ntitle: x\n----\nbody\n"),
            ("---  \ntext\n", "----  \ntext\n"),
            ("+++\ntext\n+++\n", "++\\+\ntext\n++\\+\n"),
            // An underline and a longer rule were never front matter.
            ("Title\n---\n", "Title\n----\n"),
            ("-----\ntext\n", "-----\ntext\n"),
            ("   ---\ntext\n", "   ---\ntext\n"),
            ("--- not a rule\n", "--- not a rule\n"),
            // Generics and tags outside backticks.
            (
                "Use Option<String> or Vec<Option<String>>.\n",
                "Use Option\\<String> or Vec\\<Option\\<String>>.\n",
            ),
            ("HashMap<K, V> maps\n", "HashMap\\<K, V> maps\n"),
            (
                "<details>\n<summary>More</summary>\n\nbody\n</details>\n",
                "\\<details>\n\\<summary>More\\</summary>\n\nbody\n\\</details>\n",
            ),
            ("Press <kbd>Ctrl</kbd>\n", "Press \\<kbd>Ctrl\\</kbd>\n"),
            ("<!-- note -->\n", "\\<!-- note -->\n"),
            ("a < b and a <= b\n", "a \\< b and a \\<= b\n"),
            ("already \\<escaped>\n", "already \\<escaped>\n"),
            // The break survives, in the spelling the parser knows.
            (
                "one<br>two<br/>three<BR />four\n",
                "one<br>two<br>three<br>four\n",
            ),
            // Autolinks and bracketed destinations are links, not tags.
            (
                "See <https://example.com/a?b=c> and <me@example.com>.\n",
                "See <https://example.com/a?b=c> and <me@example.com>.\n",
            ),
            ("[docs](<my file.md>)\n", "[docs](<my file.md>)\n"),
            (
                "[ref]: <https://example.com>\n",
                "[ref]: <https://example.com>\n",
            ),
            ("<not a link>\n", "\\<not a link>\n"),
            // Nothing is interpreted in a code span, so nothing is escaped.
            (
                "`Vec<T>` and ``a ` <b>`` then <c>\n",
                "`Vec<T>` and ``a ` <b>`` then \\<c>\n",
            ),
            ("a lone ` then <c>\n", "a lone ` then \\<c>\n"),
            // An indented block is fenced, without the blank lines after it.
            (
                "Run:\n\n    cargo test\n\n    cargo fmt\n\nThen <done>.\n",
                "Run:\n\n~~~~~~~~\ncargo test\n\ncargo fmt\n~~~~~~~~\n\nThen \\<done>.\n",
            ),
            ("\tfn a<T>() {}\n", "~~~~~~~~\nfn a<T>() {}\n"),
            ("      deeper\n", "~~~~~~~~\n  deeper\n"),
            // An indented line continues a paragraph or a list item.
            ("text\n    more <b>\n", "text\n    more \\<b>\n"),
            (
                "- item\n\n    still the item\n\nAfter\n\n    code\n",
                "- item\n\n    still the item\n\nAfter\n\n~~~~~~~~\ncode\n",
            ),
        ] {
            assert_eq!(rewrite(source), expected, "{source:?}");
        }
    }

    /// A fence's text reaches the parser byte for byte, whatever it holds.
    #[test]
    fn fenced_code_is_never_rewritten() {
        const CODE: &str = "---\nlet x: Vec<T> = <br>;\n+++\n\n    indented\n";
        for (open, close) in [
            ("```", "```"),
            ("```rust", "```"),
            ("```rust,ignore", "```"),
            ("```ts title=\"x.ts\"", "```"),
            ("~~~", "~~~"),
            ("~~~~ text", "~~~~~"),
            ("   ```", "   ```"),
            ("> ```rust", "> ```"),
        ] {
            let quoted = open.starts_with('>');
            let body = if quoted {
                CODE.lines()
                    .map(|line| format!("> {line}\n"))
                    .collect::<String>()
            } else {
                CODE.to_owned()
            };
            let source = format!("Before <a>\n\n{open}\n{body}{close}\n\nAfter <b>\n");
            let expected = format!("Before \\<a>\n\n{open}\n{body}{close}\n\nAfter \\<b>\n");
            assert_eq!(rewrite(&source), expected, "{open:?}");
        }
        // A longer fence holds a shorter one, and only its own length closes it.
        let nested = "````md\n```rust\nVec<T>\n```\n---\n<b>\n````\n<c>\n";
        assert_eq!(
            rewrite(nested),
            "````md\n```rust\nVec<T>\n```\n---\n<b>\n````\n\\<c>\n"
        );
        // A tilde fence is not closed by backticks, nor the other way round.
        let mixed = "~~~\n```\n<a>\n~~~\n<b>\n";
        assert_eq!(rewrite(mixed), "~~~\n```\n<a>\n~~~\n\\<b>\n");
        // A fence inside a list item, at the item's indentation and deeper.
        let listed = "1. Step\n\n   ```rust\n   Vec<T>\n   ```\n\n   - nested\n\n     ```\n     <x>\n     ```\n\nDone <y>\n";
        assert_eq!(
            rewrite(listed),
            "1. Step\n\n   ```rust\n   Vec<T>\n   ```\n\n   - nested\n\n     ```\n     <x>\n     ```\n\nDone \\<y>\n"
        );
        // A fence opened on the item's own line.
        assert_eq!(rewrite("- ```\n  <x>\n  ```\n"), "- ```\n  <x>\n  ```\n");
        // A quote that ends takes its unclosed fence with it.
        assert_eq!(rewrite("> ```\n> <a>\n\n<b>\n"), "> ```\n> <a>\n\n\\<b>\n");
        // An unclosed fence runs to the end.
        assert_eq!(rewrite("```\n<a>\n---\n"), "```\n<a>\n---\n");
        // Backticks with a backtick in their info string open no fence.
        assert_eq!(rewrite("```a`b <c>\n"), "```a`b \\<c>\n");
    }

    const CORPUS: &[&str] = &[
        "Intro\n\n---\nNext <part>\n\n---\n\n+++\nLast\n+++\n",
        "Use Vec<Option<String>> and HashMap<K, V>, then `Vec<T>`.\n\nSee <https://example.com/a> or <me@example.com>.\n",
        "| a | b |\n|---|---|\n| one<br>two | x<br/>y |\n| `Vec<T>` | Option<T> |\n",
        "Before\n\n```rust,ignore\nfn a<T>() {}\n---\n```\n\n~~~\n<b>\n~~~\n\nAfter <c>\n",
        "Run:\n\n    cargo test\n\n    cargo fmt  \n\nThen:\n\n\tmake <all>\n",
        "1. One\n\n   ```sh\n   echo <x>\n   ```\n\n       not fenced\n\n2. Two <kbd>k</kbd>\n\nλ → <é>\r\n---\r\ntext\r\n",
        "<details>\n<summary>S</summary>\n\nbody\n</details>\n\ntrailing <br",
        "a `code <x>\nstill` b <y>\n\n  \n   \n    late\n",
        "> quoted <b>\n> \n> - item <c>\n>\n> ```\n> <code>\n> ```\n> \n\nafter <d>\n",
    ];

    /// However an answer is cut into deltas, the parser ends up holding the
    /// rewrite of the whole of it: every append extends what it was given,
    /// and a restart hands over the whole rewrite again.
    #[test]
    fn a_streamed_answer_is_rewritten_as_the_whole_one_is() {
        for body in CORPUS {
            let whole = rewrite(body);
            let boundaries: Vec<usize> = (0..=body.len())
                .filter(|index| body.is_char_boundary(*index))
                .collect();
            // Every delta one character long, then every way of cutting the
            // answer in two and in three.
            let mut cuts: Vec<Vec<usize>> = vec![boundaries.clone()];
            for (index, first) in boundaries.iter().enumerate() {
                cuts.push(vec![*first, body.len()]);
                for second in boundaries[index..].iter().step_by(7) {
                    cuts.push(vec![*first, *second, body.len()]);
                }
            }
            for cut in cuts {
                let mut source = Source::default();
                let mut given = String::new();
                for end in &cut {
                    match source.advance(&body[..*end]) {
                        Step::Append(text) => given.push_str(&text),
                        Step::Restart(text) => given = text,
                    }
                    assert_eq!(given, rewrite(&body[..*end]), "{body:?} cut at {cut:?}");
                }
                assert_eq!(given, whole, "{body:?} cut at {cut:?}");
            }
        }
    }

    /// An ordinary answer never needs the parser to start again; only a tag
    /// that turns out to be a link or a break does.
    #[test]
    fn only_a_late_link_or_break_restarts_the_parse() {
        let restarts = |body: &str| {
            let mut source = Source::default();
            (1..=body.len())
                .filter(|end| body.is_char_boundary(*end))
                .filter(|end| matches!(source.advance(&body[..*end]), Step::Restart(_)))
                .count()
        };
        assert_eq!(
            restarts(
                "Intro\n\n---\nUse Vec<Option<String>> and `Vec<T>` here.  \n\n    code\n\n+++\n```rust\nfn a<T>() {}\n```\n- item\n"
            ),
            0
        );
        assert_eq!(restarts("a<br>b\n"), 1);
        assert_eq!(restarts("see <https://example.com>\n"), 1);
    }

    #[test]
    fn a_rewritten_body_is_not_trusted_as_an_append() {
        let mut source = Source::default();
        assert_eq!(
            source.advance("one\ntwo\nthree"),
            Step::Append("one\ntwo\nthree".into())
        );
        // Shorter than what was consumed: the caller's body is another text.
        assert_eq!(source.advance("one"), Step::Restart("one".into()));
        assert_eq!(source.advance("one\n<b>"), Step::Append("\n\\<b>".into()));
    }
}
