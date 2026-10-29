//! Durable intent precedes every reset effect. An interrupted attempt never
//! becomes a new attempt merely because a daemon or CLI restarted.
use super::{Allowance, Outcome, ProviderAccount, identifier_str};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const VERSION: u8 = 1;
const MAX_BYTES: u64 = 256 * 1024;
const MAX_ATTEMPTS: usize = 128;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    credit_id: String,
    idempotency_key: String,
    started_at_ms: u64,
    outcome: Option<Outcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    account_id: String,
    backend_origin: String,
    attempts: Vec<Attempt>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Already recorded and freshly verified. Never consume this credit again.
    Complete(Outcome),
    /// New durable intent, or the SAME original attempt requiring fresh
    /// provider reconciliation before its idempotent request can be retried.
    Pending {
        idempotency_key: String,
        reconcile_first: bool,
    },
}

/// Held only during one actual-account operation, outside Host/global locks.
/// The provider key also deduplicates the same concrete credit across devices.
/// This local file lock alone makes no cross-device serialization claim.
pub(crate) struct Ledger {
    _lock: crate::instance::OwnedLock,
    root: PathBuf,
    path: PathBuf,
    state: State,
    failed: bool,
}

fn digest(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"cagents/codex-reset-credit/v1");
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

/// A concrete credit defines one logical redemption, regardless of which
/// local home, session or device observed it. Origin is checked separately;
/// it cannot cause a different retry key for the same provider account/credit.
fn attempt_key(account: &str, credit: &str) -> String {
    let bytes = digest(&[account, credit]);
    format!(
        "{}-{}-8{}-a{}-{}",
        &bytes[..8],
        &bytes[8..12],
        &bytes[13..16],
        &bytes[17..20],
        &bytes[20..32]
    )
}

impl Ledger {
    pub(crate) fn open(root: &Path, account: &ProviderAccount) -> Result<Self, String> {
        identifier_str(&account.account_id)?;
        identifier_str(&account.backend_origin)?;
        crate::secure_file::prepare_private_directory(root)
            .map_err(|e| format!("Cannot prepare reset-attempt storage: {e}"))?;
        let stem = digest(&[&account.account_id, &account.backend_origin]);
        let lock = crate::instance::owned_lock(
            &root.join(format!("{stem}.lock")),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .map_err(|e| format!("Reset account is busy or its attempt storage is unavailable: {e}"))?;
        let path = root.join(format!("{stem}.json"));
        let state = match crate::secure_file::open_existing_private_file(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("Cannot read reset attempts: {e}"))?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(
                        "Reset-attempt storage exceeds its safe bound; no credit was spent".into(),
                    );
                }
                serde_json::from_slice::<State>(&bytes).map_err(|e| format!("Reset-attempt storage is corrupt; refusing to forget uncertain effects: {e}"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: VERSION,
                account_id: account.account_id.clone(),
                backend_origin: account.backend_origin.clone(),
                attempts: Vec::new(),
            },
            Err(e) => return Err(format!("Cannot safely open reset-attempt storage: {e}")),
        };
        if state.version != VERSION
            || state.account_id != account.account_id
            || state.backend_origin != account.backend_origin
            || state.attempts.len() > MAX_ATTEMPTS
        {
            return Err("Reset-attempt storage has an unsupported version, account or size; no credit was spent".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for attempt in &state.attempts {
            identifier_str(&attempt.credit_id)?;
            if !seen.insert(&attempt.credit_id)
                || attempt.idempotency_key != attempt_key(&state.account_id, &attempt.credit_id)
            {
                return Err(
                    "Reset-attempt storage contains inconsistent attempts; no credit was spent"
                        .into(),
                );
            }
        }
        Ok(Self {
            _lock: lock,
            root: root.to_owned(),
            path,
            state,
            failed: false,
        })
    }

    pub(crate) fn uncertain_credit(&self) -> Result<Option<&str>, String> {
        self.ensure_usable()?;
        Ok(self
            .state
            .attempts
            .iter()
            .find(|attempt| attempt.outcome.is_none())
            .map(|attempt| attempt.credit_id.as_str()))
    }

    pub(crate) fn original_key(&self, credit_id: &str) -> Result<String, String> {
        self.ensure_usable()?;
        identifier_str(credit_id)?;
        Ok(self
            .state
            .attempts
            .iter()
            .find(|attempt| attempt.credit_id == credit_id)
            .map(|attempt| attempt.idempotency_key.clone())
            .unwrap_or_else(|| attempt_key(&self.state.account_id, credit_id)))
    }

    /// The owning lease stays held through actor reservation and journal
    /// admission. Only current terminal records qualify: a revived credit's
    /// outcome is None and cannot resolve an actor's uncertainty.
    pub(crate) fn verified_attempts(
        &self,
        fresh: &Allowance,
    ) -> Result<Vec<super::transport::Verified>, String> {
        self.ensure_usable()?;
        let expected = ProviderAccount {
            account_id: self.state.account_id.clone(),
            backend_origin: self.state.backend_origin.clone(),
        };
        if fresh.account.as_ref() != Some(&expected) || fresh.reset_credits.is_none() {
            return Err(
                "Fresh matching account and reset inventory are required for actor reservation"
                    .into(),
            );
        }
        Ok(self
            .state
            .attempts
            .iter()
            .filter(|attempt| attempt.outcome.is_some())
            .map(|attempt| {
                super::transport::Verified::recorded(super::transport::Attempt {
                    expected: expected.clone(),
                    credit_id: attempt.credit_id.clone(),
                    key: attempt.idempotency_key.clone(),
                })
            })
            .collect())
    }

    pub(crate) fn begin_available(
        &mut self,
        credit_id: &str,
        now_ms: u64,
        fresh: &Allowance,
    ) -> Result<Admission, String> {
        self.ensure_usable()?;
        if self
            .uncertain_credit()?
            .is_some_and(|pending| pending != credit_id)
        {
            return Err("Another reset credit has an uncertain outcome; reconcile that original attempt before selecting a different credit".into());
        }
        if !self.state.attempts.iter().any(|attempt| {
            attempt.credit_id == credit_id
                && !matches!(
                    attempt.outcome,
                    Some(Outcome::NothingToReset | Outcome::NoCredit)
                )
        }) {
            let matching = fresh.account.as_ref().is_some_and(|account| {
                account.account_id == self.state.account_id
                    && account.backend_origin == self.state.backend_origin
            });
            let eligible = fresh
                .reset_credits
                .as_ref()
                .filter(|credits| credits.available_count > 0)
                .and_then(|credits| credits.details.as_ref())
                .is_some_and(|rows| {
                    rows.iter().any(|credit| {
                        credit.id == credit_id
                            && credit.available
                            && credit.supported_type
                            && credit.expires_at_ms.is_none_or(|at| at > now_ms)
                    })
                });
            if !matching
                || !eligible
                || now_ms
                    .checked_sub(fresh.observed_at_ms)
                    .is_none_or(|age| age > 30_000)
            {
                return Err("The selected reset credit is not freshly available on the exact provider account; no intent or effect was issued".into());
            }
        }
        self.begin(credit_id, now_ms)
    }

    fn begin(&mut self, credit_id: &str, now_ms: u64) -> Result<Admission, String> {
        self.ensure_usable()?;
        identifier_str(credit_id)?;
        if let Some(index) = self
            .state
            .attempts
            .iter()
            .position(|attempt| attempt.credit_id == credit_id)
        {
            if matches!(
                self.state.attempts[index].outcome,
                Some(Outcome::NothingToReset | Outcome::NoCredit)
            ) && self
                .state
                .attempts
                .iter()
                .any(|attempt| attempt.credit_id != credit_id && attempt.outcome.is_none())
            {
                return Err("A previous reset has an uncertain outcome; reconcile that credit before reviving another".into());
            }
            let attempt = &mut self.state.attempts[index];
            let key = attempt.idempotency_key.clone();
            match attempt.outcome {
                Some(Outcome::Reset | Outcome::AlreadyRedeemed) => {
                    return Ok(Admission::Complete(attempt.outcome.unwrap()));
                }
                None => {
                    return Ok(Admission::Pending {
                        idempotency_key: key,
                        reconcile_first: true,
                    });
                }
                Some(Outcome::NothingToReset | Outcome::NoCredit) => {
                    attempt.outcome = None;
                    attempt.started_at_ms = now_ms;
                    self.save()?;
                    return Ok(Admission::Pending {
                        idempotency_key: key,
                        reconcile_first: false,
                    });
                }
            }
        }
        if self
            .state
            .attempts
            .iter()
            .any(|attempt| attempt.outcome.is_none())
        {
            return Err("A previous reset has an uncertain outcome; reconcile that credit before selecting another".into());
        }
        if self.state.attempts.len() == MAX_ATTEMPTS {
            return Err(
                "Reset-attempt history is full; refusing to discard deduplication records".into(),
            );
        }
        let key = attempt_key(&self.state.account_id, credit_id);
        self.state.attempts.push(Attempt {
            credit_id: credit_id.into(),
            idempotency_key: key.clone(),
            started_at_ms: now_ms,
            outcome: None,
        });
        self.save()?;
        Ok(Admission::Pending {
            idempotency_key: key,
            reconcile_first: false,
        })
    }

    /// A consume ACK alone is insufficient. The caller must first obtain
    /// these fresh allowance/identity results on the SAME current actor.
    pub(crate) fn finish(
        &mut self,
        credit_id: &str,
        outcome: Outcome,
        refreshed: &Allowance,
    ) -> Result<(), String> {
        self.ensure_usable()?;
        let attempt = self
            .state
            .attempts
            .iter_mut()
            .find(|attempt| attempt.credit_id == credit_id)
            .ok_or("No durable intent exists for this reset credit")?;
        if refreshed.account.as_ref().is_none_or(|account| {
            account.account_id != self.state.account_id
                || account.backend_origin != self.state.backend_origin
        }) || refreshed.observed_at_ms < attempt.started_at_ms
            || refreshed.reset_credits.is_none()
        {
            return Err("Fresh matching account and reset inventory are required before resolving a reset attempt".into());
        }
        if let Some(prior) = attempt.outcome {
            return if prior == outcome {
                Ok(())
            } else {
                Err("Reset attempt already has a different verified outcome".into())
            };
        }
        attempt.outcome = Some(outcome);
        self.save()
    }

    fn ensure_usable(&self) -> Result<(), String> {
        if self.failed {
            Err("Reset-attempt persistence previously failed; reopen and reconcile before continuing".into())
        } else {
            Ok(())
        }
    }

    fn save(&mut self) -> Result<(), String> {
        let bytes = serde_json::to_vec(&self.state).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Reset-attempt storage exceeds its safe bound".into());
        }
        let temporary = self
            .root
            .join(format!(".reset-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            File::open(&self.root)?.sync_all()
        })();
        if result.is_err() {
            self.failed = true;
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|e| format!("Cannot durably record reset attempt; do not send or retry a credit consumption: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn account() -> ProviderAccount {
        ProviderAccount {
            account_id: "actual-account".into(),
            backend_origin: "https://chatgpt.com".into(),
        }
    }
    #[test]
    fn same_credit_across_sessions_and_restart_preserves_uncertain_original_key() {
        let root = tempfile::tempdir().unwrap();
        let mut first = Ledger::open(root.path(), &account()).unwrap();
        let Admission::Pending {
            idempotency_key: key,
            reconcile_first: false,
        } = first.begin("credit-one", 1000).unwrap()
        else {
            panic!()
        };
        assert!(Ledger::open(root.path(), &account()).is_err());
        drop(first);
        let mut restarted = Ledger::open(root.path(), &account()).unwrap();
        assert_eq!(
            restarted.begin("credit-one", 2000).unwrap(),
            Admission::Pending {
                idempotency_key: key.clone(),
                reconcile_first: true
            }
        );
        assert!(restarted.begin("another-credit", 2000).is_err());
        let elsewhere = tempfile::tempdir().unwrap();
        let mut other_device = Ledger::open(elsewhere.path(), &account()).unwrap();
        assert_eq!(
            other_device.begin("credit-one", 2000).unwrap(),
            Admission::Pending {
                idempotency_key: key,
                reconcile_first: false
            }
        );
    }
    #[test]
    fn acknowledged_reset_stays_uncertain_until_matching_fresh_inventory_is_recorded() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path(), &account()).unwrap();
        ledger.begin("credit-one", 1000).unwrap();
        let mut fresh = Allowance {
            observed_at_ms: 999,
            account: Some(account()),
            account_binding_detail: None,
            windows: vec![],
            reset_credits: Some(super::super::ResetCredits {
                available_count: 0,
                details: Some(vec![]),
            }),
        };
        assert!(ledger.finish("credit-one", Outcome::Reset, &fresh).is_err());
        fresh.observed_at_ms = 1100;
        fresh.account.as_mut().unwrap().account_id = "other-account".into();
        assert!(ledger.finish("credit-one", Outcome::Reset, &fresh).is_err());
        fresh.account = Some(account());
        ledger.finish("credit-one", Outcome::Reset, &fresh).unwrap();
        drop(ledger);
        let mut restarted = Ledger::open(root.path(), &account()).unwrap();
        assert_eq!(
            restarted.begin("credit-one", 1200).unwrap(),
            Admission::Complete(Outcome::Reset)
        );
    }
    #[test]
    fn nonconsuming_provider_outcome_can_retry_later_fresh_credit_using_the_same_key() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path(), &account()).unwrap();
        let Admission::Pending {
            idempotency_key: key,
            ..
        } = ledger.begin("credit-one", 1000).unwrap()
        else {
            panic!()
        };
        let mut fresh = Allowance {
            observed_at_ms: 1100,
            account: Some(account()),
            account_binding_detail: None,
            windows: vec![],
            reset_credits: Some(super::super::ResetCredits {
                available_count: 1,
                details: Some(vec![super::super::ResetCredit {
                    id: "credit-one".into(),
                    supported_type: true,
                    available: true,
                    expires_at_ms: None,
                }]),
            }),
        };
        ledger
            .finish("credit-one", Outcome::NothingToReset, &fresh)
            .unwrap();
        drop(ledger);
        let mut ledger = Ledger::open(root.path(), &account()).unwrap();
        fresh.observed_at_ms = 2000;
        assert_eq!(
            ledger.begin_available("credit-one", 2000, &fresh).unwrap(),
            Admission::Pending {
                idempotency_key: key,
                reconcile_first: false
            }
        );
    }
    #[test]
    fn retryable_credit_cannot_bypass_another_credit_uncertainty_or_mutate_the_journal() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path(), &account()).unwrap();
        ledger.begin("credit-a", 1000).unwrap();
        let fresh = Allowance {
            observed_at_ms: 1100,
            account: Some(account()),
            account_binding_detail: None,
            windows: vec![],
            reset_credits: Some(super::super::ResetCredits {
                available_count: 2,
                details: Some(vec![super::super::ResetCredit {
                    id: "credit-a".into(),
                    supported_type: true,
                    available: true,
                    expires_at_ms: None,
                }]),
            }),
        };
        ledger
            .finish("credit-a", Outcome::NothingToReset, &fresh)
            .unwrap();
        ledger.begin("credit-b", 1100).unwrap();
        assert_eq!(ledger.uncertain_credit().unwrap(), Some("credit-b"));
        let before = fs::read(&ledger.path).unwrap();
        assert!(
            ledger
                .begin_available("credit-a", 1200, &fresh)
                .unwrap_err()
                .contains("uncertain")
        );
        assert_eq!(fs::read(&ledger.path).unwrap(), before);
        assert_eq!(
            ledger.state.attempts[0].outcome,
            Some(Outcome::NothingToReset)
        );
        assert!(ledger.state.attempts[1].outcome.is_none());
    }
    #[test]
    fn corrupt_or_future_attempt_state_never_becomes_an_empty_new_history() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path(), &account()).unwrap();
        let path = ledger.path.clone();
        drop(ledger);
        fs::write(&path, b"not json").unwrap();
        assert!(Ledger::open(root.path(), &account()).is_err());
        fs::write(&path, br#"{"version":2,"account_id":"actual-account","backend_origin":"https://chatgpt.com","attempts":[]}"#).unwrap();
        assert!(Ledger::open(root.path(), &account()).is_err());
    }
}
