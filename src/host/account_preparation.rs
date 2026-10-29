//! Physical Create admission for the owning daemon's retained preparation.
//! Replies confirm Store metadata only; this path never starts a provider.

use super::{
    account_lifecycle::{PreparedAdmission, PreparedCreationReservation},
    delivery::DeliveryQueue,
    state::{ClientId, SharedHost},
};
use crate::{
    account::{
        Account,
        home::Homes,
        home_variable, inherited_selections,
        preparation::{
            AccountPreparationOutcome, AccountPreparationRecord, AccountPreparationReply,
            AccountPreparationRequest, Preparation, PreparationRequest, PreparationState,
        },
        registry::Store,
    },
    agent::Provider,
    model::HostSessionProof,
    terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode},
};
use std::{collections::BTreeMap, io, path::PathBuf, sync::Arc};

/// Host-derived operands, captured once. These paths never come from a request
/// and are not authority to read anything beyond the fixed seed whitelist.
pub(super) struct SeedSelection {
    pub(super) system: PathBuf,
    pub(super) claude_global: Option<PathBuf>,
}

impl SeedSelection {
    fn owning(provider: Provider) -> io::Result<Self> {
        let environment = [
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            crate::account::SELECTION_MARKER,
        ]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name, value)))
        .collect::<BTreeMap<_, _>>();
        Self::from_environment(provider, &|name| environment.get(name).cloned())
    }

    fn from_environment(
        provider: Provider,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> io::Result<Self> {
        let system = Account::system(provider)
            .home_directory(environment)
            .ok_or_else(|| {
                io::Error::other("The device System configuration source is unavailable")
            })?;
        let overridden = home_variable(provider).is_some_and(|variable| {
            environment(variable).is_some_and(|value| !value.is_empty())
                && !inherited_selections(environment).contains(&variable)
        });
        let claude_global = if provider == Provider::Claude {
            Some(if overridden {
                system.join(".claude.json")
            } else {
                PathBuf::from(
                    environment("HOME")
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            io::Error::other("The device HOME configuration source is unavailable")
                        })?,
                )
                .join(".claude.json")
            })
        } else {
            None
        };
        Ok(Self {
            system,
            claude_global,
        })
    }
}

/// The original admitted connection, not a numeric client ID that can later
/// be registered again. Once physical admission succeeds its immutable token
/// owns just this operation; a lost reply does not cause authority fallback.
pub(super) struct OriginalCaller {
    client: ClientId,
    delivery: Arc<DeliveryQueue>,
    proof: HostSessionProof,
}

impl OriginalCaller {
    pub(super) fn capture(host: &SharedHost, client: ClientId) -> Result<Self, ProtocolError> {
        let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.agent_clients.contains_key(&client)
            || !matches!(
                state.account_preparation_callers.get(&client),
                Some(super::agent_execution::terminals::AccountPreparationCaller::User)
            )
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "Account preparation requires the original connection's explicit User scope",
                false,
            ));
        }
        let delivery = state.clients.get(&client).cloned().ok_or_else(|| {
            ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "The original User connection is unavailable",
                false,
            )
        })?;
        Ok(Self {
            client,
            delivery,
            proof: state.proof().clone(),
        })
    }

    pub(super) fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    pub(super) fn revalidate(
        &self,
        host: &SharedHost,
        request: &ClientRequest,
    ) -> Result<(), ProtocolError> {
        {
            let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.proof() != &self.proof
                || state.agent_clients.contains_key(&self.client)
                || !matches!(
                    state.account_preparation_callers.get(&self.client),
                    Some(super::agent_execution::terminals::AccountPreparationCaller::User)
                )
                || !state
                    .clients
                    .get(&self.client)
                    .is_some_and(|current| Arc::ptr_eq(current, &self.delivery))
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    "The original User connection or daemon changed during account preparation",
                    false,
                ));
            }
        }
        super::client::admit_preparation_create(host, self.client, request)
    }
}

struct OwningStores {
    store: Store,
    homes: Homes,
}

fn create_stores() -> Option<OwningStores> {
    #[cfg(test)]
    {
        // Create fixtures must explicitly install their private operands on
        // the actual handler thread. A missing fixture never reads ambient
        // installation/account state to turn a failed test into a real effect.
        TEST_STORES.with(|slot| slot.borrow().clone())
    }
    #[cfg(not(test))]
    {
        Some(OwningStores {
            store: Store::installation()?,
            homes: Homes::installation()?,
        })
    }
}

pub(super) fn metadata_stores() -> (Option<Store>, Option<Homes>) {
    #[cfg(test)]
    {
        match TEST_STORES.with(|slot| slot.borrow().clone()) {
            Some(stores) => (Some(stores.store), Some(stores.homes)),
            None => (None, None),
        }
    }
    #[cfg(not(test))]
    {
        (Store::installation(), Homes::installation())
    }
}

/// Resolve only the owning registry. Observing identity must not resolve Homes
/// or enter account discovery. Tests require the private handler-thread fixture.
pub(super) fn observation_store() -> Option<Store> {
    #[cfg(test)]
    {
        TEST_STORES.with(|slot| slot.borrow().as_ref().map(|stores| stores.store.clone()))
    }
    #[cfg(not(test))]
    {
        Store::installation()
    }
}

pub(super) fn seed_selection(provider: Provider) -> io::Result<SeedSelection> {
    #[cfg(test)]
    {
        let _ = provider;
        TEST_SOURCE
            .with(|slot| slot.borrow().clone())
            .ok_or_else(|| {
                io::Error::other("The private Create source fixture is unavailable on this handler")
            })
    }
    #[cfg(not(test))]
    {
        SeedSelection::owning(provider)
    }
}

pub(super) fn create(
    host: &SharedHost,
    client: ClientId,
    request: AccountPreparationRequest,
) -> Result<AccountPreparationReply, ProtocolError> {
    let AccountPreparationRequest::Preparation(PreparationRequest::Create { binding }) = &request
    else {
        return Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "Physical preparation admission requires an exact Create request",
            false,
        ));
    };
    let wire_request = ClientRequest::DeviceOperation {
        operation: crate::device_service::Operation::AccountPreparation {
            request: request.clone(),
        },
    };
    let gate = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .control_gate
        .clone();
    let (caller, stores, observed) = {
        let _reader = gate.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let caller = OriginalCaller::capture(host, client)?;
        super::client::admit_preparation_create(host, client, &wire_request)?;
        super::account_lifecycle::ensure_idle(host)?;
        let Some(stores) = create_stores() else {
            return Ok(AccountPreparationReply {
                request,
                outcome: AccountPreparationOutcome::NotAttempted {
                    message: "The device account preparation store is unavailable".into(),
                },
            });
        };
        let observed = match stores.store.try_reconcile_preparation(binding) {
            Ok(record) => record,
            Err(error) => return Ok(uncertain(request, error.to_string(), None)),
        };
        (caller, stores, observed)
    };
    if observed.state != PreparationState::Planned {
        return Ok(confirmed(request, observed));
    }

    // No global reader remains here. Reservation installation and actual
    // receipt enrollment both need their own short control writer.
    let admission = PreparedCreationReservation::reserve(
        host,
        stores.store.clone(),
        stores.homes,
        binding,
        || seed_selection(binding.payload.provider),
        || caller.revalidate(host, &wire_request),
    );
    let result = match admission {
        Ok(PreparedAdmission::Reserved(reservation)) => {
            let result = reservation.create();
            // Keep the physical token alive until the exact typed settlement
            // is constructed. Delivery can still be lost and needs replay.
            return Ok(match result {
                Ok(record) => confirmed(request, record),
                Err(error) => uncertain(request, error.to_string(), Some(observed)),
            });
        }
        Ok(PreparedAdmission::ReplayNeeded) => {
            let _reader = gate.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = caller
                .revalidate(host, &wire_request)
                .and_then(|()| super::account_lifecycle::ensure_idle(host))
            {
                return Ok(uncertain(request, error.message, Some(observed)));
            }
            stores.store.try_reconcile_preparation(binding)
        }
        Err(error) => return Ok(uncertain(request, error.message, Some(observed))),
    };
    Ok(match result {
        Ok(record) => confirmed(request, record),
        Err(error) => uncertain(request, error.to_string(), Some(observed)),
    })
}

fn confirmed(request: AccountPreparationRequest, record: Preparation) -> AccountPreparationReply {
    AccountPreparationReply {
        request,
        outcome: AccountPreparationOutcome::Confirmed {
            record: AccountPreparationRecord::Preparation(record),
        },
    }
}

fn uncertain(
    request: AccountPreparationRequest,
    message: String,
    observed: Option<Preparation>,
) -> AccountPreparationReply {
    AccountPreparationReply {
        request,
        outcome: AccountPreparationOutcome::ReconcileRequired {
            message,
            observed: observed.map(AccountPreparationRecord::Preparation),
        },
    }
}

// Test overrides are thread-local, explicitly scoped private fixture operands.
// Production has only the owning daemon resolution above. No environment or
// inherited authority is rewritten to make a test pass.
#[cfg(test)]
impl Clone for OwningStores {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            homes: self.homes.clone(),
        }
    }
}

#[cfg(test)]
impl Clone for SeedSelection {
    fn clone(&self) -> Self {
        Self {
            system: self.system.clone(),
            claude_global: self.claude_global.clone(),
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_STORES: std::cell::RefCell<Option<OwningStores>> = const { std::cell::RefCell::new(None) };
    static TEST_SOURCE: std::cell::RefCell<Option<SeedSelection>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn with_fixture<T>(
    store: Store,
    homes: Homes,
    source: SeedSelection,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<OwningStores>, Option<SeedSelection>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_STORES.with(|slot| {
                slot.replace(self.0.take());
            });
            TEST_SOURCE.with(|slot| {
                slot.replace(self.1.take());
            });
        }
    }
    let _restore = Restore(
        TEST_STORES.with(|slot| slot.replace(Some(OwningStores { store, homes }))),
        TEST_SOURCE.with(|slot| slot.replace(Some(source))),
    );
    run()
}

#[cfg(test)]
mod tests;
