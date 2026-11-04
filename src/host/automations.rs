//! Daemon-owned automation coordination. Process execution and clock planning
//! stay outside HostState and SQLite transactions.

mod notification_event;
mod notifications;
pub(super) mod precheck;
pub(super) use crate::automations::schedule;
pub(super) mod service;
pub(super) mod target;

pub(super) fn helper_entry() -> Option<Result<(), String>> {
    precheck::worker::entry()
}

pub(super) fn start(host: &super::state::SharedHost) -> Result<Broker, String> {
    let directories =
        crate::instance::project_dirs().ok_or("The application data directory is unavailable")?;
    let root = directories.data_dir().join("automation-prechecks");
    crate::secure_file::prepare_private_directory(&root).map_err(|e| e.to_string())?;
    let broker = Broker::start(host, root)?;
    host.lock().unwrap_or_else(|p| p.into_inner()).automations = Some(broker.handle());
    Ok(broker)
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
