//! Hit testing reads only the hovered token, never the terminal history or disk.
use super::{RenderSnapshot, RowSnapshot};
use std::{path::Path, sync::Arc};

const MAX_TOKEN_CELLS: usize = 4096;

#[derive(Clone)]
pub(super) struct Link {
    pub url: url::Url,
    pub spans: Vec<(usize, u16, u16)>,
}

/// Cache a hit (including misses) across drawing, pointer feedback and clicks.
/// A weak snapshot reference avoids retaining retired terminal history.
#[derive(Default)]
pub(super) struct Cache(Option<Cached>);

struct Cached {
    snapshot: std::sync::Weak<RenderSnapshot>,
    position: Position,
    cwd: std::path::PathBuf,
    link: Option<Link>,
}

impl Cache {
    pub fn at_cursor(
        &mut self,
        snapshot: &Arc<RenderSnapshot>,
        geometry: super::TerminalGeometry,
        cursor: super::mouse::Cursor,
        cwd: &Path,
    ) -> Option<Link> {
        let position = geometry.cursor_position(cursor)?;
        let (column, row) = geometry.cell_within(position, snapshot.columns, snapshot.rows);
        self.at(snapshot, usize::from(row), column, cwd)
    }

    pub fn at(
        &mut self,
        snapshot: &Arc<RenderSnapshot>,
        row: usize,
        column: u16,
        cwd: &Path,
    ) -> Option<Link> {
        let position = (row, usize::from(column));
        if let Some(cached) = &self.0
            && cached.position == position
            && cached.cwd == cwd
            && cached.snapshot.ptr_eq(&Arc::downgrade(snapshot))
        {
            return cached.link.clone();
        }
        let link = at(snapshot, row, column, cwd);
        self.0 = Some(Cached {
            snapshot: Arc::downgrade(snapshot),
            position,
            cwd: cwd.to_path_buf(),
            link: link.clone(),
        });
        link
    }
}

type Position = (usize, usize);

fn previous(rows: &[Arc<RowSnapshot>], (row, column): Position) -> Option<Position> {
    if column > 0 {
        Some((row, column - 1))
    } else if row > 0 && rows[row - 1].wrapped {
        rows[row - 1]
            .cells
            .len()
            .checked_sub(1)
            .map(|column| (row - 1, column))
    } else {
        None
    }
}

fn next(rows: &[Arc<RowSnapshot>], (row, column): Position) -> Option<Position> {
    if column + 1 < rows[row].cells.len() {
        Some((row, column + 1))
    } else if rows[row].wrapped && rows.get(row + 1).is_some_and(|row| !row.cells.is_empty()) {
        Some((row + 1, 0))
    } else {
        None
    }
}

fn boundary(row: &RowSnapshot, column: usize) -> bool {
    let cell = &row.cells[column];
    cell.style.invisible
        || (cell.text.is_empty()
            && !(column > 0
                && unicode_width::UnicodeWidthStr::width(row.cells[column - 1].text.as_str()) > 1))
        || cell
            .text
            .as_str()
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`'))
}

fn safe_url(text: &str) -> Option<url::Url> {
    let mut url = url::Url::parse(text).ok()?;
    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => Some(url),
        "file" => {
            if let Some(host) = url.host_str() {
                if !host.eq_ignore_ascii_case("localhost")
                    && !crate::terminal::local_host_name()
                        .is_some_and(|local| local.eq_ignore_ascii_case(host))
                {
                    return None;
                }
                url.set_host(None).ok()?;
            }
            url.to_file_path().ok()?;
            Some(url)
        }
        _ => None,
    }
}

fn parse(text: &str, cwd: &Path) -> Option<url::Url> {
    if let Some(url) = safe_url(text) {
        return Some(url);
    }
    if text.starts_with("www.") {
        return safe_url(&format!("https://{text}"));
    }
    // Unsupported URL schemes must never become file paths.
    if text.contains("://") {
        return None;
    }
    let mut path = text;
    let mut position = None;
    if let Some((prefix, suffix)) = path.rsplit_once(':')
        && let Ok(last) = suffix.parse::<usize>()
    {
        if last == 0 {
            return None;
        }
        let (prefix, line, column) = match prefix.rsplit_once(':') {
            Some((prefix, line)) if line.parse::<usize>().is_ok() => {
                (prefix, line.parse::<usize>().ok()?, last)
            }
            _ => (prefix, last, 1),
        };
        if line == 0 {
            return None;
        }
        path = prefix;
        position = Some(format!("L{line}:{column}"));
    }
    // Slash paths and filenames with an extension are useful without a line
    // suffix. Ordinary prose and arbitrary URI schemes are not file links.
    if path.is_empty()
        || path.contains(':')
        || path.contains(['\0', '\n', '\r'])
        || !(position.is_some()
            || path.contains('/')
            || (path.starts_with('.')
                && path.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic))
            || Path::new(path)
                .extension()
                .is_some_and(|ext| !ext.is_empty()))
    {
        return None;
    }
    let path = if let Some(relative) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)?
            .join(relative)
    } else {
        cwd.join(path)
    };
    let mut url = url::Url::from_file_path(path).ok()?;
    url.set_fragment(position.as_deref());
    Some(url)
}

fn from_positions(
    rows: &[Arc<RowSnapshot>],
    positions: &[Position],
    hovered: Position,
    cwd: &Path,
    quoted: bool,
) -> Option<Link> {
    let mut text = String::new();
    let mut offsets = Vec::with_capacity(positions.len());
    for &(row, column) in positions {
        let start = text.len();
        text.push_str(rows[row].cells[column].text.as_str());
        offsets.push(if start == text.len() {
            offsets.last().copied().unwrap_or((start, start))
        } else {
            (start, text.len())
        });
        if text.len() > MAX_TOKEN_CELLS * 4 {
            return None;
        }
    }
    if quoted && text.trim() != text {
        return None;
    }
    let (start, mut trimmed) = if quoted {
        (0, text.as_str())
    } else {
        let leading = text.trim_start_matches(['(', '[', '{']);
        (
            text.len() - leading.len(),
            leading.trim_end_matches(['.', ',', ';', '!']),
        )
    };
    if !quoted {
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            while trimmed.ends_with(close)
                && trimmed.matches(close).count() > trimmed.matches(open).count()
            {
                trimmed = &trimmed[..trimmed.len() - 1];
            }
        }
    }
    let end = start + trimmed.len();
    let hit = positions.iter().position(|position| *position == hovered)?;
    // Wide character continuation cells have no text, but belong to the glyph.
    let (hit_start, hit_end) = offsets[hit];
    if hit_end <= start || hit_start >= end {
        return None;
    }
    let url = parse(trimmed, cwd)?;
    let mut spans: Vec<(usize, u16, u16)> = Vec::new();
    for (&(row, column), &(cell_start, cell_end)) in positions.iter().zip(&offsets) {
        if cell_end < start || cell_start >= end || (cell_end == start && cell_start != cell_end) {
            continue;
        }
        let column = u16::try_from(column).ok()?;
        let end = column.checked_add(1)?;
        if let Some(last) = spans
            .last_mut()
            .filter(|last| last.0 == row && last.2 == column)
        {
            last.2 = end;
        } else {
            spans.push((row, column, end));
        }
    }
    Some(Link { url, spans })
}

fn quoted_position_suffix(rows: &[Arc<RowSnapshot>], quote: Position) -> Vec<Position> {
    let mut positions = Vec::new();
    let mut text = String::new();
    let mut position = quote;
    while let Some(next) = next(rows, position) {
        let cell = &rows[next.0].cells[next.1];
        let value = cell.text.as_str();
        if cell.style.invisible
            || !value.chars().all(|ch| ch.is_ascii_digit() || ch == ':')
            || value.is_empty()
        {
            break;
        }
        if positions.len() >= 32 {
            return Vec::new();
        }
        positions.push(next);
        text.push_str(value);
        position = next;
    }
    if !text.starts_with(':') {
        return Vec::new();
    }
    let parts: Vec<_> = text[1..].split(':').collect();
    if !(1..=2).contains(&parts.len())
        || !parts
            .iter()
            .all(|part| part.parse::<usize>().is_ok_and(|number| number > 0))
    {
        return Vec::new();
    }
    if let Some(after) = next(rows, position)
        && rows[after.0].cells[after.1]
            .text
            .as_str()
            .chars()
            .any(|ch| ch.is_alphanumeric() || ch == '_' || ch == '/')
    {
        return Vec::new();
    }
    positions
}

pub(super) fn at(snapshot: &RenderSnapshot, row: usize, column: u16, cwd: &Path) -> Option<Link> {
    let rows = &snapshot.lines;
    let line = rows.get(row)?;
    if let Some(span) = line
        .hyperlinks
        .iter()
        .find(|span| span.start <= column && column < span.end)
    {
        return Some(Link {
            url: safe_url(&span.uri)?,
            spans: vec![(row, span.start, span.end)],
        });
    }
    let hovered = (row, usize::from(column));
    line.cells.get(hovered.1)?;
    // A nearest opening quote may be on the preceding soft-wrapped row.
    // Walk a bounded local region; never concatenate the whole logical line.
    let mut position = hovered;
    let mut scanned = 0;
    while let Some(previous) = previous(rows, position) {
        scanned += 1;
        if scanned >= MAX_TOKEN_CELLS {
            break;
        }
        position = previous;
        let delimiter = rows[position.0].cells[position.1].text.as_str();
        if matches!(delimiter, "\"" | "'" | "`") {
            // A quote immediately followed by :line[:column] closes a path.
            // Continue backward to its opening quote when hovering the suffix.
            if next(rows, position)
                .is_some_and(|next| rows[next.0].cells[next.1].text.as_str() == ":")
            {
                continue;
            }
            let mut positions = Vec::new();
            let mut next_position = position;
            while let Some(next) = next(rows, next_position) {
                scanned += 1;
                if scanned > MAX_TOKEN_CELLS {
                    break;
                }
                next_position = next;
                let cell = &rows[next.0].cells[next.1];
                if cell.text.as_str() == delimiter {
                    positions.extend(quoted_position_suffix(rows, next));
                    if positions.contains(&hovered)
                        && let Some(link) = from_positions(rows, &positions, hovered, cwd, true)
                    {
                        return Some(link);
                    }
                    break;
                }
                if cell.style.invisible {
                    break;
                }
                positions.push(next);
            }
        }
    }
    if boundary(line, hovered.1) {
        return None;
    }
    let mut first = hovered;
    let mut length = 1;
    while let Some(position) = previous(rows, first) {
        if boundary(&rows[position.0], position.1) {
            break;
        }
        length += 1;
        if length > MAX_TOKEN_CELLS {
            return None;
        }
        first = position;
    }
    let mut positions = Vec::new();
    let mut position = first;
    loop {
        positions.push(position);
        if positions.len() > MAX_TOKEN_CELLS {
            return None;
        }
        match next(rows, position) {
            Some(next) if !boundary(&rows[next.0], next.1) => position = next,
            _ => break,
        }
    }
    from_positions(rows, &positions, hovered, cwd, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_links_accept_only_the_local_machine() {
        for uri in ["file:///p", "file://localhost/p"] {
            assert_eq!(
                safe_url(uri).unwrap().to_file_path().unwrap(),
                Path::new("/p")
            );
        }
        let local = crate::terminal::local_host_name().expect("local hostname");
        assert_eq!(
            safe_url(&format!("file://{local}/p"))
                .unwrap()
                .to_file_path()
                .unwrap(),
            Path::new("/p")
        );
        assert!(safe_url("file://foreign-host.invalid/p").is_none());
    }
}
