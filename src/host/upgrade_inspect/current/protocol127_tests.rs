//! Literal predecessor contracts, independent of the destination's encoder.
//! The adjacent fixture README pins the protocol-127 schema and generation.
//! Scripted peers qualify framing and identity checks, not the unchanged
//! production lease/SO_PEERCRED admission or a running legacy provider.
use super::*;
use crate::{
    agent::Provider,
    chat::{Launch, NativeContinuity},
    model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalId},
};

macro_rules! frozen {
    ($name:literal) => {
        include_bytes!(concat!(
            "../../../../tests/fixtures/upgrade-inspection-v127/",
            $name
        ))
    };
}

fn owner() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("source"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("actor"),
    }
}

fn saved() -> Session {
    Session {
        id: TerminalId::from_uuid("00000000-0000-4000-8000-000000000127".parse().unwrap()),
        launch: Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: vec!["--model".into(), "fixture".into()],
                working_directory: "/fixture/removed-worktree".into(),
                environment: [("CLAUDE_CONFIG_DIR".into(), "/fixture/claude-config".into())].into(),
                unset_environment: ["FIXTURE_UNSET".into()].into(),
            },
            session_id: Some("11111111-1111-4111-8111-111111111111".into()),
        },
        proof: Some(HostSessionProof {
            host_session_id: HostSessionId::new("saved"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("previous"),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: NativeContinuity::ReconnectOnly,
    }
}

fn retained() -> Session {
    let mut source = saved();
    source.id = TerminalId::from_uuid("00000000-0000-4000-8000-000000000128".parse().unwrap());
    source.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
    source.proof = Some(owner());
    source.native_continuity = NativeContinuity::RetainedClaudeActor { protocol: 2 };
    source
}

fn client(stream: UnixStream, next: u64) -> Client {
    Client {
        stream,
        proof: owner(),
        next,
        protocol: 127,
        deadline: Instant::now() + Duration::from_secs(5),
    }
}

// The peer checks historical bytes, not a request decoded using current DTOs.
fn expect_request(peer: &mut UnixStream, expected: &[u8]) {
    let mut prefix = [0; 4];
    peer.read_exact(&mut prefix).unwrap();
    let length = u32::from_be_bytes(prefix) as usize;
    assert!(length > 0 && length <= MAX_FRAME);
    let mut actual = prefix.to_vec();
    actual.resize(4 + length, 0);
    peer.read_exact(&mut actual[4..]).unwrap();
    assert_eq!(
        actual, expected,
        "request departed the frozen read-only contract"
    );
}

fn pair() -> (UnixStream, UnixStream) {
    let (stream, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    (stream, peer)
}

#[test]
fn frozen_127_saved_inventory_and_history_are_readable_by_the_139_inspector() {
    assert_eq!(
        ipc::PROTOCOL_VERSION,
        139,
        "qualify every destination wire change"
    );
    assert!(supports_protocol(127));
    let (stream, mut peer) = pair();
    let worker = std::thread::spawn(move || {
        let steps: [(&[u8], &[u8]); 8] = [
            (
                frozen!("request-01-connect.bin"),
                frozen!("response-01-connected.bin"),
            ),
            (
                frozen!("request-02-chat-list.bin"),
                frozen!("response-02-chat-listed.bin"),
            ),
            (
                frozen!("request-03-list.bin"),
                frozen!("response-03-listed.bin"),
            ),
            (
                frozen!("request-04-resolve.bin"),
                frozen!("response-04-resolved.bin"),
            ),
            (
                frozen!("request-05-chat-attach.bin"),
                frozen!("response-05-chat-attached.bin"),
            ),
            (
                frozen!("request-06-chat-visibility.bin"),
                frozen!("response-06-accepted.bin"),
            ),
            (
                frozen!("request-07-chat-read.bin"),
                frozen!("response-07-chat-snapshot.bin"),
            ),
            (
                frozen!("request-08-chat-detach.bin"),
                frozen!("response-08-detached.bin"),
            ),
        ];
        for (index, (request, response)) in steps.into_iter().enumerate() {
            expect_request(&mut peer, request);
            if index >= 4 {
                peer.write_all(frozen!("event-chat-changed.bin")).unwrap();
            }
            peer.write_all(response).unwrap();
        }
        assert_eq!(
            peer.read(&mut [0]).unwrap(),
            0,
            "unexpected extra source command"
        );
    });
    let mut client = client(stream, 1);
    assert!(matches!(
        client.request(ClientRequest::Connect { expected_host: Some(owner()), resume: Vec::new() }, false).unwrap(),
        ServerResponse::Connected { proof, resumed } if proof == owner() && resumed.is_empty()
    ));
    let source = saved();
    let retained = retained();
    assert!(source.is_saved_claude_history());
    assert!(retained.has_valid_native_continuity());
    assert_eq!(client.list().unwrap(), [source.clone(), retained.clone()]);
    assert!(client.terminals().unwrap().is_empty());
    // 127 already has no-launch Resolve. The 119-only bootstrap must stay idle.
    client.reconcile_retained(&retained).unwrap();
    assert_eq!(client.resolve(&retained).unwrap(), retained);
    let original = source.clone();
    client.inspect_saved_history(&source).unwrap();
    assert_eq!(
        source, original,
        "inspection must not rewrite the saved ownership proof"
    );
    drop(client);
    worker.join().unwrap();

    // Also qualify real transcript content in the historical Snapshot layout.
    let frame = frozen!("response-07-chat-snapshot.bin");
    assert_eq!(&frame[..4], &((frame.len() - 4) as u32).to_be_bytes());
    let (reply, remaining) = postcard::take_from_bytes::<ServerEnvelope>(&frame[4..]).unwrap();
    assert!(remaining.is_empty());
    assert_eq!(reply.version, 127);
    let ServerMessage::Response {
        response:
            ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: Some(snapshot),
            },
    } = reply.message
    else {
        panic!("missing historical transcript");
    };
    assert_eq!(chat_id, source.id);
    assert_eq!(snapshot.session_id, source.launch.session_id);
    assert!(matches!(&snapshot.items[0].content,
        crate::chat::ItemContent::Text { text, thinking: false, phase: crate::chat::Phase::Final }
        if text == "frozen127 retained history"));
}

fn request_id(mut frame: Vec<u8>, id: u8) -> Vec<u8> {
    assert_eq!(frame[4], 127);
    assert_eq!(frame[5], 1, "golden response must have a request ID");
    assert!(id < 128 && frame[6] < 128);
    frame[6] = id;
    frame
}

fn request_number(mut frame: Vec<u8>, id: u8) -> Vec<u8> {
    assert_eq!(frame[4], 127);
    assert!(id < 128 && frame[5] < 128);
    frame[5] = id;
    frame
}

fn replace(mut frame: Vec<u8>, before: &[u8], after: &[u8]) -> Vec<u8> {
    assert_eq!(before.len(), after.len());
    let at = frame
        .windows(before.len())
        .position(|value| value == before)
        .unwrap();
    frame[at..at + after.len()].copy_from_slice(after);
    frame
}

fn trailing(mut frame: Vec<u8>) -> Vec<u8> {
    frame.push(0);
    let length = (frame.len() - 4) as u32;
    frame[..4].copy_from_slice(&length.to_be_bytes());
    frame
}

#[test]
fn frozen_127_inventory_refuses_changed_identity_errors_and_unknown_layouts() {
    let original = frozen!("response-02-chat-listed.bin").to_vec();
    let mut version = original.clone();
    version[4] = 126;
    let mut truncated = original.clone();
    truncated.pop();
    let cases = [
        (version, "Source inspection ownership or protocol changed"),
        (
            request_id(original.clone(), 3),
            "Source inspection request or event identity changed",
        ),
        (
            replace(original.clone(), b"actor", b"other"),
            "Source inspection ownership or protocol changed",
        ),
        (trailing(original), "Source response has an unknown layout"),
        (truncated, "Truncated source response"),
        (
            request_id(frozen!("error-permission-denied.bin").to_vec(), 2),
            "Unsupported source response",
        ),
        (
            frozen!("event-chat-changed.bin").to_vec(),
            "Unsupported source response",
        ),
    ];
    for (response, expected) in cases {
        let (stream, mut peer) = pair();
        let worker = std::thread::spawn(move || {
            expect_request(&mut peer, frozen!("request-02-chat-list.bin"));
            peer.write_all(&response).unwrap();
        });
        let mut client = client(stream, 2);
        let error = client.list().unwrap_err();
        assert_eq!(error, expected, "a later I/O failure is not this refusal");
        assert!(
            !error.contains("legacy source refused"),
            "source details stay redacted"
        );
        drop(client);
        worker.join().unwrap();
    }
}

#[test]
fn frozen_127_history_refuses_changed_owner_event_error_and_trailing_payload() {
    let original = frozen!("response-05-chat-attached.bin").to_vec();
    let mut version = original.clone();
    version[4] = 126;
    let mut truncated = original.clone();
    truncated.pop();
    let mut foreign_chat = frozen!("event-chat-changed.bin").to_vec();
    *foreign_chat.last_mut().unwrap() ^= 1;
    for (response, expected) in [
        (version, "Source inspection ownership or protocol changed"),
        (
            request_id(original.clone(), 6),
            "Source inspection request or event identity changed",
        ),
        (
            replace(original.clone(), b"actor", b"other"),
            "Source inspection ownership or protocol changed",
        ),
        (
            trailing(original),
            "Source retained-owner response has an unknown layout",
        ),
        (truncated, "Truncated source response"),
        (
            frozen!("error-permission-denied.bin").to_vec(),
            "Unsupported legacy retained-owner response",
        ),
        (
            foreign_chat,
            "Source inspection request or event identity changed",
        ),
    ] {
        let (stream, mut peer) = pair();
        let worker = std::thread::spawn(move || {
            expect_request(&mut peer, frozen!("request-05-chat-attach.bin"));
            peer.write_all(&response).unwrap();
        });
        let mut client = client(stream, 5);
        let original = saved();
        let error = client.inspect_saved_history(&original).unwrap_err();
        assert_eq!(error, expected, "a later I/O failure is not this refusal");
        assert!(
            !error.contains("legacy source refused"),
            "source details stay redacted"
        );
        assert_eq!(original, saved());
        drop(client);
        worker.join().unwrap();
    }
}

#[test]
fn frozen_127_history_detaches_after_an_inexact_conversation_without_recovery_commands() {
    let (stream, mut peer) = pair();
    let worker = std::thread::spawn(move || {
        expect_request(&mut peer, frozen!("request-05-chat-attach.bin"));
        // Change the issued recipe while retaining a fully decodable frame.
        let response = replace(
            frozen!("response-05-chat-attached.bin").to_vec(),
            b"fixture",
            b"fixturE",
        );
        peer.write_all(&response).unwrap();
        expect_request(
            &mut peer,
            &request_number(frozen!("request-08-chat-detach.bin").to_vec(), 6),
        );
        peer.write_all(&request_id(frozen!("response-08-detached.bin").to_vec(), 6))
            .unwrap();
        assert_eq!(
            peer.read(&mut [0]).unwrap(),
            0,
            "refusal must not start or replace a provider"
        );
    });
    let mut client = client(stream, 5);
    assert_eq!(
        client.inspect_saved_history(&saved()).unwrap_err(),
        "Saved history changed its exact account, recipe or conversation"
    );
    drop(client);
    worker.join().unwrap();
}

#[test]
fn frozen_127_error_envelope_has_no_destination_error_details() {
    #[derive(serde::Serialize)]
    struct Error127 {
        code: u8,
        message: &'static str,
        retryable: bool,
    }
    // Independent three-field historical contract, not ProtocolError::new.
    let payload = postcard::to_allocvec(&Error127 {
        code: 10,
        message: "legacy source refused",
        retryable: false,
    })
    .unwrap();
    let frame = frozen!("error-permission-denied.bin");
    assert_eq!(
        frozen!("error-minimal.bin").as_slice(),
        b"\x00\x00\x00\x22\x7f\x01\x05\x01s\x01b\x01i\x06\x0a\x15legacy source refused\x00",
        "the literal predecessor error has exactly three positional fields"
    );
    assert!(frame.ends_with(&payload));
    assert!(
        postcard::from_bytes::<ipc::ProtocolError>(&payload).is_err(),
        "serde default does not qualify a missing positional field"
    );
}
