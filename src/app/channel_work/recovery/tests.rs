use super::*;
use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};
use std::cell::Cell;

fn remote() -> Profile {
    Profile {
        name: "fixture-device".into(),
        host: "fixture-user@fixture-host".into(),
        executable: "/fixture/bin/cagents".into(),
    }
}

fn saved(profile: Option<Profile>) -> Destination {
    Destination {
        profile,
        socket: "/fixture/retired-process-proxy/daemon.sock".into(),
        proof: HostSessionProof {
            host_session_id: HostSessionId::new("fixture-original-host"),
            boot_id: BootId::new("fixture-original-boot"),
            host_incarnation: HostIncarnation::new("fixture-original-incarnation"),
        },
    }
}

#[test]
fn changed_saved_profile_cannot_resolve_a_transport_or_fall_back_to_local() {
    let remote = remote();
    let mut renamed = remote.clone();
    renamed.name = "other-name".into();
    let mut retargeted = remote.clone();
    retargeted.host = "fixture-user@other-host".into();
    let mut executable = remote.clone();
    executable.executable = "/other/bin/cagents".into();
    let destination = saved(Some(remote.clone()));
    for current in [None, Some(renamed), Some(retargeted), Some(executable)] {
        let resolves = Cell::new(0);
        assert_eq!(
            require_profile(&destination, current.as_ref()),
            Err(PROFILE_CHANGED)
        );
        assert_eq!(
            explicit_check_socket_with(&destination, current.as_ref(), || {
                resolves.set(resolves.get() + 1);
                Ok("/fixture/wrong-route/daemon.sock".into())
            }),
            Err(PROFILE_CHANGED)
        );
        assert_eq!(resolves.get(), 0);
    }
    let local = saved(None);
    assert_eq!(
        explicit_check_socket_with(&local, Some(&remote), || {
            panic!("a local saved operation must not initialize a remote bridge")
        }),
        Err(PROFILE_CHANGED)
    );
}

#[test]
fn explicit_check_can_resolve_a_fresh_same_profile_socket_without_rewriting_original_proof() {
    for profile in [None, Some(remote())] {
        let destination = saved(profile.clone());
        let original = destination.clone();
        let resolves = Cell::new(0);
        // Restoration and Retry admission only compare the captured profile;
        // they do not resolve even the original socket.
        assert_eq!(require_profile(&destination, profile.as_ref()), Ok(()));
        assert_eq!(resolves.get(), 0);
        let fresh = PathBuf::from("/fixture/new-process-proxy/daemon.sock");
        let candidate = explicit_check_socket_with(&destination, profile.as_ref(), || {
            resolves.set(resolves.get() + 1);
            Ok(fresh.clone())
        })
        .unwrap();
        assert_eq!(candidate, fresh);
        assert_ne!(candidate, original.socket);
        assert_eq!(resolves.get(), 1);
        assert_eq!(destination, original);
        // Candidate selection is not a handshake or Registry/Home check. Root
        // integration owns those reads before any saved mutation can be sent.
    }
}

#[test]
fn failed_explicit_reconnect_preserves_saved_destination_and_exposes_only_fixed_error() {
    let profile = remote();
    let destination = saved(Some(profile.clone()));
    let original = destination.clone();
    let resolves = Cell::new(0);
    let result = explicit_check_socket_with(&destination, Some(&profile), || {
        resolves.set(resolves.get() + 1);
        Err(io::Error::other("private fixture transport diagnostic"))
    });
    assert_eq!(result, Err(SOCKET_UNAVAILABLE));
    assert_eq!(resolves.get(), 1, "no retry or saved-socket fallback");
    assert_eq!(destination, original);
    assert!(
        !result
            .unwrap_err()
            .contains("private fixture transport diagnostic")
    );
}
