//! Structured native-agent conversations. Provider JSON stays behind adapters;
//! the UI and daemon exchange bounded, typed messages rather than terminal text.
use crate::agent::{ActivityState, Provider};
use crate::model::LaunchRecipe;
use serde::{Deserialize, Serialize};

/// Grace periods shared with clients that must wait for confirmed teardown.
pub(crate) const INTERRUPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub(crate) const TERMINATE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(feature = "ui")]
pub mod client;
#[cfg(feature = "host")]
pub mod protocol;
#[cfg(feature = "host")]
pub mod runtime;
#[cfg(feature = "ui")]
pub mod skills;

/// Local wall-clock time for a conversation stamp, as `14:32`.
///
/// A transcript is read while the work is happening, so the hour and minute
/// are what place an entry; the date would be noise in a conversation that
/// lasts an afternoon. `localtime_r` is used because the reader's own
/// timezone is the only one that answers "when did this happen", and the
/// daemon that writes a stamp runs on the same machine as the surface that
/// reads it.
pub fn clock_label(unix_ms: u64) -> String {
    let seconds = i64::try_from(unix_ms / 1000).unwrap_or(i64::MAX);
    local_time(seconds).map_or_else(String::new, |time| time.clock())
}

/// The reader's local calendar position of a unix time.
pub struct LocalTime {
    pub day_of_year: i32,
    /// Years since 1900, as `struct tm` counts them.
    pub year: i32,
    /// Days since Sunday.
    pub weekday: usize,
    /// Months since January.
    pub month: usize,
    pub day: i32,
    pub hour: i32,
    pub minute: i32,
}

impl LocalTime {
    /// The time of day, as `14:32`.
    pub fn clock(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// Where `unix_seconds` falls on the reader's local calendar, or `None` for a
/// time the platform cannot represent.
pub fn local_time(unix_seconds: i64) -> Option<LocalTime> {
    let seconds = unix_seconds as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes into the caller's `tm` and reads only the
    // time it is given, so it is sound for any `time_t` value.
    if unsafe { libc::localtime_r(&seconds, &mut parts) }.is_null() {
        return None;
    }
    Some(LocalTime {
        day_of_year: parts.tm_yday,
        year: parts.tm_year,
        weekday: parts.tm_wday.clamp(0, 6) as usize,
        month: parts.tm_mon.clamp(0, 11) as usize,
        day: parts.tm_mday,
        hour: parts.tm_hour,
        minute: parts.tm_min,
    })
}

/// Unix milliseconds now, or `None` from a clock set before the epoch.
///
/// The daemon stamps every item it observes with this, and the surface
/// measures how long a stretch of work has been running against it, so both
/// sides have to read the same clock: an elapsed time taken from a different
/// one would report the difference between two clocks rather than the time the
/// agent spent.
pub fn now_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub provider: Provider,
    /// The original agent executable, launch options, working directory and environment.
    pub recipe: LaunchRecipe,
    /// An exact provider conversation identifier, never a latest-session selector.
    pub session_id: Option<String>,
}

impl Launch {
    /// Return to the CLI using the current provider-confirmed identity, not a
    /// stale ID from the original argv before a native /clear or session switch.
    pub fn terminal_recipe(&self) -> Option<LaunchRecipe> {
        let id = self.session_id.as_deref()?;
        if id.len() != 36
            || uuid::Uuid::parse_str(id).is_err()
            || std::path::Path::new(&self.recipe.program)
                .file_name()?
                .to_str()?
                != self.provider.executable()
        {
            return None;
        }
        let mut recipe = self.recipe.clone();
        let mut arguments = Vec::with_capacity(recipe.arguments.len() + 2);
        let mut original = recipe.arguments.iter();
        while let Some(argument) = original.next() {
            let (flag, inline) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(flag, value)| {
                    (flag, Some(value))
                });
            if self.provider.is_identity_option(flag) {
                if inline.is_none() {
                    original.next()?;
                }
            } else {
                arguments.push(argument.clone());
            }
        }
        self.provider.append_resume(&mut arguments, id.into());
        recipe.arguments = arguments;
        crate::model::agent_recovery::resume_recipe(&recipe)
    }
}

/// What a card is asking the daemon to do with a conversation.
///
/// The distinction is not "spawn or attach": a saved card cannot know which of
/// those is possible, because only the daemon's live proof says whether the
/// incarnation that hosted the conversation is still running. The card states
/// its intent and the connection works out how to honour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Begin this launch. A conversation the launch names by id is resumed by
    /// the provider's own argv rather than by attaching to a daemon writer.
    Start,
    /// Put a saved card back on the conversation it was showing, by whichever
    /// route the live daemon still supports.
    Restore,
}

/// A conversation that could not be put on screen, and the one thing left that
/// would recover it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartFailure {
    /// The provider's, the adapter's or the daemon's own words. A card renders
    /// this verbatim, because a summary of a failure the reader has to act on
    /// is worth less than the sentence that named it.
    pub message: String,
    pub recovery: Recovery,
}

/// What the reader can still do about a conversation that did not come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Running the same start again is a real next step, because what refused
    /// it - a stopped daemon, a busy writer, a provider that failed to launch -
    /// can succeed on the next attempt.
    Retry,
    /// The saved conversation cannot be reached by any retry. Only a new
    /// conversation in the same place is left, and the reader has to ask for
    /// it rather than find one started on their behalf.
    ///
    /// Asking for it destroys nothing. A provider keeps one conversation per
    /// file, named after its own identifier, and a new conversation takes a
    /// new identifier and therefore a new file; the session directory is a
    /// multi-conversation store that every native card of the same provider
    /// already shares. What the reader gives up is this card's pointer to the
    /// old conversation, because no surface re-opens a conversation by
    /// identifier afterwards except the Codex binding panel. So a card
    /// offering this says the old conversation stays on disk and that it will
    /// stop being the way back to it, rather than implying either a deletion
    /// or a round trip that is not there.
    Fresh,
}

impl StartFailure {
    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            recovery: Recovery::Retry,
        }
    }
    pub fn fresh(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            recovery: Recovery::Fresh,
        }
    }
}

/// Every bare string error on the start path is a condition that can pass, so
/// `?` classifies one as retryable without each call site saying so.
impl From<String> for StartFailure {
    fn from(message: String) -> Self {
        Self::retry(message)
    }
}

impl From<&str> for StartFailure {
    fn from(message: &str) -> Self {
        Self::retry(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub path: std::path::PathBuf,
}

/// The file extensions every provider's image input recognises.
pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

const MIB: u64 = 1024 * 1024;

/// What a provider's native transport takes as an attachment.
///
/// The provider drivers enforce these when a prompt is sent. The composer
/// checks a dropped file against them before attaching it, so a file the
/// provider would refuse becomes its path in the draft rather than an error
/// waiting at send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentRules {
    /// Whether the provider takes images and nothing else.
    pub images_only: bool,
    /// The most attachments one prompt may carry, if there is a limit.
    pub max_count: Option<usize>,
    /// The largest image the provider takes.
    pub max_image_bytes: u64,
    /// The largest file of any other kind the provider takes, if it limits
    /// them.
    pub max_file_bytes: Option<u64>,
}

impl AttachmentRules {
    pub const fn for_provider(provider: Provider) -> Self {
        match provider {
            Provider::Claude => Self {
                images_only: false,
                max_count: Some(8),
                max_image_bytes: 5 * MIB,
                max_file_bytes: Some(20 * MIB),
            },
            // Codex is given the image's path and reads it itself.
            Provider::Codex => Self {
                images_only: true,
                max_count: None,
                max_image_bytes: u64::MAX,
                max_file_bytes: None,
            },
            // OMP sends images inline and names any other file in the prompt.
            Provider::Omp => Self {
                images_only: false,
                max_count: None,
                max_image_bytes: 16 * MIB,
                max_file_bytes: None,
            },
        }
    }

    /// Whether the provider would take the file at `path` as an attachment,
    /// as far as its name and metadata tell.
    pub fn accepts(&self, path: &std::path::Path) -> bool {
        // Every driver passes or prints the path as text somewhere.
        if path.to_str().is_none() {
            return false;
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        if is_image_path(path) {
            metadata.len() <= self.max_image_bytes
        } else {
            !self.images_only
                && self
                    .max_file_bytes
                    .is_none_or(|limit| metadata.len() <= limit)
        }
    }
}

/// Whether `path` names an image by its extension.
pub fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|image| extension.eq_ignore_ascii_case(image))
        })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: crate::model::TerminalId,
    pub launch: Launch,
    pub proof: Option<crate::model::HostSessionProof>,
    /// Original terminal identity for an explicit return to the CLI view.
    #[serde(default)]
    pub terminal: Option<crate::model::TerminalSession>,
    /// Daemon-issued allocation evidence, independent of the reader's TMPDIR.
    #[serde(default)]
    pub owned_codex_listener: Option<OwnedCodexListener>,
}

/// The exact endpoint a daemon allocated for one native Codex conversation.
/// The allocator remains unchanged when the current host proof rotates.
/// This is durable provenance, not authority for an arbitrary client to claim
/// a socket: the daemon also checks its retained/checkpointed state or its exact
/// allocation namespace before acting on client-supplied evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedCodexListener {
    pub socket: std::path::PathBuf,
    pub allocator: crate::model::HostSessionProof,
}

impl OwnedCodexListener {
    pub fn matches(&self, id: crate::model::TerminalId, recipe: &LaunchRecipe) -> bool {
        use std::path::Component;
        self.allocator.is_complete()
            && self.socket.is_absolute()
            && self
                .socket
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
            && self
                .socket
                .file_name()
                .is_some_and(|name| name == "control.sock")
            && self
                .socket
                .parent()
                .and_then(std::path::Path::file_name)
                .is_some_and(|name| name == format!("cagents-codex-{id}").as_str())
            && recipe
                .arguments
                .iter()
                .position(|arg| arg == "--remote")
                .and_then(|index| recipe.arguments.get(index + 1))
                .is_some_and(|remote| remote == &format!("unix://{}", self.socket.display()))
    }
}

impl Session {
    /// Whether no daemon has ever held this conversation: it has no provider
    /// identity, no proof of a host that ran it, and no terminal it came
    /// from. A conversation is proven the moment its start is answered, which
    /// is before anything can be sent to it, so nothing a reader wrote can be
    /// in one that is still unstarted.
    pub fn is_unstarted(&self) -> bool {
        self.launch.session_id.is_none()
            && self.proof.is_none()
            && self.terminal.is_none()
            && self.owned_codex_listener.is_none()
    }

    /// An endpoint allocated for this exact cagents thread, retained in both views.
    pub fn owns_codex_listener(&self) -> bool {
        self.launch.provider == Provider::Codex
            && self
                .owned_codex_listener
                .as_ref()
                .is_some_and(|owned| owned.matches(self.id, &self.launch.recipe))
    }

    /// Give up the allocation when an unnamed, lost conversation returns to a
    /// draft. Its next start receives a fresh identity and asks the daemon for
    /// a new listener rather than reusing the abandoned conversation's socket.
    pub fn reset_owned_codex_listener(&mut self) {
        if self.owns_codex_listener() {
            let index = self
                .launch
                .recipe
                .arguments
                .iter()
                .position(|argument| argument == "--remote")
                .expect("owned listener has a remote argument");
            self.launch.recipe.arguments[index + 1] = "unix://".into();
        }
        self.owned_codex_listener = None;
    }

    /// Whether `started` is this new Codex thread as the daemon started it.
    ///
    /// A new thread asks for a listener of its own with a bare `unix://`
    /// remote, which the daemon resolves to its reported owned endpoint
    /// before it launches anything; that resolution is the one change the
    /// launch undergoes, and the rest of the recipe is exactly as asked.
    pub fn resolved_to_owned_codex_listener(&self, started: &Self) -> bool {
        if self.launch.session_id.is_some()
            || self.terminal.is_some()
            || started.id != self.id
            || !started.owns_codex_listener()
            || !started
                .proof
                .as_ref()
                .is_some_and(crate::model::HostSessionProof::is_complete)
        {
            return false;
        }
        let mut recipe = self.launch.recipe.clone();
        let Some(index) = recipe.arguments.iter().position(|arg| arg == "--remote") else {
            return false;
        };
        let Some(remote) = recipe.arguments.get_mut(index + 1) else {
            return false;
        };
        if self.launch.provider != Provider::Codex || remote != "unix://" {
            return false;
        }
        *remote = format!(
            "unix://{}",
            started
                .owned_codex_listener
                .as_ref()
                .unwrap()
                .socket
                .display()
        );
        recipe == started.launch.recipe
    }

    /// Point this Codex thread's first `--remote` at its own listener.
    ///
    /// Only the daemon chooses `root`. Existing evidence must have come from
    /// trusted retained/checkpointed state before invoking this method. Legacy
    /// recipes can be promoted only at this daemon's exact allocation path.
    pub fn claim_owned_codex_listener(
        &mut self,
        root: &std::path::Path,
        allocator: &crate::model::HostSessionProof,
    ) -> bool {
        if self.launch.provider != Provider::Codex {
            return false;
        }
        if self.owns_codex_listener() {
            return true;
        }
        if !root.is_absolute() || !allocator.is_complete() {
            return false;
        }
        let socket = root
            .join(format!("cagents-codex-{}", self.id))
            .join("control.sock");
        let owned = format!("unix://{}", socket.display());
        let new_thread = self.launch.session_id.is_none();
        let arguments = &mut self.launch.recipe.arguments;
        let Some(index) = arguments.iter().position(|argument| argument == "--remote") else {
            return false;
        };
        match arguments.get_mut(index + 1) {
            Some(remote) if *remote == owned || (new_thread && remote == "unix://") => {
                *remote = owned;
                self.owned_codex_listener = Some(OwnedCodexListener {
                    socket,
                    allocator: allocator.clone(),
                });
                true
            }
            _ => false,
        }
    }
}

/// Read-only provider metadata plus an expiring, single-use daemon capability.
/// The token binds explicit user confirmation to the observed source process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPreview {
    pub token: String,
    pub session_id: String,
    pub title: String,
    pub working_directory: std::path::PathBuf,
}

/// One conversation a Codex account's shared server is currently holding open.
/// Listing carries no token because enumeration authorizes nothing; an attach
/// still requires the separate confirmed preview capability.
///
/// `title` is the thread's own name where it has one and the server's own
/// preview of the conversation where it does not, because an unnamed thread
/// has nothing else to tell one conversation from another by. Listing
/// therefore carries a little conversation text, within the same account and
/// the same user, in exchange for being choosable at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingCandidate {
    pub session_id: String,
    pub title: String,
    pub working_directory: std::path::PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: u64,
    pub items: Vec<Item>,
    pub requests: Vec<Request>,
    pub capabilities: Capabilities,
    pub models: Vec<Model>,
    pub selected_model: Option<String>,
    pub reasoning: Option<String>,
    pub commands: Vec<SlashCommand>,
    pub skills: Vec<Skill>,
    pub usage: Usage,
    pub busy: bool,
    pub detail: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub error: Option<String>,
    pub exited: bool,
    pub has_earlier: bool,
    pub has_later: bool,
    pub composer: Option<String>,
    pub open_url: Option<(String, String)>,
    /// Provider-confirmed user attention, including requests not yet delivered.
    pub waiting: bool,
    /// The steps of the plan the conversation is working to, newest only.
    ///
    /// Defaulted so a snapshot written before plans were carried still loads.
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    /// Child conversations associated with this exact parent conversation.
    #[serde(default)]
    pub subagents: Vec<Subagent>,
    /// Whether the provider takes commands, so that what waits to be sent
    /// can wait for it rather than for a refusal.
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    /// The parent conversation's tool-use ID, which also keys child frames.
    pub id: String,
    pub kind: String,
    pub description: String,
    pub status: ToolStatus,
    pub items: Vec<Item>,
}

impl Snapshot {
    pub fn activity(&self) -> ActivityState {
        if self.exited {
            ActivityState::Unknown
        } else if self.waiting || !self.requests.is_empty() {
            ActivityState::Waiting
        } else if self.busy {
            ActivityState::Working
        } else {
            ActivityState::Idle
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptMode {
    Send,
    Steer,
    Queue,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Prompt {
        text: String,
        attachments: Vec<Attachment>,
        mode: PromptMode,
    },
    Interrupt,
    Answer {
        request_id: String,
        action: String,
        answers: Vec<Answer>,
    },
    SetModel {
        model: String,
        reasoning: Option<String>,
    },
    SetPermissionMode {
        mode: String,
    },
    Compact,
    Refresh,
    LoadEarlier,
    /// Provider slash commands are sent through its own command dispatcher.
    Slash {
        command: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub field: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}
/// Where an assistant message sits within its turn.
///
/// Providers narrate continuously while they work and only a small fraction of
/// what they emit is the answer to the question. Measured over this
/// workstation's own Codex history, 101 227 assistant messages were commentary
/// against 13 750 final answers. A transcript that renders both identically
/// buries the answer in the narration, so the distinction has to reach the
/// view rather than being flattened away in the adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// The answer a turn was asked for.
    #[default]
    Final,
    /// Progress narration emitted while the turn is still working.
    Commentary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    Denied,
}

/// What a shell command was actually for.
///
/// Providers classify their own command invocations, which is what lets a
/// transcript say "Read src/app.rs" instead of reprinting an argv. Anything a
/// provider does not classify stays [`CommandAction::Run`] and is shown as the
/// command it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandAction {
    #[default]
    Run,
    Read,
    Search,
    List,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Deleted,
    Updated,
    /// `from` is the pre-rename path; the change's own path is the new one.
    Renamed {
        from: String,
    },
}

/// One file's worth of a provider's edit, carrying the patch rather than a
/// rendering of it so the view can use the repository pane's diff canvas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    /// Unified diff for this path alone.
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemContent {
    Text {
        text: String,
        thinking: bool,
        /// Defaulted so transcripts stored before phases existed still load.
        #[serde(default)]
        phase: Phase,
    },
    /// A shell command the provider ran on the user's behalf.
    Command {
        /// The provider's own summary, e.g. `Read src/app.rs`. Empty when the
        /// provider offered none, in which case the view falls back to
        /// `command`.
        label: String,
        command: String,
        working_directory: String,
        action: CommandAction,
        status: ToolStatus,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
        output: String,
    },
    /// A provider edit, one entry per touched path.
    Changes {
        changes: Vec<FileChange>,
        status: ToolStatus,
    },
    WebSearch {
        query: String,
        results: Vec<SearchResult>,
    },
    /// Every other tool call, including MCP. The fallback that keeps an
    /// unrecognized provider item renderable instead of dropped.
    Tool {
        name: String,
        input: String,
        output: String,
        status: ToolStatus,
    },
    Image {
        path: String,
        description: String,
    },
}

impl ItemContent {
    /// Byte cost of the payload, which is what an adapter charges an item
    /// against its per-item ceiling before the item reaches the store.
    pub fn byte_len(&self) -> usize {
        fn diff_bytes(changes: &[FileChange]) -> usize {
            changes.iter().fold(0, |total, change| {
                total
                    .saturating_add(change.path.len())
                    .saturating_add(change.diff.len())
            })
        }
        match self {
            Self::Text { text, .. } => text.len(),
            Self::Command {
                label,
                command,
                working_directory,
                output,
                ..
            } => label
                .len()
                .saturating_add(command.len())
                .saturating_add(working_directory.len())
                .saturating_add(output.len()),
            Self::Changes { changes, .. } => diff_bytes(changes),
            Self::WebSearch { query, results } => {
                results.iter().fold(query.len(), |total, result| {
                    total
                        .saturating_add(result.title.len())
                        .saturating_add(result.url.len())
                        .saturating_add(result.snippet.len())
                })
            }
            Self::Tool {
                name,
                input,
                output,
                ..
            } => name
                .len()
                .saturating_add(input.len())
                .saturating_add(output.len()),
            Self::Image { path, description } => path.len().saturating_add(description.len()),
        }
    }

    /// Whether the item is something the agent did rather than something it
    /// wrote.
    ///
    /// Steps collapse into a group's activity strip, one click-to-open row
    /// each. Text never does, reasoning included: filing a paragraph of
    /// reasoning behind a row captioned "Thinking" hid the one part of a
    /// turn that explains why the agent did what it did, and made the
    /// transcript an accordion the reader had to operate to read prose.
    pub fn is_step(&self) -> bool {
        match self {
            Self::Command { .. }
            | Self::Changes { .. }
            | Self::WebSearch { .. }
            | Self::Tool { .. } => true,
            Self::Text { .. } | Self::Image { .. } => false,
        }
    }

    pub fn status(&self) -> Option<ToolStatus> {
        match self {
            Self::Command { status, .. }
            | Self::Changes { status, .. }
            | Self::Tool { status, .. } => Some(*status),
            Self::Text { .. } | Self::WebSearch { .. } | Self::Image { .. } => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub role: Role,
    pub content: ItemContent,
    /// Unix milliseconds when the daemon last saw this item change, which is
    /// when it finished for an item that is no longer being written to.
    #[serde(default)]
    pub at: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub description: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub id: String,
    pub label: String,
    pub description: String,
    pub choices: Vec<Choice>,
    pub multiple: bool,
    pub secret: bool,
    pub initial: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub title: String,
    pub description: String,
    pub fields: Vec<Field>,
    /// Explicit provider decisions; no implicit approval or permissive default.
    pub actions: Vec<Choice>,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub reasoning: Vec<String>,
}

impl Model {
    /// One entry of the `models` Claude Code's `initialize` control response
    /// lists, which both a conversation and an account probe receive.
    pub fn from_claude(value: &serde_json::Value) -> Option<Self> {
        let id = value["value"].as_str()?;
        Some(Self {
            id: id.into(),
            label: value["displayName"].as_str().unwrap_or(id).into(),
            reasoning: strings(value["supportedEffortLevels"].as_array(), |effort| {
                effort.as_str()
            }),
        })
    }

    /// One entry of the Codex app server's `model/list` page.
    ///
    /// An entry that does not describe its efforts, or does not name itself
    /// for the picker, is still a model the user can select, so only the
    /// unreadable parts are dropped; an entry without an ID is not a model.
    pub fn from_codex(value: &serde_json::Value) -> Option<Self> {
        let id = value["model"].as_str()?;
        Some(Self {
            id: id.into(),
            label: value["displayName"].as_str().unwrap_or(id).into(),
            reasoning: strings(value["supportedReasoningEfforts"].as_array(), |effort| {
                effort["reasoningEffort"].as_str()
            }),
        })
    }
}

fn strings(
    values: Option<&Vec<serde_json::Value>>,
    read: impl Fn(&serde_json::Value) -> Option<&str>,
) -> Vec<String> {
    values
        .into_iter()
        .flatten()
        .filter_map(read)
        .map(str::to_owned)
        .collect()
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}
/// Something the provider will do on request that is not one of its own
/// commands: a repository skill, a saved prompt, a workflow.
///
/// The invocation is carried rather than derived from the name because each
/// provider spells it differently. Codex acts on a skill named in the message
/// text as `$Name`, which is not a command dispatch at all, so a client that
/// assumed a leading slash would send something the provider ignores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub invocation: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub steer: bool,
    pub queue: bool,
    pub attachments: bool,
    pub compact: bool,
    pub model_selection: bool,
    pub reasoning_selection: bool,
    pub permission_modes: Vec<String>,
    pub skill_reach: SkillReach,
}
/// Which of the skills a message names the provider loads with it.
///
/// Every provider reads a skill named in a message, but not every one reads
/// all of them: a skill it does not load reaches the agent as plain words,
/// which the agent may or may not act on. The composer says so rather than
/// dressing such a name up as a skill that will run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillReach {
    /// Every skill named anywhere in the message, as Codex reads `$name`.
    #[default]
    Anywhere,
    /// Only a skill the message opens with, as Claude Code expands a leading
    /// `/name`; one named later is left for the agent to take up.
    Leading,
    /// Only the first skill named, wherever it is, as oh-my-pi reads
    /// `/skill:name`: set apart by whitespace, and in a message that does not
    /// open with another command or a shell escape.
    First,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_tokens: u64,
    pub context_limit: u64,
    pub cost_microusd: Option<u64>,
}

/// One step of a plan a provider published, with the state it is in.
///
/// A plan is carried as its steps rather than as the document the provider
/// wrote, because the surface pins it above the transcript and what a reader
/// needs from a pinned plan is which step is being worked on now. Codex used
/// to reach the transcript as a text item holding the raw JSON of its plan
/// payload, which is the same information in the one shape nobody can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanState {
    #[default]
    Pending,
    Active,
    Done,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// A provider-confirmed conversation reset. The next Session event supplies
    /// its authoritative identity; never infer an ID from a reset hint.
    Reset {
        reason: String,
    },
    Session {
        id: String,
        title: Option<String>,
    },
    Capabilities(Capabilities),
    Models {
        models: Vec<Model>,
        selected: Option<String>,
        reasoning: Option<String>,
    },
    Commands(Vec<SlashCommand>),
    Skills(Vec<Skill>),
    /// Upsert a completed/history item without duplicating a streamed item.
    Item(Item),
    Subagent {
        id: String,
        kind: String,
        description: String,
        status: Option<ToolStatus>,
    },
    SubagentItem {
        id: String,
        item: Item,
    },
    SubagentTextDelta {
        id: String,
        item_id: String,
        role: Role,
        thinking: bool,
        delta: String,
    },
    SubagentItemsRemoved {
        id: String,
        ids: Vec<String>,
    },
    ItemsRemoved {
        ids: Vec<String>,
    },
    History {
        items: Vec<Item>,
        has_earlier: bool,
    },
    TextDelta {
        id: String,
        role: Role,
        thinking: bool,
        delta: String,
    },
    Request(Request),
    RequestResolved {
        id: String,
    },
    Status {
        busy: bool,
        detail: String,
    },
    Usage(Usage),
    Composer {
        text: String,
    },
    /// Offered to the user; never opened automatically.
    OpenUrl {
        url: String,
        instructions: String,
    },
    Error {
        message: String,
        fatal: bool,
    },
    Exited {
        code: Option<i32>,
    },
    /// Authoritative provider attention independent of request presentation.
    Waiting(bool),
    /// The plan the provider is now working to, replacing any earlier one.
    ///
    /// A plan is republished whole on every revision, so the newest event is
    /// the plan: there is no incremental step state to merge.
    Plan {
        steps: Vec<PlanStep>,
    },
    /// What the provider's CLI reported about the account this conversation
    /// runs on. It is about the account, not the conversation: the runtime
    /// forwards it to every surface and keeps it out of the transcript.
    AccountUsage(crate::account::LiveReport),
    /// Whether the provider now takes commands. A provider refuses them
    /// while it connects or loads a conversation, and says so only by
    /// refusing, which a sender cannot tell from a refusal of the command.
    Ready(bool),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_is_offered_only_the_attachments_it_takes() {
        let directory = tempfile::tempdir().unwrap();
        let file = |name: &str, bytes: u64| {
            let path = directory.path().join(name);
            std::fs::File::create(&path)
                .unwrap()
                .set_len(bytes)
                .unwrap();
            path
        };
        let image = file("shot.PNG", 1024);
        let large_image = file("large.webp", 6 * MIB);
        let document = file("notes.pdf", 1024);
        let large_document = file("dump.bin", 21 * MIB);

        let codex = AttachmentRules::for_provider(Provider::Codex);
        assert!(codex.accepts(&image));
        assert!(codex.accepts(&large_image));
        assert!(!codex.accepts(&document));

        let claude = AttachmentRules::for_provider(Provider::Claude);
        assert!(claude.accepts(&image));
        assert!(!claude.accepts(&large_image));
        assert!(claude.accepts(&document));
        assert!(!claude.accepts(&large_document));
        assert_eq!(claude.max_count, Some(8));

        let omp = AttachmentRules::for_provider(Provider::Omp);
        assert!(omp.accepts(&large_image));
        assert!(omp.accepts(&large_document));

        // A folder, or a path that is not there, is no provider's attachment.
        for rules in [codex, claude, omp] {
            assert!(!rules.accepts(directory.path()));
            assert!(!rules.accepts(&directory.path().join("missing.png")));
        }
    }

    /// The daemon refuses to checkpoint a conversation whose launch cannot be
    /// rebuilt into an exact resume, and a refused checkpoint is what leaves a
    /// card with no upgrade lineage to be restored through. A native card that
    /// records its conversation in `session_id` and its storage in
    /// `--session-dir` has to pass that gate, including for a UUIDv7.
    #[test]
    fn the_persisted_native_launch_rebuilds_an_exact_resume_for_the_checkpoint() {
        let launch = Launch {
            provider: crate::agent::Provider::Omp,
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: "omp".into(),
                arguments: vec![
                    "--session-dir".into(),
                    "/home/user/.omp/agent/sessions/native".into(),
                ],
                working_directory: "/home/user".into(),
                environment: Default::default(),
            },
            session_id: Some("01a0ab66-21d6-7065-9fe5-95f302b669e1".into()),
        };
        let recipe = launch
            .terminal_recipe()
            .expect("a native launch naming its conversation is resumable");
        assert_eq!(
            recipe.arguments,
            [
                "--session-dir",
                "/home/user/.omp/agent/sessions/native",
                "--resume",
                "01a0ab66-21d6-7065-9fe5-95f302b669e1",
            ]
        );
    }

    /// The daemon and the stored card agree on which endpoint a Codex thread
    /// listens on, so the claim looks only at the first `--remote`, as the
    /// launch itself does, and never re-points an endpoint it did not allocate.
    #[test]
    fn only_a_new_thread_asking_for_its_own_listener_is_claimed() {
        let session = |arguments: &[&str], session_id: Option<&str>| Session {
            owned_codex_listener: None,
            id: crate::model::TerminalId::new(),
            launch: Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    unset_environment: Default::default(),
                    program: "codex".into(),
                    arguments: arguments.iter().map(|&argument| argument.into()).collect(),
                    working_directory: "/home/user".into(),
                    environment: Default::default(),
                },
                session_id: session_id.map(str::to_owned),
            },
            proof: None,
            terminal: None,
        };

        let root = std::path::Path::new("/daemon-temporary-root");
        let allocator = crate::model::HostSessionProof {
            host_session_id: "daemon".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        };
        let mut asked = session(&["--remote", "unix://"], None);
        let unstarted = asked.clone();
        assert!(asked.claim_owned_codex_listener(root, &allocator));
        asked.proof = Some(allocator.clone());
        assert!(asked.owns_codex_listener());
        assert!(unstarted.resolved_to_owned_codex_listener(&asked));
        let claimed = asked.clone();
        assert!(
            asked.claim_owned_codex_listener(
                std::path::Path::new("/different-ui-temp-root"),
                &allocator
            ),
            "an owned listener stays claimed"
        );
        assert_eq!(asked, claimed);

        for (arguments, session_id) in [
            (
                &["--remote", "unix:///elsewhere", "--remote", "unix://"][..],
                None,
            ),
            (&["--remote", "unix://"][..], Some("begun")),
            (&["--search"][..], None),
        ] {
            let mut other = session(arguments, session_id);
            let before = other.clone();
            assert!(
                !other.claim_owned_codex_listener(root, &allocator),
                "{arguments:?}"
            );
            assert_eq!(other, before);
        }

        // Proof rotation does not rewrite the allocator or the saved endpoint.
        let mut restored: Session =
            serde_json::from_str(&serde_json::to_string(&asked).unwrap()).unwrap();
        restored.proof.as_mut().unwrap().host_incarnation = "replacement".into();
        assert!(restored.owns_codex_listener());
        assert_eq!(restored.owned_codex_listener, claimed.owned_codex_listener);

        // Legacy descriptors retain their exact recipe, but the path suffix
        // alone supplies no ownership evidence to an unrelated reader.
        let mut legacy = serde_json::to_value(&asked).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("owned_codex_listener");
        let mut legacy: Session = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.owns_codex_listener());
        assert!(
            !legacy.claim_owned_codex_listener(std::path::Path::new("/unrelated-root"), &allocator)
        );
        assert_eq!(legacy.launch.recipe, asked.launch.recipe);
        assert!(legacy.claim_owned_codex_listener(root, &allocator));

        let mut unrelated = asked.clone();
        unrelated.launch.recipe.arguments[1] = format!(
            "unix:///unrelated/cagents-codex-{}/control.sock",
            unrelated.id
        );
        assert!(!unrelated.owns_codex_listener());
        assert!(!unstarted.resolved_to_owned_codex_listener(&unrelated));
        unrelated = asked.clone();
        unrelated
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .host_incarnation = "".into();
        assert!(!unrelated.owns_codex_listener());
    }
}
