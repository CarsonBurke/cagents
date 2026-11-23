use super::*;
use crate::model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe};
use std::os::unix::fs::DirBuilderExt;

#[test]
fn failed_upgrade_stop_requires_its_listener_to_be_unserved() {
    let root = tempfile::tempdir().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("failed-upgrade-stop"),
        host_incarnation: HostIncarnation::new("failed-upgrade-stop"),
        boot_id: BootId::new("failed-upgrade-stop"),
    };
    let mut session = Session {
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: "codex".into(),
                arguments: vec!["--remote".into(), "unix://".into()],
                working_directory: root.path().into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
            draft_model: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    assert!(session.claim_owned_codex_listener(root.path(), &proof));
    let socket = &session.owned_codex_listener.as_ref().unwrap().socket;
    let allocation = socket.parent().unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(allocation)
        .unwrap();
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();

    assert_eq!(
        return_listener_allocation_checked(&session, true).unwrap_err(),
        "The failed restart still serves its local listener"
    );
    assert!(allocation.exists());
    // Existing retirement callers retain served allocations without failing.
    return_listener_allocation(&session).unwrap();
    assert!(allocation.exists());

    drop(listener);
    return_listener_allocation_checked(&session, true).unwrap();
    assert!(!allocation.exists());
}
