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
use std::sync::{Arc, Mutex};

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
