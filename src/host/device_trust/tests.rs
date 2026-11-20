use super::*;
use crate::model::TerminalId;

fn key(byte: u8) -> LinkKey {
    LinkKey::from_bytes([byte; SECRET_BYTES])
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
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, session);
    let authority = authenticate_at(&path, &host, client, peer, session, &proof).unwrap();
    assert!(is_peer_capability(&authority));
    let expected = crate::device_trust::peer_lineage(peer, session);
    assert_eq!(
        peer_origin(&host, &authority).unwrap().unwrap(),
        Origin::Child { parent: expected }
    );

    // The nonce was spent: the same proof cannot be presented again.
    let replay = authenticate_at(&path, &host, client, peer, session, &proof).unwrap_err();
    assert_eq!(replay.code, ProtocolErrorCode::PermissionDenied);

    host.lock().unwrap().peers.disconnect(client);
    assert!(peer_origin(&host, &authority).unwrap().is_err());
}

#[test]
fn a_proof_for_another_key_lineage_or_connection_is_refused() {
    let (_directory, path) = store();
    let host = super::super::service::test_host();
    let (peer, session) = (uuid::Uuid::new_v4(), Lineage(TerminalId::new()));
    register(&path, peer, key(5));

    let nonce = challenge(&host, 1).unwrap();
    let wrong_key = crate::device_trust::proof(&key(6), &nonce, peer, session);
    assert!(authenticate_at(&path, &host, 1, peer, session, &wrong_key).is_err());

    let nonce = challenge(&host, 1).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, session);
    let other = Lineage(TerminalId::new());
    assert!(authenticate_at(&path, &host, 1, peer, other, &proof).is_err());

    // Another connection's challenge does not stand in for this one's.
    let nonce = challenge(&host, 1).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, peer, session);
    assert!(authenticate_at(&path, &host, 2, peer, session, &proof).is_err());

    let unknown = uuid::Uuid::new_v4();
    let nonce = challenge(&host, 3).unwrap();
    let proof = crate::device_trust::proof(&key(5), &nonce, unknown, session);
    let error = authenticate_at(&path, &host, 3, unknown, session, &proof).unwrap_err();
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
