//! Process-local transport health. Only bounded daemon requests contribute;
//! an idle subscription socket has no reply outstanding and is not a probe.
use crate::{control::ControlError, terminal::ipc::FrameError};
use std::{
    io,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};
use tokio::sync::watch;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum State {
    #[default]
    Responsive,
    Unresponsive {
        since: SystemTime,
        timeout: Duration,
    },
    Unreachable {
        since: SystemTime,
    },
    Incompatible {
        since: SystemTime,
        actual: u16,
    },
}

impl State {
    pub(crate) fn unavailable(&self) -> bool {
        !matches!(self, Self::Responsive)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Request(u64);
struct Observation {
    state: State,
    replies: u64,
}
struct Monitor {
    state: Mutex<Observation>,
    sender: watch::Sender<State>,
}
impl Monitor {
    fn new() -> Self {
        let (sender, _) = watch::channel(State::Responsive);
        Self {
            state: Mutex::new(Observation {
                state: State::Responsive,
                replies: 0,
            }),
            sender,
        }
    }
    fn begin(&self) -> Request {
        Request(self.state.lock().unwrap_or_else(|p| p.into_inner()).replies)
    }
    #[cfg(test)]
    fn observe(&self, outcome: Outcome, now: SystemTime) {
        self.observe_request(outcome, now, self.begin());
    }
    fn observe_request(&self, outcome: Outcome, now: SystemTime, request: Request) {
        let mut observation = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // A compatible response already in flight before a newly observed
        // mismatch can belong to the previous daemon. It cannot clear that
        // mismatch. Preserve ordinary transport recovery's existing behavior.
        let compatible = matches!(outcome, Outcome::Responsive);
        if request.0 != observation.replies
            && (!compatible || matches!(observation.state, State::Incompatible { .. }))
        {
            return;
        }
        if compatible || matches!(outcome, Outcome::Incompatible(_)) {
            observation.replies = observation.replies.wrapping_add(1);
        }
        let state = &mut observation.state;
        let next = match outcome {
            Outcome::Responsive => State::Responsive,
            Outcome::TimedOut(_) if matches!(state, State::Incompatible { .. }) => (*state).clone(),
            Outcome::TimedOut(timeout) => match &*state {
                State::Unresponsive { .. } => (*state).clone(),
                _ => State::Unresponsive {
                    since: now,
                    timeout,
                },
            },
            Outcome::Incompatible(actual) => State::Incompatible {
                since: match &*state {
                    State::Incompatible {
                        since,
                        actual: previous,
                    } if *previous == actual => *since,
                    _ => now,
                },
                actual,
            },
            // A dropped incompatible connection is not recovery. Only a
            // validated compatible response clears the actionable mismatch.
            Outcome::Unreachable if matches!(state, State::Incompatible { .. }) => (*state).clone(),
            Outcome::Unreachable => State::Unreachable {
                since: match &*state {
                    State::Unreachable { since } => *since,
                    _ => now,
                },
            },
        };
        if *state != next {
            tracing::warn!(?next, "daemon transport health changed");
            *state = next.clone();
            self.sender.send_replace(next);
        }
    }
}
#[derive(Clone, Copy)]
enum Outcome {
    Responsive,
    TimedOut(Duration),
    Unreachable,
    Incompatible(u16),
}
fn monitor() -> &'static Monitor {
    static MONITOR: OnceLock<Monitor> = OnceLock::new();
    MONITOR.get_or_init(Monitor::new)
}
pub(crate) fn begin() -> Request {
    monitor().begin()
}
pub(crate) fn current() -> State {
    monitor().sender.borrow().clone()
}
pub(crate) fn stream() -> impl cosmic::iced::futures::Stream<Item = State> {
    cosmic::iced::futures::stream::unfold(
        (monitor().sender.subscribe(), true),
        |(mut receiver, first)| async move {
            if !first {
                receiver.changed().await.ok()?;
            }
            let state = receiver.borrow_and_update().clone();
            Some((state, (receiver, false)))
        },
    )
}
fn selected_socket(path: &Path) -> bool {
    crate::terminal::ipc::daemon_socket_path().is_ok_and(|selected| selected == path)
}
fn selected_stream(stream: &UnixStream) -> bool {
    stream
        .peer_addr()
        .ok()
        .and_then(|a| a.as_pathname().map(Path::to_owned))
        .is_some_and(|path| selected_socket(&path))
}
fn timeout(error: &FrameError) -> bool {
    matches!(error, FrameError::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}
fn control_outcome<T>(result: &Result<T, ControlError>) -> Option<Outcome> {
    match result {
        Ok(_) | Err(ControlError::Remote { .. }) => Some(Outcome::Responsive),
        Err(ControlError::UnsupportedVersion { actual }) => Some(Outcome::Incompatible(*actual)),
        Err(ControlError::ReplyTimeout { timeout, .. }) => Some(Outcome::TimedOut(*timeout)),
        Err(ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Some(Outcome::Unreachable)
        }
        Err(ControlError::ConnectionClosed) => Some(Outcome::Unreachable),
        Err(ControlError::Frame(FrameError::Io(error)))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Some(Outcome::Unreachable)
        }
        _ => None,
    }
}
pub(crate) fn connection<T>(path: &Path, result: &Result<T, ControlError>, request: Request) {
    if selected_socket(path)
        && let Some(outcome) = control_outcome(result)
    {
        monitor().observe_request(outcome, SystemTime::now(), request);
    }
}
/// Report only an explicitly decoded version; malformed frames do not prove
/// what protocol the peer speaks. Socket selection fences remote/other hosts.
pub(crate) fn incompatible(path: &Path, actual: u16, request: Request) {
    if selected_socket(path) {
        monitor().observe_request(Outcome::Incompatible(actual), SystemTime::now(), request);
    }
}
pub(crate) fn incompatible_stream(stream: &UnixStream, actual: u16, request: Request) {
    if selected_stream(stream) {
        monitor().observe_request(Outcome::Incompatible(actual), SystemTime::now(), request);
    }
}
pub(crate) fn connection_io(path: &Path, result: &io::Result<UnixStream>, request: Request) {
    if selected_socket(path)
        && let Err(error) = result
        && matches!(
            error.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        )
    {
        monitor().observe_request(Outcome::Unreachable, SystemTime::now(), request);
    }
}
pub(crate) fn request<T>(stream: &UnixStream, result: &Result<T, ControlError>, request: Request) {
    if selected_stream(stream)
        && let Some(outcome) = control_outcome(result)
    {
        monitor().observe_request(outcome, SystemTime::now(), request);
    }
}
pub(crate) fn frame(stream: &UnixStream, error: Option<&FrameError>, request: Request) {
    if selected_stream(stream) {
        let outcome = match error {
            None => Some(Outcome::Responsive),
            Some(FrameError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                Some(Outcome::Unreachable)
            }
            Some(error) if timeout(error) => Some(Outcome::TimedOut(
                stream
                    .read_timeout()
                    .ok()
                    .flatten()
                    .unwrap_or(Duration::from_secs(5)),
            )),
            _ => None,
        };
        if let Some(outcome) = outcome {
            monitor().observe_request(outcome, SystemTime::now(), request);
        }
    }
}
pub(crate) fn disconnected(stream: &UnixStream, request: Request) {
    if selected_stream(stream) {
        monitor().observe_request(Outcome::Unreachable, SystemTime::now(), request);
    }
}
pub(crate) fn retry_delay() -> Duration {
    if current().unavailable() {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(2)
    }
}
/// Suppress only transport diagnostics replaced by the persistent banner.
/// A provider or filesystem error remains a feature-level notice; a confirmed
/// protocol mismatch has its own persistent explanation and recovery action.
pub(crate) fn replaces_notice(message: &str) -> bool {
    replaces_notice_for(&current(), message)
}
fn replaces_notice_for(state: &State, message: &str) -> bool {
    let ipc_failure = message
        .split_once("I/O error while transferring IPC frame: ")
        .is_some_and(|(_, error)| {
            let error = error.to_ascii_lowercase();
            [
                "resource temporarily unavailable",
                "connection reset",
                "broken pipe",
                "failed to fill whole buffer",
                "unexpected end of file",
            ]
            .iter()
            .any(|transport| error.starts_with(transport))
        });
    let protocol_failure = matches!(state, State::Incompatible { .. })
        && (message.contains("daemon protocol version")
            || message.contains("Daemon version mismatch"));
    protocol_failure
        || state.unavailable()
            && (ipc_failure
                || message.contains("Timed out waiting for the daemon response")
                || message.contains("the daemon did not answer within")
                || message.contains("could not connect to the daemon: Connection refused")
                || message.contains("the daemon closed the connection before responding")
                || message.contains("Daemon disconnected"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_health_replaces_transport_notices_without_hiding_file_failures() {
        let offline = State::Unreachable {
            since: SystemTime::UNIX_EPOCH,
        };
        for error in [
            "Connection reset by peer (os error 104)",
            "Broken pipe (os error 32)",
            "failed to fill whole buffer",
        ] {
            let notice = format!(
                "Notification service unavailable: I/O error while transferring IPC frame: {error}"
            );
            assert!(replaces_notice_for(&offline, &notice));
            assert!(!replaces_notice_for(&State::Responsive, &notice));
            assert!(!replaces_notice_for(
                &offline,
                &format!("File save failed: {error}")
            ));
        }
        assert!(!replaces_notice_for(
            &offline,
            "I/O error while transferring IPC frame: Permission denied (os error 13)"
        ));
        assert!(!replaces_notice_for(
            &offline,
            "the daemon protocol version is unsupported"
        ));
    }
    #[test]
    fn decoded_mismatch_persists_through_disconnect_and_clears_on_compatible_reply() {
        let monitor = Monitor::new();
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let version = crate::terminal_ipc::PROTOCOL_VERSION + 1;
        let mismatch: Result<(), _> = Err(ControlError::UnsupportedVersion { actual: version });
        let previous_daemon_reply = monitor.begin();
        monitor.observe(control_outcome(&mismatch).unwrap(), first);
        monitor.observe_request(Outcome::Responsive, first, previous_daemon_reply);
        let expected = State::Incompatible {
            since: first,
            actual: version,
        };
        assert_eq!(*monitor.sender.borrow(), expected);
        let mut updates = monitor.sender.subscribe();
        for outcome in [
            Outcome::Incompatible(version),
            Outcome::Unreachable,
            Outcome::TimedOut(Duration::from_secs(5)),
        ] {
            monitor.observe(outcome, first + Duration::from_secs(30));
            assert_eq!(*monitor.sender.borrow(), expected);
            assert!(!updates.has_changed().unwrap());
        }
        assert!(replaces_notice_for(
            &expected,
            "Notification service unavailable: daemon protocol version 99 is unsupported; expected 66"
        ));
        assert!(!replaces_notice_for(
            &expected,
            "File save failed: Permission denied"
        ));
        assert!(!replaces_notice_for(
            &expected,
            "Daemon identity changed during native chat attachment"
        ));
        let stale = monitor.begin();
        monitor.observe(Outcome::Responsive, first + Duration::from_secs(31));
        monitor.observe_request(
            Outcome::Incompatible(version),
            first + Duration::from_secs(32),
            stale,
        );
        assert_eq!(*monitor.sender.borrow(), State::Responsive);
        assert!(updates.has_changed().unwrap());
        updates.borrow_and_update();
    }

    #[test]
    fn repeated_timeouts_keep_the_original_since_and_one_transition() {
        let monitor = Monitor::new();
        let mut receiver = monitor.sender.subscribe();
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        monitor.observe(Outcome::TimedOut(Duration::from_secs(5)), first);
        assert!(receiver.has_changed().unwrap());
        receiver.borrow_and_update();
        monitor.observe(
            Outcome::TimedOut(Duration::from_secs(5)),
            first + Duration::from_secs(30),
        );
        assert!(!receiver.has_changed().unwrap());
        assert_eq!(
            *monitor.sender.borrow(),
            State::Unresponsive {
                since: first,
                timeout: Duration::from_secs(5)
            }
        );
        monitor.observe(Outcome::Responsive, first + Duration::from_secs(31));
        assert_eq!(*monitor.sender.borrow(), State::Responsive);
    }
    #[test]
    fn a_newer_reply_fences_an_older_request_failure() {
        let monitor = Monitor::new();
        let pending = monitor.begin();
        monitor.observe(
            Outcome::TimedOut(Duration::from_secs(5)),
            SystemTime::UNIX_EPOCH,
        );
        monitor.observe(Outcome::Responsive, SystemTime::UNIX_EPOCH);
        monitor.observe_request(Outcome::Unreachable, SystemTime::UNIX_EPOCH, pending);
        assert_eq!(*monitor.sender.borrow(), State::Responsive);
        monitor.observe_request(
            Outcome::TimedOut(Duration::from_secs(20)),
            SystemTime::UNIX_EPOCH,
            monitor.begin(),
        );
        assert!(
            matches!(*monitor.sender.borrow(), State::Unresponsive { timeout, .. } if timeout == Duration::from_secs(20))
        );
    }
    #[test]
    fn request_write_pressure_is_not_a_daemon_reply_timeout() {
        let local: Result<(), _> = Err(ControlError::Frame(FrameError::Io(io::Error::from(
            io::ErrorKind::WouldBlock,
        ))));
        assert!(control_outcome(&local).is_none());
        let remote: Result<(), _> = Err(ControlError::ReplyTimeout {
            timeout: Duration::from_secs(75),
            error: FrameError::Io(io::Error::from(io::ErrorKind::TimedOut)),
        });
        assert!(
            matches!(control_outcome(&remote), Some(Outcome::TimedOut(timeout)) if timeout == Duration::from_secs(75))
        );
    }
    #[test]
    fn terminal_control_transport_failures_are_unreachable() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::UnexpectedEof,
        ] {
            let result: Result<(), _> =
                Err(ControlError::Frame(FrameError::Io(io::Error::from(kind))));
            assert!(matches!(
                control_outcome(&result),
                Some(Outcome::Unreachable)
            ));
        }
    }
    #[test]
    fn rejected_requests_prove_responsiveness_but_local_failures_do_not() {
        let remote: Result<(), _> = Err(ControlError::Remote {
            code: crate::terminal::ipc::ProtocolErrorCode::InvalidRequest,
            message: "rejected".into(),
            retryable: false,
        });
        assert!(matches!(
            control_outcome(&remote),
            Some(Outcome::Responsive)
        ));
        let local: Result<(), _> = Err(ControlError::Configure(io::Error::other("configuration")));
        assert!(control_outcome(&local).is_none());
    }
}
