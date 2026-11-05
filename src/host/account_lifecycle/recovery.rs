//! The only permission to ask an unregistered managed home who is signed in.
//!
//! Discovery is not launch admission. This context retains the daemon's
//! existing reservation, including its exact physical basis, until every
//! blocking spawn attempt and its checked registration have finished.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    account::{
        Account, AccountHome,
        home::Homes,
        recovery::{Candidate, HomeStatus, inspect},
        registry::{Registry, Store},
    },
    agent::Provider,
};

use super::{Reservation, Reserved};

#[derive(Clone)]
pub(crate) struct Recovery {
    owner: Arc<Reservation>,
    captured: Arc<Reserved>,
    homes: Homes,
    candidate: Candidate,
    account: Account,
    system: Option<Account>,
}

impl Recovery {
    pub(crate) fn new(
        owner: Arc<Reservation>,
        homes: Homes,
        candidate: Candidate,
    ) -> Result<Self, String> {
        let account = Account::managed(
            candidate.provider,
            candidate.key,
            homes.path(candidate.provider, candidate.key),
        );
        if owner.create.is_some() || owner.accounts.len() != 2 {
            return Err("Restoring an account requires its own registration reservation".into());
        }
        let managed_index = owner
            .accounts
            .iter()
            .position(|value| value == &account)
            .ok_or(
                "The preserved account does not match its reserved provider, key and directory",
            )?;
        let system_index = owner
            .accounts
            .iter()
            .position(|value| value.provider == candidate.provider && value.is_system())
            .ok_or("The provider's system account was not reserved for duplicate verification")?;
        if managed_index == system_index
            || owner.registered.len() != 1
            || !owner
                .registered
                .iter()
                .any(|(key, row, pending, archived)| {
                    *key == candidate.key && row.is_none() && pending.is_none() && !archived
                })
        {
            return Err("This account is already registered or has an unfinished sign-in".into());
        }
        owner.verify()?;
        let captured = owner.current()?;
        if captured.homes.len() != 2 || captured.homes[managed_index].pin.is_none() {
            return Err("The preserved account no longer has its reserved directory".into());
        }
        let system = captured.homes[system_index].pin.as_ref().map(|_| Account {
            provider: candidate.provider,
            home: AccountHome::SystemAt {
                path: captured.homes[system_index].path.clone(),
            },
        });
        let recovery = Self {
            owner,
            captured,
            homes,
            candidate,
            account,
            system,
        };
        let registry = recovery
            .store()
            .try_load()
            .map_err(|error| error.to_string())?;
        recovery.verify_registry(&registry)?;
        Ok(recovery)
    }

    pub(crate) fn candidate(&self) -> &Candidate {
        &self.candidate
    }
    pub(crate) fn homes(&self) -> &Homes {
        &self.homes
    }
    pub(crate) fn store(&self) -> &Store {
        &self.owner.store
    }
    pub(crate) fn account(&self) -> Account {
        self.account.clone()
    }

    /// `None` means the originally reserved system path was absent. The
    /// reservation verifies that absence again before registration; it is
    /// never inferred from a missing status response or a failed probe.
    pub(crate) fn system(&self) -> Option<Account> {
        self.system.clone()
    }

    /// Invoked with the Store's actual registry lease already held. Never
    /// call owner.verify() here: that would acquire the same flock again.
    pub(crate) fn verify_registry(&self, registry: &Registry) -> Result<(), String> {
        if !Arc::ptr_eq(&self.captured, &self.owner.current()?) {
            return Err("The account reservation changed while restoration was pending".into());
        }
        self.owner.verify_registry(registry)?;
        match inspect(&self.homes, registry, self.candidate.provider, self.candidate.key)
            .map_err(|error| error.to_string())?
        {
            HomeStatus::Available(candidate) if candidate == self.candidate => Ok(()),
            HomeStatus::Ambiguous => Err("More than one provider home has this account key; no account was restored".into()),
            _ => Err("The preserved account changed while restoration was pending; reload accounts and retry".into()),
        }
    }

    pub(crate) fn status_permit(
        &self,
        account: Account,
        program: PathBuf,
    ) -> Result<StatusPermit, String> {
        if account != self.account && self.system.as_ref() != Some(&account) {
            return Err("This provider status request is outside its account reservation".into());
        }
        if !program.is_absolute()
            || program
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err("Provider status requires an absolute executable path".into());
        }
        Ok(StatusPermit {
            recovery: self.clone(),
            account,
            program,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_install(
        host: &super::super::state::SharedHost,
        store: Store,
        homes: &Homes,
        candidate: Candidate,
    ) -> Result<Self, String> {
        let account = Account::managed(
            candidate.provider,
            candidate.key,
            homes.path(candidate.provider, candidate.key),
        );
        let system = Account {
            provider: candidate.provider,
            home: AccountHome::SystemAt {
                path: homes
                    .path(candidate.provider, candidate.key)
                    .with_file_name(".recovery-system-fixture"),
            },
        };
        let owner = Reservation::test_install(host, &[account, system], store, homes)
            .map_err(|error| error.message)?;
        Self::new(Arc::new(owner), homes.clone(), candidate)
    }
}

/// The command subject and executable are sealed before entering the async
/// transport. Cloning this permit keeps the original reservation alive; the
/// reservation never retains a permit, so cancellation cannot create a cycle.
#[derive(Clone)]
pub(crate) struct StatusPermit {
    recovery: Recovery,
    account: Account,
    program: PathBuf,
}

impl StatusPermit {
    pub(crate) fn account(&self) -> &Account {
        &self.account
    }
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    /// Acquire immediately around EACH actual spawn attempt, never around
    /// a provider response or an executable-busy retry delay.
    pub(crate) fn acquire(
        &self,
        command: &std::process::Command,
    ) -> io::Result<crate::account::registry::recovery::StatusLease> {
        if command.get_program() != self.program.as_os_str() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Provider status executable differs from its reserved command",
            ));
        }
        let expected: &[&str] = match self.account.provider {
            Provider::Claude => &["auth", "status", "--json"],
            Provider::Codex => &["app-server"],
            Provider::Omp => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "This provider manages its own accounts",
                ));
            }
        };
        let arguments = command.get_args().collect::<Vec<_>>();
        if arguments.len() != expected.len()
            || arguments
                .iter()
                .zip(expected)
                .any(|(argument, expected)| *argument != std::ffi::OsStr::new(expected))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Only the provider's read-only account status command can restore this home",
            ));
        }
        let home = self
            .account
            .resolved_home()
            .ok_or_else(|| io::Error::other("The status account has no reserved directory"))?;
        let variable = crate::account::home_variable(self.account.provider)
            .ok_or_else(|| io::Error::other("The provider has no account-home variable"))?;
        let selected = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new(variable));
        if selected.and_then(|(_, value)| value) != Some(home.as_os_str()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Provider status does not select its exact reserved account directory",
            ));
        }
        for variable in self.account.credential_overrides() {
            if !command
                .get_envs()
                .any(|(name, value)| name == std::ffi::OsStr::new(variable) && value.is_none())
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Provider status must use the selected managed account's own login",
                ));
            }
        }
        self.recovery
            .store()
            .try_recovery_status_lease(&self.recovery)
    }
}

#[cfg(test)]
mod tests;
