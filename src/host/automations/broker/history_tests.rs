/// This fixture runs only an owned exit7 shell through the actual same-head
/// precheck supervisor. It never accepts or dispatches an agent/provider.
#[test]
#[cfg(target_os = "linux")]
fn retired_precheck_worker_keeps_retry_custody_until_history_ack_commits() {
    use crate::{
        host::model::Registry,
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind},
        workspace_model::ModelEdit,
    };
    use std::os::unix::fs::MetadataExt;
    for (use_startup_sweep, restart) in [
        (false, 0),
        (true, 0),
        (false, 1),
        (true, 1),
        (false, 2),
        (true, 2),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut model = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        model
            .apply(&ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "Precheck cleanup fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let definition = crate::automations::Definition {
            name: "Owned exit7".into(),
            prompt: "Must never dispatch".into(),
            agent: crate::agent_execution::Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: crate::agent_execution::Target {
                    repository: directory.path().into(),
                    directory: directory.path().into(),
                    placement: None,
                },
                surface: crate::agent_execution::Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            schedule: crate::automations::Schedule {
                pattern: crate::automations::SchedulePattern::Daily { hour: 9, minute: 0 },
                timezone: "UTC".into(),
            },
            session_mode: SessionMode::Fresh,
            precheck: Some(crate::automations::Precheck {
                command: "exit 7".into(),
                timeout_seconds: 5,
            }),
        };
        let metadata = directory.path().metadata().unwrap();
        let identity = crate::automations::DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let automation = model
            .create_automation(definition.clone(), 1, identity.clone())
            .unwrap();
        let claim = model
            .claim_automation(
                automation.id,
                automation.execution_revision,
                None,
                Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                2,
            )
            .unwrap();
        assert!(claim.accepted.is_none());
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("history-fixture"),
            boot_id: BootId::new("history-fixture"),
            host_incarnation: HostIncarnation::new("history-fixture"),
        });
        state.install_model(model);
        let mut host = Arc::new(HostLock::new(state));
        let mut registry = agent_execution::registry(&host).unwrap();
        let fault_db = rusqlite::Connection::open(directory.path().join("model.sqlite3")).unwrap();
        let root = directory.path().join("prechecks");
        let request = Request {
            firing: claim.firing.id,
            revision: claim.firing.revision,
            directory: directory.path().into(),
            target_identity: identity,
            precheck: definition.precheck.unwrap(),
        };
        // launch requires CAGENTS_TEST_RETAINED_ACTOR_HELPER; no skip/fallback.
        let worker = Worker::launch(&root, &request)
            .expect("Actual precheck test requires the pinned same-head cagentsd helper");
        let mut handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(&host),
            registry: registry.clone(),
            root: root.clone(),
            wake: Wake::default(),
            workers: Mutex::new(BTreeMap::from([(claim.firing.id, worker)])),
            cleanup: Mutex::new(None),
            attention: Mutex::default(),
            notifications: Mutex::default(),
        }));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observer = Worker::recover(&root, claim.firing.id, claim.firing.revision);
        loop {
            if use_startup_sweep {
                // Read the actual supervisor receipt without polling/reaping the
                // mapped Child. The startup sweep must retire THAT original owner.
                if let Status::Complete(outcome) = observer.status().unwrap() {
                    registry
                        .lock()
                        .unwrap()
                        .finish_automation_precheck(claim.firing.id, outcome, 3)
                        .unwrap();
                }
            } else {
                handle.observe_precheck(&host, &claim.firing, 3).unwrap();
            }
            if registry
                .lock()
                .unwrap()
                .automation_firing(claim.firing.id)
                .unwrap()
                .state
                == FiringState::Skipped
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "owned precheck did not publish its bounded exit7 result"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        if use_startup_sweep {
            assert!(
                handle
                    .0
                    .workers
                    .lock()
                    .unwrap()
                    .get(&claim.firing.id)
                    .unwrap()
                    .owns_child_for_test(),
                "observer must preserve the original Child custody before sweep"
            );
            *handle.0.cleanup.lock().unwrap() = Some(std::fs::read_dir(&root).unwrap());
        }
        {
            let model = registry.lock().unwrap();
            let firing = model.automation_firing(claim.firing.id).unwrap();
            assert_eq!(
                firing.skip,
                Some(crate::automations::SkipReason::PrecheckExit)
            );
            assert!(firing.run.is_none());
            model
                .set_automation_history_ack_failure_for_test(true)
                .unwrap();
        }
        loop {
            if use_startup_sweep {
                // A bounded new census keeps this oracle on the sweep path if
                // the first pass reaches the owner just before process exit.
                *handle.0.cleanup.lock().unwrap() = Some(std::fs::read_dir(&root).unwrap());
            }
            match if use_startup_sweep {
                handle.sweep_prechecks().map(|_| ())
            } else {
                handle.tick(&host).map(|_| ())
            } {
                Err(error) => {
                    assert!(error.contains("fixture durable ACK failure"));
                    break;
                }
                Ok(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "Worker never reached actual retirement/ACK"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        assert!(
            !root.join(claim.firing.id.to_string()).exists(),
            "actual retire removed its owned proof directory before ACK"
        );
        {
            let workers = handle.0.workers.lock().unwrap();
            assert!(
                !workers
                    .get(&claim.firing.id)
                    .expect("ACK failure lost custody")
                    .owns_child_for_test(),
                "the actual mapped Child must be reaped BEFORE durable ACK, including startup sweep"
            );
        }
        {
            let model = registry.lock().unwrap();
            assert_eq!(
                model.retired_automation_precheck(claim.firing.id).unwrap(),
                Some(claim.firing.revision)
            );
            let markers: usize = fault_db
                .query_row(
                    "SELECT count(*) FROM automation_precheck_history_cleanup",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(markers, 0);
            if restart != 1 {
                model
                    .set_automation_history_ack_failure_for_test(false)
                    .unwrap();
            }
        }
        let retirement = root.join(format!("{}.retired", claim.firing.id));
        assert!(
            retirement.exists(),
            "durable custody must outlive proof deletion"
        );
        if restart == 2 {
            // Crash on the other side: ACK commits, then the ordinary removed
            // history sweep deletes the firing and its cascading SQLite ACK.
            // The filesystem receipt is still the only cleanup work to replay.
            let mut model = registry.lock().unwrap();
            model
                .confirm_retired_automation_precheck(claim.firing.id, claim.firing.revision, 4)
                .unwrap();
            model
                .remove_automation(automation.id, automation.revision, 5)
                .unwrap();
            model
                .maintain_removed_automation_history(
                    6 + crate::automations::REMOVED_HISTORY_RETENTION_DAYS * 24 * 60 * 60 * 1_000,
                )
                .unwrap();
            assert!(model.automation_firing(claim.firing.id).is_err());
            assert!(retirement.exists());
        }
        if restart != 0 {
            let old_registry = Arc::downgrade(&registry);
            let old_host = Arc::downgrade(&host);
            let old_handle = Arc::downgrade(&handle.0);
            drop(observer);
            drop(handle);
            drop(registry);
            drop(host);
            assert!(
                old_registry.upgrade().is_none(),
                "old Registry survived restart"
            );
            assert!(old_host.upgrade().is_none());
            assert!(old_handle.upgrade().is_none());
            // Reopening drops the original connection-local ACK fault. No
            // Worker, Registry or owner Arc bridges this startup boundary.
            let model = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
            let mut state = HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("history-restarted"),
                boot_id: BootId::new("history-fixture"),
                host_incarnation: HostIncarnation::new("history-restarted"),
            });
            state.install_model(model);
            host = Arc::new(HostLock::new(state));
            registry = agent_execution::registry(&host).unwrap();
            handle = Handle(Arc::new(Inner {
                host: Arc::downgrade(&host),
                registry: registry.clone(),
                root: root.clone(),
                wake: Wake::default(),
                workers: Mutex::new(BTreeMap::new()),
                cleanup: Mutex::new(Some(std::fs::read_dir(&root).unwrap())),
                attention: Mutex::default(),
                notifications: Mutex::default(),
            }));
            assert!(handle.0.workers.lock().unwrap().is_empty());
            assert!(
                registry
                    .lock()
                    .unwrap()
                    .active_automation_firings()
                    .unwrap()
                    .is_empty()
            );
            handle.sweep_prechecks().unwrap();
        }
        handle.tick(&host).unwrap();
        assert!(
            !handle
                .0
                .workers
                .lock()
                .unwrap()
                .contains_key(&claim.firing.id)
        );
        assert!(
            !retirement.exists(),
            "ACKed or compacted receipt was orphaned"
        );
        // A repeated cached census candidate must not manufacture a new
        // missing-directory owner after the first pass removed both names.
        handle
            .retire_precheck(claim.firing.id, claim.firing.revision)
            .unwrap();
        assert!(handle.0.workers.lock().unwrap().is_empty());
        handle.tick(&host).unwrap();
        let model = registry.lock().unwrap();
        if restart != 2 {
            assert_eq!(
                model.automation_firing(claim.firing.id).unwrap().state,
                FiringState::Skipped
            );
        } else {
            assert!(model.automation_firing(claim.firing.id).is_err());
        }
        let markers: usize = fault_db
            .query_row(
                "SELECT count(*) FROM automation_precheck_history_cleanup WHERE firing=?1",
                [claim.firing.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(markers, usize::from(restart != 2));
        let runs: usize = fault_db
            .query_row("SELECT count(*) FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(runs, 0, "exit7 must never accept a provider run");
    }
}

/// A real SQLite DELETE abort must not disable either delivery owner or the
/// coordinator's ordinary observation path. No provider/precheck is launched.
#[test]
fn history_delete_failure_rolls_back_but_startup_and_tick_keep_both_outboxes_live() {
    use crate::{
        agent_execution::{State, Update},
        automations::{Definition, DirectoryIdentity, Occurrence},
        host::{model::Registry, notifications::JournalTestBridge},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind},
        notifications::Preferences,
        workspace_model::ModelEdit,
    };
    use std::os::unix::fs::MetadataExt;

    for startup in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let mut model = Registry::at(&path).unwrap();
        for repository in [directory.path().to_path_buf(), "/notice".into()] {
            model
                .apply(&ModelEdit::OpenProject {
                    name: "Maintenance fixture".into(),
                    repository,
                    kind: ProjectKind::Folder,
                })
                .unwrap();
        }
        let metadata = directory.path().metadata().unwrap();
        let identity = DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let definition: Definition = serde_json::from_value(serde_json::json!({
            "name":"Maintenance fixture", "prompt":"Never dispatched",
            "agent":{"provider":"Claude","account":null,
                "target":{"repository":directory.path(),"directory":directory.path(),"placement":null},
                "surface":"Chat","model":null,"permission_mode":null},
            "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},
            "session_mode":"fresh","precheck":null
        })).unwrap();
        let automation = model
            .create_automation(definition.clone(), 1, identity.clone())
            .unwrap();
        let request = Uuid::new_v4();
        let claim = model
            .claim_automation(
                automation.id,
                1,
                None,
                Trigger::Manual { request },
                None,
                true,
                2,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        let run = model
            .update_agent_run(
                accepted.run.id,
                accepted.run.revision,
                Update::Started {
                    provider_key: "model-only-fixture".into(),
                },
                3,
            )
            .unwrap();
        model
            .update_agent_run(
                run.id,
                run.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                4,
            )
            .unwrap();
        let done = model.reconcile_automation_run(run.id, 5).unwrap().unwrap();
        let agent = model.agent(run.agent).unwrap();
        let closed = model.close_agent(&agent.origin, agent.id).unwrap();
        model
            .finish_automation_release(done.id, &closed, 6)
            .unwrap();
        model.remove_automation(automation.id, 1, 7).unwrap();

        let mut notice = definition.clone();
        notice.agent.target.repository = "/notice".into();
        notice.agent.target.directory = "/notice".into();
        let notice = model
            .create_automation(notice, 8, identity.clone())
            .unwrap();
        model
            .set_automation_enabled(
                notice.id,
                notice.revision,
                Some(Occurrence {
                    due_ms: u64::MAX / 2,
                    local: "future fixture".into(),
                }),
                true,
                9,
            )
            .unwrap();
        model
            .apply(&ModelEdit::CloseProject {
                repository: "/notice".into(),
            })
            .unwrap();

        let observed = if startup {
            None
        } else {
            let mut precheck = definition;
            precheck.precheck = Some(crate::automations::Precheck {
                command: "exit 7".into(),
                timeout_seconds: 5,
            });
            let automation = model.create_automation(precheck, 10, identity).unwrap();
            Some(
                model
                    .claim_automation(
                        automation.id,
                        1,
                        None,
                        Trigger::Manual {
                            request: Uuid::new_v4(),
                        },
                        None,
                        true,
                        11,
                    )
                    .unwrap()
                    .firing,
            )
        };
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("history-maintenance"),
            boot_id: BootId::new("history-maintenance"),
            host_incarnation: HostIncarnation::new("history-maintenance"),
        });
        state.install_model(model);
        let host = Arc::new(HostLock::new(state));
        let registry = agent_execution::registry(&host).unwrap();
        let (notifications, mut journal) =
            JournalTestBridge::new(&directory.path().join("notices.sqlite3")).unwrap();
        host.lock().unwrap().notifications = Some(notifications);
        let before_firing = registry
            .lock()
            .unwrap()
            .automation_firing(claim.firing.id)
            .unwrap();
        let before_run = registry.lock().unwrap().agent_run(run.id).unwrap();
        let pending_run = || {
            registry
                .lock()
                .unwrap()
                .pending_automation_attention(100)
                .unwrap()
                .into_iter()
                .map(|entry| (entry.receipt, entry.event))
                .collect::<Vec<_>>()
        };
        let before_run_attention = pending_run();
        assert_eq!(before_run_attention.len(), 1);
        let before_no_run = registry
            .lock()
            .unwrap()
            .pending_automation_notifications(100)
            .unwrap();
        assert_eq!(before_no_run.len(), 1);
        let db = rusqlite::Connection::open(&path).unwrap();
        let fences = || {
            db.query_row(
                "SELECT count(*) FROM automation_retired_manual_requests",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap()
        };
        assert_eq!(fences(), 0);
        registry
            .lock()
            .unwrap()
            .set_automation_history_prune_failure_for_test(Some(claim.firing.id))
            .unwrap();
        let error = registry
            .lock()
            .unwrap()
            .maintain_removed_automation_history(service::now().unwrap())
            .unwrap_err();
        assert!(error.contains("fixture history DELETE failure"), "{error}");
        assert_eq!(
            fences(),
            0,
            "manual fence insertion must roll back with the DELETE"
        );
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .automation_firing(claim.firing.id)
                .unwrap(),
            before_firing
        );
        assert_eq!(
            registry.lock().unwrap().agent_run(run.id).unwrap(),
            before_run
        );
        assert_eq!(pending_run(), before_run_attention);
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .pending_automation_notifications(100)
                .unwrap(),
            before_no_run
        );
        let root = directory.path().join("prechecks");
        std::fs::create_dir(&root).unwrap();

        if startup {
            let gate = host.lock().unwrap().control_gate.clone();
            let exclusive = gate.write().unwrap();
            let broker =
                Broker::start(&host, root).expect("optional prune failure cannot prevent startup");
            drop(broker); // Normal scheduler stop/join, even while control is held.
            drop(exclusive);
            assert_eq!(pending_run(), before_run_attention);
            assert_eq!(
                registry
                    .lock()
                    .unwrap()
                    .pending_automation_notifications(100)
                    .unwrap(),
                before_no_run
            );
            assert_eq!(fences(), 0);
            continue;
        }

        let handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(&host),
            registry: registry.clone(),
            root,
            wake: Wake::default(),
            workers: Mutex::default(),
            cleanup: Mutex::new(None),
            attention: Mutex::default(),
            notifications: Mutex::default(),
        }));
        let mut maintained = Instant::now().checked_sub(Duration::from_secs(61)).unwrap();
        handle
            .tick_with_history_maintenance(&host, &mut maintained)
            .unwrap();
        assert!(
            maintained.elapsed() < Duration::from_secs(60),
            "failed attempt retains finite backoff"
        );
        assert_eq!(fences(), 0);
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .automation_firing(claim.firing.id)
                .unwrap(),
            before_firing
        );
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .automation_firing(observed.as_ref().unwrap().id)
                .unwrap()
                .state,
            FiringState::Interrupted,
            "real ordinary observation must run after maintenance Err"
        );
        let queued = journal.pending_events();
        assert!(
            !queued.is_empty(),
            "both delivery paths must reach the actual journal queue"
        );
        assert!(queued.contains(&before_run_attention[0].1));
        let expected_events = {
            let model = registry.lock().unwrap();
            let pending = model.pending_automation_notifications(100).unwrap();
            assert_eq!(pending.len(), 2);
            assert!(pending.contains(&before_no_run[0]));
            assert!(pending.iter().any(|entry| {
                entry.event().firing == Some(observed.as_ref().unwrap().id)
                    && entry.event().cause
                        == crate::automations::LifecycleCause::PrecheckInterrupted
            }));
            let mut events = vec![before_run_attention[0].1.clone()];
            for entry in pending {
                let mut event = notification_event::build(&model, &entry).unwrap();
                if entry == before_no_run[0] {
                    assert!(queued.contains(&event));
                }
                // Real journal policy is metadata-only by default.
                event.content = None;
                events.push(event);
            }
            events
        };
        assert_eq!(pending_run(), before_run_attention, "enqueue is not an ACK");
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_automation_notifications(100)
                .unwrap()
                .contains(&before_no_run[0])
        );
        registry
            .lock()
            .unwrap()
            .set_automation_history_prune_failure_for_test(None)
            .unwrap();
        // Removing the fault does not bypass the maintenance retry interval.
        let attempted = maintained;
        handle
            .tick_with_history_maintenance(&host, &mut maintained)
            .unwrap();
        assert_eq!(maintained, attempted);
        assert_eq!(fences(), 0);
        for _ in 0..4 {
            for _ in 0..8 {
                if journal.pending_events().is_empty() {
                    break;
                }
                journal.commit_next(&Preferences::default(), false).unwrap();
            }
            assert!(
                journal.pending_events().is_empty(),
                "bounded fixture journal drain"
            );
            handle
                .tick_with_history_maintenance(&host, &mut maintained)
                .unwrap();
        }
        assert!(pending_run().is_empty());
        assert!(
            registry
                .lock()
                .unwrap()
                .pending_automation_notifications(100)
                .unwrap()
                .is_empty()
        );
        let notices = journal.list().unwrap();
        assert_eq!(
            notices.len(),
            3,
            "run, ProjectClosed and genuine interrupted precheck receipts"
        );
        for event in expected_events {
            assert_eq!(
                notices
                    .iter()
                    .filter(|notice| notice.event == event)
                    .count(),
                1,
                "the original exact event must commit once before its source ACK"
            );
        }
        assert!(
            handle
                .0
                .workers
                .lock()
                .unwrap()
                .values()
                .all(|worker| !worker.owns_child_for_test()),
            "ordinary observation must not launch a precheck or provider"
        );
        registry
            .lock()
            .unwrap()
            .maintain_removed_automation_history(service::now().unwrap())
            .unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .automation_firing(claim.firing.id)
                .is_err()
        );
        assert_eq!(fences(), 1);
        assert!(
            registry
                .lock()
                .unwrap()
                .previous_manual_automation_firing(automation.id, 1, request)
                .is_err(),
            "expired original request must not be accepted again"
        );
    }
}
