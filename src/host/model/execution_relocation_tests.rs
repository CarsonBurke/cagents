// Included in execution::tests: all mutations go through a real Registry.
fn located_console_fixture() -> (
    Registry, tempfile::TempDir, Accepted, crate::workspace_model::views::Pane,
    crate::project_recovery::Relocation,
) {
    use crate::model::{CardId, CardKind, TerminalContinuity, TerminalSession};
    let directory = tempfile::tempdir().unwrap();
    let old = directory.path().join("old");
    let moved = directory.path().join("moved");
    std::fs::create_dir(&old).unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    registry.apply(&ModelEdit::OpenProject {
        repository: old.clone(), name: "Located fixture".into(), kind: ProjectKind::Folder,
    }).unwrap();
    let request = Submit {
        name: None, destination: Destination::New(Spec {
            provider: Provider::Claude, account: None,
            target: Target { repository: old.clone(), directory: old.clone(), placement: None },
            surface: Surface::Chat, model: None, permission_mode: None, reasoning_effort: None,
        }), prompt: "No provider process is launched by this Registry fixture".into(),
        idempotency_key: "located-ended-console".into(),
    };
    let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
    let mut retained = source();
    let HostedSession::Chat(chat) = &mut retained else { unreachable!() };
    chat.launch.recipe.working_directory = old.clone();
    registry.bind_agent(accepted.agent.id, None, retained.clone()).unwrap();
    let started = registry.update_agent_run(accepted.run.id, accepted.run.revision,
        Update::Started { provider_key: "located-provider".into() }, 2).unwrap();
    registry.update_agent_run(accepted.run.id, started.revision, Update::Finished {
        state: State::Succeeded, result: ResultSummary { text: "Retained report".into(), ..Default::default() },
    }, 3).unwrap();
    let HostedSession::Chat(chat) = &retained else { unreachable!() };
    let proof = chat.proof.as_ref().unwrap();
    let console = TerminalSession {
        id: chat.id, owned_codex_listener: None, recovery_origin: None, predecessor: None,
        launch: chat.launch.recipe.clone(), transcript_path: None, resume_hint: None,
        continuity: TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        },
    };
    registry.bind_agent_console(accepted.agent.id, &retained, console.clone()).unwrap();
    let pane = crate::workspace_model::views::Pane {
        id: CardId::from_uuid(accepted.agent.id.as_uuid()), title: "Console".into(), user_title: None,
        pending_terminal_start: false, width_fraction: 0.5, scope: Some(old.clone()),
        kind: CardKind::Terminal(console),
    };
    let relocation = crate::project_recovery::Relocation {
        repository: old.clone(), destination: moved.clone(), old_root: old, new_root: moved,
    };
    (registry, directory, accepted, pane, relocation)
}

fn located_source_json(registry: &Registry, card: crate::model::CardId) -> serde_json::Value {
    let source: String = registry.db.query_row(
        "SELECT source FROM shared_view_terminal_sources WHERE pane=?1", [card.to_string()],
        |row| row.get(0),
    ).unwrap();
    serde_json::from_str(&source).unwrap()
}

#[test]
fn located_proved_ended_console_keeps_history_through_hints_restart_and_close() {
    use crate::{model::{CardKind, TerminalContinuity}, workspace_model::views::Scope};
    for already_closed in [false, true] {
        let (mut registry, directory, accepted, pane, relocation) = located_console_fixture();
        if already_closed { registry.close_agent(&Origin::User, accepted.agent.id).unwrap(); }
        let scope = Scope::Channel(accepted.agent.channel().unwrap());
        registry.apply(&ModelEdit::SeedView { scope: scope.clone(), panes: vec![pane.clone()], closed: vec![] }).unwrap();
        let ended = ended_console(&pane);
        publish_console(&mut registry, &scope, &pane, &ended).unwrap();
        let issued = registry.agent(accepted.agent.id).unwrap();
        let run = registry.agent_run(accepted.run.id).unwrap();
        std::fs::rename(&relocation.old_root, &relocation.new_root).unwrap();
        registry.apply(&ModelEdit::RelocateProject { relocation: relocation.clone() }).unwrap();
        assert_eq!(registry.agent(accepted.agent.id).unwrap(), issued, "Locate never rewrites issued Agent launch/spec/source");
        let current = super::super::views::read(&registry.db, &scope).unwrap().panes.remove(0);
        let CardKind::Terminal(session) = &current.kind else { unreachable!() };
        let TerminalContinuity::RelaunchAvailable { recipe, .. } = &session.continuity else { unreachable!() };
        assert_eq!(session.launch.working_directory, relocation.old_root);
        assert_eq!(recipe.working_directory, relocation.new_root);
        let receipt = located_source_json(&registry, pane.id);
        assert_eq!(receipt["source"], serde_json::to_value(issued.console.as_ref().unwrap()).unwrap());
        let CardKind::Terminal(historical) = &ended.kind else { unreachable!() };
        assert_eq!(receipt["historical_end"], serde_json::to_value(historical).unwrap());
        let mut late = current.clone();
        let CardKind::Terminal(session) = &mut late.kind else { unreachable!() };
        session.resume_hint = Some("late hint after Locate".into());
        late.title = "Completed located console".into();
        publish_console(&mut registry, &scope, &current, &late).unwrap();
        assert_eq!(located_source_json(&registry, pane.id), receipt, "annotations do not rewrite historical receipt");
        let again = crate::project_recovery::Relocation {
            repository: relocation.destination.clone(), old_root: relocation.new_root.clone(),
            destination: directory.path().join("moved-again"), new_root: directory.path().join("moved-again"),
        };
        std::fs::rename(&again.old_root, &again.new_root).unwrap();
        registry.apply(&ModelEdit::RelocateProject { relocation: again.clone() }).unwrap();
        assert_eq!(located_source_json(&registry, pane.id), receipt, "repeat Locate keeps the same fixed historical anchor");
        let projected = super::super::views::read(&registry.db, &scope).unwrap();
        let CardKind::Terminal(session) = &projected.panes[0].kind else { unreachable!() };
        let TerminalContinuity::RelaunchAvailable { recipe, .. } = &session.continuity else { unreachable!() };
        assert_eq!(recipe.working_directory, again.new_root);
        assert_eq!(session.launch.working_directory, relocation.old_root);
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry.preflight_agent_close(&Origin::User, accepted.agent.id).unwrap();
        registry.close_agent(&Origin::User, accepted.agent.id).unwrap();
        let view = super::super::views::read(&registry.db, &scope).unwrap();
        assert!(view.panes.is_empty());
        assert!(view.retired.contains(&pane.id));
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
        let count: u64 = registry.db.query_row("SELECT count(*) FROM shared_view_terminal_sources", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn located_unknown_end_stays_unproven_and_requires_current_presentation_consent() {
    use crate::workspace_model::views::Scope;
    let (mut registry, directory, accepted, pane, relocation) = located_console_fixture();
    registry.close_agent(&Origin::User, accepted.agent.id).unwrap();
    let scope = Scope::Channel(accepted.agent.channel().unwrap());
    let ended = ended_console(&pane);
    registry.apply(&ModelEdit::SeedView { scope: scope.clone(), panes: vec![ended], closed: vec![] }).unwrap();
    let agent = registry.agent(accepted.agent.id).unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    std::fs::rename(&relocation.old_root, &relocation.new_root).unwrap();
    registry.apply(&ModelEdit::RelocateProject { relocation }).unwrap();
    assert!(located_source_json(&registry, pane.id)["source"].is_null());
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let before = registry.model().unwrap();
    assert!(registry.close_agent(&Origin::User, accepted.agent.id).unwrap_err().contains("historical generation proof"));
    assert_eq!(registry.model().unwrap(), before);
    let observation = registry.inspect_presentation_repair(&Origin::User, accepted.agent.id).unwrap().unwrap();
    assert!(registry.confirm_presentation_repair(&Origin::User, &observation).unwrap());
    assert!(super::super::views::read(&registry.db, &scope).unwrap().panes.is_empty());
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent);
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
}

#[test]
fn located_same_id_foreign_generation_cannot_be_retired_by_the_old_agent() {
    use crate::{model::{CardKind, HostIncarnation, TerminalContinuity}, workspace_model::views::Scope};
    let (mut registry, _directory, accepted, pane, relocation) = located_console_fixture();
    let scope = Scope::Channel(accepted.agent.channel().unwrap());
    registry.apply(&ModelEdit::SeedView { scope: scope.clone(), panes: vec![pane.clone()], closed: vec![] }).unwrap();
    let mut foreign = pane.clone();
    let CardKind::Terminal(session) = &mut foreign.kind else { unreachable!() };
    let TerminalContinuity::Live { host_incarnation, .. } = &mut session.continuity else { unreachable!() };
    *host_incarnation = HostIncarnation::new("foreign-same-public-id");
    publish_console(&mut registry, &scope, &pane, &foreign).unwrap();
    let ended = ended_console(&foreign);
    publish_console(&mut registry, &scope, &foreign, &ended).unwrap();
    std::fs::rename(&relocation.old_root, &relocation.new_root).unwrap();
    registry.apply(&ModelEdit::RelocateProject { relocation }).unwrap();
    let current = super::super::views::read(&registry.db, &scope).unwrap();
    registry.close_agent(&Origin::User, accepted.agent.id).unwrap();
    assert_eq!(super::super::views::read(&registry.db, &scope).unwrap(), current);
    assert!(registry.inspect_presentation_repair(&Origin::User, accepted.agent.id).unwrap_err().contains("Recorded terminal provenance"));
}

#[test]
fn located_ended_receipt_and_view_roll_back_atomically_on_write_failure() {
    use crate::workspace_model::views::Scope;
    let (mut registry, _directory, accepted, pane, relocation) = located_console_fixture();
    let scope = Scope::Channel(accepted.agent.channel().unwrap());
    registry.apply(&ModelEdit::SeedView { scope: scope.clone(), panes: vec![pane.clone()], closed: vec![] }).unwrap();
    publish_console(&mut registry, &scope, &pane, &ended_console(&pane)).unwrap();
    let before = registry.model().unwrap();
    let receipt = located_source_json(&registry, pane.id);
    std::fs::rename(&relocation.old_root, &relocation.new_root).unwrap();
    registry.db.execute_batch("CREATE TRIGGER refuse_located_end BEFORE UPDATE ON shared_views BEGIN SELECT RAISE(FAIL,'fixture location write refused'); END;").unwrap();
    assert!(registry.apply(&ModelEdit::RelocateProject { relocation: relocation.clone() }).is_err());
    assert_eq!(registry.model().unwrap(), before);
    assert_eq!(located_source_json(&registry, pane.id), receipt);
    registry.db.execute_batch("DROP TRIGGER refuse_located_end").unwrap();
    registry.apply(&ModelEdit::RelocateProject { relocation }).unwrap();
    registry.close_agent(&Origin::User, accepted.agent.id).unwrap();
    assert!(super::super::views::read(&registry.db, &scope).unwrap().panes.is_empty());
}

#[test]
fn arbitrary_relaunch_directory_edits_do_not_acquire_locate_provenance() {
    use crate::{model::{CardKind, TerminalContinuity}, workspace_model::views::Scope};
    let (mut registry, _directory, accepted, pane, _relocation) = located_console_fixture();
    let scope = Scope::Channel(accepted.agent.channel().unwrap());
    registry.apply(&ModelEdit::SeedView { scope: scope.clone(), panes: vec![pane.clone()], closed: vec![] }).unwrap();
    let ended = ended_console(&pane);
    publish_console(&mut registry, &scope, &pane, &ended).unwrap();
    let mut changed = ended.clone();
    let CardKind::Terminal(session) = &mut changed.kind else { unreachable!() };
    let TerminalContinuity::RelaunchAvailable { recipe, .. } = &mut session.continuity else { unreachable!() };
    recipe.working_directory = "/unrelated/unapproved".into();
    publish_console(&mut registry, &scope, &ended, &changed).unwrap();
    let before = registry.model().unwrap();
    assert!(registry.close_agent(&Origin::User, accepted.agent.id).unwrap_err().contains("historical generation proof"));
    assert_eq!(registry.model().unwrap(), before);
    assert!(!registry.agent(accepted.agent.id).unwrap().closed);
}

#[test]
fn schema17_upgrade_preserves_issued_end_and_all_stored_authority_before_locate() {
    use crate::workspace_model::views::Scope;
    let (mut registry, directory, accepted, pane, relocation) = located_console_fixture();
    let scope = Scope::Channel(accepted.agent.channel().unwrap());
    registry.apply(&ModelEdit::SeedView { scope: scope.clone(), panes: vec![pane.clone()], closed: vec![] }).unwrap();
    publish_console(&mut registry, &scope, &pane, &ended_console(&pane)).unwrap();
    let agent = registry.agent(accepted.agent.id).unwrap();
    let source = serde_json::to_string(agent.session.as_ref().unwrap()).unwrap();
    // Populate the non-presentation selected rows too: upgrading their schema
    // must neither rotate an owner/cursor nor fabricate new runtime authority.
    registry.db.execute("INSERT INTO session_locations(session,source,directory,previous_sources) VALUES(?1,?2,?3,'[]')",
        params![agent.session.as_ref().unwrap().id().to_string(), source, relocation.old_root.to_str().unwrap()]).unwrap();
    registry.db.execute("INSERT INTO native_actor_cursors(actor,owner,sequence,digest) VALUES(?1,?2,7,?3)",
        params!["preserved-actor", "preserved-owner", vec![23u8; 32]]).unwrap();
    registry.db.execute("INSERT INTO session_observer_owners(session,instance,stamp,source) VALUES(?1,?2,?3,?4)",
        params![agent.session.as_ref().unwrap().id().to_string(), uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string(), source]).unwrap();
    for table in ["agent_locations", "session_locations", "native_actor_cursors", "shared_view_terminal_sources", "session_observer_owners"] {
        let count: u64 = registry.db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).unwrap();
        assert!(count > 0, "preservation fixture must populate {table}");
    }
    super::super::automation_extensions_schema::discard_for_legacy_fixture(&registry.db);
    registry.db.pragma_update(None, "user_version", 17).unwrap();
    let before = super::super::schema::stored_contents(&registry.db);
    let issued = located_source_json(&registry, pane.id);
    assert!(issued.get("historical_end").is_none(), "schema17 fixture retains its original issued-source JSON");
    let run = registry.agent_run(accepted.run.id).unwrap();
    drop(registry);
    let path = directory.path().join("model.sqlite3");
    for _ in 0..2 {
        let registry = Registry::at(&path).unwrap();
        assert_eq!(registry.db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), super::super::SCHEMA_VERSION);
        let migrated=super::super::schema::stored_contents(&registry.db);
        for (name,ddl,rows) in &before {
            let (_,current_ddl,current_rows)=migrated.iter().find(|(table,_,_)|table==name).expect("original table retained");
            if name=="metadata" {
                let columns=rows.first().map(Vec::len).unwrap_or(0);
                let original_columns:Vec<_>=current_rows.iter().map(|row|row[..columns].to_vec()).collect();
                assert_eq!(&original_columns,rows,"original metadata unchanged");
            } else {
                assert_eq!(current_ddl,ddl,"original {name} layout unchanged");
                assert_eq!(current_rows,rows,"original {name} authority unchanged");
            }
        }
        assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent);
        assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
        assert_eq!(located_source_json(&registry, pane.id), issued);
    }
    let mut registry = Registry::at(&path).unwrap();
    std::fs::rename(&relocation.old_root, &relocation.new_root).unwrap();
    registry.apply(&ModelEdit::RelocateProject { relocation }).unwrap();
    assert_eq!(located_source_json(&registry, pane.id)["source"], issued);
    assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent);
    registry.close_agent(&Origin::User, accepted.agent.id).unwrap();
    assert!(super::super::views::read(&registry.db, &scope).unwrap().panes.is_empty());
    assert_eq!(registry.agent_run(accepted.run.id).unwrap(), run);
}
