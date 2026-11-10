use super::*;
use crate::model::{
    BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity,
};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
};

type Record = crate::upgrade::floor::Record;

struct FakeSource {
    preview: impact::Preview,
    inventory: Inventory,
    events: Arc<Mutex<Vec<String>>>,
    observations: Arc<AtomicUsize>,
    settled: Arc<AtomicBool>,
    records: Arc<Mutex<Vec<Record>>>,
    outcomes: VecDeque<ShutdownOutcome>,
    prepare_refusals: usize,
    lose_prepare_ack: bool,
    predicted_service: bool,
    fail_inventory: bool,
    foreign_preparation: bool,
    preparation_checks: usize,
    close_attempts: usize,
    fail_close_at: Option<usize>,
    change_on_first_close: bool,
    retain_attempts: usize,
    fail_retain_at: Option<usize>,
    fail_stopped: bool,
    fail_archive: bool,
    change_after_capture: bool,
    cancel_on_capture: Option<PathBuf>,
    cancel_after_close: Option<PathBuf>,
    interrupt_polls: usize,
    never_confirm_interrupts: bool,
    interrupt_pending: Vec<TerminalId>,
    change_after_interrupt: bool,
    cancel_on_interrupt: Option<PathBuf>,
}

impl FakeSource {
    fn event(&self, event: &str) {
        self.events.lock().unwrap().push(event.into());
    }
    fn ledger(&self) -> crate::upgrade::floor::Ledger {
        crate::upgrade::floor::Ledger {
            records: self.records.lock().unwrap().clone(),
        }
    }
    fn assert_running_panes_are_unmarked(&self) {
        let ledger = self.ledger();
        for chat in &self.inventory.chats {
            assert!(!ledger.blocks_chat(chat), "running chat was marked stopped");
            assert!(
                ledger.resume_source(chat).is_none(),
                "later ordinary update would skip a running chat"
            );
        }
        for terminal in &self.inventory.terminals {
            assert!(
                !ledger.blocks_terminal(terminal.session.id),
                "running terminal was marked stopped"
            );
        }
    }
}

impl FloorSource for FakeSource {
    fn observe(&mut self) -> Result<impact::Preview, UpdateError> {
        self.observations.fetch_add(1, Ordering::Release);
        if self.settled.load(Ordering::Acquire) {
            for item in &mut self.preview.daemon.as_mut().unwrap().sessions {
                if item.disposition == impact::Disposition::WaitsForIdle {
                    item.disposition = impact::Disposition::Resumes;
                }
            }
        }
        Ok(self.preview.clone())
    }
    fn observe_policy(&mut self, policy: impact::Policy) -> Result<impact::Preview, UpdateError> {
        let daemon = self.preview.daemon.as_mut().unwrap();
        daemon.policy = policy;
        if policy == impact::Policy::Interrupt && needs_service_stop(daemon) {
            for session in &mut daemon.sessions {
                if session.disposition == impact::Disposition::WaitsForIdle {
                    session.disposition = impact::Disposition::Interrupted;
                    session.interrupted_subagents = vec!["reviewed-background-task".into()];
                }
            }
        }
        self.observe()
    }
    fn inventory(&mut self, _: &impact::DaemonImpact, _: bool) -> Result<Inventory, UpdateError> {
        if self.fail_inventory {
            return Err(invalid("legacy lifecycle source has no pane inventory API"));
        }
        Ok(self.inventory.clone())
    }
    fn begin_preparation(
        &mut self,
        _: &Path,
        _: &UpdateJob,
        _: &impact::DaemonImpact,
    ) -> Result<(), UpdateError> {
        if self.fail_inventory {
            self.preparation_checks += 1;
            preparation_census(
                true,
                || {
                    if self.foreign_preparation {
                        Err(
                            "A checkpoint already exists; it was kept without preparing the source"
                                .into(),
                        )
                    } else {
                        Ok(())
                    }
                },
                || Err(invalid("legacy lifecycle source has no pane inventory API")),
            )?;
        }
        Ok(())
    }
    fn prepare(&mut self, _: &impact::DaemonImpact) -> Result<(), source::PrepareError> {
        if self.lose_prepare_ack {
            self.event("PrepareConfirmedUpgrade sent; acknowledgement lost");
            return Err(source::PrepareError::Unconfirmed(invalid(
                "connection closed",
            )));
        }
        if self.prepare_refusals > 0 {
            self.prepare_refusals -= 1;
            self.event("PrepareConfirmedUpgrade refused");
            Err(source::PrepareError::Refused(invalid(
                "No exact resumable conversation",
            )))
        } else {
            self.event("PrepareConfirmedUpgrade accepted");
            Ok(())
        }
    }
    fn begin_interrupts(
        &mut self,
        daemon: &impact::DaemonImpact,
        _: Instant,
    ) -> Result<(), UpdateError> {
        self.event("Interrupt acknowledged; provider still working");
        self.interrupt_pending = daemon
            .sessions
            .iter()
            .filter(|item| item.disposition == impact::Disposition::Interrupted)
            .map(|item| item.id)
            .collect();
        if let Some(state) = &self.cancel_on_interrupt {
            let job: UpdateJob = serde_json::from_slice(&fs::read(state.join("status.json"))?)?;
            assert!(confirmation::cancel_at(state, &job.id)?);
        }
        Ok(())
    }
    fn pending_interrupts(&mut self) -> Result<Vec<TerminalId>, UpdateError> {
        if self.never_confirm_interrupts || self.interrupt_polls > 0 {
            self.interrupt_polls = self.interrupt_polls.saturating_sub(1);
            self.event("provider still working");
            return Ok(self.interrupt_pending.clone());
        }
        self.event("provider interruption confirmed; run inactive");
        for item in &mut self.preview.daemon.as_mut().unwrap().sessions {
            if self.interrupt_pending.contains(&item.id) {
                item.disposition = impact::Disposition::Resumes;
                item.interrupted_subagents.clear();
                if self.change_after_interrupt {
                    item.identity.push_str(" replacement owner");
                }
            }
        }
        self.interrupt_pending.clear();
        self.preview.daemon.as_mut().unwrap().policy = impact::Policy::WhenIdle;
        Ok(Vec::new())
    }
    fn interrupt_impact(&self) -> Result<impact::DaemonImpact, UpdateError> {
        Ok(self.preview.daemon.clone().unwrap())
    }
    fn close_chat(
        &mut self,
        _: &impact::DaemonImpact,
        chat: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError> {
        self.close_attempts += 1;
        if self.change_on_first_close && self.close_attempts == 1 {
            self.event("ChatClose descriptor changed");
            let changed = self
                .inventory
                .chats
                .iter_mut()
                .find(|source| source.id == chat.id)
                .unwrap();
            changed.launch.session_id = Some("replacement-conversation".into());
            self.preview
                .daemon
                .as_mut()
                .unwrap()
                .sessions
                .iter_mut()
                .find(|item| item.id == chat.id)
                .unwrap()
                .identity
                .push_str(" replacement owner");
            return Err(invalid(
                "chat attachment no longer matches the reviewed descriptor",
            ));
        }
        if self.fail_close_at == Some(self.close_attempts) {
            self.event("ChatClose refused");
            return Err(invalid("fake close refused"));
        }
        self.event("ChatClose acknowledged");
        self.inventory.chats.retain(|current| current.id != chat.id);
        let daemon = self.preview.daemon.as_mut().unwrap();
        daemon.sessions.retain(|item| item.id != chat.id);
        daemon.capability.inventory_generation += 1;
        if let Some(state) = &self.cancel_after_close {
            let job: UpdateJob = serde_json::from_slice(&fs::read(state.join("status.json"))?)?;
            assert!(!confirmation::cancel_at(state, &job.id)?);
            self.event("cancellation refused after close");
        }
        Ok(crate::chat::Snapshot {
            session_id: chat.launch.session_id,
            exited: true,
            ..Default::default()
        })
    }
    fn capture_record(
        &mut self,
        daemon: &impact::DaemonImpact,
        chats: Vec<crate::chat::Session>,
        terminals: Vec<TerminalId>,
        service: bool,
    ) -> Result<Record, UpdateError> {
        self.event("capture record");
        let mut record = Record::new(daemon.capability.proof.clone(), chats, terminals);
        record.service_stopped = service;
        record.protocol = 156;
        record.snapshots = record
            .chats
            .iter()
            .map(|chat| crate::upgrade::floor::StoppedSnapshot {
                source: chat.clone(),
                snapshot: crate::chat::Snapshot::default(),
                limitation: None,
            })
            .collect();
        Ok(record)
    }
    fn predict_service(&mut self, _: &impact::Preview, _: &Inventory) -> Result<bool, UpdateError> {
        Ok(self.predicted_service)
    }
    fn preflight(
        &mut self,
        daemon: &impact::DaemonImpact,
    ) -> Result<ShutdownCapability, UpdateError> {
        self.event("ShutdownV1 Preflight");
        Ok(daemon.capability.clone())
    }
    fn shutdown(
        &mut self,
        daemon: &impact::DaemonImpact,
        capability: ShutdownCapability,
    ) -> Result<ShutdownOutcome, UpdateError> {
        assert_eq!(daemon.capability.proof, capability.proof);
        self.event("ShutdownV1 Confirm");
        let outcome = self
            .outcomes
            .pop_front()
            .unwrap_or(ShutdownOutcome::Accepted {
                live_terminals_before: 1,
                termination_requested: true,
            });
        if matches!(outcome, ShutdownOutcome::InventoryChanged { .. }) {
            self.inventory.terminals[0]
                .session
                .launch
                .arguments
                .push("new foreground owner".into());
            self.preview
                .daemon
                .as_mut()
                .unwrap()
                .capability
                .inventory_generation += 1;
        } else if matches!(outcome, ShutdownOutcome::Accepted { .. }) {
            self.inventory.chats.clear();
            self.inventory.terminals.clear();
        }
        Ok(outcome)
    }
    fn retain(&mut self, _: &impact::DaemonImpact, record: Record) -> Result<(), UpdateError> {
        self.retain_attempts += 1;
        if self.fail_retain_at == Some(self.retain_attempts) {
            self.event("retain failed");
            return Err(invalid("fake retention failed"));
        }
        self.event("retain acknowledged");
        self.records.lock().unwrap().push(record);
        Ok(())
    }
    fn capture(&mut self, _: &StopList) -> Result<(), UpdateError> {
        if let Some(state) = &self.cancel_on_capture {
            let job: UpdateJob = serde_json::from_slice(&fs::read(state.join("status.json"))?)?;
            assert!(confirmation::cancel_at(state, &job.id)?);
            self.event("cancelled during capture");
        }
        if self.change_after_capture {
            self.change_after_capture = false;
            self.inventory.terminals[0]
                .session
                .launch
                .arguments
                .push("changed command".into());
            self.preview.daemon.as_mut().unwrap().sessions[0]
                .identity
                .push_str(" changed owner");
        }
        Ok(())
    }
    fn archive(&mut self) -> Result<(), UpdateError> {
        self.event("archive checkpoint");
        if self.fail_archive {
            Err(invalid("fake archive failed"))
        } else {
            Ok(())
        }
    }
    fn stopped(&mut self, _: &Path, _: &mut UpdateJob, service: bool) -> Result<(), UpdateError> {
        self.event(if service {
            "handoff without checkpoint"
        } else {
            "handoff with checkpoint"
        });
        if self.fail_stopped {
            Err(invalid("fake wait failed"))
        } else {
            Ok(())
        }
    }
}

fn fixture() -> FakeSource {
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("source"),
    };
    let mut terminal = TerminalDescriptor::relaunchable(
        TerminalId::new(),
        LaunchRecipe {
            program: "bash".into(),
            arguments: Vec::new(),
            working_directory: "/work".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        None,
    );
    terminal.session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let daemon = impact::DaemonImpact {
        capability: ShutdownCapability {
            proof,
            token: "capability".into(),
            inventory_generation: 1,
            live_terminals: 1,
            background_subagents: 0,
            background_identity: String::new(),
            expires_at_unix_ms: i64::MAX,
        },
        policy: impact::Policy::WhenIdle,
        sessions: vec![impact::SessionImpact {
            id: terminal.session.id,
            kind: impact::SessionKind::Terminal,
            title: "build".into(),
            identity: "exact terminal owner".into(),
            disposition: impact::Disposition::Continues,
            reason: String::new(),
            interrupted_subagents: Vec::new(),
        }],
        blockers: Vec::new(),
    };
    FakeSource {
        preview: impact::Preview {
            editors: Vec::new(),
            candidate: impact::Candidate {
                version: "new".into(),
                protocol: 160,
                digest: "verified".into(),
            },
            replaces_daemon: true,
            daemon: Some(daemon),
            from_version: "old".into(),
            windows: 1,
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
        },
        inventory: Inventory {
            chats: Vec::new(),
            chat_states: BTreeMap::new(),
            terminals: vec![terminal],
        },
        events: Arc::new(Mutex::new(Vec::new())),
        observations: Arc::new(AtomicUsize::new(0)),
        settled: Arc::new(AtomicBool::new(false)),
        records: Arc::new(Mutex::new(Vec::new())),
        outcomes: VecDeque::new(),
        prepare_refusals: 1,
        lose_prepare_ack: false,
        predicted_service: false,
        fail_inventory: false,
        foreign_preparation: false,
        preparation_checks: 0,
        close_attempts: 0,
        fail_close_at: None,
        change_on_first_close: false,
        retain_attempts: 0,
        fail_retain_at: None,
        fail_stopped: false,
        fail_archive: false,
        change_after_capture: false,
        cancel_on_capture: None,
        cancel_after_close: None,
        interrupt_polls: 0,
        never_confirm_interrupts: false,
        interrupt_pending: Vec::new(),
        change_after_interrupt: false,
        cancel_on_interrupt: None,
    }
}

fn add_chat(
    source: &mut FakeSource,
    disposition: impact::Disposition,
    recorded: bool,
) -> TerminalId {
    let id = TerminalId::new();
    let chat = crate::chat::Session {
        id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: source.inventory.terminals[0].session.launch.clone(),
            session_id: recorded.then(|| format!("recorded-{id}")),
        },
        proof: Some(
            source
                .preview
                .daemon
                .as_ref()
                .unwrap()
                .capability
                .proof
                .clone(),
        ),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    source.inventory.chats.push(chat);
    source
        .preview
        .daemon
        .as_mut()
        .unwrap()
        .sessions
        .push(impact::SessionImpact {
            id,
            kind: impact::SessionKind::NativeChat,
            title: format!("chat-{id}"),
            identity: id.to_string(),
            disposition,
            reason: "cannot checkpoint".into(),
            interrupted_subagents: Vec::new(),
        });
    id
}

fn job(directory: &Path) -> UpdateJob {
    let job = UpdateJob {
        id: uuid::Uuid::new_v4().to_string(),
        version: "new".into(),
        phase: UpdatePhase::Verifying,
        message: String::new(),
        log_path: directory.join("worker.log"),
        confirmation: None,
    };
    let file = private_create(&directory.join(format!("{}.request", job.id))).unwrap();
    serde_json::to_writer(
        file,
        &UpdateRequest::InstalledFromTerminal {
            root: "/fake".into(),
        },
    )
    .unwrap();
    confirmation::initialize(directory, &job.id).unwrap();
    write_job(directory, &job).unwrap();
    job
}

fn interrupt_fixture() -> (FakeSource, TerminalId) {
    let mut source = fixture();
    let daemon = source.preview.daemon.as_mut().unwrap();
    daemon.policy = impact::Policy::Interrupt;
    daemon.blockers.clear();
    for item in &mut daemon.sessions {
        item.disposition = impact::Disposition::Continues;
    }
    source.prepare_refusals = 0;
    let id = add_chat(&mut source, impact::Disposition::Interrupted, true);
    source
        .preview
        .labels
        .names
        .insert(id, "Offline tool rows · alpha".into());
    (source, id)
}

#[test]
fn interrupted_unblocked_source_waits_for_provider_and_inactive_run_before_prepare() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (mut source, _) = interrupt_fixture();
    source.interrupt_polls = 2;
    let preview = source.preview.clone();
    assert!(!preview.daemon.as_ref().unwrap().blocked());
    quiesce_source(path, &mut job(path), &preview, true, &mut source).unwrap();
    let events = source.events.lock().unwrap();
    let confirmed = events
        .iter()
        .position(|event| event == "provider interruption confirmed; run inactive")
        .unwrap();
    let prepared = events
        .iter()
        .position(|event| event == "PrepareConfirmedUpgrade accepted")
        .unwrap();
    assert!(confirmed < prepared, "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "provider still working")
            .count(),
        2
    );
    assert!(
        !events.iter().any(|event| event.contains("ChatClose")
            || event.contains("shutdown")
            || event.contains("archive")),
        "{events:?}"
    );
}

#[test]
fn lost_prepare_ack_does_not_reach_stop_review_or_automatic_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (mut source, _) = interrupt_fixture();
    source.lose_prepare_ack = true;
    let preview = source.preview.clone();
    let mut current_job = job(path);
    let error = quiesce_source(path, &mut current_job, &preview, true, &mut source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("preparation was not confirmed"), "{error}");
    assert!(
        error.contains("automatic recovery was not authorized"),
        "{error}"
    );
    assert!(current_job.confirmation.is_none());
    let events = source.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.contains("acknowledgement lost"))
    );
    assert!(
        !events.iter().any(|event| event.contains("ChatClose")
            || event.contains("shutdown")
            || event.contains("handoff")
            || event.contains("archive")),
        "{events:?}"
    );
    drop(events);
    source.assert_running_panes_are_unmarked();
}

#[test]
fn settled_interruption_preserves_known_blocked_panes_for_their_stop_review() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (mut source, interrupted) = interrupt_fixture();
    let blocked = add_chat(&mut source, impact::Disposition::Blocked, true);
    let before = source.preview.clone();
    let settled = settle_interrupts(
        path,
        &mut job(path),
        before.clone(),
        &mut source,
        Duration::from_secs(1),
    )
    .unwrap();
    let daemon = settled.daemon.as_ref().unwrap();
    assert_eq!(daemon.policy, impact::Policy::WhenIdle);
    assert!(daemon.sessions.iter().any(|item| {
        item.id == interrupted && item.disposition == impact::Disposition::Resumes
    }));
    assert!(
        daemon
            .sessions
            .iter()
            .any(|item| { item.id == blocked && item.disposition == impact::Disposition::Blocked })
    );
    assert!(daemon.blocked());
    assert_no_destructive_actions(&source);
    source.assert_running_panes_are_unmarked();
}

#[test]
fn recovered_paused_conversations_name_the_manual_resume_action() {
    let (mut source, chat) = interrupt_fixture();
    let unrecorded = add_chat(&mut source, impact::Disposition::Resumes, false);
    let mut survivors = Survivors::default();
    survivors.record_prepared(&source.preview, source.preview.daemon.as_ref().unwrap());
    survivors.prepared_source = Some(PreparationEvidence::Full {
        daemon: source.preview.daemon.clone().unwrap(),
        census: crate::upgrade::recovery::Census {
            chats: source.inventory.chats.clone(),
            ..Default::default()
        },
    });
    let terminal = TerminalId::new();
    survivors
        .labels
        .insert(chat, "Offline tool rows · alpha".into());
    survivors
        .labels
        .insert(terminal, "Build shell · alpha".into());
    survivors
        .labels
        .insert(unrecorded, "Unrecorded chat · alpha".into());
    survivors.recovery_outcome = Some(crate::upgrade::recovery::RecoveryOutcome {
        paused_chats: vec![chat, unrecorded],
        paused_terminals: vec![terminal],
    });
    let report = survivors.paused_report();
    assert!(report.contains("Offline tool rows · alpha: choose Resume conversation in the new tab (+) menu to continue"), "{report}");
    assert!(
        report.contains("Build shell · alpha: reopen a terminal to continue"),
        "{report}"
    );
    assert!(!report.contains("resumes after the update"), "{report}");
    assert!(
        report.contains("Unrecorded chat · alpha: start a new conversation to continue"),
        "{report}"
    );
    assert!(
        !report.contains("Unrecorded chat · alpha: choose Resume"),
        "{report}"
    );
    assert!(
        survivors
            .prepared_report()
            .contains("its conversation is paused after recovery")
    );
    assert!(
        !survivors
            .prepared_report()
            .contains("resumes after the update")
    );
}

#[test]
fn zombie_source_metadata_does_not_mask_prepared_failure_or_start_another_daemon() {
    use std::os::unix::fs::PermissionsExt;

    const CHILD: &str = "CAGENTS_TEST_UPDATE_ZOMBIE_CLEANUP";
    const TEST: &str = "update::floor::tests::zombie_source_metadata_does_not_mask_prepared_failure_or_start_another_daemon";
    let Some(root) = std::env::var_os(CHILD) else {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD, root.path())
            .env("HOME", root.path())
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    };
    let root = PathBuf::from(root);
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    fs::create_dir_all(&paths.directory).unwrap();
    fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o700)).unwrap();
    let mut daemon = fixture().preview.daemon.unwrap();
    daemon.sessions.clear();
    daemon.capability.live_terminals = 0;
    daemon.capability.proof.boot_id = crate::daemon::read_boot_id().unwrap();
    let child = Command::new("/usr/bin/python3")
        .arg("-c")
        .arg(
            r#"import fcntl,json,os,socket,sys
lock=os.open(sys.argv[1],os.O_RDWR|os.O_CREAT,0o600)
fcntl.flock(lock,fcntl.LOCK_EX)
os.write(lock,json.dumps({'pid':os.getpid(),'proof':json.loads(sys.argv[3])}).encode())
listener=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
listener.bind(sys.argv[2]);listener.listen()
"#,
        )
        .arg(&paths.lock)
        .arg(&paths.socket)
        .arg(serde_json::to_string(&daemon.capability.proof).unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut source = StreamingChild::new(child).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while source.observed_exit_status().unwrap().is_none() {
        assert!(Instant::now() < deadline, "private source did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(source.observed_exit_status().unwrap().unwrap().success());
    // WNOWAIT pins this real exited child as a zombie through the handoff.
    let stat = fs::read_to_string(format!("/proc/{}/stat", source.child.id())).unwrap();
    assert_eq!(
        stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
        Some("Z")
    );
    let mut handoff =
        Some(crate::daemon::lease_handoff::LeaseHandoff::wait(&paths, source.child.id()).unwrap());
    assert!(
        crate::daemon::running_executable_at(&paths).is_err(),
        "a stale source PID cannot authenticate the worker's held handoff"
    );
    assert!(matches!(
        cleanup_daemon(&paths, true),
        CleanupDaemon::Absent
    ));
    let store = crate::upgrade::handoff::Store::discover().unwrap();
    let checkpoint = serde_json::to_vec(&serde_json::json!({
        "version": 5, "proof": daemon.capability.proof, "phase": "Prepared",
        "terminals": [], "carried": [], "chats": []
    }))
    .unwrap();
    let mut file = private_create(&store.path).unwrap();
    file.write_all(&checkpoint).unwrap();
    file.sync_all().unwrap();
    let survivors = Survivors {
        source_prepared: true,
        prepared_source: Some(PreparationEvidence::Full {
            daemon,
            census: crate::upgrade::recovery::Census::default(),
        }),
        ..Default::default()
    };
    let state = root.join("update");
    fs::create_dir(&state).unwrap();
    let mut job = job(&state);
    let guard = crate::instance::UiUpdateGuard::acquire().unwrap();
    let installation = Installation {
        prefix: root.join("install"),
        version: "old".into(),
    };
    let marker = root.join("unexpected-daemon-start");
    let bin = installation.prefix.join("current/bin");
    fs::create_dir_all(&bin).unwrap();
    let cli = bin.join("cagents");
    fs::write(
        &cli,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let original = "the original accepted preparation could not produce a recovery receipt";
    let error = finish_handoff(
        &installation,
        &state,
        &mut job,
        &Err(invalid(original)),
        &mut handoff,
        &survivors,
        &guard,
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with(original), "{error}");
    assert!(error.contains("still Prepared"), "{error}");
    assert!(!error.contains("PID does not own"), "{error}");
    let saved: UpdateJob =
        serde_json::from_slice(&fs::read(state.join("status.json")).unwrap()).unwrap();
    assert_eq!(saved.phase, UpdatePhase::Failed);
    assert_eq!(saved.message, error);
    assert!(
        !marker.exists(),
        "Prepared must never reach ordinary daemon start"
    );
    assert_eq!(fs::read(&store.path).unwrap(), checkpoint);
    assert!(handoff.is_none());
    source.finish(false).unwrap();
}

#[test]
fn unconfirmed_interrupt_times_out_before_prepare_and_keeps_panes_running() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (mut source, id) = interrupt_fixture();
    source.never_confirm_interrupts = true;
    let preview = source.preview.clone();
    let mut current_job = job(path);
    let error = settle_interrupts(
        path,
        &mut current_job,
        preview,
        &mut source,
        Duration::from_millis(20),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("Offline tool rows · alpha did not stop in time; nothing was changed"),
        "{error}"
    );
    let events = source.events.lock().unwrap();
    assert!(
        !events.iter().any(|event| event.contains("Prepare")
            || event.contains("ChatClose")
            || event.contains("shutdown")
            || event.contains("archive")),
        "{events:?}"
    );
    assert!(source.inventory.chats.iter().any(|chat| chat.id == id));
    source.assert_running_panes_are_unmarked();
    assert!(
        confirmation::cancel_at(path, &current_job.id).unwrap(),
        "preparation did not revoke cancellation"
    );
}

#[test]
fn changed_owner_after_interrupt_never_prepares_or_stops_service() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (mut source, _) = interrupt_fixture();
    source.change_after_interrupt = true;
    let preview = source.preview.clone();
    let error = quiesce_source(path, &mut job(path), &preview, true, &mut source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Conversations changed"), "{error}");
    assert!(
        !source
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.contains("Prepare") || event.contains("shutdown"))
    );
    source.assert_running_panes_are_unmarked();
}

#[test]
fn interrupt_settlement_remains_cancellable_before_prepare() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (mut source, _) = interrupt_fixture();
    source.never_confirm_interrupts = true;
    source.cancel_on_interrupt = Some(path.into());
    let preview = source.preview.clone();
    let error = quiesce_source(path, &mut job(path), &preview, true, &mut source).unwrap_err();
    assert!(matches!(error, UpdateError::Cancelled));
    assert!(
        !source
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.contains("Prepare") || event.contains("shutdown"))
    );
    source.assert_running_panes_are_unmarked();
}

#[test]
fn partial_interruption_effects_are_reported_without_claiming_daemon_preparation() {
    let (mut source, first) = interrupt_fixture();
    let second = add_chat(&mut source, impact::Disposition::Interrupted, true);
    source
        .preview
        .labels
        .names
        .insert(second, "Still working · alpha".into());
    let daemon = source.preview.daemon.as_ref().unwrap();
    let mut survivors = Survivors::default();
    survivors.record_interrupt_requests(&source.preview, daemon);
    survivors.record_settled_interruptions(&source.preview, daemon, &[second]);
    survivors.late_turns.insert(first);
    let report = survivors.prepared_report();
    assert!(
        report.contains("Offline tool rows · alpha: current turn interrupted"),
        "{report}"
    );
    assert!(
        report.contains("also stopped a turn that started during review"),
        "{report}"
    );
    assert!(
        report.contains("Interruption was not confirmed:\n  Still working · alpha"),
        "{report}"
    );
    assert!(!survivors.needs_daemon_recovery());
    assert!(!survivors.source_prepared);
}

fn await_prompt(directory: &Path, predicate: impl Fn(&UpdateConfirmation) -> bool) -> UpdateJob {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = fs::read(directory.join("status.json"))
            && let Ok(job) = serde_json::from_slice::<UpdateJob>(&bytes)
            && job.confirmation.as_ref().is_some_and(&predicate)
        {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not publish the expected confirmation"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn await_stop(directory: &Path) -> UpdateJob {
    await_prompt(directory, |confirmation| confirmation.stop.is_some())
}
fn submit(directory: &Path, job: &UpdateJob, choice: UpdateChoice) -> PathBuf {
    let nonce = &job.confirmation.as_ref().unwrap().nonce;
    let temporary = directory.join(uuid::Uuid::new_v4().to_string());
    let file = private_create(&temporary).unwrap();
    serde_json::to_writer(
        &file,
        &serde_json::json!({"job":job.id,"nonce":nonce,"choice":choice}),
    )
    .unwrap();
    file.sync_all().unwrap();
    let path = directory.join(format!("{}.{}.decision", job.id, nonce));
    fs::hard_link(&temporary, &path).unwrap();
    fs::remove_file(temporary).unwrap();
    path
}
fn confirm_stop(directory: &Path, job: &UpdateJob) {
    let digest = job
        .confirmation
        .as_ref()
        .unwrap()
        .stop
        .as_ref()
        .unwrap()
        .digest();
    submit(
        directory,
        job,
        UpdateChoice::StopListed {
            items_digest: digest,
        },
    );
}
fn spawn_source(
    mut source: FakeSource,
    directory: &Path,
    terminal: bool,
) -> thread::JoinHandle<(Result<(), UpdateError>, FakeSource)> {
    let path = directory.to_owned();
    thread::spawn(move || {
        let preview = source.preview.clone();
        let result = quiesce_source(&path, &mut job(&path), &preview, terminal, &mut source);
        (result, source)
    })
}
fn finish(
    worker: thread::JoinHandle<(Result<(), UpdateError>, FakeSource)>,
) -> (Result<(), UpdateError>, FakeSource) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        assert!(
            Instant::now() < deadline,
            "worker unexpectedly needed another prompt or stalled"
        );
        thread::sleep(Duration::from_millis(10));
    }
    worker.join().unwrap()
}
fn assert_no_destructive_actions(source: &FakeSource) {
    assert_eq!(source.close_attempts, 0);
    assert_eq!(source.retain_attempts, 0);
    assert!(source.records.lock().unwrap().is_empty());
    let events = source.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| event == "ShutdownV1 Confirm" || event == "archive checkpoint")
    );
    source.assert_running_panes_are_unmarked();
}
fn proc_fixture(root: &Path, pid: i32, parent: i32, start: u64, cgroup: &str, environment: &[u8]) {
    let directory = root.join(pid.to_string());
    fs::create_dir_all(&directory).unwrap();
    let mut fields = vec!["0".to_owned(); 20];
    fields[0] = "S".into();
    fields[1] = parent.to_string();
    fields[19] = start.to_string();
    fs::write(
        directory.join("stat"),
        format!("{pid} (fixture) {}\n", fields.join(" ")),
    )
    .unwrap();
    fs::write(directory.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
    fs::write(directory.join("environ"), environment).unwrap();
    fs::write(directory.join("cmdline"), format!("work-{pid}\0")).unwrap();
}

#[test]
fn survivor_report_finds_late_reparented_scope_and_origin_members_without_shared_scope_guesses() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let id = TerminalId::new();
    let origin = TerminalId::new();
    let scope = format!(
        "/user.slice/user-1000.slice/user@1000.service/app.slice/cagents-session-{}.scope",
        uuid::Uuid::new_v4()
    );
    let shared = "/user.slice/user-1000.slice/user@1000.service/app.slice";
    proc_fixture(root, 101, 1, 10, &format!("{scope}/provider"), b"");
    proc_fixture(root, 102, 1, 20, shared, b"");
    let list = StopList {
        background_tasks: 0,
        background_identity: String::new(),
        from_version: "old".into(),
        items: vec![super::super::floor_display::StopItem {
            id,
            kind: impact::SessionKind::Terminal,
            label: "build".into(),
            identity: "reviewed".into(),
            running: "terminal session".into(),
            recovery: "Restart".into(),
        }],
    };
    let mut survivors = Survivors::default();
    survivors.remember_origin(id, origin);
    survivors.remember_list(&list);
    survivors
        .capture_at(
            root,
            crate::daemon::current_uid(),
            BTreeMap::from([(
                101,
                (
                    Process {
                        pid: 101,
                        start: 10,
                    },
                    id,
                    "build".into(),
                ),
            )]),
        )
        .unwrap();
    assert_eq!(survivors.processes.len(), 1);
    assert_eq!(survivors.scopes.keys().collect::<Vec<_>>(), [&scope]);
    fs::remove_dir_all(root.join("101")).unwrap();
    // Both jobs appear only after the last ancestry census, and their
    // original provider/holder has already exited and been reparented.
    proc_fixture(
        root,
        201,
        1,
        31,
        &format!("{scope}/command-new/nested"),
        b"",
    );
    proc_fixture(
        root,
        202,
        1,
        32,
        shared,
        format!("CAGENTS_RECOVERY_ORIGIN={origin}\0").as_bytes(),
    );
    proc_fixture(root, 203, 1, 33, shared, b"");
    proc_fixture(
        root,
        204,
        1,
        34,
        &format!("{shared}/cagents-session-{}.scope", uuid::Uuid::new_v4()),
        b"",
    );
    proc_fixture(
        root,
        205,
        1,
        35,
        shared,
        format!("CAGENTS_RECOVERY_ORIGIN={origin}-different\0").as_bytes(),
    );
    proc_fixture(
        root,
        206,
        1,
        36,
        shared,
        format!("CAGENTS_TERMINAL_ID={id}\0").as_bytes(),
    );
    // A reused numeric PID in an unrelated shared scope is not the root
    // whose epoch was captured before shutdown.
    proc_fixture(root, 101, 1, 99, shared, b"");
    let live = survivors
        .live_at(root, crate::daemon::current_uid())
        .unwrap();
    assert_eq!(
        live.iter()
            .map(|survivor| survivor.process.pid)
            .collect::<Vec<_>>(),
        [201, 202, 206]
    );
    assert!(
        live.iter()
            .all(|survivor| survivor.id == id && survivor.description.contains("build"))
    );
    assert!(
        survivors
            .live_at(root, crate::daemon::current_uid().saturating_add(1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(session_scope(&format!("0::{shared}\n")), None);
}

#[test]
fn a_restarted_terminal_with_an_old_origin_is_not_reported_as_unattached() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let old = TerminalId::new();
    let origin = TerminalId::new();
    let restarted = TerminalId::new();
    let shared = "/user.slice/app.slice";
    proc_fixture(
        root,
        201,
        1,
        20,
        shared,
        format!("CAGENTS_TERMINAL_ID={old}\0CAGENTS_RECOVERY_ORIGIN={origin}\0").as_bytes(),
    );
    proc_fixture(
        root,
        202,
        1,
        30,
        shared,
        format!("CAGENTS_TERMINAL_ID={restarted}\0CAGENTS_RECOVERY_ORIGIN={origin}\0").as_bytes(),
    );
    let mut survivors = Survivors::default();
    survivors.markers.insert(
        format!("CAGENTS_RECOVERY_ORIGIN={origin}").into_bytes(),
        (old, "old build".into()),
    );
    let live = survivors
        .live_at(root, crate::daemon::current_uid())
        .unwrap();
    assert_eq!(live.len(), 2);
    assert!(live.iter().all(|survivor| survivor.id == old));
    let attached = BTreeSet::from([restarted, origin]);
    let unattached = Survivors::unattached_at(root, live.clone(), &attached);
    assert_eq!(
        unattached
            .iter()
            .map(|survivor| survivor.process.pid)
            .collect::<Vec<_>>(),
        [201]
    );
    // Re-read the current terminal marker when the destination inventory
    // is checked, rather than relying on the earlier origin association.
    fs::write(
        root.join("201/environ"),
        format!("CAGENTS_TERMINAL_ID={restarted}\0CAGENTS_RECOVERY_ORIGIN={origin}\0"),
    )
    .unwrap();
    assert!(Survivors::unattached_at(root, live.clone(), &attached).is_empty());
    // Reusing a PID cannot turn a captured epoch into a new survivor.
    proc_fixture(root, 201, 1, 99, shared, b"");
    assert!(Survivors::unattached_at(root, live, &attached).is_empty());
}

#[test]
fn daemon_ancestry_does_not_claim_its_inherited_session_scope() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let ambient = format!(
        "/user.slice/app.slice/cagents-session-{}.scope",
        uuid::Uuid::new_v4()
    );
    let dedicated = format!(
        "/user.slice/app.slice/cagents-session-{}.scope",
        uuid::Uuid::new_v4()
    );
    proc_fixture(root, 101, 1, 10, &ambient, b"");
    proc_fixture(root, 102, 101, 20, &ambient, b"");
    proc_fixture(root, 103, 101, 30, &format!("{dedicated}/provider"), b"");
    let mut survivors = Survivors::default();
    survivors.exclude_ambient(root, 101);
    survivors
        .capture_at(
            root,
            crate::daemon::current_uid(),
            BTreeMap::from([(
                101,
                (
                    Process {
                        pid: 101,
                        start: 10,
                    },
                    TerminalId::from_uuid(uuid::Uuid::nil()),
                    "terminal service".into(),
                ),
            )]),
        )
        .unwrap();
    assert!(!survivors.scopes.contains_key(&ambient));
    assert!(survivors.scopes.contains_key(&dedicated));
    for pid in [101, 102, 103] {
        fs::remove_dir_all(root.join(pid.to_string())).unwrap();
    }
    proc_fixture(root, 201, 1, 40, &ambient, b"");
    proc_fixture(root, 202, 1, 50, &format!("{dedicated}/command-late"), b"");
    let live = survivors
        .live_at(root, crate::daemon::current_uid())
        .unwrap();
    assert_eq!(
        live.iter()
            .map(|survivor| survivor.process.pid)
            .collect::<Vec<_>>(),
        [202]
    );
    assert!(live[0].description.contains("terminal service"));
}

fn pending_source() -> (FakeSource, TerminalId) {
    let mut source = fixture();
    let daemon = source.preview.daemon.as_mut().unwrap();
    daemon.sessions.clear();
    daemon
        .blockers
        .push("Wait for starting sessions to finish opening.".into());
    let pending = crate::chat::Session {
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: source.inventory.terminals[0].session.launch.clone(),
            session_id: None,
        },
        proof: Some(daemon.capability.proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let id = pending.id;
    source.inventory.chats.push(pending);
    source.inventory.chat_states.insert(id, "starting".into());
    source.preview.labels.names.insert(
        id,
        "Untitled conversation in beta, start time unavailable".into(),
    );
    (source, id)
}

#[test]
fn pending_launch_consent_hashes_exact_descriptors_without_exposing_private_recipes() {
    let (mut source, pending) = pending_source();
    source.inventory.chats[0]
        .launch
        .recipe
        .environment
        .insert("PRIVATE_TOKEN".into(), "private-value-before".into());
    let before = stop_list(&source.preview, &source.inventory, true).unwrap();
    assert!(
        stop_list(&source.preview, &source.inventory, false)
            .unwrap()
            .items
            .is_empty(),
        "floor A only covers chats Blocked by impact"
    );
    let private = serde_json::to_string(&before).unwrap();
    assert!(!private.contains("PRIVATE_TOKEN") && !private.contains("private-value-before"));
    let before_chat = before.items.iter().find(|item| item.id == pending).unwrap();
    assert_eq!(before_chat.identity.len(), 64);
    source.inventory.chats[0]
        .launch
        .recipe
        .environment
        .insert("PRIVATE_TOKEN".into(), "private-value-after".into());
    let after = stop_list(&source.preview, &source.inventory, true).unwrap();
    assert_ne!(before.digest(), after.digest());
    assert_ne!(
        before_chat.identity,
        after
            .items
            .iter()
            .find(|item| item.id == pending)
            .unwrap()
            .identity
    );
    source.inventory.terminals[0]
        .session
        .launch
        .arguments
        .push("a different pending launch".into());
    assert_ne!(
        after.digest(),
        stop_list(&source.preview, &source.inventory, true)
            .unwrap()
            .digest()
    );
}

#[test]
fn paired_chat_and_console_are_listed_as_one_pane() {
    let mut source = fixture();
    let terminal = &source.inventory.terminals[0].session;
    let chat = crate::chat::Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: terminal.launch.clone(),
            session_id: Some("recorded".into()),
        },
        proof: source
            .preview
            .daemon
            .as_ref()
            .map(|daemon| daemon.capability.proof.clone()),
        terminal: Some(terminal.clone()),
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    source.inventory.chats.push(chat);
    source.preview.daemon.as_mut().unwrap().sessions[0].kind = impact::SessionKind::NativeChat;
    let list = stop_list(&source.preview, &source.inventory, true).unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].kind, impact::SessionKind::NativeChat);
}

#[test]
fn pending_launch_is_retained_only_after_the_service_accepts_one_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let (source, pending) = pending_source();
    let records = source.records.clone();
    let worker = spawn_source(source, directory.path(), true);
    let published = await_stop(directory.path());
    let list = published
        .confirmation
        .as_ref()
        .unwrap()
        .stop
        .as_ref()
        .unwrap();
    assert_eq!(list.items.len(), 2);
    let chat = list.items.iter().find(|item| item.id == pending).unwrap();
    assert_eq!(chat.running, "Claude chat, starting (can't be resumed)");
    assert_eq!(
        chat.recovery,
        "the transcript stays visible; start a new conversation"
    );
    assert!(
        records.lock().unwrap().is_empty(),
        "capture must not record intent"
    );
    confirm_stop(directory.path(), &published);
    let (result, source) = finish(worker);
    result.unwrap();
    assert_eq!(
        source.close_attempts, 0,
        "service stop already covers every listed chat"
    );
    let ledger = source.ledger();
    assert_eq!(ledger.records.len(), 1);
    assert!(ledger.records[0].service_stopped);
    assert_eq!(ledger.records[0].chats[0].id, pending);
    assert_eq!(ledger.records[0].terminals.len(), 1);
    let events = source.events.lock().unwrap();
    let at = |name| events.iter().position(|event| event == name).unwrap();
    assert!(at("ShutdownV1 Confirm") < at("archive checkpoint"));
    assert!(at("archive checkpoint") < at("retain acknowledged"));
    assert!(at("retain acknowledged") < at("handoff without checkpoint"));
}

#[test]
fn refused_prepare_is_known_before_the_only_destructive_confirmation() {
    for approve in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let source = fixture();
        let worker = spawn_source(source, directory.path(), true);
        let published = await_stop(directory.path());
        if approve {
            confirm_stop(directory.path(), &published);
        } else {
            submit(directory.path(), &published, UpdateChoice::Cancel);
        }
        let (result, source) = finish(worker);
        assert_eq!(
            source.events.lock().unwrap()[0],
            "PrepareConfirmedUpgrade refused"
        );
        if approve {
            result.unwrap();
            assert_eq!(source.retain_attempts, 1);
            assert!(source.ledger().records[0].service_stopped);
        } else {
            assert!(matches!(result, Err(UpdateError::Cancelled)));
            assert_no_destructive_actions(&source);
        }
    }
}

#[test]
fn an_ordinary_yes_does_not_authorize_the_stop_list() {
    let directory = tempfile::tempdir().unwrap();
    let source = fixture();
    let observations = source.observations.clone();
    let worker = spawn_source(source, directory.path(), true);
    let published = await_stop(directory.path());
    let count = observations.load(Ordering::Acquire);
    let decision = submit(directory.path(), &published, UpdateChoice::Confirm);
    let deadline = Instant::now() + Duration::from_secs(10);
    while decision.exists() || observations.load(Ordering::Acquire) <= count + 1 {
        assert!(!worker.is_finished());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    submit(directory.path(), &published, UpdateChoice::Cancel);
    let (result, source) = finish(worker);
    assert!(matches!(result, Err(UpdateError::Cancelled)));
    assert_no_destructive_actions(&source);
}

#[test]
fn window_updates_publish_the_stop_preview_and_terminal_command_then_end_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let source = fixture();
    let worker = spawn_source(source, directory.path(), false);
    let (result, source) = finish(worker);
    let error = result.unwrap_err().to_string();
    assert!(error.contains("cagents daemon upgrade"), "{error}");
    assert!(error.contains("system terminal outside cagents"), "{error}");
    assert!(error.contains("Updating stops them:"), "{error}");
    assert!(error.contains("build"), "{error}");
    assert!(!error.contains("Type \"stop\""), "{error}");
    assert_no_destructive_actions(&source);
}

#[test]
fn every_shutdown_refusal_ends_without_waiting_recording_or_archiving() {
    let refusals = [
        ShutdownOutcome::InvalidCapability,
        ShutdownOutcome::AlreadyShuttingDown,
        ShutdownOutcome::DaemonRestarted,
        ShutdownOutcome::EmptyRequired { current_count: 1 },
        ShutdownOutcome::ConfirmationRequired { current_count: 1 },
    ];
    for outcome in refusals {
        let directory = tempfile::tempdir().unwrap();
        let (mut source, _) = pending_source();
        source.outcomes.push_back(outcome.clone());
        let worker = spawn_source(source, directory.path(), true);
        let published = await_stop(directory.path());
        confirm_stop(directory.path(), &published);
        let (result, source) = finish(worker);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("did not accept the stop"),
            "{outcome:?}: {error}"
        );
        assert!(source.records.lock().unwrap().is_empty());
        assert_eq!(source.retain_attempts, 0);
        assert_eq!(source.close_attempts, 0);
        assert!(
            !source
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "archive checkpoint" || event.starts_with("handoff"))
        );
        source.assert_running_panes_are_unmarked();
    }
}

#[test]
fn changed_or_expired_shutdown_consent_is_reasked_before_any_record_or_action() {
    for outcome in [
        ShutdownOutcome::InventoryChanged { current_count: 2 },
        ShutdownOutcome::Expired,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (mut source, _) = pending_source();
        let inventory_changed = matches!(outcome, ShutdownOutcome::InventoryChanged { .. });
        source.outcomes.push_back(outcome);
        let records = source.records.clone();
        let worker = spawn_source(source, directory.path(), true);
        let first = await_stop(directory.path());
        confirm_stop(directory.path(), &first);
        let nonce = &first.confirmation.as_ref().unwrap().nonce;
        let renewed = await_prompt(directory.path(), |confirmation| {
            confirmation.stop.is_some() && &confirmation.nonce != nonce
        });
        let before_digest = first
            .confirmation
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .digest();
        let after_digest = renewed
            .confirmation
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .digest();
        assert_eq!(before_digest != after_digest, inventory_changed);
        assert!(
            records.lock().unwrap().is_empty(),
            "refusal cannot record the still-running owners"
        );
        submit(directory.path(), &renewed, UpdateChoice::Cancel);
        let (result, source) = finish(worker);
        assert!(matches!(result, Err(UpdateError::Cancelled)));
        assert_eq!(source.close_attempts, 0);
        assert_eq!(source.retain_attempts, 0);
        assert!(
            !source
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "archive checkpoint" || event.starts_with("handoff"))
        );
        source.assert_running_panes_are_unmarked();
    }
}

#[test]
fn renewed_shutdown_confirmation_can_finish_after_an_expired_capability() {
    let directory = tempfile::tempdir().unwrap();
    let (mut source, _) = pending_source();
    source.outcomes.push_back(ShutdownOutcome::Expired);
    let worker = spawn_source(source, directory.path(), true);
    let first = await_stop(directory.path());
    confirm_stop(directory.path(), &first);
    let nonce = &first.confirmation.as_ref().unwrap().nonce;
    let renewed = await_prompt(directory.path(), |confirmation| {
        confirmation.stop.is_some() && &confirmation.nonce != nonce
    });
    confirm_stop(directory.path(), &renewed);
    let (result, source) = finish(worker);
    result.unwrap();
    assert_eq!(source.retain_attempts, 1);
    assert_eq!(
        source
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "ShutdownV1 Confirm")
            .count(),
        2
    );
}

#[test]
fn cancel_at_ordinary_review_wait_policy_review_or_stop_ends_nothing() {
    for prompt in ["ordinary", "wait", "policy", "stop"] {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture();
        add_chat(&mut source, impact::Disposition::Blocked, false);
        add_chat(&mut source, impact::Disposition::WaitsForIdle, true);
        let settled = source.settled.clone();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let mut current_job = job(&path);
            let result = if prompt == "ordinary" {
                confirmation::wait(&path, &mut current_job, |policy| {
                    source.observe_policy(policy)
                })
                .map(|_| ())
            } else {
                let preview = source.preview.clone();
                quiesce_source(&path, &mut current_job, &preview, true, &mut source)
            };
            (result, source)
        });
        let mut published =
            await_prompt(directory.path(), |confirmation| confirmation.stop.is_none());
        if prompt == "policy" {
            let previous = published.confirmation.as_ref().unwrap().nonce.clone();
            submit(
                directory.path(),
                &published,
                UpdateChoice::Policy(impact::Policy::Interrupt),
            );
            published = await_prompt(directory.path(), |confirmation| {
                confirmation.stop.is_none() && confirmation.nonce != previous
            });
        } else if prompt == "stop" {
            settled.store(true, Ordering::Release);
            published = await_stop(directory.path());
        }
        submit(directory.path(), &published, UpdateChoice::Cancel);
        let (result, source) = finish(worker);
        assert!(
            matches!(result, Err(UpdateError::Cancelled)),
            "{prompt}: {result:?}"
        );
        assert_no_destructive_actions(&source);
    }
}

#[test]
fn floor_closes_happen_only_after_work_finishes_and_the_single_stop_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.prepare_refusals = 0;
    let blocked = add_chat(&mut source, impact::Disposition::Blocked, false);
    add_chat(&mut source, impact::Disposition::WaitsForIdle, true);
    let settled = source.settled.clone();
    let events = source.events.clone();
    let worker = spawn_source(source, directory.path(), true);
    await_prompt(directory.path(), |confirmation| confirmation.stop.is_none());
    assert!(
        events.lock().unwrap().is_empty(),
        "working agents must be awaited before the stop review"
    );
    settled.store(true, Ordering::Release);
    let published = await_stop(directory.path());
    assert!(
        events.lock().unwrap().is_empty(),
        "stop review is not an action"
    );
    let list = published
        .confirmation
        .as_ref()
        .unwrap()
        .stop
        .as_ref()
        .unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].id, blocked);
    confirm_stop(directory.path(), &published);
    let (result, source) = finish(worker);
    result.unwrap();
    assert_eq!(source.close_attempts, 1);
    assert_eq!(source.retain_attempts, 1);
    assert!(!source.ledger().records[0].service_stopped);
    source.assert_running_panes_are_unmarked();
    let events = events.lock().unwrap();
    let close = events
        .iter()
        .position(|event| event == "ChatClose acknowledged")
        .unwrap();
    let retain = events
        .iter()
        .position(|event| event == "retain acknowledged")
        .unwrap();
    assert!(close < retain);
    assert!(!events.iter().any(|event| event == "ShutdownV1 Confirm"));
}

#[test]
fn close_or_retain_failure_mid_loop_never_marks_the_chats_that_are_still_running() {
    for failure in [
        "first-close",
        "second-close",
        "first-retain",
        "second-retain",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture();
        source.prepare_refusals = 0;
        let first = add_chat(&mut source, impact::Disposition::Blocked, false);
        let second = add_chat(&mut source, impact::Disposition::Blocked, true);
        match failure {
            "first-close" => source.fail_close_at = Some(1),
            "second-close" => source.fail_close_at = Some(2),
            "first-retain" => source.fail_retain_at = Some(1),
            "second-retain" => source.fail_retain_at = Some(2),
            _ => unreachable!(),
        }
        let worker = spawn_source(source, directory.path(), true);
        let published = await_stop(directory.path());
        confirm_stop(directory.path(), &published);
        let (result, source) = finish(worker);
        let error = result.unwrap_err().to_string();
        source.assert_running_panes_are_unmarked();
        let ledger = source.ledger();
        let recorded: Vec<_> = ledger
            .records
            .iter()
            .flat_map(|record| record.chats.iter().map(|chat| chat.id))
            .collect();
        let expected = match failure {
            "first-close" | "first-retain" => vec![],
            _ => vec![first],
        };
        assert_eq!(recorded, expected, "{failure}");
        assert!(
            !recorded.contains(&second),
            "never retain the failed or unattempted close"
        );
        assert!(ledger.records.iter().all(|record| !record.service_stopped));
        if failure.ends_with("retain") {
            assert!(
                error.contains("could not confirm that the captured transcript was saved"),
                "{failure}: {error}"
            );
            if failure == "first-retain" {
                assert!(!error.contains("transcript stays visible"), "{error}");
            }
        }
        if failure != "first-close" {
            assert!(error.contains("Ended or stopped:"), "{failure}: {error}");
            assert!(
                error.contains("start a new conversation"),
                "{failure}: {error}"
            );
        }
        assert!(
            !source
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("handoff") || event == "ShutdownV1 Confirm")
        );
    }
}

#[test]
fn late_prepare_failure_reports_ended_chats_without_a_second_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    let ended = add_chat(&mut source, impact::Disposition::Blocked, false);
    let worker = spawn_source(source, directory.path(), true);
    let published = await_stop(directory.path());
    confirm_stop(directory.path(), &published);
    let (result, source) = finish(worker);
    let error = result.unwrap_err().to_string();
    assert!(error.contains("Ended or stopped:"), "{error}");
    assert!(error.contains("start a new conversation"), "{error}");
    assert_eq!(source.ledger().records[0].chats[0].id, ended);
    assert!(
        !source
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "ShutdownV1 Confirm")
    );
    source.assert_running_panes_are_unmarked();
}

#[test]
fn accepted_shutdown_archives_checkpoint_even_if_retaining_or_waiting_fails() {
    for failure in ["retain", "wait", "archive-and-retain"] {
        let directory = tempfile::tempdir().unwrap();
        let (mut source, _) = pending_source();
        if failure.contains("retain") {
            source.fail_retain_at = Some(1);
            source.fail_archive = failure == "archive-and-retain";
        } else {
            source.fail_stopped = true;
        }
        let worker = spawn_source(source, directory.path(), true);
        let published = await_stop(directory.path());
        confirm_stop(directory.path(), &published);
        let (result, source) = finish(worker);
        let error = result.unwrap_err().to_string();
        if failure.contains("retain") {
            assert!(
                error.contains("could not confirm that the captured transcript was saved"),
                "{error}"
            );
            assert!(
                error.contains("could not confirm that the terminal's update recovery was saved"),
                "{error}"
            );
            assert!(!error.contains("transcript stays visible"), "{error}");
            assert!(!error.contains("screen and directory kept"), "{error}");
        }
        assert!(
            error.contains("The terminal service accepted the stop"),
            "{error}"
        );
        if failure == "wait" {
            assert!(error.contains("press Restart"), "{error}");
        } else {
            assert!(error.contains("restart the terminal"), "{error}");
        }
        if failure == "archive-and-retain" {
            assert!(error.contains("fake archive failed"), "{error}");
            assert!(error.contains("fake retention failed"), "{error}");
            assert_eq!(
                source.retain_attempts, 1,
                "retention is attempted even when archive fails"
            );
        }
        assert!(error.contains("start a new conversation"), "{error}");
        let events = source.events.lock().unwrap();
        let archive = events
            .iter()
            .position(|event| event == "archive checkpoint")
            .unwrap();
        let confirm = events
            .iter()
            .position(|event| event == "ShutdownV1 Confirm")
            .unwrap();
        assert!(confirm < archive);
        if failure == "wait" {
            assert!(source.ledger().records[0].service_stopped);
        }
    }
}

#[test]
fn changed_stop_list_after_consent_requires_a_new_list_before_any_action() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.change_after_capture = true;
    let records = source.records.clone();
    let worker = spawn_source(source, directory.path(), true);
    let first = await_stop(directory.path());
    confirm_stop(directory.path(), &first);
    let before = first.confirmation.as_ref().unwrap();
    let next = await_prompt(directory.path(), |confirmation| {
        confirmation.stop.is_some() && confirmation.nonce != before.nonce
    });
    assert_ne!(
        before.stop.as_ref().unwrap().digest(),
        next.confirmation
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .digest()
    );
    assert!(records.lock().unwrap().is_empty());
    submit(directory.path(), &next, UpdateChoice::Cancel);
    let (result, source) = finish(worker);
    assert!(matches!(result, Err(UpdateError::Cancelled)));
    assert_no_destructive_actions(&source);
}

#[test]
fn stop_review_keeps_consent_when_only_chat_activity_changes() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    let chat = add_chat(&mut source, impact::Disposition::Blocked, true);
    let changed = Arc::new(AtomicBool::new(false));
    let change = changed.clone();
    let path = directory.path().to_owned();
    let worker = thread::spawn(move || {
        let mut current_job = job(&path);
        confirmation::wait_stop(&path, &mut current_job, true, || {
            if change.load(Ordering::Acquire) {
                source
                    .preview
                    .daemon
                    .as_mut()
                    .unwrap()
                    .sessions
                    .iter_mut()
                    .find(|item| item.id == chat)
                    .unwrap()
                    .disposition = impact::Disposition::Resumes;
                source.inventory.chat_states.insert(chat, "idle".into());
            } else {
                source.inventory.chat_states.insert(chat, "working".into());
            }
            let list = stop_list(&source.preview, &source.inventory, true)?;
            Ok((source.preview.clone(), list))
        })
    });
    let first = await_stop(directory.path());
    let original = first.confirmation.as_ref().unwrap();
    changed.store(true, Ordering::Release);
    let refreshed = await_prompt(directory.path(), |confirmation| {
        confirmation
            .preview
            .daemon
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .any(|item| item.id == chat && item.disposition == impact::Disposition::Resumes)
    });
    let latest = refreshed.confirmation.as_ref().unwrap();
    assert_eq!(
        original.nonce, latest.nonce,
        "activity must not demand a second typed stop"
    );
    assert_eq!(
        original.stop.as_ref().unwrap().digest(),
        latest.stop.as_ref().unwrap().digest()
    );
    assert_ne!(
        original.stop.as_ref().unwrap().items,
        latest.stop.as_ref().unwrap().items,
        "working/idle remains visible display metadata"
    );
    confirm_stop(directory.path(), &first);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    worker.join().unwrap().unwrap();
}

#[test]
fn recovery_report_keeps_completed_floor_actions_for_failures_after_quiescence() {
    let (source, _) = pending_source();
    let stopped = stop_list(&source.preview, &source.inventory, true).unwrap();
    let mut survivors = Survivors::default();
    assert!(survivors.recovery_report().is_empty());
    for item in &stopped.items {
        survivors.ended.insert((item.kind, item.id), item.clone());
    }
    survivors.service_stopped = true;
    let report = survivors.recovery_report();
    assert!(
        report.contains("The terminal service accepted the stop"),
        "{report}"
    );
    for item in &stopped.items {
        assert!(report.contains(&item.label), "{report}");
        assert!(report.contains(&item.recovery), "{report}");
    }
    assert!(report.contains("press Restart in the pane"), "{report}");
    assert!(
        report.contains("the transcript stays visible; start a new conversation"),
        "{report}"
    );
    // Later survivor census and destination work must not erase completed-action
    // evidence before the coordinator formats a failure from a later phase.
    survivors.remember_list(&StopList {
        background_tasks: 0,
        background_identity: String::new(),
        from_version: "new".into(),
        items: Vec::new(),
    });
    assert_eq!(survivors.recovery_report(), report);
}

#[test]
fn ordinary_prepare_preserves_legacy_sources_without_floor_inventory_operations() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.prepare_refusals = 0;
    source.fail_inventory = true;
    let preview = source.preview.clone();
    quiesce_source(
        directory.path(),
        &mut job(directory.path()),
        &preview,
        true,
        &mut source,
    )
    .unwrap();
    assert_eq!(source.preparation_checks, 1);
    assert_eq!(
        *source.events.lock().unwrap(),
        [
            "PrepareConfirmedUpgrade accepted",
            "handoff with checkpoint"
        ]
    );
    assert_no_destructive_actions(&source);
}

#[test]
fn legacy_preparation_checks_absence_without_reading_unqualified_inventory() {
    use std::cell::Cell;
    for absent in [true, false] {
        let checked = Cell::new(false);
        let observed = preparation_census(
            true,
            || {
                checked.set(true);
                if absent {
                    Ok(())
                } else {
                    Err("Foreign Prepared was kept".into())
                }
            },
            || panic!("legacy preparation must not request current inventory DTOs"),
        );
        assert!(checked.get());
        if absent {
            assert!(observed.unwrap().is_none());
        } else {
            assert!(
                observed
                    .unwrap_err()
                    .to_string()
                    .contains("Foreign Prepared was kept")
            );
        }
    }
}

#[test]
fn preexisting_legacy_checkpoint_refuses_before_prepare_or_destructive_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let mut source = fixture();
    source.prepare_refusals = 0;
    source.fail_inventory = true;
    source.foreign_preparation = true;
    let preview = source.preview.clone();
    let mut current = job(path);
    let error = quiesce_source(path, &mut current, &preview, true, &mut source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("checkpoint already exists"), "{error}");
    assert_eq!(source.preparation_checks, 1);
    assert!(source.events.lock().unwrap().is_empty());
    assert_no_destructive_actions(&source);
    assert!(
        confirmation::cancel_at(path, &current.id).unwrap(),
        "a preflight refusal must not close cancellation admission"
    );
}

#[test]
fn changed_attachment_before_the_first_close_requires_fresh_typed_consent() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.prepare_refusals = 0;
    source.change_on_first_close = true;
    let chat = add_chat(&mut source, impact::Disposition::Blocked, false);
    let records = source.records.clone();
    let events = source.events.clone();
    let observations = source.observations.clone();
    let worker = spawn_source(source, directory.path(), true);
    let first = await_stop(directory.path());
    let original = first.confirmation.as_ref().unwrap();
    let old_digest = original.stop.as_ref().unwrap().digest();
    confirm_stop(directory.path(), &first);
    let renewed = await_prompt(directory.path(), |confirmation| {
        confirmation.stop.is_some() && confirmation.nonce != original.nonce
    });
    let new_digest = renewed
        .confirmation
        .as_ref()
        .unwrap()
        .stop
        .as_ref()
        .unwrap()
        .digest();
    assert_ne!(old_digest, new_digest);
    assert!(records.lock().unwrap().is_empty());
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "ChatClose acknowledged")
    );
    // Even a decision addressed to the new prompt cannot reuse the old owners'
    // digest. Wait for its consumption before typing the current confirmation.
    let observed = observations.load(Ordering::Acquire);
    let stale = submit(
        directory.path(),
        &renewed,
        UpdateChoice::StopListed {
            items_digest: old_digest,
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while stale.exists() || observations.load(Ordering::Acquire) <= observed + 1 {
        assert!(!worker.is_finished());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(records.lock().unwrap().is_empty());
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "ChatClose acknowledged")
    );
    confirm_stop(directory.path(), &renewed);
    let (result, source) = finish(worker);
    result.unwrap();
    assert_eq!(source.retain_attempts, 1);
    let ledger = source.ledger();
    assert_eq!(ledger.records[0].chats[0].id, chat);
    assert_eq!(
        ledger.records[0].chats[0].launch.session_id.as_deref(),
        Some("replacement-conversation")
    );
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "ChatClose acknowledged")
            .count(),
        1
    );
}

#[test]
fn ctrl_c_at_stop_prompt_after_refused_prepare_ends_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let source = fixture();
    let worker = spawn_source(source, directory.path(), true);
    let current = await_stop(directory.path());
    assert!(confirmation::cancel_at(directory.path(), &current.id).unwrap());
    let (result, source) = finish(worker);
    assert!(matches!(result, Err(UpdateError::Cancelled)));
    assert_no_destructive_actions(&source);
    assert!(
        source
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "PrepareConfirmedUpgrade refused")
    );
}

#[test]
fn ctrl_c_during_floor_capture_stops_before_first_destructive_request() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    add_chat(&mut source, impact::Disposition::Blocked, false);
    source.cancel_on_capture = Some(directory.path().to_owned());
    let worker = spawn_source(source, directory.path(), true);
    let current = await_stop(directory.path());
    confirm_stop(directory.path(), &current);
    let (result, source) = finish(worker);
    assert!(matches!(result, Err(UpdateError::Cancelled)), "{result:?}");
    assert_no_destructive_actions(&source);
    assert!(
        source
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "cancelled during capture")
    );
}

#[test]
fn ctrl_c_after_first_chat_end_finishes_without_another_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.prepare_refusals = 0;
    add_chat(&mut source, impact::Disposition::Blocked, false);
    add_chat(&mut source, impact::Disposition::Blocked, true);
    source.cancel_after_close = Some(directory.path().to_owned());
    let worker = spawn_source(source, directory.path(), true);
    let current = await_stop(directory.path());
    confirm_stop(directory.path(), &current);
    let (result, source) = finish(worker);
    result.unwrap();
    assert_eq!(source.close_attempts, 2);
    assert_eq!(source.retain_attempts, 2);
    assert_eq!(
        source
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "cancellation refused after close")
            .count(),
        2
    );
}

#[test]
fn ctrl_c_after_shutdown_expiry_cancels_the_renewed_stop_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.outcomes.push_back(ShutdownOutcome::Expired);
    let worker = spawn_source(source, directory.path(), true);
    let first = await_stop(directory.path());
    confirm_stop(directory.path(), &first);
    let first_nonce = &first.confirmation.as_ref().unwrap().nonce;
    let renewed = await_prompt(directory.path(), |confirmation| {
        confirmation.stop.is_some() && &confirmation.nonce != first_nonce
    });
    assert!(confirmation::cancel_at(directory.path(), &renewed.id).unwrap());
    let (result, source) = finish(worker);
    assert!(matches!(result, Err(UpdateError::Cancelled)));
    assert_eq!(source.close_attempts, 0);
    assert_eq!(source.retain_attempts, 0);
    assert!(
        !source
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "archive checkpoint")
    );
    source.assert_running_panes_are_unmarked();
}

/// Exercise the actual terminal driver against the real worker confirmation
/// files. Only source operations are fake; approvals, idle waiting, typed
/// consent, and preparation boundaries are the production implementations.
#[test]
fn terminal_update_prompts_once_through_idle_wait_and_typed_stop() {
    struct Terminal {
        state: PathBuf,
        output: Vec<u8>,
        answers: VecDeque<&'static str>,
        settled: Arc<AtomicBool>,
        deadline: Instant,
    }
    impl Write for Terminal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl TerminalReview for Terminal {
        fn status(&mut self) -> Result<Option<UpdateJob>, UpdateError> {
            Ok(Some(serde_json::from_slice(&fs::read(
                self.state.join("status.json"),
            )?)?))
        }
        fn decide(
            &mut self,
            job: &str,
            nonce: &str,
            choice: UpdateChoice,
        ) -> Result<(), UpdateError> {
            confirmation::decide_at(&self.state, job, nonce, choice)
        }
        fn cancel(&mut self, job: &str) -> Result<bool, UpdateError> {
            confirmation::cancel_at(&self.state, job)
        }
        fn read_line(&mut self, answer: &mut String) -> io::Result<usize> {
            let next = self
                .answers
                .pop_front()
                .expect("the CLI asked an extra question");
            answer.push_str(next);
            Ok(next.len())
        }
        fn is_terminal(&self) -> bool {
            true
        }
        fn interrupted(&self) -> bool {
            false
        }
        fn pause(&mut self) {
            assert!(
                Instant::now() < self.deadline,
                "the full terminal update stalled"
            );
            if String::from_utf8_lossy(&self.output)
                .contains("Ctrl-C cancels the update; nothing has been ended or stopped.")
            {
                self.settled.store(true, Ordering::Release);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    for ordinary_answer in ["\n", "i\n"] {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture();
        source.preview.from_version = "0.1.3+local.991daa43e".into();
        source.preview.candidate.version = "0.1.3+local.updated".into();
        source.preview.windows = 2;
        source.preview.daemon.as_mut().unwrap().sessions[0].title =
            "build · Release Details: rollout · alpha".into();
        source.preview.daemon.as_mut().unwrap().sessions[0].disposition =
            impact::Disposition::Blocked;
        let working = add_chat(&mut source, impact::Disposition::WaitsForIdle, false);
        let idle = add_chat(&mut source, impact::Disposition::Resumes, true);
        for (id, title) in [
            (
                working,
                "Untitled conversation in beta, started 9 Nov 14:02",
            ),
            (idle, "review · General · alpha"),
        ] {
            source.preview.labels.names.insert(id, title.into());
        }
        let initial = job(directory.path());
        let mut worker_job = initial.clone();
        let path = directory.path().to_owned();
        let settled = source.settled.clone();
        let worker = thread::spawn(move || {
            let result = (|| {
                let reviewed = confirmation::wait(&path, &mut worker_job, |policy| {
                    source.observe_policy(policy)
                })?;
                quiesce_source(&path, &mut worker_job, &reviewed, true, &mut source)
            })();
            worker_job.phase = if result.is_ok() {
                UpdatePhase::Complete
            } else {
                UpdatePhase::Failed
            };
            worker_job.confirmation = None;
            worker_job.message = match &result {
                Ok(()) => "Update installed; saved windows reopened".into(),
                Err(error) => error.to_string(),
            };
            write_job(&path, &worker_job).unwrap();
            (result, source)
        });
        let mut terminal = Terminal {
            state: directory.path().to_owned(),
            output: Vec::new(),
            answers: VecDeque::from([ordinary_answer, "stop\n"]),
            settled,
            deadline: Instant::now() + Duration::from_secs(10),
        };
        drive_terminal_review_with(initial, false, false, &mut terminal).unwrap();
        let (result, source) = finish(worker);
        result.unwrap();
        let output = String::from_utf8(terminal.output).unwrap();
        println!("{output}");
        for prompt in [
            "Update cagents ",
            "Enter: review stops when they finish   i: interrupt them now   n: cancel",
            "Type \"stop\" to stop these and update. Anything else cancels.",
        ] {
            assert_eq!(output.matches(prompt).count(), 1, "{output}");
        }
        if ordinary_answer == "\n" {
            for message in [
                "Waiting for these conversations to finish their turns:",
                "Ctrl-C cancels the update; nothing has been ended or stopped.",
            ] {
                assert_eq!(output.matches(message).count(), 1, "{output}");
            }
        }
        assert!(terminal.answers.is_empty());
        assert!(output.contains("Stops (2)"), "{output}");
        assert!(!output.contains("Needs attention"), "{output}");
        assert!(
            source
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "ShutdownV1 Confirm")
        );
    }
}

#[test]
fn service_stop_consent_covers_exact_background_tasks_even_when_the_count_stays_the_same() {
    let mut source = fixture();
    let daemon = source.preview.daemon.as_mut().unwrap();
    daemon.capability.background_subagents = 2;
    daemon.capability.background_identity = "first exact task set".into();
    let first = stop_list(&source.preview, &source.inventory, true).unwrap();
    assert_eq!(first.background_tasks, 2);
    assert_eq!(first.background_identity, "first exact task set");
    let rendered = first.render();
    assert!(rendered.contains("2"), "{rendered}");
    assert!(rendered.contains("background"), "{rendered}");
    assert!(rendered.contains("not resume automatically"), "{rendered}");
    source
        .preview
        .daemon
        .as_mut()
        .unwrap()
        .capability
        .background_identity = "replacement exact task set".into();
    let changed = stop_list(&source.preview, &source.inventory, true).unwrap();
    assert_eq!(first.background_tasks, changed.background_tasks);
    assert_ne!(first.digest(), changed.digest());
}

#[test]
fn a_chat_only_stop_list_covers_only_its_listed_background_tasks() {
    let mut source = fixture();
    let blocked = add_chat(&mut source, impact::Disposition::Blocked, false);
    let unchanged = add_chat(&mut source, impact::Disposition::Resumes, true);
    for session in &mut source.preview.daemon.as_mut().unwrap().sessions {
        if session.id == blocked {
            session.interrupted_subagents = vec!["stopped background task".into()];
        } else if session.id == unchanged {
            session.interrupted_subagents = vec!["unaffected background task".into()];
        }
    }
    let first = stop_list(&source.preview, &source.inventory, false).unwrap();
    assert_eq!(first.background_tasks, 1);
    source
        .preview
        .daemon
        .as_mut()
        .unwrap()
        .sessions
        .iter_mut()
        .find(|session| session.id == blocked)
        .unwrap()
        .interrupted_subagents = vec!["replacement background task".into()];
    let changed = stop_list(&source.preview, &source.inventory, false).unwrap();
    assert_eq!(first.background_tasks, changed.background_tasks);
    assert_ne!(first.digest(), changed.digest());
}

#[test]
fn completed_service_stop_reports_background_tasks_and_their_recovery() {
    let report = completed_error(invalid("later startup failed"), &[], true, 2).to_string();
    assert!(
        report.contains("2 background tasks stopped; they do not resume automatically."),
        "{report}"
    );
    let survivors = Survivors {
        service_stopped: true,
        background_tasks: 2,
        ..Default::default()
    };
    assert!(
        survivors
            .recovery_report()
            .contains("2 background tasks stopped; they do not resume automatically.")
    );
}

#[test]
fn accepted_prepare_reports_only_interrupted_turns_with_their_reviewed_recovery() {
    let mut source = fixture();
    let unchanged = add_chat(&mut source, impact::Disposition::Resumes, true);
    let resumes = add_chat(&mut source, impact::Disposition::Interrupted, true);
    let paused = add_chat(&mut source, impact::Disposition::Interrupted, true);
    for (id, label) in [
        (unchanged, "Unchanged conversation"),
        (resumes, "Reviewed automatic resume"),
        (paused, "Reviewed paused conversation"),
    ] {
        source.preview.labels.names.insert(id, label.into());
    }
    for session in &mut source.preview.daemon.as_mut().unwrap().sessions {
        if session.id == resumes {
            session.interrupted_subagents = vec!["task-a".into(), "task-b".into(), "task-b".into()];
        } else if session.id == paused {
            session.interrupted_subagents = vec!["task-c".into()];
        }
    }
    source
        .preview
        .stopped
        .push(super::super::source_inspection::StoppedPane {
            pane: paused,
            kind: super::super::source_inspection::StoppedKind::Chat,
            label: "stale saved label".into(),
            reason: "its conversation could not be confirmed".into(),
            recovery: "Choose Resume conversation in the new tab (+) menu".into(),
        });
    let mut survivors = Survivors::default();
    assert!(survivors.prepared_report().is_empty());
    assert!(!survivors.needs_daemon_recovery());
    let daemon = source.preview.daemon.as_ref().unwrap();
    survivors.record_prepared(&source.preview, daemon);
    let report = survivors.prepared_report();
    assert!(
        report.contains(
            "Reviewed automatic resume: current turn interrupted; its saved conversation was kept"
        ),
        "{report}"
    );
    assert!(report.contains("Reviewed paused conversation: current turn interrupted; after the update: its conversation could not be confirmed. Choose Resume conversation in the new tab (+) menu"), "{report}");
    assert!(
        report.contains("3 background tasks stopped; they do not resume automatically."),
        "{report}"
    );
    assert!(!report.contains("Unchanged conversation"), "{report}");
    assert!(!report.contains("build"), "{report}");
    assert!(!report.contains("Ended or stopped"), "{report}");
    assert!(!report.contains("terminal service accepted"), "{report}");
    assert!(survivors.needs_daemon_recovery());
    assert!(!survivors.service_stopped);
    assert!(survivors.recovery_report().is_empty());
    survivors.record_prepared(&source.preview, daemon);
    assert_eq!(
        survivors.prepared_report(),
        report,
        "a repeated acknowledgment must not duplicate ended turns or background tasks"
    );

    let stopped = stop_list(&source.preview, &source.inventory, true).unwrap();
    let already_ended = stopped
        .items
        .iter()
        .find(|item| item.id == resumes)
        .unwrap();
    let mut mixed = Survivors::default();
    mixed
        .ended
        .insert((already_ended.kind, resumes), already_ended.clone());
    mixed.record_prepared(&source.preview, daemon);
    let report = mixed.prepared_report();
    assert!(!report.contains("Reviewed automatic resume"), "{report}");
    assert!(report.contains("Reviewed paused conversation"), "{report}");
    assert!(
        report.contains("1 background task stopped; it does not resume automatically."),
        "{report}"
    );
}
