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
    // Reuse the authenticated-connection fixture from the channel-work tests.
    // This supplies the state produced by Authenticate, not a fake wire token
    // or a claim that this unit test exercised a live provider's authentication.
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
