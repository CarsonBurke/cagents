//! Revision-fenced transcript updates. Full snapshots remain the resync boundary.
use super::{Item, ItemContent, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presentation {
    pub base_generation: u64,
    /// Unchanged immutable edit metadata is retained locally, not retransmitted
    /// with every prose/tool delta. Full snapshots remain the resync boundary.
    #[serde(default)]
    pub file_changes_unchanged: bool,
    /// Everything except the parent transcript, whose items are updated below.
    pub metadata: Snapshot,
    pub order: Vec<String>,
    pub changes: Vec<ItemChange>,
    pub children: Vec<ChildUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildUpdate {
    pub id: String,
    pub order: Vec<String>,
    pub changes: Vec<ItemChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemChange {
    Replace(Item),
    /// `item` carries the new identity/status/stamp with its streamed body empty.
    Append {
        item: Item,
        offset: usize,
        suffix: String,
    },
}

fn body(content: &ItemContent) -> Option<&str> {
    match content {
        ItemContent::Text { text, .. } => Some(text),
        ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => Some(output),
        _ => None,
    }
}
fn body_mut(content: &mut ItemContent) -> Option<&mut String> {
    match content {
        ItemContent::Text { text, .. } => Some(text),
        ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => Some(output),
        _ => None,
    }
}
impl ItemChange {
    #[cfg(any(feature = "host", test))]
    pub(crate) fn between(previous: Option<&Item>, current: &Item) -> Self {
        if let Some(previous) = previous
            && let (Some(old), Some(new)) = (body(&previous.content), body(&current.content))
            && new.starts_with(old)
        {
            let content = match &current.content {
                ItemContent::Text {
                    thinking, phase, ..
                } => ItemContent::Text {
                    text: String::new(),
                    thinking: *thinking,
                    phase: *phase,
                },
                ItemContent::Command {
                    label,
                    command,
                    working_directory,
                    action,
                    status,
                    exit_code,
                    duration_ms,
                    ..
                } => ItemContent::Command {
                    label: label.clone(),
                    command: command.clone(),
                    working_directory: working_directory.clone(),
                    action: *action,
                    status: *status,
                    exit_code: *exit_code,
                    duration_ms: *duration_ms,
                    output: String::new(),
                },
                ItemContent::Tool {
                    name,
                    input,
                    status,
                    ..
                } => ItemContent::Tool {
                    name: name.clone(),
                    input: input.clone(),
                    status: *status,
                    output: String::new(),
                },
                _ => unreachable!("streamed body"),
            };
            let item = Item {
                id: current.id.clone(),
                role: current.role,
                started_at: current.started_at,
                at: current.at,
                content,
            };
            return Self::Append {
                item,
                offset: old.len(),
                suffix: new[old.len()..].into(),
            };
        }
        Self::Replace(current.clone())
    }
    fn id(&self) -> &str {
        match self {
            Self::Replace(item) | Self::Append { item, .. } => &item.id,
        }
    }
}

fn apply_items(
    previous: &[std::sync::Arc<Item>],
    order: Vec<String>,
    changes: Vec<ItemChange>,
) -> Result<Vec<std::sync::Arc<Item>>, &'static str> {
    let previous: HashMap<_, _> = previous
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();
    let mut changed = HashSet::new();
    for change in &changes {
        if !changed.insert(change.id()) {
            return Err("Duplicate transcript change");
        }
        if let ItemChange::Append { item, offset, .. } = change {
            let old = previous
                .get(item.id.as_str())
                .and_then(|old| body(&old.content));
            if old.is_none_or(|old| old.len() != *offset) || body(&item.content) != Some("") {
                return Err("Transcript append boundary mismatch");
            }
        }
    }
    let mut ordered = HashSet::new();
    for id in &order {
        if !ordered.insert(id.as_str())
            || (!previous.contains_key(id.as_str()) && !changed.contains(id.as_str()))
        {
            return Err("Invalid transcript order");
        }
    }
    if changed.iter().any(|id| !ordered.contains(id)) {
        return Err("Unordered transcript change");
    }
    let mut items: HashMap<_, _> = previous
        .into_iter()
        .map(|(id, item)| (id.to_owned(), item.clone()))
        .collect();
    for change in changes {
        let item = match change {
            ItemChange::Replace(item) => item,
            ItemChange::Append {
                mut item,
                offset: _,
                suffix,
            } => {
                let old = items.remove(&item.id).unwrap();
                let mut text = body(&old.content).unwrap().to_owned();
                text.push_str(&suffix);
                *body_mut(&mut item.content).unwrap() = text;
                item
            }
        };
        items.insert(item.id.clone(), std::sync::Arc::new(item));
    }
    Ok(order
        .into_iter()
        .map(|id| items.remove(&id).unwrap())
        .collect())
}

impl Presentation {
    /// Build a complete next publication from an immutable previous snapshot.
    /// A malformed or missed patch cannot change the installed snapshot,
    /// including its child transcripts. Unchanged payloads remain shared.
    pub fn apply(self, previous: &Snapshot) -> Result<Snapshot, &'static str> {
        if self.base_generation != previous.generation
            || self.metadata.generation <= self.base_generation
            || !self.metadata.items.is_empty()
        {
            return Err("Transcript revision mismatch");
        }
        let mut metadata = self.metadata;
        if self.file_changes_unchanged {
            if !metadata.file_changes.files.is_empty() {
                return Err("Retained file summary carries replacement rows");
            }
            metadata.file_changes = previous.file_changes.clone();
        }
        metadata.items = apply_items(&previous.items, self.order, self.changes)?;
        let mut children = HashMap::new();
        for child in self.children {
            if children.insert(child.id.clone(), child).is_some() {
                return Err("Duplicate child transcript");
            }
        }
        for agent in &mut metadata.subagents {
            if !agent.items.is_empty() {
                return Err("Unexpected child transcript body");
            }
            let child = children
                .remove(&agent.id)
                .ok_or("Missing child transcript")?;
            let previous_items = previous
                .subagents
                .iter()
                .find(|old| old.id == agent.id)
                .map_or(&[][..], |old| old.items.as_slice());
            agent.items = apply_items(previous_items, child.order, child.changes)?;
        }
        if !children.is_empty() {
            return Err("Unknown child transcript");
        }
        Ok(metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_window_updates_share_payloads_with_the_retained_ui_snapshot() {
        let items: Vec<_> = (0..127)
            .map(|index| {
                std::sync::Arc::new(text(&format!("fixed-{index}"), &"x".repeat(23 * 1024)))
            })
            .chain([std::sync::Arc::new(text("live", "hello"))])
            .collect();
        let mut installed = std::sync::Arc::new(Snapshot {
            generation: 1,
            items,
            ..Default::default()
        });
        let retained_ui = installed.clone();
        let next = text("live", "hello tail");
        let update = Presentation {
            file_changes_unchanged: false,
            base_generation: 1,
            metadata: Snapshot {
                generation: 2,
                ..Default::default()
            },
            order: installed.items.iter().map(|item| item.id.clone()).collect(),
            children: Vec::new(),
            changes: vec![ItemChange::between(
                installed.items.last().map(|item| item.as_ref()),
                &next,
            )],
        };
        let full_bytes = postcard::to_stdvec(&*installed).unwrap().len();
        let delta_bytes = postcard::to_stdvec(&update).unwrap().len();
        assert!(full_bytes > 2 * 1024 * 1024);
        assert!(delta_bytes < 4096);
        installed = std::sync::Arc::new(update.apply(&installed).unwrap());
        for (old, new) in retained_ui.items.iter().zip(&installed.items).take(127) {
            assert!(
                std::sync::Arc::ptr_eq(old, new),
                "unchanged body was copied during client publication"
            );
        }
        assert_eq!(*installed.items[127], next);
        assert!(
            matches!(&retained_ui.items[127].content, ItemContent::Text { text, .. } if text == "hello")
        );
    }
    #[test]
    fn unchanged_child_payloads_are_shared_and_tool_rewrites_replace() {
        let child = std::sync::Arc::new(text("child-answer", &"x".repeat(64 * 1024)));
        let tool = Item {
            id: "tool".into(),
            role: super::super::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Tool {
                name: "shell".into(),
                input: "input".into(),
                output: "original".into(),
                status: super::super::ToolStatus::Running,
            },
        };
        let mut command = tool.clone();
        command.content = ItemContent::Command {
            label: "Run".into(),
            command: "cmd".into(),
            working_directory: "/repo".into(),
            action: super::super::CommandAction::Run,
            status: super::super::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(1),
            output: "final output".into(),
        };
        let agent = super::super::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: "child".into(),
            kind: "review".into(),
            description: String::new(),
            status: super::super::ToolStatus::Running,
            background: false,
            items: vec![child.clone()],
        };
        let mut snapshot = Snapshot {
            generation: 1,
            items: vec![tool.clone().into()],
            subagents: vec![agent.clone()],
            ..Default::default()
        };
        let mut metadata = Snapshot {
            generation: 2,
            subagents: vec![agent],
            ..Default::default()
        };
        metadata.subagents[0].items.clear();
        let update = Presentation {
            file_changes_unchanged: false,
            base_generation: 1,
            metadata,
            order: vec!["tool".into()],
            changes: vec![ItemChange::between(Some(&tool), &command)],
            children: vec![ChildUpdate {
                id: "child".into(),
                order: vec!["child-answer".into()],
                changes: Vec::new(),
            }],
        };
        assert!(postcard::to_stdvec(&update).unwrap().len() < 1024);
        snapshot = update.apply(&snapshot).unwrap();
        assert_eq!(*snapshot.items[0], command);
        assert!(std::sync::Arc::ptr_eq(
            &child,
            &snapshot.subagents[0].items[0]
        ));
    }
    fn text(id: &str, text: &str) -> Item {
        Item {
            id: id.into(),
            role: crate::chat::Role::Assistant,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: crate::chat::Phase::Final,
            },
            started_at: Some(1),
            at: Some(1),
        }
    }
    #[test]
    fn patches_preserve_order_remove_items_and_append_only_the_suffix() {
        let old = text("answer", &"α".repeat(32 * 1024));
        let mut new = old.clone();
        body_mut(&mut new.content).unwrap().push_str("β");
        new.at = Some(2);
        let change = ItemChange::between(Some(&old), &new);
        assert!(
            matches!(&change, ItemChange::Append { offset: 65536, suffix, item } if suffix == "β" && body(&item.content) == Some(""))
        );
        assert!(postcard::to_stdvec(&change).unwrap().len() < 128);
        let mut snapshot = Snapshot {
            generation: 4,
            items: vec![text("removed", "x").into(), old.into()],
            ..Snapshot::default()
        };
        let metadata = Snapshot {
            generation: 7,
            busy: true,
            ..Snapshot::default()
        };
        snapshot = Presentation {
            file_changes_unchanged: false,
            children: Vec::new(),
            base_generation: 4,
            metadata,
            order: vec!["new".into(), "answer".into()],
            changes: vec![change, ItemChange::Replace(text("new", "hello"))],
        }
        .apply(&snapshot)
        .unwrap();
        assert_eq!(snapshot.generation, 7);
        assert!(snapshot.busy);
        assert_eq!(
            snapshot.items,
            vec![
                std::sync::Arc::new(text("new", "hello")),
                std::sync::Arc::new(new)
            ]
        );
    }
    #[test]
    fn rewrites_replace_and_invalid_updates_are_atomic() {
        let old = text("answer", "original");
        let new = text("answer", "rewritten");
        assert!(matches!(
            ItemChange::between(Some(&old), &new),
            ItemChange::Replace(_)
        ));
        for (generation, offset, order) in [
            (2, 8, vec!["answer".into()]),
            (1, 7, vec!["answer".into()]),
            (1, 8, vec!["unknown".into()]),
        ] {
            let snapshot = Snapshot {
                generation: 1,
                items: vec![old.clone().into()],
                ..Snapshot::default()
            };
            let before = snapshot.clone();
            let mut shell = new.clone();
            body_mut(&mut shell.content).unwrap().clear();
            assert!(
                Presentation {
                    file_changes_unchanged: false,
                    children: Vec::new(),
                    base_generation: generation,
                    metadata: Snapshot {
                        generation: 3,
                        ..Snapshot::default()
                    },
                    order,
                    changes: vec![ItemChange::Append {
                        item: shell,
                        offset,
                        suffix: "suffix".into()
                    }]
                }
                .apply(&snapshot)
                .is_err()
            );
            assert_eq!(snapshot, before);
        }
    }

    #[test]
    fn unchanged_file_summary_is_shared_without_retransmitting_rows() {
        let summary = std::sync::Arc::new(super::super::FileChangesSummary {
            revision: 8,
            total_files: 100,
            applied_files: 99,
            ..Default::default()
        });
        let previous = Snapshot {
            generation: 1,
            file_changes: summary.clone(),
            ..Default::default()
        };
        let update = Presentation {
            base_generation: 1,
            file_changes_unchanged: true,
            metadata: Snapshot {
                generation: 2,
                ..Default::default()
            },
            order: vec![],
            changes: vec![],
            children: vec![],
        };
        assert!(postcard::to_stdvec(&update).unwrap().len() < 1024);
        let current = update.apply(&previous).unwrap();
        assert!(std::sync::Arc::ptr_eq(&summary, &current.file_changes));
        assert_eq!(current.file_changes.applied_files, 99);
    }
}
