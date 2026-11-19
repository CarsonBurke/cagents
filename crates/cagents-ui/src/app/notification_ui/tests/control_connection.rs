//! The window's presence, preferences and acknowledgements reach the daemon
//! over one kept connection, not a new connection and handshake per change.
use super::*;
use crate::terminal_ipc::{
    ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse,
    read_frame, write_frame,
};
use std::{
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("control-fixture"),
        boot_id: crate::model::BootId::new("control-fixture"),
        host_incarnation: crate::model::HostIncarnation::new("control-fixture"),
    }
}

/// One fixture daemon connection: handshakes, then accepts every request,
/// reporting each with the connection's number. With `close_after` it
/// closes the connection once it has accepted that many, as a daemon that
/// exits does.
fn serve(
    mut peer: UnixStream,
    connection: usize,
    close_after: Option<usize>,
    requests: mpsc::Sender<(usize, ClientRequest)>,
) {
    let reply = |peer: &mut UnixStream, request: &ClientEnvelope, response| {
        write_frame(
            peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof(),
                message: ServerMessage::Response { response },
            },
        )
        .unwrap();
    };
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
    reply(
        &mut peer,
        &hello,
        ServerResponse::Connected {
            proof: proof(),
            resumed: Vec::new(),
        },
    );
    let mut accepted = 0;
    while close_after != Some(accepted)
        && let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap()
    {
        reply(&mut peer, &request, ServerResponse::Accepted);
        accepted += 1;
        requests.send((connection, request.request)).unwrap();
    }
}

struct Fixture {
    pending: Arc<Mutex<Pending>>,
    wake: Option<crossbeam_channel::Sender<()>>,
    requests: mpsc::Receiver<(usize, ClientRequest)>,
    connects: Arc<AtomicUsize>,
    actions: async_channel::Receiver<Action>,
    control: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    /// A control thread whose daemon closes its first connection after
    /// `close_first_after` requests, if given.
    fn new(close_first_after: Option<usize>) -> Self {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (wake, receiver) = crossbeam_channel::bounded(1);
        let (report, requests) = mpsc::channel();
        let connects = Arc::new(AtomicUsize::new(0));
        let (sender, actions) = async_channel::bounded(32);
        let control = {
            let pending = pending.clone();
            let connects = connects.clone();
            thread::spawn(move || {
                deliver_control(&receiver, &pending, &sender, || {
                    let connection = connects.fetch_add(1, Ordering::SeqCst);
                    let (stream, peer) = UnixStream::pair().unwrap();
                    let report = report.clone();
                    let close = close_first_after.filter(|_| connection == 0);
                    thread::spawn(move || serve(peer, connection, close, report));
                    ControlClient::from_stream(stream).map_err(|error| error.to_string())
                })
            })
        };
        Self {
            pending,
            wake: Some(wake),
            requests,
            connects,
            actions,
            control: Some(control),
        }
    }

    /// Publishes a presence focusing `focused` and returns the connection
    /// it arrived on.
    fn publish(&self, focused: TerminalId) -> usize {
        self.pending.lock().unwrap().presence = Some(Presence {
            sessions: vec![focused],
            focused: Some(focused),
            ..Presence::default()
        });
        self.wake.as_ref().unwrap().send(()).unwrap();
        let (connection, request) = self
            .requests
            .recv_timeout(Duration::from_secs(10))
            .expect("the presence reaches the daemon");
        assert!(
            matches!(&request, ClientRequest::NotificationPresence { presence }
                if presence.focused == Some(focused)),
            "{request:?}"
        );
        connection
    }

    fn finish(mut self) -> Vec<Action> {
        drop(self.wake.take());
        self.control.take().unwrap().join().unwrap();
        std::iter::from_fn(|| self.actions.try_recv().ok()).collect()
    }
}

#[test]
fn presence_changes_ride_one_kept_connection() {
    let fixture = Fixture::new(None);
    for _ in 0..3 {
        assert_eq!(fixture.publish(TerminalId::new()), 0);
    }
    fixture
        .pending
        .lock()
        .unwrap()
        .acknowledged
        .insert(uuid::Uuid::new_v4());
    fixture.wake.as_ref().unwrap().send(()).unwrap();
    let (connection, request) = fixture
        .requests
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(connection, 0);
    assert!(matches!(
        request,
        ClientRequest::AcknowledgeNotification { .. }
    ));
    assert_eq!(fixture.connects.load(Ordering::SeqCst), 1);
    let actions = fixture.finish();
    assert!(actions.is_empty(), "{} unexpected actions", actions.len());
}

/// A kept connection whose daemon has since closed it is replaced, so the
/// change still arrives and nothing reports a failure.
#[test]
fn a_kept_connection_the_daemon_closed_is_replaced_without_a_notice() {
    let fixture = Fixture::new(Some(1));
    assert_eq!(fixture.publish(TerminalId::new()), 0);
    // Whether the close lands before the quiet check or only after the
    // request is written, the change arrives over a new connection.
    assert_eq!(fixture.publish(TerminalId::new()), 1);
    assert_eq!(fixture.connects.load(Ordering::SeqCst), 2);
    let actions = fixture.finish();
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::Failed(_))),
        "a replaced connection reported a failure"
    );
}
