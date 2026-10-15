//! Historical page provenance belongs to its confirmed request and immutable
//! physical launch scope. Neither a mutable thread selector nor a resume recipe
//! may qualify the response after the request was sent.
use super::*;
use crate::chat::turn_outcomes::{HistoryAccountOrigin, validate_history_origins};

impl Codex {
    pub(super) fn history_annotation_origins(&self, scope: &crate::account::scope::LaunchAccountScope, thread: &str, begin: usize, step: &mut Step) {
        let account = HistoryAccountOrigin { provider: scope.provider, canonical_home: scope.canonical_home.clone(), device: scope.device, inode: scope.inode };
        if account.validate().is_err() { return; }
        let mut batches = Vec::new();
        let mut batch = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut total = 0usize;
        let mut bytes = 96 + account.canonical_home.as_os_str().len() + thread.len();
        let base = bytes;
        for event in &step.events[begin..] {
            let (Event::History { items, .. } | Event::LatestHistory { items, .. }) = event else { continue; };
            for item in items {
                if item.id.is_empty() || item.id.len() > 4096 || seen.contains(&item.id) { continue; }
                let cost = item.id.len() + 8 + base;
                if total.saturating_add(cost) > 256 * 1024 { continue; }
                total += cost;
                seen.insert(item.id.clone());
                if batch.len() == 128 || bytes + item.id.len() + 8 > 64 * 1024 {
                    batches.push(std::mem::take(&mut batch)); bytes = base;
                }
                bytes += item.id.len() + 8;
                batch.push(item.id.clone());
            }
        }
        if !batch.is_empty() { batches.push(batch); }
        for item_ids in batches {
            if validate_history_origins(&account, thread, &item_ids).is_ok() {
                step.events.push(Event::HistoryOrigins { account: account.clone(), session_id: thread.into(), item_ids });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::scope::LaunchAccountScope;
    fn captured(root: &std::path::Path) -> LaunchAccountScope {
        let home = root.join("codex-home"); std::fs::create_dir(&home).unwrap();
        LaunchAccountScope::resolve(crate::agent::Provider::Codex, root, |key| (key == "CODEX_HOME").then(|| home.as_os_str().to_owned())).unwrap()
    }
    fn reply(id: &Value) -> Value {
        json!({"id":id,"result":{"data":[{"id":"native-turn","status":"failed","items":[{"id":"native-tool","type":"commandExecution","command":"git status","cwd":"/repo","status":"failed","aggregatedOutput":"generic replay"}]}],"nextCursor":null}})
    }
    fn request(adapter: &mut Codex) -> Value {
        let mut step = Step::default(); adapter.history(&mut step, None).unwrap(); step.writes[0]["id"].clone()
    }
    #[test]
    fn actual_history_rpc_emits_only_confirmed_thread_and_captured_account_origins() {
        let root = crate::chat::annotations::storage::test_directory();
        let scope = captured(root.path());
        let mut adapter = super::super::tests::adapter();
        adapter.expected_home = scope.canonical_home.clone();
        adapter.annotation_scope(Some(scope.clone()));
        let id = request(&mut adapter);
        let step = adapter.receive(reply(&id)).unwrap();
        let origins: Vec<_> = step.events.iter().filter_map(|event| match event {
            Event::HistoryOrigins { account, session_id, item_ids } => Some((account, session_id, item_ids)), _ => None,
        }).collect();
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].1, "thread-exact");
        assert_eq!(origins[0].0.canonical_home, scope.canonical_home);
        assert_eq!(origins[0].0.device, scope.device);
        assert_eq!(origins[0].0.inode, scope.inode);
        assert_eq!(origins[0].2, &["native-turn/native-tool"]);
        assert!(step.events.iter().any(|event| matches!(event, Event::LatestHistory { items, .. } if items.iter().any(|item| item.id == "native-turn/native-tool"))));
    }
    #[test]
    fn requested_thread_or_missing_scope_cannot_grant_history_metadata_authority() {
        let mut adapter = super::super::tests::adapter();
        adapter.attached = false;
        assert!(adapter.history(&mut Step::default(), None).is_err());
        assert!(adapter.pending.is_empty());
        adapter.attached = true;
        let id = request(&mut adapter);
        let step = adapter.receive(reply(&id)).unwrap();
        assert!(step.events.iter().any(|event| matches!(event, Event::LatestHistory { .. })));
        assert!(step.events.iter().all(|event| !matches!(event, Event::HistoryOrigins { .. })));
    }
    #[test]
    fn pending_history_does_not_borrow_replacement_thread_or_home() {
        for replace_home in [false, true] {
            let root = crate::chat::annotations::storage::test_directory();
            let scope = captured(root.path());
            let mut adapter = super::super::tests::adapter();
            adapter.expected_home = scope.canonical_home.clone(); adapter.annotation_scope(Some(scope.clone()));
            let id = request(&mut adapter);
            if replace_home {
                std::fs::rename(&scope.canonical_home, root.path().join("old-home")).unwrap();
                std::fs::create_dir(&scope.canonical_home).unwrap();
            } else { adapter.thread = Some("newer-thread".into()); }
            let step = adapter.receive(reply(&id)).unwrap();
            assert!(step.events.iter().all(|event| !matches!(event, Event::HistoryOrigins { .. })));
            if !replace_home {
                assert!(step.events.iter().all(|event| !matches!(event, Event::History { .. } | Event::LatestHistory { .. })));
                assert!(step.events.iter().any(|event| matches!(event, Event::HistoryError(_))));
            }
        }
    }
}
