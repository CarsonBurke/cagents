//! Durable intent retirement without creating any provider or helper process.
use super::*;
use crate::{
    agent::Provider,
    agent_execution::Target,
    host::state::{HostLock, HostState},
    model::{HostSessionProof, ProjectKind},
    workspace_model::ModelEdit,
};

#[test]
fn ordinary_intent_clear_failure_retains_exact_context_and_retries_in_same_host() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let database = root.path().join("registry.sqlite3");
    let mut store = Registry::at(&database).unwrap();
    store
        .apply(&ModelEdit::OpenProject {
            repository: project.clone(),
            name: "Intent cleanup fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let request = Submit {
        name: None,
        destination: Destination::New(Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository: project.clone(),
                directory: project.clone(),
                placement: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        }),
        prompt: "No provider is dispatched by this fixture".into(),
        idempotency_key: "retained-intent".into(),
    };
    let accepted = store.submit_agent(&Origin::User, &request, 1).unwrap();
    let proof = HostSessionProof {
        host_session_id: "intent-cleanup".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "same-host".into(),
    };
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        project,
        &Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(proof.clone());
    let intent = HostedSession::Chat(source);
    store
        .reserve_agent_launch(accepted.agent.id, &intent)
        .unwrap();
    let context = Arc::new(chat::protocol::ExecutionContext::default());
    // No owner or process was ever created. The ordinary latch is still
    // required before this pre-provider evidence can clear a durable intent.
    let mut state = HostState::new(proof);
    state.install_model(store);
    state.begin_ordinary_exit_cleanup();
    state
        .execution_startups
        .insert(accepted.agent.id, (intent.clone(), context.clone()));
    let host = Arc::new(HostLock::new(state));
    finish_ordinary_startups(&host).unwrap();
    assert!(
        host.lock()
            .unwrap()
            .execution_startups
            .contains_key(&accepted.agent.id)
    );
    context.request_ordinary_cleanup();
    let fault = rusqlite::Connection::open(&database).unwrap();
    fault.execute_batch("CREATE TRIGGER refuse_owned_launch_clear BEFORE DELETE ON agent_launches BEGIN SELECT RAISE(ABORT, 'owned launch clear fault'); END;").unwrap();
    assert!(
        finish_ordinary_startups(&host)
            .unwrap_err()
            .contains("owned launch clear fault")
    );
    {
        let state = host.lock().unwrap();
        let (retained, owner) = &state.execution_startups[&accepted.agent.id];
        assert_eq!(retained, &intent);
        assert!(Arc::ptr_eq(owner, &context));
    }
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap(),
        Some(intent.clone())
    );
    fault
        .execute_batch("DROP TRIGGER refuse_owned_launch_clear;")
        .unwrap();
    // A successful no-op DELETE is not a receipt for a different durable
    // source. Inject only the row replacement; keep the original memory owner
    // and its no-provider evidence unchanged.
    let mut successor = intent.clone();
    let HostedSession::Chat(session) = &mut successor else {
        unreachable!()
    };
    session.launch.recipe.arguments.push("successor".into());
    fault
        .execute(
            "UPDATE agent_launches SET source = ?2 WHERE agent = ?1",
            rusqlite::params![
                accepted.agent.id.to_string(),
                serde_json::to_string(&successor).unwrap()
            ],
        )
        .unwrap();
    assert!(
        finish_ordinary_startups(&host)
            .unwrap_err()
            .contains("successor was preserved")
    );
    {
        let state = host.lock().unwrap();
        let (original, owner) = &state.execution_startups[&accepted.agent.id];
        assert_eq!(original, &intent);
        assert!(Arc::ptr_eq(owner, &context));
    }
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap(),
        Some(successor)
    );
    fault
        .execute(
            "UPDATE agent_launches SET source = ?2 WHERE agent = ?1",
            rusqlite::params![
                accepted.agent.id.to_string(),
                serde_json::to_string(&intent).unwrap()
            ],
        )
        .unwrap();
    finish_ordinary_startups(&host).unwrap();
    assert!(host.lock().unwrap().execution_startups.is_empty());
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap(),
        None
    );
}
