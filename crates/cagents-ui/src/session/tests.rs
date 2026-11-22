use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::atomic::{AtomicUsize, Ordering},
};

use rusqlite::{Connection, OptionalExtension};

use tempfile::tempdir;

use crate::model::{
    CardId, CardKind, CardState, Existence, LaunchRecipe, TerminalContinuity, TerminalId,
    TerminalSession, Tombstone, WorkspaceId,
};

use super::*;

#[derive(Debug, Clone, Copy)]
enum FakeWrite {
    Succeed,
    Fail,
    /// A failure no rebase can address.
    FailDiskFull,
    /// A base generation storage rejected.
    FailStale,
    /// A write storage refuses by construction.
    Refuse,
    /// Storage committed the workspace but refused one pane's content.
    PartlyRefused,
    Panic,
}

struct FakeStore {
    writes: Mutex<VecDeque<FakeWrite>>,
    /// What every write past the scripted ones does, so a test can describe
    /// a store that never accepts a write without scripting each attempt.
    exhausted: FakeWrite,
    /// Counts the generation re-reads the writer lane performed, which is
    /// what tells a rebase apart from an ordinary retry.
    rebases: Arc<AtomicUsize>,
    /// Counts the clean shutdowns the lane recorded.
    shutdowns: Arc<AtomicUsize>,
    /// Counts every write the lane offered, whatever its outcome.
    attempts: Arc<AtomicUsize>,
    shutdown_failure: bool,
}

impl FakeStore {
    fn new(writes: impl IntoIterator<Item = FakeWrite>) -> Self {
        Self {
            writes: Mutex::new(writes.into_iter().collect()),
            exhausted: FakeWrite::Succeed,
            rebases: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            attempts: Arc::new(AtomicUsize::new(0)),
            shutdown_failure: false,
        }
    }

    fn always(write: FakeWrite) -> Self {
        Self {
            writes: Mutex::new(VecDeque::new()),
            exhausted: write,
            rebases: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            attempts: Arc::new(AtomicUsize::new(0)),
            shutdown_failure: false,
        }
    }

    fn next_write(&self) -> FakeWrite {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.writes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(self.exhausted)
    }

    fn write_result(write: FakeWrite) -> Result<(), StoreError> {
        match write {
            FakeWrite::Succeed => Ok(()),
            FakeWrite::Fail => Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "TOKEN=secret terminal output must never reach health",
            ))),
            FakeWrite::FailDiskFull => Err(StoreError::Io(std::io::Error::from_raw_os_error(
                libc::ENOSPC,
            ))),
            FakeWrite::FailStale => Err(StoreError::StaleGeneration {
                entity: "workspace".to_owned(),
                expected: 1,
                actual: 2,
            }),
            FakeWrite::Refuse => Err(invalid_state!("injected refusal")),
            FakeWrite::PartlyRefused => unreachable!("partial refusal is a workspace commit"),
            FakeWrite::Panic => panic!("injected writer panic"),
        }
    }

    fn write(&self) -> Result<(), StoreError> {
        Self::write_result(self.next_write())
    }
}

impl SessionStore for FakeStore {
    fn preserve_chat_copy(
        &self,
        copy: &crate::persistence::RetainedChatCopy,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        self.write()?;
        Ok(crate::persistence::WorkspaceCommit {
            generation: 1,
            card_generations: BTreeMap::from([(copy.source_card, 1), (copy.copy.id, 1)]),
            refused: Vec::new(),
        })
    }

    fn rebind_chat_upgrade(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        self.write()
    }

    fn rebind_chat_account(
        &self,
        _card_id: CardId,
        _source: &crate::chat::Session,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        self.write()
    }

    fn replace_terminal_identity(
        &self,
        _card_id: CardId,
        _expected_terminal_id: TerminalId,
        _replacement: &TerminalSession,
    ) -> Result<(), StoreError> {
        self.write()
    }

    fn replace_chat_identity(
        &self,
        _card_id: CardId,
        _expected_id: TerminalId,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        self.write()
    }

    fn save_workspace(
        &self,
        _state: &WorkspaceState,
        _base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        match self.next_write() {
            FakeWrite::PartlyRefused => Ok(crate::persistence::WorkspaceCommit {
                refused: vec![StoreError::Card {
                    card: CardId::new(),
                    error: Box::new(invalid_state!("injected partial refusal")),
                }],
                ..Default::default()
            }),
            write => {
                Self::write_result(write)?;
                Ok(crate::persistence::WorkspaceCommit::default())
            }
        }
    }

    fn save_project(&self, _project: &ProjectState) -> Result<u64, StoreError> {
        self.write().map(|()| 1)
    }

    fn save_worktree(&self, _worktree: &WorktreeState) -> Result<u64, StoreError> {
        self.write().map(|()| 1)
    }

    fn stored_generations(&self, _workspace: WorkspaceId) -> Result<StoredGenerations, StoreError> {
        self.rebases.fetch_add(1, Ordering::Relaxed);
        Ok(StoredGenerations::default())
    }

    fn stored_project_generation(&self, _project: ProjectId) -> Result<Option<u64>, StoreError> {
        Ok(None)
    }

    fn stored_worktree_generation(&self, _worktree: WorktreeId) -> Result<Option<u64>, StoreError> {
        Ok(None)
    }

    fn shutdown(&self) -> Result<(), StoreError> {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
        if self.shutdown_failure {
            Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "TOKEN=secret clean-marker error",
            )))
        } else {
            Ok(())
        }
    }
}

fn fast_persistence_config(max_automatic_attempts: u32) -> PersistenceConfig {
    PersistenceConfig {
        debounce: Duration::ZERO,
        max_delay: PERSIST_MAX_DELAY,
        retry_initial: Duration::from_millis(1),
        retry_max: Duration::from_millis(1),
        max_automatic_attempts,
        shutdown_flush_budget: Duration::from_millis(20),
    }
}

fn next_health(
    receiver: &async_channel::Receiver<SessionSaveHealthEvent>,
) -> SessionSaveHealthEvent {
    receiver
        .recv_blocking()
        .expect("health publisher must remain connected")
}

fn workspace() -> WorkspaceState {
    let launch = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-l".into()],
        working_directory: PathBuf::from("/tmp"),
        environment: BTreeMap::new(),
    };
    WorkspaceState {
        schema_version: 1,
        id: WorkspaceId::new(),
        generation: 0,
        existence: Existence::Open,
        focused: None,
        active_worktree: None,
        worktree_focus: HashMap::new(),
        project_order: Vec::new(),
        channels: Vec::new(),
        active_channel: None,
        card_scopes: HashMap::new(),
        chat_drafts: HashMap::new(),
        chat_draft_attachments: HashMap::new(),
        chat_draft_models: HashMap::new(),
        chat_waiting_messages: HashMap::new(),
        kept_chat_drafts: HashMap::new(),
        agent_updates: BTreeMap::new(),
        workspace_history: Vec::new(),
        repository_navigation: HashMap::new(),
        workspace_model_identity: None,
        local_only_cards: HashSet::new(),
        pending_chat_admissions: Default::default(),
        pending_terminal_creations: HashSet::new(),
        cards: vec![CardState {
            id: CardId::new(),
            generation: 0,
            title: "shell".into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            kind: CardKind::Terminal(TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: TerminalId::new(),
                launch: launch.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch,
                    last_exit: None,
                },
            }),
        }]
        .into(),
    }
}

#[test]
fn new_changes_trail_debounce_without_postponing_an_active_retry() {
    let now = Instant::now();
    let config = PersistenceConfig::default();
    let mut oldest_change = Some(now - Duration::from_millis(100));
    let old_debounce = now + Duration::from_millis(10);
    let new_debounce = now + Duration::from_millis(250);
    assert_eq!(
        schedule_flush_after_changes(
            Some(old_debounce),
            true,
            false,
            0,
            now,
            config,
            &mut oldest_change,
        ),
        Some(new_debounce)
    );

    let retry_due = now + Duration::from_millis(100);
    assert_eq!(
        schedule_flush_after_changes(
            Some(retry_due),
            true,
            false,
            1,
            now,
            config,
            &mut oldest_change,
        ),
        Some(retry_due)
    );
    assert_eq!(oldest_change, Some(now - Duration::from_millis(100)));
    assert_eq!(
        schedule_flush_after_changes(None, true, true, 4, now, config, &mut oldest_change,),
        None,
        "new changes must not restart an exhausted retry cycle"
    );
}

#[test]
fn replica_capture_original_times_do_not_wait_out_a_second_debounce() {
    let first = Instant::now();
    let changes = SnapshotChanges {
        first,
        latest: first + Duration::from_millis(100),
    };
    assert_eq!(changes.deadline(), first + Duration::from_millis(350));
    let mut oldest = None;
    let due = schedule_flush_after_interval(
        None,
        Some(changes),
        false,
        0,
        PersistenceConfig::default(),
        &mut oldest,
    )
    .unwrap();
    assert_eq!(due, changes.deadline());
    assert!(
        due <= first + Duration::from_millis(400),
        "late preparation is already due"
    );
    let continuous = changes.merge(SnapshotChanges::at(first + Duration::from_millis(950)));
    assert_eq!(continuous.deadline(), first + Duration::from_secs(1));
}

#[test]
fn replica_capture_late_times_merge_with_other_activity_and_preserve_retry_policy() {
    let start = Instant::now();
    let config = PersistenceConfig::default();
    let mut oldest = None;
    // A project/worktree change reached the writer before an older
    // replica snapshot finished preparing. Keep the later activity too.
    let normal = start + Duration::from_millis(400);
    let due = schedule_flush_after_changes(None, true, false, 0, normal, config, &mut oldest);
    let due = schedule_flush_after_interval(
        due,
        Some(SnapshotChanges {
            first: start,
            latest: start + Duration::from_millis(100),
        }),
        false,
        0,
        config,
        &mut oldest,
    );
    assert_eq!(oldest, Some(start));
    assert_eq!(due, Some(normal + config.debounce));
    let continuous = SnapshotChanges::at(start + Duration::from_millis(1_200));
    assert_eq!(
        schedule_flush_after_interval(due, Some(continuous), false, 0, config, &mut oldest),
        Some(start + config.max_delay)
    );
    let retry = Some(start + Duration::from_secs(2));
    for (exhausted, attempts, deadline) in [(false, 1, retry), (true, 4, None)] {
        let before = oldest;
        assert_eq!(
            schedule_flush_after_interval(
                deadline,
                Some(continuous),
                exhausted,
                attempts,
                config,
                &mut oldest
            ),
            deadline
        );
        assert_eq!(
            oldest, before,
            "snapshot offers cannot rearm a failed batch"
        );
    }
}

#[test]
fn replica_capture_old_normal_queue_activity_does_not_become_new_drain_activity() {
    let start = Instant::now();
    let mut queue = QueueState::default();
    queue.record_snapshot_changes(Some(SnapshotChanges::at(
        start + Duration::from_millis(400),
    )));
    queue.record_snapshot_changes(Some(SnapshotChanges {
        first: start + Duration::from_millis(700),
        latest: start + Duration::from_millis(800),
    }));
    let drained_at = start + Duration::from_millis(1_070);
    let mut oldest = None;
    let due = schedule_flush_after_interval(
        None,
        queue.snapshot_changes.take(),
        false,
        0,
        PersistenceConfig::default(),
        &mut oldest,
    )
    .unwrap();
    assert_eq!(due, start + Duration::from_millis(1_050));
    assert!(
        due < drained_at,
        "old queue metadata cannot create a second debounce"
    );
}

#[test]
fn replica_capture_empty_worktree_offer_cannot_postpone_a_prepared_snapshot() {
    let directory = tempdir().unwrap();
    let state = Arc::new(workspace());
    let runtime = queue_only_runtime(&directory, &state, false);
    runtime.queue.0.lock().unwrap().latest = None;
    runtime.save_worktrees(std::iter::empty());
    assert_eq!(runtime.queue.0.lock().unwrap().snapshot_changes, None);
    let old = SnapshotChanges::at(Instant::now() - Duration::from_secs(2));
    runtime
        .offer_snapshot(SnapshotOffer {
            state,
            changes: Some(old),
        })
        .unwrap();
    runtime.save_worktrees(std::iter::empty());
    assert_eq!(runtime.queue.0.lock().unwrap().snapshot_changes, Some(old));
    let mut first = None;
    assert_eq!(
        schedule_flush_after_interval(
            None,
            Some(old),
            false,
            0,
            PersistenceConfig::default(),
            &mut first
        ),
        Some(old.deadline())
    );
    let before = Instant::now();
    runtime.save_worktrees([worktree(&project("queued"), "feature")]);
    let recorded = runtime.queue.0.lock().unwrap().snapshot_changes.unwrap();
    assert_eq!(recorded.first, old.first);
    assert!(recorded.latest >= before);
}

#[test]
fn replica_capture_offers_acknowledge_only_an_accepted_queue() {
    let directory = tempdir().unwrap();
    let state = Arc::new(workspace());
    let runtime = queue_only_runtime(&directory, &state, false);
    let changes = SnapshotChanges::at(Instant::now());
    runtime
        .offer_snapshot(SnapshotOffer {
            state: state.clone(),
            changes: Some(changes),
        })
        .unwrap();
    {
        let mut queue = runtime.queue.0.lock().unwrap();
        assert_eq!(queue.snapshot_changes, Some(changes));
        queue.shutdown = true;
    }
    let later = SnapshotChanges::at(changes.latest + Duration::from_secs(1));
    assert!(
        runtime
            .offer_snapshot(SnapshotOffer {
                state: state.clone(),
                changes: Some(later)
            })
            .is_err()
    );
    assert_eq!(
        runtime.queue.0.lock().unwrap().snapshot_changes,
        Some(changes)
    );
    runtime.queue.0.lock().unwrap().shutdown = false;
}

#[test]
fn replica_capture_writer_fence_distinguishes_new_queue_with_the_same_store() {
    let directory = tempdir().unwrap();
    let state = Arc::new(workspace());
    let old = queue_only_runtime(&directory, &state, false);
    let mut new = queue_only_runtime(&directory, &state, false);
    new.operation_store = old.operation_store.clone();
    assert!(Arc::ptr_eq(&old.operation_store, &new.operation_store));
    let token = old.writer_identity();
    assert!(old.owns_writer(&token));
    assert!(!new.owns_writer(&token));
}

#[test]
fn replica_capture_timed_replacement_keeps_authority_ahead_of_its_snapshot() {
    let directory = tempdir().unwrap();
    let mut state = workspace();
    let source = staged_chat(&state);
    state.cards[0].kind = CardKind::Chat(source.clone());
    let older = Arc::new(state.clone());
    let runtime = queue_only_runtime(&directory, &older, false);
    let mut replacement = source.clone();
    replacement.id = TerminalId::new();
    state.cards[0].kind = CardKind::Chat(replacement.clone());
    let changes = SnapshotChanges::at(Instant::now());
    runtime
        .offer_chat_identity(
            state.cards[0].id,
            source.id,
            replacement.clone(),
            SnapshotOffer {
                state: Arc::new(state),
                changes: Some(changes),
            },
        )
        .unwrap();
    let queue = runtime.queue.0.lock().unwrap();
    assert!(
        queue.conversions.is_empty(),
        "no extra candidate snapshot precedes replacement authority"
    );
    assert_eq!(
        queue.identity_replacements[&older.cards[0].id].expected_terminal_id,
        source.id
    );
    assert_eq!(
        queue.latest.as_ref().unwrap().cards[0].kind,
        CardKind::Chat(replacement)
    );
    assert_eq!(queue.snapshot_changes, Some(changes));
}

#[test]
fn replica_capture_writer_loop_does_not_debounce_an_already_prepared_offer_again() {
    let directory = tempdir().unwrap();
    let state = Arc::new(workspace());
    let runtime = queue_only_runtime(&directory, &state, false);
    {
        runtime.queue.0.lock().unwrap().latest = None;
    }
    let store = FakeStore::new([]);
    let attempts = store.attempts.clone();
    let queue = runtime.queue.clone();
    let (health_tx, _health) = async_channel::unbounded();
    let worker = thread::spawn(move || {
        persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::new(Mutex::new(None)),
            &mut HealthPublisher::new(health_tx),
            PersistenceConfig {
                debounce: Duration::from_secs(60),
                max_delay: Duration::from_secs(120),
                ..PersistenceConfig::default()
            },
        )
    });
    runtime
        .offer_snapshot(SnapshotOffer {
            state,
            changes: Some(SnapshotChanges::at(
                Instant::now() - Duration::from_secs(180),
            )),
        })
        .unwrap();
    // This ample bound is far shorter than a fabricated second60s wait;
    // it makes no assertion about sub-frame or250ms scheduling latency.
    let deadline = Instant::now() + Duration::from_secs(5);
    while attempts.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let wrote_without_a_barrier = attempts.load(Ordering::Relaxed) > 0;
    runtime.queue.0.lock().unwrap().shutdown = true;
    runtime.queue.1.notify_one();
    assert!(matches!(worker.join().unwrap(), WriterExit::Shutdown(_)));
    assert!(
        wrote_without_a_barrier,
        "prepared offer waited out a second debounce"
    );
}

#[test]
fn continuous_changes_cannot_postpone_the_oldest_pending_save() {
    let start = Instant::now();
    let config = PersistenceConfig::default();
    let mut oldest_change = None;
    let mut due = None;
    for elapsed_ms in [0, 200, 400, 600, 800, 1_000, 1_200] {
        let now = start + Duration::from_millis(elapsed_ms);
        due = schedule_flush_after_changes(due, true, false, 0, now, config, &mut oldest_change);
        assert_eq!(
            due,
            Some((now + config.debounce).min(start + config.max_delay))
        );
        assert_eq!(oldest_change, Some(start));
        if elapsed_ms >= 1_000 {
            assert!(due.unwrap() <= now, "an overdue arrival must still flush");
        }
    }
    let overdue = due;
    assert_eq!(
        schedule_flush_after_changes(
            due,
            false,
            false,
            0,
            start + Duration::from_secs(2),
            config,
            &mut oldest_change,
        ),
        overdue,
        "a timer wake must preserve the pending deadline"
    );
}

#[test]
fn idle_wakes_do_not_start_a_save_batch() {
    let now = Instant::now();
    let config = PersistenceConfig::default();
    let mut oldest_change = None;
    assert_eq!(
        schedule_flush_after_changes(None, false, false, 0, now, config, &mut oldest_change),
        None
    );
    assert_eq!(oldest_change, None);
    let next = now + Duration::from_secs(10);
    assert_eq!(
        schedule_flush_after_changes(None, true, false, 0, next, config, &mut oldest_change),
        Some(next + config.debounce)
    );
    assert_eq!(oldest_change, Some(next));
}

#[test]
fn continuous_snapshots_commit_before_the_producer_stops() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("continuous.sqlite3");
    let boot = BootId::new("continuous-save");
    let store = StateStore::open(&path, boot.clone()).unwrap();
    let mut state = workspace();
    let mut first = state.clone();
    first.cards[0].title = "ordered first".into();
    let mut second = state.clone();
    second.cards[0].title = "ordered second".into();
    let queue = Arc::new((
        Mutex::new(QueueState {
            conversions: VecDeque::from([
                ConversionCheckpoint::Snapshot(Arc::new(first)),
                ConversionCheckpoint::Snapshot(Arc::new(second)),
            ]),
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None));
    let worker_queue = Arc::clone(&queue);
    let worker_error = Arc::clone(&last_error);
    let worker = thread::spawn(move || {
        let (health_tx, _health_rx) = async_channel::unbounded();
        persistence_loop(
            store,
            empty_writes(),
            worker_queue,
            worker_error,
            &mut HealthPublisher::new(health_tx),
            PersistenceConfig {
                // A deadline shorter than this trailing gap proves the cap,
                // even if a loaded test host pauses the producer briefly.
                debounce: Duration::from_secs(30),
                max_delay: Duration::from_millis(50),
                ..PersistenceConfig::default()
            },
        )
    });
    let reader = Connection::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut committed_while_producing = false;
    let mut sequence = 0;
    while Instant::now() < deadline {
        state.cards[0].title = format!("streaming {sequence}");
        sequence += 1;
        {
            let mut pending = queue.0.lock().unwrap();
            pending.latest = Some(Arc::new(state.clone()));
            queue.1.notify_one();
        }
        let generation = reader
            .query_row(
                "SELECT generation FROM workspaces WHERE workspace_id = ?1",
                [state.id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .unwrap();
        // Both ordered snapshots and a coalesced autosave must have
        // committed while updates continue, without a flush barrier.
        if generation.is_some_and(|generation| generation >= 3) {
            committed_while_producing = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    state.cards[0].title = "final snapshot".into();
    {
        let mut pending = queue.0.lock().unwrap();
        pending.latest = Some(Arc::new(state.clone()));
        pending.shutdown = true;
        queue.1.notify_one();
    }
    assert!(matches!(worker.join().unwrap(), WriterExit::Shutdown(_)));
    assert!(last_error.lock().unwrap().is_none());
    assert!(
        committed_while_producing,
        "continuous changes starved saves until the producer stopped"
    );
    let reopened = StateStore::open(&path, boot).unwrap();
    let restored = reopened.load_workspace(state.id, None).unwrap().unwrap();
    assert_eq!(restored.cards[0].title, "final snapshot");
    reopened.shutdown().unwrap();
}

#[test]
fn health_category_tracks_the_current_batch_deterministically() {
    let (health_tx, health_rx) = async_channel::unbounded();
    let mut publisher = HealthPublisher::new(health_tx);
    assert!(matches!(
        next_health(&health_rx).health,
        SessionSaveHealth::Healthy
    ));

    publisher.failed(
        &[StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret details",
        ))],
        false,
    );
    let first = next_health(&health_rx);
    let SessionSaveHealth::Retrying {
        first_failed_at_unix_ms,
        ..
    } = first.health
    else {
        panic!("first failure must be retrying");
    };

    publisher.failed(
        &[
            StoreError::StaleGeneration {
                entity: "workspace id".to_owned(),
                expected: 1,
                actual: 2,
            },
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "different secret details",
            )),
        ],
        true,
    );
    assert_eq!(
        next_health(&health_rx).health,
        SessionSaveHealth::RetryExhausted {
            category: SaveFailureCategory::PermissionDenied,
            first_failed_at_unix_ms,
            attempts: 2,
        }
    );

    publisher.failed(
        &[StoreError::StaleGeneration {
            entity: "workspace id".to_owned(),
            expected: 1,
            actual: 2,
        }],
        true,
    );
    assert_eq!(
        next_health(&health_rx).health,
        SessionSaveHealth::RetryExhausted {
            category: SaveFailureCategory::Conflict,
            first_failed_at_unix_ms,
            attempts: 3,
        }
    );
}

#[test]
fn a_refused_checkpoint_names_the_cause_without_the_store_error() {
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::new(workspace())),
            checkpoints: vec![response],
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let (health_tx, _health_rx) = async_channel::unbounded();
    let worker_queue = Arc::clone(&queue);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            FakeStore::new([FakeWrite::Fail]),
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            worker_queue,
            Arc::new(Mutex::new(None)),
            HealthPublisher::new(health_tx),
            fast_persistence_config(2),
        );
    });

    let refusal = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the checkpoint is answered rather than dropped")
        .expect_err("a failed flush cannot report a saved workspace");
    assert!(
        refusal.contains(SaveFailureCategory::PermissionDenied.label())
            && refusal.contains(SaveFailureCategory::PermissionDenied.guidance()),
        "a refusal that asks the reader to resolve something must say what and how: {refusal}"
    );
    assert!(
        !refusal.contains("TOKEN=secret"),
        "the raw store error retains secrets and must not reach a dialog: {refusal}"
    );

    let (lock, changed) = &*queue;
    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
}

#[test]
fn health_contract_reports_failure_exhaustion_retry_and_recovery() {
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::new(workspace())),
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None));
    let (health_tx, health_rx) = async_channel::unbounded();
    let health = HealthPublisher::new(health_tx);
    let worker_queue = Arc::clone(&queue);
    let worker_error = Arc::clone(&last_error);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            FakeStore::new([FakeWrite::Fail, FakeWrite::Fail, FakeWrite::Succeed]),
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            worker_queue,
            worker_error,
            health,
            fast_persistence_config(2),
        );
    });

    assert!(matches!(
        next_health(&health_rx).health,
        SessionSaveHealth::Healthy
    ));
    let retrying = next_health(&health_rx);
    let SessionSaveHealth::Retrying {
        category,
        first_failed_at_unix_ms,
        attempts,
    } = retrying.health
    else {
        panic!("first failed flush must enter retrying health");
    };
    assert_eq!(category, SaveFailureCategory::PermissionDenied);
    assert_eq!(attempts, 1);
    assert_eq!(retrying.last_successful_flush_at_unix_ms, None);

    let exhausted = next_health(&health_rx);
    assert_eq!(
        exhausted.health,
        SessionSaveHealth::RetryExhausted {
            category,
            first_failed_at_unix_ms,
            attempts: 2,
        }
    );
    assert!(!format!("{exhausted:?}").contains("TOKEN=secret"));
    assert!(
        last_error
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|error| { error.contains("TOKEN=secret") })
    );

    let (lock, changed) = &*queue;
    lock.lock().unwrap().retry_requested = true;
    changed.notify_one();
    let recovered = next_health(&health_rx);
    let SessionSaveHealth::Recovered {
        recovered_at_unix_ms,
    } = recovered.health
    else {
        panic!("successful manual retry must report recovery");
    };
    assert_eq!(
        recovered.last_successful_flush_at_unix_ms,
        Some(recovered_at_unix_ms)
    );
    assert!(last_error.lock().unwrap().is_none());

    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
}

#[test]
fn injected_writer_panic_reports_unavailable() {
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::new(workspace())),
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let (health_tx, health_rx) = async_channel::unbounded();
    let health = HealthPublisher::new(health_tx);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            FakeStore::new([FakeWrite::Panic]),
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            queue,
            Arc::new(Mutex::new(None)),
            health,
            fast_persistence_config(2),
        );
    });

    assert!(matches!(
        next_health(&health_rx).health,
        SessionSaveHealth::Healthy
    ));
    assert!(matches!(
        next_health(&health_rx).health,
        SessionSaveHealth::WriterUnavailable
    ));
    worker.join().unwrap();
}

#[test]
fn chained_terminal_retries_preserve_the_persisted_expected_identity() {
    let card_id = CardId::new();
    let original_id = TerminalId::new();
    let middle_id = TerminalId::new();
    let final_id = TerminalId::new();
    let launch = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-l".into()],
        working_directory: PathBuf::from("/tmp"),
        environment: BTreeMap::new(),
    };
    let terminal = |id| TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id,
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch.clone(),
            last_exit: None,
        },
    };
    let mut replacements = HashMap::new();

    merge_identity_replacement(
        &mut replacements,
        IdentityReplacement {
            card_id,
            expected_terminal_id: original_id,
            replacement: Replacement::Terminal(terminal(middle_id)),
        },
    );
    merge_identity_replacement(
        &mut replacements,
        IdentityReplacement {
            card_id,
            expected_terminal_id: middle_id,
            replacement: Replacement::Terminal(terminal(final_id)),
        },
    );

    let replacement = &replacements[&card_id];
    assert_eq!(replacement.expected_terminal_id, original_id);
    assert_eq!(replacement.replacement.id(), final_id);
}

/// The shared view can project the pane back before the window has saved
/// anything, so the lane's very first write of it must already be made
/// against the generation storage holds for the unreadable row. Flushed
/// without the writer's rebase, which would otherwise hide a wrong start.
#[test]
fn the_first_snapshot_writes_an_unrestored_card_against_its_stored_generation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let boot = BootId::new("boot-unrestored");
    let mut latest = workspace();
    let mut lost = latest.cards[0].clone();
    lost.id = CardId::new();
    let CardKind::Terminal(session) = &mut lost.kind else {
        unreachable!()
    };
    session.id = TerminalId::new();
    let lost_terminal = session.id;
    latest.cards.push(lost.clone());
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    runtime.snapshot(Arc::new(latest.clone()));
    runtime.shutdown();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE terminal_recipes SET continuity_json = '{\"state\":\"newer\"}'
             WHERE terminal_id = ?1",
            [lost_terminal.to_string()],
        )
        .unwrap();

    let store = StateStore::open(&path, boot).unwrap();
    let RestoredSession {
        workspace,
        warnings,
        mut writes,
        ..
    } = restore_session(&store).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let mut projected = workspace.unwrap();
    assert!(!projected.cards.iter().any(|card| card.id == lost.id));
    projected.cards.push(lost.clone());
    writes.workspace = Some(Arc::new(projected));
    let errors = writes.flush(&store);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(writes.is_settled());
}

/// A card storage could not decode is left out of the restored window,
/// and the daemon's shared view projects the pane straight back. The
/// writer must save that copy over the unreadable row once, rather than
/// tombstone the card and then be refused on every later save (#3062).
#[test]
fn a_projected_copy_of_an_unrestored_card_saves_once() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let boot = BootId::new("boot-unrestored");
    let mut latest = workspace();
    let mut lost = latest.cards[0].clone();
    lost.id = CardId::new();
    let CardKind::Terminal(session) = &mut lost.kind else {
        unreachable!()
    };
    session.id = TerminalId::new();
    let lost_terminal = session.id;
    latest.cards.push(lost.clone());
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    runtime.snapshot(Arc::new(latest.clone()));
    runtime.shutdown();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE terminal_recipes SET continuity_json = '{\"state\":\"newer\"}'
             WHERE terminal_id = ?1",
            [lost_terminal.to_string()],
        )
        .unwrap();

    let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let restored = restored.unwrap();
    assert_eq!(runtime.restore_warnings().len(), 1);
    assert!(!restored.cards.iter().any(|card| card.id == lost.id));
    // The row as storage holds it. A refused write would leave it as it
    // was, so this, not the writer's last error, shows what was saved.
    let stored = || -> (String, i64) {
        rusqlite::Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT existence_json, generation FROM cards WHERE card_id = ?1",
                [lost.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    let (_, unreadable_generation) = stored();
    // A save before the projection, then the projection and later saves.
    runtime.snapshot(Arc::new(restored.clone()));
    runtime.flush(Duration::from_secs(10)).unwrap();
    assert_eq!(
        stored(),
        ("\"open\"".to_owned(), unreadable_generation),
        "a snapshot that never held the card must leave it as stored"
    );
    let mut projected = restored;
    projected.cards.push(lost.clone());
    let projected = Arc::new(projected);
    runtime.snapshot(Arc::clone(&projected));
    runtime.flush(Duration::from_secs(10)).unwrap();
    let replaced = stored();
    assert_eq!(replaced.0, "\"open\"");
    assert!(
        replaced.1 > unreadable_generation,
        "the projected copy must replace the unreadable row"
    );
    runtime.snapshot(Arc::clone(&projected));
    runtime.flush(Duration::from_secs(10)).unwrap();
    assert_eq!(stored(), replaced, "the replaced row is settled");
    assert_eq!(runtime.last_error(), None);
    runtime.shutdown();

    let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
    assert!(
        reopened.restore_warnings().is_empty(),
        "{:?}",
        reopened.restore_warnings()
    );
    assert!(
        restored
            .unwrap()
            .cards
            .iter()
            .any(|card| card.id == lost.id)
    );
    reopened.shutdown();
}

#[test]
fn writer_coalesces_and_restores_latest_snapshot() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let boot = BootId::new("boot-test");
    let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
    assert!(restored.is_none());

    let mut latest = workspace();
    latest.focused = latest.cards.first().map(|card| card.id);
    let latest = Arc::new(latest);
    runtime.snapshot(Arc::clone(&latest));
    runtime.snapshot(Arc::clone(&latest));
    runtime.shutdown();

    let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
    let restored = restored.unwrap();
    assert_eq!(restored.id, latest.id);
    assert_eq!(restored.focused, latest.focused);
    assert_eq!(
        restored
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<HashSet<_>>(),
        latest.cards.iter().map(|card| card.id).collect()
    );
    reopened.shutdown();
}

#[test]
fn window_snapshots_keep_identity_and_reopen_independently_while_source_is_live() {
    let directory = tempdir().unwrap();
    let primary = directory.path().join("sessions.sqlite3");
    let secondary = directory
        .path()
        .join("windows/monitor-two/sessions.sqlite3");
    let boot = BootId::new("window-test");
    let (mut first, _) = SessionRuntime::open(&primary, boot.clone()).unwrap();
    let mut one = workspace();
    first.snapshot(Arc::new(one.clone()));
    first.flush(Duration::from_secs(5)).unwrap();
    first
        .operation_store()
        .save_window_view("first-monitor")
        .unwrap();
    StateStore::copy_snapshot(&primary, &secondary).unwrap();
    let (mut second, restored) = SessionRuntime::open(&secondary, boot.clone()).unwrap();
    let mut two = restored.unwrap();
    assert_eq!(one.id, two.id);
    assert_eq!(one.cards[0].id, two.cards[0].id);
    assert_eq!(one.cards[0].kind, two.cards[0].kind);
    one.cards[0].width_fraction = 0.3;
    one.focused = Some(one.cards[0].id);
    two.cards[0].width_fraction = 0.8;
    two.focused = None;
    first.snapshot(Arc::new(one.clone()));
    second.snapshot(Arc::new(two.clone()));
    second
        .operation_store()
        .save_window_view("second-monitor")
        .unwrap();
    first.flush(Duration::from_secs(5)).unwrap();
    second.flush(Duration::from_secs(5)).unwrap();
    first.shutdown();
    // Closing one writer does not close or corrupt the other's live store.
    two.cards[0].width_fraction = 0.9;
    second.snapshot(Arc::new(two.clone()));
    second.flush(Duration::from_secs(5)).unwrap();
    second.shutdown();
    let (mut first, restored_one) = SessionRuntime::open(&primary, boot.clone()).unwrap();
    let (mut second, restored_two) = SessionRuntime::open(&secondary, boot).unwrap();
    assert_eq!(restored_one.unwrap().cards[0].width_fraction, 0.3);
    let restored_two = restored_two.unwrap();
    assert_eq!(restored_two.cards[0].width_fraction, 0.9);
    assert_eq!(restored_two.focused, None);
    assert_eq!(
        first
            .operation_store()
            .load_window_view()
            .unwrap()
            .as_deref(),
        Some("first-monitor")
    );
    assert_eq!(
        second
            .operation_store()
            .load_window_view()
            .unwrap()
            .as_deref(),
        Some("second-monitor")
    );
    first.shutdown();
    second.shutdown();
}

/// A chat card moved to another provider or account before its thread
/// began saves as its new conversation, whether or not the lane is keeping
/// conversions in order, and the next launch restores that conversation.
#[test]
fn a_replaced_chat_conversation_saves_with_its_snapshot_and_restores() {
    for ordered in [false, true] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("chat-replacement.sqlite3");
        let boot = BootId::new("chat-replacement");
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let mut state = workspace();
        let card = state.cards[0].id;
        let CardKind::Terminal(terminal) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        let chat = |provider: crate::agent::Provider, id: TerminalId| crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id,
            launch: crate::chat::Launch {
                provider,
                recipe: LaunchRecipe {
                    program: provider.executable().into(),
                    arguments: Vec::new(),
                    ..terminal.launch.clone()
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
        };
        let codex = chat(crate::agent::Provider::Codex, TerminalId::new());
        state.cards[0].kind = CardKind::Chat(codex.clone());
        state.cards[0].title = "Codex chat".into();
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(Duration::from_secs(10)).unwrap();
        if ordered {
            runtime.snapshot_conversion(Arc::new(state.clone()));
        }
        // An autosave still queued from before the switch.
        state.focused = Some(card);
        runtime.snapshot(Arc::new(state.clone()));

        let claude = chat(crate::agent::Provider::Claude, TerminalId::new());
        state.cards[0].kind = CardKind::Chat(claude.clone());
        state.cards[0].title = "Claude chat".into();
        runtime.replace_chat_identity(card, codex.id, claude.clone(), Arc::new(state.clone()));
        runtime.flush(Duration::from_secs(10)).unwrap();
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None, "ordered: {ordered}");

        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        let restored = restored.unwrap();
        assert_eq!(
            restored.cards[0].kind,
            CardKind::Chat(claude),
            "ordered: {ordered}"
        );
        assert_eq!(restored.cards[0].title, "Claude chat");
        reopened.shutdown();
    }
}

/// A write that records the conversations other windows gave shared chat
/// cards and then replaces another card's conversation, both with the
/// state that shows them, saves both, whether or not the lane is keeping
/// conversions in order.
#[test]
fn adopted_and_replaced_conversations_save_with_their_one_snapshot() {
    for ordered in [false, true] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("adopted-and-replaced.sqlite3");
        let boot = BootId::new("adopted-and-replaced");
        let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
        let health = runtime.health_receiver();
        let mut state = workspace();
        let CardKind::Terminal(terminal) = state.cards[0].kind.clone() else {
            unreachable!()
        };
        let chat = |provider: crate::agent::Provider| crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider,
                recipe: LaunchRecipe {
                    program: provider.executable().into(),
                    arguments: Vec::new(),
                    ..terminal.launch.clone()
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
        };
        let shared_before = chat(crate::agent::Provider::Codex);
        let own_before = chat(crate::agent::Provider::Codex);
        let mut own = state.cards[0].clone();
        own.id = CardId::new();
        own.kind = CardKind::Chat(own_before.clone());
        state.cards[0].kind = CardKind::Chat(shared_before.clone());
        state.cards.push(own.clone());
        let shared = state.cards[0].id;
        runtime.snapshot(Arc::new(state.clone()));
        runtime.flush(Duration::from_secs(10)).unwrap();
        if ordered {
            runtime.snapshot_conversion(Arc::new(state.clone()));
        }

        let theirs = chat(crate::agent::Provider::Claude);
        let mine = chat(crate::agent::Provider::Claude);
        state.cards[0].kind = CardKind::Chat(theirs.clone());
        state.cards[1].kind = CardKind::Chat(mine.clone());
        let saved = Arc::new(state.clone());
        runtime.stage_chat_adoptions([ChatIdentityAdoption {
            card: shared,
            expected: shared_before.id,
            canonical: theirs.clone(),
        }]);
        runtime.replace_chat_identity(own.id, own_before.id, mine.clone(), saved);
        runtime.flush(Duration::from_secs(10)).unwrap();
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None, "ordered: {ordered}");
        while let Ok(event) = health.try_recv() {
            assert!(event.quarantined.is_empty(), "ordered: {ordered}");
        }

        let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
        let restored = restored.unwrap();
        assert_eq!(
            restored
                .cards
                .iter()
                .map(|card| card.kind.clone())
                .collect::<Vec<_>>(),
            [CardKind::Chat(theirs), CardKind::Chat(mine)],
            "ordered: {ordered}"
        );
        reopened.shutdown();
    }
}

/// A session runtime with no writer thread, whose queue holds `older` as
/// the pending snapshot, so a test can read what each call queues.
fn queue_only_runtime(
    directory: &tempfile::TempDir,
    older: &Arc<WorkspaceState>,
    ordered: bool,
) -> SessionRuntime {
    let path = directory.path().join("queue-only.sqlite3");
    let store = Arc::new(StateStore::open(&path, BootId::new("queue-only")).unwrap());
    let (health_tx, health) = async_channel::unbounded();
    drop(health_tx);
    SessionRuntime {
        queue: Arc::new((
            Mutex::new(QueueState {
                latest: Some(Arc::clone(older)),
                ordered_identities: ordered,
                ..QueueState::default()
            }),
            Condvar::new(),
        )),
        last_error: Arc::new(Mutex::new(None)),
        health,
        worker: None,
        database_path: path,
        restored_projects: Vec::new(),
        restored_worktrees: Vec::new(),
        restore_warnings: Vec::new(),
        operation_store: store,
    }
}

fn staged_chat(state: &WorkspaceState) -> crate::chat::Session {
    let CardKind::Terminal(terminal) = state.cards[0].kind.clone() else {
        unreachable!()
    };
    crate::chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: terminal.launch,
            session_id: None,
        },
        proof: None,
        terminal: None,
    }
}

/// Adoptions wait, out of the writer's reach, for the write that shows
/// them, and are queued ahead of it under the same lock: never behind an
/// older state that still shows the replaced conversation on its own,
/// nor ahead of a replacement queued with a state that shows both.
#[test]
fn adoptions_are_queued_with_the_write_that_shows_them() {
    for ordered in [false, true] {
        let directory = tempdir().unwrap();
        let older = Arc::new(workspace());
        let runtime = queue_only_runtime(&directory, &older, ordered);
        let chat = staged_chat(&older);
        let (shared, own) = (CardId::new(), CardId::new());
        runtime.stage_chat_adoptions([ChatIdentityAdoption {
            card: shared,
            expected: TerminalId::new(),
            canonical: chat.clone(),
        }]);
        {
            let queue = runtime.queue.0.lock().unwrap();
            assert!(
                queue.conversions.is_empty() && queue.identity_replacements.is_empty(),
                "the writer cannot take a staged adoption, ordered: {ordered}"
            );
        }
        let saved = Arc::new(workspace());
        runtime.replace_chat_identity(own, TerminalId::new(), chat, Arc::clone(&saved));

        let queue = runtime.queue.0.lock().unwrap();
        let name = |card: CardId| {
            if card == shared {
                "adoption"
            } else {
                "replacement"
            }
        };
        let order = queue
            .conversions
            .iter()
            .map(|checkpoint| match checkpoint {
                ConversionCheckpoint::Snapshot(state) if Arc::ptr_eq(state, &older) => {
                    "older state"
                }
                ConversionCheckpoint::IdentityReplacement(replacement) => name(replacement.card_id),
                _ => "something else",
            })
            .collect::<Vec<_>>();
        if ordered {
            assert_eq!(order, ["older state", "adoption", "replacement"]);
        } else {
            assert!(order.is_empty());
            let mut merged = queue
                .identity_replacements
                .keys()
                .map(|card| name(*card))
                .collect::<Vec<_>>();
            merged.sort_unstable();
            assert_eq!(merged, ["adoption", "replacement"]);
        }
        assert!(
            queue
                .latest
                .as_ref()
                .is_some_and(|latest| Arc::ptr_eq(latest, &saved)),
            "the state that shows both is written after them, ordered: {ordered}"
        );
        assert!(queue.staged_adoptions.is_empty());
    }
}

/// The write that first keeps the lane in order queues staged adoptions
/// behind the older pending snapshot, not ahead of it: that snapshot still
/// shows the conversations the adoptions replace.
#[test]
fn adoptions_follow_the_older_state_when_the_lane_turns_ordered() {
    let directory = tempdir().unwrap();
    let older = Arc::new(workspace());
    let runtime = queue_only_runtime(&directory, &older, false);
    let shared = CardId::new();
    runtime.stage_chat_adoptions([ChatIdentityAdoption {
        card: shared,
        expected: TerminalId::new(),
        canonical: staged_chat(&older),
    }]);
    let saved = Arc::new(workspace());
    runtime.snapshot_conversion(Arc::clone(&saved));

    let queue = runtime.queue.0.lock().unwrap();
    let order = queue
        .conversions
        .iter()
        .map(|checkpoint| match checkpoint {
            ConversionCheckpoint::Snapshot(state) if Arc::ptr_eq(state, &older) => "older state",
            ConversionCheckpoint::Snapshot(state) if Arc::ptr_eq(state, &saved) => "saved state",
            ConversionCheckpoint::IdentityReplacement(replacement)
                if replacement.card_id == shared =>
            {
                "adoption"
            }
            _ => "something else",
        })
        .collect::<Vec<_>>();
    assert_eq!(order, ["older state", "adoption", "saved state"]);
}

#[test]
fn conversion_checkpoints_preserve_discovered_identity_before_fast_cli_return() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("chat-conversions.sqlite3");
    let boot = BootId::new("chat-conversions");
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let mut state = workspace();
    let CardKind::Terminal(mut terminal) = state.cards[0].kind.clone() else {
        unreachable!()
    };
    terminal.launch.program = "omp".into();
    terminal.launch.arguments.clear();
    terminal.transcript_path = None;
    let mut chat = crate::chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Omp,
            recipe: terminal.launch.clone(),
            session_id: None,
        },
        proof: None,
        terminal: None,
    };
    state.cards[0].kind = CardKind::Chat(chat.clone());
    runtime.snapshot_conversion(Arc::new(state.clone()));
    let exact_id = "77777777-1111-4111-8111-111111111111";
    chat.launch.session_id = Some(exact_id.into());
    state.cards[0].kind = CardKind::Chat(chat);
    runtime.snapshot(Arc::new(state.clone()));
    terminal.id = TerminalId::new();
    terminal.launch.arguments = vec!["--resume".into(), exact_id.into()];
    terminal.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: terminal.launch.clone(),
        last_exit: None,
    };
    state.cards[0].kind = CardKind::Terminal(terminal.clone());
    runtime.snapshot_conversion(Arc::new(state.clone()));
    // Even an immediate CLI retry must run after the return checkpoint.
    let previous_id = terminal.id;
    terminal.id = TerminalId::new();
    runtime.replace_terminal_identity(state.cards[0].id, previous_id, terminal.clone());
    state.cards[0].kind = CardKind::Terminal(terminal.clone());
    state.cards[0].title = "Latest terminal title".into();
    runtime.snapshot(Arc::new(state));
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);
    let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
    let restored = restored.unwrap();
    assert_eq!(restored.cards[0].kind, CardKind::Terminal(terminal));
    assert_eq!(restored.cards[0].title, "Latest terminal title");
    reopened.shutdown();
}

#[test]
fn terminal_identity_replacement_precedes_snapshot_on_writer_lane() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("terminal-relaunch.sqlite3");
    let boot = BootId::new("boot-terminal-relaunch");
    let initial = workspace();
    {
        let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
        assert!(restored.is_none());
        runtime.snapshot(Arc::new(initial.clone()));
        runtime.shutdown();
    }

    let (mut runtime, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let mut relaunched = restored.unwrap();
    let card_id = relaunched.cards[0].id;
    let CardKind::Terminal(session) = &mut relaunched.cards[0].kind else {
        panic!("expected terminal card");
    };
    let old_id = session.id;
    let new_id = TerminalId::new();
    let recipe = session.launch.clone();
    let replacement = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: new_id,
        launch: recipe.clone(),
        transcript_path: Some(PathBuf::from("/tmp/relaunched.log")),
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    *session = replacement.clone();

    runtime.replace_terminal_identity(card_id, old_id, replacement);
    runtime.snapshot(Arc::new(relaunched));
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);

    let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
    let restored = restored.unwrap();
    let CardKind::Terminal(session) = &restored.cards[0].kind else {
        panic!("expected terminal card");
    };
    assert_eq!(session.id, new_id);
    assert_eq!(
        session.transcript_path,
        Some(PathBuf::from("/tmp/relaunched.log"))
    );
    reopened.shutdown();
}

#[test]
fn writer_restores_project_and_worktree_catalog() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let boot = BootId::new("boot-catalog");
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let project = ProjectState {
        kind: Default::default(),
        id: ProjectId::new(),
        generation: 0,
        name: "cagents".into(),
        repository: PathBuf::from("/repo/cagents"),
        existence: Existence::Open,
    };
    let worktree = WorktreeState {
        id: WorktreeId::new(),
        project_id: project.id,
        generation: 0,
        path: PathBuf::from("/repo/cagents"),
        branch: Some("main".into()),
        existence: Existence::Open,
    };
    runtime.save_project(project.clone());
    runtime.save_worktrees([worktree.clone()]);
    runtime.shutdown();

    let (mut reopened, _) = SessionRuntime::open(&path, boot).unwrap();
    let (projects, worktrees) = reopened.take_catalog();
    assert_eq!(
        projects,
        vec![ProjectState {
            generation: 1,
            ..project
        }]
    );
    assert_eq!(
        worktrees,
        vec![WorktreeState {
            generation: 1,
            ..worktree
        }]
    );
    reopened.shutdown();
}

/// Worktree ids are derived from their path, so creating a worktree where a
/// deleted one lived reuses the tombstoned id. That save used to be refused
/// forever, exhausting the lane and stranding every catalog write after it
/// (#245).
#[test]
fn worktree_recreated_at_a_deleted_path_reopens_its_catalog_row() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let boot = BootId::new("boot-recreate");
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let project = ProjectState {
        kind: Default::default(),
        id: ProjectId::new(),
        generation: 0,
        name: "cagents".into(),
        repository: PathBuf::from("/repo/cagents"),
        existence: Existence::Open,
    };
    let worktree = WorktreeState {
        id: WorktreeId::new(),
        project_id: project.id,
        generation: 0,
        path: PathBuf::from("/repo/cagents/worktrees/feature"),
        branch: Some("feature".into()),
        existence: Existence::Open,
    };
    let deleted = WorktreeState {
        existence: Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("worktree deleted by user".into()),
            },
        },
        ..worktree.clone()
    };
    let timeout = Duration::from_secs(5);
    runtime.save_project(project);
    runtime.save_worktrees([worktree.clone()]);
    runtime.flush(timeout).unwrap();
    runtime.save_worktrees([deleted]);
    runtime.flush(timeout).unwrap();

    runtime.save_worktrees([worktree.clone()]);
    runtime.flush(timeout).unwrap();
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);

    let (mut reopened, _) = SessionRuntime::open(&path, boot).unwrap();
    let (_, worktrees) = reopened.take_catalog();
    assert_eq!(
        worktrees,
        vec![WorktreeState {
            generation: 3,
            ..worktree
        }]
    );
    reopened.shutdown();
}

#[test]
fn transient_write_failure_retries_without_losing_snapshot() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("retry.sqlite3");
    let store = StateStore::open_with_options(
        &path,
        BootId::new("boot-retry"),
        crate::persistence::StoreOptions {
            busy_timeout: Duration::from_millis(10),
        },
    )
    .unwrap();
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
    queue.0.lock().unwrap().latest = Some(Arc::new(workspace()));
    let last_error = Arc::new(Mutex::new(None));
    let worker_queue = Arc::clone(&queue);
    let worker_error = Arc::clone(&last_error);
    let worker = thread::spawn(move || {
        let (health_tx, _health_rx) = async_channel::unbounded();
        let mut health = HealthPublisher::new(health_tx);
        persistence_loop(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            worker_queue,
            worker_error,
            &mut health,
            PersistenceConfig::default(),
        );
    });

    crate::test_timing::eventually("the initial write failing", || {
        last_error.lock().unwrap().is_some()
    });
    blocker.execute_batch("ROLLBACK").unwrap();
    crate::test_timing::eventually("the snapshot being retried", || {
        blocker
            .query_row("SELECT COUNT(*) FROM workspaces", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
            == 1
    });
    let (lock, changed) = &*queue;
    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
    assert!(last_error.lock().unwrap().is_none());
}

#[test]
fn final_clean_marker_failure_is_classified_even_with_no_pending_writes() {
    let mut store = FakeStore::new([]);
    store.shutdown_failure = true;
    let queue = Arc::new((
        Mutex::new(QueueState {
            shutdown: true,
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None));
    let (health_tx, _health_rx) = async_channel::unbounded();
    let mut health = HealthPublisher::new(health_tx);
    let outcome = persistence_loop(
        store,
        PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
        queue,
        Arc::clone(&last_error),
        &mut health,
        fast_persistence_config(1),
    );
    assert_eq!(
        outcome,
        WriterExit::Shutdown(crate::diagnostics::Event::FinalFlush {
            complete: false,
            unsaved: 0,
            refused: 0,
            category: Some(crate::diagnostics::Failure::PermissionDenied)
        })
    );
    assert!(
        last_error
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("TOKEN")
    );
    let WriterExit::Shutdown(event) = outcome else {
        panic!("writer failed unexpectedly");
    };
    assert!(!serde_json::to_string(&event).unwrap().contains("TOKEN"));
}

#[test]
fn failed_final_write_does_not_mark_store_clean() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("failed-shutdown.sqlite3");
    StateStore::open(&path, BootId::new("boot-shutdown"))
        .unwrap()
        .shutdown()
        .unwrap();
    let store = StateStore::open_with_options(
        &path,
        BootId::new("boot-shutdown"),
        crate::persistence::StoreOptions {
            busy_timeout: Duration::from_millis(10),
        },
    )
    .unwrap();
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::new(workspace())),
            shutdown: true,
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None));
    let (health_tx, _health_rx) = async_channel::unbounded();
    let mut health = HealthPublisher::new(health_tx);

    let outcome = persistence_loop(
        store,
        PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
        queue,
        Arc::clone(&last_error),
        &mut health,
        PersistenceConfig {
            // The blocker never lets go, so the budget only decides how long
            // this test waits for the flush to give up.
            shutdown_flush_budget: Duration::from_millis(200),
            ..PersistenceConfig::default()
        },
    );
    assert!(matches!(
        outcome,
        WriterExit::Shutdown(crate::diagnostics::Event::FinalFlush {
            complete: false,
            unsaved: 1,
            refused: 0,
            category: Some(crate::diagnostics::Failure::Busy)
        })
    ));
    assert!(last_error.lock().unwrap().is_some());
    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);

    let reopened = StateStore::open(&path, BootId::new("boot-shutdown")).unwrap();
    assert!(reopened.recovery_report().dirty_shutdown);
}

#[test]
fn retried_shutdown_flush_persists_the_snapshot_and_records_a_clean_shutdown() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("retried-shutdown.sqlite3");
    let boot = BootId::new("boot-shutdown-retry");
    StateStore::open(&path, boot.clone())
        .unwrap()
        .shutdown()
        .unwrap();
    let store = StateStore::open_with_options(
        &path,
        boot.clone(),
        crate::persistence::StoreOptions {
            busy_timeout: Duration::from_millis(10),
        },
    )
    .unwrap();
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Released only after the first attempt has already failed, so nothing
    // is persisted unless the shutdown flush retries.
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        blocker.execute_batch("ROLLBACK").unwrap();
    });

    let snapshot = Arc::new(workspace());
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::clone(&snapshot)),
            shutdown: true,
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None));
    let (health_tx, _health_rx) = async_channel::unbounded();
    let mut health = HealthPublisher::new(health_tx);

    let outcome = persistence_loop(
        store,
        PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
        queue,
        Arc::clone(&last_error),
        &mut health,
        PersistenceConfig::default(),
    );
    release.join().unwrap();
    assert!(matches!(
        outcome,
        WriterExit::Shutdown(crate::diagnostics::Event::FinalFlush {
            complete: true,
            unsaved: 0,
            refused: 0,
            category: None,
        })
    ));
    assert!(last_error.lock().unwrap().is_none());

    let reopened = StateStore::open(&path, boot).unwrap();
    assert!(
        !reopened.recovery_report().dirty_shutdown,
        "a shutdown flush that eventually succeeded must still record a \
             clean shutdown"
    );
    assert!(
        reopened
            .load_workspace(snapshot.id, None)
            .unwrap()
            .is_some(),
        "the final snapshot must survive a shutdown flush that failed once"
    );
}

#[test]
fn a_stale_base_is_rebased_and_the_snapshot_still_lands() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("stale-base.sqlite3");
    let boot = BootId::new("boot-stale-base");
    let store = StateStore::open(&path, boot.clone()).unwrap();
    let mut state = workspace();
    // A commit the writer lane never learns about: storage ends up one
    // generation ahead of the bases the lane is about to write against,
    // which is what a batch that applied without the lane seeing it leaves
    // behind.
    store
        .save_workspace(
            &state,
            SnapshotBase {
                workspace: 0,
                cards: &BTreeMap::new(),
            },
        )
        .unwrap();

    state.cards[0].title = "written after the conflict".into();
    state.focused = Some(state.cards[0].id);
    let snapshot = Arc::new(state);
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::clone(&snapshot)),
            shutdown: true,
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let last_error = Arc::new(Mutex::new(None));
    let (health_tx, _health_rx) = async_channel::unbounded();
    let mut health = HealthPublisher::new(health_tx);

    persistence_loop(
        store,
        PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
        queue,
        Arc::clone(&last_error),
        &mut health,
        fast_persistence_config(4),
    );
    assert_eq!(
        last_error.lock().unwrap().clone(),
        None,
        "a base the writer can re-read is not a save failure"
    );

    let reopened = StateStore::open(&path, boot).unwrap();
    let stored = reopened.load_workspace(snapshot.id, None).unwrap().unwrap();
    assert_eq!(stored.cards.len(), snapshot.cards.len());
    assert_eq!(stored.cards[0].id, snapshot.cards[0].id);
    assert_eq!(stored.cards[0].title, snapshot.cards[0].title);
    assert_eq!(stored.focused, snapshot.focused);
}

#[test]
fn a_checkpoint_is_honoured_after_the_base_went_stale() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("stale-checkpoint.sqlite3");
    let boot = BootId::new("boot-stale-checkpoint");
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let mut state = workspace();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(Duration::from_secs(5)).unwrap();

    // Advance the stored generation behind the writer lane's back, through
    // the very store it writes through. The lane's base is now one commit
    // behind, and the close path cannot tell: it only asks for a barrier.
    let mut cards = BTreeMap::new();
    cards.insert(state.cards[0].id, 1);
    runtime
        .operation_store()
        .save_workspace(
            &state,
            SnapshotBase {
                workspace: 1,
                cards: &cards,
            },
        )
        .unwrap();

    state.cards[0].width_fraction = 0.25;
    runtime.snapshot(Arc::new(state.clone()));
    runtime
        .flush(Duration::from_secs(5))
        .expect("a checkpoint must still be satisfied after the base went stale");
    assert_eq!(runtime.last_error(), None);
    runtime.shutdown();

    let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
    let restored = restored.expect("the workspace survives the conflict");
    assert_eq!(restored.cards[0].width_fraction, 0.25);
    reopened.shutdown();
}

#[test]
fn a_failure_no_rebase_can_fix_is_reported_instead_of_rebased() {
    let store = FakeStore::always(FakeWrite::FailDiskFull);
    let rebases = Arc::clone(&store.rebases);
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::new(workspace())),
            checkpoints: vec![response],
            shutdown: true,
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let (health_tx, health_rx) = async_channel::unbounded();
    let mut health = HealthPublisher::new(health_tx);

    persistence_loop(
        store,
        PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
        queue,
        Arc::new(Mutex::new(None)),
        &mut health,
        fast_persistence_config(2),
    );

    let refusal = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the checkpoint is answered rather than dropped")
        .expect_err("a full disk cannot report a saved workspace");
    assert!(
        refusal.contains(SaveFailureCategory::DiskFull.label())
            && refusal.contains(SaveFailureCategory::DiskFull.guidance()),
        "a full disk has to reach the reader as one: {refusal}"
    );
    assert_eq!(
        rebases.load(Ordering::Relaxed),
        0,
        "re-reading generations cannot free disk space, so no rebase may be attempted"
    );
    let mut categories = Vec::new();
    while let Ok(event) = health_rx.try_recv() {
        match event.health {
            SessionSaveHealth::Retrying { category, .. }
            | SessionSaveHealth::RetryExhausted { category, .. } => categories.push(category),
            _ => {}
        }
    }
    assert!(
        !categories.is_empty()
            && categories
                .iter()
                .all(|category| *category == SaveFailureCategory::DiskFull),
        "a hard failure must never be reported as a conflict: {categories:?}"
    );
}

#[test]
fn a_conflict_the_rebase_cannot_resolve_still_reports_its_category() {
    let store = FakeStore::always(FakeWrite::FailStale);
    let rebases = Arc::clone(&store.rebases);
    let queue = Arc::new((
        Mutex::new(QueueState {
            latest: Some(Arc::new(workspace())),
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let (health_tx, health_rx) = async_channel::unbounded();
    let health = HealthPublisher::new(health_tx);
    let worker_queue = Arc::clone(&queue);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            store,
            PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new()),
            worker_queue,
            Arc::new(Mutex::new(None)),
            health,
            fast_persistence_config(2),
        );
    });

    let exhausted = loop {
        match next_health(&health_rx).health {
            SessionSaveHealth::RetryExhausted {
                category, attempts, ..
            } => break (category, attempts),
            _ => continue,
        }
    };
    assert_eq!(
        exhausted,
        (SaveFailureCategory::Conflict, 2),
        "a conflict the rebase could not resolve still has to surface as one"
    );
    assert_eq!(
        rebases.load(Ordering::Relaxed),
        1,
        "a batch is rebased once however often it is retried"
    );

    let (lock, changed) = &*queue;
    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
}

fn project(name: &str) -> ProjectState {
    ProjectState {
        kind: Default::default(),
        id: ProjectId::new(),
        generation: 0,
        name: name.into(),
        repository: PathBuf::from("/repo").join(name),
        existence: Existence::Open,
    }
}

fn worktree(project: &ProjectState, name: &str) -> WorktreeState {
    WorktreeState {
        id: WorktreeId::new(),
        project_id: project.id,
        generation: 0,
        path: project.repository.join("worktrees").join(name),
        branch: Some(name.into()),
        existence: Existence::Open,
    }
}

fn closed() -> Existence {
    Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("closed by the test".into()),
        },
    }
}

/// The last event the lane published so far.
fn latest_health(
    receiver: &async_channel::Receiver<SessionSaveHealthEvent>,
) -> SessionSaveHealthEvent {
    let mut latest = None;
    while let Ok(event) = receiver.try_recv() {
        latest = Some(event);
    }
    latest.expect("the lane publishes an event for every change the test waits on")
}

/// The first event, up to a deadline, that satisfies `wanted`.
fn health_until(
    receiver: &async_channel::Receiver<SessionSaveHealthEvent>,
    wanted: impl Fn(&SessionSaveHealthEvent) -> bool,
) -> SessionSaveHealthEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match receiver.try_recv() {
            Ok(event) if wanted(&event) => return event,
            Ok(_) => {}
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "the lane never reached the awaited health"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn foreign_key_violation() -> StoreError {
    StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
        Some("FOREIGN KEY constraint failed".into()),
    ))
}

#[derive(Debug, Clone, Copy)]
enum ProjectWrite {
    Land,
    Busy,
    Refuse,
}

/// A catalog store that enforces the worktree -> project reference the way
/// SQLite does, and scripts what each project's writes do. A project with
/// no script left lands.
#[derive(Default)]
struct CatalogStore {
    projects: Mutex<HashMap<ProjectId, VecDeque<ProjectWrite>>>,
    stored_projects: Mutex<HashSet<ProjectId>>,
    saved_worktrees: Mutex<Vec<WorktreeId>>,
    missing_parents: AtomicUsize,
}

impl CatalogStore {
    fn new<const N: usize>(project: ProjectId, writes: [ProjectWrite; N]) -> Self {
        Self {
            projects: Mutex::new(HashMap::from([(project, writes.into())])),
            ..Self::default()
        }
    }

    fn saved_worktrees(&self) -> Vec<WorktreeId> {
        self.saved_worktrees.lock().unwrap().clone()
    }
}

impl SessionStore for CatalogStore {
    fn replace_terminal_identity(
        &self,
        _card_id: CardId,
        _expected_terminal_id: TerminalId,
        _replacement: &TerminalSession,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    fn replace_chat_identity(
        &self,
        _card_id: CardId,
        _expected_id: TerminalId,
        _replacement: &crate::chat::Session,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    fn save_workspace(
        &self,
        _state: &WorkspaceState,
        _base: SnapshotBase<'_>,
    ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
        Ok(crate::persistence::WorkspaceCommit::default())
    }

    fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError> {
        let write = self
            .projects
            .lock()
            .unwrap()
            .get_mut(&project.id)
            .and_then(VecDeque::pop_front)
            .unwrap_or(ProjectWrite::Land);
        match write {
            ProjectWrite::Land => {
                self.stored_projects.lock().unwrap().insert(project.id);
                Ok(project.generation + 1)
            }
            ProjectWrite::Busy => Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            ))),
            ProjectWrite::Refuse => Err(invalid_state!(
                "tombstoned project {} cannot be resurrected",
                project.id
            )),
        }
    }

    fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError> {
        if !self
            .stored_projects
            .lock()
            .unwrap()
            .contains(&worktree.project_id)
        {
            self.missing_parents.fetch_add(1, Ordering::Relaxed);
            return Err(foreign_key_violation());
        }
        self.saved_worktrees.lock().unwrap().push(worktree.id);
        Ok(worktree.generation + 1)
    }

    fn stored_generations(&self, _workspace: WorkspaceId) -> Result<StoredGenerations, StoreError> {
        Ok(StoredGenerations::default())
    }

    fn stored_project_generation(&self, _project: ProjectId) -> Result<Option<u64>, StoreError> {
        Ok(None)
    }

    fn stored_worktree_generation(&self, _worktree: WorktreeId) -> Result<Option<u64>, StoreError> {
        Ok(None)
    }

    fn shutdown(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

fn empty_writes() -> PendingWrites {
    PendingWrites::restored(0, BTreeMap::new(), HashMap::new(), HashMap::new())
}

#[test]
fn autosave_skips_a_snapshot_that_already_landed_unchanged() {
    let store = FakeStore::always(FakeWrite::Succeed);
    let attempts = Arc::clone(&store.attempts);
    let snapshot = Arc::new(workspace());
    let mut writes = empty_writes();

    writes.workspace = Some(Arc::clone(&snapshot));
    writes.flush_checkpoints(&store).unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 1);

    // An ordinary publication can prepare the same immutable state after the
    // preceding debounce/flush completed. It must not rewrite its workspace
    // row only to advance its generation and timestamp.
    writes.workspace = Some(Arc::clone(&snapshot));
    writes.flush_checkpoints(&store).unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 1);

    // The saved weak reference makes copy-on-write detach before mutation, so
    // a real persistent change always reaches storage.
    let mut changed = snapshot;
    Arc::make_mut(&mut changed).cards[0].title = "changed after save".into();
    writes.workspace = Some(changed);
    writes.flush_checkpoints(&store).unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
}

#[test]
fn autosave_retries_a_previously_saved_snapshot_after_a_refusal() {
    for refused in [FakeWrite::Refuse, FakeWrite::PartlyRefused] {
        let store = FakeStore::new([FakeWrite::Succeed, refused, FakeWrite::Succeed]);
        let attempts = Arc::clone(&store.attempts);
        let saved = Arc::new(workspace());
        let mut writes = empty_writes();

        writes.workspace = Some(Arc::clone(&saved));
        writes.flush_checkpoints(&store).unwrap();
        let different_offer = Arc::new((*saved).clone());
        writes.workspace = Some(different_offer);
        writes.flush_checkpoints(&store).unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        // Storage may now hold only part of the intervening offer (or none of
        // it). The older clean pointer must not hide this renewed offer.
        writes.workspace = Some(Arc::clone(&saved));
        writes.flush_checkpoints(&store).unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }
}

#[test]
fn autosave_retries_a_previously_saved_snapshot_after_an_io_error() {
    let store = FakeStore::new([FakeWrite::Succeed, FakeWrite::Fail, FakeWrite::Succeed]);
    let attempts = Arc::clone(&store.attempts);
    let saved = Arc::new(workspace());
    let mut writes = empty_writes();

    writes.workspace = Some(Arc::clone(&saved));
    writes.flush_checkpoints(&store).unwrap();
    writes.workspace = Some(Arc::new((*saved).clone()));
    assert!(writes.flush_checkpoints(&store).is_err());
    assert_eq!(attempts.load(Ordering::Relaxed), 2);

    // A retry may coalesce back to an earlier snapshot. Its previous clean
    // pointer no longer proves storage holds it after a failed write.
    writes.workspace = Some(Arc::clone(&saved));
    writes.flush_checkpoints(&store).unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 3);
}

#[test]
fn a_conversion_of_the_last_clean_snapshot_is_never_skipped() {
    let store = FakeStore::always(FakeWrite::Succeed);
    let attempts = Arc::clone(&store.attempts);
    let snapshot = Arc::new(workspace());
    let mut writes = empty_writes();

    writes.workspace = Some(Arc::clone(&snapshot));
    writes.flush_checkpoints(&store).unwrap();
    writes
        .conversions
        .push_back(ConversionCheckpoint::Snapshot(snapshot));
    writes.flush_checkpoints(&store).unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
}

#[test]
fn chat_recovery_io_failure_is_acknowledged_without_a_later_retry() {
    for kind in [ChatRecoveryKind::Account, ChatRecoveryKind::Upgrade] {
        let store = FakeStore::new([FakeWrite::FailDiskFull, FakeWrite::Succeed]);
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: None,
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                session_id: Some("saved-conversation".into()),
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: BTreeMap::from([("CODEX_HOME".into(), "/removed".into())]),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut replacement = source.clone();
        replacement.id = TerminalId::new();
        replacement
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), "/selected".into());
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        let mut writes = empty_writes();
        writes
            .conversions
            .push_back(ConversionCheckpoint::ChatRecovery(Box::new(
                ChatRecoveryWrite {
                    kind,
                    card: CardId::new(),
                    source,
                    replacement,
                    reply,
                },
            )));
        assert!(
            writes.flush(&store).is_empty(),
            "failure belongs to the explicit request"
        );
        assert!(receipt.recv().unwrap().is_err());
        assert!(writes.is_settled());
        writes.rearm_quarantine();
        assert!(writes.flush(&store).is_empty());
        assert_eq!(
            store.attempts.load(Ordering::Relaxed),
            1,
            "a rejected candidate must never commit later"
        );
        assert!(writes.chat_rebindings.is_empty());
    }
}

#[test]
fn upgrade_recovery_translates_queued_source_snapshots_before_the_next_save() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("session.db"), BootId::new("boot")).unwrap();
    let proof = |name| crate::model::HostSessionProof {
        host_session_id: crate::model::HostSessionId::new(name),
        boot_id: BootId::new("boot"),
        host_incarnation: crate::model::HostIncarnation::new(name),
    };
    let source = crate::chat::Session {
        id: TerminalId::new(),
        proof: Some(proof("old")),
        terminal: None,
        native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: "/project".into(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
        },
    };
    let mut replacement = source.clone();
    replacement.proof = Some(proof("new"));
    // Upgrade recovery advances the same still-unnamed retained actor.
    let mut snapshot = workspace();
    let card = snapshot.cards[0].id;
    snapshot.cards[0].kind = CardKind::Chat(source.clone());
    let mut writes = empty_writes();
    writes.workspace = Some(Arc::new(snapshot.clone()));
    assert!(writes.flush(&store).is_empty());

    let (raw_reply, raw_receipt) = std::sync::mpsc::sync_channel(1);
    writes
        .conversions
        .push_back(ConversionCheckpoint::ChatRecovery(Box::new(
            ChatRecoveryWrite {
                kind: ChatRecoveryKind::Upgrade,
                card,
                source: source.clone(),
                replacement: replacement.clone(),
                reply: raw_reply,
            },
        )));
    assert!(writes.flush(&store).is_empty());
    assert!(
        raw_receipt.recv().unwrap().is_err(),
        "raw retained descriptors cannot authorize an upgrade or queued-snapshot translation"
    );
    assert!(writes.chat_rebindings.is_empty());
    assert_eq!(
        store.load_card(card, None).unwrap().unwrap().kind,
        CardKind::Chat(source.clone())
    );

    let handle = crate::chat::client::ClientHandle::fixture_upgrade_handshake(
        source.clone(),
        replacement.clone(),
        crate::chat::Snapshot {
            session_id: replacement.launch.session_id.clone(),
            ready: true,
            ..Default::default()
        },
    )
    .unwrap();
    let recovery = handle.authenticated_upgrade_recovery().unwrap().clone();
    assert!(recovery.validate());
    assert_eq!(recovery.source(), &source);
    assert_eq!(recovery.replacement(), &replacement);
    let (reply, receipt) = std::sync::mpsc::sync_channel(1);
    writes
        .conversions
        .push_back(ConversionCheckpoint::ChatRecovery(Box::new(
            ChatRecoveryWrite {
                kind: ChatRecoveryKind::AuthenticatedUpgrade(Box::new(recovery)),
                card,
                source,
                replacement: replacement.clone(),
                reply,
            },
        )));
    // An autosave produced while Ready awaits the receipt still carries
    // the old descriptor. It must not undo the authenticated handoff.
    snapshot.cards[0].title = "Saved during recovery".into();
    writes.workspace = Some(Arc::new(snapshot));
    assert!(writes.flush(&store).is_empty());
    receipt.recv().unwrap().unwrap();
    let saved = store.load_card(card, None).unwrap().unwrap();
    assert_eq!(saved.kind, CardKind::Chat(replacement));
    assert_eq!(saved.title, "Saved during recovery");
    assert!(writes.is_settled());
}

#[test]
fn retained_copy_keeps_its_typed_payload_for_io_retry_but_not_content_refusal() {
    for transient in [false, true] {
        let mut state = workspace();
        let origin = state.cards[0].id;
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: None,
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: Some("saved-thread".into()),
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
            },
        };
        state.cards[0].id = CardId::new();
        state.cards[0].kind = CardKind::Chat(source.clone());
        let copy = crate::persistence::RetainedChatCopy {
            workspace: state.id,
            source_card: origin,
            source,
            copy: state.cards[0].clone(),
            draft: "Keep exact retry bytes".into(),
            attachments: Vec::new(),
            draft_model: None,
            scope: None,
            channel: None,
        };
        let store = FakeStore::new([
            if transient {
                FakeWrite::FailDiskFull
            } else {
                FakeWrite::Refuse
            },
            FakeWrite::Succeed,
        ]);
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        let mut writes = empty_writes();
        writes
            .conversions
            .push_back(ConversionCheckpoint::RetainedChatCopy {
                copy: Box::new(copy.clone()),
                reply: Some(reply),
            });
        let failed = writes.flush(&store);
        assert_eq!(!failed.is_empty(), transient);
        assert!(
            receipt
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        drop(receipt);
        if transient {
            assert!(
                matches!(writes.conversions.front(), Some(ConversionCheckpoint::RetainedChatCopy { copy: retained, reply: None }) if **retained == copy)
            );
            // The existing explicit retry path does not reconstruct the
            // failed canonical snapshot or a different source descriptor.
            writes.rearm_quarantine();
            assert!(writes.flush(&store).is_empty());
            assert_eq!(writes.retained_chat_copies[&copy.copy.id], copy);
            assert_eq!(store.attempts.load(Ordering::Relaxed), 2);
        } else {
            assert!(writes.is_settled());
            writes.rearm_quarantine();
            assert!(writes.flush(&store).is_empty());
            assert_eq!(store.attempts.load(Ordering::Relaxed), 1);
        }
    }
}

#[test]
fn retained_copy_explicit_retry_commits_real_bytes_after_its_initial_error() {
    struct FailFirstCopy {
        store: Arc<StateStore>,
        attempts: Arc<AtomicUsize>,
    }
    impl SessionStore for FailFirstCopy {
        fn preserve_chat_copy(
            &self,
            copy: &crate::persistence::RetainedChatCopy,
        ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(StoreError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOSPC,
                )));
            }
            self.store.preserve_chat_copy(copy)
        }
        fn replace_terminal_identity(
            &self,
            card: CardId,
            expected: TerminalId,
            replacement: &TerminalSession,
        ) -> Result<(), StoreError> {
            self.store
                .replace_terminal_identity(card, expected, replacement)
        }
        fn replace_chat_identity(
            &self,
            card: CardId,
            expected: TerminalId,
            replacement: &crate::chat::Session,
        ) -> Result<(), StoreError> {
            self.store
                .replace_chat_identity(card, expected, replacement)
        }
        fn adopt_chat_identity(
            &self,
            card: CardId,
            expected: TerminalId,
            canonical: &crate::chat::Session,
        ) -> Result<(), StoreError> {
            self.store.adopt_chat_identity(card, expected, canonical)
        }
        fn save_workspace(
            &self,
            state: &WorkspaceState,
            base: SnapshotBase<'_>,
        ) -> Result<crate::persistence::WorkspaceCommit, StoreError> {
            self.store.save_workspace(state, base)
        }
        fn save_project(&self, project: &ProjectState) -> Result<u64, StoreError> {
            self.store.save_project(project)
        }
        fn save_worktree(&self, worktree: &WorktreeState) -> Result<u64, StoreError> {
            self.store.save_worktree(worktree)
        }
        fn stored_generations(
            &self,
            workspace: WorkspaceId,
        ) -> Result<StoredGenerations, StoreError> {
            self.store.stored_generations(workspace)
        }
        fn stored_project_generation(&self, project: ProjectId) -> Result<Option<u64>, StoreError> {
            self.store.stored_project_generation(project)
        }
        fn stored_worktree_generation(
            &self,
            worktree: WorktreeId,
        ) -> Result<Option<u64>, StoreError> {
            self.store.stored_worktree_generation(worktree)
        }
        fn shutdown(&self) -> Result<(), StoreError> {
            self.store.shutdown()
        }
    }

    let directory = tempdir().unwrap();
    let path = directory.path().join("late-copy.db");
    let scope = directory.path().join("project");
    std::fs::create_dir(&scope).unwrap();
    let store = Arc::new(StateStore::open(&path, BootId::new("boot")).unwrap());
    let source = crate::chat::Session {
        id: TerminalId::new(),
        proof: Some(crate::model::HostSessionProof {
            host_session_id: "old".into(),
            boot_id: "boot".into(),
            host_incarnation: "old".into(),
        }),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: scope.clone(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
        },
    };
    let mut original = workspace();
    let origin = original.cards[0].id;
    original.cards[0].kind = CardKind::Chat(source.clone());
    original
        .chat_drafts
        .insert(origin, "Canonical draft stays here".into());
    let committed = store
        .save_workspace(
            &original,
            SnapshotBase {
                workspace: 0,
                cards: &BTreeMap::new(),
            },
        )
        .unwrap();
    let canonical_before = store.load_card(origin, None).unwrap().unwrap();
    let mut card = original.cards[0].clone();
    card.id = CardId::new();
    let copy = crate::persistence::RetainedChatCopy {
        workspace: original.id,
        source_card: origin,
        source: source.clone(),
        copy: card,
        draft: "Retry exact bytes  \nUnicode 🦀".into(),
        attachments: Vec::new(),
        draft_model: None,
        scope: Some(scope.clone()),
        channel: None,
    };
    let (reply, receipt) = std::sync::mpsc::sync_channel(1);
    let queue = Arc::new((
        Mutex::new(QueueState {
            conversions: VecDeque::from([ConversionCheckpoint::RetainedChatCopy {
                copy: Box::new(copy.clone()),
                reply: Some(reply),
            }]),
            ordered_identities: true,
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let attempts = Arc::new(AtomicUsize::new(0));
    let wrapped = FailFirstCopy {
        store: Arc::clone(&store),
        attempts: Arc::clone(&attempts),
    };
    let (health_tx, health_rx) = async_channel::unbounded();
    let health = HealthPublisher::new(health_tx);
    let worker_queue = Arc::clone(&queue);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            wrapped,
            PendingWrites::restored(
                committed.generation,
                committed.card_generations,
                HashMap::new(),
                HashMap::new(),
            ),
            worker_queue,
            Arc::new(Mutex::new(None)),
            health,
            fast_persistence_config(1),
        )
    });

    let first_receipt = receipt.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        first_receipt.is_err(),
        "the failed first attempt is never a successful receipt"
    );
    let failed = health_until(&health_rx, |event| {
        matches!(
            event.health,
            SessionSaveHealth::RetryExhausted {
                category: SaveFailureCategory::DiskFull,
                attempts: 1,
                ..
            }
        )
    });
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(store.load_card(copy.copy.id, None).unwrap().is_none());
    assert_eq!(
        store.load_card(origin, None).unwrap().unwrap(),
        canonical_before
    );

    // This is the queue action used by Retry Saving. No second copy,
    // different descriptor, replacement snapshot or actor action is sent.
    let (lock, changed) = &*queue;
    lock.lock().unwrap().retry_requested = true;
    changed.notify_one();
    let recovered = health_until(&health_rx, |event| {
        matches!(event.health, SessionSaveHealth::Recovered { .. })
    });
    assert!(recovered.revision > failed.revision);
    assert!(recovered.last_successful_flush_at_unix_ms.is_some());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        matches!(
            receipt.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ),
        "late recovery is a health event, not a changed first receipt"
    );
    let saved = store.load_workspace(copy.workspace, None).unwrap().unwrap();
    assert_eq!(
        saved
            .cards
            .iter()
            .find(|card| card.id == copy.copy.id)
            .unwrap()
            .kind,
        CardKind::Chat(source.clone())
    );
    assert_eq!(saved.chat_drafts[&copy.copy.id], copy.draft);
    assert!(saved.local_only_cards.contains(&copy.copy.id));
    assert_eq!(saved.pending_chat_admissions[&copy.copy.id], source);
    assert_eq!(saved.card_scopes[&copy.copy.id], scope);
    assert_eq!(saved.chat_drafts[&origin], "Canonical draft stays here");
    assert_eq!(
        store.load_card(origin, None).unwrap().unwrap(),
        canonical_before
    );

    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
    drop(store);
    let reopened = StateStore::open(&path, BootId::new("boot")).unwrap();
    let restored = reopened
        .load_workspace(copy.workspace, None)
        .unwrap()
        .unwrap();
    assert!(
        restored
            .cards
            .iter()
            .any(|card| card.id == copy.copy.id && card.existence.is_open())
    );
    assert_eq!(restored.chat_drafts[&copy.copy.id], copy.draft);
    assert!(restored.local_only_cards.contains(&copy.copy.id));
    assert_eq!(restored.pending_chat_admissions[&copy.copy.id], copy.source);
    assert_eq!(restored.card_scopes[&copy.copy.id], scope);
    assert_eq!(
        reopened.load_card(origin, None).unwrap().unwrap().kind,
        canonical_before.kind
    );
}

#[test]
fn retained_copy_writer_survives_refused_canonical_snapshots_and_explicit_close() {
    for after_upgrade in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retained-copy.db");
        let (mut runtime, _) = SessionRuntime::open(&path, BootId::new("boot")).unwrap();
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(crate::model::HostSessionProof {
                host_session_id: "old".into(),
                boot_id: "boot".into(),
                host_incarnation: "old".into(),
            }),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: Some("11111111-1111-4111-8111-111111111111".into()),
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: BTreeMap::new(),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut original = workspace();
        let origin = original.cards[0].id;
        original.cards[0].kind = CardKind::Chat(source.clone());
        original
            .chat_drafts
            .insert(origin, "Canonical bytes".into());
        runtime.snapshot(Arc::new(original.clone()));
        runtime.flush(Duration::from_secs(5)).unwrap();
        let mut canonical = source.clone();
        if after_upgrade {
            canonical.proof.as_mut().unwrap().host_incarnation = "new".into();
            runtime
                .rebind_chat_upgrade(origin, source.clone(), canonical.clone())
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
        }
        let before = runtime
            .operation_store
            .load_card(origin, None)
            .unwrap()
            .unwrap();
        let mut card = original.cards[0].clone();
        card.id = CardId::new();
        let copy = crate::persistence::RetainedChatCopy {
            workspace: original.id,
            source_card: origin,
            source: source.clone(),
            copy: card,
            draft: "Saved draft  \nUnicode 🦀".into(),
            attachments: Vec::new(),
            draft_model: None,
            scope: None,
            channel: None,
        };
        // This newer shared actor is deliberately not an ordinary snapshot
        // identity change. Its refusal must not prevent copying local text.
        let mut refused = original.clone();
        let mut other = canonical.clone();
        other.id = TerminalId::new();
        other.launch.recipe.arguments.push("--other-recipe".into());
        refused.cards[0].kind = CardKind::Chat(other);
        runtime.snapshot_conversion(Arc::new(refused.clone()));
        runtime
            .preserve_chat_copy(copy.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime
                .operation_store
                .load_card(origin, None)
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            runtime
                .operation_store
                .load_workspace(original.id, None)
                .unwrap()
                .unwrap()
                .chat_drafts[&copy.copy.id],
            copy.draft
        );
        runtime.snapshot(Arc::new(refused));
        assert!(runtime.flush(Duration::from_secs(5)).is_err());
        assert!(
            runtime
                .operation_store
                .load_card(copy.copy.id, None)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        // A later stale autosave omits the newly retained copy. The writer
        // keeps the committed bytes, markers and navigation reachable.
        runtime.snapshot(Arc::new(original));
        runtime.flush(Duration::from_secs(5)).unwrap();
        let mut saved = runtime
            .operation_store
            .load_workspace(copy.workspace, None)
            .unwrap()
            .unwrap();
        assert_eq!(saved.chat_drafts[&copy.copy.id], copy.draft);
        assert_eq!(saved.pending_chat_admissions[&copy.copy.id], source);
        assert!(saved.local_only_cards.contains(&copy.copy.id));
        assert!(saved.card_scopes.contains_key(&copy.copy.id));
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == origin)
                .unwrap()
                .kind,
            CardKind::Chat(canonical.clone())
        );
        saved
            .cards
            .iter_mut()
            .find(|card| card.id == copy.copy.id)
            .unwrap()
            .existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("discarded retained text".into()),
            },
        };
        saved.chat_drafts.remove(&copy.copy.id);
        saved.pending_chat_admissions.remove(&copy.copy.id);
        saved.local_only_cards.remove(&copy.copy.id);
        saved.card_scopes.remove(&copy.copy.id);
        runtime.snapshot(Arc::new(saved));
        runtime.flush(Duration::from_secs(5)).unwrap();
        assert!(
            runtime
                .preserve_chat_copy(copy.clone())
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
        runtime.shutdown();
        let reopened = StateStore::open(&path, BootId::new("boot")).unwrap();
        let restored = reopened
            .load_workspace(copy.workspace, None)
            .unwrap()
            .unwrap();
        assert!(
            !restored
                .cards
                .iter()
                .find(|card| card.id == copy.copy.id)
                .unwrap()
                .existence
                .is_open()
        );
        assert!(!restored.chat_drafts.contains_key(&copy.copy.id));
    }
}

#[test]
fn only_a_refusal_of_the_content_itself_is_permanent() {
    let constraint = |extended_code| {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(extended_code),
            None,
        ))
    };
    for permanent in [
        invalid_state!("tombstoned card cannot be resurrected"),
        StoreError::Serialization(serde_json::from_str::<u8>("x").unwrap_err()),
        constraint(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
        constraint(rusqlite::ffi::SQLITE_CONSTRAINT_CHECK),
        constraint(rusqlite::ffi::SQLITE_TOOBIG),
        foreign_key_violation(),
        StoreError::Card {
            card: CardId::new(),
            error: Box::new(invalid_state!("invalid conversion")),
        },
    ] {
        assert!(permanent.refuses_content(), "{permanent:?}");
    }
    for transient in [
        StoreError::CorruptRow {
            field: "card existence".into(),
            reason: "malformed JSON".into(),
        },
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
        constraint(rusqlite::ffi::SQLITE_BUSY),
        constraint(rusqlite::ffi::SQLITE_READONLY),
        constraint(rusqlite::ffi::SQLITE_FULL),
        constraint(rusqlite::ffi::SQLITE_CORRUPT),
        StoreError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)),
        StoreError::StaleGeneration {
            entity: "workspace".into(),
            expected: 1,
            actual: 2,
        },
        StoreError::UnsupportedSchema {
            found: 99,
            supported: 1,
        },
        StoreError::Closed,
        StoreError::Card {
            card: CardId::new(),
            error: Box::new(constraint(rusqlite::ffi::SQLITE_BUSY)),
        },
    ] {
        assert!(!transient.refuses_content(), "{transient:?}");
    }
    assert_eq!(
        save_failure_category(&constraint(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)),
        SaveFailureCategory::InvalidData
    );
    assert_eq!(
        save_failure_category(&foreign_key_violation()),
        SaveFailureCategory::InvalidData
    );
}

#[test]
fn corrupt_stored_rows_keep_valid_edits_pending_for_recovery() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("corrupt-row.sqlite3");
    let store = StateStore::open(&path, BootId::new("corrupt-row")).unwrap();
    let mut state = workspace();
    let mut writes = empty_writes();
    writes.workspace = Some(Arc::new(state.clone()));
    assert!(writes.flush(&store).is_empty());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE cards SET existence_json = 'broken-json'", [])
        .unwrap();
    state.cards[0].title = "valid edit waiting for repair".into();
    writes.workspace = Some(Arc::new(state.clone()));
    let errors = writes.flush(&store);
    assert_eq!(errors.len(), 1);
    assert_eq!(
        save_failure_category(&errors[0]),
        SaveFailureCategory::Corrupt
    );
    assert!(!errors[0].refuses_content());
    assert!(writes.workspace.is_some(), "valid edits remain queued");
    assert!(
        writes.quarantine.workspace.is_none(),
        "corruption is not blamed on the edited card"
    );
    connection
        .execute(
            "UPDATE cards SET existence_json = ?1",
            [serde_json::to_string(&Existence::Open).unwrap()],
        )
        .unwrap();
    assert!(writes.flush(&store).is_empty());
    assert!(writes.workspace.is_none());
    assert_eq!(
        store.load_workspace(state.id, None).unwrap().unwrap().cards[0].title,
        state.cards[0].title
    );
    assert!(SaveFailureCategory::Corrupt.guidance().contains("repair"));
}

#[test]
fn missing_or_undecodable_stored_values_are_corruption_not_refused_content() {
    let bad_id = "not-a-uuid";
    for error in [
        StoreError::InvalidUuid {
            kind: "card",
            value: bad_id.into(),
            source: bad_id.parse::<uuid::Uuid>().unwrap_err(),
        },
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
        StoreError::Sqlite(rusqlite::Error::InvalidColumnType(
            0,
            "card_id".into(),
            rusqlite::types::Type::Blob,
        )),
    ] {
        assert!(!error.refuses_content());
        assert_eq!(save_failure_category(&error), SaveFailureCategory::Corrupt);
    }
}

/// #268: a write storage refuses by construction used to exhaust the
/// lane, after which nothing else the window produced was written.
#[test]
fn a_refused_catalog_write_is_set_aside_while_later_writes_land() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("refused-catalog.sqlite3");
    let boot = BootId::new("boot-refused-catalog");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let poisoned = project("poisoned");
    runtime.save_project(poisoned.clone());
    runtime.flush(timeout).unwrap();
    runtime.save_project(ProjectState {
        existence: closed(),
        ..poisoned.clone()
    });
    runtime.flush(timeout).unwrap();

    runtime.save_project(poisoned.clone());
    runtime
        .flush(timeout)
        .expect("a refused catalog row leaves the window state whole");
    let refused = latest_health(&health);
    assert_eq!(refused.health, SessionSaveHealth::Healthy);
    assert_eq!(refused.quarantined.len(), 1);
    assert_eq!(
        refused.quarantined[0].subject,
        WriteSubject::Project {
            id: poisoned.id,
            name: "poisoned".into(),
        }
    );
    assert_eq!(
        refused.quarantined[0].category,
        SaveFailureCategory::InvalidData
    );

    let unrelated = project("unrelated");
    let unrelated_worktree = worktree(&unrelated, "feature");
    let later = workspace();
    runtime.save_project(unrelated.clone());
    runtime.save_worktrees([unrelated_worktree.clone()]);
    runtime.snapshot(Arc::new(later.clone()));
    runtime.flush(timeout).unwrap();
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);

    let (mut reopened, restored) = SessionRuntime::open(&path, boot.clone()).unwrap();
    assert_eq!(restored.map(|state| state.id), Some(later.id));
    let (projects, worktrees) = reopened.take_catalog();
    assert!(projects.iter().any(|project| project.id == unrelated.id));
    assert!(
        projects
            .iter()
            .any(|project| project.id == poisoned.id && !project.existence.is_open()),
        "the refused resurrection never reached storage"
    );
    assert_eq!(
        worktrees
            .iter()
            .map(|worktree| worktree.id)
            .collect::<Vec<_>>(),
        vec![unrelated_worktree.id]
    );
    reopened.shutdown();
    let store = StateStore::open(&path, boot).unwrap();
    assert!(
        !store.recovery_report().dirty_shutdown,
        "a refused catalog row does not cost the session its clean shutdown"
    );
}

#[test]
fn a_worktree_waits_for_its_project_instead_of_failing_on_it() {
    let parent = project("parent");
    let child = worktree(&parent, "child");
    let store = CatalogStore::new(parent.id, [ProjectWrite::Busy]);
    let mut writes = empty_writes();
    writes.projects.insert(parent.id, parent.clone());
    writes.worktrees.insert(child.id, child.clone());

    let errors = writes.flush(&store);
    assert_eq!(errors.len(), 1, "only the project failed: {errors:?}");
    assert_eq!(save_failure_category(&errors[0]), SaveFailureCategory::Busy);
    assert!(
        !writes.is_settled(),
        "the project's retry keeps the lane busy"
    );
    assert!(
        writes.quarantine.is_empty(),
        "a busy database refuses nothing"
    );
    assert!(writes.orphaned_worktrees().is_empty());

    assert!(writes.flush(&store).is_empty());
    assert!(writes.is_settled());
    assert_eq!(store.saved_worktrees(), vec![child.id]);
    assert_eq!(
        store.missing_parents.load(Ordering::Relaxed),
        0,
        "the lane never offers a worktree ahead of its project"
    );
}

#[test]
fn only_the_worktrees_of_a_refused_project_are_held_back() {
    let refused = project("refused");
    let held = worktree(&refused, "held");
    let unrelated = project("unrelated");
    let landed = worktree(&unrelated, "landed");
    let store = CatalogStore::new(refused.id, [ProjectWrite::Refuse]);
    let mut writes = empty_writes();
    writes.projects.insert(refused.id, refused.clone());
    writes.worktrees.insert(held.id, held.clone());
    writes.projects.insert(unrelated.id, unrelated.clone());
    writes.worktrees.insert(landed.id, landed.clone());

    let errors = writes.flush(&store);
    assert!(errors.is_empty(), "a refusal is not a retry: {errors:?}");
    assert!(
        writes.is_settled(),
        "a held-back write neither retries nor exhausts the lane"
    );
    assert_eq!(
        writes.quarantined(),
        vec![QuarantinedWrite {
            subject: WriteSubject::Project {
                id: refused.id,
                name: "refused".into(),
            },
            category: SaveFailureCategory::InvalidData,
            check: crate::diagnostics::Check::new("tombstoned project … cannot be resurrected"),
            refused_at_unix_ms: writes.quarantine.projects[&refused.id].1.at_unix_ms,
            held_back: 1,
        }]
    );
    assert_eq!(store.saved_worktrees(), vec![landed.id]);
    assert_eq!(store.missing_parents.load(Ordering::Relaxed), 0);
    assert!(
        writes.orphaned_worktrees().is_empty(),
        "the refusal explains the held-back worktree"
    );

    // A newer write of the refused project supersedes the refusal and
    // releases the worktree it held back, in the same flush.
    writes.projects.insert(refused.id, refused.clone());
    assert!(writes.flush(&store).is_empty());
    assert!(writes.is_settled());
    assert!(writes.quarantine.is_empty());
    assert_eq!(store.saved_worktrees(), vec![landed.id, held.id]);
    assert_eq!(store.missing_parents.load(Ordering::Relaxed), 0);
}

#[test]
fn a_worktree_whose_project_was_never_queued_is_named() {
    let unsaved = project("unsaved");
    let orphan = worktree(&unsaved, "orphan");
    let store = CatalogStore::default();
    let mut writes = empty_writes();
    writes.worktrees.insert(orphan.id, orphan.clone());

    assert!(writes.flush(&store).is_empty());
    assert!(writes.is_settled());
    assert!(store.saved_worktrees().is_empty());
    assert_eq!(writes.orphaned_worktrees(), vec![orphan.id]);
}

#[test]
fn a_refused_update_of_a_stored_project_holds_nothing_back() {
    let parent = project("parent");
    let child = worktree(&parent, "child");
    let store = CatalogStore::new(parent.id, [ProjectWrite::Refuse]);
    store.stored_projects.lock().unwrap().insert(parent.id);
    let mut writes = PendingWrites::restored(
        0,
        BTreeMap::new(),
        HashMap::from([(parent.id, 1)]),
        HashMap::new(),
    );
    writes.projects.insert(parent.id, parent.clone());
    writes.worktrees.insert(child.id, child.clone());

    assert!(writes.flush(&store).is_empty());
    assert!(writes.is_settled());
    assert_eq!(store.saved_worktrees(), vec![child.id]);
    assert_eq!(writes.quarantined()[0].held_back, 0);
}

#[test]
fn a_refused_checkpoint_does_not_stop_the_snapshots_after_it() {
    let store = FakeStore::new([FakeWrite::Refuse, FakeWrite::Succeed, FakeWrite::Succeed]);
    let attempts = Arc::clone(&store.attempts);
    let mut writes = empty_writes();
    writes
        .conversions
        .push_back(ConversionCheckpoint::Snapshot(Arc::new(workspace())));
    writes
        .conversions
        .push_back(ConversionCheckpoint::Snapshot(Arc::new(workspace())));
    writes.workspace = Some(Arc::new(workspace()));

    assert!(writes.flush(&store).is_empty());
    assert_eq!(attempts.load(Ordering::Relaxed), 3);
    assert!(writes.is_settled());
    assert!(
        writes.quarantine.is_empty(),
        "a later snapshot that landed supersedes the refused one"
    );
}

#[test]
fn a_refused_autosave_stays_reported_until_a_later_one_lands() {
    let store = FakeStore::new([FakeWrite::Refuse, FakeWrite::Refuse, FakeWrite::Succeed]);
    let mut writes = empty_writes();
    writes.workspace = Some(Arc::new(workspace()));
    assert!(writes.flush(&store).is_empty());
    assert!(writes.is_settled());
    let first = writes.quarantined();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].subject, WriteSubject::Workspace { card: None });
    assert_eq!(
        writes.quarantine.window_state(),
        Some(SaveFailureCategory::InvalidData)
    );

    // The reader's retry offers the refused snapshot again; refused
    // again, it keeps the time storage first refused it.
    writes.rearm_quarantine();
    assert!(!writes.is_settled());
    assert!(writes.flush(&store).is_empty());
    assert_eq!(writes.quarantined(), first);

    writes.workspace = Some(Arc::new(workspace()));
    assert!(writes.flush(&store).is_empty());
    assert!(writes.quarantine.is_empty());
    assert_eq!(writes.quarantine.window_state(), None);
}

#[test]
fn a_refused_snapshot_fails_a_barrier_and_the_clean_shutdown() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("refused-workspace.sqlite3");
    let boot = BootId::new("boot-refused-workspace");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    let card = state.cards.remove(0);
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();

    // Bringing a closed card back is refused by construction. Storage
    // keeps its tombstone, and the rest of the window saves around it,
    // without focusing a closed card.
    let resurrected = card.id;
    state.cards.push(card);
    state.focused = Some(resurrected);
    state.chat_drafts.insert(resurrected, "first draft".into());
    runtime.snapshot(Arc::new(state.clone()));
    let refusal = runtime
        .flush(timeout)
        .expect_err("a barrier cannot vouch for a window storage refused");
    assert!(refusal.contains("refused part of the window"), "{refusal}");
    assert!(
        refusal.contains(&format!(
            "the latest changes to pane {resurrected} (refused by rule: tombstoned card … \
                 cannot be resurrected)."
        )),
        "a refused barrier names what the reader has to change, and the rule it broke: \
             {refusal}"
    );
    assert!(!refusal.contains("first draft"), "{refusal}");
    assert!(refusal.contains(QUARANTINE_GUIDANCE), "{refusal}");
    let refused = latest_health(&health);
    assert_eq!(refused.health, SessionSaveHealth::Healthy);
    assert_eq!(
        refused
            .quarantined
            .iter()
            .map(|write| &write.subject)
            .collect::<Vec<_>>(),
        vec![&WriteSubject::Pane { card: resurrected }]
    );
    let saved = runtime
        .operation_store()
        .load_workspace(state.id, None)
        .unwrap()
        .unwrap();
    assert_eq!(saved.focused, None);
    assert_eq!(
        saved.chat_drafts.get(&resurrected).map(String::as_str),
        Some("first draft")
    );
    assert!(matches!(
        saved
            .cards
            .iter()
            .find(|card| card.id == resurrected)
            .map(|card| &card.existence),
        Some(Existence::Closed { .. })
    ));

    // Later edits to the window keep saving while the pane stays refused,
    // and an explicit retry offers the pane again without clearing it.
    state.chat_drafts.insert(resurrected, "kept draft".into());
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap_err();
    assert!(runtime.retry_save());
    runtime.flush(timeout).unwrap_err();
    let saved = runtime
        .operation_store()
        .load_workspace(state.id, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.chat_drafts.get(&resurrected).map(String::as_str),
        Some("kept draft")
    );
    assert_eq!(
        latest_health(&health)
            .quarantined
            .iter()
            .map(|write| &write.subject)
            .collect::<Vec<_>>(),
        vec![&WriteSubject::Pane { card: resurrected }]
    );

    // The catalog still saves beside it.
    let unrelated = project("unrelated");
    runtime.save_project(unrelated.clone());
    runtime.flush(timeout).unwrap_err();
    runtime.shutdown();

    let store = StateStore::open(&path, boot).unwrap();
    assert!(
        store.recovery_report().dirty_shutdown,
        "storage does not hold the window the session ended with"
    );
    assert!(
        store
            .list_projects()
            .unwrap()
            .iter()
            .any(|project| project.id == unrelated.id)
    );
}

#[test]
fn ownership_recovery_unblocks_the_actual_update_flush_without_losing_unsaved_work() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("ownership.sqlite3");
    let boot = BootId::new("ownership-recovery");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    state.cards[0].existence = Existence::Closed {
        tombstone: Tombstone {
            operation_id: None,
            closed_at_unix_ms: 773,
            reason: Some("closed by user".into()),
        },
    };
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    let mut live = state.cards[0].clone();
    live.id = CardId::new();
    live.title = "Unsaved terminal".into();
    live.existence = Existence::Open;
    let CardKind::Terminal(terminal) = &live.kind else {
        unreachable!()
    };
    let recovery = crate::persistence::TerminalPaneAdoption {
        alias: state.cards[0].id,
        canonical: live.id,
        terminal: terminal.id,
    };
    state.focused = Some(live.id);
    state.chat_drafts.insert(live.id, "refused draft".into());
    state.cards.push(live);
    runtime.snapshot(Arc::new(state.clone()));
    assert!(
        runtime
            .flush(timeout)
            .unwrap_err()
            .contains("refused part of the window")
    );
    let refused = latest_health(&health).quarantined;
    assert_eq!(
        refused[0].subject,
        WriteSubject::Workspace {
            card: Some(recovery.canonical)
        }
    );
    assert!(runtime.retry_save());
    assert!(runtime.flush(timeout).is_err());
    assert_eq!(latest_health(&health).quarantined, refused);
    state.chat_drafts.insert(
        recovery.canonical,
        "latest unsaved draft\nkept exactly".into(),
    );
    let receipt = runtime
        .recover_terminal_ownership(Arc::new(state.clone()), recovery)
        .unwrap();
    receipt.recv_timeout(timeout).unwrap().unwrap();
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.shutdown();
    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
    let restored = store.load_workspace(state.id, None).unwrap().unwrap();
    assert_eq!(restored.chat_drafts, state.chat_drafts);
    assert_eq!(restored.focused, state.focused);
    assert_eq!(
        store.load_card(recovery.alias, None).unwrap().unwrap().kind,
        state.cards[0].kind
    );
    assert!(
        store
            .load_card(recovery.canonical, None)
            .unwrap()
            .unwrap()
            .existence
            .is_open()
    );
}

fn refused_ownership_writes(
    store: &StateStore,
) -> (
    PendingWrites,
    Arc<WorkspaceState>,
    crate::persistence::TerminalPaneAdoption,
) {
    let mut writes = empty_writes();
    let mut state = workspace();
    writes.workspace = Some(Arc::new(state.clone()));
    assert!(writes.flush(store).is_empty());
    state.cards[0].existence = Existence::Closed {
        tombstone: Tombstone {
            operation_id: None,
            closed_at_unix_ms: 773,
            reason: None,
        },
    };
    writes.workspace = Some(Arc::new(state.clone()));
    assert!(writes.flush(store).is_empty());
    let mut live = state.cards[0].clone();
    live.id = CardId::new();
    live.existence = Existence::Open;
    let CardKind::Terminal(terminal) = &live.kind else {
        unreachable!()
    };
    let recovery = crate::persistence::TerminalPaneAdoption {
        alias: state.cards[0].id,
        canonical: live.id,
        terminal: terminal.id,
    };
    state.focused = Some(live.id);
    state.cards.push(live);
    let snapshot = Arc::new(state);
    writes.workspace = Some(Arc::clone(&snapshot));
    assert!(writes.flush(store).is_empty());
    assert!(writes.quarantine.workspace.is_some());
    (writes, snapshot, recovery)
}

#[test]
fn ownership_recovery_retires_only_the_old_rearmed_snapshot_and_preserves_newer_work() {
    for newer in [false, true] {
        let directory = tempdir().unwrap();
        let store = StateStore::open(
            directory.path().join("state.sqlite3"),
            BootId::new("rearmed"),
        )
        .unwrap();
        let (mut writes, refused, recovery) = refused_ownership_writes(&store);
        writes.rearm_quarantine();
        let unrelated = project("unrelated refusal");
        writes
            .quarantine
            .refuse_project(unrelated, &invalid_state!("unrelated"));
        let mut reviewed = (*refused).clone();
        reviewed
            .chat_drafts
            .insert(recovery.canonical, "reviewed draft".into());
        if newer {
            let mut latest = reviewed.clone();
            latest.chat_drafts.insert(
                recovery.canonical,
                "newer draft arriving during recovery".into(),
            );
            writes.workspace = Some(Arc::new(latest));
        }
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        writes
            .conversions
            .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
                snapshot: Arc::new(reviewed),
                recovery,
                reply,
            });
        assert!(writes.flush(&store).is_empty());
        receipt.recv().unwrap().unwrap();
        assert!(writes.quarantine.workspace.is_none());
        assert_eq!(writes.quarantine.projects.len(), 1);
        let saved = store.load_workspace(refused.id, None).unwrap().unwrap();
        assert_eq!(
            saved.chat_drafts[&recovery.canonical],
            if newer {
                "newer draft arriving during recovery"
            } else {
                "reviewed draft"
            }
        );
    }
}

#[test]
fn failed_ownership_recovery_is_acknowledged_without_a_later_unreviewed_transfer() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let store = StateStore::open_with_options(
        &path,
        BootId::new("one-shot-recovery"),
        crate::persistence::StoreOptions {
            busy_timeout: Duration::from_millis(10),
        },
    )
    .unwrap();
    let (mut writes, snapshot, recovery) = refused_ownership_writes(&store);
    let refused = writes.quarantined();
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (reply, receipt) = std::sync::mpsc::sync_channel(1);
    writes
        .conversions
        .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
            snapshot: Arc::clone(&snapshot),
            recovery,
            reply,
        });
    assert!(!writes.flush(&store).is_empty());
    assert!(receipt.recv().unwrap().is_err());
    assert_eq!(writes.quarantined(), refused);
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(writes.flush(&store).is_empty());
    assert_eq!(writes.quarantined(), refused);
    assert!(store.load_card(recovery.canonical, None).unwrap().is_none());
    let (reply, receipt) = std::sync::mpsc::sync_channel(1);
    writes
        .conversions
        .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
            snapshot,
            recovery,
            reply,
        });
    assert!(writes.flush(&store).is_empty());
    receipt.recv().unwrap().unwrap();
    assert!(writes.quarantine.workspace.is_none());
}

#[test]
fn an_earlier_io_failure_rejects_queued_recovery_but_retains_its_current_work() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("queued-recovery.sqlite3");
    let store = StateStore::open_with_options(
        &path,
        BootId::new("queued-recovery"),
        crate::persistence::StoreOptions {
            busy_timeout: Duration::from_millis(10),
        },
    )
    .unwrap();
    let (mut writes, refused, recovery) = refused_ownership_writes(&store);
    let mut current = (*refused).clone();
    current
        .chat_drafts
        .insert(recovery.canonical, "new work captured by the review".into());
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    writes
        .conversions
        .push_back(ConversionCheckpoint::Snapshot(Arc::clone(&refused)));
    let (reply, receipt) = std::sync::mpsc::sync_channel(1);
    writes
        .conversions
        .push_back(ConversionCheckpoint::TerminalOwnershipRecovery {
            snapshot: Arc::new(current),
            recovery,
            reply,
        });
    assert!(!writes.flush(&store).is_empty());
    assert!(
        receipt
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .contains("earlier session write failed")
    );
    assert!(writes.conversions.iter().all(|checkpoint| !matches!(
        checkpoint,
        ConversionCheckpoint::TerminalOwnershipRecovery { .. }
    )));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(writes.flush(&store).is_empty());
    assert!(store.load_card(recovery.canonical, None).unwrap().is_none());
    let (latest, card, _) = writes.quarantine.workspace.as_ref().unwrap();
    assert_eq!(*card, Some(recovery.canonical));
    assert_eq!(
        latest.chat_drafts[&recovery.canonical],
        "new work captured by the review"
    );
}

#[test]
fn writer_commits_canonical_terminal_adoption_and_later_snapshots() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("canonical-adoption.sqlite3");
    let boot = BootId::new("boot-canonical-adoption");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    let alias = state.cards[0].id;
    let CardKind::Terminal(session) = &state.cards[0].kind else {
        panic!("terminal fixture")
    };
    let terminal = session.id;
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();

    let mut canonical = state.cards[0].clone();
    canonical.id = CardId::new();
    let canonical_id = canonical.id;
    state.cards[0].existence = closed();
    state.cards.push(canonical);
    state.focused = Some(canonical_id);
    runtime.snapshot_terminal_adoptions(
        Arc::new(state.clone()),
        vec![crate::persistence::TerminalPaneAdoption {
            alias,
            canonical: canonical_id,
            terminal,
        }],
    );
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());

    state.cards[1].title = "canonical shell".into();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    runtime.shutdown();
    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
    let restored = store.load_workspace(state.id, None).unwrap().unwrap();
    assert_eq!(restored.focused, Some(canonical_id));
    assert!(restored.cards.iter().any(|card| card.id == canonical_id
        && card.title == "canonical shell"
        && card.existence.is_open()));
    assert!(
        !store
            .load_card(alias, None)
            .unwrap()
            .unwrap()
            .existence
            .is_open()
    );
}

#[test]
fn writer_resolves_terminal_restoration_after_pending_closure_and_before_later_snapshots() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("canonical-restoration.sqlite3");
    let boot = BootId::new("boot-canonical-restoration");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    let alias = state.cards[0].id;
    let CardKind::Terminal(session) = &state.cards[0].kind else {
        panic!("terminal fixture")
    };
    let terminal = session.id;
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();

    let mut canonical = state.cards[0].clone();
    canonical.id = CardId::new();
    let canonical_id = canonical.id;
    state.cards[0].existence = closed();
    state.focused = None;
    // This close may still be coalesced when restoration arrives. The
    // checkpoint must commit it before resolving the actual recipe owner.
    runtime.snapshot(Arc::new(state.clone()));
    state.cards.push(canonical);
    state.focused = Some(canonical_id);
    let restoration = crate::persistence::TerminalPaneRestoration {
        canonical: canonical_id,
        terminal,
    };
    runtime.snapshot_terminal_panes(Arc::new(state.clone()), Vec::new(), vec![restoration]);
    state.cards[1].title = "canonical shell".into();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    // Repeated model observations are idempotent even when the historical
    // closed alias still exists in the snapshot.
    runtime.snapshot_terminal_panes(
        Arc::new(state.clone()),
        Vec::new(),
        vec![restoration, restoration],
    );
    runtime.flush(timeout).unwrap();
    // A successful idempotent checkpoint does not change save health, so
    // the lane need not publish another event. The barrier still refuses
    // quarantined writes and errors, and disk assertions below verify it.
    assert!(runtime.last_error().is_none());
    while let Ok(event) = health.try_recv() {
        assert_eq!(event.health, SessionSaveHealth::Healthy);
        assert!(event.quarantined.is_empty());
    }
    runtime.shutdown();
    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
    let restored = store.load_workspace(state.id, None).unwrap().unwrap();
    assert_eq!(restored.focused, Some(canonical_id));
    assert!(restored.cards.iter().any(|card| card.id == canonical_id
        && card.title == "canonical shell"
        && card.existence.is_open()));
    assert!(
        !store
            .load_card(alias, None)
            .unwrap()
            .unwrap()
            .existence
            .is_open()
    );
}

#[test]
fn a_later_snapshot_supersedes_a_refused_one() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("superseded-workspace.sqlite3");
    let boot = BootId::new("boot-superseded-workspace");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    let card = state.cards.remove(0);
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    state.cards.push(card);
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap_err();

    state.cards.clear();
    state.focused = None;
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.shutdown();

    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
}

/// A barrier refuses for what is unsaved now, not for what was once
/// refused: the refused snapshot is the whole window, so the first later
/// one storage accepts is everything the barrier vouches for. An update is
/// such a barrier, and one it blocks for good cannot deliver the fix.
#[test]
fn a_barrier_passes_once_the_window_saves_after_a_refused_focus() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("refused-focus.sqlite3");
    let boot = BootId::new("boot-refused-focus");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    state.focused = Some(state.cards[0].id);
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();

    // Storage refuses a window that focuses a pane it holds closed.
    state.cards[0].existence = closed();
    runtime.snapshot(Arc::new(state.clone()));
    let refusal = runtime.flush(timeout).unwrap_err();
    assert!(
        refusal.contains(
            "(saved state is invalid): the window's layout and panes (refused by rule: \
                 workspace … focuses missing or closed card …)."
        ),
        "{refusal}"
    );
    // Asking again changes nothing: the same window is still unsaved.
    runtime.flush(timeout).unwrap_err();

    state.focused = None;
    runtime.snapshot(Arc::new(state));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.flush(timeout).unwrap();
    runtime.shutdown();

    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
}

/// Private diagnostics keep which subject storage refused, by identifier,
/// and the rule it broke - never the content the full error names.
#[test]
fn a_refusal_is_recorded_by_subject_and_rule_without_its_content() {
    let project = project("private-client");
    let worktree = worktree(&project, "secret-branch");
    let card = CardId::new();
    let cases = [
        (
            project_subject(&project),
            invalid_state!(
                "project {} at {} cannot be saved",
                project.name,
                project.repository.display()
            ),
            "project",
            Some(project.id.as_uuid()),
            "project … at … cannot be saved",
        ),
        (
            worktree_subject(&worktree),
            invalid_state!(
                "worktree {} on {:?} is invalid",
                worktree.path.display(),
                worktree.branch
            ),
            "worktree",
            Some(worktree.id.as_uuid()),
            "worktree … on … is invalid",
        ),
        (
            WriteSubject::Pane { card },
            StoreError::Card {
                card,
                error: Box::new(invalid_state!("chat card {card} changed source identity")),
            },
            "pane",
            Some(card.as_uuid()),
            "chat card … changed source identity",
        ),
        (
            WriteSubject::CardIdentity {
                card,
                kind: ReplacedIdentity::Chat,
            },
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
                Some("UNIQUE constraint failed: cards.terminal_id".into()),
            )),
            "new_conversation",
            Some(card.as_uuid()),
            "a SQLite UNIQUE constraint on cards.terminal_id",
        ),
        (
            WriteSubject::Workspace { card: None },
            StoreError::Serialization(
                serde_json::to_string(&HashMap::from([((1, 2), "/home/someone")])).unwrap_err(),
            ),
            "workspace",
            None,
            "the state could not be encoded as JSON",
        ),
    ];
    for (subject, error, write, id, check) in cases {
        let refusal = Refusal::new(&error);
        let event = refusal_event(&subject, &refusal);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "storage_refused",
                "write": write,
                "id": id,
                "check": check,
            }),
            "{error}"
        );
        let text = json.to_string();
        for private in ["private-client", "secret-branch", "/repo", "/home/someone"] {
            assert!(!text.contains(private), "{text} leaks {private}");
        }
    }
}

/// A subject refused again keeps the time of its first refusal, and the
/// lane can still tell that the reason it is refused for has changed.
#[test]
fn a_refusal_for_a_new_reason_is_told_apart_from_a_repeated_one() {
    let refused = |reason: &'static str| {
        StoreError::InvalidState(crate::persistence::Violation::new(reason, reason))
    };
    let mut quarantine = Quarantine::default();
    quarantine.refuse_workspace(Arc::new(workspace()), &refused("first"));
    let (_, _, first) = quarantine.workspace.clone().unwrap();
    quarantine.refuse_workspace(Arc::new(workspace()), &refused("first"));
    let (_, _, repeated) = quarantine.workspace.clone().unwrap();
    quarantine.refuse_workspace(Arc::new(workspace()), &refused("second"));
    let (_, _, changed) = quarantine.workspace.clone().unwrap();

    assert_eq!(repeated.cause, first.cause);
    assert_ne!(changed.cause, first.cause);
    assert_eq!(changed.at_unix_ms, first.at_unix_ms);
    assert_eq!(
        (first.check.as_str(), changed.check.as_str()),
        ("first", "second"),
        "the reported rule follows the newest refusal"
    );

    // Diagnostics record a subject once per rule: the same rule over
    // other values is logged as a new cause but not recorded again.
    let card = CardId::new();
    let pane = |generation: i64| {
        Refusal::new(&invalid_state!(
            "card {card} expected generation {generation}"
        ))
    };
    let (earlier, later) = (pane(1), pane(2));
    assert_ne!(earlier.cause, later.cause);
    assert!(names_a_new_rule(None, &earlier));
    assert!(!names_a_new_rule(Some(&earlier), &later));
    assert!(names_a_new_rule(Some(&first), &changed));
    assert!(!names_a_new_rule(Some(&first), &repeated));
}

/// A relaunch storage refused leaves the stored identity where it was, and
/// the next relaunch continues from the refused one's identity. Without the
/// merge the second replacement could only be refused for expecting an
/// identity storage never held, and every snapshot carrying it after that.
#[test]
fn a_relaunch_after_a_refused_one_starts_from_the_stored_identity() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("refused-relaunch.sqlite3");
    let boot = BootId::new("boot-refused-relaunch");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    let card_id = state.cards[0].id;
    let CardKind::Terminal(original) = state.cards[0].kind.clone() else {
        unreachable!()
    };
    let relaunch = |id| TerminalSession {
        owned_codex_listener: None,
        id,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: original.launch.clone(),
            last_exit: None,
        },
        ..original.clone()
    };

    // Storage refuses a relaunch whose recipe does not match its launch.
    let mut refused = relaunch(TerminalId::new());
    refused.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: LaunchRecipe {
            program: "/bin/false".into(),
            ..original.launch.clone()
        },
        last_exit: None,
    };
    runtime.replace_terminal_identity(card_id, original.id, refused.clone());
    runtime.flush(timeout).unwrap_err();
    assert_eq!(
        latest_health(&health)
            .quarantined
            .iter()
            .map(|write| &write.subject)
            .collect::<Vec<_>>(),
        vec![&WriteSubject::CardIdentity {
            card: card_id,
            kind: ReplacedIdentity::Terminal,
        }]
    );

    let latest = relaunch(TerminalId::new());
    runtime.replace_terminal_identity(card_id, refused.id, latest.clone());
    state.cards[0].kind = CardKind::Terminal(latest.clone());
    runtime.snapshot(Arc::new(state));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.shutdown();

    let (mut reopened, restored) = SessionRuntime::open(&path, boot).unwrap();
    assert_eq!(restored.unwrap().cards[0].kind, CardKind::Terminal(latest));
    reopened.shutdown();
}

#[test]
fn closing_a_card_drops_its_refused_relaunch() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("closed-refused-relaunch.sqlite3");
    let boot = BootId::new("boot-closed-refused-relaunch");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();
    let card_id = state.cards[0].id;
    let CardKind::Terminal(original) = state.cards[0].kind.clone() else {
        unreachable!()
    };
    // Storage refuses a relaunch whose recipe does not match its launch.
    let refused = TerminalSession {
        owned_codex_listener: None,
        id: TerminalId::new(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: LaunchRecipe {
                program: "/bin/false".into(),
                ..original.launch.clone()
            },
            last_exit: None,
        },
        ..original.clone()
    };
    runtime.replace_terminal_identity(card_id, original.id, refused.clone());
    runtime.flush(timeout).unwrap_err();

    // A snapshot that still holds the card at the identity storage kept
    // lands, and the relaunch it never took stays refused.
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap_err();
    assert_eq!(
        latest_health(&health)
            .quarantined
            .iter()
            .map(|write| &write.subject)
            .collect::<Vec<_>>(),
        vec![&WriteSubject::CardIdentity {
            card: card_id,
            kind: ReplacedIdentity::Terminal,
        }]
    );

    // The window moves on to the relaunch, and storage refuses that pane
    // of every snapshot that rebinds the card to it.
    state.cards[0].kind = CardKind::Terminal(refused);
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap_err();
    assert_eq!(
        latest_health(&health)
            .quarantined
            .iter()
            .map(|write| &write.subject)
            .collect::<Vec<_>>(),
        vec![
            &WriteSubject::Pane { card: card_id },
            &WriteSubject::CardIdentity {
                card: card_id,
                kind: ReplacedIdentity::Terminal,
            },
        ]
    );

    // Closing the card, as the notice says, stores its tombstone over the
    // identity storage kept, and leaves no identity to replace.
    state.cards[0].existence = closed();
    runtime.snapshot(Arc::new(state));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.shutdown();

    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
    let stored = store.load_card(card_id, None).unwrap().unwrap();
    assert!(!stored.existence.is_open());
    assert!(matches!(stored.kind, CardKind::Terminal(session) if session.id == original.id));
}

#[test]
fn a_card_first_saved_at_its_new_identity_drops_its_refused_relaunch() {
    let directory = tempdir().unwrap();
    let path = directory
        .path()
        .join("first-saved-refused-relaunch.sqlite3");
    let boot = BootId::new("boot-first-saved-refused-relaunch");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    let card_id = state.cards[0].id;
    let CardKind::Terminal(original) = state.cards[0].kind.clone() else {
        unreachable!()
    };
    // The terminal exits before its first snapshot, and storage refuses
    // the relaunch's content before it ever looks for the card.
    let fresh_id = TerminalId::new();
    runtime.replace_terminal_identity(
        card_id,
        original.id,
        TerminalSession {
            owned_codex_listener: None,
            id: fresh_id,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: LaunchRecipe {
                    program: "/bin/false".into(),
                    ..original.launch.clone()
                },
                last_exit: None,
            },
            ..original.clone()
        },
    );
    runtime.flush(timeout).unwrap_err();
    assert_eq!(latest_health(&health).quarantined.len(), 1);

    // The first snapshot stores the card at the fresh identity, which
    // leaves the refused relaunch nothing to replace.
    state.cards[0].kind = CardKind::Terminal(TerminalSession {
        owned_codex_listener: None,
        id: fresh_id,
        ..original
    });
    runtime.snapshot(Arc::new(state));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.shutdown();

    let store = StateStore::open(&path, boot).unwrap();
    assert!(!store.recovery_report().dirty_shutdown);
}

#[test]
fn closing_a_chat_card_drops_its_refused_conversation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("closed-refused-conversation.sqlite3");
    let boot = BootId::new("boot-closed-refused-conversation");
    let timeout = Duration::from_secs(5);
    let (mut runtime, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let health = runtime.health_receiver();
    let mut state = workspace();
    let card = state.cards[0].id;
    let CardKind::Terminal(terminal) = state.cards[0].kind.clone() else {
        unreachable!()
    };
    let chat = |session_id: Option<&str>| crate::chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: LaunchRecipe {
                program: crate::agent::Provider::Codex.executable().into(),
                arguments: Vec::new(),
                ..terminal.launch.clone()
            },
            session_id: session_id.map(str::to_owned),
        },
        proof: None,
        terminal: None,
    };
    let original = chat(None);
    state.cards[0].kind = CardKind::Chat(original.clone());
    runtime.snapshot(Arc::new(state.clone()));
    runtime.flush(timeout).unwrap();

    // Storage refuses a replacement conversation that has already begun.
    // The window still shows the conversation storage holds.
    runtime.replace_chat_identity(
        card,
        original.id,
        chat(Some("begun")),
        Arc::new(state.clone()),
    );
    runtime.flush(timeout).unwrap_err();
    assert_eq!(
        latest_health(&health)
            .quarantined
            .iter()
            .map(|write| &write.subject)
            .collect::<Vec<_>>(),
        vec![&WriteSubject::CardIdentity {
            card,
            kind: ReplacedIdentity::Chat,
        }]
    );

    state.cards[0].existence = closed();
    runtime.snapshot(Arc::new(state));
    runtime.flush(timeout).unwrap();
    assert!(latest_health(&health).quarantined.is_empty());
    runtime.shutdown();
}

#[test]
fn a_retry_storage_refuses_again_is_still_answered() {
    let store = FakeStore::always(FakeWrite::Refuse);
    let attempts = Arc::clone(&store.attempts);
    let refused = project("refused");
    let queue = Arc::new((
        Mutex::new(QueueState {
            projects: HashMap::from([(refused.id, refused)]),
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let (health_tx, health_rx) = async_channel::unbounded();
    let health = HealthPublisher::new(health_tx);
    let worker_queue = Arc::clone(&queue);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            store,
            empty_writes(),
            worker_queue,
            Arc::new(Mutex::new(None)),
            health,
            fast_persistence_config(2),
        );
    });
    let quarantined = health_until(&health_rx, |event| !event.quarantined.is_empty());

    let (lock, changed) = &*queue;
    lock.lock().unwrap().retry_requested = true;
    changed.notify_one();
    // The reader's notice waits on this event to offer a retry again.
    let answer = health_until(&health_rx, |event| event.revision > quarantined.revision);
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert_eq!(answer.health, SessionSaveHealth::Healthy);
    assert_eq!(answer.quarantined, quarantined.quarantined);

    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
}

#[test]
fn shutdown_skips_refused_writes() {
    for (window_state, clean) in [(false, true), (true, false)] {
        let store = FakeStore::always(FakeWrite::Refuse);
        let attempts = Arc::clone(&store.attempts);
        let shutdowns = Arc::clone(&store.shutdowns);
        let queue = Arc::new((
            Mutex::new(QueueState {
                latest: window_state.then(|| Arc::new(workspace())),
                projects: HashMap::from_iter((!window_state).then(|| {
                    let project = project("refused");
                    (project.id, project)
                })),
                shutdown: true,
                ..QueueState::default()
            }),
            Condvar::new(),
        ));
        crate::test_timing::returns("a shutdown with refused writes", move || {
            let (health_tx, _health_rx) = async_channel::unbounded();
            let mut health = HealthPublisher::new(health_tx);
            persistence_loop(
                store,
                empty_writes(),
                queue,
                Arc::new(Mutex::new(None)),
                &mut health,
                PersistenceConfig {
                    // Far longer than the test waits, so spending it fails.
                    shutdown_flush_budget: Duration::from_secs(3600),
                    ..fast_persistence_config(2)
                },
            );
        });
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a refused write is offered once, not retried until the budget runs out"
        );
        assert_eq!(shutdowns.load(Ordering::Relaxed), usize::from(clean));
    }
}

#[test]
fn a_retry_offers_refused_writes_to_storage_again() {
    let store = FakeStore::new([FakeWrite::Refuse, FakeWrite::Succeed]);
    let attempts = Arc::clone(&store.attempts);
    let refused = project("refused");
    let queue = Arc::new((
        Mutex::new(QueueState {
            projects: HashMap::from([(refused.id, refused.clone())]),
            ..QueueState::default()
        }),
        Condvar::new(),
    ));
    let (health_tx, health_rx) = async_channel::unbounded();
    let health = HealthPublisher::new(health_tx);
    let worker_queue = Arc::clone(&queue);
    let worker = thread::spawn(move || {
        run_persistence_worker(
            store,
            empty_writes(),
            worker_queue,
            Arc::new(Mutex::new(None)),
            health,
            fast_persistence_config(2),
        );
    });

    let quarantined = health_until(&health_rx, |event| !event.quarantined.is_empty());
    assert_eq!(quarantined.health, SessionSaveHealth::Healthy);
    // Nothing retries a refusal on its own.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);

    let (lock, changed) = &*queue;
    lock.lock().unwrap().retry_requested = true;
    changed.notify_one();
    let cleared = health_until(&health_rx, |event| event.quarantined.is_empty());
    assert_eq!(cleared.health, SessionSaveHealth::Healthy);
    assert_eq!(attempts.load(Ordering::Relaxed), 2);

    lock.lock().unwrap().shutdown = true;
    changed.notify_one();
    worker.join().unwrap();
}

#[test]
fn a_quarantine_change_never_repeats_a_recovery() {
    let (health_tx, health_rx) = async_channel::unbounded();
    let mut publisher = HealthPublisher::new(health_tx);
    publisher.failed(&[StoreError::Closed], false);
    publisher.flushed();
    let _ = latest_health(&health_rx);

    let refused = QuarantinedWrite {
        subject: WriteSubject::Workspace { card: None },
        category: SaveFailureCategory::InvalidData,
        check: crate::diagnostics::Check::new("injected refusal"),
        refused_at_unix_ms: 1,
        held_back: 0,
    };
    publisher.quarantine(vec![refused.clone()]);
    let event = latest_health(&health_rx);
    assert_eq!(event.health, SessionSaveHealth::Healthy);
    assert_eq!(event.quarantined, vec![refused.clone()]);

    publisher.quarantine(vec![refused]);
    assert!(
        health_rx.try_recv().is_err(),
        "an unchanged quarantine publishes nothing"
    );
}
