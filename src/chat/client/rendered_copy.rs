//! Body-free, current-publication authority for rendered transcript parts.
use crate::chat::{
    Item, ItemContent, Snapshot,
    presentation::{ItemChange, Presentation},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    Text,
    Input,
    Output,
}

#[derive(Clone)]
pub(crate) struct Receipt {
    item: Weak<Item>,
    parts: [Arc<()>; 3],
}
impl Receipt {
    fn new(item: &Arc<Item>) -> Self {
        Self {
            item: Arc::downgrade(item),
            parts: std::array::from_fn(|_| Arc::new(())),
        }
    }
    pub(crate) fn matches(&self, item: &Item) -> bool {
        std::ptr::eq(self.item.as_ptr(), item)
    }
    pub(crate) fn continues(&self, previous: &Self) -> bool {
        self.parts
            .iter()
            .zip(&previous.parts)
            .all(|(a, b)| Arc::ptr_eq(a, b))
    }
    pub(crate) fn token(&self, part: Part) -> Weak<()> {
        Arc::downgrade(&self.parts[part as usize])
    }
}

/// Only resident items own tokens. No item body or append history is retained.
#[derive(Default)]
pub(crate) struct Owners {
    parent: HashMap<String, Receipt>,
    children: HashMap<String, HashMap<String, Receipt>>,
}
impl Owners {
    pub(crate) fn fresh(snapshot: &Snapshot) -> Self {
        Self::derive(None, snapshot, None)
    }
    pub(crate) fn receipt(&self, child: Option<&str>, item: &Item) -> Option<Receipt> {
        let receipt = self.entry(child, &item.id)?;
        receipt.matches(item).then(|| receipt.clone())
    }
    fn entry(&self, child: Option<&str>, id: &str) -> Option<&Receipt> {
        match child {
            None => self.parent.get(id),
            Some(child) => self.children.get(child)?.get(id),
        }
    }
    pub(crate) fn owns(&self, child: Option<&str>, id: &str, part: Part, token: &Weak<()>) -> bool {
        self.entry(child, id)
            .is_some_and(|receipt| receipt.token(part).ptr_eq(token))
    }
    pub(crate) fn appends(update: &Presentation) -> HashSet<(Option<String>, String)> {
        let mut appended = HashSet::new();
        for change in &update.changes {
            if let ItemChange::Append { item, .. } = change {
                appended.insert((None, item.id.clone()));
            }
        }
        for child in &update.children {
            for change in &child.changes {
                if let ItemChange::Append { item, .. } = change {
                    appended.insert((Some(child.id.clone()), item.id.clone()));
                }
            }
        }
        appended
    }
    pub(crate) fn appended(
        previous: &Self,
        snapshot: &Snapshot,
        appends: &HashSet<(Option<String>, String)>,
    ) -> Self {
        Self::derive(Some(previous), snapshot, Some(appends))
    }
    fn derive(
        previous: Option<&Self>,
        snapshot: &Snapshot,
        appends: Option<&HashSet<(Option<String>, String)>>,
    ) -> Self {
        let entries = |child: Option<&str>, items: &[Arc<Item>]| {
            items
                .iter()
                .map(|item| {
                    let old = previous.and_then(|owners| owners.entry(child, &item.id));
                    let receipt = old
                        .and_then(|old| {
                            if old.matches(item) {
                                return Some(old.clone());
                            }
                            if !appends.is_some_and(|appends| {
                                appends.contains(&(child.map(str::to_owned), item.id.clone()))
                            }) {
                                return None;
                            }
                            let previous = old.item.upgrade()?;
                            // Append validates streamed bytes, not the other fields.
                            // Compare identities here, off the UI thread, before any
                            // token is inherited. Status/timing may legitimately move.
                            let stable = previous.role == item.role
                                && match (&previous.content, &item.content) {
                                    (
                                        ItemContent::Tool {
                                            name: a, input: x, ..
                                        },
                                        ItemContent::Tool {
                                            name: b, input: y, ..
                                        },
                                    ) => a == b && x == y,
                                    (
                                        ItemContent::Command {
                                            label: l,
                                            command: a,
                                            working_directory: x,
                                            action: p,
                                            ..
                                        },
                                        ItemContent::Command {
                                            label: m,
                                            command: b,
                                            working_directory: y,
                                            action: q,
                                            ..
                                        },
                                    ) => l == m && a == b && x == y && p == q,
                                    _ => false,
                                };
                            stable.then(|| Receipt {
                                item: Arc::downgrade(item),
                                parts: old.parts.clone(),
                            })
                        })
                        .unwrap_or_else(|| Receipt::new(item));
                    (item.id.clone(), receipt)
                })
                .collect()
        };
        Self {
            parent: entries(None, &snapshot.items),
            children: snapshot
                .subagents
                .iter()
                .map(|child| (child.id.clone(), entries(Some(&child.id), &child.items)))
                .collect(),
        }
    }
}
