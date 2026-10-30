//! Durable identity and monotonic retention for an account setup attempt.
//! A preparation records no credentials or filesystem paths and grants no
//! deletion authority. A consumed attempt is never restarted from discovery.
use super::*;

impl Registry {
    pub fn preparation(&self, request_id: Uuid) -> Option<&Preparation> {
        self.preparations
            .iter()
            .find(|entry| entry.binding.request_id == request_id)
    }

    pub fn preparation_for_home(&self, key: Uuid) -> Option<&Preparation> {
        self.preparations
            .iter()
            .find(|entry| entry.binding.key == key)
    }

    pub fn preparation_for_key(&self, provider: Provider, key: Uuid) -> Option<&Preparation> {
        self.preparation_for_home(key)
            .filter(|entry| entry.binding.payload.provider == provider)
    }

    fn exact_preparation(&self, binding: &PreparationBinding) -> io::Result<&Preparation> {
        self.preparation(binding.request_id)
            .filter(|entry| {
                &entry.binding == binding && self.installation == Some(binding.installation_id)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "The account setup binding does not match this installation",
                )
            })
    }

    pub(super) fn validate_preparations(&self) -> io::Result<()> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "The accounts file has inconsistent setup records",
            )
        };
        self.validate_existing_pending()?;
        let mut registered_keys = std::collections::HashSet::new();
        if self
            .accounts
            .iter()
            .any(|account| !registered_keys.insert(account.key))
        {
            return Err(invalid());
        }
        let mut pending_keys = std::collections::HashSet::new();
        if self
            .pending
            .iter()
            .any(|row| !pending_keys.insert(row.key) || registered_keys.contains(&row.key))
        {
            return Err(invalid());
        }
        if self.preparations.is_empty() && self.installation.is_none() {
            return Ok(());
        }
        if self.version < VERSION || self.installation.is_none_or(|id| id.is_nil()) {
            return Err(invalid());
        }
        let mut requests = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for entry in &self.preparations {
            let binding = &entry.binding;
            if binding.request_id.is_nil()
                || binding.key.is_nil()
                || Some(binding.installation_id) != self.installation
                || !Account::PROVIDERS.contains(&binding.payload.provider)
                || !requests.insert(binding.request_id)
                || !keys.insert(binding.key)
            {
                return Err(invalid());
            }
            let pending = self.pending.iter().find(|row| row.key == binding.key);
            let registered = self.find(binding.key);
            if pending.is_some_and(|row| row.provider != binding.payload.provider)
                || self
                    .pending
                    .iter()
                    .filter(|row| row.key == binding.key)
                    .count()
                    > 1
                || (entry.state == PreparationState::Created && pending.is_none())
                || (entry.state == PreparationState::Registered && pending.is_some())
                || (matches!(
                    entry.state,
                    PreparationState::Planned | PreparationState::Uncertain
                ) && pending.is_some())
                || (registered.is_some() && entry.state != PreparationState::Registered)
                || registered.is_some_and(|account| {
                    account.provider != binding.payload.provider || account.linked_home.is_some()
                })
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    pub fn pending_is_eligible(&self, provider: Provider, key: Uuid) -> bool {
        self.pending
            .iter()
            .any(|row| row.provider == provider && row.key == key)
            && self.existing_pending(key).is_none_or(|entry| {
                entry.binding.provider == provider && entry.state == ExistingPendingState::Pending
            })
            && self
                .preparation_for_key(provider, key)
                .is_none_or(|entry| entry.state == PreparationState::Created)
    }

    pub fn managed_home_is_eligible(&self, homes: &Homes, provider: Provider, key: Uuid) -> bool {
        if let Some(entry) = self.existing_pending(key) {
            return entry.binding.provider == provider
                && match entry.state {
                    ExistingPendingState::Pending => self.pending_is_eligible(provider, key),
                    ExistingPendingState::Registered => {
                        self.registered_home_is_eligible(homes, provider, key)
                    }
                    ExistingPendingState::CancelledRetained => false,
                };
        }
        if let Some(entry) = self.preparation_for_key(provider, key) {
            return match entry.state {
                PreparationState::Created => self.pending_is_eligible(provider, key),
                PreparationState::Registered => {
                    self.registered_home_is_eligible(homes, provider, key)
                }
                _ => false,
            };
        }
        self.pending_is_eligible(provider, key)
            || self.registered_home_is_eligible(homes, provider, key)
    }
}

impl Store {
    /// Reconcile only an exact retained binding. Even unchanged metadata must
    /// complete the namespace barrier before being returned successfully.
    /// This does not inspect or initialize a home, run a provider, or establish
    /// whether a historic registration is still current.
    pub(crate) fn try_reconcile_preparation(
        &self,
        binding: &PreparationBinding,
    ) -> io::Result<Preparation> {
        self.preparation_transaction(true, |registry| {
            registry.exact_preparation(binding).cloned()
        })
    }

    /// Persist the request-to-key mapping before any managed-home effects.
    /// Equal retries still complete the directory durability barrier.
    pub fn plan_preparation(
        &self,
        request_id: Uuid,
        payload: PreparationPayload,
    ) -> io::Result<Preparation> {
        self.plan_preparation_with_mode(request_id, payload, false)
    }

    pub(crate) fn try_plan_preparation(
        &self,
        request_id: Uuid,
        payload: PreparationPayload,
    ) -> io::Result<Preparation> {
        self.plan_preparation_with_mode(request_id, payload, true)
    }

    fn plan_preparation_with_mode(
        &self,
        request_id: Uuid,
        payload: PreparationPayload,
        nonblocking: bool,
    ) -> io::Result<Preparation> {
        if request_id.is_nil() || !Account::PROVIDERS.contains(&payload.provider) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The account setup request is invalid",
            ));
        }
        self.preparation_transaction(nonblocking, |registry| {
            if let Some(existing) = registry.preparation(request_id) {
                if existing.binding.payload != payload {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "The account setup request was reused with different options",
                    ));
                }
                return Ok(existing.clone());
            }
            let installation_id = *registry.installation.get_or_insert_with(Uuid::new_v4);
            let entry = Preparation {
                binding: PreparationBinding {
                    request_id,
                    installation_id,
                    key: Uuid::new_v4(),
                    payload,
                },
                state: PreparationState::Planned,
            };
            registry.preparations.push(entry.clone());
            Ok(entry)
        })
    }

    pub fn cancel_preparation(
        &self,
        homes: &Homes,
        binding: &PreparationBinding,
    ) -> io::Result<Preparation> {
        self.cancel_preparation_with_mode(homes, binding, false)
    }

    pub(crate) fn try_cancel_preparation(
        &self,
        homes: &Homes,
        binding: &PreparationBinding,
    ) -> io::Result<Preparation> {
        self.cancel_preparation_with_mode(homes, binding, true)
    }

    fn cancel_preparation_with_mode(
        &self,
        homes: &Homes,
        binding: &PreparationBinding,
        nonblocking: bool,
    ) -> io::Result<Preparation> {
        let _lifecycle = if nonblocking {
            homes.try_lifecycle_lock()?
        } else {
            homes.lifecycle_lock()?
        };
        self.preparation_transaction(nonblocking, |registry| {
            let existing = registry.exact_preparation(binding)?.clone();
            let path = homes.path(binding.payload.provider, binding.key);
            if matches!(
                existing.state,
                PreparationState::Registered | PreparationState::CancelledRetained
            ) {
                return Ok(existing);
            }
            if registry.accounts.iter().any(|account| {
                account.key == binding.key
                    || account.linked_home.as_ref().is_some_and(|linked| {
                        linked == &path
                            || homes.canonical_managed_identity(linked)
                                == Some((binding.payload.provider, binding.key))
                    })
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "The account home is already registered; cancellation kept its registration",
                ));
            }
            let entry = registry
                .preparations
                .iter_mut()
                .find(|entry| entry.binding.request_id == binding.request_id)
                .unwrap();
            entry.state = PreparationState::CancelledRetained;
            Ok(entry.clone())
        })
    }

    /// The caller holds the physical admission token. The validator runs
    /// under the same registry lease, before consuming the planned attempt.
    pub(crate) fn create_prepared(
        &self,
        homes: &Homes,
        binding: &PreparationBinding,
        nonblocking: bool,
        validate: impl FnOnce(&Registry) -> io::Result<()>,
        create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
    ) -> io::Result<Preparation> {
        self.create_prepared_with_io(
            homes,
            binding,
            nonblocking,
            validate,
            create,
            |registry| self.persist_preparation(registry),
            |namespace| namespace.sync(),
        )
    }

    fn create_prepared_with_io(
        &self,
        homes: &Homes,
        binding: &PreparationBinding,
        nonblocking: bool,
        validate: impl FnOnce(&Registry) -> io::Result<()>,
        create: impl FnOnce() -> io::Result<(Uuid, PathBuf)>,
        mut persist: impl FnMut(&mut Registry) -> io::Result<()>,
        sync_replay: impl FnOnce(&super::super::durability::Directories) -> io::Result<()>,
    ) -> io::Result<Preparation> {
        let _lifecycle = if nonblocking {
            homes.try_lifecycle_lock()?
        } else {
            homes.lifecycle_lock()?
        };
        let _lock = if nonblocking {
            self.try_lock()?
        } else {
            self.lock()?
        };
        let mut registry = read(&self.path())?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts were saved by a newer cagents",
            ));
        }
        let existing = registry.exact_preparation(binding)?.clone();
        if existing.state != PreparationState::Planned {
            sync_replay(&super::super::durability::Directories::capture(
                &self.directory,
            )?)?;
            return Ok(existing);
        }
        validate(&registry)?;
        registry
            .preparations
            .iter_mut()
            .find(|entry| entry.binding.request_id == binding.request_id)
            .unwrap()
            .state = PreparationState::Uncertain;
        persist(&mut registry)?;
        let (key, home) = create()?;
        if key != binding.key || home != homes.path(binding.payload.provider, binding.key) {
            return Err(io::Error::other(
                "Account creation did not match its planned binding; files were retained",
            ));
        }
        registry
            .preparations
            .iter_mut()
            .find(|entry| entry.binding.request_id == binding.request_id)
            .unwrap()
            .state = PreparationState::Created;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or_default();
        registry.begin(binding.payload.provider, binding.key, now);
        persist(&mut registry)?;
        Ok(registry.exact_preparation(binding)?.clone())
    }

    fn persist_preparation(&self, registry: &mut Registry) -> io::Result<()> {
        self.persist_preparation_with_sync(registry, |namespace| namespace.sync())
    }

    fn persist_preparation_with_sync(
        &self,
        registry: &mut Registry,
        sync: impl FnOnce(&super::super::durability::Directories) -> io::Result<()>,
    ) -> io::Result<()> {
        self.persist_preparation_with_io(registry, || Ok(()), sync)
    }

    fn persist_preparation_with_io(
        &self,
        registry: &mut Registry,
        before_publish: impl FnOnce() -> io::Result<()>,
        sync: impl FnOnce(&super::super::durability::Directories) -> io::Result<()>,
    ) -> io::Result<()> {
        registry.version = VERSION;
        registry.revision = registry
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Accounts revision exhausted"))?;
        registry.validate_preparations()?;
        write_with_io(
            &self.directory,
            &self.path(),
            registry,
            before_publish,
            sync,
        )
    }

    pub(super) fn preparation_transaction<T>(
        &self,
        nonblocking: bool,
        change: impl FnOnce(&mut Registry) -> io::Result<T>,
    ) -> io::Result<T> {
        self.preparation_transaction_with_sync(nonblocking, change, |namespace| namespace.sync())
    }

    pub(super) fn preparation_transaction_with_sync<T>(
        &self,
        nonblocking: bool,
        change: impl FnOnce(&mut Registry) -> io::Result<T>,
        mut sync: impl FnMut(&super::super::durability::Directories) -> io::Result<()>,
    ) -> io::Result<T> {
        let _lock = if nonblocking {
            self.try_lock()?
        } else {
            self.lock()?
        };
        let mut registry = read(&self.path())?;
        if registry.is_newer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The accounts were saved by a newer cagents",
            ));
        }
        let before = registry.clone();
        let result = change(&mut registry)?;
        if registry != before {
            self.persist_preparation_with_sync(&mut registry, sync)?;
        } else {
            sync(&super::super::durability::Directories::capture(
                &self.directory,
            )?)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        _directory: tempfile::TempDir,
        store: Store,
        homes: Homes,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            Self {
                store: Store::at(directory.path().join("registry")),
                homes: Homes::at(directory.path().join("homes")),
                _directory: directory,
            }
        }
        fn plan(&self) -> Preparation {
            self.store
                .plan_preparation(
                    Uuid::new_v4(),
                    PreparationPayload {
                        provider: Provider::Codex,
                        copy_codex_config: true,
                    },
                )
                .unwrap()
        }
        fn create(&self, binding: &PreparationBinding) -> Preparation {
            self.store
                .create_prepared(
                    &self.homes,
                    binding,
                    false,
                    |_| Ok(()),
                    || {
                        // Observe the persisted attempt fence without reacquiring
                        // the registry lease held by this creation transaction.
                        let on_disk = read(&self.store.path()).unwrap();
                        assert_eq!(
                            on_disk.exact_preparation(binding).unwrap().state,
                            PreparationState::Uncertain
                        );
                        self.homes.create_planned_with_config_initialized(
                            binding.payload.provider,
                            binding.key,
                            None,
                            super::super::super::home::CodexConfig::Copy,
                            |_| Ok(()),
                        )
                    },
                )
                .unwrap()
        }
        fn account(&self, binding: &PreparationBinding) -> Registered {
            Registered {
                key: binding.key,
                provider: binding.payload.provider,
                linked_home: None,
                label: None,
                email: Some("fixture@example.invalid".into()),
                organization: None,
                added_at: 1,
            }
        }
    }

    #[test]
    fn reconciliation_preserves_each_exact_metadata_state_without_home_effects() {
        for state in [
            PreparationState::Planned,
            PreparationState::Uncertain,
            PreparationState::Created,
            PreparationState::CancelledRetained,
            PreparationState::Registered,
        ] {
            let fixture = Fixture::new();
            let mut planned = fixture.plan();
            fixture
                .store
                .preparation_transaction(false, |registry| {
                    registry.preparations[0].state = state;
                    if state == PreparationState::Created {
                        registry.begin(planned.binding.payload.provider, planned.binding.key, 1);
                    }
                    Ok(())
                })
                .unwrap();
            planned.state = state;
            let before = fixture.store.load().unwrap();
            let bytes = fs::read(fixture.store.path()).unwrap();
            assert_eq!(
                fixture
                    .store
                    .try_reconcile_preparation(&planned.binding)
                    .unwrap(),
                planned
            );
            assert_eq!(fixture.store.load().unwrap(), before);
            assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
            assert!(before.find(planned.binding.key).is_none());
            assert!(!fixture._directory.path().join("homes").exists());
        }
    }

    #[test]
    fn reconciliation_refuses_every_changed_binding_and_busy_storage() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let before = fixture.store.load().unwrap();
        let bytes = fs::read(fixture.store.path()).unwrap();
        let mut changed_bindings = Vec::new();
        let mut changed = planned.binding.clone();
        changed.request_id = Uuid::new_v4();
        changed_bindings.push(changed);
        let mut changed = planned.binding.clone();
        changed.installation_id = Uuid::new_v4();
        changed_bindings.push(changed);
        let mut changed = planned.binding.clone();
        changed.key = Uuid::new_v4();
        changed_bindings.push(changed);
        let mut changed = planned.binding.clone();
        changed.payload.provider = Provider::Claude;
        changed_bindings.push(changed);
        let mut changed = planned.binding.clone();
        changed.payload.copy_codex_config = !changed.payload.copy_codex_config;
        changed_bindings.push(changed);
        for binding in changed_bindings {
            assert_eq!(
                fixture
                    .store
                    .try_reconcile_preparation(&binding)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        {
            let _lock = fixture.store.lock().unwrap();
            assert_eq!(
                fixture
                    .store
                    .try_reconcile_preparation(&planned.binding)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
        }
        assert_eq!(fixture.store.load().unwrap(), before);
        assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
        assert!(!fixture._directory.path().join("homes").exists());
    }

    #[test]
    fn planning_replays_exact_binding_and_conflicting_options_preserve_bytes() {
        let fixture = Fixture::new();
        assert!(fixture.store.load().unwrap().installation.is_none());
        assert!(!fixture.store.path().exists());
        let planned = fixture.plan();
        let before = fs::read(fixture.store.path()).unwrap();
        assert_eq!(
            fixture
                .store
                .plan_preparation(planned.binding.request_id, planned.binding.payload.clone())
                .unwrap(),
            planned
        );
        let mut conflicting = planned.binding.payload.clone();
        conflicting.copy_codex_config = false;
        assert_eq!(
            fixture
                .store
                .plan_preparation(planned.binding.request_id, conflicting)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
        assert!(!fixture._directory.path().join("homes").exists());
        let other = fixture.plan();
        assert_eq!(
            other.binding.installation_id,
            planned.binding.installation_id
        );
        assert_ne!(other.binding.key, planned.binding.key);
    }

    #[test]
    fn cancellation_before_creation_retains_identity_without_calling_creation() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let cancelled = fixture
            .store
            .cancel_preparation(&fixture.homes, &planned.binding)
            .unwrap();
        assert_eq!(cancelled.state, PreparationState::CancelledRetained);
        assert_eq!(
            fixture
                .store
                .cancel_preparation(&fixture.homes, &planned.binding)
                .unwrap(),
            cancelled
        );
        let replay = fixture
            .store
            .create_prepared(
                &fixture.homes,
                &planned.binding,
                false,
                |_| panic!("no physical validation for terminal replay"),
                || panic!("cancelled creation was replayed"),
            )
            .unwrap();
        assert_eq!(replay, cancelled);
        assert!(
            !fixture
                .homes
                .path(planned.binding.payload.provider, planned.binding.key)
                .exists()
        );
        assert!(fixture.store.load().unwrap().pending().is_empty());
    }

    #[test]
    fn consumed_creation_failure_never_reseeds_or_adopts_retained_files() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let path = fixture
            .homes
            .path(planned.binding.payload.provider, planned.binding.key);
        let result = fixture.store.create_prepared(
            &fixture.homes,
            &planned.binding,
            false,
            |_| Ok(()),
            || {
                let (_, home) = fixture.homes.create_planned_with_config_initialized(
                    planned.binding.payload.provider,
                    planned.binding.key,
                    None,
                    super::super::super::home::CodexConfig::Copy,
                    |_| Ok(()),
                )?;
                fs::write(home.join("retained"), "original bytes")?;
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
        let replay = fixture
            .store
            .create_prepared(
                &fixture.homes,
                &planned.binding,
                false,
                |_| panic!("no readmission"),
                || panic!("uncertain creation was repeated"),
            )
            .unwrap();
        assert_eq!(replay.state, PreparationState::Uncertain);
        assert!(fixture.store.load().unwrap().pending().is_empty());
        fixture
            .store
            .cancel_preparation(&fixture.homes, &planned.binding)
            .unwrap();
        let registry = fixture.store.load().unwrap();
        assert!(registry.contains_managed_home(
            &fixture.homes,
            planned.binding.payload.provider,
            planned.binding.key
        ));
        fixture
            .store
            .remove_home(
                &fixture.homes,
                planned.binding.payload.provider,
                planned.binding.key,
                false,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(path.join("retained")).unwrap(),
            "original bytes"
        );
    }

    #[test]
    fn created_cancellation_fences_registration_abandon_and_final_launch_but_retains_home() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        assert_eq!(
            fixture.create(&planned.binding).state,
            PreparationState::Created
        );
        let path = fixture
            .homes
            .path(planned.binding.payload.provider, planned.binding.key);
        fs::write(path.join("retained"), "user data").unwrap();
        fixture
            .store
            .cancel_preparation(&fixture.homes, &planned.binding)
            .unwrap();
        let mut registry = fixture.store.load().unwrap();
        assert!(registry.contains_managed_home(
            &fixture.homes,
            planned.binding.payload.provider,
            planned.binding.key
        ));
        assert!(
            !registry.pending_is_eligible(planned.binding.payload.provider, planned.binding.key)
        );
        assert_eq!(
            registry.register(fixture.account(&planned.binding), &[]),
            Err(RegistrationError::RetainedPreparation)
        );
        let mut wrong_provider = fixture.account(&planned.binding);
        wrong_provider.provider = Provider::Claude;
        assert_eq!(
            registry.register(wrong_provider, &[]),
            Err(RegistrationError::RetainedPreparation)
        );
        let mut linked_alias = fixture.account(&planned.binding);
        linked_alias.linked_home = Some(path.clone());
        assert_eq!(
            registry.register(linked_alias, &[]),
            Err(RegistrationError::RetainedPreparation)
        );
        registry.abandon(planned.binding.key);
        assert_eq!(registry.pending().len(), 1);
        assert!(
            fixture
                .store
                .register_pending(&fixture.homes, fixture.account(&planned.binding), &[])
                .is_err()
        );
        let variable =
            super::super::super::home_variable(planned.binding.payload.provider).unwrap();
        let environment =
            BTreeMap::from([(variable.to_owned(), path.to_str().unwrap().to_owned())]);
        assert!(
            matches!(fixture.store.authorize_environment(&fixture.homes, &environment, None), Err(super::super::LaunchAccountError::Removed { key, .. }) if key == planned.binding.key)
        );
        let alias = fixture._directory.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let alias_environment =
            BTreeMap::from([(variable.to_owned(), alias.to_str().unwrap().to_owned())]);
        assert!(
            matches!(fixture.store.authorize_environment(&fixture.homes, &alias_environment, None), Err(super::super::LaunchAccountError::Removed { key, .. }) if key == planned.binding.key)
        );
        let replay = fixture
            .store
            .create_prepared(
                &fixture.homes,
                &planned.binding,
                false,
                |_| panic!("no physical replay"),
                || panic!("no recreation"),
            )
            .unwrap();
        assert_eq!(replay.state, PreparationState::CancelledRetained);
        assert_eq!(
            fs::read_to_string(path.join("retained")).unwrap(),
            "user data"
        );
    }

    #[test]
    fn registered_winner_cannot_be_cancelled_or_unregistered_by_a_late_reply() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        fixture.create(&planned.binding);
        fixture
            .store
            .register_pending(&fixture.homes, fixture.account(&planned.binding), &[])
            .unwrap()
            .1
            .unwrap();
        let outcome = fixture
            .store
            .cancel_preparation(&fixture.homes, &planned.binding)
            .unwrap();
        assert_eq!(outcome.state, PreparationState::Registered);
        let registry = fixture.store.load().unwrap();
        assert!(registry.find(planned.binding.key).is_some());
        assert!(registry.pending().is_empty());
        assert_eq!(
            registry.exact_preparation(&planned.binding).unwrap(),
            &outcome
        );
        fixture
            .store
            .update(|registry| registry.unregister(planned.binding.key))
            .unwrap();
        let registry = fixture.store.load().unwrap();
        assert!(registry.contains_managed_home(
            &fixture.homes,
            planned.binding.payload.provider,
            planned.binding.key
        ));
        assert!(!registry.managed_home_is_eligible(
            &fixture.homes,
            planned.binding.payload.provider,
            planned.binding.key
        ));
    }

    #[test]
    fn every_binding_field_is_checked_before_cancellation_or_creation() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let mut altered = Vec::new();
        let mut value = planned.binding.clone();
        value.request_id = Uuid::new_v4();
        altered.push(value);
        let mut value = planned.binding.clone();
        value.installation_id = Uuid::new_v4();
        altered.push(value);
        let mut value = planned.binding.clone();
        value.key = Uuid::new_v4();
        altered.push(value);
        let mut value = planned.binding.clone();
        value.payload.provider = Provider::Claude;
        altered.push(value);
        let mut value = planned.binding.clone();
        value.payload.copy_codex_config = false;
        altered.push(value);
        let before = fs::read(fixture.store.path()).unwrap();
        for binding in altered {
            assert!(
                fixture
                    .store
                    .cancel_preparation(&fixture.homes, &binding)
                    .is_err()
            );
            assert!(
                fixture
                    .store
                    .create_prepared(
                        &fixture.homes,
                        &binding,
                        false,
                        |_| panic!("wrong binding"),
                        || panic!("wrong binding")
                    )
                    .is_err()
            );
        }
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
    }

    #[test]
    fn linked_alias_registered_before_cancel_preserves_its_registration() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        fixture.create(&planned.binding);
        let path = fixture
            .homes
            .path(planned.binding.payload.provider, planned.binding.key);
        let alias = fixture._directory.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let linked = Uuid::new_v4();
        fixture
            .store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    linked,
                    alias.clone(),
                    None,
                    Some(&fixture.homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        assert_eq!(
            fixture
                .store
                .cancel_preparation(&fixture.homes, &planned.binding)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        let registry = fixture.store.load().unwrap();
        assert!(registry.find(linked).is_some());
        assert_eq!(
            registry.exact_preparation(&planned.binding).unwrap().state,
            PreparationState::Created
        );
    }

    #[test]
    fn reconciliation_requires_the_barrier_for_an_equal_visible_record() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let mut registry = fixture.store.load().unwrap();
        registry.preparations[0].state = PreparationState::CancelledRetained;
        registry.revision += 1;
        let result = write_with_directory_sync(
            &fixture.store.directory,
            &fixture.store.path(),
            &registry,
            |_| Err(io::Error::from_raw_os_error(libc::EIO)),
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(
            fixture
                .store
                .load()
                .unwrap()
                .exact_preparation(&planned.binding)
                .unwrap()
                .state,
            PreparationState::CancelledRetained
        );
        let bytes = fs::read(fixture.store.path()).unwrap();
        // The equal replay enters the directory sync path instead of relying
        // on visibility or skipping the write because equality is unchanged.
        let mut calls = 0;
        let failed_replay = fixture.store.preparation_transaction_with_sync(
            true,
            |registry| registry.exact_preparation(&planned.binding).cloned(),
            |_| {
                calls += 1;
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        );
        assert_eq!(failed_replay.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(
            calls, 1,
            "equal visible state still requires a durability barrier"
        );
        let replay = fixture
            .store
            .preparation_transaction_with_sync(
                true,
                |registry| registry.exact_preparation(&planned.binding).cloned(),
                |namespace| {
                    calls += 1;
                    namespace.sync()
                },
            )
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(replay.state, PreparationState::CancelledRetained);
        assert_eq!(
            fixture
                .store
                .try_reconcile_preparation(&planned.binding)
                .unwrap(),
            replay
        );
        assert_eq!(fixture.store.load().unwrap(), registry);
        assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
        assert!(!fixture._directory.path().join("homes").exists());
    }

    #[test]
    fn canonical_linked_registration_authorizes_a_noncanonical_ordinary_home_root() {
        let directory = tempfile::tempdir().unwrap();
        let extra = directory.path().join("extra");
        fs::create_dir(&extra).unwrap();
        // Absolute XDG roots can retain ParentDir components. This exercises
        // canonical identity without accepting a symlink storage ancestor.
        let homes = Homes::at(extra.join("../homes"));
        let store = Store::at(directory.path().join("registry"));
        let (key, path) = homes.create(Provider::Codex, None).unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        let external = directory.path().join("home-alias");
        std::os::unix::fs::symlink(&canonical, &external).unwrap();
        let linked = Uuid::new_v4();
        store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    linked,
                    canonical.clone(),
                    None,
                    Some(&homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        for selected in [&path, &canonical, &external] {
            let environment =
                BTreeMap::from([("CODEX_HOME".into(), selected.to_str().unwrap().to_owned())]);
            let lease = store
                .authorize_environment(&homes, &environment, None)
                .unwrap();
            assert!(lease.is_some());
            drop(lease);
        }
        store
            .update(|registry| registry.unregister(linked))
            .unwrap();
        let environment =
            BTreeMap::from([("CODEX_HOME".into(), canonical.to_str().unwrap().to_owned())]);
        assert!(
            matches!(store.authorize_environment(&homes, &environment, None), Err(super::super::LaunchAccountError::Removed { key: actual, .. }) if actual == key)
        );
    }

    #[test]
    fn canonical_registered_aliases_do_not_authorize_retained_preparations() {
        for state in [
            PreparationState::CancelledRetained,
            PreparationState::Uncertain,
        ] {
            let mut fixture = Fixture::new();
            let extra = fixture._directory.path().join("extra");
            fs::create_dir(&extra).unwrap();
            fixture.homes = Homes::at(extra.join("../homes"));
            let planned = fixture.plan();
            fixture.create(&planned.binding);
            if state == PreparationState::CancelledRetained {
                fixture
                    .store
                    .cancel_preparation(&fixture.homes, &planned.binding)
                    .unwrap();
            } else {
                fixture
                    .store
                    .update(|registry| {
                        registry
                            .preparations
                            .iter_mut()
                            .find(|entry| entry.binding == planned.binding)
                            .unwrap()
                            .state = state;
                        registry
                            .pending
                            .retain(|row| row.key != planned.binding.key);
                    })
                    .unwrap();
            }
            let path = fixture.homes.path(Provider::Codex, planned.binding.key);
            let canonical = fs::canonicalize(&path).unwrap();
            let alias = fixture._directory.path().join("alias");
            std::os::unix::fs::symlink(&canonical, &alias).unwrap();
            fixture
                .store
                .update(|registry| {
                    registry.link(
                        Provider::Codex,
                        Uuid::new_v4(),
                        canonical.clone(),
                        None,
                        Some(&fixture.homes),
                        1,
                    )
                })
                .unwrap()
                .1
                .unwrap();
            if state == PreparationState::CancelledRetained {
                let before = fs::read(fixture.store.path()).unwrap();
                assert_eq!(
                    fixture
                        .store
                        .cancel_preparation(&fixture.homes, &planned.binding)
                        .unwrap()
                        .state,
                    state
                );
                assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
            }
            let registry = fixture.store.load().unwrap();
            assert!(registry.contains_managed_home(
                &fixture.homes,
                Provider::Codex,
                planned.binding.key
            ));
            assert!(!registry.has_registered_home(
                &fixture.homes,
                Provider::Codex,
                planned.binding.key
            ));
            assert!(registry.registered_home_is_eligible(
                &fixture.homes,
                Provider::Codex,
                planned.binding.key
            ));
            assert!(!registry.managed_home_is_eligible(
                &fixture.homes,
                Provider::Codex,
                planned.binding.key
            ));
            for selected in [&path, &canonical, &alias] {
                let environment =
                    BTreeMap::from([("CODEX_HOME".into(), selected.to_str().unwrap().to_owned())]);
                assert!(
                    matches!(fixture.store.authorize_environment(&fixture.homes, &environment, None), Err(super::super::LaunchAccountError::Removed { key, .. }) if key == planned.binding.key)
                );
            }
        }
    }

    #[test]
    fn duplicate_registered_keys_are_rejected_in_both_orders_without_rewriting() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        fixture.create(&planned.binding);
        fixture
            .store
            .register_pending(&fixture.homes, fixture.account(&planned.binding), &[])
            .unwrap()
            .1
            .unwrap();
        let valid = fixture.store.load().unwrap();
        for ordinary in [false, true] {
            for reverse in [false, true] {
                let mut invalid = valid.clone();
                if ordinary {
                    invalid.preparations.clear();
                    invalid.installation = None;
                }
                let mut duplicate = fixture.account(&planned.binding);
                duplicate.provider = Provider::Claude;
                duplicate.linked_home = Some(fixture._directory.path().join("different"));
                invalid.accounts.push(duplicate);
                if reverse {
                    invalid.accounts.reverse();
                }
                assert_eq!(
                    invalid.validate_preparations().unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                let before = fs::read(fixture.store.path()).unwrap();
                assert_eq!(
                    write(&fixture.store.directory, &fixture.store.path(), &invalid)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::InvalidData
                );
                assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
                fs::write(
                    fixture.store.path(),
                    toml::to_string_pretty(&invalid).unwrap(),
                )
                .unwrap();
                let malformed = fs::read(fixture.store.path()).unwrap();
                assert_eq!(
                    fixture.store.load().unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                assert_eq!(fs::read(fixture.store.path()).unwrap(), malformed);
                fs::write(fixture.store.path(), before).unwrap();
            }
        }
    }

    #[test]
    fn linking_an_occupied_key_and_invalid_created_registration_preserve_state() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        fixture.create(&planned.binding);
        let mut registry = fixture.store.load().unwrap();
        let before = registry.clone();
        for linked in [false, true] {
            let mut account = fixture.account(&planned.binding);
            if linked {
                account.linked_home =
                    Some(fixture.homes.path(Provider::Codex, planned.binding.key));
            } else {
                account.provider = Provider::Claude;
            }
            assert_eq!(
                registry.register(account, &[]),
                Err(RegistrationError::RetainedPreparation)
            );
            assert_eq!(registry, before);
        }
        registry
            .register(fixture.account(&planned.binding), &[])
            .unwrap();
        let registered = registry.clone();
        let other = fixture._directory.path().join("different");
        fs::create_dir(&other).unwrap();
        assert!(
            registry
                .link(
                    Provider::Claude,
                    planned.binding.key,
                    other,
                    None,
                    Some(&fixture.homes),
                    1
                )
                .is_err()
        );
        assert_eq!(registry, registered);
    }

    fn assert_consumed_replay_requires_sync(fixture: &Fixture, binding: &PreparationBinding) {
        let before = fs::read(fixture.store.path()).unwrap();
        let registry = fixture.store.load().unwrap();
        let expected = registry.exact_preparation(binding).unwrap().clone();
        let barriers = std::cell::Cell::new(0);
        for fail in [true, false] {
            let result = fixture.store.create_prepared_with_io(
                &fixture.homes,
                binding,
                false,
                |_| panic!("consumed replay validated physical admission"),
                || panic!("consumed replay recreated a home"),
                |_| panic!("consumed replay rewrote its record"),
                |namespace| {
                    barriers.set(barriers.get() + 1);
                    if fail {
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    } else {
                        namespace.sync()
                    }
                },
            );
            if fail {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
            } else {
                assert_eq!(result.unwrap(), expected);
            }
            assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
            assert_eq!(fixture.store.load().unwrap(), registry);
        }
        assert_eq!(barriers.get(), 2);
    }

    #[test]
    fn creation_save_failures_preserve_consumed_attempts_and_require_replay_sync() {
        for target in [PreparationState::Uncertain, PreparationState::Created] {
            for after_rename in [false, true] {
                let fixture = Fixture::new();
                let planned = fixture.plan();
                let original = fs::read(fixture.store.path()).unwrap();
                let revision = fixture.store.load().unwrap().revision;
                let creations = std::cell::Cell::new(0);
                let validations = std::cell::Cell::new(0);
                let fence_bytes = std::cell::RefCell::new(None);
                let result = fixture.store.create_prepared_with_io(
                    &fixture.homes,
                    &planned.binding,
                    false,
                    |_| {
                        validations.set(validations.get() + 1);
                        Ok(())
                    },
                    || {
                        creations.set(creations.get() + 1);
                        *fence_bytes.borrow_mut() = Some(fs::read(fixture.store.path())?);
                        let created = fixture.homes.create_planned_with_config_initialized(
                            Provider::Codex,
                            planned.binding.key,
                            None,
                            super::super::super::home::CodexConfig::Copy,
                            |_| Ok(()),
                        )?;
                        fs::write(created.1.join("retained"), b"original bytes")?;
                        Ok(created)
                    },
                    |registry| {
                        if registry.exact_preparation(&planned.binding)?.state != target {
                            return fixture.store.persist_preparation(registry);
                        }
                        fixture.store.persist_preparation_with_io(
                            registry,
                            || {
                                if after_rename {
                                    Ok(())
                                } else {
                                    Err(io::Error::from_raw_os_error(libc::EIO))
                                }
                            },
                            |namespace| {
                                if after_rename {
                                    Err(io::Error::from_raw_os_error(libc::EIO))
                                } else {
                                    namespace.sync()
                                }
                            },
                        )
                    },
                    |namespace| namespace.sync(),
                );
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
                assert_eq!(validations.get(), 1);
                let expected_state = if target == PreparationState::Uncertain && !after_rename {
                    PreparationState::Planned
                } else if target == PreparationState::Created && after_rename {
                    PreparationState::Created
                } else {
                    PreparationState::Uncertain
                };
                let registry = fixture.store.load().unwrap();
                assert_eq!(
                    registry.exact_preparation(&planned.binding).unwrap().state,
                    expected_state
                );
                let expected_writes = if expected_state == PreparationState::Planned {
                    0
                } else if expected_state == PreparationState::Created {
                    2
                } else {
                    1
                };
                assert_eq!(registry.revision, revision + expected_writes);
                assert_eq!(
                    creations.get(),
                    usize::from(target == PreparationState::Created)
                );
                assert_eq!(
                    registry.pending.len(),
                    usize::from(expected_state == PreparationState::Created)
                );
                let path = fixture.homes.path(Provider::Codex, planned.binding.key);
                if target == PreparationState::Created {
                    assert_eq!(fs::read(path.join("retained")).unwrap(), b"original bytes");
                    if !after_rename {
                        assert_eq!(
                            fs::read(fixture.store.path()).unwrap(),
                            *fence_bytes.borrow().as_ref().unwrap()
                        );
                    }
                } else {
                    assert!(!path.exists());
                }
                if expected_state == PreparationState::Planned {
                    assert_eq!(fs::read(fixture.store.path()).unwrap(), original);
                    assert_eq!(
                        fixture.create(&planned.binding).state,
                        PreparationState::Created
                    );
                } else {
                    assert_consumed_replay_requires_sync(&fixture, &planned.binding);
                }
                if target == PreparationState::Created {
                    assert_eq!(fs::read(path.join("retained")).unwrap(), b"original bytes");
                }
            }
        }
    }

    #[test]
    fn creation_size_limits_refuse_before_effects_or_retain_the_consumed_home() {
        for overflow_before_creation in [true, false] {
            let fixture = Fixture::new();
            let planned = fixture.plan();
            let mut prospective = fixture.store.load().unwrap();
            prospective.preparations[0].state = PreparationState::Uncertain;
            prospective.revision += 1;
            prospective
                .system_labels
                .insert("codex".into(), String::new());
            let serialized = toml::to_string_pretty(&prospective).unwrap();
            assert!(serialized.ends_with('\n'));
            let target_size = MAX_BYTES as usize + usize::from(overflow_before_creation);
            let padding = "x".repeat(target_size - serialized.len());
            fixture
                .store
                .update(|registry| registry.system_labels.insert("codex".into(), padding))
                .unwrap();
            let original = fs::read(fixture.store.path()).unwrap();
            let revision = fixture.store.load().unwrap().revision;
            let creations = std::cell::Cell::new(0);
            let fence_bytes = std::cell::RefCell::new(None);
            let result = fixture.store.create_prepared(
                &fixture.homes,
                &planned.binding,
                false,
                |_| Ok(()),
                || {
                    creations.set(creations.get() + 1);
                    *fence_bytes.borrow_mut() = Some(fs::read(fixture.store.path())?);
                    let created = fixture.homes.create_planned_with_config_initialized(
                        Provider::Codex,
                        planned.binding.key,
                        None,
                        super::super::super::home::CodexConfig::Copy,
                        |_| Ok(()),
                    )?;
                    fs::write(created.1.join("retained"), b"kept after size refusal")?;
                    Ok(created)
                },
            );
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert_eq!(creations.get(), usize::from(!overflow_before_creation));
            let registry = fixture.store.load().unwrap();
            assert_eq!(
                registry.revision,
                revision + u64::from(!overflow_before_creation)
            );
            assert!(registry.pending.is_empty());
            let path = fixture.homes.path(Provider::Codex, planned.binding.key);
            if overflow_before_creation {
                assert_eq!(
                    registry.exact_preparation(&planned.binding).unwrap().state,
                    PreparationState::Planned
                );
                assert_eq!(fs::read(fixture.store.path()).unwrap(), original);
                assert!(!path.exists());
            } else {
                assert_eq!(
                    registry.exact_preparation(&planned.binding).unwrap().state,
                    PreparationState::Uncertain
                );
                assert_eq!(
                    fs::read(fixture.store.path()).unwrap(),
                    *fence_bytes.borrow().as_ref().unwrap()
                );
                assert_eq!(
                    fs::read(path.join("retained")).unwrap(),
                    b"kept after size refusal"
                );
                assert_consumed_replay_requires_sync(&fixture, &planned.binding);
            }
        }
    }

    #[test]
    fn registered_preparation_replay_completes_a_failed_directory_barrier() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        fixture.create(&planned.binding);
        let barriers = std::cell::Cell::new(0);
        let first = fixture.store.register_pending_with_io(
            &fixture.homes,
            fixture.account(&planned.binding),
            &[],
            false,
            |_| Ok(()),
            |_| {
                barriers.set(barriers.get() + 1);
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        );
        assert_eq!(first.unwrap_err().raw_os_error(), Some(libc::EIO));
        let visible = fixture.store.load().unwrap();
        assert_eq!(
            visible.exact_preparation(&planned.binding).unwrap().state,
            PreparationState::Registered
        );
        assert_eq!(visible.accounts.len(), 1);
        assert!(visible.pending.is_empty());
        let bytes = fs::read(fixture.store.path()).unwrap();
        for fail in [true, false] {
            let result = fixture.store.register_pending_with_io(
                &fixture.homes,
                fixture.account(&planned.binding),
                &[],
                false,
                |_| Ok(()),
                |namespace| {
                    barriers.set(barriers.get() + 1);
                    if fail {
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    } else {
                        namespace.sync()
                    }
                },
            );
            if fail {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
            } else {
                assert_eq!(result.unwrap(), (visible.clone(), Ok(())));
            }
            assert_eq!(fixture.store.load().unwrap(), visible);
            assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
        }
        assert_eq!(barriers.get(), 3);
    }

    #[test]
    fn ordinary_pending_remains_eligible_and_terminal_setup_identity_survives_preferences() {
        let fixture = Fixture::new();
        let (key, _) = fixture
            .store
            .create_pending(&fixture.homes, Provider::Claude, 1, || {
                fixture.homes.create(Provider::Claude, None)
            })
            .unwrap();
        let planned = fixture.plan();
        fixture
            .store
            .cancel_preparation(&fixture.homes, &planned.binding)
            .unwrap();
        fixture
            .store
            .update(|registry| {
                registry.set_label(&Account::system(Provider::Claude), Some("Personal".into()))
            })
            .unwrap();
        let registry = fixture.store.load().unwrap();
        assert!(registry.pending_is_eligible(Provider::Claude, key));
        assert!(registry.managed_home_is_eligible(&fixture.homes, Provider::Claude, key));
        assert_eq!(
            registry.exact_preparation(&planned.binding).unwrap().state,
            PreparationState::CancelledRetained
        );
    }
}
