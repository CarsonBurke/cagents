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

    pub fn preparation_for_key(&self, provider: Provider, key: Uuid) -> Option<&Preparation> {
        self.preparations
            .iter()
            .find(|entry| entry.binding.key == key && entry.binding.payload.provider == provider)
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
            && self
                .preparation_for_key(provider, key)
                .is_none_or(|entry| entry.state == PreparationState::Created)
    }

    pub fn managed_home_is_eligible(&self, homes: &Homes, provider: Provider, key: Uuid) -> bool {
        if let Some(entry) = self.preparation_for_key(provider, key) {
            return match entry.state {
                PreparationState::Created => self.pending_is_eligible(provider, key),
                PreparationState::Registered => self.has_registered_home(homes, provider, key),
                _ => false,
            };
        }
        self.contains_managed_home(homes, provider, key)
    }
}

impl Store {
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
            if existing.state == PreparationState::Registered {
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
            sync_registry_directory(&self.directory)?;
            return Ok(existing);
        }
        validate(&registry)?;
        registry
            .preparations
            .iter_mut()
            .find(|entry| entry.binding.request_id == binding.request_id)
            .unwrap()
            .state = PreparationState::Uncertain;
        self.persist_preparation(&mut registry)?;
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
        self.persist_preparation(&mut registry)?;
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
        registry.version = VERSION;
        registry.revision = registry
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Accounts revision exhausted"))?;
        registry.validate_preparations()?;
        write_with_directory_sync(&self.directory, &self.path(), registry, sync)
    }

    fn preparation_transaction<T>(
        &self,
        nonblocking: bool,
        change: impl FnOnce(&mut Registry) -> io::Result<T>,
    ) -> io::Result<T> {
        self.preparation_transaction_with_sync(nonblocking, change, |namespace| namespace.sync())
    }

    fn preparation_transaction_with_sync<T>(
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

pub(super) fn sync_registry_directory(directory: &Path) -> io::Result<()> {
    super::super::durability::Directories::capture(directory)?.sync()
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
    fn visible_rename_with_failed_directory_sync_requires_durability_retry() {
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
        // The equal replay enters the directory sync path instead of relying
        // on visibility or skipping the write because equality is unchanged.
        let mut calls = 0;
        let failed_replay = fixture.store.preparation_transaction_with_sync(
            false,
            |registry| Ok(registry.exact_preparation(&planned.binding)?.clone()),
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
                false,
                |registry| Ok(registry.exact_preparation(&planned.binding)?.clone()),
                |namespace| {
                    calls += 1;
                    namespace.sync()
                },
            )
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(replay.state, PreparationState::CancelledRetained);
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
