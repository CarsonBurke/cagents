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
        self.resume_existing_pending_with_sync(target, nonblocking, |namespace| namespace.sync())
    }

    fn resume_existing_pending_with_sync(
        &self,
        target: &ExistingPendingTarget,
        nonblocking: bool,
        sync: impl FnMut(&super::super::durability::Directories) -> io::Result<()>,
    ) -> io::Result<ExistingPending> {
        self.preparation_transaction_with_sync(
            nonblocking,
            |registry| registry.resolve_existing_pending(target),
            sync,
        )
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
        self.cancel_existing_pending_with_sync(
            homes,
            operation_id,
            binding,
            nonblocking,
            |namespace| namespace.sync(),
        )
    }

    fn cancel_existing_pending_with_sync(
        &self,
        homes: &Homes,
        operation_id: Uuid,
        binding: &ExistingPendingBinding,
        nonblocking: bool,
        sync: impl FnMut(&super::super::durability::Directories) -> io::Result<()>,
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
        self.preparation_transaction_with_sync(nonblocking, |registry| {
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
                return Ok(entry.clone());
            }
            let exact = registry.exact_existing_pending(binding)?.clone();
            if exact.state == ExistingPendingState::Pending
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
        }, sync)
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
        fn pending(
            &self,
            provider: Provider,
            started_at: u64,
        ) -> (Uuid, PathBuf, ExistingPendingTarget) {
            let (key, home) = self
                .store
                .create_pending(&self.homes, provider, started_at, || {
                    self.homes.create(provider, None)
                })
                .unwrap();
            let target = ExistingPendingTarget::Observed {
                provider,
                key,
                started_at,
                expected_installation: self.store.load().unwrap().installation_id(),
            };
            (key, home, target)
        }
        fn account(&self, key: Uuid) -> Registered {
            Registered {
                key,
                provider: Provider::Codex,
                linked_home: None,
                label: None,
                email: Some("legacy@example.invalid".into()),
                organization: None,
                added_at: 1,
            }
        }
    }

    #[test]
    fn concurrent_original_observations_share_one_durable_generation() {
        let fixture = Fixture::new();
        let (key, home, target) = fixture.pending(Provider::Codex, 11);
        fs::write(home.join("retained"), b"unchanged").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let target = target.clone();
                let directory = fixture.store.directory.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::at(directory)
                        .resume_existing_pending(&target)
                        .unwrap()
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results[0], results[1]);
        let before = fs::read(fixture.store.path()).unwrap();
        assert_eq!(
            fixture.store.resume_existing_pending(&target).unwrap(),
            results[0]
        );
        assert_eq!(
            fixture
                .store
                .resume_existing_pending(&ExistingPendingTarget::Bound {
                    binding: results[0].binding.clone()
                })
                .unwrap(),
            results[0]
        );
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
        assert_eq!(fixture.store.load().unwrap().existing_pending.len(), 1);
        assert_eq!(results[0].binding.key, key);
        assert_eq!(fs::read(home.join("retained")).unwrap(), b"unchanged");
    }

    #[test]
    fn stale_observations_and_every_binding_field_refuse_without_rewriting() {
        let fixture = Fixture::new();
        let (_, _, first) = fixture.pending(Provider::Codex, 1);
        let (other, _, stale) = fixture.pending(Provider::Claude, 2);
        let record = fixture.store.resume_existing_pending(&first).unwrap();
        let before = fs::read(fixture.store.path()).unwrap();
        assert_eq!(
            fixture
                .store
                .resume_existing_pending(&stale)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let refreshed = ExistingPendingTarget::Observed {
            provider: Provider::Claude,
            key: other,
            started_at: 2,
            expected_installation: Some(record.binding.installation_id),
        };
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
        fixture.store.resume_existing_pending(&refreshed).unwrap();
        let before = fs::read(fixture.store.path()).unwrap();
        let mut wrong = Vec::new();
        let mut value = record.binding.clone();
        value.installation_id = Uuid::new_v4();
        wrong.push(value);
        let mut value = record.binding.clone();
        value.provider = Provider::Claude;
        wrong.push(value);
        let mut value = record.binding.clone();
        value.key = Uuid::new_v4();
        wrong.push(value);
        let mut value = record.binding.clone();
        value.started_at += 1;
        wrong.push(value);
        let mut value = record.binding.clone();
        value.generation_id = Uuid::new_v4();
        wrong.push(value);
        for binding in wrong {
            assert!(
                fixture
                    .store
                    .resume_existing_pending(&ExistingPendingTarget::Bound {
                        binding: binding.clone()
                    })
                    .is_err()
            );
            assert!(
                fixture
                    .store
                    .cancel_existing_pending(&fixture.homes, Uuid::new_v4(), &binding)
                    .is_err()
            );
            assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
        }
    }

    #[test]
    fn registration_before_first_resume_retains_history_after_unregister() {
        let fixture = Fixture::new();
        let (key, _, target) = fixture.pending(Provider::Codex, 7);
        fixture
            .store
            .register_pending(&fixture.homes, fixture.account(key), &[])
            .unwrap()
            .1
            .unwrap();
        let registered = fixture.store.resume_existing_pending(&target).unwrap();
        assert_eq!(registered.state, ExistingPendingState::Registered);
        assert!(fixture.store.load().unwrap().pending.is_empty());
        fixture
            .store
            .update(|registry| registry.unregister(key))
            .unwrap();
        let cancelled = fixture
            .store
            .cancel_existing_pending(&fixture.homes, Uuid::new_v4(), &registered.binding)
            .unwrap();
        assert_eq!(cancelled.state, ExistingPendingState::Registered);
        let replay = fixture.store.resume_existing_pending(&target).unwrap();
        assert_eq!(replay.binding, registered.binding);
        let registry = fixture.store.load().unwrap();
        assert!(registry.find(key).is_none());
        assert!(registry.contains_managed_home(&fixture.homes, Provider::Codex, key));
        assert!(!registry.managed_home_is_eligible(&fixture.homes, Provider::Codex, key));
        let before = registry.clone();
        let mut next = registry;
        next.begin(Provider::Codex, key, 99);
        next.begin(Provider::Claude, key, 99);
        next.abandon(key);
        assert_eq!(next, before);
        assert_eq!(
            next.register(fixture.account(key), &[]),
            Err(RegistrationError::RetainedPreparation)
        );
        assert_eq!(next, before);
    }

    #[test]
    fn cancellation_receipts_survive_later_aliases_and_reject_other_bindings() {
        let fixture = Fixture::new();
        let (key, path, target) = fixture.pending(Provider::Codex, 1);
        let first = fixture.store.resume_existing_pending(&target).unwrap();
        let (_, _, other_target) = fixture.pending(Provider::Claude, 2);
        let other = fixture
            .store
            .resume_existing_pending(&other_target)
            .unwrap();
        fs::write(path.join("retained"), b"original").unwrap();
        let operation = Uuid::new_v4();
        let cancelled = fixture
            .store
            .cancel_existing_pending(&fixture.homes, operation, &first.binding)
            .unwrap();
        assert_eq!(cancelled.state, ExistingPendingState::CancelledRetained);
        fixture
            .store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    Uuid::new_v4(),
                    path.canonicalize().unwrap(),
                    None,
                    Some(&fixture.homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        let before = fs::read(fixture.store.path()).unwrap();
        assert_eq!(
            fixture
                .store
                .cancel_existing_pending(&fixture.homes, operation, &first.binding)
                .unwrap(),
            cancelled
        );
        assert_eq!(
            fixture
                .store
                .cancel_existing_pending(&fixture.homes, operation, &other.binding)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
        let registry = fixture.store.load().unwrap();
        assert!(registry.contains_managed_home(&fixture.homes, Provider::Codex, key));
        assert!(!registry.managed_home_is_eligible(&fixture.homes, Provider::Codex, key));
        assert!(
            fixture
                .store
                .register_pending(&fixture.homes, fixture.account(key), &[])
                .is_err()
        );
        assert_eq!(fs::read(path.join("retained")).unwrap(), b"original");
    }

    #[test]
    fn original_observation_replays_adoption_only_after_its_directory_barrier() {
        let fixture = Fixture::new();
        let (_, _, target) = fixture.pending(Provider::Codex, 1);
        let calls = std::cell::Cell::new(0);
        let first = fixture
            .store
            .resume_existing_pending_with_sync(&target, false, |_| {
                calls.set(calls.get() + 1);
                Err(io::Error::from_raw_os_error(libc::EIO))
            });
        assert_eq!(first.unwrap_err().raw_os_error(), Some(libc::EIO));
        let visible = fixture.store.load().unwrap();
        let bytes = fs::read(fixture.store.path()).unwrap();
        let expected = visible.existing_pending[0].clone();
        for fail in [true, false] {
            let result =
                fixture
                    .store
                    .resume_existing_pending_with_sync(&target, false, |namespace| {
                        calls.set(calls.get() + 1);
                        if fail {
                            Err(io::Error::from_raw_os_error(libc::EIO))
                        } else {
                            namespace.sync()
                        }
                    });
            if fail {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
            } else {
                assert_eq!(result.unwrap(), expected);
            }
            assert_eq!(fixture.store.load().unwrap(), visible);
            assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
        }
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn cancellation_state_and_receipt_publish_together_and_replay_sync_errors() {
        for registration_wins in [false, true] {
            let fixture = Fixture::new();
            let (key, _, target) = fixture.pending(Provider::Codex, 1);
            let adopted = fixture.store.resume_existing_pending(&target).unwrap();
            if registration_wins {
                fixture
                    .store
                    .register_pending(&fixture.homes, fixture.account(key), &[])
                    .unwrap()
                    .1
                    .unwrap();
            }
            let operation = Uuid::new_v4();
            let calls = std::cell::Cell::new(0);
            let first = fixture.store.cancel_existing_pending_with_sync(
                &fixture.homes,
                operation,
                &adopted.binding,
                false,
                |_| {
                    calls.set(calls.get() + 1);
                    Err(io::Error::from_raw_os_error(libc::EIO))
                },
            );
            assert_eq!(first.unwrap_err().raw_os_error(), Some(libc::EIO));
            let visible = fixture.store.load().unwrap();
            let expected = visible.existing_pending(key).unwrap().clone();
            assert_eq!(expected.cancellation_ids, vec![operation]);
            assert_eq!(
                expected.state,
                if registration_wins {
                    ExistingPendingState::Registered
                } else {
                    ExistingPendingState::CancelledRetained
                }
            );
            let bytes = fs::read(fixture.store.path()).unwrap();
            for fail in [true, false] {
                let result = fixture.store.cancel_existing_pending_with_sync(
                    &fixture.homes,
                    operation,
                    &adopted.binding,
                    false,
                    |namespace| {
                        calls.set(calls.get() + 1);
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
                assert_eq!(fixture.store.load().unwrap(), visible);
                assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
            }
            assert_eq!(calls.get(), 3);
            assert_eq!(visible.find(key).is_some(), registration_wins);
        }
    }

    #[test]
    fn legacy_registered_replay_completes_a_failed_directory_barrier() {
        let fixture = Fixture::new();
        let (key, _, target) = fixture.pending(Provider::Codex, 1);
        let calls = std::cell::Cell::new(0);
        let first = fixture.store.register_pending_with_io(
            &fixture.homes,
            fixture.account(key),
            &[],
            false,
            |_| Ok(()),
            |_| {
                calls.set(calls.get() + 1);
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        );
        assert_eq!(first.unwrap_err().raw_os_error(), Some(libc::EIO));
        let visible = fixture.store.load().unwrap();
        assert_eq!(
            visible.existing_pending(key).unwrap().state,
            ExistingPendingState::Registered
        );
        let bytes = fs::read(fixture.store.path()).unwrap();
        for fail in [true, false] {
            let result = fixture.store.register_pending_with_io(
                &fixture.homes,
                fixture.account(key),
                &[],
                false,
                |_| Ok(()),
                |namespace| {
                    calls.set(calls.get() + 1);
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
            assert_eq!(fs::read(fixture.store.path()).unwrap(), bytes);
        }
        assert_eq!(calls.get(), 3);
        assert_eq!(
            fixture
                .store
                .resume_existing_pending(&target)
                .unwrap()
                .binding,
            visible.existing_pending(key).unwrap().binding
        );
    }

    #[test]
    fn unadopted_pending_keys_cannot_be_reused_by_link() {
        let fixture = Fixture::new();
        let (key, _, _) = fixture.pending(Provider::Codex, 1);
        let other = fixture._directory.path().join("other");
        fs::create_dir(&other).unwrap();
        let before = fixture.store.load().unwrap();
        for provider in [Provider::Codex, Provider::Claude] {
            let mut registry = before.clone();
            assert!(
                registry
                    .link(provider, key, other.clone(), None, Some(&fixture.homes), 1)
                    .is_err()
            );
            assert_eq!(registry, before);
        }
        assert!(before.existing_pending.is_empty());
    }

    #[test]
    fn malformed_legacy_associations_fail_closed_and_round_trips_preserve_receipts() {
        let fixture = Fixture::new();
        let (key, _, target) = fixture.pending(Provider::Codex, 1);
        let record = fixture.store.resume_existing_pending(&target).unwrap();
        fixture
            .store
            .cancel_existing_pending(&fixture.homes, Uuid::new_v4(), &record.binding)
            .unwrap();
        let valid = fixture.store.load().unwrap();
        assert_eq!(
            serde_json::from_value::<Registry>(serde_json::to_value(&valid).unwrap()).unwrap(),
            valid
        );
        assert_eq!(
            toml::from_str::<Registry>(&toml::to_string_pretty(&valid).unwrap()).unwrap(),
            valid
        );
        let mut invalids = Vec::new();
        let mut bad = valid.clone();
        bad.existing_pending.push(bad.existing_pending[0].clone());
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.installation = Some(Uuid::new_v4());
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.existing_pending[0].binding.generation_id = Uuid::nil();
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.existing_pending[0].cancellation_ids.clear();
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.existing_pending[0].state = ExistingPendingState::Pending;
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.pending.clear();
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.pending[0].provider = Provider::Claude;
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.pending[0].started_at += 1;
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.pending.push(bad.pending[0].clone());
        invalids.push(bad);
        let mut bad = valid.clone();
        bad.accounts.push(fixture.account(key));
        invalids.push(bad);
        let original = fs::read(fixture.store.path()).unwrap();
        for bad in invalids {
            assert_eq!(
                bad.validate_preparations().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                write(&fixture.store.directory, &fixture.store.path(), &bad)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(fs::read(fixture.store.path()).unwrap(), original);
            fs::write(fixture.store.path(), toml::to_string_pretty(&bad).unwrap()).unwrap();
            let malformed = fs::read(fixture.store.path()).unwrap();
            assert_eq!(
                fixture.store.load().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(fs::read(fixture.store.path()).unwrap(), malformed);
            fs::write(fixture.store.path(), &original).unwrap();
        }
    }

    #[test]
    fn oversized_cancellation_does_not_claim_its_uuid_or_change_the_pending_state() {
        let fixture = Fixture::new();
        let (_, _, target) = fixture.pending(Provider::Codex, 1);
        let first = fixture.store.resume_existing_pending(&target).unwrap();
        let (_, _, other_target) = fixture.pending(Provider::Claude, 2);
        let other = fixture
            .store
            .resume_existing_pending(&other_target)
            .unwrap();
        let operation = Uuid::new_v4();
        let mut prospective = fixture.store.load().unwrap();
        prospective.existing_pending[0].state = ExistingPendingState::CancelledRetained;
        prospective.existing_pending[0]
            .cancellation_ids
            .push(operation);
        prospective
            .system_labels
            .insert("codex".into(), String::new());
        let document = toml::to_string_pretty(&prospective).unwrap();
        let padding = "x".repeat(MAX_BYTES as usize + 1 - document.len());
        fixture
            .store
            .update(|registry| registry.system_labels.insert("codex".into(), padding))
            .unwrap();
        let before = fs::read(fixture.store.path()).unwrap();
        assert_eq!(
            fixture
                .store
                .cancel_existing_pending(&fixture.homes, operation, &first.binding)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
        assert_eq!(
            fixture
                .store
                .load()
                .unwrap()
                .existing_pending(first.binding.key)
                .unwrap()
                .state,
            ExistingPendingState::Pending
        );
        fixture
            .store
            .update(|registry| registry.system_labels.clear())
            .unwrap();
        let second = fixture
            .store
            .cancel_existing_pending(&fixture.homes, operation, &other.binding)
            .unwrap();
        assert_eq!(second.cancellation_ids, vec![operation]);
        let before = fs::read(fixture.store.path()).unwrap();
        assert!(
            fixture
                .store
                .cancel_existing_pending(&fixture.homes, operation, &first.binding)
                .is_err()
        );
        assert_eq!(fs::read(fixture.store.path()).unwrap(), before);
    }
}
