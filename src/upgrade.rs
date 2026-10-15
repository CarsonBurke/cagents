//! Shared exact-session recovery authority and durable daemon upgrade handoff.
#[cfg(any(feature = "host", test))]
use crate::model::HostSessionProof;
#[cfg(any(feature = "host", feature = "ui", test))]
use crate::model::{LaunchRecipe, TerminalSession};
#[cfg(any(feature = "host", feature = "ui", test))]
use std::{fs, io};
use std::{os::unix::fs::MetadataExt, path::Path};

/// Whether an upgrade relaunches agents it cannot resume exactly into their
/// resume picker, when the command names neither choice: the Settings preference
/// `relaunch_agents_on_upgrade` in `settings`, which is on by default.
///
/// Only that key is read, so the CLI needs none of the UI's settings model.
/// A file that exists but cannot be read is an error rather than the default,
/// because it may be the one saying no.
pub fn relaunch_agents_by_default(settings: &Path) -> Result<bool, String> {
    #[derive(serde::Deserialize)]
    struct Preference {
        #[serde(default = "relaunch_by_default")]
        relaunch_agents_on_upgrade: bool,
    }
    fn relaunch_by_default() -> bool {
        true
    }
    let document = match std::fs::read_to_string(settings) {
        Ok(document) => document,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(format!("cannot read {}: {error}", settings.display())),
    };
    toml::from_str::<Preference>(&document)
        .map(|preference| preference.relaunch_agents_on_upgrade)
        .map_err(|error| format!("cannot read {}: {error}", settings.display()))
}

/// How a terminal comes back after its daemon is gone, and why.
#[cfg(any(feature = "host", feature = "ui", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryKind {
    /// The exact saved conversation resumes.
    Exact,
    /// A recognised agent without an exact conversation reopens its provider's
    /// resume picker, so the conversation stays one choice away.
    Picker,
    /// A plain login shell relaunches.
    Shell,
}

#[cfg(any(feature = "host", feature = "ui"))]
impl RecoveryKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact resume",
            Self::Picker => "resume picker",
            Self::Shell => "shell",
        }
    }
}

/// Automatic recovery never replays arbitrary command recipes.
#[cfg(any(feature = "host", test))]
pub(crate) fn recovery_recipe(session: &TerminalSession) -> Result<LaunchRecipe, String> {
    recovery_plan(session).map(|(recipe, _)| recipe)
}

/// The recipe `recovery_recipe` chooses, with the kind of recovery it is.
///
/// An agent the terminal was known to run is never downgraded to the shell it
/// ran in: a shell would leave its conversation findable only by someone who
/// knows where the provider keeps it.
#[cfg(any(feature = "host", feature = "ui", test))]
pub(crate) fn recovery_plan(
    session: &TerminalSession,
) -> Result<(LaunchRecipe, RecoveryKind), String> {
    use crate::model::agent_recovery;
    if let Some(recipe) = session.agent_resume_recipe() {
        return Ok((recipe, RecoveryKind::Exact));
    }
    // A recognised agent without exact identity: the hint's observed launch
    // when it ran inside a shell, otherwise the terminal's own agent command.
    if let Some(recipe) = session
        .resume_hint
        .as_deref()
        .and_then(agent_recovery::observed_launch_from_hint)
        .and_then(|launch| agent_recovery::picker_recipe(&launch))
        .or_else(|| agent_recovery::picker_recipe(&session.launch))
    {
        return Ok((recipe, RecoveryKind::Picker));
    }
    if recoverable_shell(&session.launch) {
        return Ok((session.launch.clone(), RecoveryKind::Shell));
    }
    Err("This command has no agent identity and cannot safely be replayed; finish or close it before upgrading".into())
}

/// A shell cagents made can carry the selected provider homes. Other
/// environment overrides can change what the shell executes and are not
/// enough evidence to replay a command during an upgrade.
#[cfg(any(feature = "host", feature = "ui", test))]
fn recoverable_shell(recipe: &LaunchRecipe) -> bool {
    let Ok(shell) = LaunchRecipe::shell(recipe.working_directory.clone()) else {
        return false;
    };
    shell.program == recipe.program
        && shell.arguments == recipe.arguments
        && recipe.environment.iter().all(|(name, home)| {
            matches!(name.as_str(), "CODEX_HOME" | "CLAUDE_CONFIG_DIR")
                && std::path::Path::new(home).is_absolute()
        })
}

/// Setup recipes are retained for an explicit reviewed run, never automatic
/// upgrade replay. The marker is classification, not execution authority.
#[cfg(any(feature = "host", test))]
pub(crate) fn is_worktree_setup_recipe(recipe: &LaunchRecipe) -> bool {
    recipe.environment.get("CAGENTS_WORKTREE_SETUP").is_some_and(|value| value == "1")
}

#[cfg(any(feature = "host", test))]
pub(crate) fn checkpoint_recipe(
    session: &TerminalSession,
    relaunches: &[crate::terminal_ipc::UpgradeRelaunch],
) -> Result<LaunchRecipe, String> {
    if let Some(accepted) = relaunches
        .iter()
        .find(|accepted| accepted.source == *session)
    {
        use crate::model::agent_recovery;
        if !agent_recovery::is_picker_recipe(&accepted.recipe)
            && agent_recovery::fresh_recipe(&accepted.recipe).as_ref() != Some(&accepted.recipe)
        {
            return Err(
                "Accepted relaunch must be a resume picker or fresh interactive agent recipe"
                    .into(),
            );
        }
        return Ok(accepted.recipe.clone());
    }
    recovery_recipe(session)
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
    let mut starts = HashMap::new();
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
            if let Some(start) = fields.nth(17).and_then(|start| start.parse::<u64>().ok()) {
                starts.insert(pid, start);
            }
        }
    }
    let exact_resume = session.agent_resume_recipe();
    let exact_identity = exact_resume.as_ref().and_then(exact_conversation);
    let terminal_marker = format!("CAGENTS_TERMINAL_ID={}", session.id);
    let origin_marker = format!(
        "CAGENTS_RECOVERY_ORIGIN={}",
        session.recovery_origin.unwrap_or(session.id)
    );
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
            // A PID-namespace init can remain sleeping inside do_exit while
            // waiting for its children, after losing its address space and
            // proc invocation. PF_EXITING is kernel-owned and irreversible:
            // a single-thread process cannot return to userspace or write the
            // conversation. An exiting leader can still have live threads.
            // Keep its ancestry above; descendants are inspected separately.
            let stat = match fs::read_to_string(path.join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(process_error(error)),
            };
            if stat.rsplit_once(')').is_some_and(|(_, fields)| {
                let fields: Vec<_> = fields.split_whitespace().collect();
                fields.get(17) == Some(&"1")
                    && fields
                        .get(19)
                        .and_then(|start| start.parse::<u64>().ok())
                        .is_some_and(|start| starts.get(&pid) == Some(&start))
                    && fields
                        .get(6)
                        .and_then(|flags| flags.parse::<u64>().ok())
                        .is_some_and(|flags| flags & 0x4 != 0)
            }) {
                continue;
            }
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
        for entry in environment.split(|byte| *byte == 0) {
            if entry == terminal_marker.as_bytes() || entry == origin_marker.as_bytes() {
                if detached_browser_helper(&path) {
                    break;
                }
                return Err(format!(
                    "terminal {} still has a live process ({pid}); preserving it",
                    session.id
                ));
            }
        }
        // Terminal branding and another terminal's marker can be inherited by
        // detached browsers and services. Neither proves ownership of this
        // session. Recovery lineage and exact agent identities do.
    }
    Ok(())
}

/// Browser tooling starts long-lived services which inherit the launching
/// terminal's environment. Those markers alone do not make a detached browser
/// service a surviving terminal. Keep this exception positive and narrow:
/// arbitrary detached commands (including agents) can still own a conversation.
/// The exact conversation check above always takes precedence.
#[cfg(any(feature = "host", feature = "ui"))]
fn detached_browser_helper(process: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    let Ok(executable) = fs::read_link(process.join("exe")) else {
        return false;
    };
    let Some(name) = executable.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !matches!(
        name.strip_suffix(" (deleted)").unwrap_or(name),
        "agent-browser-linux-x64"
            | "agent-browser-linux-arm64"
            | "agent-browser-linux-musl-x64"
            | "agent-browser-linux-musl-arm64"
            | "chrome_crashpad_handler"
    ) {
        return false;
    }
    let Ok(stat) = fs::read_to_string(process.join("stat")) else {
        return false;
    };
    let detached = stat.rsplit_once(')').is_some_and(|(_, fields)| {
        // Fields after comm start at 3; tty_nr is field 7.
        fields.split_whitespace().nth(4) == Some("0")
    });
    if !detached {
        return false;
    }
    let Ok(null) = fs::metadata("/dev/null") else {
        return false;
    };
    let is_null = |metadata: &fs::Metadata| {
        metadata.file_type().is_char_device() && metadata.rdev() == null.rdev()
    };
    // Crashpad can keep a diagnostic pipe on stderr after double-forking.
    // Require null input/output and reject any retained terminal descriptor,
    // including descriptors beyond stdio. Unreadable evidence still blocks.
    for fd in ["0", "1"] {
        if !fs::metadata(process.join("fd").join(fd)).is_ok_and(|metadata| is_null(&metadata)) {
            return false;
        }
    }
    let Ok(descriptors) = fs::read_dir(process.join("fd")) else {
        return false;
    };
    descriptors.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            fs::metadata(entry.path())
                .is_ok_and(|metadata| !metadata.file_type().is_char_device() || is_null(&metadata))
        })
    })
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

    /// Legacy relaunches use 1, carried terminals 2, retained native owners 3,
    /// and consent that forbids carried recipe fallback 4.
    const VERSION: u16 = 4;
    const VERSIONS: std::ops::RangeInclusive<u16> = 1..=VERSION;
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
        #[serde(default)]
        pub relaunches: Vec<crate::terminal_ipc::UpgradeRelaunch>,
        pub recipe: LaunchRecipe,
        pub options: TerminalSpawnOptions,
    }

    /// A terminal left running under its holder for the next daemon to adopt,
    /// processes, screen and all. Its launch is unchanged, so its lineage
    /// records the carry as a source whose successor launched the same way.
    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct Carried {
        pub source: TerminalSession,
        /// Oldest first, as for [`Terminal`].
        #[serde(default)]
        pub previous_sources: Vec<TerminalSession>,
        #[serde(default)]
        pub relaunches: Vec<crate::terminal_ipc::UpgradeRelaunch>,
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
        /// Provider-confirmed continuation. Durable native aliases all retain
        /// the same live thread; ordinary restarts use the next daemon's launch.
        pub launch: chat::Launch,
    }

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct Checkpoint {
        /// Read, and checked, but written by [`Store::save`] from what the
        /// checkpoint holds.
        #[serde(skip_serializing)]
        version: u16,
        pub proof: HostSessionProof,
        pub phase: Phase,
        pub terminals: Vec<Terminal>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub carried: Vec<Carried>,
        /// These carried processes were promised continuity, not permission to
        /// execute their recipes if authenticated adoption becomes unavailable.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub preserve_carried: bool,
        pub chats: Vec<Chat>,
    }

    #[cfg(any(feature = "host", test))]
    impl Checkpoint {
        pub fn new(
            proof: HostSessionProof,
            terminals: Vec<Terminal>,
            carried: Vec<Carried>,
            chats: Vec<Chat>,
        ) -> Self {
            Self {
                version: VERSION,
                proof,
                phase: Phase::Prepared,
                terminals,
                carried,
                preserve_carried: false,
                chats,
            }
        }

        /// The oldest version that can hold this checkpoint.
        fn format(&self) -> u16 {
            if self.preserve_carried {
                // Older readers may relaunch a missing carried holder.
                4
            } else if self.chats.iter().any(|chat| {
                matches!(
                    chat.source.native_continuity,
                    chat::NativeContinuity::Durable { .. }
                )
            }) {
                // Older readers would ignore this metadata and relaunch a
                // recipe while its native holder still owns the server.
                3
            } else if self.carried.is_empty() {
                1
            } else {
                2
            }
        }

        pub fn validate(&self) -> Result<(), String> {
            if !VERSIONS.contains(&self.version)
                || self.format() > self.version
                || !self.proof.is_complete()
            {
                return Err(
                    "Unsupported or incomplete upgrade checkpoint; checkpoint retained".into(),
                );
            }
            if self.terminals.len() + self.carried.len() > MAX_TERMINALS
                || self.chats.len() > MAX_CHATS
            {
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
            // Each source daemon either relaunched its terminal from the
            // source's safe recipe, or carried it, launched as it was.
            let lineage = |source: &TerminalSession,
                           previous_sources: &[TerminalSession],
                           relaunches: &[crate::terminal_ipc::UpgradeRelaunch]|
             -> Result<(), String> {
                if source.continuity.live_proof().as_ref() != Some(&self.proof) {
                    return Err("Upgrade terminal source is inconsistent".into());
                }
                let mut proofs = Vec::new();
                for (previous, next) in previous_sources.iter().zip(
                    previous_sources
                        .iter()
                        .skip(1)
                        .chain(std::iter::once(source)),
                ) {
                    source_proof(previous.continuity.live_proof(), &mut proofs)?;
                    if previous.id != source.id
                        || (previous.launch != next.launch
                            && checkpoint_recipe(previous, relaunches)? != next.launch)
                    {
                        return Err("Upgrade terminal lineage is inconsistent".into());
                    }
                }
                source_proof(source.continuity.live_proof(), &mut proofs)?;
                for (index, accepted) in relaunches.iter().enumerate() {
                    if (accepted.source != *source && !previous_sources.contains(&accepted.source))
                        || relaunches[..index]
                            .iter()
                            .any(|old| old.source == accepted.source)
                    {
                        return Err(
                            "Relaunch acceptance does not identify a unique checkpoint source"
                                .into(),
                        );
                    }
                }
                Ok(())
            };
            for terminal in &self.terminals {
                // An interrupted setup may be present in legacy checkpoint
                // evidence even though its command is deliberately not safe
                // to replay. Accept only its unchanged recipe; restore retains
                // it for review without ever executing it.
                let recipe_matches = if is_worktree_setup_recipe(&terminal.source.launch) {
                    terminal.source.launch == terminal.recipe
                } else {
                    checkpoint_recipe(&terminal.source, &terminal.relaunches)? == terminal.recipe
                };
                if !ids.insert(terminal.source.id) || !recipe_matches
                {
                    return Err("Upgrade terminal source or safe recipe is inconsistent".into());
                }
                lineage(
                    &terminal.source,
                    &terminal.previous_sources,
                    &terminal.relaunches,
                )?;
                if let Some((provider, id)) = exact_conversation(&terminal.recipe)
                    && !conversations.insert((provider.executable(), id))
                {
                    return Err("Upgrade checkpoint contains duplicate conversation writers".into());
                }
            }
            for carried in &self.carried {
                if !ids.insert(carried.source.id) {
                    return Err("Upgrade terminal source is inconsistent".into());
                }
                lineage(
                    &carried.source,
                    &carried.previous_sources,
                    &carried.relaunches,
                )?;
                // A carried agent is still the writer of its conversation,
                // whether it was launched into it or was last seen in it.
                let observed = carried
                    .source
                    .resume_hint
                    .as_deref()
                    .and_then(crate::model::agent_recovery::recipe_from_hint);
                let mut writes = [Some(&carried.source.launch), observed.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter_map(exact_conversation)
                    .map(|(provider, id)| (provider.executable(), id))
                    .collect::<Vec<_>>();
                writes.dedup();
                if writes
                    .into_iter()
                    .any(|conversation| !conversations.insert(conversation))
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
                let durable = matches!(
                    chat.source.native_continuity,
                    crate::chat::NativeContinuity::Durable { .. }
                );
                let mut aliases = Vec::new();
                for (previous, next) in chat.previous_sources.iter().zip(
                    chat.previous_sources
                        .iter()
                        .skip(1)
                        .map(|previous| &previous.source)
                        .chain(std::iter::once(&chat.source)),
                ) {
                    if durable {
                        // A retained native owner can issue several descriptors
                        // under one daemon proof (thread confirmation, paired
                        // view publication). These are exact aliases, not a
                        // sequence of provider relaunches.
                        if previous.source.proof.as_ref().is_none_or(|proof| {
                            !proof.is_complete() || proof.boot_id != self.proof.boot_id
                        }) || aliases.contains(&&previous.source)
                            || previous.source == chat.source
                            || previous
                                .source
                                .launch
                                .session_id
                                .as_ref()
                                .is_some_and(|id| Some(id) != chat.launch.session_id.as_ref())
                        {
                            return Err("Upgrade native alias ownership is inconsistent".into());
                        }
                        aliases.push(&previous.source);
                    } else {
                        source_proof(previous.source.proof.clone(), &mut proofs)?;
                    }
                    validate_chat_source(&previous.source, &previous.launch)?;
                    let follows = if durable {
                        previous.launch == chat.launch
                    } else {
                        // Restart argv is normalized, while the successor's
                        // issued descriptor retains the immutable source recipe.
                        // Bind both to the confirmed identity before comparing
                        // the complete executable, argv, cwd and environment.
                        previous.launch.provider == next.launch.provider
                            && previous.launch.session_id == next.launch.session_id
                            && previous
                                .launch
                                .terminal_recipe()
                                .is_some_and(|recipe| Some(recipe) == next.launch.terminal_recipe())
                    };
                    if previous.source.id != chat.source.id || !follows {
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
        if !source.has_valid_native_continuity() {
            return Err("Upgrade native continuity evidence is inconsistent".into());
        }
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

    /// The durable record of retained native conversations that an upgrade
    /// restore could not reattach. The daemon started without them: each
    /// provider holder was left as it was, nothing was relaunched or resent,
    /// and the exact checkpoint entry is kept here as evidence for recovery.
    const UNATTACHED_FILE: &str = "daemon-upgrade.unattached.json";
    const UNATTACHED_VERSION: u16 = 1;

    /// What any client may read about an unattached conversation. The host
    /// writes the exact evidence beside it; readers ignore that payload.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
    pub(crate) struct UnattachedSummary {
        pub id: crate::model::TerminalId,
        pub provider: crate::agent::Provider,
        /// The provider's confirmed conversation (Codex thread) identity.
        pub conversation: Option<String>,
        pub working_directory: PathBuf,
        /// Why reattachment was refused, as the daemon reported it.
        pub reason: String,
    }

    #[cfg(any(feature = "host", test))]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(crate) struct UnattachedChat {
        #[serde(flatten)]
        pub summary: UnattachedSummary,
        /// The checkpoint entry exactly as the previous daemon issued it.
        pub evidence: Chat,
        /// The conversation's paired terminal, when the checkpoint asked for it
        /// to be relaunched. Relaunching it would attach an unowned client to
        /// the retained server, so it is kept here instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub terminal: Option<Terminal>,
    }

    #[cfg(any(feature = "host", test))]
    impl UnattachedChat {
        pub fn new(evidence: Chat, reason: String) -> Self {
            const REASON_BYTES: usize = 4096;
            let reason = reason[..reason.floor_char_boundary(REASON_BYTES)].to_owned();
            Self {
                summary: UnattachedSummary {
                    id: evidence.source.id,
                    provider: evidence.launch.provider,
                    conversation: evidence.launch.session_id.clone(),
                    working_directory: evidence.launch.recipe.working_directory.clone(),
                    reason,
                },
                evidence,
                terminal: None,
            }
        }
    }

    #[derive(Deserialize)]
    #[cfg_attr(any(feature = "host", test), derive(Serialize))]
    struct UnattachedReport<T> {
        version: u16,
        conversations: T,
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
        #[serde(default, deserialize_with = "count_items")]
        carried: usize,
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
        /// Serializes this daemon's read-modify-write updates of the
        /// unattached record; only the lease-holding daemon writes it.
        #[cfg(any(feature = "host", test))]
        unattached: std::sync::Mutex<()>,
    }

    impl Store {
        pub fn discover() -> Result<Self, String> {
            let dirs =
                crate::instance::project_dirs().ok_or("Cannot discover durable upgrade storage")?;
            Self::open(&dirs.data_local_dir().join("daemon-upgrade"))
        }

        /// Opens the durable storage only if some daemon already created it,
        /// so read-only callers never create it.
        pub fn existing() -> Result<Option<Self>, String> {
            let dirs =
                crate::instance::project_dirs().ok_or("Cannot discover durable upgrade storage")?;
            let directory = dirs.data_local_dir().join("daemon-upgrade");
            match std::fs::symlink_metadata(&directory) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
                Ok(_) => Self::open(&directory).map(Some),
            }
        }

        pub fn open(directory: &Path) -> Result<Self, String> {
            crate::secure_file::prepare_private_directory(directory)
                .map_err(|error| error.to_string())?;
            let root = fs_safety::Root::open(directory).map_err(|error| error.to_string())?;
            Ok(Self {
                root,
                path: directory.join(FILE),
                #[cfg(any(feature = "host", test))]
                unattached: Default::default(),
            })
        }

        fn read_bytes(&self) -> Result<Option<Vec<u8>>, String> {
            self.read_named(FILE)
        }

        fn read_named(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
            let mut file = match fs_safety::open_beneath(&self.root, Path::new(name)) {
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
                return Err(format!(
                    "Unsafe upgrade evidence {}",
                    self.path.with_file_name(name).display()
                ));
            }
            let bytes =
                fs_safety::read_bounded(&mut file, MAX_BYTES).map_err(|error| error.to_string())?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("Upgrade evidence exceeds durable byte limit".into());
            }
            Ok(Some(bytes))
        }

        fn startup_timeout(&self) -> Result<std::time::Duration, String> {
            let Some(bytes) = self.read_bytes()? else {
                return Ok(std::time::Duration::from_secs(5));
            };
            let inventory: Inventory =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if !VERSIONS.contains(&inventory.version) {
                return Err("Unsupported upgrade checkpoint; checkpoint retained".into());
            }
            if inventory.terminals + inventory.carried > MAX_TERMINALS
                || inventory.chats > MAX_CHATS
            {
                return Err("Upgrade checkpoint exceeds session limits".into());
            }
            // Adopting a carried terminal is a local handshake with its holder.
            Ok(std::time::Duration::from_secs(
                5 + 30 * (inventory.terminals + inventory.chats) as u64
                    + 5 * inventory.carried as u64,
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
            #[derive(Serialize)]
            struct Versioned<'a> {
                version: u16,
                #[serde(flatten)]
                checkpoint: &'a Checkpoint,
            }
            let versioned = Versioned {
                version: checkpoint.format(),
                checkpoint,
            };
            let mut bytes = Bounded(Vec::new());
            serde_json::to_writer(&mut bytes, &versioned).map_err(|error| error.to_string())?;
            self.write_named(FILE, &bytes.0)
        }

        /// Atomically and durably replaces `name` with `bytes`.
        #[cfg(any(feature = "host", test))]
        fn write_named(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
            let (mut file, temporary) = fs_safety::create_temporary_in(&self.root, 0o600)
                .map_err(|error| error.to_string())?;
            let result = (|| {
                file.write_all(bytes).map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())?;
                fs_safety::rename_within(&self.root, &temporary, OsStr::new(name))
                    .map_err(|error| error.to_string())?;
                fs_safety::sync_directory(&self.root).map_err(|error| error.to_string())
            })();
            if result.is_err() {
                let _ = fs_safety::unlink_within(&self.root, &temporary);
            }
            result
        }

        /// Where retained conversations that could not be reattached are
        /// recorded; see [`Store::unattached`].
        pub fn unattached_path(&self) -> PathBuf {
            self.path.with_file_name(UNATTACHED_FILE)
        }

        /// Retained native conversations an upgrade restore left unattached
        /// that have not since been reattached or explicitly closed.
        pub fn unattached(&self) -> Result<Vec<UnattachedSummary>, String> {
            Ok(self
                .read_unattached::<UnattachedSummary>()?
                .unwrap_or_default())
        }

        fn read_unattached<T: serde::de::DeserializeOwned>(
            &self,
        ) -> Result<Option<Vec<T>>, String> {
            let Some(bytes) = self.read_named(UNATTACHED_FILE)? else {
                return Ok(None);
            };
            let report: UnattachedReport<Vec<T>> =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if report.version != UNATTACHED_VERSION {
                return Err(format!(
                    "Unsupported unattached conversation evidence {}",
                    self.unattached_path().display()
                ));
            }
            Ok(Some(report.conversations))
        }

        /// Adds conversations to the unattached record, replacing any earlier
        /// entry for the same conversation, and forgets `resolved` ones, in one
        /// durable replacement. The record stays within its count and byte
        /// bounds: the entries recorded longest ago give way first, because an
        /// oversized record must never make a restore fail.
        #[cfg(any(feature = "host", test))]
        pub fn record_unattached(
            &self,
            entries: &[UnattachedChat],
            resolved: &[crate::model::TerminalId],
        ) -> Result<(), String> {
            let _update = self.unattached.lock().unwrap_or_else(|p| p.into_inner());
            let mut conversations = match self.read_unattached::<UnattachedChat>() {
                Ok(None) if entries.is_empty() => return Ok(()),
                Ok(conversations) => conversations.unwrap_or_default(),
                // Evidence this release cannot read, such as a newer release's
                // before a rollback, must neither block the restore recording
                // its own nor be overwritten. Keep it intact beside the new one.
                Err(_) if entries.is_empty() => return Ok(()),
                Err(error) => {
                    let aside = format!(
                        "daemon-upgrade.unattached.{}.unreadable.json",
                        uuid::Uuid::new_v4()
                    );
                    fs_safety::rename_new_within(
                        &self.root,
                        OsStr::new(UNATTACHED_FILE),
                        OsStr::new(&aside),
                    )
                    .map_err(|rename| {
                        format!("Cannot set aside unreadable evidence ({error}): {rename}")
                    })?;
                    tracing::warn!(%error, aside, "set aside unattached-conversation evidence this release cannot read");
                    Vec::new()
                }
            };
            let before = conversations.len();
            conversations.retain(|old| {
                !resolved.contains(&old.summary.id)
                    && !entries
                        .iter()
                        .any(|entry| entry.summary.id == old.summary.id)
            });
            if entries.is_empty() && conversations.len() == before {
                return Ok(());
            }
            conversations.extend(entries.iter().cloned());
            let excess = conversations.len().saturating_sub(MAX_CHATS);
            conversations.drain(..excess);
            self.write_unattached(conversations)
        }

        /// Forgets a conversation that has since been reattached or closed,
        /// removing the record once nothing in it needs attention.
        #[cfg(any(feature = "host", test))]
        pub fn retire_unattached(&self, id: crate::model::TerminalId) -> Result<(), String> {
            let _update = self.unattached.lock().unwrap_or_else(|p| p.into_inner());
            let Some(mut conversations) = self.read_unattached::<UnattachedChat>()? else {
                return Ok(());
            };
            let before = conversations.len();
            conversations.retain(|entry| entry.summary.id != id);
            if conversations.len() == before {
                return Ok(());
            }
            self.write_unattached(conversations)
        }

        /// Durably replaces the record, or removes it when empty, evicting the
        /// oldest entries while it exceeds the durable byte limit.
        #[cfg(any(feature = "host", test))]
        fn write_unattached(&self, mut conversations: Vec<UnattachedChat>) -> Result<(), String> {
            loop {
                if conversations.is_empty() {
                    return match fs_safety::unlink_within(&self.root, OsStr::new(UNATTACHED_FILE)) {
                        Err(error) if error.is_not_found() => Ok(()),
                        result => result.map_err(|error| error.to_string()).and_then(|()| {
                            fs_safety::sync_directory(&self.root).map_err(|error| error.to_string())
                        }),
                    };
                }
                let bytes = serde_json::to_vec(&UnattachedReport {
                    version: UNATTACHED_VERSION,
                    conversations: &conversations,
                })
                .map_err(|error| error.to_string())?;
                if bytes.len() as u64 <= MAX_BYTES {
                    return self.write_named(UNATTACHED_FILE, &bytes);
                }
                let evicted = conversations.remove(0);
                tracing::warn!(session = %evicted.summary.id,
                    conversation = ?evicted.summary.conversation,
                    working_directory = %evicted.summary.working_directory.display(),
                    reason = %evicted.summary.reason,
                    "unattached-conversation evidence exceeded its byte limit; forgot the oldest entry");
            }
        }

        pub fn archive(&self) -> Result<PathBuf, String> {
            self.archive_as("daemon-upgrade.abandoned.json")
        }

        /// Preserve deferred setup evidence without leaving a replayable
        /// checkpoint in the daemon's boot path or replacing older evidence.
        #[cfg(feature = "host")]
        pub fn archive_interrupted_setup(&self) -> Result<PathBuf, String> {
            self.archive_as(&format!("daemon-upgrade.interrupted-setup.{}.json", uuid::Uuid::new_v4()))
        }

        fn archive_as(&self, archive: &str) -> Result<PathBuf, String> {
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
            fs_safety::rename_new_within(&self.root, OsStr::new(FILE), OsStr::new(archive))
                .map_err(|error| format!("Cannot archive checkpoint without replacing prior evidence: {error}. Preserve and explicitly move any existing {} first", self.path.with_file_name(archive).display()))?;
            fs_safety::sync_directory(&self.root).map_err(|error| error.to_string())?;
            Ok(self.path.with_file_name(archive))
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
            save(br#"{"version":2,"terminals":[{}],"carried":[{},{}],"chats":[]}"#);
            assert_eq!(
                store.startup_timeout().unwrap(),
                std::time::Duration::from_secs(45)
            );
            save(br#"{"version":3,"terminals":[],"chats":[]}"#);
            assert!(store.startup_timeout().is_ok());
            save(
                &serde_json::to_vec(&serde_json::json!({
                    "version": VERSION + 1,
                    "terminals": [],
                    "chats": [],
                }))
                .unwrap(),
            );
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
    use handoff::{Carried, Checkpoint, Phase, Store, Terminal};

    #[test]
    fn upgrades_relaunch_agents_unless_settings_say_otherwise() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        assert_eq!(relaunch_agents_by_default(&settings), Ok(true));
        // Other preferences, including tables, are none of the CLI's business.
        std::fs::write(
            &settings,
            "schema_version = 3\nshow_status_bar = false\n\n[keyboard_shortcuts]\nnew_terminal = \"Ctrl+T\"\n",
        )
        .unwrap();
        assert_eq!(relaunch_agents_by_default(&settings), Ok(true));
        std::fs::write(&settings, "relaunch_agents_on_upgrade = false\n").unwrap();
        assert_eq!(relaunch_agents_by_default(&settings), Ok(false));
        // A file that may be saying no is never read as yes.
        std::fs::write(&settings, "relaunch_agents_on_upgrade = \"no\"\n").unwrap();
        assert!(relaunch_agents_by_default(&settings).is_err());
        std::fs::write(&settings, "relaunch_agents_on_upgrade = false\n[").unwrap();
        assert!(relaunch_agents_by_default(&settings).is_err());
        assert!(relaunch_agents_by_default(directory.path()).is_err());
    }

    fn checkpoint(directory: &Path) -> Checkpoint {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("upgrade-source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let recipe = LaunchRecipe::shell(directory.to_owned()).unwrap();
        let source = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
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
                relaunches: Vec::new(),
                recipe,
                options: crate::terminal_ipc::TerminalSpawnOptions::default(),
            }],
            Vec::new(),
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
        // A recognised agent without exact identity reopens its picker, never
        // the newest conversation its latest-session shortcut would pick.
        source.launch.arguments = vec!["--continue".into()];
        assert_eq!(
            recovery_plan(source).unwrap(),
            (
                LaunchRecipe {
                    arguments: vec!["--resume".into()],
                    ..source.launch.clone()
                },
                RecoveryKind::Picker
            )
        );
        source.launch.arguments = vec!["repeat this prompt".into()];
        assert!(recovery_recipe(source).is_err());
    }

    #[test]
    fn shell_recovery_keeps_selected_provider_homes_without_accepting_other_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let mut checkpoint = checkpoint(directory.path());
        let source = &mut checkpoint.terminals[0].source;
        source.launch.environment.insert(
            "CODEX_HOME".into(),
            directory.path().join("codex").display().to_string(),
        );
        source.launch.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            directory.path().join("claude").display().to_string(),
        );
        assert_eq!(recovery_plan(source).unwrap().1, RecoveryKind::Shell);
        source
            .launch
            .environment
            .insert("PATH".into(), "/another/bin".into());
        assert!(recovery_plan(source).is_err());
    }

    #[test]
    fn agent_without_exact_identity_reopens_its_picker_from_the_best_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let mut checkpoint = checkpoint(directory.path());
        let source = &mut checkpoint.terminals[0].source;
        let shell = source.launch.clone();
        let claude = |arguments: &[&str]| LaunchRecipe {
            program: "claude".into(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            ..shell.clone()
        };
        // An agent observed inside the shell reopens as that agent.
        source.resume_hint = crate::model::agent_recovery::observed_hint_from_launch(&claude(&[
            "--model", "sonnet",
        ]));
        let (recipe, kind) = recovery_plan(source).unwrap();
        assert_eq!(kind, RecoveryKind::Picker);
        assert_eq!(recipe, claude(&["--model", "sonnet", "--resume"]));
        // An observation the picker cannot carry falls back to the terminal's
        // own agent command, and to its shell when that is all it ran.
        source.resume_hint =
            crate::model::agent_recovery::observed_hint_from_launch(&claude(&["--unknown"]));
        assert_eq!(
            recovery_plan(source).unwrap(),
            (shell.clone(), RecoveryKind::Shell)
        );
        source.launch = claude(&["--bare"]);
        assert_eq!(
            recovery_plan(source).unwrap(),
            (claude(&["--bare", "--resume"]), RecoveryKind::Picker)
        );
        // An exact identity always wins over the picker.
        source.launch = claude(&["--resume", "12345678-1234-4234-8234-123456789abc"]);
        assert_eq!(recovery_plan(source).unwrap().1, RecoveryKind::Exact);
    }

    #[test]
    fn relaunch_acceptance_is_explicit_source_bound_and_never_replays_a_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let mut checkpoint = checkpoint(directory.path());
        let terminal = &mut checkpoint.terminals[0];
        terminal.source.launch.program = "claude".into();
        terminal.source.launch.arguments = Vec::new();
        terminal.recipe = terminal.source.launch.clone();
        assert!(checkpoint.validate().is_err());
        let terminal = &mut checkpoint.terminals[0];
        terminal
            .relaunches
            .push(crate::terminal_ipc::UpgradeRelaunch {
                source: terminal.source.clone(),
                recipe: terminal.recipe.clone(),
            });
        checkpoint.validate().unwrap();
        let mut forged = checkpoint.clone();
        forged.terminals[0].relaunches[0].source.id = TerminalId::new();
        assert!(forged.validate().is_err());
        let mut prompt = checkpoint.clone();
        prompt.terminals[0].relaunches[0].recipe.arguments = vec!["run this again".into()];
        assert!(prompt.validate().is_err());
        let mut duplicate = checkpoint.clone();
        let accepted = duplicate.terminals[0].relaunches[0].clone();
        duplicate.terminals[0].relaunches.push(accepted);
        assert!(duplicate.validate().is_err());
        let mut picker = checkpoint.clone();
        picker.terminals[0].relaunches[0].recipe.arguments = vec!["--resume".into()];
        picker.terminals[0].recipe = picker.terminals[0].relaunches[0].recipe.clone();
        picker.validate().unwrap();
        let mut exact = picker.clone();
        exact.terminals[0].relaunches[0]
            .recipe
            .arguments
            .push("12345678-1234-4234-8234-123456789abc".into());
        exact.terminals[0].recipe = exact.terminals[0].relaunches[0].recipe.clone();
        assert!(exact.validate().is_err());
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn detached_browser_helpers_do_not_own_inherited_terminal_markers() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let mut source = checkpoint(directory.path()).terminals.remove(0).source;
        let process = directory.path().join("42");
        fs::create_dir_all(process.join("fd")).unwrap();
        // A double-forked crash handler need not be its own session leader.
        fs::write(process.join("stat"), "42 (helper) S 1 41 41 0").unwrap();
        fs::write(process.join("cmdline"), b"browser-helper\0").unwrap();
        for fd in ["0", "1", "2"] {
            symlink("/dev/null", process.join("fd").join(fd)).unwrap();
        }
        for marker in ["CAGENTS_TERMINAL_ID", "CAGENTS_RECOVERY_ORIGIN"] {
            fs::write(process.join("environ"), format!("{marker}={}\0", source.id)).unwrap();
            for executable in [
                "agent-browser-linux-x64",
                "agent-browser-linux-arm64",
                "agent-browser-linux-musl-x64",
                "agent-browser-linux-musl-arm64",
                "chrome_crashpad_handler",
                "chrome_crashpad_handler (deleted)",
            ] {
                symlink(Path::new("/tools").join(executable), process.join("exe")).unwrap();
                assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());
                fs::remove_file(process.join("exe")).unwrap();
            }
        }

        // Unknown detached writers, including providers and custom wrappers,
        // retain protection. argv alone must not grant the helper exception.
        fs::write(process.join("cmdline"), b"agent-browser-linux-x64\0").unwrap();
        for executable in ["codex", "claude", "omp", "custom-agent"] {
            symlink(Path::new("/tools").join(executable), process.join("exe")).unwrap();
            assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
            fs::remove_file(process.join("exe")).unwrap();
        }
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
        symlink("/tools/chrome_crashpad_handler", process.join("exe")).unwrap();

        // A diagnostic pipe is normal for detached crash handlers.
        let (reader, _writer) = std::io::pipe().unwrap();
        use std::os::fd::AsRawFd;
        fs::remove_file(process.join("fd/2")).unwrap();
        symlink(
            format!("/proc/self/fd/{}", reader.as_raw_fd()),
            process.join("fd/2"),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());

        fs::write(process.join("stat"), "42 (helper) S 1 41 41 34816").unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
        fs::write(process.join("stat"), "42 (helper) S 1 41 41 0").unwrap();
        // Retaining a terminal on any descriptor must prevent the exemption.
        symlink("/dev/ptmx", process.join("fd/7")).unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
        fs::remove_file(process.join("fd/7")).unwrap();
        fs::remove_file(process.join("fd/0")).unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
        symlink("/dev/null", process.join("fd/0")).unwrap();

        // An exact conversation selector is stronger than helper evidence.
        let id = "12345678-1234-4234-8234-123456789abc";
        source.launch.program = "codex".into();
        source.launch.arguments = vec!["resume".into(), id.into()];
        fs::write(process.join("cmdline"), format!("browser-helper\0{id}\0")).unwrap();
        let error = ensure_no_surviving_process(&source, u32::MAX, directory.path()).unwrap_err();
        assert!(error.contains("agent session still has a live process"));
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

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn exiting_namespace_init_does_not_hide_a_surviving_descendant() {
        let directory = tempfile::tempdir().unwrap();
        let source = checkpoint(directory.path()).terminals.remove(0).source;
        let parent = directory.path().join("42");
        fs::create_dir(&parent).unwrap();
        let mut fields = ["0"; 20];
        fields[0] = "S";
        fields[1] = "1";
        fields[6] = "4";
        fields[17] = "1";
        fields[19] = "123";
        let stat = |fields: &[&str]| format!("42 (namespace init) {}", fields.join(" "));
        fs::write(parent.join("stat"), stat(&fields)).unwrap();
        fs::write(parent.join("cmdline"), b"").unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());

        let child = directory.path().join("43");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("stat"), "43 (writer) S 42").unwrap();
        fs::write(child.join("cmdline"), b"/bin/fish\0").unwrap();
        fs::write(
            child.join("environ"),
            format!("CAGENTS_TERMINAL_ID={}\0", source.id),
        )
        .unwrap();
        assert!(
            ensure_no_surviving_process(&source, u32::MAX, directory.path())
                .unwrap_err()
                .contains("live process (43)")
        );
        fs::remove_dir_all(child).unwrap();
        fields[17] = "2";
        fs::write(parent.join("stat"), stat(&fields)).unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
        fields[17] = "1";
        fields[6] = "0";
        fs::write(parent.join("stat"), stat(&fields)).unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn recovery_ignores_unrelated_terminals_even_after_their_daemon_disappears() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let source = checkpoint(directory.path()).terminals.remove(0).source;
        let shell = directory.path().join("42");
        let daemon = directory.path().join("43");
        fs::create_dir(&shell).unwrap();
        fs::create_dir_all(daemon.join("fd")).unwrap();
        fs::write(shell.join("stat"), "42 (fish) S 43").unwrap();
        fs::write(shell.join("cmdline"), b"fish\0").unwrap();
        fs::write(
            shell.join("environ"),
            b"TERM_PROGRAM=cagents\0CAGENTS_TERMINAL_ID=other-terminal\0",
        )
        .unwrap();
        fs::write(daemon.join("stat"), "43 (cagentsd) S 1").unwrap();
        fs::write(daemon.join("cmdline"), b"cagentsd\0").unwrap();
        fs::write(daemon.join("environ"), b"PATH=/bin\0").unwrap();
        symlink(
            directory.path().join("other-instance/cagentsd.lock"),
            daemon.join("fd/7"),
        )
        .unwrap();
        for name in ["cagentsd", "cagentsd (deleted)"] {
            symlink(Path::new("/build/bin").join(name), daemon.join("exe")).unwrap();
            assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());
            fs::remove_file(daemon.join("exe")).unwrap();
        }
        symlink("/build/bin/cagentsd-helper (deleted)", daemon.join("exe")).unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());
        fs::write(shell.join("environ"), b"TERM_PROGRAM=cagents\0").unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_ok());
        // Exact terminal evidence still blocks duplication.
        fs::remove_file(daemon.join("exe")).unwrap();
        symlink("/build/bin/cagentsd (deleted)", daemon.join("exe")).unwrap();
        fs::write(
            shell.join("environ"),
            format!("CAGENTS_TERMINAL_ID={}\0", source.id),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&source, u32::MAX, directory.path()).is_err());
    }

    #[test]
    fn retained_native_checkpoint_requires_a_holder_aware_reader() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut checkpoint = checkpoint(directory.path());
        checkpoint.terminals.clear();
        let mut source = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec!["--remote".into(), "unix://".into()],
                    working_directory: directory.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(checkpoint.proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        assert!(source.claim_owned_codex_listener(directory.path(), &checkpoint.proof));
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        source.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "0.157.1".into(),
        };
        checkpoint.chats.push(handoff::Chat {
            launch: source.launch.clone(),
            source,
            previous_sources: Vec::new(),
        });
        store.save(&checkpoint).unwrap();
        let mut saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        assert_eq!(saved["version"], 3);
        store.load().unwrap().unwrap().validate().unwrap();
        saved["version"] = serde_json::json!(2);
        fs::write(&store.path, serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(
            store.load().is_err(),
            "durable ownership cannot masquerade as recipe-only recovery"
        );
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

    fn carry(terminal: Terminal) -> Carried {
        Carried {
            source: terminal.source,
            previous_sources: terminal.previous_sources,
            relaunches: terminal.relaunches,
            options: terminal.options,
        }
    }

    #[test]
    fn only_a_checkpoint_that_carries_terminals_needs_the_newer_format() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let written = || -> serde_json::Value {
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap()
        };
        // A daemon from before carrying can still replay what does not carry.
        let mut checkpoint = checkpoint(directory.path());
        store.save(&checkpoint).unwrap();
        assert_eq!(written()["version"], 1);
        assert!(written().get("carried").is_none());
        store.load().unwrap().unwrap().validate().unwrap();

        let terminal = checkpoint.terminals.pop().unwrap();
        checkpoint.carried.push(carry(terminal));
        store.save(&checkpoint).unwrap();
        assert_eq!(written()["version"], 2);
        let loaded = store.load().unwrap().unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.carried[0].source, checkpoint.carried[0].source);
        // Carried terminals never pass as an older format that ignores them.
        let mut mislabelled = written();
        mislabelled["version"] = 1.into();
        fs::write(&store.path, serde_json::to_vec(&mislabelled).unwrap()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn carried_terminals_keep_one_identity_one_writer_and_a_consistent_lineage() {
        let directory = tempfile::tempdir().unwrap();
        let mut checkpoint = checkpoint(directory.path());
        let relaunched = checkpoint.terminals[0].clone();

        // One terminal cannot be both relaunched and carried.
        checkpoint.carried.push(carry(relaunched.clone()));
        assert!(checkpoint.validate().is_err());

        // A terminal carried once before launched exactly as its source did;
        // one relaunched before launched the source's safe recipe.
        let mut carried = carry(relaunched.clone());
        carried.source.id = TerminalId::new();
        let mut earlier = carried.source.clone();
        earlier.continuity = TerminalContinuity::DetachedSameBoot {
            host_session_id: HostSessionId::new("earlier-daemon"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("earlier"),
        };
        carried.previous_sources.push(earlier);
        checkpoint.carried[0] = carried.clone();
        checkpoint.validate().unwrap();
        let mut forged = checkpoint.clone();
        forged.carried[0].previous_sources[0].launch.arguments = vec!["-c".into(), "rm".into()];
        assert!(forged.validate().is_err());
        let mut foreign = checkpoint.clone();
        foreign.carried[0].source.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: carried.source.launch.clone(),
            last_exit: None,
        };
        assert!(foreign.validate().is_err());

        // A carried agent still writes its conversation, so nothing restarts
        // another writer of it.
        let exact = LaunchRecipe {
            program: "claude".into(),
            arguments: vec![
                "--resume".into(),
                "12345678-1234-4234-8234-123456789abc".into(),
            ],
            ..relaunched.recipe.clone()
        };
        let mut writers = checkpoint.clone();
        writers.carried[0].previous_sources.clear();
        writers.carried[0].source.launch = exact.clone();
        writers.validate().unwrap();
        let mut observed = checkpoint.clone();
        observed.carried[0].previous_sources.clear();
        observed.carried[0].source.resume_hint =
            crate::model::agent_recovery::hint_from_launch(&exact);
        observed.validate().unwrap();
        observed.terminals[0].source.launch = exact.clone();
        observed.terminals[0].recipe = recovery_recipe(&observed.terminals[0].source).unwrap();
        assert!(observed.validate().is_err());
        writers.terminals[0].source.launch = exact.clone();
        writers.terminals[0].recipe = recovery_recipe(&writers.terminals[0].source).unwrap();
        assert!(writers.validate().is_err());
    }

    #[test]
    fn unattached_conversations_are_recorded_exactly_bounded_and_retired() {
        use handoff::{Chat, UnattachedChat};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        assert!(store.unattached().unwrap().is_empty());
        store.retire_unattached(TerminalId::new()).unwrap();
        let unattached = |reason: &str| {
            let source = crate::chat::Session {
                id: TerminalId::new(),
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Codex,
                    recipe: LaunchRecipe {
                        program: "codex".into(),
                        arguments: vec!["resume".into()],
                        working_directory: directory.path().into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                    session_id: Some(uuid::Uuid::new_v4().to_string()),
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: crate::chat::NativeContinuity::Durable {
                    provider_version: "0.157.1".into(),
                },
            };
            UnattachedChat::new(
                Chat {
                    launch: source.launch.clone(),
                    source,
                    previous_sources: Vec::new(),
                },
                reason.into(),
            )
        };
        let first = unattached("no rollout found for thread id");
        let second = unattached(&"é".repeat(4096));
        assert_eq!(second.summary.reason.len(), 4096, "reasons are bounded");
        store
            .record_unattached(&[first.clone(), second.clone()], &[])
            .unwrap();
        assert_eq!(
            fs::metadata(store.unattached_path()).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            store.unattached().unwrap(),
            vec![first.summary.clone(), second.summary.clone()]
        );
        assert_eq!(
            first.summary.conversation, first.evidence.launch.session_id,
            "the confirmed conversation is named"
        );
        // A later restore of the same conversation replaces its entry.
        let mut again = first.clone();
        again.summary.reason = "The durable native holder is unavailable".into();
        store
            .record_unattached(std::slice::from_ref(&again), &[])
            .unwrap();
        assert_eq!(
            store.unattached().unwrap(),
            vec![second.summary.clone(), again.summary.clone()]
        );
        // Readers need only the summary, whatever evidence a newer daemon keeps.
        let exact = fs::read(store.unattached_path()).unwrap();
        let mut report: serde_json::Value = serde_json::from_slice(&exact).unwrap();
        report["conversations"][0]["evidence"] = serde_json::json!({ "future": [1, 2] });
        fs::write(
            store.unattached_path(),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        assert_eq!(store.unattached().unwrap()[0], second.summary);
        // The daemon never rewrites evidence it cannot read exactly.
        assert!(store.retire_unattached(second.summary.id).is_err());
        fs::write(store.unattached_path(), exact).unwrap();
        store.retire_unattached(second.summary.id).unwrap();
        assert_eq!(store.unattached().unwrap(), vec![again.summary.clone()]);
        // The record stays bounded, giving way oldest first.
        let many = (0..130)
            .map(|_| unattached("unavailable"))
            .collect::<Vec<_>>();
        store.record_unattached(&many, &[]).unwrap();
        let listed = store.unattached().unwrap();
        assert_eq!(listed.len(), 128);
        assert_eq!(listed[0], many[2].summary);
        // Resolution and new refusals land in one replacement.
        let resolved = many[2..]
            .iter()
            .map(|entry| entry.summary.id)
            .collect::<Vec<_>>();
        store
            .record_unattached(std::slice::from_ref(&second), &resolved)
            .unwrap();
        assert_eq!(store.unattached().unwrap(), vec![second.summary.clone()]);
        store.record_unattached(&[], &[second.summary.id]).unwrap();
        assert!(
            !store.unattached_path().exists(),
            "nothing left needs attention"
        );
        // An oversized record gives way oldest first rather than failing.
        let bulky = |reason: &str| {
            let mut entry = unattached(reason);
            entry.evidence.launch.recipe.environment =
                [("PAYLOAD".into(), "x".repeat(3 * 1024 * 1024))].into();
            entry
        };
        let (old, newer, newest) = (bulky("old"), bulky("newer"), bulky("newest"));
        store.record_unattached(&[old, newer.clone()], &[]).unwrap();
        store
            .record_unattached(std::slice::from_ref(&newest), &[])
            .unwrap();
        assert_eq!(
            store.unattached().unwrap(),
            vec![newer.summary.clone(), newest.summary.clone()]
        );
        store
            .record_unattached(&[], &[newer.summary.id, newest.summary.id])
            .unwrap();
        assert!(!store.unattached_path().exists());
        // Unreadable evidence is an error, never an empty report.
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(store.unattached_path())
            .unwrap()
            .write_all(br#"{"version":2,"conversations":[]}"#)
            .unwrap();
        let error = store.unattached().unwrap_err();
        assert!(error.contains("Unsupported"), "{error}");
        // Recording never waits on, or overwrites, evidence it cannot read.
        store.record_unattached(&[], &[first.summary.id]).unwrap();
        assert!(
            store.unattached().is_err(),
            "resolving alone never sets evidence aside"
        );
        store
            .record_unattached(std::slice::from_ref(&first), &[])
            .unwrap();
        assert_eq!(store.unattached().unwrap(), vec![first.summary.clone()]);
        let aside = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.to_string_lossy().ends_with(".unreadable.json"))
            .collect::<Vec<_>>();
        assert_eq!(aside.len(), 1);
        assert_eq!(
            fs::read(&aside[0]).unwrap(),
            br#"{"version":2,"conversations":[]}"#
        );
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
