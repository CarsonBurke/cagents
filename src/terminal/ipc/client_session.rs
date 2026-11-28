//! Socket establishment and reply validation for daemon clients.
//!
//! The clients choose their deadlines, expected host, and reconnect policy.

use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use super::{ProtocolError, ServerEnvelope, ServerMessage, ServerResponse};
use crate::model::HostSessionProof;

/// Linux AF_UNIX backlog pressure returns EAGAIN without connecting. Retry
/// fresh nonblocking sockets within the same establishment deadline, rather
/// than treating writability/SO_ERROR as evidence that EAGAIN completed.
/// Darwin answers that pressure with a refusal, which callers take to mean no
/// daemon runs; see [`crate::descriptor::retry_refusal`].
pub(crate) fn connect_until(
    path: &Path,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> io::Result<UnixStream> {
    connect_with_deadline(path, deadline, cancelled, false)
}

/// An authenticated owner may bind before accepting any clients. Darwin can
/// refuse a full backlog throughout that recovery, so wait within the same
/// deadline while the exact process remains alive.
#[cfg(any(
    target_os = "macos",
    all(test, not(any(feature = "host", feature = "ui")))
))]
pub(crate) fn connect_until_with_owner(
    path: &Path,
    deadline: Instant,
    owner: &crate::pinned_process::PinnedProcess,
) -> io::Result<UnixStream> {
    let stream = connect_with_deadline(
        path,
        deadline,
        || !matches!(owner.exited(), Ok(false)),
        true,
    )?;
    let peer = crate::daemon::peer_credentials(&stream)?;
    if peer.uid != crate::daemon::current_uid() || peer.pid != owner.pid() || owner.exited()? {
        return Err(io::Error::other(
            "the selected daemon's listener changed during startup",
        ));
    }
    Ok(stream)
}

fn connect_with_deadline(
    path: &Path,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
    owner_is_known: bool,
) -> io::Result<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid daemon Unix socket path",
        ));
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    let refusing_since = Instant::now();
    let mut delay = Duration::from_millis(1);
    loop {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "daemon socket connection cancelled",
            ));
        }
        remaining(deadline, "daemon socket connection deadline elapsed")?;
        let stream = UnixStream::from(crate::descriptor::unix_socket(libc::SOCK_STREAM, true)?);
        let fd = stream.as_raw_fd();
        // SAFETY: address is initialized with its valid length and the
        // descriptor belongs exclusively to this connection attempt.
        let result = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _,
            )
        };
        if result == 0 {
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        let refused_by_owner = owner_is_known && error.kind() == io::ErrorKind::ConnectionRefused;
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) && !refused_by_owner
            && !crate::descriptor::retry_refusal(&error, refusing_since)
        {
            return Err(error);
        }
        // EAGAIN has not queued a connection. Close this attempt before
        // retrying, preserving bounded descriptor use and the selected endpoint.
        drop(stream);
        thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
        delay = (delay * 2).min(Duration::from_millis(20));
    }
}

/// Recompute the socket timeout before each partial read. Polling callers can
/// set a shorter idle interval and retry with the same codec and deadline.
pub(crate) struct DeadlineReader<'a> {
    stream: &'a mut UnixStream,
    pub(crate) deadline: Instant,
    poll_interval: Option<Duration>,
}

impl<'a> DeadlineReader<'a> {
    pub(crate) fn new(stream: &'a mut UnixStream, deadline: Instant) -> Self {
        Self {
            stream,
            deadline,
            poll_interval: None,
        }
    }

    pub(crate) fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = Some(interval);
        self
    }
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = remaining(self.deadline, "daemon response deadline expired")?;
        let timeout = self
            .poll_interval
            .map_or(remaining, |interval| interval.min(remaining));
        crate::descriptor::set_read_timeout_or_hangup(self.stream, Some(timeout))?;
        self.stream.read(buffer)
    }
}

/// Bound every partial write of a request against the same overall deadline.
pub(crate) struct DeadlineWriter<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl<'a> DeadlineWriter<'a> {
    pub(crate) fn new(stream: &'a mut UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }
}

impl Write for DeadlineWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = remaining(self.deadline, "daemon request deadline expired")?;
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn remaining(deadline: Instant, message: &'static str) -> io::Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, message));
    }
    Ok(duration)
}

#[derive(Debug, PartialEq)]
pub(crate) enum ReplyError {
    UnsupportedVersion { actual: u16 },
    RequestIdMismatch { expected: u64, actual: Option<u64> },
    IncompleteProof,
    ProofMismatch,
}

#[derive(Debug, PartialEq)]
pub(crate) enum HandshakeError<'a> {
    InvalidReply(ReplyError),
    Remote(&'a ProtocolError),
    UnexpectedMessage(&'a ServerMessage),
}

/// Validate one connect reply, checking its envelope before its message.
/// Protocol qualification and any expected host remain the caller's policy.
pub(crate) fn validate_handshake(
    envelope: &ServerEnvelope,
    protocol: u16,
    request_id: u64,
) -> Result<&HostSessionProof, HandshakeError<'_>> {
    validate_reply(envelope, protocol, request_id).map_err(HandshakeError::InvalidReply)?;
    match &envelope.message {
        ServerMessage::Response {
            response: ServerResponse::Connected { proof, .. },
        } => Ok(proof),
        ServerMessage::Error(error) => Err(HandshakeError::Remote(error)),
        message => Err(HandshakeError::UnexpectedMessage(message)),
    }
}

/// Validate the fields common to a connect response and subsequent replies.
pub(crate) fn validate_reply(
    envelope: &ServerEnvelope,
    protocol: u16,
    request_id: u64,
) -> Result<(), ReplyError> {
    if envelope.version != protocol {
        return Err(ReplyError::UnsupportedVersion {
            actual: envelope.version,
        });
    }
    if envelope.request_id != Some(request_id) {
        return Err(ReplyError::RequestIdMismatch {
            expected: request_id,
            actual: envelope.request_id,
        });
    }
    if !envelope.proof.is_complete() {
        return Err(ReplyError::IncompleteProof);
    }
    if let ServerMessage::Response {
        response: ServerResponse::Connected { proof, .. },
    } = &envelope.message
    {
        if !proof.is_complete() {
            return Err(ReplyError::IncompleteProof);
        }
        if proof != &envelope.proof {
            return Err(ReplyError::ProofMismatch);
        }
    }
    Ok(())
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal::ipc::{PROTOCOL_VERSION, ProtocolErrorCode},
    };

    #[test]
    fn expired_deadline_does_not_queue_a_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        let listener = crate::descriptor::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        let error = connect_until(&path, Instant::now(), || false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let error = crate::descriptor::accept(&listener).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn dialing_checks_cancellation_before_retrying_a_full_backlog() {
        use std::cell::Cell;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        let listener = crate::descriptor::bind(&path).unwrap();
        // SAFETY: the fixture owns this listening descriptor.
        assert_eq!(
            unsafe { libc::listen(listener.as_raw_fd(), crate::descriptor::ONE_PENDING_BACKLOG) },
            0
        );
        let _pending =
            connect_until(&path, Instant::now() + Duration::from_secs(1), || false).unwrap();

        let checked = Cell::new(false);
        let error = connect_until(&path, Instant::now() + Duration::from_secs(5), || {
            checked.replace(true)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn a_refused_selected_listener_waits_within_its_exact_owner_budget() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        drop(crate::descriptor::bind(&socket).unwrap());
        let owner = crate::pinned_process::PinnedProcess::open(std::process::id()).unwrap();

        let ordinary =
            connect_until(&socket, Instant::now() + Duration::from_secs(2), || false).unwrap_err();
        assert_eq!(ordinary.kind(), io::ErrorKind::ConnectionRefused);
        let until = Instant::now() + Duration::from_millis(250);
        let busy = connect_until_with_owner(&socket, until, &owner).unwrap_err();
        assert_eq!(busy.kind(), io::ErrorKind::TimedOut);

        std::fs::remove_file(&socket).unwrap();
        let _listener = crate::descriptor::bind(&socket).unwrap();
        connect_until_with_owner(&socket, Instant::now() + Duration::from_secs(1), &owner).unwrap();
    }

    fn proof(session: &str, boot: &str, incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(session),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn connected() -> ServerEnvelope {
        let proof = proof("session", "boot", "incarnation");
        ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(7),
            proof: proof.clone(),
            message: ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof,
                    resumed: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn accepts_matching_proofs_only_at_the_selected_protocol() {
        let mut envelope = connected();
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Ok(&envelope.proof)
        );
        let selected = PROTOCOL_VERSION - 1;
        envelope.version = selected;
        assert_eq!(
            validate_handshake(&envelope, selected, 7),
            Ok(&envelope.proof)
        );
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::InvalidReply(
                ReplyError::UnsupportedVersion { actual: selected }
            ))
        );
        envelope.version = PROTOCOL_VERSION;
        assert_eq!(
            validate_handshake(&envelope, selected, 7),
            Err(HandshakeError::InvalidReply(
                ReplyError::UnsupportedVersion {
                    actual: PROTOCOL_VERSION
                }
            ))
        );
    }

    #[test]
    fn rejects_missing_and_incorrect_request_ids() {
        for actual in [None, Some(0), Some(1), Some(8)] {
            let mut envelope = connected();
            envelope.request_id = actual;
            assert_eq!(
                validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                Err(HandshakeError::InvalidReply(
                    ReplyError::RequestIdMismatch {
                        expected: 7,
                        actual
                    }
                ))
            );
        }
    }

    #[test]
    fn rejects_each_incomplete_envelope_or_body_proof_field() {
        for incomplete in [
            proof("", "boot", "incarnation"),
            proof("session", "", "incarnation"),
            proof("session", "boot", ""),
        ] {
            for in_body in [false, true] {
                let mut envelope = connected();
                if in_body {
                    envelope.message = ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: incomplete.clone(),
                            resumed: Vec::new(),
                        },
                    };
                } else {
                    envelope.proof = incomplete.clone();
                }
                assert_eq!(
                    validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                    Err(HandshakeError::InvalidReply(ReplyError::IncompleteProof))
                );
            }
        }
    }

    #[test]
    fn rejects_each_body_envelope_proof_mismatch() {
        for mismatched in [
            proof("other", "boot", "incarnation"),
            proof("session", "other", "incarnation"),
            proof("session", "boot", "other"),
        ] {
            let mut envelope = connected();
            envelope.message = ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: mismatched,
                    resumed: Vec::new(),
                },
            };
            assert_eq!(
                validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                Err(HandshakeError::InvalidReply(ReplyError::ProofMismatch))
            );
        }
    }

    #[test]
    fn validates_version_then_correlation_then_proof_before_remote_errors() {
        let mut envelope = connected();
        let error = ProtocolError::new(ProtocolErrorCode::InvalidRequest, "refused", true);
        envelope.message = ServerMessage::Error(error.clone());
        envelope.version = PROTOCOL_VERSION + 1;
        envelope.request_id = None;
        envelope.proof = proof("", "boot", "incarnation");
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::InvalidReply(
                ReplyError::UnsupportedVersion {
                    actual: PROTOCOL_VERSION + 1
                }
            ))
        );
        envelope.version = PROTOCOL_VERSION;
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::InvalidReply(
                ReplyError::RequestIdMismatch {
                    expected: 7,
                    actual: None
                }
            ))
        );
        envelope.request_id = Some(7);
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::InvalidReply(ReplyError::IncompleteProof))
        );
        envelope.proof = proof("session", "boot", "incarnation");
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::Remote(&error))
        );
    }

    #[test]
    fn rejects_streaming_and_non_connected_responses() {
        for message in [
            ServerMessage::HostShuttingDown,
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        ] {
            let mut envelope = connected();
            envelope.message = message;
            assert_eq!(
                validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                Err(HandshakeError::UnexpectedMessage(&envelope.message))
            );
        }
    }
}
