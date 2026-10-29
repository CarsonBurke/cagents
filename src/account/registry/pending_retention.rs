//! Stable identities for pending homes written before planned preparations.
//! These records retain metadata and never authorize creation or deletion.
use super::*;

impl Registry {
    pub fn installation_id(&self) -> Option<Uuid> {
        self.installation
    }

    pub fn existing_pending(&self, key: Uuid) -> Option<&ExistingPending> {
        self.existing_pending
            .iter()
            .find(|entry| entry.binding.key == key)
    }

    fn exact_existing_pending(
        &self,
        binding: &ExistingPendingBinding,
    ) -> io::Result<&ExistingPending> {
        self.existing_pending(binding.key)
            .filter(|entry| {
                &entry.binding == binding && self.installation == Some(binding.installation_id)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "The pending sign-in binding does not match this installation",
                )
            })
    }

    pub(super) fn validate_existing_pending(&self) -> io::Result<()> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "The accounts file has inconsistent retained sign-in records",
            )
        };
        let mut keys = std::collections::HashSet::new();
        let mut generations = std::collections::HashSet::new();
        let mut cancellations = std::collections::HashSet::new();
        for entry in &self.existing_pending {
            let binding = &entry.binding;
            if self.version < VERSION
                || binding.installation_id.is_nil()
                || self.installation != Some(binding.installation_id)
                || binding.key.is_nil()
                || binding.generation_id.is_nil()
                || !Account::PROVIDERS.contains(&binding.provider)
                || !keys.insert(binding.key)
                || !generations.insert(binding.generation_id)
                || self
                    .preparations
                    .iter()
                    .any(|entry| entry.binding.key == binding.key)
                || entry
                    .cancellation_ids
                    .iter()
                    .any(|id| id.is_nil() || !cancellations.insert(*id))
            {
                return Err(invalid());
            }
            let pending: Vec<_> = self
                .pending
                .iter()
                .filter(|row| row.key == binding.key)
                .collect();
            let registered = self.find(binding.key);
            if entry.state == ExistingPendingState::Registered {
                if !pending.is_empty()
                    || registered.is_some_and(|row| {
                        row.provider != binding.provider || row.linked_home.is_some()
                    })
                {
                    return Err(invalid());
                }
            } else if pending.len() != 1
                || registered.is_some()
                || pending[0].provider != binding.provider
                || pending[0].started_at != binding.started_at
            {
                return Err(invalid());
            }
            if entry.state == ExistingPendingState::CancelledRetained
                && entry.cancellation_ids.is_empty()
            {
                return Err(invalid());
            }
            if entry.state == ExistingPendingState::Pending && !entry.cancellation_ids.is_empty() {
                return Err(invalid());
            }
        }
        Ok(())
    }

    /// Called only while committing the actual pending row. A key has one
    /// adopted incarnation forever; changing its timestamp cannot reset it.
    pub(super) fn adopt_existing_pending(
        &mut self,
        provider: Provider,
        key: Uuid,
        started_at: u64,
    ) -> io::Result<ExistingPending> {
        if self
            .preparations
            .iter()
            .any(|entry| entry.binding.key == key)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "This home belongs to a planned account setup",
            ));
        }
        if let Some(existing) = self.existing_pending(key) {
            if existing.binding.provider != provider || existing.binding.started_at != started_at {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "The pending sign-in observation is stale",
                ));
            }
            return Ok(existing.clone());
        }
        if key.is_nil()
            || !Account::PROVIDERS.contains(&provider)
            || !self.pending.iter().any(|row| {
                row.key == key && row.provider == provider && row.started_at == started_at
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "The observed pending sign-in is no longer available",
            ));
        }
        let installation_id = *self.installation.get_or_insert_with(Uuid::new_v4);
        let entry = ExistingPending {
            binding: ExistingPendingBinding {
                installation_id,
                provider,
                key,
                started_at,
                generation_id: Uuid::new_v4(),
            },
            state: ExistingPendingState::Pending,
            cancellation_ids: Vec::new(),
        };
        self.existing_pending.push(entry.clone());
        Ok(entry)
    }

    fn resolve_existing_pending(
        &mut self,
        target: &ExistingPendingTarget,
    ) -> io::Result<ExistingPending> {
        match target {
            ExistingPendingTarget::Bound { binding } => {
                self.exact_existing_pending(binding).cloned()
            }
            ExistingPendingTarget::Observed {
                provider,
                key,
                started_at,
                expected_installation,
            } => {
                // Resolve retained identity before requiring a live Pending row:
                // registration can have won and removed it before this request.
                if let Some(entry) = self.existing_pending(*key) {
                    if entry.binding.provider != *provider
                        || entry.binding.started_at != *started_at
                        || expected_installation
                            .is_some_and(|id| id != entry.binding.installation_id)
                        || self.installation != Some(entry.binding.installation_id)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "The pending sign-in observation is stale",
                        ));
                    }
                    return Ok(entry.clone());
                }
                // A missing installation identity only bootstraps an untouched
                // legacy registry. It cannot adopt a new row after identity is
                // established by another operation; refresh that observation.
                if *expected_installation != self.installation {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Refresh the pending sign-in before resuming it",
                    ));
                }
                self.adopt_existing_pending(*provider, *key, *started_at)
            }
        }
    }
}

impl Store {
    pub fn resume_existing_pending(
        &self,
        target: &ExistingPendingTarget,
    ) -> io::Result<ExistingPending> {
        self.resume_existing_pending_with_mode(target, false)
    }

    pub(crate) fn try_resume_existing_pending(
        &self,
        target: &ExistingPendingTarget,
    ) -> io::Result<ExistingPending> {
        self.resume_existing_pending_with_mode(target, true)
    }

    fn resume_existing_pending_with_mode(
        &self,
        target: &ExistingPendingTarget,
        nonblocking: bool,
    ) -> io::Result<ExistingPending> {
        self.preparation_transaction(nonblocking, |registry| {
            registry.resolve_existing_pending(target)
        })
    }

    pub fn cancel_existing_pending(
        &self,
        homes: &Homes,
        operation_id: Uuid,
        binding: &ExistingPendingBinding,
    ) -> io::Result<ExistingPending> {
        self.cancel_existing_pending_with_mode(homes, operation_id, binding, false)
    }

    pub(crate) fn try_cancel_existing_pending(
        &self,
        homes: &Homes,
        operation_id: Uuid,
        binding: &ExistingPendingBinding,
    ) -> io::Result<ExistingPending> {
        self.cancel_existing_pending_with_mode(homes, operation_id, binding, true)
    }

    fn cancel_existing_pending_with_mode(
        &self,
        homes: &Homes,
        operation_id: Uuid,
        binding: &ExistingPendingBinding,
        nonblocking: bool,
    ) -> io::Result<ExistingPending> {
        if operation_id.is_nil() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "A cancellation request needs its original identity",
            ));
        }
        let _lifecycle = if nonblocking {
            homes.try_lifecycle_lock()?
        } else {
            homes.lifecycle_lock()?
        };
        self.preparation_transaction(nonblocking, |registry| {
            if let Some(entry) = registry
                .existing_pending
                .iter()
                .find(|entry| entry.cancellation_ids.contains(&operation_id))
            {
                if &entry.binding != binding {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "The cancellation request belongs to another sign-in",
                    ));
                }
            }
            let exact = registry.exact_existing_pending(binding)?.clone();
            if exact.state != ExistingPendingState::Registered
                && registry.registered_home_is_eligible(homes, binding.provider, binding.key)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "The account home is already registered; cancellation kept its registration",
                ));
            }
            let entry = registry
                .existing_pending
                .iter_mut()
                .find(|entry| entry.binding == *binding)
                .unwrap();
            if entry.state == ExistingPendingState::Pending {
                entry.state = ExistingPendingState::CancelledRetained;
            }
            if !entry.cancellation_ids.contains(&operation_id) {
                entry.cancellation_ids.push(operation_id);
            }
            Ok(entry.clone())
        })
    }
}
