//! A command's output as a transcript shows it.
//!
//! A provider hands over what the command wrote, and a command that believes
//! it is writing to a terminal writes for one: colours as escape sequences,
//! window titles and hyperlinks as longer ones, and a progress line redrawn
//! in place with carriage returns. A transcript is text, where each of those
//! arrives as `[1;32m` and a line of percentages run together, so they are
//! taken out where the output enters it.
//!
//! All three adapters use this: [`display`] for output a provider reports
//! whole, and [`Stream`] for output it reports in deltas, where a sequence may
//! be cut anywhere.

/// Output reported whole, without its escape sequences and with each line as
/// it was last redrawn.
pub(super) fn display(raw: String) -> String {
    if !raw.bytes().any(interpreted) {
        return raw;
    }
    let mut text = String::with_capacity(raw.len());
    // Whatever is left unfinished at the end is part of a sequence, not text.
    strip(&raw, &mut text);
    let mut shown = String::with_capacity(text.len());
    let (mut line, mut returned) = (0, false);
    for character in text.chars() {
        match character {
            '\r' => returned = true,
            '\n' => {
                shown.push('\n');
                line = shown.len();
                returned = false;
            }
            character => {
                // A carriage return only rewrites the line once something is
                // written after it: before a newline it is a line ending.
                if std::mem::take(&mut returned) {
                    shown.truncate(line);
                }
                shown.push(character);
            }
        }
    }
    shown
}

/// Output reported in deltas, cleaned as it arrives.
///
/// What a delta contributes is only ever appended, because that is all a
/// delta may do to the output already sent to the surfaces. An escape
/// sequence cut by the end of a delta leaves only its grammar state; its
/// already-discarded payload is never buffered or scanned again.
/// A line redrawn with a carriage return cannot be taken back, so each
/// redraw is given a line of its own while the command runs - the newest is
/// the last line, which is what a reader following it sees - and the output
/// the provider reports whole at the end goes through [`display`], which
/// keeps only the last.
#[derive(Debug)]
pub(super) struct Stream {
    escape: Escape,
    returned: bool,
    /// Whether nothing has been written to the current line.
    fresh: bool,
    #[cfg(test)]
    steps: usize,
}

/// No sequence payload is needed to identify its terminator. In particular,
/// an arbitrarily long unterminated OSC/DCS/APC string costs one enum value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Escape {
    #[default]
    Ground,
    Introducer,
    CsiParameters,
    CsiIntermediates,
    Intermediates,
    String,
    StringEscape,
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            escape: Escape::Ground,
            returned: false,
            fresh: true,
            #[cfg(test)]
            steps: 0,
        }
    }
}

impl Stream {
    /// What `delta` adds to the output shown so far.
    pub(super) fn push(&mut self, delta: &str) -> String {
        let mut shown = String::new();
        for character in delta.chars() {
            if !self.visible(character) {
                continue;
            }
            match character {
                '\r' => self.returned = true,
                '\n' => {
                    shown.push('\n');
                    self.returned = false;
                    self.fresh = true;
                }
                character => {
                    if std::mem::take(&mut self.returned) && !self.fresh {
                        shown.push('\n');
                    }
                    shown.push(character);
                    self.fresh = false;
                }
            }
        }
        shown
    }

    fn visible(&mut self, character: char) -> bool {
        // A malformed sequence ends before the character that broke it. Retry
        // only that character in its new state, never the preceding payload.
        // At most StringEscape -> Introducer -> Ground examines it again.
        loop {
            #[cfg(test)]
            { self.steps += 1; }
            match self.escape {
                Escape::Ground => {
                    if character == '\x1b' {
                        self.escape = Escape::Introducer;
                        return false;
                    }
                    return !character.is_ascii()
                        || !interpreted(character as u8)
                        || character == '\r';
                }
                Escape::Introducer => {
                    self.escape = match character {
                        '[' => Escape::CsiParameters,
                        ']' | 'P' | 'X' | '^' | '_' => Escape::String,
                        '\x20'..='\x2f' => Escape::Intermediates,
                        '\x30'..='\x7e' => Escape::Ground,
                        _ => { self.escape = Escape::Ground; continue; }
                    };
                    return false;
                }
                Escape::CsiParameters => match character {
                    '\x30'..='\x3f' => return false,
                    '\x20'..='\x2f' => {
                        self.escape = Escape::CsiIntermediates;
                        return false;
                    }
                    '\x40'..='\x7e' => { self.escape = Escape::Ground; return false; }
                    _ => self.escape = Escape::Ground,
                },
                Escape::CsiIntermediates => match character {
                    '\x20'..='\x2f' => return false,
                    '\x40'..='\x7e' => { self.escape = Escape::Ground; return false; }
                    _ => self.escape = Escape::Ground,
                },
                Escape::Intermediates => match character {
                    '\x20'..='\x2f' => return false,
                    '\x30'..='\x7e' => { self.escape = Escape::Ground; return false; }
                    _ => self.escape = Escape::Ground,
                },
                Escape::String => match character {
                    '\x07' => { self.escape = Escape::Ground; return false; }
                    '\n' => self.escape = Escape::Ground,
                    '\x1b' => { self.escape = Escape::StringEscape; return false; }
                    _ => return false,
                },
                Escape::StringEscape => {
                    if character == '\\' {
                        self.escape = Escape::Ground;
                        return false;
                    }
                    // A different ESC sequence terminates the string before
                    // its introducer. That ESC now parses this character.
                    self.escape = Escape::Introducer;
                }
            }
        }
    }
}

/// Whether a byte is something a terminal acts on rather than shows.
fn interpreted(byte: u8) -> bool {
    (byte < 0x20 && !matches!(byte, b'\n' | b'\t')) || byte == 0x7f
}

/// Copies `raw` to `text` without its escape sequences and control
/// characters, keeping newlines, tabs and carriage returns. Returns how much
/// of `raw` was read: the rest is a sequence that has not ended yet.
fn strip(raw: &str, text: &mut String) -> usize {
    let bytes = raw.as_bytes();
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        let length = match bytes[at] {
            0x1b => match sequence(&bytes[at..]) {
                Some(length) => length,
                None => {
                    text.push_str(&raw[copied..at]);
                    return at;
                }
            },
            byte if interpreted(byte) && byte != b'\r' => 1,
            _ => {
                at += 1;
                continue;
            }
        };
        text.push_str(&raw[copied..at]);
        at += length;
        copied = at;
    }
    text.push_str(&raw[copied..]);
    bytes.len()
}

/// The length of the escape sequence `bytes` starts with, or `None` while it
/// is unfinished. A sequence that breaks its own grammar ends where it does,
/// so the byte that broke it is read as whatever it is.
fn sequence(bytes: &[u8]) -> Option<usize> {
    let run = |from: usize, range: std::ops::RangeInclusive<u8>| {
        from + bytes[from..]
            .iter()
            .take_while(|byte| range.contains(byte))
            .count()
    };
    match *bytes.get(1)? {
        // A control sequence: parameters, intermediates, one final byte. Every
        // colour and cursor movement is one of these.
        b'[' => {
            let end = run(run(2, 0x30..=0x3f), 0x20..=0x2f);
            match *bytes.get(end)? {
                0x40..=0x7e => Some(end + 1),
                _ => Some(end),
            }
        }
        // A string - a window title, a hyperlink - that runs to a bell or a
        // string terminator. One that is never terminated ends with its
        // line, so a stray introducer cannot swallow the rest of the output.
        b']' | b'P' | b'X' | b'^' | b'_' => {
            for (index, byte) in bytes.iter().enumerate().skip(2) {
                match byte {
                    0x07 => return Some(index + 1),
                    b'\n' => return Some(index),
                    0x1b => {
                        return match *bytes.get(index + 1)? {
                            b'\\' => Some(index + 2),
                            _ => Some(index),
                        };
                    }
                    _ => {}
                }
            }
            None
        }
        // A character-set selection and its like: intermediates, then a final.
        0x20..=0x2f => {
            let end = run(1, 0x20..=0x2f);
            match *bytes.get(end)? {
                0x30..=0x7e => Some(end + 1),
                _ => Some(end),
            }
        }
        0x30..=0x7e => Some(2),
        _ => Some(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(&str, &str)] = &[
        ("plain text\nwith\ttabs\n", "plain text\nwith\ttabs\n"),
        // Colours, bold and their resets.
        (
            "\x1b[1m\x1b[32m   Compiling\x1b[0m cagents v0.1.0\n",
            "   Compiling cagents v0.1.0\n",
        ),
        (
            "\x1b[38;5;196mred\x1b[39m \x1b[48;2;1;2;3mbg\x1b[m\n",
            "red bg\n",
        ),
        // Cursor movement, erasing and private modes.
        ("\x1b[2K\x1b[1Gdone\x1b[?25h\x1b[?2004l\n", "done\n"),
        // A window title, and a hyperlink ended either way.
        ("\x1b]0;cargo test\x07ready\n", "ready\n"),
        (
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07 after\n",
            "link after\n",
        ),
        // Device control and application strings.
        ("a\x1bP1$r0m\x1b\\b\x1b_payload\x1b\\c\n", "abc\n"),
        // Character sets, keypad modes, save and restore.
        ("\x1b(B\x1b=\x1b7text\x1b8\x1b>\n", "text\n"),
        // Bells, backspaces and other control characters.
        ("be\x07ep\x08 \x00\x7fok\n", "beep ok\n"),
        // Text that is not ASCII is untouched around a sequence.
        ("λ \x1b[31m→\x1b[0m é\n", "λ → é\n"),
        // A sequence that breaks its grammar ends there.
        ("\x1b[12\nnext\n", "\nnext\n"),
        ("\x1b]0;never ended\nnext\n", "\nnext\n"),
        ("a\x1b\x1b[1mb\n", "ab\n"),
        // Windows line endings are line endings.
        ("one\r\ntwo\r\n", "one\ntwo\n"),
    ];

    #[test]
    fn output_is_shown_without_what_a_terminal_would_act_on() {
        for (raw, shown) in CASES {
            assert_eq!(display((*raw).to_owned()), *shown, "{raw:?}");
        }
        // Output with nothing to remove is handed back as it came.
        let plain = String::from("untouched λ\n");
        let pointer = plain.as_ptr();
        let shown = display(plain);
        assert_eq!(shown.as_ptr(), pointer);
        // A sequence the output ends inside is not text.
        assert_eq!(display("done\x1b[3".into()), "done");
        assert_eq!(display("done\x1b]8;;https://exa".into()), "done");
        assert_eq!(display("done\x1b".into()), "done");
    }

    /// A progress line redrawn in place is the line it ended as.
    #[test]
    fn a_redrawn_line_is_shown_as_it_was_last_drawn() {
        for (raw, shown) in [
            (
                "Downloading 10%\rDownloading 55%\rDownloading 100%\ndone\n",
                "Downloading 100%\ndone\n",
            ),
            ("first\n 1/3\r 2/3\r 3/3", "first\n 3/3"),
            // Returning the cursor without writing anything redraws nothing.
            ("kept\r", "kept"),
            ("kept\r\r\n", "kept\n"),
            ("\x1b[2K\rbuilding\x1b[2K\rbuilt\n", "built\n"),
        ] {
            assert_eq!(display(raw.to_owned()), shown, "{raw:?}");
        }
    }

    /// However output is cut into deltas, the same text is appended: a
    /// sequence split by a delta's end never leaks its second half.
    #[test]
    fn streamed_output_is_cleaned_the_same_however_it_is_cut() {
        let mut cases: Vec<&str> = CASES.iter().map(|(raw, _)| *raw).collect();
        cases.push("\x1b[32m 1/3\x1b[0m\r\x1b[32m 2/3\x1b[0m\r\n\rnext\r\n");
        for raw in cases {
            let whole = Stream::default().push(raw);
            let boundaries: Vec<usize> = (0..=raw.len())
                .filter(|index| raw.is_char_boundary(*index))
                .collect();
            for first in &boundaries {
                for second in boundaries.iter().filter(|second| *second >= first) {
                    let mut stream = Stream::default();
                    let mut shown = stream.push(&raw[..*first]);
                    assert!(!shown.contains('\x1b'));
                    shown.push_str(&stream.push(&raw[*first..*second]));
                    shown.push_str(&stream.push(&raw[*second..]));
                    assert_eq!(shown, whole, "{raw:?} cut at {first} and {second}");
                }
            }
        }
        // Without a redrawn line, what was streamed is what the whole shows.
        for (raw, shown) in CASES {
            assert_eq!(Stream::default().push(raw), *shown, "{raw:?}");
        }
    }

    /// A redraw cannot take back what a delta already appended, so each one
    /// is a line while the command runs and the last is the newest.
    #[test]
    fn a_streamed_redraw_starts_a_line_of_its_own() {
        let mut stream = Stream::default();
        let mut shown = String::new();
        for delta in [
            "Downloading 10%",
            "\rDownloading 55%\r",
            "Downloading 100%\r\n",
            "\rdone\n",
        ] {
            shown.push_str(&stream.push(delta));
        }
        assert_eq!(
            shown,
            "Downloading 10%\nDownloading 55%\nDownloading 100%\ndone\n"
        );
    }

    #[test]
    fn fragmented_unclosed_sequences_keep_constant_storage_and_linear_work() {
        let string_payload = "private escape payload λ".repeat(512);
        let parameters = "123456789;".repeat(1024);
        let intermediates = " ".repeat(8192);
        let cases = [
            ("\x1b]", string_payload.as_str(), Escape::String, "\x1b\\visible"),
            ("\x1bP", string_payload.as_str(), Escape::String, "\x07visible"),
            ("\x1bX", string_payload.as_str(), Escape::String, "\nvisible"),
            ("\x1b^", string_payload.as_str(), Escape::String, "\x1b\\visible"),
            ("\x1b_", string_payload.as_str(), Escape::String, "\x07visible"),
            ("\x1b[", parameters.as_str(), Escape::CsiParameters, "mvisible"),
            ("\x1b[ ", intermediates.as_str(), Escape::CsiIntermediates, "mvisible"),
            ("\x1b(", intermediates.as_str(), Escape::Intermediates, "Bvisible"),
        ];
        for (prefix, payload, pending, terminator) in cases {
            let mut stream = Stream::default();
            assert_eq!(stream.push("before "), "before ");
            assert!(stream.push(prefix).is_empty());
            let mut characters = "before ".chars().count() + prefix.chars().count();
            for _ in 0..256 {
                assert!(stream.push(payload).is_empty());
                characters += payload.chars().count();
                assert_eq!(stream.escape, pending);
                // The test-only step counter is included in this bound. The
                // production state contains only the enum and two booleans.
                assert!(std::mem::size_of_val(&stream) <= 16);
            }
            // Exercise every cut of the terminator, including ESC | backslash.
            let mut shown = String::new();
            let mut encoded = [0; 4];
            for character in terminator.chars() {
                shown.push_str(&stream.push(character.encode_utf8(&mut encoded)));
                characters += 1;
            }
            assert_eq!(shown, if terminator.starts_with('\n') { "\nvisible" } else { "visible" });
            assert_eq!(stream.escape, Escape::Ground);
            assert!(stream.steps <= 3 * characters,
                "discarded payload must never be scanned again at a later delta");
        }
    }

    #[test]
    fn incomplete_grammar_recovers_like_whole_output_for_every_ascii_byte() {
        for prefix in ["\x1b", "\x1b[12", "\x1b[ ", "\x1b(", "\x1b]hidden", "\x1b]hidden\x1b"] {
            for byte in 0..=127u8 {
                let tail = format!("{}tail\n", char::from(byte));
                let expected = display(format!("{prefix}{tail}"));
                let mut stream = Stream::default();
                assert!(stream.push(prefix).is_empty());
                assert_eq!(stream.push(&tail), expected, "prefix {prefix:?}, byte {byte}");
            }
        }
    }
}
