//! Estimates how long each provider keeps a conversation's prompt cache.
//!
//! Claude says with each request how long the cache it wrote lasts, so its
//! estimate rests on that. Other providers keep a cache for a time they only
//! document, and loosely: there the documented lifetime is where an estimate
//! starts. Each conversation's runtime measures whether its cache survived
//! the long idle gaps it saw (see [`crate::chat::prompt_cache`]); the daemon
//! collects those observations per provider account whenever it lists
//! sessions, keeps them across restarts, and settles on the lifetime they
//! contradict least.
use crate::account::AccountId;
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
const VERSION: u32 = 2;
const MAX_BYTES: u64 = 1024 * 1024;
/// Observations kept per account, newest last.
const KEPT: usize = 64;
/// Accounts kept, most recently observed last.
const ACCOUNTS: usize = 32;
/// The shortest gap a runtime measures; below it nothing is known.
const SHORTEST_MEASURED_MS: u64 = 2 * MINUTE_MS;
/// How many more observations a lifetime other than the documented one must
/// explain: one hit or miss may come from something other than time, such as
/// a changed system prompt or model.
const DOCUMENTED_WEIGHT: usize = 1;

/// How long `provider` documents keeping a cache, when it says and the
/// conversation didn't. Codex keeps OpenAI's in-memory retention, five to
/// ten minutes idle; Claude reports its lifetime with each request.
fn assumed_ttl(provider: Provider) -> Option<u64> {
    match provider {
        Provider::Codex => Some(5 * MINUTE_MS),
        Provider::Claude | Provider::Omp | Provider::Antigravity | Provider::Muse => None,
    }
}

/// The lifetime `observations` contradict least, starting from `assumed`.
///
/// A hit after a gap says the cache lasts at least that long, and a miss
/// says it lasts less. Each candidate lifetime counts the observations it
/// contradicts, and one other than `assumed` counts [`DOCUMENTED_WEIGHT`]
/// more. Ties go to the documented lifetime, then to the shorter one, so an
/// estimate never calls a cache warm on less evidence than it has.
fn estimate(assumed: Option<u64>, observations: &[Observation]) -> Option<(u64, TtlBasis)> {
    if observations.is_empty() {
        return assumed.map(|assumed| (assumed, TtlBasis::Assumed));
    }
    let contradicted = |ttl: u64| {
        observations
            .iter()
            .filter(|observation| (observation.gap_ms <= ttl) != observation.hit)
            .count()
    };
    let observed = observations
        .iter()
        .filter(|observation| observation.hit)
        .map(|observation| observation.gap_ms)
        .chain([SHORTEST_MEASURED_MS])
        .filter(|ttl| Some(*ttl) != assumed)
        .map(|ttl| (contradicted(ttl) + DOCUMENTED_WEIGHT, ttl))
        .min();
    match (assumed, observed) {
        (Some(assumed), Some((cost, _))) if contradicted(assumed) <= cost => {
            Some((assumed, TtlBasis::Assumed))
        }
        (_, Some((_, ttl))) => Some((ttl, TtlBasis::Observed)),
        (Some(assumed), None) => Some((assumed, TtlBasis::Assumed)),
        (None, None) => None,
    }
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
    observations: Vec<Observation>,
}

#[derive(Default)]
struct Accounts {
    /// Absent until first read from the file.
    accounts: Option<Vec<Account>>,
    /// Something was learned that the file doesn't hold yet.
    unsaved: bool,
}

/// Every account's observations, and where they are kept.
pub(in crate::host) struct Calibration {
    /// Absent for a daemon that keeps them in memory only, as tests do.
    path: Option<PathBuf>,
    accounts: Mutex<Accounts>,
    /// Held while saving, so saves land in the order they were taken.
    saving: Mutex<()>,
}

impl Calibration {
    /// The running installation's calibration, kept in its cache directory.
    pub(in crate::host) fn installation() -> Self {
        Self::kept_at(crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join(FILE_NAME)))
    }

    fn kept_at(path: Option<PathBuf>) -> Self {
        Self {
            path,
            accounts: Mutex::default(),
            saving: Mutex::new(()),
        }
    }

    pub(in crate::host) fn in_memory() -> Self {
        Self::kept_at(None)
    }

    /// Estimates the conversation's cache from what `activity` reports and,
    /// where its provider doesn't say how long a cache lasts, from what is
    /// known about its account, which first learns what `activity` observed.
    /// `None` when no request was observed, or nothing says how long the
    /// provider keeps a cache. Call [`Self::save`] once done estimating.
    pub(in crate::host) fn estimate(
        &self,
        provider: Provider,
        account: Option<&SessionAccount>,
        activity: Activity,
        context_tokens: u64,
    ) -> Option<PromptCache> {
        let requested_at = activity.requested_at?;
        let (ttl_ms, basis) = match activity.ttl_ms {
            Some(ttl_ms) => (ttl_ms, TtlBasis::Reported),
            None => self.calibrate(provider, account, activity.observations)?,
        };
        Some(PromptCache {
            requested_at,
            ttl_ms,
            basis,
            context_tokens,
        })
    }

    fn calibrate(
        &self,
        provider: Provider,
        account: Option<&SessionAccount>,
        observations: Vec<Observation>,
    ) -> Option<(u64, TtlBasis)> {
        let name = provider.executable();
        let id = account.map(|account| account.id.clone());
        let mut state = self.accounts.lock().unwrap_or_else(|p| p.into_inner());
        let Accounts { accounts, unsaved } = &mut *state;
        let accounts = accounts.get_or_insert_with(|| {
            self.path
                .as_deref()
                .and_then(read_document)
                .map(|document| document.accounts)
                .unwrap_or_default()
        });
        let index = accounts
            .iter()
            .position(|known| known.provider == name && known.account == id);
        let mut known = match index {
            Some(index) => accounts.remove(index),
            None => Account {
                provider: name.to_owned(),
                account: id,
                observations: Vec::new(),
            },
        };
        let before = known.observations.clone();
        known.observations.extend(
            observations
                .into_iter()
                .filter(|observation| !before.contains(observation)),
        );
        known.observations.sort_by_key(|observation| observation.at);
        let excess = known.observations.len().saturating_sub(KEPT);
        known.observations.drain(..excess);
        // An observation already dropped as old comes back with every listing
        // while its conversation runs; it teaches nothing.
        let learned = known.observations != before;
        let estimated = estimate(assumed_ttl(provider), &known.observations);
        match index {
            // Most recently observed last, so the oldest is dropped first.
            Some(_) if learned => accounts.push(known),
            Some(index) => accounts.insert(index, known),
            None if known.observations.is_empty() => {}
            None => {
                if accounts.len() == ACCOUNTS {
                    accounts.remove(0);
                }
                accounts.push(known);
            }
        }
        *unsaved |= learned;
        estimated
    }

    /// Writes what was learned since the last save to the calibration file.
    pub(in crate::host) fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let _saving = self.saving.lock().unwrap_or_else(|p| p.into_inner());
        let document = {
            let mut state = self.accounts.lock().unwrap_or_else(|p| p.into_inner());
            if !std::mem::take(&mut state.unsaved) {
                return;
            }
            Document {
                version: VERSION,
                accounts: state.accounts.clone().unwrap_or_default(),
            }
        };
        if let Err(error) = write_document(path, &document) {
            tracing::warn!(%error, "prompt cache observations were not saved");
        }
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

    fn observation(at: u64, gap_minutes: u64, hit: bool) -> Observation {
        Observation {
            at,
            gap_ms: gap_minutes * MINUTE_MS,
            hit,
        }
    }

    fn account() -> SessionAccount {
        SessionAccount {
            id: "codex/system".parse().unwrap(),
            label: None,
            status: None,
            remaining: Vec::new(),
            stale: false,
            observed_at: None,
            ordinary_usage_allowed: None,
        }
    }

    fn requested(observations: Vec<Observation>) -> Activity {
        Activity {
            requested_at: Some(1),
            observations,
            ttl_ms: None,
        }
    }

    #[test]
    fn the_documented_ttl_holds_until_observations_contradict_it() {
        let five = Some(5 * MINUTE_MS);
        let assumed = Some((5 * MINUTE_MS, TtlBasis::Assumed));
        assert_eq!(estimate(five, &[]), assumed);
        // Hits within it, and a single hit or miss against it, change nothing.
        assert_eq!(
            estimate(five, &[observation(1, 3, true), observation(2, 4, false)]),
            assumed
        );
        assert_eq!(estimate(five, &[observation(1, 9, true)]), assumed);
        // Repeated hits after longer gaps prove the cache lasts longer, as
        // far as the shortest of them that most observations agree with.
        assert_eq!(
            estimate(
                five,
                &[
                    observation(1, 9, true),
                    observation(2, 12, true),
                    observation(3, 30, false),
                ]
            ),
            Some((12 * MINUTE_MS, TtlBasis::Observed))
        );
        // Repeated misses within it lower it to what is known to survive.
        assert_eq!(
            estimate(
                five,
                &[
                    observation(1, 3, true),
                    observation(2, 4, false),
                    observation(3, 4, false),
                ]
            ),
            Some((3 * MINUTE_MS, TtlBasis::Observed))
        );
    }

    #[test]
    fn one_lucky_hit_does_not_outweigh_the_misses_around_it() {
        let five = Some(5 * MINUTE_MS);
        let mut observations = vec![observation(1, 9, true)];
        observations.extend((2..6).map(|at| observation(at, 6 + at % 3, false)));
        assert_eq!(
            estimate(five, &observations),
            Some((5 * MINUTE_MS, TtlBasis::Assumed))
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
    fn a_reported_lifetime_is_taken_as_reported() {
        let calibration = Calibration::in_memory();
        let estimated = calibration
            .estimate(
                Provider::Claude,
                None,
                Activity {
                    requested_at: Some(7),
                    // Whatever was observed, the provider said.
                    observations: vec![observation(1, 3, false), observation(2, 4, false)],
                    ttl_ms: Some(60 * MINUTE_MS),
                },
                120_000,
            )
            .unwrap();
        assert_eq!(
            estimated,
            PromptCache {
                requested_at: 7,
                ttl_ms: 60 * MINUTE_MS,
                basis: TtlBasis::Reported,
                context_tokens: 120_000,
            }
        );
        assert_eq!(
            calibration.estimate(Provider::Claude, None, requested(Vec::new()), 0),
            None,
            "Claude has not said how long its cache lasts"
        );
        assert_eq!(
            calibration.estimate(Provider::Codex, None, Activity::default(), 0),
            None,
            "no request was observed"
        );
        assert_eq!(
            calibration.estimate(Provider::Omp, None, requested(Vec::new()), 0),
            None,
            "nothing says how long the cache lasts"
        );
    }

    #[test]
    fn observations_are_collected_once_and_kept_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache").join(FILE_NAME);
        let activity = requested(vec![observation(100, 9, true), observation(200, 12, true)]);
        let calibration = Calibration::kept_at(Some(path.clone()));
        let estimated = calibration
            .estimate(Provider::Codex, None, activity.clone(), 120_000)
            .unwrap();
        assert_eq!(
            (estimated.ttl_ms, estimated.basis),
            (12 * MINUTE_MS, TtlBasis::Observed)
        );
        assert!(!path.exists(), "saved only once asked");
        calibration.save();
        let saved = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Listing again collects nothing new, so nothing is written.
        calibration.estimate(Provider::Codex, None, activity, 120_000);
        calibration.save();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), saved);

        let restarted = Calibration::kept_at(Some(path.clone()));
        let estimated = restarted
            .estimate(Provider::Codex, None, requested(Vec::new()), 1_000)
            .unwrap();
        assert_eq!(estimated.ttl_ms, 12 * MINUTE_MS);
        let saved = read_document(&path).unwrap();
        assert_eq!(saved.accounts.len(), 1);
        assert_eq!(saved.accounts[0].observations.len(), 2);
    }

    #[test]
    fn each_account_is_calibrated_on_its_own() {
        let calibration = Calibration::in_memory();
        let observed = requested(vec![observation(1, 9, true), observation(2, 12, true)]);
        let longer = calibration
            .estimate(Provider::Codex, Some(&account()), observed, 0)
            .unwrap();
        assert_eq!(longer.ttl_ms, 12 * MINUTE_MS);
        let other = calibration
            .estimate(Provider::Codex, None, requested(Vec::new()), 0)
            .unwrap();
        assert_eq!(
            (other.ttl_ms, other.basis),
            (5 * MINUTE_MS, TtlBasis::Assumed)
        );
    }

    #[test]
    fn an_observation_dropped_as_old_is_not_learned_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let calibration = Calibration::kept_at(Some(path.clone()));
        let newer = requested(
            (0..KEPT as u64)
                .map(|at| observation(1_000 + at, 3, true))
                .collect(),
        );
        calibration.estimate(Provider::Codex, None, newer, 0);
        calibration.save();
        std::fs::remove_file(&path).unwrap();
        let old = requested(vec![observation(5, 3, true)]);
        calibration.estimate(Provider::Codex, None, old, 0);
        calibration.save();
        assert!(!path.exists(), "nothing changed, so nothing was written");
    }
}
