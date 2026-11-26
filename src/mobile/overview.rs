//! One read-only, connection-scoped subscription for a phone's overview.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use cagents_mobile_api as api;

use super::{Failure, Outbox, WatchSocket, daemon_lost, translate};
use crate::{
    control::{ControlClient, ControlError},
    model::HostSessionProof,
    terminal::ipc::MobileWatch,
};

pub(super) struct Watch {
    stop: WatchSocket,
    active: Arc<AtomicBool>,
    outbox: Outbox,
}

impl Watch {
    pub(super) fn start(
        socket: &Path,
        proof: &HostSessionProof,
        topic: MobileWatch,
        id: u64,
        outbox: &Outbox,
    ) -> Result<Self, Failure> {
        let socket = socket.to_owned();
        let proof = proof.clone();
        let stop = WatchSocket::default();
        let worker_stop = stop.clone();
        let active = Arc::new(AtomicBool::new(true));
        let watching = active.clone();
        let publish = outbox.clone();
        thread::Builder::new()
            .name("mobile-overview".into())
            .spawn(move || {
                let stopped = || api::Error {
                    code: api::ErrorCode::Unavailable,
                    message: "The watch stopped before it opened.".into(),
                };
                if !publish.wait_for_room(&watching) {
                    publish.error(Some(id), stopped());
                    return;
                }
                let open = || -> Result<_, Failure> {
                    let mut control = ControlClient::connect_observing_socket(&socket, |stream| {
                        worker_stop.register(stream)
                    })
                    .map_err(daemon_lost)?;
                    if control.proof() != &proof {
                        return Err(Failure::DaemonLost(
                            "The computer's cagents daemon was replaced.".into(),
                        ));
                    }
                    control.watch_mobile(topic).map_err(daemon_lost)?;
                    // Register before reading so a racing change arms another read.
                    let initial = snapshot(&mut control, topic)
                        .map_err(|error| Failure::DaemonLost(lost(&error)))?;
                    Ok((control, initial))
                };
                match open() {
                    Ok((mut control, event)) => {
                        publish.ok(id);
                        publish
                            .send_while_watched(&watching, api::ServerLine::Event(event.clone()));
                        if let Err(error) = follow(&mut control, topic, event, &publish, &watching)
                        {
                            publish.close_while_watched(&watching, lost(&error));
                        }
                    }
                    Err(_) if !watching.load(Ordering::Relaxed) => {
                        publish.error(Some(id), stopped())
                    }
                    Err(failure) => {
                        if let Some(message) = failure.reply(id, &publish) {
                            publish.close_while_watched(&watching, message);
                        }
                    }
                }
                worker_stop.stop();
            })
            .map_err(|error| {
                Failure::Refused(api::Error {
                    code: api::ErrorCode::Unavailable,
                    message: format!("Could not start following changes: {error}"),
                })
            })?;
        Ok(Self {
            stop,
            active,
            outbox: outbox.clone(),
        })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.outbox.stop_watch(&self.active);
        self.stop.stop();
    }
}

fn snapshot(control: &mut ControlClient, topic: MobileWatch) -> Result<api::Event, ControlError> {
    match topic {
        MobileWatch::Sessions => {
            let sessions = control
                .mobile_sessions()?
                .iter()
                .map(translate::session)
                .collect();
            Ok(api::Event::Sessions(api::Sessions { sessions }))
        }
        MobileWatch::Notifications => {
            let notices = translate::notices(&control.read_notifications()?);
            Ok(api::Event::Notifications(api::Notifications { notices }))
        }
        MobileWatch::Conversation(_) => unreachable!("conversation watches own their projection"),
    }
}

fn follow(
    control: &mut ControlClient,
    topic: MobileWatch,
    mut last: api::Event,
    outbox: &Outbox,
    active: &AtomicBool,
) -> Result<(), ControlError> {
    loop {
        control.next_mobile_change()?;
        if !outbox.wait_for_room(active) {
            return Ok(());
        }
        let event = snapshot(control, topic)?;
        if event != last {
            last = event.clone();
            outbox.send_while_watched(active, api::ServerLine::Event(event));
        }
    }
}

fn lost(error: &ControlError) -> String {
    format!("Lost the connection to the computer's cagents daemon: {error}")
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        sync::mpsc,
        time::Duration,
    };

    use super::*;
    use crate::{
        mobile::Outgoing,
        model::{BootId, HostIncarnation, HostSessionId, TerminalId},
        notifications,
        terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        },
    };

    #[test]
    fn unwatch_can_interrupt_the_daemon_handshake() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("daemon");
        let listener = UnixListener::bind(&path).unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("fixture-session"),
            boot_id: BootId::new("fixture-boot"),
            host_incarnation: HostIncarnation::new("fixture-daemon"),
        };
        let (outbox, lines) = Outbox::new();
        let deadline = Duration::from_secs(5);
        let watch = Watch::start(&path, &proof, MobileWatch::Notifications, 1, &outbox)
            .unwrap_or_else(|_| panic!("the watch started"));
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(deadline)).unwrap();
        let hello = read_frame::<ClientEnvelope>(&mut socket).unwrap().unwrap();
        assert!(matches!(hello.request, ClientRequest::Connect { .. }));

        // Keep the handshake pending. An unwatch closes the connection
        // without requiring the daemon to finish opening it.
        drop(watch);
        assert!(read_frame::<ClientEnvelope>(&mut socket).unwrap().is_none());
        assert!(matches!(
            lines.outgoing.recv_timeout(deadline).unwrap(),
            Outgoing::Line(api::ServerLine::Error { id: Some(1), .. })
        ));
        assert!(lines.outgoing.try_recv().is_err());
    }

    #[test]
    fn notification_watch_reads_on_change_and_closes_its_socket_on_unwatch() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("daemon");
        let listener = UnixListener::bind(&path).unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("fixture-session"),
            boot_id: BootId::new("fixture-boot"),
            host_incarnation: HostIncarnation::new("fixture-daemon"),
        };
        let (outbox, lines) = Outbox::new();
        let (changed, change) = mpsc::channel();
        let deadline = Duration::from_secs(5);
        let daemon_proof = proof.clone();
        let daemon = thread::spawn(move || {
            let (mut wire, _) = listener.accept().unwrap();
            wire.set_read_timeout(Some(deadline)).unwrap();
            let request =
                |wire: &mut UnixStream| read_frame::<ClientEnvelope>(wire).unwrap().unwrap();
            let reply = |wire: &mut UnixStream, id: Option<u64>, message| {
                write_frame(
                    wire,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: id,
                        proof: daemon_proof.clone(),
                        message,
                    },
                )
                .unwrap();
            };
            let response = |wire: &mut UnixStream, id, response| {
                reply(wire, Some(id), ServerMessage::Response { response });
            };
            let connect = request(&mut wire);
            response(
                &mut wire,
                connect.request_id,
                ServerResponse::Connected {
                    proof: daemon_proof.clone(),
                    resumed: Vec::new(),
                },
            );
            let subscribe = request(&mut wire);
            let topic = MobileWatch::Notifications;
            assert_eq!(subscribe.request, ClientRequest::WatchMobile { topic });
            response(&mut wire, subscribe.request_id, ServerResponse::Accepted);
            let read = request(&mut wire);
            assert_eq!(read.request, ClientRequest::ReadNotifications);

            // Invalidate while the initial read is pending. The bridge must
            // keep the edge even if it lands ahead of the correlated reply.
            reply(&mut wire, None, ServerMessage::MobileChanged { topic });
            response(
                &mut wire,
                read.request_id,
                ServerResponse::Notifications {
                    snapshot: notifications::Snapshot::default(),
                },
            );
            let reread = request(&mut wire);
            assert_eq!(reread.request, ClientRequest::ReadNotifications);
            response(
                &mut wire,
                reread.request_id,
                ServerResponse::Notifications {
                    snapshot: notifications::Snapshot {
                        notices: vec![notifications::Notice {
                            id: uuid::Uuid::new_v4(),
                            event: notifications::Event::session(
                                TerminalId::new(),
                                crate::agent::Provider::Claude,
                                "/fixture".into(),
                                notifications::Evidence {
                                    key: "request".into(),
                                    kind: notifications::Kind::Waiting,
                                },
                            ),
                            delivered: false,
                            acknowledged: false,
                            window: None,
                        }],
                        diagnostic: None,
                    },
                },
            );
            changed.send(()).unwrap();
            // After those two snapshots, nothing else is requested. Dropping
            // the watch shuts down the stream and unregisters it at the host.
            assert!(read_frame::<ClientEnvelope>(&mut wire).unwrap().is_none());
        });

        let watch = Watch::start(&path, &proof, MobileWatch::Notifications, 1, &outbox)
            .unwrap_or_else(|_| panic!("the watch opened"));
        change.recv_timeout(deadline).unwrap();
        let line = || match lines.outgoing.recv_timeout(deadline).unwrap() {
            Outgoing::Line(line) => line,
            Outgoing::Exit => panic!("the bridge exited"),
        };
        assert_eq!(line(), api::ServerLine::Ok { id: 1, page: None });
        assert!(
            matches!(line(), api::ServerLine::Event(api::Event::Notifications(event))
            if event.notices.is_empty())
        );
        assert!(
            matches!(line(), api::ServerLine::Event(api::Event::Notifications(event))
            if event.notices.len() == 1)
        );
        drop(watch);
        daemon.join().unwrap();
        assert!(lines.outgoing.try_recv().is_err());
    }
}
