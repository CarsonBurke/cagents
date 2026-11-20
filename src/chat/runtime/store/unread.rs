//! The conversation's items as a reader that follows all of it sees them:
//! each item whose content changed since it last asked, whole rather than
//! paged or excerpted for the view. The Work tracker reads a conversation
//! cagents is the client of this way, whether or not anything shows it.
//!
//! Following leaves the presented window alone: it neither moves nor
//! refreshes the window, nor consumes the changes the next presentation is
//! computed from.

use super::{Store, StoreError};
use crate::chat::{Item, ItemContent};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

/// The newest items a first read serves.
const FIRST_ITEMS: usize = 256;
/// Changed items remembered between reads; past this many, the next read
/// starts again from the newest items.
const REMEMBERED: usize = 4096;
/// The most of one body served: its start and its end, where a command
/// prints what it made. The middle of a longer one is left out.
const BODY_BYTES: usize = 1024 * 1024;
/// The most of all bodies one read serves; the rest waits for the next.
const READ_BYTES: usize = 8 * 1024 * 1024;

/// What has changed since the follower last read.
#[derive(Debug, Default)]
pub(super) enum Following {
    /// Nothing has read yet, so nothing is remembered.
    #[default]
    Not,
    Changed(HashSet<String>),
    /// More changed than is remembered.
    Lost,
}

impl Following {
    pub(super) fn changed(&mut self, id: &str) {
        if let Self::Changed(ids) = self {
            ids.insert(id.to_owned());
            if ids.len() > REMEMBERED {
                *self = Self::Lost;
            }
        }
    }
}

/// One read of what changed in the conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unread {
    /// Changed items, oldest first, with whole bodies up to a bound.
    pub items: Vec<Item>,
    /// This read started from the newest items rather than from what
    /// changed since the last: the first read, or one after more changed
    /// than was remembered.
    pub restarted: bool,
    /// Items before the oldest one served that this read did not serve.
    /// Only a restarted read leaves any out.
    pub has_earlier: bool,
    /// The newest item in the conversation, the only one still written to.
    pub newest: Option<String>,
}

impl Store {
    /// The items changed since the last call, or the newest ones on the
    /// first, together with the items `again` names. What does not fit in
    /// one read is served by the next.
    pub(in crate::chat::runtime) fn unread(&mut self, again: &[String]) -> Result<Unread, String> {
        // Streamed prose waits for its next part before it is stored.
        self.flush_text().map_err(|error| error.to_string())?;
        let mut unread = Unread::default();
        let ids = match std::mem::replace(&mut self.following, Following::Changed(HashSet::new())) {
            Following::Changed(mut ids) => {
                ids.extend(again.iter().cloned());
                let mut ordered = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(ordinal) = self.ordinal(&id).map_err(|e| e.to_string())? {
                        ordered.push((ordinal, id));
                    }
                }
                ordered.sort();
                ordered
            }
            Following::Not | Following::Lost => {
                unread.restarted = true;
                let mut statement = self
                    .db
                    .prepare_cached("SELECT ordinal,id FROM items ORDER BY ordinal DESC LIMIT ?1")
                    .map_err(|e| e.to_string())?;
                let mut newest = statement
                    .query_map([FIRST_ITEMS as i64 + 1], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .map_err(|e| e.to_string())?
                    .collect::<Result<Vec<(i64, String)>, _>>()
                    .map_err(|e| e.to_string())?;
                unread.has_earlier = newest.len() > FIRST_ITEMS || self.provider_earlier;
                newest.truncate(FIRST_ITEMS);
                newest.reverse();
                newest
            }
        };
        let mut budget = READ_BYTES;
        let mut left = Vec::new();
        for (_, id) in ids {
            if !left.is_empty() {
                left.push(id);
                continue;
            }
            match self.whole(&id, &mut budget).map_err(|e| e.to_string())? {
                Some(item) => unread.items.push(item),
                None => left.push(id),
            }
        }
        if !left.is_empty() {
            if unread.restarted {
                // The newest are served first next time, so a restarted
                // read gives up its oldest instead.
                unread.has_earlier = true;
            } else {
                for id in &left {
                    self.following.changed(id);
                }
            }
        }
        unread.newest = self
            .db
            .prepare_cached("SELECT id FROM items ORDER BY ordinal DESC LIMIT 1")
            .and_then(|mut statement| statement.query_row([], |row| row.get(0)).optional())
            .map_err(|e| e.to_string())?;
        Ok(unread)
    }

    /// The item `id` with its bodies, or `None` when they do not fit what is
    /// left of `budget`. The first item of a read is served whatever it costs.
    fn whole(&self, id: &str, budget: &mut usize) -> Result<Option<Item>, StoreError> {
        let Some((metadata, len0, len1)) = self
            .db
            .prepare_cached("SELECT metadata,len0,len1 FROM items WHERE id=?1")?
            .query_row(params![id], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)? as usize,
                ))
            })
            .optional()?
        else {
            return Ok(None);
        };
        let mut item: Item = postcard::from_bytes(&metadata)
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        // A patch's paths are in the item itself; neither a patch nor an
        // image says anything more about the work.
        let (len0, len1) = match &item.content {
            ItemContent::Text { .. }
            | ItemContent::Delivery { .. }
            | ItemContent::Command { .. } => (len0, 0),
            ItemContent::Tool { .. } => (len0, len1),
            ItemContent::Changes { .. }
            | ItemContent::WebSearch { .. }
            | ItemContent::Image { .. } => (0, 0),
        };
        let cost = metadata.len() + len0.min(BODY_BYTES) + len1.min(BODY_BYTES);
        if cost > *budget && *budget < READ_BYTES {
            return Ok(None);
        }
        *budget = budget.saturating_sub(cost);
        let body0 = self.body(id, 0, len0)?;
        let body1 = self.body(id, 1, len1)?;
        match &mut item.content {
            ItemContent::Text { text, .. } => *text = body0,
            ItemContent::Delivery { body, .. } => *body = body0,
            ItemContent::Command { output, .. } => *output = body0,
            ItemContent::Tool { input, output, .. } => {
                *input = body0;
                *output = body1;
            }
            ItemContent::Changes { .. }
            | ItemContent::WebSearch { .. }
            | ItemContent::Image { .. } => {}
        }
        Ok(Some(item))
    }

    /// One field's bytes, or its start and end when it is longer than
    /// `BODY_BYTES`, cut between characters.
    fn body(&self, id: &str, field: i64, length: usize) -> Result<String, StoreError> {
        let bytes = if length <= BODY_BYTES {
            self.field(id, field, 0, length)?
        } else {
            let head = self.boundary(id, field, BODY_BYTES / 2)?;
            let tail = self.boundary(id, field, length - BODY_BYTES / 2)?;
            let mut bytes = self.field(id, field, 0, head)?;
            bytes.extend_from_slice(b"\n");
            bytes.extend(self.field(id, field, tail, length)?);
            bytes
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{CommandAction, Event, Role, ToolStatus};

    fn command(id: &str, output: &str, status: ToolStatus) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Command {
                label: String::new(),
                command: "gh pr create --fill".into(),
                working_directory: "/repo".into(),
                action: CommandAction::Run,
                status,
                exit_code: None,
                duration_ms: None,
                output: output.into(),
            },
            started_at: None,
            at: None,
        }
    }

    fn ids(unread: &Unread) -> Vec<&str> {
        unread.items.iter().map(|item| item.id.as_str()).collect()
    }

    #[test]
    fn a_first_read_serves_the_newest_items_and_later_ones_only_what_changed() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        for index in 0..FIRST_ITEMS + 2 {
            store
                .apply(Event::Item(command(
                    &format!("c{index}"),
                    "",
                    ToolStatus::Succeeded,
                )))
                .unwrap();
        }
        let first = store.unread(&[]).unwrap();
        assert!(first.restarted && first.has_earlier);
        assert_eq!(first.items.len(), FIRST_ITEMS);
        assert_eq!(first.items[0].id, "c2");
        assert_eq!(
            first.newest.as_deref(),
            Some(format!("c{}", FIRST_ITEMS + 1).as_str())
        );

        assert!(store.unread(&[]).unwrap().items.is_empty());
        store
            .apply(Event::Item(command("new", "", ToolStatus::Running)))
            .unwrap();
        store
            .apply(Event::OutputDelta {
                id: "new".into(),
                offset: 0,
                delta: "https://github.com/o/r/pull/7".into(),
            })
            .unwrap();
        let changed = store.unread(&[]).unwrap();
        assert!(!changed.restarted && !changed.has_earlier);
        assert_eq!(ids(&changed), ["new"]);
        assert!(matches!(&changed.items[0].content,
            ItemContent::Command { output, .. } if output == "https://github.com/o/r/pull/7"));
        // Asked for again, an unchanged item is served again.
        assert_eq!(ids(&store.unread(&["c5".into()]).unwrap()), ["c5"]);
    }

    #[test]
    fn following_leaves_the_presented_window_and_its_changes_alone() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        store
            .apply(Event::Item(command("a", "", ToolStatus::Succeeded)))
            .unwrap();
        store.window().unwrap();
        store.unread(&[]).unwrap();
        store
            .apply(Event::Item(command("b", "", ToolStatus::Succeeded)))
            .unwrap();
        assert_eq!(ids(&store.unread(&[]).unwrap()), ["b"]);
        assert!(store.dirty);
        assert!(
            store
                .changed
                .as_ref()
                .is_some_and(|changed| changed.contains("b"))
        );
    }

    #[test]
    fn a_long_body_is_served_by_its_start_and_its_end() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let output = format!(
            "start\n{}\nhttps://github.com/o/r/pull/9\n",
            "é".repeat(BODY_BYTES)
        );
        store
            .apply(Event::Item(command("long", &output, ToolStatus::Succeeded)))
            .unwrap();
        let unread = store.unread(&[]).unwrap();
        let ItemContent::Command { output: served, .. } = &unread.items[0].content else {
            panic!("{unread:?}");
        };
        // Each cut moves back to the start of a character, of up to 4 bytes.
        assert!(served.len() <= BODY_BYTES + 1 + 3, "{}", served.len());
        assert!(served.starts_with("start\n"));
        assert!(served.ends_with("https://github.com/o/r/pull/9\n"));
        assert!(!served.contains('\u{fffd}'));
    }

    #[test]
    fn a_read_that_runs_out_of_budget_leaves_the_rest_for_the_next() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        store.unread(&[]).unwrap();
        let big = "x".repeat(BODY_BYTES);
        let count = READ_BYTES / BODY_BYTES + 2;
        for index in 0..count {
            store
                .apply(Event::Item(command(
                    &format!("b{index}"),
                    &big,
                    ToolStatus::Succeeded,
                )))
                .unwrap();
        }
        let first = store.unread(&[]).unwrap();
        assert!(first.items.len() < count);
        let rest = store.unread(&[]).unwrap();
        assert_eq!(first.items.len() + rest.items.len(), count);
        assert_eq!(
            rest.items.last().map(|item| item.id.as_str()),
            Some(format!("b{}", count - 1).as_str())
        );
    }
}
