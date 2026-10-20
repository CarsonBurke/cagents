use super::*;
use crate::{
    agent::{Activity, ActivityState, Provider},
    chat::{Launch, NativeContinuity, OwnedCodexListener},
    model::{BootId, HostIncarnation, HostSessionId, TerminalContinuity, TerminalId},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("source"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("owner"),
    }
}

fn id(byte: u8) -> TerminalId {
    TerminalId::from_uuid(uuid::Uuid::from_bytes([byte; 16]))
}

fn source(owner: &HostSessionProof) -> Session {
    let mut session = super::super::tests::session();
    session.id = id(1);
    session.launch = Launch {
        provider: Provider::Codex,
        recipe: crate::model::LaunchRecipe {
            program: "codex".into(),
            arguments: vec!["--remote".into(), "unix://private.sock".into()],
            working_directory: "/private".into(),
            environment: [("ACCOUNT".into(), "fixture".into())].into(),
            unset_environment: ["REMOVE".into()].into(),
        },
        session_id: Some("exact-thread".into()),
    };
    session.proof = Some(owner.clone());
    session.owned_codex_listener = Some(OwnedCodexListener {
        socket: "/private/listener".into(),
        allocator: owner.clone(),
    });
    session.native_continuity = NativeContinuity::Durable {
        provider_version: "fixture-version".into(),
    };
    session
}

// Independent postcard byte fixture, transcribed from the complete selected
// declarations at 503a3319573cfa19d54e4450894bd94ded0c4d65. Neither response
// generation nor expected requests use today's Client/ServerEnvelope enums.
fn uint(mut value: u64, bytes: &mut Vec<u8>) {
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}

fn text(value: &str, bytes: &mut Vec<u8>) {
    uint(value.len() as u64, bytes);
    bytes.extend(value.as_bytes());
}

fn frozen_proof(owner: &HostSessionProof, bytes: &mut Vec<u8>) {
    for value in [
        owner.host_session_id.as_str(),
        owner.boot_id.as_str(),
        owner.host_incarnation.as_str(),
    ] {
        text(value, bytes);
    }
}

fn frozen_id(byte: u8, bytes: &mut Vec<u8>) {
    bytes.push(16); // UUID's non-human-readable byte string length.
    bytes.extend([byte; 16]);
}

fn frozen_recipe(bytes: &mut Vec<u8>) {
    text("codex", bytes);
    bytes.push(2);
    text("--remote", bytes);
    text("unix://private.sock", bytes);
    text("/private", bytes);
    bytes.push(1);
    text("ACCOUNT", bytes);
    text("fixture", bytes);
    bytes.push(1);
    text("REMOVE", bytes);
}

fn frozen_listener(owner: &HostSessionProof, bytes: &mut Vec<u8>) {
    bytes.push(1);
    text("/private/listener", bytes);
    frozen_proof(owner, bytes);
}

fn frozen_session(owner: &HostSessionProof) -> Vec<u8> {
    let mut bytes = Vec::new();
    frozen_id(1, &mut bytes);
    bytes.push(2); // Provider::Codex.
    frozen_recipe(&mut bytes);
    bytes.push(1);
    text("exact-thread", &mut bytes);
    bytes.push(1);
    frozen_proof(owner, &mut bytes);
    bytes.push(0); // No paired terminal in this native chat fixture.
    frozen_listener(owner, &mut bytes);
    bytes.push(1); // NativeContinuity::Durable.
    text("fixture-version", &mut bytes);
    bytes
}

fn terminal(owner: &HostSessionProof) -> ipc::TerminalDescriptor {
    let chat = source(owner);
    let mut value = ipc::TerminalDescriptor::relaunchable(id(2), chat.launch.recipe, None);
    value.session.owned_codex_listener = chat.owned_codex_listener;
    value.session.recovery_origin = Some(id(3));
    value.session.predecessor = Some(id(4));
    value.session.transcript_path = Some("/private/transcript".into());
    value.session.resume_hint = Some("exact-thread".into());
    value.session.continuity = TerminalContinuity::Live {
        host_session_id: owner.host_session_id.clone(),
        boot_id: owner.boot_id.clone(),
        host_incarnation: owner.host_incarnation.clone(),
    };
    value.generation = 17;
    value.event_sequence = 19;
    value.oldest_event_sequence = 3;
    value.attached = true;
    value.transcript = Some(crate::model::TranscriptMetadata {
        path: "/private/transcript".into(),
        byte_len: 100,
        durable_offset: 90,
        updated_at_unix_ms: 9,
        location: Some(crate::model::TranscriptLocationReceipt {
            root_device: 1,
            root_inode: 2,
            file_device: 3,
            file_inode: 4,
        }),
    });
    value.activity = Some(crate::model::TerminalOutputActivity {
        sequence: 5,
        last_output_at_unix_ms: 12,
    });
    value.agent_activity = Some(Activity {
        provider: Provider::Codex,
        state: ActivityState::Working,
    });
    value.upgrade_sources = vec![value.session.clone()];
    value.upgrade_relaunches = vec![ipc::UpgradeRelaunch {
        source: value.session.clone(),
        recipe: value.session.launch.clone(),
    }];
    value
}

// The historical byte oracle must serialize the complete 68 descriptor rather
// than today's descriptor, which may append fields such as terminal titles.
// These eleven fields/order are from source 503a331; frozen_terminal below is
// still the independent primitive-byte fixture for the same historical layout.
#[derive(Serialize)]
struct Descriptor68<'a> {
    screen_lost_after_crash: bool,
    session: &'a crate::model::TerminalSession,
    generation: u64,
    event_sequence: u64,
    oldest_event_sequence: u64,
    attached: bool,
    transcript: &'a Option<crate::model::TranscriptMetadata>,
    activity: &'a Option<crate::model::TerminalOutputActivity>,
    agent_activity: &'a Option<Activity>,
    upgrade_sources: &'a [crate::model::TerminalSession],
    upgrade_relaunches: &'a [ipc::UpgradeRelaunch],
}

impl<'a> From<&'a ipc::TerminalDescriptor> for Descriptor68<'a> {
    fn from(value: &'a ipc::TerminalDescriptor) -> Self {
        Self {
            screen_lost_after_crash: value.screen_lost_after_crash,
            session: &value.session,
            generation: value.generation,
            event_sequence: value.event_sequence,
            oldest_event_sequence: value.oldest_event_sequence,
            attached: value.attached,
            transcript: &value.transcript,
            activity: &value.activity,
            agent_activity: &value.agent_activity,
            upgrade_sources: &value.upgrade_sources,
            upgrade_relaunches: &value.upgrade_relaunches,
        }
    }
}

fn frozen_terminal_session(owner: &HostSessionProof, bytes: &mut Vec<u8>) {
    frozen_id(2, bytes);
    frozen_listener(owner, bytes);
    bytes.push(1);
    frozen_id(3, bytes);
    bytes.push(1);
    frozen_id(4, bytes);
    frozen_recipe(bytes);
    bytes.push(1);
    text("/private/transcript", bytes);
    bytes.push(1);
    text("exact-thread", bytes);
    bytes.push(0); // TerminalContinuity::Live.
    frozen_proof(owner, bytes);
}

fn frozen_terminal(owner: &HostSessionProof) -> Vec<u8> {
    let mut bytes = vec![0]; // screen_lost_after_crash.
    frozen_terminal_session(owner, &mut bytes);
    bytes.extend([17, 19, 3, 1, 1]);
    text("/private/transcript", &mut bytes);
    bytes.extend([100, 90, 18, 1, 1, 2, 3, 4]); // i64 time uses zigzag.
    bytes.extend([1, 5, 24, 1, 2, 2]); // output activity; Codex/Working.
    bytes.push(1);
    frozen_terminal_session(owner, &mut bytes);
    bytes.push(1);
    frozen_terminal_session(owner, &mut bytes);
    frozen_recipe(&mut bytes);
    bytes
}

fn response(owner: &HostSessionProof, request: u64, variant: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![68, 1];
    uint(request, &mut bytes);
    frozen_proof(owner, &mut bytes);
    bytes.extend([0, variant]); // ServerMessage::Response.
    bytes.extend(payload);
    bytes
}

fn expected_request(owner: &HostSessionProof, request: u64, variant: u8) -> Vec<u8> {
    let mut bytes = vec![68];
    uint(request, &mut bytes);
    bytes.push(u8::from(variant != 0));
    if variant != 0 {
        frozen_proof(owner, &mut bytes);
    }
    bytes.push(variant);
    match variant {
        0 => {
            bytes.push(1);
            frozen_proof(owner, &mut bytes);
            bytes.push(0);
        }
        38 => bytes.extend(frozen_session(owner)),
        _ => {}
    }
    bytes
}

fn receive_raw(peer: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut size = [0; 4];
    peer.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; size];
    peer.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn send_raw(peer: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    peer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    peer.write_all(bytes)
}

#[test]
fn authentic_68_bytes_pin_populated_read_only_requests_and_responses() {
    let owner = proof();
    let session = source(&owner);
    assert_eq!(
        postcard::to_allocvec(&session).unwrap(),
        frozen_session(&owner)
    );
    assert_eq!(
        postcard::to_allocvec(&Descriptor68::from(&terminal(&owner))).unwrap(),
        frozen_terminal(&owner)
    );
    for (variant, request) in [
        (0, Request::Connect(&owner)),
        (1, Request::ListTerminals),
        (29, Request::ListChats),
        (38, Request::Resolve(&session)),
    ] {
        assert_eq!(
            postcard::to_allocvec(&RequestEnvelope {
                version: VERSION,
                request_id: 7,
                proof: (variant != 0).then_some(&owner),
                request,
            })
            .unwrap(),
            expected_request(&owner, 7, variant)
        );
    }
    let mut connected = Vec::new();
    frozen_proof(&owner, &mut connected);
    connected.push(0);
    let value: Envelope = postcard::from_bytes(&response(&owner, 7, 0, &connected)).unwrap();
    assert!(
        matches!(value.message, Message::Response { response: Response::Connected(p) } if p == owner)
    );
    let mut listed = vec![1];
    listed.extend(frozen_session(&owner));
    let value: Envelope = postcard::from_bytes(&response(&owner, 7, 13, &listed)).unwrap();
    assert!(
        matches!(value.message, Message::Response { response: Response::Listed(s) } if s == vec![session.clone()])
    );
    let value: Envelope =
        postcard::from_bytes(&response(&owner, 7, 18, &frozen_session(&owner))).unwrap();
    assert!(
        matches!(value.message, Message::Response { response: Response::Resolved(s) } if s == session)
    );
    let mut terminals = vec![1];
    terminals.extend(frozen_terminal(&owner));
    let value: Envelope = postcard::from_bytes(&response(&owner, 7, 1, &terminals)).unwrap();
    assert!(
        matches!(value.message, Message::Response { response: Response::Terminals(t) } if t.len() == 1 && t[0].session == terminal(&owner).session && t[0].upgrade_sources == terminal(&owner).upgrade_sources)
    );
}

#[test]
fn private_68_peer_authenticates_real_lease_and_four_read_only_calls() {
    let directory = tempfile::tempdir().unwrap();
    let paths = crate::daemon::RuntimePaths {
        directory: directory.path().into(),
        socket: directory.path().join("cagentsd.sock"),
        lock: directory.path().join("cagentsd.lock"),
    };
    let mut owner = proof();
    owner.boot_id = crate::daemon::read_boot_id().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &owner).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server_owner = owner.clone();
    let worker = std::thread::spawn(move || -> Result<Vec<Vec<u8>>, String> {
        let until = Instant::now() + Duration::from_secs(3);
        let (mut peer, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        peer.set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        let mut received = Vec::new();
        for (request, variant) in [(1, 0), (2, 29), (3, 1), (4, 38)] {
            received.push(receive_raw(&mut peer).map_err(|e| e.to_string())?);
            let (reply, payload) = match variant {
                0 => {
                    let mut payload = Vec::new();
                    frozen_proof(&server_owner, &mut payload);
                    payload.push(0);
                    (0, payload)
                }
                29 => (13, [vec![1], frozen_session(&server_owner)].concat()),
                1 => (1, [vec![1], frozen_terminal(&server_owner)].concat()),
                _ => (18, frozen_session(&server_owner)),
            };
            send_raw(
                &mut peer,
                &response(&server_owner, request, reply, &payload),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(received)
    });
    let outcome = (|| -> Result<_, String> {
        let mut client = Client::connect(
            &paths.socket,
            &owner,
            Instant::now() + Duration::from_secs(3),
        )?;
        let chats = client.list()?;
        let terminals = client.terminals()?;
        let resolved = client.resolve(&source(&owner))?;
        Ok((chats, terminals, resolved))
    })();
    let joined = worker.join();
    drop(lease);
    let received = joined.unwrap().unwrap();
    let (chats, terminals, resolved) = outcome.unwrap();
    assert_eq!(chats, vec![source(&owner)]);
    assert_eq!(terminals[0].session, terminal(&owner).session);
    assert_eq!(resolved, source(&owner));
    assert_eq!(
        received,
        [(1, 0), (2, 29), (3, 1), (4, 38)].map(|(id, v)| expected_request(&owner, id, v))
    );
}

#[test]
fn stale_foreign_malformed_and_streamed_68_responses_are_refused() {
    for mismatch in 0..7 {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = std::thread::spawn(move || {
            let request = receive_raw(&mut peer).unwrap();
            let mut bytes = response(&proof(), 1, 13, &[0]);
            match mismatch {
                0 => bytes[0] = 77,
                1 => bytes = response(&proof(), 2, 13, &[0]),
                2 => {
                    let mut owner = proof();
                    owner.host_incarnation = HostIncarnation::new("foreign");
                    bytes = response(&owner, 1, 13, &[0]);
                }
                3 => {
                    bytes.pop();
                }
                4 => {
                    bytes = response(&proof(), 1, 5, &[]); // Accepted is forbidden.
                }
                5 => {
                    bytes = vec![68, 0]; // Unsolicited request identity.
                    frozen_proof(&proof(), &mut bytes);
                    bytes.push(2); // ServerMessage::Snapshot: reject before payload.
                }
                _ => bytes = response(&proof(), 1, 13, &[129, 1]), // Truncated oversized inventory.
            }
            send_raw(&mut peer, &bytes).unwrap();
            request
        });
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            deadline: Instant::now() + Duration::from_secs(2),
        };
        let outcome = client.list();
        drop(client);
        let joined = worker.join();
        let request = joined.unwrap();
        assert!(outcome.is_err(), "mismatch {mismatch}");
        assert_eq!(request, expected_request(&proof(), 1, 29));
    }
}

#[test]
fn expired_68_inspection_sends_no_request_and_oversized_frames_are_refused() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut client = Client {
        stream,
        proof: proof(),
        next: 1,
        deadline: Instant::now() - Duration::from_millis(1),
    };
    let outcome = client.list();
    let observed = peer.read(&mut [0; 1]).unwrap_err().kind();
    drop(client);
    assert!(outcome.is_err());
    assert_eq!(observed, io::ErrorKind::WouldBlock);
    let header = ((MAX_FRAME + 1) as u32).to_be_bytes();
    assert!(ipc::read_frame_with_limit::<Envelope>(&mut &header[..], MAX_FRAME).is_err());
}

#[test]
fn full_68_inventories_obey_caps_and_stale_lease_sends_no_request() {
    for (terminals, count, expected_refusal) in [
        (false, 129, false),
        (false, crate::upgrade::handoff::MAX_CHATS as u64, false),
        (false, crate::upgrade::handoff::MAX_CHATS as u64 + 1, true),
        (true, 1025, true),
    ] {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = std::thread::spawn(move || {
            let request = receive_raw(&mut peer).unwrap();
            let (variant, item) = if terminals {
                (1, frozen_terminal(&proof()))
            } else {
                (13, frozen_session(&proof()))
            };
            let mut payload = Vec::new();
            uint(count, &mut payload);
            for _ in 0..count {
                payload.extend(&item);
            }
            send_raw(&mut peer, &response(&proof(), 1, variant, &payload)).unwrap();
            request
        });
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            deadline: Instant::now() + Duration::from_secs(2),
        };
        let refused = if terminals {
            client.terminals().is_err()
        } else {
            client.list().is_err()
        };
        drop(client);
        let request = worker.join().unwrap();
        assert_eq!(
            refused, expected_refusal,
            "terminals={terminals}, count={count}"
        );
        assert_eq!(
            request,
            expected_request(&proof(), 1, if terminals { 1 } else { 29 })
        );
    }

    // A reachable listener cannot replace positive exclusive-lease ownership.
    let directory = tempfile::tempdir().unwrap();
    let paths = crate::daemon::RuntimePaths {
        directory: directory.path().into(),
        socket: directory.path().join("cagentsd.sock"),
        lock: directory.path().join("cagentsd.lock"),
    };
    let mut owner = proof();
    owner.boot_id = crate::daemon::read_boot_id().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &owner).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut foreign = owner.clone();
    foreign.host_incarnation = HostIncarnation::new("stale-discovery");
    let refused = Client::connect(
        &paths.socket,
        &foreign,
        Instant::now() + Duration::from_secs(2),
    )
    .is_err();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let bytes = peer.read(&mut [0; 1]).unwrap();
    drop(peer);
    drop(listener);
    drop(lease);
    assert!(refused);
    assert_eq!(
        bytes, 0,
        "stale lease must fail before sending even Connect"
    );
}
