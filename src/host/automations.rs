//! Daemon-owned automation coordination. Process execution and clock planning
//! stay outside HostState and SQLite transactions.

pub(super) mod precheck;
pub(super) mod schedule;

pub(super) fn helper_entry() -> Option<Result<(), String>> {
    precheck::worker::entry()
}
