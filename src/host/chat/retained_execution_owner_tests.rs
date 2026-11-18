//! Durable ownership is required before either run settlement or its helper ACK.
use super::*;
use crate::{
    account::scope::LaunchAccountScope,
    agent_execution::{Destination, HostedSession, Origin, Run, Spec, Submit, Surface, Target},
    daemon::holder_registry::{Kind, Process, Record as HolderRecord},
    host::model::Registry,
};
use std::os::unix::fs::MetadataExt;

struct Fixture {
    root: tempfile::TempDir,
    registry: Registry,
    identity: chat::runtime::ActorIdentity,
    source: Session,
    scope: LaunchAccountScope,
    run: Run,
    issued_sources: Vec<Session>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        registry
            .apply(&crate::workspace_model::ModelEdit::OpenProject {
                repository: root.path().into(),
                name: "Exact retained execution owner".into(),
                kind: crate::model::ProjectKind::Folder,
            })
            .unwrap();
        let accepted = registry
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: None,
                    destination: Destination::New(Spec {
                        provider: Provider::Claude,
                        account: None,
                        target: Target {
                            repository: root.path().into(),
                            directory: root.path().into(),
                            placement: None,
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "A durable accepted run".into(),
                    idempotency_key: "exact-retained-owner".into(),
                },
                1,
            )
            .unwrap();
        let proof = HostSessionProof {
            host_session_id: "execution-owner-host".into(),
            boot_id: "execution-owner-boot".into(),
            host_incarnation: "execution-owner-incarnation".into(),
        };
        let source = Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        registry
            .bind_agent(accepted.agent.id, None, HostedSession::Chat(source.clone()))
            .unwrap();
        let identity = chat::runtime::ActorIdentity {
            instance: uuid::Uuid::new_v4(),
            record: HolderRecord {
                version: 4,
                build: None,
                kind: Kind::Native,
                application: crate::instance::APPLICATION_ID.into(),
                boot: proof.boot_id,
                terminal: source.id,
                holder: Process { pid: 42, start: 7 },
                root: Process { pid: 43, start: 8 },
            },
        };
        let home = root.path().join("provider-home");
        std::fs::create_dir(&home).unwrap();
        let metadata = std::fs::metadata(&home).unwrap();
        let scope = LaunchAccountScope {
            provider: Provider::Claude,
            canonical_home: home.canonicalize().unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: Vec::new(),
        };
        Self {
            root,
            registry,
            identity,
            issued_sources: vec![source.clone()],
            source,
            scope,
            run: accepted.run,
        }
    }

    fn install(&mut self) {
        self.registry
            .register_native_actor_ownership(
                self.identity.instance,
                &self.identity.record,
                &self.source,
                Ok(&self.scope),
            )
            .unwrap();
    }

    fn authenticate(&self) -> Result<ExecutionOwner, String> {
        ExecutionOwner::authenticate(
            self.registry
                .native_actor_ownership(self.identity.instance)?,
            &self.identity,
            &self.source,
            Ok(&self.scope),
            |sources| {
                if sources
                    .iter()
                    .all(|source| self.issued_sources.contains(source))
                {
                    Ok(())
                } else {
                    Err("Fixture helper refused a source outside its issued aliases".into())
                }
            },
        )
    }

    fn settle_loss_with(&mut self, owner: &ExecutionOwner) -> Result<bool, String> {
        require_execution_owner(&self.registry, Some(owner), true, None)?;
        self.registry.commit_native_actor_loss(
            self.identity.instance,
            &self.identity.record,
            &self.source,
            &[self.run.id],
            "Fixture retained outcome loss",
            2,
        )
    }

    fn settle_loss(&mut self) -> Result<bool, String> {
        let owner = self.authenticate()?;
        self.settle_loss_with(&owner)
    }

    fn assert_unsettled(&self) {
        assert_eq!(self.registry.agent_run(self.run.id).unwrap(), self.run);
        assert_eq!(
            self.registry
                .native_actor_cursor_for_test(self.identity.instance)
                .unwrap(),
            None
        );
    }
}

#[test]
fn missing_pending_and_revoked_owners_cannot_settle_an_accepted_run() {
    for phase in 0..3 {
        let mut fixture = Fixture::new();
        match phase {
            0 => {}
            1 => fixture
                .registry
                .register_pending_native_actor_ownership(
                    fixture.identity.instance,
                    &fixture.identity.record,
                    &fixture.source,
                    Ok(&fixture.scope),
                )
                .unwrap(),
            _ => {
                fixture.install();
                // A separate private fixture connection models durable ownership
                // disappearing after HOST captured a previously valid handle.
                rusqlite::Connection::open(fixture.root.path().join("registry.sqlite3"))
                    .unwrap()
                    .execute("DELETE FROM native_actor_ownerships", [])
                    .unwrap();
            }
        }
        assert!(fixture.settle_loss().is_err(), "phase {phase}");
        fixture.assert_unsettled();
        // Cleanup attention and a zero-run loss still grant no run authority.
        require_execution_owner(&fixture.registry, None, false, None).unwrap();
        fixture.assert_unsettled();
    }
}

#[test]
fn installed_owner_requires_exact_actor_source_account_and_unretired_lifetime() {
    for mutation in 0..6 {
        let mut fixture = Fixture::new();
        fixture.install();
        match mutation {
            0 => fixture.identity.record.holder.start += 1,
            1 => {
                fixture.source.proof.as_mut().unwrap().host_incarnation =
                    "different-attachment".into();
            }
            2 => fixture.source.launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
            3 => fixture.scope.inode += 1,
            4 => fixture
                .source
                .launch
                .recipe
                .arguments
                .push("--other-model".into()),
            _ => fixture
                .registry
                .commit_native_actor_retirement(
                    fixture.identity.instance,
                    &fixture.identity.record,
                    &fixture.source,
                    Ok(&fixture.scope),
                    fixture.source.launch.session_id.as_deref(),
                )
                .unwrap(),
        }
        assert!(fixture.settle_loss().is_err(), "mutation {mutation}");
        fixture.assert_unsettled();
    }
}

#[test]
fn exact_installed_owner_settles_once_without_reopening_its_account_path() {
    let mut fixture = Fixture::new();
    fixture.install();
    // Settlement relies on the captured tuple and never grants provider-home
    // access. Closing an actor must still settle after that home disappears.
    std::fs::remove_dir(&fixture.scope.canonical_home).unwrap();
    assert!(fixture.settle_loss().unwrap());
    assert_eq!(
        fixture.registry.agent_run(fixture.run.id).unwrap().state,
        crate::agent_execution::State::Interrupted
    );
    assert!(!fixture.settle_loss().unwrap());
}

#[test]
fn acknowledged_pair_and_proof_aliases_keep_the_original_durable_execution_owner() {
    let mut fixture = Fixture::new();
    fixture.install();
    let original = fixture.source.clone();
    let proof = fixture.source.proof.as_mut().unwrap();
    proof.host_incarnation = "successor-attachment".into();
    let proof = proof.clone();
    fixture.source.terminal = Some(crate::model::TerminalSession {
        id: fixture.source.id,
        launch: crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: fixture.root.path().into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        continuity: crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        },
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    });
    fixture.issued_sources.push(fixture.source.clone());
    fixture
        .registry
        .adopt_authenticated_agent_session(
            &[HostedSession::Chat(original.clone())],
            &HostedSession::Chat(fixture.source.clone()),
        )
        .unwrap();
    assert_eq!(
        fixture
            .registry
            .native_actor_ownership(fixture.identity.instance)
            .unwrap()
            .unwrap()
            .source(),
        &original,
        "the census deliberately predates the exact acknowledged aliases"
    );
    assert!(fixture.settle_loss().unwrap());
    assert_eq!(
        fixture.registry.agent_run(fixture.run.id).unwrap().state,
        crate::agent_execution::State::Interrupted
    );
}

#[test]
fn source_authentication_does_not_survive_durable_row_mutation_or_revocation() {
    for revoked in [false, true] {
        let mut fixture = Fixture::new();
        fixture.install();
        let captured = fixture.authenticate().unwrap();
        if revoked {
            rusqlite::Connection::open(fixture.root.path().join("registry.sqlite3"))
                .unwrap()
                .execute("DELETE FROM native_actor_ownerships", [])
                .unwrap();
        } else {
            let mut changed = fixture.source.clone();
            changed.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            fixture
                .registry
                .register_native_actor_ownership(
                    fixture.identity.instance,
                    &fixture.identity.record,
                    &changed,
                    Ok(&fixture.scope),
                )
                .unwrap();
        }
        assert!(fixture.settle_loss_with(&captured).is_err());
        fixture.assert_unsettled();
    }
}

#[test]
fn a_registered_descriptor_also_needs_exact_helper_alias_recognition() {
    let mut fixture = Fixture::new();
    fixture.install();
    fixture.issued_sources.clear();
    assert!(fixture.settle_loss().is_err());
    fixture.assert_unsettled();
}

fn answer_sync_fixture() -> (tempfile::TempDir, SharedHost, AnswerSync, Run, Handle) {
    let mut fixture = Fixture::new();
    fixture.install();
    let owner = fixture.authenticate().unwrap();
    // Same retained actor, different attachments. These proxies deliberately
    // have no helper socket: rejected guards must not reach input or ACK RPCs.
    let (handle, replacement) = Handle::fixture_remote_attachment_pair();
    let mut state = HostState::new(fixture.source.proof.clone().unwrap());
    state.install_model(fixture.registry);
    state.chats.insert(
        fixture.source.id,
        super::super::Record {
            execution_instance: fixture.identity.instance,
            console_recipe: None,
            paired_terminal: None,
            session: fixture.source.clone(),
            handle: handle.clone(),
            source_process: None,
            subscribers: Default::default(),
            observations: Default::default(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    let host = Arc::new(HostLock::new(state));
    let registry = super::super::super::agent_execution::registry(&host).unwrap();
    let synchronization = AnswerSync {
        target: SemanticTarget {
            handle,
            source: fixture.source,
            identity: fixture.identity,
            notifications: None,
        },
        registry,
        owner: Some(owner),
    };
    synchronization.require_current(&host).unwrap();
    (
        fixture.root,
        host,
        synchronization,
        fixture.run,
        replacement,
    )
}

#[test]
fn answer_watermark_never_accepts_revoked_source_account_or_attachment_authority() {
    for mutation in ["missing", "source", "account", "attachment", "actor"] {
        let (root, host, synchronization, run, replacement) = answer_sync_fixture();
        let id = synchronization.target.source.id;
        match mutation {
            "missing" => {
                rusqlite::Connection::open(root.path().join("registry.sqlite3"))
                    .unwrap()
                    .execute("DELETE FROM native_actor_ownerships", [])
                    .unwrap();
            }
            "source" => {
                host.lock()
                    .unwrap()
                    .chats
                    .get_mut(&id)
                    .unwrap()
                    .session
                    .launch
                    .session_id = Some(uuid::Uuid::new_v4().to_string())
            }
            "account" => {
                // Physical admission correctly refuses a fabricated inode.
                // Model a durable row mutation after the exact capture using
                // only this private fixture's database, preserving admission.
                let changed =
                    rusqlite::Connection::open(root.path().join("registry.sqlite3")).unwrap();
                let actor = synchronization.target.identity.instance.to_string();
                let stored: String = changed
                    .query_row(
                        "SELECT ownership FROM native_actor_ownerships WHERE actor=?1",
                        [&actor],
                        |row| row.get(0),
                    )
                    .unwrap();
                let mut ownership: serde_json::Value = serde_json::from_str(&stored).unwrap();
                let inode = ownership["scope"]["Ok"]["inode"].as_u64().unwrap();
                ownership["scope"]["Ok"]["inode"] = (inode + 1).into();
                assert_eq!(
                    changed
                        .execute(
                            "UPDATE native_actor_ownerships SET ownership=?1 WHERE actor=?2",
                            rusqlite::params![serde_json::to_string(&ownership).unwrap(), actor],
                        )
                        .unwrap(),
                    1
                );
            }
            "attachment" => host.lock().unwrap().chats.get_mut(&id).unwrap().handle = replacement,
            "actor" => {
                host.lock()
                    .unwrap()
                    .chats
                    .get_mut(&id)
                    .unwrap()
                    .execution_instance = uuid::Uuid::new_v4()
            }
            _ => unreachable!(),
        }
        let registry = synchronization.registry.clone();
        let handle = synchronization.target.handle.clone();
        let actor = synchronization.target.identity.instance;
        assert!(synchronization.complete(&host).is_err(), "{mutation}");
        let registry = registry.lock().unwrap();
        assert_eq!(registry.agent_run(run.id).unwrap(), run, "{mutation}");
        assert_eq!(registry.native_actor_cursor_for_test(actor).unwrap(), None);
        assert!(
            handle.pending_retained_inputs().is_empty(),
            "no answer was regenerated"
        );
    }
}

#[test]
fn answer_watermark_shared_commit_does_not_ack_a_failed_durable_write() {
    let (root, host, mut synchronization, run, _) = answer_sync_fixture();
    let fault = rusqlite::Connection::open(root.path().join("registry.sqlite3")).unwrap();
    fault.execute_batch("CREATE TRIGGER reject_answer_cursor BEFORE INSERT ON native_actor_cursors BEGIN SELECT RAISE(ABORT, 'fixture Answer cursor commit failure'); END;").unwrap();
    let error = commit_semantic_entry(
        &host,
        &synchronization.target,
        &synchronization.registry,
        &mut synchronization.owner,
        OutboxEntry {
            sequence: 1,
            update: Update::Execution {
                run: run.id,
                update: crate::agent_execution::Update::Started {
                    provider_key: "exact-fixture-provider".into(),
                },
            },
            confirmed_session: None,
        },
        false,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(
        error.contains("fixture Answer cursor commit failure"),
        "{error}"
    );
    let registry = synchronization.registry.lock().unwrap();
    assert_eq!(registry.agent_run(run.id).unwrap(), run);
    assert_eq!(
        registry
            .native_actor_cursor_for_test(synchronization.target.identity.instance)
            .unwrap(),
        None
    );
    assert!(
        synchronization
            .target
            .handle
            .pending_retained_inputs()
            .is_empty()
    );
}

#[test]
fn answer_keeps_its_owner_across_a_concurrent_identity_confirmation_only() {
    // The provider confirms its conversation while an Answer is in flight and
    // the semantic drain publishes that projection. Only the exact projection
    // of the helper-confirmed conversation preserves the captured owner.
    for published in ["confirmed", "other", "unconfirmed"] {
        let (_root, host, synchronization, _run, _) = answer_sync_fixture();
        let confirmed = uuid::Uuid::new_v4().to_string();
        if published != "unconfirmed" {
            synchronization
                .target
                .handle
                .fixture_confirm_session(confirmed.clone());
        }
        let mut projected = synchronization.target.source.clone();
        projected.launch.session_id = Some(match published {
            "other" => uuid::Uuid::new_v4().to_string(),
            _ => confirmed,
        });
        {
            let mut registry = synchronization.registry.lock().unwrap();
            let row = registry
                .native_actor_ownership(synchronization.target.identity.instance)
                .unwrap()
                .unwrap();
            registry
                .confirm_native_actor_identity(&row, &projected)
                .unwrap();
        }
        assert_eq!(
            synchronization.require_current(&host).is_ok(),
            published == "confirmed",
            "{published}"
        );
    }
}

#[test]
fn a_confirmed_identity_projects_the_resident_source_until_retirement() {
    let mut fixture = Fixture::new();
    fixture.install();
    // The census predates the resident attachment's acknowledged alias.
    let registered = fixture.source.clone();
    fixture.source.proof.as_mut().unwrap().host_incarnation = "successor-attachment".into();
    fixture.issued_sources.push(fixture.source.clone());
    let captured = fixture.authenticate().unwrap();
    let project = |source: &Session, confirmed: &str| {
        let mut projected = source.clone();
        projected.launch.session_id = Some(confirmed.into());
        projected
    };
    let confirmed = uuid::Uuid::new_v4().to_string();
    let resident = project(&fixture.source, &confirmed);
    fixture
        .registry
        .confirm_native_actor_identity(&captured.0, &resident)
        .unwrap();
    let check = |fixture: &Fixture, projected: &Session| {
        require_execution_owner(&fixture.registry, Some(&captured), true, Some(projected))
    };
    assert!(check(&fixture, &project(&registered, &confirmed)).is_err());
    assert!(check(&fixture, &resident).is_ok());
    fixture
        .registry
        .commit_native_actor_retirement(
            fixture.identity.instance,
            &fixture.identity.record,
            &resident,
            Ok(&fixture.scope),
            Some(&confirmed),
        )
        .unwrap();
    assert!(check(&fixture, &resident).is_err());
}

#[test]
fn answer_helper_ack_cannot_replace_a_missing_or_different_durable_cursor() {
    let (_root, _host, synchronization, run, _) = answer_sync_fixture();
    // Model an already-ACKed helper watermark. It is not accepted when the
    // corresponding HOST cursor is absent, even with exact installed ownership.
    assert!(synchronization.require_committed_through(1).is_err());
    let identity = &synchronization.target.identity;
    synchronization
        .registry
        .lock()
        .unwrap()
        .commit_native_actor_event(
            identity.instance,
            &identity.record,
            &synchronization.target.source,
            1,
            &NativeActorEvent::Execution {
                run: run.id,
                update: crate::agent_execution::Update::Started {
                    provider_key: "exact-fixture-provider".into(),
                },
            },
            2,
        )
        .unwrap();
    synchronization.require_committed_through(1).unwrap();
    assert!(synchronization.require_committed_through(2).is_err());
    let mut different = identity.record.clone();
    different.holder.start += 1;
    assert!(
        synchronization
            .registry
            .lock()
            .unwrap()
            .native_actor_committed_through(identity.instance, &different)
            .is_err()
    );
}

#[test]
fn a_run_transition_requires_the_confirmed_conversation_in_the_census() {
    let mut fixture = Fixture::new();
    fixture.install();
    let actor = fixture.identity.instance;
    let confirmed = uuid::Uuid::new_v4().to_string();
    // Without a helper-confirmed conversation there is nothing to require.
    require_published_identity(&fixture.registry, actor, None).unwrap();
    // A confirmed conversation the census does not carry yet refuses the
    // transition, so its run stays open until the identity is published.
    assert!(require_published_identity(&fixture.registry, actor, Some(&confirmed)).is_err());
    let owner = fixture.authenticate().unwrap();
    let mut projected = fixture.source.clone();
    projected.launch.session_id = Some(confirmed.clone());
    fixture
        .registry
        .confirm_native_actor_identity(&owner.0, &projected)
        .unwrap();
    require_published_identity(&fixture.registry, actor, Some(&confirmed)).unwrap();
    let other = uuid::Uuid::new_v4().to_string();
    assert!(require_published_identity(&fixture.registry, actor, Some(&other)).is_err());
}

/// Retained actors whose helpers are scripts, each owning one started run of
/// its own agent, attached to one HOST with a durable census.
struct ScriptedActors {
    root: tempfile::TempDir,
    host: SharedHost,
    registry: super::super::super::agent_execution::SharedRegistry,
    actors: Vec<ScriptedActor>,
}

struct ScriptedActor {
    source: Session,
    handle: Handle,
    helper: crate::chat::runtime::ScriptedHelper,
    identity: chat::runtime::ActorIdentity,
    run: crate::model::RunId,
}

impl ScriptedActor {
    fn script(&self) -> std::sync::MutexGuard<'_, crate::chat::runtime::Script> {
        self.helper.script.lock().unwrap()
    }
}

impl ScriptedActors {
    fn new(count: usize) -> Self {
        let fixture = Fixture::new();
        let Fixture {
            root,
            mut registry,
            source: template,
            scope,
            ..
        } = fixture;
        let mut actors = Vec::new();
        for index in 0..count {
            let accepted = registry
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: None,
                            target: Target {
                                repository: root.path().into(),
                                directory: root.path().into(),
                                placement: None,
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "A run its retained actor owns".into(),
                        idempotency_key: format!("scripted-retained-actor-{index}"),
                    },
                    1,
                )
                .unwrap();
            let mut source = template.clone();
            source.id = TerminalId::new();
            registry
                .bind_agent(accepted.agent.id, None, HostedSession::Chat(source.clone()))
                .unwrap();
            // Dispatched: a restart cannot treat it as never sent.
            registry
                .update_agent_run(
                    accepted.run.id,
                    accepted.run.revision,
                    crate::agent_execution::Update::Started {
                        provider_key: format!("scripted-provider-{index}"),
                    },
                    2,
                )
                .unwrap();
            let (handle, helper) = Handle::fixture_scripted(&source, Ok(scope.clone()));
            let identity = handle.actor_identity().unwrap();
            registry
                .register_native_actor_ownership(
                    identity.instance,
                    &identity.record,
                    &source,
                    Ok(&scope),
                )
                .unwrap();
            helper.script.lock().unwrap().retained_runs = vec![accepted.run.id];
            actors.push(ScriptedActor {
                source,
                handle,
                helper,
                identity,
                run: accepted.run.id,
            });
        }
        let mut state = HostState::new(template.proof.clone().unwrap());
        state.install_model(registry);
        for actor in &actors {
            state.chats.insert(
                actor.source.id,
                super::super::Record {
                    execution_instance: actor.identity.instance,
                    console_recipe: None,
                    paired_terminal: None,
                    session: actor.source.clone(),
                    handle: actor.handle.clone(),
                    source_process: None,
                    subscribers: Default::default(),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        let host = Arc::new(HostLock::new(state));
        let registry = super::super::super::agent_execution::registry(&host).unwrap();
        Self {
            root,
            host,
            registry,
            actors,
        }
    }

    fn fault(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.root.path().join("registry.sqlite3")).unwrap()
    }

    fn run(&self, actor: &ScriptedActor) -> Run {
        self.registry.lock().unwrap().agent_run(actor.run).unwrap()
    }

    fn census_session(&self, actor: &ScriptedActor) -> Option<String> {
        self.registry
            .lock()
            .unwrap()
            .native_actor_ownership(actor.identity.instance)
            .unwrap()
            .unwrap()
            .source()
            .launch
            .session_id
            .clone()
    }

    fn wait(&self, reason: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ScriptedActors {
    fn drop(&mut self) {
        // Stop the outcome broker this HOST may have started.
        self.host.lock().unwrap().shutting_down = true;
    }
}

const LOSS_REASON: &str = "Scripted helper lost its outcome continuity";

fn record_loss(actor: &ScriptedActor, confirmed: &str) {
    let mut script = actor.script();
    script.confirmed_session = Some(confirmed.into());
    script.continuity_loss = Some(crate::chat::runtime::ContinuityLoss {
        id: uuid::Uuid::new_v4(),
        affected_runs: vec![actor.run],
        reason: LOSS_REASON.into(),
        acknowledged: false,
    });
}

fn interrupted_by_loss(run: &Run) -> bool {
    run.state == crate::agent_execution::State::Interrupted
        && run.result.diagnostic.as_deref() == Some(LOSS_REASON)
}

/// A boot whose census cannot take one actor's confirmed conversation keeps
/// recovering every other actor. That actor's loss stays unacknowledged, its
/// run stays protected and open, and the outcome broker retries the
/// publication until it succeeds and the loss settles the run.
#[test]
fn a_failed_identity_publication_at_boot_is_retried_without_aborting_recovery() {
    let fixture = ScriptedActors::new(2);
    let [failing, healthy] = &fixture.actors[..] else {
        unreachable!()
    };
    let failing_conversation = uuid::Uuid::new_v4().to_string();
    let healthy_conversation = uuid::Uuid::new_v4().to_string();
    record_loss(failing, &failing_conversation);
    record_loss(healthy, &healthy_conversation);
    let fault = fixture.fault();
    fault
        .execute_batch(&format!(
            "CREATE TRIGGER reject_boot_census BEFORE UPDATE ON native_actor_ownerships \
             WHEN NEW.actor = '{}' BEGIN SELECT RAISE(ABORT, 'fixture census write failure'); END;",
            failing.identity.instance
        ))
        .unwrap();

    reconcile_startup_losses(&fixture.host)
        .expect("one actor's failed publication must not abort startup recovery");

    // Every other actor recovered: its conversation is durable, then its loss
    // settled its run and was acknowledged.
    assert_eq!(
        fixture.census_session(healthy).as_deref(),
        Some(healthy_conversation.as_str())
    );
    assert!(interrupted_by_loss(&fixture.run(healthy)));
    assert!(
        healthy
            .script()
            .continuity_loss
            .as_ref()
            .unwrap()
            .acknowledged
    );
    // The failing actor's run is neither settled by its loss nor by boot's
    // restart policy, and the helper still holds the loss.
    assert_ne!(
        fixture.census_session(failing).as_deref(),
        Some(failing_conversation.as_str())
    );
    let retained = retained_runs(&fixture.host);
    fixture
        .registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(3, &retained)
        .unwrap();
    assert!(!fixture.run(failing).state.is_terminal());
    assert!(
        !failing
            .script()
            .continuity_loss
            .as_ref()
            .unwrap()
            .acknowledged
    );

    // The broker keeps retrying while the census refuses the write.
    let attempts = failing.script().calls_of("state");
    fixture.wait("the broker never retried the publication", || {
        failing.script().calls_of("state") >= attempts + 2
    });
    assert!(!fixture.run(failing).state.is_terminal());
    assert!(
        !failing
            .script()
            .continuity_loss
            .as_ref()
            .unwrap()
            .acknowledged
    );

    fault
        .execute_batch("DROP TRIGGER reject_boot_census;")
        .unwrap();
    fixture.wait("the retried publication never settled the run", || {
        failing
            .script()
            .continuity_loss
            .as_ref()
            .unwrap()
            .acknowledged
    });
    assert_eq!(
        fixture.census_session(failing).as_deref(),
        Some(failing_conversation.as_str())
    );
    assert!(interrupted_by_loss(&fixture.run(failing)));
}

/// Publishing the conversation an outcome followed rebinds the agent before
/// the census row and the outcome commit. A restart in between finds the
/// agent on that projection while the helper has since confirmed another
/// conversation; boot must still leave the run to the helper's outcome.
#[test]
fn boot_protects_a_run_whose_agent_is_bound_to_a_followed_conversation() {
    let fixture = ScriptedActors::new(1);
    let actor = &fixture.actors[0];
    let followed = uuid::Uuid::new_v4().to_string();
    actor.script().confirmed_session = Some(uuid::Uuid::new_v4().to_string());
    actor.handle.refresh_retained_state().unwrap();
    let mut bound = actor.source.clone();
    bound.launch.session_id = Some(followed);
    let mut registry = fixture.registry.lock().unwrap();
    assert!(
        registry
            .adopt_authenticated_agent_session(
                &[HostedSession::Chat(actor.source.clone())],
                &HostedSession::Chat(bound),
            )
            .unwrap()
    );
    drop(registry);

    let retained = retained_runs(&fixture.host);
    fixture
        .registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(3, &retained)
        .unwrap();
    assert!(!fixture.run(actor).state.is_terminal());
}

/// #2849 follow-up: an outcome journaled while the helper followed one
/// conversation is read only after a reset cleared or replaced it. The run may
/// settle only once the conversation it followed is in the census.
#[test]
fn an_outcome_requires_the_conversation_it_followed_after_a_reset() {
    for reset in ["replaced", "cleared"] {
        let fixture = ScriptedActors::new(1);
        let actor = &fixture.actors[0];
        let followed = uuid::Uuid::new_v4().to_string();
        let current = (reset == "replaced").then(|| uuid::Uuid::new_v4().to_string());
        {
            let mut script = actor.script();
            script.confirmed_session = current.clone();
            script.outbox = vec![OutboxEntry {
                sequence: 1,
                update: Update::Execution {
                    run: actor.run,
                    update: crate::agent_execution::Update::Finished {
                        state: crate::agent_execution::State::Succeeded,
                        result: Default::default(),
                    },
                },
                confirmed_session: Some(followed.clone()),
            }];
        }
        // Observe the census at the instant the run settles.
        fixture
            .fault()
            .execute_batch(&format!(
                "CREATE TRIGGER settle_with_followed_identity BEFORE UPDATE ON agent_runs \
                 WHEN NEW.state = 'succeeded' AND (SELECT json_extract(ownership, \
                 '$.source.launch.session_id') FROM native_actor_ownerships WHERE actor = '{}') \
                 IS NOT '{followed}' BEGIN SELECT RAISE(ABORT, \
                 'the run settled without the conversation it followed in the census'); END;",
                actor.identity.instance
            ))
            .unwrap();

        let drained = drain(&fixture.host, actor.source.id);

        assert_eq!(drained, Ok(true), "{reset}");
        assert_eq!(
            fixture.run(actor).state,
            crate::agent_execution::State::Succeeded,
            "{reset}"
        );
        assert!(actor.script().outbox.is_empty(), "{reset}");
        // The census then returns to the helper's current conversation; a
        // cleared one keeps the last conversation it confirmed.
        assert_eq!(
            fixture.census_session(actor),
            Some(current.unwrap_or(followed)),
            "{reset}"
        );
    }
}
