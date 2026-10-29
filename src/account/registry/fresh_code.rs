//! Stable ownership for a separately consented fresh Codex code login. This
//! collection lives in accounts.toml; visibility never mints a spawn permit.
use super::*;
use crate::account::fresh_code::{CodeOwner, CodeState, ConditionalPlan, FreshCodeStart};
use crate::model::HostSessionProof;

const FRESH_CODE_MIN_VERSION: u32 = 7;

pub(crate) enum CodeClaim {
    Fresh(CodeOwner),
    /// The exact attempt already owns this home. This is metadata only.
    Retained(CodeOwner),
}

pub(crate) enum CodeRegistration {
    Registered(CodeOwner),
    DuplicateRetained {
        owner: CodeOwner,
        existing: AccountId,
    },
}

fn conflict(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

impl Registry {
    pub(crate) fn fresh_code_owner(&self, request_id: Uuid) -> Option<&CodeOwner> {
        self.fresh_code_owners
            .iter()
            .find(|owner| owner.binding.request_id == request_id)
    }

    pub(super) fn validate_fresh_code(&self) -> io::Result<()> {
        let mut requests = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        let mut attempts = std::collections::HashSet::new();
        let mut terminals = std::collections::HashSet::new();
        for owner in &self.fresh_code_owners {
            owner
                .validate()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if self.version < FRESH_CODE_MIN_VERSION
                || !requests.insert(owner.binding.request_id)
                || !keys.insert(owner.binding.key)
                || !attempts.insert(owner.attempt.id)
                || !terminals.insert(owner.attempt.terminal_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "The fresh code ownership is inconsistent",
                ));
            }
            let preparation = self.exact_preparation(&owner.binding)?;
            if !matches!(
                preparation.state,
                PreparationState::Created
                    | PreparationState::CancelledRetained
                    | PreparationState::Registered
            ) || ((owner.state == CodeState::Registered)
                != (preparation.state == PreparationState::Registered))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "The fresh code owner lost its preparation",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn require_code_owner(&self, owner: &CodeOwner) -> io::Result<()> {
        if self.fresh_code_owner(owner.binding.request_id) != Some(owner) {
            return Err(conflict("The fresh code attempt changed"));
        }
        let preparation = self.exact_preparation(&owner.binding)?;
        if preparation.state != PreparationState::Created
            || !self.pending_is_eligible(owner.binding.payload.provider, owner.binding.key)
        {
            return Err(conflict("The fresh code preparation is no longer eligible"));
        }
        Ok(())
    }
}

impl Store {
    /// Observation must not use the ordinary registry reader: that reader
    /// intentionally repairs legacy file permissions for mutation callers.
    fn fresh_code_observation(&self) -> io::Result<Registry> {
        let directory = if self.directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &self.directory
        };
        let path = self.path();
        crate::secure_file::reject_symlink_components(&path)?;
        match crate::secure_file::validate_owned_directory(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Registry::default());
            }
            Err(error) => return Err(error),
        }
        let file = match crate::secure_file::open_existing_private_file_readonly(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Registry::default());
            }
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the accounts file is too large",
            ));
        }
        let registry: Registry = toml::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        registry.validate_preparations()?;
        Ok(registry)
    }

    /// Resolve a lost Plan reply by its actual original request UUID. This
    /// reads existing metadata only; it never repeats Plan or invents a key.
    pub(crate) fn inspect_fresh_code_plan(
        &self,
        request: &ConditionalPlan,
    ) -> io::Result<Option<Preparation>> {
        request.validate()?;
        let registry = self.fresh_code_observation()?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts file is newer than this application",
            ));
        }
        if registry.installation_id() != Some(request.expected_installation) {
            return Err(conflict(
                "The original installation cannot be confirmed for this plan observation",
            ));
        }
        let observed = registry.preparation(request.request_id);
        if observed.is_some_and(|record| record.binding.payload != request.payload) {
            return Err(conflict(
                "The observed fresh-code plan has different options",
            ));
        }
        Ok(observed.cloned())
    }

    /// Plan only in the already observed installation. No absent or raced
    /// identity is initialized by this separate physical action.
    pub(crate) fn plan_fresh_code(&self, request: &ConditionalPlan) -> io::Result<Preparation> {
        request.validate()?;
        if self.inspect_installation_id()? != Some(request.expected_installation) {
            return Err(conflict(
                "The original installation changed before the fresh code plan",
            ));
        }
        self.preparation_transaction(true, |registry| {
            if registry.installation != Some(request.expected_installation) {
                return Err(conflict(
                    "The original installation changed before the fresh code plan",
                ));
            }
            if let Some(existing) = registry.preparation(request.request_id) {
                if existing.binding.payload != request.payload {
                    return Err(conflict(
                        "The fresh code request was reused with different options",
                    ));
                }
                return Ok(existing.clone());
            }
            let planned = Preparation {
                binding: PreparationBinding {
                    request_id: request.request_id,
                    installation_id: request.expected_installation,
                    key: Uuid::new_v4(),
                    payload: request.payload.clone(),
                },
                state: PreparationState::Planned,
            };
            registry.preparations.push(planned.clone());
            Ok(planned)
        })
    }

    /// A new owner is persisted before entering the terminal constructor. The
    /// returned metadata must be paired with the live original creation pin;
    /// neither this result nor an equal replay alone grants spawn authority.
    pub(crate) fn claim_fresh_code(
        &self,
        request: &FreshCodeStart,
        host: &HostSessionProof,
    ) -> io::Result<CodeClaim> {
        request.validate()?;
        if !host.is_complete() {
            return Err(conflict(
                "The original code-login host proof is unavailable",
            ));
        }
        self.claim_fresh_code_with_sync(request, host, |namespace| namespace.sync())
    }

    fn claim_fresh_code_with_sync(
        &self,
        request: &FreshCodeStart,
        host: &HostSessionProof,
        sync: impl FnMut(&crate::account::durability::Directories) -> io::Result<()>,
    ) -> io::Result<CodeClaim> {
        self.preparation_transaction_with_sync(
            true,
            |registry| {
                request.validate()?;
                if !host.is_complete() {
                    return Err(conflict(
                        "The original code-login host proof is unavailable",
                    ));
                }
                let preparation = registry.exact_preparation(&request.binding)?;
                if preparation.state != PreparationState::Created
                    || !registry
                        .pending_is_eligible(request.binding.payload.provider, request.binding.key)
                {
                    return Err(conflict(
                        "The fresh code home is not currently Created and pending",
                    ));
                }
                if let Some(existing) = registry.fresh_code_owner(request.binding.request_id) {
                    if !existing.matches(request) || &existing.host != host {
                        return Err(conflict(
                            "Another code-login attempt already owns this preparation",
                        ));
                    }
                    return Ok(CodeClaim::Retained(existing.clone()));
                }
                if registry.fresh_code_owners.iter().any(|owner| {
                    owner.binding.key == request.binding.key
                        || owner.attempt.id == request.attempt.id
                        || owner.attempt.terminal_id == request.attempt.terminal_id
                }) {
                    return Err(conflict(
                        "The fresh code attempt or terminal is already owned",
                    ));
                }
                let owner = CodeOwner {
                    binding: request.binding.clone(),
                    attempt: request.attempt.clone(),
                    host: host.clone(),
                    state: CodeState::Owned,
                };
                registry.fresh_code_owners.push(owner.clone());
                Ok(CodeClaim::Fresh(owner))
            },
            sync,
        )
    }

    /// Exact read reconciliation grants no constructor or durability receipt.
    pub(crate) fn observe_fresh_code(
        &self,
        request: &FreshCodeStart,
    ) -> io::Result<Option<CodeOwner>> {
        request.validate()?;
        let registry = self.fresh_code_observation()?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts file is newer than this application",
            ));
        }
        registry.exact_preparation(&request.binding)?;
        let owner = registry.fresh_code_owner(request.binding.request_id);
        if owner.is_some_and(|owner| !owner.matches(request)) {
            return Err(conflict(
                "The retained code-login owner differs from this attempt",
            ));
        }
        Ok(owner.cloned())
    }

    pub(crate) fn code_started(&self, owner: &CodeOwner) -> io::Result<CodeOwner> {
        self.preparation_transaction(true, |registry| {
            let mut started = owner.clone();
            started.state = CodeState::Started;
            let saved = registry
                .fresh_code_owner(owner.binding.request_id)
                .ok_or_else(|| conflict("The fresh code owner is missing"))?;
            if saved == &started {
                return Ok(started);
            }
            if owner.state != CodeState::Owned || saved != owner {
                return Err(conflict(
                    "The code-login start belongs to another owner phase",
                ));
            }
            // A cancellation after actual spawn does not erase that physical
            // fact. It still fences every later probe/registration operation.
            let index = registry
                .fresh_code_owners
                .iter()
                .position(|saved| saved.binding.request_id == owner.binding.request_id)
                .unwrap();
            registry.fresh_code_owners[index] = started.clone();
            Ok(started)
        })
    }

    /// Official probe data reaches this method only through the bound host
    /// owner. The final callback revalidates live root/caller/physical scope
    /// under the SAME lifecycle and registry leases as registration.
    pub(crate) fn register_fresh_code(
        &self,
        homes: &Homes,
        owner: &CodeOwner,
        managed: Registered,
        system: &[(Provider, Identity)],
        validate_live: impl FnOnce(&Registry) -> io::Result<()>,
    ) -> io::Result<CodeRegistration> {
        owner.validate()?;
        if owner.state != CodeState::Started
            || managed.key != owner.binding.key
            || managed.provider != owner.binding.payload.provider
            || managed.linked_home.is_some()
        {
            return Err(conflict(
                "The code registration does not match its actual started owner",
            ));
        }
        let _lifecycle = homes.try_lifecycle_lock()?;
        self.preparation_transaction(true, |registry| {
            registry.require_code_owner(owner)?;
            validate_live(registry)?;
            let mut settled = owner.clone();
            let duplicate = match registry.register_managed(managed, system) {
                Ok(()) => {
                    settled.state = CodeState::Registered;
                    None
                }
                Err(RegistrationError::Duplicate(existing)) => {
                    settled.state = CodeState::DuplicateRetained;
                    Some(existing)
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            let index = registry
                .fresh_code_owners
                .iter()
                .position(|saved| saved.binding.request_id == owner.binding.request_id)
                .unwrap();
            registry.fresh_code_owners[index] = settled.clone();
            Ok(match duplicate {
                Some(existing) => CodeRegistration::DuplicateRetained {
                    owner: settled,
                    existing,
                },
                None => CodeRegistration::Registered(settled),
            })
        })
    }

    /// Extend the actual process-start lease. The caller's root/connection
    /// checks run while this SAME lock fences Cancel through process creation.
    #[cfg(feature = "host")]
    pub(crate) fn authorize_fresh_code_spawn(
        &self,
        homes: &Homes,
        owner: &CodeOwner,
        validate_live: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<LaunchAccountLease> {
        if owner.state != CodeState::Owned {
            return Err(conflict(
                "Only a newly owned code attempt can enter process creation",
            ));
        }
        let lock = homes.lifecycle_lock()?;
        let registry = read(&self.path())?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts file is newer than this application",
            ));
        }
        registry.require_code_owner(owner)?;
        validate_live()?;
        Ok(LaunchAccountLease { _lock: lock })
    }
}

#[cfg(test)]
mod tests;
