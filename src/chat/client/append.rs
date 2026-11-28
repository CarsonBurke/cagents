//! Bounded local proof of append-only lineage between immutable item versions.
//! Only validated wire append operations create edges. Weak pointers prevent
//! retained transcript bodies, and keep pointer identities immune to reuse.
use super::super::{
    Item, ItemContent, Snapshot, TextCopySource, TranscriptPart,
    presentation::{ItemChange, Presentation},
};
use std::collections::VecDeque;
use std::sync::{Arc, Weak};

const LIMIT: usize = 256;

type Candidate = (Weak<Item>, Option<String>, String);

#[derive(Clone, Default)]
pub(super) struct Lineage {
    edges: VecDeque<(Weak<Item>, Weak<Item>)>,
}

fn item<'a>(snapshot: &'a Snapshot, child: Option<&str>, id: &str) -> Option<&'a Arc<Item>> {
    let items = match child {
        Some(child) => {
            &snapshot
                .subagents
                .iter()
                .find(|agent| agent.id == child)?
                .items
        }
        None => &snapshot.items,
    };
    items.iter().find(|item| item.id == id)
}

impl Lineage {
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.edges.clear();
    }

    pub fn candidates(update: &Presentation, previous: &Snapshot) -> Vec<Candidate> {
        let collect = |child: Option<&str>, changes: &[ItemChange]| {
            changes
                .iter()
                .filter_map(|change| {
                    let ItemChange::Append { item: next, .. } = change else {
                        return None;
                    };
                    let old = item(previous, child, &next.id)?;
                    // The UI markdown cache only consumes Text append proofs.
                    matches!(
                        (&old.content, &next.content),
                        (ItemContent::Text { .. }, ItemContent::Text { .. })
                    )
                    .then(|| {
                        (
                            Arc::downgrade(old),
                            child.map(str::to_owned),
                            next.id.clone(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        let mut candidates = collect(None, &update.changes);
        for child in &update.children {
            candidates.extend(collect(Some(&child.id), &child.changes));
        }
        candidates
    }

    /// Call only after Presentation::apply accepted the complete transaction.
    pub fn record(&mut self, candidates: Vec<Candidate>, next: &Snapshot) {
        for (old, child, id) in candidates {
            if let Some(next) = item(next, child.as_deref(), &id) {
                if self.edges.len() == LIMIT {
                    self.edges.pop_front();
                }
                self.edges.push_back((old, Arc::downgrade(next)));
            }
        }
    }

    /// Carry the unchanged items of a resident `previous` publication into a
    /// full `next` snapshot of the same conversation, and prove the text that
    /// only grew. A full snapshot arrives after a resync or when a reader
    /// returns to a chat it kept while hidden; without this, every message in
    /// it would be a new version, and the interface would prepare each one
    /// again behind a loading placeholder. Runs on the client's reader thread:
    /// comparing bodies is O(bytes), which the interface thread never pays.
    pub fn adopt(&mut self, previous: &Snapshot, next: &mut Snapshot) {
        fn adopt_items(
            edges: &mut Lineage,
            previous: &[Arc<Item>],
            next: &mut [Arc<Item>],
            old_sources: &[Option<TextCopySource>],
            new_sources: &[Option<TextCopySource>],
        ) {
            if previous.is_empty() {
                return;
            }
            let mut ambiguous = std::collections::HashSet::new();
            let mut old_by_id = std::collections::HashMap::new();
            for (index, item) in previous.iter().enumerate() {
                if old_by_id.insert(item.id.as_str(), (index, item)).is_some() {
                    ambiguous.insert(item.id.as_str());
                }
            }
            let mut seen = std::collections::HashSet::new();
            for item in next.iter() {
                if !seen.insert(item.id.as_str()) {
                    ambiguous.insert(item.id.as_str());
                }
            }
            // Equal text and display IDs still cannot authorize sharing a
            // resident allocation between two distinct transcript rows.
            let ambiguous: std::collections::HashSet<String> =
                ambiguous.into_iter().map(str::to_owned).collect();
            for (index, item) in next.iter_mut().enumerate() {
                if ambiguous.contains(item.id.as_str()) {
                    continue;
                }
                let Some(&(old_index, old)) = old_by_id.get(item.id.as_str()) else {
                    continue;
                };
                if Arc::ptr_eq(old, item) {
                    continue;
                }
                if TranscriptPart::parse(&item.id).is_some()
                    && !matches!(
                        (old_sources.get(old_index), new_sources.get(index)),
                        (Some(Some(before)), Some(Some(after)))
                            if before.cache == after.cache && before.token == after.token
                                && before.scope == after.scope
                    )
                {
                    // A seek can visit an opaque provider row and a generated
                    // part in separate windows. Their logical sources differ.
                    continue;
                }
                if old.as_ref() == item.as_ref() {
                    *item = Arc::clone(old);
                    continue;
                }
                let grew = old.role == item.role
                    && matches!((&old.content, &item.content),
                        (ItemContent::Text { text: before, thinking: a, phase: x },
                         ItemContent::Text { text: after, thinking: b, phase: y })
                        if a == b && x == y && after.len() > before.len()
                            && after.starts_with(before.as_str()));
                if grew {
                    if edges.edges.len() == LIMIT {
                        edges.edges.pop_front();
                    }
                    edges
                        .edges
                        .push_back((Arc::downgrade(old), Arc::downgrade(item)));
                }
            }
        }
        adopt_items(
            self,
            &previous.items,
            &mut next.items,
            &previous.text_copy_sources.conversation,
            &next.text_copy_sources.conversation,
        );
        for agent in &mut next.subagents {
            if let Some(old) = previous.subagents.iter().find(|old| old.id == agent.id) {
                let old_sources = previous.text_copy_sources.children.get(&agent.id);
                let new_sources = next.text_copy_sources.children.get(&agent.id);
                adopt_items(
                    self,
                    &old.items,
                    &mut agent.items,
                    old_sources.map_or(&[], Vec::as_slice),
                    new_sources.map_or(&[], Vec::as_slice),
                );
            }
        }
    }

    pub fn proves(&self, previous: &Arc<Item>, current: &Arc<Item>) -> bool {
        let mut cursor = Arc::downgrade(previous);
        let target = Arc::downgrade(current);
        for (old, next) in &self.edges {
            if !Weak::ptr_eq(old, &cursor) {
                continue;
            }
            if Weak::ptr_eq(next, &target) {
                return true;
            }
            cursor = next.clone();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(value: &str) -> Arc<Item> {
        Arc::new(Item {
            id: "answer".into(),
            role: super::super::super::Role::Assistant,
            content: ItemContent::Text {
                text: value.into(),
                thinking: false,
                phase: super::super::super::Phase::Final,
            },
            at: None,
            started_at: None,
        })
    }
    #[test]
    fn full_refresh_keeps_same_id_rows_separate_even_when_their_text_matches() {
        let previous = Snapshot {
            items: vec![text("same text")],
            ..Snapshot::default()
        };
        let mut next = Snapshot {
            items: vec![text("same text"), text("same text")],
            ..Snapshot::default()
        };
        Lineage::default().adopt(&previous, &mut next);
        assert!(!Arc::ptr_eq(&next.items[0], &next.items[1]));
        assert!(!Arc::ptr_eq(&previous.items[0], &next.items[0]));
    }

    #[test]
    fn full_refresh_checks_multipart_row_identity_across_history_windows() {
        let mut part = text("the same text");
        Arc::make_mut(&mut part).id = "answer:native-part:1:text:131072-262144:0-0".into();
        let source = TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: super::super::super::TextCopyScope::Conversation,
            token: 1,
            revision: 1,
            bytes: 262144,
        };
        let mut previous = Snapshot {
            items: vec![part.clone()],
            ..Snapshot::default()
        };
        previous.text_copy_sources.conversation = vec![Some(source.clone())];
        for (current, same_row) in [
            (None, false),
            (
                Some(TextCopySource {
                    token: 2,
                    ..source.clone()
                }),
                false,
            ),
            (Some(source), true),
        ] {
            let mut next = Snapshot {
                items: vec![Arc::new((*part).clone())],
                ..Snapshot::default()
            };
            next.text_copy_sources.conversation = vec![current];
            Lineage::default().adopt(&previous, &mut next);
            assert_eq!(Arc::ptr_eq(&part, &next.items[0]), same_row);
        }
    }

    #[test]
    fn coalesced_append_proof_drops_bodies_and_refuses_rewrites() {
        let first = text("first");
        let middle = text("first middle");
        let last = text("first middle last");
        let weak = Arc::downgrade(&middle);
        let mut lineage = Lineage::default();
        lineage
            .edges
            .push_back((Arc::downgrade(&first), weak.clone()));
        lineage
            .edges
            .push_back((weak.clone(), Arc::downgrade(&last)));
        drop(middle);
        assert!(
            weak.upgrade().is_none(),
            "lineage must not retain body owners"
        );
        assert!(lineage.proves(&first, &last));
        let rewrite = text("other middle last");
        assert!(!lineage.proves(&first, &rewrite));
        lineage.clear();
        assert!(!lineage.proves(&first, &last));
    }
    #[test]
    fn validated_wire_append_creates_proof_but_replace_does_not() {
        let first = text("first");
        let previous = Snapshot {
            generation: 3,
            items: vec![first.clone()],
            ..Snapshot::default()
        };
        let update = Presentation {
            base_generation: 3,
            file_changes_unchanged: false,
            metadata: Snapshot {
                generation: 4,
                ..Snapshot::default()
            },
            order: vec!["answer".into()],
            children: vec![],
            changes: vec![ItemChange::Append {
                item: (*text("")).clone(),
                offset: 5,
                suffix: " middle".into(),
            }],
        };
        let candidates = Lineage::candidates(&update, &previous);
        let next = update.apply(&previous).unwrap();
        let mut lineage = Lineage::default();
        lineage.record(candidates, &next);
        assert!(lineage.proves(&first, &next.items[0]));
        let rewrite = Presentation {
            base_generation: 4,
            file_changes_unchanged: false,
            metadata: Snapshot {
                generation: 5,
                ..Snapshot::default()
            },
            order: vec!["answer".into()],
            children: vec![],
            changes: vec![ItemChange::Replace((*text("other middle")).clone())],
        };
        let candidates = Lineage::candidates(&rewrite, &next);
        assert!(candidates.is_empty());
        let rewritten = rewrite.apply(&next).unwrap();
        lineage.record(candidates, &rewritten);
        assert!(!lineage.proves(&first, &rewritten.items[0]));
    }

    #[test]
    fn proof_storage_is_bounded_and_missing_ancestors_fall_back() {
        let first = text("first");
        let mut last = first.clone();
        let mut lineage = Lineage::default();
        for at in 0..LIMIT + 10 {
            let next = text(&format!("body{at}"));
            let snapshot = Snapshot {
                items: vec![next.clone()],
                ..Snapshot::default()
            };
            lineage.record(
                vec![(Arc::downgrade(&last), None, "answer".into())],
                &snapshot,
            );
            last = next;
        }
        assert_eq!(lineage.edges.len(), LIMIT);
        assert!(!lineage.proves(&first, &last));
    }
}
