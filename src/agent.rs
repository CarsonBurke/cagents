//! Provider identity and conventions shared by terminal recovery and native chat.
//! Recognition and view activity never authorize conversation recovery.
use crate::model::LaunchRecipe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod claude;
// The window reads a conversation's opening to name its channel; the daemon
// reads whole transcripts to resume and observe them.
#[cfg(any(feature = "ui", feature = "host"))]
pub use claude::transcript as claude_transcript;
#[cfg(feature = "host")]
pub(crate) use claude::{
    TranscriptError as ClaudeTranscriptError, transcript_exists as claude_transcript_exists,
    transcript_for_start as claude_transcript_for_start,
};
mod codex;
#[cfg(feature = "host")]
pub(crate) use codex::archived_rollout as codex_archived_rollout;
#[cfg(any(feature = "ui", feature = "host"))]
pub use codex::rollout as codex_rollout;
mod antigravity;
#[cfg(feature = "host")]
pub(crate) use antigravity::MODES as ANTIGRAVITY_MODES;
mod muse;
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) use muse::offers_model as muse_offers_model;
mod omp;

#[cfg(feature = "host")]
pub(crate) mod observe;

/// Where Omp keeps its data - its sessions and its agent database - for
/// `launch`, resolved as Omp resolves it: its profile, its configuration
/// directory, `PI_CODING_AGENT_DIR` and `XDG_DATA_HOME`.
pub fn omp_data_root(launch: &LaunchRecipe, home: &Path) -> Option<PathBuf> {
    omp::data_root(launch, home)
}

// Declaration order is persisted by the binary IPC codec and indexes ADAPTERS.
// Append new providers, and give each the protocol that added it in
// `Provider::since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum Provider {
    Omp,
    Claude,
    Codex,
    Antigravity,
    Muse,
}

/// Encoded as the derived implementation would, except to a client from
/// before the provider existed, which has no encoding for it: there the value
/// refuses to encode, and the daemon refuses or withholds the message holding
/// it rather than send that client something it cannot decode. An optional
/// value marked with [`absent_where_unknown`] reaches that client as absent
/// instead.
impl Serialize for Provider {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.is_encodable() {
            return Err(serde::ser::Error::custom(format!(
                "a client before protocol {} cannot name {}",
                self.since().unwrap_or_default(),
                self.label()
            )));
        }
        let name = match self {
            Self::Omp => "Omp",
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Muse => "Muse",
        };
        serializer.serialize_unit_variant("Provider", *self as u32, name)
    }
}

/// A value that names a provider.
pub(crate) trait NamesProvider {
    fn provider(&self) -> Provider;
}

impl NamesProvider for Provider {
    fn provider(&self) -> Provider {
        *self
    }
}

impl NamesProvider for Activity {
    fn provider(&self) -> Provider {
        self.provider
    }
}

/// Serializes an optional value naming a provider as absent for a client from
/// before that provider, which could not decode it, so the rest of the message
/// still reaches that client. Use with `#[serde(serialize_with)]` where absence
/// is something the client already understands.
pub(crate) fn absent_where_unknown<T, S>(
    value: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: NamesProvider + Serialize,
    S: serde::Serializer,
{
    value
        .as_ref()
        .filter(|value| value.provider().is_encodable())
        .serialize(serializer)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ActivityState {
    #[default]
    Unknown,
    Idle,
    Working,
    /// Waiting on the user for something its provider does not name, such as
    /// a terminal agent whose status says only that it waits.
    Waiting,
    /// Waiting for the user to allow or refuse an action.
    Approval,
    /// Waiting for the user to answer a question.
    Question,
}

impl ActivityState {
    /// Whether the agent is held up until the user does something, whatever
    /// the provider says it waits for.
    pub const fn waits_on_user(self) -> bool {
        matches!(self, Self::Waiting | Self::Approval | Self::Question)
    }

    /// The state in a sentence's words, as the CLI lists it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Approval => "waiting for approval",
            Self::Question => "waiting for an answer",
        }
    }
}

/// As derived, except that a client from before the kinds of wait were named
/// is sent `Waiting`, which is what it was told before.
impl Serialize for ActivityState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let state = match self {
            Self::Approval | Self::Question
                if crate::terminal_ipc::encoding_before(crate::terminal_ipc::WAIT_KINDS) =>
            {
                Self::Waiting
            }
            state => *state,
        };
        let name = match state {
            Self::Unknown => "Unknown",
            Self::Idle => "Idle",
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Approval => "Approval",
            Self::Question => "Question",
        };
        serializer.serialize_unit_variant("ActivityState", state as u32, name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub provider: Provider,
    pub state: ActivityState,
}

struct ProviderAdapter {
    provider: Provider,
    executable: &'static str,
    /// Other executable names that run this provider, such as the launcher a
    /// wrapper script named `executable` execs.
    aliases: &'static [&'static str],
    label: &'static str,
    recognizes_script: fn(&Path) -> bool,
    /// Whether a launch whose first argument is this runs something other
    /// than an interactive conversation: a server, a tool command or account
    /// management. A terminal signing in is not an agent.
    background_command: fn(&str) -> bool,
    session_file_id: fn(&str) -> Option<&str>,
    session_root: fn(&LaunchRecipe, &Path) -> Option<PathBuf>,
    #[cfg(feature = "host")]
    session_directory: &'static str,
    activity_from_title: fn(&str) -> Option<ActivityState>,
    identity_options: &'static [&'static str],
    /// The variables the provider sets for the processes one of its sessions
    /// starts: that session's identity, its control sockets and its process.
    /// They describe the parent, so a process cagents starts never takes
    /// them, wherever it was started from.
    session_environment: fn(&str) -> bool,
    cwd_options: &'static [&'static str],
    value_options: &'static [&'static str],
    switches: &'static [&'static str],
    resume_flag: &'static str,
    resume_precedes_options: bool,
    /// Whether `resume_flag` alone opens the provider's conversation picker.
    resume_picker: bool,
    /// Selectors that continue the most recent conversation rather than
    /// naming one, which neither a bound identity nor a picker keeps.
    latest_selectors: &'static [&'static str],
    /// Whether cagents can run this provider's conversations as native chat.
    /// Without it the provider runs only in a terminal.
    native_chat: bool,
    /// The permission modes an agent cagents starts may be given, by the
    /// names `agent spawn --permission-mode` and automations take: the
    /// provider's own for Claude, Codex and OMP, and for an ACP agent the
    /// cagents name of each of its session modes.
    permission_modes: &'static [&'static str],
    /// Other names the provider's own terminal gives the same modes, which a
    /// launch or a saved automation may use instead.
    permission_mode_aliases: &'static [&'static str],
    /// Sandboxes a mode may name after its approval policy and a `/`, as in
    /// Codex's `on-request/read-only`.
    permission_sandboxes: &'static [&'static str],
    /// Whether its native chat asks for tool approvals as ordinary questions,
    /// so answering one of its questions may approve a tool.
    approves_through_questions: bool,
    /// What cagents knows of a provider whose native chat is the Agent
    /// Client Protocol, for one that serves it.
    acp: Option<Acp>,
    #[cfg(feature = "host")]
    forwards_to_native_child: bool,
    #[cfg(feature = "host")]
    create_chat: fn(
        &crate::chat::Launch,
    ) -> Result<
        (
            crate::chat::protocol::ProcessSpec,
            Box<dyn crate::chat::protocol::Adapter>,
        ),
        String,
    >,
}

/// How a provider that serves the Agent Client Protocol spells its session
/// settings on its terminal command line, and what its permission modes mean.
///
/// A terminal recipe and a native ACP conversation share one launch: the
/// recipe is what a terminal would run, and the provider's ACP profile reads
/// these options from it to configure the session it opens.
#[derive(Debug, Clone, Copy)]
pub struct Acp {
    /// The option naming the permission mode, such as agy's `--mode`.
    pub mode_option: &'static str,
    /// The option naming the reasoning effort, such as agy's `--effort`.
    pub effort_option: &'static str,
    /// Each of the agent's own mode identifiers, beside the cagents mode
    /// that means the same, which the interface and the CLI name it by. A
    /// mode not listed keeps its own identifier.
    pub modes: &'static [(&'static str, &'static str)],
    /// The agent's mode its terminal starts in without the mode option, which
    /// its command line may not accept by name.
    pub default_mode: &'static str,
}

impl Acp {
    /// The cagents name for the agent's mode `mode`.
    pub fn mode_meaning(self, mode: &str) -> &str {
        self.modes
            .iter()
            .find(|(own, _)| *own == mode)
            .map_or(mode, |(_, meaning)| *meaning)
    }

    /// The agent's own mode for the cagents mode `meaning`.
    pub fn own_mode(self, meaning: &str) -> &str {
        self.modes
            .iter()
            .find(|(_, candidate)| *candidate == meaning)
            .map_or(meaning, |(own, _)| *own)
    }

    /// The cagents names of the agent's known modes, in its order.
    pub fn mode_meanings(self) -> impl Iterator<Item = &'static str> {
        self.modes.iter().map(|(_, meaning)| *meaning)
    }
}

const ADAPTERS: [ProviderAdapter; 5] = [
    omp::ADAPTER,
    claude::ADAPTER,
    codex::ADAPTER,
    antigravity::ADAPTER,
    muse::ADAPTER,
];

/// The commands that do not own a foreground conversation for the providers
/// that were recognised before each provider listed its own. They stay shared
/// so recognition of those providers does not change.
fn shared_background_command(argument: &str) -> bool {
    matches!(
        argument,
        "app-server"
            | "exec"
            | "review"
            | "mcp-server"
            | "proxy"
            | "agents"
            | "auth"
            | "login"
            | "logout"
    )
}

/// Whether `name` is a variable some provider set for a process one of its
/// sessions started (see `ProviderAdapter::session_environment`).
///
/// A daemon, window or CLI started from inside an agent's shell carries the
/// parent session's identity and control channels: Claude Code's messaging
/// socket and token, Codex's escalation socket, and so on. Every provider is
/// asked, not only the one being launched, because a Codex conversation started
/// from a Claude session must not find that session's socket either.
pub fn is_session_variable(name: &str) -> bool {
    ADAPTERS
        .iter()
        .any(|adapter| (adapter.session_environment)(name))
}

/// Forgets the provider session this process was started from, so that no
/// daemon, window, terminal or conversation it starts inherits that session's
/// identity or control channels (see [`is_session_variable`]).
///
/// # Safety
///
/// Must be called before the process starts a second thread: it changes the
/// process environment, which another thread may be reading.
pub unsafe fn detach_session_environment() {
    let inherited = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_str().is_some_and(is_session_variable))
        .collect::<Vec<_>>();
    for name in inherited {
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::remove_var(name) };
    }
}

impl Provider {
    fn adapter(self) -> &'static ProviderAdapter {
        &ADAPTERS[self as usize]
    }

    /// The protocol that added this provider, for one appended after the
    /// first clients this daemon serves.
    fn since(self) -> Option<u16> {
        match self {
            Self::Omp | Self::Claude | Self::Codex => None,
            Self::Antigravity | Self::Muse => Some(crate::terminal_ipc::ANTIGRAVITY_AND_MUSE),
        }
    }

    /// Whether the client being encoded for, if any, knows this provider.
    fn is_encodable(self) -> bool {
        !self
            .since()
            .is_some_and(crate::terminal_ipc::encoding_before)
    }

    /// Every provider in declaration order, for menus and for recognizing the
    /// placeholder titles a card carries before its conversation names itself.
    pub const ALL: [Self; 5] = [
        Self::Omp,
        Self::Claude,
        Self::Codex,
        Self::Antigravity,
        Self::Muse,
    ];

    pub fn executable(self) -> &'static str {
        self.adapter().executable
    }

    pub fn label(self) -> &'static str {
        self.adapter().label
    }

    /// Every executable name this provider is recognised by, its own first.
    pub fn programs(self) -> impl Iterator<Item = &'static str> {
        let adapter = self.adapter();
        std::iter::once(adapter.executable).chain(adapter.aliases.iter().copied())
    }

    /// Recognize an exact executable basename, not a shell command or JS wrapper.
    pub fn from_program(program: &str) -> Option<Self> {
        let name = Path::new(program).file_name()?.to_str()?;
        ADAPTERS
            .iter()
            .map(|adapter| adapter.provider)
            .find(|provider| provider.programs().any(|program| program == name))
    }

    /// The executable that runs one of this provider's subcommands, such as
    /// its ACP server or its login, for `program`, the one its terminal runs.
    /// They differ only where `program` is a wrapper that puts options before
    /// any subcommand: Muse's `muse`. A relative `program` or `path` entry is
    /// taken from `directory`.
    pub fn subcommand_program(self, program: &str, directory: &Path, path: &str) -> String {
        match self {
            Self::Muse => muse::subcommand_program(program, directory, path),
            _ => program.to_owned(),
        }
    }

    /// Whether cagents can run this provider's conversations as native chat.
    /// A provider without it is recognised in a terminal but never offered,
    /// converted or started as a native conversation.
    pub fn has_native_chat(self) -> bool {
        self.adapter().native_chat
    }

    /// The permission modes an agent of this provider may be started with.
    pub fn permission_modes(self) -> &'static [&'static str] {
        self.adapter().permission_modes
    }

    /// Other names its terminal gives the same modes, also accepted.
    pub fn permission_mode_aliases(self) -> &'static [&'static str] {
        self.adapter().permission_mode_aliases
    }

    /// Whether answering one of its native chat's questions may approve a
    /// tool, as an Oh My Pi confirmation does.
    pub fn approves_through_questions(self) -> bool {
        self.adapter().approves_through_questions
    }

    /// Sandboxes a mode may name after `/`.
    pub fn permission_sandboxes(self) -> &'static [&'static str] {
        self.adapter().permission_sandboxes
    }

    /// The permission modes this provider takes, as its help and errors name
    /// them: "a, b or c", then any sandbox suffix or terminal aliases.
    pub fn permission_mode_choices(self) -> String {
        fn list<S: AsRef<str>>(names: &[S]) -> String {
            match names {
                [] => String::new(),
                [only] => only.as_ref().to_owned(),
                [rest @ .., last] => {
                    let rest: Vec<_> = rest.iter().map(AsRef::as_ref).collect();
                    format!("{} or {}", rest.join(", "), last.as_ref())
                }
            }
        }
        let mut choices = list(self.permission_modes());
        if let sandboxes @ [_, ..] = self.permission_sandboxes() {
            let sandboxes: Vec<_> = sandboxes
                .iter()
                .map(|sandbox| format!("/{sandbox}"))
                .collect();
            choices = format!(
                "{choices}, each optionally followed by {}",
                list(&sandboxes)
            );
        }
        if let aliases @ [_, ..] = self.permission_mode_aliases() {
            choices = format!("{choices}, or its terminal's {}", list(aliases));
        }
        choices
    }

    /// Refuses a permission mode this provider has no such mode for, such as
    /// another provider's, naming the ones it has.
    pub fn check_permission_mode(self, mode: &str) -> Result<(), String> {
        let (named, sandbox) = mode
            .split_once('/')
            .map_or((mode, None), |(named, sandbox)| (named, Some(sandbox)));
        let sandbox_known =
            sandbox.is_none_or(|sandbox| self.permission_sandboxes().contains(&sandbox));
        if sandbox_known && self.permission_modes().contains(&named)
            || self.permission_mode_aliases().contains(&mode)
        {
            return Ok(());
        }
        if self.permission_modes().is_empty() {
            return Err(format!("{} takes no permission mode", self.label()));
        }
        Err(format!(
            "{} has no permission mode {mode:?}; choose {}",
            self.label(),
            self.permission_mode_choices()
        ))
    }

    /// How this provider serves the Agent Client Protocol, for one whose
    /// native chat speaks it.
    pub fn acp(self) -> Option<Acp> {
        self.adapter().acp
    }

    /// Whether this provider's native chat is the Agent Client Protocol.
    pub fn speaks_acp(self) -> bool {
        self.adapter().acp.is_some()
    }

    /// Why a native conversation with this provider cannot start, for a
    /// provider without native chat.
    pub fn native_chat_refusal(self) -> String {
        format!(
            "Native chat with {} isn't available yet. Run `{}` in a terminal instead.",
            self.label(),
            self.executable()
        )
    }

    /// Recognize only known foreground executable/JavaScript entrypoints.
    /// Options and identity still require recovery's separate exact validation.
    pub fn recognize_launch(launch: &LaunchRecipe) -> Option<Self> {
        let (provider, arguments) = Self::launched(launch)?;
        if arguments
            .first()
            .is_some_and(|arg| (provider.adapter().background_command)(arg))
        {
            return None;
        }
        Some(provider)
    }

    /// The provider a launch runs, whatever it runs it for, and the arguments
    /// it passes that provider: a known executable, or an interpreter running
    /// one of the provider's known entrypoints.
    pub fn launched(launch: &LaunchRecipe) -> Option<(Self, &[String])> {
        if Self::is_interpreter(&launch.program) {
            let script = Path::new(launch.arguments.first()?);
            let provider = ADAPTERS
                .iter()
                .find(|adapter| (adapter.recognizes_script)(script))?
                .provider;
            Some((provider, &launch.arguments[1..]))
        } else {
            Some((
                Self::from_program(&launch.program)?,
                launch.arguments.as_slice(),
            ))
        }
    }

    /// Preserve the existing process-recovery normalization without granting an ID.
    pub fn normalize_launch(mut launch: LaunchRecipe) -> Option<LaunchRecipe> {
        let provider = Self::recognize_launch(&launch)?;
        if Self::is_interpreter(&launch.program) {
            launch.program = provider.executable().into();
            launch.arguments.remove(0);
        }
        Some(launch)
    }

    pub(crate) fn is_interpreter(program: &str) -> bool {
        matches!(
            Path::new(program)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("node" | "nodejs" | "bun")
        )
    }

    #[cfg(feature = "host")]
    pub(crate) fn forwards_to_native_child(self) -> bool {
        self.adapter().forwards_to_native_child
    }

    pub fn resume_flag(self) -> &'static str {
        self.adapter().resume_flag
    }

    pub(crate) fn is_identity_option(self, flag: &str) -> bool {
        self.adapter().identity_options.contains(&flag)
    }

    pub(crate) fn is_cwd_option(self, flag: &str) -> bool {
        self.adapter().cwd_options.contains(&flag)
    }

    pub(crate) fn is_value_option(self, flag: &str) -> bool {
        self.adapter().value_options.contains(&flag)
    }

    /// Read only the canonical selector position in an already normalized recipe.
    /// Shared checkpoint validation uses this parser before any daemon starts.
    pub fn resume_id(self, arguments: &[String]) -> Option<&str> {
        let (selector, id) = if self.adapter().resume_precedes_options {
            match arguments {
                [selector, id, ..] => (selector, id),
                _ => return None,
            }
        } else {
            match arguments {
                [.., selector, id] => (selector, id),
                _ => return None,
            }
        };
        (selector == self.resume_flag()).then_some(id.as_str())
    }

    pub(crate) fn is_switch(self, flag: &str) -> bool {
        self.adapter().switches.contains(&flag)
    }

    /// Whether `flag` continues the most recent conversation rather than
    /// naming one.
    pub(crate) fn is_latest_selector(self, flag: &str) -> bool {
        self.adapter().latest_selectors.contains(&flag)
    }

    /// Whether the provider has a conversation picker cagents can open.
    pub(crate) fn has_resume_picker(self) -> bool {
        self.adapter().resume_picker
    }

    pub(crate) fn append_resume(self, arguments: &mut Vec<String>, id: String) {
        let resume = [self.resume_flag().into(), id];
        if self.adapter().resume_precedes_options {
            arguments.splice(0..0, resume);
        } else {
            arguments.extend(resume);
        }
    }

    /// Open the provider's own conversation picker rather than naming one.
    pub(crate) fn append_resume_picker(self, arguments: &mut Vec<String>) {
        if self.adapter().resume_precedes_options {
            arguments.insert(0, self.resume_flag().into());
        } else {
            arguments.push(self.resume_flag().into());
        }
    }

    /// A filename alone is a convention, never proof that a process owns a session.
    pub fn session_file_id(self, path: &Path) -> Option<String> {
        (path.extension()?.to_str()? == "jsonl").then_some(())?;
        let id = (self.adapter().session_file_id)(path.file_stem()?.to_str()?)?;
        (id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()).then(|| id.to_owned())
    }

    /// Authored skills and commands belong to configuration, which OMP may
    /// keep separately from its session data under XDG.
    #[cfg(any(feature = "ui", feature = "host"))]
    pub fn authored_content_root(self, launch: &LaunchRecipe) -> Option<PathBuf> {
        if self == Self::Omp {
            omp::config_root(
                launch,
                &launch
                    .working_directory
                    .join(launch.environment.get("HOME")?),
            )
        } else {
            self.session_root(launch)
        }
    }

    /// Resolve account/profile roots exclusively from the preserved launch.
    /// Relative environment paths are interpreted by the child in its own cwd,
    /// never in the cwd of the UI or daemon reading its history.
    pub fn session_root(self, launch: &LaunchRecipe) -> Option<PathBuf> {
        let home = launch
            .working_directory
            .join(launch.environment.get("HOME")?);
        (self.adapter().session_root)(launch, &home).map(|root| launch.working_directory.join(root))
    }

    #[cfg(feature = "host")]
    pub(crate) fn session_state_root(self, launch: &LaunchRecipe) -> Option<PathBuf> {
        if self == Self::Omp {
            omp::state_root(
                launch,
                &launch
                    .working_directory
                    .join(launch.environment.get("HOME")?),
            )
        } else {
            self.session_root(launch)
        }
    }

    #[cfg(feature = "host")]
    pub(crate) fn session_directory(self) -> &'static str {
        self.adapter().session_directory
    }

    /// Decode only an authoritative provider lifecycle title grammar.
    /// This describes the provider's viewed agent, not aggregate session activity.
    pub fn activity_from_title(self, title: &str) -> Option<ActivityState> {
        (self.adapter().activity_from_title)(title)
    }

    /// Presentation metadata from this provider's own PTY, never a session ID
    /// or permission to attach to a conversation.
    pub fn conversation_title<'a>(self, title: &'a str, cwd: &std::path::Path) -> Option<&'a str> {
        let label = match self {
            Self::Omp => {
                self.activity_from_title(title)?;
                let rest = title.strip_prefix("π ")?;
                rest[rest.chars().next()?.len_utf8()..].trim()
            }
            Self::Codex => codex_title_without_status(title).trim(),
            // Claude exposes names through its PID-scoped inventory. Arbitrary
            // OSC titles are not an alternative to that authoritative source.
            Self::Claude => return None,
            // Neither has a title grammar that separates a conversation's
            // name from the rest of its terminal title.
            Self::Antigravity | Self::Muse => return None,
        };
        let directory = cwd.file_name().and_then(|name| name.to_str());
        // Codex's default title ends in the project name, separated from the
        // thread name by " | ". Only remove a suffix matching this PTY's cwd.
        let label = if self == Self::Codex {
            label
                .rsplit_once(" | ")
                .filter(|(_, project)| {
                    Some(*project) == directory || Some(*project) == cwd.to_str()
                })
                .map_or(label, |(name, _)| name)
        } else {
            label
        };
        if label.is_empty()
            || Some(label) == cwd.to_str()
            || Some(label) == directory
            || [
                "codex",
                "omp",
                "Ready",
                "Working",
                "Idle",
                "Action Required",
            ]
            .iter()
            .any(|default| label.eq_ignore_ascii_case(default))
        {
            return None;
        }
        Some(label)
    }

    #[cfg(feature = "host")]
    pub(crate) fn create_chat(
        self,
        launch: &crate::chat::Launch,
    ) -> Result<
        (
            crate::chat::protocol::ProcessSpec,
            Box<dyn crate::chat::protocol::Adapter>,
        ),
        String,
    > {
        (self.adapter().create_chat)(launch)
    }
}

/// Strip Codex's activity decoration from the conversation name. The app
/// renders status with icons using observed activity, never inferred title text.
pub fn codex_title_without_status(title: &str) -> &str {
    for marker in ["[ ! ] ", "[ . ] ", "[!] ", "[.] "] {
        if let Some(status) = title.strip_prefix(marker)
            && let Some(rest) = status.strip_prefix("Action Required")
        {
            if rest.is_empty() {
                return rest;
            }
            if let Some(name) = rest.strip_prefix(" | ") {
                return name;
            }
        }
    }
    let Some(first) = title.chars().next() else {
        return title;
    };
    if "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(first) {
        let rest = &title[first.len_utf8()..];
        if rest.is_empty() {
            rest
        } else {
            rest.strip_prefix(' ').unwrap_or(title)
        }
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a Claude Code, Codex or Omp session exports to the processes it
    /// starts, including the socket another session messages it through.
    const PARENT_SESSION: &[(&str, &str)] = &[
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", "parent-session"),
        ("CLAUDE_CODE_CHILD_SESSION", "1"),
        ("CLAUDE_CODE_SESSION_ATTENDED", "0"),
        ("CLAUDE_CODE_ENTRYPOINT", "sdk-cli"),
        (
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "/run/user/1000/cc-socks/parent.sock",
        ),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "parent-token"),
        ("CLAUDE_PID", "4242"),
        ("CLAUDE_EFFORT", "medium"),
        ("CLAUDE_JOB_DIR", "/home/a/.claude/jobs/parent"),
        ("CLAUDE_CODE_BRIDGE_SESSION_ID", "parent-bridge"),
        ("CLAUDE_CODE_REMOTE", "true"),
        ("CLAUDE_CODE_REMOTE_SESSION_ID", "parent-remote"),
        ("AI_AGENT", "claude-code_agent"),
        ("CODEX_THREAD_ID", "parent-thread"),
        ("CODEX_ESCALATE_SOCKET", "/tmp/parent-escalate.sock"),
        ("CODEX_SANDBOX", "seccomp"),
        ("PI_TOOL_BRIDGE_TOKEN", "parent-bridge"),
        ("OMP_SESSION_ID", "parent-session"),
        ("ANTIGRAVITY_CONVERSATION_ID", "parent-conversation"),
        ("ANTIGRAVITY_CSRF_TOKEN", "parent-token"),
        ("ANTIGRAVITY_LS_ADDRESS", "127.0.0.1:41234"),
        ("ANTIGRAVITY_TRAJECTORY_ID", "parent-trajectory"),
    ];

    /// The user's own provider configuration, which may sit beside them.
    const USER_CONFIGURATION: &[(&str, &str)] = &[
        ("CLAUDE_CONFIG_DIR", "/home/a/.claude"),
        ("CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION", "10"),
        ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ("CLAUDE_CODE_TMPDIR", "/home/a/tmp"),
        ("ANTHROPIC_MODEL", "claude-sonnet-4-5"),
        ("CODEX_HOME", "/home/a/.codex"),
        ("OMP_PROFILE", "work"),
        ("PI_CODING_AGENT_DIR", "/home/a/.pi"),
        (
            "ANTIGRAVITY_APP_DATA_DIR",
            "/home/a/.gemini/antigravity-cli",
        ),
        ("XDG_DATA_HOME", "/home/a/.local/share"),
    ];

    #[test]
    fn a_parent_session_is_told_apart_from_the_users_configuration() {
        for (name, _) in PARENT_SESSION {
            assert!(is_session_variable(name), "{name} passes to children");
        }
        for (name, _) in USER_CONFIGURATION {
            assert!(!is_session_variable(name), "{name} is withheld");
        }
    }

    /// The binaries detach before anything starts, so every daemon, window,
    /// terminal and conversation they start inherits no parent session.
    #[test]
    fn a_child_of_a_detached_process_inherits_no_parent_session() {
        const CHILD: &str = "CAGENTS_TEST_DETACH_SESSION";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent::tests::a_child_of_a_detached_process_inherits_no_parent_session",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .envs(PARENT_SESSION.iter().chain(USER_CONFIGURATION).copied())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // SAFETY: the harness runs this child with a single test thread, and
        // nothing else in it reads the environment concurrently.
        unsafe { detach_session_environment() };
        let output = std::process::Command::new("/usr/bin/env").output().unwrap();
        assert!(output.status.success());
        let inherited = String::from_utf8(output.stdout).unwrap();
        let inherited = inherited
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (name, _) in PARENT_SESSION {
            assert_eq!(inherited.get(name), None, "{name} reached the child");
        }
        for (name, value) in USER_CONFIGURATION {
            assert_eq!(inherited.get(name), Some(value), "{name} was lost");
        }
    }

    #[test]
    fn conversation_titles_omit_activity_and_ignore_launch_placeholders() {
        let cwd = std::path::Path::new("/repo/project");
        assert_eq!(
            Provider::Omp.conversation_title("π ◕ Fix parser", cwd),
            Some("Fix parser")
        );
        assert_eq!(Provider::Omp.conversation_title("π > project", cwd), None);
        assert_eq!(
            Provider::Omp.conversation_title("π > /repo/project", cwd),
            None
        );
        assert_eq!(
            Provider::Omp.conversation_title("arbitrary shell title", cwd),
            None
        );
        assert_eq!(
            Provider::Codex.conversation_title("⠋ Fix parser", cwd),
            Some("Fix parser")
        );
        assert_eq!(
            Provider::Codex.conversation_title("⠋ Fix parser | project", cwd),
            Some("Fix parser")
        );
        assert_eq!(Provider::Codex.conversation_title("⠋ project", cwd), None);
        assert_eq!(Provider::Codex.conversation_title("codex", cwd), None);
        assert_eq!(Provider::Claude.conversation_title("Fix parser", cwd), None);
        for marker in ["[ ! ]", "[ . ]", "[!]", "[.]"] {
            let title = format!("{marker} Action Required | Fix parser | project");
            assert_eq!(
                Provider::Codex.conversation_title(&title, cwd),
                Some("Fix parser")
            );
            assert_eq!(
                Provider::Codex.conversation_title(&format!("{marker} Action Required"), cwd),
                None
            );
        }
        assert_eq!(
            Provider::Codex.conversation_title("[ ! ] Action Required documentation", cwd),
            Some("[ ! ] Action Required documentation")
        );
    }

    fn launch(program: &str, arguments: &[&str]) -> LaunchRecipe {
        LaunchRecipe {
            unset_environment: Default::default(),
            program: program.into(),
            arguments: arguments.iter().map(|arg| (*arg).into()).collect(),
            working_directory: "/tmp".into(),
            environment: Default::default(),
        }
    }

    #[test]
    fn session_roots_resolve_relative_paths_in_the_provider_launch_directory() {
        let mut recipe = launch("claude", &[]);
        recipe.working_directory = "/project/worktree".into();
        recipe.environment.insert("HOME".into(), "home".into());
        for (provider, default) in [
            (Provider::Claude, "home/.claude"),
            (Provider::Codex, "home/.codex"),
            (Provider::Omp, "home/.omp/agent"),
        ] {
            assert_eq!(
                provider.session_root(&recipe),
                Some(recipe.working_directory.join(default))
            );
        }
        assert_eq!(
            Provider::Antigravity.session_root(&recipe),
            Some(
                recipe
                    .working_directory
                    .join("home/.gemini/antigravity-cli")
            )
        );
        assert_eq!(
            Provider::Muse.session_root(&recipe),
            Some(recipe.working_directory.join("home/.local/share/muse"))
        );
        recipe
            .environment
            .insert("XDG_DATA_HOME".into(), "relative/data".into());
        assert_eq!(
            Provider::Muse.session_root(&recipe),
            Some(recipe.working_directory.join("home/.local/share/muse")),
            "the base directory spec ignores a relative XDG_DATA_HOME"
        );
        recipe
            .environment
            .insert("XDG_DATA_HOME".into(), "/data".into());
        assert_eq!(
            Provider::Muse.session_root(&recipe),
            Some("/data/muse".into())
        );
        for (provider, variable) in [
            (Provider::Claude, "CLAUDE_CONFIG_DIR"),
            (Provider::Codex, "CODEX_HOME"),
            (Provider::Omp, "PI_CODING_AGENT_DIR"),
        ] {
            recipe
                .environment
                .insert(variable.into(), "selected-account".into());
            assert_eq!(
                provider.session_root(&recipe),
                Some(recipe.working_directory.join("selected-account"))
            );
            recipe
                .environment
                .insert(variable.into(), "/absolute/account".into());
            assert_eq!(
                provider.session_root(&recipe),
                Some("/absolute/account".into())
            );
        }
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn omp_authored_content_resolves_relative_home_in_the_launch_directory() {
        let mut recipe = launch("omp", &[]);
        recipe.working_directory = "/project/worktree".into();
        recipe.environment.insert("HOME".into(), "home".into());
        assert_eq!(
            Provider::Omp.authored_content_root(&recipe),
            Some(recipe.working_directory.join("home/.omp/agent"))
        );
        #[cfg(feature = "host")]
        assert_eq!(
            Provider::Omp.session_state_root(&recipe),
            Some(recipe.working_directory.join("home/.omp/agent"))
        );
    }

    #[test]
    fn recognition_distinguishes_foreground_wrappers_from_scripts_and_servers() {
        for (program, script, provider) in [
            ("node", "/opt/@openai/codex/bin/codex.js", Provider::Codex),
            (
                "nodejs",
                "/opt/@anthropic-ai/claude-code/cli.js",
                Provider::Claude,
            ),
            ("bun", "/opt/omp/omp.js", Provider::Omp),
        ] {
            assert_eq!(
                Provider::recognize_launch(&launch(program, &[script])),
                Some(provider)
            );
            assert_eq!(
                Provider::recognize_launch(&launch(program, &[script, "app-server"])),
                None
            );
        }
        for recipe in [
            launch("sh", &["-c", "omp"]),
            launch("node", &["/tmp/cli.js", "claude"]),
            launch("node", &["/tmp/tool.js", "codex"]),
            launch("omp", &["__omp_worker"]),
            launch("codex", &["exec"]),
            launch("claude", &["auth", "login"]),
            launch("codex", &["login"]),
            launch("/usr/bin/codex", &["logout"]),
        ] {
            assert_eq!(Provider::recognize_launch(&recipe), None);
        }
        for (recipe, provider) in [
            (launch("agy", &[]), Provider::Antigravity),
            (
                launch("/usr/local/bin/agy", &["--model", "x"]),
                Provider::Antigravity,
            ),
            (
                launch("agy", &["--conversation", "id"]),
                Provider::Antigravity,
            ),
            (launch("muse", &[]), Provider::Muse),
            (
                launch("/home/a/.local/bin/muse-upstream", &[]),
                Provider::Muse,
            ),
            (launch("muse", &["resume"]), Provider::Muse),
        ] {
            assert_eq!(
                Provider::recognize_launch(&recipe),
                Some(provider),
                "{recipe:?}"
            );
        }
        // Each provider's own servers and one-shot commands, and only its own.
        for recipe in [
            launch("agy", &["acp", "--disable-telemetry"]),
            launch("agy", &["update"]),
            launch("agy", &["models"]),
            launch("muse", &["acp"]),
            launch("muse-upstream", &["acp", "--contributor-mode", "off"]),
            launch("muse", &["serve"]),
            launch("muse", &["sessions"]),
            launch("muse", &["login"]),
        ] {
            assert_eq!(Provider::recognize_launch(&recipe), None, "{recipe:?}");
        }
        assert_eq!(
            Provider::recognize_launch(&launch("claude", &["acp"])),
            Some(Provider::Claude)
        );
        assert_eq!(
            Provider::recognize_launch(&launch("agy", &["exec"])),
            Some(Provider::Antigravity)
        );
        // Foreground recognition must not turn a fresh launch into resume authority.
        let fresh = Provider::normalize_launch(launch("bun", &["/opt/omp/omp.js"])).unwrap();
        assert!(crate::model::agent_recovery::resume_recipe(&fresh).is_none());
    }

    #[test]
    fn providers_keep_their_persisted_names_and_refuse_clients_before_them() {
        for provider in Provider::ALL {
            let name = serde_json::to_string(&provider).unwrap();
            assert_eq!(name, format!("\"{provider:?}\""));
            assert_eq!(serde_json::from_str::<Provider>(&name).unwrap(), provider);
            assert_eq!(
                postcard::from_bytes::<Provider>(&postcard::to_allocvec(&provider).unwrap())
                    .unwrap(),
                provider
            );
        }
        let older = crate::terminal_ipc::ANTIGRAVITY_AND_MUSE - 1;
        for provider in [Provider::Antigravity, Provider::Muse] {
            assert!(
                crate::terminal_ipc::encoding_for(older, || postcard::to_allocvec(&provider))
                    .is_err(),
                "{provider:?} reached a client from before it"
            );
        }
        for provider in [Provider::Omp, Provider::Claude, Provider::Codex] {
            assert_eq!(
                crate::terminal_ipc::encoding_for(older, || postcard::to_allocvec(&provider))
                    .unwrap(),
                postcard::to_allocvec(&provider).unwrap()
            );
        }
    }
}
