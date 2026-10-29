//! Undo and redo for the native editor.
//!
//! The history is a stack of *transactions*, each an ordered list of
//! [`TextChange`]s plus the caret on either side of it. A change is measured
//! from the buffer itself rather than predicted from the keystroke that caused
//! it, so the history stays exact whatever the buffer does with an edit —
//! line-ending splits, grapheme-wide deletes, indentation widths — and every
//! entry records only the few bytes that actually changed.
//!
//! Consecutive keystrokes coalesce the way a reader expects from any desktop
//! editor: a run of typing is undone a word at a time, a run of Backspace or
//! Delete as one gesture, and a pause, a caret movement, or a change of kind
//! starts a new step. Everything else — a paste, a cut, indentation, a
//! completion, discarding unsaved edits — is a step of its own.
//!
//! Memory is bounded per document by both the number of steps and the bytes
//! they retain; the oldest steps are forgotten first.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use cosmic::iced::widget::text_editor::{Cursor, Position};

/// A pause longer than this ends the typing or deleting run in progress.
const GROUP_IDLE: Duration = Duration::from_secs(1);
/// Most steps one document keeps.
const MAX_TRANSACTIONS: usize = 10_000;
/// Most text, in bytes, one document's history retains.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Bookkeeping charged per step on top of its text, so a history of many
/// tiny steps is bounded by the byte budget as well as by the count.
const TRANSACTION_OVERHEAD: usize = 128;

/// One contiguous replacement: `removed`, starting at `start`, became
/// `inserted`.
///
/// Positions are the buffer's own: a line index and a byte column into that
/// line. Each change is addressed against the document exactly as it stood
/// immediately before the change was made.
#[derive(Debug, Clone, PartialEq)]
pub struct TextChange {
    pub start: Position,
    pub removed: String,
    pub inserted: String,
    /// Byte offsets immediately after each native line ending. Native edits
    /// can keep LF and CRLF separate even where a text parser reads LFCR.
    pub removed_breaks: Vec<usize>,
    pub inserted_breaks: Vec<usize>,
}

impl TextChange {
    /// Where the removed text ended, before the change.
    pub fn removed_end(&self) -> Position {
        advance_native(self.start, self.removed.len(), &self.removed_breaks)
    }

    /// Where the inserted text ends, after the change.
    pub fn inserted_end(&self) -> Position {
        advance_native(self.start, self.inserted.len(), &self.inserted_breaks)
    }

    fn bytes(&self) -> usize {
        self.removed.capacity()
            + self.inserted.capacity()
            + (self.removed_breaks.capacity() + self.inserted_breaks.capacity())
                * std::mem::size_of::<usize>()
    }
}

/// What kind of gesture produced a change, which decides whether it may
/// coalesce with the step before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Typed text, including Enter.
    Insert,
    /// Backspace with nothing selected.
    DeleteBackward,
    /// Delete with nothing selected.
    DeleteForward,
    /// A paste, cut, indentation, completion, discard, or any other edit
    /// that is always a step of its own.
    Other,
}

/// A buffer the history can rewind and replay.
pub trait EditBuffer {
    /// Replaces `expected`, which starts at `start`, with `replacement`.
    ///
    /// Refuses — answering `false` and changing nothing — when the text at
    /// `start` is not `expected`. That is the guard that keeps a history that
    /// has somehow diverged from its buffer from ever corrupting it.
    fn replace(
        &mut self,
        start: Position,
        expected: &str,
        replacement: &str,
        replacement_breaks: &[usize],
    ) -> bool;

    /// Places the caret, and the selection with it.
    fn set_cursor(&mut self, cursor: Cursor);
}

#[derive(Debug, Clone)]
struct Transaction {
    before_revision: u64,
    after_revision: u64,
    changes: Vec<TextChange>,
    /// File existence changes independently of text (an empty file is not absent).
    presence: Option<((bool, u64), (bool, u64))>,
    /// The caret before the first change, restored by undo.
    before: Cursor,
    /// The caret after the last change, restored by redo.
    after: Cursor,
    kind: EditKind,
    last_edit: Instant,
}

impl Transaction {
    fn bytes(&self) -> usize {
        TRANSACTION_OVERHEAD
            + self.changes.capacity() * std::mem::size_of::<TextChange>()
            + self.changes.iter().map(TextChange::bytes).sum::<usize>()
    }
}

/// The outcome of an undo or redo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replay {
    /// There was nothing to undo or redo.
    Empty,
    /// The step was applied.
    Applied,
    /// The buffer no longer held what the history expected. Whatever had
    /// already been applied stays applied, and the history is cleared so it
    /// cannot try again against a buffer it does not describe.
    Diverged,
}

/// One document's undo and redo stacks.
#[derive(Debug)]
pub struct History {
    revision: u64,
    next_revision: u64,
    undo: VecDeque<Transaction>,
    redo: Vec<Transaction>,
    /// Whether the newest undo step may still absorb the next keystroke.
    open: bool,
    /// Bytes retained by both stacks, as [`Transaction::bytes`] counts them.
    bytes: usize,
    max_transactions: usize,
    max_bytes: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::with_limits(MAX_TRANSACTIONS, MAX_BYTES)
    }
}

impl History {
    fn with_limits(max_transactions: usize, max_bytes: usize) -> Self {
        Self {
            revision: 0,
            next_revision: 1,
            undo: VecDeque::new(),
            redo: Vec::new(),
            open: false,
            bytes: 0,
            max_transactions: max_transactions.max(1),
            max_bytes,
        }
    }

    /// Identifies this exact point in the undo graph, independent of stack
    /// length, pruning and coalescing. Explicit Save seals the active
    /// transaction; background autosave preserves the current typing group.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Reserves an identity for an external disk baseline without claiming
    /// that the current buffer or any existing undo state contains its bytes.
    /// The caller may adopt it only after verifying the corresponding text.
    pub fn reserve_revision(&mut self) -> u64 {
        let reserved = self.next_revision;
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        reserved
    }

    fn advance_revision(&mut self) -> (u64, u64) {
        let before = self.revision;
        self.revision = self.reserve_revision();
        (before, self.revision)
    }

    /// The caller verified that the current bytes equal a known prior state
    /// (for example Discard edits restored the saved contents).
    pub fn adopt_revision(&mut self, known: u64) {
        if let Some(transaction) = self.undo.back_mut()
            && transaction.after_revision == self.revision
        {
            transaction.after_revision = known;
        }
        if let Some(transaction) = self.redo.last_mut()
            && transaction.before_revision == self.revision
        {
            transaction.before_revision = known;
        }
        self.revision = known;
        self.seal();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Ends the step in progress, so the next edit starts a new one.
    ///
    /// Called for every caret movement and selection change: typing that
    /// resumes somewhere else is a separate thought even when it lands back
    /// where it left off.
    pub fn seal(&mut self) {
        self.open = false;
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.advance_revision();
        self.undo.clear();
        self.redo.clear();
        self.open = false;
        self.bytes = 0;
    }

    /// Records one edit that has just been applied to the buffer.
    ///
    /// `before` is the caret and selection as they were before the edit —
    /// the exact span selected, which undo reselects — and `after` as they
    /// are now. Any redo steps are dropped: they describe a future this edit has
    /// replaced.
    pub fn record(
        &mut self,
        change: TextChange,
        kind: EditKind,
        before: Cursor,
        after: Cursor,
        now: Instant,
    ) {
        self.drop_redo();
        let (before_revision, after_revision) = self.advance_revision();
        if self.open
            && let Some(last) = self.undo.back_mut()
        {
            let previous_bytes = last.bytes();
            if coalesce(last, &change, kind, now) {
                self.bytes = self.bytes - previous_bytes + last.bytes();
                last.after_revision = after_revision;
                last.after = after;
                last.last_edit = now;
                self.enforce_limits();
                return;
            }
        }
        self.push(
            Transaction {
                before_revision,
                after_revision,
                before,
                changes: vec![change],
                presence: None,
                after,
                kind,
                last_edit: now,
            },
            kind != EditKind::Other,
        );
    }

    /// Records several changes that together are one step, such as a
    /// completion that edits both the call site and the imports.
    ///
    /// The changes are in the order they were applied, each addressed against
    /// the document the one before it produced. The step never coalesces
    /// with its neighbours.
    pub fn record_group(
        &mut self,
        changes: Vec<TextChange>,
        before: Cursor,
        after: Cursor,
        now: Instant,
    ) {
        self.record_group_with_presence(changes, before, after, now, None);
    }

    /// Records text and file existence as a single native Undo step. A pure
    /// existence change still earns a transaction when the text is unchanged.
    pub fn record_group_with_presence(
        &mut self,
        changes: Vec<TextChange>,
        before: Cursor,
        after: Cursor,
        now: Instant,
        presence: Option<(bool, bool)>,
    ) {
        self.record_group_with_presence_identities(
            changes,
            before,
            after,
            now,
            presence.map(|(before, after)| ((before, 0), (after, 0))),
        );
    }

    /// Existence history also owns the immutable restoration lineage. Text and
    /// metadata intentions can change independently, including empty files.
    pub fn record_group_with_presence_identities(
        &mut self,
        changes: Vec<TextChange>,
        before: Cursor,
        after: Cursor,
        now: Instant,
        presence: Option<((bool, u64), (bool, u64))>,
    ) {
        let presence = presence.filter(|(before, after)| before != after);
        if changes.is_empty() && presence.is_none() {
            return;
        }
        self.drop_redo();
        let (before_revision, after_revision) = self.advance_revision();
        self.push(
            Transaction {
                before_revision,
                after_revision,
                changes,
                presence,
                before,
                after,
                kind: EditKind::Other,
                last_edit: now,
            },
            false,
        );
    }

    /// Attach existence to an edit which recreates an intentionally absent
    /// document. Subsequent typing can still coalesce into that same gesture.
    pub fn record_latest_presence(&mut self, before: bool, after: bool) {
        self.record_latest_presence_identity((before, 0), (after, 0));
    }

    pub fn record_latest_presence_identity(&mut self, before: (bool, u64), after: (bool, u64)) {
        if let Some(transaction) = self.undo.back_mut() {
            let original = transaction.presence.map_or(before, |(before, _)| before);
            transaction.presence = (original != after).then_some((original, after));
        }
    }

    /// Expected and resulting existence for the next replay. Install only
    /// after replay answers Applied.
    pub fn replay_presence(&self, redo: bool) -> Option<(bool, bool)> {
        self.replay_presence_identity(redo)
            .map(|(before, after)| (before.0, after.0))
    }

    pub fn replay_presence_identity(&self, redo: bool) -> Option<((bool, u64), (bool, u64))> {
        let presence = if redo {
            self.redo.last()
        } else {
            self.undo.back()
        }?
        .presence?;
        Some(if redo {
            presence
        } else {
            (presence.1, presence.0)
        })
    }

    pub fn retained_presence_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.undo
            .iter()
            .chain(self.redo.iter())
            .filter_map(|transaction| transaction.presence)
            .flat_map(|(before, after)| [before.1, after.1])
    }

    /// Reverts the newest step and restores the caret it started from.
    pub fn undo(&mut self, buffer: &mut impl EditBuffer) -> Replay {
        self.open = false;
        let Some(transaction) = self.undo.pop_back() else {
            return Replay::Empty;
        };
        for change in transaction.changes.iter().rev() {
            if !buffer.replace(
                change.start,
                &change.inserted,
                &change.removed,
                &change.removed_breaks,
            ) {
                self.clear();
                return Replay::Diverged;
            }
        }
        buffer.set_cursor(transaction.before);
        self.revision = transaction.before_revision;
        self.redo.push(transaction);
        Replay::Applied
    }

    /// Re-applies the newest undone step and restores the caret it ended at.
    pub fn redo(&mut self, buffer: &mut impl EditBuffer) -> Replay {
        self.open = false;
        let Some(transaction) = self.redo.pop() else {
            return Replay::Empty;
        };
        for change in &transaction.changes {
            if !buffer.replace(
                change.start,
                &change.removed,
                &change.inserted,
                &change.inserted_breaks,
            ) {
                self.clear();
                return Replay::Diverged;
            }
        }
        buffer.set_cursor(transaction.after);
        self.revision = transaction.after_revision;
        self.undo.push_back(transaction);
        Replay::Applied
    }

    fn push(&mut self, transaction: Transaction, open: bool) {
        self.bytes += transaction.bytes();
        self.undo.push_back(transaction);
        self.open = open;
        self.enforce_limits();
    }

    fn drop_redo(&mut self) {
        for transaction in self.redo.drain(..) {
            self.bytes -= transaction.bytes();
        }
    }

    /// Forgets the oldest steps until the history fits its budget. The
    /// newest step is always kept, however large: the edit the user just
    /// made is the one they are most likely to want back.
    fn enforce_limits(&mut self) {
        while self.undo.len() > 1
            && (self.undo.len() + self.redo.len() > self.max_transactions
                || self.bytes > self.max_bytes)
        {
            let Some(oldest) = self.undo.pop_front() else {
                break;
            };
            self.bytes -= oldest.bytes();
        }
    }
}

/// Whether `change` extends `last`, merging it in when it does.
fn coalesce(last: &mut Transaction, change: &TextChange, kind: EditKind, now: Instant) -> bool {
    if kind == EditKind::Other
        || last.kind != kind
        || now.saturating_duration_since(last.last_edit) > GROUP_IDLE
    {
        return false;
    }
    let [previous] = last.changes.as_mut_slice() else {
        return false;
    };
    match kind {
        EditKind::Insert => {
            if !change.removed.is_empty()
                || !same(previous.inserted_end(), change.start)
                || starts_word(&previous.inserted, &change.inserted)
            {
                return false;
            }
            let offset = previous.inserted.len();
            previous
                .inserted_breaks
                .extend(change.inserted_breaks.iter().map(|end| offset + end));
            previous.inserted.push_str(&change.inserted);
        }
        EditKind::DeleteBackward => {
            if !change.inserted.is_empty()
                || !previous.inserted.is_empty()
                || !same(change.removed_end(), previous.start)
            {
                return false;
            }
            previous.start = change.start;
            let mut endings = change.removed_breaks.clone();
            endings.extend(
                previous
                    .removed_breaks
                    .iter()
                    .map(|end| change.removed.len() + end),
            );
            previous.removed_breaks = endings;
            previous.removed.insert_str(0, &change.removed);
        }
        EditKind::DeleteForward => {
            if !change.inserted.is_empty()
                || !previous.inserted.is_empty()
                || !same(change.start, previous.start)
            {
                return false;
            }
            let offset = previous.removed.len();
            previous
                .removed_breaks
                .extend(change.removed_breaks.iter().map(|end| offset + end));
            previous.removed.push_str(&change.removed);
        }
        EditKind::Other => return false,
    }
    true
}

/// Whether typing `next` after `previous` begins a new word, which is where
/// a run of typing is split into separate undo steps.
fn starts_word(previous: &str, next: &str) -> bool {
    let ended_in_space = previous
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    let starts_text = next.chars().next().is_some_and(|c| !c.is_whitespace());
    ended_in_space && starts_text
}

fn order(position: Position) -> (usize, usize) {
    (position.line, position.column)
}

fn same(left: Position, right: Position) -> bool {
    order(left) == order(right)
}

/// Byte offsets of each line break in `text`, and its length, using the
/// buffer's own rules: `\r\n` and `\n\r` are single breaks, and a lone `\r`
/// or `\n` is one.
fn breaks(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let bytes = text.as_bytes();
    let mut index = 0;
    std::iter::from_fn(move || {
        while index < bytes.len() {
            let at = index;
            match bytes[at] {
                first @ (b'\r' | b'\n') => {
                    let pair = match bytes.get(at + 1) {
                        Some(&second) if second != first && matches!(second, b'\r' | b'\n') => 2,
                        _ => 1,
                    };
                    index = at + pair;
                    return Some((at, pair));
                }
                _ => index += 1,
            }
        }
        None
    })
}

/// The position reached by inserting `text` at `start`.
pub fn advance(start: Position, text: &str) -> Position {
    let mut lines = 0;
    let mut tail = 0;
    for (at, length) in breaks(text) {
        lines += 1;
        tail = at + length;
    }
    if lines == 0 {
        Position {
            line: start.line,
            column: start.column + text.len(),
        }
    } else {
        Position {
            line: start.line + lines,
            column: text.len() - tail,
        }
    }
}

fn advance_native(start: Position, length: usize, endings: &[usize]) -> Position {
    match endings.last() {
        Some(end) => Position {
            line: start.line + endings.len(),
            column: length - end,
        },
        None => Position {
            line: start.line,
            column: start.column + length,
        },
    }
}

/// Whether pasting these bytes once preserves the recorded native endings.
/// This scan allocates nothing; ordinary text takes the native bulk path.
pub fn canonical_breaks(text: &str, endings: &[usize]) -> bool {
    breaks(text)
        .map(|(start, length)| start + length)
        .eq(endings.iter().copied())
}

/// A run of consecutive buffer lines serialized exactly as the saved file
/// would hold them, remembering where each line's text ends.
///
/// Two excerpts of the same lines, taken before and after an edit, are all
/// [`replacement`] needs to recover precisely what the edit did.
#[derive(Debug, Clone, Default)]
pub struct Excerpt {
    first_line: usize,
    text: String,
    /// `(start, text_end)` of each line, as byte offsets into `text`.
    lines: Vec<(usize, usize)>,
}

impl Excerpt {
    /// An empty excerpt that will begin at buffer line `first_line`.
    pub fn new(first_line: usize) -> Self {
        Self {
            first_line,
            ..Self::default()
        }
    }

    /// Appends one line and whatever the saved file writes after it.
    pub fn push_line(&mut self, text: &str, terminator: &str) {
        let start = self.text.len();
        self.text.push_str(text);
        self.lines.push((start, self.text.len()));
        self.text.push_str(terminator);
    }

    /// Splits a whole document into lines the way the buffer does.
    pub fn document(text: &str) -> Self {
        let mut excerpt = Self::new(0);
        let mut start = 0;
        for (at, length) in breaks(text) {
            excerpt.lines.push((start, at));
            start = at + length;
        }
        excerpt.lines.push((start, text.len()));
        excerpt.text = text.to_owned();
        excerpt
    }

    /// The byte offset of a buffer position inside this excerpt, when the
    /// excerpt covers it.
    pub fn offset(&self, position: Position) -> Option<usize> {
        let &(start, text_end) = self
            .lines
            .get(position.line.checked_sub(self.first_line)?)?;
        let offset = start + position.column;
        (offset <= text_end).then_some(offset)
    }

    /// The text of the whole excerpt.
    pub fn text(&self) -> &str {
        &self.text
    }

    fn endings(&self) -> impl DoubleEndedIterator<Item = (usize, usize)> + '_ {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(index, &(_, start))| {
                let end = self
                    .lines
                    .get(index + 1)
                    .map_or(self.text.len(), |&(start, _)| start);
                (end > start).then_some((start, end))
            })
    }

    fn slice_breaks(&self, start: usize, end: usize) -> Vec<usize> {
        self.endings()
            .filter_map(|(_, boundary)| {
                (boundary > start && boundary <= end).then(|| boundary - start)
            })
            .collect()
    }

    /// The line an offset falls on, by index into `lines`.
    fn line_of(&self, offset: usize) -> usize {
        self.lines
            .partition_point(|&(start, _)| start <= offset)
            .saturating_sub(1)
    }

    /// Whether `offset` names a caret position: anywhere but strictly inside
    /// a line break.
    fn is_position(&self, offset: usize) -> bool {
        let Some(&(_, text_end)) = self.lines.get(self.line_of(offset)) else {
            return true;
        };
        offset <= text_end || offset == self.text.len()
    }

    /// Where the line break an offset sits inside of begins, and where it ends.
    fn break_around(&self, offset: usize) -> (usize, usize) {
        let index = self.line_of(offset);
        let text_end = self.lines.get(index).map_or(0, |&(_, end)| end);
        let next = self
            .lines
            .get(index + 1)
            .map_or(self.text.len(), |&(start, _)| start);
        (text_end, next)
    }

    /// The buffer position of a byte offset into this excerpt.
    pub fn position(&self, offset: usize) -> Position {
        let index = self.line_of(offset);
        match self.lines.get(index) {
            Some(&(start, text_end)) if offset <= text_end => Position {
                line: self.first_line + index,
                column: offset - start,
            },
            // Past the break that ends the excerpt: the start of the line
            // after it.
            _ => Position {
                line: self.first_line + self.lines.len(),
                column: 0,
            },
        }
    }
}

/// The single replacement that turns `before` into `after`, or `None` when
/// they are the same.
///
/// Both excerpts must cover the same lines of the document: the lines the
/// edit could touch, taken before and after it. The replacement is the
/// smallest one that does not split a line break, since a caret cannot sit
/// between the two halves of `\r\n`.
///
/// `max_prefix` is the offset into `before` where the edit is known to
/// begin at the latest, such as the caret a character was typed at. Next to
/// a repeated character the difference alone is ambiguous — typing `l` in
/// `he|lo` reads as well at either `l` — and the rightmost reading would
/// put the change after the caret, splitting a run of typing or deleting
/// into separate undo steps. Pass `usize::MAX` when nothing is known.
pub fn replacement(before: &Excerpt, after: &Excerpt, max_prefix: usize) -> Option<TextChange> {
    debug_assert_eq!(before.first_line, after.first_line);
    let (old, new) = (before.text.as_str(), after.text.as_str());
    if old == new && before.lines == after.lines {
        return None;
    }
    let canonical = |excerpt: &Excerpt| {
        excerpt
            .endings()
            .eq(breaks(excerpt.text()).map(|(start, length)| (start, start + length)))
    };
    // Native edits can retain adjacent endings that a plain-text parser
    // would pair differently. In that uncommon case preserve the captured
    // native lines as a whole: the excerpt is still only the affected lines
    // and their immediate neighbours, and no byte-only trim can lose a
    // boundary needed by an earlier history step.
    if !canonical(before) || !canonical(after) {
        return Some(TextChange {
            start: Position {
                line: before.first_line,
                column: 0,
            },
            removed: old.to_owned(),
            inserted: new.to_owned(),
            removed_breaks: before.slice_breaks(0, old.len()),
            inserted_breaks: after.slice_breaks(0, new.len()),
        });
    }
    // Capping only widens the change, so any cap still describes the edit.
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(left, right)| left == right)
        .count()
        .min(max_prefix);
    while !old.is_char_boundary(prefix) {
        prefix -= 1;
    }
    while !before.is_position(prefix) || !after.is_position(prefix) {
        prefix = if before.is_position(prefix) {
            after.break_around(prefix).0
        } else {
            before.break_around(prefix).0
        }
        .min(prefix);
    }
    let room = old.len().min(new.len()) - prefix;
    let mut suffix = old[prefix..]
        .bytes()
        .rev()
        .zip(new[prefix..].bytes().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(room);
    while !old.is_char_boundary(old.len() - suffix) {
        suffix -= 1;
    }
    loop {
        let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
        if !before.is_position(old_end) {
            suffix -= before.break_around(old_end).1 - old_end;
        } else if !after.is_position(new_end) {
            suffix -= after.break_around(new_end).1 - new_end;
        } else {
            break;
        }
    }
    let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
    if prefix == old_end && prefix == new_end {
        return None;
    }
    debug_assert!(same(before.position(prefix), after.position(prefix)));
    Some(TextChange {
        start: before.position(prefix),
        removed: old[prefix..old_end].to_owned(),
        inserted: new[prefix..new_end].to_owned(),
        removed_breaks: before.slice_breaks(prefix, old_end),
        inserted_breaks: after.slice_breaks(prefix, new_end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: usize, column: usize) -> Position {
        Position { line, column }
    }

    fn caret(line: usize, column: usize) -> Cursor {
        Cursor {
            position: at(line, column),
            selection: None,
        }
    }

    /// A plain-string buffer with the editor's line rules, standing in for
    /// the native one.
    #[derive(Debug)]
    struct Model {
        text: String,
        cursor: Cursor,
    }

    impl Model {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_owned(),
                cursor: caret(0, 0),
            }
        }

        fn offset(&self, position: Position) -> Option<usize> {
            let excerpt = Excerpt::document(&self.text);
            let &(start, end) = excerpt.lines.get(position.line)?;
            (start + position.column <= end).then_some(start + position.column)
        }

        fn position(&self, offset: usize) -> Position {
            Excerpt::document(&self.text).position(offset)
        }

        /// Replaces `length` bytes at `offset` and records the change the
        /// way the editor does: by comparing the document before and after.
        fn edit(
            &mut self,
            history: &mut History,
            offset: usize,
            length: usize,
            text: &str,
            kind: EditKind,
            now: Instant,
        ) {
            let before_cursor = self.cursor;
            let before = Excerpt::document(&self.text);
            self.text.replace_range(offset..offset + length, text);
            let after = Excerpt::document(&self.text);
            self.cursor = Cursor {
                position: self.position(offset + text.len()),
                selection: None,
            };
            let change = replacement(&before, &after, offset).expect("the edit changed something");
            history.record(change, kind, before_cursor, self.cursor, now);
        }

        fn type_text(&mut self, history: &mut History, text: &str, now: Instant) {
            for character in text.chars() {
                let offset = self.offset(self.cursor.position).unwrap();
                self.edit(
                    history,
                    offset,
                    0,
                    character.encode_utf8(&mut [0; 4]),
                    EditKind::Insert,
                    now,
                );
            }
        }

        fn backspace(&mut self, history: &mut History, now: Instant) {
            let offset = self.offset(self.cursor.position).unwrap();
            let previous = self.text[..offset].chars().next_back().unwrap();
            let length = previous.len_utf8();
            self.edit(
                history,
                offset - length,
                length,
                "",
                EditKind::DeleteBackward,
                now,
            );
        }
    }

    impl EditBuffer for Model {
        fn replace(
            &mut self,
            start: Position,
            expected: &str,
            replacement: &str,
            _replacement_breaks: &[usize],
        ) -> bool {
            let Some(offset) = self.offset(start) else {
                return false;
            };
            if !self.text[offset..].starts_with(expected) {
                return false;
            }
            self.text
                .replace_range(offset..offset + expected.len(), replacement);
            true
        }

        fn set_cursor(&mut self, cursor: Cursor) {
            self.cursor = cursor;
        }
    }

    fn undo_all(history: &mut History, model: &mut Model) -> usize {
        let mut steps = 0;
        while history.undo(model) == Replay::Applied {
            steps += 1;
        }
        steps
    }

    #[test]
    fn file_presence_only_revert_is_one_transaction_without_a_text_edit() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("");
        history.record_group_with_presence(
            vec![],
            model.cursor,
            model.cursor,
            now,
            Some((true, false)),
        );
        assert_eq!(history.replay_presence(false), Some((false, true)));
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert!(model.text.is_empty());
        assert!(!history.can_undo());
        assert_eq!(history.replay_presence(true), Some((true, false)));
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert!(model.text.is_empty());
    }

    #[test]
    fn presence_only_revision_replays_and_branches_without_aliasing_saved_state() {
        let mut history = History::default();
        let mut model = Model::new("");
        let saved = history.revision();
        let now = Instant::now();
        history.record_group_with_presence_identities(
            vec![],
            model.cursor,
            model.cursor,
            now,
            Some(((true, 7), (false, 8))),
        );
        let removed = history.revision();
        assert_ne!(
            removed, saved,
            "equal empty text does not erase a presence edit"
        );
        assert_eq!(
            history.replay_presence_identity(false),
            Some(((false, 8), (true, 7)))
        );
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), saved);
        assert_eq!(
            history.replay_presence_identity(true),
            Some(((true, 7), (false, 8)))
        );
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), removed);
        assert!(model.text.is_empty());
        assert_eq!(history.undo(&mut model), Replay::Applied);
        history.record_group_with_presence_identities(
            vec![],
            model.cursor,
            model.cursor,
            now,
            Some(((true, 7), (false, 9))),
        );
        assert_ne!(history.revision(), saved);
        assert_ne!(
            history.revision(),
            removed,
            "a new removal has its own revision"
        );
        assert!(!history.can_redo());
        assert_eq!(
            history
                .retained_presence_ids()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([7, 9]),
        );
    }

    #[test]
    fn revisions_restore_saved_checkpoint_and_distinguish_branched_history() {
        let mut history = History::default();
        let mut model = Model::new("");
        let now = Instant::now();
        let initial = history.revision();
        model.type_text(&mut history, "hello", now);
        let saved = history.revision();
        history.seal();
        model.type_text(&mut history, "!", now);
        let later = history.revision();
        assert_ne!(saved, later);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), saved);
        assert_eq!(model.text, "hello");
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), later);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        model.type_text(&mut history, "?", now);
        assert_ne!(
            history.revision(),
            later,
            "a branch cannot reuse a redo state identity"
        );
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), saved);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), initial);
    }

    #[test]
    fn external_baseline_identity_does_not_alias_undo_or_future_edits() {
        let mut history = History::default();
        let mut model = Model::new("A");
        let initial = history.revision();
        let now = Instant::now();
        model.edit(&mut history, 0, 1, "B", EditKind::Other, now);
        let mine = history.revision();
        let external = history.reserve_revision();
        assert_eq!(
            history.revision(),
            mine,
            "Keep mine must retain the buffer's identity"
        );
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "A");
        assert_eq!(history.revision(), initial);
        assert_ne!(
            history.revision(),
            external,
            "Undo to A is dirty against external disk C"
        );
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(model.text, "B");
        assert_eq!(history.revision(), mine);
        assert_ne!(history.revision(), external);
        model.edit(&mut history, 0, 1, "C", EditKind::Other, now);
        assert_ne!(
            history.revision(),
            external,
            "future edits cannot consume the reserved identity"
        );
        // A byte comparison now proves that this state equals disk C.
        history.adopt_revision(external);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "B");
        assert_ne!(history.revision(), external);
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(model.text, "C");
        assert_eq!(history.revision(), external);
    }

    #[test]
    fn adopting_verified_saved_bytes_keeps_discard_and_undo_states_coherent() {
        let mut history = History::default();
        let mut model = Model::new("saved");
        let saved = history.revision();
        let now = Instant::now();
        model.type_text(&mut history, "x", now);
        let dirty = history.revision();
        model.edit(&mut history, 0, 1, "", EditKind::Other, now);
        assert_eq!(model.text, "saved");
        history.adopt_revision(saved);
        assert_eq!(history.revision(), saved);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), dirty);
        assert_eq!(model.text, "xsaved");
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(history.revision(), saved);
        assert_eq!(model.text, "saved");
    }

    #[test]
    fn typing_is_undone_a_word_at_a_time_and_redone_in_order() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("");
        model.type_text(&mut history, "hello brave world", now);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "hello brave ");
        assert_eq!(model.cursor, caret(0, 12));
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "hello ");
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "");
        assert_eq!(history.undo(&mut model), Replay::Empty);
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(model.text, "hello ");
        assert_eq!(model.cursor, caret(0, 6));
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(model.text, "hello brave world");
        assert_eq!(history.redo(&mut model), Replay::Empty);
    }

    #[test]
    fn a_new_line_ends_the_word_before_it() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("");
        model.type_text(&mut history, "fn\nmain", now);
        history.undo(&mut model);
        assert_eq!(model.text, "fn\n");
        history.undo(&mut model);
        assert_eq!(model.text, "");
    }

    #[test]
    fn a_pause_or_a_caret_move_starts_a_new_step() {
        let start = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("");
        model.type_text(&mut history, "ab", start);
        model.type_text(&mut history, "cd", start + GROUP_IDLE * 2);
        history.seal();
        model.type_text(&mut history, "ef", start + GROUP_IDLE * 2);
        assert_eq!(undo_all(&mut history, &mut model), 3);
        assert_eq!(model.text, "");
    }

    #[test]
    fn backspace_runs_coalesce_and_restore_the_caret_they_started_from() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("keep this text");
        model.cursor = caret(0, 14);
        for _ in 0..5 {
            model.backspace(&mut history, now);
        }
        assert_eq!(model.text, "keep this");
        // Switching from deleting to typing is a new step.
        model.type_text(&mut history, "!", now);
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "keep this");
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "keep this text");
        assert_eq!(model.cursor, caret(0, 14));
    }

    #[test]
    fn forward_deletes_coalesce_at_a_fixed_caret() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("abcdef");
        model.cursor = caret(0, 1);
        for _ in 0..3 {
            model.edit(&mut history, 1, 1, "", EditKind::DeleteForward, now);
        }
        assert_eq!(model.text, "aef");
        assert_eq!(undo_all(&mut history, &mut model), 1);
        assert_eq!(model.text, "abcdef");
    }

    #[test]
    fn coalescing_preserves_separate_native_line_endings() {
        let now = Instant::now();
        for kind in [
            EditKind::Insert,
            EditKind::DeleteForward,
            EditKind::DeleteBackward,
        ] {
            let mut history = History::default();
            let changes = match kind {
                EditKind::Insert => [
                    TextChange {
                        start: caret(0, 0).position,
                        removed: String::new(),
                        inserted: "\n".into(),
                        removed_breaks: vec![],
                        inserted_breaks: vec![1],
                    },
                    TextChange {
                        start: caret(1, 0).position,
                        removed: String::new(),
                        inserted: "\r\n".into(),
                        removed_breaks: vec![],
                        inserted_breaks: vec![2],
                    },
                ],
                EditKind::DeleteForward => [
                    TextChange {
                        start: caret(0, 0).position,
                        removed: "\n".into(),
                        inserted: String::new(),
                        removed_breaks: vec![1],
                        inserted_breaks: vec![],
                    },
                    TextChange {
                        start: caret(0, 0).position,
                        removed: "\r\n".into(),
                        inserted: String::new(),
                        removed_breaks: vec![2],
                        inserted_breaks: vec![],
                    },
                ],
                EditKind::DeleteBackward => [
                    TextChange {
                        start: caret(1, 0).position,
                        removed: "\r\n".into(),
                        inserted: String::new(),
                        removed_breaks: vec![2],
                        inserted_breaks: vec![],
                    },
                    TextChange {
                        start: caret(0, 0).position,
                        removed: "\n".into(),
                        inserted: String::new(),
                        removed_breaks: vec![1],
                        inserted_breaks: vec![],
                    },
                ],
                EditKind::Other => unreachable!(),
            };
            for change in changes {
                history.record(change, kind, caret(0, 0), caret(0, 0), now);
            }
            assert_eq!(history.undo.len(), 1, "{kind:?}");
            let change = &history.undo[0].changes[0];
            let endings = if kind == EditKind::Insert {
                &change.inserted_breaks
            } else {
                &change.removed_breaks
            };
            assert_eq!(endings, &[1, 3], "{kind:?}");
        }
    }

    #[test]
    fn replacing_identical_bytes_still_records_changed_native_boundaries() {
        let mut before = Excerpt::new(0);
        before.push_line("a", "\n");
        before.push_line("", "\r\n");
        before.push_line("b", "");
        let after = Excerpt::document("a\n\r\nb");
        let change = replacement(&before, &after, usize::MAX).unwrap();
        assert_eq!(change.start, at(0, 0));
        assert_eq!(change.removed, "a\n\r\nb");
        assert_eq!(change.removed, change.inserted);
        assert_eq!(change.removed_breaks, [2, 4]);
        assert_eq!(change.inserted_breaks, [3, 4]);
        assert_eq!(change.removed_end(), at(2, 1));
        assert_eq!(change.inserted_end(), at(2, 1));
    }

    #[test]
    fn runs_beside_a_repeated_character_stay_one_step() {
        let now = Instant::now();
        // Typing `l` after `hex|lo` reads just as well as an insertion after
        // the existing `l`; it must be recorded at the caret it was typed at.
        let mut history = History::default();
        let mut model = Model::new("helo");
        model.cursor = caret(0, 2);
        model.type_text(&mut history, "xl", now);
        assert_eq!(model.text, "hexllo");
        assert_eq!(undo_all(&mut history, &mut model), 1);
        assert_eq!(model.text, "helo");

        let mut history = History::default();
        let mut model = Model::new("abbc");
        model.cursor = caret(0, 1);
        for _ in 0..2 {
            model.edit(&mut history, 1, 1, "", EditKind::DeleteForward, now);
        }
        assert_eq!(model.text, "ac");
        assert_eq!(undo_all(&mut history, &mut model), 1);
        assert_eq!(model.text, "abbc");

        let mut history = History::default();
        let mut model = Model::new("abbc");
        model.cursor = caret(0, 2);
        for _ in 0..2 {
            model.backspace(&mut history, now);
        }
        assert_eq!(model.text, "bc");
        assert_eq!(undo_all(&mut history, &mut model), 1);
        assert_eq!(model.text, "abbc");
        assert_eq!(model.cursor, caret(0, 2));
    }

    #[test]
    fn a_replaced_selection_is_reselected_on_undo() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("one two three");
        // Selected right to left, the way a reader drags backwards.
        model.cursor = Cursor {
            position: at(0, 4),
            selection: Some(at(0, 7)),
        };
        model.edit(&mut history, 4, 3, "2", EditKind::Insert, now);
        model.type_text(&mut history, "2", now);
        assert_eq!(model.text, "one 22 three");
        assert_eq!(undo_all(&mut history, &mut model), 1);
        assert_eq!(model.text, "one two three");
        assert_eq!(
            model.cursor,
            Cursor {
                position: at(0, 4),
                selection: Some(at(0, 7)),
            }
        );
    }

    #[test]
    fn a_new_edit_discards_the_redo_stack() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("");
        model.type_text(&mut history, "abc", now);
        history.undo(&mut model);
        assert!(history.can_redo());
        model.type_text(&mut history, "x", now);
        assert!(!history.can_redo());
        assert_eq!(history.redo(&mut model), Replay::Empty);
        assert_eq!(model.text, "x");
    }

    #[test]
    fn grouped_changes_undo_as_one_step_in_reverse() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("call(\n");
        // Two edits applied last-first, the way a completion applies them.
        let first = TextChange {
            start: at(0, 4),
            removed: String::new(),
            inserted: "_me".into(),
            removed_breaks: vec![],
            inserted_breaks: vec![],
        };
        let second = TextChange {
            start: at(0, 0),
            removed: String::new(),
            inserted: "use x;\n".into(),
            removed_breaks: vec![],
            inserted_breaks: vec![7],
        };
        assert!(model.replace(first.start, "", &first.inserted, &first.inserted_breaks));
        assert!(model.replace(second.start, "", &second.inserted, &second.inserted_breaks));
        history.record_group(vec![first, second], caret(0, 4), caret(1, 7), now);
        assert_eq!(model.text, "use x;\ncall_me(\n");
        assert_eq!(history.undo(&mut model), Replay::Applied);
        assert_eq!(model.text, "call(\n");
        assert_eq!(model.cursor, caret(0, 4));
        assert_eq!(history.redo(&mut model), Replay::Applied);
        assert_eq!(model.text, "use x;\ncall_me(\n");
        assert_eq!(model.cursor, caret(1, 7));
    }

    #[test]
    fn a_buffer_that_diverged_is_left_alone_and_the_history_is_dropped() {
        let now = Instant::now();
        let mut history = History::default();
        let mut model = Model::new("");
        model.type_text(&mut history, "abc", now);
        model.text = "something else".into();
        assert_eq!(history.undo(&mut model), Replay::Diverged);
        assert_eq!(model.text, "something else");
        assert!(!history.can_undo() && !history.can_redo());
    }

    #[test]
    fn the_oldest_steps_are_forgotten_past_the_budget() {
        let now = Instant::now();
        let mut history = History::with_limits(3, usize::MAX);
        let mut model = Model::new("");
        for word in ["a ", "b ", "c ", "d ", "e "] {
            model.type_text(&mut history, word, now);
            history.seal();
        }
        assert_eq!(undo_all(&mut history, &mut model), 3);
        assert_eq!(model.text, "a b ");

        let mut history = History::with_limits(
            usize::MAX,
            2 * (TRANSACTION_OVERHEAD + std::mem::size_of::<TextChange>()) + 16,
        );
        let mut model = Model::new("");
        for word in ["1234", "5678", "9abc"] {
            model.type_text(&mut history, word, now);
            history.seal();
        }
        assert_eq!(undo_all(&mut history, &mut model), 2);
        assert_eq!(model.text, "1234");
        // The newest step survives even when it alone is over budget.
        let mut history = History::with_limits(usize::MAX, 1);
        let mut model = Model::new("");
        model.edit(&mut history, 0, 0, "big paste", EditKind::Other, now);
        assert_eq!(undo_all(&mut history, &mut model), 1);
        assert_eq!(model.text, "");
    }

    #[test]
    fn replacements_never_split_a_line_break() {
        // Turning CRLF into LF must not be recorded as deleting half of it.
        let change = replacement(
            &Excerpt::document("a\r\nb"),
            &Excerpt::document("a\nb"),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(change.start, at(0, 1));
        assert_eq!(change.removed, "\r\n");
        assert_eq!(change.inserted, "\n");
        // Joining lines across a CRLF removes the whole break.
        let change = replacement(
            &Excerpt::document("a\r\nb"),
            &Excerpt::document("ab"),
            usize::MAX,
        )
        .unwrap();
        assert_eq!((change.start, change.removed.as_str()), (at(0, 1), "\r\n"));
        // An insertion right before an existing break stays an insertion.
        let change = replacement(
            &Excerpt::document("a\nb"),
            &Excerpt::document("a\n\nb"),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(change.removed, "");
        assert_eq!(change.inserted, "\n");
        assert!(
            replacement(
                &Excerpt::document("same"),
                &Excerpt::document("same"),
                usize::MAX
            )
            .is_none()
        );
    }

    #[test]
    fn replacements_keep_multibyte_characters_whole() {
        let change = replacement(
            &Excerpt::document("aé"),
            &Excerpt::document("aè"),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(change.removed, "é");
        assert_eq!(change.inserted, "è");
        assert_eq!(change.start, at(0, 1));
    }

    #[test]
    fn advance_follows_the_buffer_line_rules() {
        assert_eq!(advance(at(2, 3), "abc"), at(2, 6));
        assert_eq!(advance(at(2, 3), "a\nbc"), at(3, 2));
        assert_eq!(advance(at(0, 0), "a\r\nb\n\rc\rd"), at(3, 1));
        assert_eq!(advance(at(0, 5), "\n"), at(1, 0));
    }

    /// Random edits of every kind, then undoing everything, must reproduce
    /// the original document byte for byte; redoing everything must reproduce
    /// the final one.
    #[test]
    fn random_edit_sequences_round_trip() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound.max(1) as u64) as usize
        };
        let pieces = ["a", "é", " ", "\n", "\r\n", "xyz", "\t", "漢字", "\r"];
        let start = Instant::now();
        for round in 0..200 {
            let original = "fn main() {\r\n    println!(\"hé\");\n}\n";
            let mut model = Model::new(original);
            let mut history = History::default();
            for step in 0..40 {
                let now = start + Duration::from_millis((round * 1000 + step * 300) as u64);
                let boundaries: Vec<usize> = (0..=model.text.len())
                    .filter(|&offset| model.text.is_char_boundary(offset))
                    .collect();
                let index = next(boundaries.len());
                let from = boundaries[index];
                let to = boundaries[(index + next(4)).min(boundaries.len() - 1)];
                let text = pieces[next(pieces.len())];
                let kind = [
                    EditKind::Insert,
                    EditKind::DeleteBackward,
                    EditKind::DeleteForward,
                    EditKind::Other,
                ][next(4)];
                let mut after = model.text.clone();
                after.replace_range(from..to, text);
                let Some(change) = replacement(
                    &Excerpt::document(&model.text),
                    &Excerpt::document(&after),
                    from,
                ) else {
                    continue;
                };
                let before = Cursor {
                    position: model.position(from),
                    selection: (from != to).then(|| model.position(to)),
                };
                // Apply the recorded change rather than the raw edit, which
                // is what proves the recording exact.
                assert!(model.replace(
                    change.start,
                    &change.removed,
                    &change.inserted,
                    &change.inserted_breaks
                ));
                assert_eq!(model.text, after, "round {round} step {step}");
                model.cursor = Cursor {
                    position: change.inserted_end(),
                    selection: None,
                };
                history.record(change, kind, before, model.cursor, now);
                if next(5) == 0 {
                    history.seal();
                }
            }
            let last = model.text.clone();
            undo_all(&mut history, &mut model);
            assert_eq!(model.text, original, "round {round}");
            while history.redo(&mut model) == Replay::Applied {}
            assert_eq!(model.text, last, "round {round}");
        }
    }
}
