//! Read-only inspection using this build's exact source protocol and DTOs.
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
    deadline: Instant,
}

impl Client {
    pub(super) fn connect(
        socket: &Path,
        expected: &HostSessionProof,
        deadline: Instant,
    ) -> Result<Self, String> {
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
        let request_id = self.next;
        self.next += 1;
        let mut stream = DeadlineStream {
            stream: &mut self.stream,
            deadline: self.deadline,
        };
        ipc::write_frame(
            &mut stream,
            &ClientEnvelope {
                version: ipc::PROTOCOL_VERSION,
                request_id,
                proof: authenticated.then(|| self.proof.clone()),
                request,
            },
        )
        .map_err(|_| "Cannot send source inspection")?;
        let envelope: ServerEnvelope = ipc::read_frame_with_limit(&mut stream, MAX_FRAME)
            .map_err(|_| "Source response is unsupported or unverifiable")?
            .ok_or("Source inspection disconnected")?;
        if envelope.version != ipc::PROTOCOL_VERSION
            || envelope.request_id != Some(request_id)
            || envelope.proof != self.proof
        {
            return Err("Source inspection ownership, protocol or request identity changed".into());
        }
        match envelope.message {
            ServerMessage::Response { response } => Ok(response),
            _ => Err("Source response is outside the read-only inspection contract".into()),
        }
    }

    pub(super) fn list(&mut self) -> Result<Vec<Session>, String> {
        match self.request(ClientRequest::ChatList, true)? {
            ServerResponse::ChatListed { sessions } if sessions.len() <= 128 => Ok(sessions),
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

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("private-source"),
            boot_id: crate::model::BootId::new("private-boot"),
            host_incarnation: crate::model::HostIncarnation::new("private-owner"),
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
            deadline: Instant::now() - Duration::from_millis(1),
        };
        let refused = client.list().is_err();
        let observed = peer.read(&mut [0u8; 1]).unwrap_err().kind();
        assert!(refused);
        assert_eq!(observed, io::ErrorKind::WouldBlock);
    }
}
