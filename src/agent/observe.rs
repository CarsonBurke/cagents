//! Read-only live worktree census. This is view state, never resume authority.
use super::{Activity, ActivityState, Provider};
use crate::model::LaunchRecipe;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod claude;
mod codex;
pub(crate) mod socket;

const TTL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ACCOUNTS: usize = 32;
const MAX_ROWS: usize = 256;
const MAX_BYTES: usize = 2 * 1024 * 1024;
type Census = BTreeMap<PathBuf, ActivityState>;
type ProbeResult<T> = Result<T, ()>;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    provider: u8,
    home: PathBuf,
    endpoint: PathBuf,
}

struct Entry {
    updated: Option<Instant>,
    census: Option<Census>,
}

/// Lazily invoked by terminal activity requests; no background thread or timer.
pub(crate) struct Observer {
    accounts: Mutex<BTreeMap<Key, Arc<Mutex<Entry>>>>,
}

impl Observer {
    pub fn new() -> Self {
        Self {
            accounts: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn activity(&self, provider: Provider, recipe: &LaunchRecipe) -> Option<Activity> {
        if provider == Provider::Omp || !recipe.working_directory.is_absolute() {
            return None;
        }
        let key = key(provider, recipe).ok()?;
        let entry = {
            let mut accounts = self.accounts.lock().ok()?;
            if !accounts.contains_key(&key) && accounts.len() >= MAX_ACCOUNTS {
                // Do not evict an in-flight query and accidentally duplicate it.
                let unused = accounts
                    .iter()
                    .find(|(_, entry)| Arc::strong_count(entry) == 1)
                    .map(|(key, _)| key.clone())?;
                accounts.remove(&unused);
            }
            accounts
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(Entry {
                        updated: None,
                        census: None,
                    }))
                })
                .clone()
        };
        // Only the account lock spans I/O, never HostState or the registry lock.
        let mut entry = entry.lock().ok()?;
        if entry.updated.is_none_or(|updated| updated.elapsed() >= TTL) {
            entry.census = match provider {
                Provider::Claude => claude::census(recipe),
                Provider::Codex => codex::census(&key),
                Provider::Omp => unreachable!(),
            }
            .ok();
            // Failure is cached too, replacing (not retaining) previous Working.
            entry.updated = Some(Instant::now());
        }
        let state = *entry
            .census
            .as_ref()?
            .get(&recipe.working_directory)
            .unwrap_or(&ActivityState::Idle);
        (state != ActivityState::Unknown).then_some(Activity { provider, state })
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
