//! Exercise the real registry source and notification journal handoff, without
//! starting a broker thread, portal, provider, precheck, or scheduled operation.
use super::*;
use crate::{
    agent_execution::{Run, Update},
    automations::DirectoryIdentity,
    host::{model::Registry, notifications::JournalTestBridge},
    model::ProjectKind,
    notifications::Preferences,
    workspace_model::ModelEdit,
};

fn add_waiting_run(registry: &mut Registry) -> Run {
    let definition = serde_json::from_value(serde_json::json!({
        "name":"private automation title", "prompt":"private automation prompt",
        "agent":{"provider":"Claude","account":null,"target":{"repository":"/fixture","directory":"/fixture","channel":null},"surface":"Chat","model":null,"permission_mode":null},
        "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},
        "session_mode":"fresh","precheck":null
    })).unwrap();
    let automation = registry
        .create_automation(
            definition,
            100,
            DirectoryIdentity {
                device: 1,
                inode: 1,
            },
        )
        .unwrap();
    let accepted = registry
        .claim_automation(
            automation.id,
            1,
            None,
            Trigger::Manual {
                request: Uuid::new_v4(),
            },
            None,
            true,
            200,
        )
        .unwrap()
        .accepted
        .unwrap();
    let run = registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Started {
                provider_key: "fixture-turn".into(),
            },
            300,
        )
        .unwrap();
    registry
        .update_agent_run(run.id, run.revision, Update::Waiting, 400)
        .unwrap()
}

fn fixture() -> (tempfile::TempDir, agent_execution::SharedRegistry, Run) {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: "/fixture".into(),
            name: "Fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let run = add_waiting_run(&mut registry);
    (directory, Arc::new(Mutex::new(registry)), run)
}

fn retry_due(transfer: &mut AttentionTransfer) {
    // Move only the local retry clock instead of making kernel tests sleep.
    transfer.retry_at = None;
}

#[test]
fn stopped_and_full_journals_keep_source_pending_and_retry_without_hot_enqueue() {
    for full in [false, true] {
        let (directory, registry, _) = fixture();
        let (notifications, mut journal) =
            JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
        let event = registry
            .lock()
            .unwrap()
            .pending_automation_attention(1)
            .unwrap()
            .remove(0)
            .event;
        let receipts = if full {
            (0..2048)
                .map(|_| notifications.record_committed(event.clone()).unwrap())
                .collect::<Vec<_>>()
        } else {
            journal.set_stopped(true);
            Vec::new()
        };
        let mut transfer = AttentionTransfer::default();
        assert_eq!(
            transfer.pump(&registry, Some(&notifications)).unwrap(),
            Some(Duration::from_secs(1))
        );
        assert!(transfer.in_flight.is_empty());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_automation_attention(100)
                .unwrap()
                .len(),
            1
        );
        let (notifications, mut journal) = if full {
            journal.drop_pending();
            assert!(
                receipts
                    .into_iter()
                    .all(|receipt| receipt.try_recv().unwrap().is_err())
            );
            (notifications, journal)
        } else {
            // A stopped handle never revives. Reopen the actual journal with
            // a fresh bounded handle, as a new notification owner would.
            drop(notifications);
            drop(journal);
            JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap()
        };
        // New capacity/owner does not bypass the local retry deadline.
        transfer.pump(&registry, Some(&notifications)).unwrap();
        assert!(journal.pending_events().is_empty());
        retry_due(&mut transfer);
        transfer.pump(&registry, Some(&notifications)).unwrap();
        assert_eq!(journal.pending_events().len(), 1);
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_automation_attention(100)
                .unwrap()
                .len(),
            1,
            "enqueue is not an acknowledgement"
        );
        journal.commit_next(&Preferences::default(), false).unwrap();
        assert_eq!(
            transfer.pump(&registry, Some(&notifications)).unwrap(),
            None
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn journal_write_failure_keeps_source_until_a_later_real_commit() {
    let (directory, registry, _) = fixture();
    let path = directory.path().join("notices.sqlite3");
    let (notifications, mut journal) = JournalTestBridge::new(&path).unwrap();
    let mut transfer = AttentionTransfer::default();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("DROP TABLE seen").unwrap();
    assert!(journal.commit_next(&Preferences::default(), false).is_err());
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert!(transfer.in_flight.is_empty());
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .len(),
        1
    );
    connection.execute_batch("CREATE TABLE seen(session TEXT NOT NULL,evidence TEXT NOT NULL,ordinal INTEGER PRIMARY KEY AUTOINCREMENT,UNIQUE(session,evidence))").unwrap();
    retry_due(&mut transfer);
    transfer.pump(&registry, Some(&notifications)).unwrap();
    journal.commit_next(&Preferences::default(), false).unwrap();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(journal.list().unwrap().len(), 1);
}

#[test]
fn source_ack_failure_retains_commit_proof_without_resending_the_event() {
    let (directory, registry, _) = fixture();
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let mut transfer = AttentionTransfer::default();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    journal.commit_next(&Preferences::default(), false).unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("registry.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_source_ack BEFORE DELETE ON automation_run_attention BEGIN SELECT RAISE(ABORT,'fixture source acknowledgement refused'); END").unwrap();
    assert!(transfer.pump(&registry, Some(&notifications)).is_err());
    assert!(matches!(
        transfer.in_flight.values().next(),
        Some(AttentionCommit::Committed)
    ));
    assert!(journal.pending_events().is_empty());
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .len(),
        1
    );
    connection
        .execute_batch("DROP TRIGGER fail_source_ack")
        .unwrap();
    retry_due(&mut transfer);
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert!(transfer.in_flight.is_empty());
    assert!(
        journal.pending_events().is_empty(),
        "retry the source ACK alone"
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reopened_source_replays_a_lost_ack_without_resurrecting_dismissed_attention() {
    let (directory, registry, _) = fixture();
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let mut transfer = AttentionTransfer::default();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    let notice = journal
        .commit_next(&Preferences::default(), false)
        .unwrap()
        .unwrap();
    journal.acknowledge(notice.id).unwrap();
    // Lose only the original in-memory receipt after actual journal commit.
    drop(transfer);
    drop(notifications);
    drop(journal);
    drop(registry);
    let registry = Arc::new(Mutex::new(
        Registry::at(&directory.path().join("registry.sqlite3")).unwrap(),
    ));
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let mut reopened = AttentionTransfer::default();
    reopened.pump(&registry, Some(&notifications)).unwrap();
    assert!(
        journal
            .commit_next(&Preferences::default(), false)
            .unwrap()
            .is_none()
    );
    reopened.pump(&registry, Some(&notifications)).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
    let notices = journal.list().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].id, notice.id);
    assert!(notices[0].acknowledged);
}

#[test]
fn newer_waiting_edge_queues_after_the_original_and_old_ack_cannot_delete_it() {
    let (directory, registry, first_run) = fixture();
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let mut transfer = AttentionTransfer::default();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    let first = journal.pending_events().remove(0);
    let second_run = {
        let mut registry = registry.lock().unwrap();
        let run = registry
            .update_agent_run(first_run.id, first_run.revision, Update::Resumed, 500)
            .unwrap();
        registry
            .update_agent_run(run.id, run.revision, Update::Waiting, 600)
            .unwrap()
    };
    transfer.pump(&registry, Some(&notifications)).unwrap();
    let queued = journal.pending_events();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0], first);
    assert_ne!(queued[0].evidence, queued[1].evidence);
    journal.commit_next(&Preferences::default(), false).unwrap();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    let pending = registry
        .lock()
        .unwrap()
        .pending_automation_attention(100)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].receipt.revision, second_run.revision);
    assert_eq!(
        journal.pending_events().len(),
        1,
        "the second edge is not re-enqueued"
    );
    journal.commit_next(&Preferences::default(), false).unwrap();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(journal.list().unwrap()[0].event, queued[1]);
}

#[test]
fn withheld_journal_receipt_never_holds_control_host_or_registry_scope() {
    let (directory, registry, _) = fixture();
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let host = crate::host::service::test_host();
    host.lock().unwrap().notifications = Some(notifications);
    let handle = Handle(Arc::new(Inner {
        host: Arc::downgrade(&host),
        registry: registry.clone(),
        root: directory.path().into(),
        wake: Wake::default(),
        workers: Mutex::default(),
        cleanup: Mutex::new(None),
        attention: Mutex::default(),
    }));
    let gate = host.lock().unwrap().control_gate.clone();
    let exclusive = gate.write().unwrap();
    assert_eq!(handle.tick(&host).unwrap(), Duration::from_millis(250));
    assert_eq!(journal.pending_events().len(), 1);
    assert!(registry.try_lock().is_ok());
    assert!(host.lock().is_ok());
    assert!(matches!(
        handle.0.attention.lock().unwrap().in_flight.values().next(),
        Some(AttentionCommit::Waiting(_))
    ));
    // Actual journal work runs after the pump, while execution admission is
    // still closed. Receipt acceptance likewise does not need that reader.
    journal.commit_next(&Preferences::default(), false).unwrap();
    assert_eq!(handle.tick(&host).unwrap(), Duration::from_millis(250));
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
    drop(exclusive);
}

#[test]
fn suppressed_private_automation_attention_commits_without_prompt_or_title_content() {
    let (directory, registry, _) = fixture();
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let mut transfer = AttentionTransfer::default();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    let notice = journal
        .commit_next(
            &Preferences {
                message_previews: false,
                ..Preferences::default()
            },
            true,
        )
        .unwrap()
        .unwrap();
    let serialized = serde_json::to_string(&notice).unwrap();
    assert!(notice.event.content.is_none());
    assert!(!serialized.contains("private automation title"));
    assert!(!serialized.contains("private automation prompt"));
    assert!(
        notice.delivered,
        "suppressed is durable, not a delivery failure"
    );
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn receipt_ownership_is_bounded_and_later_source_pages_make_progress() {
    let (directory, registry, _) = fixture();
    for _ in 0..MAX_ATTENTION_IN_FLIGHT {
        add_waiting_run(&mut registry.lock().unwrap());
    }
    let (notifications, mut journal) =
        JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
    let mut transfer = AttentionTransfer::default();
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert_eq!(transfer.in_flight.len(), MAX_ATTENTION_IN_FLIGHT);
    assert_eq!(journal.pending_events().len(), MAX_ATTENTION_IN_FLIGHT);
    // A withheld first page neither duplicates nor allocates more receivers.
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert_eq!(transfer.in_flight.len(), MAX_ATTENTION_IN_FLIGHT);
    assert_eq!(journal.pending_events().len(), MAX_ATTENTION_IN_FLIGHT);
    for _ in 0..MAX_ATTENTION_IN_FLIGHT {
        journal.commit_next(&Preferences::default(), true).unwrap();
    }
    transfer.pump(&registry, Some(&notifications)).unwrap();
    assert_eq!(transfer.in_flight.len(), 1);
    assert_eq!(journal.pending_events().len(), 1);
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .len(),
        1
    );
    journal.commit_next(&Preferences::default(), true).unwrap();
    assert_eq!(
        transfer.pump(&registry, Some(&notifications)).unwrap(),
        None
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );
}
