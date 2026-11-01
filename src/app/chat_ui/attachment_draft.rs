//! Pending async clipboard/chooser anchors follow measured native edits. They
//! do not mutate the editor's undo history. Only affected native lines are measured.
use crate::editor_history::{self, Excerpt, TextChange};
use cosmic::widget::text_editor::{self, Content, Cursor, Position};

/// Native lines preserve boundaries which a whole-string line parser can merge.
pub(super) fn excerpt(
    content: &Content<cosmic::Renderer>,
    first: usize,
    last: usize,
    normalized: bool,
) -> Excerpt {
    let count = content.line_count();
    let mut result = Excerpt::new(first);
    for index in first..=last.min(count.saturating_sub(1)) {
        if let Some(line) = content.line(index) {
            let ending = if index + 1 == count {
                ""
            } else if normalized {
                "\n"
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

fn selected_span(content: &Content<cosmic::Renderer>, view: &Excerpt) -> Option<Cursor> {
    let cursor = content.cursor();
    let Some(anchor) = cursor.selection else {
        return Some(cursor);
    };
    let Some(selected) = content.selection().filter(|s| !s.is_empty()) else {
        return Some(Cursor {
            position: cursor.position,
            selection: None,
        });
    };
    let locate = |view: &Excerpt| {
        let caret = view.offset(cursor.position)?;
        let anchor = view.offset(anchor)?;
        let (low, high) = (caret.min(anchor), caret.max(anchor));
        let mut found = None;
        // Word/line selections expose their original anchor, not the expanded
        // span. Accept one exact containing match; ambiguity cancels late IO.
        let mut from = high.saturating_sub(selected.len());
        while !view.text().is_char_boundary(from) {
            from -= 1;
        }
        while from <= low {
            let Some(at) = view.text().get(from..)?.find(&selected).map(|at| at + from) else {
                break;
            };
            if at > low {
                break;
            }
            let end = at + selected.len();
            if end >= high {
                if found.is_some() {
                    return None;
                }
                found = Some((view.position(at), view.position(end)));
            }
            from = at + view.text().get(at..)?.chars().next()?.len_utf8();
        }
        let (start, end) = found?;
        Some(if caret < anchor {
            Cursor {
                position: start,
                selection: Some(end),
            }
        } else {
            Cursor {
                position: end,
                selection: Some(start),
            }
        })
    };
    locate(view).or_else(|| {
        let first = view.position(0).line;
        let last = cursor.position.line.max(anchor.line).saturating_add(1);
        locate(&excerpt(content, first, last, true))
    })
}

/// Perform the real composer edit, measuring its local replacement only when
/// an attachment needs mapping. This adapter leaves native editing and Undo alone.
pub(super) fn edit(
    content: &mut Content<cosmic::Renderer>,
    edit: text_editor::Edit,
) -> Result<Option<TextChange>, ()> {
    let cursor = content.cursor();
    let anchor = cursor.selection.unwrap_or(cursor.position);
    let count = content.line_count();
    let first = cursor.position.line.min(anchor.line).saturating_sub(1);
    let last = cursor
        .position
        .line
        .max(anchor.line)
        .saturating_add(1)
        .min(count.saturating_sub(1));
    let before = excerpt(content, first, last, false);
    let selection = selected_span(content, &before);
    let changed_from = selection.and_then(|selection| {
        let caret = before.offset(selection.position)?;
        Some(match selection.selection {
            Some(anchor) => caret.min(before.offset(anchor)?),
            None if matches!(edit, text_editor::Edit::Backspace) => before
                .text()
                .get(..caret)?
                .chars()
                .next_back()
                .map_or(caret, |c| caret - c.len_utf8()),
            None => caret,
        })
    });
    content.perform(text_editor::Action::Edit(edit));
    let changed_from = changed_from.ok_or(())?;
    let after_count = content.line_count();
    let after_last = last
        .saturating_add(after_count)
        .saturating_sub(count)
        .max(first)
        .min(after_count.saturating_sub(1));
    let after = excerpt(content, first, after_last, false);
    Ok(editor_history::replacement(&before, &after, changed_from))
}

#[derive(Clone, Copy)]
pub(super) struct Bookmark {
    pub cursor: Cursor,
    pub valid: bool,
}
impl Bookmark {
    pub fn capture(content: &Content) -> Self {
        let cursor = content.cursor();
        let anchor = cursor.selection.unwrap_or(cursor.position);
        let view = excerpt(
            content,
            cursor.position.line.min(anchor.line).saturating_sub(1),
            cursor.position.line.max(anchor.line).saturating_add(1),
            false,
        );
        match selected_span(content, &view) {
            Some(cursor) => Self::new(cursor),
            None => Self {
                cursor,
                valid: false,
            },
        }
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
pub(super) struct Ticket {
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
    static POOL: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let pool = POOL
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone();
    #[cfg(test)]
    let pool = TEST_POOL.with(|slot| slot.borrow().clone()).unwrap_or(pool);
    Ticket::acquire(pool)
}

#[cfg(test)]
thread_local! {
    static TEST_POOL: std::cell::RefCell<Option<std::sync::Arc<tokio::sync::Semaphore>>> = const { std::cell::RefCell::new(None) };
}
/// An explicit isolated application fixture uses the same two-slot contract.
/// All Runtime/cards inside that fixture share it; ordinary test code still
/// uses production's process-global pool unless this guard is installed.
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
        crate::agent::Provider::Claude
        | crate::agent::Provider::Antigravity
        | crate::agent::Provider::Omp => item_id.rsplit_once(':')?,
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
        let change = edit(&mut content, Edit::Paste(Arc::new("🚀\n".into())))
            .unwrap()
            .unwrap();
        bookmark.changed(&change);
        assert!(bookmark.valid);
        assert_eq!(bookmark.cursor.position, point(2, 3));
        // Later typing at the exact captured point goes after the incoming
        // image; callbacks completing out of order remain at their own anchor.
        content.move_to(bookmark.cursor);
        let change = edit(&mut content, Edit::Insert('x')).unwrap().unwrap();
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
        let change = edit(&mut content, Edit::Insert('!')).unwrap().unwrap();
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
            let change = edit(&mut content, Edit::Insert('!')).unwrap().unwrap();
            bookmark.changed(&change);
            assert_eq!(bookmark.valid, valid, "insertion at selection edge {at}");
            if valid {
                content.move_to(bookmark.cursor);
                edit(&mut content, Edit::Paste(Arc::new("[Image 1]".into()))).unwrap();
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
            let change = edit(&mut content, Edit::Paste(Arc::new("[Image 1]".into())))
                .unwrap()
                .unwrap();
            assert_eq!(change.start, bookmark.cursor.selection.unwrap());
            assert_eq!(change.removed.replace("\r\n", "\n"), selected);
            assert!(content.text().contains("[Image 1]"));
        }
    }

    #[test]
    fn repeated_text_and_native_line_boundaries_measure_at_the_original_caret() {
        let mut content = Content::<cosmic::Renderer>::with_text("aaaa\r\nπ\nend");
        content.move_to(Cursor {
            position: point(0, 1),
            selection: None,
        });
        let change = edit(&mut content, Edit::Insert('a')).unwrap().unwrap();
        assert_eq!(change.start, point(0, 1));
        assert_eq!(change.inserted, "a");
        content.move_to(Cursor {
            position: point(1, 2),
            selection: None,
        });
        let change = edit(&mut content, Edit::Backspace).unwrap().unwrap();
        assert_eq!(change.start, point(1, 0));
        assert_eq!(change.removed, "π");
        assert_eq!(
            excerpt(&content, 0, content.line_count() - 1, false).text(),
            content.text()
        );
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
