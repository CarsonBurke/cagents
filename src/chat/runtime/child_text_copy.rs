//! Copy authority for complete text already held by the bounded child view.
//! This module owns metadata only; trimming never leaves another body owner.
use super::{Event, Snapshot};
use crate::chat::{TEXT_COPY_PAGE_BYTES, TextCopyPage, TextCopyScope, TextCopySource};
use std::collections::BTreeMap;

const MAX_BYTES: usize = 64 * 1024;

pub(super) struct Update {
    child: Option<(String, BTreeMap<String, Option<TextCopySource>>)>,
    reconcile: bool,
}

fn text(item: &crate::chat::Item) -> Option<&str> {
    item.logical_copy_body()
}

fn valid(source: &TextCopySource, child: &str, body: &str) -> bool {
    source.scope == (TextCopyScope::Subagent { id: child.into() })
        && source.bytes == body.len() as u64
        && body.len() <= MAX_BYTES
}

fn fresh(child: &str, bytes: usize) -> TextCopySource {
    TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: TextCopyScope::Subagent { id: child.into() },
        token: 1,
        revision: 1,
        bytes: bytes as u64,
    }
}

/// Compute authority while the previous, untrimmed text is still available.
/// Only the changed child's item IDs and body-free metadata cross mutation.
pub(super) fn prepare(state: &mut Snapshot, event: &Event) -> Update {
    let child = match event {
        Event::SubagentItem { id, .. }
        | Event::SubagentTextDelta { id, .. }
        | Event::SubagentOutputDelta { id, .. }
        | Event::SubagentItemsRemoved { id, .. } => id,
        _ => {
            return Update {
                child: None,
                reconcile: matches!(event, Event::Subagent { .. } | Event::Reset { .. }),
            };
        }
    };
    let sources = state
        .text_copy_sources
        .children
        .remove(child)
        .unwrap_or_default();
    let agent = state.subagents.iter().find(|agent| agent.id == *child);
    let mut entries: BTreeMap<_, _> = agent
        .into_iter()
        .flat_map(|agent| agent.items.iter())
        .enumerate()
        .map(|(index, item)| {
            let source = sources
                .get(index)
                .cloned()
                .flatten()
                .filter(|source| text(item).is_some_and(|body| valid(source, child, body)));
            (item.id.clone(), source)
        })
        .collect();
    match event {
        Event::SubagentItem { item, .. } => {
            let next = text(item)
                .filter(|body| body.len() <= MAX_BYTES)
                .map(|body| {
                    let previous = agent
                        .and_then(|agent| agent.items.iter().find(|old| old.id == item.id))
                        .and_then(|old| text(old))
                        .filter(|old| body.starts_with(old));
                    let mut source = previous
                        .and_then(|_| entries.get(&item.id).cloned().flatten())
                        .unwrap_or_else(|| fresh(child, body.len()));
                    source.bytes = body.len() as u64;
                    source
                });
            entries.insert(item.id.clone(), next);
        }
        Event::SubagentTextDelta { item_id, delta, .. } => {
            // A delta without a known initial full item may be only a suffix.
            let next = entries
                .get(item_id)
                .cloned()
                .flatten()
                .and_then(|mut source| {
                    let bytes = source.bytes.checked_add(delta.len() as u64)?;
                    if bytes > MAX_BYTES as u64 {
                        return None;
                    }
                    source.bytes = bytes;
                    Some(source)
                });
            entries.insert(item_id.clone(), next);
        }
        _ => {}
    }
    Update {
        child: Some((child.clone(), entries)),
        reconcile: true,
    }
}

/// Align with the actual post-trim items, including global rich-history eviction.
pub(super) fn finish(state: &mut Snapshot, update: Update) {
    if !update.reconcile {
        return;
    }
    if let Some((child, mut entries)) = update.child
        && let Some(agent) = state.subagents.iter().find(|agent| agent.id == child)
    {
        let aligned = agent
            .items
            .iter()
            .map(|item| {
                entries
                    .remove(&item.id)
                    .flatten()
                    .filter(|source| text(item).is_some_and(|body| valid(source, &child, body)))
            })
            .collect();
        state.text_copy_sources.children.insert(child, aligned);
    }
    state.text_copy_sources.children.retain(|child, sources| {
        let Some(agent) = state.subagents.iter().find(|agent| agent.id == *child) else {
            return false;
        };
        if agent.items.is_empty() {
            sources.clear();
        }
        true
    });
}

/// Read only a verified prefix of the current existing item under the state lock.
pub(super) fn read(
    state: &Snapshot,
    source: &TextCopySource,
    offset: u64,
) -> Result<TextCopyPage, String> {
    let TextCopyScope::Subagent { id } = &source.scope else {
        return Err("Expected a child text copy source".into());
    };
    let agent = state
        .subagents
        .iter()
        .find(|agent| agent.id == *id)
        .ok_or("Child text copy is no longer available")?;
    let sources = state
        .text_copy_sources
        .children
        .get(id)
        .ok_or("Child text copy is unavailable")?;
    let (index, current) = sources
        .iter()
        .enumerate()
        .find_map(|(index, current)| {
            current
                .as_ref()
                .filter(|current| {
                    current.cache == source.cache
                        && current.scope == source.scope
                        && current.token == source.token
                        && current.revision == source.revision
                })
                .map(|current| (index, current))
        })
        .ok_or("Child text copy source changed or was trimmed")?;
    let body = agent
        .items
        .get(index)
        .and_then(|item| text(item))
        .ok_or("Child text copy item is unavailable")?;
    if !valid(current, id, body) || source.bytes > current.bytes || offset > source.bytes {
        return Err("Child text copy prefix is unavailable".into());
    }
    let start = usize::try_from(offset).map_err(|_| "Invalid child text copy offset")?;
    let length = usize::try_from(source.bytes).map_err(|_| "Invalid child text copy length")?;
    let prefix = body
        .get(..length)
        .ok_or("Child text copy length splits UTF-8")?;
    if !prefix.is_char_boundary(start) {
        return Err("Child text copy offset splits UTF-8".into());
    }
    let mut end = start.saturating_add(TEXT_COPY_PAGE_BYTES).min(length);
    while !prefix.is_char_boundary(end) {
        end -= 1;
    }
    Ok(TextCopyPage {
        source: source.clone(),
        offset,
        text: prefix[start..end].into(),
        done: end == length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{DeliveryKind, Item, ItemContent, Phase, Role, ToolStatus};

    fn apply(state: &mut Snapshot, event: Event) {
        super::super::apply_event(state, &mut BTreeMap::new(), event);
    }
    fn item(id: &str, body: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }
    fn full(state: &mut Snapshot, child: &str, id: &str, body: &str) {
        apply(
            state,
            Event::SubagentItem {
                id: child.into(),
                item: item(id, body),
            },
        );
    }
    fn delta(state: &mut Snapshot, child: &str, id: &str, body: &str) {
        apply(
            state,
            Event::SubagentTextDelta {
                id: child.into(),
                item_id: id.into(),
                role: Role::Assistant,
                thinking: false,
                delta: body.into(),
            },
        );
    }
    fn source(state: &Snapshot, child: &str, index: usize) -> TextCopySource {
        state.text_copy_sources.children[child][index]
            .clone()
            .expect("complete child source")
    }

    fn context(id: &str, body: &str) -> Item {
        Item {
            role: Role::System,
            content: ItemContent::Delivery {
                kind: DeliveryKind::Notice,
                from: String::new(),
                sender: String::new(),
                summary: "Claude context".into(),
                body: body.into(),
            },
            ..item(id, "")
        }
    }

    #[test]
    fn provider_context_copy_keeps_child_identity_and_rejects_rewrite_trim_and_user_notice() {
        let mut state = Snapshot::default();
        let publish = |state: &mut Snapshot, child: &str, item| {
            apply(
                state,
                Event::SubagentItem {
                    id: child.into(),
                    item,
                },
            );
        };
        publish(&mut state, "first", context("same", "original λ\r\n"));
        publish(&mut state, "second", context("same", "other child"));
        let first = source(&state, "first", 0);
        let second = source(&state, "second", 0);
        assert_eq!(read(&state, &first, 0).unwrap().text, "original λ\r\n");
        assert_eq!(read(&state, &second, 0).unwrap().text, "other child");
        let mut wrong_child = first.clone();
        wrong_child.scope = second.scope.clone();
        assert!(read(&state, &wrong_child, 0).is_err());
        publish(
            &mut state,
            "first",
            context("same", "original λ\r\n appended"),
        );
        assert_eq!(read(&state, &first, 0).unwrap().text, "original λ\r\n");
        publish(&mut state, "first", context("same", "rewritten"));
        assert!(read(&state, &first, 0).is_err());
        let rewritten = source(&state, "first", 0);
        publish(
            &mut state,
            "first",
            context("same", &"x".repeat(MAX_BYTES + 1)),
        );
        assert!(state.text_copy_sources.children["first"][0].is_none());
        assert!(read(&state, &rewritten, 0).is_err());
        publish(&mut state, "first", context("same", "recovered full body"));
        assert_eq!(
            read(&state, &source(&state, "first", 0), 0).unwrap().text,
            "recovered full body"
        );
        let mut user_notice = context("same", "ordinary visible notice");
        user_notice.role = Role::User;
        publish(&mut state, "first", user_notice);
        assert!(state.text_copy_sources.children["first"][0].is_none());
        assert_eq!(read(&state, &second, 0).unwrap().text, "other child");
    }

    #[test]
    fn actual_child_full_and_delta_preserve_verified_prefix_but_rewrite_revokes_it() {
        let mut state = Snapshot::default();
        full(&mut state, "child", "same", "猫");
        let initial = source(&state, "child", 0);
        let old_item = std::sync::Arc::downgrade(&state.subagents[0].items[0]);
        delta(&mut state, "child", "same", "犬");
        assert!(
            old_item.upgrade().is_none(),
            "copy metadata must not retain the original Arc"
        );
        assert_eq!(source(&state, "child", 0).cache, initial.cache);
        assert_eq!(read(&state, &initial, 0).unwrap().text, "猫");
        let streamed = source(&state, "child", 0);
        full(&mut state, "child", "same", "猫犬\r\n ");
        assert_eq!(source(&state, "child", 0).cache, initial.cache);
        assert_eq!(read(&state, &streamed, 0).unwrap().text, "猫犬");
        assert_eq!(
            read(&state, &source(&state, "child", 0), 0).unwrap().text,
            "猫犬\r\n "
        );
        full(&mut state, "child", "same", "rewritten");
        assert!(read(&state, &initial, 0).is_err());
        assert!(read(&state, &streamed, 0).is_err());
        assert_ne!(source(&state, "child", 0).cache, initial.cache);
        assert_eq!(
            read(&state, &source(&state, "child", 0), 0).unwrap().text,
            "rewritten"
        );
    }

    #[test]
    fn unknown_initial_delta_and_actual_large_item_trim_never_authorize_suffixes() {
        let mut state = Snapshot::default();
        delta(&mut state, "child", "same", "suffix");
        assert!(state.text_copy_sources.children["child"][0].is_none());
        delta(&mut state, "child", "same", " more");
        assert!(state.text_copy_sources.children["child"][0].is_none());
        full(&mut state, "child", "same", "complete");
        let complete = source(&state, "child", 0);
        full(&mut state, "child", "same", &"x".repeat(MAX_BYTES + 1));
        assert!(state.subagents[0].items[0].content.byte_len() < MAX_BYTES);
        assert!(state.text_copy_sources.children["child"][0].is_none());
        assert!(read(&state, &complete, 0).is_err());
        delta(&mut state, "child", "same", " tail");
        assert!(state.text_copy_sources.children["child"][0].is_none());
        // The literal body can also be legitimate provider text: ownership is
        // determined before trimming, never by matching a placeholder string.
        full(
            &mut state,
            "child",
            "same",
            "[This large item is available in the provider transcript.]",
        );
        assert!(read(&state, &source(&state, "child", 0), 0).is_ok());
        full(&mut state, "child", "same", &"x".repeat(MAX_BYTES));
        let at_limit = source(&state, "child", 0);
        assert_eq!(read(&state, &at_limit, 0).unwrap().text.len(), MAX_BYTES);
        delta(&mut state, "child", "same", "x");
        assert!(state.text_copy_sources.children["child"][0].is_none());
        assert!(read(&state, &at_limit, 0).is_err());
    }

    #[test]
    fn actual_child_item_count_and_byte_eviction_realign_sources() {
        let mut state = Snapshot::default();
        full(&mut state, "child", "first", "first");
        let first = source(&state, "child", 0);
        for index in 0..64 {
            full(&mut state, "child", &format!("item-{index}"), "x");
        }
        assert_eq!(state.subagents[0].items.len(), 64);
        assert_eq!(state.text_copy_sources.children["child"].len(), 64);
        assert!(read(&state, &first, 0).is_err());
        for (index, item) in state.subagents[0].items.iter().enumerate() {
            assert_eq!(
                read(&state, &source(&state, "child", index), 0)
                    .unwrap()
                    .text,
                text(item).unwrap()
            );
        }
        let removed = source(&state, "child", 63);
        full(&mut state, "child", "large", &"z".repeat(MAX_BYTES));
        assert_eq!(state.subagents[0].items.len(), 1);
        assert_eq!(state.text_copy_sources.children["child"].len(), 1);
        assert!(read(&state, &removed, 0).is_err());
        assert_eq!(
            read(&state, &source(&state, "child", 0), 0)
                .unwrap()
                .text
                .len(),
            MAX_BYTES
        );
    }

    #[test]
    fn actual_child_global_rich_history_retirement_and_status_eviction_remove_authority() {
        let mut state = Snapshot::default();
        full(&mut state, "child-0", "same", "first");
        let first = source(&state, "child-0", 0);
        for index in 1..33 {
            full(&mut state, &format!("child-{index}"), "same", "other");
        }
        assert_eq!(
            state.subagents.len(),
            33,
            "active interruption identities survive"
        );
        assert!(state.subagents[0].items.is_empty());
        assert!(state.text_copy_sources.children["child-0"].is_empty());
        assert!(read(&state, &first, 0).is_err());
        let latest = source(&state, "child-32", 0);
        assert_eq!(read(&state, &latest, 0).unwrap().text, "other");
        let mut wrong_child = latest.clone();
        wrong_child.scope = TextCopyScope::Subagent {
            id: "child-31".into(),
        };
        assert!(read(&state, &wrong_child, 0).is_err());
        apply(
            &mut state,
            Event::Subagent {
                id: "child-0".into(),
                kind: String::new(),
                description: String::new(),
                status: Some(ToolStatus::Succeeded),
                background: None,
                model: None,
                waiting: None,
                detail: None,
            },
        );
        assert_eq!(state.subagents.len(), 32);
        assert!(!state.text_copy_sources.children.contains_key("child-0"));
        assert_eq!(read(&state, &latest, 0).unwrap().text, "other");
    }

    #[test]
    fn actual_child_remove_reset_and_utf8_boundaries_refuse_stale_authority() {
        let mut state = Snapshot::default();
        full(&mut state, "child", "same", "猫犬");
        let initial = source(&state, "child", 0);
        assert_eq!(read(&state, &initial, 3).unwrap().text, "犬");
        assert!(read(&state, &initial, 1).is_err());
        assert!(read(&state, &initial, 7).is_err());
        let mut split = initial.clone();
        split.bytes = 4;
        assert!(read(&state, &split, 0).is_err());
        let mut too_long = initial.clone();
        too_long.bytes = 7;
        assert!(read(&state, &too_long, 0).is_err());
        apply(
            &mut state,
            Event::SubagentItemsRemoved {
                id: "child".into(),
                ids: vec!["same".into()],
            },
        );
        assert!(state.text_copy_sources.children["child"].is_empty());
        assert!(read(&state, &initial, 0).is_err());
        full(&mut state, "child", "same", "猫犬");
        let recreated = source(&state, "child", 0);
        assert_ne!(recreated.cache, initial.cache);
        apply(
            &mut state,
            Event::Reset {
                reason: "new conversation".into(),
            },
        );
        assert!(state.text_copy_sources.children.is_empty());
        assert!(read(&state, &recreated, 0).is_err());
        full(&mut state, "child", "same", "猫犬");
        assert_ne!(source(&state, "child", 0).cache, recreated.cache);
    }
}
