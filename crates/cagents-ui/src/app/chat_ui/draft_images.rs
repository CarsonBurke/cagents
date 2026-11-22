//! A draft's inline `[Image N]` reference and its attachment are one object.
//!
//! The text decides. An image whose reference leaves the draft, by any edit,
//! leaves the attachments with it and waits here, so that the edit that
//! brings the reference back (Undo, Redo, or pasting text cut from the
//! draft) brings the same receipt back, at the place in the tray it left.
//! Nothing is renumbered: the label is the receipt's own.
//!
//! A reference is one piece of the draft, not nine characters: the caret
//! never stops inside it, a selection that touches it takes all of it, and
//! Backspace or Delete beside it removes it whole. No edit can leave half of
//! one, such as `[Imag`, behind. Only the references of images the draft
//! holds are pieces; other bracketed text is the reader's own.
//!
//! The composer's own undo history lives here too, since an image's removal
//! and return are text edits it replays.
use crate::chat::Attachment;
use crate::editor_history::{EditBuffer, EditKind};
use cosmic::iced::advanced::text::editor::Direction;
use cosmic::widget::text_editor::{self, Content, Cursor, Motion, Position};
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

/// An image whose reference the draft no longer holds.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Detached {
    /// Where it stood among the attachments the moment it left, after any
    /// image that left before it.
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
    bracketed(text).map(|span| &text[span]).collect()
}

/// The byte span of each short bracketed run in `text`, from its `[` to the
/// first `]` after it.
fn bracketed(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let bytes = text.as_bytes();
    text.match_indices('[').filter_map(move |(start, _)| {
        let reach = bytes.len().min(start + MAX_TOKEN);
        bytes[start + 1..reach]
            .iter()
            .position(|&b| b == b']')
            .map(|end| start..start + end + 2)
    })
}

/// The tokens of the images `attachments` holds: the text that is one of
/// the draft's pieces rather than the reader's own words.
pub(super) fn live_tokens(attachments: &[Attachment]) -> HashSet<String> {
    attachments
        .iter()
        .filter_map(|attachment| attachment.reference.as_ref())
        .map(|reference| reference.token())
        .collect()
}

/// Where in `line` the references `live` names stand.
fn spans<'a>(line: &'a str, live: &'a HashSet<String>) -> impl Iterator<Item = Range<usize>> + 'a {
    bracketed(line).filter(|span| live.contains(&line[span.clone()]))
}

/// Which side of a reference a caret that landed inside it goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Toward {
    Start,
    End,
    /// Whichever side is closer, as for a click or a move between lines.
    Nearer,
    /// Away from the selection's other end, so the selection takes the
    /// reference in; a caret with no selection goes to the nearer side.
    Outward,
}

impl Toward {
    /// The side a motion was heading for: across a reference, never back.
    pub(super) fn of(motion: Motion) -> Self {
        match motion {
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => Self::Nearer,
            _ => match motion.direction() {
                Direction::Left => Self::Start,
                Direction::Right => Self::End,
            },
        }
    }
}

/// `column` of `line`, moved out of the reference it falls inside, if any.
fn outside(line: &str, live: &HashSet<String>, column: usize, toward: Toward) -> usize {
    spans(line, live)
        .find(|span| span.start < column && column < span.end)
        .map_or(column, |span| match toward {
            Toward::Start => span.start,
            Toward::End => span.end,
            Toward::Nearer | Toward::Outward if column - span.start <= span.end - column => {
                span.start
            }
            Toward::Nearer | Toward::Outward => span.end,
        })
}

fn moved_out(
    content: &Content<cosmic::Renderer>,
    live: &HashSet<String>,
    position: Position,
    toward: Toward,
) -> Position {
    Position {
        column: content.line(position.line).map_or(position.column, |line| {
            outside(&line.text, live, position.column, toward)
        }),
        ..position
    }
}

/// The cursor the composer must take so that neither of its ends is inside
/// one of the references `live` names, or `None` when it already is.
///
/// The caret is the end the reader moves, and goes `toward` a side: a
/// selection shrinking back over a reference lets go of all of it, one
/// growing over it takes all of it. The anchor, which the reader isn't
/// moving, only ever grows the selection.
pub(super) fn whole(
    content: &Content<cosmic::Renderer>,
    live: &HashSet<String>,
    toward: Toward,
) -> Option<Cursor> {
    let cursor = content.cursor();
    let selecting = content
        .selection()
        .is_some_and(|selection| !selection.is_empty());
    if !selecting {
        let position = moved_out(content, live, cursor.position, toward);
        return (position != cursor.position).then_some(Cursor {
            position,
            selection: None,
        });
    }
    // A word or line selection reports the place it began rather than its
    // ends; this reads the span it actually covers. A plain selection's
    // cursor is its span, so it stands in when that read is ambiguous.
    let span = super::attachment_draft::selection_span(content).unwrap_or(cursor);
    let (caret, anchor) = (span.position, span.selection?);
    let forward = (anchor.line, anchor.column) <= (caret.line, caret.column);
    let (away, back) = if forward {
        (Toward::End, Toward::Start)
    } else {
        (Toward::Start, Toward::End)
    };
    let moved = Cursor {
        position: moved_out(
            content,
            live,
            caret,
            if toward == Toward::Outward {
                away
            } else {
                toward
            },
        ),
        selection: Some(moved_out(content, live, anchor, back)),
    };
    (moved != span).then_some(moved)
}

/// The selection `edit` must delete instead, when it is Backspace just after
/// a reference or Delete just before one: the whole reference, not its
/// bracket.
pub(super) fn deleted_whole(
    content: &Content<cosmic::Renderer>,
    live: &HashSet<String>,
    edit: &text_editor::Edit,
) -> Option<Cursor> {
    let backward = match edit {
        text_editor::Edit::Backspace => true,
        text_editor::Edit::Delete => false,
        _ => return None,
    };
    if content
        .selection()
        .is_some_and(|selection| !selection.is_empty())
    {
        return None;
    }
    let caret = content.cursor().position;
    let line = content.line(caret.line)?;
    let span = spans(&line.text, live).find(|span| {
        if backward {
            span.end == caret.column
        } else {
            span.start == caret.column
        }
    })?;
    let at = |column| Position { column, ..caret };
    Some(Cursor {
        position: at(span.end),
        selection: Some(at(span.start)),
    })
}

/// Text the reader copied or cut from a draft, with the images whose
/// references it holds. Pasted into another draft, it brings those images
/// along, so the references there name the same pictures rather than none.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Copied {
    text: String,
    images: Vec<Attachment>,
}

impl Copied {
    /// What copying `text` out of a draft holding `attachments` takes along;
    /// `None` when it names none of them.
    pub(super) fn of(text: String, attachments: &[Attachment]) -> Option<Self> {
        let present = tokens(&text);
        let images: Vec<_> = attachments
            .iter()
            .filter(|attachment| {
                attachment
                    .reference
                    .as_ref()
                    .is_some_and(|reference| present.contains(reference.token().as_str()))
            })
            .cloned()
            .collect();
        (!images.is_empty()).then_some(Self { text, images })
    }

    /// The images a paste of `pasted` brings: this copy's, when the paste is
    /// this copy. The clipboard may since hold text from anywhere else,
    /// whose `[Image N]` names nothing this draft can vouch for.
    pub(super) fn carried(&self, pasted: &str) -> &[Attachment] {
        if pasted == self.text {
            &self.images
        } else {
            &[]
        }
    }
}

/// The ids of `attachments` whose reference `text` holds.
pub(super) fn inline<'a>(
    text: &str,
    attachments: impl IntoIterator<Item = &'a Attachment>,
) -> HashSet<uuid::Uuid> {
    let present = tokens(text);
    attachments
        .into_iter()
        .filter_map(|attachment| attachment.reference.as_ref())
        .filter(|reference| present.contains(reference.token().as_str()))
        .map(|reference| reference.id)
        .collect()
}

/// The ids of the draft's images, held or `detached`, whose reference
/// `text` holds.
pub(super) fn named(
    text: &str,
    attachments: &[Attachment],
    detached: &[Detached],
) -> HashSet<uuid::Uuid> {
    inline(
        text,
        attachments
            .iter()
            .chain(detached.iter().map(|entry| &entry.attachment)),
    )
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
/// back from history, stays: no edit removed anything of it. No image comes
/// back past `capacity` attachments; one that would waits, detached, while
/// its reference stays as text, and returns once there is room. `before`
/// also holds the detached images whose references the text held, so a
/// waiting image is reported only when its reference first comes back.
pub(super) fn reconcile(
    text: &str,
    before: &HashSet<uuid::Uuid>,
    attachments: &mut Vec<Attachment>,
    detached: &mut Vec<Detached>,
    capacity: usize,
) -> Reconciled {
    let present = tokens(text);
    let holds = |attachment: &Attachment| {
        attachment
            .reference
            .as_ref()
            .is_some_and(|reference| present.contains(reference.token().as_str()))
    };
    let mut changed = false;
    let mut index = 0;
    while index < attachments.len() {
        let leaves = attachments[index]
            .reference
            .as_ref()
            .is_some_and(|reference| before.contains(&reference.id))
            && !holds(&attachments[index]);
        if leaves {
            let attachment = attachments.remove(index);
            detached.push(Detached { index, attachment });
            changed = true;
        } else {
            index += 1;
        }
    }
    // Each index was read after the images detached before it had gone, so
    // putting them back newest first undoes the removals exactly, whether
    // they left in one edit or over several an undo step joins.
    let mut returning = Vec::new();
    detached.retain(|entry| {
        let back = holds(&entry.attachment);
        if back {
            returning.push(entry.clone());
        }
        !back
    });
    let mut refused = Vec::new();
    for entry in returning.into_iter().rev() {
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
        if attachments.len() >= capacity {
            // Only a reference this edit brought back is news; one already
            // in the text before it was refused, and said so, then.
            let returned = id.is_some_and(|id| !before.contains(&id));
            refused.push((entry, returned));
            continue;
        }
        attachments.insert(entry.index.min(attachments.len()), entry.attachment);
        changed = true;
    }
    let refused = refused
        .into_iter()
        .rev()
        .filter_map(|(entry, returned)| {
            let label = returned.then(|| entry.attachment.label());
            detached.push(entry);
            label
        })
        .collect();
    if detached.len() > MAX_DETACHED {
        detached.drain(..detached.len() - MAX_DETACHED);
    }
    Reconciled { changed, refused }
}

/// What [`reconcile`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Reconciled {
    /// Whether the attachments changed.
    pub changed: bool,
    /// The labels of images whose references this edit brought back while
    /// the draft was full, in the order they were detached.
    pub refused: Vec<String>,
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
            assert!(
                reconcile(
                    &without,
                    &before,
                    &mut attachments,
                    &mut detached,
                    usize::MAX
                )
                .changed
            );
            assert_eq!(attachments, [first.clone(), third.clone()]);
            let before = inline(&without, &attachments);
            assert!(reconcile(&full, &before, &mut attachments, &mut detached, usize::MAX).changed);
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
            assert!(
                !reconcile(&typed, &before, &mut attachments, &mut detached, usize::MAX).changed
            );
            assert_eq!(attachments, all, "an edit elsewhere keeps every image");

            let without = draft(Some(4));
            assert!(
                reconcile(
                    &without,
                    &before,
                    &mut attachments,
                    &mut detached,
                    usize::MAX
                )
                .changed
            );
            let mut expected = all.clone();
            let removed = expected.remove(4);
            assert_eq!(attachments, expected);
            assert_eq!(detached.len(), 1);
            assert_eq!(detached[0].attachment, removed);
        });
    }

    #[test]
    fn a_reference_brought_back_to_a_full_draft_waits_for_room() {
        attachment_assets::with_test_image(|first| {
            let second = attachment_assets::test_image("second.png");
            let third = attachment_assets::test_image("third.png");
            let token = |a: &Attachment| a.reference.as_ref().unwrap().token();
            let mut attachments = vec![first.clone(), second.clone()];
            let mut detached = Vec::new();
            let full = format!("{} {}", token(&first), token(&second));
            let cut = token(&first);
            let reconciled =
                |text: &str, before: &str, attachments: &mut Vec<_>, detached: &mut Vec<_>| {
                    let before = named(before, attachments, detached);
                    reconcile(text, &before, attachments, detached, 2)
                };
            assert!(reconciled(&cut, &full, &mut attachments, &mut detached).changed);
            // A third image takes the freed place, at the provider's limit.
            attachments.push(third.clone());

            // Pasting the reference back leaves it text and says so once.
            let back = format!("{full} {}", token(&third));
            let pasted = reconciled(&back, &cut, &mut attachments, &mut detached);
            assert_eq!(
                pasted,
                Reconciled {
                    changed: false,
                    refused: vec![second.label()],
                }
            );
            assert_eq!(attachments, [first.clone(), third.clone()]);
            let typed = format!("{back}.");
            let later = reconciled(&typed, &back, &mut attachments, &mut detached);
            assert_eq!(
                later,
                Reconciled::default(),
                "a later edit says nothing new"
            );

            // Once a place frees up, the waiting image returns.
            attachments.pop();
            assert!(reconciled(&typed, &typed, &mut attachments, &mut detached).changed);
            assert_eq!(attachments, [first.clone(), second.clone()]);
            assert!(detached.is_empty());
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
            assert!(reconcile(&cut, &before, &mut attachments, &mut detached, usize::MAX).changed);
            assert_eq!(attachments, [third.clone()]);
            let before = inline(&cut, &attachments);
            assert!(reconcile(&full, &before, &mut attachments, &mut detached, usize::MAX).changed);
            assert_eq!(attachments, all);
        });
    }

    #[test]
    fn images_that_leave_over_several_edits_return_together_in_their_order() {
        attachment_assets::with_test_image(|first| {
            let second = attachment_assets::test_image("second.png");
            let third = attachment_assets::test_image("third.png");
            let all = vec![first.clone(), second.clone(), third.clone()];
            let token = |a: &Attachment| a.reference.as_ref().unwrap().token();
            let full = format!("{} {} {}", token(&first), token(&second), token(&third));
            // Each removal is its own edit, and one undo step brings every
            // reference back at once: forward Delete held at the start takes
            // the first two from the head of the tray; the other pair leaves
            // from both ends.
            for gone in [[&first, &second], [&first, &third]] {
                let mut attachments = all.clone();
                let mut detached = Vec::new();
                let mut text = full.clone();
                for image in gone {
                    let before = inline(&text, &attachments);
                    text = text.replacen(&token(image), "", 1);
                    assert!(
                        reconcile(&text, &before, &mut attachments, &mut detached, usize::MAX)
                            .changed
                    );
                }
                assert_eq!(attachments.len(), 1);
                let before = inline(&text, &attachments);
                assert!(
                    reconcile(&full, &before, &mut attachments, &mut detached, usize::MAX).changed
                );
                assert_eq!(attachments, all);
                assert!(detached.is_empty());
            }
        });
    }

    #[test]
    fn a_caret_inside_a_reference_goes_to_the_side_it_was_heading_for() {
        let live = HashSet::from(["[Image 2]".to_owned()]);
        let line = "[Image 1] é[Image 2]x";
        let start = line.find("[Image 2]").unwrap();
        let end = start + "[Image 2]".len();
        for column in start + 1..end {
            assert_eq!(outside(line, &live, column, Toward::Start), start);
            assert_eq!(outside(line, &live, column, Toward::End), end);
        }
        assert_eq!(outside(line, &live, start + 2, Toward::Nearer), start);
        assert_eq!(outside(line, &live, end - 2, Toward::Nearer), end);
        // Its edges are outside it, and text no image holds is text.
        for column in [start, end, 3] {
            assert_eq!(outside(line, &live, column, Toward::End), column);
        }
        assert_eq!(Toward::of(Motion::WordLeft), Toward::Start);
        assert_eq!(Toward::of(Motion::End), Toward::End);
        assert_eq!(Toward::of(Motion::Down), Toward::Nearer);
    }

    #[test]
    fn a_copy_carries_the_images_it_names_only_into_its_own_paste() {
        attachment_assets::with_test_image(|first| {
            let second = attachment_assets::test_image("second.png");
            let token = |a: &Attachment| a.reference.as_ref().unwrap().token();
            let held = [first.clone(), second.clone()];
            assert_eq!(Copied::of("plain words".into(), &held), None);
            let text = format!("see {}", token(&second));
            let copied = Copied::of(text.clone(), &held).unwrap();
            assert_eq!(copied.carried(&text), [second.clone()]);
            assert!(copied.carried(&token(&second)).is_empty());
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
            assert!(
                !reconcile(
                    "typed",
                    &before,
                    &mut attachments,
                    &mut detached,
                    usize::MAX
                )
                .changed
            );
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
