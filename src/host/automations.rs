//! Daemon-owned automation coordination. Process execution and clock planning
//! stay outside HostState and SQLite transactions.

pub(super) mod precheck;
pub(super) mod schedule;
pub(super) mod service;
pub(super) mod target;

pub(super) fn helper_entry() -> Option<Result<(), String>> {
    precheck::worker::entry()
}

pub(super) fn wake(host: &super::state::SharedHost) {
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .automations
        .clone();
    if let Some(handle) = handle {
        handle.wake();
    }
}
mod broker;
pub(super) use broker::{Broker, Handle};
