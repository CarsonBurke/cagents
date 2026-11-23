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

/// A Claude session's claim in `mode`.
fn claim_in(lineage: Lineage, mode: &str) -> Claim {
    let mut claim = claim(lineage);
    claim.ceiling.mode = Some(mode.into());
    claim
}

/// Proves `client` for `session` of `peer`, whose key is `key`.
fn prove(
    path: &Path,
    host: &SharedHost,
    client: ClientId,
    peer: uuid::Uuid,
    key: &LinkKey,
    claim: &Claim,
) -> Result<Authority, ProtocolError> {
    let nonce = challenge(host, client).unwrap();
    let proof = crate::device_trust::proof(key, &nonce, peer, claim);
    authenticate_at(path, host, client, peer, claim, &proof)
}

/// Waits, briefly, for the daemon to close `delivery`'s connection.
fn closes(delivery: &crate::host::delivery::DeliveryQueue) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !delivery.is_closed() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    true
}

fn refused(host: &SharedHost, client: ClientId, request: crate::terminal::ipc::ClientRequest) {
    let Err(error) = super::super::agent_execution::terminals::admit(host, client, &request) else {
        panic!("an ended device connection must be refused {request:?}");
    };
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
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

    // Nor can another connection present it in a request without
    // authenticating, as if any connection could hold it.
    let foreign = [
        crate::terminal::ipc::ClientRequest::Agent {
            authority: authority.clone(),
            operation: crate::agent_execution::Operation::List,
        },
        crate::terminal::ipc::ClientRequest::Automation {
            authority: authority.clone(),
            operation: crate::automations::Request::List {
                after: None,
                limit: 1,
            },
        },
    ];
    for request in &foreign {
        let Err(error) =
            super::super::agent_execution::terminals::admit(&host, client + 1, request)
        else {
            panic!("an unbound connection must not act with another's capability");
        };
        assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    }
    // The connection it was issued to still may.
    let own = crate::terminal::ipc::ClientRequest::Agent {
        authority: authority.clone(),
        operation: crate::agent_execution::Operation::List,
    };
    super::super::agent_execution::terminals::admit(&host, client, &own).unwrap();

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
    assert_eq!(host.lock().unwrap().peers.connections.len(), 1);

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
    assert!(host.lock().unwrap().peers.connections.is_empty());
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
    assert!(state.peers.connections.is_empty());
}

#[test]
fn an_open_connection_proves_itself_again_or_loses_its_capability_and_closes() {
    use crate::terminal::ipc::ClientRequest;
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));
    let client: ClientId = 7;
    let delivery = host.lock().unwrap().register_client(client);

    let first = prove(
        &path,
        &host,
        client,
        peer,
        &key(5),
        &claim_in(session, "acceptEdits"),
    )
    .unwrap();
    // A person lowered the session's mode on the linked computer, which then
    // vouches for the lower ceiling: proving the open connection again holds
    // it to that ceiling, and the capability it held before is dead.
    let lowered = prove(&path, &host, client, peer, &key(5), &claim(session)).unwrap();
    assert_eq!(
        peer_ceiling(&host, &lowered).unwrap().unwrap(),
        Ceiling::from_stated(&claim(session).ceiling).unwrap()
    );
    assert!(peer_ceiling(&host, &first).unwrap().is_err());
    assert!(peer_origin(&host, &first).unwrap().is_err());
    super::super::agent_execution::terminals::admit(
        &host,
        client,
        &ClientRequest::Agent {
            authority: lowered.clone(),
            operation: crate::agent_execution::Operation::List,
        },
    )
    .unwrap();
    assert!(!delivery.is_closed());

    // Unproved for its lifetime, as when the linked computer stops vouching
    // for the session, the capability lapses: the connection may do nothing,
    // not even prove itself again, and the daemon closes it.
    host.lock().unwrap().peers.lapse_now(client);
    let error = peer_origin(&host, &lowered).unwrap().unwrap_err();
    assert!(error.contains("capability has ended"), "{error}");
    assert!(peer_ceiling(&host, &lowered).unwrap().is_err());
    for request in [
        ClientRequest::Agent {
            authority: lowered.clone(),
            operation: crate::agent_execution::Operation::List,
        },
        ClientRequest::List,
        ClientRequest::ReadWorkspaceModel,
        ClientRequest::PeerChallenge,
    ] {
        refused(&host, client, request);
    }
    assert!(
        super::super::agent_execution::terminals::authenticate(&host, client, &lowered).is_err()
    );
    // Still bound to the session until it closes, never a person's connection.
    assert!(host.lock().unwrap().agent_clients.contains_key(&client));
    assert!(closes(&delivery), "the lapsed connection must be closed");

    // With nothing live, the watcher stops; the next proof starts another.
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while host.lock().unwrap().peers.reaper.is_some() {
        assert!(Instant::now() < deadline, "the watcher must stop when idle");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    host.lock().unwrap().peers.disconnect(client);
    let other = host.lock().unwrap().register_client(8);
    prove(&path, &host, 8, peer, &key(5), &claim(session)).unwrap();
    assert!(host.lock().unwrap().peers.reaper.is_some());
    host.lock().unwrap().peers.lapse_now(8);
    assert!(closes(&other));
}

#[test]
fn a_capability_lasts_its_lifetime_from_the_latest_proof() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));
    let before = Instant::now();
    prove(&path, &host, 1, peer, &key(5), &claim(session)).unwrap();
    let state = host.lock().unwrap();
    let expires = state.peers.connections[&1].expires;
    assert!(expires >= before + CAPABILITY_LIFETIME);
    assert!(expires <= Instant::now() + CAPABILITY_LIFETIME);
    assert!(crate::device_trust::CAPABILITY_RENEWAL < CAPABILITY_LIFETIME);
}

#[test]
fn forgetting_or_rekeying_a_peer_ends_its_open_connections_at_once() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (unlinked, rekeyed) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let session = Lineage(TerminalId::new());
    register(&path, unlinked, key(5));
    register(&path, rekeyed, key(6));
    let deliveries: Vec<_> = (1..=3)
        .map(|client| host.lock().unwrap().register_client(client))
        .collect();
    let a = prove(&path, &host, 1, unlinked, &key(5), &claim(session)).unwrap();
    let b = prove(&path, &host, 2, unlinked, &key(5), &claim(session)).unwrap();
    let c = prove(&path, &host, 3, rekeyed, &key(6), &claim(session)).unwrap();

    // Unlinking from the linked computer forgets it here: both of its open
    // connections end and close, and the other peer's does not.
    assert_eq!(
        process_with(&host, &path, Request::ForgetPeer { peer: unlinked }).unwrap(),
        Reply::Forgotten { existed: true }
    );
    for (client, authority, delivery) in [(1, &a, &deliveries[0]), (2, &b, &deliveries[1])] {
        assert!(peer_origin(&host, authority).unwrap().is_err());
        assert!(delivery.is_closed(), "closed at once, not when it lapses");
        refused(&host, client, crate::terminal::ipc::ClientRequest::List);
    }
    assert!(peer_origin(&host, &c).unwrap().is_ok());
    assert!(!deliveries[2].is_closed());
    // Nor can it prove a new connection.
    assert!(prove(&path, &host, 4, unlinked, &key(5), &claim(session)).is_err());

    // Linking again replaces the key: connections proved with the old one end.
    assert_eq!(
        process_with(
            &host,
            &path,
            Request::RegisterPeer {
                peer: rekeyed,
                label: "laptop".into(),
                key: key(7),
            },
        )
        .unwrap(),
        Reply::Registered
    );
    assert!(peer_origin(&host, &c).unwrap().is_err());
    assert!(deliveries[2].is_closed());
    assert!(prove(&path, &host, 5, rekeyed, &key(6), &claim(session)).is_err());
    prove(&path, &host, 5, rekeyed, &key(7), &claim(session)).unwrap();

    // A person's other requests end nothing.
    let d = prove(&path, &host, 6, rekeyed, &key(7), &claim(session)).unwrap();
    process_with(&host, &path, Request::Links).unwrap();
    assert!(peer_origin(&host, &d).unwrap().is_ok());
}
