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
pub mod socket;

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
pub fn exact_session(
    process: &crate::model::agent_recovery::AgentProcess,
) -> Result<Option<String>, String> {
    match process.provider {
        Provider::Claude => claude::exact_session(&process.launch, process.pid)
            .map_err(|_| "Claude could not report an unambiguous live session through `claude agents --json`.".into()),
        Provider::Codex | Provider::Omp | Provider::Antigravity | Provider::Muse => Ok(None),
    }
}

pub fn codex_preview(recipe: &LaunchRecipe, id: &str) -> Result<(String, PathBuf), String> {
    let key = key(Provider::Codex, recipe)
        .map_err(|_| "Cannot verify this Codex account and local endpoint.")?;
    codex::preview(&key, id).map_err(|_| "The exact thread is not loaded on this account's shared Codex server, or its metadata could not be verified. Copy the full ID from the intended live CLI and retry.".into())
}

/// Read-only enumeration for user selection; grants no attach authority.
pub fn codex_candidates(
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
#[derive(Default)]
pub struct Observer {
    accounts: Mutex<BTreeMap<Key, Arc<Mutex<Entry>>>>,
    rollouts: rollout::Rollouts,
    attention: Mutex<attention::Tails>,
    claude_activity: Mutex<BTreeMap<(i32, u64), ClaudeActivity>>,
}

#[derive(Default)]
pub struct Observation {
    pub activity: Option<Activity>,
    pub title: Option<String>,
    pub attention: Option<crate::notifications::Evidence>,
    pub clear_attention: Option<AttentionClear>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionClear {
    TurnStarted,
    RequestResolved,
}

fn observed_claude_progress(
    previous: Option<ActivityState>,
    current: ActivityState,
) -> Option<AttentionClear> {
    match (previous?, current) {
        (ActivityState::Idle, ActivityState::Working) => Some(AttentionClear::TurnStarted),
        (previous, ActivityState::Idle | ActivityState::Working) if previous.waits_on_user() => {
            Some(AttentionClear::RequestResolved)
        }
        _ => None,
    }
}

struct ClaudeActivity {
    state: ActivityState,
    seen: Instant,
    order: ClaudeOrdering,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaudeFence {
    AwaitingInput,
    InputObserved,
}

#[derive(Default)]
struct ClaudeOrdering {
    transcript: Option<(u64, u64)>,
    idle_input: bool,
    fence: Option<ClaudeFence>,
}

impl ClaudeOrdering {
    fn notice(
        &mut self,
        state: ActivityState,
        clear: Option<AttentionClear>,
        mut edges: attention::Edges,
    ) -> Option<crate::notifications::Evidence> {
        if let Some(transcript) = edges.claude_transcript
            && (self.transcript != Some(transcript) || edges.claude_baseline)
        {
            self.transcript = Some(transcript);
            self.idle_input = false;
            self.fence = self.fence.map(|_| ClaudeFence::AwaitingInput);
        }
        if clear == Some(AttentionClear::TurnStarted) {
            self.fence = Some(if self.idle_input {
                ClaudeFence::InputObserved
            } else {
                ClaudeFence::AwaitingInput
            });
        }
        if let Some(fence) = self.fence {
            // An older completion may arrive on any poll after the census
            // reported a new start. Even returning to Idle doesn't establish
            // which turn emitted it. Require order in this exact transcript.
            let ordered = if edges.claude_input {
                edges.claude_ordered_notice
            } else {
                fence == ClaudeFence::InputObserved
            };
            if edges.notice.is_some() && ordered {
                self.fence = None;
            } else {
                edges.notice = None;
                if edges.claude_input {
                    self.fence = Some(ClaudeFence::InputObserved);
                }
            }
        }
        if state != ActivityState::Idle || edges.notice.is_some() {
            self.idle_input = false;
        }
        if state == ActivityState::Idle && edges.claude_input {
            // A root input can reach disk before the census shows Working.
            // Retain it as ordering evidence without treating it as a start.
            self.idle_input = !edges.claude_ordered_notice;
        }
        edges.notice
    }
}

pub enum ObserveError {
    Busy,
    Unavailable,
}

impl Observer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_observe(
        &self,
        process: &crate::model::agent_recovery::AgentProcess,
    ) -> Result<Observation, ObserveError> {
        let provider = process.provider;
        let recipe = &process.launch;
        // Only Claude Code and Codex publish what their sessions are doing.
        if !matches!(provider, Provider::Claude | Provider::Codex)
            || !recipe.working_directory.is_absolute()
        {
            return Err(ObserveError::Unavailable);
        }
        // A Codex CLI that keeps its own threads says what they are doing in
        // the rollouts it holds open; only one that holds none can be a client
        // of the shared server the census asks.
        if provider == Provider::Codex
            && let Some((state, edges)) = self.rollouts.observe(
                process.pid,
                process.start,
                u32::try_from(process.pid)
                    .ok()
                    .and_then(|pid| crate::process::clock::started_at(pid, process.start))
                    .and_then(|started| started.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|started| started.as_secs()),
            )
        {
            return Ok(Observation {
                activity: (state != ActivityState::Unknown).then_some(Activity { provider, state }),
                title: None,
                attention: edges.notice,
                clear_attention: edges.started.then_some(AttentionClear::TurnStarted),
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
                    Provider::Omp | Provider::Antigravity | Provider::Muse => unreachable!(),
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
        let edges = match census {
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
                    })
                    .unwrap_or_default()
            }
            Inventory::Codex(_) => attention::Edges::default(),
        };
        let (previous, clear_attention, attention) = if provider == Provider::Claude {
            // Claude's PID-scoped inventory explicitly distinguishes waiting.
            // A first census is a baseline, including when already waiting.
            let mut activity = self
                .claude_activity
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let key = (process.pid, process.start);
            if !activity.contains_key(&key) && activity.len() >= 2048 {
                let stale = activity
                    .iter()
                    .min_by_key(|(_, record)| record.seen)
                    .map(|(key, _)| *key);
                if let Some(stale) = stale {
                    activity.remove(&stale);
                }
            }
            let previous = activity.get(&key).map(|record| record.state);
            let record = activity.entry(key).or_insert_with(|| ClaudeActivity {
                state,
                seen: Instant::now(),
                order: ClaudeOrdering::default(),
            });
            record.state = state;
            record.seen = Instant::now();
            let clear = observed_claude_progress(previous, state);
            (previous, clear, record.order.notice(state, clear, edges))
        } else {
            (None, None, edges.notice)
        };
        let waiting = (state.waits_on_user() && previous.is_some_and(|previous| previous != state))
            .then(|| crate::notifications::Evidence {
                key: format!("waiting:{}", uuid::Uuid::new_v4()),
                kind: crate::notifications::Kind::Waiting,
            });
        Ok(Observation {
            activity: (state != ActivityState::Unknown).then_some(Activity { provider, state }),
            title,
            attention: waiting.or(attention),
            clear_attention,
        })
    }
}

#[cfg(test)]
#[test]
fn claude_terminal_needs_a_known_progress_transition_to_clear_attention() {
    use ActivityState::*;
    assert_eq!(
        observed_claude_progress(Some(Idle), Working),
        Some(AttentionClear::TurnStarted)
    );
    assert_eq!(
        observed_claude_progress(Some(Approval), Working),
        Some(AttentionClear::RequestResolved)
    );
    assert_eq!(
        observed_claude_progress(Some(Question), Idle),
        Some(AttentionClear::RequestResolved)
    );
    for (previous, current) in [
        (None, Working),
        (Some(Approval), Unknown),
        (Some(Unknown), Working),
        (Some(Working), Working),
        (Some(Question), Approval),
    ] {
        assert_eq!(observed_claude_progress(previous, current), None);
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
        Provider::Omp | Provider::Antigravity | Provider::Muse => return Err(()),
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
        ActivityState::Waiting | ActivityState::Approval | ActivityState::Question => 1,
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

    #[test]
    fn claude_terminal_fences_late_outcomes_across_polls() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let process = process(&root, root.join("unused-census"));
        let id = "00000000-0000-4000-8000-000000000001";
        let project_key: String = root
            .to_str()
            .unwrap()
            .encode_utf16()
            .map(|unit| match u8::try_from(unit) {
                Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
                _ => '-',
            })
            .collect();
        let project = root.join("projects").join(project_key);
        std::fs::create_dir_all(&project).unwrap();
        let mut transcript = std::fs::File::create(project.join(format!("{id}.jsonl"))).unwrap();

        let observer = Observer::new();
        let entry = cached_account(&observer, &process, ActivityState::Idle, "Fixture");
        let observe = |state| {
            {
                let mut entry = entry.lock().unwrap();
                entry.checked = Some(Instant::now());
                let Some(Inventory::Claude(census)) = &mut entry.census else {
                    panic!("missing process census");
                };
                let record = census.get_mut(&process.pid).unwrap();
                record.1 = state;
                record.3 = Some(id.into());
            }
            observer
                .try_observe(&process)
                .unwrap_or_else(|_| panic!("fixture census failed"))
        };
        let mut append = |frame: serde_json::Value| writeln!(transcript, "{frame}").unwrap();
        let result = |id| {
            serde_json::json!({
                "type": "assistant", "uuid": id, "message": {"stop_reason": "end_turn"}
            })
        };
        let input = |id, content| {
            serde_json::json!({
                "type": "user", "uuid": id, "message": {"role": "user", "content": content}
            })
        };

        assert!(observe(ActivityState::Idle).attention.is_none());
        let observation = observe(ActivityState::Working);
        assert_eq!(
            observation.clear_attention,
            Some(AttentionClear::TurnStarted)
        );
        assert!(observation.attention.is_none());

        append(input("tool", serde_json::json!([{"type": "tool_result"}])));
        append(result("previous"));
        let observation = observe(ActivityState::Working);
        assert_eq!(observation.clear_attention, None);
        assert!(observation.attention.is_none());
        append(result("previous-at-idle"));
        let observation = observe(ActivityState::Idle);
        assert_eq!(observation.clear_attention, None);
        assert!(observation.attention.is_none());

        append(input("new-turn", serde_json::json!("Read the next issue")));
        let observation = observe(ActivityState::Idle);
        assert_eq!(observation.clear_attention, None);
        assert!(observation.attention.is_none());
        let observation = observe(ActivityState::Working);
        assert_eq!(
            observation.clear_attention,
            Some(AttentionClear::TurnStarted)
        );
        assert!(observation.attention.is_none());
        append(result("later"));
        let observation = observe(ActivityState::Idle);
        assert_eq!(observation.clear_attention, None);
        assert_eq!(observation.attention.unwrap().key, "claude:later");

        append(result("before-unconfirmed-input"));
        append(input(
            "human-queued",
            serde_json::json!("Read the next issue"),
        ));
        let observation = observe(ActivityState::Idle);
        assert_eq!(observation.clear_attention, None);
        assert_eq!(
            observation.attention.unwrap().key,
            "claude:before-unconfirmed-input"
        );

        append(result("before-input"));
        append(input(
            "human",
            serde_json::json!("Search for the next issue"),
        ));
        append(result("after-input"));
        let observation = observe(ActivityState::Working);
        assert_eq!(
            observation.clear_attention,
            Some(AttentionClear::TurnStarted)
        );
        assert_eq!(observation.attention.unwrap().key, "claude:after-input");
    }

    #[test]
    fn claude_terminal_does_not_transfer_input_proof_to_another_transcript() {
        let mut ordering = ClaudeOrdering::default();
        let old_transcript = Some((1, 1));
        let new_transcript = Some((1, 2));
        let setup = attention::Edges {
            claude_transcript: old_transcript,
            claude_input: true,
            ..attention::Edges::default()
        };
        assert!(
            ordering
                .notice(
                    ActivityState::Working,
                    Some(AttentionClear::TurnStarted),
                    setup,
                )
                .is_none()
        );
        for claude_transcript in [new_transcript, old_transcript] {
            let unproven = attention::Edges {
                claude_transcript,
                notice: Some(crate::notifications::Evidence {
                    key: "unproven".into(),
                    kind: crate::notifications::Kind::Succeeded,
                }),
                ..attention::Edges::default()
            };
            assert!(
                ordering
                    .notice(ActivityState::Idle, None, unproven)
                    .is_none()
            );
        }
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
