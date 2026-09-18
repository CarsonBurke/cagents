//! Shared exact-session recovery authority and durable daemon upgrade handoff.
#[cfg(any(feature = "host", feature = "ui", test))]
use crate::model::{HostSessionProof, LaunchRecipe, TerminalSession};
#[cfg(any(feature = "host", feature = "ui", test))]
use std::{fs, io};
use std::{os::unix::fs::MetadataExt, path::Path};

/// Automatic recovery never replays arbitrary command recipes.
#[cfg(any(feature = "host", feature = "ui", test))]
pub(crate) fn recovery_recipe(session: &TerminalSession) -> Result<LaunchRecipe, String> {
    if let Some(recipe) = session.agent_resume_recipe() {
        return Ok(recipe);
    }
    if LaunchRecipe::shell(session.launch.working_directory.clone())
        .is_ok_and(|shell| shell == session.launch)
    {
        return Ok(session.launch.clone());
    }
    Err("This command has no exact agent identity and cannot safely be replayed; finish or close it before upgrading".into())
}

/// Recipes passed here have already been normalized by agent_resume_recipe.
/// Only the provider's resume selector identifies the writer, never UUID-shaped
/// values of unrelated model, profile, or configuration flags.
#[cfg(any(feature = "host", feature = "ui", test))]
pub(crate) fn exact_conversation(
    recipe: &LaunchRecipe,
) -> Option<(crate::agent::Provider, uuid::Uuid)> {
    let provider = crate::agent::Provider::from_program(&recipe.program)?;
    let id = provider.resume_id(&recipe.arguments)?;
    Some((provider, uuid::Uuid::parse_str(id).ok()?))
}

/// The lease proves the previous daemon no longer owns this runtime. Its PTY
/// descendants can nevertheless survive SIGHUP, so inspect before recreating a
/// shell or opening an exact conversation for writing. Unknown evidence blocks.
#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn ensure_no_surviving_process(
    session: &TerminalSession,
    owner: u32,
    proc_root: &Path,
) -> Result<(), String> {
    use std::collections::HashMap;
    let mut processes = HashMap::new();
    let manager = UserManagerSession::connect();
    for entry in fs::read_dir(proc_root).map_err(process_error)? {
        let entry = entry.map_err(process_error)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(process_error(error)),
        };
        if metadata.uid() != crate::daemon::current_uid() {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(process_error(error)),
        };
        let fields = stat
            .rsplit_once(')')
            .ok_or("cannot inspect process ancestry")?
            .1;
        let mut fields = fields.split_whitespace();
        let state = fields.next().ok_or("cannot inspect process state")?;
        let parent = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or("cannot inspect process parent")?;
        if state != "Z" && state != "X" {
            processes.insert(pid, parent);
        }
    }
    let exact_resume = session.agent_resume_recipe();
    let exact_identity = exact_resume.as_ref().and_then(exact_conversation);
    let terminal_marker = format!("CAGENTS_TERMINAL_ID={}", session.id);
    for &pid in processes.keys() {
        if pid == std::process::id() || belongs_to_owner(pid, owner, &processes) {
            continue;
        }
        let path = proc_root.join(pid.to_string());
        let argv = match fs::read(path.join("cmdline")) {
            Ok(argv) => argv,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(process_error(error)),
        };
        // exec can temporarily expose an empty proc file before publishing the
        // new arguments/environment. Absence of evidence cannot permit replay.
        if argv.is_empty() {
            return Err(format!("cannot inspect process invocation ({pid})"));
        }
        if argv.split(|byte| *byte == 0).any(|arg| {
            exact_identity.is_some_and(|(provider, id)| {
                std::str::from_utf8(arg).ok().is_some_and(|arg| {
                    uuid::Uuid::parse_str(arg).ok() == Some(id)
                        || provider
                            .session_file_id(Path::new(arg))
                            .and_then(|locator| uuid::Uuid::parse_str(&locator).ok())
                            == Some(id)
                })
            })
        }) {
            return Err(format!(
                "agent session still has a live process ({pid}); automatic recovery would duplicate its writer"
            ));
        }
        let environment = match fs::read(path.join("environ")) {
            Ok(environment) => environment,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && manager
                        .as_ref()
                        .is_some_and(|manager| manager.contains(pid)) =>
            {
                continue;
            }
            Err(error) => return Err(process_error(error)),
        };
        if environment.is_empty() {
            // Sandboxes can intentionally launch with no environment. Only an
            // initialized zero-length range proves this is not an exec-time gap.
            let stat = match fs::read_to_string(path.join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(process_error(error)),
            };
            let published_empty = stat.rsplit_once(')').is_some_and(|(_, fields)| {
                // Fields after comm begin at 3; env_start/env_end are 50/51.
                let mut fields = fields.split_whitespace().skip(47);
                let start = fields.next().and_then(|value| value.parse::<u64>().ok());
                let end = fields.next().and_then(|value| value.parse::<u64>().ok());
                matches!((start, end), (Some(start), Some(end)) if start != 0 && start == end)
            });
            if !published_empty {
                return Err(format!("cannot inspect process environment ({pid})"));
            }
        }
        let mut cagents = false;
        for entry in environment.split(|byte| *byte == 0) {
            if entry == terminal_marker.as_bytes() {
                return Err(format!(
                    "terminal {} still has a live process ({pid}); preserving it",
                    session.id
                ));
            }
            cagents |=
                entry.starts_with(b"CAGENTS_TERMINAL_ID=") || entry == b"TERM_PROGRAM=cagents";
        }
        if cagents && !belongs_to_other_daemon(pid, session, &processes, proc_root)? {
            return Err(format!(
                "an unclassified cagents process ({pid}) survives outside the replacement daemon; preserving it instead of launching a duplicate"
            ));
        }
    }
    Ok(())
}

/// The user manager and PAM helper can deny environ reads even to their owner.
/// A hosted PTY always starts a new POSIX session; its descendants cannot join
/// this pre-existing service session. Authenticate it, never trust process names.
#[cfg(any(feature = "host", feature = "ui"))]
struct UserManagerSession {
    id: i32,
    process: fs::File,
}

#[cfg(any(feature = "host", feature = "ui"))]
impl UserManagerSession {
    fn connect() -> Option<Self> {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::net::UnixStream,
        };
        let uid = crate::daemon::current_uid();
        let stream = UnixStream::connect(format!("/run/user/{uid}/systemd/private")).ok()?;
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        // SAFETY: both output buffers are valid for the supplied lengths.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of_val(&credentials)
            || credentials.uid != uid
            || credentials.pid <= 1
        {
            return None;
        }
        let mut descriptor: libc::c_int = -1;
        length = std::mem::size_of_val(&descriptor) as libc::socklen_t;
        // SO_PEERPIDFD binds the descriptor to the actual socket peer, unlike
        // opening a numeric PID after obtaining credentials (a PID reuse race).
        // SAFETY: descriptor/length are valid writable buffers.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut descriptor as *mut libc::c_int).cast(),
                &mut length,
            )
        } != 0
        {
            return None;
        }
        // SAFETY: successful SO_PEERPIDFD transfers one owned descriptor.
        let process = unsafe { fs::File::from_raw_fd(descriptor) };
        let manager = Self {
            id: credentials.pid,
            process,
        };
        manager.contains(credentials.pid as u32).then_some(manager)
    }

    fn contains(&self, pid: u32) -> bool {
        use std::os::fd::AsRawFd;
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        let mut poll = libc::pollfd {
            fd: self.process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: getsid takes scalar PIDs; poll borrows one initialized entry.
        // The manager must still be a live session leader after the SID reads.
        unsafe {
            libc::getsid(self.id) == self.id
                && libc::getsid(pid) == self.id
                && libc::poll(&mut poll, 1, 0) == 0
        }
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn belongs_to_owner(
    pid: u32,
    owner: u32,
    processes: &std::collections::HashMap<u32, u32>,
) -> bool {
    let mut current = pid;
    for _ in 0..processes.len() {
        if current == owner {
            return true;
        }
        let Some(parent) = processes.get(&current).copied() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    false
}

/// Whether a daemon lock file lives in this instance's runtime directory.
///
/// Compared against the directory this build's own socket resolves to when
/// that is known, and against the instance's runtime subdirectory name when
/// `XDG_RUNTIME_DIR` is unavailable, which is the case inside a test.
#[cfg(any(feature = "host", feature = "ui"))]
fn same_instance_runtime(lock_path: &Path) -> bool {
    let Some(directory) = lock_path.parent() else {
        return true;
    };
    match crate::terminal::ipc::daemon_socket_path() {
        Ok(socket) => socket.parent() == Some(directory),
        Err(_) => directory
            .file_name()
            .is_none_or(|name| name == crate::instance::RUNTIME_SUBDIRECTORY),
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn belongs_to_other_daemon(
    pid: u32,
    session: &TerminalSession,
    processes: &std::collections::HashMap<u32, u32>,
    proc_root: &Path,
) -> Result<bool, String> {
    let mut current = pid;
    for _ in 0..processes.len() {
        let path = proc_root.join(current.to_string());
        if fs::read_link(path.join("exe"))
            .ok()
            .is_some_and(|exe| exe.file_name().is_some_and(|name| name == "cagentsd"))
        {
            for entry in fs::read_dir(path.join("fd")).map_err(process_error)? {
                let entry = entry.map_err(process_error)?;
                let Ok(lock_path) = fs::read_link(entry.path()) else {
                    continue;
                };
                if lock_path
                    .file_name()
                    .is_none_or(|name| name != "cagentsd.lock")
                {
                    continue;
                }
                // A daemon of another instance - a development build beside
                // the production install, a second install - hosts terminals
                // whose environment carries the same instance-agnostic
                // markers. Its processes are doing exactly what they should
                // and have nothing to do with this recovery, so they are
                // owned rather than unclassified. Which instance a daemon
                // serves is the runtime directory its lock lives in.
                if !same_instance_runtime(&lock_path) {
                    return Ok(true);
                }
                #[derive(serde::Deserialize)]
                struct Metadata {
                    proof: HostSessionProof,
                }
                let metadata: Metadata =
                    serde_json::from_reader(fs::File::open(&lock_path).map_err(process_error)?)
                        .map_err(|error| {
                            format!("cannot inspect surviving daemon proof: {error}")
                        })?;
                if session.continuity.live_proof().as_ref() == Some(&metadata.proof) {
                    return Err(
                        "the original daemon still has live processes; preserving its terminals"
                            .into(),
                    );
                }
                let socket = lock_path.with_file_name(crate::terminal::ipc::SOCKET_FILE_NAME);
                return crate::daemon::replacement_owner(&socket, &metadata.proof)
                    .map(|owner| owner == current)
                    .map_err(|error| format!("cannot authenticate surviving daemon: {error}"));
            }
            return Err(
                "a surviving daemon has unknown ownership; automatic recovery is unsafe".into(),
            );
        }
        let Some(parent) = processes.get(&current).copied() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    Ok(false)
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn process_error(error: io::Error) -> String {
    format!("cannot rule out surviving terminal processes; automatic recovery is unsafe: {error}")
}

pub(crate) mod handoff {
    use super::*;
    use crate::fs_safety;
    #[cfg(any(feature = "host", test))]
    use crate::{chat, terminal_ipc::TerminalSpawnOptions};
    use serde::Deserialize;
    #[cfg(any(feature = "host", test))]
    use serde::Serialize;
    #[cfg(any(feature = "host", test))]
    use std::io::Write;
    use std::{ffi::OsStr, path::PathBuf};

    const VERSION: u16 = 1;
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_TERMINALS: usize = 1024;
    const MAX_CHATS: usize = 128;
    const FILE: &str = "daemon-upgrade.json";

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) enum Phase {
        /// Persisted before any irreversible stop. Provider closure is unproven.
        Prepared,
        /// All old writers were confirmed closed and latest identities saved.
        Ready,
        /// At least one new writer may have started. Never replay this blindly.
        Restoring,
    }

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct Terminal {
        pub source: TerminalSession,
        /// Oldest first; each source's safe restart is the next issued launch.
        #[serde(default)]
        pub previous_sources: Vec<TerminalSession>,
        pub recipe: LaunchRecipe,
        pub options: TerminalSpawnOptions,
    }

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct Chat {
        /// Immutable issued launch and former daemon ownership, independent of
        /// any visible-only session identity cached by a client.
        pub source: chat::Session,
        #[serde(default)]
        pub previous_sources: Vec<ChatSource>,
        /// Final provider-confirmed identity and normalized exact restart argv.
        pub launch: chat::Launch,
    }

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct ChatSource {
        pub source: chat::Session,
        /// Provider-confirmed launch issued by the next daemon.
        pub launch: chat::Launch,
    }

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct Checkpoint {
        version: u16,
        pub proof: HostSessionProof,
        pub phase: Phase,
        pub terminals: Vec<Terminal>,
        pub chats: Vec<Chat>,
    }

    #[cfg(any(feature = "host", test))]
    impl Checkpoint {
        pub fn new(proof: HostSessionProof, terminals: Vec<Terminal>, chats: Vec<Chat>) -> Self {
            Self {
                version: VERSION,
                proof,
                phase: Phase::Prepared,
                terminals,
                chats,
            }
        }

        pub fn validate(&self) -> Result<(), String> {
            if self.version != VERSION || !self.proof.is_complete() {
                return Err(
                    "Unsupported or incomplete upgrade checkpoint; checkpoint retained".into(),
                );
            }
            if self.terminals.len() > MAX_TERMINALS || self.chats.len() > MAX_CHATS {
                return Err("Upgrade checkpoint exceeds session limits".into());
            }
            fn source_proof(
                proof: Option<HostSessionProof>,
                seen: &mut Vec<HostSessionProof>,
            ) -> Result<(), String> {
                let proof = proof
                    .filter(HostSessionProof::is_complete)
                    .ok_or("Upgrade lineage has incomplete ownership")?;
                if seen.contains(&proof) {
                    return Err("Upgrade lineage repeats daemon ownership".into());
                }
                seen.push(proof);
                Ok(())
            }
            let mut ids = std::collections::BTreeSet::new();
            let mut conversations = std::collections::BTreeSet::new();
            let mut chat_ids = std::collections::BTreeSet::new();
            for terminal in &self.terminals {
                if !ids.insert(terminal.source.id)
                    || terminal.source.continuity.live_proof().as_ref() != Some(&self.proof)
                    || recovery_recipe(&terminal.source)? != terminal.recipe
                {
                    return Err("Upgrade terminal source or safe recipe is inconsistent".into());
                }
                let mut proofs = Vec::new();
                for (source, next) in terminal.previous_sources.iter().zip(
                    terminal
                        .previous_sources
                        .iter()
                        .skip(1)
                        .chain(std::iter::once(&terminal.source)),
                ) {
                    source_proof(source.continuity.live_proof(), &mut proofs)?;
                    if source.id != terminal.source.id || recovery_recipe(source)? != next.launch {
                        return Err("Upgrade terminal lineage is inconsistent".into());
                    }
                }
                source_proof(terminal.source.continuity.live_proof(), &mut proofs)?;
                if let Some((provider, id)) = exact_conversation(&terminal.recipe)
                    && !conversations.insert((provider.executable(), id))
                {
                    return Err("Upgrade checkpoint contains duplicate conversation writers".into());
                }
            }
            for chat in &self.chats {
                let paired_codex = chat.launch.provider == crate::agent::Provider::Codex
                    && self.terminals.iter().any(|terminal| {
                        terminal.source.id == chat.source.id
                            && chat
                                .source
                                .terminal
                                .as_ref()
                                .is_some_and(|source| source.id == terminal.source.id)
                            && chat.launch.terminal_recipe().as_ref() == Some(&terminal.recipe)
                    });
                if !chat_ids.insert(chat.source.id)
                    || (!ids.insert(chat.source.id) && !paired_codex)
                    || chat.source.proof.as_ref() != Some(&self.proof)
                    || chat.source.launch.provider != chat.launch.provider
                {
                    return Err("Upgrade native source identity is inconsistent".into());
                }
                let mut proofs = Vec::new();
                for (previous, next) in chat.previous_sources.iter().zip(
                    chat.previous_sources
                        .iter()
                        .skip(1)
                        .map(|previous| &previous.source)
                        .chain(std::iter::once(&chat.source)),
                ) {
                    source_proof(previous.source.proof.clone(), &mut proofs)?;
                    validate_chat_source(&previous.source, &previous.launch)?;
                    if previous.source.id != chat.source.id || previous.launch != next.launch {
                        return Err("Upgrade native lineage is inconsistent".into());
                    }
                }
                source_proof(chat.source.proof.clone(), &mut proofs)?;
                validate_chat_source(&chat.source, &chat.launch)?;
                let recipe = chat
                    .launch
                    .terminal_recipe()
                    .ok_or("Native chat has no exact resumable provider identity")?;
                let (provider, id) = exact_conversation(&recipe)
                    .ok_or("Native chat has no exact resumable provider identity")?;
                if !conversations.insert((provider.executable(), id)) && !paired_codex {
                    return Err("Upgrade checkpoint contains duplicate conversation writers".into());
                }
            }
            Ok(())
        }

        pub fn require_ready(&self) -> Result<(), String> {
            self.validate()?;
            if self.phase != Phase::Ready {
                return Err(format!(
                    "Upgrade checkpoint is {:?}; previous writer shutdown or partial restore is unconfirmed. Checkpoint retained; do not remove it until all previous provider work is confirmed closed",
                    self.phase
                ));
            }
            Ok(())
        }
    }

    #[cfg(any(feature = "host", test))]
    fn validate_chat_source(source: &chat::Session, launch: &chat::Launch) -> Result<(), String> {
        let mut confirmed = source.launch.clone();
        confirmed.session_id = launch.session_id.clone();
        let recipe = confirmed
            .terminal_recipe()
            .ok_or("Native chat has no exact resumable provider identity")?;
        if source.launch.provider != launch.provider || Some(recipe) != launch.terminal_recipe() {
            return Err("Upgrade native source recipe is inconsistent".into());
        }
        if let Some(terminal) = &source.terminal
            && (terminal.id != source.id
                || terminal
                    .continuity
                    .live_proof()
                    .as_ref()
                    .is_some_and(|proof| Some(proof) != source.proof.as_ref()))
        {
            return Err("Upgrade native terminal ownership is inconsistent".into());
        }
        Ok(())
    }

    /// Budget sequential provider confirmation without parsing session payloads.
    pub(crate) fn startup_timeout() -> Result<std::time::Duration, String> {
        Store::discover()?.startup_timeout()
    }

    #[derive(Deserialize)]
    struct Inventory {
        version: u16,
        #[serde(deserialize_with = "count_items")]
        terminals: usize,
        #[serde(deserialize_with = "count_items")]
        chats: usize,
    }

    fn count_items<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
        struct Count;
        impl<'de> serde::de::Visitor<'de> for Count {
            type Value = usize;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a session inventory")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<usize, A::Error> {
                let mut count = 0;
                while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    count += 1;
                }
                Ok(count)
            }
        }
        deserializer.deserialize_seq(Count)
    }

    pub(crate) struct Store {
        root: fs_safety::Root,
        pub path: PathBuf,
    }

    impl Store {
        pub fn discover() -> Result<Self, String> {
            let dirs =
                crate::instance::project_dirs().ok_or("Cannot discover durable upgrade storage")?;
            Self::open(&dirs.data_local_dir().join("daemon-upgrade"))
        }

        pub fn open(directory: &Path) -> Result<Self, String> {
            crate::secure_file::prepare_private_directory(directory)
                .map_err(|error| error.to_string())?;
            let root = fs_safety::Root::open(directory).map_err(|error| error.to_string())?;
            Ok(Self {
                root,
                path: directory.join(FILE),
            })
        }

        fn read_bytes(&self) -> Result<Option<Vec<u8>>, String> {
            let mut file = match fs_safety::open_beneath(&self.root, Path::new(FILE)) {
                Ok(file) => file,
                Err(error) if error.is_not_found() => return Ok(None),
                Err(error) => return Err(error.to_string()),
            };
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file()
                || metadata.uid() != crate::daemon::current_uid()
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(format!("Unsafe upgrade checkpoint {}", self.path.display()));
            }
            let bytes =
                fs_safety::read_bounded(&mut file, MAX_BYTES).map_err(|error| error.to_string())?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("Upgrade checkpoint exceeds durable byte limit".into());
            }
            Ok(Some(bytes))
        }

        fn startup_timeout(&self) -> Result<std::time::Duration, String> {
            let Some(bytes) = self.read_bytes()? else {
                return Ok(std::time::Duration::from_secs(5));
            };
            let inventory: Inventory =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if inventory.version != VERSION {
                return Err("Unsupported upgrade checkpoint; checkpoint retained".into());
            }
            if inventory.terminals > MAX_TERMINALS || inventory.chats > MAX_CHATS {
                return Err("Upgrade checkpoint exceeds session limits".into());
            }
            Ok(std::time::Duration::from_secs(
                5 + 30 * (inventory.terminals + inventory.chats) as u64,
            ))
        }

        #[cfg(any(feature = "host", test))]
        pub fn load(&self) -> Result<Option<Checkpoint>, String> {
            let Some(bytes) = self.read_bytes()? else {
                return Ok(None);
            };
            let checkpoint: Checkpoint =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            checkpoint.validate()?;
            Ok(Some(checkpoint))
        }

        #[cfg(any(feature = "host", test))]
        pub fn save(&self, checkpoint: &Checkpoint) -> Result<(), String> {
            checkpoint.validate()?;
            // Serialization is bounded independently of the number of records:
            // launch environments and paths are supplied by clients.
            struct Bounded(Vec<u8>);
            impl Write for Bounded {
                fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                    if bytes.len() as u64 > MAX_BYTES - self.0.len() as u64 {
                        return Err(io::Error::other(
                            "Upgrade checkpoint exceeds durable byte limit",
                        ));
                    }
                    self.0.extend_from_slice(bytes);
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            let mut bytes = Bounded(Vec::new());
            serde_json::to_writer(&mut bytes, checkpoint).map_err(|error| error.to_string())?;
            let (mut file, name) = fs_safety::create_temporary_in(&self.root, 0o600)
                .map_err(|error| error.to_string())?;
            let result = (|| {
                file.write_all(&bytes.0)
                    .map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())?;
                fs_safety::rename_within(&self.root, &name, OsStr::new(FILE))
                    .map_err(|error| error.to_string())?;
                fs_safety::sync_directory(&self.root).map_err(|error| error.to_string())
            })();
            if result.is_err() {
                let _ = fs_safety::unlink_within(&self.root, &name);
            }
            result
        }

        pub fn archive(&self) -> Result<PathBuf, String> {
            const ARCHIVE: &str = "daemon-upgrade.abandoned.json";
            let file = fs_safety::open_beneath(&self.root, Path::new(FILE))
                .map_err(|error| error.to_string())?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file()
                || metadata.uid() != crate::daemon::current_uid()
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
                || metadata.len() > MAX_BYTES
            {
                return Err(format!(
                    "Unsafe upgrade checkpoint {}; evidence was not moved",
                    self.path.display()
                ));
            }
            fs_safety::rename_new_within(&self.root, OsStr::new(FILE), OsStr::new(ARCHIVE))
                .map_err(|error| format!("Cannot archive checkpoint without replacing prior evidence: {error}. Preserve and explicitly move any existing {} first", self.path.with_file_name(ARCHIVE).display()))?;
            fs_safety::sync_directory(&self.root).map_err(|error| error.to_string())?;
            Ok(self.path.with_file_name(ARCHIVE))
        }

        /// Host-only: the daemon removes a checkpoint it has finished
        /// resuming from. No UI or CLI path removes one, so the method exists
        /// only where its caller does and no lane carries it as dead code.
        #[cfg(feature = "host")]
        pub fn remove(&self) -> Result<(), String> {
            fs_safety::unlink_within(&self.root, OsStr::new(FILE))
                .map_err(|error| error.to_string())?;
            fs_safety::sync_directory(&self.root).map_err(|error| error.to_string())
        }
    }

    #[cfg(test)]
    mod timeout_tests {
        use super::*;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        #[test]
        fn startup_budget_counts_sequential_items_and_rejects_untrusted_inventory() {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            assert_eq!(
                store.startup_timeout().unwrap(),
                std::time::Duration::from_secs(5)
            );
            let save = |bytes: &[u8]| {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&store.path)
                    .unwrap();
                file.write_all(bytes).unwrap();
            };
            save(br#"{"version":1,"terminals":[{"nested":["ignored"]},{}],"chats":[{}]}"#);
            assert_eq!(
                store.startup_timeout().unwrap(),
                std::time::Duration::from_secs(95)
            );
            save(br#"{"version":2,"terminals":[],"chats":[]}"#);
            assert!(store.startup_timeout().is_err());
            save(br#"{"version":1,"terminals":[],"chats":["#);
            assert!(store.startup_timeout().is_err());
            for (terminals, chats) in [(MAX_TERMINALS + 1, 0), (0, MAX_CHATS + 1)] {
                save(
                    &serde_json::to_vec(&serde_json::json!({
                        "version": VERSION,
                        "terminals": vec![serde_json::Value::Null; terminals],
                        "chats": vec![serde_json::Value::Null; chats],
                    }))
                    .unwrap(),
                );
                assert!(store.startup_timeout().is_err());
            }
            save(br#"{"version":1,"terminals":[],"chats":[]}"#);
            std::fs::File::options()
                .write(true)
                .open(&store.path)
                .unwrap()
                .set_len(MAX_BYTES + 1)
                .unwrap();
            assert!(store.startup_timeout().is_err());
            save(br#"{"version":1,"terminals":[],"chats":[]}"#);
            std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(store.startup_timeout().is_err());
        }
    }
}

/// Explicitly abandon automatic replay while retaining every source descriptor.
/// Requires a stopped daemon and never replaces a previous abandoned archive.
pub fn abandon() -> Result<std::path::PathBuf, String> {
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    let _lease = crate::daemon::HostLease::acquire_for_maintenance(&paths)
        .map_err(|error| error.to_string())?;
    let store = handoff::Store::discover()?;
    store.archive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, TerminalContinuity, TerminalId};
    use handoff::{Checkpoint, Phase, Store, Terminal};

    fn checkpoint(directory: &Path) -> Checkpoint {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("upgrade-source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let recipe = LaunchRecipe::shell(directory.to_owned()).unwrap();
        let source = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: Some(directory.join("terminal.log")),
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        Checkpoint::new(
            proof,
            vec![Terminal {
                source,
                previous_sources: Vec::new(),
                recipe,
                options: crate::terminal_ipc::TerminalSpawnOptions::default(),
            }],
            Vec::new(),
        )
    }

    #[test]
    fn safe_recovery_accepts_shell_and_exact_agent_but_never_arbitrary_commands() {
        let directory = tempfile::tempdir().unwrap();
        let mut checkpoint = checkpoint(directory.path());
        checkpoint.validate().unwrap();
        let source = &mut checkpoint.terminals[0].source;
        source.launch.arguments = vec!["-c".into(), "touch already-ran".into()];
        assert!(recovery_recipe(source).is_err());
        source.launch.program = "claude".into();
        source.launch.arguments = vec![
            "--resume".into(),
            "12345678-1234-4234-8234-123456789abc".into(),
        ];
        assert_eq!(
            exact_conversation(&recovery_recipe(source).unwrap())
                .unwrap()
                .1,
            uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap()
        );
        source.launch.arguments = vec!["--continue".into()];
        assert!(recovery_recipe(source).is_err());
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn recovery_distinguishes_exec_evidence_from_published_empty_environments() {
        let directory = tempfile::tempdir().unwrap();
        let source = checkpoint(directory.path()).terminals.remove(0).source;
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (exec) D 1").unwrap();
        fs::write(process.join("cmdline"), b"").unwrap();
        fs::write(process.join("environ"), b"TERM_PROGRAM=cagents\0").unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());

        fs::write(process.join("cmdline"), b"/bin/sleep\0").unwrap();
        fs::write(process.join("environ"), b"").unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());

        let mut fields = ["0"; 49];
        fields[0] = "S";
        fields[1] = "1";
        fields[47] = "4096"; // stat fields 50/51: env_start and env_end.
        fields[48] = "4096";
        fs::write(
            process.join("stat"),
            format!("42 (empty environment) {}", fields.join(" ")),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());

        fields[48] = "8192";
        fs::write(
            process.join("stat"),
            format!("42 (unread environment) {}", fields.join(" ")),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());

        fields[47] = "0";
        fields[48] = "0";
        fs::write(
            process.join("stat"),
            format!("42 (unpublished environment) {}", fields.join(" ")),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());

        fs::write(process.join("environ"), b"PATH=/bin\0").unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());
    }

    #[test]
    fn durable_handoff_preserves_source_and_requires_confirmed_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut checkpoint = checkpoint(directory.path());
        store.save(&checkpoint).unwrap();
        let prepared = store.load().unwrap().unwrap();
        assert!(prepared.require_ready().is_err());
        assert_eq!(prepared.terminals[0].source, checkpoint.terminals[0].source);
        assert_eq!(fs::metadata(&store.path).unwrap().mode() & 0o777, 0o600);
        checkpoint.phase = Phase::Ready;
        store.save(&checkpoint).unwrap();
        store.load().unwrap().unwrap().require_ready().unwrap();
        checkpoint.phase = Phase::Restoring;
        store.save(&checkpoint).unwrap();
        assert!(store.load().unwrap().unwrap().require_ready().is_err());
        // A second load cannot convert an uncertain partial restore into replay.
        assert!(store.load().unwrap().unwrap().require_ready().is_err());
    }

    #[test]
    fn checkpoint_rejects_forged_sources_and_duplicate_exact_writers() {
        let directory = tempfile::tempdir().unwrap();
        let mut checkpoint = checkpoint(directory.path());
        let mut forged = checkpoint.terminals[0].clone();
        forged.source.id = TerminalId::new();
        forged.source.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: forged.recipe.clone(),
            last_exit: None,
        };
        checkpoint.terminals.push(forged);
        assert!(checkpoint.validate().is_err());
        checkpoint.terminals.pop();
        let terminal = &mut checkpoint.terminals[0];
        terminal.source.launch.program = "claude".into();
        terminal.source.launch.arguments = vec![
            "--resume".into(),
            "12345678-1234-4234-8234-123456789abc".into(),
        ];
        terminal.recipe = recovery_recipe(&terminal.source).unwrap();
        let mut duplicate = terminal.clone();
        duplicate.source.id = TerminalId::new();
        checkpoint.terminals.push(duplicate);
        assert!(checkpoint.validate().is_err());
    }

    #[test]
    fn abandoning_preserves_checkpoint_and_never_replaces_prior_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let first = checkpoint(directory.path());
        store.save(&first).unwrap();
        let archive = store.archive().unwrap();
        let evidence = fs::read(&archive).unwrap();
        assert!(store.load().unwrap().is_none());
        let second = checkpoint(directory.path());
        store.save(&second).unwrap();
        assert!(store.archive().is_err());
        assert_eq!(fs::read(&archive).unwrap(), evidence);
        assert_eq!(
            store.load().unwrap().unwrap().terminals[0].source.id,
            second.terminals[0].source.id
        );
    }
}
