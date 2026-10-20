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
        require_execution_owner(&self.registry, Some(owner), true)?;
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
        require_execution_owner(&fixture.registry, None, false).unwrap();
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
        (
            1,
            Update::Execution {
                run: run.id,
                update: crate::agent_execution::Update::Started {
                    provider_key: "exact-fixture-provider".into(),
                },
            },
        ),
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
