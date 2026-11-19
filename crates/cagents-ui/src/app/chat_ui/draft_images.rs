//! A draft's inline `[Image N]` reference and its attachment are one object.
//!
//! The text decides. An image whose reference leaves the draft, by any edit,
//! leaves the attachments with it and waits here, so that the edit that
//! brings the reference back (Undo, Redo, or pasting text cut from the
//! draft) brings the same receipt back, at the place in the tray it left.
//! Nothing is renumbered: the label is the receipt's own.
//!
//! The composer's own undo history lives here too, since an image's removal
//! and return are text edits it replays.
use crate::chat::Attachment;
use crate::editor_history::{EditBuffer, EditKind};
use cosmic::widget::text_editor::{self, Content, Cursor, Position};
use std::collections::HashSet;
use std::sync::Arc;

/// An image whose reference the draft no longer holds.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Detached {
    /// Where it stood among the attachments when it left.
    index: usize,
    attachment: Attachment,
}

/// Most detached images one draft remembers: as many as the undo history
/// has steps, since each step can take one reference out and undoing it must
/// find its image. Each is a receipt, not pixels.
const MAX_DETACHED: usize = crate::editor_history::MAX_TRANSACTIONS;

/// The longest `[Image N]` token [`tokens`] looks for.
const MAX_TOKEN: usize = 32;

/// Every short bracketed span in `text`, read in one pass, so that asking
/// whether the draft holds a reference costs a lookup rather than a search
/// of the whole draft per image. A token has no bracket inside it, so this
/// finds every one a substring search would.
fn tokens(text: &str) -> HashSet<&str> {
    let bytes = text.as_bytes();
    let mut found = HashSet::new();
    for (start, _) in text.match_indices('[') {
        let reach = bytes.len().min(start + MAX_TOKEN);
        if let Some(end) = bytes[start + 1..reach].iter().position(|&b| b == b']') {
            found.insert(&text[start..start + end + 2]);
        }
    }
    found
}

/// The ids of `attachments` whose reference `text` holds.
pub(super) fn inline(text: &str, attachments: &[Attachment]) -> HashSet<uuid::Uuid> {
    let present = tokens(text);
    attachments
        .iter()
        .filter_map(|attachment| attachment.reference.as_ref())
        .filter(|reference| present.contains(reference.token().as_str()))
        .map(|reference| reference.id)
        .collect()
}

/// Whether anything can move between `attachments` and `detached`.
pub(super) fn tracked(attachments: &[Attachment], detached: &[Detached]) -> bool {
    !detached.is_empty()
        || attachments
            .iter()
            .any(|attachment| attachment.reference.is_some())
}

/// Brings the attachments in line with `text` after an edit. An image whose
/// reference was in the draft before the edit (`before`) and is not now
/// leaves; a detached image whose reference is back returns. An image whose
/// reference was never in the draft, such as an image-only message brought
/// back from history, stays: no edit removed anything of it. Answers whether
/// the attachments changed.
pub(super) fn reconcile(
    text: &str,
    before: &HashSet<uuid::Uuid>,
    attachments: &mut Vec<Attachment>,
    detached: &mut Vec<Detached>,
) -> bool {
    let present = tokens(text);
    let holds = |attachment: &Attachment| {
        attachment
            .reference
            .as_ref()
            .is_some_and(|reference| present.contains(reference.token().as_str()))
    };
    let mut changed = false;
    let mut index = 0;
    // Where each leaving image stood before this edit, so that images that
    // leave together return in their order.
    let mut left = 0;
    while index < attachments.len() {
        let leaves = attachments[index]
            .reference
            .as_ref()
            .is_some_and(|reference| before.contains(&reference.id))
            && !holds(&attachments[index]);
        if leaves {
            let attachment = attachments.remove(index);
            detached.push(Detached {
                index: index + left,
                attachment,
            });
            left += 1;
            changed = true;
        } else {
            index += 1;
        }
    }
    // The most recently detached return first, so undoing several removals
    // puts each back where it was.
    let mut returning = Vec::new();
    detached.retain(|entry| {
        let back = holds(&entry.attachment);
        if back {
            returning.push(entry.clone());
        }
        !back
    });
    returning.sort_by_key(|entry| entry.index);
    for entry in returning {
        let id = entry
            .attachment
            .reference
            .as_ref()
            .map(|reference| reference.id);
        if attachments
            .iter()
            .any(|attachment| attachment.reference.as_ref().map(|r| r.id) == id)
        {
            continue;
        }
        attachments.insert(entry.index.min(attachments.len()), entry.attachment);
        changed = true;
    }
    if detached.len() > MAX_DETACHED {
        detached.drain(..detached.len() - MAX_DETACHED);
    }
    changed
}

/// Forgets detached copies of images the draft holds again by other means.
pub(super) fn forget(detached: &mut Vec<Detached>, attachments: &[Attachment]) {
    detached.retain(|entry| {
        entry.attachment.reference.as_ref().is_none_or(|detached| {
            !attachments.iter().any(|attachment| {
                attachment
                    .reference
                    .as_ref()
                    .is_some_and(|reference| reference.id == detached.id)
            })
        })
    });
}

/// How an edit coalesces in the composer's undo history.
pub(super) fn kind(edit: &text_editor::Edit, selected: bool) -> EditKind {
    match edit {
        _ if selected => EditKind::Other,
        text_editor::Edit::Insert(_) | text_editor::Edit::Enter => EditKind::Insert,
        text_editor::Edit::Backspace => EditKind::DeleteBackward,
        text_editor::Edit::Delete => EditKind::DeleteForward,
        _ => EditKind::Other,
    }
}

/// The composer as the undo history rewinds it: each replacement is a native
/// paste over the exact span, verified before it is made.
pub(super) struct Buffer<'a>(pub &'a mut Content<cosmic::Renderer>);

impl EditBuffer for Buffer<'_> {
    fn replace(
        &mut self,
        start: Position,
        expected: &str,
        replacement: &str,
        replacement_breaks: &[usize],
    ) -> bool {
        // An upper bound on the lines `expected` spans; the excerpt stops at
        // the buffer's end.
        let lines = expected.matches(['\n', '\r']).count();
        let view = super::attachment_draft::excerpt(
            self.0,
            start.line,
            start.line.saturating_add(lines),
            false,
        );
        let Some(from) = view.offset(start) else {
            return false;
        };
        let to = from + expected.len();
        if view.text().get(from..to) != Some(expected) {
            return false;
        }
        super::attachment_draft::move_to(
            self.0,
            Cursor {
                position: view.position(to),
                selection: (!expected.is_empty()).then_some(start),
            },
        );
        let paste = |content: &mut Content<cosmic::Renderer>, text: &str| {
            content.perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new(text.to_owned()),
            )));
        };
        if replacement.is_empty() {
            if !expected.is_empty() {
                paste(self.0, "");
            }
        } else if crate::editor_history::canonical_breaks(replacement, replacement_breaks) {
            paste(self.0, replacement);
        } else {
            // One paste of these bytes would read their line endings again
            // and could join a recorded LF and CR into one break; each line's
            // fragment goes in on its own, as the repository editor does.
            let mut previous = 0;
            for end in replacement_breaks
                .iter()
                .copied()
                .chain(std::iter::once(replacement.len()))
            {
                if end > previous {
                    paste(self.0, &replacement[previous..end]);
                }
                previous = end;
            }
        }
        true
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        super::attachment_draft::move_to(self.0, cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::attachment_assets;

    fn ids(attachments: &[Attachment]) -> Vec<u64> {
        attachments
            .iter()
            .map(|attachment| attachment.reference.as_ref().unwrap().number)
            .collect()
    }

    #[test]
    fn a_reference_leaving_detaches_its_image_and_returning_restores_its_place() {
        attachment_assets::with_test_image(|first| {
            let second = attachment_assets::test_image("second.png");
            let third = attachment_assets::test_image("third.png");
            let all = vec![first.clone(), second.clone(), third.clone()];
            let token = |a: &Attachment| a.reference.as_ref().unwrap().token();
            let full = format!("{} {} {}", token(&first), token(&second), token(&third));
            let mut attachments = all.clone();
            let mut detached = Vec::new();
            let before = inline(&full, &attachments);
            let without = format!("{} {}", token(&first), token(&third));
            assert!(reconcile(
                &without,
                &before,
                &mut attachments,
                &mut detached
            ));
            assert_eq!(attachments, [first.clone(), third.clone()]);
            let before = inline(&without, &attachments);
            assert!(reconcile(&full, &before, &mut attachments, &mut detached));
            assert_eq!(ids(&attachments), ids(&all), "same receipts, same labels");
            assert!(detached.is_empty());
        });
    }

    #[test]
    fn a_long_draft_with_many_images_detaches_only_the_reference_it_lost() {
        attachment_assets::with_test_image(|first| {
            let mut all = vec![first];
            all.extend((2..=8).map(|n| attachment_assets::test_image(&format!("{n}.png"))));
            let token = |a: &Attachment| a.reference.as_ref().unwrap().token();
            let paragraph = "A long line of prose that surrounds each image. ".repeat(500);
            let draft = |skip: Option<usize>| {
                all.iter()
                    .enumerate()
                    .filter(|(index, _)| Some(*index) != skip)
                    .map(|(_, image)| format!("{paragraph}{}\n", token(image)))
                    .collect::<String>()
            };
            let full = draft(None);
            assert!(full.len() > 190_000, "the draft is long");
            let mut attachments = all.clone();
            let mut detached = Vec::new();

            let before = inline(&full, &attachments);
            let typed = format!("{full}and one more sentence");
            assert!(!reconcile(&typed, &before, &mut attachments, &mut detached));
            assert_eq!(attachments, all, "an edit elsewhere keeps every image");

            let without = draft(Some(4));
            assert!(reconcile(
                &without,
                &before,
                &mut attachments,
                &mut detached
            ));
            let mut expected = all.clone();
            let removed = expected.remove(4);
            assert_eq!(attachments, expected);
            assert_eq!(detached.len(), 1);
            assert_eq!(detached[0].attachment, removed);
        });
    }

    #[test]
    fn images_that_leave_together_return_in_their_order() {
        attachment_assets::with_test_image(|first| {
            let second = attachment_assets::test_image("second.png");
            let third = attachment_assets::test_image("third.png");
            let all = vec![first.clone(), second.clone(), third.clone()];
            let token = |a: &Attachment| a.reference.as_ref().unwrap().token();
            let full = format!("{} {} {}", token(&first), token(&second), token(&third));
            let mut attachments = all.clone();
            let mut detached = Vec::new();
            // One cut takes the first two.
            let before = inline(&full, &attachments);
            let cut = token(&third);
            assert!(reconcile(&cut, &before, &mut attachments, &mut detached));
            assert_eq!(attachments, [third.clone()]);
            let before = inline(&cut, &attachments);
            assert!(reconcile(&full, &before, &mut attachments, &mut detached));
            assert_eq!(attachments, all);
        });
    }

    #[test]
    fn the_token_scan_finds_what_a_search_would() {
        let text = "[[Image 1]] [Image 2 [Image 3]] é[Image 4] [Image 5";
        let found = tokens(text);
        for n in 1..=5 {
            let token = format!("[Image {n}]");
            assert_eq!(
                found.contains(token.as_str()),
                text.contains(&token),
                "{token}"
            );
        }
    }

    #[test]
    fn an_image_whose_reference_was_never_in_the_draft_stays() {
        attachment_assets::with_test_image(|image| {
            let mut attachments = vec![image.clone(), Attachment::file("/notes.txt")];
            let mut detached = Vec::new();
            let before = inline("", &attachments);
            assert!(!reconcile(
                "typed",
                &before,
                &mut attachments,
                &mut detached
            ));
            assert_eq!(attachments.len(), 2);
        });
    }

    #[test]
    fn the_buffer_replays_a_native_edit_only_over_the_text_it_expects() {
        let mut content = Content::<cosmic::Renderer>::with_text("one [Image 7]\ntwo");
        let mut buffer = Buffer(&mut content);
        assert!(!buffer.replace(Position { line: 0, column: 4 }, "[Image 8]", "", &[]));
        assert!(buffer.replace(Position { line: 0, column: 4 }, "[Image 7]\nt", "x", &[]));
        assert_eq!(content.text(), "one xwo");
        let mut buffer = Buffer(&mut content);
        assert!(buffer.replace(Position { line: 0, column: 4 }, "", "[Image 7]\nt", &[]));
        assert_eq!(content.text(), "one [Image 7]\ntxwo");
    }
}
