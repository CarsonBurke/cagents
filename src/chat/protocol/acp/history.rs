//! A resumed conversation's history, as `session/load` replays it.
//!
//! ACP has no history paging: an agent answers `session/load` only after it
//! has streamed the whole conversation as ordinary session updates. Those
//! updates are folded here into the items they make, keeping the newest that
//! fit the bounds, and published as one `History` event once the load is
//! answered. A long conversation therefore costs a bounded buffer rather
//! than one transcript event per chunk.
use crate::chat::{Event, Item, ItemContent, Phase, Role};
use std::collections::{HashMap, VecDeque};

/// The most replayed items kept; older ones are left out, with a note.
pub(super) const MAX_ITEMS: usize = 2048;
/// The most replayed text and output kept, counted the same way.
const MAX_BYTES: usize = 24 * 1024 * 1024;
/// A single replayed message keeps no more than this of its text.
const MAX_TEXT: usize = 4 * 1024 * 1024;

pub(super) const TRUNCATED_ID: &str = "acp-history-truncated";

#[derive(Default)]
pub(super) struct Replay {
    items: VecDeque<Item>,
    /// The ordinal of `items[0]`; an ordinal below it was dropped.
    first: u64,
    ordinals: HashMap<String, u64>,
    bytes: usize,
    dropped: bool,
    /// The user's message being replayed and the images it carried, so the
    /// image list cagents appended when sending it can be read back.
    user: Option<(u64, Vec<u64>)>,
}

impl Replay {
    /// Folds a transcript event into the history. Any other event describes
    /// the session's current state rather than its past, and is handed back
    /// to be published as it is.
    pub fn absorb(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::TextDelta {
                id,
                role,
                thinking,
                delta,
            } => {
                let continues = role == Role::User
                    && self
                        .user
                        .as_ref()
                        .is_some_and(|(ordinal, _)| self.ordinals.get(&id) == Some(ordinal));
                self.close_user_unless(continues);
                if let Some(item) = self.get_mut(&id)
                    && let ItemContent::Text { text, .. } = &mut item.content
                {
                    let room = MAX_TEXT.saturating_sub(text.len());
                    let delta = prefix(&delta, room);
                    text.push_str(delta);
                    self.bytes += delta.len();
                } else {
                    let ordinal = self.push(Item {
                        started_at: None,
                        id,
                        role,
                        content: ItemContent::Text {
                            text: prefix(&delta, MAX_TEXT).to_owned(),
                            thinking,
                            phase: Phase::Final,
                        },
                        at: None,
                    });
                    if role == Role::User {
                        self.user = Some((ordinal, Vec::new()));
                    }
                }
                self.trim();
                None
            }
            Event::Item(item) => {
                let user_image =
                    item.role == Role::User && matches!(item.content, ItemContent::Image { .. });
                if !user_image {
                    self.close_user_unless(false);
                }
                let replaced = self.ordinals.get(&item.id).copied();
                match replaced {
                    Some(ordinal) if ordinal >= self.first => {
                        let index = (ordinal - self.first) as usize;
                        self.bytes = self.bytes.saturating_sub(size(&self.items[index]));
                        self.bytes += size(&item);
                        self.items[index] = item;
                    }
                    // A revision of something already dropped stays dropped
                    // rather than reappearing out of place.
                    Some(_) => {}
                    None => {
                        let ordinal = self.push(item);
                        if user_image {
                            match &mut self.user {
                                Some((_, images)) => images.push(ordinal),
                                None => self.user = Some((u64::MAX, vec![ordinal])),
                            }
                        }
                    }
                }
                self.trim();
                None
            }
            other => Some(other),
        }
    }

    /// The history to publish, oldest first.
    pub fn finish(mut self) -> Vec<Item> {
        self.close_user_unless(false);
        let mut items = Vec::from(self.items);
        if self.dropped {
            items.insert(
                0,
                Item {
                    started_at: None,
                    id: TRUNCATED_ID.into(),
                    role: Role::System,
                    content: ItemContent::Text {
                        text: "Earlier parts of this conversation are not shown: it is longer than cagents loads when resuming it.".into(),
                        thinking: false,
                        phase: Phase::Final,
                    },
                    at: None,
                },
            );
        }
        items
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Item> {
        let ordinal = *self.ordinals.get(id)?;
        let index = ordinal.checked_sub(self.first)? as usize;
        self.items.get_mut(index)
    }

    fn push(&mut self, item: Item) -> u64 {
        let ordinal = self.first + self.items.len() as u64;
        self.bytes += size(&item);
        self.ordinals.insert(item.id.clone(), ordinal);
        self.items.push_back(item);
        ordinal
    }

    fn trim(&mut self) {
        while self.items.len() > MAX_ITEMS || (self.bytes > MAX_BYTES && self.items.len() > 1) {
            let Some(item) = self.items.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(size(&item));
            self.ordinals.remove(&item.id);
            self.first += 1;
            self.dropped = true;
        }
    }

    /// Ends the user's message being replayed, unless `continues`, reading
    /// back the images it carried from the list cagents appended to its text.
    fn close_user_unless(&mut self, continues: bool) {
        if continues {
            return;
        }
        let Some((text_ordinal, images)) = self.user.take() else {
            return;
        };
        if images.is_empty() {
            return;
        }
        let index = |ordinal: u64| ordinal.checked_sub(self.first).map(|index| index as usize);
        let sources = images
            .iter()
            .filter_map(|ordinal| match &self.items.get(index(*ordinal)?)?.content {
                ItemContent::Image { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if sources.len() != images.len() {
            return;
        }
        let Some(text_index) = index(text_ordinal) else {
            return;
        };
        let Some(Item {
            content: ItemContent::Text { text, .. },
            ..
        }) = self.items.get(text_index)
        else {
            return;
        };
        let Some((plain, references)) =
            super::super::image_references::restore_list(text, &sources)
        else {
            return;
        };
        let plain = plain.to_owned();
        if let Some(Item {
            content: ItemContent::Text { text, .. },
            ..
        }) = self.items.get_mut(text_index)
        {
            *text = plain;
        }
        for (ordinal, restored) in images.into_iter().zip(references) {
            if let Some(Item {
                content:
                    ItemContent::Image {
                        reference,
                        description,
                        ..
                    },
                ..
            }) = index(ordinal).and_then(|index| self.items.get_mut(index))
                && let Some(restored) = restored
            {
                *description = restored.label();
                *reference = Some(restored);
            }
        }
    }
}

fn prefix(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// What an item costs the history's byte budget: the text and output it
/// carries, which is what grows with a conversation.
fn size(item: &Item) -> usize {
    item.id.len()
        + match &item.content {
            ItemContent::Text { text, .. } => text.len(),
            ItemContent::Command {
                command, output, ..
            } => command.len() + output.len(),
            ItemContent::Changes { changes, .. } => changes
                .iter()
                .map(|change| change.path.len() + change.diff.len())
                .sum(),
            ItemContent::Tool { input, output, .. } => input.len() + output.len(),
            ItemContent::Image { path, .. } => path.len(),
            ItemContent::WebSearch { query, .. } => query.len(),
            ItemContent::Delivery { body, .. } => body.len(),
        }
}
