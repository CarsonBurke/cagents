//! Bounded capture of actual provider stderr. This module has no transport,
//! process, transcript, or copy authority; its caller must supply those proofs.
//! The 8 MiB cap applies to raw bytes for one capture, not global memory usage
//! or the larger readable UTF-8 representation. A pipe can retain unread bytes
//! after stdout EOF or process exit, and a descendant can keep stderr open.
//! Only an actual stderr EOF observation justifies `End::StderrEof`.
use std::collections::VecDeque;

pub(super) const RAW_BYTES: usize = 8 * 1024 * 1024;
const SUMMARY_BYTES: usize = 1024;

#[derive(Default)]
pub(super) struct Capture {
    raw: VecDeque<u8>,
    total: u64,
    prefix: Decoder,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    pub raw_total: u64,
    pub raw_retained: usize,
    pub raw_prefix_omitted: u64,
    pub utf8_boundary_bytes_omitted: usize,
    pub invalid_utf8_replacements: usize,
    pub control_bytes_removed: usize,
    pub unfinished_terminal_sequence: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Captured {
    pub text: String,
    pub metadata: Metadata,
}

#[derive(Clone, Copy)]
pub(super) enum Origin {
    Claude,
    Codex,
    Omp,
}

#[derive(Clone, Copy)]
pub(super) enum Exit {
    Code(i32),
    Signal(i32),
    Unconfirmed,
}

#[derive(Clone, Copy)]
pub(super) enum End {
    StderrEof,
    BeforeStderrEof,
}

pub(super) struct Report {
    pub summary: String,
    pub body: String,
}

impl Capture {
    /// Only bytes actually read from this provider's stderr belong here.
    /// Overflow refuses the entire append before changing the retained tail.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        let additional = u64::try_from(bytes.len()).map_err(|_| "Stderr byte count overflow")?;
        let total = self
            .total
            .checked_add(additional)
            .ok_or("Stderr byte count overflow")?;
        let mut ignored = Readable::default();
        for &byte in bytes {
            if self.raw.len() == RAW_BYTES {
                // Advance the actual discarded stream, including unfinished
                // UTF-8 and terminal strings. A tail beginning inside an OSC
                // must not expose the discarded command's remaining payload.
                self.prefix
                    .push(self.raw.pop_front().unwrap(), false, &mut ignored);
            }
            self.raw.push_back(byte);
        }
        self.total = total;
        Ok(())
    }

    pub fn finish(self) -> Captured {
        let retained = self.raw.len();
        let mut decoder = self.prefix;
        let mut readable = Readable::default();
        for byte in self.raw {
            decoder.push(byte, true, &mut readable);
        }
        decoder.flush_utf8(&mut readable);
        Captured {
            text: readable.text,
            metadata: Metadata {
                raw_total: self.total,
                raw_retained: retained,
                raw_prefix_omitted: self.total - retained as u64,
                utf8_boundary_bytes_omitted: readable.boundary,
                invalid_utf8_replacements: readable.replacements,
                control_bytes_removed: readable.controls,
                unfinished_terminal_sequence: !matches!(decoder.escape, Escape::Ground),
            },
        }
    }
}

impl Captured {
    /// Labels describe captured bytes, never a log file or a complete provider
    /// transcript. Status and EOF must come from the caller's actual observation.
    pub fn report(self, origin: Origin, exit: Exit, end: End) -> Report {
        let provider = match origin {
            Origin::Claude => "Claude",
            Origin::Codex => "Codex",
            Origin::Omp => "Omp",
        };
        let status = match exit {
            Exit::Code(code) => format!("{provider} exited with status {code}."),
            Exit::Signal(signal) => format!("{provider} exited after signal {signal}."),
            Exit::Unconfirmed => format!("{provider} exit status is unconfirmed."),
        };
        let m = &self.metadata;
        let completion = match end {
            End::StderrEof => "Capture reached stderr EOF.",
            End::BeforeStderrEof => {
                "Capture stopped before stderr EOF; later output may be unavailable."
            }
        };
        let unfinished = if m.unfinished_terminal_sequence {
            " An unfinished terminal sequence was withheld."
        } else {
            ""
        };
        let summary = summary(&status, &self.text);
        let header = format!(
            "Captured {provider} stderr\n{status}\nObserved raw bytes: {}; retained tail: {}; omitted prefix: {}.\nUTF-8: {} invalid sequences replaced; {} boundary bytes omitted.\nTerminal controls removed: {} bytes.{unfinished}\n{completion}\n\n",
            m.raw_total,
            m.raw_retained,
            m.raw_prefix_omitted,
            m.invalid_utf8_replacements,
            m.utf8_boundary_bytes_omitted,
            m.control_bytes_removed,
        );
        // Transfer the readable body rather than retaining a second full-body
        // owner alongside the report. Allocator capacity is not the raw cap.
        let mut body = self.text;
        body.insert_str(0, &header);
        Report { summary, body }
    }
}

fn summary(status: &str, text: &str) -> String {
    let mut remaining = SUMMARY_BYTES.saturating_sub(status.len());
    let mut lines = Vec::new();
    for line in text.lines().rev().map(str::trim).filter(|s| !s.is_empty()) {
        if lines.len() == 4 || remaining <= 1 {
            break;
        }
        remaining -= 1; // Newline before this line.
        let mut start = line.len().saturating_sub(remaining);
        while !line.is_char_boundary(start) {
            start += 1;
        }
        let tail = &line[start..];
        if tail.is_empty() {
            break;
        }
        remaining -= tail.len();
        lines.push(tail);
    }
    let mut result = status.to_owned();
    for line in lines.into_iter().rev() {
        result.push('\n');
        result.push_str(line);
    }
    result
}

#[derive(Default)]
struct Readable {
    text: String,
    boundary: usize,
    replacements: usize,
    controls: usize,
}

#[derive(Clone, Copy, Default)]
enum Escape {
    #[default]
    Ground,
    Esc,
    Intermediate,
    Csi,
    String {
        osc: bool,
        esc: bool,
        c2: bool,
    },
}

#[derive(Default)]
struct Decoder {
    escape: Escape,
    utf8: [u8; 4],
    retained: [bool; 4],
    len: usize,
}

impl Decoder {
    fn push(&mut self, byte: u8, retained: bool, out: &mut Readable) {
        if !matches!(self.escape, Escape::Ground) {
            out.controls += usize::from(retained);
            self.escape = match self.escape {
                Escape::Esc => match byte {
                    b'[' => Escape::Csi,
                    b']' => Escape::String {
                        osc: true,
                        esc: false,
                        c2: false,
                    },
                    b'P' | b'X' | b'^' | b'_' => Escape::String {
                        osc: false,
                        esc: false,
                        c2: false,
                    },
                    0x20..=0x2f => Escape::Intermediate,
                    0x1b => Escape::Esc,
                    _ => Escape::Ground,
                },
                Escape::Intermediate | Escape::Csi if byte == 0x1b => Escape::Esc,
                Escape::Intermediate if (0x30..=0x7e).contains(&byte) => Escape::Ground,
                Escape::Csi if (0x40..=0x7e).contains(&byte) => Escape::Ground,
                Escape::String { osc, esc, c2 }
                    if (osc && byte == 7) || (esc && byte == b'\\') || (c2 && byte == 0x9c) =>
                {
                    Escape::Ground
                }
                Escape::String { osc, .. } => Escape::String {
                    osc,
                    esc: byte == 0x1b,
                    c2: byte == 0xc2,
                },
                ref other => match other {
                    Escape::Intermediate => Escape::Intermediate,
                    Escape::Csi => Escape::Csi,
                    _ => unreachable!(),
                },
            };
            return;
        }
        if byte == 0x1b || matches!(byte, 0..=8 | 11..=31 | 127) {
            self.flush_utf8(out);
            out.controls += usize::from(retained);
            if byte == 0x1b {
                self.escape = Escape::Esc;
            }
            return;
        }
        self.utf8[self.len] = byte;
        self.retained[self.len] = retained;
        self.len += 1;
        while self.len > 0 {
            match std::str::from_utf8(&self.utf8[..self.len]) {
                Ok(text) => {
                    let ch = text.chars().next().unwrap();
                    let kept = self.retained[..self.len].iter().filter(|b| **b).count();
                    if ch.is_control() && ch != '\n' && ch != '\t' {
                        out.controls += kept;
                        self.escape = match ch {
                            '\u{009b}' => Escape::Csi,
                            '\u{009d}' => Escape::String {
                                osc: true,
                                esc: false,
                                c2: false,
                            },
                            '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => Escape::String {
                                osc: false,
                                esc: false,
                                c2: false,
                            },
                            _ => Escape::Ground,
                        };
                    } else if self.retained[0] {
                        out.text.push(ch);
                    } else {
                        out.boundary += kept;
                    }
                    self.len = 0;
                }
                Err(error) => {
                    let Some(invalid) = error.error_len() else {
                        break;
                    };
                    self.invalid(invalid, out);
                    self.utf8.copy_within(invalid..self.len, 0);
                    self.retained.copy_within(invalid..self.len, 0);
                    self.len -= invalid;
                }
            }
        }
    }

    fn invalid(&self, bytes: usize, out: &mut Readable) {
        if self.retained[0] {
            out.text.push('\u{fffd}');
            out.replacements += 1;
        } else {
            out.boundary += self.retained[..bytes].iter().filter(|b| **b).count();
        }
    }

    fn flush_utf8(&mut self, out: &mut Readable) {
        if self.len > 0 {
            self.invalid(self.len, out);
            self.len = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(parts: &[&[u8]]) -> Captured {
        let mut capture = Capture::default();
        for part in parts {
            capture.push(part).unwrap();
        }
        capture.finish()
    }

    #[test]
    fn exact_raw_limit_and_chunked_overflow_conserve_the_actual_tail() {
        let exact = vec![b'a'; RAW_BYTES];
        let mut c = Capture::default();
        for chunk in exact.chunks(8191) {
            c.push(chunk).unwrap();
        }
        c.push(b"LAST").unwrap();
        let captured = c.finish();
        assert_eq!(captured.metadata.raw_total, RAW_BYTES as u64 + 4);
        assert_eq!(captured.metadata.raw_retained, RAW_BYTES);
        assert_eq!(captured.metadata.raw_prefix_omitted, 4);
        let expected = [&exact[4..], b"LAST"].concat();
        assert_eq!(captured.text.as_bytes(), expected.as_slice());
        let exact = capture(&[&exact]);
        assert_eq!(exact.metadata.raw_prefix_omitted, 0);
        assert_eq!(exact.text.len(), RAW_BYTES);
    }

    #[test]
    fn split_utf8_appends_preserve_every_scalar_and_invalid_sequence() {
        let bytes = b"head \xf0\x9f\xa6\x80\xc3\xa9\xe0\x80\x80\xed\xa0\x80\xff\xe2\x82";
        let expected = String::from_utf8_lossy(bytes);
        for split in 0..=bytes.len() {
            let result = capture(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(result.text, expected);
            assert_eq!(result.metadata.invalid_utf8_replacements, 8);
            assert_eq!(result.metadata.utf8_boundary_bytes_omitted, 0);
        }
    }

    #[test]
    fn raw_tail_cut_inside_a_scalar_drops_only_its_retained_fragment() {
        for omitted in 1..4 {
            let mut bytes = "🦀".as_bytes().to_vec();
            bytes.extend(vec![b'x'; RAW_BYTES - 4 + omitted]);
            let result = capture(&[&bytes]);
            assert_eq!(result.metadata.raw_prefix_omitted, omitted as u64);
            assert_eq!(result.metadata.utf8_boundary_bytes_omitted, 4 - omitted);
            assert_eq!(result.metadata.invalid_utf8_replacements, 0);
            assert_eq!(result.text.as_bytes(), &bytes[4..]);
        }
    }

    #[test]
    fn terminal_sequences_are_removed_across_every_append_boundary() {
        let bytes = b"a\x1b[31mred\x1b[0m\x1b]8;;private://token\x07link\x1b]8;;\x1b\\\x1bPsecret\x1b\\\x1b^hidden\x1b\\\x1b_hidden\x1b\\\r\n\tend";
        for split in 0..=bytes.len() {
            let result = capture(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(result.text, "aredlink\n\tend");
            assert_eq!(
                result.metadata.control_bytes_removed,
                bytes.len() - result.text.len()
            );
            assert!(!result.metadata.unfinished_terminal_sequence);
        }
    }

    #[test]
    fn tail_inside_an_omitted_terminal_string_never_exposes_its_payload() {
        let mut c = Capture::default();
        c.push(b"\x1b]8;;").unwrap();
        c.push(&vec![b's'; RAW_BYTES]).unwrap();
        c.push(b"\x1b\\visible").unwrap();
        let result = c.finish();
        assert_eq!(result.text, "visible");
        assert!(result.metadata.raw_prefix_omitted > 0);
        assert_eq!(result.metadata.control_bytes_removed, RAW_BYTES - 7);
        assert_eq!(result.metadata.invalid_utf8_replacements, 0);
    }

    #[test]
    fn unicode_terminal_controls_and_incomplete_strings_are_accounted() {
        let result =
            capture(&["a\u{009b}31mred\u{009d}private\u{009c}tail\u{0090}secret".as_bytes()]);
        assert_eq!(result.text, "aredtail");
        assert!(result.metadata.unfinished_terminal_sequence);
        assert_eq!(
            result.metadata.raw_retained,
            result.text.len() + result.metadata.control_bytes_removed
        );
        assert_eq!(result.metadata.invalid_utf8_replacements, 0);
    }

    #[test]
    fn readable_report_labels_state_the_actual_capture_limits_and_observation() {
        let captured = capture(&[b"first\nlast reason\n"]);
        let report = captured.report(Origin::Codex, Exit::Code(3), End::StderrEof);
        assert!(
            report
                .body
                .starts_with("Captured Codex stderr\nCodex exited with status 3.")
        );
        assert_eq!(report.body.matches("omitted prefix:").count(), 1);
        assert!(
            report
                .body
                .contains("Observed raw bytes: 18; retained tail: 18; omitted prefix: 0.")
        );
        assert!(report.body.contains("Capture reached stderr EOF."));
        assert!(report.body.ends_with("first\nlast reason\n"));
        assert_eq!(
            report.summary,
            "Codex exited with status 3.\nfirst\nlast reason"
        );
        let unconfirmed = capture(&[b"first\nlast reason\n"]).report(
            Origin::Claude,
            Exit::Unconfirmed,
            End::BeforeStderrEof,
        );
        assert!(unconfirmed.body.contains("exit status is unconfirmed"));
        assert!(unconfirmed.body.contains("later output may be unavailable"));
        assert!(!unconfirmed.body.contains("Capture reached stderr EOF"));
        assert!(
            capture(&[b"first\nlast reason\n"])
                .report(Origin::Omp, Exit::Signal(15), End::StderrEof)
                .body
                .contains("signal 15")
        );
    }

    #[test]
    fn summary_is_bounded_without_rewriting_the_captured_body() {
        let text = format!(
            "first\nsecond\nthird\nfourth\nfifth\n{}last",
            "🦀".repeat(900)
        );
        let captured = capture(&[text.as_bytes()]);
        assert_eq!(captured.text, text);
        let report = captured.report(Origin::Claude, Exit::Code(1), End::StderrEof);
        assert!(report.summary.len() <= SUMMARY_BYTES);
        assert!(report.summary.lines().count() <= 5);
        assert!(report.summary.ends_with("last"));
        assert!(!report.summary.contains("first"));
        assert!(report.body.ends_with(&text));
    }

    #[test]
    fn maximum_invalid_utf8_expansion_stays_below_existing_copy_limit() {
        let captured = capture(&[&vec![0xff; RAW_BYTES]]);
        assert_eq!(captured.metadata.invalid_utf8_replacements, RAW_BYTES);
        assert_eq!(captured.text.len(), 3 * RAW_BYTES);
        let report = captured.report(Origin::Codex, Exit::Code(3), End::StderrEof);
        assert!(report.body.len() < 32 * 1024 * 1024);
    }

    #[test]
    fn counter_overflow_refuses_before_mutating_any_capture_bytes() {
        let mut capture = Capture::default();
        capture.push(b"kept").unwrap();
        capture.total = u64::MAX;
        assert_eq!(capture.push(b"refused"), Err("Stderr byte count overflow"));
        assert_eq!(capture.raw.iter().copied().collect::<Vec<_>>(), b"kept");
        assert_eq!(capture.total, u64::MAX);
    }
}
