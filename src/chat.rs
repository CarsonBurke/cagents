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
pub(crate) mod native_holder;
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
    /// Resume with the last provider-confirmed Claude policy. A permission
    /// change is session state, so an original CLI bypass must not restore it
    /// when the user returns from native chat.
    pub(crate) fn terminal_recipe_with_permission_mode(
        &self,
        mode: Option<&str>,
    ) -> Option<LaunchRecipe> {
        let mut recipe = self.terminal_recipe()?;
        if self.provider == Provider::Claude
            && let Some(mode) = mode
        {
            apply_claude_permission_mode(&mut recipe, mode);
        }
        Some(recipe)
    }
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

pub(crate) fn apply_claude_permission_mode(recipe: &mut LaunchRecipe, mode: &str) -> bool {
    if !matches!(
        mode,
        "default" | "manual" | "auto" | "acceptEdits" | "plan" | "dontAsk" | "bypassPermissions"
    ) {
        return false;
    }
    recipe.environment.remove("CLAUDE_CODE_PERMISSION_MODE");
    let mut arguments = Vec::with_capacity(recipe.arguments.len() + 2);
    let mut original = recipe.arguments.iter();
    while let Some(argument) = original.next() {
        match argument.as_str() {
            "--permission-mode" => {
                original.next();
            }
            "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions" => {}
            value if value.starts_with("--permission-mode=") => {}
            _ => arguments.push(argument.clone()),
        }
    }
    arguments.extend(["--permission-mode".into(), mode.into()]);
    recipe.arguments = arguments;
    true
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
    /// The recorded managed home was removed; choose another account of this
    /// provider before attempting the exact conversation again.
    Account,
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

/// Inline tool images are spooled to private files before entering a snapshot.
pub(crate) const MAX_INLINE_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_INLINE_IMAGE_URL_BYTES: usize = (MAX_INLINE_IMAGE_BYTES.div_ceil(3) * 4) + 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Promoted only after the private owner acknowledges a verified server
    /// and exact conversation. A durable source may never silently relaunch.
    #[serde(default)]
    pub native_continuity: NativeContinuity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeContinuity {
    #[default]
    ReconnectOnly,
    Durable {
        provider_version: String,
    },
}

/// Account recovery changes only account selection. It never changes the
/// provider, conversation, executable, working directory, or launch options.
/// Runtime identities and daemon-allocated listener paths may be refreshed.
pub fn is_account_recovery(source: &Session, replacement: &Session) -> bool {
    if source.id == replacement.id
        || source.launch.session_id.is_none()
        || source.launch.session_id != replacement.launch.session_id
        || source.launch.provider != replacement.launch.provider
        || replacement.terminal.is_some()
    {
        return false;
    }
    let Some(home) = crate::account::home_variable(source.launch.provider) else {
        return false;
    };
    let names = crate::account::credential_overrides(source.launch.provider);
    if replacement
        .launch
        .recipe
        .environment
        .keys()
        .any(|name| names.contains(&name.as_str()))
    {
        return false;
    }
    let mut original = source.clone();
    let mut candidate = replacement.clone();
    original.reset_owned_codex_listener();
    candidate.reset_owned_codex_listener();
    let original = &mut original.launch.recipe;
    let candidate = &mut candidate.launch.recipe;
    if original.environment.get(home) == candidate.environment.get(home) {
        return false;
    }
    for recipe in [&mut *original, &mut *candidate] {
        recipe.environment.remove(home);
        recipe
            .environment
            .retain(|name, _| !names.contains(&name.as_str()));
        recipe
            .unset_environment
            .retain(|name| !names.contains(&name.as_str()));
    }
    original == candidate
}

/// The narrow descriptor change an authenticated daemon upgrade may commit.
/// This predicate is not provenance: callers must also have resolved the exact
/// source through ResolveUpgradeChat and attached its confirmed replacement.
pub fn is_upgrade_recovery(source: &Session, replacement: &Session) -> bool {
    let Some((old, new)) = source.proof.as_ref().zip(replacement.proof.as_ref()) else {
        return false;
    };
    if !old.is_complete()
        || !new.is_complete()
        || old == new
        || source.id != replacement.id
        || source.launch.provider != replacement.launch.provider
        || !upgrade_recipe_matches(source, replacement)
        || source.owned_codex_listener != replacement.owned_codex_listener
        || (source.native_continuity != replacement.native_continuity
            && !matches!(source.native_continuity, NativeContinuity::ReconnectOnly))
        || !replacement.has_valid_native_continuity()
        || !source
            .launch
            .session_id
            .as_ref()
            .is_some_and(|id| !id.is_empty() && id.len() <= 1024)
        || !replacement
            .launch
            .session_id
            .as_ref()
            .is_some_and(|id| !id.is_empty() && id.len() <= 1024)
    {
        return false;
    }
    match (&source.terminal, &replacement.terminal) {
        (None, None) => true,
        (Some(old), Some(new)) if old.id == source.id && new.id == replacement.id => {
            let mut normalized = new.clone();
            normalized.continuity = old.continuity.clone();
            &normalized == old
        }
        _ => false,
    }
}

/// A retained Codex holder may have issued the same confirmed thread with an
/// explicit resume prefix after its original fresh-thread descriptor. Only
/// that prefix can change; the endpoint and all execution settings stay exact.
fn upgrade_recipe_matches(source: &Session, replacement: &Session) -> bool {
    if source.launch.recipe == replacement.launch.recipe {
        return true;
    }
    if source.launch.provider != Provider::Codex
        || source.launch.session_id != replacement.launch.session_id
        || !matches!(source.native_continuity, NativeContinuity::Durable { .. })
        || !source.owns_codex_listener()
        || !replacement.owns_codex_listener()
        || source
            .launch
            .recipe
            .arguments
            .iter()
            .any(|argument| argument == "resume")
    {
        return false;
    }
    let mut normalized = replacement.launch.recipe.clone();
    let Some(conversation) = replacement.launch.session_id.as_ref() else {
        return false;
    };
    let arguments = &normalized.arguments;
    if arguments.first().map(String::as_str) != Some("resume")
        || arguments.get(1) != Some(conversation)
    {
        return false;
    }
    normalized.arguments.drain(..2);
    normalized == source.launch.recipe
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
            && self.native_continuity == NativeContinuity::ReconnectOnly
    }

    /// An endpoint allocated for this exact cagents thread, retained in both views.
    pub fn owns_codex_listener(&self) -> bool {
        self.launch.provider == Provider::Codex
            && self
                .owned_codex_listener
                .as_ref()
                .is_some_and(|owned| owned.matches(self.id, &self.launch.recipe))
    }

    pub fn has_valid_native_continuity(&self) -> bool {
        match &self.native_continuity {
            NativeContinuity::ReconnectOnly => true,
            NativeContinuity::Durable { provider_version } => {
                self.owns_codex_listener()
                    && self
                        .proof
                        .as_ref()
                        .is_some_and(crate::model::HostSessionProof::is_complete)
                    && self
                        .launch
                        .session_id
                        .as_ref()
                        .is_some_and(|id| !id.is_empty() && id.len() <= 1024)
                    && !provider_version.is_empty()
                    && provider_version.len() <= 64
                    && provider_version
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'.')
            }
        }
    }

    /// Authenticated native adoption rotates only daemon ownership. Its paired
    /// terminal has its own holder lineage and keeps the same logical recipe.
    pub fn retains_native_pair_from(&self, previous: &Self) -> bool {
        let (Some(old), Some(new)) = (&previous.terminal, &self.terminal) else {
            return false;
        };
        matches!(previous.native_continuity, NativeContinuity::Durable { .. })
            && self.native_continuity == previous.native_continuity
            && self.has_valid_native_continuity()
            && previous
                .proof
                .as_ref()
                .zip(self.proof.as_ref())
                .is_some_and(|(old, new)| old.boot_id == new.boot_id)
            && old.id == new.id
            && new.id == self.id
            && old.launch == new.launch
            && old.owned_codex_listener == new.owned_codex_listener
            && old.continuity.live_proof().as_ref() == previous.proof.as_ref()
            && new.continuity.live_proof().as_ref() == self.proof.as_ref()
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
        self.native_continuity = NativeContinuity::ReconnectOnly;
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
    #[serde(default)]
    pub permission_mode: Option<String>,
    pub commands: Vec<SlashCommand>,
    pub skills: Vec<Skill>,
    pub usage: Usage,
    pub busy: bool,
    pub detail: String,
    pub session_id: Option<String>,
    #[serde(default)]
    pub native_continuity: NativeContinuity,
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
    #[serde(default)]
    pub queued_messages: Vec<QueuedMessage>,
}

/// A bounded preview from the provider's confirmed queue, independent of history pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
}

impl QueuedMessage {
    pub const LIMIT: usize = 128;
    pub fn new(mut id: String, mut text: String) -> Self {
        id.truncate(id.floor_char_boundary(256));
        const PREVIEW_BYTES: usize = 16 * 1024;
        if text.len() > PREVIEW_BYTES {
            text.truncate(text.floor_char_boundary(PREVIEW_BYTES));
            text.push_str("\n[Queue preview truncated]");
        }
        Self { id, text }
    }
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
    /// Something another agent, or the harness on its behalf, delivered to
    /// this conversation: a subagent's report, a teammate's or another
    /// session's message, or word that background work changed state.
    ///
    /// It arrives in the user's slot and the agent acts on it as on a
    /// prompt, but the user did not write it. Typesetting it as their own
    /// message, wrapped in the framing the harness adds for the model,
    /// made the transcript read as though they had.
    Delivery {
        kind: DeliveryKind,
        /// The name the sender goes by: a subagent's type, a teammate's or a
        /// session's name. Empty when it has none.
        from: String,
        /// An opaque identifier telling this sender apart from others of the
        /// same name, such as a subagent's task ID. Empty when there is none.
        sender: String,
        /// The harness's own one-line account of what happened, such as
        /// `Agent "Review" finished`. Empty when it wrote none, and the
        /// heading is then formed from `kind` and the sender.
        summary: String,
        /// A report's or message's Markdown, or the output a notice quotes,
        /// with the harness's framing removed.
        body: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryKind {
    /// A delegated agent's final report to the conversation that started it.
    Report,
    /// A message from a teammate or another session.
    Message,
    /// The harness reporting that background work - a command, a monitor or
    /// an agent - finished or produced an event.
    Notice,
}

impl DeliveryKind {
    /// The words a delivery's heading opens with; a notice's own summary
    /// stands in their place.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Report => "Report from",
            Self::Message => "Message from",
            Self::Notice => "",
        }
    }
}

/// Who sent a delivery, as one phrase: its name, or failing that its
/// identifier, which is shortened because a task ID is only ever needed to
/// tell two senders apart at a glance.
pub fn delivery_sender(from: &str, sender: &str) -> String {
    let short: String = sender.chars().take(8).collect();
    match (from.is_empty(), short.is_empty()) {
        (false, _) => from.into(),
        (true, false) => format!("agent {short}"),
        (true, true) => "another agent".into(),
    }
}

/// A delivery's heading as plain text: the harness's own summary where it
/// wrote one, such as `Agent "Review" finished`, and otherwise who sent it,
/// as in `Report from general-purpose`.
pub fn delivery_heading(kind: DeliveryKind, from: &str, sender: &str, summary: &str) -> String {
    if summary.is_empty() {
        format!("{} {}", kind.verb(), delivery_sender(from, sender))
    } else {
        summary.into()
    }
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
            Self::Delivery {
                from,
                sender,
                summary,
                body,
                ..
            } => from
                .len()
                .saturating_add(sender.len())
                .saturating_add(summary.len())
                .saturating_add(body.len()),
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
            Self::Text { .. } | Self::Image { .. } | Self::Delivery { .. } => false,
        }
    }

    pub fn status(&self) -> Option<ToolStatus> {
        match self {
            Self::Command { status, .. }
            | Self::Changes { status, .. }
            | Self::Tool { status, .. } => Some(*status),
            Self::Text { .. }
            | Self::WebSearch { .. }
            | Self::Image { .. }
            | Self::Delivery { .. } => None,
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
    PermissionMode(Option<String>),
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
    /// Authoritative live turn completion; never emitted for transcript history,
    /// tool results, compaction or a change in presentation status.
    /// A new main turn, independently of aggregate subagent/tool activity.
    TurnStarted,
    /// The provider's current turn settled; its final attention outcome may
    /// still await a correlated control acknowledgement.
    TurnSettled,
    TurnFinished {
        key: Option<String>,
        outcome: crate::notifications::Kind,
    },
    /// Correlated to the harness prompt when it was accepted by this adapter,
    /// not when a potentially delayed provider result reaches the daemon.
    Execution {
        run: crate::model::RunId,
        event: crate::agent_execution::ProviderEvent,
    },
    QueueChanged(Vec<QueuedMessage>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_handoff_uses_confirmed_claude_permissions_without_changing_identity() {
        let launch = Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: vec![
                    "--permission-mode=manual".into(),
                    "--dangerously-skip-permissions".into(),
                    "--model".into(),
                    "opus".into(),
                ],
                working_directory: "/project".into(),
                environment: [("CLAUDE_CONFIG_DIR".into(), "/selected-account".into())].into(),
                unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
            },
            session_id: Some("d1714898-6c54-4727-8113-0d5b9db7aeb9".into()),
        };
        let recipe = launch
            .terminal_recipe_with_permission_mode(Some("acceptEdits"))
            .unwrap();
        assert_eq!(
            recipe.arguments,
            [
                "--model",
                "opus",
                "--resume",
                "d1714898-6c54-4727-8113-0d5b9db7aeb9",
                "--permission-mode",
                "acceptEdits"
            ]
        );
        assert_eq!(recipe.environment, launch.recipe.environment);
        assert_eq!(recipe.unset_environment, launch.recipe.unset_environment);
        assert_eq!(recipe.working_directory, launch.recipe.working_directory);
        assert!(
            launch
                .recipe
                .arguments
                .iter()
                .any(|argument| argument == "--dangerously-skip-permissions")
        );
        assert_eq!(
            launch.terminal_recipe_with_permission_mode(None),
            launch.terminal_recipe()
        );
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn a_fresh_claude_chat_uses_its_accounts_configured_default_mode() {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"auto"}}"#,
        )
        .unwrap();
        let account = crate::account::Account::managed(
            Provider::Claude,
            uuid::Uuid::new_v4(),
            config.path().into(),
        );
        let session = fresh_session(
            Provider::Claude,
            crate::model::TerminalId::new(),
            config.path().into(),
            &account,
        )
        .unwrap();
        assert_eq!(
            session.launch.recipe.arguments,
            ["--permission-mode", "auto"]
        );

        std::fs::create_dir(config.path().join("project")).unwrap();
        std::fs::create_dir(config.path().join("project/.claude")).unwrap();
        std::fs::write(
            config.path().join("project/.claude/settings.json"),
            r#"{"permissions":{"defaultMode":"plan"}}"#,
        )
        .unwrap();
        let project_session = fresh_session(
            Provider::Claude,
            crate::model::TerminalId::new(),
            config.path().join("project"),
            &account,
        )
        .unwrap();
        assert!(project_session.launch.recipe.arguments.is_empty());
    }

    #[test]
    #[cfg(any(feature = "host", feature = "ui"))]
    fn claude_default_permissions_use_the_launch_home_and_account_settings_layer() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude");
        let cwd = home.path().join("project");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            config.join("settings.json"),
            r#"{"permissions":{"defaultMode":"auto"}}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("settings.local.json"),
            r#"{"permissions":{"defaultMode":"plan"}}"#,
        )
        .unwrap();
        let mut recipe = LaunchRecipe {
            unset_environment: ["CLAUDE_CONFIG_DIR".into()].into(),
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: cwd,
            environment: [("HOME".into(), home.path().to_string_lossy().into_owned())].into(),
        };
        assert_eq!(
            claude_configured_default_mode(&recipe).as_deref(),
            Some("auto")
        );
        recipe.working_directory = home.path().into();
        assert_eq!(claude_configured_default_mode(&recipe), None);
        recipe.working_directory = home.path().join("project");
        recipe.unset_environment.clear();
        recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "../.claude".into());
        assert_eq!(
            claude_configured_default_mode(&recipe).as_deref(),
            Some("auto")
        );
        recipe.unset_environment = ["HOME".into(), "CLAUDE_CONFIG_DIR".into()].into();
        assert_eq!(claude_configured_default_mode(&recipe), None);
    }

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
            native_continuity: Default::default(),
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

/// Shared native launch policy for UI drafts and durable daemon executions.
#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn fresh_session(
    provider: Provider,
    id: crate::model::TerminalId,
    working_directory: std::path::PathBuf,
    account: &crate::account::Account,
) -> Result<Session, String> {
    let mut recipe = LaunchRecipe {
        unset_environment: account
            .credential_overrides()
            .iter()
            .map(|name| (*name).into())
            .collect(),
        program: provider.executable().into(),
        arguments: Vec::new(),
        working_directory,
        environment: crate::model::agent_recovery::fresh_native_environment(provider, true),
    };
    if account.provider == provider
        && let (Some(variable), Some(home)) = (
            crate::account::home_variable(provider),
            account.resolved_home(),
        )
    {
        let home = home
            .into_os_string()
            .into_string()
            .map_err(|_| "The account's home is not a path a conversation can record".to_owned())?;
        recipe.environment.insert(variable.to_owned(), home);
    }
    recipe
        .environment
        .retain(|name, _| !recipe.unset_environment.contains(name));
    match provider {
        // A shared listener lets CLI and native views use the same thread.
        Provider::Codex => recipe
            .arguments
            .extend(["--remote".into(), "unix://".into()]),
        Provider::Omp => {
            let root = provider
                .session_root(&recipe)
                .ok_or("Cannot locate the configured account for a new conversation")?;
            recipe.arguments.extend([
                "--session-dir".into(),
                root.join("sessions/native").to_string_lossy().into_owned(),
            ]);
        }
        Provider::Claude => {
            // Claude's stream-json/print transport reports `default` even when
            // the interactive CLI's configured default is Auto. Pin the
            // account's user choice before the first native prompt so the
            // two surfaces start with the same permissions. Project settings
            // are left to Claude's own trust and policy handling.
            if let Some(mode) = claude_configured_default_mode(&recipe) {
                recipe.arguments.extend(["--permission-mode".into(), mode]);
            }
        }
    }
    Ok(Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id,
        launch: Launch {
            provider,
            recipe,
            session_id: None,
        },
        proof: None,
        terminal: None,
    })
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn claude_configured_default_mode(recipe: &LaunchRecipe) -> Option<String> {
    fn setting(path: &std::path::Path) -> Option<String> {
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let mode = settings["permissions"]["defaultMode"].as_str()?;
        matches!(
            mode,
            "default"
                | "manual"
                | "auto"
                | "acceptEdits"
                | "plan"
                | "dontAsk"
                | "bypassPermissions"
        )
        .then(|| mode.to_owned())
    }

    let environment = |name: &str| {
        (!recipe.unset_environment.contains(name))
            .then(|| {
                recipe
                    .environment
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
            })
            .flatten()
            .filter(|value| !value.is_empty())
    };
    let resolve = |path: std::path::PathBuf| {
        if path.is_absolute() {
            path
        } else {
            recipe.working_directory.join(path)
        }
    };
    let user_home = environment("HOME")
        .map(std::path::PathBuf::from)
        .map(resolve);
    let root = environment("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .map(resolve)
        .or_else(|| user_home.as_ref().map(|home| home.join(".claude")))?;
    let account_settings = std::fs::canonicalize(root.join("settings.json")).ok();
    for directory in recipe.working_directory.ancestors() {
        if user_home.as_deref() == Some(directory) {
            if recipe.working_directory == directory
                && setting(&directory.join(".claude/settings.local.json")).is_some()
            {
                // HOME itself can be a project cwd. Its local setting then
                // belongs to that session, but never to descendant projects.
                return None;
            }
            break;
        }
        if ["settings.local.json", "settings.json"]
            .into_iter()
            .any(|name| {
                let path = directory.join(".claude").join(name);
                if account_settings.is_some()
                    && std::fs::canonicalize(&path).ok() == account_settings
                {
                    return false;
                }
                setting(&path).is_some()
            })
        {
            // A project-specific choice belongs to Claude's own settings
            // handling, not to a command-line override from another account.
            return None;
        }
    }

    // Local settings belong to a project; a settings.local.json beside the
    // account's user settings is not a user configuration layer.
    setting(&root.join("settings.json")).filter(|mode| mode != "default")
}
