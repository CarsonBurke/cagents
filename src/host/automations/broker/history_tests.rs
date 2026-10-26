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
    for use_startup_sweep in [false, true] {
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
        let host = Arc::new(HostLock::new(state));
        let registry = agent_execution::registry(&host).unwrap();
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
        let handle = Handle(Arc::new(Inner {
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
            model
                .set_automation_history_ack_failure_for_test(false)
                .unwrap();
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
        handle.tick(&host).unwrap();
        let model = registry.lock().unwrap();
        assert_eq!(
            model.automation_firing(claim.firing.id).unwrap().state,
            FiringState::Skipped
        );
        let markers: usize = fault_db
            .query_row(
                "SELECT count(*) FROM automation_precheck_history_cleanup WHERE firing=?1",
                [claim.firing.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(markers, 1);
        let runs: usize = fault_db
            .query_row("SELECT count(*) FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(runs, 0, "exit7 must never accept a provider run");
    }
}
