//! Read-only inspection of the wire-proven source inventory subset.
use crate::{
    chat::Session,
    model::{HostSessionProof, TerminalId},
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
/// How long a saved-history reader may take to apply its saved frame once
/// inspection shows it. Readiness is the reader's own change announcement;
/// this only fails a wedged reader, leaving the rest of inspection its time.
const HISTORY_READER_DEADLINE: Duration = Duration::from_secs(5);

pub(in crate::host) struct Client {
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
        if !crate::update::source::is_qualified_source(protocol) {
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
        self.exchange(request, authenticated)
            .map(|(response, _)| response)
    }

    /// Send one request and return its response, and whether the source
    /// announced a presentation change for the addressed chat meanwhile.
    fn exchange(
        &mut self,
        request: ClientRequest,
        authenticated: bool,
    ) -> Result<(ServerResponse, bool), String> {
        if !matches!(
            request,
            ClientRequest::Connect { .. }
                | ClientRequest::List
                | ClientRequest::ListPage { .. }
                | ClientRequest::ChatList
                | ClientRequest::ResolveUpgradeChat { .. }
                | ClientRequest::ChatDetach { .. }
                | ClientRequest::ChatAttach { .. }
                | ClientRequest::ChatVisibility { .. }
                | ClientRequest::ChatRead { .. }
        ) {
            return Err("Request is outside the retained-source inspection contract".into());
        }
        let bootstrap_chat = match &request {
            ClientRequest::ChatAttach { session } => Some(session.id),
            ClientRequest::ChatDetach { chat_id }
            | ClientRequest::ChatVisibility { chat_id, .. }
            | ClientRequest::ChatRead { chat_id, .. } => Some(*chat_id),
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
        let mut changed = false;
        for _ in 0..256 {
            let envelope = if bootstrap_chat.is_some() {
                read_chat_envelope(self.protocol, &mut stream)?
            } else {
                read_response(self.protocol, &mut stream)?
            };
            if envelope.version != self.protocol || envelope.proof != self.proof {
                return Err("Source inspection ownership or protocol changed".into());
            }
            if envelope.request_id == Some(request_id) {
                return match envelope.message {
                    ServerMessage::Response { response } => Ok((response, changed)),
                    _ => Err("Source response is outside the inspection contract".into()),
                };
            }
            match bootstrap_chat {
                Some(chat_id) => changed |= chat_event_changed(&envelope, chat_id)?,
                None => return Err("Source inspection request or event identity changed".into()),
            }
        }
        Err("Retained-owner events exceeded the bounded inspection budget".into())
    }

    /// Wait for the source to announce that `chat_id`'s presentation changed.
    /// Only that chat's qualified events may arrive; no request is in flight.
    /// `deadline` fails a reader that never changes; a frame that has begun
    /// arriving is read under the connection's own deadline.
    fn await_chat_change(&mut self, chat_id: TerminalId, deadline: Instant) -> Result<(), String> {
        #[cfg(test)]
        note_chat_change_wait(chat_id);
        loop {
            if !readable(&self.stream, deadline.min(self.deadline))? {
                return Err(if Instant::now() < self.deadline {
                    unsettled_reader()
                } else {
                    "Source migration inspection timed out".into()
                });
            }
            let envelope = read_chat_envelope(
                self.protocol,
                &mut DeadlineStream {
                    stream: &mut self.stream,
                    deadline: self.deadline,
                },
            )?;
            if envelope.version != self.protocol || envelope.proof != self.proof {
                return Err("Source inspection ownership or protocol changed".into());
            }
            if chat_event_changed(&envelope, chat_id)? {
                return Ok(());
            }
        }
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

    pub(super) fn terminals(&mut self) -> Result<Vec<ipc::TerminalDescriptor>, String> {
        const LIMIT: usize = 1024;
        let unsupported =
            || "Source terminal inventory is unsupported or exceeds the inspection limit".into();
        if self.protocol < ipc::LIST_PAGES_PROTOCOL {
            return match self.request(ClientRequest::List, true)? {
                ServerResponse::Listed { terminals } if terminals.len() <= LIMIT => Ok(terminals),
                _ => Err(unsupported()),
            };
        }
        // A source that pages its inventory is read page by page, each one
        // strictly after the last, so no single reply outgrows a frame.
        let mut terminals = Vec::new();
        loop {
            let after = terminals
                .last()
                .map(|terminal: &ipc::TerminalDescriptor| terminal.session.id);
            let ServerResponse::ListedPage {
                terminals: page,
                complete,
            } = self.request(ClientRequest::ListPage { after }, true)?
            else {
                return Err(unsupported());
            };
            if !ipc::page_follows(after, &page, complete) || terminals.len() + page.len() > LIMIT {
                return Err(unsupported());
            }
            terminals.extend(page);
            if complete {
                return Ok(terminals);
            }
        }
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

    #[cfg(test)]
    pub(in crate::host) fn fixture_inspect_history(
        stream: UnixStream,
        source: &Session,
        proof: HostSessionProof,
    ) -> Result<(), String> {
        let mut client = Self {
            stream,
            proof: proof.clone(),
            next: 1,
            protocol: ipc::PROTOCOL_VERSION,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        match client.request(
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: Vec::new(),
            },
            false,
        )? {
            ServerResponse::Connected {
                proof: connected,
                resumed,
            } if connected == proof && resumed.is_empty() => client.inspect_saved_history(source),
            _ => Err("History fixture handshake did not authenticate the current owner".into()),
        }
    }

    /// Whether a history inspection has waited for `chat_id` to announce a
    /// presentation change.
    #[cfg(test)]
    pub(in crate::host) fn fixture_awaited_change(chat_id: TerminalId) -> bool {
        CHAT_CHANGE_WAITS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&chat_id)
    }

    /// Use only the existing forced-history Attach API. The request's current
    /// proof addresses that read-only API; it is never persisted as a recovered
    /// descriptor or presented as evidence of an old provider's execution.
    /// The host's normal owner/managed-work fences remain. No resident data is evicted.
    pub(super) fn inspect_saved_history(&mut self, source: &Session) -> Result<(), String> {
        if !source.is_saved_claude_history() {
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
            let (visible, _) = self.exchange(
                ClientRequest::ChatVisibility {
                    chat_id: source.id,
                    visible: true,
                },
                true,
            )?;
            if !matches!(visible, ServerResponse::Accepted) {
                return Err("Saved history could not be inspected".into());
            }
            // A reader applies its saved frame on its own worker after the
            // source attaches it, so the attached snapshot may predate it.
            // While visible, the reader announces each applied frame as a
            // presentation change; read once now, since the attached snapshot
            // predates visibility, then again after each announced change.
            let reader_deadline = Instant::now() + HISTORY_READER_DEADLINE;
            let mut changed = true;
            loop {
                // An exit ends the reader's frame and follows its read-only
                // reason, so an exited, loaded snapshot is final.
                if snapshot.exited && !snapshot.history_loading {
                    if snapshot.read_only_reason.is_some()
                        && snapshot.session_id == source.launch.session_id
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
                if !changed {
                    self.await_chat_change(source.id, reader_deadline)?;
                } else if Instant::now() >= reader_deadline {
                    return Err(unsettled_reader());
                }
                let (read, changed_meanwhile) = self.exchange(
                    ClientRequest::ChatRead {
                        chat_id: source.id,
                        after_generation: None,
                    },
                    true,
                )?;
                changed = changed_meanwhile;
                match read {
                    ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(current),
                    } if chat_id == source.id => snapshot = current,
                    _ => return Err("Saved history did not publish its exact transcript".into()),
                }
            }
        })();
        let detached = self.request(ClientRequest::ChatDetach { chat_id: source.id }, true)?;
        if !matches!(detached, ServerResponse::Detached) {
            return Err("Saved history inspection could not detach its reader".into());
        }
        // Keep the reader through consent and normal source shutdown. Removing
        // and reconstructing a pager advances the source's global inventory
        // generation and invalidates every unrelated live owner's consent.
        // Other attached history views remain attached; no cache is evicted.
        inspection
    }
}

/// Chats whose inspection has waited for a presentation change, so a test can
/// hold a reader until inspection depends on that event rather than a poll.
#[cfg(test)]
static CHAT_CHANGE_WAITS: std::sync::Mutex<Vec<TerminalId>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn note_chat_change_wait(chat_id: TerminalId) {
    CHAT_CHANGE_WAITS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(chat_id);
}

fn unsettled_reader() -> String {
    format!(
        "Saved history reader did not settle its saved frame within {} seconds",
        HISTORY_READER_DEADLINE.as_secs()
    )
}

/// Read one frame of the chat bootstrap subset: a response or an event, as
/// a source at `protocol` encoded it.
fn read_chat_envelope(protocol: u16, reader: &mut impl Read) -> Result<ServerEnvelope, String> {
    let payload = read_payload_with_limit(reader, ipc::MAX_FRAME_BYTES)?;
    ipc::decoding_from(protocol, || {
        postcard::from_bytes::<ServerEnvelope>(&payload)
    })
    .map_err(|_| "Unsupported legacy retained-owner response".into())
}

/// Accept only an unsolicited qualified event for `chat_id`, reporting whether
/// it announces a presentation change.
fn chat_event_changed(envelope: &ServerEnvelope, expected: TerminalId) -> Result<bool, String> {
    let (event_chat, changed) = match envelope.message {
        ServerMessage::ChatChanged { chat_id } => (Some(chat_id), true),
        ServerMessage::ChatActivity { chat_id, .. }
        | ServerMessage::ChatObserved { chat_id, .. }
        | ServerMessage::ChatAccountUsage { chat_id, .. }
        | ServerMessage::ChatActiveModel { chat_id, .. }
        | ServerMessage::ChatLimit { chat_id, .. }
        | ServerMessage::ChatAgentAddresses { chat_id, .. } => (Some(chat_id), false),
        _ => (None, false),
    };
    if envelope.request_id.is_some() || event_chat != Some(expected) {
        return Err("Source inspection request or event identity changed".into());
    }
    Ok(changed)
}

/// Whether `stream` has input before `deadline`, without consuming any.
fn readable(stream: &UnixStream, deadline: Instant) -> Result<bool, String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let mut fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Round up so a sub-millisecond remainder still waits.
        let timeout = remaining
            .as_micros()
            .div_ceil(1000)
            .min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: poll receives one initialized entry whose descriptor the
        // borrowed stream keeps open for the entire call.
        match unsafe { libc::poll(&mut fd, 1, timeout) } {
            0 => continue,
            n if n > 0 => return Ok(true),
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
            _ => return Err("Cannot wait for the source service".into()),
        }
    }
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

fn read_response(protocol: u16, reader: &mut impl Read) -> Result<ServerEnvelope, String> {
    let payload = read_payload_with_limit(reader, MAX_FRAME)?;
    let (envelope, remaining) = ipc::decoding_from(protocol, || {
        postcard::take_from_bytes::<InspectionEnvelope>(&payload)
    })
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
// that field. Accept only that message and the frozen inventory replies.
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
                struct ListedPage {
                    terminals: Vec<crate::terminal_ipc::TerminalDescriptor>,
                    complete: bool,
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
                    46 => {
                        let value: ListedPage = variant.newtype_variant()?;
                        ServerResponse::ListedPage {
                            terminals: value.terminals,
                            complete: value.complete,
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
        crate::update::source::protocols().collect()
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
        source.terminal = Some(Box::new(terminal.session.clone()));
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
            assert_eq!(read_response(100, &mut &frozen[..]).unwrap(), envelope);
            for protocol in source_protocols() {
                let historical = source_frame_version(frozen, protocol);
                let mut expected = envelope.clone();
                expected.version = protocol;
                let mut current = Vec::new();
                ipc::write_frame(&mut current, &expected).unwrap();
                assert_eq!(current, historical);
                assert_eq!(
                    read_response(protocol, &mut historical.as_slice()).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn frozen_inventory_subset_accepts_qualified_current_wire_sources() {
        assert!(!crate::update::source::is_qualified_source(98));
        assert!(!crate::update::source::is_qualified_source(
            ipc::PROTOCOL_VERSION + 1
        ));
        assert_eq!(postcard::to_allocvec(&ClientRequest::List).unwrap(), [1]);
        assert_eq!(
            postcard::to_allocvec(&ClientRequest::ChatList).unwrap(),
            [29]
        );
        assert_eq!(
            postcard::to_allocvec(&ClientRequest::ListPage { after: None }).unwrap(),
            [85, 0]
        );
        assert_eq!(
            postcard::to_allocvec(&ServerResponse::ListedPage {
                terminals: vec![],
                complete: true
            })
            .unwrap(),
            [46, 0, 1]
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
            // Frozen postcard envelope with request 7, proof s/b/i and an empty
            // ChatListed response. Only the leading version varint varies; from
            // 128 it takes two bytes.
            let mut payload = postcard::to_allocvec(&protocol).unwrap();
            payload.extend([1, 7, 1, b's', 1, b'b', 1, b'i', 0, 13, 0]);
            let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
            frame.extend(payload);
            let envelope = read_response(protocol, &mut frame.as_slice()).unwrap();
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
                    terminals: vec![terminal.clone()],
                },
                ServerResponse::ListedPage {
                    terminals: vec![terminal],
                    complete: false,
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
                assert_eq!(
                    read_response(protocol, &mut frame.as_slice()).unwrap(),
                    envelope
                );
            }
        }
    }

    #[test]
    fn history_preview_only_attaches_reads_and_detaches_without_execution_or_eviction() {
        for protocol in source_protocols() {
            history_preview_at_protocol(protocol);
        }
    }

    fn history_preview_at_protocol(protocol: u16) {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let source = super::super::tests::session();
        let mut requested = source.clone();
        requested.proof = Some(proof());
        let expected = requested.clone();
        let worker = std::thread::spawn(move || {
            for step in 0..4 {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(request.version, protocol);
                assert_eq!(request.proof, Some(proof()));
                let response = match (step, request.request) {
                    (0, ClientRequest::ChatAttach { session }) if session == expected => ServerResponse::ChatAttached {
                        session,
                        snapshot: crate::chat::Snapshot {
                            session_id: expected.launch.session_id.clone(), exited: true, history_loading: true,
                            read_only_reason: Some("Saved conversation history is read-only; no provider was started.".into()),
                            ..Default::default()
                        },
                    },
                    (1, ClientRequest::ChatVisibility { chat_id, visible: true }) if chat_id == expected.id => ServerResponse::Accepted,
                    (2, ClientRequest::ChatRead { chat_id, after_generation: None }) if chat_id == expected.id => ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(crate::chat::Snapshot {
                            session_id: expected.launch.session_id.clone(), exited: true,
                            read_only_reason: Some("Saved conversation history is read-only; no provider was started.".into()),
                            // A saved edit, which the source encodes in its own layout.
                            items: vec![std::sync::Arc::new(crate::chat::Item {
                                started_at: None,
                                id: "edit".into(),
                                role: crate::chat::Role::Assistant,
                                content: crate::chat::ItemContent::Changes {
                                    changes: vec![crate::chat::FileChange {
                                        path: "src/lib.rs".into(),
                                        kind: crate::chat::ChangeKind::Updated,
                                        diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
                                        lines: crate::chat::LineOrigin::File,
                                    }],
                                    status: crate::chat::ToolStatus::Succeeded,
                                },
                                at: None,
                            })],
                            ..Default::default()
                        }),
                    },
                    (3, ClientRequest::ChatDetach { chat_id }) if chat_id == expected.id => ServerResponse::Detached,
                    other => panic!("Unexpected history request (no Start, Close or Release permitted): {other:?}"),
                };
                // Only exact target-chat events may interleave with the reply.
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: protocol,
                        request_id: None,
                        proof: proof(),
                        message: ServerMessage::ChatChanged {
                            chat_id: expected.id,
                        },
                    },
                )
                .unwrap();
                ipc::encoding_for(protocol, || {
                    ipc::write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: protocol,
                            request_id: Some(request.request_id),
                            proof: proof(),
                            message: ServerMessage::Response { response },
                        },
                    )
                })
                .unwrap();
            }
        });
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            protocol,
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
                read_response(100, &mut frame.as_slice()).is_err(),
                "mutation {mutation}"
            );
        }
        assert!(
            read_response(100, &mut ((MAX_FRAME as u32 + 1).to_be_bytes()).as_slice()).is_err()
        );
        assert!(read_response(100, &mut [0, 0].as_slice()).is_err());
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
    fn a_paging_source_inventory_is_read_page_by_page_up_to_the_ceiling() {
        fn descriptor() -> crate::terminal_ipc::TerminalDescriptor {
            let source = super::super::tests::session();
            crate::terminal_ipc::TerminalDescriptor::relaunchable(
                crate::model::TerminalId::new(),
                source.launch.recipe,
                None,
            )
        }
        // Each case is the page sizes the source answers; the last is complete.
        for pages in [
            vec![0],
            vec![2, 3],
            vec![1, 1, 0],
            vec![1000, 24],
            vec![1000, 25],
        ] {
            let total: usize = pages.iter().sum();
            let mut inventory: Vec<_> = (0..total).map(|_| descriptor()).collect();
            inventory.sort_by_key(|terminal| terminal.session.id);
            let expected = inventory.clone();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let count = pages.len();
            let worker = std::thread::spawn(move || {
                let mut rest = inventory.as_slice();
                let mut last = None;
                for (index, size) in pages.into_iter().enumerate() {
                    let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                    let ClientRequest::ListPage { after } = request.request else {
                        panic!("a paging source is read with ListPage");
                    };
                    assert_eq!(after, last);
                    let (page, remaining) = rest.split_at(size);
                    rest = remaining;
                    last = page.last().map(|terminal| terminal.session.id).or(last);
                    ipc::write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: ipc::LIST_PAGES_PROTOCOL,
                            request_id: Some(request.request_id),
                            proof: proof(),
                            message: ServerMessage::Response {
                                response: ServerResponse::ListedPage {
                                    terminals: page.to_vec(),
                                    complete: index + 1 == count,
                                },
                            },
                        },
                    )
                    .unwrap();
                }
            });
            let mut client = Client {
                stream,
                proof: proof(),
                next: 1,
                protocol: ipc::LIST_PAGES_PROTOCOL,
                deadline: Instant::now() + Duration::from_secs(10),
            };
            let result = client.terminals();
            worker.join().unwrap();
            if total <= 1024 {
                assert_eq!(result.unwrap(), expected);
            } else {
                assert!(result.unwrap_err().contains("inspection limit"));
            }
        }
    }

    #[test]
    fn qualified_sources_authenticate_and_resolve_at_their_own_protocol() {
        for protocol in source_protocols() {
            let directory = tempfile::tempdir().unwrap();
            let paths = crate::daemon::RuntimePaths {
                directory: directory.path().into(),
                socket: directory.path().join("cagentsd.sock"),
                lock: directory.path().join("cagentsd.lock"),
            };
            let mut owner = proof();
            owner.boot_id = crate::daemon::read_boot_id().unwrap();
            let _lease = crate::daemon::HostLease::acquire(&paths, &owner).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
            let expected = owner.clone();
            let source = super::super::tests::session();
            let expected_source = source.clone();
            let worker = std::thread::spawn(move || {
                let (mut peer, _) = listener.accept().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                for step in 0..2 {
                    let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                    assert_eq!(request.version, protocol);
                    let response = if step == 0 {
                        assert!(request.proof.is_none());
                        assert!(
                            matches!(request.request, ClientRequest::Connect { expected_host: Some(proof), resume } if proof == expected && resume.is_empty())
                        );
                        ServerResponse::Connected {
                            proof: expected.clone(),
                            resumed: Vec::new(),
                        }
                    } else {
                        assert_eq!(request.proof.as_ref(), Some(&expected));
                        assert!(
                            matches!(request.request, ClientRequest::ResolveUpgradeChat { source } if source == expected_source)
                        );
                        ServerResponse::UpgradeChatResolved {
                            session: expected_source.clone(),
                        }
                    };
                    ipc::write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: protocol,
                            request_id: Some(request.request_id),
                            proof: expected.clone(),
                            message: ServerMessage::Response { response },
                        },
                    )
                    .unwrap();
                }
            });
            let mut client = Client::connect(
                &paths.socket,
                &owner,
                Instant::now() + Duration::from_secs(5),
                protocol,
            )
            .unwrap();
            assert_eq!(client.resolve(&source).unwrap(), source);
            drop(client);
            worker.join().unwrap();
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
            assert_eq!(
                result.unwrap_err(),
                if mismatch == 1 {
                    "Source inspection request or event identity changed"
                } else {
                    "Source inspection ownership or protocol changed"
                },
                "mismatch case {mismatch}"
            );
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

    #[test]
    fn history_change_wait_needs_a_presentation_change_and_fails_a_wedged_reader_cleanly() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let chat = super::super::tests::session().id;
        let mut client = Client {
            stream,
            proof: proof(),
            next: 1,
            protocol: ipc::PROTOCOL_VERSION,
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let event = |message| ServerEnvelope {
            version: ipc::PROTOCOL_VERSION,
            request_id: None,
            proof: proof(),
            message,
        };
        // Observation never authorizes a presentation read, so it does not end
        // the wait; with nothing else, the reader's own deadline does.
        ipc::write_frame(
            &mut peer,
            &event(ServerMessage::ChatObserved {
                chat_id: chat,
                epoch: 1,
            }),
        )
        .unwrap();
        assert_eq!(
            client
                .await_chat_change(chat, Instant::now() + Duration::from_millis(20))
                .unwrap_err(),
            unsettled_reader()
        );
        // That failure consumed no partial frame: the connection stays in step
        // for the change that follows and for detach.
        ipc::write_frame(
            &mut peer,
            &event(ServerMessage::ChatChanged { chat_id: chat }),
        )
        .unwrap();
        client
            .await_chat_change(chat, Instant::now() + Duration::from_secs(30))
            .unwrap();
        ipc::write_frame(
            &mut peer,
            &event(ServerMessage::ChatChanged {
                chat_id: crate::model::TerminalId::new(),
            }),
        )
        .unwrap();
        assert_eq!(
            client
                .await_chat_change(chat, Instant::now() + Duration::from_secs(30))
                .unwrap_err(),
            "Source inspection request or event identity changed"
        );
    }
}
