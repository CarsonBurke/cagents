//! Restore explicit decisions before history bodies enter the presentation
//! cache. Source sidecars grant only exact historical reads, never a live
//! session, original prompt, or a current approval decision.
use super::{Authority, annotations, normalize_item};
use crate::chat::{Event, ToolStatus};
use super::super::turn_outcomes::{account_origin, history_origins, physical_scope};

impl Authority {
    pub(in crate::chat::runtime) fn normalize_history_events(&mut self, events: &mut [Event]) -> Result<usize, String> {
        let proofs = history_origins(events);
        if proofs.is_empty() { return Ok(0); }
        let captured = self.scope.as_ref().ok().map(account_origin);
        let mut groups = Vec::new();
        let mut warning = None;
        for event in events.iter() {
            let (Event::History { items, .. } | Event::LatestHistory { items, .. }) = event else { continue; };
            for item in items {
                if item.content.status().is_none_or(|status| status == ToolStatus::Denied) { continue; }
                let Some(Some(proof)) = proofs.get(&item.id) else { continue; };
                if captured.as_ref().is_some_and(|account| account != &proof.account) || groups.contains(proof) { continue; }
                if groups.len() == 8 {
                    warning.get_or_insert_with(|| "History decision source limit exceeded".to_owned());
                    continue;
                }
                groups.push(proof.clone());
            }
        }
        let mut changed = 0;
        for proof in groups {
            let result = (|| -> Result<(), String> {
                let scope = physical_scope(&proof.account)?;
                let live = self.session.as_deref() == Some(proof.session_id.as_str()) && captured.is_some();
                let store = if live {
                    self.store.as_ref().ok_or("Qualified denial storage is unavailable")?
                } else {
                    if self.turn_history_store.as_ref().is_none_or(|(account, session, _)| account != &proof.account || session != &proof.session_id) {
                        #[cfg(test)]
                        let existing = if let Some(root) = &self.root {
                            annotations::storage::Store::existing_for_test(root, &scope, &proof.session_id)
                        } else { annotations::storage::Store::existing_installation(&scope, &proof.session_id) };
                        #[cfg(not(test))]
                        let existing = annotations::storage::Store::existing_installation(&scope, &proof.session_id);
                        let Some(store) = existing? else { return Ok(()); };
                        self.turn_history_store = Some((proof.account.clone(), proof.session_id.clone(), store));
                    }
                    &self.turn_history_store.as_ref().expect("qualified history decision store").2
                };
                for event in events.iter_mut() {
                    let (Event::History { items, .. } | Event::LatestHistory { items, .. }) = event else { continue; };
                    for item in items {
                        if proofs.get(&item.id).and_then(Option::as_ref) != Some(&proof) { continue; }
                        let previous = item.content.status();
                        normalize_item(store, None, item)?;
                        if item.content.status() != previous { changed += 1; }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result { warning.get_or_insert(error); }
        }
        if let Some(warning) = warning { Err(warning) } else { Ok(changed) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account::scope::LaunchAccountScope, agent::Provider, chat::{Item, ItemContent, Role, annotations::{ApprovalDecision, ApprovalKey, ApprovalUpdate}}};
    use std::path::Path;

    fn scope(root: &Path, name: &str) -> LaunchAccountScope {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        LaunchAccountScope::resolve(Provider::Claude, root, |key| (key == "CLAUDE_CONFIG_DIR").then(|| home.as_os_str().to_owned())).unwrap()
    }
    fn item(id: &str) -> Item {
        Item { id: id.into(), role: Role::Assistant, at: None, started_at: None,
            content: ItemContent::Tool { name: "Bash".into(), input: "{}".into(), output: "generic native failure".into(), status: ToolStatus::Failed } }
    }
    fn history(scope: &LaunchAccountScope, session: &str, ids: &[&str], proved: &[&str]) -> Vec<Event> {
        vec![Event::History { items: ids.iter().map(|id| item(id)).collect(), has_earlier: false },
            Event::HistoryOrigins { account: account_origin(scope), session_id: session.into(), item_ids: proved.iter().map(|id| (*id).into()).collect() }]
    }
    fn status(events: &[Event], id: &str) -> ToolStatus {
        events.iter().find_map(|event| match event {
            Event::History { items, .. } | Event::LatestHistory { items, .. } => items.iter().find(|item| item.id == id).and_then(|item| item.content.status()),
            _ => None,
        }).unwrap()
    }
    fn deny(store: &mut annotations::storage::Store, id: &str) {
        let key = ApprovalKey { agent_id: None, item_id: id.into() };
        store.approval(&key, "request-1", ApprovalDecision::Awaiting).unwrap();
        store.approval(&key, "request-1", ApprovalDecision::Denied).unwrap();
    }
    #[test]
    fn cold_history_denials_require_exact_source_and_grant_no_original_authority() {
        let root = annotations::storage::test_directory();
        let source = scope(root.path(), "source");
        let other = scope(root.path(), "other");
        let path = root.path().join("annotations");
        let mut durable = annotations::storage::Store::for_test(&path, &source, "session-A").unwrap();
        for id in ["known", "missing-proof", "conflicting"] { deny(&mut durable, id); }
        drop(durable);
        for _ in 0..2 {
            let mut authority = Authority::for_history_outcomes_test(path.clone());
            let mut events = history(&source, "session-A", &["known", "missing-proof", "conflicting"], &["known", "conflicting"]);
            events.push(Event::HistoryOrigins { account: account_origin(&source), session_id: "session-B".into(), item_ids: vec!["conflicting".into()] });
            assert_eq!(authority.normalize_history_events(&mut events).unwrap(), 1);
            assert_eq!(status(&events, "known"), ToolStatus::Denied);
            assert_eq!(status(&events, "missing-proof"), ToolStatus::Failed);
            assert_eq!(status(&events, "conflicting"), ToolStatus::Failed);
            assert!(authority.session().is_none());
            assert!(authority.original(Some("known"), 0, 4).is_err());
            let mut wrong_session = history(&source, "session-B", &["known"], &["known"]);
            assert_eq!(authority.normalize_history_events(&mut wrong_session).unwrap(), 0);
            let mut wrong_account = history(&other, "session-A", &["known"], &["known"]);
            assert_eq!(authority.normalize_history_events(&mut wrong_account).unwrap(), 0);
            assert_eq!(status(&wrong_account, "known"), ToolStatus::Failed);
        }
        // Absence does not create even an empty namespace or alias index.
        let absent = root.path().join("absent-annotations");
        let mut authority = Authority::for_history_outcomes_test(absent.clone());
        assert_eq!(authority.normalize_history_events(&mut history(&source, "session-A", &["known"], &["known"])).unwrap(), 0);
        assert!(!absent.exists());
        let mut reader = annotations::storage::Store::existing_for_test(&path, &source, "session-A").unwrap().unwrap();
        assert!(reader.latest_user("no-write-authority").is_err(), "archive connection must be SQLite read-only");
    }
    #[test]
    fn new_awaiting_decision_is_applied_before_history_normalization() {
        let root = annotations::storage::test_directory();
        let scope = scope(root.path(), "source");
        let path = root.path().join("annotations");
        let mut durable = annotations::storage::Store::for_test(&path, &scope, "session-A").unwrap();
        deny(&mut durable, "known");
        let mut authority = Authority::for_turn_outcomes_test(scope.clone(), path, "session-A");
        let mut events = history(&scope, "session-A", &["known"], &["known"]);
        let update = |request: &str| ApprovalUpdate { session_id: Some("session-A".into()), key: ApprovalKey { agent_id: None, item_id: "known".into() }, request_id: request.into(), decision: ApprovalDecision::Awaiting };
        assert!(authority.apply(None, vec![update("request-1")], &mut events).is_none());
        assert_eq!(authority.normalize_history_events(&mut events).unwrap(), 1);
        assert_eq!(status(&events, "known"), ToolStatus::Denied, "replayed request cannot reset denial");
        let mut events = history(&scope, "session-A", &["known"], &["known"]);
        assert!(authority.apply(None, vec![update("request-2")], &mut events).is_none());
        assert_eq!(authority.normalize_history_events(&mut events).unwrap(), 0);
        assert_eq!(status(&events, "known"), ToolStatus::Failed, "real new decision must not inherit the old denial");
    }
    #[test]
    fn live_apply_cannot_lend_its_session_to_unknown_or_conflicting_history() {
        let root = annotations::storage::test_directory();
        let source = scope(root.path(), "source");
        let other = scope(root.path(), "other");
        let path = root.path().join("annotations");
        let mut durable = annotations::storage::Store::for_test(&path, &source, "session-A").unwrap();
        deny(&mut durable, "same-native-id");
        let mut authority = Authority::for_turn_outcomes_test(source.clone(), path, "session-A");
        let mut cases = vec![
            (history(&source, "session-A", &["same-native-id"], &["same-native-id"]), ToolStatus::Denied),
            (history(&source, "session-B", &["same-native-id"], &["same-native-id"]), ToolStatus::Failed),
            (history(&other, "session-A", &["same-native-id"], &["same-native-id"]), ToolStatus::Failed),
            (vec![Event::History { items: vec![item("same-native-id")], has_earlier: false }], ToolStatus::Failed),
        ];
        let mut conflicting = history(&source, "session-A", &["same-native-id"], &["same-native-id"]);
        conflicting.push(Event::HistoryOrigins { account: account_origin(&source), session_id: "session-B".into(), item_ids: vec!["same-native-id".into()] });
        cases.push((conflicting, ToolStatus::Failed));
        for (mut events, expected) in cases {
            assert!(authority.apply(None, vec![], &mut events).is_none());
            assert_eq!(status(&events, "same-native-id"), ToolStatus::Failed, "live first pass must leave history untouched");
            authority.normalize_history_events(&mut events).unwrap();
            assert_eq!(status(&events, "same-native-id"), expected);
        }
    }

    #[test]
    fn actual_codex_initialize_resume_page_denials_reach_qualified_cache_ingestion() {
        use crate::chat::{protocol::{codex, ProcessSpec, TransportSpec}, annotations::ApprovalDecision};
        use serde_json::json;
        let root = annotations::storage::test_directory();
        let home = root.path().join("codex-home"); std::fs::create_dir(&home).unwrap();
        let source = LaunchAccountScope::resolve(Provider::Codex, root.path(), |name| (name == "CODEX_HOME").then(|| home.as_os_str().to_owned())).unwrap();
        let mut adapter = codex::annotation_history_test_adapter(source.clone());
        let initialize = adapter.initialize().unwrap();
        let initialized = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":source.canonical_home}})).unwrap();
        let resume = initialized.writes.iter().find(|frame| frame["method"] == "thread/resume").expect("real initialize must request native resume");
        let mut initial = adapter.receive(json!({"id":resume["id"],"result":{"thread":{"id":"thread-exact","status":{"type":"idle"}},"initialTurnsPage":{"data":[{"id":"native-turn","status":"failed","items":[{"id":"tool","type":"commandExecution","command":"git status","cwd":"/repo","status":"failed","aggregatedOutput":"generic native replay"}]}],"nextCursor":null}}})).unwrap();
        let path = root.path().join("annotations");
        let mut durable = annotations::storage::Store::for_test(&path, &source, "thread-exact").unwrap();
        let key = ApprovalKey { agent_id: None, item_id: "native-turn/tool".into() };
        durable.approval(&key, "request", ApprovalDecision::Awaiting).unwrap();
        durable.approval(&key, "request", ApprovalDecision::Denied).unwrap();
        drop(durable);
        let mut authority = Authority::for_history_outcomes_test(path);
        assert_eq!(authority.normalize_history_events(&mut initial.events).unwrap(), 1);
        let spec = ProcessSpec { execution: None, messages: None, transport: TransportSpec::History, program: "/bin/cat".into(), arguments: vec![], working_directory: root.path().into(), environment: Default::default(), unset_environment: Default::default() };
        let mut cache = super::super::super::preflight(&spec, &mut initial).unwrap();
        let window = cache.window().unwrap();
        assert_eq!(window.items.iter().find(|item| item.id == "native-turn/tool").unwrap().content.status(), Some(ToolStatus::Denied));
        assert!(authority.session().is_none(), "source projection must not confirm live/original authority");
    }

    #[test]
    fn actual_readonly_claude_history_normalizes_before_preflight_cache_ingestion() {
        use crate::chat::{Launch, protocol::claude};
        use serde_json::json;
        use std::io::Write;
        let root = annotations::storage::test_directory();
        let cwd = root.path().join("removed-project");
        std::fs::create_dir(&cwd).unwrap();
        let source = scope(root.path(), "config");
        let project: String = cwd.to_str().unwrap().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let directory = source.canonical_home.join("projects").join(project);
        std::fs::create_dir_all(&directory).unwrap();
        let session = "00000000-0000-4000-8000-000000000002";
        let mut file = std::fs::File::create(directory.join(format!("{session}.jsonl"))).unwrap();
        writeln!(file, "{}", json!({"type":"assistant","uuid":"call","sessionId":session,"message":{"id":"assistant-call","role":"assistant","content":[{"type":"tool_use","id":"tool-known","name":"Bash","input":{"command":"git status"}}]}})).unwrap();
        writeln!(file, "{}", json!({"type":"user","uuid":"result","parentUuid":"call","sessionId":session,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-known","is_error":true,"content":"generic failure on replay"}]}})).unwrap();
        drop(file);
        std::fs::remove_dir(&cwd).unwrap();
        let launch = Launch { provider: Provider::Claude, session_id: Some(session.into()), recipe: crate::model::LaunchRecipe {
            program: "claude".into(), arguments: vec![], working_directory: cwd,
            environment: std::collections::BTreeMap::from([("CLAUDE_CONFIG_DIR".into(), source.canonical_home.to_string_lossy().into_owned())]), unset_environment: Default::default(),
        }};
        let (spec, mut adapter) = claude::create(&launch).unwrap();
        assert!(matches!(spec.transport, crate::chat::protocol::TransportSpec::History));
        let mut initial = adapter.initialize().unwrap();
        assert!(initial.events.iter().all(|event| !matches!(event, Event::Session { .. })));
        let native = initial.events.iter().find_map(|event| match event {
            Event::History { items, .. } => items.iter().find(|item| item.content.status() == Some(ToolStatus::Failed)).cloned(), _ => None,
        }).expect("actual history decoder must emit the replayed failed tool");
        let path = root.path().join("annotations");
        let mut durable = annotations::storage::Store::for_test(&path, &source, session).unwrap();
        deny(&mut durable, &native.id);
        drop(durable);
        let mut authority = Authority::for_history_outcomes_test(path);
        assert_eq!(authority.normalize_history_events(&mut initial.events).unwrap(), 1);
        let mut cache = super::super::super::preflight(&spec, &mut initial).unwrap();
        let window = cache.window().unwrap();
        assert_eq!(window.items.iter().find(|item| item.id == native.id).unwrap().content.status(), Some(ToolStatus::Denied));
        assert!(authority.session().is_none());
        assert!(authority.original(Some(&native.id), 0, 4).is_err());
    }
}
