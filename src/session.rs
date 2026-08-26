use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use directories::ProjectDirs;
use thiserror::Error;

use crate::{
    model::{
        BootId, CardId, ProjectId, ProjectState, TerminalId, TerminalSession, WorkspaceState,
        WorktreeId, WorktreeState,
    },
    persistence::{SnapshotBase, StateStore, StoreError},
};

const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);
const PERSIST_RETRY_INITIAL: Duration = Duration::from_millis(100);
const PERSIST_RETRY_MAX: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("cagents data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("failed to read Linux boot identifier: {0}")]
    BootId(std::io::Error),
    #[error("session storage failed: {0}")]
    Store(#[from] StoreError),
    #[error("session persistence worker could not start: {0}")]
    Worker(std::io::Error),
}

#[derive(Debug, Clone)]
struct TerminalIdentityReplacement {
    card_id: CardId,
    expected_terminal_id: TerminalId,
    replacement: TerminalSession,
}

fn merge_terminal_replacement(
    replacements: &mut HashMap<CardId, TerminalIdentityReplacement>,
    incoming: TerminalIdentityReplacement,
) {
    match replacements.entry(incoming.card_id) {
        std::collections::hash_map::Entry::Occupied(mut entry)
            if entry.get().replacement.id == incoming.expected_terminal_id =>
        {
            entry.get_mut().replacement = incoming.replacement;
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.insert(incoming);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(incoming);
        }
    }
}

#[derive(Debug, Default)]
struct QueueState {
    /// Snapshots travel as a shared pointer: the UI thread hands over a
    /// finished `WorkspaceState` and the writer thread reads it in place, so a
    /// change costs one `Arc` bump on this lane rather than a second deep clone
    /// of every card, title, and browser history.
    latest: Option<Arc<WorkspaceState>>,
    projects: HashMap<ProjectId, ProjectState>,
    worktrees: HashMap<WorktreeId, WorktreeState>,
    terminal_replacements: HashMap<CardId, TerminalIdentityReplacement>,
    shutdown: bool,
}

#[derive(Debug)]
pub struct SessionRuntime {
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    last_error: Arc<Mutex<Option<String>>>,
    worker: Option<thread::JoinHandle<()>>,
    database_path: PathBuf,
    restored_projects: Vec<ProjectState>,
    restored_worktrees: Vec<WorktreeState>,
}

impl SessionRuntime {
    pub fn open_default() -> Result<(Self, Option<WorkspaceState>), SessionError> {
        let project_dirs = ProjectDirs::from("dev", "cagents", "cagents")
            .ok_or(SessionError::DataDirectoryUnavailable)?;
        let database_path = project_dirs.data_dir().join("sessions.sqlite3");
        Self::open(database_path, current_boot_id()?)
    }

    pub fn open(
        database_path: impl AsRef<Path>,
        boot_id: BootId,
    ) -> Result<(Self, Option<WorkspaceState>), SessionError> {
        let database_path = database_path.as_ref().to_path_buf();
        let store = StateStore::open(&database_path, boot_id)?;
        let restored = store
            .list_workspaces()?
            .into_iter()
            .find(|summary| summary.existence.is_open())
            .map(|summary| store.load_workspace(summary.id, None))
            .transpose()?
            .flatten();
        let projects = store.list_projects()?;
        let worktrees = store.list_worktrees()?;
        let persisted_generation = restored.as_ref().map_or(0, |state| state.generation);
        let card_generations = restored
            .as_ref()
            .map(|state| {
                state
                    .cards
                    .iter()
                    .map(|card| (card.id, card.generation))
                    .collect()
            })
            .unwrap_or_default();
        let project_generations = projects
            .iter()
            .map(|project| (project.id, project.generation))
            .collect();
        let worktree_generations = worktrees
            .iter()
            .map(|worktree| (worktree.id, worktree.generation))
            .collect();

        let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
        let last_error = Arc::new(Mutex::new(None));
        let worker_queue = Arc::clone(&queue);
        let worker_error = Arc::clone(&last_error);
        let worker = thread::Builder::new()
            .name("cagents-session-writer".into())
            .spawn(move || {
                persistence_loop(
                    store,
                    persisted_generation,
                    card_generations,
                    project_generations,
                    worktree_generations,
                    worker_queue,
                    worker_error,
                )
            })
            .map_err(SessionError::Worker)?;

        Ok((
            Self {
                queue,
                last_error,
                worker: Some(worker),
                database_path,
                restored_projects: projects,
                restored_worktrees: worktrees,
            },
            restored,
        ))
    }

    /// Replaces any uncommitted snapshot. This keeps mutations non-blocking and
    /// bounds memory while preserving the newest complete state.
    ///
    /// The snapshot is shared, not copied: the writer thread reads it through
    /// the `Arc` and never mutates it, which is why the storage generations it
    /// writes against live in the writer thread rather than in the snapshot.
    pub fn snapshot(&self, state: Arc<WorkspaceState>) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.latest = Some(state);
            changed.notify_one();
        }
    }

    pub fn save_project(&self, project: ProjectState) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            queue.projects.insert(project.id, project);
            changed.notify_one();
        }
    }

    pub fn save_worktrees(&self, worktrees: impl IntoIterator<Item = WorktreeState>) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            for worktree in worktrees {
                queue.worktrees.insert(worktree.id, worktree);
            }
            changed.notify_one();
        }
    }

    pub fn replace_terminal_identity(
        &self,
        card_id: CardId,
        expected_terminal_id: TerminalId,
        replacement: TerminalSession,
    ) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock()
            && !queue.shutdown
        {
            merge_terminal_replacement(
                &mut queue.terminal_replacements,
                TerminalIdentityReplacement {
                    card_id,
                    expected_terminal_id,
                    replacement,
                },
            );
            changed.notify_one();
        }
    }

    pub fn take_catalog(&mut self) -> (Vec<ProjectState>, Vec<WorktreeState>) {
        (
            std::mem::take(&mut self.restored_projects),
            std::mem::take(&mut self.restored_worktrees),
        )
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|error| error.clone())
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn shutdown(&mut self) {
        let (lock, changed) = &*self.queue;
        if let Ok(mut queue) = lock.lock() {
            queue.shutdown = true;
            changed.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn persistence_loop(
    store: StateStore,
    mut persisted_generation: u64,
    mut card_generations: BTreeMap<CardId, u64>,
    mut project_generations: HashMap<ProjectId, u64>,
    mut worktree_generations: HashMap<WorktreeId, u64>,
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    last_error: Arc<Mutex<Option<String>>>,
) {
    let mut pending: Option<Arc<WorkspaceState>> = None;
    let mut pending_projects = HashMap::<ProjectId, ProjectState>::new();
    let mut pending_worktrees = HashMap::<WorktreeId, WorktreeState>::new();
    let mut pending_terminal_replacements = HashMap::<CardId, TerminalIdentityReplacement>::new();
    let mut due: Option<Instant> = None;
    let mut retry_delay = PERSIST_RETRY_INITIAL;

    loop {
        let (lock, changed) = &*queue;
        let mut state = match lock.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        while state.latest.is_none()
            && state.projects.is_empty()
            && state.worktrees.is_empty()
            && state.terminal_replacements.is_empty()
            && !state.shutdown
        {
            state = if let Some(deadline) = due {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match changed.wait_timeout(state, timeout) {
                    Ok((next, result)) => {
                        state = next;
                        if result.timed_out() {
                            break;
                        }
                        continue;
                    }
                    Err(_) => return,
                }
            } else {
                match changed.wait(state) {
                    Ok(next) => next,
                    Err(_) => return,
                }
            };
        }

        let received_changes = state.latest.is_some()
            || !state.projects.is_empty()
            || !state.worktrees.is_empty()
            || !state.terminal_replacements.is_empty();
        if let Some(latest) = state.latest.take() {
            pending = Some(latest);
        }
        pending_projects.extend(std::mem::take(&mut state.projects));
        pending_worktrees.extend(std::mem::take(&mut state.worktrees));
        for replacement in std::mem::take(&mut state.terminal_replacements).into_values() {
            merge_terminal_replacement(&mut pending_terminal_replacements, replacement);
        }
        if received_changes {
            due = Some(Instant::now() + PERSIST_DEBOUNCE);
        }
        let shutdown = state.shutdown;
        let should_persist = shutdown || due.is_some_and(|deadline| Instant::now() >= deadline);
        drop(state);

        if should_persist {
            let mut batch_errors = Vec::new();
            let replacement_cards = pending_terminal_replacements
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for card_id in replacement_cards {
                let replacement = &pending_terminal_replacements[&card_id];
                match store.replace_terminal_identity(
                    replacement.card_id,
                    replacement.expected_terminal_id,
                    &replacement.replacement,
                ) {
                    Ok(()) => {
                        pending_terminal_replacements.remove(&card_id);
                    }
                    Err(error) => batch_errors.push(error.to_string()),
                }
            }

            if pending_terminal_replacements.is_empty()
                && let Some(snapshot) = pending.as_deref()
            {
                match store.save_workspace(
                    snapshot,
                    SnapshotBase {
                        workspace: persisted_generation,
                        cards: &card_generations,
                    },
                ) {
                    Ok(commit) => {
                        persisted_generation = commit.generation;
                        card_generations = commit.card_generations;
                        pending = None;
                    }
                    Err(error) => batch_errors.push(error.to_string()),
                }
            }

            let project_ids = pending_projects.keys().copied().collect::<Vec<_>>();
            for id in project_ids {
                let mut project = pending_projects[&id].clone();
                project.generation = project_generations.get(&id).copied().unwrap_or(0);
                match store.save_project(&project) {
                    Ok(generation) => {
                        project_generations.insert(id, generation);
                        pending_projects.remove(&id);
                    }
                    Err(error) => batch_errors.push(error.to_string()),
                }
            }

            let worktree_ids = pending_worktrees.keys().copied().collect::<Vec<_>>();
            for id in worktree_ids {
                let mut worktree = pending_worktrees[&id].clone();
                worktree.generation = worktree_generations.get(&id).copied().unwrap_or(0);
                match store.save_worktree(&worktree) {
                    Ok(generation) => {
                        worktree_generations.insert(id, generation);
                        pending_worktrees.remove(&id);
                    }
                    Err(error) => batch_errors.push(error.to_string()),
                }
            }

            let has_pending = pending.is_some()
                || !pending_projects.is_empty()
                || !pending_worktrees.is_empty()
                || !pending_terminal_replacements.is_empty();
            if let Ok(mut error) = last_error.lock() {
                *error = (!batch_errors.is_empty()).then(|| batch_errors.join("; "));
            }
            if has_pending {
                due = Some(Instant::now() + retry_delay);
                retry_delay = retry_delay.saturating_mul(2).min(PERSIST_RETRY_MAX);
            } else {
                due = None;
                retry_delay = PERSIST_RETRY_INITIAL;
            }
        }

        if shutdown {
            let has_pending = pending.is_some()
                || !pending_projects.is_empty()
                || !pending_worktrees.is_empty()
                || !pending_terminal_replacements.is_empty();
            if !has_pending
                && let Err(error) = store.shutdown()
                && let Ok(mut message) = last_error.lock()
            {
                *message = Some(error.to_string());
            }
            return;
        }
    }
}

pub fn current_boot_id() -> Result<BootId, SessionError> {
    let value =
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(SessionError::BootId)?;
    Ok(BootId::new(value.trim()))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use rusqlite::Connection;

    use tempfile::tempdir;

    use crate::model::{
        CardId, CardKind, CardState, Existence, Exposure, LaunchRecipe, Residency,
        TerminalContinuity, TerminalId, TerminalSession, WorkspaceId,
    };

    use super::*;

    fn workspace() -> WorkspaceState {
        let launch = LaunchRecipe {
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
            cards: vec![CardState {
                id: CardId::new(),
                generation: 0,
                title: "shell".into(),
                width_fraction: 0.7,
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
                kind: CardKind::Terminal(TerminalSession {
                    id: TerminalId::new(),
                    launch: launch.clone(),
                    transcript_path: None,
                    resume_hint: None,
                    continuity: TerminalContinuity::RelaunchAvailable {
                        recipe: launch,
                        last_exit: None,
                    },
                }),
            }],
        }
    }

    #[test]
    fn chained_terminal_retries_preserve_the_persisted_expected_identity() {
        let card_id = CardId::new();
        let original_id = TerminalId::new();
        let middle_id = TerminalId::new();
        let final_id = TerminalId::new();
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-l".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let terminal = |id| TerminalSession {
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

        merge_terminal_replacement(
            &mut replacements,
            TerminalIdentityReplacement {
                card_id,
                expected_terminal_id: original_id,
                replacement: terminal(middle_id),
            },
        );
        merge_terminal_replacement(
            &mut replacements,
            TerminalIdentityReplacement {
                card_id,
                expected_terminal_id: middle_id,
                replacement: terminal(final_id),
            },
        );

        let replacement = &replacements[&card_id];
        assert_eq!(replacement.expected_terminal_id, original_id);
        assert_eq!(replacement.replacement.id, final_id);
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
            persistence_loop(
                store,
                0,
                BTreeMap::new(),
                HashMap::new(),
                HashMap::new(),
                worker_queue,
                worker_error,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while last_error.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "initial write did not fail");
            thread::sleep(Duration::from_millis(10));
        }
        blocker.execute_batch("ROLLBACK").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count: i64 = blocker
                .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
                .unwrap();
            if count == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "snapshot was not retried");
            thread::sleep(Duration::from_millis(10));
        }
        let (lock, changed) = &*queue;
        lock.lock().unwrap().shutdown = true;
        changed.notify_one();
        worker.join().unwrap();
        assert!(last_error.lock().unwrap().is_none());
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

        persistence_loop(
            store,
            0,
            BTreeMap::new(),
            HashMap::new(),
            HashMap::new(),
            queue,
            Arc::clone(&last_error),
        );
        assert!(last_error.lock().unwrap().is_some());
        blocker.execute_batch("ROLLBACK").unwrap();
        drop(blocker);

        let reopened = StateStore::open(&path, BootId::new("boot-shutdown")).unwrap();
        assert!(reopened.recovery_report().dirty_shutdown);
    }
}
