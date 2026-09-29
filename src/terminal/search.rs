//! Bounded scrollback search on the terminal-owner thread.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub start_row: u64,
    pub start_column: u16,
    pub end_row: u64,
    pub end_column: u16,
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
    use unicode_casefold::UnicodeCaseFold;
    const ROWS_PER_BATCH: u64 = 256;
    const MAX_MATCHES: usize = 10_000;
    if query.is_empty() {
        return Ok(Some((Vec::new(), false)));
    }
    let query = if case_sensitive {
        query.to_owned()
    } else {
        query.case_fold().collect::<String>()
    };
    let total = terminal.scrollbar()?.total;
    let columns = terminal.cols()?;
    if columns == 0 || total == 0 {
        return Ok(Some((Vec::new(), false)));
    }
    let mut matches = Vec::new();
    let mut logical = String::new();
    let mut positions = Vec::new();
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
            let mut append = |character: char, column| {
                let start = logical.len();
                if case_sensitive {
                    logical.push(character);
                } else {
                    logical.extend(std::iter::once(character).case_fold());
                }
                positions.resize(logical.len(), None);
                positions[start] = Some((row, column));
            };
            if text.is_ascii() {
                for character in text.chars() {
                    append(character, column);
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
                        append(' ', x);
                    } else {
                        for character in &graphemes[..length] {
                            append(*character, x);
                        }
                    }
                    column = x + width;
                }
            }
            let wrapped = reference(row, 0)?.row()?.is_wrapped()? && row + 1 < total;
            positions.push(Some((row, column)));
            let mut offset = 0;
            while offset + query.len() <= logical.len() {
                let end = offset + query.len();
                if let (Some((start_row, start_column)), Some((end_row, end_column))) =
                    (positions[offset], positions[end])
                    && &logical[offset..end] == query
                {
                    if matches.len() == MAX_MATCHES {
                        return Ok(Some((matches, true)));
                    }
                    matches.push(SearchMatch {
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
                // Only the untested suffix can participate in a match that
                // crosses the next physical row. An unbroken million-row
                // wrapped line must not retain a million-row position table.
                while !logical.is_char_boundary(offset) {
                    offset -= 1;
                }
                logical.drain(..offset);
                positions.drain(..offset);
                positions.pop();
            } else {
                logical.clear();
                positions.clear();
            }
        }
    }
    Ok(Some((matches, false)))
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
