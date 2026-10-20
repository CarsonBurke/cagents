//! Bounded local proof of append-only lineage between immutable item versions.
//! Only validated wire append operations create edges. Weak pointers prevent
//! retained transcript bodies, and keep pointer identities immune to reuse.
use super::super::{
    Item, ItemContent, Snapshot,
    presentation::{ItemChange, Presentation},
};
use std::collections::VecDeque;
use std::sync::{Arc, Weak};

const LIMIT: usize = 256;

type Candidate = (Weak<Item>, Option<String>, String);

#[derive(Default)]
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
    pub fn clear(&mut self) {
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
