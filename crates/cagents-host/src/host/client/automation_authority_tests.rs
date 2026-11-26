//! Real framed dispatcher coverage: authority refusals retain their protocol
//! classification while ordinary invalid requests remain recoverable errors.
use super::*;
use crate::{
    agent_execution::{Authority, HostedSession},
    automations::{Request, Response},
    control::{ControlClient, ControlError},
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{BootId, HostIncarnation, HostSessionId, TerminalId},
};

#[test]
fn framed_automation_authority_denials_are_permission_denied_and_business_errors_stay_invalid() {
    let directory = tempfile::Builder::new()
        .prefix("automation-authority-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("automation-authority-classification"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
    };
    let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
    // An actually registered but nonresident session exercises the live-source
    // authorization check. This descriptor never launches a provider/process.
    let stale = registry
        .register_agent_authority(&HostedSession::Chat(crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "/nonexistent-offline-fixture".into(),
                    arguments: Vec::new(),
                    working_directory: directory.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                draft_model: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        }))
        .unwrap();
    let mut state = HostState::new(proof);
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let (stream, peer) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || serve(peer, 1, host));
    let mut client = ControlClient::from_stream(stream).unwrap();
    let id = uuid::Uuid::new_v4();
    for authority in [
        Authority::session(String::new()),
        Authority::session("invalid-synthetic-authority".into()),
        Authority::session(uuid::Uuid::new_v4().to_string()),
        stale,
    ] {
        for operation in [
            Request::List {
                after: None,
                limit: 1,
            },
            Request::PendingProposals {
                repository: directory.path().into(),
                after: None,
                limit: 1,
            },
            Request::Proposal { id },
            Request::ResolveProposal {
                id,
                revision: 1,
                approved: false,
            },
            Request::Audit {
                id,
                before: None,
                limit: 1,
            },
        ] {
            let error = client.automation(authority.clone(), operation).unwrap_err();
            assert!(
                matches!(
                    error,
                    ControlError::Remote {
                        code: ProtocolErrorCode::PermissionDenied,
                        retryable: false,
                        ..
                    }
                ),
                "{error}"
            );
        }
    }
    // Classification is not guessed from caller presence or an arbitrary
    // string error. User's missing objects/cursor errors remain InvalidRequest.
    for operation in [
        Request::Proposal { id },
        Request::ResolveProposal {
            id,
            revision: 1,
            approved: false,
        },
        Request::Show { id },
        Request::Audit {
            id,
            before: Some(u64::MAX),
            limit: 1,
        },
    ] {
        let error = client
            .automation(Authority::default(), operation)
            .unwrap_err();
        assert!(
            matches!(
                error,
                ControlError::Remote {
                    code: ProtocolErrorCode::InvalidRequest,
                    retryable: false,
                    ..
                }
            ),
            "{error}"
        );
    }
    // Damage only this fixture's authority storage through a second connection.
    // A valid token shape must reach SQLite, so this distinguishes a registry
    // failure from malformed/revoked credentials at the real dispatch boundary.
    let fault = rusqlite::Connection::open(directory.path().join("registry.sqlite3")).unwrap();
    fault.busy_timeout(Duration::from_secs(3)).unwrap();
    fault
        .execute_batch("ALTER TABLE agent_authorities RENAME TO fixture_unavailable_authorities")
        .unwrap();
    drop(fault);
    let error = client
        .automation(
            Authority::session(uuid::Uuid::new_v4().to_string()),
            Request::List {
                after: None,
                limit: 1,
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            ControlError::Remote {
                code: ProtocolErrorCode::InvalidRequest,
                retryable: false,
                ..
            }
        ),
        "{error}"
    );
    // The errors keep the connection usable and do not poison User's following
    // ordinary read, which does not require session authority storage.
    assert!(
        matches!(client.automation(Authority::default(), Request::List { after: None, limit: 1 }).unwrap(), Response::Listed(rows) if rows.is_empty())
    );
    drop(client);
    server.join().unwrap();
}
