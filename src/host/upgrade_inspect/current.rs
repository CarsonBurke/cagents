//! Read-only inspection of the stable 99/100/101/current inventory subset.
//! The frozen protocol-77 reader remains separate from this matched connection.
use super::legacy_session::LegacySession;
use crate::{
    chat::Session,
    model::HostSessionProof,
    terminal::ipc::{self, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse},
};
use serde::{Deserialize, Serialize, ser::SerializeStructVariant};
use std::{
    io::{self, Read, Write},
    marker::PhantomData,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 4 * 1024 * 1024;

#[derive(Serialize)]
struct RequestEnvelope<'a> {
    version: u16,
    request_id: u64,
    proof: Option<&'a HostSessionProof>,
    request: VersionedRequest<'a>,
}

/// The old Session has no draft-model field. Only these three admitted
/// requests contain it; no general current request may leak into that codec.
struct VersionedRequest<'a> {
    protocol: u16,
    request: &'a ClientRequest,
}

impl Serialize for VersionedRequest<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.protocol == ipc::PROTOCOL_VERSION {
            return self.request.serialize(serializer);
        }
        match self.request {
            ClientRequest::ChatStart {
                session,
                source,
                terminate_source,
                execution_environment,
            } => {
                let mut value =
                    serializer.serialize_struct_variant("ClientRequest", 20, "ChatStart", 4)?;
                value.serialize_field("session", &LegacySession::from(session.clone()))?;
                value.serialize_field("source", source)?;
                value.serialize_field("terminate_source", terminate_source)?;
                value.serialize_field("execution_environment", execution_environment)?;
                value.end()
            }
            ClientRequest::ChatAttach { session } => {
                let mut value =
                    serializer.serialize_struct_variant("ClientRequest", 22, "ChatAttach", 1)?;
                value.serialize_field("session", &LegacySession::from(session.clone()))?;
                value.end()
            }
            ClientRequest::ResolveUpgradeChat { source } => {
                let mut value = serializer.serialize_struct_variant(
                    "ClientRequest",
                    38,
                    "ResolveUpgradeChat",
                    1,
                )?;
                value.serialize_field("source", &LegacySession::from(source.clone()))?;
                value.end()
            }
            ClientRequest::Connect { .. }
            | ClientRequest::List
            | ClientRequest::ChatList
            | ClientRequest::ChatDetach { .. }
            | ClientRequest::ChatVisibility { .. }
            | ClientRequest::ChatRead { .. } => self.request.serialize(serializer),
            _ => Err(serde::ser::Error::custom(
                "request is outside the source inspection codec",
            )),
        }
    }
}

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
            | ClientRequest::ChatRead { chat_id, .. } => Some(*chat_id),
            _ => None,
        };
        if bootstrap_chat.is_some() && !supports_history_protocol(self.protocol) {
            return Err("Source protocol has no qualified snapshot inspection API".into());
        }
        let request_id = self.next;
        self.next += 1;
        let mut stream = DeadlineStream {
            stream: &mut self.stream,
            deadline: self.deadline,
        };
        ipc::write_frame(
            &mut stream,
            &RequestEnvelope {
                version: self.protocol,
                request_id,
                proof: authenticated.then_some(&self.proof),
                request: VersionedRequest {
                    protocol: self.protocol,
                    request: &request,
                },
            },
        )
        .map_err(|_| "Cannot send source inspection")?;
        // Legacy ChatStart installs its hidden subscriber before responding.
        // Activity can arrive first, including during Detach. Drain only that
        // exact chat's qualified events under the same absolute deadline.
        for _ in 0..256 {
            let envelope = read_response(&mut stream, self.protocol, bootstrap_chat.is_some())?;
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

    /// Use only the existing forced-history Attach API. The request's current
    /// proof addresses that read-only API; it is never persisted as a recovered
    /// descriptor or presented as evidence of an old provider's execution.
    /// The host's normal owner/managed-work fences remain. No resident data is evicted.
    pub(super) fn inspect_saved_history(&mut self, source: &Session) -> Result<(), String> {
        if !supports_history_protocol(self.protocol) || !source.is_saved_claude_history() {
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
        // Keep the reader through consent and normal source shutdown. Removing
        // and reconstructing a pager advances legacy119's global inventory
        // generation and invalidates every unrelated live owner's consent.
        // Other attached history views remain attached; no cache is evicted.
        inspection
    }
}

// These versions have identical Connect/List/ChatList/ResolveUpgradeChat tags
// and inventory DTOs. Version 100 appends capacity operations; 101 requires
// atomic managed-pane adoption without changing these DTOs. This restricted
// Inventory uses the frozen restricted decoder. The separate retained/history
// methods admit full snapshots only for qualified119/127/128/current layouts.
// Versions 102 (Copy sidecars), 103 (retained Claude continuity) and 104 (the
// appended Work board request) preserve this subset, including the existing
// continuity enum discriminants. Protocol127 was the previous matched version.
// Qualified installed128 (c68275e94) appends StartupState without changing the
// consumed layouts. A separate private128 appends Provider::Antigravity; unknown
// provider variants remain rejected. Current148 adds draft_model and uses the
// current Session. Other private versions have no qualified source mapping.
pub(super) fn supports_protocol(protocol: u16) -> bool {
    [99, 100, 101, 102, 103, 119, 127, 128, ipc::PROTOCOL_VERSION].contains(&protocol)
}

fn supports_history_protocol(protocol: u16) -> bool {
    [119, 127, 128, ipc::PROTOCOL_VERSION].contains(&protocol)
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

fn read_response(
    reader: &mut impl Read,
    protocol: u16,
    bootstrap: bool,
) -> Result<ServerEnvelope, String> {
    if !supports_protocol(protocol) || bootstrap && !supports_history_protocol(protocol) {
        return Err("Source protocol has no qualified response inspection API".into());
    }
    let limit = if bootstrap {
        ipc::MAX_FRAME_BYTES
    } else {
        MAX_FRAME
    };
    let payload = read_payload_with_limit(reader, limit)?;
    // Choose the layout from the authenticated connection's protocol, never
    // by trying an old and new decoder until one happens to accept a frame.
    match (protocol == ipc::PROTOCOL_VERSION, bootstrap) {
        (true, false) => decode_response::<Session, false>(&payload),
        (true, true) => decode_response::<Session, true>(&payload),
        (false, false) => decode_response::<LegacySession, false>(&payload),
        (false, true) => decode_response::<LegacySession, true>(&payload),
    }
}

fn decode_response<S, const BOOTSTRAP: bool>(payload: &[u8]) -> Result<ServerEnvelope, String>
where
    S: serde::de::DeserializeOwned + Into<Session>,
{
    let (envelope, remaining) =
        postcard::take_from_bytes::<InspectionEnvelope<InspectionMessage<S, BOOTSTRAP>>>(payload)
            .map_err(|_| "Unsupported source response")?;
    if !remaining.is_empty() {
        return Err("Source response has an unknown layout".into());
    }
    Ok(ServerEnvelope {
        version: envelope.version,
        request_id: envelope.request_id,
        proof: envelope.proof,
        message: envelope.message.0,
    })
}

#[derive(Deserialize)]
struct InspectionEnvelope<M> {
    version: u16,
    request_id: Option<u64>,
    proof: HostSessionProof,
    message: M,
}

/// Inventory never consumes native snapshots. Only the qualified history
/// attachment path may read them and drain its four exact-chat event types.
struct InspectionMessage<S, const BOOTSTRAP: bool>(ServerMessage, PhantomData<S>);
impl<'de, S, const BOOTSTRAP: bool> Deserialize<'de> for InspectionMessage<S, BOOTSTRAP>
where
    S: Deserialize<'de> + Into<Session>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<S, const BOOTSTRAP: bool>(PhantomData<S>);
        impl<'de, S, const BOOTSTRAP: bool> serde::de::Visitor<'de> for Visitor<S, BOOTSTRAP>
        where
            S: Deserialize<'de> + Into<Session>,
        {
            type Value = InspectionMessage<S, BOOTSTRAP>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a qualified source inspection message")
            }
            fn visit_enum<A: serde::de::EnumAccess<'de>>(
                self,
                data: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::VariantAccess;
                #[derive(Deserialize)]
                struct Changed {
                    chat_id: crate::model::TerminalId,
                }
                #[derive(Deserialize)]
                struct Observed {
                    chat_id: crate::model::TerminalId,
                    epoch: u64,
                }
                #[derive(Deserialize)]
                struct Activity {
                    chat_id: crate::model::TerminalId,
                    state: crate::agent::ActivityState,
                    generation: u64,
                }
                #[derive(Deserialize)]
                struct AccountUsage {
                    chat_id: crate::model::TerminalId,
                    report: crate::account::LiveReport,
                    observed_at: u64,
                }
                let (tag, variant) = data.variant::<u32>()?;
                let message = match tag {
                    0 => {
                        let response: InspectionResponse<S, BOOTSTRAP> =
                            variant.newtype_variant()?;
                        ServerMessage::Response {
                            response: response.0,
                        }
                    }
                    7 if BOOTSTRAP => {
                        let value: Changed = variant.newtype_variant()?;
                        ServerMessage::ChatChanged {
                            chat_id: value.chat_id,
                        }
                    }
                    8 if BOOTSTRAP => {
                        let value: Observed = variant.newtype_variant()?;
                        ServerMessage::ChatObserved {
                            chat_id: value.chat_id,
                            epoch: value.epoch,
                        }
                    }
                    10 if BOOTSTRAP => {
                        let value: Activity = variant.newtype_variant()?;
                        ServerMessage::ChatActivity {
                            chat_id: value.chat_id,
                            state: value.state,
                            generation: value.generation,
                        }
                    }
                    13 if BOOTSTRAP => {
                        let value: AccountUsage = variant.newtype_variant()?;
                        ServerMessage::ChatAccountUsage {
                            chat_id: value.chat_id,
                            report: value.report,
                            observed_at: value.observed_at,
                        }
                    }
                    _ => {
                        return Err(serde::de::Error::custom(
                            "message is outside the read-only inspection contract",
                        ));
                    }
                };
                Ok(InspectionMessage(message, PhantomData))
            }
        }
        deserializer.deserialize_enum(
            "InspectionMessage",
            &[],
            Visitor::<S, BOOTSTRAP>(PhantomData),
        )
    }
}

struct InspectionResponse<S, const BOOTSTRAP: bool>(ServerResponse, PhantomData<S>);
impl<'de, S, const BOOTSTRAP: bool> Deserialize<'de> for InspectionResponse<S, BOOTSTRAP>
where
    S: Deserialize<'de> + Into<Session>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<S, const BOOTSTRAP: bool>(PhantomData<S>);
        impl<'de, S, const BOOTSTRAP: bool> serde::de::Visitor<'de> for Visitor<S, BOOTSTRAP>
        where
            S: Deserialize<'de> + Into<Session>,
        {
            type Value = InspectionResponse<S, BOOTSTRAP>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a qualified source inspection response")
            }
            fn visit_enum<A: serde::de::EnumAccess<'de>>(
                self,
                data: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::VariantAccess;
                #[derive(Deserialize)]
                struct Connected {
                    proof: HostSessionProof,
                    resumed: Vec<crate::terminal_ipc::TerminalDescriptor>,
                }
                #[derive(Deserialize)]
                struct Listed {
                    terminals: Vec<crate::terminal_ipc::TerminalDescriptor>,
                }
                #[derive(Deserialize)]
                struct ChatListed<S> {
                    sessions: Vec<S>,
                }
                #[derive(Deserialize)]
                struct Resolved<S> {
                    session: S,
                }
                #[derive(Deserialize)]
                struct Attached<S> {
                    session: S,
                    // Source119 and current share this snapshot layout. It
                    // contains no Session/Launch; the attachment beside it does.
                    snapshot: crate::chat::Snapshot,
                }
                #[derive(Deserialize)]
                struct Snapshot {
                    chat_id: crate::model::TerminalId,
                    snapshot: Option<crate::chat::Snapshot>,
                }
                let (tag, variant) = data.variant::<u32>()?;
                let response = match tag {
                    0 if !BOOTSTRAP => {
                        let value: Connected = variant.newtype_variant()?;
                        ServerResponse::Connected {
                            proof: value.proof,
                            resumed: value.resumed,
                        }
                    }
                    1 if !BOOTSTRAP => {
                        let value: Listed = variant.newtype_variant()?;
                        ServerResponse::Listed {
                            terminals: value.terminals,
                        }
                    }
                    13 if !BOOTSTRAP => {
                        let value: ChatListed<S> = variant.newtype_variant()?;
                        ServerResponse::ChatListed {
                            sessions: value.sessions.into_iter().map(Into::into).collect(),
                        }
                    }
                    18 if !BOOTSTRAP => {
                        let value: Resolved<S> = variant.newtype_variant()?;
                        ServerResponse::UpgradeChatResolved {
                            session: value.session.into(),
                        }
                    }
                    4 if BOOTSTRAP => {
                        variant.unit_variant()?;
                        ServerResponse::Detached
                    }
                    5 if BOOTSTRAP => {
                        variant.unit_variant()?;
                        ServerResponse::Accepted
                    }
                    11 if BOOTSTRAP => {
                        let value: Attached<S> = variant.newtype_variant()?;
                        ServerResponse::ChatAttached {
                            session: value.session.into(),
                            snapshot: value.snapshot,
                        }
                    }
                    12 if BOOTSTRAP => {
                        let value: Snapshot = variant.newtype_variant()?;
                        ServerResponse::ChatSnapshot {
                            chat_id: value.chat_id,
                            snapshot: value.snapshot,
                        }
                    }
                    _ => {
                        return Err(serde::de::Error::custom(
                            "response is outside the read-only inspection contract",
                        ));
                    }
                };
                Ok(InspectionResponse(response, PhantomData))
            }
        }
        deserializer.deserialize_enum(
            "InspectionResponse",
            &[],
            Visitor::<S, BOOTSTRAP>(PhantomData),
        )
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
    use crate::terminal_ipc::ClientEnvelope;

    fn legacy_protocols() -> std::collections::BTreeSet<u16> {
        [99, 100, 101, 102, 103, 119, 127, 128].into()
    }

    fn source_protocols() -> std::collections::BTreeSet<u16> {
        let mut protocols = legacy_protocols();
        protocols.insert(ipc::PROTOCOL_VERSION);
        protocols
    }

    fn append<T: Serialize>(bytes: &mut Vec<u8>, value: &T) {
        bytes.extend(postcard::to_allocvec(value).unwrap());
    }

    // Frozen top-level field sequence from qualified source119
    // 3fcce2a7bf7dcca4cb307de544613aedb0388b61. The source audit separately
    // verified the transitive DTO declarations against127/128. Do not replace
    // this sequence with current Snapshot serialization in old-peer fixtures.
    fn frozen_snapshot119(snapshot: &crate::chat::Snapshot) -> Vec<u8> {
        let mut bytes = Vec::new();
        macro_rules! fields {
            ($($field:ident),* $(,)?) => { $(append(&mut bytes, &snapshot.$field);)* };
        }
        fields!(
            sampled_at,
            generation,
            turn_generation,
            finished_at,
            items,
            text_copy_sources,
            requests,
            capabilities,
            models,
            selected_model,
            reasoning,
            fast_mode,
            permission_mode,
            permissions,
            permission_denials,
            commands,
            skills,
            usage,
            busy,
            detail,
            session_id,
            native_continuity,
            title,
            error,
            read_only_reason,
            exited,
            has_earlier,
            has_later,
            composer,
            prompt_suggestion,
            open_url,
            waiting,
            plan,
            subagents,
            ready,
            queued_messages,
            queue_removals,
            history_loading,
            interrupt_mode,
            branch_points,
            forked,
            file_changes,
        );
        bytes
    }

    // The peer encodes the old field sequence independently of the production
    // decoder. Never serialize a current Session just with an older version.
    fn source_frame(envelope: &ServerEnvelope) -> Vec<u8> {
        if envelope.version == ipc::PROTOCOL_VERSION {
            let mut frame = Vec::new();
            ipc::write_frame(&mut frame, envelope).unwrap();
            return frame;
        }
        let mut payload = Vec::new();
        append(&mut payload, &envelope.version);
        append(&mut payload, &envelope.request_id);
        append(&mut payload, &envelope.proof);
        match &envelope.message {
            ServerMessage::Response { response } => {
                append(&mut payload, &0u32);
                match response {
                    ServerResponse::Connected { proof, resumed } => {
                        append(&mut payload, &0u32);
                        append(&mut payload, proof);
                        append(&mut payload, resumed);
                    }
                    ServerResponse::Listed { terminals } => {
                        append(&mut payload, &1u32);
                        append(&mut payload, terminals);
                    }
                    ServerResponse::Detached => append(&mut payload, &4u32),
                    ServerResponse::Accepted => append(&mut payload, &5u32),
                    ServerResponse::ChatAttached { session, snapshot } => {
                        append(&mut payload, &11u32);
                        append(&mut payload, &LegacySession::from(session.clone()));
                        payload.extend(frozen_snapshot119(snapshot));
                    }
                    ServerResponse::ChatSnapshot { chat_id, snapshot } => {
                        append(&mut payload, &12u32);
                        append(&mut payload, chat_id);
                        append(&mut payload, &snapshot.as_ref().map(|_| ()));
                        if let Some(snapshot) = snapshot {
                            payload.extend(frozen_snapshot119(snapshot));
                        }
                    }
                    ServerResponse::ChatListed { sessions } => {
                        append(&mut payload, &13u32);
                        let sessions: Vec<_> =
                            sessions.iter().cloned().map(LegacySession::from).collect();
                        append(&mut payload, &sessions);
                    }
                    ServerResponse::UpgradeChatResolved { session } => {
                        append(&mut payload, &18u32);
                        append(&mut payload, &LegacySession::from(session.clone()));
                    }
                    response => panic!("unqualified fixture response: {response:?}"),
                }
            }
            message @ (ServerMessage::ChatChanged { .. }
            | ServerMessage::ChatObserved { .. }
            | ServerMessage::ChatActivity { .. }
            | ServerMessage::ChatAccountUsage { .. }) => append(&mut payload, message),
            message => panic!("unqualified fixture message: {message:?}"),
        }
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend(payload);
        frame
    }

    fn write_source_frame(writer: &mut impl Write, envelope: &ServerEnvelope) -> io::Result<()> {
        writer.write_all(&source_frame(envelope))
    }

    fn take<T: serde::de::DeserializeOwned>(payload: &mut &[u8]) -> T {
        let (value, remaining) = postcard::take_from_bytes(payload).unwrap();
        *payload = remaining;
        value
    }

    fn read_legacy_request(reader: &mut impl Read) -> ClientEnvelope {
        let payload = read_payload_with_limit(reader, ipc::MAX_CLIENT_FRAME_BYTES).unwrap();
        let mut remaining = payload.as_slice();
        let version = take(&mut remaining);
        let request_id = take(&mut remaining);
        let proof = take(&mut remaining);
        let tag: u32 = take(&mut remaining);
        let request = match tag {
            0 => ClientRequest::Connect {
                expected_host: take(&mut remaining),
                resume: take(&mut remaining),
            },
            1 => ClientRequest::List,
            20 => ClientRequest::ChatStart {
                session: take::<LegacySession>(&mut remaining).into(),
                source: take(&mut remaining),
                terminate_source: take(&mut remaining),
                execution_environment: take(&mut remaining),
            },
            22 => ClientRequest::ChatAttach {
                session: take::<LegacySession>(&mut remaining).into(),
            },
            23 => ClientRequest::ChatDetach {
                chat_id: take(&mut remaining),
            },
            25 => ClientRequest::ChatVisibility {
                chat_id: take(&mut remaining),
                visible: take(&mut remaining),
            },
            26 => ClientRequest::ChatRead {
                chat_id: take(&mut remaining),
                after_generation: take(&mut remaining),
            },
            29 => ClientRequest::ChatList,
            38 => ClientRequest::ResolveUpgradeChat {
                source: take::<LegacySession>(&mut remaining).into(),
            },
            _ => panic!("unqualified source request tag {tag}"),
        };
        assert!(
            remaining.is_empty(),
            "a current Session field leaked into a legacy request"
        );
        ClientEnvelope {
            version,
            request_id,
            proof,
            request,
        }
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
            write_source_frame(&mut bytes, &envelope).unwrap();
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
            assert_eq!(
                read_response(&mut &frozen[..], 100, false).unwrap(),
                envelope
            );
            for protocol in source_protocols() {
                let historical = source_frame_version(frozen, protocol);
                let mut expected = envelope.clone();
                expected.version = protocol;
                let mut current = Vec::new();
                write_source_frame(&mut current, &expected).unwrap();
                if protocol == ipc::PROTOCOL_VERSION
                    && matches!(
                        expected.message,
                        ServerMessage::Response {
                            response: ServerResponse::ChatListed { .. }
                                | ServerResponse::UpgradeChatResolved { .. }
                        }
                    )
                {
                    assert_ne!(current, historical);
                    assert!(read_response(&mut historical.as_slice(), protocol, false).is_err());
                } else {
                    assert_eq!(current, historical);
                }
                assert_eq!(
                    read_response(&mut current.as_slice(), protocol, false).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn installed_128_inventory_rejects_an_unqualified_provider_variant() {
        let frozen =
            include_bytes!("../../../tests/fixtures/upgrade-inspection-v100/chat_listed.bin");
        let mut frame = source_frame_version(frozen, 128);
        let mut remaining = &frame[4..];
        assert_eq!(take::<u16>(&mut remaining), 128);
        assert_eq!(take::<Option<u64>>(&mut remaining), Some(7));
        assert_eq!(take::<HostSessionProof>(&mut remaining), proof());
        assert_eq!(take::<u32>(&mut remaining), 0); // ServerMessage::Response.
        assert_eq!(take::<u32>(&mut remaining), 13); // ChatListed.
        assert_eq!(take::<usize>(&mut remaining), 1);
        let _: crate::model::TerminalId = take(&mut remaining);
        let provider_offset = frame.len() - remaining.len();
        assert_eq!(
            take::<crate::agent::Provider>(&mut remaining),
            crate::agent::Provider::Codex
        );
        // The separate Antigravity draft also labels its source128, but its
        // appended Provider index3 is outside the installed source's schema.
        frame[provider_offset] = 3;
        assert!(read_response(&mut frame.as_slice(), 128, false).is_err());
    }

    #[test]
    fn frozen_inventory_subset_accepts_qualified_legacy_and_current_versions() {
        assert!(!supports_protocol(98));
        assert!(!supports_protocol(ipc::PROTOCOL_VERSION + 1));
        for protocol in 129..=147 {
            assert!(!supports_protocol(protocol));
            assert!(!supports_history_protocol(protocol));
        }
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
            // ChatListed response. Only the encoded version scalar changes.
            let mut payload = postcard::to_allocvec(&protocol).unwrap();
            payload.extend([1, 7, 1, b's', 1, b'b', 1, b'i', 0, 13, 0]);
            let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
            frame.extend(payload);
            let envelope = read_response(&mut frame.as_slice(), protocol, false).unwrap();
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
                write_source_frame(&mut frame, &envelope).unwrap();
                assert_eq!(
                    read_response(&mut frame.as_slice(), protocol, false).unwrap(),
                    envelope
                );
            }
        }
    }

    #[test]
    fn session_requests_use_the_selected_sources_frozen_or_current_layout() {
        for protocol in source_protocols() {
            let mut session = populated_inventory().0;
            session.launch.draft_model = Some(crate::chat::DraftModel {
                model: "current-only-model".into(),
                reasoning: Some("high".into()),
            });
            let mut requests = vec![ClientRequest::ResolveUpgradeChat {
                source: session.clone(),
            }];
            if supports_history_protocol(protocol) {
                requests.push(ClientRequest::ChatAttach {
                    session: session.clone(),
                });
            }
            if protocol == 119 {
                requests.push(ClientRequest::ChatStart {
                    session: session.clone(),
                    source: None,
                    terminate_source: false,
                    execution_environment: Default::default(),
                });
            }
            for request in requests {
                let mut bytes = Vec::new();
                ipc::write_frame(
                    &mut bytes,
                    &RequestEnvelope {
                        version: protocol,
                        request_id: 7,
                        proof: Some(&proof()),
                        request: VersionedRequest {
                            protocol,
                            request: &request,
                        },
                    },
                )
                .unwrap();
                let decoded = if protocol == ipc::PROTOCOL_VERSION {
                    ipc::read_frame::<ClientEnvelope>(&mut bytes.as_slice())
                        .unwrap()
                        .unwrap()
                } else {
                    read_legacy_request(&mut bytes.as_slice())
                };
                assert_eq!(decoded.version, protocol);
                assert_eq!(decoded.request_id, 7);
                assert_eq!(decoded.proof, Some(proof()));
                let mut expected = request;
                if protocol != ipc::PROTOCOL_VERSION {
                    match &mut expected {
                        ClientRequest::ResolveUpgradeChat { source }
                        | ClientRequest::ChatAttach { session: source }
                        | ClientRequest::ChatStart {
                            session: source, ..
                        } => source.launch.draft_model = None,
                        _ => unreachable!(),
                    }
                }
                assert_eq!(decoded.request, expected);
            }
        }
    }

    #[test]
    fn source_session_decoders_never_fall_back_to_a_different_protocol_layout() {
        let mut session = populated_inventory().0;
        session.launch.draft_model = Some(crate::chat::DraftModel {
            model: "current-only-model".into(),
            reasoning: Some("high".into()),
        });
        for response in [
            ServerResponse::ChatListed {
                sessions: vec![session.clone()],
            },
            ServerResponse::UpgradeChatResolved {
                session: session.clone(),
            },
        ] {
            let envelope = ServerEnvelope {
                version: ipc::PROTOCOL_VERSION,
                request_id: Some(7),
                proof: proof(),
                message: ServerMessage::Response { response },
            };
            let current = source_frame(&envelope);
            assert_eq!(
                read_response(&mut current.as_slice(), ipc::PROTOCOL_VERSION, false).unwrap(),
                envelope
            );
            for protocol in [119, 127, 128] {
                let mut mislabeled = envelope.clone();
                mislabeled.version = protocol;
                let mut bytes = Vec::new();
                // Deliberately bypass the legacy adapter to model a packet
                // that contains the new Session under an old protocol label.
                ipc::write_frame(&mut bytes, &mislabeled).unwrap();
                assert!(read_response(&mut bytes.as_slice(), protocol, false).is_err());
            }
        }
        for protocol in [119, 127, 128] {
            let attached = ServerEnvelope {
                version: protocol,
                request_id: Some(7),
                proof: proof(),
                message: ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session: session.clone(),
                        snapshot: crate::chat::Snapshot::default(),
                    },
                },
            };
            let mut bytes = Vec::new();
            ipc::write_frame(&mut bytes, &attached).unwrap();
            assert!(read_response(&mut bytes.as_slice(), protocol, true).is_err());
        }
    }

    #[test]
    fn source_snapshot_layout_and_decoder_scope_remain_qualified() {
        let source = populated_inventory().0;
        let snapshot = crate::chat::Snapshot {
            sampled_at: Some(71),
            generation: 257,
            turn_generation: 13,
            session_id: source.launch.session_id.clone(),
            ready: true,
            selected_model: Some("fixture-model".into()),
            reasoning: Some("high".into()),
            native_continuity: source.native_continuity.clone(),
            items: vec![std::sync::Arc::new(crate::chat::Item {
                id: "snapshot119".into(),
                role: crate::chat::Role::Assistant,
                content: crate::chat::ItemContent::Text {
                    text: "Retained history 🦀".into(),
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: Some(123),
                at: Some(456),
            })],
            ..Default::default()
        };
        assert_eq!(
            postcard::to_allocvec(&snapshot).unwrap(),
            frozen_snapshot119(&snapshot)
        );
        for protocol in [119, 127, 128, ipc::PROTOCOL_VERSION] {
            for response in [
                ServerResponse::Detached,
                ServerResponse::Accepted,
                ServerResponse::ChatAttached {
                    session: source.clone(),
                    snapshot: snapshot.clone(),
                },
                ServerResponse::ChatSnapshot {
                    chat_id: source.id,
                    snapshot: Some(snapshot.clone()),
                },
            ] {
                let envelope = ServerEnvelope {
                    version: protocol,
                    request_id: Some(7),
                    proof: proof(),
                    message: ServerMessage::Response { response },
                };
                let bytes = source_frame(&envelope);
                assert!(read_response(&mut bytes.as_slice(), protocol, false).is_err());
                assert_eq!(
                    read_response(&mut bytes.as_slice(), protocol, true).unwrap(),
                    envelope
                );
                let mut trailing = bytes[4..].to_vec();
                trailing.push(0);
                let mut frame = (trailing.len() as u32).to_be_bytes().to_vec();
                frame.extend(trailing);
                assert!(read_response(&mut frame.as_slice(), protocol, true).is_err());
            }
            let inventory = ServerEnvelope {
                version: protocol,
                request_id: Some(7),
                proof: proof(),
                message: ServerMessage::Response {
                    response: ServerResponse::ChatListed {
                        sessions: vec![source.clone()],
                    },
                },
            };
            assert!(
                read_response(&mut source_frame(&inventory).as_slice(), protocol, true).is_err()
            );
        }
        for protocol in [99, 100, 101, 102, 103] {
            assert!(read_response(&mut [].as_slice(), protocol, true).is_err());
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
                let request = read_legacy_request(&mut peer);
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
                    write_source_frame(
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
                write_source_frame(
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
    fn legacy_history_preview_only_attaches_reads_and_detaches_without_execution_or_eviction() {
        for protocol in [119, 127, 128, ipc::PROTOCOL_VERSION] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let source = super::super::tests::session();
            let mut requested = source.clone();
            requested.proof = Some(proof());
            let expected = requested.clone();
            let worker = std::thread::spawn(move || {
                for step in 0..4 {
                    let request = if protocol == ipc::PROTOCOL_VERSION {
                        ipc::read_frame::<ClientEnvelope>(&mut peer)
                            .unwrap()
                            .unwrap()
                    } else {
                        read_legacy_request(&mut peer)
                    };
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
                            ..Default::default()
                        }),
                    },
                    (3, ClientRequest::ChatDetach { chat_id }) if chat_id == expected.id => ServerResponse::Detached,
                    other => panic!("Unexpected history request (no Start, Close or Release permitted): {other:?}"),
                };
                    // Only exact target-chat events may interleave with the reply.
                    write_source_frame(
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
                    write_source_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: protocol,
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
                protocol,
                deadline: Instant::now() + Duration::from_secs(5),
            };
            client.inspect_saved_history(&source).unwrap();
            assert_ne!(source, requested, "original saved proof remains unchanged");
            drop(client);
            worker.join().unwrap();
        }
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
                read_response(&mut frame.as_slice(), 100, false).is_err(),
                "mutation {mutation}"
            );
        }
        assert!(
            read_response(
                &mut ((MAX_FRAME as u32 + 1).to_be_bytes()).as_slice(),
                100,
                false
            )
            .is_err()
        );
        assert!(read_response(&mut [0, 0].as_slice(), 100, false).is_err());
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
}
