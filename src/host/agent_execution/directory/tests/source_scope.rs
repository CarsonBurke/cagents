//! Real Registry admission and directory reads against a current Host record.
//! The resident History handle is process-free and stays active until close;
//! this tests source attribution, not native process or holder authentication.
use super::*;
use crate::{
    agent_execution::{DirectoryIdentity, HostedSession},
    chat::{
        self,
        protocol::{Adapter, ProcessSpec, Step, TransportSpec},
        runtime::{self, Handle},
    },
    host::{
        chat::Record,
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{
        CardId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        ProjectKind,
    },
    workspace_model::{ModelEdit, views::Pane},
};
use std::sync::Arc;

struct MetadataOnly;
impl Adapter for MetadataOnly {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, _: chat::Command) -> Result<Step, String> {
        Err("The directory fixture cannot execute provider commands".into())
    }
    fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
        Err("The directory fixture has no provider transport".into())
    }
}

struct Fixture {
    host: SharedHost,
    handle: Handle,
    source: chat::Session,
    physical: PathBuf,
    logical: PathBuf,
    _directory: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.close().unwrap();
    }
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let logical = directory.path().join("logical");
        for path in [&physical, &logical] {
            std::fs::create_dir(path).unwrap();
        }
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("directory-source-fixture"),
            boot_id: crate::model::BootId::new("fixture-boot"),
            host_incarnation: HostIncarnation::new("current"),
        };
        let source = chat::Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Omp,
                recipe: LaunchRecipe {
                    program: "process-free-directory-fixture".into(),
                    arguments: Vec::new(),
                    working_directory: physical.clone(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
                session_id: Some("fixture-current-thread".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        };
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            transport: TransportSpec::History,
            program: source.launch.recipe.program.clone(),
            arguments: Vec::new(),
            working_directory: physical.clone(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let mut initial = Step::default();
        let store = runtime::preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(
            spec,
            Box::new(MetadataOnly),
            initial,
            store,
            source.launch.session_id.clone(),
            |_| {},
        )
        .unwrap();
        assert!(!handle.is_completed());
        let mut registry = Registry::at(&directory.path().join("registry.sqlite")).unwrap();
        for (repository, name) in [(&physical, "Physical"), (&logical, "Logical")] {
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: repository.clone(),
                    name: name.into(),
                    kind: ProjectKind::Folder,
                })
                .unwrap();
        }
        let mut state = HostState::new(proof);
        state.install_model(registry);
        state.chats.insert(
            source.id,
            Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::new(),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        Self {
            host: Arc::new(HostLock::new(state)),
            handle,
            source,
            physical,
            logical,
            _directory: directory,
        }
    }

    fn project(&self, path: &Path) -> Directory {
        assert!(!self.handle.is_completed());
        let result = read(
            &self.host,
            &Authority::default(),
            Scope::Project(path.into()),
        )
        .unwrap();
        assert!(!self.handle.is_completed());
        result
    }

    fn seed(&self, cached: chat::Session) {
        let registry = registry(&self.host).unwrap();
        let mut registry = registry.lock().unwrap();
        let applied = registry
            .apply(&ModelEdit::SeedView {
                scope: crate::workspace_model::views::Scope::Worktree {
                    repository: self.logical.clone(),
                    path: self.logical.clone(),
                },
                panes: vec![Pane {
                    id: CardId::new(),
                    title: "Saved pane".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.0,
                    kind: CardKind::Chat(cached.clone()),
                    scope: Some(self.logical.clone()),
                }],
                closed: Vec::new(),
            })
            .unwrap();
        assert!(applied.changed);
        assert_eq!(applied.model.views[0].panes[0].kind, CardKind::Chat(cached));
    }
}

#[test]
fn current_directory_reads_reject_unproved_same_id_panes_but_keep_accepted_aliases() {
    for case in [
        "current", "adopted", "proof", "recipe", "thread", "unproved",
    ] {
        let fixture = Fixture::new();
        let initial = fixture.project(&fixture.physical);
        assert_eq!(
            initial.sessions.len(),
            1,
            "fixture must expose the current record"
        );
        assert_eq!(initial.sessions[0].id, fixture.source.id);
        assert_eq!(
            initial.sessions[0].project.as_ref(),
            Some(&fixture.physical)
        );
        let mut cached = fixture.source.clone();
        match case {
            "current" => {}
            "adopted" => {
                cached.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("previous");
                let registry = registry(&fixture.host).unwrap();
                let mut registry = registry.lock().unwrap();
                let retained = HostedSession::Chat(cached.clone());
                let current = HostedSession::Chat(fixture.source.clone());
                let identity = DirectoryIdentity::from_metadata(
                    &std::fs::metadata(&fixture.physical).unwrap(),
                );
                // The private registry adoption API records the accepted
                // predecessor used by Locate; no capability is fabricated.
                registry
                    .bind_session_location(&retained, None, &fixture.physical, identity)
                    .unwrap();
                registry
                    .adopt_session_location(&[retained.clone()], &current)
                    .unwrap();
                assert!(
                    registry
                        .authenticated_session_alias(&retained, &current)
                        .unwrap()
                );
            }
            "proof" => {
                cached.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("unrelated")
            }
            "recipe" => cached
                .launch
                .recipe
                .arguments
                .push("different-issued-launch".into()),
            "thread" => cached.launch.session_id = Some("unrelated-confirmed-thread".into()),
            "unproved" => cached.proof = None,
            _ => unreachable!(),
        }
        fixture.seed(cached);
        let physical = fixture.project(&fixture.physical);
        let logical = fixture.project(&fixture.logical);
        let accepted = matches!(case, "current" | "adopted");
        let (visible, absent, project) = if accepted {
            (&logical, &physical, &fixture.logical)
        } else {
            (&physical, &logical, &fixture.physical)
        };
        assert_eq!(
            visible.sessions.len(),
            1,
            "{case}: current record must remain discoverable"
        );
        assert_eq!(visible.sessions[0].id, fixture.source.id);
        assert_eq!(
            visible.sessions[0].project.as_ref(),
            Some(project),
            "{case}"
        );
        assert_eq!(
            visible.sessions[0].directory.as_ref(),
            Some(&fixture.physical)
        );
        assert!(
            absent.sessions.is_empty(),
            "{case}: stale scope must not redirect current membership"
        );
        assert_eq!(
            visible.sessions[0].view_scope.is_some(),
            accepted,
            "{case}: unproved pane cannot supply implicit placement"
        );
    }
}

#[test]
fn terminal_directory_source_projects_current_proof_before_binding_saved_scope() {
    use crate::model::{TerminalContinuity, TerminalSession};
    use crate::workspace_model::views::{Change, Scope as ViewScope};

    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("terminal-scope.sqlite")).unwrap();
    let recipe = LaunchRecipe {
        program: "no-process-directory-fixture".into(),
        arguments: Vec::new(),
        working_directory: "/issued-directory".into(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let raw = TerminalSession {
        id: TerminalId::new(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: recipe.clone(),
        transcript_path: Some("/not-opened-by-directory-source".into()),
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("terminal-directory-fixture"),
        boot_id: crate::model::BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("current"),
    };
    // This is the pure projection used after live_sessions' completed-worker
    // guard. No PTY, terminal worker, provider or transcript file is required.
    let projected = terminal_source(&raw, &proof);
    let HostedSession::Terminal(current) = &projected else {
        unreachable!()
    };
    assert_eq!(current.continuity.live_proof(), Some(proof));
    assert_eq!(current.launch, raw.launch);
    assert_eq!(current.transcript_path, raw.transcript_path);
    assert!(raw.continuity.live_proof().is_none());
    let scope = ViewScope::Worktree {
        repository: "/logical-project".into(),
        path: "/logical-project".into(),
    };
    registry
        .apply(&ModelEdit::OpenProject {
            repository: "/logical-project".into(),
            name: "Logical".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let mut pane = Pane {
        id: CardId::new(),
        title: "Terminal scope fixture".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: CardKind::Terminal(current.clone()),
        scope: None,
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: Vec::new(),
        })
        .unwrap();
    for case in ["current", "old-daemon", "raw-launch", "different-recipe"] {
        let before = pane.clone();
        let mut cached = current.clone();
        match case {
            "current" => {}
            "old-daemon" => {
                cached.continuity = TerminalContinuity::DetachedSameBoot {
                    host_session_id: HostSessionId::new("terminal-directory-fixture"),
                    boot_id: crate::model::BootId::new("fixture-boot"),
                    host_incarnation: HostIncarnation::new("previous"),
                };
            }
            "raw-launch" => cached = raw.clone(),
            "different-recipe" => cached.launch.arguments.push("different-launch".into()),
            _ => unreachable!(),
        }
        pane.kind = CardKind::Terminal(cached);
        let model = registry.model().unwrap();
        registry
            .apply(&ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: model.views[0].revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before,
                    pane: pane.clone(),
                },
            })
            .unwrap();
        let scopes = session_scopes(&registry.model().unwrap(), |cached| {
            registry.authenticated_session_alias(cached, &projected)
        })
        .unwrap();
        assert_eq!(
            scopes.get(&raw.id).cloned(),
            (case == "current").then_some(Some(scope.clone())),
            "{case}: only current proved terminal metadata can supply placement"
        );
    }
}

#[test]
fn current_directory_reads_ignore_stale_agent_delegation_without_a_shared_pane() {
    use crate::agent_execution::{Destination, Spec, Submit, Surface as AgentSurface, Target};

    for matching in [true, false] {
        let fixture = Fixture::new();
        let initial = fixture.project(&fixture.physical);
        assert_eq!(initial.sessions.len(), 1);
        assert_eq!(initial.sessions[0].id, fixture.source.id);
        let mut cached = fixture.source.clone();
        if !matching {
            cached.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("previous");
        }
        let placement = crate::agent_execution::Placement::Worktree {
            repository: fixture.logical.clone(),
            path: fixture.logical.clone(),
        };
        let (agent, run) = {
            let registry = registry(&fixture.host).unwrap();
            let mut registry = registry.lock().unwrap();
            // Acceptance and descriptor binding are database-only. No dispatch,
            // provider, capability or shared pane is needed to expose fallback.
            let accepted = registry
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Omp,
                            account: None,
                            target: Target {
                                repository: fixture.logical.clone(),
                                directory: fixture.logical.clone(),
                                placement: Some(placement.clone()),
                            },
                            surface: AgentSurface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "Directory metadata fixture; never dispatched".into(),
                        idempotency_key: "directory-delegation-fixture".into(),
                    },
                    1,
                )
                .unwrap();
            let bound = registry
                .bind_agent(accepted.agent.id, None, HostedSession::Chat(cached.clone()))
                .unwrap();
            assert_eq!(bound.session, Some(HostedSession::Chat(cached)));
            assert!(registry.model().unwrap().views.is_empty());
            (accepted.agent.id, accepted.run.id)
        };
        let physical = fixture.project(&fixture.physical);
        let logical = fixture.project(&fixture.logical);
        if matching {
            assert_eq!(logical.sessions.len(), 1);
            let current = &logical.sessions[0];
            assert_eq!(current.id, fixture.source.id);
            assert_eq!(current.project.as_ref(), Some(&fixture.logical));
            assert_eq!(current.view_scope.as_ref(), Some(&placement));
            assert_eq!(current.relation, Relation::Child);
            let delegation = current.delegation.as_ref().unwrap();
            assert_eq!(delegation.agent, agent);
            assert_eq!(delegation.run, Some(run));
            assert_eq!(delegation.owner, Owner::User);
        } else {
            assert!(
                logical.sessions.is_empty(),
                "stale agent must not supply a logical scope"
            );
            assert_eq!(physical.sessions.len(), 1);
            let current = &physical.sessions[0];
            assert_eq!(current.id, fixture.source.id);
            assert_eq!(current.project.as_ref(), Some(&fixture.physical));
            assert!(current.view_scope.is_none());
            assert!(current.delegation.is_none());
            assert_eq!(current.relation, Relation::Peer);
        }
        assert!(
            physical
                .sessions
                .iter()
                .chain(&logical.sessions)
                .all(|entry| { entry.directory.as_ref() == Some(&fixture.physical) })
        );
    }
}
