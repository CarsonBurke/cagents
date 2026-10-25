//! One bounded journal handoff for durable automation attention.
//!
//! This owner never waits for a receipt under Registry, HostState or control
//! guards. Dropping it loses only a volatile waiter: the exact durable outbox
//! entry remains eligible for replay after restart.
use std::{sync::mpsc, time::Duration};

use crate::{
    host::{
        agent_execution::SharedRegistry,
        model::{Registry, automation_notifications::Entry},
        notifications::Handle,
    },
    notifications::Event,
};

pub(super) const IDLE: Duration = Duration::from_secs(60);
pub(super) const RETRY: Duration = Duration::from_secs(5);
const RECEIPT_POLL: Duration = Duration::from_millis(250);

enum Receipt {
    Waiting(mpsc::Receiver<Result<(), String>>),
    /// Preserve actual journal success across a failed SQLite ACK. Enqueueing
    /// again would waste capacity and could replay after journal seen eviction.
    Committed,
}

struct Pending {
    entry: Entry,
    receipt: Receipt,
}

#[derive(Default)]
pub(super) struct Transfer {
    pending: Option<Pending>,
}

impl Transfer {
    /// Advance at most one journal handoff or exact ACK per scheduler tick.
    ///
    /// `event` constructs bounded metadata/optional preview using the selected
    /// entry and current registry. It must not perform network/filesystem I/O.
    /// The caller treats failure as notification-only retry, allowing normal
    /// scheduler cleanup and deadlines to continue independently.
    pub(super) fn advance(
        &mut self,
        registry: &SharedRegistry,
        notifications: &Handle,
        event: impl FnOnce(&Registry, &Entry) -> Result<Event, String>,
    ) -> Result<Duration, String> {
        if let Some(pending) = &mut self.pending {
            // No Registry/HostState/control guard is held while observing the
            // receiver. Empty retains this exact waiter, never another enqueue.
            if let Receipt::Waiting(receiver) = &pending.receipt {
                match receiver.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => return Ok(RECEIPT_POLL),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.pending = None;
                        return Err("Automation notification journal reply was lost".into());
                    }
                    Ok(Err(error)) => {
                        self.pending = None;
                        return Err(error);
                    }
                    Ok(Ok(())) => pending.receipt = Receipt::Committed,
                }
            }
            // Only actual journal success (including a durable duplicate)
            // reaches ACK. A newer replacement makes this exact ACK a no-op.
            registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .acknowledge_automation_notification(&pending.entry)?;
            self.pending = None;
            return Ok(RECEIPT_POLL);
        }

        // Serialize read + enqueue with lifecycle writers. Notification enqueue
        // takes only a bounded pending mutex; the journal broker releases that
        // mutex before Registry/HostState access or disk I/O.
        let registry = registry.lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = registry
            .pending_automation_notifications(1)?
            .into_iter()
            .next()
        else {
            return Ok(IDLE);
        };
        let receiver = notifications.record_committed(event(&registry, &entry)?)?;
        self.pending = Some(Pending {
            entry,
            receipt: Receipt::Waiting(receiver),
        });
        Ok(RECEIPT_POLL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::{
        agent::Provider,
        agent_execution::{Spec, Surface, Target},
        automations::{
            Automation, Definition, DirectoryIdentity, Occurrence, Schedule, SchedulePattern,
            SessionMode,
        },
        workspace_model::{ModelEdit, ProjectKind},
    };

    // Uses the actual composed Registry migration and project-close hook. No
    // private table injection may bypass its published-layout validation.
    fn fixture() -> (tempfile::TempDir, SharedRegistry, Automation, Entry) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let repository = directory.path().to_owned();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let automation = registry
            .create_automation(
                Definition {
                    name: "Private fixture title".into(),
                    prompt: "Private fixture prompt".into(),
                    agent: Spec {
                        provider: Provider::Codex,
                        account: None,
                        target: Target {
                            repository: repository.clone(),
                            directory: repository.clone(),
                            placement: None,
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    },
                    schedule: Schedule {
                        pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                        timezone: "UTC".into(),
                    },
                    session_mode: SessionMode::Reuse,
                    precheck: None,
                },
                100,
                DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        registry
            .set_automation_enabled(
                automation.id,
                automation.revision,
                Some(Occurrence {
                    due_ms: 1_000,
                    local: "next".into(),
                }),
                true,
                200,
            )
            .unwrap();
        registry
            .apply(&ModelEdit::CloseProject { repository })
            .unwrap();
        let entry = registry
            .pending_automation_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        (directory, Arc::new(Mutex::new(registry)), automation, entry)
    }

    fn waiting(entry: Entry) -> (Transfer, mpsc::Sender<Result<(), String>>) {
        let (sender, receiver) = mpsc::channel();
        (
            Transfer {
                pending: Some(Pending {
                    entry,
                    receipt: Receipt::Waiting(receiver),
                }),
            },
            sender,
        )
    }

    fn no_enqueue(_: &Registry, _: &Entry) -> Result<Event, String> {
        panic!("an existing receipt must not enqueue a second notification")
    }

    #[test]
    fn pending_journal_reply_retains_the_same_waiter_and_durable_entry() {
        let (_directory, registry, _automation, entry) = fixture();
        let (mut transfer, sender) = waiting(entry.clone());
        let (notifications, _) = Handle::inert_for_test();
        for _ in 0..4 {
            assert_eq!(
                transfer
                    .advance(&registry, &notifications, no_enqueue)
                    .unwrap(),
                RECEIPT_POLL
            );
            assert_eq!(
                registry
                    .lock()
                    .unwrap()
                    .pending_automation_notifications(1)
                    .unwrap(),
                vec![entry.clone()]
            );
        }
        sender.send(Ok(())).unwrap();
        assert_eq!(
            transfer
                .advance(&registry, &notifications, no_enqueue)
                .unwrap(),
            RECEIPT_POLL
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_automation_notifications(1)
                .unwrap()
                .is_empty()
        );
        assert!(transfer.pending.is_none());
    }

    #[test]
    fn failed_or_lost_journal_reply_never_acknowledges_the_outbox() {
        for lost in [false, true] {
            let (_directory, registry, _automation, entry) = fixture();
            let (mut transfer, sender) = waiting(entry.clone());
            let (notifications, _) = Handle::inert_for_test();
            if !lost {
                sender
                    .send(Err("synthetic journal write failure".into()))
                    .unwrap();
            }
            drop(sender);
            assert!(
                transfer
                    .advance(&registry, &notifications, no_enqueue)
                    .is_err()
            );
            assert!(transfer.pending.is_none());
            assert_eq!(
                registry
                    .lock()
                    .unwrap()
                    .pending_automation_notifications(1)
                    .unwrap(),
                vec![entry]
            );
        }
    }

    #[test]
    fn dropping_a_waiter_preserves_the_same_identity_for_restart() {
        let (directory, registry, _automation, entry) = fixture();
        let (transfer, sender) = waiting(entry.clone());
        drop(transfer);
        assert!(sender.send(Ok(())).is_err());
        drop(registry);
        let reopened = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            reopened.pending_automation_notifications(1).unwrap(),
            vec![entry]
        );
    }

    #[test]
    fn an_old_success_reply_cannot_consume_newer_pause_attention() {
        let (directory, registry, automation, old) = fixture();
        let (mut transfer, sender) = waiting(old.clone());
        let newer = {
            let mut registry = registry.lock().unwrap();
            registry
                .apply(&ModelEdit::ReopenProject {
                    repository: directory.path().to_owned(),
                    name: "Fixture".into(),
                    kind: ProjectKind::Folder,
                })
                .unwrap();
            let current = registry.automation(automation.id).unwrap();
            registry
                .set_automation_enabled(
                    current.id,
                    current.revision,
                    Some(Occurrence {
                        due_ms: 2_000,
                        local: "later".into(),
                    }),
                    true,
                    300,
                )
                .unwrap();
            registry
                .apply(&ModelEdit::CloseProject {
                    repository: directory.path().to_owned(),
                })
                .unwrap();
            registry
                .pending_automation_notifications(1)
                .unwrap()
                .pop()
                .unwrap()
        };
        assert!(newer.event().sequence > old.event().sequence);
        sender.send(Ok(())).unwrap();
        let (notifications, _) = Handle::inert_for_test();
        transfer
            .advance(&registry, &notifications, no_enqueue)
            .unwrap();
        assert!(transfer.pending.is_none());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_automation_notifications(1)
                .unwrap(),
            vec![newer]
        );
    }

    #[test]
    fn a_stopped_notification_broker_leaves_the_durable_entry_available() {
        let (directory, registry, _automation, entry) = fixture();
        // The real inert handle has no wake receiver, so real enqueue admission
        // returns the production stopping error before accepting any event.
        let (notifications, _) = Handle::inert_for_test();
        let mut transfer = Transfer::default();
        let error = transfer
            .advance(&registry, &notifications, |_, entry| {
                Ok(Event::session(
                    crate::model::TerminalId::new(),
                    Provider::Codex,
                    directory.path().to_owned(),
                    crate::notifications::Evidence {
                        key: entry.evidence_key(),
                        kind: crate::notifications::Kind::Failed,
                    },
                ))
            })
            .unwrap_err();
        assert!(error.contains("stopping"));
        assert!(transfer.pending.is_none());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_automation_notifications(1)
                .unwrap(),
            vec![entry]
        );
    }
}
