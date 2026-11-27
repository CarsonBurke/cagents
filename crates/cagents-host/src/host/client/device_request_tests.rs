//! The early cleanup dispatch must retain an already authenticated caller's scope.
use super::{ClientRequest, ProtocolErrorCode, ServerResponse, SharedHost, process_request};
use crate::{
    agent_execution::Lineage,
    github_identity::{GitHubRepoKey, PullRequestKey},
    host::state::{HostLock, HostState},
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    worktree_cleanup::RemovalRequest,
};
use std::{path::Path, sync::Arc};

const USER: u64 = 1;
const AGENT: u64 = 2;

fn fixture() -> SharedHost {
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("device-request-fixture"),
        boot_id: BootId::new("device-request-boot"),
        host_incarnation: HostIncarnation::new("device-request-incarnation"),
    });
    state.register_client(USER);
    state.register_client(AGENT);
    // The state Authenticate leaves for an agent connection. It is not a
    // fake wire token, nor a claim that this test exercised a live provider's
    // authentication.
    state
        .agent_clients
        .insert(AGENT, Lineage(TerminalId::new()));
    Arc::new(HostLock::new(state))
}

fn invalid_removal(root: &Path) -> ClientRequest {
    let source = GitHubRepoKey::new("github.com", None, "fixture", "repository").unwrap();
    ClientRequest::DeviceOperation {
        operation: crate::device_service::Operation::RemoveMergedWorktree {
            request: RemovalRequest {
                repository: root.join("repository"),
                path: root.join("worktree"),
                branch: "feature".into(),
                // checked_record rejects this before filesystem/Git/GitHub work.
                // The old early dispatch swallowed that error into Ok(None).
                head_oid: "not-an-object-id".into(),
                source_repository: source.clone(),
                pull_request: PullRequestKey {
                    repository: source,
                    number: 1,
                },
            },
        },
    }
}

#[test]
fn authenticated_agent_cannot_prepare_merged_worktree_removal() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture();
    let error = process_request(&host, AGENT, invalid_removal(root.path()))
        .expect_err("agent admission must run before the early removal branch");
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    assert!(!root.path().join("repository").exists());
    assert!(!root.path().join("worktree").exists());
}

#[test]
fn user_invalid_merged_worktree_removal_keeps_its_inert_response() {
    let root = tempfile::tempdir().unwrap();
    let host = fixture();
    let (response, extras, shutdown) =
        process_request(&host, USER, invalid_removal(root.path())).unwrap();
    assert!(matches!(
        response,
        ServerResponse::DeviceOperation {
            reply: crate::device_service::Reply::MergedWorktreeRemoved(None)
        }
    ));
    assert!(extras.is_empty());
    assert!(!shutdown);
    assert!(!root.path().join("repository").exists());
    assert!(!root.path().join("worktree").exists());
}

#[test]
fn worktree_sessions_close_in_one_user_request_without_attaching() {
    use crate::device_service::{Operation, Reply, SessionCloseScope};

    let root = tempfile::tempdir().unwrap();
    let worktree = root.path().join("worktree");
    let other = root.path().join("other");
    std::fs::create_dir(&worktree).unwrap();
    std::fs::create_dir(&other).unwrap();
    let host = fixture();
    host.lock().unwrap().install_model(
        crate::host::model::Registry::at(&root.path().join("model.sqlite3")).unwrap(),
    );
    let first = spawn(&host, &worktree).id;
    let second = spawn(&host, &worktree).id;
    let kept = spawn(&host, &other).id;
    const UNATTACHED: u64 = 3;
    host.lock().unwrap().register_client(UNATTACHED);
    let request = ClientRequest::DeviceOperation {
        operation: Operation::CloseSessions(SessionCloseScope::Worktree {
            path: worktree.clone(),
        }),
    };
    let refused = process_request(&host, AGENT, request.clone()).unwrap_err();
    assert_eq!(refused.code, ProtocolErrorCode::PermissionDenied);
    let inventory = || {
        let state = host.lock().unwrap();
        [first, second, kept].map(|id| state.terminals.contains_key(&id))
    };
    assert_eq!(inventory(), [true, true, true]);
    for _ in 0..2 {
        let (response, _, _) = process_request(&host, UNATTACHED, request.clone()).unwrap();
        assert!(matches!(
            response,
            ServerResponse::DeviceOperation {
                reply: Reply::SessionsClosed(None),
            }
        ));
        assert_eq!(inventory(), [false, false, true]);
    }
    let worker = host.lock().unwrap().terminals[&kept].handle.clone();
    worker.shutdown().unwrap();
}

fn spawn(host: &SharedHost, cwd: &Path) -> crate::model::TerminalSession {
    use crate::model::{LaunchRecipe, TerminalContinuity, TerminalSession};
    let mut recipe = LaunchRecipe::shell(cwd.to_owned()).unwrap();
    recipe.program = "/bin/cat".into();
    let id = TerminalId::new();
    let session = TerminalSession {
        id,
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    process_request(
        host,
        USER,
        ClientRequest::Spawn {
            session: session.clone(),
            options: Default::default(),
        },
    )
    .unwrap();
    session
}

#[test]
fn project_close_honors_the_device_windows_saved_ownership() {
    use crate::{
        device_service::{Reply, SessionCloseScope},
        model::{
            CardId, CardKind, CardState, ChannelId, Existence, ProjectChannel, ProjectId,
            ProjectKind, ProjectState, WorkspaceState,
        },
        persistence::{SnapshotBase, StateStore},
        workspace_model::ModelEdit,
    };

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let other = root.path().join("other");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&other).unwrap();
    let host = fixture();
    host.lock().unwrap().install_model(
        crate::host::model::Registry::at(&root.path().join("model.sqlite3")).unwrap(),
    );
    process_request(
        &host,
        USER,
        ClientRequest::ApplyWorkspaceEdit {
            edit: ModelEdit::OpenProject {
                repository: project.clone(),
                name: "Project".into(),
                kind: ProjectKind::Folder,
            },
        },
    )
    .unwrap();

    let owned = spawn(&host, &project);
    let protected = spawn(&host, &project);
    let path = root.path().join("device-window.sqlite3");
    let store = StateStore::open(&path, BootId::new("device-request-boot")).unwrap();
    let foreign = ProjectId::new();
    store
        .save_project(&ProjectState {
            id: foreign,
            generation: 0,
            name: "Other".into(),
            repository: other,
            kind: ProjectKind::Folder,
            existence: Existence::Open,
        })
        .unwrap();
    let card = CardId::new();
    let workspace = WorkspaceState {
        cards: vec![CardState {
            id: card,
            generation: 0,
            title: "Filed under Other".into(),
            width_fraction: 1.0,
            existence: Existence::Open,
            kind: CardKind::Terminal(protected.clone()),
        }]
        .into(),
        channels: vec![ProjectChannel {
            title_source: Default::default(),
            id: ChannelId::new(),
            project_id: foreign,
            title: "Other project".into(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
            checkout: None,
        }],
        ..Default::default()
    };
    store
        .save_workspace(
            &workspace,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    let close = |only_if_closed| {
        super::project_sessions::close(
            &host,
            USER,
            SessionCloseScope::Project {
                repository: project.clone(),
                kind: ProjectKind::Folder,
                roots: vec![project.clone()],
                ownership: Default::default(),
                only_if_closed,
            },
            || Ok(vec![path.clone()]),
        )
    };
    assert!(close(true).unwrap_err().message.contains("reopened"));
    assert!(host.lock().unwrap().terminals.contains_key(&owned.id));

    let Reply::SessionsClosed(Some(closure)) = close(false).unwrap() else {
        panic!("expected the merged ownership to be returned to the window");
    };
    assert!(closure.ownership.protected.contains(&protected.id));
    let worker = {
        let state = host.lock().unwrap();
        assert!(!state.terminals.contains_key(&owned.id));
        state.terminals[&protected.id].handle.clone()
    };
    worker.shutdown().unwrap();
}
