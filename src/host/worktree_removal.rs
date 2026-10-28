//! Removal reservations. While a worktree is being removed nothing new may
//! start inside it, and whatever already runs there refuses the removal.
//!
//! A reservation is installed under the short control writer, so every launch
//! admitted before it has already published its hosted record; launches after
//! it see the reservation in `ensure_outside`. Unlike automatic cleanup, which
//! waits for global quiescence, this lets the user remove one worktree while
//! sessions elsewhere keep running. The removal itself runs outside control
//! admission, so a daemon upgrade or shutdown is refused until it finishes,
//! as for an account operation.
use super::state::SharedHost;
use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(super) type Reservations = Arc<Mutex<BTreeMap<uuid::Uuid, Arc<Scope>>>>;

/// Removals reserved at once, which also bounds the work every launch does to
/// check them.
const MAX_RESERVATIONS: usize = 16;

/// How long installation retries the control writer while other requests
/// briefly hold the gate. It never waits on the writer itself, which would
/// stall every new request behind it.
const INSTALL_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a process found in the worktree gets to leave before it refuses
/// the removal.
const OCCUPANT_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

pub(super) struct Scope {
    path: PathBuf,
    identity: (u64, u64),
    /// Every directory from the worktree up to `/`, the worktree included.
    ancestors: BTreeSet<(u64, u64)>,
    /// Keeps the inode from being reused while the reservation lasts.
    _pin: File,
}

impl Scope {
    fn capture(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("The worktree path must be absolute".into());
        }
        let path = path
            .canonicalize()
            .map_err(|error| format!("Cannot resolve the worktree: {error}"))?;
        let pin = open_directory(&path, libc::O_NOFOLLOW)
            .map_err(|error| format!("Cannot open the worktree: {error}"))?;
        let metadata = pin.metadata().map_err(|error| error.to_string())?;
        let ancestors = super::account_lifecycle::physical_ancestors(&path, Some(&pin))?;
        Ok(Self {
            path,
            identity: (metadata.dev(), metadata.ino()),
            ancestors,
            _pin: pin,
        })
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.ancestors.contains(&other.identity) || other.ancestors.contains(&self.identity)
    }

    /// Whether a directory with these physical ancestors, at this path, lies
    /// inside the worktree. Either view is enough: the inode chain follows a
    /// renamed worktree, the path covers a directory not yet created.
    fn contains(&self, path: &Path, ancestors: &BTreeSet<(u64, u64)>) -> bool {
        path.starts_with(&self.path) || ancestors.contains(&self.identity)
    }
}

fn open_directory(path: &Path, flags: libc::c_int) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | flags)
        .open(path)
}

fn refusal(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

/// Fences one worktree for removal until dropped.
pub(crate) struct Reservation {
    map: Reservations,
    token: uuid::Uuid,
    scope: Arc<Scope>,
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
    /// Caller holds no control admission: this takes the short writer itself.
    pub(super) fn install(host: &SharedHost, path: &Path) -> Result<Self, ProtocolError> {
        Self::install_within(host, path, INSTALL_PATIENCE)
    }

    fn install_within(
        host: &SharedHost,
        path: &Path,
        patience: std::time::Duration,
    ) -> Result<Self, ProtocolError> {
        super::client::ensure_control_open(host)?;
        let scope = Arc::new(Scope::capture(path).map_err(refusal)?);
        let (gate, map) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            (state.control_gate.clone(), state.worktree_removals.clone())
        };
        let deadline = std::time::Instant::now() + patience;
        let mut pause = std::time::Duration::from_millis(5);
        let writer = loop {
            match super::client::try_scoped_control_write(&gate) {
                Ok(writer) => break writer,
                Err(error) if std::time::Instant::now() + pause > deadline => return Err(error),
                Err(_) => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(std::time::Duration::from_millis(200));
                }
            }
        };
        super::client::ensure_control_open(host)?;
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        if entries.values().any(|other| other.overlaps(&scope)) {
            return Err(refusal(
                "This worktree, or one inside or around it, is already being removed",
            ));
        }
        if entries.len() >= MAX_RESERVATIONS {
            return Err(refusal(
                "Too many worktrees are being removed at once; wait and retry",
            ));
        }
        let token = uuid::Uuid::new_v4();
        entries.insert(token, Arc::clone(&scope));
        drop(entries);
        drop(writer);
        Ok(Self { map, token, scope })
    }

    /// The worktree's canonical path.
    pub(crate) fn path(&self) -> &Path {
        &self.scope.path
    }

    /// Refuses while hosted work runs in the worktree, durable agent or
    /// automation work targets it, or an account operation is changing a
    /// credential directory inside it. Call outside HostState.
    pub(super) fn ensure_unoccupied(&self, host: &SharedHost) -> Result<(), String> {
        let hosted = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let mut hosted: Vec<(&'static str, PathBuf)> = Vec::new();
            for record in state.terminals.values() {
                if !record.handle.is_completed() {
                    hosted.push(("A terminal", record.current_directory().to_owned()));
                    hosted.push((
                        "A terminal",
                        record.execution_launch().working_directory.clone(),
                    ));
                }
            }
            for record in state.chats.values() {
                if record.handle.owns_work() {
                    hosted.push((
                        "A chat",
                        record.session.launch.recipe.working_directory.clone(),
                    ));
                }
            }
            for session in state.chat_spawning.values() {
                hosted.push((
                    "A starting chat",
                    session.launch.recipe.working_directory.clone(),
                ));
            }
            for binding in state.chat_bindings.values() {
                hosted.push((
                    "An agent being adopted",
                    binding.working_directory().to_owned(),
                ));
            }
            for handoff in state.chat_handoffs.values() {
                for directory in handoff.working_directories() {
                    hosted.push(("A chat handing off to a terminal", directory.to_owned()));
                }
            }
            for closing in state.native_closing.values() {
                hosted.push(("A closing chat", closing.working_directory().to_owned()));
            }
            for (session, _) in state.execution_startups.values() {
                let directory = match session {
                    crate::agent_execution::HostedSession::Chat(session) => {
                        &session.launch.recipe.working_directory
                    }
                    crate::agent_execution::HostedSession::Terminal(session) => {
                        &session.launch.working_directory
                    }
                };
                hosted.push(("A starting agent", directory.clone()));
            }
            hosted
        };
        let registry = super::agent_execution::registry(host)?;
        let (agents, automations) = {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            (
                registry.active_agent_directories()?,
                registry.active_automation_directories()?,
            )
        };
        let durable = agents
            .into_iter()
            .map(|directory| ("Agent work", directory))
            .chain(
                automations
                    .into_iter()
                    .map(|directory| ("An automation", directory)),
            );
        for (owner, directory) in hosted.into_iter().chain(durable) {
            if self.contains(&directory) {
                return Err(format!("{owner} is running in this worktree"));
            }
        }
        super::account_lifecycle::ensure_paths(host, [self.scope.path.clone()])
            .map_err(|error| error.message)
    }

    /// Refuses while any process this user can inspect has its working
    /// directory, root, executable, open file or mapped file inside the worktree.
    /// This sees what the daemon does not host: shells, editors, build watchers.
    pub(crate) fn ensure_no_processes(&self) -> Result<(), String> {
        let scan = || occupant(Path::new("/proc"), &self.scope.path, std::process::id());
        let mut found = scan();
        // A short-lived Git child of the board's own scan is gone moments
        // later; what still uses the worktree after a pause is no passerby.
        if matches!(found, Ok(Some(_))) {
            std::thread::sleep(OCCUPANT_GRACE);
            found = scan();
        }
        match found {
            Ok(None) => Ok(()),
            Ok(Some((pid, name))) => Err(format!(
                "{name} (process {pid}) is using this worktree; close it and retry"
            )),
            Err(error) => Err(format!(
                "Cannot check processes using the worktree: {error}"
            )),
        }
    }

    /// A relative directory names nothing the daemon can resolve, and one it
    /// cannot open is judged by its path alone, so neither blocks every
    /// removal; the process scan still finds whatever actually runs there.
    fn contains(&self, directory: &Path) -> bool {
        if !directory.is_absolute() {
            return false;
        }
        let ancestors = existing_ancestors(directory).unwrap_or_default();
        self.scope.contains(directory, &ancestors)
    }
}

/// Physical ancestors of the nearest existing directory at or above `path`,
/// resolved the way `chdir` would. A missing directory has none of its own.
fn existing_ancestors(path: &Path) -> Result<BTreeSet<(u64, u64)>, String> {
    let mut existing = path;
    loop {
        match open_directory(existing, 0) {
            Ok(directory) => {
                return super::account_lifecycle::physical_ancestors(existing, Some(&directory));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                match existing.parent() {
                    Some(parent) => existing = parent,
                    None => return Ok(BTreeSet::new()),
                }
            }
            Err(error) => return Err(format!("Cannot inspect {}: {error}", existing.display())),
        }
    }
}

fn scopes(host: &SharedHost) -> Vec<Arc<Scope>> {
    let map = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .worktree_removals
        .clone();
    map.lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect()
}

/// Refuses a daemon upgrade or shutdown while a removal runs: either would end
/// Git's deletion partway through.
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
    ) && (!scopes(host).is_empty()
        || host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .worktree_mutations
            != 0)
    {
        return Err(refusal(
            "A worktree operation is still finishing. Wait for it before shutting down or upgrading this daemon.",
        ));
    }
    Ok(())
}

const STARTING_INSIDE: &str = "This worktree is being removed. Start the session somewhere else, or wait for the removal to finish.";

/// Refuses a launch whose working directory lies in a worktree being removed.
/// Caller holds shared control admission through the launch it checks.
pub(super) fn ensure_outside(host: &SharedHost, cwd: &Path) -> Result<(), ProtocolError> {
    let scopes = scopes(host);
    if scopes.is_empty() {
        return Ok(());
    }
    if !cwd.is_absolute() {
        return Err(refusal(
            "Cannot place a relative working directory while a worktree is being removed",
        ));
    }
    let ancestors = existing_ancestors(cwd).map_err(refusal)?;
    if scopes.iter().any(|scope| scope.contains(cwd, &ancestors)) {
        return Err(refusal(STARTING_INSIDE));
    }
    Ok(())
}

/// `ensure_outside` for a launch pinned to an open directory, which can have
/// moved away from the path it was opened at.
pub(super) fn ensure_outside_directory(
    host: &SharedHost,
    directory: &File,
) -> Result<(), ProtocolError> {
    let scopes = scopes(host);
    if scopes.is_empty() {
        return Ok(());
    }
    let ancestors = super::account_lifecycle::physical_ancestors(Path::new("/"), Some(directory))
        .map_err(refusal)?;
    if scopes
        .iter()
        .any(|scope| ancestors.contains(&scope.identity))
    {
        return Err(refusal(STARTING_INSIDE));
    }
    Ok(())
}

/// The first process under `proc` other than `own` with its working
/// directory, root, executable, open descriptor or mapped file inside `scope`.
/// Processes that vanish, or that belong to another user, are skipped.
fn occupant(proc: &Path, scope: &Path, own: u32) -> io::Result<Option<(u32, String)>> {
    let inside = |link: &Path| match std::fs::read_link(link) {
        Ok(target) => Ok(Path::new(
            target
                .to_str()
                .and_then(|target| target.strip_suffix(" (deleted)"))
                .map_or(target.as_os_str(), std::ffi::OsStr::new),
        )
        .starts_with(scope)),
        Err(error) if skippable(&error) => Ok(false),
        Err(error) => Err(error),
    };
    for entry in std::fs::read_dir(proc)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        let process = entry.path();
        let mut found = false;
        for link in ["cwd", "root", "exe"] {
            if inside(&process.join(link))? {
                found = true;
                break;
            }
        }
        if !found {
            let descriptors = match std::fs::read_dir(process.join("fd")) {
                Ok(descriptors) => descriptors,
                Err(error) if skippable(&error) => continue,
                Err(error) => return Err(error),
            };
            for descriptor in descriptors {
                let descriptor = match descriptor {
                    Ok(descriptor) => descriptor,
                    Err(error) if skippable(&error) => break,
                    Err(error) => return Err(error),
                };
                if inside(&descriptor.path())? {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            // Closing a descriptor does not release a file-backed mapping.
            // A writer can still change that file after the content inspection.
            let mappings = match std::fs::read_dir(process.join("map_files")) {
                Ok(mappings) => mappings,
                Err(error) if skippable(&error) => continue,
                Err(error) => return Err(error),
            };
            for mapping in mappings {
                let mapping = match mapping {
                    Ok(mapping) => mapping,
                    Err(error) if skippable(&error) => break,
                    Err(error) => return Err(error),
                };
                if inside(&mapping.path())? {
                    found = true;
                    break;
                }
            }
        }
        if found {
            let name = std::fs::read_to_string(process.join("comm"))
                .map(|name| name.trim_end().to_owned())
                .unwrap_or_else(|_| "A process".into());
            return Ok(Some((pid, name)));
        }
    }
    Ok(None)
}

fn skippable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    ) || error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(test)]
mod tests;
