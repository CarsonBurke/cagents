//! Requests to end the application that come from outside its window.
//!
//! A session manager sends SIGTERM at logout, and so does `kill`. SIGHUP
//! arrives when the terminal or login session that started the process goes
//! away. With their default action, either signal ends the process before
//! anything saved only on a normal close is written, so both are delivered to
//! the application instead and it saves and closes as it would for its
//! window's close button.
//!
//! Handling a signal means the process no longer ends on it by itself. A
//! second request, or an exit that has not finished by
//! [`FORCED_EXIT_DEADLINE`], ends the process with the signal's own default
//! action, as though no handler had been installed.

use cosmic::iced::futures::{SinkExt, Stream};
use std::time::Duration;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// How long a graceful exit may take before the signal that asked for it ends
/// the process anyway.
///
/// The exit's own bounds run concurrently: the window gets two seconds to
/// report its close, and the language servers five to be reaped. What remains
/// is the final save in `Drop`. A UI thread still running at this deadline is
/// wedged, and only a forced exit ends it.
pub(crate) const FORCED_EXIT_DEADLINE: Duration = Duration::from_secs(15);

/// The signal that asked the application to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Terminate,
    Hangup,
}

impl Request {
    fn signal(self) -> libc::c_int {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Hangup => libc::SIGHUP,
        }
    }
}

/// SIGTERM and SIGHUP, each caught from the moment [`Requests::listen`]
/// returns.
pub(crate) struct Requests {
    terminate: Option<Signal>,
    hangup: Option<Signal>,
}

impl Requests {
    /// Installs the handlers. A signal whose handler cannot be installed
    /// keeps its default action, which ends the process.
    pub(crate) fn listen() -> Self {
        let install = |kind: SignalKind, name: &str| {
            signal(kind)
                .inspect_err(|error| {
                    tracing::warn!(%error, signal = name,
                        "could not handle the signal; it will end the window without saving");
                })
                .ok()
        };
        Self {
            terminate: install(SignalKind::terminate(), "SIGTERM"),
            hangup: install(SignalKind::hangup(), "SIGHUP"),
        }
    }

    /// The next request, or `None` once neither signal can arrive.
    pub(crate) async fn next(&mut self) -> Option<Request> {
        loop {
            let request = tokio::select! {
                Some(received) = recv(&mut self.terminate) => received.map(|()| Request::Terminate),
                Some(received) = recv(&mut self.hangup) => received.map(|()| Request::Hangup),
                else => return None,
            };
            match request {
                Some(request) => return Some(request),
                // That stream has ended; the `else` arm ends the wait once
                // both have.
                None => continue,
            }
        }
    }
}

/// `None` once a signal is unavailable, so `select!` disables its arm;
/// otherwise its next delivery, `Some(None)` when its stream has ended.
async fn recv(signal: &mut Option<Signal>) -> Option<Option<()>> {
    let received = signal.as_mut()?.recv().await;
    if received.is_none() {
        *signal = None;
    }
    Some(received)
}

/// Each request to end the application, in a subscription.
///
/// After the first, the stream only waits for a second request or for
/// [`FORCED_EXIT_DEADLINE`], and then ends the process.
pub(crate) fn stream() -> impl Stream<Item = Request> {
    cosmic::iced::stream::channel(1, async |mut output| {
        let mut requests = Requests::listen();
        let Some(first) = requests.next().await else {
            return;
        };
        tracing::info!(request = ?first, "asked to end; saving and closing the window");
        // A closed output means the application is already tearing down. The
        // deadline below still applies, so the signal still ends the process.
        let _ = output.send(first).await;
        let forced = match tokio::time::timeout(FORCED_EXIT_DEADLINE, requests.next()).await {
            Ok(Some(again)) => {
                tracing::warn!(request = ?again, "asked again to end; ending without waiting for the save");
                again
            }
            Ok(None) => first,
            Err(_) => {
                tracing::error!(
                    deadline_secs = FORCED_EXIT_DEADLINE.as_secs(),
                    "the window did not finish closing; ending without waiting for the save"
                );
                first
            }
        };
        end_as_signalled(forced.signal());
    })
}

/// Ends the process with `signal`'s default action, so the parent sees the
/// same status as if no handler had been installed.
fn end_as_signalled(signal: libc::c_int) -> ! {
    // SAFETY: restoring the default disposition and raising a signal take no
    // pointers and touch no Rust state. For SIGTERM and SIGHUP the default
    // action ends the whole process, whichever thread raises it.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    // Only reached if the signal was somehow blocked on this thread.
    std::process::exit(128 + signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SIGHUP rather than SIGTERM so a test harness that forwards SIGTERM to
    /// end a run is never confused by this one. The handler is installed by
    /// `listen` before the signal is sent, so the test process survives it.
    #[tokio::test]
    async fn a_hangup_is_delivered_as_a_request_instead_of_ending_the_process() {
        let mut requests = Requests::listen();
        assert!(requests.hangup.is_some());
        // SAFETY: sends a signal to this process, whose handler is installed.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGHUP) }, 0);
        let request = tokio::time::timeout(Duration::from_secs(5), requests.next())
            .await
            .expect("the hangup was delivered");
        assert_eq!(request, Some(Request::Hangup));
    }

    #[test]
    fn a_forced_exit_ends_with_the_signal_that_asked() {
        assert_eq!(Request::Terminate.signal(), libc::SIGTERM);
        assert_eq!(Request::Hangup.signal(), libc::SIGHUP);
    }
}
