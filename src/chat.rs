//! Structured native-agent conversations. Provider JSON stays behind adapters;
//! the UI and daemon exchange bounded, typed messages rather than terminal text.
use crate::agent::{ActivityState, Provider};
use crate::model::LaunchRecipe;
use serde::{Deserialize, Serialize};

#[cfg(feature = "ui")]
pub mod client;
#[cfg(feature = "host")]
pub mod protocol;
#[cfg(feature = "host")]
pub mod runtime;

/// Local wall-clock time for a conversation stamp, as `14:32`.
///
/// A transcript is read while the work is happening, so the hour and minute
/// are what place an entry; the date would be noise in a conversation that
/// lasts an afternoon. `localtime_r` is used because the reader's own
/// timezone is the only one that answers "when did this happen", and the
/// daemon that writes a stamp runs on the same machine as the surface that
/// reads it.
pub fn clock_label(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes into the caller's `tm` and reads only the
    // time it is given, so it is sound for any `time_t` value.
    if unsafe { libc::localtime_r(&seconds, &mut parts) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", parts.tm_hour, parts.tm_min)
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
}
impl Session {
    /// An endpoint allocated for this exact cagents thread, retained in both views.
    pub fn owns_codex_listener(&self) -> bool {
        let socket = std::env::temp_dir()
            .join(format!("cagents-codex-{}", self.id))
            .join("control.sock");
        let remote = format!("unix://{}", socket.display());
        self.launch.provider == Provider::Codex
            && self
                .launch
                .recipe
                .arguments
                .windows(2)
                .any(|pair| pair[0] == "--remote" && pair[1] == remote)
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub reasoning: Vec<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}
