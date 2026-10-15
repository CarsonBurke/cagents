// Included in execution::tests to reuse the real durable managed-agent fixtures.
#[test]
fn legacy_open_presentation_close_preflight_is_read_only_and_repeats_commit_predicate() {
    let (mut registry, _directory, accepted, pane) = console_pane_fixture();
    let scope = crate::workspace_model::views::Scope::Channel(accepted.agent.channel().unwrap());
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![ended_console(&pane)],
            closed: vec![],
        })
        .unwrap();
    let agent = registry.agent(accepted.agent.id).unwrap();
    assert!(!agent.closed);
    let before = registry.model().unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    assert!(
        registry
            .preflight_agent_close(&Origin::User, agent.id)
            .unwrap_err()
            .contains("historical generation proof")
    );
    assert!(
        registry
            .preflight_agent_retirement(agent.session.as_ref().unwrap())
            .unwrap_err()
            .contains("historical generation proof")
    );
    assert_eq!(registry.model().unwrap(), before);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    assert_eq!(registry.agent(agent.id).unwrap(), agent);
    registry
        .apply(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: registry.view_revision(&scope).unwrap(),
            operation: uuid::Uuid::new_v4(),
            change: crate::workspace_model::views::Change::Remove { id: pane.id },
        })
        .unwrap();
    assert_eq!(
        registry
            .preflight_agent_close(&Origin::User, agent.id)
            .unwrap(),
        agent
    );
    registry
        .preflight_agent_retirement(agent.session.as_ref().unwrap())
        .unwrap();
}

fn presentation_repair_fixture() -> (
    Registry,
    tempfile::TempDir,
    Accepted,
    crate::agent_execution::PresentationRepairTarget,
) {
    let (mut registry, directory, accepted, pane) = console_pane_fixture();
    registry
        .close_agent(&Origin::User, accepted.agent.id)
        .unwrap();
    let scope = crate::workspace_model::views::Scope::Channel(accepted.agent.channel().unwrap());
    registry
        .apply(&ModelEdit::SeedView {
            scope,
            panes: vec![ended_console(&pane)],
            closed: vec![],
        })
        .unwrap();
    let target = registry
        .inspect_presentation_repair(&Origin::User, accepted.agent.id)
        .unwrap()
        .unwrap();
    (registry, directory, accepted, target)
}

#[test]
fn presentation_repair_removes_only_explicit_current_observation_and_preserves_history() {
    let (mut registry, directory, accepted, mut target) = presentation_repair_fixture();
    let mut unrelated = target.pane.clone();
    unrelated.id = crate::model::CardId::new();
    unrelated.title = "Unrelated ended pane".into();
    let crate::model::CardKind::Terminal(session) = &mut unrelated.kind else {
        unreachable!()
    };
    session.id = crate::model::TerminalId::new();
    registry
        .apply(&ModelEdit::SeedView {
            scope: target.scope.clone(),
            panes: vec![unrelated.clone()],
            closed: vec![],
        })
        .unwrap();
    target = registry
        .inspect_presentation_repair(&Origin::User, accepted.agent.id)
        .unwrap()
        .unwrap();
    let before_agent = registry.agent(accepted.agent.id).unwrap();
    let before_run = registry.agent_run(accepted.run.id).unwrap();
    let request = crate::agent_execution::Operation::ConfirmPresentationRepair {
        observed: target.clone(),
    };
    let wire = postcard::to_allocvec(&request).unwrap();
    assert_eq!(
        postcard::from_bytes::<crate::agent_execution::Operation>(&wire).unwrap(),
        request
    );
    assert_eq!(
        postcard::from_bytes::<crate::agent_execution::Reply>(
            &postcard::to_allocvec(&crate::agent_execution::Reply::PresentationRepair(Some(
                target.clone()
            )))
            .unwrap()
        )
        .unwrap(),
        crate::agent_execution::Reply::PresentationRepair(Some(target.clone()))
    );
    assert!(
        registry
            .close_agent(&Origin::User, accepted.agent.id)
            .unwrap_err()
            .contains("historical generation proof")
    );
    let sequence = registry.model().unwrap().sequence;
    assert!(
        registry
            .confirm_presentation_repair(&Origin::User, &target)
            .unwrap()
    );
    let view = super::super::views::read(&registry.db, &target.scope).unwrap();
    assert_eq!(view.panes, vec![unrelated.clone()]);
    assert!(view.retired.contains(&target.pane.id));
    assert_eq!(view.revision, target.view_revision + 1);
    assert_eq!(registry.model().unwrap().sequence, sequence + 1);
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), before_agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), before_run);
    assert!(
        !registry
            .confirm_presentation_repair(&Origin::User, &target)
            .unwrap()
    );
    assert!(
        registry
            .inspect_presentation_repair(&Origin::User, accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(registry.model().unwrap().sequence, sequence + 1);
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert!(
        !registry
            .confirm_presentation_repair(&Origin::User, &target)
            .unwrap()
    );
    registry
        .apply(&ModelEdit::SeedView {
            scope: target.scope.clone(),
            panes: vec![target.pane.clone()],
            closed: vec![],
        })
        .unwrap();
    assert_eq!(
        super::super::views::read(&registry.db, &target.scope)
            .unwrap()
            .panes,
        vec![unrelated],
        "stale client cannot resurrect confirmed presentation"
    );
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), before_agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), before_run);
}

#[test]
fn presentation_repair_refuses_changed_owner_revision_descriptor_registry_and_agent() {
    use crate::workspace_model::views::{Change, Scope};
    for change in [
        "descriptor",
        "revision",
        "moved",
        "registry",
        "agent",
        "forged-pane",
    ] {
        let (mut registry, directory, accepted, mut target) = presentation_repair_fixture();
        match change {
            "descriptor" => {
                let mut replacement = target.pane.clone();
                replacement.title = "Newly observed pane".into();
                publish_console(&mut registry, &target.scope, &target.pane, &replacement).unwrap();
            }
            "revision" => {
                registry
                    .db
                    .execute("UPDATE shared_views SET revision=revision+1", [])
                    .unwrap();
            }
            "moved" => {
                registry
                    .apply(&ModelEdit::ChangeView {
                        scope: target.scope.clone(),
                        expected_revision: target.view_revision,
                        operation: uuid::Uuid::new_v4(),
                        change: Change::Transfer {
                            id: target.pane.id,
                            destination: Scope::Worktree {
                                repository: directory.path().into(),
                                path: directory.path().into(),
                            },
                            before: None,
                        },
                    })
                    .unwrap();
            }
            "registry" => target.registry = uuid::Uuid::new_v4(),
            "agent" => {
                target.agent.name = Some("forged selected agent binding".into());
            }
            "forged-pane" => {
                target.pane.title = "Not what was displayed".into();
            }
            _ => unreachable!(),
        }
        let before = registry.model().unwrap();
        let run = registry.agent_run(accepted.run.id).unwrap();
        assert!(
            registry
                .confirm_presentation_repair(&Origin::User, &target)
                .is_err(),
            "{change}"
        );
        assert_eq!(
            registry.model().unwrap(),
            before,
            "{change} cannot remove a replacement"
        );
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    }
}

#[test]
fn presentation_repair_inspection_refuses_unowned_open_active_pending_or_recorded_generation() {
    for case in [
        "origin",
        "open",
        "active",
        "pending",
        "recorded",
        "mismatched-recorded",
        "live",
        "pending-pane",
    ] {
        let (mut registry, _directory, accepted, target) = presentation_repair_fixture();
        let origin = if case == "origin" {
            Origin::Automation {
                id: uuid::Uuid::new_v4(),
            }
        } else {
            Origin::User
        };
        match case {
            "open" => {
                let mut agent = target.agent.clone();
                agent.closed = false;
                registry
                    .db
                    .execute(
                        "UPDATE agents SET closed=0,record=?2 WHERE id=?1",
                        params![agent.id.to_string(), encode(&agent).unwrap()],
                    )
                    .unwrap();
            }
            "active" => {
                registry
                    .db
                    .execute(
                        "UPDATE agent_runs SET state='waiting' WHERE agent=?1",
                        [accepted.agent.id.to_string()],
                    )
                    .unwrap();
            }
            "pending" => {
                // Deliberately inconsistent legacy state must not let a closed
                // record bypass an independently retained launch reservation.
                let source = target.agent.session.as_ref().unwrap();
                registry
                    .db
                    .execute(
                        "INSERT INTO agent_launches(agent,session_id,source) VALUES(?1,?2,?3)",
                        params![
                            accepted.agent.id.to_string(),
                            source.id().to_string(),
                            encode(source).unwrap()
                        ],
                    )
                    .unwrap();
            }
            "recorded" | "mismatched-recorded" => {
                let source = target.agent.console.as_ref().unwrap();
                let crate::model::CardKind::Terminal(ended) = &target.pane.kind else {
                    unreachable!()
                };
                let mut recorded = ended.clone();
                if case == "mismatched-recorded" {
                    recorded.id = crate::model::TerminalId::new();
                }
                registry.db.execute("INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,?2,?3)",
                    params![target.pane.id.to_string(), encode(source).unwrap(), encode(&recorded).unwrap()]).unwrap();
            }
            "live" | "pending-pane" => {
                let mut pane = target.pane.clone();
                if case == "live" {
                    pane.kind = crate::model::CardKind::Terminal(
                        target.agent.console.as_ref().unwrap().clone(),
                    );
                } else {
                    pane.pending_terminal_start = true;
                }
                publish_console(&mut registry, &target.scope, &target.pane, &pane).unwrap();
            }
            "origin" => {}
            _ => unreachable!(),
        }
        let before = registry.model().unwrap();
        assert!(
            registry
                .inspect_presentation_repair(&origin, accepted.agent.id)
                .is_err(),
            "{case}"
        );
        assert!(
            registry
                .confirm_presentation_repair(&origin, &target)
                .is_err(),
            "{case}"
        );
        assert_eq!(registry.model().unwrap(), before);
    }
}

#[test]
fn presentation_repair_rolls_back_membership_tombstone_and_sequence_on_write_failure() {
    let (mut registry, _directory, accepted, target) = presentation_repair_fixture();
    let before = registry.model().unwrap();
    let agent = registry.agent(accepted.agent.id).unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    registry
        .db
        .execute_batch(
            "CREATE TRIGGER reject_presentation_repair BEFORE UPDATE ON metadata
        BEGIN SELECT RAISE(FAIL, 'fixture metadata write refused'); END;",
        )
        .unwrap();
    assert!(
        registry
            .confirm_presentation_repair(&Origin::User, &target)
            .unwrap_err()
            .contains("fixture metadata")
    );
    assert_eq!(registry.model().unwrap(), before);
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    assert_eq!(
        registry
            .inspect_presentation_repair(&Origin::User, accepted.agent.id)
            .unwrap(),
        Some(target.clone())
    );
    registry
        .db
        .execute_batch("DROP TRIGGER reject_presentation_repair;")
        .unwrap();
    assert!(
        registry
            .confirm_presentation_repair(&Origin::User, &target)
            .unwrap()
    );
}

#[test]
fn presentation_repair_service_authenticates_without_any_hosted_provider() {
    use crate::agent_execution::{Operation, Reply};
    let (registry, _directory, accepted, target) = presentation_repair_fixture();
    let mut state = crate::host::state::HostState::new(crate::model::HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("repair-no-provider"),
        boot_id: crate::model::BootId::new("repair-boot"),
        host_incarnation: crate::model::HostIncarnation::new("repair-incarnation"),
    });
    state.install_model(registry);
    let host = std::sync::Arc::new(crate::host::state::HostLock::new(state));
    assert!(
        crate::host::agent_execution::process(
            &host,
            Authority::session("unissued".into()),
            Operation::ConfirmPresentationRepair {
                observed: target.clone()
            }
        )
        .is_err()
    );
    assert_eq!(
        crate::host::agent_execution::process(
            &host,
            Authority::default(),
            Operation::InspectPresentationRepair(accepted.agent.id)
        )
        .unwrap(),
        Reply::PresentationRepair(Some(target.clone()))
    );
    assert_eq!(
        crate::host::agent_execution::process(
            &host,
            Authority::default(),
            Operation::ConfirmPresentationRepair {
                observed: target.clone()
            }
        )
        .unwrap(),
        Reply::PresentationRemoved {
            agent: accepted.agent.id,
            removed: true
        }
    );
    assert_eq!(
        crate::host::agent_execution::process(
            &host,
            Authority::default(),
            Operation::ConfirmPresentationRepair { observed: target }
        )
        .unwrap(),
        Reply::PresentationRemoved {
            agent: accepted.agent.id,
            removed: false
        }
    );
    let state = host.lock().unwrap();
    assert!(
        state.chats.is_empty() && state.terminals.is_empty(),
        "repair never needs a hosted provider or terminal"
    );
}

fn presentation_close_fixture() -> (
    Registry,
    tempfile::TempDir,
    Accepted,
    crate::agent_execution::PresentationCloseObservation,
) {
    let (mut registry, directory, accepted, pane) = console_pane_fixture();
    let scope = accepted.agent.placement.clone();
    registry
        .apply(&ModelEdit::SeedView {
            scope,
            panes: vec![ended_console(&pane)],
            closed: vec![],
        })
        .unwrap();
    let observed = registry
        .inspect_presentation_close(&Origin::User, accepted.agent.id)
        .unwrap()
        .unwrap();
    (registry, directory, accepted, observed)
}

#[test]
fn combined_close_model_commits_exact_pane_and_agent_without_rewriting_completed_result() {
    let (mut registry, directory, accepted, observed) = presentation_close_fixture();
    let before_run = registry.agent_run(accepted.run.id).unwrap();
    let sequence = registry.model().unwrap().sequence;
    registry
        .preflight_presentation_close(&Origin::User, &observed)
        .unwrap();
    let encoded = postcard::to_allocvec(&observed).unwrap();
    assert_eq!(
        postcard::from_bytes::<crate::agent_execution::PresentationCloseObservation>(&encoded)
            .unwrap(),
        observed
    );
    assert!(!format!("{observed:?}").contains("Preserved review report"));
    // This is the model boundary only: real service tests must first prove
    // normal provider/paired teardown and retain its exact receipt on failure.
    assert!(
        registry
            .retire_agent_process_with_presentation(
                &Origin::User,
                &observed.source,
                10,
                &observed,
                std::slice::from_ref(&observed.source)
            )
            .unwrap()
    );
    let closed = registry.agent(accepted.agent.id).unwrap();
    let mut expected = observed.agent.clone();
    expected.closed = true;
    assert_eq!(closed, expected);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), before_run);
    let view = super::super::views::read(&registry.db, &observed.scope).unwrap();
    assert!(view.panes.is_empty());
    assert!(view.retired.contains(&observed.pane.id));
    assert_eq!(view.revision, observed.view_revision + 1);
    assert_eq!(registry.model().unwrap().sequence, sequence + 1);
    assert!(
        registry
            .retire_agent_process_with_presentation(
                &Origin::User,
                &observed.source,
                11,
                &observed,
                std::slice::from_ref(&observed.source)
            )
            .is_err()
    );
    // Subsequent ordinary close is provider-free/idempotent at this boundary.
    assert_eq!(
        registry.close_agent(&Origin::User, closed.id).unwrap(),
        closed
    );
    drop(registry);
    let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert_eq!(registry.agent(closed.id).unwrap(), closed);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), before_run);
}

#[test]
fn combined_close_model_cancels_only_unfinished_runs_and_preserves_their_result() {
    let (mut registry, _directory, accepted, observed) = presentation_close_fixture();
    let historical = registry.agent_run(accepted.run.id).unwrap();
    let next = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::Existing(accepted.agent.id),
                prompt: "A queued turn".into(),
                idempotency_key: "queued-consented-close".into(),
            },
            5,
        )
        .unwrap();
    let observed = registry
        .inspect_presentation_close(&Origin::User, observed.agent.id)
        .unwrap()
        .unwrap();
    registry
        .retire_agent_process_with_presentation(
            &Origin::User,
            &observed.source,
            10,
            &observed,
            std::slice::from_ref(&observed.source),
        )
        .unwrap();
    assert_eq!(registry.agent_run(historical.id).unwrap(), historical);
    let cancelled = registry.agent_run(next.run.id).unwrap();
    assert_eq!(cancelled.state, State::Cancelled);
    assert_eq!(cancelled.result, next.run.result);
    assert_eq!(cancelled.finished_ms, Some(10));
    assert_eq!(cancelled.revision, next.run.revision + 1);
}

#[test]
fn combined_close_model_refuses_late_cas_and_fresh_observation_can_retry_without_record_loss() {
    for case in [
        "pane", "revision", "moved", "registry", "source", "origin", "agent",
    ] {
        let (mut registry, directory, accepted, mut observed) = presentation_close_fixture();
        let mut source = observed.source.clone();
        let mut origin = Origin::User;
        match case {
            "pane" => {
                let mut pane = observed.pane.clone();
                pane.title = "Changed before final CAS".into();
                publish_console(&mut registry, &observed.scope, &observed.pane, &pane).unwrap();
            }
            "revision" => {
                registry
                    .db
                    .execute("UPDATE shared_views SET revision=revision+1", [])
                    .unwrap();
            }
            "moved" => {
                registry
                    .apply(&ModelEdit::ChangeView {
                        scope: observed.scope.clone(),
                        expected_revision: observed.view_revision,
                        operation: uuid::Uuid::new_v4(),
                        change: crate::workspace_model::views::Change::Transfer {
                            id: observed.pane.id,
                            destination: crate::agent_execution::Placement::Worktree {
                                repository: directory.path().into(),
                                path: directory.path().into(),
                            },
                            before: None,
                        },
                    })
                    .unwrap();
            }
            "registry" => observed.registry = uuid::Uuid::new_v4(),
            "source" => {
                let HostedSession::Chat(session) = &mut source else {
                    unreachable!()
                };
                session.proof.as_mut().unwrap().host_incarnation =
                    crate::model::HostIncarnation::new("unrelated successor");
            }
            "origin" => {
                origin = Origin::Automation {
                    id: uuid::Uuid::new_v4(),
                }
            }
            "agent" => observed.agent.name = Some("unobserved binding".into()),
            _ => unreachable!(),
        }
        let before = registry.model().unwrap();
        let agent = registry.agent(accepted.agent.id).unwrap();
        let run = registry.agent_run(accepted.run.id).unwrap();
        assert!(
            registry
                .retire_agent_process_with_presentation(
                    &origin,
                    &source,
                    10,
                    &observed,
                    std::slice::from_ref(&observed.source)
                )
                .is_err(),
            "{case}"
        );
        assert_eq!(registry.model().unwrap(), before, "{case}");
        assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent);
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
        let fresh = registry
            .inspect_presentation_close(&Origin::User, accepted.agent.id)
            .unwrap()
            .unwrap();
        registry
            .retire_agent_process_with_presentation(
                &Origin::User,
                &fresh.source,
                11,
                &fresh,
                std::slice::from_ref(&fresh.source),
            )
            .unwrap();
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    }
}

#[test]
fn combined_close_model_write_failure_rolls_back_pane_runs_authority_and_agent() {
    let (mut registry, _directory, accepted, observed) = presentation_close_fixture();
    let before = registry.model().unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    registry
        .db
        .execute_batch(
            "CREATE TRIGGER reject_combined_close BEFORE UPDATE ON agents
        BEGIN SELECT RAISE(FAIL, 'fixture close commit refused'); END;",
        )
        .unwrap();
    assert!(
        registry
            .retire_agent_process_with_presentation(
                &Origin::User,
                &observed.source,
                10,
                &observed,
                std::slice::from_ref(&observed.source)
            )
            .unwrap_err()
            .contains("fixture close commit refused")
    );
    assert_eq!(registry.model().unwrap(), before);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), observed.agent);
    registry
        .preflight_presentation_close(&Origin::User, &observed)
        .unwrap();
    registry
        .db
        .execute_batch("DROP TRIGGER reject_combined_close;")
        .unwrap();
    registry
        .retire_agent_process_with_presentation(
            &Origin::User,
            &observed.source,
            11,
            &observed,
            std::slice::from_ref(&observed.source),
        )
        .unwrap();
}

#[test]
fn combined_close_observation_refuses_closed_pending_live_recorded_and_wrong_owner() {
    for case in ["closed", "pending", "live", "recorded", "wrong-owner"] {
        let (mut registry, _directory, accepted, observed) = presentation_close_fixture();
        let mut origin = Origin::User;
        match case {
            "closed" => {
                let mut agent = observed.agent.clone();
                agent.closed = true;
                registry
                    .db
                    .execute(
                        "UPDATE agents SET closed=1,record=?2 WHERE id=?1",
                        params![agent.id.to_string(), encode(&agent).unwrap()],
                    )
                    .unwrap();
            }
            "pending" => {
                registry
                    .db
                    .execute(
                        "INSERT INTO agent_launches(agent,session_id,source) VALUES (?1,?2,?3)",
                        params![
                            accepted.agent.id.to_string(),
                            observed.source.id().to_string(),
                            encode(&observed.source).unwrap()
                        ],
                    )
                    .unwrap();
            }
            "live" => {
                let mut pane = observed.pane.clone();
                pane.kind =
                    crate::model::CardKind::Terminal(observed.agent.console.clone().unwrap());
                publish_console(&mut registry, &observed.scope, &observed.pane, &pane).unwrap();
            }
            "recorded" => {
                registry.db.execute("INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES (?1,?2,?3)",
                params![observed.pane.id.to_string(), encode(&observed.agent.console).unwrap(),
                    encode(&observed.agent.console).unwrap()]).unwrap();
            }
            "wrong-owner" => {
                origin = Origin::Automation {
                    id: uuid::Uuid::new_v4(),
                }
            }
            _ => unreachable!(),
        }
        let before = registry.model().unwrap();
        assert!(
            registry
                .inspect_presentation_close(&origin, accepted.agent.id)
                .is_err(),
            "{case}"
        );
        assert!(
            registry
                .preflight_presentation_close(&origin, &observed)
                .is_err(),
            "{case}"
        );
        assert!(
            registry
                .retire_agent_process_with_presentation(
                    &origin,
                    &observed.source,
                    10,
                    &observed,
                    std::slice::from_ref(&observed.source)
                )
                .is_err(),
            "{case}"
        );
        assert_eq!(registry.model().unwrap(), before, "{case}");
    }
}

#[test]
fn combined_close_retires_every_verified_authority_alias_in_its_single_commit() {
    let (mut registry, _directory, accepted, observed) = presentation_close_fixture();
    let mut alias = observed.source.clone();
    let HostedSession::Chat(session) = &mut alias else {
        unreachable!()
    };
    session.proof.as_mut().unwrap().host_incarnation =
        crate::model::HostIncarnation::new("issued-before-adoption");
    let authority = registry.register_agent_authority(&alias).unwrap();
    let before = registry.model().unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    // The model is not allowed to revoke an alias whose exact cleanup proof
    // was absent from the service's verified set, even at the same session ID.
    assert!(
        registry
            .retire_agent_process_with_presentation(
                &Origin::User,
                &observed.source,
                10,
                &observed,
                std::slice::from_ref(&observed.source)
            )
            .is_err()
    );
    assert_eq!(registry.model().unwrap(), before);
    assert_eq!(
        registry.agent_authority_source(&authority).unwrap(),
        Some(alias.clone())
    );
    registry
        .retire_agent_process_with_presentation(
            &Origin::User,
            &observed.source,
            11,
            &observed,
            &[observed.source.clone(), alias],
        )
        .unwrap();
    assert!(registry.agent_authority_source(&authority).is_err());
    assert!(registry.agent(accepted.agent.id).unwrap().closed);
    assert_eq!(registry.agent_run(run.id).unwrap(), run);
}
