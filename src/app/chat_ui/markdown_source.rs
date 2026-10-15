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
//! shorter one, and only the new bytes are handed on. Two things are only
//! known later than they are read. Whether a `<` starts a link or a line
//! break is known once its `>` arrives, so while an answer may still grow the
//! end of its last line is kept back from the first `<` that could yet be
//! either - a word at most, and handed over as soon as the answer is known to
//! have ended. Whether a backtick opens a code span that runs on over the
//! following lines is known once the paragraph has ended or the span has
//! closed, and until then the span is taken to close. Where that turns out
//! wrong the caller is told to parse the answer again, which is rare and
//! always right.

/// The fence an indented block is given. Longer than any fence an answer is
/// likely to quote, since the block's own lines could otherwise close it.
const FENCE: &str = "~~~~~~~~";

/// What the parser is to be given after an answer grew.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// More of the same document.
    Append(String),
    /// The whole document again, because what was handed over for the end of
    /// the answer no longer begins what that end became.
    Restart(String),
}

/// The rewrite of one answer, resumable from the last line it was sure of.
#[derive(Debug, Default)]
pub(super) struct Source {
    blocks: Blocks,
    /// The bytes of the answer that were whole lines read for good, which is
    /// all `blocks` has been told about.
    consumed: usize,
    /// What was handed over for the lines past `consumed`.
    tail: String,
    /// Whether those lines were read as they are only because the answer may
    /// still grow, and so have to be read again once it cannot.
    provisional: bool,
    seen: usize,
    literal_tail: bool,
    #[cfg(test)]
    rewritten_bytes: usize,
}

impl Source {
    /// The caller has verified that the previously seen source is an exact
    /// prefix. Ordinary single-line appends cannot change the source rewrite;
    /// retain the unfinished tail without rescanning or cloning its old bytes.
    pub(super) fn advance_append(&mut self, body: &str, open: bool) -> Step {
        if open
            && self.literal_tail
            && let Some(suffix) = body.get(self.seen..)
            && suffix.chars().all(|ch| {
                ch.is_alphanumeric()
                    || matches!(
                        ch,
                        ' ' | '.' | ',' | ':' | ';' | '?' | '!' | '/' | '(' | ')' | '=' | '+' | '-'
                    )
            })
        {
            self.tail.push_str(suffix);
            self.seen = body.len();
            return Step::Append(suffix.to_owned());
        }
        self.advance(body, open)
    }

    pub(super) fn retained_capacity(&self) -> usize {
        self.tail.capacity() + self.blocks.items.capacity() * std::mem::size_of::<usize>()
    }


    /// Takes `body`, the answer so far, of which everything up to the bytes
    /// seen last time is unchanged. `open` is whether more may follow.
    pub(super) fn advance(&mut self, body: &str, open: bool) -> Step {
        let Some(rest) = body.get(self.consumed..) else {
            return self.restart(body, open);
        };
        let mut text = String::new();
        // The lines not yet read for good are read by a copy: what an
        // unfinished line turns out to be is only known once it ends, and an
        // open code span is only known to close once it has.
        let mut blocks = self.blocks.clone();
        let (mut at, mut settled, mut sure) = (0, 0, true);
        self.provisional = false;
        for line in rest.split_inclusive('\n') {
            #[cfg(test)]
            {
                self.rewritten_bytes += line.len();
            }
            at += line.len();
            let complete = line.ends_with('\n');
            let beyond = Beyond {
                complete,
                open,
                following: &rest[at..],
            };
            let read = blocks.line(line, beyond, &mut text);
            self.provisional |= read != Read::Final;
            sure &= complete && read == Read::Final;
            if sure {
                self.blocks.clone_from(&blocks);
                self.consumed += line.len();
                settled = text.len();
            }
        }
        // The lines read for good begin with the ones that were not, so what
        // was handed over for those has to begin what they became.
        if !text.starts_with(&self.tail) {
            return self.restart(body, open);
        }
        let tail = text[settled..].to_owned();
        text.drain(..self.tail.len());
        self.tail = tail;
        self.seen = body.len();
        self.literal_tail = body.get(self.consumed..).is_some_and(|raw| {
            raw == self.tail
                && !raw.contains('\n')
                && raw.chars().next().is_some_and(char::is_alphabetic)
        });
        Step::Append(text)
    }

    /// Whether the answer's end was read on the assumption that more follows,
    /// and has to be taken again once the answer is known to have ended.
    pub(super) fn provisional(&self) -> bool {
        self.provisional
    }

    fn restart(&mut self, body: &str, open: bool) -> Step {
        *self = Self::default();
        match self.advance(body, open) {
            Step::Append(text) | Step::Restart(text) => Step::Restart(text),
        }
    }
}

/// The whole of `body`, an answer that has ended, as the parser is to be
/// given it.
#[cfg(test)]
pub(super) fn rewrite(body: &str) -> String {
    match Source::default().advance(body, false) {
        Step::Append(text) | Step::Restart(text) => text,
    }
}

/// What is known of the answer beyond the line being read.
#[derive(Clone, Copy)]
struct Beyond<'a> {
    /// Whether the line has ended. An unfinished line is one more bytes may
    /// still be appended to.
    complete: bool,
    /// Whether the answer may still grow.
    open: bool,
    /// The lines after this one, as far as they have arrived.
    following: &'a str,
}

/// How far a line's reading can be relied on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// As it will stay, whatever follows.
    Final,
    /// With a code span taken to close in text that has not arrived.
    Open,
    /// Up to a `<` that may yet start a link or a line break.
    Held,
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
    /// The column the text of the list item around the fence starts at, or
    /// zero outside one. A line that starts short of it has left the item,
    /// and the fence with it.
    item: usize,
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
    /// The columns the text of each list item that may still be open starts
    /// at, outermost first. An indented line under a list item continues the
    /// item rather than starting a code block, so this errs towards keeping
    /// an item: a block that goes unfenced is no worse off than before, and a
    /// list item fenced as code would be.
    items: Vec<usize>,
    /// Whether the previous line was blank. An indented line can only start a
    /// code block there, since anywhere else it continues a paragraph.
    blank: bool,
    /// The length of the backtick run that opened a code span the paragraph's
    /// last line left open, or zero.
    span: usize,
}

impl Default for Blocks {
    fn default() -> Self {
        Self {
            fence: None,
            indented: None,
            items: Vec::new(),
            blank: true,
            span: 0,
        }
    }
}

impl Blocks {
    /// Writes `line` as the parser is to see it.
    fn line(&mut self, line: &str, beyond: Beyond<'_>, out: &mut String) -> Read {
        let complete = beyond.complete;
        let (text, ending) = match line.strip_suffix('\n') {
            Some(text) => match text.strip_suffix('\r') {
                Some(text) => (text, "\r\n"),
                None => (text, "\n"),
            },
            None => (line, ""),
        };
        if let Some(fence) = self.fence {
            // Only the markers of the quotes the fence is in are the quotes':
            // one more is the code's own.
            let (quote, quoted) = quote_prefix(text, fence.quote);
            let inner = &text[quoted..];
            let indent = indentation(inner);
            if quote == fence.quote && (inner.trim().is_empty() || indent >= fence.item) {
                if complete && fence.closed_by(inner) {
                    self.fence = None;
                    self.blank = false;
                }
                out.push_str(line);
                return Read::Final;
            }
            // The line has left the quote or the list item, and is read as
            // any other.
            self.fence = None;
            if fence.quote == 0 {
                self.items.retain(|column| *column <= indent);
            }
        }
        // Outside a fence the white space ending a line means nothing until
        // something follows it, and what follows decides what the line is.
        // A quote's own marker may end with the space set aside here.
        let text = if complete { text } else { text.trim_end() };
        let (quote, quoted) = quote_prefix(text, usize::MAX);
        if text.trim().is_empty() {
            if !complete {
                return Read::Final;
            }
            match &mut self.indented {
                Some(blanks) => *blanks += 1,
                None => out.push_str(line),
            }
            self.blank = true;
            self.span = 0;
            return Read::Final;
        }
        let indent = indentation(text);
        if let Some(blanks) = self.indented {
            if indent >= 4 {
                out.extend(std::iter::repeat_n('\n', blanks));
                out.push_str(outdented(text));
                out.push_str(ending);
                self.indented = Some(0);
                self.blank = false;
                return Read::Final;
            }
            out.push_str(FENCE);
            out.push('\n');
            out.extend(std::iter::repeat_n('\n', blanks));
            self.indented = None;
        } else if indent >= 4 && self.blank && self.items.is_empty() {
            out.push_str(FENCE);
            out.push('\n');
            out.push_str(outdented(text));
            out.push_str(ending);
            self.indented = Some(0);
            self.blank = false;
            return Read::Final;
        }
        let (item, content) = list_prefix(&text[quoted..]);
        let content = quoted + content;
        let column = content - quoted + indentation(&text[content..]);
        let opener = fence_opener(text[content..].trim_start());
        // The list items of a quote are not followed: their columns are
        // counted from the quote's marker, and the quote ends with them.
        if quote == 0 {
            if item {
                self.items.retain(|column| *column <= indent);
                self.items.push(content);
            } else if self.blank || opener.is_some() || text.starts_with('#') || rule(text) {
                // Only a line that cannot continue an item ends it: one after
                // a blank line, a fence, a heading or a rule, starting short
                // of the item's text.
                self.items.retain(|column| *column <= indent);
            }
        }
        if let Some((marker, length)) = opener
            && (quote > 0 || indent < 4 || !self.items.is_empty())
        {
            self.fence = Some(Fence {
                marker,
                length,
                indent: column,
                quote,
                item: match quote {
                    0 => self.items.last().copied().unwrap_or(0),
                    _ if item => content - quoted,
                    _ => 0,
                },
            });
            self.blank = false;
            self.span = 0;
            out.push_str(text);
            out.push_str(ending);
            return Read::Final;
        }
        self.blank = false;
        if !continues(text) {
            self.span = 0;
        }
        let prose = Prose {
            complete,
            open: beyond.open,
            // A heading is one line, and a table's cells are read apart, so
            // a code span left open in either does not run on.
            following: (!text[content..].trim_start().starts_with('#') && !text.contains('|'))
                .then_some(beyond.following),
        };
        let read = if let Some(rest) = text.strip_prefix("---")
            && rest.trim().is_empty()
        {
            // Four dashes are the same separator, or the same underline of a
            // heading, and are not the three that open front matter. The
            // fourth waits for the line to end, so nothing already handed
            // over changes.
            out.push_str(if complete { "----" } else { "---" });
            out.push_str(rest);
            Read::Final
        } else if let Some(rest) = text.strip_prefix("+++")
            && self.span == 0
        {
            // Front matter in the other spelling. Pluses mean nothing else at
            // the start of a line, so an escaped one reads the same.
            out.push_str("++\\+");
            inline(rest, &mut self.span, prose, out)
        } else {
            inline(text, &mut self.span, prose, out)
        };
        out.push_str(ending);
        read
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

/// The depth of block quote `line` sits in, counted no further than `limit`,
/// and where its text starts.
fn quote_prefix(line: &str, limit: usize) -> (usize, usize) {
    let bytes = line.as_bytes();
    let (mut depth, mut start) = (0, 0);
    while depth < limit {
        let mut at = start;
        while at < bytes.len() && at - start < 3 && bytes[at] == b' ' {
            at += 1;
        }
        if bytes.get(at) != Some(&b'>') {
            break;
        }
        depth += 1;
        start = at + 1;
        if bytes.get(start) == Some(&b' ') {
            start += 1;
        }
    }
    (depth, start)
}

/// Whether `line` can be the next line of the paragraph before it, which is
/// what lets a code span run on into it. A line that could start a block of
/// its own is taken to: text read as past a span's end is at worst shown
/// with its backslash, where text read as inside one loses its tags.
fn continues(line: &str) -> bool {
    let content = line.trim_start();
    !(content.is_empty()
        || content.starts_with(['#', '>', '|', '='])
        || list_prefix(line).0
        || fence_opener(content).is_some()
        || rule(line))
}

/// Whether `line` holds nothing but what a rule, a heading's underline or
/// the line under a table's header is drawn with.
fn rule(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty()
        && line
            .bytes()
            .all(|byte| matches!(byte, b'-' | b'*' | b'_' | b':' | b'|' | b' ' | b'\t'))
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

/// What a line of prose is read against.
#[derive(Clone, Copy)]
struct Prose<'a> {
    complete: bool,
    open: bool,
    /// The lines after this one, where a code span it leaves open could run
    /// on into them.
    following: Option<&'a str>,
}

/// Writes a line of prose with every `<` that would start a tag escaped.
///
/// `span` is the length of the backtick run that opened a code span the line
/// before left open, and is left as this line leaves it. A span that is not
/// closed on its own line runs on if a later line of the paragraph closes it,
/// and is otherwise a backtick and nothing more.
fn inline(text: &str, span: &mut usize, prose: Prose<'_>, out: &mut String) -> Read {
    let bytes = text.as_bytes();
    let (mut at, mut copied) = (0, 0);
    if *span > 0 {
        match closing_run(bytes, 0, *span) {
            Some(end) => {
                at = end;
                *span = 0;
            }
            None => {
                out.push_str(text);
                return Read::Final;
            }
        }
    }
    let mut read = Read::Final;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if bytes.get(at + 1).is_some_and(u8::is_ascii_punctuation) => at += 2,
            b'`' => {
                let run = run_length(&bytes[at..], b'`');
                if let Some(end) = closing_run(bytes, at + run, run) {
                    at = end;
                    continue;
                }
                let runs_on = match prose.following {
                    // The span may yet close, and the far likelier reading of
                    // an unfinished line is that it will.
                    _ if !prose.complete => (!prose.open).then_some(false),
                    Some(following) => closes(following, run, prose.open),
                    None => Some(false),
                };
                if runs_on == Some(false) {
                    at += run;
                    continue;
                }
                if runs_on.is_none() {
                    read = Read::Open;
                }
                *span = run;
                at = bytes.len();
            }
            b'<' => {
                let before = text[..at].trim_end();
                if before.ends_with("](") || before.ends_with("]:") {
                    // A link destination in angle brackets.
                    at += 1;
                    continue;
                }
                let (link, line_break) = (autolink(&bytes[at..]), line_break(&bytes[at..]));
                if let Scan::Whole(length) = link {
                    at += length;
                } else if let Scan::Whole(length) = line_break {
                    // The one tag the parser honours, in the one spelling it
                    // knows.
                    out.push_str(&text[copied..at]);
                    out.push_str("<br>");
                    at += length;
                    copied = at;
                } else if !prose.complete
                    && prose.open
                    && (link == Scan::Unfinished || line_break == Scan::Unfinished)
                {
                    // Escaped now, it could not become the link or the break
                    // its next bytes may make it, so it waits for them.
                    out.push_str(&text[copied..at]);
                    return Read::Held;
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
    read
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

/// Whether the code span a run of `run` backticks left open at the end of
/// its line is closed in `following`, the lines after it: `None` while the
/// lines that would say have not all arrived.
fn closes(following: &str, run: usize, open: bool) -> Option<bool> {
    for line in following.split_inclusive('\n') {
        let text = match line.strip_suffix('\n') {
            Some(text) => text.strip_suffix('\r').unwrap_or(text),
            // An unfinished line may yet become one that ends the paragraph.
            None if open => return None,
            None => line,
        };
        if !continues(text) {
            return Some(false);
        }
        if closing_run(text.as_bytes(), 0, run).is_some() {
            return Some(true);
        }
    }
    (!open).then_some(false)
}

/// What a `<` turned out to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// The thing looked for, of this length.
    Whole(usize),
    /// What may still become it: the bytes ran out before any ruled it out.
    Unfinished,
    Nothing,
}

/// The autolink `bytes` starts with: `<scheme:…>` or `<name@host>`, as the
/// parser reads them, with no white space inside. Read no further than the
/// first byte that settles it, since a line is read once for each `<` in it.
fn autolink(bytes: &[u8]) -> Scan {
    match uri(bytes) {
        Scan::Nothing => email(bytes),
        found => found,
    }
}

fn uri(bytes: &[u8]) -> Scan {
    let scheme = bytes[1..]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
        .count();
    if bytes.get(1).is_some_and(|byte| !byte.is_ascii_alphabetic()) || scheme > 32 {
        return Scan::Nothing;
    }
    let mut at = 1 + scheme;
    match bytes.get(at) {
        None => return Scan::Unfinished,
        Some(b':') if scheme >= 2 => at += 1,
        Some(_) => return Scan::Nothing,
    }
    // The parser would take `<std::string>` for a link to the scheme `std`.
    // No address starts with a colon, and a path written between a
    // generic's brackets always does.
    if bytes.get(at) == Some(&b':') {
        return Scan::Nothing;
    }
    loop {
        match bytes.get(at) {
            None => return Scan::Unfinished,
            Some(b'>') => return Scan::Whole(at + 1),
            Some(0..=b' ' | b'<') => return Scan::Nothing,
            Some(_) => at += 1,
        }
    }
}

fn email(bytes: &[u8]) -> Scan {
    let mut at = 1;
    loop {
        match bytes.get(at) {
            None => return Scan::Unfinished,
            Some(b'@') if at > 1 => break,
            Some(byte)
                if byte.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(byte) =>
            {
                at += 1
            }
            Some(_) => return Scan::Nothing,
        }
    }
    // A host is labels joined by dots, none empty, over 63 bytes long, or
    // with a hyphen at either end.
    loop {
        at += 1;
        let label = bytes[at..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'-')
            .count();
        let Some(next) = bytes.get(at + label) else {
            return Scan::Unfinished;
        };
        let hyphens = [bytes[at], bytes[at + label - usize::from(label > 0)]];
        if label == 0 || label > 63 || hyphens.contains(&b'-') {
            return Scan::Nothing;
        }
        at += label;
        match next {
            b'.' => {}
            b'>' => return Scan::Whole(at + 1),
            _ => return Scan::Nothing,
        }
    }
}

/// The line break `bytes` starts with: `<br>`, `<br/>` or `<br />`, in
/// either case.
fn line_break(bytes: &[u8]) -> Scan {
    let mut at = 1;
    for letter in [b'b', b'r'] {
        match bytes.get(at) {
            None => return Scan::Unfinished,
            Some(byte) if byte.eq_ignore_ascii_case(&letter) => at += 1,
            Some(_) => return Scan::Nothing,
        }
    }
    while bytes.get(at) == Some(&b' ') {
        at += 1;
    }
    if bytes.get(at) == Some(&b'/') {
        at += 1;
    }
    match bytes.get(at) {
        None => Scan::Unfinished,
        Some(b'>') => Scan::Whole(at + 1),
        Some(_) => Scan::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the reader loses today, and what the parser is given instead.
    #[test]
    fn verified_plain_appends_do_not_rewrite_the_unfinished_line_again() {
        let mut source = Source::default();
        let mut body = "Answer".to_owned();
        let mut given = match source.advance(&body, true) {
            Step::Append(text) | Step::Restart(text) => text,
        };
        let rewritten = source.rewritten_bytes;
        for _ in 0..10000 {
            body.push_str(" ordinary α text");
            match source.advance_append(&body, true) {
                Step::Append(text) => given.push_str(&text),
                Step::Restart(text) => given = text,
            }
        }
        assert_eq!(source.rewritten_bytes, rewritten);
        assert_eq!(given, body);
        for suffix in [
            " <",
            "Type",
            ">",
            " `code",
            " span`",
            "\n",
            "    indented",
            "\n\n",
            "---\nnext",
        ] {
            body.push_str(suffix);
            match source.advance_append(&body, true) {
                Step::Append(text) => given.push_str(&text),
                Step::Restart(text) => given = text,
            }
            let expected = match Source::default().advance(&body, true) {
                Step::Append(text) | Step::Restart(text) => text,
            };
            assert_eq!(
                given, expected,
                "syntax/newlines return to the normal rewrite: {suffix:?}"
            );
        }
    }

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
            (
                "<mailto:me@example.com> <file:///tmp/a> <a.b+c@d-e.example>\n",
                "<mailto:me@example.com> <file:///tmp/a> <a.b+c@d-e.example>\n",
            ),
            // A path between a generic's brackets is not an address, though
            // the parser would take its first segment for a scheme.
            (
                "Vec<std::string> or Option<std::path::PathBuf>\n",
                "Vec\\<std::string> or Option\\<std::path::PathBuf>\n",
            ),
            (
                "<a:b> <T:Clone> <x@-y.z> <x@y..z>\n",
                "\\<a:b> \\<T:Clone> \\<x@-y.z> \\<x@y..z>\n",
            ),
            // Nothing is interpreted in a code span, so nothing is escaped.
            (
                "`Vec<T>` and ``a ` <b>`` then <c>\n",
                "`Vec<T>` and ``a ` <b>`` then \\<c>\n",
            ),
            ("a lone ` then <c>\n", "a lone ` then \\<c>\n"),
            ("a lone ` then <c>", "a lone ` then \\<c>"),
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

    /// A fence is left by a line that leaves the quote or the list item it
    /// is in, and by nothing that only looks like its closing line.
    #[test]
    fn a_fence_ends_where_its_container_does() {
        for (source, expected) in [
            // A quoted fence inside a fence is the code's own text.
            (
                "```\n> ```\nVec<T>\n---\n```\n<a>\n",
                "```\n> ```\nVec<T>\n---\n```\n\\<a>\n",
            ),
            (
                "> ```\n> > ```\n> <a>\n> ```\n> <b>\n",
                "> ```\n> > ```\n> <a>\n> ```\n> \\<b>\n",
            ),
            // A line that starts short of the item's text has left the item.
            (
                "- ```\n  code <a>\nout <x>\n",
                "- ```\n  code <a>\nout \\<x>\n",
            ),
            (
                "1. Step\n\n   ```\n   <a>\n\n  <b>\n",
                "1. Step\n\n   ```\n   <a>\n\n  \\<b>\n",
            ),
            ("> - ```\n>   <a>\n> <b>\n", "> - ```\n>   <a>\n> \\<b>\n"),
            // So a fence at the margin does not close the item's: it opens
            // one of its own.
            (
                "- ```\n  <a>\n```\n<x>\n```\n<y>\n",
                "- ```\n  <a>\n```\n<x>\n```\n\\<y>\n",
            ),
            // Blank lines and deeper lines stay in the item's fence.
            (
                "- a\n  - ```\n\n      <a>\n    <b>\n    ```\n  <c>\n",
                "- a\n  - ```\n\n      <a>\n    <b>\n    ```\n  \\<c>\n",
            ),
        ] {
            assert_eq!(rewrite(source), expected, "{source:?}");
        }
    }

    /// A code span runs on over the lines of its paragraph, and a backtick
    /// that no later line of the paragraph answers is only a backtick.
    #[test]
    fn a_code_span_runs_on_to_the_line_that_closes_it() {
        for (source, expected) in [
            (
                "a `code <x>\nstill` b <y>\n",
                "a `code <x>\nstill` b \\<y>\n",
            ),
            (
                "a ``code <x>\nmore ` <y>\n  still`` b <z>\n",
                "a ``code <x>\nmore ` <y>\n  still`` b \\<z>\n",
            ),
            (
                "- a `code <x>\n  still` <y>\n",
                "- a `code <x>\n  still` \\<y>\n",
            ),
            (
                "a `code <x>\r\nstill` <y>\r\n",
                "a `code <x>\r\nstill` \\<y>\r\n",
            ),
            ("a `code <x>\nstill` <y>", "a `code <x>\nstill` \\<y>"),
            // The paragraph ends first, so the span never opened.
            (
                "a ` lone <x>\nnext <y>\n\nafter` <z>\n",
                "a ` lone \\<x>\nnext \\<y>\n\nafter` \\<z>\n",
            ),
            ("a ` lone <x>\nnext <y>", "a ` lone \\<x>\nnext \\<y>"),
            ("a `b <x>\n- item` <y>\n", "a `b \\<x>\n- item` \\<y>\n"),
            ("a `b <x>\n# head` <y>\n", "a `b \\<x>\n# head` \\<y>\n"),
            ("a `b <x>\n```\n<c>`\n", "a `b \\<x>\n```\n<c>`\n"),
            ("a `b <x>\n---\nc` <y>\n", "a `b \\<x>\n----\nc` \\<y>\n"),
            // A heading is one line, and a table's cells are read apart.
            ("# a `b <x>\nc` <y>\n", "# a `b \\<x>\nc` \\<y>\n"),
            (
                "| a `b <x> | c |\n| d` | <y> |\n",
                "| a `b \\<x> | c |\n| d` | \\<y> |\n",
            ),
        ] {
            assert_eq!(rewrite(source), expected, "{source:?}");
        }
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
        "a ` lone <x>\nnext <y>\n\nb ``two <x>\nmore\n- item`` <z>\n",
        "> quoted <b>\n> \n> - item <c>\n>\n> ```\n> <code>\n> ```\n> \n\nafter <d>\n",
        "- ```\n  <a>\n```\n> ```\n<x>\n```\nVec<std::string> <br /> <https://a.b/c> <me@x.y\n",
    ];

    /// The whole of `body`, an answer that may still grow, as the parser is
    /// given it meanwhile.
    fn unfinished(body: &str) -> String {
        match Source::default().advance(body, true) {
            Step::Append(text) | Step::Restart(text) => text,
        }
    }

    /// However an answer is cut into deltas, the parser ends up holding the
    /// rewrite of the whole of it: every append extends what it was given,
    /// a restart hands over the whole rewrite again, and what was kept back
    /// while the answer grew is handed over once it has ended.
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
                let mut take = |step: Step, given: &mut String| match step {
                    Step::Append(text) => given.push_str(&text),
                    Step::Restart(text) => *given = text,
                };
                for end in &cut {
                    take(source.advance(&body[..*end], true), &mut given);
                    assert_eq!(given, unfinished(&body[..*end]), "{body:?} cut at {cut:?}");
                    // Only the end of the last line is ever kept back.
                    let ended = rewrite(&body[..*end]);
                    assert!(
                        source.provisional() || given == ended,
                        "{body:?} cut at {cut:?}"
                    );
                }
                take(source.advance(body, false), &mut given);
                assert_eq!(given, whole, "{body:?} cut at {cut:?}");
                assert!(!source.provisional());
            }
        }
    }

    /// The parser starts again only where a backtick turned out not to open
    /// a code span. A tag that turns out to be a link or a break was kept
    /// back until it had, however the answer was cut.
    #[test]
    fn only_a_stray_backtick_restarts_the_parse() {
        let restarts = |body: &str| {
            let mut source = Source::default();
            (1..=body.len())
                .filter(|end| body.is_char_boundary(*end))
                .filter(|end| matches!(source.advance(&body[..*end], true), Step::Restart(_)))
                .count()
        };
        assert_eq!(
            restarts(
                "Intro\n\n---\nUse Vec<Option<String>> and `Vec<T>` here.  \n\n    code\n\n+++\n```rust\nfn a<T>() {}\n```\n- item\n"
            ),
            0
        );
        assert_eq!(
            restarts(
                &"| a<br>b<BR />c | <https://example.com/a?b=c> <me@example.com> x < y <kbd>k</kbd> |\n"
                    .repeat(64)
            ),
            0
        );
        assert_eq!(restarts("a `code <x>\nstill` b <y>\n\nnext\n"), 0);
        assert_eq!(restarts("a ` lone <x>\n\nnext `b`\n"), 1);
    }

    /// What is kept back is the one unfinished word, and it is handed over
    /// with the byte that says what it is.
    #[test]
    fn an_unfinished_tag_is_kept_back_until_it_is_known() {
        let mut source = Source::default();
        let mut advance = |body: &str, open: bool| match source.advance(body, open) {
            Step::Append(text) => (text, source.provisional()),
            Step::Restart(text) => panic!("restarted with {text:?}"),
        };
        assert_eq!(advance("see <", true), ("see ".into(), true));
        assert_eq!(advance("see <https://exa", true), (String::new(), true));
        assert_eq!(
            advance("see <https://example.com> or one<b", true),
            ("<https://example.com> or one".into(), true)
        );
        assert_eq!(
            advance("see <https://example.com> or one<br /", true),
            (String::new(), true)
        );
        assert_eq!(
            advance("see <https://example.com> or one<br />two Vec<Opt", true),
            ("<br>two Vec".into(), true)
        );
        assert_eq!(
            advance(
                "see <https://example.com> or one<br />two Vec<Option<",
                true
            ),
            ("\\<Option".into(), true)
        );
        // The answer ended there, so the `<` is only that.
        assert_eq!(
            advance(
                "see <https://example.com> or one<br />two Vec<Option<",
                false
            ),
            ("\\<".into(), false)
        );
    }

    /// Reading a `<` looks no further than the word it starts, so a line of
    /// them is not read once for each.
    #[test]
    fn a_tag_is_read_no_further_than_its_word() {
        for (tag, rest) in [
            ("<https://example.com/a b", " c>"),
            ("<me@example.com b", " c>"),
            ("<Option<", "String>>"),
            ("<details ", "open>"),
        ] {
            let whole = format!("{tag}{rest}");
            assert_eq!(
                autolink(tag.as_bytes()),
                Scan::Nothing,
                "{tag:?} is settled by its own last byte"
            );
            assert_eq!(autolink(whole.as_bytes()), Scan::Nothing, "{whole:?}");
            assert_eq!(
                line_break(tag.as_bytes()),
                line_break(whole.as_bytes()),
                "{whole:?}"
            );
        }
        // Whitespace can still precede a line break's closing delimiter.
        assert_eq!(line_break(b"<br "), Scan::Unfinished);
        assert_eq!(line_break(b"<br x>"), Scan::Nothing);
        assert_eq!(line_break(b"<br />"), Scan::Whole(6));
    }

    #[test]
    fn a_rewritten_body_is_not_trusted_as_an_append() {
        let mut source = Source::default();
        assert_eq!(
            source.advance("one\ntwo\nthree", true),
            Step::Append("one\ntwo\nthree".into())
        );
        // Shorter than what was consumed: the caller's body is another text.
        assert_eq!(source.advance("one", true), Step::Restart("one".into()));
        assert_eq!(
            source.advance("one\n<b>", true),
            Step::Append("\n\\<b>".into())
        );
    }
}
