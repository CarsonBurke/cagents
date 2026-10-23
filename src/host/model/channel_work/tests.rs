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
    publish_native(
        &mut registry,
        home,
        &origin.proof.as_ref().unwrap().source,
        CardId::new(),
    );
    let mut proof = origin.proof.unwrap();
    proof.view_revision = registry
        .view_revision(&crate::workspace_model::views::Scope::Channel(home.channel))
        .unwrap();
    let duplicate = TrustedResolvedOrigin {
        origin: origin.origin,
        proof: Some(proof),
    };
    assert!(matches!(
        registry.apply_channel_work(&duplicate, &claim(home, 2, 0)),
        Err(ApplyError::Refused(_))
    ));
    assert_eq!(snapshot(&registry, &home), before);
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
