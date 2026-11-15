//! Complete ordered matches without retaining a pair of native positions per hit.
//!
//! A run repeats (line delta, column gap/absolute column, width). Four unsigned
//! varints encode its repetition count and those three values. Checkpoints every
//! 64 runs permit random access without expanding the intervening matches. A
//! captured selection is a view over the same immutable allocation.
use super::Position;
use std::sync::Arc;

const CHECKPOINT_RUNS: usize = 64;
#[cfg(test)]
thread_local! { static QUERY_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
type Span = (Position, Position);

#[derive(Debug, Clone, Copy)]
struct Checkpoint {
    ordinal: usize,
    offset: usize,
    previous: Position,
}

#[derive(Debug, Default)]
struct Data {
    bytes: Vec<u8>,
    checkpoints: Vec<Checkpoint>,
}

/// Complete, ordered, line-local UTF-8 match ranges backed by immutable storage.
///
/// Cloning and scoped views share their storage. Spans collected with
/// `FromIterator` must be ordered and non-overlapping within each source line.
#[derive(Clone, Default)]
pub struct MatchIndex {
    data: Arc<Data>,
    first: usize,
    len: usize,
}

impl std::fmt::Debug for MatchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatchIndex")
            .field("count", &self.len)
            .field("encoded_bytes", &self.data.bytes.len())
            .field("checkpoints", &self.data.checkpoints.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    lines: usize,
    column: usize,
    width: usize,
    count: usize,
}

#[derive(Debug)]
pub(crate) struct MatchBuilder {
    data: Data,
    pending: Option<(Run, Position, usize)>,
    previous: Position,
    len: usize,
    runs: usize,
}

impl Default for MatchBuilder {
    fn default() -> Self {
        Self {
            data: Data::default(),
            pending: None,
            previous: Position { line: 0, column: 0 },
            len: 0,
            runs: 0,
        }
    }
}

fn encode(mut value: usize, bytes: &mut Vec<u8>) {
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn decode(bytes: &[u8], offset: &mut usize) -> usize {
    let mut value = 0;
    let mut shift = 0;
    loop {
        // Only the private builder creates these bytes; they are not persisted
        // or parsed from a provider, file, IPC message or plugin.
        let byte = bytes[*offset];
        *offset += 1;
        value |= usize::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return value;
        }
        shift += 7;
    }
}

impl MatchBuilder {
    pub(crate) fn push(&mut self, start: Position, end: Position) {
        assert_eq!(start.line, end.line, "line-local matcher contract");
        assert!(start.column <= end.column);
        assert!((start.line, start.column) >= (self.previous.line, self.previous.column));
        let lines = start.line - self.previous.line;
        let column = if lines == 0 {
            start.column - self.previous.column
        } else {
            start.column
        };
        let candidate = Run {
            lines,
            column,
            width: end.column - start.column,
            count: 1,
        };
        if let Some((run, _, _)) = self.pending.as_mut()
            && (run.lines, run.column, run.width)
                == (candidate.lines, candidate.column, candidate.width)
        {
            run.count += 1;
        } else {
            self.flush();
            self.pending = Some((candidate, self.previous, self.len));
        }
        self.previous = end;
        self.len += 1;
    }

    fn flush(&mut self) {
        let Some((run, previous, ordinal)) = self.pending.take() else {
            return;
        };
        if self.runs % CHECKPOINT_RUNS == 0 {
            self.data.checkpoints.push(Checkpoint {
                ordinal,
                offset: self.data.bytes.len(),
                previous,
            });
        }
        for value in [run.count, run.lines, run.column, run.width] {
            encode(value, &mut self.data.bytes);
        }
        self.runs += 1;
    }

    pub(crate) fn finish(mut self) -> MatchIndex {
        self.flush();
        MatchIndex {
            data: Arc::new(self.data),
            first: 0,
            len: self.len,
        }
    }
}

impl MatchIndex {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn iter_at(&self, absolute: usize) -> Matches<'_> {
        let checkpoint = self
            .data
            .checkpoints
            .partition_point(|point| point.ordinal <= absolute)
            .checked_sub(1)
            .map(|index| self.data.checkpoints[index]);
        let mut iter = Matches {
            data: &self.data,
            offset: 0,
            previous: Position { line: 0, column: 0 },
            run: None,
            remaining: 0,
        };
        if let Some(point) = checkpoint {
            iter.offset = point.offset;
            iter.previous = point.previous;
            iter.remaining = absolute - point.ordinal + 1;
            iter.skip_matches(absolute - point.ordinal);
        }
        iter
    }

    pub fn get(&self, ordinal: usize) -> Option<Span> {
        (ordinal < self.len)
            .then(|| self.iter_at(self.first + ordinal).next())
            .flatten()
    }

    pub fn iter(&self) -> Matches<'_> {
        if self.is_empty() {
            return Matches {
                data: &self.data,
                offset: 0,
                previous: Position { line: 0, column: 0 },
                run: None,
                remaining: 0,
            };
        }
        let mut iter = self.iter_at(self.first);
        iter.remaining = self.len;
        iter
    }

    /// The predicate must be true for a prefix, as with slice::partition_point.
    pub fn partition_point(&self, mut before: impl FnMut(Span) -> bool) -> usize {
        let (mut first, mut last) = (0, self.len);
        while first < last {
            let middle = first + (last - first) / 2;
            if before(self.get(middle).expect("index bounds")) {
                first = middle + 1;
            } else {
                last = middle;
            }
        }
        first
    }

    fn view(&self, first: usize, last: usize) -> Self {
        Self {
            data: Arc::clone(&self.data),
            first: self.first + first,
            len: last.saturating_sub(first),
        }
    }

    pub fn scoped(&self, scope: Option<Span>) -> Self {
        let Some((start, end)) = scope else {
            return self.clone();
        };
        let first =
            self.partition_point(|(at, _)| (at.line, at.column) < (start.line, start.column));
        let last = self.partition_point(|(_, at)| (at.line, at.column) <= (end.line, end.column));
        self.view(first, last.max(first))
    }

    /// Only matches overlapping glyph bytes in this actual visible layout run.
    /// This avoids scanning every match on a long horizontally scrolled line.
    pub fn visible(
        &self,
        line: usize,
        first_byte: usize,
        last_byte: usize,
    ) -> (usize, Matches<'_>) {
        let first = self.partition_point(|(_, end)| (end.line, end.column) <= (line, first_byte));
        let last =
            self.partition_point(|(start, _)| (start.line, start.column) < (line, last_byte));
        let mut iter = self.iter_at(self.first + first);
        iter.remaining = last.saturating_sub(first);
        (first, iter)
    }

    #[cfg(test)]
    pub(crate) fn take_query_runs() -> usize {
        QUERY_RUNS.with(|count| count.replace(0))
    }

    #[cfg(test)]
    pub(crate) fn allocation_id(&self) -> *const () {
        Arc::as_ptr(&self.data).cast()
    }

    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.data.bytes.capacity()
            + self.data.checkpoints.capacity() * std::mem::size_of::<Checkpoint>()
            + std::mem::size_of::<Data>()
            + 2 * std::mem::size_of::<usize>()
    }
}

/// A lazy, exact-size iterator over UTF-8 source match ranges.
pub struct Matches<'a> {
    data: &'a Data,
    offset: usize,
    previous: Position,
    run: Option<Run>,
    remaining: usize,
}

impl Matches<'_> {
    fn load_run(&mut self) {
        if self.run.is_none() {
            #[cfg(test)]
            QUERY_RUNS.with(|count| count.set(count.get() + 1));
            self.run = Some(Run {
                count: decode(&self.data.bytes, &mut self.offset),
                lines: decode(&self.data.bytes, &mut self.offset),
                column: decode(&self.data.bytes, &mut self.offset),
                width: decode(&self.data.bytes, &mut self.offset),
            });
        }
    }

    fn skip_matches(&mut self, mut count: usize) {
        while count > 0 {
            self.load_run();
            let run = self.run.as_mut().unwrap();
            let take = count.min(run.count);
            if run.lines == 0 {
                self.previous.column += take * (run.column + run.width);
            } else {
                self.previous.line += take * run.lines;
                self.previous.column = run.column + run.width;
            }
            run.count -= take;
            count -= take;
            self.remaining -= take;
            if run.count == 0 {
                self.run = None;
            }
        }
    }
}

impl Iterator for Matches<'_> {
    type Item = Span;
    fn next(&mut self) -> Option<Span> {
        if self.remaining == 0 {
            return None;
        }
        self.load_run();
        let run = self.run.as_mut().unwrap();
        let start = Position {
            line: self.previous.line + run.lines,
            column: if run.lines == 0 {
                self.previous.column + run.column
            } else {
                run.column
            },
        };
        let end = Position {
            line: start.line,
            column: start.column + run.width,
        };
        self.previous = end;
        run.count -= 1;
        self.remaining -= 1;
        if run.count == 0 {
            self.run = None;
        }
        Some((start, end))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for Matches<'_> {}

impl<'a> IntoIterator for &'a MatchIndex {
    type Item = Span;
    type IntoIter = Matches<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl FromIterator<Span> for MatchIndex {
    fn from_iter<T: IntoIterator<Item = Span>>(values: T) -> Self {
        let mut builder = MatchBuilder::default();
        for (start, end) in values {
            builder.push(start, end);
        }
        builder.finish()
    }
}

impl PartialEq for MatchIndex {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && ((self.first == other.first && Arc::ptr_eq(&self.data, &other.data))
                || self.iter().eq(other.iter()))
    }
}
impl Eq for MatchIndex {}
impl PartialEq<Vec<Span>> for MatchIndex {
    fn eq(&self, other: &Vec<Span>) -> bool {
        self.len == other.len() && self.iter().eq(other.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(line: usize, column: usize) -> Position {
        Position { line, column }
    }
    fn span(line: usize, column: usize, width: usize) -> Span {
        (at(line, column), at(line, column + width))
    }

    #[test]
    fn dense_arithmetic_runs_keep_complete_counts_and_indexed_boundaries() {
        let count = 8 * 1024 * 1024;
        let index: MatchIndex = (0..count).map(|column| span(0, column, 1)).collect();
        assert_eq!(index.len(), count);
        assert!(
            index.retained_bytes() < 1024,
            "a dense file needs one run, not a position per hit"
        );
        for ordinal in [0, 1, 63, 64, 127, count / 2, count - 1] {
            assert_eq!(index.get(ordinal), Some(span(0, ordinal, 1)));
        }
        assert_eq!(index.get(count), None);
        let (first, visible) = index.visible(0, count / 2, count / 2 + 80);
        assert_eq!(first, count / 2);
        assert_eq!(
            visible.collect::<Vec<_>>(),
            (count / 2..count / 2 + 80)
                .map(|column| span(0, column, 1))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn changing_widths_gaps_lines_and_varints_round_trip_at_checkpoints() {
        let mut expected = Vec::new();
        for line in 0..137 {
            let mut column = 0;
            for item in 0..131 {
                column += (item * 11 + line * 3) % 5;
                let width = (item + line) % 7;
                expected.push(span(line * 131, column, width));
                column += width;
            }
        }
        let index: MatchIndex = expected.iter().copied().collect();
        assert_eq!(index, expected);
        for (ordinal, wanted) in expected.iter().enumerate() {
            assert_eq!(index.get(ordinal), Some(*wanted));
        }
        assert_eq!(index.iter().collect::<Vec<_>>(), expected);
        assert_eq!(index.get(expected.len()), None);
    }

    #[test]
    fn selection_is_an_exact_shared_view_and_visible_queries_preserve_occurrence_indices() {
        let original: MatchIndex = [
            span(0, 0, 2),
            span(0, 2, 0),
            span(0, 3, 2),
            span(1, 0, 2),
            span(1, 4, 1),
        ]
        .into_iter()
        .collect();
        let scoped = original.scoped(Some((at(0, 2), at(1, 2))));
        assert!(Arc::ptr_eq(&original.data, &scoped.data));
        assert_eq!(
            scoped.iter().collect::<Vec<_>>(),
            vec![span(0, 2, 0), span(0, 3, 2), span(1, 0, 2)]
        );
        assert_eq!(scoped.get(0), Some(span(0, 2, 0)));
        assert_eq!(
            scoped.partition_point(|(start, _)| (start.line, start.column) < (1, 0)),
            2
        );
        let (ordinal, hits) = scoped.visible(0, 3, 4);
        assert_eq!(ordinal, 1);
        assert_eq!(hits.collect::<Vec<_>>(), vec![span(0, 3, 2)]);
        assert!(original.scoped(Some((at(0, 1), at(0, 1)))).is_empty());
        assert!(original.scoped(Some((at(2, 0), at(3, 0)))).is_empty());
        assert!(original.visible(20, 0, 80).1.next().is_none());
        assert!(MatchIndex::default().visible(0, 0, 80).1.next().is_none());
    }

    #[test]
    fn empty_line_matches_compress_across_rows_without_losing_last_line() {
        let count = 65_537;
        let index: MatchIndex = (0..count).map(|line| span(line, 0, 0)).collect();
        assert_eq!(index.len(), count);
        assert!(index.retained_bytes() < 1024);
        assert_eq!(index.get(count - 1), Some(span(count - 1, 0, 0)));
        assert_eq!(index.scoped(Some((at(63, 0), at(128, 0)))).len(), 66);
    }
}
