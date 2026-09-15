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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub path: std::path::PathBuf,
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

/// Read-only provider metadata plus an expiring, single-use daemon capability.
/// The token binds explicit user confirmation to the observed source process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPreview {
    pub token: String,
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
    /// Byte cost of the payload, used by the transcript store's paging window.
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

    /// Whether the item is a step of a turn rather than conversation prose.
    ///
    /// Steps collapse into the turn's activity strip; prose does not.
    pub fn is_step(&self) -> bool {
        match self {
            Self::Text { thinking, .. } => *thinking,
            Self::Command { .. }
            | Self::Changes { .. }
            | Self::WebSearch { .. }
            | Self::Tool { .. } => true,
            Self::Image { .. } => false,
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
}
