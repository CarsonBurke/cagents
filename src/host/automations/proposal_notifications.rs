//! The existing scheduler owns one extra bounded source transfer. No thread,
//! controller, journal implementation, lifecycle outbox, or policy is replaced.
use std::{sync::mpsc, time::Instant};

use super::notifications::{IDLE, RETRY};
use crate::{
    host::{
        agent_execution::SharedRegistry, model::automation_proposal_notifications::Entry,
        notifications::Handle,
    },
    notifications::{Content, Event, Evidence, Kind},
};
use std::time::Duration;
const POLL: Duration = Duration::from_millis(250);

enum Receipt {
    Waiting(mpsc::Receiver<Result<(), String>>),
    Committed,
}

struct Pending {
    entry: Entry,
    receipt: Receipt,
}

#[derive(Default)]
pub(super) struct Transfer {
    pending: Option<Pending>,
    retry_at: Option<Instant>,
}

impl Transfer {
    pub(super) fn advance(
        &mut self,
        registry: &SharedRegistry,
        notifications: &Handle,
    ) -> Result<Duration, String> {
        self.advance_at(Instant::now(), registry, notifications)
    }

    fn advance_at(
        &mut self,
        now: Instant,
        registry: &SharedRegistry,
        notifications: &Handle,
    ) -> Result<Duration, String> {
        if let Some(retry) = self.retry_at {
            if now < retry {
                return Ok(retry.duration_since(now));
            }
            self.retry_at = None;
        }
        let result = self.advance_ready(registry, notifications);
        if result.is_err() {
            self.retry_at = now.checked_add(RETRY);
        }
        result
    }

    fn advance_ready(
        &mut self,
        registry: &SharedRegistry,
        notifications: &Handle,
    ) -> Result<Duration, String> {
        if let Some(pending) = &mut self.pending {
            // Observe the actual journal receipt with NO Registry/Host/control
            // guard. Empty retains the exact waiter; no duplicate enqueue.
            if let Receipt::Waiting(receiver) = &pending.receipt {
                match receiver.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => return Ok(POLL),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.pending = None;
                        return Err("Proposal notification journal reply was lost".into());
                    }
                    Ok(Err(error)) => {
                        self.pending = None;
                        return Err(error);
                    }
                    Ok(Ok(())) => pending.receipt = Receipt::Committed,
                }
            }
            // Failed SQL ACK retains known journal success, not another send.
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .acknowledge_proposal_notification(&pending.entry)?;
            self.pending = None;
            return Ok(POLL);
        }
        let prepared = {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let Some((entry, name)) = registry.prepare_proposal_notification()? else {
                return Ok(IDLE);
            };
            let attribution = entry.attribution();
            let content = if name.is_some() || attribution.is_some() {
                let mut preview = attribution.unwrap_or_default();
                if !preview.is_empty() {
                    preview.push(' ');
                }
                preview.push_str("Review its current saved settings before deciding.");
                Some(Content {
                    conversation: name.unwrap_or_else(|| "Automation request".into()),
                    preview,
                    request: None,
                })
            } else {
                None
            };
            let event = Event {
                target: entry.target(),
                source_session: None,
                provider: None,
                directory: None,
                destination: None,
                evidence: Evidence {
                    key: entry.evidence_key(),
                    kind: Kind::ProposalCreated,
                },
                content,
            };
            event.validate()?;
            (entry, event)
        }; // Release ALL registry guards before the bounded enqueue.
        let (entry, event) = prepared;
        let receipt = notifications.record_committed(event)?;
        self.pending = Some(Pending {
            entry,
            receipt: Receipt::Waiting(receipt),
        });
        Ok(POLL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Origin, Spec, Surface, Target},
        automations::{Definition, DirectoryIdentity, Schedule, SchedulePattern, SessionMode},
        host::model::Registry,
        model::ProjectKind,
        workspace_model::ModelEdit,
    };
    use std::sync::{Arc, Mutex};

    fn fixture() -> (
        tempfile::TempDir,
        SharedRegistry,
        crate::automations::Proposal,
        Entry,
    ) {
        // Same real Registry/migration boundary as the existing lifecycle
        // transfer tests. Root must adopt the dormant forward migration first.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let mut registry = Registry::at(&path.join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: path.clone(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let definition = Definition {
            name: "Fixture".into(),
            prompt: "Not notification metadata".into(),
            agent: Spec {
                provider: Provider::Codex,
                account: None,
                target: Target {
                    repository: path.clone(),
                    directory: path,
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                reasoning_effort: None,
                permission_mode: None,
            },
            schedule: Schedule {
                timezone: "UTC".into(),
                pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
            },
            session_mode: SessionMode::Fresh,
            precheck: None,
        };
        let proposal = registry
            .propose_automation(
                uuid::Uuid::new_v4(),
                Origin::User,
                definition.clone(),
                definition,
                DirectoryIdentity {
                    device: 1,
                    inode: 2,
                },
                None,
                1,
            )
            .unwrap();
        let entry = registry
            .pending_proposal_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        (root, Arc::new(Mutex::new(registry)), proposal, entry)
    }

    fn waiting(entry: Entry) -> (Transfer, mpsc::Sender<Result<(), String>>) {
        let (send, receive) = mpsc::channel();
        (
            Transfer {
                pending: Some(Pending {
                    entry,
                    receipt: Receipt::Waiting(receive),
                }),
                retry_at: None,
            },
            send,
        )
    }

    #[test]
    fn pending_failed_and_lost_receipts_keep_the_exact_source_for_retry() {
        for failed in [false, true] {
            let (_root, registry, _proposal, entry) = fixture();
            let (mut transfer, sender) = waiting(entry.clone());
            let (handle, _) = Handle::inert_for_test();
            for _ in 0..3 {
                assert_eq!(transfer.advance_ready(&registry, &handle).unwrap(), POLL);
                assert_eq!(
                    registry
                        .try_lock()
                        .unwrap()
                        .pending_proposal_notifications(1)
                        .unwrap(),
                    vec![entry.clone()]
                );
                assert!(matches!(
                    transfer.pending.as_ref().unwrap().receipt,
                    Receipt::Waiting(_)
                ));
            }
            if failed {
                sender
                    .send(Err("synthetic journal failure".into()))
                    .unwrap();
            }
            drop(sender);
            assert!(transfer.advance_ready(&registry, &handle).is_err());
            assert!(transfer.pending.is_none());
            assert_eq!(
                registry
                    .lock()
                    .unwrap()
                    .pending_proposal_notifications(1)
                    .unwrap(),
                vec![entry]
            );
        }
    }

    #[test]
    fn known_journal_success_survives_sql_ack_failure_and_resolution_of_prepared_source() {
        let (_root, registry, proposal, entry) = fixture();
        let (mut transfer, sender) = waiting(entry.clone());
        let (handle, _) = Handle::inert_for_test();
        let blocker = rusqlite::Connection::open(_root.path().join("model.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        sender.send(Ok(())).unwrap();
        assert!(transfer.advance_ready(&registry, &handle).is_err());
        assert!(matches!(
            transfer.pending.as_ref().unwrap().receipt,
            Receipt::Committed
        ));
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap(),
            vec![entry]
        );
        blocker.execute_batch("ROLLBACK").unwrap();
        // Another connection/owner can resolve while the selected immutable
        // event is already prepared. Old ACK must not reinterpret the decision.
        registry
            .lock()
            .unwrap()
            .resolve_automation_proposal(&proposal, None, 2)
            .unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(transfer.advance_ready(&registry, &handle).unwrap(), POLL);
        assert!(transfer.pending.is_none());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .automation_proposal(proposal.id, None)
                .unwrap()
                .unwrap()
                .state,
            crate::automations::ProposalState::Declined
        );
    }

    #[test]
    fn actual_prepare_enqueue_held_journal_commit_ack_and_stopped_retry_preserve_custody() {
        use crate::{
            host::notifications::JournalTestBridge,
            notifications::{Preferences, Target as NoticeTarget},
        };
        let (root, registry, proposal, entry) = fixture();
        let (handle, mut journal) =
            JournalTestBridge::new(&root.path().join("notifications.sqlite3")).unwrap();
        let mut transfer = Transfer::default();
        assert_eq!(transfer.advance(&registry, &handle).unwrap(), POLL);
        let queued = journal.pending_events();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].target, entry.target());
        assert_eq!(queued[0].evidence.key, entry.evidence_key());
        assert_eq!(queued[0].evidence.kind, Kind::ProposalCreated);
        assert!(
            matches!(&queued[0].target,NoticeTarget::AutomationProposal{id,revision,..} if *id==proposal.id && *revision==proposal.revision)
        );
        assert!(
            queued[0].source_session.is_none()
                && queued[0].provider.is_none()
                && queued[0].directory.is_none()
                && queued[0].destination.is_none()
        );
        // The ACTUAL Handle receipt is pending: do not send a synthetic Ok.
        assert_eq!(
            registry
                .try_lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap(),
            vec![entry.clone()]
        );
        let host = crate::host::service::test_host();
        let control = {
            let state = host.lock().unwrap();
            let _proof = state.proof().clone();
            state.control_gate.clone()
        };
        drop(
            control
                .try_write()
                .expect("unrelated host/control progress while journal receipt is held"),
        );
        assert_eq!(transfer.advance(&registry, &handle).unwrap(), POLL);
        assert_eq!(
            journal.pending_events(),
            queued,
            "one actual enqueue while receipt is held"
        );
        let notice = journal
            .commit_next(&Preferences::default(), false)
            .unwrap()
            .unwrap();
        assert_eq!(notice.event.target, entry.target());
        assert!(
            notice.event.content.is_none(),
            "real journal policy strips opt-in title by default"
        );
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap(),
            vec![entry]
        );
        assert_eq!(transfer.advance(&registry, &handle).unwrap(), POLL);
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap()
                .is_empty()
        );
        assert!(transfer.pending.is_none());
        assert_eq!(journal.list().unwrap(), vec![notice]);

        let next = registry
            .lock()
            .unwrap()
            .propose_automation(
                uuid::Uuid::new_v4(),
                Origin::User,
                proposal.requested.clone(),
                proposal.definition.clone(),
                proposal.target_identity.clone(),
                None,
                3,
            )
            .unwrap();
        journal.set_stopped(true);
        let mut stopped = Transfer::default();
        let now = Instant::now();
        assert!(stopped.advance_at(now, &registry, &handle).is_err());
        assert!(stopped.pending.is_none());
        assert!(journal.pending_events().is_empty());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            stopped
                .advance_at(now + Duration::from_secs(1), &registry, &handle)
                .unwrap(),
            RETRY - Duration::from_secs(1)
        );
        journal.set_stopped(false);
        // A fresh owner can replay the SAME source after a stopped owner.
        let mut restarted = Transfer::default();
        restarted.advance(&registry, &handle).unwrap();
        assert!(
            matches!(&journal.pending_events()[0].target,NoticeTarget::AutomationProposal{id,..} if *id==next.id)
        );
        journal
            .commit_next(&Preferences::default(), false)
            .unwrap()
            .unwrap();
        restarted.advance(&registry, &handle).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn actual_journal_attribution_uses_creation_requester_and_basename_only_when_previews_are_enabled()
     {
        use crate::{
            host::notifications::JournalTestBridge,
            notifications::{Preferences, presentation},
        };
        for previews in [false, true] {
            let (root, registry, first, _) = fixture();
            registry
                .lock()
                .unwrap()
                .resolve_automation_proposal(&first, None, 2)
                .unwrap();
            let proposal = registry
                .lock()
                .unwrap()
                .propose_automation_with_requester(
                    uuid::Uuid::new_v4(),
                    Origin::User,
                    first.requested.clone(),
                    first.definition.clone(),
                    first.target_identity.clone(),
                    Some("Unrelated destination title".into()),
                    Some("Planning conversation"),
                    3,
                )
                .unwrap();
            let entry = registry
                .lock()
                .unwrap()
                .pending_proposal_notifications(1)
                .unwrap()
                .pop()
                .unwrap();
            let (handle, mut journal) =
                JournalTestBridge::new(&root.path().join("attribution.sqlite3")).unwrap();
            let mut transfer = Transfer::default();
            transfer.advance(&registry, &handle).unwrap();
            let events = journal.pending_events();
            assert_eq!(events.len(), 1);
            let event = &events[0];
            assert_eq!(event.target, entry.target());
            assert_eq!(event.evidence.key, entry.evidence_key());
            let content = event.content.as_ref().unwrap();
            assert!(
                content
                    .preview
                    .contains("Requested by Planning conversation")
            );
            let basename = proposal
                .definition
                .agent
                .target
                .repository
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            assert!(content.preview.contains(&format!("Project {basename}")));
            assert!(!content.preview.contains("Unrelated destination"));
            assert!(!content.preview.contains(&proposal.definition.prompt));
            assert!(
                !content
                    .preview
                    .contains(root.path().parent().unwrap().to_str().unwrap())
            );
            assert!(
                event.source_session.is_none()
                    && event.provider.is_none()
                    && event.directory.is_none()
                    && event.destination.is_none()
            );
            let notice = journal
                .commit_next(
                    &Preferences {
                        message_previews: previews,
                        ..Preferences::default()
                    },
                    false,
                )
                .unwrap()
                .unwrap();
            let visible = presentation::message(&notice.event, previews);
            if previews {
                assert_eq!(notice.event.content, event.content);
                assert!(visible.body.contains("Planning conversation"));
                assert!(visible.body.contains(basename));
            } else {
                assert!(notice.event.content.is_none());
                assert_eq!(visible.title, "Automation request created");
                assert_eq!(visible.body, "Automation request created");
            }
            transfer.advance(&registry, &handle).unwrap();
            assert!(
                registry
                    .lock()
                    .unwrap()
                    .pending_proposal_notifications(1)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
