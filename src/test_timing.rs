//! Waiting in tests without making the host's speed part of the assertion.
//!
//! A test that bounds how long an operation takes fails whenever the host is
//! busy, which says nothing about the operation. Where the contract is that
//! something happens at all, such as a bounded call returning or a fixture
//! reaching a state, wait for it up to [`EVENTUALLY`]. A slow host still
//! passes, and code that hangs still fails.

use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// How long a test waits for something that must eventually happen. It is
/// not a performance budget: correct code finishes far sooner, even on a
/// heavily loaded host.
pub(crate) const EVENTUALLY: Duration = Duration::from_secs(60);

/// Run `operation` on its own thread and return its result, failing the test
/// if it has not returned within [`EVENTUALLY`]. A hung operation is left
/// running; the test process ends it.
#[track_caller]
pub(crate) fn returns<T: Send + 'static>(
    what: &str,
    operation: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, result) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(operation());
    });
    match result.recv_timeout(EVENTUALLY) {
        Ok(value) => value,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} did not return within {EVENTUALLY:?}")
        }
        // The operation panicked; its own message is already reported.
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

/// Poll `condition` until it holds, failing the test after [`EVENTUALLY`].
///
/// Only for conditions whose failure mode is never holding. Where a bug
/// would make the condition hold later, such as after a fallback timeout,
/// assert on something that tells the two apart instead.
#[track_caller]
pub(crate) fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    wait_for(what, || condition().then_some(()))
}

/// Poll until `poll` returns a value and return it, failing the test after
/// [`EVENTUALLY`]. The same caveat as [`eventually`] applies.
#[track_caller]
pub(crate) fn wait_for<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + EVENTUALLY;
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {EVENTUALLY:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
