//! Scoped admission for account-home mutations. Install reservations under the
//! short control writer; never retain a map or HostState lock during I/O.
use super::state::SharedHost;
use crate::{
    account::{
        Account,
        registry::{Registry, Store},
    },
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type Map = BTreeMap<uuid::Uuid, Vec<Home>>;
pub(super) type Reservations = Arc<Mutex<Map>>;

#[derive(Clone)]
pub(super) struct Home {
    path: PathBuf,
    canonical: PathBuf,
    pin: Option<Arc<File>>,
}

fn refusal(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

impl Home {
    fn capture(path: PathBuf) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("Account homes must be absolute".into());
        }
        let canonical = normalize(&path)?;
        let pin = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&canonical)
        {
            Ok(file) => Some(Arc::new(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot reserve account home: {error}")),
        };
        let home = Self {
            path,
            canonical,
            pin,
        };
        home.verify()?;
        Ok(home)
    }

    fn verify(&self) -> Result<(), String> {
        if normalize(&self.path)? != self.canonical {
            return Err("The account home changed while its operation was pending; reload accounts and retry".into());
        }
        match (&self.pin, std::fs::metadata(&self.path)) {
            (Some(pin), Ok(current)) => {
                let original = pin.metadata().map_err(|error| error.to_string())?;
                if current.is_dir() && (current.dev(), current.ino()) == (original.dev(), original.ino()) { Ok(()) }
                else { Err("The account home was replaced while its operation was pending".into()) }
            }
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("The account home appeared, disappeared or became inaccessible during its operation".into()),
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.canonical.starts_with(&other.canonical)
            || other.canonical.starts_with(&self.canonical)
            || match (&self.pin, &other.pin) {
                (Some(left), Some(right)) => left
                    .metadata()
                    .ok()
                    .zip(right.metadata().ok())
                    .is_some_and(|(left, right)| {
                        (left.dev(), left.ino()) == (right.dev(), right.ino())
                    }),
                _ => false,
            }
    }
}

/// Resolve aliases even for a not-yet-created descendant without interpreting
/// `..` through a missing directory. Missing-home cleanup remains possible.
fn normalize(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Account home must be an absolute path without parent traversal".into());
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for part in suffix.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or("Account home has no existing parent")?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or("Account home has no existing parent")?;
            }
            Err(error) => return Err(format!("Cannot resolve account home: {error}")),
        }
    }
}

pub(super) struct Reservation {
    map: Reservations,
    token: uuid::Uuid,
    homes: Vec<Home>,
    /// Only relevant registrations, not the global revision: unrelated edits
    /// must be preserved while a provider is taking time to sign out.
    registered: Vec<(
        uuid::Uuid,
        Option<crate::account::registry::Registered>,
        Option<crate::account::registry::Pending>,
        bool,
    )>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.token);
    }
}
impl Reservation {
    pub(super) fn install(host: &SharedHost, accounts: &[Account]) -> Result<Self, ProtocolError> {
        let map = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .account_lifecycle
            .clone();
        let homes = accounts
            .iter()
            .map(|account| {
                account
                    .resolved_home()
                    .ok_or_else(|| "This account has no credential directory".to_owned())
                    .and_then(Home::capture)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(refusal)?;
        let registry = Store::installation()
            .ok_or_else(|| refusal("Account registry unavailable"))?
            .load()
            .map_err(|error| refusal(error.to_string()))?;
        let registered = accounts
            .iter()
            .filter_map(Account::registry_key)
            .map(|key| {
                (
                    key,
                    registry.find(key).cloned(),
                    registry
                        .pending()
                        .iter()
                        .find(|pending| pending.key == key)
                        .cloned(),
                    registry.is_archived(key),
                )
            })
            .collect();
        let token = uuid::Uuid::new_v4();
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        if entries.len() >= 64 {
            return Err(refusal(
                "Too many account operations are already pending; wait and retry",
            ));
        }
        if entries
            .values()
            .flatten()
            .any(|other| homes.iter().any(|home| home.overlaps(other)))
        {
            return Err(refusal(
                "An operation is already changing this account home. Wait for it to finish, then retry.",
            ));
        }
        entries.insert(token, homes.clone());
        drop(entries);
        Ok(Self {
            map,
            token,
            homes,
            registered,
        })
    }

    /// Call outside HostState and before each destructive stage/final commit.
    pub(super) fn verify(&self) -> Result<(), String> {
        for home in &self.homes {
            home.verify()?;
        }
        let registry = Store::installation()
            .ok_or("Account registry unavailable")?
            .load()
            .map_err(|error| error.to_string())?;
        for (key, account, pending, archived) in &self.registered {
            if registry.find(*key) != account.as_ref()
                || registry.pending().iter().find(|value| value.key == *key) != pending.as_ref()
                || registry.is_archived(*key) != *archived
            {
                return Err(
                    "The affected account registration changed; reload accounts before retrying"
                        .into(),
                );
            }
        }
        Ok(())
    }
}

/// Caller already owns shared admission through its ensuing startup/write.
/// Clone reservation metadata, then perform path resolution outside all locks.
pub(super) fn ensure_paths(
    host: &SharedHost,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<(), ProtocolError> {
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_lifecycle
        .clone();
    let reserved: Vec<_> = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .flatten()
        .cloned()
        .collect();
    if reserved.is_empty() {
        return Ok(());
    }
    for path in paths {
        let home = Home::capture(path).map_err(refusal)?;
        if reserved.iter().any(|other| home.overlaps(other)) {
            return Err(refusal(
                "This account home is being changed. Wait for the account operation to finish before using or editing it.",
            ));
        }
    }
    Ok(())
}

pub(super) fn ensure_launch(
    host: &SharedHost,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    unset: &BTreeSet<String>,
    inherit: bool,
) -> Result<(), ProtocolError> {
    let mut paths = Vec::new();
    let effective = |name: &str| {
        if unset.contains(name) {
            None
        } else {
            environment
                .get(name)
                .map(std::ffi::OsString::from)
                .or_else(|| inherit.then(|| std::env::var_os(name)).flatten())
        }
    };
    // Terminals can invoke either provider later. Never infer credential-root
    // ownership from a program basename or the provider of a registry alias.
    for (variable, fallback) in [("CLAUDE_CONFIG_DIR", ".claude"), ("CODEX_HOME", ".codex")] {
        if let Some(selected) = effective(variable).filter(|value| !value.is_empty()) {
            paths.push(cwd.join(selected));
        } else if let Some(home) = effective("HOME").filter(|value| !value.is_empty()) {
            paths.push(cwd.join(home).join(fallback));
        }
    }
    ensure_paths(host, paths)
}

pub(super) fn ensure_update(host: &SharedHost, proposed: &Registry) -> Result<(), ProtocolError> {
    let current = Store::installation()
        .ok_or_else(|| refusal("Account registry unavailable"))?
        .load()
        .map_err(|error| refusal(error.to_string()))?;
    let homes = crate::account::home::Homes::installation();
    let changed = current.changed_account_homes(proposed, homes.as_ref());
    ensure_paths(host, changed)
}

pub(super) fn ensure_idle(host: &SharedHost) -> Result<(), ProtocolError> {
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_lifecycle
        .clone();
    if map.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
        Ok(())
    } else {
        Err(refusal(
            "An account operation is still finishing. Wait for it before shutting down or upgrading this daemon.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::{home::Homes, registry::Registered},
        agent::Provider,
        model::{
            BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
            TerminalContinuity, TerminalId, TerminalSession,
        },
        terminal_ipc::{ClientRequest, ServerResponse},
    };
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    fn wait_file(path: &Path) {
        let until = Instant::now() + Duration::from_secs(5);
        while !path.is_file() {
            assert!(
                Instant::now() < until,
                "fixture did not reach {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn request(host: &SharedHost, request: ClientRequest) -> Result<ServerResponse, ProtocolError> {
        super::super::client::process_request(host, 1, request).map(|result| result.0)
    }
    fn lifecycle(
        host: &SharedHost,
        operation: crate::account::remote::Operation,
    ) -> Result<ServerResponse, ProtocolError> {
        request(
            host,
            ClientRequest::DeviceOperation {
                operation: crate::device_service::Operation::AccountLifecycle { operation },
            },
        )
    }
    fn terminal(root: &Path, environment: BTreeMap<String, String>) -> TerminalSession {
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                "while IFS= read -r line; do printf '%s' \"$line\" > \"$1\"; done".into(),
                "fixture".into(),
                root.join("pty-input").display().to_string(),
            ],
            working_directory: root.into(),
            environment,
            unset_environment: Default::default(),
        };
        TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        }
    }
    fn registered(homes: &Homes, store: &Store, provider: Provider, name: &str) -> Account {
        let (key, home) = homes.create(provider, None).unwrap();
        store
            .update(|registry| {
                registry
                    .register(
                        Registered {
                            key,
                            provider,
                            linked_home: None,
                            label: None,
                            email: Some(format!("{name}@fixture.invalid")),
                            organization: None,
                            added_at: 0,
                        },
                        &[],
                    )
                    .unwrap()
            })
            .unwrap();
        Account::managed(provider, key, home)
    }
    fn close(host: &SharedHost, id: TerminalId) {
        request(
            host,
            ClientRequest::CloseTerminalChecked {
                terminal_id: id,
                allow_active: true,
            },
        )
        .unwrap();
        request(host, ClientRequest::Detach { terminal_id: id }).unwrap();
    }

    /// A subprocess owns all environment/config/cache/provider fixtures. The
    /// parent test process never changes its HOME or accesses a real account.
    #[test]
    fn account_lifecycle_reservation_keeps_unrelated_pty_responsive() {
        const CHILD: &str = "CAGENTS_ACCOUNT_RESERVATION_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            for name in ["home", "config", "data", "cache", "runtime"] {
                std::fs::create_dir(root.path().join(name)).unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::account_lifecycle::tests::account_lifecycle_reservation_keeps_unrelated_pty_responsive", "--nocapture"])
                .env_clear().env(CHILD,root.path()).env("HOME",root.path().join("home"))
                .env("XDG_CONFIG_HOME",root.path().join("config")).env("XDG_DATA_HOME",root.path().join("data"))
                .env("XDG_CACHE_HOME",root.path().join("cache")).env("XDG_RUNTIME_DIR",root.path().join("runtime"))
                .env("PATH","/usr/bin:/bin").env("SHELL","/bin/sh").env("TERM","xterm-256color")
                .env("CODEX_HOME",root.path().join("inherited-home"))
                .output().unwrap();
            assert!(
                output.status.success(),
                "isolated fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
        let executable = root.join("fixture-claude");
        std::fs::write(&executable, "#!/bin/sh\nif [ \"$1 $2\" = 'auth logout' ]; then\n : > \"$CLAUDE_CONFIG_DIR/logout-started\"\n while [ -f \"$CLAUDE_CONFIG_DIR/logout-hold\" ]; do /bin/sleep 0.02; done\n : > \"$CLAUDE_CONFIG_DIR/signed-out\"\n exit 0\nfi\nexit 4\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut settings = crate::settings::AppSettings::default();
        settings.agents.insert(
            "claude".into(),
            crate::settings::AgentLaunchSettings {
                executable: Some(executable.display().to_string()),
                ..Default::default()
            },
        );
        crate::settings::save(crate::settings::default_path().unwrap(), &settings).unwrap();
        let homes = Homes::installation().unwrap();
        let store = Store::installation().unwrap();
        let source = registered(&homes, &store, Provider::Claude, "source");
        let other = registered(&homes, &store, Provider::Claude, "other");
        let inherited_home = root.join("inherited-home");
        std::fs::create_dir(&inherited_home).unwrap();
        let inherited_key = uuid::Uuid::new_v4();
        let inherited = Account::linked(Provider::Claude, inherited_key, inherited_home.clone());
        store
            .update(|registry| {
                registry
                    .register(
                        Registered {
                            key: inherited_key,
                            provider: Provider::Claude,
                            linked_home: Some(inherited_home),
                            label: None,
                            email: Some("inherited@fixture.invalid".into()),
                            organization: None,
                            added_at: 0,
                        },
                        &[],
                    )
                    .unwrap()
            })
            .unwrap();
        let source_home = source.resolved_home().unwrap();
        let hold = source_home.join("logout-hold");
        std::fs::write(&hold, "").unwrap();
        let host = Arc::new(super::super::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("account-reservation"),
                boot_id: BootId::new("fixture"),
                host_incarnation: HostIncarnation::new("fixture"),
            }),
        ));
        host.lock().unwrap().register_client(1);
        let unrelated = terminal(
            &root,
            BTreeMap::from([("HOME".into(), root.join("home").display().to_string())]),
        );
        let unrelated_id = unrelated.id;
        request(
            &host,
            ClientRequest::Spawn {
                session: unrelated,
                options: Default::default(),
            },
        )
        .unwrap();
        assert!(
            lifecycle(
                &host,
                crate::account::remote::Operation::Remove { id: inherited.id() }
            )
            .is_err(),
            "an ordinary terminal owns its inherited provider home even when its recipe omits it"
        );
        let worker_host = host.clone();
        let source_id = source.id();
        let worker = std::thread::spawn(move || {
            lifecycle(
                &worker_host,
                crate::account::remote::Operation::Remove { id: source_id },
            )
        });
        struct Release(PathBuf);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let release = Release(hold);
        wait_file(&source_home.join("logout-started"));
        assert!(!source_home.join("signed-out").exists());
        let (sent, received) = std::sync::mpsc::channel();
        let responsive = host.clone();
        let listing = std::thread::spawn(move || {
            sent.send(request(&responsive, ClientRequest::List))
                .unwrap()
        });
        assert!(matches!(
            received
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            ServerResponse::Listed { .. }
        ));
        listing.join().unwrap();
        request(
            &host,
            ClientRequest::Input {
                terminal_id: unrelated_id,
                bytes: b"responsive\n".to_vec(),
            },
        )
        .unwrap();
        wait_file(&root.join("pty-input"));
        assert_eq!(
            std::fs::read_to_string(root.join("pty-input")).unwrap(),
            "responsive"
        );
        let alias = root.join("source-alias");
        std::os::unix::fs::symlink(&source_home, &alias).unwrap();
        let related = terminal(
            &root,
            BTreeMap::from([("CODEX_HOME".into(), alias.display().to_string())]),
        );
        assert!(
            request(
                &host,
                ClientRequest::Spawn {
                    session: related,
                    options: Default::default()
                }
            )
            .is_err(),
            "cross-provider physical aliases must be reserved"
        );
        // Cross-provider aliases share probe locks even for daemon monitor
        // work which does not enter the RPC control gate.
        let cache = crate::account::cache::Cache::installation().unwrap();
        let alias_account = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), alias.clone());
        assert!(crate::account::probe::lock_accounts(&cache, &[&alias_account]).is_err());
        assert!(
            lifecycle(
                &host,
                crate::account::remote::Operation::DeleteFiles {
                    id: source.id(),
                    expected_home: None
                }
            )
            .is_err()
        );
        assert!(
            lifecycle(
                &host,
                crate::account::remote::Operation::Link {
                    provider: Provider::Codex,
                    key: uuid::Uuid::new_v4(),
                    path: alias.display().to_string()
                }
            )
            .is_err()
        );
        let mut current = store.load().unwrap();
        let revision = current.revision();
        current.set_label(&source, Some("busy-source".into()));
        assert!(
            request(
                &host,
                ClientRequest::DeviceOperation {
                    operation: crate::device_service::Operation::AccountsUpdate {
                        expected_revision: revision,
                        registry: serde_json::to_value(current).unwrap()
                    }
                }
            )
            .is_err()
        );
        let mut current = store.load().unwrap();
        let revision = current.revision();
        current.set_label(&other, Some("preserved-other".into()));
        request(
            &host,
            ClientRequest::DeviceOperation {
                operation: crate::device_service::Operation::AccountsUpdate {
                    expected_revision: revision,
                    registry: serde_json::to_value(current).unwrap(),
                },
            },
        )
        .unwrap();
        assert!(ensure_idle(&host).is_err());
        drop(release);
        worker.join().unwrap().unwrap();
        let current = store.load().unwrap();
        assert!(current.is_archived(source.registry_key().unwrap()));
        assert_eq!(current.label(&other), Some("preserved-other"));
        assert!(ensure_idle(&host).is_ok());
        close(&host, unrelated_id);

        // A path replacement never transfers the retained operation's authority.
        let gate = host.lock().unwrap().control_gate.clone();
        let reservation = {
            let _writer = gate.write().unwrap();
            Reservation::install(&host, &[other.clone()]).unwrap()
        };
        assert!(
            ensure_captured_scope(&host, None).is_err(),
            "legacy native adoption cannot enter an active account mutation"
        );
        let scope = crate::account::scope::LaunchAccountScope {
            provider: Provider::Claude,
            canonical_home: source_home.canonicalize().unwrap(),
            device: std::fs::metadata(&source_home).unwrap().dev(),
            inode: std::fs::metadata(&source_home).unwrap().ino(),
            registered_id: None,
            purge_ids: Vec::new(),
        };
        assert!(
            ensure_captured_scope(&host, Some(&scope)).is_ok(),
            "a proved unrelated native home stays available"
        );
        let other_home = other.resolved_home().unwrap();
        let retained = root.join("original-other");
        std::fs::rename(&other_home, &retained).unwrap();
        std::fs::create_dir(&other_home).unwrap();
        std::fs::write(other_home.join("unrelated"), "keep").unwrap();
        assert!(
            crate::account::remote::execute_reserved(
                crate::account::remote::Operation::DeleteFiles {
                    id: other.id(),
                    expected_home: None
                },
                gate.clone(),
                || reservation.verify()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(other_home.join("unrelated")).unwrap(),
            "keep"
        );
        assert!(!other_home.join("logout-started").exists());
        assert!(retained.is_dir());
        assert!(
            store
                .load()
                .unwrap()
                .find(other.registry_key().unwrap())
                .is_some()
        );
        std::fs::remove_dir_all(&other_home).unwrap();
        std::fs::rename(&retained, &other_home).unwrap();
        drop(reservation);

        // An already admitted startup completes under its reader. A waiting
        // lifecycle writer must census the resulting actual PTY, not an earlier
        // empty inventory. This invokes the same spawn endpoint as RPC dispatch.
        let race = registered(&homes, &store, Provider::Claude, "race");
        let reader = gate.read().unwrap();
        let race_host = host.clone();
        let race_id = race.id();
        let (entered, waiting) = std::sync::mpsc::channel();
        let pending = std::thread::spawn(move || {
            entered.send(()).unwrap();
            lifecycle(
                &race_host,
                crate::account::remote::Operation::Remove { id: race_id },
            )
        });
        waiting.recv().unwrap();
        let racing_terminal = terminal(
            &root,
            BTreeMap::from([(
                "CODEX_HOME".into(),
                race.resolved_home().unwrap().display().to_string(),
            )]),
        );
        let racing_id = racing_terminal.id;
        super::super::state::spawn_terminal(&host, 1, racing_terminal, Default::default()).unwrap();
        drop(reader);
        assert!(pending.join().unwrap().is_err());
        assert!(
            !race
                .resolved_home()
                .unwrap()
                .join("logout-started")
                .exists()
        );
        request(
            &host,
            ClientRequest::CloseTerminalChecked {
                terminal_id: racing_id,
                allow_active: true,
            },
        )
        .unwrap();
        assert!(
            lifecycle(
                &host,
                crate::account::remote::Operation::Remove { id: race.id() }
            )
            .is_err(),
            "a completed but still attached pane retains its home"
        );
        request(
            &host,
            ClientRequest::Detach {
                terminal_id: racing_id,
            },
        )
        .unwrap();

        // Linked descendants, including another provider, retain the entire
        // containing managed tree when its owner explicitly deletes files.
        let nested = race.resolved_home().unwrap().join("nested-codex");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("history"), "keep").unwrap();
        store
            .update(|registry| {
                registry
                    .register(
                        Registered {
                            key: uuid::Uuid::new_v4(),
                            provider: Provider::Codex,
                            linked_home: Some(nested.clone()),
                            label: None,
                            email: Some("nested@fixture.invalid".into()),
                            organization: None,
                            added_at: 0,
                        },
                        &[],
                    )
                    .unwrap()
            })
            .unwrap();
        lifecycle(
            &host,
            crate::account::remote::Operation::DeleteFiles {
                id: race.id(),
                expected_home: None,
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(nested.join("history")).unwrap(),
            "keep"
        );
        assert!(race.resolved_home().unwrap().is_dir());
    }
}

/// Reattaching a completed pane must not add a reader while its retained
/// account history is being deleted. Copy only nonsecret home metadata.
pub(super) fn ensure_terminal(
    host: &SharedHost,
    id: crate::model::TerminalId,
) -> Result<(), ProtocolError> {
    let selected = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.terminals.get(&id).map(|record| {
            let environment = record
                .execution_environment
                .as_ref()
                .map(|value| &value.0)
                .unwrap_or(&record.session.launch.environment);
            let environment = ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"]
                .into_iter()
                .filter_map(|name| {
                    environment
                        .get(name)
                        .map(|value| (name.into(), value.clone()))
                })
                .collect();
            (
                record.session.launch.working_directory.clone(),
                environment,
                crate::account::launch_credential_overrides(&record.session.launch),
                record.execution_environment.is_none(),
            )
        })
    };
    if let Some((cwd, environment, unset, inherit)) = selected {
        ensure_launch(host, &cwd, &environment, &unset, inherit)?;
    }
    Ok(())
}

pub(super) fn ensure_pane(
    host: &SharedHost,
    pane: &crate::workspace_model::views::Pane,
) -> Result<(), ProtocolError> {
    let recipe = match &pane.kind {
        crate::model::CardKind::Chat(session) => &session.launch.recipe,
        crate::model::CardKind::Terminal(session) => &session.launch,
        _ => return Ok(()),
    };
    ensure_launch(
        host,
        &recipe.working_directory,
        &recipe.environment,
        &recipe.unset_environment,
        false,
    )
}

/// Retained native adoption must use captured provenance before any adapter or
/// provider attachment. A legacy unknown owner cannot enter an active mutation.
pub(super) fn ensure_captured_scope(
    host: &SharedHost,
    scope: Option<&crate::account::scope::LaunchAccountScope>,
) -> Result<(), ProtocolError> {
    if let Some(scope) = scope {
        scope.revalidate().map_err(refusal)?;
        ensure_paths(host, [scope.canonical_home.clone()])
    } else {
        let map = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .account_lifecycle
            .clone();
        if map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|homes| !homes.is_empty())
        {
            Err(refusal(
                "This retained provider has no captured account ownership. Let the account operation finish before reattaching it.",
            ))
        } else {
            Ok(())
        }
    }
}
