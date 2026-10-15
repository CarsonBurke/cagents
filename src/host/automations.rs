//! Daemon-owned automation coordination. Process execution and clock planning
//! stay outside HostState and SQLite transactions.

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

/// Transfer committed scheduler events to the daemon's existing attention
/// broker. With no broker available, leave the durable outbox untouched.
pub(super) fn flush_notifications(host: &super::state::SharedHost) -> Result<(), String> {
    let notifications = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .notifications
        .clone();
    let Some(notifications) = notifications else {
        return Ok(());
    };
    let registry = super::agent_execution::registry(host)?;
    transfer_notifications(&registry, &notifications)
}

pub(super) fn transfer_notifications(
    registry: &super::agent_execution::SharedRegistry,
    notifications: &super::notifications::Handle,
) -> Result<(), String> {
    // Serialize read + enqueue with every outbox writer and other transfer.
    // Enqueue only takes the bounded pending mutex; the notification broker
    // drops that mutex before journal I/O, scheduler wakeups or host access.
    // Releasing the registry between these operations could enqueue an older
    // loaded event after its committed replacement from a concurrent flush.
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    registry.acknowledge_automation_notifications(&notifications.take_automation_receipts())?;
    for event in registry.automation_notifications()? {
        notifications.record(event);
    }
    Ok(())
}
