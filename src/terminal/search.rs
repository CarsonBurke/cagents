//! Bounded scrollback search on the terminal-owner thread.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub start_row: u64,
    pub start_column: u16,
    pub end_row: u64,
    pub end_column: u16,
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
        if self.case_sensitive {
            self.text.push(character);
        } else {
            self.text.extend(std::iter::once(character).case_fold());
        }
        self.starts.resize(self.text.len(), None);
        self.ends.resize(self.text.len(), None);
        self.starts[start] = Some((row, column));
        self.ends[self.text.len() - 1] = Some((row, end_column));
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
            let end = offset + self.query.len();
            if let (Some((start_row, start_column)), Some((end_row, end_column))) =
                (self.starts[offset], self.ends[end - 1])
                && self.text[offset..end] == self.query
            {
                if self.matches.len() == 10_000 {
                    self.truncated = true;
                    return false;
                }
                self.matches.push(SearchMatch {
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
            let mut column = 0;
            if text.is_ascii() {
                for character in text.chars() {
                    matcher.append(character, row, column, column + 1);
                    column += 1;
                }
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
