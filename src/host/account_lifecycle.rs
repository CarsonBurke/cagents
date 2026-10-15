//! Scoped admission for account-home mutations. Install reservations under the
//! short control writer; never retain a map or HostState lock during I/O.
//!
//! This foundation issues no deletion permit. Managed removal refuses and
//! pending/failed homes remain intact. Historical namespace enrollment and
//! complete actor/reader retirement census belong to that separate cut.
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
    os::{fd::{AsRawFd, FromRawFd}, unix::fs::{MetadataExt, OpenOptionsExt}},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type Map = BTreeMap<uuid::Uuid, Reserved>;
pub(super) struct Reserved {
    homes: Vec<Home>,
    /// Opaque physical annotation namespaces from a selected missing account's
    /// historical purge index. These grant deletion only, never scope recovery.
    namespaces: BTreeSet<String>,
}
pub(super) type Reservations = Arc<Mutex<Map>>;

#[derive(Clone)]
pub(super) struct Home {
    path: PathBuf,
    canonical: PathBuf,
    pin: Option<Arc<File>>,
    identity: Option<(u64, u64)>,
    ancestors: BTreeSet<(u64, u64)>,
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
        let identity = pin.as_ref().map(|pin| pin.metadata()).transpose().map_err(|e| e.to_string())?
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let ancestors = physical_ancestors(&canonical, pin.as_deref())?;
        let home = Self {
            path,
            canonical,
            pin,
            identity,
            ancestors,
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

    fn refresh_ancestors(&mut self) -> Result<(), String> {
        if let Some(pin) = &self.pin {
            // Once cleanup unlinks this exact root, it has no current
            // descendants; retain the original lexical guard until Drop.
            if pin.metadata().map_err(|error| error.to_string())?.nlink() == 0 { return Ok(()); }
            self.ancestors.extend(physical_ancestors(&self.canonical, Some(pin.as_ref()))?);
        }
        Ok(())
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.canonical.starts_with(&other.canonical)
            || other.canonical.starts_with(&self.canonical)
            || self.identity.is_some_and(|identity| other.ancestors.contains(&identity))
            || other.identity.is_some_and(|identity| self.ancestors.contains(&identity))
    }

    fn scope(&self, provider: crate::agent::Provider) -> Option<crate::account::scope::LaunchAccountScope> {
        self.identity.map(|(device,inode)| crate::account::scope::LaunchAccountScope {
            provider, canonical_home: self.canonical.clone(), device, inode,
            registered_id: None })
    }
}

/// Walk the actual directory parent chain through descriptors. Lexical
/// aliases can change when a reserved home/provider parent is renamed.
fn physical_ancestors(path: &Path, pin: Option<&File>) -> Result<BTreeSet<(u64, u64)>, String> {
    let mut cursor = if let Some(pin) = pin {
        pin.try_clone().map_err(|error| error.to_string())?
    } else {
        let mut existing = path;
        loop {
            match std::fs::OpenOptions::new().read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(existing) {
                Ok(file) => break file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or("Account scope has no existing directory ancestor")?;
                }
                Err(error) => return Err(format!("Cannot capture account scope ancestry: {error}")),
            }
        }
    };
    let mut identities = BTreeSet::new();
    for _ in 0..256 {
        let metadata = cursor.metadata().map_err(|error| error.to_string())?;
        let identity = (metadata.dev(), metadata.ino());
        if !identities.insert(identity) { return Ok(identities); }
        // SAFETY: the parent component is fixed and the owned descriptor
        // outlives openat; the returned descriptor is newly owned.
        let parent = unsafe { libc::openat(cursor.as_raw_fd(), c"..".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if parent < 0 { return Err(format!("Cannot capture account scope parent: {}", std::io::Error::last_os_error())); }
        cursor = unsafe { File::from_raw_fd(parent) };
    }
    Err("Account scope ancestry exceeds the directory bound".into())
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

pub(crate) struct Reservation {
    map: Reservations,
    gate: Arc<std::sync::RwLock<()>>,
    token: uuid::Uuid,
    homes: Vec<Home>,
    accounts: Vec<Account>,
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
            .try_load()
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
        let physical_namespaces = accounts.iter().zip(&homes)
            .filter_map(|(account, home)| home.scope(account.provider))
            .map(|scope| crate::account::scope::namespace_key(&scope))
            .collect::<BTreeSet<_>>();
        let token = uuid::Uuid::new_v4();
        let (previous_tokens, mut previous_homes) = {
            let entries = map.lock().unwrap_or_else(|p| p.into_inner());
            (entries.keys().copied().collect::<BTreeSet<_>>(), entries.values().flat_map(|entry| entry.homes.iter().cloned()).collect::<Vec<_>>())
        };
        for home in &mut previous_homes { home.refresh_ancestors().map_err(refusal)?; }
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        if previous_tokens != entries.keys().copied().collect::<BTreeSet<_>>() {
            return Err(refusal("Account operations changed during reservation; retry"));
        }
        if entries.len() >= 64 {
            return Err(refusal(
                "Too many account operations are already pending; wait and retry",
            ));
        }
        if previous_homes.iter().any(|other| homes.iter().any(|home| home.overlaps(other)))
        {
            return Err(refusal(
                "An operation is already changing this account home. Wait for it to finish, then retry.",
            ));
        }
        if entries.values().any(|entry| !entry.namespaces.is_disjoint(&physical_namespaces)) {
            return Err(refusal("This physical account's historical annotations are being deleted; wait and retry"));
        }
        entries.insert(token, Reserved { homes: homes.clone(), namespaces: BTreeSet::new() });
        drop(entries);
        let gate = host.lock().unwrap_or_else(|p| p.into_inner()).control_gate.clone();
        Ok(Self {
            map,
            gate,
            token,
            homes,
            accounts: accounts.to_vec(),
            registered,
        })
    }

    /// No provider work or long filesystem effects may occur inside this cut.
    /// A busy writer refuses before the supplied registry effect is invoked.
    pub(crate) fn commit<T>(&self, effect: impl FnOnce() -> std::io::Result<T>) -> Result<T, String> {
        let _writer = super::client::try_scoped_control_write(&self.gate).map_err(|error| error.message)?;
        self.verify()?;
        effect().map_err(|error| error.to_string())
    }

    /// Call outside HostState and before each destructive stage/final commit.
    pub(crate) fn verify(&self) -> Result<(), String> {
        if !self.map.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&self.token) {
            return Err("The account mutation reservation was released; reload accounts and retry".into());
        }
        for home in &self.homes {
            home.verify()?;
        }
        let registry = Store::installation()
            .ok_or("Account registry unavailable")?
            .try_load()
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
    let mut reserved: Vec<_> = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .flat_map(|entry| entry.homes.iter())
        .cloned()
        .collect();
    if reserved.is_empty() {
        return Ok(());
    }
    for home in &mut reserved { home.refresh_ancestors().map_err(refusal)?; }
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
    ensure_paths(host, paths)?;
    if has_namespace_reservations(host) {
        for provider in crate::account::Account::PROVIDERS {
            let scope = crate::account::scope::LaunchAccountScope::resolve(provider, cwd, effective)
                .map_err(|_| refusal("Cannot establish the launched account's physical namespace while historical deletion is pending"))?;
            ensure_namespace(host, &scope)?;
        }
    }
    Ok(())
}

/// An operational directory FD can outlive or move away from the recipe's
/// issued cwd. Relative credential roots would then be checked against a
/// different directory from the one the child uses. Refuse that ambiguity
/// before spawn; absolute roots still enter the ordinary reservation checks.
pub(super) fn ensure_launch_at_directory(
    host: &SharedHost,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    unset: &BTreeSet<String>,
    inherit: bool,
    execution_directory: Option<&File>,
) -> Result<(), ProtocolError> {
    if let Some(directory) = execution_directory {
        let metadata = directory.metadata().map_err(|error| {
            refusal(format!("Cannot inspect the pinned terminal directory: {error}"))
        })?;
        if !metadata.is_dir() || metadata.nlink() == 0 {
            return Err(refusal("The pinned terminal directory is no longer available"));
        }
        require_absolute_roots(environment, unset, inherit)?;
    }
    // Keep the normal admission path, including captured account namespace
    // checks, authoritative for every absolute or unspecified root.
    ensure_launch(host, cwd, environment, unset, inherit)
}

fn require_absolute_roots(environment: &BTreeMap<String, String>, unset: &BTreeSet<String>, inherit: bool) -> Result<(), ProtocolError> {
        let effective = |name: &str| {
            if unset.contains(name) {
                None
            } else {
                environment.get(name).map(std::ffi::OsString::from)
                    .or_else(|| inherit.then(|| std::env::var_os(name)).flatten())
            }
        };
        // Both providers remain usable from an ordinary terminal, regardless
        // of which provider initiated the handoff or owns a reserved account.
        for variable in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
            let selected = effective(variable).filter(|value| !value.is_empty());
            let (name, root) = match selected {
                Some(root) => (variable, Some(root)),
                None => ("HOME", effective("HOME").filter(|value| !value.is_empty())),
            };
            if root.as_ref().is_some_and(|root| Path::new(root).is_relative()) {
                return Err(refusal(format!(
                    "Cannot launch with relative {name} while using a pinned working directory. Set an absolute credential directory and retry."
                )));
            }
        }
    Ok(())
}

fn reservations_active(host: &SharedHost) -> bool {
    let map = host.lock().unwrap_or_else(|p| p.into_inner()).account_lifecycle.clone();
    !map.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
}

/// Legacy adopted terminals have no newly observed process environment or cwd.
/// Absolute captured roots are usable; unknown/relative provenance waits until
/// account mutation completes rather than guessing from today's daemon HOME.
pub(super) fn ensure_terminal_adoption(host: &SharedHost, environment: Option<&BTreeMap<String, String>>, unset: &BTreeSet<String>) -> Result<(), ProtocolError> {
    if !reservations_active(host) { return Ok(()); }
    let Some(environment) = environment else { return ensure_captured_scope(host, None); };
    require_absolute_roots(environment, unset, false)
}

/// Check the exact environment and directory which a terminal handoff will
/// use before stopping its predecessor. Spawn repeats the check to fence a
/// reservation which starts after this preflight.
pub(super) fn ensure_terminal_handoff(
    host: &SharedHost,
    recipe: &crate::model::LaunchRecipe,
    environment: &crate::terminal_ipc::ChatExecutionEnvironment,
    execution: &crate::chat::protocol::ExecutionContext,
) -> Result<(), ProtocolError> {
    let directory = execution.directory_descriptor().ok_or_else(|| {
        refusal("The terminal handoff has no pinned working directory; recover its location before retrying")
    })?;
    ensure_launch_at_directory(
        host,
        &recipe.working_directory,
        &environment.0,
        &crate::account::launch_credential_overrides(recipe),
        false,
        Some(&directory),
    )
}

pub(super) fn ensure_update(host: &SharedHost, proposed: &Registry) -> Result<(), ProtocolError> {
    let current = Store::installation()
        .ok_or_else(|| refusal("Account registry unavailable"))?
        .try_load()
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
        ensure_terminal_adoption(host, (!inherit).then_some(&environment), &unset)?;
        ensure_launch(host, &cwd, &environment, &unset, inherit)?;
    }
    Ok(())
}

pub(super) fn ensure_pane(host: &SharedHost, pane: &crate::workspace_model::views::Pane) -> Result<(), ProtocolError> {
    use crate::model::CardKind;
    match &pane.kind {
        CardKind::Chat(session) => {
            let captured = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.chats.get(&session.id).filter(|record| {
                    let mut projected = record.session.clone();
                    projected.launch.session_id = record.handle.confirmed_session_id();
                    &record.session == session || &projected == session
                }).map(|record| record.handle.launch_account_scope().cloned().map_err(str::to_owned))
            };
            if let Some(scope) = captured { return ensure_captured_scope(host, scope.as_ref().ok()); }
            ensure_launch(host, &session.launch.recipe.working_directory, &session.launch.recipe.environment, &session.launch.recipe.unset_environment, false)
        }
        CardKind::Terminal(session) => {
            let exact = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.descriptor(0, session.id).ok().is_some_and(|current| {
                    let Some(proof) = current.session.continuity.live_proof().filter(|proof| proof.is_complete()) else { return false; };
                    if session.continuity.live_proof().as_ref() != Some(&proof) { return false; }
                    let mut current = current.session; current.continuity = session.continuity.clone(); current == *session
                })
            };
            if exact { return ensure_terminal(host, session.id); }
            ensure_launch(host, &session.launch.working_directory, &session.launch.environment, &session.launch.unset_environment, false)
        }
        _ => Ok(()),
    }
}

/// Retained native adoption must use captured provenance before any adapter or
/// provider attachment. A legacy unknown owner cannot enter an active mutation.
pub(super) fn ensure_captured_scope(
    host: &SharedHost,
    scope: Option<&crate::account::scope::LaunchAccountScope>,
) -> Result<(), ProtocolError> {
    if let Some(scope) = scope {
        scope.revalidate().map_err(refusal)?;
        ensure_namespace(host, scope)?;
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
            .any(|entry| !entry.homes.is_empty() || !entry.namespaces.is_empty())
        {
            Err(refusal(
                "This retained provider has no captured account ownership. Let the account operation finish before reattaching it.",
            ))
        } else {
            Ok(())
        }
    }
}

fn has_namespace_reservations(host: &SharedHost) -> bool {
    let map = host.lock().unwrap_or_else(|p| p.into_inner()).account_lifecycle.clone();
    map.lock().unwrap_or_else(|p| p.into_inner()).values().any(|entry| !entry.namespaces.is_empty())
}

fn ensure_namespace(host: &SharedHost, scope: &crate::account::scope::LaunchAccountScope) -> Result<(), ProtocolError> {
    let key = crate::account::scope::namespace_key(scope);
    let map = host.lock().unwrap_or_else(|p| p.into_inner()).account_lifecycle.clone();
    if map.lock().unwrap_or_else(|p| p.into_inner()).values().any(|entry| entry.namespaces.contains(&key)) {
        Err(refusal("This exact physical account's historical annotations are being deleted. Wait for the account operation before starting or reattaching its provider."))
    } else { Ok(()) }
}

/// Existing caller owns control admission from this check through its effect.
pub(super) fn admit(host: &SharedHost, request: &crate::terminal_ipc::ClientRequest) -> Result<(), ProtocolError> {
    use crate::terminal_ipc::ClientRequest;
    match request {
        ClientRequest::Attach { terminal_id, .. } => ensure_terminal(host, *terminal_id)?,
        ClientRequest::ChatStart { session, .. } | ClientRequest::ChatStartSelected { session, .. } => {
            let captured = host.lock().unwrap_or_else(|p| p.into_inner()).chats.get(&session.id)
                .map(|record| record.handle.launch_account_scope().cloned().map_err(str::to_owned));
            if let Some(scope) = captured { ensure_captured_scope(host, scope.as_ref().ok())?; }
        }
        ClientRequest::ChatAttach { session } => {
            let scope = host.lock().unwrap_or_else(|p| p.into_inner()).chats.get(&session.id)
                .map(|record| record.handle.launch_account_scope().cloned().map_err(str::to_owned));
            match scope {
                Some(Ok(scope)) => ensure_captured_scope(host, Some(&scope))?,
                Some(Err(_)) => ensure_captured_scope(host, None)?,
                None => ensure_launch(host, &session.launch.recipe.working_directory,
                    &session.launch.recipe.environment, &session.launch.recipe.unset_environment, false)?,
            }
        }
        ClientRequest::EditWorkspaceModel { edit: crate::workspace_model::ModelEdit::SeedView { panes, .. } } => {
            for pane in panes { ensure_pane(host, pane)?; }
        }
        ClientRequest::EditWorkspaceModel { edit: crate::workspace_model::ModelEdit::ChangeView { change, .. } } => {
            use crate::workspace_model::views::Change;
            match change { Change::Insert { pane, .. } | Change::InsertIfEmpty { pane } | Change::Update { pane, .. } => ensure_pane(host, pane)?, _ => {} }
        }
        ClientRequest::PrepareUpgrade { .. } | ClientRequest::PrepareConfirmedUpgrade { .. }
        | ClientRequest::Shutdown | ClientRequest::ShutdownV1 { .. } => ensure_idle(host)?,
        _ => {}
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, model::{HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalId, TerminalSession}, terminal_ipc::{ClientRequest, ServerResponse}};
    use std::{os::unix::fs::PermissionsExt, time::{Duration, Instant}};

    fn isolated(name: &str) -> Option<PathBuf> {
        const KEY: &str = "CAGENTS_MODERN_ACCOUNT_ADMISSION_TEST";
        if let Some(root) = std::env::var_os(KEY) { return Some(root.into()); }
        let root = tempfile::Builder::new().prefix("cagents-modern-account-").tempdir_in("/var/tmp").unwrap();
        for directory in ["home", "config", "data", "cache", "runtime", "bin"] { std::fs::create_dir(root.path().join(directory)).unwrap(); }
        let log = root.path().join("test.log");
        let output = File::create(&log).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("host::account_lifecycle::tests::{name}"), "--nocapture"])
            .env_clear().env(KEY, root.path()).env("HOME", root.path().join("home"))
            .env("XDG_CONFIG_HOME", root.path().join("config")).env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache")).env("XDG_RUNTIME_DIR", root.path().join("runtime"))
            .env("PATH", format!("{}:/usr/bin:/bin", root.path().join("bin").display()))
            .env("SHELL", "/bin/sh").env("TERM", "xterm-256color")
            .stdout(output.try_clone().unwrap()).stderr(output).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(40);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() { break status; }
            if Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); panic!("isolated account test exceeded budget; evidence {}", root.keep().display()); }
            std::thread::sleep(Duration::from_millis(10));
        };
        let log = std::fs::read_to_string(log).unwrap();
        assert!(status.success(), "isolated account test failed; evidence {}\n{log}", root.path().display());
        assert!(log.contains("1 passed; 0 failed"), "{log}");
        None
    }
    fn host() -> SharedHost {
        let host = Arc::new(crate::host::state::HostLock::new(crate::host::state::HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("modern-account-fixture"), boot_id: crate::daemon::read_boot_id().unwrap(), host_incarnation: HostIncarnation::new("fixture"),
        })));
        host.lock().unwrap().register_client(1);
        host
    }
    fn request(host: &SharedHost, request: ClientRequest) -> Result<ServerResponse, ProtocolError> {
        crate::host::client::process_request(host, 1, request).map(|value| value.0)
    }
    fn lifecycle(host: &SharedHost, operation: crate::account::remote::Operation) -> Result<ServerResponse, ProtocolError> {
        request(host, ClientRequest::DeviceOperation { operation: crate::device_service::Operation::AccountLifecycle { operation } })
    }
    fn wait_file(file: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !file.exists() { assert!(Instant::now() < deadline, "missing private fixture marker {}", file.display()); std::thread::sleep(Duration::from_millis(5)); }
    }
    fn terminal(root: &Path, environment: BTreeMap<String, String>) -> TerminalSession {
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(), arguments: vec!["-c".into(), "IFS= read -r line; printf '%s' \"$line\" > \"$1\"".into(), "sh".into(), root.join("pty-input").display().to_string()],
            working_directory: root.to_owned(), environment, unset_environment: Default::default(),
        };
        TerminalSession { id: TerminalId::new(), launch: recipe.clone(), owned_codex_listener: None, recovery_origin: None, predecessor: None, transcript_path: None, resume_hint: None, continuity: crate::model::TerminalContinuity::RelaunchAvailable { recipe, last_exit: None } }
    }

    #[test]
    fn account_admission_namespace_codec_uses_physical_metadata_and_ignores_alias_ids() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let home = root.path().join(std::ffi::OsString::from_vec(vec![b'h', 0xff]));
        std::fs::create_dir(&home).unwrap();
        let alias = root.path().join("alias"); std::os::unix::fs::symlink(&home, &alias).unwrap();
        let metadata = std::fs::metadata(&home).unwrap();
        let mut scope = crate::account::scope::LaunchAccountScope { provider: Provider::Claude,
            canonical_home: home.canonicalize().unwrap(), device: metadata.dev(), inode: metadata.ino(), registered_id: None };
        let key = crate::account::scope::namespace_key(&scope);
        scope.registered_id = Some(Account::linked(Provider::Claude, uuid::Uuid::new_v4(), alias).id());
        assert_eq!(crate::account::scope::namespace_key(&scope), key);
        scope.provider = Provider::Codex;
        assert_ne!(crate::account::scope::namespace_key(&scope), key);
        scope.provider = Provider::Claude; scope.inode += 1;
        assert_ne!(crate::account::scope::namespace_key(&scope), key);
    }

    #[test]
    fn account_admission_long_register_keeps_unrelated_input_and_list_responsive() {
        let Some(root) = isolated("account_admission_long_register_keeps_unrelated_input_and_list_responsive") else { return; };
        let cli = root.join("bin/claude");
        std::fs::write(&cli, r#"#!/bin/sh
if [ "$1 $2" = 'auth status' ]; then
 : > "$CLAUDE_CONFIG_DIR/probe-started"
 while [ -f "$CLAUDE_CONFIG_DIR/probe-hold" ]; do /bin/sleep 0.01; done
 printf '%s\n' '{"loggedIn":false}'
 exit 0
fi
exit 0
"#).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shell_path = format!("export PATH='{}:/usr/bin:/bin'\n", root.join("bin").display());
        for name in [".bashrc", ".zshrc"] { std::fs::write(root.join("home").join(name), &shell_path).unwrap(); }
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let search = runtime.block_on(crate::account::probe::search_path());
        assert_eq!(crate::account::probe::find_executable("claude", search.as_deref()), Some(cli), "Never invoke any installed provider in this fixture");
        let homes = crate::account::home::Homes::installation().unwrap();
        let store = Store::installation().unwrap();
        let (key, home) = store.create_pending(&homes, Provider::Claude, 1, || homes.create(Provider::Claude, None)).unwrap();
        let account = Account::managed(Provider::Claude, key, home.clone());
        let (other_key, other_home) = homes.create(Provider::Claude, None).unwrap();
        let other = Account::managed(Provider::Claude, other_key, other_home.clone());
        store.update(|registry| registry.register(crate::account::registry::Registered {
            key: other_key, provider: Provider::Claude, linked_home: None, label: None, email: Some("other@fixture.invalid".into()), organization: None, added_at: 0,
        }, &[]).unwrap()).unwrap();
        let hold = home.join("probe-hold"); std::fs::write(&hold, "").unwrap();
        struct Release(PathBuf); impl Drop for Release { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
        let release = Release(hold);
        let host = host();
        let source = terminal(&root, BTreeMap::from([("HOME".into(), root.join("home").display().to_string())]));
        let source_id = source.id;
        request(&host, ClientRequest::Spawn { session: source, options: Default::default() }).unwrap();
        let worker_host = host.clone();
        let worker = std::thread::spawn(move || lifecycle(&worker_host, crate::account::remote::Operation::Register { provider: Provider::Claude, key }));
        wait_file(&home.join("probe-started"));
        let started = Instant::now();
        assert!(matches!(request(&host, ClientRequest::List).unwrap(), ServerResponse::Listed { .. }));
        request(&host, ClientRequest::Input { terminal_id: source_id, bytes: b"responsive\n".to_vec() }).unwrap();
        wait_file(&root.join("pty-input"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(std::fs::read(root.join("pty-input")).unwrap(), b"responsive");
        let alias = root.join("alias"); std::os::unix::fs::symlink(&home, &alias).unwrap();
        let related = terminal(&root, BTreeMap::from([("CODEX_HOME".into(), alias.display().to_string())]));
        assert!(request(&host, ClientRequest::Spawn { session: related, options: Default::default() }).is_err());
        assert!(lifecycle(&host, crate::account::remote::Operation::Link { provider: Provider::Codex, key: uuid::Uuid::new_v4(), path: alias.display().to_string() }).is_err());
        assert!(request(&host, ClientRequest::DeviceOperation { operation: crate::device_service::Operation::AccountProbe { account: account.clone(), force: true } }).is_err());
        let mut registry = store.load().unwrap(); let revision = registry.revision(); registry.set_label(&other, Some("preserved-other".into()));
        request(&host, ClientRequest::DeviceOperation { operation: crate::device_service::Operation::AccountsUpdate { expected_revision: revision, registry: serde_json::to_value(registry).unwrap() } }).unwrap();
        assert!(ensure_idle(&host).is_err());
        drop(release);
        worker.join().unwrap().unwrap();
        assert!(ensure_idle(&host).is_ok());
        assert_eq!(store.load().unwrap().label(&other), Some("preserved-other"));
        assert!(home.is_dir(), "failed sign-in metadata cannot authorize file deletion");
        assert!(lifecycle(&host, crate::account::remote::Operation::Remove { id: other.id() }).unwrap_err().message.contains("final-retirement"));
        assert!(other_home.is_dir()); assert!(store.load().unwrap().find(other_key).is_some());
        request(&host, ClientRequest::CloseTerminalChecked { terminal_id: source_id, allow_active: true }).unwrap();
        request(&host, ClientRequest::Detach { terminal_id: source_id }).unwrap();
    }

    #[test]
    fn account_admission_original_inode_and_physical_ancestry_survive_alias_moves() {
        let Some(root) = isolated("account_admission_original_inode_and_physical_ancestry_survive_alias_moves") else { return; };
        let parent = root.join("parent"); let home = parent.join("home"); std::fs::create_dir_all(home.join("child")).unwrap();
        let host = host(); let gate = host.lock().unwrap().control_gate.clone();
        let account = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), home.clone());
        let reservation = { let _writer = crate::host::client::try_scoped_control_write(&gate).unwrap(); Reservation::install(&host, &[account]).unwrap() };
        let moved = root.join("moved"); std::fs::rename(&parent, &moved).unwrap(); std::fs::create_dir_all(&home).unwrap();
        assert!(ensure_paths(&host, [moved.join("home/child")]).is_err());
        assert!(reservation.verify().is_err(), "new inode at the old spelling grants nothing");
        assert!(ensure_captured_scope(&host, None).is_err());
        drop(reservation);
        ensure_paths(&host, [moved.join("home/child")]).unwrap();
        let parked = root.join("data/old-parked"); std::fs::create_dir(&parked).unwrap(); std::fs::write(parked.join("keep"), b"retained").unwrap();
        Store::installation().unwrap().load_and_sweep(&crate::account::home::Homes::installation().unwrap()).unwrap();
        assert_eq!(std::fs::read(parked.join("keep")).unwrap(), b"retained");
    }

    #[test]
    fn account_admission_short_writers_refuse_actual_flocks_and_shared_admission() {
        let Some(root) = isolated("account_admission_short_writers_refuse_actual_flocks_and_shared_admission") else { return; };
        let host = host(); let gate = host.lock().unwrap().control_gate.clone();
        let reader = gate.read().unwrap();
        assert!(crate::host::client::try_scoped_control_write(&gate).unwrap_err().retryable);
        drop(reader);
        let store = Store::installation().unwrap();
        // Use the exact registry location from the real Store, not guessed
        // lock spellings. A second owned descriptor must fail immediately.
        let _ = root;
        let held = store.hold_registry_lock_for_account_test().unwrap();
        let started = Instant::now();
        assert!(store.try_load().is_err());
        assert!(store.try_update(|_| ()).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
        store.try_load().unwrap();
    }
}
