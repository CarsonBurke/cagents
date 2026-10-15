//! Give copied private drafts their own identities before a new window opens.
use super::*;

impl StateStore {
    /// New Window preserves private conversation choices, but cannot publish
    /// two distinct issued conversations under the same copied pane ID. The
    /// source stays read-only; rewrite only the unpublished destination.
    pub fn copy_window_snapshot(source: &Path, destination: &Path, boot: BootId) -> Result<()> {
        Self::copy_snapshot_prepared(source, destination, |path| {
            let store = Self::open(path, boot)?;
            let mut renamed = HashMap::new();
            for summary in store.list_workspaces()? {
                let Some(manifest) = store.load_workspace_manifest(summary.id)? else { continue };
                if manifest.configured_chat_drafts.is_empty() {
                    continue;
                }
                let Some(restored) = store.load_workspace_restored(summary.id, None)? else { continue };
                if !restored.warnings.is_empty() {
                    return Err(StoreError::InvalidState(
                        "Cannot copy configured drafts while this workspace has unreadable cards. Restore and repair the original window first.".into(),
                    ));
                }
                let mut state = restored.state;
                let ids = fork_private_drafts(&mut state)?;
                if ids.is_empty() {
                    continue;
                }
                let base = store.stored_generations(state.id)?;
                store.save_workspace(&state, SnapshotBase { workspace: base.workspace, cards: &base.cards })?;
                renamed.extend(ids);
            }
            if !renamed.is_empty()
                && let Some(view) = store.load_window_view()?
            {
                let mut view: Value = serde_json::from_str(&view)?;
                if let Some(titles) = view.get_mut("user_titles").and_then(Value::as_object_mut) {
                    for (old, new) in renamed {
                        if let Some(title) = titles.remove(&old.to_string()) {
                            titles.insert(new.to_string(), title);
                        }
                    }
                }
                store.save_window_view(&serde_json::to_string(&view)?)?;
            }
            // This private staging file is published without sidecars. Unlike
            // ordinary shutdown housekeeping, a blocked checkpoint must refuse
            // publication rather than lose committed identity remapping.
            let inner = store.lock_inner()?;
            let (busy, frames, checkpointed) = inner.connection.query_row(
                "PRAGMA wal_checkpoint(TRUNCATE)", [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )?;
            if busy != 0 || frames != checkpointed {
                return Err(StoreError::InvalidState("Could not commit the private new-window draft identities".into()));
            }
            Ok(())
        })
    }
}

fn fork_private_drafts(state: &mut WorkspaceState) -> Result<HashMap<CardId, CardId>> {
    let mut renamed = HashMap::new();
    for card in &mut state.cards {
        if !card.existence.is_open() {
            continue;
        }
        let CardKind::Chat(session) = &mut card.kind else { continue };
        if !session.is_unstarted() {
            continue;
        }
        let Some(intent) = state.configured_chat_drafts.get_mut(&card.id) else { continue };
        if !intent.matches_draft(session) || intent.validate().is_err() {
            return Err(StoreError::InvalidState(
                "Cannot copy a configured draft whose saved choices do not match its conversation. Choose its account again or start a new draft in the original window.".into(),
            ));
        }
        let old = card.id;
        card.id = CardId::new();
        session.id = TerminalId::new();
        intent.id = session.id;
        renamed.insert(old, card.id);
    }
    for (&old, &new) in &renamed {
        if state.focused == Some(old) { state.focused = Some(new); }
        for focus in state.worktree_focus.values_mut() {
            if *focus == old { *focus = new; }
        }
        for channel in &mut state.channels {
            for card in &mut channel.cards {
                if *card == old { *card = new; }
            }
            if channel.focused == Some(old) { channel.focused = Some(new); }
        }
        if let Some(value) = state.card_scopes.remove(&old) { state.card_scopes.insert(new, value); }
        if let Some(value) = state.chat_drafts.remove(&old) { state.chat_drafts.insert(new, value); }
        if let Some(value) = state.configured_chat_drafts.remove(&old) { state.configured_chat_drafts.insert(new, value); }
        if let Some(value) = state.repository_navigation.remove(&old) { state.repository_navigation.insert(new, value); }
        if state.local_only_cards.remove(&old) { state.local_only_cards.insert(new); }
        if state.pending_terminal_creations.remove(&old) { state.pending_terminal_creations.insert(new); }
    }
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, chat::{ConfiguredChatStart, Launch, Session}, model::LaunchRecipe};

    fn configured_workspace() -> WorkspaceState {
        let mut state = WorkspaceState::default();
        let card = CardId::new();
        let session = Session {
            id: TerminalId::new(),
            launch: Launch { provider: Provider::Codex, session_id: None, recipe: LaunchRecipe {
                program: "/private/fixture-codex".into(), arguments: vec![], working_directory: "/repo".into(),
                environment: Default::default(), unset_environment: Default::default(),
            } }, proof: None, terminal: None, owned_codex_listener: None, native_continuity: Default::default(),
        };
        state.configured_chat_drafts.insert(card, ConfiguredChatStart {
            id: session.id, provider: Provider::Codex, working_directory: "/repo".into(), account: "codex/system".parse().unwrap(),
            model: Some("private-model".into()), effort: Some("low".into()), permission_mode: None, automatic: true,
        });
        state.cards.push(CardState { id: card, generation: 0, title: "private draft".into(), width_fraction: 0.6,
            existence: Existence::Open, exposure: Exposure::Visible, residency: Residency::Cold, kind: CardKind::Chat(session), });
        state.focused = Some(card);
        state.worktree_focus.insert("/repo".into(), card);
        state.card_scopes.insert(card, "/repo".into());
        state.chat_drafts.insert(card, "private composer".into());
        state.local_only_cards.insert(card);
        let channel = ChannelId::new();
        state.channels.push(ProjectChannel { id: channel, project_id: ProjectId::new(), title: "private".into(), title_source: Default::default(),
            ordinal: 0, shared: true, cards: vec![card], focused: Some(card) });
        state.active_channel = Some(channel);
        // A genuine legacy draft stays identified and shared as before.
        let mut legacy = state.cards[0].clone();
        legacy.id = CardId::new();
        let CardKind::Chat(session) = &mut legacy.kind else { unreachable!() };
        session.id = TerminalId::new();
        state.channels[0].cards.push(legacy.id);
        state.cards.push(legacy);
        state
    }

    #[test]
    fn new_window_copy_forks_every_private_draft_and_references_without_rewriting_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite3");
        let boot = BootId::new("private-window-copy");
        let store = StateStore::open(&source, boot.clone()).unwrap();
        let states = [configured_workspace(), configured_workspace()];
        for state in &states {
            store.save_workspace(state, SnapshotBase { workspace: 0, cards: &BTreeMap::new() }).unwrap();
        }
        let titles = states.iter().map(|state| (state.cards[0].id.to_string(), "manual title".to_owned())).collect::<HashMap<_, _>>();
        store.save_window_view(&serde_json::json!({ "user_titles": titles, "other": [1,2,3] }).to_string()).unwrap();
        let destination = directory.path().join("copied.sqlite3");
        StateStore::copy_window_snapshot(&source, &destination, boot.clone()).unwrap();
        let copied = StateStore::open(&destination, boot).unwrap();
        let view: Value = serde_json::from_str(&copied.load_window_view().unwrap().unwrap()).unwrap();
        assert_eq!(view["other"], serde_json::json!([1,2,3]));
        for original in &states {
            let old = original.cards[0].id;
            let state = copied.load_workspace(original.id, None).unwrap().unwrap();
            let (&new, intent) = state.configured_chat_drafts.iter().next().unwrap();
            assert_ne!(new, old);
            let CardKind::Chat(session) = &state.cards.iter().find(|card| card.id == new).unwrap().kind else { panic!("copied draft") };
            assert_ne!(session.id, original.configured_chat_drafts[&old].id);
            assert!(intent.matches_draft(session));
            let mut normalized = intent.clone(); normalized.id = original.configured_chat_drafts[&old].id;
            assert_eq!(normalized, original.configured_chat_drafts[&old]);
            assert_eq!(state.focused, Some(new));
            assert_eq!(state.worktree_focus[Path::new("/repo")], new);
            assert_eq!(state.channels[0].cards, vec![new, original.cards[1].id]);
            assert_eq!(state.channels[0].focused, Some(new));
            assert_eq!(state.card_scopes[&new], PathBuf::from("/repo"));
            assert_eq!(state.chat_drafts[&new], "private composer");
            assert!(state.local_only_cards.contains(&new) && !state.local_only_cards.contains(&old));
            assert_eq!(state.cards.iter().find(|card| card.id == original.cards[1].id).unwrap().kind, original.cards[1].kind);
            assert!(!state.cards.iter().find(|card| card.id == old).unwrap().existence.is_open());
            assert_eq!(view["user_titles"][new.to_string().as_str()], "manual title");
            assert!(view["user_titles"].get(old.to_string().as_str()).is_none());
            let untouched = store.load_workspace(original.id, None).unwrap().unwrap();
            assert_eq!(untouched.configured_chat_drafts, original.configured_chat_drafts);
            assert!(untouched.cards.iter().find(|card| card.id == old).unwrap().existence.is_open());
        }
    }

    #[test]
    fn new_window_copy_refuses_invalid_intent_without_healing_or_publishing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite3");
        let boot = BootId::new("invalid-window-copy");
        let store = StateStore::open(&source, boot.clone()).unwrap();
        let mut state = configured_workspace();
        let old = state.cards[0].id;
        store.save_workspace(&state, SnapshotBase { workspace: 0, cards: &BTreeMap::new() }).unwrap();
        state.configured_chat_drafts.get_mut(&old).unwrap().id = TerminalId::new();
        let mut inner = store.lock_inner().unwrap();
        let transaction = inner.connection.transaction().unwrap();
        set_metadata(&transaction, &format!("workspace:{}:navigation:configured_chat_drafts", state.id), &to_json(&state.configured_chat_drafts).unwrap()).unwrap();
        transaction.commit().unwrap();
        drop(inner);
        let destination = directory.path().join("refused.sqlite3");
        let error = StateStore::copy_window_snapshot(&source, &destination, boot).unwrap_err();
        assert!(error.to_string().contains("saved choices"), "{error}");
        assert!(!destination.exists());
        assert_eq!(store.load_workspace(state.id, None).unwrap().unwrap().configured_chat_drafts, state.configured_chat_drafts);
    }
}
