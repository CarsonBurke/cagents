//! Metadata dispatch for one owning installation's retained account setup.
//!
//! Successful replies confirm only the Store's metadata durability barrier.
//! They grant no home, provider, login, registration, or deletion authority.
//!
//! This is not a complete implementation of all six request flows: Create and
//! Observed targets without an installation identity are refused before storage.
//! A future Create consumer must use Store::create_prepared under its actual
//! physical admission token and validator, never the legacy create_pending path.

use std::io;

use super::{
    home::Homes,
    preparation::{
        AccountPreparationOutcome, AccountPreparationRecord, AccountPreparationReply,
        AccountPreparationRequest, ExistingPending, ExistingPendingRecord, ExistingPendingRequest,
        ExistingPendingTarget, PreparationRequest,
    },
    registry::Store,
};

/// Keeps metadata requests attached to the Store and Homes selected by the
/// caller. Request identifiers never select another installation or path.
///
/// Lease acquisition does not wait for another owner. Filesystem durability
/// work still belongs off an interface's event loop.
#[derive(Debug, Clone)]
pub struct MetadataPreparationApi {
    store: Store,
    homes: Homes,
}

impl MetadataPreparationApi {
    pub fn new(store: Store, homes: Homes) -> Self {
        Self { store, homes }
    }

    /// Returns the entire submitted request unchanged, including an existing
    /// cancellation's original target and operation ID after partial progress.
    pub fn handle(&self, request: AccountPreparationRequest) -> AccountPreparationReply {
        let outcome = match &request {
            AccountPreparationRequest::Preparation(request) => self.preparation(request),
            AccountPreparationRequest::ExistingPending(request) => self.existing_pending(request),
        };
        AccountPreparationReply { request, outcome }
    }

    fn preparation(&self, request: &PreparationRequest) -> AccountPreparationOutcome {
        let result = match request {
            PreparationRequest::Plan {
                request_id,
                payload,
            } => self
                .store
                .try_plan_preparation(*request_id, payload.clone()),
            PreparationRequest::Create { .. } => {
                // This facade has no physical admission or creation receipt.
                // In particular it must never bridge to legacy Create.
                return not_attempted("Home creation requires separate physical admission");
            }
            PreparationRequest::CancelRetained { binding } => {
                self.store.try_cancel_preparation(&self.homes, binding)
            }
            PreparationRequest::Reconcile { binding } => {
                self.store.try_reconcile_preparation(binding)
            }
        };
        storage_outcome(result.map(AccountPreparationRecord::Preparation), None)
    }

    fn existing_pending(&self, request: &ExistingPendingRequest) -> AccountPreparationOutcome {
        let target = match request {
            ExistingPendingRequest::Resume { target }
            | ExistingPendingRequest::Cancel { target, .. } => target,
        };
        if matches!(
            target,
            ExistingPendingTarget::Observed {
                expected_installation: None,
                ..
            }
        ) {
            // The current Store API can resolve an old None observation after
            // an installation identity exists. This facade cannot invent the
            // explicit reaffirmation and new causally linked intent required
            // to change that original target. A refresh is not authorization.
            return not_attempted(
                "This observation has no installation identity; continuing requires explicit reaffirmation as a new intent",
            );
        }

        match request {
            ExistingPendingRequest::Resume { target } => storage_outcome(
                self.store
                    .try_resume_existing_pending(target)
                    .map(existing_record),
                None,
            ),
            ExistingPendingRequest::Cancel {
                operation_id,
                target,
            } => {
                let (binding, observed) = match target {
                    ExistingPendingTarget::Bound { binding } => (binding.clone(), None),
                    ExistingPendingTarget::Observed { .. } => {
                        match self.store.try_resume_existing_pending(target) {
                            Ok(record) => {
                                let binding = record.binding.clone();
                                (binding, Some(existing_record(record)))
                            }
                            Err(error) => return storage_outcome(Err(error), None),
                        }
                    }
                };
                // Resolution and cancellation each complete their own Store
                // barrier. The exact resolved binding is cancellation's CAS;
                // the reply continues to echo the original submitted target.
                // If cancellation fails, the earlier observation is historical
                // metadata, never confirmation of the complete Cancel request.
                storage_outcome(
                    self.store
                        .try_cancel_existing_pending(&self.homes, *operation_id, &binding)
                        .map(existing_record),
                    observed,
                )
            }
        }
    }
}

fn existing_record(record: ExistingPending) -> AccountPreparationRecord {
    AccountPreparationRecord::ExistingPending(ExistingPendingRecord::from(&record))
}

fn not_attempted(message: &str) -> AccountPreparationOutcome {
    AccountPreparationOutcome::NotAttempted {
        message: message.into(),
    }
}

fn storage_outcome(
    result: io::Result<AccountPreparationRecord>,
    observed: Option<AccountPreparationRecord>,
) -> AccountPreparationOutcome {
    match result {
        Ok(record) => AccountPreparationOutcome::Confirmed { record },
        // ErrorKind does not establish whether publication or other effects
        // happened. Keep the owning API and original request for reconciliation
        // rather than manufacturing a pre-storage admission refusal.
        Err(error) => AccountPreparationOutcome::ReconcileRequired {
            message: error.to_string(),
            observed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::{
            preparation::{
                ExistingPendingBinding, ExistingPendingState, Preparation, PreparationBinding,
                PreparationPayload, PreparationState,
            },
            registry::Registered,
        },
        agent::Provider,
    };
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    struct Fixture {
        directory: tempfile::TempDir,
        registry_directory: PathBuf,
        api: MetadataPreparationApi,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let registry_directory = directory.path().join("registry");
            let api = MetadataPreparationApi::new(
                Store::at(&registry_directory),
                Homes::at(directory.path().join("homes")),
            );
            Self {
                directory,
                registry_directory,
                api,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            fs::read(self.registry_directory.join("accounts.toml")).unwrap()
        }

        fn plan(&self) -> Preparation {
            let request = AccountPreparationRequest::Preparation(PreparationRequest::Plan {
                request_id: Uuid::new_v4(),
                payload: PreparationPayload {
                    provider: Provider::Codex,
                    copy_codex_config: true,
                },
            });
            let reply = self.api.handle(request.clone());
            assert_eq!(reply.request, request);
            match confirmed(reply) {
                AccountPreparationRecord::Preparation(record) => record,
                _ => panic!("expected preparation metadata"),
            }
        }

        fn observed_pending(&self) -> ExistingPendingTarget {
            // Establish installation identity independently of this legacy row.
            self.plan();
            let key = Uuid::new_v4();
            let (registry, ()) = self
                .api
                .store
                .update(|registry| registry.begin(Provider::Codex, key, 17))
                .unwrap();
            ExistingPendingTarget::Observed {
                provider: Provider::Codex,
                key,
                started_at: 17,
                expected_installation: registry.installation_id(),
            }
        }
    }

    fn confirmed(reply: AccountPreparationReply) -> AccountPreparationRecord {
        match reply.outcome {
            AccountPreparationOutcome::Confirmed { record } => record,
            other => panic!("expected confirmed metadata, got {other:?}"),
        }
    }

    fn pending_record(reply: AccountPreparationReply) -> ExistingPendingRecord {
        match confirmed(reply) {
            AccountPreparationRecord::ExistingPending(record) => record,
            _ => panic!("expected existing pending metadata"),
        }
    }

    fn sample_binding() -> PreparationBinding {
        PreparationBinding {
            request_id: Uuid::from_u128(1),
            installation_id: Uuid::from_u128(2),
            key: Uuid::from_u128(3),
            payload: PreparationPayload {
                provider: Provider::Codex,
                copy_codex_config: true,
            },
        }
    }

    #[test]
    fn physical_creation_and_unbound_observations_are_refused_before_storage() {
        let fixture = Fixture::new();
        let target = ExistingPendingTarget::Observed {
            provider: Provider::Codex,
            key: Uuid::new_v4(),
            started_at: 17,
            expected_installation: None,
        };
        for request in [
            AccountPreparationRequest::Preparation(PreparationRequest::Create {
                binding: sample_binding(),
            }),
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
                target: target.clone(),
            }),
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                operation_id: Uuid::new_v4(),
                target,
            }),
        ] {
            let reply = fixture.api.handle(request.clone());
            assert_eq!(reply.request, request);
            assert!(matches!(
                reply.outcome,
                AccountPreparationOutcome::NotAttempted { .. }
            ));
            assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn storage_errors_never_claim_that_effects_were_not_attempted() {
        let observed = AccountPreparationRecord::Preparation(Preparation {
            binding: sample_binding(),
            state: PreparationState::Uncertain,
        });
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Unsupported,
            io::ErrorKind::Other,
        ] {
            assert_eq!(
                storage_outcome(
                    Err(io::Error::new(kind, "unresolved storage result")),
                    Some(observed.clone()),
                ),
                AccountPreparationOutcome::ReconcileRequired {
                    message: "unresolved storage result".into(),
                    observed: Some(observed.clone()),
                }
            );
        }
    }

    #[test]
    fn plan_reconcile_and_cancel_keep_the_original_owner_and_whole_request() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let plan = AccountPreparationRequest::Preparation(PreparationRequest::Plan {
            request_id: planned.binding.request_id,
            payload: planned.binding.payload.clone(),
        });
        let before = fixture.bytes();
        let reply = fixture.api.handle(plan.clone());
        assert_eq!(reply.request, plan);
        assert_eq!(
            confirmed(reply),
            AccountPreparationRecord::Preparation(planned.clone())
        );
        assert_eq!(fixture.bytes(), before);

        let mut conflicting = planned.binding.payload.clone();
        conflicting.copy_codex_config = false;
        let request = AccountPreparationRequest::Preparation(PreparationRequest::Plan {
            request_id: planned.binding.request_id,
            payload: conflicting,
        });
        let reply = fixture.api.handle(request.clone());
        assert_eq!(reply.request, request);
        assert!(matches!(
            reply.outcome,
            AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
        ));
        assert_eq!(fixture.bytes(), before);

        for request in [
            PreparationRequest::Reconcile {
                binding: planned.binding.clone(),
            },
            PreparationRequest::CancelRetained {
                binding: planned.binding.clone(),
            },
            PreparationRequest::Reconcile {
                binding: planned.binding.clone(),
            },
        ] {
            let cancellation = matches!(request, PreparationRequest::CancelRetained { .. });
            let request = AccountPreparationRequest::Preparation(request);
            let reply = fixture.api.handle(request.clone());
            assert_eq!(reply.request, request);
            let AccountPreparationRecord::Preparation(record) = confirmed(reply) else {
                panic!("expected preparation metadata");
            };
            assert_eq!(record.binding, planned.binding);
            if cancellation {
                assert_eq!(record.state, PreparationState::CancelledRetained);
            }
        }
        let registry = fixture.api.store.load().unwrap();
        assert!(registry.pending().is_empty());
        assert!(registry.registered().is_empty());
        assert!(
            !fixture
                .api
                .homes
                .path(Provider::Codex, planned.binding.key)
                .exists()
        );

        let other = Fixture::new();
        let request = AccountPreparationRequest::Preparation(PreparationRequest::Reconcile {
            binding: planned.binding.clone(),
        });
        assert!(matches!(
            other.api.handle(request).outcome,
            AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
        ));
        assert!(!other.registry_directory.join("accounts.toml").exists());
    }

    #[test]
    fn reconciliation_and_cancellation_require_every_binding_field() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let mut changed = Vec::new();
        let mut binding = planned.binding.clone();
        binding.request_id = Uuid::new_v4();
        changed.push(binding);
        let mut binding = planned.binding.clone();
        binding.installation_id = Uuid::new_v4();
        changed.push(binding);
        let mut binding = planned.binding.clone();
        binding.key = Uuid::new_v4();
        changed.push(binding);
        let mut binding = planned.binding.clone();
        binding.payload.provider = Provider::Claude;
        changed.push(binding);
        let mut binding = planned.binding.clone();
        binding.payload.copy_codex_config = false;
        changed.push(binding);
        let before = fixture.bytes();
        for binding in changed {
            for request in [
                PreparationRequest::Reconcile {
                    binding: binding.clone(),
                },
                PreparationRequest::CancelRetained { binding },
            ] {
                let request = AccountPreparationRequest::Preparation(request);
                let reply = fixture.api.handle(request.clone());
                assert_eq!(reply.request, request);
                assert!(matches!(
                    reply.outcome,
                    AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
                ));
                assert_eq!(fixture.bytes(), before);
            }
        }
    }

    #[test]
    fn existing_resume_and_both_cancellation_targets_keep_exact_identity() {
        let fixture = Fixture::new();
        let target = fixture.observed_pending();
        let resume = AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
            target: target.clone(),
        });
        let reply = fixture.api.handle(resume.clone());
        assert_eq!(reply.request, resume);
        let resumed = pending_record(reply);
        assert_eq!(resumed.state, ExistingPendingState::Pending);
        let operation_id = Uuid::new_v4();
        let cancel = AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
            operation_id,
            target,
        });
        let reply = fixture.api.handle(cancel.clone());
        assert_eq!(reply.request, cancel);
        let cancelled = pending_record(reply);
        assert_eq!(cancelled.binding, resumed.binding);
        assert_eq!(cancelled.state, ExistingPendingState::CancelledRetained);
        let before = fixture.bytes();
        for target in [
            match cancel {
                AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                    target,
                    ..
                }) => target,
                _ => unreachable!(),
            },
            ExistingPendingTarget::Bound {
                binding: cancelled.binding.clone(),
            },
        ] {
            let request =
                AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                    operation_id,
                    target,
                });
            let reply = fixture.api.handle(request.clone());
            assert_eq!(reply.request, request);
            assert_eq!(pending_record(reply), cancelled);
            assert_eq!(fixture.bytes(), before);
        }
        let registry = fixture.api.store.load().unwrap();
        assert_eq!(registry.pending().len(), 1);
        assert!(registry.registered().is_empty());
        assert!(
            !fixture
                .api
                .homes
                .path(Provider::Codex, resumed.binding.key)
                .exists()
        );
    }

    #[test]
    fn existing_requests_reject_every_changed_target_field() {
        let fixture = Fixture::new();
        let target = fixture.observed_pending();
        let resumed = pending_record(fixture.api.handle(
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
                target: target.clone(),
            }),
        ));
        let mut changed = Vec::new();
        let ExistingPendingTarget::Observed {
            provider,
            key,
            started_at,
            expected_installation,
        } = target
        else {
            unreachable!();
        };
        for (provider, key, started_at, expected_installation) in [
            (Provider::Claude, key, started_at, expected_installation),
            (provider, Uuid::new_v4(), started_at, expected_installation),
            (provider, key, started_at + 1, expected_installation),
            (provider, key, started_at, Some(Uuid::new_v4())),
        ] {
            changed.push(ExistingPendingTarget::Observed {
                provider,
                key,
                started_at,
                expected_installation,
            });
        }
        let mut binding = resumed.binding.clone();
        binding.installation_id = Uuid::new_v4();
        changed.push(ExistingPendingTarget::Bound { binding });
        let mut binding = resumed.binding.clone();
        binding.provider = Provider::Claude;
        changed.push(ExistingPendingTarget::Bound { binding });
        let mut binding = resumed.binding.clone();
        binding.key = Uuid::new_v4();
        changed.push(ExistingPendingTarget::Bound { binding });
        let mut binding = resumed.binding.clone();
        binding.started_at += 1;
        changed.push(ExistingPendingTarget::Bound { binding });
        let mut binding = resumed.binding.clone();
        binding.generation_id = Uuid::new_v4();
        changed.push(ExistingPendingTarget::Bound { binding });
        let before = fixture.bytes();
        for target in changed {
            for request in [
                ExistingPendingRequest::Resume {
                    target: target.clone(),
                },
                ExistingPendingRequest::Cancel {
                    operation_id: Uuid::new_v4(),
                    target,
                },
            ] {
                let request = AccountPreparationRequest::ExistingPending(request);
                let reply = fixture.api.handle(request.clone());
                assert_eq!(reply.request, request);
                assert!(matches!(
                    reply.outcome,
                    AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
                ));
                assert_eq!(fixture.bytes(), before);
            }
        }
    }

    #[test]
    fn original_none_observation_is_never_upgraded_to_a_persisted_binding() {
        let fixture = Fixture::new();
        let target = fixture.observed_pending();
        pending_record(
            fixture
                .api
                .handle(AccountPreparationRequest::ExistingPending(
                    ExistingPendingRequest::Resume {
                        target: target.clone(),
                    },
                )),
        );
        let ExistingPendingTarget::Observed {
            provider,
            key,
            started_at,
            ..
        } = target
        else {
            unreachable!();
        };
        let original = ExistingPendingTarget::Observed {
            provider,
            key,
            started_at,
            expected_installation: None,
        };
        let before = fixture.bytes();
        for request in [
            ExistingPendingRequest::Resume {
                target: original.clone(),
            },
            ExistingPendingRequest::Cancel {
                operation_id: Uuid::new_v4(),
                target: original,
            },
        ] {
            let request = AccountPreparationRequest::ExistingPending(request);
            let reply = fixture.api.handle(request.clone());
            assert_eq!(reply.request, request);
            assert!(matches!(
                reply.outcome,
                AccountPreparationOutcome::NotAttempted { .. }
            ));
            assert_eq!(fixture.bytes(), before);
        }
        assert!(!fixture.directory.path().join("homes").exists());
    }

    #[test]
    fn cancellation_error_keeps_resolved_observation_without_retargeting() {
        let fixture = Fixture::new();
        let target = fixture.observed_pending();
        let _lifecycle = fixture.api.homes.lifecycle_lock().unwrap();
        let request = AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
            operation_id: Uuid::new_v4(),
            target: target.clone(),
        });
        let reply = fixture.api.handle(request.clone());
        assert_eq!(reply.request, request);
        let AccountPreparationOutcome::ReconcileRequired {
            observed: Some(AccountPreparationRecord::ExistingPending(observed)),
            ..
        } = reply.outcome
        else {
            panic!("resolved metadata cannot confirm failed cancellation");
        };
        assert_eq!(observed.state, ExistingPendingState::Pending);
        let registry = fixture.api.store.load().unwrap();
        assert_eq!(
            ExistingPendingRecord::from(registry.existing_pending(observed.binding.key).unwrap()),
            observed
        );
        let ExistingPendingTarget::Observed {
            provider,
            key,
            started_at,
            expected_installation,
        } = target
        else {
            unreachable!();
        };
        assert_eq!(observed.binding.provider, provider);
        assert_eq!(observed.binding.key, key);
        assert_eq!(observed.binding.started_at, started_at);
        assert_eq!(
            Some(observed.binding.installation_id),
            expected_installation
        );
    }

    #[test]
    fn busy_registry_is_uncertain_and_does_not_try_a_blocking_observation() {
        let fixture = Fixture::new();
        let planned = fixture.plan();
        let before = fixture.bytes();
        let _lock = crate::instance::owned_lock(
            &fixture.registry_directory.join("accounts.toml.lock"),
            libc::LOCK_EX,
        )
        .unwrap();
        let request = AccountPreparationRequest::Preparation(PreparationRequest::Reconcile {
            binding: planned.binding,
        });
        let reply = fixture.api.handle(request.clone());
        assert_eq!(reply.request, request);
        assert!(matches!(
            reply.outcome,
            AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
        ));
        assert_eq!(fixture.bytes(), before);
    }

    #[test]
    fn confirmed_registered_history_does_not_establish_a_current_account() {
        let fixture = Fixture::new();
        let target = fixture.observed_pending();
        let reply = fixture
            .api
            .handle(AccountPreparationRequest::ExistingPending(
                ExistingPendingRequest::Resume { target },
            ));
        let resumed = pending_record(reply);
        let ExistingPendingBinding { provider, key, .. } = resumed.binding;
        fixture
            .api
            .store
            .update(|registry| {
                registry
                    .register(
                        Registered {
                            key,
                            provider,
                            linked_home: None,
                            label: None,
                            email: Some("metadata-fixture@example.invalid".into()),
                            organization: None,
                            added_at: 1,
                        },
                        &[],
                    )
                    .unwrap();
                registry.unregister(key);
            })
            .unwrap();
        let request = AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
            target: ExistingPendingTarget::Bound {
                binding: resumed.binding,
            },
        });
        let reply = fixture.api.handle(request.clone());
        assert_eq!(reply.request, request);
        assert_eq!(
            pending_record(reply).state,
            ExistingPendingState::Registered
        );
        assert!(fixture.api.store.load().unwrap().find(key).is_none());
        assert!(!fixture.api.homes.path(provider, key).exists());
    }
}
