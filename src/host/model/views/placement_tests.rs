mod placement_hooks {
    use super::*;
    use crate::agent_execution::HostedSession;

    fn token(registry: &Registry, id: CardId) -> u64 {
        channel_work::placement::token(&registry.db, id).unwrap()
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
        let removed = token(&registry, original.id);
        assert_ne!(removed, returned);
        assert!(owner(&registry.db, original.id).unwrap().is_none());
        // This fresh target has no retired marker for this ID. Its retained epoch
        // still prevents a reinserted exact descriptor from reviving a token.
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
        assert_ne!(token(&registry, original.id), removed);
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
    fn channel_and_project_cascades_retire_actor_and_worktree_epochs_without_deleting_tombstones() {
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
        assert_ne!(token(&registry, channel_actor.id), channel_before);
        assert_eq!(token(&registry, other_actor.id), other_before);
        assert_eq!(token(&registry, worktree_actor.id), worktree_before);
        let channel_tombstone = token(&registry, channel_actor.id);
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/repo".into(),
            })
            .unwrap();
        assert!(all(&registry.db).unwrap().is_empty());
        assert_eq!(token(&registry, channel_actor.id), channel_tombstone);
        assert_ne!(token(&registry, other_actor.id), other_before);
        assert_ne!(token(&registry, worktree_actor.id), worktree_before);
        for id in [channel_actor.id, other_actor.id, worktree_actor.id] {
            assert!(owner(&registry.db, id).unwrap().is_none());
            assert!(channel_work::placement::token(&registry.db, id).is_ok());
        }
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
