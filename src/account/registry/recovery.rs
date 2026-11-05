//! Checked registration of an existing, independently confirmed managed home.
//!
//! Discovery is metadata only. The host's sealed recovery scope owns the exact
//! home and status command; neither an identity nor a discovery candidate is
//! sufficient to enter these Store operations on its own.

use std::io;

use super::{Registered, Registry, Store};
use crate::account::{
    Account, AccountId, Identity,
    home::Homes,
    recovery::{Candidate, HomeStatus, inspect},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestoredManaged {
    pub candidate: Candidate,
    /// The restored row remains registered, but archived, when this is set.
    pub duplicate_of: Option<AccountId>,
}

struct RestorePlan {
    row: Registered,
    retained: Option<Account>,
}

fn supported_registry(registry: &Registry) -> io::Result<()> {
    if registry.is_newer() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "The accounts were saved by a newer cagents; recovery was not committed",
        ));
    }
    Ok(())
}

/// Logical checks only. Physical aliases and the original directory pin are
/// additionally checked by the sealed host scope under the Store lease.
fn unrepresented(registry: &Registry, homes: &Homes, candidate: &Candidate) -> io::Result<()> {
    supported_registry(registry)?;
    if !Account::PROVIDERS.contains(&candidate.provider) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "This provider manages its own account homes",
        ));
    }
    if registry.find(candidate.key).is_some()
        || registry
            .pending()
            .iter()
            .any(|pending| pending.key == candidate.key)
        || registry.is_archived(candidate.key)
        || registry.contains_managed_home(homes, candidate.provider, candidate.key)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "The account home is already represented; reload accounts before restoring it",
        ));
    }
    Ok(())
}

fn stable_email(identity: &Identity, subject: &str) -> io::Result<()> {
    if identity
        .email
        .as_deref()
        .is_none_or(|email| email.trim().is_empty() || email.chars().any(char::is_control))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{subject} did not report a stable account email; recovery was not committed"),
        ));
    }
    Ok(())
}

/// Pure planning never reads provider files or mutates the registry. `system`
/// is either a confirmed System identity or a confirmed absence of that login;
/// callers must not turn a failed/unverified System probe into `None`.
fn plan(
    registry: &Registry,
    homes: &Homes,
    candidate: &Candidate,
    identity: &Identity,
    system: Option<&Identity>,
    now: u64,
) -> io::Result<RestorePlan> {
    unrepresented(registry, homes, candidate)?;
    stable_email(identity, "The surviving home")?;
    if let Some(system) = system {
        stable_email(system, "The System account")?;
    }
    let retained = if system.is_some_and(|system| system.same_login(identity)) {
        Some(Account::system(candidate.provider))
    } else {
        registry
            .registered()
            .iter()
            .find(|row| {
                row.provider == candidate.provider
                    && !registry.is_archived(row.key)
                    && row.identity().same_login(identity)
            })
            .map(|row| match &row.linked_home {
                Some(path) => Account::linked(row.provider, row.key, path.clone()),
                None => Account::managed(row.provider, row.key, homes.path(row.provider, row.key)),
            })
    };
    Ok(RestorePlan {
        row: Registered {
            key: candidate.key,
            provider: candidate.provider,
            linked_home: None,
            label: None,
            email: identity.email.clone(),
            organization: identity.organization.clone(),
            added_at: now,
        },
        retained,
    })
}

fn restore(
    registry: &mut Registry,
    homes: &Homes,
    candidate: &Candidate,
    identity: &Identity,
    system: Option<&Identity>,
    now: u64,
) -> io::Result<RestoredManaged> {
    let planned = plan(registry, homes, candidate, identity, system, now)?;
    let duplicate_of = planned.retained.as_ref().map(Account::id);
    let mut next = registry.clone();
    // Ordinary register() refuses duplicates before insertion. Recovery keeps
    // the original key even for duplicates, so saved conversations can resume.
    next.accounts.push(planned.row);
    if let Some(retained) = &planned.retained
        && !next.archive_duplicate(candidate.key, retained)
    {
        return Err(io::Error::other(
            "The duplicate account changed before recovery could be committed",
        ));
    }
    *registry = next;
    Ok(RestoredManaged {
        candidate: candidate.clone(),
        duplicate_of,
    })
}

/// Held across exact status-process creation only, never a provider turn.
/// Drop releases the registry lock before the lifecycle lock.
#[cfg(feature = "host")]
pub(crate) struct StatusLease {
    _registry: crate::instance::OwnedLock,
    _lifecycle: crate::instance::OwnedLock,
}

#[cfg(feature = "host")]
fn same_store(store: &Store, recovery: &crate::host::AccountRecovery) -> io::Result<()> {
    if store.directory != recovery.store().directory {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The recovery scope belongs to a different account registry",
        ));
    }
    Ok(())
}

#[cfg(feature = "host")]
impl Store {
    /// The sealed status permit calls this only after exact command validation.
    pub(crate) fn try_recovery_status_lease(
        &self,
        recovery: &crate::host::AccountRecovery,
    ) -> io::Result<StatusLease> {
        same_store(self, recovery)?;
        let lifecycle = recovery.homes().try_lifecycle_lock()?;
        let registry_lock = self.try_lock()?;
        let registry = super::read(&self.path())?;
        supported_registry(&registry)?;
        recovery
            .verify_registry(&registry)
            .map_err(io::Error::other)?;
        unrepresented(&registry, recovery.homes(), recovery.candidate())?;
        Ok(StatusLease {
            _registry: registry_lock,
            _lifecycle: lifecycle,
        })
    }

    pub(crate) fn try_restore_managed_reserved(
        &self,
        recovery: &crate::host::AccountRecovery,
        identity: &Identity,
        system: Option<&Identity>,
        now: u64,
    ) -> io::Result<(Registry, RestoredManaged)> {
        same_store(self, recovery)?;
        let homes = recovery.homes();
        let candidate = recovery.candidate();
        let _lifecycle = homes.try_lifecycle_lock()?;
        self.update_checked_with_mode(
            |registry| {
                recovery
                    .verify_registry(registry)
                    .map_err(io::Error::other)?;
                match inspect(homes, registry, candidate.provider, candidate.key)? {
                    HomeStatus::Available(current) if current == *candidate => {}
                    HomeStatus::Missing => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "The surviving home disappeared before recovery was committed",
                        ));
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "The account home changed before recovery was committed",
                        ));
                    }
                }
                restore(registry, homes, candidate, identity, system, now)
            },
            true,
        )
    }
}

#[cfg(test)]
mod tests;
