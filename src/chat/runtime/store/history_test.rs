//! Paired provider contracts, using the production Store on both sides.
use super::*;
use crate::chat::Snapshot;
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct HistoryHarness {
    store: Store,
    pub snapshot: Snapshot,
    pub completions: Vec<Event>,
}

impl Default for HistoryHarness {
    fn default() -> Self {
        Self {
            store: Store::new().unwrap(),
            snapshot: Snapshot::default(),
            completions: Vec::new(),
        }
    }
}

impl std::ops::Deref for HistoryHarness {
    type Target = Snapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl HistoryHarness {
    pub fn apply(&mut self, events: Vec<Event>) {
        let mut indices: BTreeMap<_, _> = self
            .snapshot
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect();
        for event in events {
            if matches!(
                event,
                Event::TurnFinished { .. }
                    | Event::Execution {
                        event: crate::agent_execution::ProviderEvent::Finished { .. },
                        ..
                    }
            ) {
                self.completions.push(event.clone());
            }
            // As in Actor::step, previews observe parent content before Store
            // consumes it. Only the events returned by Store reach the reducer.
            if matches!(
                event,
                Event::Item(_) | Event::TextDelta { .. } | Event::TurnStarted | Event::Reset { .. }
            ) {
                Arc::make_mut(&mut self.snapshot.notification_preview).observe(&event);
            }
            for event in self.store.apply(event).unwrap() {
                super::super::apply_projected_event(&mut self.snapshot, &mut indices, event, true);
            }
        }
        let window = self.store.window_reusing(&mut self.snapshot.items).unwrap();
        self.snapshot.items = window.items;
        self.snapshot.has_earlier = window.has_earlier;
        self.snapshot.has_later = window.has_later;
        self.store.publish_copy_sources(&mut self.snapshot);
    }

    /// Read every logical item in Store order, including bodies that normal
    /// presentation windows page or excerpt. No adapter events are compared.
    pub fn items(&mut self) -> Vec<Item> {
        self.store.flush_text().unwrap();
        let mut statement = self
            .store
            .db
            .prepare("SELECT metadata,len0,len1,len2,spans FROM items ORDER BY ordinal")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, usize>(1)?,
                    row.get::<_, usize>(2)?,
                    row.get::<_, usize>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })
            .unwrap()
            .map(|record| {
                let (metadata, len0, len1, len2, spans) = record.unwrap();
                let mut item: Item = postcard::from_bytes(&metadata).unwrap();
                let body = |field, len| {
                    String::from_utf8(self.store.field(&item.id, field, 0, len).unwrap()).unwrap()
                };
                match &mut item.content {
                    ItemContent::Text { text, .. } => *text = body(0, len0),
                    ItemContent::Delivery { body: text, .. } => *text = body(0, len0),
                    ItemContent::Command { output, .. } => *output = body(0, len0),
                    ItemContent::Tool { input, output, .. } => {
                        *input = body(0, len0);
                        *output = body(1, len1);
                    }
                    ItemContent::Changes { changes, .. } => {
                        let spans: Vec<usize> = postcard::from_bytes(&spans).unwrap();
                        assert_eq!(changes.len(), spans.len());
                        let text = body(0, len0);
                        let mut start = 0;
                        for (change, len) in changes.iter_mut().zip(spans) {
                            change.diff = text[start..start + len].into();
                            start += len;
                        }
                        assert_eq!(start, text.len());
                    }
                    ItemContent::Image {
                        description, path, ..
                    } => {
                        *description = body(0, len0);
                        *path = body(2, len2);
                    }
                    ItemContent::WebSearch { .. } => {}
                }
                item
            })
            .collect()
    }

    pub fn assert_matches(&mut self, saved: &mut Self) {
        // Intentional difference: live items carry daemon observation times;
        // saved items retain provider times (or no time). All other Item
        // fields, including IDs, roles, classification and outcomes, match.
        self.assert_matches_with(
            saved,
            "live observation times differ from saved provider times",
            |live, saved| {
                for item in live.iter_mut().chain(saved.iter_mut()) {
                    item.at = None;
                    item.started_at = None;
                }
            },
        );
    }

    /// Any additional provider-specific difference must be named by its test.
    pub fn assert_matches_with(
        &mut self,
        saved: &mut Self,
        intentional_difference: &str,
        normalize: impl FnOnce(&mut Vec<Item>, &mut Vec<Item>),
    ) {
        assert!(!intentional_difference.trim().is_empty());
        assert!(
            saved.completions.is_empty(),
            "saved replay must not emit live completion notifications: {:?}",
            saved.completions
        );
        let (mut live, mut saved) = (self.items(), saved.items());
        normalize(&mut live, &mut saved);
        assert_eq!(
            live, saved,
            "paired live/saved history differs; allowed difference: {intentional_difference}"
        );
    }
}
