//! Read-only live provider census. This is view state, never resume authority.
use super::{Activity, ActivityState, Provider};
use crate::model::LaunchRecipe;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod attention;
mod claude;
mod codex;
mod rollout;
pub(crate) mod socket;

/// How long a census, or a failure to take one, answers without checking.
const TTL: Duration = Duration::from_secs(2);
/// A Claude census whose status files are unchanged is still taken again
/// this often, in case the CLI learns of a change some other way.
const UNCHANGED_TTL: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ACCOUNTS: usize = 32;
const MAX_ROWS: usize = 256;
const MAX_BYTES: usize = 2 * 1024 * 1024;
type Census = BTreeMap<PathBuf, ActivityState>;

/// Public provider inventory keyed by the actual foreground PID, never cwd.
pub(crate) fn exact_session(
    process: &crate::model::agent_recovery::AgentProcess,
) -> Result<Option<String>, String> {
    match process.provider {
        Provider::Claude => claude::exact_session(&process.launch, process.pid)
            .map_err(|_| "Claude could not report an unambiguous live session through `claude agents --json`.".into()),
        Provider::Codex | Provider::Omp => Ok(None),
    }
}

pub(crate) fn codex_preview(recipe: &LaunchRecipe, id: &str) -> Result<(String, PathBuf), String> {
    let key = key(Provider::Codex, recipe)
        .map_err(|_| "Cannot verify this Codex account and local endpoint.")?;
    codex::preview(&key, id).map_err(|_| "The exact thread is not loaded on this account's shared Codex server, or its metadata could not be verified. Copy the full ID from the intended live CLI and retry.".into())
}

/// Read-only enumeration for user selection; grants no attach authority.
pub(crate) fn codex_candidates(
    recipe: &LaunchRecipe,
) -> Result<Vec<crate::chat::BindingCandidate>, String> {
    let key = key(Provider::Codex, recipe)
        .map_err(|_| "Cannot verify this Codex account and local endpoint.")?;
    let rows = codex::candidates(&key).map_err(|_| "This account's shared Codex server did not report a verifiable list of open conversations. Retry, or enter the full ID from the intended live CLI.".to_owned())?;
    Ok(rows
        .into_iter()
        .map(
            |(session_id, title, working_directory)| crate::chat::BindingCandidate {
                session_id,
                title,
                working_directory,
            },
        )
        .collect())
}
type ProbeResult<T> = Result<T, ()>;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    provider: u8,
    home: PathBuf,
    endpoint: PathBuf,
}

enum Inventory {
    Claude(claude::ProcessCensus),
    Codex(Census),
}

#[derive(Default)]
struct Entry {
    /// When the census was last taken or found unchanged.
    checked: Option<Instant>,
    /// When the census was last taken.
    listed: Option<Instant>,
    census: Option<Inventory>,
    /// The Claude status files as they were just before the census was taken.
    inputs: Option<claude::Inputs>,
}

impl Entry {
    /// Whether a Claude census can answer for `pid` without listing again:
    /// it lists the process, the CLI keeps a status file for it, and none
    /// changed since. A census taken before the process was listed is taken
    /// again at the next check, as before, however briefly the CLI lags.
    fn unchanged(&self, inputs: Option<&claude::Inputs>, pid: i32) -> bool {
        matches!(&self.census, Some(Inventory::Claude(census)) if census.contains_key(&pid))
            && self
                .listed
                .is_some_and(|listed| listed.elapsed() < UNCHANGED_TTL)
            && inputs
                .is_some_and(|inputs| inputs.tracks(pid) && self.inputs.as_ref() == Some(inputs))
    }
}

/// Shared account census cache used by the daemon's bounded activity workers.
pub(crate) struct Observer {
    accounts: Mutex<BTreeMap<Key, Arc<Mutex<Entry>>>>,
    rollouts: rollout::Rollouts,
    attention: Mutex<attention::Tails>,
    waiting: Mutex<BTreeMap<(i32, u64), (ActivityState, Instant)>>,
}

#[derive(Default)]
pub(crate) struct Observation {
    pub activity: Option<Activity>,
    pub title: Option<String>,
    pub attention: Option<crate::notifications::Evidence>,
}

pub(crate) enum ObserveError {
    Busy,
    Unavailable,
}

impl Observer {
    pub fn new() -> Self {
        Self {
            accounts: Mutex::new(BTreeMap::new()),
            rollouts: rollout::Rollouts::default(),
            attention: Mutex::default(),
            waiting: Mutex::default(),
        }
    }

    pub(crate) fn try_observe(
        &self,
        process: &crate::model::agent_recovery::AgentProcess,
    ) -> Result<Observation, ObserveError> {
        let provider = process.provider;
        let recipe = &process.launch;
        if provider == Provider::Omp || !recipe.working_directory.is_absolute() {
            return Err(ObserveError::Unavailable);
        }
        // A Codex CLI that keeps its own threads says what they are doing in
        // the rollouts it holds open; only one that holds none can be a client
        // of the shared server the census asks.
        if provider == Provider::Codex
            && let Some((state, attention)) = self.rollouts.observe(
                process.pid,
                process.start,
                crate::model::agent_recovery::process_started(process.start)
                    .and_then(|started| started.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|started| started.as_secs()),
            )
        {
            return Ok(Observation {
                activity: (state != ActivityState::Unknown).then_some(Activity { provider, state }),
                title: None,
                attention,
            });
        }
        let key = key(provider, recipe).map_err(|_| ObserveError::Unavailable)?;
        let entry = {
            let mut accounts = self
                .accounts
                .lock()
                .map_err(|_| ObserveError::Unavailable)?;
            if !accounts.contains_key(&key) && accounts.len() >= MAX_ACCOUNTS {
                // Do not evict an in-flight query and accidentally duplicate it.
                let unused = accounts
                    .iter()
                    .find(|(_, entry)| Arc::strong_count(entry) == 1)
                    .map(|(key, _)| key.clone())
                    .ok_or(ObserveError::Busy)?;
                accounts.remove(&unused);
            }
            accounts.entry(key.clone()).or_default().clone()
        };
        // Only the account lock spans I/O, never HostState or the registry lock.
        // A slow account must not occupy every daemon worker with duplicate
        // waiters. Another terminal can retry its cached census next time.
        let mut entry = match entry.try_lock() {
            Ok(entry) => entry,
            Err(std::sync::TryLockError::WouldBlock) => return Err(ObserveError::Busy),
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let mut recovered = error.into_inner();
                // The service contains observation panics and retries. Discard
                // potentially partial presentation state before allowing this
                // same account gate to serve a fresh census again.
                *recovered = Entry::default();
                entry.clear_poison();
                recovered
            }
        };
        if entry.checked.is_none_or(|checked| checked.elapsed() >= TTL) {
            // Read before listing, so a change made during the listing is
            // still a change at the next check.
            let inputs = (provider == Provider::Claude)
                .then(|| claude::Inputs::read(&key.home))
                .flatten();
            if !entry.unchanged(inputs.as_ref(), process.pid) {
                entry.census = match provider {
                    Provider::Claude => claude::census(recipe).map(Inventory::Claude),
                    Provider::Codex => codex::census(&key).map(Inventory::Codex),
                    Provider::Omp => unreachable!(),
                }
                .ok();
                // Failure is cached too, replacing (not retaining) previous Working.
                entry.listed = Some(Instant::now());
                entry.inputs = inputs;
            }
            entry.checked = Some(Instant::now());
        }
        let census = entry.census.as_ref().ok_or(ObserveError::Unavailable)?;
        let state = match census {
            Inventory::Claude(census) => {
                claude::activity(census, process.pid, &recipe.working_directory)
                    .ok_or(ObserveError::Unavailable)?
            }
            Inventory::Codex(census) => *census
                .get(&recipe.working_directory)
                .unwrap_or(&ActivityState::Idle),
        };
        let title = match census {
            Inventory::Claude(census) => {
                claude::title(census, process.pid, &recipe.working_directory)
            }
            Inventory::Codex(_) => None,
        };
        let attention = match census {
            Inventory::Claude(census) => {
                claude::session(census, process.pid, &recipe.working_directory)
                    .and_then(|id| {
                        crate::agent::claude_transcript(&key.home, &recipe.working_directory, id)
                            .ok()
                    })
                    .and_then(|path| {
                        use std::os::unix::fs::OpenOptionsExt;
                        std::fs::OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
                            .open(path)
                            .ok()
                    })
                    .and_then(|file| {
                        self.attention
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .read_for_process(
                                &file,
                                attention::Format::Claude,
                                (process.pid, process.start),
                            )
                            .ok()
                            .flatten()
                    })
            }
            Inventory::Codex(_) => None,
        };
        let waiting = (|| {
            // Claude's PID-scoped inventory explicitly distinguishes waiting.
            // A first census is a baseline, including when already waiting.
            if provider != Provider::Claude {
                return None;
            }
            let mut waiting = self.waiting.lock().unwrap_or_else(|p| p.into_inner());
            let key = (process.pid, process.start);
            if !waiting.contains_key(&key) && waiting.len() >= 2048 {
                let stale = waiting
                    .iter()
                    .min_by_key(|(_, (_, seen))| *seen)
                    .map(|(key, _)| *key);
                if let Some(stale) = stale {
                    waiting.remove(&stale);
                }
            }
            let old = waiting.insert(key, (state, Instant::now()));
            (state == ActivityState::Waiting && old.is_some_and(|(old, _)| old != state)).then(
                || crate::notifications::Evidence {
                    key: format!("waiting:{}", uuid::Uuid::new_v4()),
                    kind: crate::notifications::Kind::Waiting,
                },
            )
        })();
        Ok(Observation {
            activity: (state != ActivityState::Unknown).then_some(Activity { provider, state }),
            title,
            attention: waiting.or(attention),
        })
    }
}

fn absolute(recipe: &LaunchRecipe, value: &str) -> ProbeResult<PathBuf> {
    if value.is_empty() {
        return Err(());
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        recipe.working_directory.join(path)
    })
}

fn key(provider: Provider, recipe: &LaunchRecipe) -> ProbeResult<Key> {
    // A daemon's ambient HOME/CODEX_HOME is not the detected terminal's account.
    let variable = match provider {
        Provider::Claude => "CLAUDE_CONFIG_DIR",
        Provider::Codex => "CODEX_HOME",
        Provider::Omp => return Err(()),
    };
    let home = if let Some(value) = recipe.environment.get(variable) {
        absolute(recipe, value)?
    } else {
        absolute(recipe, recipe.environment.get("HOME").ok_or(())?)?.join(
            if provider == Provider::Claude {
                ".claude"
            } else {
                ".codex"
            },
        )
    };
    let home = std::fs::canonicalize(home).map_err(|_| ())?;
    let endpoint = if provider == Provider::Codex {
        codex::endpoint(recipe, &home)?
    } else {
        // HOME can affect command behavior even with an explicit config directory.
        recipe
            .environment
            .get("HOME")
            .map(|value| absolute(recipe, value))
            .transpose()?
            .unwrap_or_default()
    };
    if provider == Provider::Claude {
        // Resolve from the detected process PATH, never the host's PATH.
        claude::executable(recipe)?;
    }
    Ok(Key {
        provider: provider as u8,
        home,
        endpoint,
    })
}

fn add(census: &mut Census, cwd: &str, state: ActivityState) -> ProbeResult<()> {
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        return Err(());
    }
    let current = census
        .entry(cwd.to_path_buf())
        .or_insert(ActivityState::Idle);
    // Unknown poisons a worktree rather than hiding an unsupported live status.
    let rank = |state| match state {
        ActivityState::Idle => 0,
        ActivityState::Waiting => 1,
        ActivityState::Working => 2,
        ActivityState::Unknown => 3,
    };
    if rank(state) > rank(*current) {
        *current = state;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(directory: &Path, program: PathBuf) -> crate::model::agent_recovery::AgentProcess {
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: program.to_str().unwrap().into(),
            arguments: Vec::new(),
            working_directory: directory.into(),
            environment: BTreeMap::from([
                ("HOME".into(), directory.to_str().unwrap().into()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    directory.to_str().unwrap().into(),
                ),
            ]),
        };
        crate::model::agent_recovery::AgentProcess {
            pid: 123,
            start: 456,
            provider: Provider::Claude,
            recovery: launch.clone(),
            launch,
        }
    }

    fn cached_account(
        observer: &Observer,
        process: &crate::model::agent_recovery::AgentProcess,
        state: ActivityState,
        title: &str,
    ) -> Arc<Mutex<Entry>> {
        let entry = Arc::new(Mutex::new(Entry {
            checked: Some(Instant::now()),
            listed: Some(Instant::now()),
            inputs: None,
            census: Some(Inventory::Claude(BTreeMap::from([(
                process.pid,
                (
                    process.launch.working_directory.clone(),
                    state,
                    Some(title.into()),
                    None,
                ),
            )]))),
        }));
        observer.accounts.lock().unwrap().insert(
            key(process.provider, &process.launch).unwrap(),
            entry.clone(),
        );
        entry
    }

    fn poison_fresh_census(entry: &Mutex<Entry>) {
        {
            let held = entry.lock().unwrap();
            assert!(held.census.is_some());
            assert!(held.checked.unwrap().elapsed() < TTL);
        }
        let failure = std::panic::catch_unwind(|| {
            let _held = entry.lock().unwrap();
            panic!("fixture panic while the account census is held");
        });
        assert!(failure.is_err());
        assert!(entry.is_poisoned());
    }

    #[test]
    fn poisoned_account_refreshes_fresh_cached_state_and_preserves_its_gate() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("census-fixture");
        let marker = directory.path().join("queries");
        std::fs::write(
            &script,
            r#"#!/bin/sh
set -eu
test "$#" -eq 2
test "$1" = agents
test "$2" = --json
printf 'query\n' >> "$CAGENTS_OBSERVER_MARKER"
printf '%s\n' "$CAGENTS_OBSERVER_ROWS"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut process = process(directory.path(), script);
        process.launch.environment.extend([
            (
                "CAGENTS_OBSERVER_MARKER".into(),
                marker.to_str().unwrap().into(),
            ),
            (
                "CAGENTS_OBSERVER_ROWS".into(),
                serde_json::json!([{
                    "kind": "interactive",
                    "pid": process.pid,
                    "cwd": directory.path(),
                    "status": "waiting",
                    "name": "Fresh title"
                }])
                .to_string(),
            ),
        ]);
        let observer = Observer::new();
        let entry = cached_account(&observer, &process, ActivityState::Working, "Stale title");
        poison_fresh_census(&entry);

        for _ in 0..2 {
            let observed = observer
                .try_observe(&process)
                .unwrap_or_else(|_| panic!("fresh census failed"));
            assert_eq!(observed.activity.unwrap().state, ActivityState::Waiting);
            assert_eq!(observed.title.as_deref(), Some("Fresh title"));
            assert!(
                observed.attention.is_none(),
                "first Waiting census is a baseline"
            );
        }
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "query\n");
        assert!(!entry.is_poisoned());
        let accounts = observer.accounts.lock().unwrap();
        assert!(Arc::ptr_eq(
            accounts
                .get(&key(process.provider, &process.launch).unwrap())
                .unwrap(),
            &entry
        ));
        drop(accounts);
        let _held = entry.lock().unwrap();
        assert!(matches!(
            observer.try_observe(&process),
            Err(ObserveError::Busy)
        ));
    }

    #[test]
    fn poisoned_account_discards_stale_census_and_caches_failure_without_affecting_peers() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-census-executable");
        let process = process(directory.path(), missing.clone());
        let other_directory = tempfile::tempdir().unwrap();
        let peer = self::process(other_directory.path(), missing);
        let observer = Observer::new();
        let entry = cached_account(&observer, &process, ActivityState::Working, "Stale title");
        cached_account(&observer, &peer, ActivityState::Idle, "Peer title");
        poison_fresh_census(&entry);

        assert!(matches!(
            observer.try_observe(&process),
            Err(ObserveError::Unavailable)
        ));
        assert!(!entry.is_poisoned());
        let failed_at = {
            let cached = entry.lock().unwrap();
            assert!(
                cached.census.is_none(),
                "failed refresh must discard stale Working/title"
            );
            cached.checked.unwrap()
        };
        assert!(matches!(
            observer.try_observe(&process),
            Err(ObserveError::Unavailable)
        ));
        assert_eq!(
            entry.lock().unwrap().checked,
            Some(failed_at),
            "failed census retains its normal TTL"
        );

        let observed = observer
            .try_observe(&peer)
            .unwrap_or_else(|_| panic!("peer cache failed"));
        assert_eq!(observed.activity.unwrap().state, ActivityState::Idle);
        assert_eq!(observed.title.as_deref(), Some("Peer title"));
        assert!(observed.attention.is_none());
        assert_eq!(observer.accounts.lock().unwrap().len(), 2);
    }

    #[test]
    fn unchanged_status_files_answer_without_listing_again() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("census-fixture");
        let marker = directory.path().join("queries");
        let sessions = directory.path().join("sessions");
        let arm = directory.path().join("rewrite-during-listing");
        // Lists the fixture row; while armed, also changes the process's
        // status file during the listing, as a CLI changing state would.
        std::fs::write(
            &script,
            r#"#!/bin/sh
set -eu
printf 'query\n' >> "$CAGENTS_OBSERVER_MARKER"
if [ -e "$CAGENTS_OBSERVER_ARM" ]; then
    rm "$CAGENTS_OBSERVER_ARM"
    printf ' ' >> "$CAGENTS_OBSERVER_STATUS"
fi
printf '%s\n' "$CAGENTS_OBSERVER_ROWS"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut process = process(directory.path(), script);
        let status = sessions.join(format!("{}.json", process.pid));
        process.launch.environment.extend([
            (
                "CAGENTS_OBSERVER_MARKER".into(),
                marker.to_str().unwrap().into(),
            ),
            ("CAGENTS_OBSERVER_ARM".into(), arm.to_str().unwrap().into()),
            (
                "CAGENTS_OBSERVER_STATUS".into(),
                status.to_str().unwrap().into(),
            ),
            (
                "CAGENTS_OBSERVER_ROWS".into(),
                serde_json::json!([{
                    "kind": "interactive",
                    "pid": process.pid,
                    "cwd": directory.path(),
                    "status": "idle",
                    "name": "Idle task"
                }])
                .to_string(),
            ),
        ]);
        let observer = Observer::new();
        let queries = || std::fs::read_to_string(&marker).unwrap().lines().count();
        let observe = || {
            let observed = observer
                .try_observe(&process)
                .unwrap_or_else(|_| panic!("census failed"));
            assert_eq!(observed.activity.unwrap().state, ActivityState::Idle);
            assert_eq!(observed.title.as_deref(), Some("Idle task"));
        };
        // Age the cache as if time had passed: the check, and optionally the
        // listing beyond its backstop.
        let expire = |listing: bool| {
            let entry = observer
                .accounts
                .lock()
                .unwrap()
                .get(&key(process.provider, &process.launch).unwrap())
                .unwrap()
                .clone();
            let mut entry = entry.lock().unwrap();
            entry.checked = entry.checked.map(|at| at - TTL);
            if listing {
                entry.listed = entry.listed.map(|at| at - UNCHANGED_TTL);
            }
        };
        // Each write changes the length, so no step relies on the clock's
        // resolution to tell versions apart.
        let rewrite = |path: &Path| {
            let mut contents = std::fs::read(path).unwrap_or_default();
            contents.push(b' ');
            std::fs::write(path, contents).unwrap();
        };

        // Without a sessions directory, every check lists.
        observe();
        expire(false);
        observe();
        assert_eq!(queries(), 2);

        // Nor with one that keeps only other processes' files, which shows
        // nothing about this one.
        std::fs::create_dir(&sessions).unwrap();
        std::fs::write(sessions.join("1.json"), "{}").unwrap();
        for _ in 0..2 {
            expire(false);
            observe();
        }
        assert_eq!(queries(), 4, "a process without a status file lists");

        std::fs::write(&status, "{}").unwrap();
        expire(false);
        observe();
        assert_eq!(queries(), 5, "the first files seen are a baseline");
        for _ in 0..5 {
            expire(false);
            observe();
        }
        assert_eq!(queries(), 5, "an idle account is not listed again");

        // Any session's change, arrival or departure is a change.
        rewrite(&status);
        expire(false);
        observe();
        assert_eq!(queries(), 6);
        std::fs::write(sessions.join("2.json"), "{}").unwrap();
        expire(false);
        observe();
        assert_eq!(queries(), 7);
        std::fs::remove_file(sessions.join("1.json")).unwrap();
        expire(false);
        observe();
        assert_eq!(queries(), 8);

        // A change made while listing is still a change at the next check:
        // the files are read before the listing, not after it.
        std::fs::write(&arm, "").unwrap();
        expire(false);
        rewrite(&status);
        observe();
        assert_eq!(queries(), 9);
        assert!(!arm.exists(), "the listing changed the status file");
        expire(false);
        observe();
        assert_eq!(queries(), 10);
        expire(false);
        observe();
        assert_eq!(queries(), 10);

        // Within the TTL nothing is read at all; past the backstop it lists.
        observe();
        assert_eq!(queries(), 10);
        expire(true);
        observe();
        assert_eq!(queries(), 11);
    }

    #[test]
    fn a_census_without_the_process_lists_again_at_the_next_check() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("census-fixture");
        let marker = directory.path().join("queries");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'query\\n' >> \"$CAGENTS_OBSERVER_MARKER\"\nprintf '[]\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut process = process(directory.path(), script);
        process.launch.environment.insert(
            "CAGENTS_OBSERVER_MARKER".into(),
            marker.to_str().unwrap().into(),
        );
        // The CLI has written its file but not yet listed it.
        std::fs::create_dir(directory.path().join("sessions")).unwrap();
        std::fs::write(
            directory
                .path()
                .join(format!("sessions/{}.json", process.pid)),
            "{}",
        )
        .unwrap();
        let observer = Observer::new();
        for _ in 0..3 {
            assert!(matches!(
                observer.try_observe(&process),
                Err(ObserveError::Unavailable)
            ));
            let entry = observer
                .accounts
                .lock()
                .unwrap()
                .get(&key(process.provider, &process.launch).unwrap())
                .unwrap()
                .clone();
            let mut entry = entry.lock().unwrap();
            entry.checked = entry.checked.map(|at| at - TTL);
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 3);
    }

    #[test]
    fn busy_account_defers_without_waiting_or_publishing_failure() {
        let directory = tempfile::tempdir().unwrap();
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/true".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), directory.path().to_str().unwrap().into()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    directory.path().to_str().unwrap().into(),
                ),
            ]),
        };
        let process = crate::model::agent_recovery::AgentProcess {
            pid: 123,
            start: 456,
            provider: Provider::Claude,
            launch: launch.clone(),
            recovery: launch.clone(),
        };
        let observer = Observer::new();
        let entry = Arc::new(Mutex::new(Entry::default()));
        observer
            .accounts
            .lock()
            .unwrap()
            .insert(key(Provider::Claude, &launch).unwrap(), Arc::clone(&entry));
        let _held = entry.lock().unwrap();
        assert!(matches!(
            observer.try_observe(&process),
            Err(ObserveError::Busy)
        ));
    }
}
