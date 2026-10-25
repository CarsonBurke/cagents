use super::*;
use crate::channel_work::{Change, Freshness, Observation, PollOutcome, PrState, Role, Target};
use crate::github_identity::{GitHubRepoKey, PullRequestKey};

fn fixture() -> (Registry, tempfile::TempDir, Home) {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let repository = directory.path().join("project");
    fs::create_dir(&repository).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            kind: Default::default(),
            repository: repository.clone(),
            name: "project".into(),
        })
        .unwrap();
    let channel = ChannelId::new();
    registry
        .apply(&ModelEdit::CreateChannel {
            id: channel,
            repository,
            title: "Durable work".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let model = registry.model().unwrap();
    let home = Home {
        registry: model.identity,
        channel,
        creation_ordinal: model.channel(channel).unwrap().ordinal,
    };
    (registry, directory, home)
}

fn user() -> TrustedResolvedOrigin {
    TrustedResolvedOrigin {
        origin: Origin::User,
        proof: None,
    }
}

fn target(number: u64) -> Target {
    Target::PullRequest(
        PullRequestKey::new(
            GitHubRepoKey::new("github.com", None, "Owner", "Repository").unwrap(),
            number,
        )
        .unwrap(),
    )
}

fn claim(home: Home, number: u64, revision: u64) -> Mutation {
    Mutation {
        home,
        expected_revision: revision,
        operation: Uuid::new_v4(),
        change: Change::Claim {
            target: target(number),
            role: Role::Contribute,
        },
    }
}

fn snapshot(registry: &Registry, home: &Home) -> Snapshot {
    registry.read_channel_work(home, Page::default()).unwrap()
}

#[test]
fn unrelated_pr_cas_and_original_receipt_replay_preserve_newer_user_correction() {
    let (mut registry, _directory, home) = fixture();
    let original = claim(home, 1, 0);
    let first = registry.apply_channel_work(&user(), &original).unwrap();
    let second = registry
        .apply_channel_work(&user(), &claim(home, 2, 0))
        .unwrap();
    assert_eq!(
        second.receipt.revision, 1,
        "entry CAS is not the home revision"
    );
    let suppress = Mutation {
        home,
        expected_revision: 1,
        operation: Uuid::new_v4(),
        change: Change::Suppress { target: target(1) },
    };
    registry.apply_channel_work(&user(), &suppress).unwrap();
    let replay = registry.apply_channel_work(&user(), &original).unwrap();
    assert_eq!(replay.receipt, first.receipt);
    assert_eq!(replay.snapshot.revision, 3);
    assert_eq!(replay.snapshot.entries.len(), 1);
    assert_eq!(replay.snapshot.history.len(), 1);
    assert!(replay.snapshot.history[0].suppressed);
    assert_eq!(replay.snapshot.history[0].revision, 2);
    let mut collision = original.clone();
    collision.change = Change::Claim {
        target: target(3),
        role: Role::Steward,
    };
    assert!(matches!(
        registry.apply_channel_work(&user(), &collision),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), replay.snapshot);
}

#[test]
fn stale_target_cas_and_missing_receipt_never_change_the_snapshot() {
    let (mut registry, _directory, home) = fixture();
    let first = claim(home, 1, 0);
    registry.apply_channel_work(&user(), &first).unwrap();
    let before = snapshot(&registry, &home);
    let stale = claim(home, 1, 0);
    assert!(matches!(
        registry.apply_channel_work(&user(), &stale),
        Err(ApplyError::Refused(_))
    ));
    assert!(
        registry
            .channel_work_receipt(&user(), &home, stale.operation)
            .unwrap()
            .is_none()
    );
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(
        registry
            .channel_work_receipt(&user(), &home, first.operation)
            .unwrap()
            .unwrap()
            .revision,
        1
    );
}

#[test]
fn failing_receipt_insert_rolls_back_entry_home_revision_and_model_sequence() {
    let (mut registry, _directory, home) = fixture();
    let before = snapshot(&registry, &home);
    let sequence = registry.model().unwrap().sequence;
    registry
        .db
        .execute_batch(
            "CREATE TEMP TRIGGER refuse_work_receipt BEFORE INSERT ON channel_work_operations
         BEGIN SELECT RAISE(ABORT,'fixed fixture receipt failure'); END;",
        )
        .unwrap();
    let request = claim(home, 1, 0);
    assert!(matches!(
        registry.apply_channel_work(&user(), &request),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(registry.model().unwrap().sequence, sequence);
    assert!(
        registry
            .channel_work_receipt(&user(), &home, request.operation)
            .unwrap()
            .is_none()
    );
    registry
        .db
        .execute_batch("DROP TRIGGER refuse_work_receipt")
        .unwrap();
    registry.apply_channel_work(&user(), &request).unwrap();
    assert_eq!(snapshot(&registry, &home).entries.len(), 1);
}

#[test]
fn one_read_transaction_does_not_mix_independent_wal_writer_revisions() {
    let (registry, _directory, home) = fixture();
    let mut writer = registry.execution_connection().unwrap();
    let tx = registry.db.unchecked_transaction().unwrap();
    assert_eq!(
        home_state(&tx, &home).unwrap().1,
        0,
        "establish reader snapshot"
    );
    writer
        .apply_channel_work(&user(), &claim(home, 1, 0))
        .unwrap();
    let stable = read_in(&tx, &home, Page::default()).unwrap();
    assert_eq!(stable.revision, 0);
    assert!(stable.entries.is_empty());
    tx.commit().unwrap();
    let fresh = snapshot(&registry, &home);
    assert_eq!(fresh.revision, 1);
    assert_eq!(fresh.entries.len(), 1);
}

#[test]
fn latest_history_pages_are_unique_targets_and_survive_reopening() {
    let (mut registry, directory, home) = fixture();
    for number in 1..=3 {
        registry
            .apply_channel_work(&user(), &claim(home, number, 0))
            .unwrap();
        registry
            .apply_channel_work(
                &user(),
                &Mutation {
                    home,
                    expected_revision: 1,
                    operation: Uuid::new_v4(),
                    change: Change::End {
                        target: target(number),
                    },
                },
            )
            .unwrap();
    }
    drop(registry);
    let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let first = registry
        .read_channel_work(
            &home,
            Page {
                current_before: None,
                before: None,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(first.history.len(), 2);
    assert!(first.entries.is_empty());
    let last = registry
        .read_channel_work(
            &home,
            Page {
                current_before: None,
                before: first.next_history,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(last.history.len(), 1);
    assert!(last.next_history.is_none());
    let keys = first
        .history
        .iter()
        .chain(&last.history)
        .map(|entry| encode(&entry.target).unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), 3, "no repeated mutation snapshots in History");
}

#[test]
fn attach_and_auto_retire_have_one_canonical_transaction_order() {
    let (mut registry, _directory, home) = fixture();
    registry
        .apply_channel_work(&user(), &claim(home, 1, 0))
        .unwrap();
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(!may_auto_retire_in(&tx, &home, 0).unwrap());
    tx.commit().unwrap();
    assert!(registry.model().unwrap().channel(home.channel).is_some());

    let (mut registry, _other, home) = fixture();
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(may_auto_retire_in(&tx, &home, 0).unwrap());
    deleted_in(&tx, home.channel).unwrap();
    tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
        .unwrap();
    tx.execute(
        "DELETE FROM channels WHERE id=?1",
        [home.channel.to_string()],
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        registry.apply_channel_work(&user(), &claim(home, 1, 0)),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home).lifecycle, HomeLifecycle::Deleted);
}

#[test]
fn retained_history_survives_project_cascade_and_reused_channel_uuid_is_a_new_home() {
    let (mut registry, _directory, home) = fixture();
    let original = claim(home, 1, 0);
    registry.apply_channel_work(&user(), &original).unwrap();
    registry
        .apply(&ModelEdit::RetitleChannel {
            id: home.channel,
            title: "Renamed work".into(),
        })
        .unwrap();
    let tx = registry.db.transaction().unwrap();
    renamed_in(&tx, home.channel).unwrap();
    tx.commit().unwrap();
    let old_project = registry
        .model()
        .unwrap()
        .channel(home.channel)
        .unwrap()
        .repository
        .clone();
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    deleted_in(&tx, home.channel).unwrap();
    tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
        .unwrap();
    tx.execute(
        "DELETE FROM projects WHERE repository=?1",
        [old_project.to_str().unwrap()],
    )
    .unwrap();
    tx.commit().unwrap();
    let old = snapshot(&registry, &home);
    assert_eq!(old.lifecycle, HomeLifecycle::Deleted);
    assert!(old.entries.is_empty());
    assert_eq!(old.history.len(), 1);
    assert!(old.history[0].user_ended);
    registry
        .apply(&ModelEdit::OpenProject {
            kind: Default::default(),
            repository: old_project.clone(),
            name: "again".into(),
        })
        .unwrap();
    registry
        .apply(&ModelEdit::CreateChannel {
            id: home.channel,
            repository: old_project,
            title: "new incarnation".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let new = Home {
        creation_ordinal: registry
            .model()
            .unwrap()
            .channel(home.channel)
            .unwrap()
            .ordinal,
        ..home
    };
    assert_ne!(new.creation_ordinal, home.creation_ordinal);
    assert!(snapshot(&registry, &new).entries.is_empty());
    assert!(matches!(
        registry.apply_channel_work(&user(), &claim(home, 2, 0)),
        Err(ApplyError::Refused(_))
    ));
    let replay = registry.apply_channel_work(&user(), &original).unwrap();
    assert_eq!(replay.snapshot.lifecycle, HomeLifecycle::Deleted);
    assert_eq!(snapshot(&registry, &home), old);
    let historical_title: String = registry
        .db
        .query_row(
            "SELECT title FROM channel_work_homes WHERE channel=?1 AND incarnation=?2",
            params![home.channel.to_string(), home.creation_ordinal],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(historical_title, "Renamed work");
}

fn observation(generation: u64, state: PrState) -> PollOutcome {
    PollOutcome::Success(Observation {
        generation,
        state,
        freshness: Freshness::Current,
        title: Some("Fixture PR".into()),
        node_id: Some("FIXTURE_NODE".into()),
        head: None,
        observed_at_ms: generation,
    })
}

#[test]
fn late_forge_results_cannot_replace_newer_user_decisions_or_create_ownership() {
    let (mut registry, _directory, home) = fixture();
    assert!(matches!(
        registry.observe_channel_work(
            &home,
            &target(1),
            0,
            1,
            observation(1, PrState::Open { draft: false })
        ),
        Err(ApplyError::Refused(_))
    ));
    assert!(snapshot(&registry, &home).entries.is_empty());
    registry
        .apply_channel_work(&user(), &claim(home, 1, 0))
        .unwrap();
    let closed = registry
        .observe_channel_work(&home, &target(1), 1, 1, observation(1, PrState::Merged))
        .unwrap();
    assert_eq!(closed.history.len(), 1);
    let reopened = registry
        .observe_channel_work(
            &home,
            &target(1),
            2,
            2,
            observation(2, PrState::Open { draft: false }),
        )
        .unwrap();
    assert_eq!(reopened.entries[0].bucket(), Bucket::NeedsConfirmation);
    registry
        .apply_channel_work(
            &user(),
            &Mutation {
                home,
                expected_revision: 3,
                operation: Uuid::new_v4(),
                change: Change::Suppress { target: target(1) },
            },
        )
        .unwrap();
    let before = snapshot(&registry, &home);
    assert!(matches!(
        registry.observe_channel_work(
            &home,
            &target(1),
            3,
            3,
            observation(3, PrState::Open { draft: false })
        ),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    let fresh = registry
        .observe_channel_work(&home, &target(1), 4, 3, PollOutcome::Failure)
        .unwrap();
    assert!(fresh.history[0].suppressed);
    assert_eq!(
        fresh.history[0].observation.as_ref().unwrap().freshness,
        Freshness::Unavailable
    );
}

fn native_source() -> HostedSession {
    HostedSession::Chat(crate::chat::Session {
        id: crate::model::TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: crate::model::LaunchRecipe {
                program: "fixture-codex".into(),
                arguments: Vec::new(),
                working_directory: "/fixture".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("fixture-host"),
            boot_id: crate::model::BootId::new("fixture-boot"),
            host_incarnation: crate::model::HostIncarnation::new("fixture-incarnation"),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    })
}

fn publish_native(registry: &mut Registry, home: Home, source: &HostedSession, card: CardId) {
    let HostedSession::Chat(session) = source else {
        panic!("chat fixture")
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: crate::workspace_model::views::Scope::Channel(home.channel),
            panes: vec![crate::workspace_model::views::Pane {
                id: card,
                title: "Native fixture".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 1.0,
                kind: CardKind::Chat(session.clone()),
                scope: None,
            }],
            closed: Vec::new(),
        })
        .unwrap();
}

fn native_origin(registry: &mut Registry, home: Home) -> TrustedResolvedOrigin {
    let source = native_source();
    let authority = registry.register_agent_authority(&source).unwrap();
    let card = CardId::new();
    publish_native(registry, home, &source, card);
    let view_revision = registry
        .view_revision(&crate::workspace_model::views::Scope::Channel(home.channel))
        .unwrap();
    TrustedResolvedOrigin {
        origin: Origin::Actor {
            home,
            actor: work::Contributor {
                session: source.id(),
                lineage: crate::agent_execution::Lineage(source.id()),
                agent: None,
                run: None,
            },
            contexts: Vec::new(),
        },
        proof: Some(SourceProof {
            authority,
            source,
            card,
            view_revision,
            placement_epoch: placement::token(&registry.db, card).unwrap(),
        }),
    }
}

#[test]
fn proved_native_session_needs_no_fabricated_managed_agent_or_run() {
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let request = claim(home, 1, 0);
    let proof = origin.proof.as_ref().unwrap();
    let resolved = registry
        .resolve_channel_work_source(&proof.authority, &proof.source)
        .unwrap();
    assert_eq!(resolved.origin, origin.origin);
    assert_eq!(resolved.proof.as_ref().unwrap().card, proof.card);
    let result = registry.apply_channel_work(&origin, &request).unwrap();
    let work::Source::Session(contributor) = &result.snapshot.entries[0].claims[0].source else {
        panic!("native source was not retained")
    };
    assert!(contributor.agent.is_none() && contributor.run.is_none());
    assert_eq!(
        contributor.session,
        origin.proof.as_ref().unwrap().source.id()
    );
    let stored: String = registry
        .db
        .query_row(
            "SELECT record FROM channel_work_operations WHERE operation=?1",
            [request.operation.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !stored.contains(origin.proof.as_ref().unwrap().authority.token().unwrap()),
        "operation provenance never retains its capability"
    );
    assert!(
        registry
            .channel_work_receipt(&origin, &home, request.operation)
            .unwrap()
            .is_some()
    );
}

#[test]
fn retired_native_authority_cannot_replay_and_duplicate_canonical_session_cannot_claim() {
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let request = claim(home, 1, 0);
    registry.apply_channel_work(&origin, &request).unwrap();
    let before = snapshot(&registry, &home);
    registry
        .retire_agent_authority(&origin.proof.as_ref().unwrap().source)
        .unwrap();
    assert!(matches!(
        registry.apply_channel_work(&origin, &request),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);

    let origin = native_origin(&mut registry, home);
    let scope = crate::workspace_model::views::Scope::Channel(home.channel);
    let original_view = views::read(&registry.db, &scope).unwrap();
    publish_native(
        &mut registry,
        home,
        &origin.proof.as_ref().unwrap().source,
        CardId::new(),
    );
    // The normal publication path deduplicates an existing session. Prove
    // that positive contract rather than mistaking it for duplicate state.
    assert_eq!(views::read(&registry.db, &scope).unwrap(), original_view);
    let mut proof = origin.proof.unwrap();
    let resolved = registry
        .resolve_channel_work_source(&proof.authority, &proof.source)
        .unwrap();
    assert_eq!(resolved.origin, origin.origin);
    assert_eq!(resolved.proof.as_ref().unwrap().card, proof.card);
    let mut damaged_view = original_view.clone();
    let mut duplicate_pane = damaged_view
        .panes
        .iter()
        .find(|pane| pane.id == proof.card)
        .unwrap()
        .clone();
    duplicate_pane.id = CardId::new();
    let duplicate_card = duplicate_pane.id;
    damaged_view.panes.push(duplicate_pane);
    damaged_view.revision += 1;
    // Only this test's private Registry is given retained/legacy duplicate
    // data. Ordinary SeedView/ChangeView correctly cannot produce it.
    let scope_key = encode(&scope).unwrap();
    let tx = registry.db.transaction().unwrap();
    assert_eq!(
        tx.execute(
            "UPDATE shared_views SET panes=?2,revision=?3 WHERE scope=?1",
            params![
                scope_key,
                encode(&damaged_view.panes).unwrap(),
                damaged_view.revision
            ],
        )
        .unwrap(),
        1
    );
    tx.execute(
        "INSERT INTO shared_view_panes(id,scope) VALUES (?1,?2)",
        params![duplicate_card.to_string(), scope_key],
    )
    .unwrap();
    tx.commit().unwrap();
    let duplicate_count = views::all(&registry.db)
        .unwrap()
        .iter()
        .flat_map(|view| &view.panes)
        .filter(
            |pane| matches!(&pane.kind, CardKind::Chat(session) if session.id == proof.source.id()),
        )
        .count();
    assert_eq!(
        duplicate_count, 2,
        "negative fixture must actually expose two canonical panes"
    );
    proof.view_revision = registry.view_revision(&scope).unwrap();
    assert_eq!(proof.view_revision, damaged_view.revision);
    let duplicate = TrustedResolvedOrigin {
        origin: origin.origin,
        proof: Some(proof),
    };
    let request = claim(home, 2, 0);
    let sequence = registry.model().unwrap().sequence;
    match registry.apply_channel_work(&duplicate, &request) {
        Err(ApplyError::Refused(reason)) => assert!(
            reason.contains("one exact canonical channel pane"),
            "{reason}"
        ),
        result => panic!("an actual duplicate canonical source must refuse: {result:?}"),
    }
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(registry.model().unwrap().sequence, sequence);
    assert!(
        registry
            .channel_work_receipt(&user(), &home, request.operation)
            .unwrap()
            .is_none()
    );
}

#[test]
fn changed_view_revision_defeats_automatic_retirement_even_when_no_panes_remain() {
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let proof = origin.proof.unwrap();
    registry
        .apply(&ModelEdit::ChangeView {
            scope: crate::workspace_model::views::Scope::Channel(home.channel),
            expected_revision: proof.view_revision,
            operation: Uuid::new_v4(),
            change: crate::workspace_model::views::Change::Remove { id: proof.card },
        })
        .unwrap();
    let current = registry
        .view_revision(&crate::workspace_model::views::Scope::Channel(home.channel))
        .unwrap();
    assert!(current > proof.view_revision);
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(!may_auto_retire_in(&tx, &home, proof.view_revision).unwrap());
    assert!(may_auto_retire_in(&tx, &home, current).unwrap());
    tx.commit().unwrap();
}

#[test]
fn leaf_migration_seeds_home_metadata_without_head_or_pr_backfill() {
    let mut db = Connection::open_in_memory().unwrap();
    let registry = Uuid::new_v4();
    let channel = ChannelId::new();
    db.execute_batch(
        "CREATE TABLE metadata(key INTEGER PRIMARY KEY,identity TEXT NOT NULL,sequence INTEGER NOT NULL) STRICT;
         CREATE TABLE channels(id TEXT PRIMARY KEY,ordinal INTEGER NOT NULL,repository TEXT NOT NULL,title TEXT NOT NULL) STRICT;",
    ).unwrap();
    db.execute(
        "INSERT INTO metadata VALUES(0,?1,23)",
        [registry.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO channels VALUES(?1,7,'/existing/project','Existing work')",
        [channel.to_string()],
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    migrate(&tx).unwrap();
    tx.commit().unwrap();
    let home = Home {
        registry,
        channel,
        creation_ordinal: 7,
    };
    let result = read_in(&db, &home, Page::default()).unwrap();
    assert_eq!(result.revision, 0);
    assert_eq!(result.lifecycle, HomeLifecycle::Active);
    assert!(result.entries.is_empty() && result.history.is_empty());
    let preserved: (String, u32, String, String) = db
        .query_row(
            "SELECT id,ordinal,repository,title FROM channels",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        preserved,
        (
            channel.to_string(),
            7,
            "/existing/project".into(),
            "Existing work".into()
        )
    );
    assert_eq!(
        db.query_row("SELECT sequence FROM metadata", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        23
    );
}

#[test]
fn archive_retires_claims_with_unique_history_cursors_and_one_enclosing_model_edge() {
    let (mut registry, _directory, home) = fixture();
    let original = claim(home, 1, 0);
    let original_receipt = registry
        .apply_channel_work(&user(), &original)
        .unwrap()
        .receipt;
    registry
        .apply_channel_work(&user(), &claim(home, 2, 0))
        .unwrap();
    registry
        .apply_channel_work(&user(), &claim(home, 3, 0))
        .unwrap();
    let before = registry.model().unwrap().sequence;
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(archived_in(&tx, &home).unwrap());
    assert!(
        !archived_in(&tx, &home).unwrap(),
        "same lifecycle is an idempotent no-op"
    );
    tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(registry.model().unwrap().sequence, before + 1);
    let mut cursor = None;
    let mut targets = std::collections::HashSet::new();
    loop {
        let page = registry
            .read_channel_work(
                &home,
                Page {
                    current_before: None,
                    before: cursor,
                    limit: 1,
                },
            )
            .unwrap();
        assert_eq!(page.lifecycle, HomeLifecycle::Archived);
        assert!(page.entries.is_empty());
        for entry in page.history {
            assert!(entry.user_ended && entry.claims.iter().all(|claim| !claim.active));
            assert_eq!(entry.revision, 2);
            assert!(
                targets.insert(encode(&entry.target).unwrap()),
                "no duplicate cursor target"
            );
        }
        cursor = page.next_history;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        targets.len(),
        3,
        "one lifecycle transaction cannot hide tied history rows"
    );
    assert!(matches!(
        registry.apply_channel_work(&user(), &claim(home, 4, 0)),
        Err(ApplyError::Refused(_))
    ));
    let replay = registry.apply_channel_work(&user(), &original).unwrap();
    assert_eq!(replay.receipt, original_receipt);
    assert_eq!(replay.snapshot.lifecycle, HomeLifecycle::Archived);
    assert_eq!(replay.snapshot.history.len(), 3);
}

#[test]
fn receipt_inspection_follows_proved_lineage_across_move_without_old_home_write_authority() {
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let request = claim(home, 1, 0);
    let original = registry
        .apply_channel_work(&origin, &request)
        .unwrap()
        .receipt;
    let channel = ChannelId::new();
    let repository = registry
        .model()
        .unwrap()
        .channel(home.channel)
        .unwrap()
        .repository
        .clone();
    registry
        .apply(&ModelEdit::CreateChannel {
            id: channel,
            repository,
            title: "Moved home".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let proof = origin.proof.as_ref().unwrap();
    registry
        .apply(&ModelEdit::ChangeView {
            scope: crate::workspace_model::views::Scope::Channel(home.channel),
            expected_revision: proof.view_revision,
            operation: Uuid::new_v4(),
            change: crate::workspace_model::views::Change::Transfer {
                id: proof.card,
                destination: crate::workspace_model::views::Scope::Channel(channel),
                before: None,
            },
        })
        .unwrap();
    let moved = registry
        .resolve_channel_work_source(&proof.authority, &proof.source)
        .unwrap();
    let Origin::Actor { home: new_home, .. } = &moved.origin else {
        panic!("actor scope")
    };
    assert_eq!(new_home.channel, channel);
    let before = snapshot(&registry, &home);
    assert_eq!(
        registry
            .channel_work_receipt(&moved, &home, request.operation)
            .unwrap(),
        Some(original)
    );
    assert!(matches!(
        registry.apply_channel_work(&moved, &request),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    let unrelated = native_origin(&mut registry, *new_home);
    assert!(
        registry
            .channel_work_receipt(&unrelated, &home, request.operation)
            .is_err()
    );
    assert_eq!(snapshot(&registry, &home), before);
}

#[test]
fn observation_rechecks_source_view_inside_write_transaction_after_independent_wal_change() {
    let (mut registry, _directory, home) = fixture();
    let actor = native_origin(&mut registry, home);
    registry
        .apply_channel_work(&actor, &claim(home, 1, 0))
        .unwrap();
    let proof = actor.proof.as_ref().unwrap();
    let mut writer = registry.execution_connection().unwrap();
    writer
        .apply(&ModelEdit::ChangeView {
            scope: crate::workspace_model::views::Scope::Channel(home.channel),
            expected_revision: proof.view_revision,
            operation: Uuid::new_v4(),
            change: crate::workspace_model::views::Change::Remove { id: proof.card },
        })
        .unwrap();
    let before = snapshot(&registry, &home);
    let sequence = registry.model().unwrap().sequence;
    assert!(matches!(
        registry.observe_channel_work_authenticated(
            &actor,
            &home,
            &target(1),
            1,
            1,
            observation(1, PrState::Open { draft: false })
        ),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(registry.model().unwrap().sequence, sequence);
    let accepted = registry
        .observe_channel_work_authenticated(
            &user(),
            &home,
            &target(1),
            1,
            1,
            observation(1, PrState::Open { draft: false }),
        )
        .unwrap();
    assert!(accepted.changed);
    assert_eq!(accepted.sequence, sequence + 1);
    assert_eq!(accepted.snapshot.entries[0].revision, 2);
    let unchanged = registry
        .observe_channel_work_authenticated(
            &user(),
            &home,
            &target(1),
            2,
            1,
            observation(1, PrState::Merged),
        )
        .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(unchanged.sequence, accepted.sequence);
    assert_eq!(unchanged.snapshot, accepted.snapshot);
}

#[test]
fn retained_connection_refuses_future_layout_before_mutation_replay_or_observation() {
    let (mut registry, directory, home) = fixture();
    let original = claim(home, 1, 0);
    let original_receipt = registry
        .apply_channel_work(&user(), &original)
        .unwrap()
        .receipt;
    let before = snapshot(&registry, &home);
    let sequence = registry.model().unwrap().sequence;
    let mut changed = claim(home, 1, 1);
    changed.change = Change::Claim {
        target: target(1),
        role: Role::Steward,
    };
    let writer = Connection::open(directory.path().join("model.sqlite3")).unwrap();
    let future = super::super::SCHEMA_VERSION + 1;
    writer.pragma_update(None, "user_version", future).unwrap();
    assert!(matches!(
        registry.apply_channel_work(&user(), &changed),
        Err(ApplyError::Refused(_))
    ));
    assert!(
        matches!(
            registry.apply_channel_work(&user(), &original),
            Err(ApplyError::Refused(_))
        ),
        "a durable receipt must not bypass retained-connection schema admission"
    );
    assert!(matches!(
        registry.observe_channel_work_authenticated(
            &user(),
            &home,
            &target(1),
            1,
            1,
            observation(1, PrState::Merged)
        ),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(registry.model().unwrap().sequence, sequence);
    assert_eq!(
        registry
            .channel_work_receipt(&user(), &home, original.operation)
            .unwrap(),
        Some(original_receipt)
    );
    assert!(
        registry
            .channel_work_receipt(&user(), &home, changed.operation)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        writer
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        future,
        "an old admitted writer must never downgrade a newer header"
    );
}

#[test]
fn current_keyset_pages_include_needs_confirmation_and_point_lookup_is_not_page_absence() {
    let (mut registry, _directory, home) = fixture();
    for number in 1..=103 {
        registry
            .apply_channel_work(&user(), &claim(home, number, 0))
            .unwrap();
    }
    for number in [1, 2] {
        registry
            .observe_channel_work(
                &home,
                &target(number),
                1,
                1,
                observation(1, PrState::Merged),
            )
            .unwrap();
        registry
            .observe_channel_work(
                &home,
                &target(number),
                2,
                2,
                observation(2, PrState::Open { draft: false }),
            )
            .unwrap();
    }
    let first = registry
        .read_channel_work(
            &home,
            Page {
                current_before: None,
                before: None,
                limit: 100,
            },
        )
        .unwrap();
    assert_eq!(first.entries.len(), 100);
    assert!(first.next_current.is_some());
    assert_eq!(
        first
            .entries
            .iter()
            .filter(|entry| entry.bucket() == Bucket::NeedsConfirmation)
            .count(),
        2,
        "current includes both active presentation buckets"
    );
    let second = registry
        .read_channel_work(
            &home,
            Page {
                current_before: first.next_current,
                before: None,
                limit: 100,
            },
        )
        .unwrap();
    assert_eq!(second.entries.len(), 3);
    assert!(second.next_current.is_none());
    let keys = first
        .entries
        .iter()
        .chain(&second.entries)
        .map(|entry| entry.target.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        keys.len(),
        103,
        "strict continuation neither skips nor repeats the boundary"
    );
    let hidden = second.entries[0].target.clone();
    assert!(!first.entries.iter().any(|entry| entry.target == hidden));
    let exact = registry.read_channel_work_target(&home, &hidden).unwrap();
    assert_eq!(exact.home, home);
    assert_eq!(exact.target, hidden);
    assert_eq!(exact.entry.as_ref().unwrap().target, hidden);
    assert_eq!(exact.revision, second.revision);
    assert_eq!(exact.entry.as_ref().unwrap().revision, 1);
    let absent = registry
        .read_channel_work_target(&home, &target(999))
        .unwrap();
    assert!(absent.entry.is_none());
    assert_eq!(absent.target, target(999).canonicalized().unwrap());
    assert!(
        registry
            .read_channel_work(
                &home,
                Page {
                    current_before: Some(0),
                    before: Some(0),
                    limit: 100,
                },
            )
            .is_err(),
        "zero is not a valid keyset cursor"
    );
}

/// Fixed model data, not real filesystem paths or claims about file authors.
/// Each row remains valid but two such rows exceed a bucket's wire budget.
fn enlarge_context_provenance(registry: &mut Registry, home: Home) {
    let mut rows =
        {
            let mut statement = registry.db.prepare(
            "SELECT target,entry FROM channel_work_entries WHERE channel=?1 AND incarnation=?2"
        ).unwrap();
            statement
                .query_map(
                    params![home.channel.to_string(), home.creation_ordinal],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
    for (key, raw) in &mut rows {
        let mut entry: Entry = decode(raw).unwrap();
        entry.contexts = (0..work::MAX_CONTEXTS)
            .map(|_| work::CheckoutContext {
                id: Uuid::new_v4(),
                registry: home.registry,
                repository: format!("/{}", "r".repeat(4095)).into(),
                path: format!("/{}", "p".repeat(4095)).into(),
                branch: Some("b".repeat(1024)),
                head: Some("a".repeat(64)),
                generation: 1,
                dirty: true,
            })
            .collect();
        entry.validate().unwrap();
        let bytes = postcard::experimental::serialized_size(&entry).unwrap();
        assert!(bytes <= work::MAX_BUCKET_BYTES);
        assert!(bytes * 2 > work::MAX_BUCKET_BYTES);
        registry.db.execute(
            "UPDATE channel_work_entries SET entry=?4 WHERE channel=?1 AND incarnation=?2 AND target=?3",
            params![home.channel.to_string(),home.creation_ordinal,key.as_str(),encode(&entry).unwrap()],
        ).unwrap();
    }
}

#[test]
fn byte_budget_continuations_preserve_the_first_unincluded_row_in_both_collections() {
    let (mut registry, directory, home) = fixture();
    for number in 1..=6 {
        registry
            .apply_channel_work(&user(), &claim(home, number, 0))
            .unwrap();
        if number % 2 == 0 {
            registry
                .apply_channel_work(
                    &user(),
                    &Mutation {
                        home,
                        expected_revision: 1,
                        operation: Uuid::new_v4(),
                        change: Change::End {
                            target: target(number),
                        },
                    },
                )
                .unwrap();
        }
    }
    enlarge_context_provenance(&mut registry, home);
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    for historical in [false, true] {
        let mut cursor = None;
        let mut keys = std::collections::HashSet::new();
        for page_index in 0..3 {
            let page = registry
                .read_channel_work(
                    &home,
                    Page {
                        current_before: if historical { None } else { cursor },
                        before: if historical { cursor } else { None },
                        limit: 100,
                    },
                )
                .unwrap();
            assert!(postcard::experimental::serialized_size(&page).unwrap() <= 4 * 1024 * 1024);
            let (entries, next) = if historical {
                (&page.history, page.next_history)
            } else {
                (&page.entries, page.next_current)
            };
            assert_eq!(
                entries.len(),
                1,
                "byte limit must apply before the count limit"
            );
            assert!(
                keys.insert(entries[0].target.clone()),
                "a continuation repeated a target"
            );
            if page_index == 2 {
                assert!(
                    next.is_none(),
                    "last included row is final; no phantom continuation"
                );
            } else {
                assert!(next.is_some());
                assert!(cursor.is_none_or(|old| next.unwrap() < old));
            }
            cursor = next;
        }
        assert_eq!(
            keys.len(),
            3,
            "the lookahead row remains the next page's first row"
        );
    }
    let exact = registry
        .read_channel_work_target(&home, &target(1))
        .unwrap();
    assert_eq!(
        exact.entry.as_ref().unwrap().contexts.len(),
        work::MAX_CONTEXTS
    );
    exact.validate().unwrap();
    let mutation = Mutation {
        home,
        expected_revision: exact.entry.as_ref().unwrap().revision,
        operation: Uuid::new_v4(),
        change: Change::Claim {
            target: target(1),
            role: Role::Steward,
        },
    };
    let applied = registry.apply_channel_work(&user(), &mutation).unwrap();
    assert!(applied.receipt.changed);
    applied.snapshot.validate().unwrap();
    assert!(work::encoded_size(&applied.snapshot).unwrap() <= work::MAX_SNAPSHOT_BYTES);
    let replay = registry.apply_channel_work(&user(), &mutation).unwrap();
    assert_eq!(replay.receipt, applied.receipt);
    assert_eq!(replay.snapshot, applied.snapshot);
}

#[test]
fn archive_and_delete_cross_ended_keyset_batches_without_losing_live_rows_or_receipts() {
    for archive in [false, true] {
        let (mut registry, _directory, home) = fixture();
        let mut requests = Vec::new();
        for number in 1..=35 {
            let request = claim(home, number, 0);
            registry.apply_channel_work(&user(), &request).unwrap();
            requests.push(request);
        }
        requests.sort_by_key(|request| encode(request.change.target()).unwrap());
        // Two complete batches and part of the third are already ended.
        // A cursor based on mutable sequence or only changed rows fails here.
        for request in requests.iter().take(17) {
            registry
                .apply_channel_work(
                    &user(),
                    &Mutation {
                        home,
                        expected_revision: 1,
                        operation: Uuid::new_v4(),
                        change: Change::End {
                            target: request.change.target().clone(),
                        },
                    },
                )
                .unwrap();
        }
        let before = registry.model().unwrap().sequence;
        let receipts: Vec<_> = requests
            .iter()
            .map(|request| {
                registry
                    .channel_work_receipt(&user(), &home, request.operation)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        if archive {
            let tx = registry
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(archived_in(&tx, &home).unwrap());
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .unwrap();
            tx.commit().unwrap();
        } else {
            registry
                .apply(&ModelEdit::DeleteChannel { id: home.channel })
                .unwrap();
        }
        assert_eq!(registry.model().unwrap().sequence, before + 1);
        let mut cursor = None;
        let mut keys = std::collections::HashSet::new();
        for page_index in 0..5 {
            let page = registry
                .read_channel_work(
                    &home,
                    Page {
                        current_before: None,
                        before: cursor,
                        limit: 7,
                    },
                )
                .unwrap();
            assert_eq!(
                page.lifecycle,
                if archive {
                    HomeLifecycle::Archived
                } else {
                    HomeLifecycle::Deleted
                }
            );
            assert!(page.entries.is_empty());
            assert_eq!(page.history.len(), 7);
            for entry in page.history {
                assert!(entry.user_ended && entry.claims.iter().all(|claim| !claim.active));
                assert_eq!(
                    entry.revision, 2,
                    "already-ended rows must not be retired twice"
                );
                assert!(keys.insert(entry.target));
            }
            cursor = page.next_history;
            assert_eq!(cursor.is_none(), page_index == 4);
        }
        assert_eq!(keys.len(), 35);
        for (request, original) in requests.iter().zip(receipts) {
            assert_eq!(
                registry
                    .channel_work_receipt(&user(), &home, request.operation)
                    .unwrap(),
                Some(original)
            );
        }
    }
}

#[test]
fn exact_target_read_keeps_its_home_revision_and_entry_in_one_wal_snapshot() {
    let (mut registry, _directory, home) = fixture();
    registry
        .apply_channel_work(&user(), &claim(home, 1, 0))
        .unwrap();
    let mut writer = registry.execution_connection().unwrap();
    let tx = registry.db.unchecked_transaction().unwrap();
    assert_eq!(home_state(&tx, &home).unwrap().1, 1);
    let mut change = claim(home, 1, 1);
    change.change = Change::Claim {
        target: target(1),
        role: Role::Review,
    };
    writer.apply_channel_work(&user(), &change).unwrap();
    let old = read_target_in(&tx, &home, &target(1)).unwrap();
    assert_eq!(old.revision, 1);
    assert_eq!(old.entry.as_ref().unwrap().revision, 1);
    assert_eq!(old.entry.as_ref().unwrap().role(), Some(Role::Contribute));
    tx.commit().unwrap();
    let fresh = registry
        .read_channel_work_target(&home, &target(1))
        .unwrap();
    assert_eq!(fresh.revision, 2);
    assert_eq!(fresh.entry.as_ref().unwrap().revision, 2);
    assert_eq!(fresh.entry.as_ref().unwrap().role(), Some(Role::Review));
    let mut foreign = home;
    foreign.registry = Uuid::new_v4();
    assert!(
        registry
            .read_channel_work_target(&foreign, &target(1))
            .is_err()
    );
}

#[test]
fn refresh_candidates_reach_hidden_current_and_history_and_rotate_attempts_without_mutating_on_read()
 {
    let (mut registry, _directory, home) = fixture();
    for number in 1..=210 {
        registry
            .apply_channel_work(&user(), &claim(home, number, 0))
            .unwrap();
        if number <= 105 {
            registry
                .apply_channel_work(
                    &user(),
                    &Mutation {
                        home,
                        expected_revision: 1,
                        operation: Uuid::new_v4(),
                        change: Change::End {
                            target: target(number),
                        },
                    },
                )
                .unwrap();
        }
        if number != 1 && number != 106 {
            let revision = if number <= 105 { 2 } else { 1 };
            registry
                .observe_channel_work(
                    &home,
                    &target(number),
                    revision,
                    1,
                    observation(1, PrState::Open { draft: false }),
                )
                .unwrap();
        }
    }
    // A valid, unclaimed local candidate is not a forge refresh target. This
    // fixture row grants no source authority or successful local Claim.
    let local = Entry {
        id: Uuid::new_v4(),
        target: Target::Local(Uuid::new_v4()),
        claims: vec![],
        contexts: vec![],
        suppressed: false,
        user_ended: false,
        observation: None,
        last_terminal_generation: 0,
        ever_claimed: false,
        revision: 1,
    };
    local.validate().unwrap();
    registry
        .db
        .execute(
            "INSERT INTO channel_work_entries(channel,incarnation,target,entry,sequence,bucket)
         VALUES(?1,?2,?3,?4,0,'needs_confirmation')",
            params![
                home.channel.to_string(),
                home.creation_ordinal,
                encode(&local.target).unwrap(),
                encode(&local).unwrap()
            ],
        )
        .unwrap();
    let visible = registry
        .read_channel_work(
            &home,
            Page {
                current_before: None,
                before: None,
                limit: 100,
            },
        )
        .unwrap();
    assert_eq!(visible.entries.len(), 100);
    assert_eq!(visible.history.len(), 100);
    assert!(visible.next_current.is_some() && visible.next_history.is_some());
    for hidden in [
        target(1).canonicalized().unwrap(),
        target(106).canonicalized().unwrap(),
    ] {
        assert!(
            !visible
                .entries
                .iter()
                .chain(&visible.history)
                .any(|entry| entry.target == hidden)
        );
    }
    let before = registry.model().unwrap().sequence;
    let first = registry
        .read_channel_work_refresh_candidates(&home)
        .unwrap();
    assert_eq!(first.len(), 4);
    assert!(
        first
            .iter()
            .all(|entry| matches!(entry.target, Target::PullRequest(_)))
    );
    assert!(
        first
            .iter()
            .any(|entry| entry.target == target(1).canonicalized().unwrap())
    );
    assert!(
        first
            .iter()
            .any(|entry| entry.target == target(106).canonicalized().unwrap())
    );
    assert_eq!(
        registry.model().unwrap().sequence,
        before,
        "candidate read does not advance attempts"
    );
    assert_eq!(
        registry
            .read_channel_work_refresh_candidates(&home)
            .unwrap(),
        first
    );
    let mut seen = std::collections::HashSet::new();
    for _ in 0..4 {
        let entries = registry
            .read_channel_work_refresh_candidates(&home)
            .unwrap();
        assert_eq!(entries.len(), 4);
        for entry in entries {
            seen.insert(entry.target.clone());
            let generation = entry
                .observation
                .as_ref()
                .map_or(0, |value| value.generation)
                + 1;
            registry
                .observe_channel_work(
                    &home,
                    &entry.target,
                    entry.revision,
                    generation,
                    PollOutcome::Failure,
                )
                .unwrap();
        }
    }
    assert!(
        seen.len() >= 10,
        "attempted rows must yield to other rows beyond visible pages"
    );
    let raw = registry.db.query_row(
        "SELECT entry FROM channel_work_entries WHERE channel=?1 AND incarnation=?2 AND target=?3",
        params![home.channel.to_string(),home.creation_ordinal,encode(&target(1).canonicalized().unwrap()).unwrap()],
        |row| row.get::<_,String>(0),
    ).unwrap();
    registry.db.execute(
        "UPDATE channel_work_entries SET entry='not-json' WHERE channel=?1 AND incarnation=?2 AND target=?3",
        params![home.channel.to_string(),home.creation_ordinal,encode(&target(1).canonicalized().unwrap()).unwrap()],
    ).unwrap();
    let sequence = registry.model().unwrap().sequence;
    let unchanged_home = registry.read_channel_work_home(&home).unwrap();
    assert!(
        registry
            .read_channel_work_refresh_candidates(&home)
            .is_err()
    );
    assert_eq!(registry.model().unwrap().sequence, sequence);
    assert_eq!(
        registry.read_channel_work_home(&home).unwrap(),
        unchanged_home
    );
    assert_eq!(registry.db.query_row(
        "SELECT entry FROM channel_work_entries WHERE channel=?1 AND incarnation=?2 AND target=?3",
        params![home.channel.to_string(),home.creation_ordinal,encode(&target(1).canonicalized().unwrap()).unwrap()],
        |row| row.get::<_,String>(0),
    ).unwrap(), "not-json", "failed candidate read must never repair or rewrite data");
    registry.db.execute(
        "UPDATE channel_work_entries SET entry=?4 WHERE channel=?1 AND incarnation=?2 AND target=?3",
        params![home.channel.to_string(),home.creation_ordinal,encode(&target(1).canonicalized().unwrap()).unwrap(),raw],
    ).unwrap();
    let mut foreign = home;
    foreign.registry = Uuid::new_v4();
    assert!(
        registry
            .read_channel_work_refresh_candidates(&foreign)
            .is_err()
    );
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(archived_in(&tx, &home).unwrap());
    tx.commit().unwrap();
    assert!(
        registry
            .read_channel_work_refresh_candidates(&home)
            .is_err()
    );
}

#[test]
fn refresh_rank_preserves_adjacent_full_u64_generations_and_timestamps() {
    let (mut registry, _directory, home) = fixture();
    let base = i64::MAX as u64 + 1;
    for (number, generation, observed_at_ms) in [
        (1, base + 1, 0),
        (2, base, base + 1),
        (3, base, base),
        (4, base + 2, 0),
        (5, base, base + 2),
    ] {
        registry
            .apply_channel_work(&user(), &claim(home, number, 0))
            .unwrap();
        let mut value = observation(generation, PrState::Open { draft: false });
        let PollOutcome::Success(observation) = &mut value else {
            unreachable!()
        };
        observation.observed_at_ms = observed_at_ms;
        registry
            .observe_channel_work(&home, &target(number), 1, generation, value)
            .unwrap();
    }
    let entries = registry
        .read_channel_work_refresh_candidates(&home)
        .unwrap();
    let ordered: Vec<_> = entries.iter().map(|entry| entry.target.clone()).collect();
    assert_eq!(
        ordered,
        [3, 2, 5, 1].map(|number| target(number).canonicalized().unwrap())
    );
    assert_eq!(entries[0].observation.as_ref().unwrap().generation, base);
    assert_eq!(
        entries[3].observation.as_ref().unwrap().generation,
        base + 1
    );
}

#[test]
fn metadata_only_home_read_preserves_retained_receipt_scope_without_decoding_projection_rows() {
    for archive in [false, true] {
        let (mut registry, _directory, home) = fixture();
        let request = claim(home, 1, 0);
        let receipt = registry
            .apply_channel_work(&user(), &request)
            .unwrap()
            .receipt;
        let active = registry.read_channel_work_home(&home).unwrap();
        assert_eq!(active.home, home);
        assert_eq!(active.lifecycle, HomeLifecycle::Active);
        assert_eq!(active.revision, 1);
        if archive {
            let tx = registry
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(archived_in(&tx, &home).unwrap());
            tx.commit().unwrap();
        } else {
            registry
                .apply(&ModelEdit::DeleteChannel { id: home.channel })
                .unwrap();
        }
        // Even an unavailable row projection cannot prevent exact metadata
        // validation followed by the separate immutable receipt lookup.
        registry.db.execute(
            "UPDATE channel_work_entries SET entry='not-json' WHERE channel=?1 AND incarnation=?2",
            params![home.channel.to_string(),home.creation_ordinal],
        ).unwrap();
        assert!(registry.read_channel_work(&home, Page::default()).is_err());
        let retained = registry.read_channel_work_home(&home).unwrap();
        assert_eq!(retained.home, home);
        assert_eq!(
            retained.lifecycle,
            if archive {
                HomeLifecycle::Archived
            } else {
                HomeLifecycle::Deleted
            }
        );
        assert!(retained.revision > active.revision);
        assert_eq!(
            registry
                .channel_work_receipt(&user(), &home, request.operation)
                .unwrap(),
            Some(receipt)
        );
        let mut wrong = home;
        wrong.registry = Uuid::new_v4();
        assert!(registry.read_channel_work_home(&wrong).is_err());
        wrong = home;
        wrong.creation_ordinal += 1;
        assert!(registry.read_channel_work_home(&wrong).is_err());
        wrong = home;
        wrong.channel = ChannelId::new();
        assert!(registry.read_channel_work_home(&wrong).is_err());
    }
}

#[test]
fn presentation_and_unrelated_pane_edits_preserve_captured_actor_placement() {
    use crate::workspace_model::views::{Change as ViewChange, Scope};
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let proof = origin.proof.as_ref().unwrap();
    let scope = Scope::Channel(home.channel);
    let original = views::read(&registry.db, &scope).unwrap();
    let before = original
        .panes
        .iter()
        .find(|pane| pane.id == proof.card)
        .unwrap()
        .clone();
    let mut renamed = before.clone();
    renamed.title = "A new presentation title".into();
    renamed.user_title = Some("A user title".into());
    renamed.width_fraction = 0.75;
    registry
        .apply(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: original.revision,
            operation: Uuid::new_v4(),
            change: ViewChange::Update {
                before,
                pane: renamed.clone(),
            },
        })
        .unwrap();
    let mut unrelated = renamed;
    unrelated.id = CardId::new();
    unrelated.kind = CardKind::Browser(crate::model::BrowserSession {
        id: crate::model::BrowserId::new(),
        current_url: "https://example.test/unrelated".parse().unwrap(),
        history: Vec::new(),
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
    });
    registry
        .apply(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: registry.view_revision(&scope).unwrap(),
            operation: Uuid::new_v4(),
            change: ViewChange::Insert {
                pane: unrelated,
                before: None,
            },
        })
        .unwrap();
    registry
        .apply(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: registry.view_revision(&scope).unwrap(),
            operation: Uuid::new_v4(),
            change: ViewChange::Move {
                id: proof.card,
                before: None,
            },
        })
        .unwrap();
    let current = registry
        .resolve_channel_work_source(&proof.authority, &proof.source)
        .unwrap();
    assert_eq!(current.origin, origin.origin);
    assert!(registry.view_revision(&scope).unwrap() > proof.view_revision);
    assert_eq!(
        current.proof.as_ref().unwrap().placement_epoch,
        proof.placement_epoch
    );
    let request = claim(home, 31, 0);
    let reply = registry.apply_channel_work(&origin, &request).unwrap();
    assert!(reply.receipt.changed);
    assert_eq!(reply.receipt.operation, request.operation);
}

#[test]
fn independent_wal_transfer_round_trip_does_not_revive_a_captured_actor_placement() {
    use crate::workspace_model::views::{Change as ViewChange, Scope};
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let proof = origin.proof.as_ref().unwrap();
    let repository = registry
        .model()
        .unwrap()
        .channel(home.channel)
        .unwrap()
        .repository
        .clone();
    let destination = ChannelId::new();
    registry
        .apply(&ModelEdit::CreateChannel {
            id: destination,
            repository,
            title: "Temporary destination".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let mut writer = registry.execution_connection().unwrap();
    for (from, to) in [(home.channel, destination), (destination, home.channel)] {
        let scope = Scope::Channel(from);
        writer
            .apply(&ModelEdit::ChangeView {
                expected_revision: writer.view_revision(&scope).unwrap(),
                scope,
                operation: Uuid::new_v4(),
                change: ViewChange::Transfer {
                    id: proof.card,
                    destination: Scope::Channel(to),
                    before: None,
                },
            })
            .unwrap();
    }
    let current = registry
        .resolve_channel_work_source(&proof.authority, &proof.source)
        .unwrap();
    assert_eq!(
        current.origin, origin.origin,
        "Home/session/run are equal after the round trip"
    );
    assert_eq!(current.proof.as_ref().unwrap().card, proof.card);
    assert_ne!(
        current.proof.as_ref().unwrap().placement_epoch,
        proof.placement_epoch
    );
    let before = snapshot(&registry, &home);
    let sequence = registry.model().unwrap().sequence;
    let request = claim(home, 32, 0);
    assert!(matches!(
        registry.apply_channel_work(&origin, &request),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(registry.model().unwrap().sequence, sequence);
    assert!(
        registry
            .channel_work_receipt(&user(), &home, request.operation)
            .unwrap()
            .is_none()
    );
    assert!(
        registry.apply_channel_work(&current, &request).is_ok(),
        "a fresh explicit context still works"
    );
}

#[test]
fn source_proof_replacement_and_restore_cannot_revive_old_actor_placement() {
    use crate::workspace_model::views::{Change as ViewChange, Scope};
    let (mut registry, _directory, home) = fixture();
    let origin = native_origin(&mut registry, home);
    let proof = origin.proof.as_ref().unwrap();
    let scope = Scope::Channel(home.channel);
    let original = views::read(&registry.db, &scope)
        .unwrap()
        .panes
        .into_iter()
        .find(|pane| pane.id == proof.card)
        .unwrap();
    let mut replaced = original.clone();
    let CardKind::Chat(session) = &mut replaced.kind else {
        panic!("native chat fixture")
    };
    session.proof.as_mut().unwrap().host_incarnation =
        crate::model::HostIncarnation::new("replacement-incarnation");
    for (before, pane) in [(original.clone(), replaced.clone()), (replaced, original)] {
        registry
            .apply(&ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: registry.view_revision(&scope).unwrap(),
                operation: Uuid::new_v4(),
                change: ViewChange::Update { before, pane },
            })
            .unwrap();
    }
    let current = registry
        .resolve_channel_work_source(&proof.authority, &proof.source)
        .unwrap();
    assert_eq!(current.origin, origin.origin);
    assert_ne!(
        current.proof.as_ref().unwrap().placement_epoch,
        proof.placement_epoch
    );
    let before = snapshot(&registry, &home);
    let sequence = registry.model().unwrap().sequence;
    let request = claim(home, 33, 0);
    assert!(matches!(
        registry.apply_channel_work(&origin, &request),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
    assert_eq!(registry.model().unwrap().sequence, sequence);
    assert!(registry.apply_channel_work(&current, &request).is_ok());
}
