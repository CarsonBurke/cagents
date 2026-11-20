//! Estimates how long each provider keeps a conversation's prompt cache.
//!
//! A provider's documented time-to-live is only where an estimate starts.
//! Each conversation's runtime measures whether its cache survived the long
//! idle gaps it saw (see [`crate::chat::prompt_cache`]); the daemon collects
//! those observations per provider account whenever it lists sessions, keeps
//! them across restarts, and lets them correct the assumption: a hit after a
//! longer gap raises it, and repeated misses within it lower it.
use crate::account::{AccountId, WindowKind, info::RemainingWindow};
use crate::agent::Provider;
use crate::agent_execution::directory::{PromptCache, SessionAccount, TtlBasis};
use crate::chat::prompt_cache::{Activity, Observation};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MINUTE_MS: u64 = 60 * 1000;
const FILE_NAME: &str = "prompt-cache.json";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 1024 * 1024;
/// Observations kept per account, newest last.
const KEPT: usize = 64;
/// Accounts kept, most recently observed last.
const ACCOUNTS: usize = 32;
/// The shortest gap a runtime measures; below it nothing is known.
const SHORTEST_MEASURED_MS: u64 = 2 * MINUTE_MS;
/// Misses within the assumed TTL that it takes to lower it: one may come
/// from something other than time, such as a changed system prompt.
const MISSES_TO_LOWER: usize = 2;

/// How long `provider` documents keeping the cache for `account`, when it
/// says. Claude Code caches for an hour on a subscription and for five
/// minutes once an account runs into overage; Codex keeps OpenAI's in-memory
/// retention, five to ten minutes idle.
fn assumed_ttl(provider: Provider, account: Option<&SessionAccount>) -> Option<u64> {
    match provider {
        Provider::Claude => {
            // Running past a window the whole account shares is overage; a
            // spent model-scoped window only stops that model.
            let overage = account.is_some_and(|account| {
                account.remaining.iter().any(|window: &RemainingWindow| {
                    matches!(window.window.kind, WindowKind::Session | WindowKind::Weekly)
                        && window.remaining_fraction <= 0.0
                        && !window.reset_elapsed
                })
            });
            Some(if overage { 5 } else { 60 } * MINUTE_MS)
        }
        Provider::Codex => Some(5 * MINUTE_MS),
        Provider::Omp | Provider::Antigravity | Provider::Muse => None,
    }
}

/// The TTL that `observations` support, starting from `assumed`.
fn estimate(assumed: Option<u64>, observations: &[Observation]) -> Option<(u64, TtlBasis)> {
    let longest_hit = observations
        .iter()
        .filter(|observation| observation.hit)
        .map(|observation| observation.gap_ms)
        .max();
    if let Some(hit) = longest_hit
        && assumed.is_none_or(|assumed| hit > assumed)
    {
        return Some((hit, TtlBasis::Observed));
    }
    // A miss after a gap no hit outlasted bounds the TTL from above.
    let misses = observations
        .iter()
        .filter(|observation| {
            !observation.hit
                && longest_hit.is_none_or(|hit| observation.gap_ms > hit)
                && assumed.is_none_or(|assumed| observation.gap_ms < assumed)
        })
        .count();
    if misses >= MISSES_TO_LOWER {
        // Trust only what is known to have survived.
        return Some((
            longest_hit.unwrap_or(SHORTEST_MEASURED_MS),
            TtlBasis::Observed,
        ));
    }
    assumed.map(|assumed| (assumed, TtlBasis::Assumed))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    version: u32,
    accounts: Vec<Account>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Account {
    /// The provider's executable name, which stays stable as providers are
    /// added, unlike its wire encoding.
    provider: String,
    account: Option<AccountId>,
    /// The lifetime the provider documents for the account at the time, so
    /// that what a Claude account showed in overage, where caches last five
    /// minutes, doesn't skew what it shows on its subscription.
    assumed_ttl_ms: Option<u64>,
    observations: Vec<Observation>,
}

/// Every account's observations, and where they are kept.
pub(in crate::host) struct Calibration {
    /// Absent for a daemon that keeps them in memory only, as tests do.
    path: Option<PathBuf>,
    accounts: Mutex<Option<Vec<Account>>>,
}

impl Calibration {
    /// The running installation's calibration, kept in its cache directory.
    pub(in crate::host) fn installation() -> Self {
        Self {
            path: crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join(FILE_NAME)),
            accounts: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            accounts: Mutex::new(None),
        }
    }

    pub(in crate::host) fn in_memory() -> Self {
        Self {
            path: None,
            accounts: Mutex::new(Some(Vec::new())),
        }
    }

    /// Records what `activity` observed that the calibration doesn't hold
    /// yet, and estimates the conversation's cache from everything known
    /// about its account. `None` when no request was observed, or nothing
    /// says how long the provider keeps a cache.
    pub(in crate::host) fn estimate(
        &self,
        provider: Provider,
        account: Option<&SessionAccount>,
        activity: Activity,
        context_tokens: u64,
    ) -> Option<PromptCache> {
        let requested_at = activity.requested_at?;
        let name = provider.executable();
        let id = account.map(|account| account.id.clone());
        let assumed = assumed_ttl(provider, account);
        let mut accounts = self.accounts.lock().unwrap_or_else(|p| p.into_inner());
        let accounts = accounts.get_or_insert_with(|| {
            self.path
                .as_deref()
                .and_then(read_document)
                .map(|document| document.accounts)
                .unwrap_or_default()
        });
        let index = match accounts.iter().position(|known| {
            known.provider == name && known.account == id && known.assumed_ttl_ms == assumed
        }) {
            Some(index) => index,
            None => {
                if accounts.len() == ACCOUNTS {
                    accounts.remove(0);
                }
                accounts.push(Account {
                    provider: name.to_owned(),
                    account: id,
                    assumed_ttl_ms: assumed,
                    observations: Vec::new(),
                });
                accounts.len() - 1
            }
        };
        let known = &mut accounts[index];
        let before = known.observations.clone();
        known.observations.extend(
            activity
                .observations
                .into_iter()
                .filter(|observation| !before.contains(observation)),
        );
        known.observations.sort_by_key(|observation| observation.at);
        let excess = known.observations.len().saturating_sub(KEPT);
        known.observations.drain(..excess);
        // An observation already dropped as old comes back with every listing
        // while its conversation runs; it teaches nothing.
        let learned = known.observations != before;
        let (ttl_ms, basis) = estimate(assumed, &known.observations)?;
        if learned && let Some(path) = &self.path {
            // Most recently observed last, so the oldest is dropped first.
            let observed = accounts.remove(index);
            accounts.push(observed);
            let document = Document {
                version: VERSION,
                accounts: accounts.clone(),
            };
            if let Err(error) = write_document(path, &document) {
                tracing::warn!(%error, "prompt cache observations were not saved");
            }
        }
        Some(PromptCache {
            requested_at,
            ttl_ms,
            basis,
            context_tokens,
        })
    }
}

fn read_document(path: &Path) -> Option<Document> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    let document: Document = serde_json::from_slice(&bytes).ok()?;
    (document.version == VERSION).then_some(document)
}

fn write_document(path: &Path, document: &Document) -> std::io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the calibration file has no directory"))?;
    crate::secure_file::prepare_private_directory(directory)?;
    let bytes = serde_json::to_vec(document).map_err(std::io::Error::other)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".prompt-cache.")
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .tempfile_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::UsageWindow;

    fn observation(at: u64, gap_minutes: u64, hit: bool) -> Observation {
        Observation {
            at,
            gap_ms: gap_minutes * MINUTE_MS,
            hit,
        }
    }

    fn account(remaining: f32) -> SessionAccount {
        SessionAccount {
            id: "claude/system".parse().unwrap(),
            label: None,
            status: None,
            remaining: vec![RemainingWindow {
                window: UsageWindow {
                    key: "five_hour".into(),
                    kind: WindowKind::Session,
                    label: "5h".into(),
                    used: 1.0 - remaining,
                    resets_at: None,
                    span_secs: None,
                    binding: false,
                },
                remaining_fraction: remaining,
                reset_elapsed: false,
            }],
            stale: false,
            observed_at: None,
            ordinary_usage_allowed: None,
        }
    }

    #[test]
    fn the_documented_ttl_holds_until_observations_contradict_it() {
        let hour = Some(60 * MINUTE_MS);
        assert_eq!(
            estimate(hour, &[]),
            Some((60 * MINUTE_MS, TtlBasis::Assumed))
        );
        // Hits within it, and a single miss, change nothing.
        assert_eq!(
            estimate(hour, &[observation(1, 30, true), observation(2, 40, false)]),
            Some((60 * MINUTE_MS, TtlBasis::Assumed))
        );
        // A hit after a longer gap proves the cache lasts longer.
        assert_eq!(
            estimate(hour, &[observation(1, 90, true)]),
            Some((90 * MINUTE_MS, TtlBasis::Observed))
        );
        // Repeated misses beyond every hit, and within the assumption,
        // lower it to what is known to survive.
        assert_eq!(
            estimate(
                hour,
                &[
                    observation(1, 4, true),
                    observation(2, 10, false),
                    observation(3, 20, false),
                ]
            ),
            Some((4 * MINUTE_MS, TtlBasis::Observed))
        );
        // Misses no longer than a hit came from something other than time.
        assert_eq!(
            estimate(
                hour,
                &[
                    observation(1, 30, true),
                    observation(2, 10, false),
                    observation(3, 20, false),
                ]
            ),
            Some((60 * MINUTE_MS, TtlBasis::Assumed))
        );
    }

    #[test]
    fn a_provider_without_a_documented_ttl_is_unknown_until_observed() {
        assert_eq!(estimate(None, &[]), None);
        assert_eq!(
            estimate(None, &[observation(1, 12, true)]),
            Some((12 * MINUTE_MS, TtlBasis::Observed))
        );
        assert_eq!(
            estimate(None, &[observation(1, 3, false), observation(2, 9, false)]),
            Some((SHORTEST_MEASURED_MS, TtlBasis::Observed))
        );
    }

    #[test]
    fn claude_assumes_an_hour_except_in_overage() {
        assert_eq!(assumed_ttl(Provider::Claude, None), Some(60 * MINUTE_MS));
        assert_eq!(
            assumed_ttl(Provider::Claude, Some(&account(0.4))),
            Some(60 * MINUTE_MS)
        );
        assert_eq!(
            assumed_ttl(Provider::Claude, Some(&account(0.0))),
            Some(5 * MINUTE_MS)
        );
        assert_eq!(assumed_ttl(Provider::Codex, None), Some(5 * MINUTE_MS));
        assert_eq!(assumed_ttl(Provider::Omp, None), None);
    }

    #[test]
    fn observations_are_collected_once_and_kept_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache").join(FILE_NAME);
        let activity = Activity {
            requested_at: Some(500 * MINUTE_MS),
            observations: vec![observation(100, 90, true)],
        };
        let calibration = Calibration::at(path.clone());
        let estimated = calibration
            .estimate(Provider::Claude, None, activity.clone(), 120_000)
            .unwrap();
        assert_eq!(
            estimated,
            PromptCache {
                requested_at: 500 * MINUTE_MS,
                ttl_ms: 90 * MINUTE_MS,
                basis: TtlBasis::Observed,
                context_tokens: 120_000,
            }
        );
        // Listing again collects nothing new.
        calibration.estimate(Provider::Claude, None, activity, 120_000);

        let restarted = Calibration::at(path);
        let estimated = restarted
            .estimate(
                Provider::Claude,
                None,
                Activity {
                    requested_at: Some(7),
                    observations: Vec::new(),
                },
                1_000,
            )
            .unwrap();
        assert_eq!(estimated.ttl_ms, 90 * MINUTE_MS);
        let saved = read_document(&directory.path().join("cache").join(FILE_NAME)).unwrap();
        assert_eq!(saved.accounts.len(), 1);
        assert_eq!(saved.accounts[0].observations.len(), 1);
    }

    #[test]
    fn each_account_and_regime_is_calibrated_on_its_own() {
        let calibration = Calibration::in_memory();
        let requested = Activity {
            requested_at: Some(1),
            observations: Vec::new(),
        };
        let observed = Activity {
            requested_at: Some(1),
            observations: vec![observation(1, 20, false), observation(2, 30, false)],
        };
        let overage = calibration
            .estimate(Provider::Claude, Some(&account(0.0)), observed, 0)
            .unwrap();
        assert_eq!(overage.ttl_ms, 5 * MINUTE_MS);
        // Misses that overage expects would lower an hour; they don't reach
        // the same account on its subscription, or another account.
        for account in [Some(account(0.5)), None] {
            let estimated = calibration
                .estimate(Provider::Claude, account.as_ref(), requested.clone(), 0)
                .unwrap();
            assert_eq!(
                (estimated.ttl_ms, estimated.basis),
                (60 * MINUTE_MS, TtlBasis::Assumed)
            );
        }
        assert_eq!(
            calibration.estimate(Provider::Claude, None, Activity::default(), 0),
            None,
            "no request was observed"
        );
        assert_eq!(
            calibration.estimate(Provider::Omp, None, requested, 0),
            None,
            "nothing says how long the cache lasts"
        );
    }

    #[test]
    fn an_observation_dropped_as_old_is_not_learned_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let calibration = Calibration::at(path.clone());
        let newer = Activity {
            requested_at: Some(1),
            observations: (0..KEPT as u64)
                .map(|at| observation(1_000 + at, 10, true))
                .collect(),
        };
        calibration.estimate(Provider::Claude, None, newer, 0);
        std::fs::remove_file(&path).unwrap();
        let old = Activity {
            requested_at: Some(1),
            observations: vec![observation(5, 10, true)],
        };
        calibration.estimate(Provider::Claude, None, old, 0);
        assert!(!path.exists(), "nothing changed, so nothing was written");
    }
}
