//! User-requested default-profile observations, never a task dispatch permit.
//! Capture and revalidate instead of holding the mutation fence during RPC:
//! an account diagnostic must not stall unrelated live conversations.
pub(super) mod use_lease;

use super::state::SharedHost;
use crate::{
    account::{
        Account, AccountHome,
        home::Homes,
        readiness::{CheckReason, CheckReport, CheckRequest, Measurement, PhysicalIdentity},
        registry::{Registry, Store},
    },
    agent::Provider,
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

fn refused(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

struct Directory {
    original: PathBuf,
    canonical: PathBuf,
    pin: File,
    identity: PhysicalIdentity,
}
impl Directory {
    fn capture(path: PathBuf) -> Result<Self, ProtocolError> {
        let canonical = path
            .canonicalize()
            .map_err(|_| refused("The selected account home or workspace is unavailable"))?;
        let pin = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&canonical)
            .map_err(|_| refused("The selected account home or workspace cannot be pinned"))?;
        let metadata = pin
            .metadata()
            .map_err(|_| refused("The selected directory cannot be identified"))?;
        let directory = Self {
            original: path,
            canonical,
            pin,
            identity: PhysicalIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        };
        if !directory.unchanged() {
            return Err(refused("The selected directory changed during capture"));
        }
        Ok(directory)
    }

    fn unchanged(&self) -> bool {
        self.original.canonicalize().ok().as_ref() == Some(&self.canonical)
            && self.pin.metadata().is_ok_and(|pinned| {
                std::fs::metadata(&self.canonical).is_ok_and(|current| {
                    current.is_dir()
                        && pinned.dev() == self.identity.device
                        && pinned.ino() == self.identity.inode
                        && current.dev() == pinned.dev()
                        && current.ino() == pinned.ino()
                })
            })
    }
}

/// Occupancy lasts across RPC and non-cancellable blocking spawn attempts, but
/// only the actual spawn owns shared control admission.
struct Owner {
    host: SharedHost,
    home: Arc<Directory>,
    working: Arc<Directory>,
    _lease: Arc<use_lease::Lease>,
}

fn verify_start(
    host: &SharedHost,
    home: &Directory,
    working: &Directory,
) -> Result<(), ProtocolError> {
    super::client::ensure_control_open(host)?;
    if !home.unchanged() || !working.unchanged() {
        return Err(refused(
            "The selected directory changed before the account check",
        ));
    }
    super::account_lifecycle::ensure_paths(
        host,
        [home.canonical.clone(), working.canonical.clone()],
    )?;
    for directory in [home, working] {
        super::worktree_removal::ensure_outside_directory(host, &directory.pin)?;
        super::worktree_removal::ensure_outside(host, &directory.canonical)?;
    }
    Ok(())
}

fn enroll(
    host: &SharedHost,
    home: Arc<Directory>,
    working: Arc<Directory>,
) -> Result<Arc<Owner>, ProtocolError> {
    let scope =
        use_lease::Scope::capture(&home.canonical, &home.pin, &working.canonical, &working.pin)
            .map_err(scope_error)?;
    let (gate, map) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.control_gate.clone(),
            state.account_readiness_uses.clone(),
        )
    };
    // A new explicit check can retire previously completed quarantined checks
    // before the bounded enrollment limit is evaluated.
    use_lease::cleanup_retained(&map);
    let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
    verify_start(host, &home, &working)?;
    let lease = Arc::new(use_lease::Lease::enroll(&map, scope).map_err(scope_error)?);
    Ok(Arc::new(Owner {
        host: host.clone(),
        home,
        working,
        _lease: lease,
    }))
}

impl crate::account::probe::SpawnOwner for Owner {
    fn spawn(
        &self,
        command: &mut tokio::process::Command,
    ) -> Result<std::io::Result<tokio::process::Child>, String> {
        let gate = self
            .host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
        verify_start(&self.host, &self.home, &self.working).map_err(|error| error.message)?;
        crate::account::probe::spawn_authorized(command)
    }

    fn preserve_unconfirmed(&self, child: tokio::process::Child, group_signal_confirmed: bool) {
        self._lease
            .retain_unconfirmed(child, group_signal_confirmed);
    }
}

fn scope_error(message: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

fn uses(host: &SharedHost) -> use_lease::Leases {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .account_readiness_uses
        .clone()
}

/// Caller serializes the final destructive census with reader enrollment.
pub(super) fn ensure_unused(host: &SharedHost, path: &Path) -> Result<(), ProtocolError> {
    let map = uses(host);
    use_lease::cleanup_retained(&map);
    use_lease::ensure_outside(&map, path).map_err(scope_error)
}

pub(super) fn ensure_unused_directory(
    host: &SharedHost,
    path: &Path,
    pin: &File,
) -> Result<(), ProtocolError> {
    let map = uses(host);
    use_lease::cleanup_retained(&map);
    use_lease::ensure_outside_directory(&map, path, pin).map_err(scope_error)
}

pub(super) fn ensure_idle(host: &SharedHost) -> Result<(), ProtocolError> {
    let map = uses(host);
    use_lease::cleanup_retained(&map);
    if use_lease::active_count(&map) == 0 {
        Ok(())
    } else {
        Err(refused(
            "An account check is still finishing. Wait for it before shutting down or upgrading this daemon.",
        ))
    }
}

/// Deliberately limited to lifecycle exits: ordinary launches and attachments
/// may share the account home and workspace with a diagnostic reader.
pub(super) fn admit(
    host: &SharedHost,
    request: &crate::terminal_ipc::ClientRequest,
) -> Result<(), ProtocolError> {
    use crate::terminal_ipc::ClientRequest;
    if matches!(
        request,
        ClientRequest::PrepareUpgrade { .. }
            | ClientRequest::PrepareConfirmedUpgrade { .. }
            | ClientRequest::Shutdown
            | ClientRequest::ShutdownV1 { .. }
    ) {
        ensure_idle(host)?;
    }
    Ok(())
}

fn workspace(path: &Path) -> Result<PathBuf, ProtocolError> {
    let path = match path.to_str().and_then(|p| p.strip_prefix("~/")) {
        Some(relative) if !Path::new(relative).is_absolute() => std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| refused("The selected device has no home directory"))?
            .join(relative),
        Some(_) => return Err(refused("Use ~/path without a doubled slash")),
        None => path.to_path_buf(),
    };
    if !path.is_absolute() {
        return Err(refused(
            "Choose an absolute workspace on the selected device",
        ));
    }
    Ok(path)
}

fn selected(
    registry: &Registry,
    homes: Option<&Homes>,
    request: &CheckRequest,
) -> Result<Account, ProtocolError> {
    if registry.is_newer() {
        return Err(refused("The account registry requires a newer cagents"));
    }
    let account = registry
        .accounts(homes)
        .into_iter()
        .find(|account| account.id() == request.account && account.provider == Provider::Codex)
        .ok_or_else(|| {
            refused("The selected Codex account is unavailable; no other account was used")
        })?;
    if let Some(record) = account.registry_key().and_then(|key| registry.find(key))
        && record.email.as_deref().is_some_and(|email| {
            !email
                .trim()
                .eq_ignore_ascii_case(request.expected_email.trim())
        })
    {
        return Err(refused(
            "The expected identity conflicts with the selected registration",
        ));
    }
    Ok(account)
}

fn at_captured_home(account: &Account, home: &Path) -> Account {
    let mut captured = account.clone();
    captured.home = match account.home {
        AccountHome::Managed { key, .. } => AccountHome::Managed {
            key,
            path: home.into(),
        },
        AccountHome::Linked { key, .. } => AccountHome::Linked {
            key,
            path: home.into(),
        },
        _ => AccountHome::SystemAt { path: home.into() },
    };
    captured
}

pub(super) fn check(
    host: &SharedHost,
    request: CheckRequest,
) -> Result<CheckReport, ProtocolError> {
    request.validate().map_err(refused)?;
    let store =
        Store::installation().ok_or_else(|| refused("The account registry is unavailable"))?;
    let registry = store
        .try_load()
        .map_err(|_| refused("The account registry cannot be read"))?;
    let homes = Homes::installation();
    let account = selected(&registry, homes.as_ref(), &request)?;
    let home = Arc::new(Directory::capture(
        account
            .resolved_home()
            .ok_or_else(|| refused("The selected account has no home"))?,
    )?);
    let working = Arc::new(Directory::capture(workspace(&request.workspace)?)?);
    let helper = crate::companion::Product::Cli
        .beside_current()
        .map_err(|_| refused("The matched cagents probe executable is unavailable"))?;
    if !helper.is_file() {
        return Err(refused(
            "The matched cagents probe executable is unavailable",
        ));
    }
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    // Declare ownership before the runtime: reverse Drop order drains blocking
    // attempts before the parent's final reader lease can disappear.
    let owner = enroll(host, home.clone(), working.clone())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| refused("The account check could not start"))?;
    let mut report = runtime.block_on(crate::account::probe::readiness::run(
        at_captured_home(&account, &home.canonical),
        working.canonical.clone(),
        request.clone(),
        helper,
        home.identity,
        working.identity,
        Some(owner.clone()),
    ));
    // Runtime shutdown waits for any spawn_blocking attempt abandoned by an
    // async timeout; that attempt also owns its own Arc independently.
    drop(runtime);
    drop(owner);
    report.host = Some(proof);
    report.registry_revision = Some(registry.revision());
    report.account_home_identity = Some(home.identity);
    report.workspace_identity = Some(working.identity);
    // Unrelated registry edits are allowed. Only the chosen registration and
    // physical operands invalidate this point-in-time observation.
    let registration_unchanged = store.try_load().is_ok_and(|current| {
        selected(&current, homes.as_ref(), &request).ok().as_ref() == Some(&account)
            && account
                .registry_key()
                .is_none_or(|key| current.find(key) == registry.find(key))
    });
    if !registration_unchanged || !home.unchanged() || !working.unchanged() {
        report.authentication = Measurement::unknown(CheckReason::AccountMismatch);
        report.execution_profile_configured = Measurement::unknown(CheckReason::AccountMismatch);
        report.invalidate_measurements(CheckReason::AccountMismatch);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease_fixture() -> (
        tempfile::TempDir,
        SharedHost,
        Arc<Directory>,
        Arc<Directory>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let working = root.path().join("workspace");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&working).unwrap();
        let host = super::super::account_lifecycle::Reservation::test_host();
        (
            root,
            host,
            Arc::new(Directory::capture(home).unwrap()),
            Arc::new(Directory::capture(working).unwrap()),
        )
    }

    #[test]
    fn diagnostic_and_worktree_removal_refuse_both_admission_orders_and_release() {
        let (root, host, home, working) = lease_fixture();
        let reserved =
            super::super::worktree_removal::Reservation::install(&host, &working.canonical)
                .unwrap();
        assert!(enroll(&host, home.clone(), working.clone()).is_err());
        drop(reserved);
        let owner = enroll(&host, home.clone(), working.clone()).unwrap();
        assert!(
            super::super::worktree_removal::Reservation::install(&host, &working.canonical,)
                .is_err()
        );
        assert!(
            super::super::worktree_removal::Reservation::install(&host, &home.canonical,).is_err()
        );
        let sibling = root.path().join("sibling");
        std::fs::create_dir(&sibling).unwrap();
        let unrelated =
            super::super::worktree_removal::Reservation::install(&host, &sibling).unwrap();
        drop(unrelated);
        drop(owner);
        super::super::worktree_removal::Reservation::install(&host, &working.canonical).unwrap();
        ensure_idle(&host).unwrap();
    }

    #[test]
    fn account_mutations_conflict_but_ordinary_launches_and_other_homes_remain_available() {
        use std::collections::{BTreeMap, BTreeSet};
        let (root, host, home, working) = lease_fixture();
        let homes = Homes::at(root.path().join("managed"));
        let store = Store::at(root.path().join("registry"));
        // Both the provider home and a workspace inside another account home
        // must obey an already admitted mutation.
        for path in [&home.canonical, &working.canonical] {
            let account = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), path.clone());
            let mutation = super::super::account_lifecycle::Reservation::test_install(
                &host,
                &[account.clone()],
                store.clone(),
                &homes,
            )
            .unwrap();
            assert!(enroll(&host, home.clone(), working.clone()).is_err());
            drop(mutation);
            let owner = enroll(&host, home.clone(), working.clone()).unwrap();
            assert!(
                super::super::account_lifecycle::Reservation::test_install(
                    &host,
                    &[account],
                    store.clone(),
                    &homes,
                )
                .is_err()
            );
            super::super::account_lifecycle::ensure_launch(
                &host,
                &working.canonical,
                &BTreeMap::from([("CODEX_HOME".into(), home.canonical.to_str().unwrap().into())]),
                &BTreeSet::new(),
                false,
            )
            .unwrap();
            let gate = host.lock().unwrap().control_gate.clone();
            assert!(
                gate.try_write().is_ok(),
                "a reader lease retains no global admission"
            );
            let unrelated = Account::linked(
                Provider::Codex,
                uuid::Uuid::new_v4(),
                root.path().join("not-created-sibling"),
            );
            let independent = super::super::account_lifecycle::Reservation::test_install(
                &host,
                &[unrelated],
                store.clone(),
                &homes,
            )
            .unwrap();
            drop(independent);
            drop(owner);
        }
    }

    #[test]
    fn renamed_reader_is_found_by_alias_and_original_destructive_pin() {
        let (root, host, home, working) = lease_fixture();
        let original = working.pin.try_clone().unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&working.canonical, &moved).unwrap();
        std::fs::create_dir(&working.canonical).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&moved, &alias).unwrap();
        let selected = Arc::new(Directory::capture(alias).unwrap());
        let owner = enroll(&host, home, selected).unwrap();
        assert!(ensure_unused(&host, &working.canonical).is_ok());
        assert!(ensure_unused_directory(&host, &working.canonical, &original).is_err());
        assert!(super::super::worktree_removal::Reservation::install(&host, &moved).is_err());
        drop(owner);
        ensure_unused_directory(&host, &working.canonical, &original).unwrap();
    }

    #[test]
    fn only_shutdown_and_upgrade_admission_wait_for_diagnostic_readers() {
        use crate::terminal_ipc::ClientRequest;
        let (_root, host, home, working) = lease_fixture();
        let owner = enroll(&host, home, working).unwrap();
        assert!(admit(&host, &ClientRequest::Shutdown).is_err());
        admit(&host, &ClientRequest::List).unwrap();
        drop(owner);
        admit(&host, &ClientRequest::Shutdown).unwrap();
    }

    #[test]
    fn delayed_blocking_start_keeps_ownership_and_rechecks_committed_shutdown() {
        use crate::account::probe::SpawnOwner;
        let (_root, host, home, working) = lease_fixture();
        let owner = enroll(&host, home, working).unwrap();
        let gate = host.lock().unwrap().control_gate.clone();
        let writer = gate.write().unwrap();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            // A committed shutdown must be classified before even trying an
            // invalid executable, without requiring any provider or runtime.
            owner.spawn(&mut tokio::process::Command::new("/no-provider-must-start"))
        });
        ready.recv().unwrap();
        assert!(
            ensure_idle(&host).is_err(),
            "the worker owns the last reader"
        );
        host.lock().unwrap().request_shutdown();
        drop(writer);
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("shutdown is committed"), "{error}");
        ensure_idle(&host).unwrap();
    }

    #[test]
    fn retained_original_child_fences_removal_until_reaped_without_blocking_launches() {
        use crate::account::probe::SpawnOwner;
        use std::{
            collections::BTreeMap,
            process::Stdio,
            time::{Duration, Instant},
        };
        let (_root, host, home, working) = lease_fixture();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let owner = enroll(&host, home.clone(), working.clone()).unwrap();
        // EOF on this owned pipe controls a real harmless child. Its original
        // Child is moved to quarantine; no PID census or signal is substituted
        // for the eventual try_wait proof.
        let mut command = tokio::process::Command::new("/bin/cat");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .current_dir(&working.canonical);
        let mut child = owner.spawn(&mut command).unwrap().unwrap();
        let input = child.stdin.take().unwrap();
        // This HOST fixture supplies the signal-confirmed custody case; real
        // Group signaling and failure classification belong to probe fixtures.
        owner.preserve_unconfirmed(child, true);
        drop(owner);
        assert!(ensure_idle(&host).is_err());
        assert!(ensure_unused(&host, &working.canonical).is_err());
        assert!(
            super::super::worktree_removal::Reservation::install(&host, &working.canonical)
                .is_err()
        );
        super::super::account_lifecycle::ensure_launch(
            &host,
            &working.canonical,
            &BTreeMap::from([("CODEX_HOME".into(), home.canonical.to_str().unwrap().into())]),
            &Default::default(),
            false,
        )
        .unwrap();
        admit(&host, &crate::terminal_ipc::ClientRequest::List).unwrap();
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ensure_idle(&host).is_err() {
            assert!(
                Instant::now() < deadline,
                "the original child did not finish and reap"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        ensure_unused(&host, &working.canonical).unwrap();
        super::super::worktree_removal::Reservation::install(&host, &working.canonical).unwrap();
    }

    #[test]
    fn directory_capture_detects_alias_and_physical_replacement() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        let alias = root.path().join("selected");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        let captured = Directory::capture(alias.clone()).unwrap();
        assert!(captured.unchanged());
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&b, &alias).unwrap();
        assert!(!captured.unchanged());
        let captured = Directory::capture(a.clone()).unwrap();
        std::fs::rename(&a, root.path().join("retired")).unwrap();
        std::fs::create_dir(&a).unwrap();
        assert!(!captured.unchanged());
    }

    #[test]
    fn ambiguous_destination_home_expansion_cannot_drop_the_home_prefix() {
        assert!(workspace(Path::new("~//another-home")).is_err());
        assert!(workspace(Path::new("relative")).is_err());
        assert_eq!(
            workspace(Path::new("/task/workspace")).unwrap(),
            Path::new("/task/workspace")
        );
    }

    #[test]
    fn selection_never_falls_back_and_rejects_a_conflicting_registered_login() {
        use crate::account::{Identity, registry::Registered};
        let root = tempfile::tempdir().unwrap();
        let key = uuid::Uuid::new_v4();
        let account = Account::linked(Provider::Codex, key, root.path().into());
        let mut request = CheckRequest {
            account: account.id(),
            workspace: root.path().into(),
            expected_email: "selected@example.test".into(),
            write: true,
            github_repository: None,
        };
        let mut registry = Registry::default();
        assert!(selected(&registry, None, &request).is_err());
        registry
            .register(
                Registered {
                    key,
                    provider: Provider::Codex,
                    linked_home: Some(root.path().into()),
                    label: None,
                    email: Some(request.expected_email.clone()),
                    organization: None,
                    added_at: 0,
                },
                &[],
            )
            .unwrap();
        assert_eq!(selected(&registry, None, &request).unwrap(), account);
        let before = registry.clone();
        request.expected_email = "another@example.test".into();
        assert!(selected(&registry, None, &request).is_err());
        assert_eq!(
            registry, before,
            "checks never adopt a login or rewrite defaults"
        );
        request.expected_email = "selected@example.test".into();
        registry.adopt_identity(
            key,
            &Identity {
                email: Some("changed@example.test".into()),
                organization: None,
                plan: None,
            },
        );
        assert!(selected(&registry, None, &request).is_err());
        registry.unregister(key);
        assert!(
            selected(&registry, None, &request).is_err(),
            "system cannot substitute for a removed selected account"
        );
    }

    #[test]
    fn captured_home_preserves_every_selected_id_and_account_isolation_policy() {
        let home = Path::new("/captured/home");
        let key = uuid::Uuid::new_v4();
        for account in [
            Account::system(Provider::Codex),
            Account::managed(Provider::Codex, key, "/original/managed".into()),
            Account::linked(Provider::Codex, key, "/original/linked".into()),
        ] {
            let captured = at_captured_home(&account, home);
            assert_eq!(captured.id(), account.id());
            assert_eq!(captured.resolved_home().as_deref(), Some(home));
            assert_eq!(
                captured.credential_overrides(),
                account.credential_overrides()
            );
        }
    }
}
