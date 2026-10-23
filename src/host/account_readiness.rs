//! User-requested default-profile observations, never a task dispatch permit.
//! Capture and revalidate instead of holding the mutation fence during RPC:
//! an account diagnostic must not stall unrelated live conversations.
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
    let home = Directory::capture(
        account
            .resolved_home()
            .ok_or_else(|| refused("The selected account has no home"))?,
    )?;
    let working = Directory::capture(workspace(&request.workspace)?)?;
    let helper = crate::companion::Product::Cli
        .beside_current()
        .map_err(|_| refused("The matched cagents probe executable is unavailable"))?;
    if !helper.is_file() {
        return Err(refused(
            "The matched cagents probe executable is unavailable",
        ));
    }
    let (gate, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (state.control_gate.clone(), state.proof().clone())
    };
    {
        let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
        super::client::ensure_control_open(host)?;
        super::account_lifecycle::ensure_paths(host, Some(home.canonical.clone()))?;
    }
    if !home.unchanged() || !working.unchanged() {
        return Err(refused(
            "The selected directory changed before the account check",
        ));
    }
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
    ));
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
