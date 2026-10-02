//! Owned completed-terminal history. No emulator, PTY, encoder or worker survives.
use std::sync::Arc;

use libghostty_vt::{
    Terminal,
    screen::CellWide,
    terminal::{Point, PointCoordinate, ScrollViewport},
};

use crate::terminal::snapshot::{HyperlinkSpan, VisualResources};
use crate::terminal::{CellSnapshot, CellStyle, RenderSnapshot, Rgb, RowSnapshot, SearchMatch};

#[derive(Clone, Copy)]
struct Cell {
    byte: u32,
    column: u16,
    width: u16,
}

#[derive(Clone)]
struct Style {
    start: u16,
    end: u16,
    foreground: Rgb,
    background: Rgb,
    style: CellStyle,
}

#[derive(Clone)]
struct Row {
    text: String,
    cells: Vec<Cell>,
    styles: Vec<Style>,
    links: Vec<HyperlinkSpan>,
    wrapped: bool,
    logical_columns: u16,
}

impl Row {
    fn capture(
        terminal: &Terminal<'static, 'static>,
        row: &RowSnapshot,
        y: usize,
        columns: u16,
    ) -> Result<Self, String> {
        let point = |x: u16| terminal.grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }));
        let mut result = Self {
            text: String::new(),
            cells: Vec::new(),
            styles: Vec::new(),
            links: row.hyperlinks.clone(),
            wrapped: row.wrapped,
            logical_columns: columns,
        };
        for (index, cell) in row.cells.iter().enumerate().take(usize::from(columns)) {
            let column = index as u16;
            if let Some(style) = result.styles.last_mut().filter(|style| {
                style.foreground == cell.foreground
                    && style.background == cell.background
                    && style.style == cell.style
            }) {
                style.end = column + 1;
            } else {
                result.styles.push(Style {
                    start: column,
                    end: column + 1,
                    foreground: cell.foreground,
                    background: cell.background,
                    style: cell.style,
                });
            }
            if cell.text.is_empty() {
                continue;
            }
            let width = if cell.text.is_narrow_ascii() {
                1
            } else {
                match point(column)
                    .and_then(|point| point.cell())
                    .and_then(|cell| cell.wide())
                    .map_err(|e| e.to_string())?
                {
                    CellWide::Wide => 2,
                    CellWide::Narrow => 1,
                    CellWide::SpacerHead | CellWide::SpacerTail => continue,
                }
            };
            result.cells.push(Cell {
                byte: result.text.len() as u32,
                column,
                width,
            });
            result.text.push_str(cell.text.as_str());
        }
        if columns > 0
            && row.wrapped
            && point(columns - 1)
                .and_then(|point| point.cell())
                .and_then(|cell| cell.wide())
                .map_err(|e| e.to_string())?
                == CellWide::SpacerHead
        {
            result.logical_columns -= 1;
        }
        Ok(result)
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.text.capacity()
            + self.cells.capacity() * std::mem::size_of::<Cell>()
            + self.styles.capacity() * std::mem::size_of::<Style>()
            + self.links.capacity() * std::mem::size_of::<HyperlinkSpan>()
            + self
                .links
                .iter()
                .map(|link| link.uri.capacity())
                .sum::<usize>()
    }

    fn cell_text(&self, index: usize) -> &str {
        let start = self.cells[index].byte as usize;
        let end = self
            .cells
            .get(index + 1)
            .map_or(self.text.len(), |cell| cell.byte as usize);
        &self.text[start..end]
    }

    fn view(&self, columns: u16) -> Arc<RowSnapshot> {
        let mut cells = vec![CellSnapshot::default(); usize::from(columns)];
        for style in &self.styles {
            for cell in &mut cells[usize::from(style.start)..usize::from(style.end)] {
                cell.foreground = style.foreground;
                cell.background = style.background;
                cell.style = style.style;
            }
        }
        for (index, cell) in self.cells.iter().enumerate() {
            cells[usize::from(cell.column)].text = self.cell_text(index).into();
        }
        Arc::new(RowSnapshot {
            cells,
            wrapped: self.wrapped,
            hyperlinks: self.links.clone(),
        })
    }

    fn append_search(&self, matcher: &mut crate::terminal::search::Matcher, row: u64) {
        let mut column = 0;
        for (index, cell) in self.cells.iter().enumerate() {
            while column < cell.column {
                matcher.append(' ', row, column, column + 1);
                column += 1;
            }
            for character in self.cell_text(index).chars() {
                matcher.append(character, row, cell.column, cell.column + cell.width);
            }
            column = cell.column + cell.width;
        }
        while column < self.logical_columns {
            matcher.append(' ', row, column, column + 1);
            column += 1;
        }
    }

    fn copy(&self, start: u16, end: u16, output: &mut String) {
        let mut column = start;
        for (index, cell) in self
            .cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.column < end && cell.column + cell.width > start)
        {
            while column < cell.column {
                output.push(' ');
                column += 1;
            }
            output.push_str(self.cell_text(index));
            column = cell.column + cell.width;
        }
        while column < end.min(self.logical_columns) {
            output.push(' ');
            column += 1;
        }
    }
}

pub(super) struct History {
    rows: Arc<[Row]>,
    base: RenderSnapshot,
    pub(super) omitted: u64,
}

pub(super) struct Source {
    rows: Arc<[Row]>,
    columns: u16,
}

impl History {
    /// Captures the current active screen before carry switches to primary.
    /// The budget accounts for owned capacities, including indices and styles.
    pub(super) fn capture(
        terminal: &mut Terminal<'static, 'static>,
        _base: &RenderSnapshot,
        budget: usize,
    ) -> Result<Self, String> {
        let scroll = terminal.scrollbar().map_err(|e| e.to_string())?;
        let height = scroll.len.max(1);
        let mut visual = VisualResources::new().map_err(|e| e.to_string())?;
        let mut rows = Vec::new();
        let mut metadata = None;
        let mut used = std::mem::size_of::<Self>() + 4 * std::mem::size_of::<usize>();
        let mut end = scroll.total;
        let result = (|| {
            'pages: while end > 0 {
                let start = end.saturating_sub(height);
                terminal.scroll_viewport(ScrollViewport::Row(start as usize));
                let view = visual
                    .capture(terminal, 0, true, None)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "The completed terminal view is unavailable".to_owned())?;
                metadata.get_or_insert_with(|| (*view).clone());
                for index in (0..view.lines.len()).rev() {
                    let absolute = view.scroll.offset + index as u64;
                    if absolute >= end || absolute < start {
                        continue;
                    }
                    let row = Row::capture(terminal, &view.lines[index], index, view.columns)?;
                    let bytes = row.bytes();
                    if used.saturating_add(bytes) > budget {
                        break 'pages;
                    }
                    used += bytes;
                    rows.push(row);
                }
                if start == 0 {
                    break;
                }
                end = start;
            }
            if rows.is_empty() {
                return Err(
                    "The completed terminal grid exceeds the retained-history limit (8 MiB)".into(),
                );
            }
            rows.reverse();
            let omitted = scroll.total.saturating_sub(rows.len() as u64);
            let mut base = metadata
                .ok_or_else(|| "The completed terminal metadata is unavailable".to_owned())?;
            base.lines = Arc::from([]);
            base.cursor = None;
            base.activity = None;
            base.scroll.total = rows.len() as u64;
            base.scroll.len = u64::from(base.rows).min(base.scroll.total);
            base.scroll.offset = scroll
                .offset
                .saturating_sub(omitted)
                .min(base.scroll.max_offset());
            base.scroll.mouse_tracking = false;
            base.scroll.bracketed_paste = false;
            Ok(Self {
                rows: rows.into(),
                base,
                omitted,
            })
        })();
        terminal.scroll_viewport(ScrollViewport::Row(scroll.offset as usize));
        result
    }

    pub(super) fn snapshot(&self, generation: u64) -> Arc<RenderSnapshot> {
        let mut snapshot = self.base.clone();
        snapshot.generation = generation;
        snapshot.detached = false;
        let first = self.base.scroll.offset as usize;
        let end = (first + usize::from(self.base.rows)).min(self.rows.len());
        snapshot.lines = self.rows[first..end]
            .iter()
            .map(|row| row.view(self.base.columns))
            .collect::<Vec<_>>()
            .into();
        Arc::new(snapshot)
    }

    pub(super) fn scroll(&mut self, command: &super::ScrollCommand) {
        let offset = self.base.scroll.offset;
        self.base.scroll.offset = match command {
            super::ScrollCommand::Top => 0,
            super::ScrollCommand::Bottom => self.base.scroll.max_offset(),
            super::ScrollCommand::Row(row) => *row as u64,
            super::ScrollCommand::Delta(delta) if *delta < 0 => {
                offset.saturating_sub(delta.unsigned_abs() as u64)
            }
            super::ScrollCommand::Delta(delta) => offset.saturating_add(*delta as u64),
        }
        .min(self.base.scroll.max_offset());
    }

    pub(super) fn resize(&mut self, size: crate::terminal::TerminalSize) {
        self.base.rows = size.rows.max(1);
        self.base.scroll.len = u64::from(self.base.rows).min(self.base.scroll.total);
        self.base.scroll.offset = self.base.scroll.offset.min(self.base.scroll.max_offset());
    }

    pub(super) fn clear(&mut self) {
        let first = self.rows.len().saturating_sub(usize::from(self.base.rows));
        self.rows = self.rows[first..].to_vec().into();
        self.base.scroll.total = self.rows.len() as u64;
        self.base.scroll.len = self.base.scroll.total;
        self.base.scroll.offset = 0;
    }

    pub(super) fn set_theme(&mut self, theme: crate::terminal::theme::TerminalTheme) {
        let palette = theme.palette();
        let color = |value: crate::terminal::theme::ThemeColor| {
            let (red, green, blue) = value.rgb();
            Rgb { red, green, blue }
        };
        let foreground = color(palette.foreground);
        let background = color(palette.background);
        for row in Arc::make_mut(&mut self.rows) {
            for style in &mut row.styles {
                if style.foreground == self.base.foreground {
                    style.foreground = foreground;
                }
                if style.background == self.base.background {
                    style.background = background;
                }
            }
        }
        self.base.foreground = foreground;
        self.base.background = background;
    }

    pub(super) fn source(&self) -> Source {
        Source {
            rows: self.rows.clone(),
            columns: self.base.columns,
        }
    }
}

impl Source {
    pub(super) fn search(
        &self,
        query: &str,
        case_sensitive: bool,
        cancelled: impl Fn() -> bool,
    ) -> Option<(Vec<SearchMatch>, bool)> {
        if query.is_empty() {
            return Some((Vec::new(), false));
        }
        let mut matcher = crate::terminal::search::Matcher::new(query, case_sensitive);
        for (index, row) in self.rows.iter().enumerate() {
            if index % 128 == 0 && cancelled() {
                return None;
            }
            row.append_search(&mut matcher, index as u64);
            if !matcher.finish_row(row.wrapped && index + 1 < self.rows.len()) {
                break;
            }
        }
        Some(matcher.results())
    }

    pub(super) fn copy(&self, range: SearchMatch) -> Result<String, String> {
        let unavailable = || "The selected terminal output is no longer available".to_owned();
        if range.start_row > range.end_row
            || range.end_row >= self.rows.len() as u64
            || range.start_column >= self.columns
            || range.end_column == 0
            || range.end_column > self.columns
            || (range.start_row == range.end_row && range.start_column >= range.end_column)
        {
            return Err(unavailable());
        }
        let mut text = String::new();
        let mut line = String::new();
        for index in range.start_row..=range.end_row {
            let row = &self.rows[index as usize];
            let start = if index == range.start_row {
                range.start_column
            } else {
                0
            };
            let end = if index == range.end_row {
                range.end_column
            } else {
                self.columns
            };
            line.clear();
            row.copy(start, end, &mut line);
            let line = if row.wrapped && index < range.end_row {
                line.as_str()
            } else {
                line.trim_end_matches(' ')
            };
            let newline = index < range.end_row && !row.wrapped;
            if text
                .len()
                .saturating_add(line.len())
                .saturating_add(usize::from(newline))
                > crate::terminal::ipc::MAX_FRAME_BYTES - 4096
            {
                return Err("The selection exceeds the clipboard transfer limit (8 MiB). Select a smaller range.".into());
            }
            text.push_str(line);
            if newline {
                text.push('\n');
            }
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::TerminalOptions;

    #[test]
    #[ignore]
    fn benchmark_archived_search() {
        use std::time::Instant;
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
        let history = History::capture(
            &mut terminal,
            &RenderSnapshot::empty(),
            super::super::holder::MAX_SCREEN,
        )
        .unwrap();
        let source = history.source();
        println!("archived rows={}", source.rows.len());
        assert!(source.rows.len() > 5_000);
        for query in ["missing needle", "STEP"] {
            let mut samples = Vec::new();
            for _ in 0..5 {
                let started = Instant::now();
                let (matches, truncated) = source.search(query, false, || false).unwrap();
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
                "archived {query:?}: median_us={} samples_us={samples:?}",
                samples[2]
            );
        }
    }

    fn terminal(text: &str) -> Terminal<'static, 'static> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 12,
            rows: 3,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(text.as_bytes());
        terminal
    }

    #[test]
    fn completed_archive_matches_live_copy_search_styles_and_links() {
        let mut terminal = terminal(
            "old É\r\n\x1b[?2027h👩‍💻hit https://example.test\r\n\x1b[31mred\x1b[0m \x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\\r\nlast",
        );
        terminal.scroll_viewport(ScrollViewport::Top);
        let history = History::capture(
            &mut terminal,
            &RenderSnapshot::empty(),
            super::super::holder::MAX_SCREEN,
        )
        .unwrap();
        assert_eq!(history.base.scroll.offset, 0);
        assert_eq!(terminal.scrollbar().unwrap().offset, 0);
        let source = history.source();
        for query in ["é", "hit", "https://example.test", "👩‍💻", "last", ""] {
            assert_eq!(
                source.search(query, false, || false).unwrap(),
                crate::terminal::search::search(&terminal, query, false, || false)
                    .unwrap()
                    .unwrap(),
                "{query}"
            );
        }
        let total = terminal.scrollbar().unwrap().total;
        let range = SearchMatch {
            start_row: 0,
            start_column: 0,
            end_row: total - 1,
            end_column: 12,
        };
        assert_eq!(
            source.copy(range).unwrap(),
            crate::terminal::search::selected_text(&terminal, range).unwrap()
        );
        for row in 0..total {
            let range = SearchMatch {
                start_row: row,
                start_column: 0,
                end_row: row,
                end_column: 12,
            };
            assert_eq!(
                source.copy(range).unwrap(),
                crate::terminal::search::selected_text(&terminal, range).unwrap(),
                "row {row}"
            );
        }
        assert!(history.rows.iter().any(|row| !row.links.is_empty()));
        assert!(history.rows.iter().any(|row| row.styles.len() > 1));
        let hit = source.search("hit", true, || false).unwrap().0[0];
        assert_eq!(hit.start_column, 2);
    }

    #[test]
    fn completed_archive_budget_retains_newest_rows_and_rebases_old_viewport() {
        let mut terminal = terminal(
            &(0..70)
                .map(|i| format!("row{i:03}\r\n"))
                .collect::<String>(),
        );
        terminal.scroll_viewport(ScrollViewport::Row(2));
        let history = History::capture(&mut terminal, &RenderSnapshot::empty(), 2000).unwrap();
        assert!(history.omitted > 0);
        assert_eq!(history.base.scroll.offset, 0);
        assert_eq!(terminal.scrollbar().unwrap().offset, 2);
        assert!(
            history.rows.iter().map(Row::bytes).sum::<usize>() + std::mem::size_of::<History>()
                <= 2000
        );
        assert_eq!(
            history
                .source()
                .search("row069", true, || false)
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(
            history
                .source()
                .search("row000", true, || false)
                .unwrap()
                .0
                .is_empty()
        );
    }
}
