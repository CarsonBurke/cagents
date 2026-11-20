//! Source protocol discovery. It reads only a source's bounded version header,
//! never its error or session DTOs, and authorizes nothing by itself.
use super::{Peer, UpdateError, invalid};
use crate::model::HostSessionProof;
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

/// Read the source protocol and refuse it unless live updates start from it.
/// Version discovery deliberately does not decode an error or session DTO.
/// Only the subsequent version-specific handshake can authenticate ownership.
pub(super) fn discover_qualified_protocol(
    socket: &Path,
    deadline: Instant,
) -> Result<u16, crate::control::ControlError> {
    let (bytes, _) = discover_reply(socket, deadline)?;
    discovered_version(&bytes).map_err(discovery_error)
}

/// Read any source protocol, qualified or not, with the peer that served it,
/// leaving admission to the caller. The caller must admit the protocol and
/// authenticate a fresh handshake before inspecting the installation.
pub(super) fn discover_protocol_and_peer(
    socket: &Path,
    deadline: Instant,
) -> Result<(u16, Peer), crate::control::ControlError> {
    let (bytes, peer) = discover_reply(socket, deadline)?;
    let version = version_header(&bytes).map_err(discovery_error)?;
    Ok((version, peer))
}

fn discover_reply(
    socket: &Path,
    deadline: Instant,
) -> Result<(Vec<u8>, Peer), crate::control::ControlError> {
    let mut stream = crate::control::connect_control_socket(
        socket,
        deadline.saturating_duration_since(Instant::now()),
    )
    .map_err(crate::control::ControlError::Connect)?;
    let peer = Peer::observe(&stream, socket).map_err(discovery_error)?;
    let bytes = postcard::to_stdvec(&(
        crate::terminal_ipc::PROTOCOL_VERSION,
        1u64,
        None::<HostSessionProof>,
        0u32,
        None::<HostSessionProof>,
        Vec::<()>::new(),
    ))
    .map_err(|_| discovery_error(invalid("Cannot encode source version discovery")))?;
    write(&mut stream, &bytes, deadline).map_err(discovery_error)?;
    let bytes = read(&mut stream, deadline).map_err(discovery_error)?;
    Ok((bytes, peer))
}

fn discovery_error(error: UpdateError) -> crate::control::ControlError {
    crate::control::ControlError::Configure(io::Error::other(error.to_string()))
}

fn version_header(bytes: &[u8]) -> Result<u16, UpdateError> {
    postcard::take_from_bytes::<u16>(bytes)
        .map(|(version, _)| version)
        .map_err(|_| invalid("Malformed source version header"))
}

fn discovered_version(bytes: &[u8]) -> Result<u16, UpdateError> {
    let version = version_header(bytes)?;
    if !super::is_qualified_source(version) {
        return Err(super::unsupported_source(version));
    }
    Ok(version)
}

struct DeadlineStream<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl DeadlineStream<'_> {
    fn remaining(&self) -> io::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "source operation deadline elapsed",
            ))
        } else {
            Ok(remaining)
        }
    }
}
impl Read for DeadlineStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn write(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<(), UpdateError> {
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(invalid(
            "The source request exceeds its bounded frame limit",
        ));
    }
    let mut stream = DeadlineStream { stream, deadline };
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)?;
    Ok(())
}
fn read(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, UpdateError> {
    let mut stream = DeadlineStream { stream, deadline };
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > 8 * 1024 * 1024 {
        return Err(invalid("The source exceeded its bounded frame limit"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::impact;
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};
    use crate::terminal_ipc::{ClientRequest, ServerEnvelope, ServerResponse};

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("source-host"),
            boot_id: BootId::new("source-boot"),
            host_incarnation: HostIncarnation::new("source-incarnation"),
        }
    }

    fn impact() -> impact::DaemonImpact {
        impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof(),
                token: "exact-reviewed-capability".into(),
                inventory_generation: 7,
                live_terminals: 1,
                background_subagents: 1,
                background_identity: "exact-background".into(),
                expires_at_unix_ms: 123,
            },
            policy: impact::Policy::Interrupt,
            sessions: vec![impact::SessionImpact {
                id: crate::model::TerminalId::new(),
                kind: impact::SessionKind::NativeChat,
                title: "Source conversation".into(),
                identity: "exact-source-hash".into(),
                disposition: impact::Disposition::Interrupted,
                reason: "User reviewed interruption".into(),
                interrupted_subagents: vec!["background-id".into()],
            }],
            blockers: Vec::new(),
        }
    }

    #[test]
    fn source_version_header_precedes_old_error_dto_and_rejects_unknown_versions() {
        // A version-mismatch reply from the immutable source may contain an
        // error variant/layout the new normal client cannot decode at all.
        for version in super::super::protocols() {
            let mut bytes = postcard::to_stdvec(&version).unwrap();
            bytes.extend_from_slice(&[255, 255, 255, 255, 255]);
            assert_eq!(discovered_version(&bytes).unwrap(), version);
            assert!(postcard::from_bytes::<ServerEnvelope>(&bytes).is_err());
        }
        for version in [0u16, 68, 77, 78, 119, 127, 128, 160, 165, u16::MAX] {
            if version != crate::terminal_ipc::PROTOCOL_VERSION {
                assert!(discovered_version(&postcard::to_stdvec(&version).unwrap()).is_err());
            }
        }
        assert!(discovered_version(&[]).is_err());
        assert!(discovered_version(&[0x80]).is_err());
    }

    #[cfg(feature = "host")]
    #[test]
    fn unqualified_165_refuses_after_discovery_without_preparing_or_writing_state() {
        use crate::terminal_ipc::ClientEnvelope;
        use std::{fs, os::unix::net::UnixListener, process::Command, process::Stdio, thread};
        const ROOT: &str = "CAGENTS_UNQUALIFIED_SOURCE_TEST_ROOT";
        const TEST: &str = "update::source::discovery::tests::unqualified_165_refuses_after_discovery_without_preparing_or_writing_state";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = std::path::PathBuf::from(root);
            let paths = crate::daemon::RuntimePaths {
                directory: root.clone(),
                socket: root.join("cagentsd.sock"),
                lock: root.join("cagentsd.lock"),
            };
            let mut source = proof();
            source.boot_id = crate::daemon::read_boot_id().unwrap();
            let _lease = crate::daemon::HostLease::acquire(&paths, &source).unwrap();
            let listener = UnixListener::bind(&paths.socket).unwrap();
            fs::write(root.join("ready"), []).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let request: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline).unwrap()).unwrap();
            assert!(
                matches!(request.request, ClientRequest::Connect { expected_host: None, ref resume } if resume.is_empty())
            );
            let mut reply = postcard::to_stdvec(&165u16).unwrap();
            reply.extend_from_slice(&[255, 255, 255]);
            write(&mut stream, &reply, deadline).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "discovery must not send preparation"
            );
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock,
                "unqualified source must not receive an authenticated control connection"
            );
            // The parent polls for this file and then reads it: publish the
            // contents whole, so it can never observe a created, empty file.
            fs::write(root.join("observed.partial"), b"Connect only").unwrap();
            fs::rename(root.join("observed.partial"), root.join("observed")).unwrap();
            // Stay alive with the original lease until the parent finishes its
            // read-only assertions and closes this owned child's input pipe.
            assert_eq!(io::stdin().read(&mut [0]).unwrap(), 0);
            return;
        }
        struct OwnedMock(std::process::Child);
        impl Drop for OwnedMock {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        let retained = state.join("retained-checkpoint.json");
        let registry = state.join("workspace.sqlite3");
        fs::write(&retained, b"immutable existing checkpoint evidence").unwrap();
        fs::write(&registry, b"immutable registry evidence").unwrap();
        let snapshot = || {
            let mut files = fs::read_dir(&state)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<Vec<_>>();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        };
        let before = snapshot();
        let mut child = OwnedMock(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(ROOT, root)
                .env("XDG_DATA_HOME", &state)
                .env("XDG_CONFIG_HOME", &state)
                .env("XDG_RUNTIME_DIR", root)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let wait_for = |path: &Path| {
            while !path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "mock source did not reach {}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for(&root.join("ready"));
        let error = super::super::Client::connect(&root.join("cagentsd.sock"))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("uses protocol 165"), "{error}");
        assert!(
            error.contains("--no-start daemon stop --require-empty"),
            "{error}"
        );
        wait_for(&root.join("observed"));
        assert_eq!(fs::read(root.join("observed")).unwrap(), b"Connect only");
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "refusal must keep the source alive"
        );
        assert_eq!(snapshot(), before);
        assert!(!state.join("daemon-upgrade.json").exists());
        drop(child.0.stdin.take());
        assert!(child.0.wait().unwrap().success());
    }

    /// Preserve the historical candidate line's frozen byte oracle. A
    /// recorded layout alone does not qualify it as a live update source.
    #[test]
    fn update_messages_keep_the_candidate_lines_variant_positions() {
        let tag = |bytes: Vec<u8>| bytes[0];
        let requests = [
            (
                ClientRequest::Connect {
                    expected_host: None,
                    resume: Vec::new(),
                },
                0,
            ),
            (ClientRequest::List, 1),
            (ClientRequest::ChatList, 29),
            (
                ClientRequest::UpgradeImpact {
                    policy: impact::Policy::WhenIdle,
                },
                36,
            ),
            (
                ClientRequest::PrepareConfirmedUpgrade {
                    impact: impact(),
                    relaunch_agents: false,
                },
                37,
            ),
        ];
        for (request, expected) in requests {
            assert_eq!(tag(postcard::to_stdvec(&request).unwrap()), expected);
        }
        let responses = [
            (
                ServerResponse::Connected {
                    proof: proof(),
                    resumed: Vec::new(),
                },
                0,
            ),
            (
                ServerResponse::Listed {
                    terminals: Vec::new(),
                },
                1,
            ),
            (
                ServerResponse::ChatListed {
                    sessions: Vec::new(),
                },
                13,
            ),
            (ServerResponse::UpgradeImpact(impact()), 9),
            (
                ServerResponse::UpgradePrepared {
                    terminals: 1,
                    chats: 1,
                },
                17,
            ),
        ];
        for (response, expected) in responses {
            assert_eq!(tag(postcard::to_stdvec(&response).unwrap()), expected);
        }
    }

    /// Every admitted current-wire source is reached through discovery and
    /// authenticated at its own protocol through preview and confirmed prepare.
    #[cfg(feature = "host")]
    #[test]
    fn authenticated_current_wire_sources_preview_and_prepare_at_their_own_protocol() {
        for protocol in super::super::protocols() {
            authenticated_current_wire_source(protocol);
        }
    }

    #[cfg(feature = "host")]
    fn authenticated_current_wire_source(protocol: u16) {
        use crate::terminal_ipc::{ClientEnvelope, ServerMessage};
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("cagentsd.sock");
        let paths = crate::daemon::RuntimePaths {
            directory: directory.path().to_owned(),
            socket: socket.clone(),
            lock: directory.path().join("cagentsd.lock"),
        };
        let mut pinned = proof();
        pinned.boot_id = crate::daemon::read_boot_id().unwrap();
        let expected_proof = pinned.clone();
        let _lease = crate::daemon::HostLease::acquire(&paths, &pinned).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let mut expected = impact();
        expected.capability.proof = pinned.clone();
        expected.policy = impact::Policy::WhenIdle;
        expected.sessions[0].disposition = impact::Disposition::Resumes;
        expected.sessions[0].interrupted_subagents.clear();
        let answer = expected.clone();
        let reviewed = expected.clone();
        let worker = std::thread::spawn(move || {
            let deadline = || Instant::now() + Duration::from_secs(2);
            let (mut discovery, _) = listener.accept().unwrap();
            let request: ClientEnvelope =
                postcard::from_bytes(&read(&mut discovery, deadline()).unwrap()).unwrap();
            assert_eq!(request.version, crate::terminal_ipc::PROTOCOL_VERSION);
            // The source refuses the newer number; only its header is read.
            let mut refusal = postcard::to_stdvec(&protocol).unwrap();
            refusal.extend_from_slice(&[255, 255, 255]);
            write(&mut discovery, &refusal, deadline()).unwrap();
            drop(discovery);
            let (mut stream, _) = listener.accept().unwrap();
            let reply = |stream: &mut UnixStream, request_id: u64, response: ServerResponse| {
                let envelope = ServerEnvelope {
                    version: protocol,
                    request_id: Some(request_id),
                    proof: pinned.clone(),
                    message: ServerMessage::Response { response },
                };
                write(stream, &postcard::to_stdvec(&envelope).unwrap(), deadline()).unwrap();
            };
            let connect: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline()).unwrap()).unwrap();
            assert_eq!(connect.version, protocol);
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            reply(
                &mut stream,
                connect.request_id,
                ServerResponse::Connected {
                    proof: pinned.clone(),
                    resumed: Vec::new(),
                },
            );
            let preview: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline()).unwrap()).unwrap();
            assert_eq!(preview.version, protocol);
            assert_eq!(preview.proof.as_ref(), Some(&pinned));
            assert!(matches!(
                preview.request,
                ClientRequest::UpgradeImpact {
                    policy: impact::Policy::WhenIdle
                }
            ));
            reply(
                &mut stream,
                preview.request_id,
                ServerResponse::UpgradeImpact(answer),
            );
            let prepare: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline()).unwrap()).unwrap();
            assert_eq!(prepare.version, protocol);
            match prepare.request {
                ClientRequest::PrepareConfirmedUpgrade {
                    impact,
                    relaunch_agents,
                } => {
                    assert_eq!(impact, reviewed);
                    assert!(!relaunch_agents);
                }
                _ => panic!("unexpected preparation request"),
            }
            reply(
                &mut stream,
                prepare.request_id,
                ServerResponse::UpgradePrepared {
                    terminals: 1,
                    chats: 1,
                },
            );
        });
        let mut client = super::super::Client::connect(&socket).unwrap();
        assert_eq!(client.protocol(), protocol);
        assert_eq!(client.proof(), &expected_proof);
        let layout = |version: &str| crate::update::InstallLayout {
            prefix: "/owned/install".into(),
            version_root: Path::new("/owned/install/versions").join(version),
            version: version.into(),
        };
        let current = layout("new");
        let previous = layout("old");
        assert_eq!(
            super::super::super::installed_candidate_version(
                &current,
                &current.version_root,
                Some(&previous),
                client.protocol(),
                Some(client.proof()),
            )
            .unwrap(),
            "new"
        );
        let observed = client.upgrade_impact(impact::Policy::WhenIdle).unwrap();
        assert_eq!(observed, expected);
        assert_eq!(
            client.prepare_confirmed_upgrade(observed, false).unwrap(),
            (1, 1)
        );
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn source_deadline_covers_trickled_prefix_and_payload_without_resetting() {
        for prefix_trickle in [true, false] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let deadline = Instant::now() + Duration::from_millis(55);
            let worker = std::thread::spawn(move || {
                let frame = if prefix_trickle {
                    [4u32.to_be_bytes().as_slice(), b"body"].concat()
                } else {
                    server.write_all(&4u32.to_be_bytes()).unwrap();
                    b"body".to_vec()
                };
                for byte in frame {
                    if server.write_all(&[byte]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(35));
                }
            });
            let result = read(&mut client, deadline);
            assert!(
                result.is_err(),
                "partial frame progress must not reset the absolute deadline"
            );
            drop(client);
            worker.join().unwrap();
        }
    }
}
