//! User-admitted fixed configuration metadata and explicit creation.
//! No provider, runtime, credential contents or execution policy is consulted.
use super::{SharedHost, account_lifecycle::Reservation};
use crate::{
    account::{
        Account,
        coding_config::{
            self, CreateReport, CreateRequest, InspectRequest, Observation, Outcome, Presence,
            Refusal,
        },
        home::Homes,
        registry::{Registry, Store},
    },
    agent::Provider,
    model::HostSessionProof,
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};

const CONTEXT_CHANGED: &str = "The selected account changed; inspect its configuration again";
const REGISTRY_UNAVAILABLE: &str = "The selected account registry is unavailable or busy";

fn refused(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

fn proof(host: &SharedHost) -> HostSessionProof {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone()
}

fn selected(
    registry: &Registry,
    homes: &Homes,
    request: &InspectRequest,
) -> Result<Account, ProtocolError> {
    request.validate().map_err(refused)?;
    if registry.is_newer() {
        return Err(refused("The account registry requires a newer cagents"));
    }
    let account = registry
        .accounts(Some(homes))
        .into_iter()
        .find(|account| account.id() == request.account && account.provider == Provider::Codex)
        .filter(|account| !account.is_system())
        .ok_or_else(|| refused("The selected registered Codex account is unavailable"))?;
    let record = account
        .registry_key()
        .and_then(|key| registry.find(key))
        .ok_or_else(|| refused(CONTEXT_CHANGED))?;
    if record.email.as_deref().is_some_and(|email| {
        !email
            .trim()
            .eq_ignore_ascii_case(request.expected_email.trim())
    }) {
        return Err(refused(
            "The expected identity conflicts with the selected registration",
        ));
    }
    Ok(account)
}

fn installation(
    request: &InspectRequest,
) -> Result<(Store, Homes, Registry, Account), ProtocolError> {
    request.validate().map_err(refused)?;
    let store = Store::installation().ok_or_else(|| refused(REGISTRY_UNAVAILABLE))?;
    let homes = Homes::installation().ok_or_else(|| refused(REGISTRY_UNAVAILABLE))?;
    let registry = store
        .try_load()
        .map_err(|_| refused(REGISTRY_UNAVAILABLE))?;
    let account = selected(&registry, &homes, request)?;
    Ok((store, homes, registry, account))
}

pub(super) fn inspect(
    host: &SharedHost,
    request: InspectRequest,
) -> Result<Observation, ProtocolError> {
    let (store, homes, registry, account) = installation(&request)?;
    let reservation = Reservation::install(host, &[account])
        .map_err(|_| refused("The selected account is busy or changed; inspect again"))?;
    inspect_reserved(host, &request, &store, &homes, &registry, &reservation)
}

fn inspect_reserved(
    host: &SharedHost,
    request: &InspectRequest,
    store: &Store,
    homes: &Homes,
    registry: &Registry,
    reservation: &Reservation,
) -> Result<Observation, ProtocolError> {
    let account = selected(registry, homes, request)?;
    let host_proof = proof(host);
    // Validate the pre-admission registration against the installed token too:
    // an identity update during reservation installation must not be adopted.
    let root = reservation
        .coding_config_root(registry, &request.account, request.expected_home)
        .map_err(|_| refused(CONTEXT_CHANGED))?;
    let before = coding_config::presence(&root);
    let latest = store
        .try_load()
        .map_err(|_| refused(REGISTRY_UNAVAILABLE))?;
    if selected(&latest, homes, request)? != account || proof(host) != host_proof {
        return Err(refused(CONTEXT_CHANGED));
    }
    let checked = reservation
        .coding_config_root(&latest, &request.account, request.expected_home)
        .map_err(|_| refused(CONTEXT_CHANGED))?;
    let after = coding_config::presence(&checked);
    let final_registry = store
        .try_load()
        .map_err(|_| refused(REGISTRY_UNAVAILABLE))?;
    if selected(&final_registry, homes, request)? != account || proof(host) != host_proof {
        return Err(refused(CONTEXT_CHANGED));
    }
    let _final_root = reservation
        .coding_config_root(&final_registry, &request.account, request.expected_home)
        .map_err(|_| refused(CONTEXT_CHANGED))?;
    Ok(Observation {
        account: request.account.clone(),
        host: host_proof,
        home: request.expected_home,
        presence: if before == after {
            after
        } else {
            Presence::Unknown
        },
        expected_email: request.expected_email.clone(),
    })
}

fn validate_create(host: &SharedHost, request: &CreateRequest) -> Result<(), ProtocolError> {
    request.validate().map_err(refused)?;
    if request.observation.host != proof(host) {
        return Err(refused(
            "The selected daemon changed; inspect the account configuration again",
        ));
    }
    Ok(())
}

pub(super) fn create(
    host: &SharedHost,
    request: CreateRequest,
) -> Result<CreateReport, ProtocolError> {
    // Reject stale backend context before any registry or filesystem lookup.
    validate_create(host, &request)?;
    let inspect = request.observation.request();
    let (store, homes, registry, account) = match installation(&inspect) {
        Ok(installation) => installation,
        Err(_) => {
            return Ok(report(
                &request,
                Outcome::NotCreated(Refusal::AccountUnavailable),
            ));
        }
    };
    let reservation = match Reservation::install(host, &[account]) {
        Ok(reservation) => reservation,
        Err(_) => {
            return Ok(report(
                &request,
                Outcome::NotCreated(Refusal::AccountBusyOrChanged),
            ));
        }
    };
    create_reserved(host, &request, &store, &homes, &registry, &reservation)
}

fn create_reserved(
    host: &SharedHost,
    request: &CreateRequest,
    store: &Store,
    homes: &Homes,
    registry: &Registry,
    reservation: &Reservation,
) -> Result<CreateReport, ProtocolError> {
    validate_create(host, request)?;
    let inspect = request.observation.request();
    if selected(registry, homes, &inspect).is_err() {
        return Ok(report(
            request,
            Outcome::NotCreated(Refusal::ContextChanged),
        ));
    }
    // Bind the expected email's registration snapshot to this reservation.
    // The Store subsequently repeats this exact context under its real lease.
    let Ok(_root) =
        reservation.coding_config_root(registry, &inspect.account, inspect.expected_home)
    else {
        return Ok(report(
            request,
            Outcome::NotCreated(Refusal::ContextChanged),
        ));
    };
    let outcome = store
        .try_initialize_codex_config_reserved(
            reservation,
            homes,
            &inspect.account,
            inspect.expected_home,
            request.network_access,
        )
        .unwrap_or_else(|reason| {
            use crate::account::registry::CodingConfigRefusal as R;
            // These Store errors precede publication. After-link failures are
            // Outcome::CreationUncertain and must never enter this projection.
            Outcome::NotCreated(match reason {
                R::RegistryUnavailable => Refusal::RegistryUnavailable,
                R::RegistryBusy => Refusal::RegistryBusy,
                R::RegistryNewer => Refusal::RegistryNewer,
                R::InvalidAccount => Refusal::AccountUnavailable,
                R::ContextChanged => Refusal::ContextChanged,
                R::SharedHome => Refusal::SharedHome,
                R::SharedConfig => Refusal::SharedConfig,
                R::CensusUnavailable => Refusal::CensusUnavailable,
                R::CensusLimit => Refusal::CensusLimit,
            })
        });
    Ok(report(request, outcome))
}

fn report(request: &CreateRequest, outcome: Outcome) -> CreateReport {
    CreateReport {
        account: request.observation.account.clone(),
        host: request.observation.host.clone(),
        home: request.observation.home,
        network_access: request.network_access,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Identity, readiness::PhysicalIdentity};
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
        path::PathBuf,
    };

    struct Fixture {
        _root: tempfile::TempDir,
        host: SharedHost,
        store: Store,
        homes: Homes,
        home: PathBuf,
        registry: Registry,
        account: Account,
        request: InspectRequest,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("selected");
        fs::create_dir(&home).unwrap();
        let store = Store::at(root.path().join("registry"));
        let homes = Homes::at(root.path().join("homes"));
        let key = uuid::Uuid::new_v4();
        let account = Account::linked(Provider::Codex, key, home.clone());
        let registry = store
            .update(|registry| {
                registry
                    .link(Provider::Codex, key, home.clone(), None, Some(&homes), 1)
                    .unwrap();
                registry.observe_identity(
                    key,
                    &Identity {
                        email: Some("selected@example.test".into()),
                        ..Identity::default()
                    },
                );
            })
            .unwrap()
            .0;
        let metadata = fs::metadata(&home).unwrap();
        let request = InspectRequest {
            account: account.id(),
            expected_home: PhysicalIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            expected_email: "selected@example.test".into(),
        };
        Fixture {
            _root: root,
            host: Reservation::test_host(),
            store,
            homes,
            home,
            registry,
            account,
            request,
        }
    }

    fn reserve(f: &Fixture) -> Reservation {
        Reservation::test_install(&f.host, &[f.account.clone()], f.store.clone(), &f.homes).unwrap()
    }

    #[test]
    fn inspection_reports_fixed_entry_presence_and_preserves_registry_and_files() {
        for occupied in 0..4 {
            let f = fixture();
            let config = f.home.join("config.toml");
            match occupied {
                0 => {}
                1 => fs::write(&config, b"opaque private fixture").unwrap(),
                2 => symlink("missing-configuration", &config).unwrap(),
                3 => fs::create_dir(&config).unwrap(),
                _ => unreachable!(),
            }
            let before = f.store.load().unwrap();
            let reservation = reserve(&f);
            let observed = inspect_reserved(
                &f.host,
                &f.request,
                &f.store,
                &f.homes,
                &f.registry,
                &reservation,
            )
            .unwrap();
            assert_eq!(observed.account, f.account.id());
            assert_eq!(observed.host, proof(&f.host));
            assert_eq!(observed.home, f.request.expected_home);
            assert_eq!(observed.expected_email, f.request.expected_email);
            assert_eq!(
                observed.presence,
                if occupied == 0 {
                    Presence::Absent
                } else {
                    Presence::Present
                }
            );
            assert_eq!(f.store.load().unwrap(), before);
            if occupied == 0 {
                assert!(!config.exists());
            }
            if occupied == 2 {
                assert_eq!(
                    fs::read_link(&config).unwrap(),
                    PathBuf::from("missing-configuration")
                );
            }
        }
    }

    #[test]
    fn inspection_refuses_stale_home_and_registration_identity_without_materializing_config() {
        let f = fixture();
        let reservation = reserve(&f);
        let mut wrong_home = f.request.clone();
        wrong_home.expected_home.inode ^= 1;
        assert!(
            inspect_reserved(
                &f.host,
                &wrong_home,
                &f.store,
                &f.homes,
                &f.registry,
                &reservation
            )
            .is_err()
        );
        let mut wrong_email = f.request.clone();
        wrong_email.expected_email = "different@example.test".into();
        assert!(
            inspect_reserved(
                &f.host,
                &wrong_email,
                &f.store,
                &f.homes,
                &f.registry,
                &reservation
            )
            .is_err()
        );
        f.store
            .update(|registry| {
                registry.adopt_identity(
                    f.account.registry_key().unwrap(),
                    &Identity {
                        email: Some("different@example.test".into()),
                        ..Identity::default()
                    },
                )
            })
            .unwrap();
        assert!(
            inspect_reserved(
                &f.host,
                &f.request,
                &f.store,
                &f.homes,
                &f.registry,
                &reservation
            )
            .is_err()
        );
        assert!(!f.home.join("config.toml").exists());
    }

    #[test]
    fn create_refuses_stale_daemon_and_non_absent_context_before_installation_lookup() {
        let f = fixture();
        let mut observation = Observation {
            account: f.request.account.clone(),
            host: proof(&f.host),
            home: f.request.expected_home,
            presence: Presence::Absent,
            expected_email: f.request.expected_email.clone(),
        };
        observation.host.host_incarnation =
            crate::model::HostIncarnation::new("stale-other-daemon");
        let error = create(
            &f.host,
            CreateRequest {
                observation: observation.clone(),
                network_access: false,
            },
        )
        .unwrap_err();
        assert!(error.message.starts_with("The selected daemon changed"));
        observation.host = proof(&f.host);
        observation.presence = Presence::Unknown;
        let error = create(
            &f.host,
            CreateRequest {
                observation,
                network_access: true,
            },
        )
        .unwrap_err();
        assert!(
            error
                .message
                .starts_with("Inspect the absent account configuration")
        );
        assert!(!f.home.join("config.toml").exists());
    }

    #[test]
    fn registry_contention_returns_definite_not_created_without_changing_files() {
        let f = fixture();
        let reservation = reserve(&f);
        let registry_path = f._root.path().join("registry/accounts.toml");
        let before = fs::read(&registry_path).unwrap();
        let lease = f.store.hold_registry_lock_for_account_test().unwrap();
        let request = CreateRequest {
            observation: Observation {
                account: f.request.account.clone(),
                host: proof(&f.host),
                home: f.request.expected_home,
                presence: Presence::Absent,
                expected_email: f.request.expected_email.clone(),
            },
            network_access: false,
        };
        let result = create_reserved(
            &f.host,
            &request,
            &f.store,
            &f.homes,
            &f.registry,
            &reservation,
        )
        .unwrap();
        assert_eq!(result.outcome, Outcome::NotCreated(Refusal::RegistryBusy));
        assert!(!f.home.join("config.toml").exists());
        assert_eq!(fs::read(&registry_path).unwrap(), before);
        drop(lease);
    }

    #[test]
    fn creation_does_not_adopt_an_identity_changed_during_reservation_installation() {
        let f = fixture();
        // The frontend/initial read names X; reservation installation sees Y.
        // The unchanged account/path alone must not silently authorize Y.
        f.store
            .update(|registry| {
                registry.adopt_identity(
                    f.account.registry_key().unwrap(),
                    &Identity {
                        email: Some("replacement@example.test".into()),
                        ..Identity::default()
                    },
                )
            })
            .unwrap();
        let reservation = reserve(&f);
        let request = CreateRequest {
            observation: Observation {
                account: f.request.account.clone(),
                host: proof(&f.host),
                home: f.request.expected_home,
                presence: Presence::Absent,
                expected_email: f.request.expected_email.clone(),
            },
            network_access: false,
        };
        let result = create_reserved(
            &f.host,
            &request,
            &f.store,
            &f.homes,
            &f.registry,
            &reservation,
        )
        .unwrap();
        assert_eq!(result.outcome, Outcome::NotCreated(Refusal::ContextChanged));
        assert!(!f.home.join("config.toml").exists());
        assert_eq!(
            f.store
                .load()
                .unwrap()
                .find(f.account.registry_key().unwrap())
                .unwrap()
                .email
                .as_deref(),
            Some("replacement@example.test")
        );
    }
}
