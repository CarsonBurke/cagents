use super::*;
use crate::model::TerminalId;

fn key(byte: u8) -> LinkKey {
    LinkKey::from_bytes([byte; SECRET_BYTES])
}

/// A session that asks before edits, as a Claude session in its default mode.
fn claim(lineage: Lineage) -> Claim {
    Claim {
        lineage,
        ceiling: crate::agent_execution::permission::StatedCeiling {
            provider: Some(crate::agent::Provider::Claude),
            mode: Some("default".into()),
        },
    }
}

fn store() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data").join("device-trust.json");
    (directory, path)
}

fn register(path: &Path, peer: uuid::Uuid, key: LinkKey) {
    assert_eq!(
        process_at(
            path,
            Request::RegisterPeer {
                peer,
                label: "laptop".into(),
                key,
            },
        )
        .unwrap(),
        Reply::Registered
    );
}

fn save_link(path: &Path, device: &str, peer: uuid::Uuid, key: LinkKey, allowed: bool) -> Reply {
    process_at(
        path,
        Request::SaveLink {
            device: device.into(),
            peer,
            key,
            agents_allowed: allowed,
        },
    )
    .unwrap()
}

#[test]
fn links_persist_privately_without_listing_their_keys() {
    use std::os::unix::fs::PermissionsExt;
    let (_directory, path) = store();
    let peer = uuid::Uuid::new_v4();
    assert_eq!(
        save_link(&path, "fw16", peer, key(4), false),
        Reply::Saved { replaced: None }
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let Reply::Links(links) = process_at(&path, Request::Links).unwrap() else {
        panic!("expected links");
    };
    assert_eq!(links.len(), 1);
    assert_eq!((links[0].device.as_str(), links[0].peer), ("fw16", peer));
    assert!(!links[0].agents_allowed);

    process_at(
        &path,
        Request::SetAgentsAllowed {
            device: "fw16".into(),
            allowed: true,
        },
    )
    .unwrap();
    let Reply::Links(links) = process_at(&path, Request::Links).unwrap() else {
        panic!("expected links");
    };
    assert!(links[0].agents_allowed);
}

#[test]
fn relinking_replaces_the_key_and_keeps_the_peer() {
    let (_directory, path) = store();
    let peer = uuid::Uuid::new_v4();
    save_link(&path, "fw16", peer, key(1), true);
    assert_eq!(
        save_link(&path, "fw16", peer, key(2), true),
        Reply::Saved {
            replaced: Some(peer)
        }
    );
    assert_eq!(
        Store::load(&path).unwrap().link("fw16").unwrap().key.0,
        key(2)
    );

    register(&path, peer, key(1));
    register(&path, peer, key(2));
    let device = Store::load(&path).unwrap();
    assert_eq!(device.peers.len(), 1);
    assert_eq!(device.peer(peer).unwrap().key.0, key(2));
    assert_eq!(
        process_at(&path, Request::ForgetPeer { peer }).unwrap(),
        Reply::Forgotten { existed: true }
    );
}

#[test]
fn granting_an_unlinked_device_is_refused_with_how_to_link_it() {
    let (_directory, path) = store();
    let error = process_at(
        &path,
        Request::SetAgentsAllowed {
            device: "fw16".into(),
            allowed: true,
        },
    )
    .unwrap_err();
    assert!(
        error.message.contains("cagents device link fw16"),
        "{error:?}"
    );
    assert!(!path.exists());
}

#[test]
fn a_link_needs_a_valid_device_name() {
    let (_directory, path) = store();
    let error = process_at(
        &path,
        Request::SaveLink {
            device: "../fw16".into(),
            peer: uuid::Uuid::new_v4(),
            key: key(1),
            agents_allowed: true,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
}

#[test]
fn a_peer_label_drops_control_characters() {
    let mut store = Store::default();
    store
        .apply(Request::RegisterPeer {
            peer: uuid::Uuid::new_v4(),
            label: format!("lap\u{1b}[31mtop{}", "x".repeat(200)),
            key: key(1),
        })
        .unwrap();
    assert!(store.peers[0].label.starts_with("lap[31mtop"));
    assert_eq!(store.peers[0].label.chars().count(), MAX_LABEL_CHARS);
}

#[test]
fn a_store_key_round_trips_as_hex_and_rejects_other_text() {
    let (_directory, path) = store();
    let peer = uuid::Uuid::new_v4();
    save_link(&path, "fw16", peer, key(0xab), true);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&"ab".repeat(SECRET_BYTES)));
    let loaded = Store::load(&path).unwrap();
    assert_eq!(loaded.link("fw16").unwrap().key.0, key(0xab));

    std::fs::write(&path, text.replace(&"ab".repeat(SECRET_BYTES), "zz")).unwrap();
    assert!(Store::load(&path).is_err());
}

#[test]
fn a_proof_answers_one_challenge_and_its_capability_ends_with_the_connection() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));
    let client: ClientId = 7;

    let nonce = challenge(&host, client).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &claim(session));
    let authority = authenticate_at(&path, &host, client, peer, &claim(session), &proof).unwrap();
    assert!(is_peer_capability(&authority));
    let expected = crate::device_trust::peer_lineage(peer, session);
    assert_eq!(
        peer_origin(&host, &authority).unwrap().unwrap(),
        Origin::Child { parent: expected }
    );

    assert_eq!(
        host.lock().unwrap().agent_clients.get(&client),
        Some(&expected)
    );
    assert_eq!(
        peer_ceiling(&host, &authority).unwrap().unwrap(),
        Ceiling::from_stated(&claim(session).ceiling).unwrap()
    );
    assert_eq!(
        host.lock().unwrap().agent_client_authorities.get(&client),
        Some(&authority)
    );

    // The nonce was spent: the same proof cannot be presented again.
    let replay = authenticate_at(&path, &host, client, peer, &claim(session), &proof).unwrap_err();
    assert_eq!(replay.code, ProtocolErrorCode::PermissionDenied);

    // It has no session here, so it moves none of the device's panes.
    let Err(error) = super::super::agent_execution::terminals::mover(&host, client) else {
        panic!("a peer connection must not move panes");
    };
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);

    // Another connection cannot act with it.
    let elsewhere =
        super::super::agent_execution::terminals::authenticate(&host, client + 1, &authority)
            .unwrap_err();
    assert_eq!(elsewhere.code, ProtocolErrorCode::PermissionDenied);
    assert!(
        !host
            .lock()
            .unwrap()
            .agent_clients
            .contains_key(&(client + 1))
    );
    super::super::agent_execution::terminals::authenticate(&host, client, &authority).unwrap();

    // Proving the connection again replaces its capability.
    let nonce = challenge(&host, client).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &claim(session));
    let again = authenticate_at(&path, &host, client, peer, &claim(session), &proof).unwrap();
    assert!(peer_origin(&host, &authority).unwrap().is_err());
    assert!(peer_origin(&host, &again).unwrap().is_ok());
    assert_eq!(host.lock().unwrap().peers.capabilities.len(), 1);

    host.lock().unwrap().peers.disconnect(client);
    assert!(peer_origin(&host, &again).unwrap().is_err());
    assert!(peer_ceiling(&host, &again).unwrap().is_err());
}

#[test]
fn a_proof_whose_ceiling_was_raised_or_cannot_be_read_is_refused() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));

    // Signed as asking, presented as bypassing permissions.
    let nonce = challenge(&host, 1).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &claim(session));
    let mut raised = claim(session);
    raised.ceiling.mode = Some("bypassPermissions".into());
    let error = authenticate_at(&path, &host, 1, peer, &raised, &proof).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);

    // Signed, but naming a mode this daemon cannot rank.
    let mut unreadable = claim(session);
    unreadable.ceiling.mode = Some("yolo".into());
    let nonce = challenge(&host, 1).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &unreadable);
    let error = authenticate_at(&path, &host, 1, peer, &unreadable, &proof).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    assert!(host.lock().unwrap().peers.capabilities.is_empty());
}

#[test]
fn a_proof_for_another_key_lineage_or_connection_is_refused() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));

    let nonce = challenge(&host, 1).unwrap();
    let wrong_key = crate::device_trust::proof(&key(6), &nonce, peer, &claim(session));
    assert!(authenticate_at(&path, &host, 1, peer, &claim(session), &wrong_key).is_err());

    let nonce = challenge(&host, 1).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &claim(session));
    let other = Lineage(TerminalId::new());
    assert!(authenticate_at(&path, &host, 1, peer, &claim(other), &proof).is_err());

    // Another connection's challenge does not stand in for this one's.
    let nonce = challenge(&host, 1).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &claim(session));
    assert!(authenticate_at(&path, &host, 2, peer, &claim(session), &proof).is_err());

    let unknown = uuid::Uuid::new_v4();
    let nonce = challenge(&host, 3).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, unknown, &claim(session));
    let error = authenticate_at(&path, &host, 3, unknown, &claim(session), &proof).unwrap_err();
    assert!(error.message.contains("not linked"), "{error:?}");
}

#[test]
fn only_a_session_of_this_computer_has_a_challenge_signed() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    save_link(&path, "fw16", uuid::Uuid::new_v4(), key(1), true);
    let nonce = [9; SECRET_BYTES];

    let person = sign_at(&path, &host, &Authority::default(), "fw16", &nonce).unwrap_err();
    assert_eq!(person.code, ProtocolErrorCode::PermissionDenied);

    let peer = Authority::session(format!(
        "{CAPABILITY_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    ));
    let forwarded = sign_at(&path, &host, &peer, "fw16", &nonce).unwrap_err();
    assert!(
        forwarded.message.contains("another computer"),
        "{forwarded:?}"
    );

    let unknown = Authority::session(uuid::Uuid::new_v4().to_string());
    let error = sign_at(&path, &host, &unknown, "fw16", &nonce).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
}

#[test]
fn a_connection_acting_for_another_session_cannot_prove_a_peer() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));
    let local = Lineage(TerminalId::new());
    host.lock().unwrap().agent_clients.insert(4, local);

    let nonce = challenge(&host, 4).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, &claim(session));
    let error = authenticate_at(&path, &host, 4, peer, &claim(session), &proof).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    let state = host.lock().unwrap();
    assert_eq!(state.agent_clients.get(&4), Some(&local));
    assert!(state.peers.capabilities.is_empty());
}
