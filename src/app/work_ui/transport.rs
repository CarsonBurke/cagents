//! Preserve the original UI request context before any resolver, future or
//! watcher thread. Work envelopes use connection authentication, unlike the
//! separate explicit-authority ChannelWork API.
use crate::{agent_execution::Authority, control::ControlClient};
use std::path::{Path, PathBuf};

pub(super) struct Context {
    authority: Authority,
    remote: bool,
    no_start: bool,
}

impl Context {
    pub(super) fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
            no_start: crate::control::daemon_autostart_disabled()
                || std::env::var("CAGENTS_NO_START").is_ok_and(|value| {
                    matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
                }),
        }
    }

    pub(super) fn connect(&self, may_start: bool) -> Result<ControlClient, String> {
        self.connect_with(
            may_start,
            || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
            || crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string()),
            |socket| ControlClient::connect(socket).map_err(|error| error.to_string()),
        )
    }

    fn connect_with(
        &self,
        may_start: bool,
        existing: impl FnOnce() -> Result<PathBuf, String>,
        start: impl FnOnce() -> Result<PathBuf, String>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, String>,
    ) -> Result<ControlClient, String> {
        let scoped = self.authority.token().is_some();
        // Local-session authority must never enter a selected remote bridge,
        // including its resolver/SSH discovery, merely to obtain a refusal.
        if scoped && self.remote {
            return Err("An inherited session capability cannot use a remote Work board".into());
        }
        let socket = if may_start && !scoped && !self.no_start {
            start()?
        } else {
            existing()?
        };
        let mut client = connect(&socket)?;
        if scoped {
            // Pin retains the unchanged authority-free Work envelope. Binding
            // its connection first reaches the daemon's existing refusal;
            // global Board/WatchWork reads remain intentionally permitted.
            client
                .authenticate(self.authority.clone())
                .map_err(|error| error.to_string())?;
        }
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        },
        work::{Board, Column, Request, Response, Scope},
    };
    use std::{cell::Cell, os::unix::net::UnixStream, thread, time::Duration};

    fn context(scoped: bool, remote: bool, no_start: bool) -> Context {
        Context {
            authority: if scoped {
                Authority::session("fixture-original-scope".into())
            } else {
                Authority::default()
            },
            remote,
            no_start,
        }
    }

    fn streams() -> (UnixStream, UnixStream) {
        let pair = UnixStream::pair().unwrap();
        for stream in [&pair.0, &pair.1] {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
        pair
    }

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("work-transport"),
            boot_id: BootId::new("work-boot"),
            host_incarnation: HostIncarnation::new("work-incarnation"),
        }
    }

    fn read(peer: &mut UnixStream) -> ClientEnvelope {
        read_frame(peer)
            .unwrap()
            .expect("one actual framed request")
    }

    fn answer(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
        write_frame(
            peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof(),
                message,
            },
        )
        .unwrap();
    }

    fn response(peer: &mut UnixStream, request: &ClientEnvelope, response: ServerResponse) {
        answer(peer, request, ServerMessage::Response { response });
    }

    fn handshake(peer: &mut UnixStream) {
        let hello = read(peer);
        assert!(
            matches!(hello.request, ClientRequest::Connect { expected_host: None, ref resume } if resume.is_empty())
        );
        response(
            peer,
            &hello,
            ServerResponse::Connected {
                proof: proof(),
                resumed: vec![],
            },
        );
    }

    fn captured_client(context: &Context, stream: UnixStream) -> Result<ControlClient, String> {
        context.connect_with(
            false,
            || Ok("/fixture/existing.sock".into()),
            || panic!("watch/scoped request must not start"),
            |path| {
                assert_eq!(path, Path::new("/fixture/existing.sock"));
                ControlClient::from_stream(stream).map_err(|error| error.to_string())
            },
        )
    }

    fn board() -> Board {
        Board {
            generated_at_unix_ms: 7,
            stale_after_days: 7,
            projects: vec![],
            items: vec![],
            linear_unavailable: None,
        }
    }

    #[test]
    fn scoped_remote_refuses_before_resolver_start_or_connect_for_requests_and_watchers() {
        for may_start in [false, true] {
            let result = context(true, true, false).connect_with(
                may_start,
                || panic!("must not resolve remote socket/SSH"),
                || panic!("must not start remote daemon"),
                |_| panic!("must not connect remote endpoint"),
            );
            assert!(result.err().unwrap().contains("remote Work board"));
        }
    }

    #[test]
    fn scoped_or_no_start_local_uses_only_existing_socket_and_user_start_policy_is_preserved() {
        for (scoped, remote, no_start, may_start, expects_start) in [
            (true, false, false, true, false),
            (false, false, true, true, false),
            (false, false, false, false, false),
            (false, false, false, true, true),
            (false, true, false, true, true),
        ] {
            let resolved = Cell::new(0);
            let started = Cell::new(0);
            let connected = Cell::new(0);
            // Ordinary no-token remote users keep the original selected-device
            // start path too; no blanket remote policy is introduced.
            let context = context(scoped, remote, no_start);
            let result = context.connect_with(
                may_start,
                || {
                    resolved.set(resolved.get() + 1);
                    Err("existing socket unavailable".into())
                },
                || {
                    started.set(started.get() + 1);
                    Err("ordinary start result".into())
                },
                |_| {
                    connected.set(connected.get() + 1);
                    panic!("failed resolver cannot connect")
                },
            );
            assert!(result.is_err());
            assert_eq!(started.get(), usize::from(expects_start));
            assert_eq!(resolved.get(), usize::from(!expects_start));
            assert_eq!(
                connected.get(),
                0,
                "no fallback or retry after a resolver failure"
            );
        }
    }

    #[test]
    fn scoped_pin_authenticates_first_then_preserves_the_exact_work_envelope_and_refusal() {
        let (stream, mut peer) = streams();
        let server = thread::spawn(move || {
            handshake(&mut peer);
            let auth = read(&mut peer);
            assert!(
                matches!(&auth.request, ClientRequest::Authenticate { authority } if authority == &context(true,false,false).authority)
            );
            response(&mut peer, &auth, ServerResponse::Accepted);
            let pin = read(&mut peer);
            assert!(
                matches!(&pin.request, ClientRequest::Work(Request::Pin { scope: Scope::Project(path), item, column: Some(Column::Stale) }) if path == Path::new("/fixture/project") && item == "original-item")
            );
            assert_eq!(pin.proof.as_ref(), Some(&proof()));
            answer(
                &mut peer,
                &pin,
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "scoped Work pin refused",
                    false,
                )),
            );
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no User retry after refusal"
            );
        });
        let mut client = captured_client(&context(true, false, false), stream).unwrap();
        let failure = client
            .pin_work_card(
                "/fixture/project".into(),
                "original-item".into(),
                Some(Column::Stale),
            )
            .unwrap_err();
        assert!(failure.to_string().contains("scoped Work pin refused"));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn refused_authentication_never_sends_a_work_request_or_retries_as_user() {
        let (stream, mut peer) = streams();
        let server = thread::spawn(move || {
            handshake(&mut peer);
            let auth = read(&mut peer);
            assert!(matches!(auth.request, ClientRequest::Authenticate { .. }));
            answer(
                &mut peer,
                &auth,
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "original capability unavailable",
                    false,
                )),
            );
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no Work request or default reconnect after failed auth"
            );
        });
        assert!(
            captured_client(&context(true, false, false), stream)
                .err()
                .unwrap()
                .contains("original capability unavailable")
        );
        server.join().unwrap();
    }

    #[test]
    fn global_refresh_and_watch_keep_their_wire_scope_for_authenticated_sessions_and_ordinary_users()
     {
        for scoped in [false, true] {
            let (stream, mut peer) = streams();
            let server = thread::spawn(move || {
                handshake(&mut peer);
                if scoped {
                    let auth = read(&mut peer);
                    assert!(
                        matches!(&auth.request, ClientRequest::Authenticate { authority } if authority == &context(true,false,false).authority)
                    );
                    response(&mut peer, &auth, ServerResponse::Accepted);
                }
                let refresh = read(&mut peer);
                assert!(matches!(
                    refresh.request,
                    ClientRequest::Work(Request::Board {
                        scope: Scope::All,
                        fresh: true
                    })
                ));
                response(
                    &mut peer,
                    &refresh,
                    ServerResponse::Work(Response::Board(board())),
                );
                if !scoped {
                    let pin = read(&mut peer);
                    assert!(
                        matches!(&pin.request, ClientRequest::Work(Request::Pin { scope: Scope::Project(project), item, column: None }) if project == Path::new("/fixture/project") && item == "original-item")
                    );
                    response(
                        &mut peer,
                        &pin,
                        ServerResponse::Work(Response::Pinned(Box::new(crate::work::Item {
                            project: "/fixture/project".into(),
                            project_name: "Fixture".into(),
                            key: crate::work::ItemKey::Branch("original-item".into()),
                            branch: Some("original-item".into()),
                            worktree: None,
                            head: None,
                            comparison: None,
                            dirty: None,
                            pull_request: None,
                            linear: None,
                            sessions: vec![],
                            last_activity: None,
                            error: None,
                            column: Column::Active,
                            derived: Column::Active,
                            pinned: false,
                        }))),
                    );
                }
                let watch = read(&mut peer);
                assert!(matches!(watch.request, ClientRequest::WatchWork));
                response(
                    &mut peer,
                    &watch,
                    ServerResponse::Work(Response::Watching {
                        revision: 4,
                        board: Box::new(board()),
                    }),
                );
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "existing hangup closes the same scoped/user stream"
                );
            });
            let mut client = captured_client(&context(scoped, false, false), stream).unwrap();
            client.refresh_work_board().unwrap();
            if !scoped {
                client
                    .pin_work_card("/fixture/project".into(), "original-item".into(), None)
                    .unwrap();
            }
            let (revision, current) = client.watch_work().unwrap();
            assert_eq!(revision, 4);
            assert_eq!(current, board());
            let hangup = client.hangup().unwrap();
            hangup.hang_up();
            assert!(client.next_work_board(revision).is_err());
            drop(client);
            server.join().unwrap();
        }
    }
}
