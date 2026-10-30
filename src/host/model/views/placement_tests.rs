mod placement_hooks {
    use super::*;
    use crate::agent_execution::HostedSession;

    fn token(registry: &Registry, id: CardId) -> u64 {
        channel_work::placement::token(&registry.db, id).unwrap()
    }

    fn placement_storage(registry: &Registry) -> (Vec<(String, i64)>, Vec<(i64, i64)>) {
        let active = registry
            .db
            .prepare("SELECT pane,epoch FROM channel_work_placement_epochs ORDER BY pane")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let clock = registry
            .db
            .prepare("SELECT id,last_epoch FROM channel_work_placement_clock ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (active, clock)
    }

    #[test]
    fn canonical_edits_preserve_presentation_tokens_but_retire_transfer_and_reinsert_aba() {
        let (mut registry, _directory, a, b) = setup();
        let original = live_source_pane();
        let unrelated = pane();
        seed(&mut registry, a, vec![original.clone(), unrelated.clone()]);
        let first = token(&registry, original.id);
        let unrelated_token = token(&registry, unrelated.id);
        let mut presentation = original.clone();
        presentation.title = "New inferred title".into();
        presentation.user_title = Some("Chosen label".into());
        presentation.width_fraction = 0.7;
        let update = edit(
            &mut registry,
            a,
            Change::Update {
                before: original.clone(),
                pane: presentation.clone(),
            },
        );
        registry.apply(&update).unwrap();
        let reorder = edit(
            &mut registry,
            a,
            Change::Move {
                id: presentation.id,
                before: None,
            },
        );
        registry.apply(&reorder).unwrap();
        registry
            .apply(&ModelEdit::RetitleChannel {
                id: a,
                title: "Chosen channel".into(),
            })
            .unwrap();
        assert_eq!(token(&registry, original.id), first);
        assert_eq!(token(&registry, unrelated.id), unrelated_token);
        let revision = registry.view_revision(&Scope::Channel(a)).unwrap();
        assert!(revision > 1, "ordinary canonical revisions still advance");

        let away = edit(
            &mut registry,
            a,
            Change::Transfer {
                id: original.id,
                destination: Scope::Channel(b),
                before: None,
            },
        );
        registry.apply(&away).unwrap();
        let moved = token(&registry, original.id);
        assert_ne!(moved, first);
        let back = edit(
            &mut registry,
            b,
            Change::Transfer {
                id: original.id,
                destination: Scope::Channel(a),
                before: None,
            },
        );
        registry.apply(&back).unwrap();
        let returned = token(&registry, original.id);
        assert_ne!(returned, moved);
        assert_ne!(
            returned, first,
            "same Home/card/source cannot revive old placement"
        );
        assert_eq!(
            read(&registry.db, &Scope::Channel(a)).unwrap().panes.last(),
            Some(&presentation)
        );
        assert_eq!(token(&registry, unrelated.id), unrelated_token);

        let remove = edit(&mut registry, a, Change::Remove { id: original.id });
        registry.apply(&remove).unwrap();
        assert!(channel_work::placement::token(&registry.db, original.id).is_err());
        assert!(owner(&registry.db, original.id).unwrap().is_none());
        // No retired map row remains. The durable allocator still gives this
        // exact reinserted descriptor a fresh stamp in a new target.
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Worktree {
                    repository: "/repo".into(),
                    path: "/repo/reinserted".into(),
                },
                panes: vec![presentation],
                closed: vec![],
            })
            .unwrap();
        assert!(token(&registry, original.id) > returned);
        assert_ne!(token(&registry, original.id), first);
    }

    #[test]
    fn authenticated_direct_adoption_rotates_only_on_committed_process_handoff() {
        let (mut registry, _directory, a, _) = setup();
        let original = live_source_pane();
        seed(&mut registry, a, vec![original.clone()]);
        let CardKind::Terminal(current) = &original.kind else {
            panic!()
        };
        let source = HostedSession::Terminal(current.clone());
        let mut next = current.clone();
        next.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("live"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("successor"),
        };
        let successor = HostedSession::Terminal(next.clone());
        let agent = crate::model::AgentId::from_uuid(original.id.as_uuid());
        let first = token(&registry, original.id);
        let before = read(&registry.db, &Scope::Channel(a)).unwrap();
        registry.db.execute_batch("CREATE TRIGGER reject_fixture_view BEFORE UPDATE ON shared_views BEGIN SELECT RAISE(ABORT,'fixture publication failure'); END;").unwrap();
        {
            let tx = registry
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                adopt_authenticated_agent_pane(
                    &tx,
                    agent,
                    std::slice::from_ref(&source),
                    &successor
                )
                .is_err()
            );
            // Drop rolls back both the epoch and the rejected descriptor write.
        }
        assert_eq!(token(&registry, original.id), first);
        assert_eq!(read(&registry.db, &Scope::Channel(a)).unwrap(), before);
        registry
            .db
            .execute_batch("DROP TRIGGER reject_fixture_view;")
            .unwrap();
        {
            let tx = registry
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                adopt_authenticated_agent_pane(
                    &tx,
                    agent,
                    std::slice::from_ref(&source),
                    &successor
                )
                .unwrap()
            );
            tx.commit().unwrap();
        }
        assert_ne!(token(&registry, original.id), first);
        let after = read(&registry.db, &Scope::Channel(a)).unwrap();
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(
            after.panes[0],
            Pane {
                kind: CardKind::Terminal(next),
                ..original
            }
        );
    }

    #[test]
    fn channel_and_project_cascades_remove_active_epochs_and_preserve_the_allocator() {
        let (mut registry, _directory, a, b) = setup();
        let channel_actor = live_source_pane();
        let other_actor = live_source_pane();
        let worktree_actor = live_source_pane();
        seed(&mut registry, a, vec![channel_actor.clone()]);
        seed(&mut registry, b, vec![other_actor.clone()]);
        let worktree = Scope::Worktree {
            repository: "/repo".into(),
            path: "/repo/tree".into(),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: worktree.clone(),
                panes: vec![worktree_actor.clone()],
                closed: vec![],
            })
            .unwrap();
        let channel_before = token(&registry, channel_actor.id);
        let other_before = token(&registry, other_actor.id);
        let worktree_before = token(&registry, worktree_actor.id);
        registry.apply(&ModelEdit::DeleteChannel { id: a }).unwrap();
        assert!(owner(&registry.db, channel_actor.id).unwrap().is_none());
        assert!(channel_work::placement::token(&registry.db, channel_actor.id).is_err());
        assert_eq!(token(&registry, other_actor.id), other_before);
        assert_eq!(token(&registry, worktree_actor.id), worktree_before);
        assert!(channel_before > 0);
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/repo".into(),
            })
            .unwrap();
        assert!(all(&registry.db).unwrap().is_empty());

        for id in [channel_actor.id, other_actor.id, worktree_actor.id] {
            assert!(owner(&registry.db, id).unwrap().is_none());
            assert!(channel_work::placement::token(&registry.db, id).is_err());
        }
        let (active, clock) = placement_storage(&registry);
        assert!(active.is_empty());
        assert_eq!(clock.len(), 1);
        assert_eq!(clock[0].0, 1);
        assert!(clock[0].1 >= other_before.max(worktree_before) as i64);
    }

    #[test]
    fn publication_failures_roll_back_transfer_agent_index_ledger_and_allocator() {
        use crate::agent_execution::{Destination, Origin, Spec, Submit, Surface, Target};
        let (mut registry, _directory, a, b) = setup();
        let accepted = registry
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: Some("fixture-worker".into()),
                    destination: Destination::New(Spec {
                        provider: crate::agent::Provider::Codex,
                        account: None,
                        target: Target {
                            repository: "/repo".into(),
                            directory: "/repo".into(),
                            placement: Some(Scope::Channel(a)),
                        },
                        surface: Surface::Terminal,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "Fixture only; never dispatched".into(),
                    idempotency_key: "placement-transfer-rollback".into(),
                },
                1,
            )
            .unwrap();
        let mut original = live_source_pane();
        original.id = CardId::from_uuid(accepted.agent.id.as_uuid());
        let CardKind::Terminal(session) = &original.kind else {
            panic!()
        };
        registry
            .bind_agent(
                accepted.agent.id,
                None,
                HostedSession::Terminal(session.clone()),
            )
            .unwrap();
        seed(&mut registry, a, vec![original.clone()]);
        seed(&mut registry, b, vec![]);
        let source = Scope::Channel(a);
        let target = Scope::Channel(b);
        let source_before = read(&registry.db, &source).unwrap();
        let target_before = read(&registry.db, &target).unwrap();
        let model_before = registry.model().unwrap();
        let agent_before = registry.agent(accepted.agent.id).unwrap();
        let storage_before = placement_storage(&registry);
        let captured = token(&registry, original.id);
        let transfer = edit(
            &mut registry,
            a,
            Change::Transfer {
                id: original.id,
                destination: target.clone(),
                before: None,
            },
        );
        let ModelEdit::ChangeView { operation, .. } = &transfer else {
            panic!()
        };
        assert!(!operation_seen(&registry.db, *operation).unwrap());
        // TEMP keeps the published main schema unchanged. Both failures occur
        // after agent/index/ledger work. The target's distinct error also proves
        // the real source view was empty before its prepared target was written.
        for fail_source in [true, false] {
            let (scope, expected, failure) = if fail_source {
                (
                    &source,
                    "fixture source publication failure",
                    "SELECT RAISE(ABORT,'fixture source publication failure');".to_owned(),
                )
            } else {
                (&target, "fixture target publication failure", format!(
                    "SELECT CASE WHEN EXISTS(SELECT 1 FROM main.shared_views WHERE scope='{}' AND json_array_length(panes)>0)
                     THEN RAISE(ABORT,'fixture target preceded source')
                     ELSE RAISE(ABORT,'fixture target publication failure') END;", key(&source).unwrap()))
            };
            registry
                .db
                .execute_batch(&format!(
                "CREATE TEMP TRIGGER reject_fixture_publication BEFORE INSERT ON main.shared_views
                 WHEN NEW.scope='{}' BEGIN {failure} END;", key(scope).unwrap(),
            ))
                .unwrap();
            assert!(registry.apply(&transfer).unwrap_err().contains(expected));
            assert_eq!(read(&registry.db, &source).unwrap(), source_before);
            assert_eq!(read(&registry.db, &target).unwrap(), target_before);
            assert_eq!(registry.model().unwrap(), model_before);
            assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent_before);
            assert_eq!(
                owner(&registry.db, original.id).unwrap(),
                Some(source.clone())
            );
            assert_eq!(placement_storage(&registry), storage_before);
            assert_eq!(token(&registry, original.id), captured);
            assert!(!operation_seen(&registry.db, *operation).unwrap());
            registry
                .db
                .execute_batch("DROP TRIGGER temp.reject_fixture_publication;")
                .unwrap();
        }
        registry.apply(&transfer).unwrap();
        assert!(operation_seen(&registry.db, *operation).unwrap());
        assert!(read(&registry.db, &source).unwrap().panes.is_empty());
        assert_eq!(
            read(&registry.db, &target).unwrap().panes,
            [original.clone()]
        );
        assert_eq!(
            owner(&registry.db, original.id).unwrap(),
            Some(target.clone())
        );
        assert_eq!(registry.agent(accepted.agent.id).unwrap().placement, target);
        let moved = token(&registry, original.id);
        assert!(moved > captured);
        let back = edit(
            &mut registry,
            b,
            Change::Transfer {
                id: original.id,
                destination: source.clone(),
                before: None,
            },
        );
        registry.apply(&back).unwrap();
        assert_eq!(
            read(&registry.db, &source).unwrap().panes,
            [original.clone()]
        );
        assert_eq!(
            owner(&registry.db, original.id).unwrap(),
            Some(source.clone())
        );
        assert_eq!(registry.agent(accepted.agent.id).unwrap().placement, source);
        assert!(token(&registry, original.id) > moved);
        assert_ne!(token(&registry, original.id), captured);
    }

    #[test]
    fn missing_epoch_refuses_canonical_publication_without_advancing_view_or_model() {
        let (mut registry, _directory, a, _) = setup();
        let original = live_source_pane();
        seed(&mut registry, a, vec![original.clone()]);
        registry
            .db
            .execute(
                "DELETE FROM channel_work_placement_epochs WHERE pane=?1",
                [original.id.to_string()],
            )
            .unwrap();
        let before = registry.model().unwrap();
        let view = read(&registry.db, &Scope::Channel(a)).unwrap();
        let mut changed = original.clone();
        changed.title = "Must not publish".into();
        let update = edit(
            &mut registry,
            a,
            Change::Update {
                before: original,
                pane: changed,
            },
        );
        assert!(registry.apply(&update).is_err());
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(read(&registry.db, &Scope::Channel(a)).unwrap(), view);
    }
}
