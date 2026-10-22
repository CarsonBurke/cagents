//! Pending async clipboard/chooser anchors follow measured native edits. They
//! do not mutate the editor's undo history or infer changes from text diffs.
use crate::{
    editor_history::TextChange,
    editor_widget::{Cursor, Position},
};

#[derive(Clone, Copy)]
pub(super) struct Bookmark {
    pub cursor: Cursor,
    pub valid: bool,
}
impl Bookmark {
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

/// IDs below are constructed by cagents' provider decoders, not image labels
/// or model prose. Strip only the final canonical block suffix, retaining any
/// delimiters inside the provider's opaque message identity. This groups
/// adjacent image-only user messages without borrowing another turn's text.
pub(super) fn message_key(provider: crate::agent::Provider, item_id: &str) -> Option<&str> {
    let (message, index) = match provider {
        crate::agent::Provider::Codex => item_id.rsplit_once("/input/")?,
        crate::agent::Provider::Claude | crate::agent::Provider::Omp => item_id.rsplit_once(':')?,
    };
    if message.is_empty()
        || index.is_empty()
        || !index.bytes().all(|b| b.is_ascii_digit())
        || index.parse::<usize>().ok()?.to_string() != index
    {
        return None;
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_widget::{Content, Edit};
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
        let edit = content
            .perform_edit(Edit::Paste(Arc::new("🚀\n".into())))
            .unwrap();
        bookmark.changed(&edit.change);
        assert!(bookmark.valid);
        assert_eq!(bookmark.cursor.position, point(2, 3));
        // Later typing at the exact captured point goes after the incoming
        // image; callbacks completing out of order remain at their own anchor.
        content.move_to(bookmark.cursor);
        let edit = content.perform_edit(Edit::Insert('x')).unwrap();
        bookmark.changed(&edit.change);
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
        let edit = content.perform_edit(Edit::Insert('!')).unwrap();
        bookmark.changed(&edit.change);
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
            let edit = content.perform_edit(Edit::Insert('!')).unwrap();
            bookmark.changed(&edit.change);
            assert_eq!(bookmark.valid, valid, "insertion at selection edge {at}");
            if valid {
                content.move_to(bookmark.cursor);
                content
                    .perform_edit(Edit::Paste(Arc::new("[Image 1]".into())))
                    .unwrap();
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
