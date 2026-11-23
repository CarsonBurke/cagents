//! Pending async clipboard/chooser anchors follow measured native edits. They
//! do not mutate the editor's undo history. Only affected native lines are measured.
use crate::editor_history::{self, Excerpt, TextChange};
use cosmic::widget::text_editor::{self, Content, Cursor, Position};

/// Native lines preserve boundaries which a whole-string line parser can merge.
pub(super) fn excerpt(content: &Content<cosmic::Renderer>, first: usize, last: usize) -> Excerpt {
    let count = content.line_count();
    let mut result = Excerpt::new(first);
    for index in first..=last.min(count.saturating_sub(1)) {
        if let Some(line) = content.line(index) {
            let ending = if index + 1 == count {
                ""
            } else if line.ending == text_editor::LineEnding::None {
                text_editor::LineEnding::default().as_str()
            } else {
                line.ending.as_str()
            };
            result.push_line(&line.text, ending);
        }
    }
    if count == 0 {
        result.push_line("", "");
    }
    result
}

/// Replace the native selection as well as its caret. `Content::move_to` only
/// installs a supplied anchor; `None` leaves an existing word/line selection
/// intact. An async insertion must not inherit that unrelated selection.
pub(super) fn move_to(content: &mut Content, cursor: Cursor) {
    content.perform(text_editor::Action::ClearSelection);
    content.move_to(cursor);
}

/// The lines an edit of the composer's selection can reach: those it covers,
/// and one either side for a delete that joins lines.
fn edited_lines(content: &Content<cosmic::Renderer>, span: Cursor) -> (usize, usize) {
    let anchor = span.selection.unwrap_or(span.position);
    let first = span.position.line.min(anchor.line).saturating_sub(1);
    let last = span
        .position
        .line
        .max(anchor.line)
        .saturating_add(1)
        .min(content.line_count().saturating_sub(1));
    (first, last)
}

/// Perform the real composer edit and measure what it replaced, or `None`
/// when it changed nothing. The selection's actual span, which a word or
/// line selection's cursor does not report, says where the edit begins, so a
/// change beside repeated text is placed where the reader made it.
pub(super) fn edit(
    content: &mut Content<cosmic::Renderer>,
    edit: text_editor::Edit,
) -> Option<TextChange> {
    let count = content.line_count();
    let span = content.selection_span();
    let (first, last) = edited_lines(content, span);
    let before = excerpt(content, first, last);
    let changed_from = (|| {
        let caret = before.offset(span.position)?;
        Some(match span.selection {
            Some(anchor) => caret.min(before.offset(anchor)?),
            None if matches!(edit, text_editor::Edit::Backspace) => before
                .text()
                .get(..caret)?
                .chars()
                .next_back()
                .map_or(caret, |c| caret - c.len_utf8()),
            None => caret,
        })
    })()
    // Without a known start the difference alone still describes the edit.
    .unwrap_or(usize::MAX);
    content.perform(text_editor::Action::Edit(edit));
    let after_count = content.line_count();
    let after_last = last
        .saturating_add(after_count)
        .saturating_sub(count)
        .max(first)
        .min(after_count.saturating_sub(1));
    let after = excerpt(content, first, after_last);
    editor_history::replacement(&before, &after, changed_from)
}

#[derive(Clone, Copy)]
pub(super) struct Bookmark {
    pub cursor: Cursor,
    pub valid: bool,
}
impl Bookmark {
    /// Where an insertion made now would go: over the selection's actual
    /// span, or at the caret.
    pub fn capture(content: &Content) -> Self {
        Self::new(content.selection_span())
    }
    pub fn new(cursor: Cursor) -> Self {
        Self {
            cursor,
            valid: true,
        }
    }
    pub fn changed(&mut self, change: &TextChange) {
        if !self.valid {
            return;
        }
        if let Some(cursor) = mapped(self.cursor, change) {
            self.cursor = cursor;
        } else {
            self.valid = false;
        }
    }
}
fn key(p: Position) -> (usize, usize) {
    (p.line, p.column)
}
fn mapped_point(p: Position, change: &TextChange) -> Option<Position> {
    let start = change.start;
    let end = change.removed_end();
    if key(p) <= key(start) {
        return Some(p);
    }
    if key(p) < key(end) {
        return None;
    }
    let inserted = change.inserted_end();
    Some(if p.line == end.line {
        Position {
            line: inserted.line,
            column: inserted.column + p.column.checked_sub(end.column)?,
        }
    } else {
        Position {
            line: p.line.checked_sub(end.line)?.checked_add(inserted.line)?,
            column: p.column,
        }
    })
}
pub(super) fn mapped(cursor: Cursor, change: &TextChange) -> Option<Cursor> {
    if let Some(anchor) = cursor.selection {
        let (a, b) = if key(anchor) <= key(cursor.position) {
            (anchor, cursor.position)
        } else {
            (cursor.position, anchor)
        };
        // A concurrent edit inside the selected text invalidates the pending
        // replacement. Its callback must never erase newly written prose.
        if key(change.start) < key(b) && key(change.removed_end()) > key(a)
            || key(change.start) >= key(a) && key(change.start) < key(b)
        {
            return None;
        }
    }
    let selection = match cursor.selection {
        Some(p) => Some(mapped_point(p, change)?),
        None => None,
    };
    Some(Cursor {
        position: mapped_point(cursor.position, change)?,
        selection,
    })
}

/// A permit covers clipboard/chooser callbacks AND queued worker ownership.
/// Resetting a draft cancels its lease, but does not release the worker's slot.
#[derive(Clone, Debug)]
pub struct Ticket {
    pub id: uuid::Uuid,
    state: std::sync::Arc<Job>,
}
#[derive(Debug)]
struct Job {
    cancelled: std::sync::atomic::AtomicBool,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
pub(super) struct Lease(std::sync::Arc<Job>);
impl Lease {
    #[cfg(test)]
    pub fn ticket(&self, id: uuid::Uuid) -> Ticket {
        Ticket {
            id,
            state: self.0.clone(),
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
impl Ticket {
    pub fn active(&self) -> bool {
        !self
            .state
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn check(&self) -> Result<(), String> {
        if self.active() {
            Ok(())
        } else {
            Err("Image insertion was cancelled before loading finished.".into())
        }
    }
    fn acquire(pool: std::sync::Arc<tokio::sync::Semaphore>) -> Option<(Self, Lease)> {
        let state = std::sync::Arc::new(Job {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            _permit: pool.try_acquire_owned().ok()?,
        });
        Some((
            Self {
                id: uuid::Uuid::new_v4(),
                state: state.clone(),
            },
            Lease(state),
        ))
    }
}
pub(super) fn admit() -> Option<(Ticket, Lease)> {
    use std::sync::Arc;

    let pool = application_capacity!(2);
    #[cfg(test)]
    let pool = TEST_POOL.with(|slot| slot.borrow().clone()).unwrap_or(pool);
    Ticket::acquire(pool)
}

#[cfg(test)]
thread_local! {
    static TEST_POOL: std::cell::RefCell<Option<std::sync::Arc<tokio::sync::Semaphore>>> = const { std::cell::RefCell::new(None) };
}
/// An explicit isolated application fixture uses the same two-slot contract.
/// All Runtime/cards inside that fixture share it. Ordinary test code gets a
/// per-test-thread pool from `application_capacity!`; this guard also asserts
/// that every permit is settled before fixture retirement.
#[cfg(test)]
pub(super) struct TestAdmission {
    previous: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    pool: std::sync::Arc<tokio::sync::Semaphore>,
}
#[cfg(test)]
pub(super) fn test_admission() -> TestAdmission {
    let pool = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
    let previous = TEST_POOL.with(|slot| slot.replace(Some(pool.clone())));
    TestAdmission { previous, pool }
}
#[cfg(test)]
impl Drop for TestAdmission {
    fn drop(&mut self) {
        TEST_POOL.with(|slot| *slot.borrow_mut() = self.previous.take());
        if !std::thread::panicking() {
            assert_eq!(
                self.pool.available_permits(),
                2,
                "fixture must settle every owned staging callback/lease before retirement"
            );
        }
    }
}

/// IDs below are constructed by cagents' provider decoders, not image labels
/// or model prose. Strip only the final canonical block suffix, retaining any
/// delimiters inside the provider's opaque message identity. This groups
/// adjacent image-only user messages without borrowing another turn's text.
pub(super) fn message_key(provider: crate::agent::Provider, item_id: &str) -> Option<&str> {
    let (message, index) = match provider {
        crate::agent::Provider::Codex => item_id.rsplit_once("/input/")?,
        crate::agent::Provider::Claude | crate::agent::Provider::Omp => item_id.rsplit_once(':')?,
        // No native chat decodes their messages yet.
        crate::agent::Provider::Antigravity | crate::agent::Provider::Muse => return None,
    };
    if message.is_empty()
        || index.is_empty()
        || !index.bytes().all(|b| b.is_ascii_digit())
        || (index.len() > 1 && index.starts_with('0'))
        || index.parse::<usize>().is_err()
    {
        return None;
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::widget::text_editor::Edit;
    use std::sync::Arc;
    fn point(line: usize, column: usize) -> Position {
        Position { line, column }
    }
    #[test]
    fn anchor_follows_actual_unicode_multiline_edit_without_following_current_caret() {
        let mut content = Content::<cosmic::Renderer>::with_text("before π\nselected\nafter");
        let mut bookmark = Bookmark::new(Cursor {
            position: point(1, 3),
            selection: None,
        });
        content.move_to(Cursor {
            position: point(0, 0),
            selection: None,
        });
        let change = edit(&mut content, Edit::Paste(Arc::new("🚀\n".into()))).unwrap();
        bookmark.changed(&change);
        assert!(bookmark.valid);
        assert_eq!(bookmark.cursor.position, point(2, 3));
        // Later typing at the exact captured point goes after the incoming
        // image; callbacks completing out of order remain at their own anchor.
        content.move_to(bookmark.cursor);
        let change = edit(&mut content, Edit::Insert('x')).unwrap();
        bookmark.changed(&change);
        assert_eq!(bookmark.cursor.position, point(2, 3));
    }
    #[test]
    fn overlapping_edit_cancels_pending_selection_instead_of_erasing_new_text() {
        let mut content = Content::<cosmic::Renderer>::with_text("alpha beta gamma");
        let mut bookmark = Bookmark::new(Cursor {
            position: point(0, 10),
            selection: Some(point(0, 6)),
        });
        content.move_to(Cursor {
            position: point(0, 8),
            selection: None,
        });
        let change = edit(&mut content, Edit::Insert('!')).unwrap();
        bookmark.changed(&change);
        assert!(!bookmark.valid);
        assert_eq!(content.text(), "alpha be!ta gamma");
    }
    #[test]
    fn pending_selection_refuses_insert_at_left_edge_and_preserves_insert_at_right_edge() {
        for (at, valid) in [(6, false), (10, true)] {
            let mut content = Content::<cosmic::Renderer>::with_text("alpha beta gamma");
            let mut bookmark = Bookmark::new(Cursor {
                position: point(0, 10),
                selection: Some(point(0, 6)),
            });
            content.move_to(Cursor {
                position: point(0, at),
                selection: None,
            });
            let change = edit(&mut content, Edit::Insert('!')).unwrap();
            bookmark.changed(&change);
            assert_eq!(bookmark.valid, valid, "insertion at selection edge {at}");
            if valid {
                content.move_to(bookmark.cursor);
                edit(&mut content, Edit::Paste(Arc::new("[Image 1]".into())));
                assert_eq!(
                    content.text(),
                    "alpha [Image 1]! gamma",
                    "text newly inserted after the selection survives"
                );
            } else {
                assert_eq!(
                    content.text(),
                    "alpha !beta gamma",
                    "left-edge concurrent insertion must not be selected by the late paste"
                );
            }
        }
    }
    #[test]
    fn native_word_and_line_selections_capture_the_actual_replacement() {
        for action in [
            text_editor::Action::SelectWord,
            text_editor::Action::SelectLine,
        ] {
            let mut content = Content::<cosmic::Renderer>::with_text("alpha beta gamma\r\nnext");
            content.move_to(Cursor {
                position: point(0, 8),
                selection: None,
            });
            content.perform(action);
            let bookmark = Bookmark::capture(&content);
            assert!(bookmark.valid);
            let selected = content.selection().unwrap();
            content.move_to(bookmark.cursor);
            let change = edit(&mut content, Edit::Paste(Arc::new("[Image 1]".into()))).unwrap();
            assert_eq!(change.start, bookmark.cursor.selection.unwrap());
            assert_eq!(change.removed.replace("\r\n", "\n"), selected);
            assert!(content.text().contains("[Image 1]"));
        }
    }

    /// A word or line selection's cursor reports the points it was made
    /// between; the editor reports the span it covers, even where the
    /// selected text fits more than one place around those points (#3693).
    #[test]
    fn word_and_line_selections_report_and_edit_the_span_they_cover() {
        let span = |content: &Content<cosmic::Renderer>| content.selection_span();
        let mut content = Content::<cosmic::Renderer>::with_text("ab 中中中\nnext");
        content.move_to(Cursor {
            position: point(0, 6),
            selection: None,
        });
        content.perform(text_editor::Action::SelectWord);
        assert_eq!(content.selection().as_deref(), Some("中中"));
        assert_eq!(content.cursor().position, point(0, 6), "the click point");
        assert_eq!(
            span(&content),
            Cursor {
                position: point(0, 9),
                selection: Some(point(0, 3)),
            }
        );
        // Dragged back over the word before, the caret leads at the start.
        content.perform(text_editor::Action::Select(text_editor::Motion::Left));
        assert_eq!(content.selection().as_deref(), Some("ab 中中"));
        assert_eq!(
            span(&content),
            Cursor {
                position: point(0, 0),
                selection: Some(point(0, 9)),
            }
        );
        let change = edit(&mut content, Edit::Insert('x')).unwrap();
        assert_eq!(change.start, point(0, 0));
        assert_eq!(change.removed, "ab 中中");
        assert_eq!(content.text(), "x中\nnext");

        content.move_to(Cursor {
            position: point(1, 2),
            selection: None,
        });
        content.perform(text_editor::Action::SelectLine);
        assert_eq!(
            span(&content),
            Cursor {
                position: point(1, 4),
                selection: Some(point(1, 0)),
            }
        );
        content.perform(text_editor::Action::ClearSelection);
        assert_eq!(span(&content).selection, None);
    }

    #[test]
    fn repeated_text_and_native_line_boundaries_measure_at_the_original_caret() {
        let mut content = Content::<cosmic::Renderer>::with_text("aaaa\r\nπ\nend");
        content.move_to(Cursor {
            position: point(0, 1),
            selection: None,
        });
        let change = edit(&mut content, Edit::Insert('a')).unwrap();
        assert_eq!(change.start, point(0, 1));
        assert_eq!(change.inserted, "a");
        content.move_to(Cursor {
            position: point(1, 2),
            selection: None,
        });
        let change = edit(&mut content, Edit::Backspace).unwrap();
        assert_eq!(change.start, point(1, 0));
        assert_eq!(change.removed, "π");
        assert_eq!(
            excerpt(&content, 0, content.line_count() - 1).text(),
            content.text()
        );
    }

    #[test]
    fn admission_is_private_to_each_test_thread() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let first = admit();
            let second = admit();
            assert!(first.is_some());
            assert!(second.is_some());
            assert!(sender.send([first.unwrap(), second.unwrap()]).is_ok());
        });
        let other_thread_admissions = receiver.recv().unwrap();
        thread.join().unwrap();

        let admission = admit();
        assert!(admission.is_some());

        drop(admission);
        drop(other_thread_admissions);
    }

    #[test]
    fn cancelled_draft_keeps_admission_until_all_owned_callbacks_finish() {
        let pool = Arc::new(tokio::sync::Semaphore::new(2));
        let (first, first_lease) = Ticket::acquire(pool.clone()).unwrap();
        let (second, second_lease) = Ticket::acquire(pool.clone()).unwrap();
        let queued_buffer_owner = first.clone();
        drop(first_lease);
        drop(second_lease);
        assert!(!first.active());
        assert!(!second.active());
        for _ in 0..100 {
            assert!(Ticket::acquire(pool.clone()).is_none());
        }
        drop(first);
        assert!(
            Ticket::acquire(pool.clone()).is_none(),
            "stalled callback still owns its slot"
        );
        drop(queued_buffer_owner);
        let (replacement, _replacement_lease) = Ticket::acquire(pool.clone()).unwrap();
        assert!(replacement.active());
        assert!(
            Ticket::acquire(pool).is_none(),
            "second cancelled callback still owns its slot"
        );
    }

    #[test]
    fn native_message_keys_strip_only_canonical_final_provider_block_suffixes() {
        use crate::agent::Provider;
        for (provider, text, image, expected) in [
            (
                Provider::Codex,
                "raw/input/nested/input/0",
                "raw/input/nested/input/2",
                "raw/input/nested",
            ),
            (Provider::Claude, "raw:uuid:0", "raw:uuid:2", "raw:uuid"),
            (
                Provider::Omp,
                r#"user:"2026-10-22T00:00:00Z":0"#,
                r#"user:"2026-10-22T00:00:00Z":2"#,
                r#"user:"2026-10-22T00:00:00Z""#,
            ),
        ] {
            assert_eq!(message_key(provider, text), Some(expected));
            assert_eq!(message_key(provider, image), Some(expected));
        }
        for provider in [Provider::Codex, Provider::Claude, Provider::Omp] {
            for bad in [
                "unknown",
                ":",
                "/input/",
                "raw/input/01",
                "raw:01",
                "raw:184467440737095516160",
            ] {
                assert!(message_key(provider, bad).is_none());
            }
        }
    }
}
