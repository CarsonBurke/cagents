use super::*;

fn bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

pub(crate) fn frames() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/update-source/56.json"
    ))
    .unwrap()
}

#[test]
fn real_0387aa2_frames_pin_protocol56_payloads_and_requests() {
    let fixture = frames();
    let mut owner = None;
    let mut previous = 0;
    for frame in fixture["frames"].as_array().unwrap() {
        let request = bytes(frame["request"].as_str().unwrap());
        let response = bytes(frame["response"].as_str().unwrap());
        let ((version, id, proof, tag), payload) =
            postcard::take_from_bytes::<(u16, u64, Option<HostSessionProof>, u32)>(&request)
                .unwrap();
        assert_eq!(version, 56);
        assert_eq!(id, previous + 1);
        previous = id;
        assert_eq!(proof, owner);
        // Reply header is independent of the general-purpose current enum.
        let ((version, request_id, actual, message), body) =
            postcard::take_from_bytes::<(u16, Option<u64>, HostSessionProof, u32)>(&response)
                .unwrap();
        assert_eq!(version, 56);
        assert_eq!(request_id, Some(id));
        if let Some(owner) = &owner {
            assert_eq!(&actual, owner);
        }
        owner = Some(actual.clone());
        if frame["operation"] == "resolve-chat" {
            assert_eq!((tag, message), (38, 6)); // Real source has no replacement yet.
            assert!(postcard::from_bytes::<Chat>(payload).is_ok());
            continue;
        }
        assert_eq!(message, 0);
        let (response_tag, body) = postcard::take_from_bytes::<u32>(body).unwrap();
        match frame["operation"].as_str().unwrap() {
            "connect" => {
                assert_eq!((tag, response_tag, payload), (0, 0, &[0, 0][..]));
                let (proof, resumed): (HostSessionProof, Vec<()>) =
                    postcard::from_bytes(body).unwrap();
                assert_eq!(proof, actual);
                assert!(resumed.is_empty());
            }
            "terminal-list" => {
                assert_eq!((tag, response_tag), (1, 1));
                assert!(payload.is_empty());
                let terminals: Vec<Terminal> = postcard::from_bytes(body).unwrap();
                assert_eq!(terminals.len(), 3);
                let terminals: Vec<ipc::TerminalDescriptor> =
                    terminals.into_iter().map(Into::into).collect();
                let transcript = terminals
                    .iter()
                    .find_map(|t| t.transcript.as_ref())
                    .unwrap();
                assert!(transcript.path.ends_with("wire56-terminal.log"));
                assert_eq!(transcript.location, None);
            }
            "chat-list" => {
                assert_eq!((tag, response_tag), (29, 13));
                assert!(payload.is_empty());
                let chats: Vec<Chat> = postcard::from_bytes(body).unwrap();
                assert_eq!(chats.len(), 3);
                assert!(chats.iter().all(|s| s.proof.as_ref() == Some(&actual)));
                for chat in chats {
                    let old = postcard::to_stdvec(&chat).unwrap();
                    let mut current = crate::chat::Session::from(chat);
                    assert_eq!(
                        postcard::to_stdvec(&Chat::try_from(&current).unwrap()).unwrap(),
                        old
                    );
                    current.native_continuity =
                        crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 1 };
                    assert!(Chat::try_from(&current).is_err());
                }
            }
            "impact" => {
                assert_eq!((tag, response_tag, payload), (36, 9, &[0][..]));
                let impact: Impact = postcard::from_bytes(body).unwrap();
                let current: impact::DaemonImpact = impact.into();
                assert_eq!(current.capability.proof, actual);
                assert_eq!(current.sessions.len(), 6);
                assert_eq!(current.capability.background_subagents, 0);
                assert!(current.capability.background_identity.is_empty());
                assert!(
                    current
                        .sessions
                        .iter()
                        .all(|s| s.interrupted_subagents.is_empty())
                );
                assert_eq!(postcard::to_stdvec(&Impact::from(current)).unwrap(), body);
                // Preparation carries precisely the source's complete old
                // impact bytes followed by one boolean; no later fields.
                let old: Impact = postcard::from_bytes(body).unwrap();
                let prepared = postcard::to_stdvec(&(old, false)).unwrap();
                assert_eq!(&prepared[..body.len()], body);
                assert_eq!(&prepared[body.len()..], &[0]);
            }
            "shutdown-preflight" => {
                assert_eq!((tag, response_tag, payload), (18, 8, &[1, 0][..]));
                let capability: Capability = postcard::from_bytes(body).unwrap();
                let current: ipc::ShutdownCapability = capability.into();
                assert_eq!(current.proof, actual);
                assert_eq!(
                    postcard::to_stdvec(&Capability::from(current)).unwrap(),
                    body
                );
            }
            other => panic!("uncovered operation {other}"),
        }
    }
}

#[cfg(feature = "host")]
#[test]
fn authenticated_protocol56_client_sends_exact_preparation_and_confirmed_stop() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };
    fn receive(stream: &mut std::os::unix::net::UnixStream) -> Vec<u8> {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_be_bytes(prefix) as usize;
        assert!(length < 1024 * 1024);
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).unwrap();
        bytes
    }
    fn send<T: Serialize>(stream: &mut std::os::unix::net::UnixStream, value: T) {
        let bytes = postcard::to_stdvec(&value).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
    }
    let fixture = frames();
    let raw = fixture["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["operation"] == "impact")
        .unwrap();
    let encoded = bytes(raw["response"].as_str().unwrap());
    let (_, body) =
        postcard::take_from_bytes::<(u16, Option<u64>, HostSessionProof, u32, u32)>(&encoded)
            .unwrap();
    for shutdown in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("cagentsd.sock");
        let mut impact: impact::DaemonImpact = postcard::from_bytes::<Impact>(body).unwrap().into();
        impact.capability.proof.boot_id = crate::daemon::read_boot_id().unwrap();
        let proof = impact.capability.proof.clone();
        let paths = crate::daemon::RuntimePaths {
            directory: directory.path().into(),
            socket: socket.clone(),
            lock: directory.path().join("cagentsd.lock"),
        };
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let answer = impact.clone();
        let owner = proof.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(
                receive(&mut stream),
                postcard::to_stdvec(&(
                    56u16,
                    1u64,
                    None::<HostSessionProof>,
                    0u32,
                    None::<HostSessionProof>,
                    Vec::<()>::new()
                ))
                .unwrap()
            );
            send(
                &mut stream,
                (
                    56u16,
                    Some(1u64),
                    &owner,
                    0u32,
                    0u32,
                    &owner,
                    Vec::<()>::new(),
                ),
            );
            if shutdown {
                assert_eq!(
                    receive(&mut stream),
                    postcard::to_stdvec(&(56u16, 2u64, Some(&owner), 18u32, 1u16, 0u32)).unwrap()
                );
                send(
                    &mut stream,
                    (
                        56u16,
                        Some(2u64),
                        &owner,
                        0u32,
                        8u32,
                        Capability::from(answer.capability.clone()),
                    ),
                );
                assert_eq!(
                    receive(&mut stream),
                    postcard::to_stdvec(&(
                        56u16,
                        3u64,
                        Some(&owner),
                        18u32,
                        1u16,
                        2u32,
                        Capability::from(answer.capability)
                    ))
                    .unwrap()
                );
                send(
                    &mut stream,
                    (
                        56u16,
                        Some(3u64),
                        &owner,
                        0u32,
                        10u32,
                        ipc::ShutdownOutcome::Accepted {
                            live_terminals_before: 3,
                            termination_requested: true,
                        },
                    ),
                );
            } else {
                assert_eq!(
                    receive(&mut stream),
                    postcard::to_stdvec(&(56u16, 2u64, Some(&owner), 36u32, 0u32)).unwrap()
                );
                send(
                    &mut stream,
                    (
                        56u16,
                        Some(2u64),
                        &owner,
                        0u32,
                        9u32,
                        Impact::from(answer.clone()),
                    ),
                );
                assert_eq!(
                    receive(&mut stream),
                    postcard::to_stdvec(&(
                        56u16,
                        3u64,
                        Some(&owner),
                        37u32,
                        Impact::from(answer),
                        false
                    ))
                    .unwrap()
                );
                send(
                    &mut stream,
                    (56u16, Some(3u64), &owner, 0u32, 17u32, 3u64, 3u64),
                );
            }
        });
        let mut client = Client::connect(&socket, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(client.proof(), &proof);
        if shutdown {
            let capability = client.shutdown_preflight().unwrap();
            let mut fabricated = capability.clone();
            fabricated.background_subagents = 1;
            assert!(client.confirm_shutdown(fabricated).is_err());
            assert!(matches!(
                client.confirm_shutdown(capability).unwrap(),
                ipc::ShutdownOutcome::Accepted { .. }
            ));
        } else {
            let observed = client.upgrade_impact(impact::Policy::WhenIdle).unwrap();
            assert_eq!(observed, impact);
            let mut fabricated = observed.clone();
            fabricated.sessions[0]
                .interrupted_subagents
                .push("unreviewed".into());
            assert!(client.prepare_confirmed_upgrade(fabricated, false).is_err());
            assert_eq!(
                client.prepare_confirmed_upgrade(observed, false).unwrap(),
                (3, 3)
            );
        }
        server.join().unwrap();
    }
}
