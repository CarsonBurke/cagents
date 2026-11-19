//! The window's presence, preferences and acknowledgements reach the daemon
//! over one kept connection, not a new connection and handshake per change.
use super::*;
use crate::terminal_ipc::{
    ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
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

/// How a fixture daemon connection answers one request.
#[derive(Clone, Copy)]
enum Answer {
    Accept,
    /// Refuses it before any effect, as the daemon refuses an invalid
    /// presence.
    Refuse(&'static str),
    /// Reads it and closes the connection without answering, as a daemon
    /// that exits with the request in flight does.
    Close,
}

/// One fixture daemon connection: handshakes, then answers its requests in
/// turn from `answers`, accepting any past their end, and reports each one
/// it answers with the connection's number.
fn serve(
    mut peer: UnixStream,
    connection: usize,
    answers: Vec<Answer>,
    requests: mpsc::Sender<(usize, ClientRequest)>,
) {
    let reply = |peer: &mut UnixStream, request: &ClientEnvelope, message| {
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
    };
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
    reply(
        &mut peer,
        &hello,
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof(),
                resumed: Vec::new(),
            },
        },
    );
    let mut answers = answers.into_iter();
    while let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap() {
        let message = match answers.next().unwrap_or(Answer::Accept) {
            Answer::Accept => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            Answer::Refuse(reason) => ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                reason,
                false,
            )),
            Answer::Close => return,
        };
        reply(&mut peer, &request, message);
        requests.send((connection, request.request)).unwrap();
    }
}

fn focusing(focused: TerminalId) -> Presence {
    Presence {
        sessions: vec![focused],
        focused: Some(focused),
        ..Presence::default()
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
    /// A control thread whose daemon answers the requests on its first
    /// connection as `first`, and accepts everything else.
    fn new(first: Vec<Answer>) -> Self {
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
                    let answers = if connection == 0 {
                        first.clone()
                    } else {
                        Vec::new()
                    };
                    thread::spawn(move || serve(peer, connection, answers, report));
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
        self.pending.lock().unwrap().presence = Some(focusing(focused));
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
    let fixture = Fixture::new(Vec::new());
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
    let fixture = Fixture::new(vec![Answer::Accept, Answer::Close]);
    assert_eq!(fixture.publish(TerminalId::new()), 0);
    // The kept connection is still quiet when the second presence is sent
    // on it, and closes without answering.
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

/// A refusal is the daemon's answer, not a lost connection: it is reported
/// once, the refused presence stays pending, and the connection is kept.
#[test]
fn a_refusal_on_the_kept_connection_is_reported_and_keeps_it() {
    const REFUSAL: &str = "fixture refuses this presence";
    let fixture = Fixture::new(vec![Answer::Accept, Answer::Refuse(REFUSAL)]);
    assert_eq!(fixture.publish(TerminalId::new()), 0);
    let refused = TerminalId::new();
    fixture.pending.lock().unwrap().presence = Some(focusing(refused));
    fixture.wake.as_ref().unwrap().send(()).unwrap();
    // The refused presence, then its retry, both on the one connection. A
    // wake ends the retry delay early, which otherwise depends on the
    // process-wide daemon health.
    for attempt in 0..2 {
        let (connection, request) = fixture
            .requests
            .recv_timeout(Duration::from_secs(10))
            .expect("the refused presence is sent again");
        assert_eq!(connection, 0);
        assert!(
            matches!(&request, ClientRequest::NotificationPresence { presence }
                if presence.focused == Some(refused)),
            "{request:?}"
        );
        if attempt == 0 {
            let _ = fixture.wake.as_ref().unwrap().try_send(());
        }
    }
    assert_eq!(fixture.connects.load(Ordering::SeqCst), 1);
    // Reported before the retry was sent, and only once.
    let actions = fixture.finish();
    assert_eq!(actions.len(), 1, "{} actions", actions.len());
    assert!(
        matches!(&actions[0], Action::Failed(message) if message.contains(REFUSAL)),
        "the refusal was not reported as itself"
    );
}
