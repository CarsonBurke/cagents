//! Read-only inspection of the stable 99/100/101/current inventory subset.
//! The frozen protocol-77 reader remains separate from this matched connection.
use crate::{
    chat::Session,
    model::HostSessionProof,
    terminal::ipc::{
        self, ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
    },
};
use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 4 * 1024 * 1024;

pub(super) struct Client {
    stream: UnixStream,
    proof: HostSessionProof,
    next: u64,
    protocol: u16,
    deadline: Instant,
}

impl Client {
    pub(super) fn connect(
        socket: &Path,
        expected: &HostSessionProof,
        deadline: Instant,
        protocol: u16,
    ) -> Result<Self, String> {
        if !supports_protocol(protocol) {
            return Err("Source protocol has no qualified migration inspector".into());
        }
        if !expected.is_complete() {
            return Err("Source service has no complete ownership proof".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Source migration inspection timed out".into());
        }
        let stream =
            crate::control::connect_control_socket(socket, remaining.min(Duration::from_secs(5)))
                .map_err(|_| "Cannot connect to the source service")?;
        let mut client = Self {
            stream,
            proof: expected.clone(),
            next: 1,
            protocol,
            deadline,
        };
        client.verify_owner(socket)?;
        match client.request(
            ClientRequest::Connect {
                expected_host: Some(expected.clone()),
                resume: Vec::new(),
            },
            false,
        )? {
            ServerResponse::Connected { proof, resumed }
                if proof == *expected && resumed.is_empty() =>
            {
                client.verify_owner(socket)?;
                Ok(client)
            }
            _ => Err("Source handshake did not confirm the expected owner".into()),
        }
    }

    fn verify_owner(&self, socket: &Path) -> Result<(), String> {
        let pid = crate::daemon::lease_owner(&socket.with_file_name("cagentsd.lock"), &self.proof)
            .map_err(|_| "Source service ownership cannot be verified")?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: initialized buffers have the exact kernel credential layout.
        if unsafe {
            libc::getsockopt(
                self.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of::<libc::ucred>()
            || peer.uid != crate::daemon::current_uid()
            || u32::try_from(peer.pid).ok() != Some(pid)
        {
            return Err("Source connection does not belong to its authenticated service".into());
        }
        Ok(())
    }

    fn request(
        &mut self,
        request: ClientRequest,
        authenticated: bool,
    ) -> Result<ServerResponse, String> {
        if !matches!(
            request,
            ClientRequest::Connect { .. }
                | ClientRequest::List
                | ClientRequest::ChatList
                | ClientRequest::ResolveUpgradeChat { .. }
                | ClientRequest::ChatDetach { .. }
                | ClientRequest::ChatAttach { .. }
                | ClientRequest::ChatVisibility { .. }
                | ClientRequest::ChatRead { .. }
                | ClientRequest::ChatReleaseInspect { .. }
                | ClientRequest::ChatRelease { .. }
        ) && !matches!(&request, ClientRequest::ChatStart { session, source: None, terminate_source: false, execution_environment }
            if self.protocol == 119 && session.native_continuity.retains_provider() && execution_environment.0.is_empty())
        {
            return Err("Request is outside the retained-source inspection contract".into());
        }
        let bootstrap_chat = match &request {
            ClientRequest::ChatStart { session, .. } if self.protocol == 119 => Some(session.id),
            ClientRequest::ChatAttach { session } => Some(session.id),
            ClientRequest::ChatDetach { chat_id }
            | ClientRequest::ChatVisibility { chat_id, .. }
            | ClientRequest::ChatRead { chat_id, .. }
            | ClientRequest::ChatReleaseInspect { chat_id } => Some(*chat_id),
            ClientRequest::ChatRelease { proof, .. } => Some(proof.session.id),
            _ => None,
        };
        let request_id = self.next;
        self.next += 1;
        let mut stream = DeadlineStream {
            stream: &mut self.stream,
            deadline: self.deadline,
        };
        ipc::write_frame(
            &mut stream,
            &ClientEnvelope {
                version: self.protocol,
                request_id,
                proof: authenticated.then(|| self.proof.clone()),
                request,
            },
        )
        .map_err(|_| "Cannot send source inspection")?;
        // Legacy ChatStart installs its hidden subscriber before responding.
        // Activity can arrive first, including during Detach. Drain only that
        // exact chat's qualified events under the same absolute deadline.
        for _ in 0..256 {
            let envelope = if bootstrap_chat.is_some() {
                let payload = read_payload_with_limit(&mut stream, ipc::MAX_FRAME_BYTES)?;
                postcard::from_bytes::<ServerEnvelope>(&payload)
                    .map_err(|_| "Unsupported legacy retained-owner response")?
            } else {
                read_response(&mut stream)?
            };
            if envelope.version != self.protocol || envelope.proof != self.proof {
                return Err("Source inspection ownership or protocol changed".into());
            }
            if envelope.request_id == Some(request_id) {
                return match envelope.message {
                    ServerMessage::Response { response } => Ok(response),
                    _ => Err("Source response is outside the inspection contract".into()),
                };
            }
            let event_chat = match envelope.message {
                ServerMessage::ChatActivity { chat_id, .. }
                | ServerMessage::ChatObserved { chat_id, .. }
                | ServerMessage::ChatChanged { chat_id }
                | ServerMessage::ChatAccountUsage { chat_id, .. } => Some(chat_id),
                _ => None,
            };
            if envelope.request_id.is_some()
                || bootstrap_chat.is_none()
                || event_chat != bootstrap_chat
            {
                return Err("Source inspection request or event identity changed".into());
            }
        }
        Err("Retained-owner events exceeded the bounded inspection budget".into())
    }

    pub(super) fn list(&mut self) -> Result<Vec<Session>, String> {
        match self.request(ClientRequest::ChatList, true)? {
            ServerResponse::ChatListed { sessions }
                if sessions.len() <= crate::upgrade::handoff::MAX_CHATS =>
            {
                Ok(sessions)
            }
            _ => Err("Source chat inventory is unsupported or exceeds the inspection limit".into()),
        }
    }

    pub(super) fn terminals(&mut self) -> Result<Vec<super::v77::Terminal>, String> {
        match self.request(ClientRequest::List, true)? {
            ServerResponse::Listed { terminals } if terminals.len() <= 1024 => {
                Ok(terminals.into_iter().map(Into::into).collect())
            }
            _ => Err(
                "Source terminal inventory is unsupported or exceeds the inspection limit".into(),
            ),
        }
    }

    /// Bootstrap an installed119 service using its existing retained-only start
    /// path. The type fences into authenticated adoption even if its owner
    /// disappears. ReconnectOnly must never reach generic ChatStart here.
    pub(super) fn reconcile_retained(&mut self, source: &Session) -> Result<(), String> {
        if self.protocol != 119 {
            return Ok(()); // newer Resolve performs no-launch adoption itself
        }
        if !source.native_continuity.retains_provider() || !source.has_valid_native_continuity() {
            return Err(
                "Legacy source needs an authenticated retained owner; no provider was launched"
                    .into(),
            );
        }
        let response = self.request(
            ClientRequest::ChatStart {
                session: source.clone(),
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
            true,
        )?;
        let ServerResponse::ChatAttached { session, snapshot } = response else {
            return Err("Legacy retained owner did not return its exact attachment".into());
        };
        let valid = (session == *source
            || crate::chat::is_upgrade_recovery(source, &session)
            || crate::chat::is_upgrade_identity_learning(source, &session))
            && snapshot.ready
            && snapshot.session_id == session.launch.session_id
            && snapshot.native_continuity == session.native_continuity
            && session.proof.as_ref() == Some(&self.proof);
        if !matches!(
            self.request(ClientRequest::ChatDetach { chat_id: source.id }, true)?,
            ServerResponse::Detached
        ) {
            return Err("Cannot detach the retained-owner inspection".into());
        }
        if !valid {
            return Err(
                "Legacy retained owner changed its exact conversation or is not ready".into(),
            );
        }
        Ok(())
    }

    pub(super) fn resolve(&mut self, source: &Session) -> Result<Session, String> {
        match self.request(
            ClientRequest::ResolveUpgradeChat {
                source: source.clone(),
            },
            true,
        )? {
            ServerResponse::UpgradeChatResolved { session } => Ok(session),
            _ => Err("Source did not authenticate a saved conversation's lineage".into()),
        }
    }

    /// Use only the existing forced-history Attach API. The request's current
    /// proof addresses that read-only API; it is never persisted as a recovered
    /// descriptor or presented as evidence of an old provider's execution.
    /// Both the host's owner fences and exact release/coverage checks remain.
    pub(super) fn inspect_saved_history(&mut self, source: &Session) -> Result<(), String> {
        if ![119, ipc::PROTOCOL_VERSION].contains(&self.protocol)
            || !source.is_saved_claude_history()
        {
            return Err("Saved history has no qualified read-only inspection API".into());
        }
        let mut requested = source.clone();
        requested.proof = Some(self.proof.clone());
        let response = self.request(
            ClientRequest::ChatAttach {
                session: requested.clone(),
            },
            true,
        )?;
        let inspection = (|| {
            let ServerResponse::ChatAttached {
                session,
                mut snapshot,
            } = response
            else {
                return Err("Saved history did not return its exact attachment".into());
            };
            if session != requested {
                return Err(
                    "Saved history changed its exact account, recipe or conversation".into(),
                );
            }
            if !matches!(
                self.request(
                    ClientRequest::ChatVisibility {
                        chat_id: source.id,
                        visible: true
                    },
                    true
                )?,
                ServerResponse::Accepted
            ) {
                return Err("Saved history could not be inspected".into());
            }
            for _ in 0..64 {
                if snapshot.exited
                    && snapshot.read_only_reason.is_some()
                    && !snapshot.history_loading
                {
                    if snapshot.session_id == source.launch.session_id
                        && matches!(
                            snapshot.native_continuity,
                            crate::chat::NativeContinuity::ReconnectOnly
                        )
                        && !snapshot.busy
                        && !snapshot.waiting
                        && snapshot.requests.is_empty()
                        && snapshot.error.is_none()
                    {
                        return Ok(());
                    }
                    return Err(
                        "Saved history did not confirm an exact settled read-only conversation"
                            .into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
                match self.request(
                    ClientRequest::ChatRead {
                        chat_id: source.id,
                        after_generation: None,
                    },
                    true,
                )? {
                    ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(current),
                    } if chat_id == source.id => snapshot = current,
                    _ => return Err("Saved history did not publish its exact transcript".into()),
                }
            }
            Err("Saved history exceeded the bounded readiness budget".into())
        })();
        let detached = self.request(ClientRequest::ChatDetach { chat_id: source.id }, true)?;
        if !matches!(detached, ServerResponse::Detached) {
            return Err("Saved history inspection could not detach its reader".into());
        }
        inspection?;
        let ServerResponse::ChatReleaseInspected { inspection } = self.request(
            ClientRequest::ChatReleaseInspect { chat_id: source.id },
            true,
        )?
        else {
            return Err("Saved history did not return its release inspection".into());
        };
        if inspection.proof.session != requested || inspection.release_blocked.is_some() {
            return Err("Saved history still owns protected state; its reader was retained".into());
        }
        match self.request(
            ClientRequest::ChatRelease {
                proof: inspection.proof.clone(),
                dry_run: false,
            },
            true,
        )? {
            ServerResponse::ChatReleased {
                inspection: released,
                dry_run: false,
            } if released == inspection => Ok(()),
            _ => Err("Saved history release changed its exact witness".into()),
        }
    }
}

// These versions have identical Connect/List/ChatList/ResolveUpgradeChat tags
// and inventory DTOs. Version 100 appends capacity operations; 101 requires
// atomic managed-pane adoption without changing these DTOs. This restricted
// Inventory uses the frozen restricted decoder. The separate retained/history
// methods admit full snapshots only for the explicitly qualified119/current pair.
// Versions 102 (Copy sidecars), 103 (retained Claude continuity) and 104 (the
// appended Work board request) preserve this subset, including the existing
// continuity enum discriminants.
pub(super) fn supports_protocol(protocol: u16) -> bool {
    [99, 100, 101, 102, 103, 119, ipc::PROTOCOL_VERSION].contains(&protocol)
}

fn read_payload_with_limit(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut prefix = [0; 4];
    reader
        .read_exact(&mut prefix)
        .map_err(|_| "Truncated source response")?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > limit {
        return Err("Source response exceeds the inspection limit".into());
    }
    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .map_err(|_| "Truncated source response")?;
    Ok(payload)
}

fn read_response(reader: &mut impl Read) -> Result<ServerEnvelope, String> {
    let payload = read_payload_with_limit(reader, MAX_FRAME)?;
    let (envelope, remaining) = postcard::take_from_bytes::<InspectionEnvelope>(&payload)
        .map_err(|_| "Unsupported source response")?;
    if !remaining.is_empty() {
        return Err("Source response has an unknown layout".into());
    }
    let InspectionMessage::Response { response } = envelope.message;
    Ok(ServerEnvelope {
        version: envelope.version,
        request_id: envelope.request_id,
        proof: envelope.proof,
        message: ServerMessage::Response {
            response: response.0,
        },
    })
}

// Postcard encodes a one-field Response struct variant as its tag followed by
// that field. Accept only that message and the four frozen inventory replies.
#[derive(serde::Deserialize)]
struct InspectionEnvelope {
    version: u16,
    request_id: Option<u64>,
    proof: HostSessionProof,
    message: InspectionMessage,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum InspectionMessage {
    Response { response: InspectionResponse },
}
struct InspectionResponse(ServerResponse);
impl<'de> serde::Deserialize<'de> for InspectionResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = InspectionResponse;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a qualified source inventory response")
            }
            fn visit_enum<A: serde::de::EnumAccess<'de>>(
                self,
                data: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::VariantAccess;
                let (tag, variant) = data.variant::<u32>()?;
                #[derive(serde::Deserialize)]
                struct Connected {
                    proof: HostSessionProof,
                    resumed: Vec<crate::terminal_ipc::TerminalDescriptor>,
                }
                #[derive(serde::Deserialize)]
                struct Listed {
                    terminals: Vec<crate::terminal_ipc::TerminalDescriptor>,
                }
                #[derive(serde::Deserialize)]
                struct ChatListed {
                    sessions: Vec<Session>,
                }
                #[derive(serde::Deserialize)]
                struct Resolved {
                    session: Session,
                }
                #[derive(serde::Deserialize)]
                struct Attached {
                    session: Session,
                    snapshot: crate::chat::Snapshot,
                }
                let response = match tag {
                    4 => {
                        let _: () = variant.newtype_variant()?;
                        ServerResponse::Detached
                    }
                    11 => {
                        let value: Attached = variant.newtype_variant()?;
                        ServerResponse::ChatAttached {
                            session: value.session,
                            snapshot: value.snapshot,
                        }
                    }
                    0 => {
                        let value: Connected = variant.newtype_variant()?;
                        ServerResponse::Connected {
                            proof: value.proof,
                            resumed: value.resumed,
                        }
                    }
                    1 => {
                        let value: Listed = variant.newtype_variant()?;
                        ServerResponse::Listed {
                            terminals: value.terminals,
                        }
                    }
                    13 => {
                        let value: ChatListed = variant.newtype_variant()?;
                        ServerResponse::ChatListed {
                            sessions: value.sessions,
                        }
                    }
                    18 => {
                        let value: Resolved = variant.newtype_variant()?;
                        ServerResponse::UpgradeChatResolved {
                            session: value.session,
                        }
                    }
                    _ => {
                        return Err(serde::de::Error::custom(
                            "response is outside the read-only inspection contract",
                        ));
                    }
                };
                Ok(InspectionResponse(response))
            }
        }
        deserializer.deserialize_enum("InspectionResponse", &[], Visitor)
    }
}

struct DeadlineStream<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl Read for DeadlineStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_protocols() -> std::collections::BTreeSet<u16> {
        [99, 100, 101, 102, 103, 119, ipc::PROTOCOL_VERSION].into()
    }

    fn source_frame_version(frame: &[u8], protocol: u16) -> Vec<u8> {
        // The captured source frame starts with a four-byte length and the
        // one-byte postcard version 100. Reframe only that version scalar;
        // the historical inventory DTO bytes remain frozen.
        assert_eq!(frame[4], 100);
        let mut payload = postcard::to_allocvec(&protocol).unwrap();
        payload.extend_from_slice(&frame[5..]);
        let mut result = (payload.len() as u32).to_be_bytes().to_vec();
        result.extend(payload);
        result
    }

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("private-source"),
            boot_id: crate::model::BootId::new("private-boot"),
            host_incarnation: crate::model::HostIncarnation::new("private-owner"),
        }
    }

    fn populated_inventory() -> (Session, ipc::TerminalDescriptor) {
        use crate::model::{
            TerminalContinuity, TerminalId, TerminalOutputActivity, TranscriptLocationReceipt,
            TranscriptMetadata,
        };
        let id = TerminalId::from_uuid(uuid::Uuid::from_u128(0x11111111222243338444555555555555));
        let previous =
            TerminalId::from_uuid(uuid::Uuid::from_u128(0x66666666777748889999aaaaaaaaaaaa));
        let owner = proof();
        let listener = crate::chat::OwnedCodexListener {
            socket: format!("/fixture/cagents-codex-{id}/control.sock").into(),
            allocator: owner.clone(),
        };
        let mut source = super::super::tests::session();
        source.id = id;
        source.proof = Some(owner.clone());
        source.launch.provider = crate::agent::Provider::Codex;
        source.launch.recipe.program = "codex".into();
        source.launch.recipe.arguments = vec![
            "--model".into(),
            "fixture".into(),
            "--remote".into(),
            format!("unix://{}", listener.socket.display()),
        ];
        source.launch.recipe.working_directory = "/fixture/project".into();
        source.launch.recipe.environment = [("FIXTURE_HOME".into(), "/fixture/home".into())].into();
        source.launch.recipe.unset_environment = ["FIXTURE_UNSET".into()].into();
        source.owned_codex_listener = Some(listener.clone());
        source.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "0.9.1".into(),
        };
        let mut terminal = ipc::TerminalDescriptor::relaunchable(
            id,
            source.launch.recipe.clone(),
            Some("/fixture/transcript".into()),
        );
        terminal.session.owned_codex_listener = Some(listener);
        terminal.session.recovery_origin = Some(previous);
        terminal.session.predecessor = Some(previous);
        terminal.session.resume_hint = Some("fixture-resume-hint".into());
        terminal.session.continuity = TerminalContinuity::Live {
            host_session_id: owner.host_session_id.clone(),
            boot_id: owner.boot_id.clone(),
            host_incarnation: owner.host_incarnation.clone(),
        };
        terminal.screen_lost_after_crash = true;
        terminal.generation = 257;
        terminal.event_sequence = 513;
        terminal.oldest_event_sequence = 129;
        terminal.attached = true;
        terminal.transcript = Some(TranscriptMetadata {
            path: "/fixture/transcript".into(),
            byte_len: 1025,
            durable_offset: 1024,
            updated_at_unix_ms: 1_234_567,
            location: Some(TranscriptLocationReceipt {
                root_device: 1,
                root_inode: 2,
                file_device: 3,
                file_inode: 4,
            }),
        });
        terminal.activity = Some(TerminalOutputActivity {
            sequence: 513,
            last_output_at_unix_ms: 1_234_568,
        });
        terminal.agent_activity = Some(crate::agent::Activity {
            provider: crate::agent::Provider::Codex,
            state: crate::agent::ActivityState::Waiting,
        });
        let mut former = terminal.session.clone();
        former.id = previous;
        let former_socket = format!("/fixture/cagents-codex-{previous}/control.sock");
        former.owned_codex_listener.as_mut().unwrap().socket = former_socket.clone().into();
        former.launch.arguments[3] = format!("unix://{former_socket}");
        terminal.upgrade_sources = vec![former.clone()];
        terminal.upgrade_relaunches = vec![ipc::UpgradeRelaunch {
            source: former,
            recipe: source.launch.recipe.clone(),
        }];
        terminal.title = Some("fixture native title 🦀".into());
        source.terminal = Some(terminal.session.clone());
        assert!(source.has_valid_native_continuity());
        assert!(terminal.session.owns_codex_listener());
        (source, terminal)
    }

    // Frozen frames detect historical layout changes which a current-DTO
    // roundtrip would accept on both sides.
    #[test]
    fn frozen_populated_source_100_inventory_wire_fixtures() {
        let (source, terminal) = populated_inventory();
        let connect = ClientEnvelope {
            version: 100,
            request_id: 7,
            proof: None,
            request: ClientRequest::Connect {
                expected_host: Some(proof()),
                resume: vec![ipc::ResumeCursor {
                    terminal_id: source.id,
                    generation: 257,
                    event_sequence: 513,
                }],
            },
        };
        let mut bytes = Vec::new();
        ipc::write_frame(&mut bytes, &connect).unwrap();
        assert_eq!(
            bytes.as_slice(),
            include_bytes!("../../../tests/fixtures/upgrade-inspection-v100/connect.bin")
        );
        for protocol in source_protocols() {
            let mut request = connect.clone();
            request.version = protocol;
            let mut current = Vec::new();
            ipc::write_frame(&mut current, &request).unwrap();
            assert_eq!(current, source_frame_version(&bytes, protocol));
        }
        for (name, response) in [
            (
                "CONNECTED",
                ServerResponse::Connected {
                    proof: proof(),
                    resumed: vec![terminal.clone()],
                },
            ),
            (
                "LISTED",
                ServerResponse::Listed {
                    terminals: vec![terminal],
                },
            ),
            (
                "CHAT_LISTED",
                ServerResponse::ChatListed {
                    sessions: vec![source.clone()],
                },
            ),
            (
                "RESOLVED",
                ServerResponse::UpgradeChatResolved { session: source },
            ),
        ] {
            let envelope = ServerEnvelope {
                version: 100,
                request_id: Some(7),
                proof: proof(),
                message: ServerMessage::Response { response },
            };
            let mut bytes = Vec::new();
            ipc::write_frame(&mut bytes, &envelope).unwrap();
            let frozen: &[u8] = match name {
                "CONNECTED" => {
                    include_bytes!("../../../tests/fixtures/upgrade-inspection-v100/connected.bin")
                }
                "LISTED" => {
                    include_bytes!("../../../tests/fixtures/upgrade-inspection-v100/listed.bin")
                }
                "CHAT_LISTED" => include_bytes!(
                    "../../../tests/fixtures/upgrade-inspection-v100/chat_listed.bin"
                ),
                "RESOLVED" => {
                    include_bytes!("../../../tests/fixtures/upgrade-inspection-v100/resolved.bin")
                }
                _ => unreachable!(),
            };
            assert_eq!(bytes.as_slice(), frozen);
            assert_eq!(read_response(&mut &frozen[..]).unwrap(), envelope);
            for protocol in source_protocols() {
                let historical = source_frame_version(frozen, protocol);
                let mut expected = envelope.clone();
                expected.version = protocol;
                let mut current = Vec::new();
                ipc::write_frame(&mut current, &expected).unwrap();
                assert_eq!(current, historical);
                assert_eq!(read_response(&mut historical.as_slice()).unwrap(), expected);
            }
        }
    }

    #[test]
    fn frozen_inventory_subset_accepts_99_through_104_only() {
        assert!(!supports_protocol(98));
        assert!(!supports_protocol(ipc::PROTOCOL_VERSION + 1));
        assert_eq!(postcard::to_allocvec(&ClientRequest::List).unwrap(), [1]);
        assert_eq!(
            postcard::to_allocvec(&ClientRequest::ChatList).unwrap(),
            [29]
        );
        assert_eq!(
            postcard::to_allocvec(&ClientRequest::Connect {
                expected_host: None,
                resume: vec![]
            })
            .unwrap(),
            [0, 0, 0]
        );
        let source = super::super::tests::session();
        assert_eq!(
            postcard::to_allocvec(&ClientRequest::ResolveUpgradeChat {
                source: source.clone()
            })
            .unwrap()[0],
            38
        );
        for protocol in source_protocols() {
            assert!(supports_protocol(protocol));
            // Frozen postcard envelope with request 7, proof s/b/i and an empty
            // ChatListed response. The only version-specific byte is first.
            let payload = [protocol as u8, 1, 7, 1, b's', 1, b'b', 1, b'i', 0, 13, 0];
            let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
            frame.extend(payload);
            let envelope = read_response(&mut frame.as_slice()).unwrap();
            assert_eq!(envelope.version, protocol);
            assert_eq!(envelope.request_id, Some(7));
            assert_eq!(
                envelope.proof.host_session_id,
                crate::model::HostSessionId::new("s")
            );
            assert!(
                matches!(envelope.message, ServerMessage::Response { response: ServerResponse::ChatListed { sessions } } if sessions.is_empty())
            );
            let terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
                source.id,
                source.launch.recipe.clone(),
                None,
            );
            for response in [
                ServerResponse::Connected {
                    proof: proof(),
                    resumed: vec![],
                },
                ServerResponse::Listed {
                    terminals: vec![terminal],
                },
                ServerResponse::ChatListed {
                    sessions: vec![source.clone()],
                },
                ServerResponse::UpgradeChatResolved {
                    session: source.clone(),
                },
            ] {
                let envelope = ServerEnvelope {
                    version: protocol,
                    request_id: Some(7),
                    proof: proof(),
                    message: ServerMessage::Response { response },
                };
                let mut frame = Vec::new();
                ipc::write_frame(&mut frame, &envelope).unwrap();
                assert_eq!(read_response(&mut frame.as_slice()).unwrap(), envelope);
            }
        }
    }

    #[test]
    fn legacy_retained_reconciliation_drains_exact_events_and_accepts_large_history() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut source = super::super::tests::session();
        let mut old = proof();
        old.host_incarnation = crate::model::HostIncarnation::new("previous-owner");
        source.proof = Some(old);
        source.native_continuity =
            crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        assert!(source.has_valid_native_continuity());
        let expected = source.clone();
        let worker = std::thread::spawn(move || {
            for attach in [true, false] {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(request.version, 119);
                assert_eq!(request.proof.as_ref(), Some(&proof()));
                if attach {
                    assert!(
                        matches!(request.request, ClientRequest::ChatStart { session, source: None, terminate_source: false, execution_environment } if session == expected && execution_environment.0.is_empty())
                    );
                } else {
                    assert!(
                        matches!(request.request, ClientRequest::ChatDetach { chat_id } if chat_id == expected.id)
                    );
                }
                for message in [
                    ServerMessage::ChatActivity {
                        chat_id: expected.id,
                        state: crate::agent::ActivityState::Working,
                        generation: 42,
                    },
                    ServerMessage::ChatObserved {
                        chat_id: expected.id,
                        epoch: 3,
                    },
                ] {
                    ipc::write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: 119,
                            request_id: None,
                            proof: proof(),
                            message,
                        },
                    )
                    .unwrap();
                }
                let response = if attach {
                    let mut resolved = expected.clone();
                    resolved.proof = Some(proof());
                    let mut snapshot = crate::chat::Snapshot {
                        ready: true,
                        session_id: resolved.launch.session_id.clone(),
                        native_continuity: resolved.native_continuity.clone(),
                        ..Default::default()
                    };
                    snapshot.items.push(std::sync::Arc::new(crate::chat::Item {
                        id: "retained-history".into(),
                        role: crate::chat::Role::Assistant,
                        content: crate::chat::ItemContent::Text {
                            text: "x".repeat(5 * 1024 * 1024),
                            thinking: false,
                            phase: Default::default(),
                        },
                        started_at: None,
                        at: None,
                    }));
                    ServerResponse::ChatAttached {
                        session: resolved,
                        snapshot,
                    }
                } else {
                    ServerResponse::Detached
                };
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: 119,
                        request_id: Some(request.request_id),
                        proof: proof(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            }
        });
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            protocol: 119,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        client.reconcile_retained(&source).unwrap();
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn legacy_reconnect_only_source_never_sends_a_start_request() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let source = super::super::tests::session();
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            protocol: 119,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(client.reconcile_retained(&source).is_err());
        assert!(
            matches!(peer.read(&mut [0]), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn legacy_history_preview_only_attaches_reads_detaches_and_releases_actual_witness() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let source = super::super::tests::session();
        let mut requested = source.clone();
        requested.proof = Some(proof());
        let expected = requested.clone();
        let worker = std::thread::spawn(move || {
            let witness = crate::terminal_ipc::ChatReleaseInspection {
                proof: crate::terminal_ipc::ChatReleaseProof {
                    session: expected.clone(),
                    execution_instance: uuid::Uuid::new_v4(),
                    generation: 7,
                },
                release_blocked: None,
            };
            for step in 0..5 {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(request.version, 119);
                assert_eq!(request.proof, Some(proof()));
                let response = match (step, request.request) {
                    (0, ClientRequest::ChatAttach { session }) if session == expected => ServerResponse::ChatAttached {
                        session,
                        snapshot: crate::chat::Snapshot {
                            session_id: expected.launch.session_id.clone(), exited: true,
                            read_only_reason: Some("Saved conversation history is read-only; no provider was started.".into()),
                            ..Default::default()
                        },
                    },
                    (1, ClientRequest::ChatVisibility { chat_id, visible: true }) if chat_id == expected.id => ServerResponse::Accepted,
                    (2, ClientRequest::ChatDetach { chat_id }) if chat_id == expected.id => ServerResponse::Detached,
                    (3, ClientRequest::ChatReleaseInspect { chat_id }) if chat_id == expected.id => ServerResponse::ChatReleaseInspected { inspection: witness.clone() },
                    (4, ClientRequest::ChatRelease { proof, dry_run: false }) if proof == witness.proof => ServerResponse::ChatReleased { inspection: witness.clone(), dry_run: false },
                    other => panic!("Unexpected history request (no Start or Close permitted): {other:?}"),
                };
                // Only exact target-chat events may interleave with the reply.
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: 119,
                        request_id: None,
                        proof: proof(),
                        message: ServerMessage::ChatChanged {
                            chat_id: expected.id,
                        },
                    },
                )
                .unwrap();
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: 119,
                        request_id: Some(request.request_id),
                        proof: proof(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            }
        });
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            protocol: 119,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        client.inspect_saved_history(&source).unwrap();
        assert_ne!(source, requested, "original saved proof remains unchanged");
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn inventory_subset_refuses_unexpected_variants_and_inexact_frames() {
        let payload = [100, 1, 7, 1, b's', 1, b'b', 1, b'i', 0, 13, 0];
        for mutation in 0..5 {
            let mut body = payload.to_vec();
            match mutation {
                0 => body.push(0), // A new trailing DTO field is not compatible.
                1 => body[10] = 5, // Accepted is outside the inventory subset.
                2 => body[9] = 1,  // Redraw must not be decoded as a response.
                3 => {
                    body.pop();
                }
                _ => body.clear(),
            }
            let mut frame = (body.len() as u32).to_be_bytes().to_vec();
            frame.extend(body);
            assert!(
                read_response(&mut frame.as_slice()).is_err(),
                "mutation {mutation}"
            );
        }
        assert!(read_response(&mut ((MAX_FRAME as u32 + 1).to_be_bytes()).as_slice()).is_err());
        assert!(read_response(&mut [0, 0].as_slice()).is_err());
    }

    #[test]
    fn source_inventory_accepts_129_and_capacity_ceiling_but_refuses_excess() {
        for count in [
            129,
            crate::upgrade::handoff::MAX_CHATS,
            crate::upgrade::handoff::MAX_CHATS + 1,
        ] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let source = populated_inventory().0;
            let worker = std::thread::spawn(move || {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(request.request, ClientRequest::ChatList));
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: ipc::PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof(),
                        message: ServerMessage::Response {
                            response: ServerResponse::ChatListed {
                                sessions: vec![source; count],
                            },
                        },
                    },
                )
                .unwrap();
            });
            let mut client = Client {
                stream,
                proof: proof(),
                next: 1,
                protocol: ipc::PROTOCOL_VERSION,
                deadline: Instant::now() + Duration::from_secs(10),
            };
            let result = client.list();
            worker.join().unwrap();
            if count <= crate::upgrade::handoff::MAX_CHATS {
                assert_eq!(result.unwrap().len(), count);
            } else {
                assert!(result.unwrap_err().contains("inspection limit"));
            }
        }
    }

    #[test]
    fn compatible_source_request_uses_its_own_version() {
        for protocol in source_protocols() {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let worker = std::thread::spawn(move || {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(request.version, protocol);
                assert!(matches!(request.request, ClientRequest::ChatList));
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: protocol,
                        request_id: Some(request.request_id),
                        proof: proof(),
                        message: ServerMessage::Response {
                            response: ServerResponse::ChatListed { sessions: vec![] },
                        },
                    },
                )
                .unwrap();
            });
            let mut client = Client {
                stream,
                proof: proof(),
                next: 1,
                protocol,
                deadline: Instant::now() + Duration::from_secs(2),
            };
            assert!(client.list().unwrap().is_empty());
            drop(client);
            worker.join().unwrap();
        }
    }

    #[test]
    fn current_response_refuses_changed_protocol_owner_and_request_identity() {
        for mismatch in 0..3 {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let worker = std::thread::spawn(move || {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(request.request, ClientRequest::ChatList));
                let mut envelope = ServerEnvelope {
                    version: ipc::PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof(),
                    message: ServerMessage::Response {
                        response: ServerResponse::ChatListed {
                            sessions: Vec::new(),
                        },
                    },
                };
                match mismatch {
                    0 => envelope.version = 77,
                    1 => envelope.request_id = Some(request.request_id + 1),
                    _ => {
                        envelope.proof.host_incarnation =
                            crate::model::HostIncarnation::new("foreign-owner")
                    }
                }
                ipc::write_frame(&mut peer, &envelope).unwrap();
            });
            let mut client = Client {
                stream,
                proof: proof(),
                next: 1,
                protocol: ipc::PROTOCOL_VERSION,
                deadline: Instant::now() + Duration::from_secs(2),
            };
            let result = client.list();
            drop(client);
            let joined = worker.join();
            joined.unwrap();
            assert!(result.unwrap_err().contains("identity changed"));
        }
    }

    #[test]
    fn expired_current_inspection_sends_no_request() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            protocol: ipc::PROTOCOL_VERSION,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        let refused = client.list().is_err();
        let observed = peer.read(&mut [0u8; 1]).unwrap_err().kind();
        assert!(refused);
        assert_eq!(observed, io::ErrorKind::WouldBlock);
    }
}
