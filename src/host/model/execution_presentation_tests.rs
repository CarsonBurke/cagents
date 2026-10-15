// Included in execution::tests to reuse the real durable managed-agent fixtures.
fn presentation_repair_fixture() -> (Registry, tempfile::TempDir, Accepted, crate::agent_execution::PresentationRepairTarget) {
    let (mut registry, directory, accepted, pane) = console_pane_fixture();
    registry.close_agent(&Origin::User, accepted.agent.id).unwrap();
    let scope = crate::workspace_model::views::Scope::Channel(accepted.agent.channel().unwrap());
    registry.apply(&ModelEdit::SeedView {scope, panes: vec![ended_console(&pane)], closed: vec![]}).unwrap();
    let target = registry.inspect_presentation_repair(&Origin::User, accepted.agent.id).unwrap().unwrap();
    (registry, directory, accepted, target)
}

#[test]
fn presentation_repair_removes_only_explicit_current_observation_and_preserves_history() {
    let (mut registry, directory, accepted, mut target) = presentation_repair_fixture();
    let mut unrelated = target.pane.clone();
    unrelated.id = crate::model::CardId::new(); unrelated.title = "Unrelated ended pane".into();
    let crate::model::CardKind::Terminal(session) = &mut unrelated.kind else {unreachable!()};
    session.id = crate::model::TerminalId::new();
    registry.apply(&ModelEdit::SeedView {scope: target.scope.clone(), panes: vec![unrelated.clone()], closed: vec![]}).unwrap();
    target = registry.inspect_presentation_repair(&Origin::User, accepted.agent.id).unwrap().unwrap();
    let before_agent = registry.agent(accepted.agent.id).unwrap();
    let before_run = registry.agent_run(accepted.run.id).unwrap();
    let request = crate::agent_execution::Operation::ConfirmPresentationRepair {observed: target.clone()};
    let wire = postcard::to_allocvec(&request).unwrap();
    assert_eq!(postcard::from_bytes::<crate::agent_execution::Operation>(&wire).unwrap(), request);
    assert_eq!(postcard::from_bytes::<crate::agent_execution::Reply>(&postcard::to_allocvec(
        &crate::agent_execution::Reply::PresentationRepair(Some(target.clone()))).unwrap()).unwrap(),
        crate::agent_execution::Reply::PresentationRepair(Some(target.clone())));
    assert!(registry.close_agent(&Origin::User, accepted.agent.id).unwrap_err().contains("historical generation proof"));
    let sequence = registry.model().unwrap().sequence;
    assert!(registry.confirm_presentation_repair(&Origin::User, &target).unwrap());
    let view = super::super::views::read(&registry.db, &target.scope).unwrap();
    assert_eq!(view.panes, vec![unrelated.clone()]);
    assert!(view.retired.contains(&target.pane.id));
    assert_eq!(view.revision, target.view_revision + 1);
    assert_eq!(registry.model().unwrap().sequence, sequence + 1);
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), before_agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), before_run);
    assert!(!registry.confirm_presentation_repair(&Origin::User, &target).unwrap());
    assert!(registry.inspect_presentation_repair(&Origin::User, accepted.agent.id).unwrap().is_none());
    assert_eq!(registry.model().unwrap().sequence, sequence + 1);
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert!(!registry.confirm_presentation_repair(&Origin::User, &target).unwrap());
    registry.apply(&ModelEdit::SeedView {scope: target.scope.clone(), panes: vec![target.pane.clone()], closed: vec![]}).unwrap();
    assert_eq!(super::super::views::read(&registry.db, &target.scope).unwrap().panes, vec![unrelated], "stale client cannot resurrect confirmed presentation");
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), before_agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), before_run);
}

#[test]
fn presentation_repair_refuses_changed_owner_revision_descriptor_registry_and_agent() {
    use crate::workspace_model::views::{Change, Scope};
    for change in ["descriptor", "revision", "moved", "registry", "agent", "forged-pane"] {
        let (mut registry, directory, accepted, mut target) = presentation_repair_fixture();
        match change {
            "descriptor" => {
                let mut replacement = target.pane.clone(); replacement.title = "Newly observed pane".into();
                publish_console(&mut registry, &target.scope, &target.pane, &replacement).unwrap();
            }
            "revision" => {
                registry.db.execute("UPDATE shared_views SET revision=revision+1", []).unwrap();
            }
            "moved" => {
                registry.apply(&ModelEdit::ChangeView {scope: target.scope.clone(), expected_revision: target.view_revision,
                    operation: uuid::Uuid::new_v4(), change: Change::Transfer {id: target.pane.id,
                    destination: Scope::Worktree {repository: directory.path().into(), path: directory.path().into()}, before: None}}).unwrap();
            }
            "registry" => target.registry = uuid::Uuid::new_v4(),
            "agent" => {target.agent.name = Some("forged selected agent binding".into());}
            "forged-pane" => {target.pane.title = "Not what was displayed".into();}
            _ => unreachable!(),
        }
        let before = registry.model().unwrap();
        let run = registry.agent_run(accepted.run.id).unwrap();
        assert!(registry.confirm_presentation_repair(&Origin::User, &target).is_err(), "{change}");
        assert_eq!(registry.model().unwrap(), before, "{change} cannot remove a replacement");
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    }
}

#[test]
fn presentation_repair_inspection_refuses_unowned_open_active_pending_or_recorded_generation() {
    for case in ["origin", "open", "active", "pending", "recorded", "mismatched-recorded", "live", "pending-pane"] {
        let (mut registry, _directory, accepted, target) = presentation_repair_fixture();
        let origin = if case == "origin" {Origin::Automation {id: uuid::Uuid::new_v4()}} else {Origin::User};
        match case {
            "open" => {
                let mut agent = target.agent.clone(); agent.closed = false;
                registry.db.execute("UPDATE agents SET closed=0,record=?2 WHERE id=?1", params![agent.id.to_string(), encode(&agent).unwrap()]).unwrap();
            }
            "active" => {
                registry.db.execute("UPDATE agent_runs SET state='waiting' WHERE agent=?1", [accepted.agent.id.to_string()]).unwrap();
            }
            "pending" => {
                // Deliberately inconsistent legacy state must not let a closed
                // record bypass an independently retained launch reservation.
                let source = target.agent.session.as_ref().unwrap();
                registry.db.execute("INSERT INTO agent_launches(agent,session_id,source) VALUES(?1,?2,?3)",
                    params![accepted.agent.id.to_string(), source.id().to_string(), encode(source).unwrap()]).unwrap();
            }
            "recorded" | "mismatched-recorded" => {
                let source = target.agent.console.as_ref().unwrap();
                let crate::model::CardKind::Terminal(ended) = &target.pane.kind else {unreachable!()};
                let mut recorded = ended.clone();
                if case == "mismatched-recorded" { recorded.id = crate::model::TerminalId::new(); }
                registry.db.execute("INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,?2,?3)",
                    params![target.pane.id.to_string(), encode(source).unwrap(), encode(&recorded).unwrap()]).unwrap();
            }
            "live" | "pending-pane" => {
                let mut pane = target.pane.clone();
                if case == "live" {pane.kind = crate::model::CardKind::Terminal(target.agent.console.as_ref().unwrap().clone());}
                else {pane.pending_terminal_start = true;}
                publish_console(&mut registry, &target.scope, &target.pane, &pane).unwrap();
            }
            "origin" => {}
            _ => unreachable!(),
        }
        let before = registry.model().unwrap();
        assert!(registry.inspect_presentation_repair(&origin, accepted.agent.id).is_err(), "{case}");
        assert!(registry.confirm_presentation_repair(&origin, &target).is_err(), "{case}");
        assert_eq!(registry.model().unwrap(), before);
    }
}

#[test]
fn presentation_repair_rolls_back_membership_tombstone_and_sequence_on_write_failure() {
    let (mut registry, _directory, accepted, target) = presentation_repair_fixture();
    let before = registry.model().unwrap();
    let agent = registry.agent(accepted.agent.id).unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    registry.db.execute_batch("CREATE TRIGGER reject_presentation_repair BEFORE UPDATE ON metadata
        BEGIN SELECT RAISE(FAIL, 'fixture metadata write refused'); END;").unwrap();
    assert!(registry.confirm_presentation_repair(&Origin::User, &target).unwrap_err().contains("fixture metadata"));
    assert_eq!(registry.model().unwrap(), before);
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
    assert_eq!(registry.inspect_presentation_repair(&Origin::User, accepted.agent.id).unwrap(), Some(target.clone()));
    registry.db.execute_batch("DROP TRIGGER reject_presentation_repair;").unwrap();
    assert!(registry.confirm_presentation_repair(&Origin::User, &target).unwrap());
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
    let host = std::sync::Arc::new(std::sync::Mutex::new(state));
    assert!(crate::host::agent_execution::process(&host, Authority::session("unissued".into()),
        Operation::ConfirmPresentationRepair {observed: target.clone()}).is_err());
    assert_eq!(crate::host::agent_execution::process(&host, Authority::default(),
        Operation::InspectPresentationRepair(accepted.agent.id)).unwrap(), Reply::PresentationRepair(Some(target.clone())));
    assert_eq!(crate::host::agent_execution::process(&host, Authority::default(),
        Operation::ConfirmPresentationRepair {observed: target.clone()}).unwrap(),
        Reply::PresentationRemoved {agent: accepted.agent.id, removed: true});
    assert_eq!(crate::host::agent_execution::process(&host, Authority::default(),
        Operation::ConfirmPresentationRepair {observed: target}).unwrap(),
        Reply::PresentationRemoved {agent: accepted.agent.id, removed: false});
    let state = host.lock().unwrap();
    assert!(state.chats.is_empty() && state.terminals.is_empty(), "repair never needs a hosted provider or terminal");
}
