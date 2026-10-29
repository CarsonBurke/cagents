//! Give copied private drafts their own identities before a new window opens.
use super::*;

impl StateStore {
    /// New Window preserves private conversation choices, but cannot publish
    /// two distinct issued conversations under the same copied pane ID. The
    /// source stays read-only; rewrite only the unpublished destination.
    pub fn copy_window_snapshot(source: &Path, destination: &Path, boot: BootId) -> Result<()> {
        Self::copy_snapshot_prepared(source, destination, |path| {
            let store = Self::open_connection(path, boot, StoreOptions::default(), false)?;
            let summaries = store.list_workspaces()?;
            let mut inner = store.lock_inner()?;
            let transaction = inner
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut renamed = HashMap::new();
            for summary in summaries {
                let Some(mut manifest) =
                    load_workspace_manifest_connection(&transaction, summary.id)?
                else {
                    continue;
                };
                if manifest.configured_chat_drafts.is_empty() {
                    continue;
                }
                renamed.extend(fork_private_drafts(&transaction, &mut manifest)?);
            }
            if !renamed.is_empty()
                && let Some(view) = metadata(&transaction, "window_view_v1")?
            {
                let mut view: Value = serde_json::from_str(&view)?;
                if let Some(titles) = view.get_mut("user_titles").and_then(Value::as_object_mut) {
                    for (old, new) in renamed {
                        if let Some(title) = titles.remove(&old.to_string()) {
                            titles.insert(new.to_string(), title);
                        }
                    }
                }
                set_metadata(
                    &transaction,
                    "window_view_v1",
                    &serde_json::to_string(&view)?,
                )?;
            }
            transaction.commit()?;
            // This private staging file is published without sidecars. Unlike
            // ordinary shutdown housekeeping, a blocked checkpoint must refuse
            // publication rather than lose committed identity remapping.
            let (busy, frames, checkpointed) =
                inner
                    .connection
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?;
            if busy != 0 || frames != checkpointed {
                return Err(StoreError::InvalidState(
                    "Could not commit the private new-window draft identities".into(),
                ));
            }
            Ok(())
        })
    }
}

fn fork_private_drafts(
    transaction: &Transaction<'_>,
    state: &mut WorkspaceManifest,
) -> Result<HashMap<CardId, CardId>> {
    let mut renamed = HashMap::new();
    let existing = existing_cards(transaction, &state.id.to_string())?;
    let timestamp = now_unix_ms();
    for card in &mut state.cards {
        if !card.existence.is_open() || card.kind != PersistedCardKind::Chat {
            continue;
        }
        let Some(intent) = state.configured_chat_drafts.get_mut(&card.id) else {
            continue;
        };
        // Read only this private payload. Ordinary workspace restore deliberately
        // interprets terminal continuity and trims old browser history, which a
        // targeted identity fork must not write back to unrelated cards.
        let raw: String = transaction.query_row(
            "SELECT content_json FROM cards WHERE card_id = ?1",
            [card.id.to_string()],
            |row| row.get(0),
        )?;
        let mut session: crate::chat::Session = from_json("configured draft", &raw)?;
        if !session.is_unstarted() {
            continue;
        }
        if !intent.matches_draft(&session) || intent.validate().is_err() {
            return Err(StoreError::InvalidState(
                "Cannot copy a configured draft whose saved choices do not match its conversation. Choose its account again or start a new draft in the original window.".into(),
            ));
        }
        let old = card.id;
        card.id = CardId::new();
        session.id = TerminalId::new();
        intent.id = session.id;
        let new = CardState {
            id: card.id,
            generation: 0,
            title: card.title.clone(),
            width_fraction: card.width_fraction,
            existence: card.existence.clone(),
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind: CardKind::Chat(session),
        };
        card.generation = save_card(
            transaction,
            &state.id.to_string(),
            card.ordinal,
            &new,
            0,
            None,
            timestamp,
        )?;
        tombstone_card(
            transaction,
            old,
            &existing[&old],
            &Existence::Closed {
                tombstone: Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: timestamp,
                    reason: Some("private draft copied into a new window".into()),
                },
            },
            timestamp,
        )?;
        renamed.insert(old, card.id);
    }
    if renamed.is_empty() {
        return Ok(renamed);
    }
    for (&old, &new) in &renamed {
        if state.focused == Some(old) {
            state.focused = Some(new);
        }
        for focus in state.worktree_focus.values_mut() {
            if *focus == old {
                *focus = new;
            }
        }
        for channel in &mut state.channels {
            for card in &mut channel.cards {
                if *card == old {
                    *card = new;
                }
            }
            if channel.focused == Some(old) {
                channel.focused = Some(new);
            }
        }
        if let Some(value) = state.card_scopes.remove(&old) {
            state.card_scopes.insert(new, value);
        }
        if let Some(value) = state.chat_drafts.remove(&old) {
            state.chat_drafts.insert(new, value);
        }
        if let Some(value) = state.configured_chat_drafts.remove(&old) {
            state.configured_chat_drafts.insert(new, value);
        }
        if let Some(value) = state.repository_navigation.remove(&old) {
            state.repository_navigation.insert(new, value);
        }
        if state.local_only_cards.remove(&old) {
            state.local_only_cards.insert(new);
        }
        if state.pending_terminal_creations.remove(&old) {
            state.pending_terminal_creations.insert(new);
        }
    }
    let changed = transaction.execute("UPDATE workspaces SET generation = ?1, focused_card_id = ?2, updated_at_unix_ms = ?3 WHERE workspace_id = ?4 AND generation = ?5", params![
        u64_to_i64(next_generation(state.generation, "workspace", state.id)?, "workspace generation")?, state.focused.map(|id| id.to_string()), timestamp, state.id.to_string(), u64_to_i64(state.generation, "workspace generation")?
    ])?;
    if changed != 1 {
        return Err(StoreError::InvalidState(
            "The copied workspace generation changed".into(),
        ));
    }
    for (field, value) in [
        ("worktree_focus", to_json(&state.worktree_focus)?),
        (CHANNELS_FIELD, to_json(&state.channels)?),
        ("card_scopes", to_json(&state.card_scopes)?),
        ("chat_drafts", to_json(&state.chat_drafts)?),
        (
            "configured_chat_drafts",
            to_json(&state.configured_chat_drafts)?,
        ),
        (
            "repository_navigation",
            to_json(&state.repository_navigation)?,
        ),
        ("local_only_cards", to_json(&state.local_only_cards)?),
        (
            "pending_terminal_creations",
            to_json(&state.pending_terminal_creations)?,
        ),
    ] {
        set_metadata(
            transaction,
            &format!("workspace:{}:navigation:{field}", state.id),
            &value,
        )?;
    }
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{ConfiguredChatStart, Launch, Session},
        model::LaunchRecipe,
    };

    fn configured_workspace() -> WorkspaceState {
        let mut state = WorkspaceState::default();
        let card = CardId::new();
        let session = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Codex,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "/private/fixture-codex".into(),
                    arguments: vec![],
                    working_directory: "/repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        state.configured_chat_drafts.insert(
            card,
            ConfiguredChatStart {
                id: session.id,
                provider: Provider::Codex,
                working_directory: "/repo".into(),
                account: "codex/system".parse().unwrap(),
                model: Some("private-model".into()),
                effort: Some("low".into()),
                permission_mode: None,
                automatic: true,
            },
        );
        state.cards.push(CardState {
            id: card,
            generation: 0,
            title: "private draft".into(),
            width_fraction: 0.6,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Cold,
            kind: CardKind::Chat(session),
        });
        state.focused = Some(card);
        state.worktree_focus.insert("/repo".into(), card);
        state.card_scopes.insert(card, "/repo".into());
        state.chat_drafts.insert(card, "private composer".into());
        state.local_only_cards.insert(card);
        let channel = ChannelId::new();
        state.channels.push(ProjectChannel {
            id: channel,
            project_id: ProjectId::new(),
            title: "private".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
        state.active_channel = Some(channel);
        // A genuine legacy draft stays identified and shared as before.
        let mut legacy = state.cards[0].clone();
        legacy.id = CardId::new();
        let CardKind::Chat(session) = &mut legacy.kind else {
            unreachable!()
        };
        session.id = TerminalId::new();
        state.channels[0].cards.push(legacy.id);
        state.cards.push(legacy);
        let mut terminal = state.cards[1].clone();
        terminal.id = CardId::new();
        terminal.kind = CardKind::Terminal(TerminalSession {
            id: TerminalId::new(),
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![],
                working_directory: "/repo".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            continuity: TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("issued-host"),
                boot_id: BootId::new("private-window-copy"),
                host_incarnation: crate::model::HostIncarnation::new("issued-incarnation"),
            },
            transcript_path: None,
            resume_hint: None,
            recovery_origin: None,
            owned_codex_listener: None,
            predecessor: None,
        });
        state.channels[0].cards.push(terminal.id);
        state.cards.push(terminal);
        let mut browser = state.cards[1].clone();
        browser.id = CardId::new();
        browser.kind = CardKind::Browser(BrowserSession {
            id: crate::model::BrowserId::new(),
            current_url: "https://example.com/".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::HistoryAndScroll,
        });
        state.channels[0].cards.push(browser.id);
        state.cards.push(browser);
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
            store
                .save_workspace(
                    state,
                    SnapshotBase {
                        workspace: 0,
                        cards: &BTreeMap::new(),
                    },
                )
                .unwrap();
        }
        // Keep an old oversized browser payload to catch accidental use of
        // ordinary restore's history trimming during the targeted rewrite.
        let inner = store.lock_inner().unwrap();
        for state in &states {
            inner
                .connection
                .execute(
                    "UPDATE browser_restore SET history_json = ?1 WHERE card_id = ?2",
                    params![
                        to_json(&vec!["https://example.com/"; 500]).unwrap(),
                        state.cards[3].id.to_string()
                    ],
                )
                .unwrap();
        }
        fn raw(connection: &Connection, table: &str, card: CardId) -> Vec<rusqlite::types::Value> {
            connection
                .query_row(
                    &format!("SELECT * FROM {table} WHERE card_id = ?1"),
                    [card.to_string()],
                    |row| {
                        (0..row.as_ref().column_count())
                            .map(|index| row.get(index))
                            .collect()
                    },
                )
                .unwrap()
        }
        let untouched = states
            .iter()
            .flat_map(|state| {
                [
                    ("cards", state.cards[1].id),
                    ("cards", state.cards[2].id),
                    ("terminal_recipes", state.cards[2].id),
                    ("cards", state.cards[3].id),
                    ("browser_restore", state.cards[3].id),
                ]
            })
            .map(|(table, card)| ((table, card), raw(&inner.connection, table, card)))
            .collect::<Vec<_>>();
        drop(inner);
        let titles = states
            .iter()
            .map(|state| (state.cards[0].id.to_string(), "manual title".to_owned()))
            .collect::<HashMap<_, _>>();
        store
            .save_window_view(
                &serde_json::json!({ "user_titles": titles, "other": [1,2,3] }).to_string(),
            )
            .unwrap();
        let destination = directory.path().join("copied.sqlite3");
        StateStore::copy_window_snapshot(&source, &destination, boot.clone()).unwrap();
        let snapshot = Connection::open_with_flags(
            &destination,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        for ((table, card), expected) in &untouched {
            assert_eq!(
                raw(&snapshot, table, *card),
                *expected,
                "unchanged {table}/{card}"
            );
        }
        drop(snapshot);
        let copied = StateStore::open(&destination, boot).unwrap();
        let view: Value =
            serde_json::from_str(&copied.load_window_view().unwrap().unwrap()).unwrap();
        assert_eq!(view["other"], serde_json::json!([1, 2, 3]));
        for original in &states {
            let old = original.cards[0].id;
            let state = copied.load_workspace(original.id, None).unwrap().unwrap();
            let (&new, intent) = state.configured_chat_drafts.iter().next().unwrap();
            assert_ne!(new, old);
            let CardKind::Chat(session) =
                &state.cards.iter().find(|card| card.id == new).unwrap().kind
            else {
                panic!("copied draft")
            };
            assert_ne!(session.id, original.configured_chat_drafts[&old].id);
            assert!(intent.matches_draft(session));
            let mut normalized = intent.clone();
            normalized.id = original.configured_chat_drafts[&old].id;
            assert_eq!(normalized, original.configured_chat_drafts[&old]);
            assert_eq!(state.focused, Some(new));
            assert_eq!(state.worktree_focus[Path::new("/repo")], new);
            let mut expected_members = original.channels[0].cards.clone();
            expected_members[0] = new;
            assert_eq!(state.channels[0].cards, expected_members);
            assert_eq!(state.channels[0].focused, Some(new));
            assert_eq!(state.card_scopes[&new], PathBuf::from("/repo"));
            assert_eq!(state.chat_drafts[&new], "private composer");
            assert!(
                state.local_only_cards.contains(&new) && !state.local_only_cards.contains(&old)
            );
            assert_eq!(
                state
                    .cards
                    .iter()
                    .find(|card| card.id == original.cards[1].id)
                    .unwrap()
                    .kind,
                original.cards[1].kind
            );
            assert!(
                !state
                    .cards
                    .iter()
                    .find(|card| card.id == old)
                    .unwrap()
                    .existence
                    .is_open()
            );
            assert_eq!(
                view["user_titles"][new.to_string().as_str()],
                "manual title"
            );
            assert!(view["user_titles"].get(old.to_string().as_str()).is_none());
            let untouched = store.load_workspace(original.id, None).unwrap().unwrap();
            assert_eq!(
                untouched.configured_chat_drafts,
                original.configured_chat_drafts
            );
            assert!(
                untouched
                    .cards
                    .iter()
                    .find(|card| card.id == old)
                    .unwrap()
                    .existence
                    .is_open()
            );
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
        store
            .save_workspace(
                &state,
                SnapshotBase {
                    workspace: 0,
                    cards: &BTreeMap::new(),
                },
            )
            .unwrap();
        state.configured_chat_drafts.get_mut(&old).unwrap().id = TerminalId::new();
        let mut inner = store.lock_inner().unwrap();
        let transaction = inner.connection.transaction().unwrap();
        set_metadata(
            &transaction,
            &format!("workspace:{}:navigation:configured_chat_drafts", state.id),
            &to_json(&state.configured_chat_drafts).unwrap(),
        )
        .unwrap();
        transaction.commit().unwrap();
        drop(inner);
        let destination = directory.path().join("refused.sqlite3");
        let error = StateStore::copy_window_snapshot(&source, &destination, boot).unwrap_err();
        assert!(error.to_string().contains("saved choices"), "{error}");
        assert!(!destination.exists());
        assert_eq!(
            store
                .load_workspace(state.id, None)
                .unwrap()
                .unwrap()
                .configured_chat_drafts,
            state.configured_chat_drafts
        );
    }
}
