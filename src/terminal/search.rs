//! Bounded scrollback search on the terminal-owner thread.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    #[serde(default)]
    pub history: super::snapshot::HistoryPosition,
    pub start_row: u64,
    pub start_column: u16,
    pub end_row: u64,
    pub end_column: u16,
}

impl SearchMatch {
    pub fn rebase(mut self, current: super::snapshot::HistoryPosition) -> Option<Self> {
        self.start_row = current.rebase_row(self.start_row, self.history)?;
        self.end_row = current.rebase_row(self.end_row, self.history)?;
        self.history = current;
        Some(self)
    }
}

/// Streaming matching shared by the live emulator and immutable completed rows.
#[cfg(feature = "host")]
pub(super) struct Matcher {
    query: String,
    case_sensitive: bool,
    text: String,
    starts: Vec<Option<(u64, u16)>>,
    ends: Vec<Option<(u64, u16)>>,
    matches: Vec<SearchMatch>,
    truncated: bool,
}

#[cfg(feature = "host")]
impl Matcher {
    pub(super) fn new(query: &str, case_sensitive: bool) -> Self {
        use unicode_casefold::UnicodeCaseFold;
        Self {
            query: if case_sensitive {
                query.to_owned()
            } else {
                query.case_fold().collect()
            },
            case_sensitive,
            text: String::new(),
            starts: Vec::new(),
            ends: Vec::new(),
            matches: Vec::new(),
            truncated: false,
        }
    }

    pub(super) fn append(&mut self, character: char, row: u64, column: u16, end_column: u16) {
        use unicode_casefold::UnicodeCaseFold;
        let start = self.text.len();
        if self.case_sensitive || character.is_ascii() {
            self.text.push(if self.case_sensitive {
                character
            } else {
                character.to_ascii_lowercase()
            });
        } else {
            self.text.extend(std::iter::once(character).case_fold());
        }
        self.starts.resize(self.text.len(), None);
        self.ends.resize(self.text.len(), None);
        self.starts[start] = Some((row, column));
        self.ends[self.text.len() - 1] = Some((row, end_column));
    }

    /// Formatter ASCII rows have exactly one byte per terminal column. Append
    /// and fold the whole row before extending its coordinate maps once.
    pub(super) fn append_ascii_row(&mut self, text: &str, row: u64) {
        debug_assert!(text.is_ascii() && text.len() <= usize::from(u16::MAX));
        let start = self.text.len();
        self.text.push_str(text);
        if !self.case_sensitive {
            self.text[start..].make_ascii_lowercase();
        }
        self.starts
            .extend((0..text.len()).map(|column| Some((row, column as u16))));
        self.ends
            .extend((0..text.len()).map(|column| Some((row, column as u16 + 1))));
    }

    pub(super) fn finish_row(&mut self, wrapped: bool) -> bool {
        if self.query.is_empty() {
            self.text.clear();
            self.starts.clear();
            self.ends.clear();
            return true;
        }
        let mut offset = 0;
        while offset + self.query.len() <= self.text.len() {
            // Reject impossible source boundaries before constructing a
            // substring searcher. Repeated partial expansion hits would
            // otherwise rebuild it at every scalar (e.g. ß / odd-length s).
            if self.starts[offset].is_none() || self.ends[offset + self.query.len() - 1].is_none() {
                offset += self.text[offset..].chars().next().unwrap().len_utf8();
                continue;
            }
            let Some(found) = self.text[offset..].find(&self.query) else {
                // Retain only the unresolved suffix for a wrapped next row,
                // just as if every remaining candidate had been examined.
                offset = self.text.len() - self.query.len() + 1;
                break;
            };
            let start = offset + found;
            let end = start + self.query.len();
            if let (Some((start_row, start_column)), Some((end_row, end_column))) =
                (self.starts[start], self.ends[end - 1])
            {
                if self.matches.len() == 10_000 {
                    self.truncated = true;
                    return false;
                }
                self.matches.push(SearchMatch {
                    history: Default::default(),
                    start_row,
                    start_column,
                    end_row,
                    end_column,
                });
                offset = end;
            } else {
                // A hit within a case-fold expansion is not a whole source
                // character. Keep overlapping candidates eligible, advancing
                // by one scalar so the next `find` starts on a UTF-8 boundary.
                offset = start + self.text[start..].chars().next().unwrap().len_utf8();
            }
        }
        if wrapped {
            while !self.text.is_char_boundary(offset) {
                offset -= 1;
            }
            self.text.drain(..offset);
            self.starts.drain(..offset);
            self.ends.drain(..offset);
        } else {
            self.text.clear();
            self.starts.clear();
            self.ends.clear();
        }
        true
    }

    pub(super) fn results(self) -> (Vec<SearchMatch>, bool) {
        (self.matches, self.truncated)
    }
}

#[cfg(feature = "host")]
pub(super) fn selected_text(
    terminal: &libghostty_vt::Terminal<'_, '_>,
    range: SearchMatch,
) -> Result<String, String> {
    use libghostty_vt::{
        fmt::{Format, Formatter, FormatterOptions},
        selection::Selection,
        terminal::{Point, PointCoordinate},
    };
    let total = terminal
        .scrollbar()
        .map_err(|error| error.to_string())?
        .total;
    let columns = terminal.cols().map_err(|error| error.to_string())?;
    if range.start_row > range.end_row
        || range.end_row >= total
        || range.end_column == 0
        || range.start_column >= columns
        || range.end_column > columns
    {
        return Err("The selected terminal output is no longer available".into());
    }
    let point = |row, x| terminal.grid_ref(Point::Screen(PointCoordinate { x, y: row as u32 }));
    let selection = Selection::new(
        point(range.start_row, range.start_column).map_err(|e| e.to_string())?,
        point(range.end_row, range.end_column - 1).map_err(|e| e.to_string())?,
        false,
    );
    let mut formatter = Formatter::new(
        terminal,
        FormatterOptions::new()
            .with_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true)
            .with_selection(&selection),
    )
    .map_err(|e| e.to_string())?;
    let length = formatter.format_len().map_err(|e| e.to_string())?;
    if length > super::ipc::MAX_FRAME_BYTES - 4096 {
        return Err(
            "The selection exceeds the clipboard transfer limit (8 MiB). Select a smaller range."
                .into(),
        );
    }
    let mut bytes = vec![0; length];
    let written = formatter
        .format_buf(&mut bytes)
        .map_err(|e| e.to_string())?;
    bytes.truncate(written);
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

#[cfg(feature = "host")]
pub(super) fn search(
    terminal: &libghostty_vt::Terminal<'_, '_>,
    query: &str,
    case_sensitive: bool,
    cancelled: impl Fn() -> bool,
) -> Result<Option<(Vec<SearchMatch>, bool)>, libghostty_vt::Error> {
    use libghostty_vt::{
        fmt::{Format, Formatter, FormatterOptions},
        selection::Selection,
        terminal::{Point, PointCoordinate},
    };
    const ROWS_PER_BATCH: u64 = 256;
    if query.is_empty() {
        return Ok(Some((Vec::new(), false)));
    }
    let total = terminal.scrollbar()?.total;
    let columns = terminal.cols()?;
    if columns == 0 || total == 0 {
        return Ok(Some((Vec::new(), false)));
    }
    let mut matcher = Matcher::new(query, case_sensitive);
    let mut graphemes = vec!['\0'; 32];
    for first in (0..total).step_by(ROWS_PER_BATCH as usize) {
        if cancelled() {
            return Ok(None);
        }
        let end = (first + ROWS_PER_BATCH).min(total);
        let reference =
            |row: u64, x| terminal.grid_ref(Point::Screen(PointCoordinate { x, y: row as u32 }));
        let selection = Selection::new(
            reference(first, 0)?,
            reference(end - 1, columns - 1)?,
            false,
        );
        let options = FormatterOptions::new()
            .with_format(Format::Plain)
            .with_unwrap(false)
            .with_trim(false)
            .with_selection(&selection);
        let bytes = Formatter::new(terminal, options)?.format_alloc(None)?;
        let text = String::from_utf8_lossy(&bytes);
        for (index, text) in text.split('\n').take((end - first) as usize).enumerate() {
            let row = first + index as u64;
            if text.is_ascii() {
                matcher.append_ascii_row(text, row);
            } else {
                // Read the owner's real cells for Unicode rows. Width tables
                // cannot reproduce both grapheme and legacy terminal modes.
                for x in 0..columns {
                    let point = reference(row, x)?;
                    let cell = point.cell()?;
                    let width = match cell.wide()? {
                        libghostty_vt::screen::CellWide::SpacerTail
                        | libghostty_vt::screen::CellWide::SpacerHead => continue,
                        libghostty_vt::screen::CellWide::Wide => 2,
                        libghostty_vt::screen::CellWide::Narrow => 1,
                    };
                    let length = match point.graphemes(&mut graphemes) {
                        Err(libghostty_vt::Error::OutOfSpace { required }) => {
                            graphemes.resize(required, '\0');
                            point.graphemes(&mut graphemes)?
                        }
                        result => result?,
                    };
                    if length == 0 {
                        matcher.append(' ', row, x, x + width);
                    } else {
                        for character in &graphemes[..length] {
                            matcher.append(*character, row, x, x + width);
                        }
                    }
                }
            }
            let wrapped = reference(row, 0)?.row()?.is_wrapped()? && row + 1 < total;
            if !matcher.finish_row(wrapped) {
                return Ok(Some(matcher.results()));
            }
        }
    }
    Ok(Some(matcher.results()))
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use libghostty_vt::{Terminal, TerminalOptions};

    #[test]
    fn history_ranges_rebase_and_reject_removed_or_replaced_origins() {
        use crate::terminal::HistoryPosition;
        let captured = HistoryPosition { epoch: 2, dropped: 100 };
        let found = SearchMatch { history: captured, start_row: 8, end_row: 9, start_column: 1, end_column: 4 };
        let current = HistoryPosition { epoch: 2, dropped: 103 };
        let rebased = found.rebase(current).unwrap();
        assert_eq!((rebased.start_row, rebased.end_row), (5, 6));
        assert_eq!(rebased.history, current);
        assert!(found.rebase(HistoryPosition { epoch: 2, dropped: 109 }).is_none());
        assert!(found.rebase(HistoryPosition { epoch: 3, dropped: 0 }).is_none());
        assert!(found.rebase(HistoryPosition { epoch: 2, dropped: 99 }).is_none());
    }

    // Independent whole-logical-line oracle: deliberately scan each byte,
    // rather than sharing the streaming matcher's search or suffix handling.
    fn reference_matches(
        rows: &[(&str, bool)],
        query: &str,
        case_sensitive: bool,
    ) -> Vec<SearchMatch> {
        use unicode_casefold::UnicodeCaseFold;
        let query = if case_sensitive {
            query.to_owned()
        } else {
            query.case_fold().collect()
        };
        if query.is_empty() {
            return Vec::new();
        }
        let mut text = String::new();
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        let mut matches = Vec::new();
        for (row, (line, wrapped)) in rows.iter().enumerate() {
            for (column, character) in line.chars().enumerate() {
                let start = text.len();
                if case_sensitive {
                    text.push(character);
                } else {
                    text.extend(std::iter::once(character).case_fold());
                }
                starts.resize(text.len(), None);
                ends.resize(text.len(), None);
                starts[start] = Some((row as u64, column as u16));
                ends[text.len() - 1] = Some((row as u64, column as u16 + 1));
            }
            if *wrapped && row + 1 < rows.len() {
                continue;
            }
            let mut offset = 0;
            while offset + query.len() <= text.len() {
                let end = offset + query.len();
                if let (Some((start_row, start_column)), Some((end_row, end_column))) =
                    (starts[offset], ends[end - 1])
                    && text.get(offset..end) == Some(query.as_str())
                {
                    matches.push(SearchMatch {
                        history: Default::default(),
                        start_row,
                        start_column,
                        end_row,
                        end_column,
                    });
                    offset = end;
                } else {
                    offset += 1;
                }
            }
            text.clear();
            starts.clear();
            ends.clear();
        }
        matches
    }

    #[test]
    fn streaming_matches_whole_line_oracle_with_unicode_and_wraps() {
        let texts = [
            "", "a", "ß", "é", "日", "ﬃ", "İ", "aa", "sß", "ßs", "aé", "éa",
        ];
        let queries = [
            "", "a", "aa", "s", "ss", "sss", "é", "aé", "éa", "i", "\u{307}", "İ", "ffi", "fi",
            "日", "日a",
        ];
        for left in texts {
            for right in texts {
                for wrapped in [false, true] {
                    let rows = [(left, wrapped), (right, true), ("aßé日", false)];
                    for query in queries {
                        for case_sensitive in [false, true] {
                            let mut matcher = Matcher::new(query, case_sensitive);
                            for (row, (text, wrap)) in rows.iter().enumerate() {
                                for (column, character) in text.chars().enumerate() {
                                    matcher.append(
                                        character,
                                        row as u64,
                                        column as u16,
                                        column as u16 + 1,
                                    );
                                }
                                assert!(matcher.finish_row(*wrap));
                            }
                            assert_eq!(
                                matcher.results(),
                                (reference_matches(&rows, query, case_sensitive), false),
                                "rows={rows:?} query={query:?} case_sensitive={case_sensitive}"
                            );
                        }
                    }
                }
            }
        }
        let rows = [("日é", true), ("ß日", true), ("éß", false)];
        let mut matcher = Matcher::new("日éß日éß", false);
        for (row, (text, wrapped)) in rows.iter().enumerate() {
            for (column, character) in text.chars().enumerate() {
                matcher.append(character, row as u64, column as u16, column as u16 + 1);
            }
            assert!(matcher.finish_row(*wrapped));
        }
        assert_eq!(
            matcher.results().0,
            reference_matches(&rows, "日éß日éß", false)
        );
    }

    #[test]
    fn result_limit_requires_an_additional_valid_match_to_truncate() {
        for count in [10_000, 10_001] {
            let mut matcher = Matcher::new("ss", false);
            for row in 0..count {
                matcher.append('ß', row, 0, 1);
                assert_eq!(matcher.finish_row(false), row < 10_000);
            }
            let (matches, truncated) = matcher.results();
            assert_eq!(matches.len(), 10_000);
            assert_eq!(truncated, count > 10_000);
        }
    }

    #[test]
    fn ascii_batch_preserves_maps_after_wrapped_unicode_and_case_folding() {
        for case_sensitive in [false, true] {
            for query in ["ssAB", "éAB", "AB", "ab", "BC日", "日ss"] {
                let mut batched = Matcher::new(query, case_sensitive);
                let mut scalar = Matcher::new(query, case_sensitive);
                for (row, text) in ["ßé", "ABC", "日ß", "", "AB"].iter().enumerate() {
                    if text.is_ascii() {
                        batched.append_ascii_row(text, row as u64);
                    } else {
                        for (column, character) in text.chars().enumerate() {
                            batched.append(character, row as u64, column as u16, column as u16 + 1);
                        }
                    }
                    for (column, character) in text.chars().enumerate() {
                        scalar.append(character, row as u64, column as u16, column as u16 + 1);
                    }
                    assert_eq!(batched.text, scalar.text);
                    assert_eq!(batched.starts, scalar.starts);
                    assert_eq!(batched.ends, scalar.ends);
                    assert!(batched.finish_row(true));
                    assert!(scalar.finish_row(true));
                }
                assert_eq!(batched.results(), scalar.results());
            }
        }
    }

    /// Opt-in, in-process owner-thread workload. Run in release mode; the
    /// formatter and cell lookups are included in the live measurements.
    #[test]
    #[ignore]
    fn benchmark_search() {
        use std::time::Instant;
        let workloads = [
            (
                "ascii-miss",
                "Build STEP completed successfully. ".repeat(4),
                "missing needle",
                false,
                false,
            ),
            (
                "ascii-hits",
                "Build STEP completed successfully. ".repeat(4),
                "step",
                false,
                false,
            ),
            (
                "wrapped-prefix",
                "a".repeat(128),
                &("a".repeat(512) + "b"),
                true,
                true,
            ),
            (
                "unicode",
                "日 Straße É 👩‍💻 completed. ".repeat(4),
                "STRASSE",
                false,
                false,
            ),
            (
                "unicode-expansion-miss",
                "ß".repeat(128),
                &"s".repeat(511),
                false,
                true,
            ),
            (
                "unicode-miss",
                "日 Straße É 👩‍💻 completed. ".repeat(4),
                "missing needle",
                false,
                false,
            ),
        ];
        for (name, text, query, case_sensitive, wrapped) in workloads {
            let mut samples = Vec::new();
            for _ in 0..5 {
                let started = Instant::now();
                let mut matcher = Matcher::new(query, case_sensitive);
                for row in 0..40_000 {
                    if text.is_ascii() {
                        matcher.append_ascii_row(&text, row);
                    } else {
                        for (column, character) in text.chars().enumerate() {
                            matcher.append(character, row, column as u16, column as u16 + 1);
                        }
                    }
                    if !matcher.finish_row(wrapped) {
                        break;
                    }
                }
                let (matches, truncated) = matcher.results();
                let count = matches.len();
                std::hint::black_box((matches, truncated));
                samples.push(started.elapsed().as_micros());
                assert_eq!(
                    (count, truncated),
                    if name == "ascii-hits" || name == "unicode" {
                        (10_000, true)
                    } else {
                        (0, false)
                    }
                );
            }
            samples.sort_unstable();
            println!(
                "matcher {name}: median_us={} samples_us={samples:?}",
                samples[2]
            );
        }
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 128,
            rows: 24,
            max_scrollback: 8 * 1024 * 1024,
        })
        .unwrap();
        for _ in 0..30_000 {
            terminal.vt_write(
                b"Build STEP completed successfully. Build STEP completed successfully.\r\n",
            );
        }
        println!("live rows={}", terminal.scrollbar().unwrap().total);
        assert!(terminal.scrollbar().unwrap().total > 5_000);
        for query in ["missing needle", "STEP"] {
            let mut samples = Vec::new();
            for _ in 0..5 {
                let started = Instant::now();
                let (matches, truncated) =
                    search(&terminal, query, false, || false).unwrap().unwrap();
                let count = matches.len();
                std::hint::black_box((matches, truncated));
                samples.push(started.elapsed().as_micros());
                assert_eq!(
                    (count, truncated),
                    if query == "STEP" {
                        (10_000, true)
                    } else {
                        (0, false)
                    }
                );
            }
            samples.sort_unstable();
            println!(
                "live {query:?}: median_us={} samples_us={samples:?}",
                samples[2]
            );
        }
    }
    #[test]
    fn searches_history_wrapped_text_and_unicode_grid_columns() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 2,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write("old hit\r\nhttps://example.org\r\n日hit\r\nlast".as_bytes());
        let (matches, truncated) = search(&terminal, "hit", false, || false).unwrap().unwrap();
        assert_eq!(matches.len(), 2);
        assert!(!truncated);
        assert_eq!(matches[1].start_column, 2);
        let (wrapped, _) = search(&terminal, "https://example.org", true, || false)
            .unwrap()
            .unwrap();
        assert_eq!(wrapped.len(), 1);
        assert!(wrapped[0].end_row > wrapped[0].start_row);
        assert!(
            search(&terminal, "HIT", true, || false)
                .unwrap()
                .unwrap()
                .0
                .is_empty()
        );
        assert!(search(&terminal, "hit", false, || true).unwrap().is_none());
        let total = terminal.scrollbar().unwrap().total;
        let copied = selected_text(
            &terminal,
            SearchMatch {
                history: Default::default(),
                start_row: 0,
                start_column: 0,
                end_row: total - 1,
                end_column: 8,
            },
        )
        .unwrap();
        assert!(copied.contains("old hit"), "{copied:?}");
        assert!(copied.contains("https://example.org"), "{copied:?}");
    }
    #[test]
    fn unicode_case_folding_uses_actual_grapheme_cell_columns() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 2,
            max_scrollback: 10,
        })
        .unwrap();
        terminal.vt_write("\u{1b}[?2027h👩‍💻hit É Straße".as_bytes());
        let find = |query, case| search(&terminal, query, case, || false).unwrap().unwrap().0;
        assert_eq!(find("hit", true)[0].start_column, 2);
        assert_eq!(find("é", false)[0].start_column, 6);
        assert!(find("é", true).is_empty());
        assert_eq!(find("STRASSE", false)[0].start_column, 8);
        // A partial case-fold expansion must not match half of one source cell.
        assert_eq!(find("s", false).len(), 1);
        assert_eq!(
            find("ss", false)[0].end_column - find("ss", false)[0].start_column,
            1
        );
    }

    #[test]
    fn searches_across_formatter_batches_and_copy_preserves_wrap_spaces() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 2,
            max_scrollback: 400,
        })
        .unwrap();
        terminal.vt_write(format!("{}needle", "x".repeat(8 * 300)).as_bytes());
        let (matches, _) = search(&terminal, "needle", true, || false)
            .unwrap()
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start_row, 300);
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 2,
            max_scrollback: 10,
        })
        .unwrap();
        terminal.vt_write(b"abc   deF");
        assert_eq!(
            selected_text(
                &terminal,
                SearchMatch {
                    history: Default::default(),
                    start_row: 0,
                    start_column: 0,
                    end_row: 1,
                    end_column: 1
                }
            )
            .unwrap(),
            "abc   deF"
        );
    }
}
