//! Structured native-agent conversations. Provider JSON stays behind adapters;
//! the UI and daemon exchange bounded, typed messages rather than terminal text.
use crate::agent::{ActivityState, Provider};
use crate::model::LaunchRecipe;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ui")]
pub mod coordination;
pub mod presentation;
pub mod pricing;

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
#[cfg(any(feature = "ui", feature = "host"))]
pub mod skills;

/// Whether a provider-level failure requires the reader to renew the login.
/// Only explicit authentication failures qualify: a bare HTTP 401 can describe
/// a tool or service unrelated to the conversation's provider account.
pub fn requires_sign_in(provider: Provider, message: &str) -> bool {
    if provider != Provider::Codex {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    [
        "access token could not be refreshed",
        "refresh token has already been used",
        "refresh token has been revoked",
        "refresh token has expired",
        "refresh token is expired",
        "not logged in",
        "authentication required",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
        || (["sign in again", "signing in again", "log in again"]
            .iter()
            .any(|phrase| lower.contains(phrase))
            && ["token", "authentication", "logged out"]
                .iter()
                .any(|phrase| lower.contains(phrase)))
}

const SIGN_IN_VERIFICATION_MARKER: &str = "\nSign-in verification: ";

pub(crate) fn sign_in_verification_detail(message: &str) -> Option<&str> {
    message
        .split_once(SIGN_IN_VERIFICATION_MARKER)
        .map(|(_, detail)| detail)
}

pub(crate) fn sign_in_verification_failure(failure: &str, detail: &str) -> String {
    let original = failure
        .split_once(SIGN_IN_VERIFICATION_MARKER)
        .map_or(failure, |(original, _)| original);
    let mut chars = detail.chars().map(|character| {
        if character.is_whitespace() {
            ' '
        } else {
            character
        }
    });
    let mut bounded: String = chars.by_ref().take(512).collect();
    if chars.next().is_some() {
        bounded.pop();
        bounded.push('…');
    }
    format!("{original}{SIGN_IN_VERIFICATION_MARKER}{}", bounded.trim())
}

#[cfg(test)]
mod authentication_tests {
    use super::*;

    #[test]
    fn sign_in_recovery_requires_an_explicit_codex_authentication_failure() {
        for message in [
            "Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.",
            "Codex RPC error: refresh token has already been used",
            "Authentication token expired. Please sign in again.",
            "Codex RPC error: Not logged in",
        ] {
            assert!(requires_sign_in(Provider::Codex, message), "{message}");
        }
        for message in [
            "HTTP 401 from the deployment tool",
            "Connection refused",
            "Permission denied",
            "Please sign in again to the database",
            "Token budget exhausted",
        ] {
            assert!(!requires_sign_in(Provider::Codex, message), "{message}");
        }
        assert!(!requires_sign_in(Provider::Claude, "Not logged in"));
        assert!(!requires_sign_in(Provider::Omp, "Not logged in"));
    }
}

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
    /// Claude's exact saved transcript remains readable after its working
    /// directory is removed, without starting a provider in another directory.
    pub(crate) fn needs_read_only_history(&self) -> bool {
        self.provider == Provider::Claude
            && self
                .session_id
                .as_ref()
                .is_some_and(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
            && self.recipe.working_directory.is_absolute()
            && !self.recipe.working_directory.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            && matches!(std::fs::metadata(&self.recipe.working_directory), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    }

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

/// An optional next prompt from the current provider process. This is not a
/// draft or transcript entry and is never sent until the user accepts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSuggestion {
    pub session_id: String,
    pub source: String,
    pub turn_generation: u64,
    pub text: String,
}

impl PromptSuggestion {
    pub const MAX_BYTES: usize = 4096;

    pub fn valid_text(text: &str) -> bool {
        !text.trim().is_empty()
            && text.len() <= Self::MAX_BYTES
            && !text
                .chars()
                .any(|character| character.is_control() && character != '\n' && character != '\t')
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Daemon wall-clock milliseconds sampled for this publication. Clients
    /// advance this with their monotonic clock instead of comparing timestamps
    /// from two machines. Absent only in older data and synthetic snapshots.
    #[serde(default)]
    pub sampled_at: Option<u64>,
    pub generation: u64,
    /// Turn starts observed by this provider process. Unlike publication
    /// generation, this changes only at a turn boundary, including queued
    /// and autonomous turns, even when publications coalesce the idle state.
    #[serde(default)]
    pub turn_generation: u64,
    /// Daemon wall-clock time when a provider-confirmed turn and its delegated
    /// work finished. Historical imports without an observed turn have no time.
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// Immutable payloads are shared by daemon, client and retained UI snapshots.
    /// Updating one answer must not copy the finished transcript around it.
    pub items: Vec<std::sync::Arc<Item>>,
    pub requests: Vec<Request>,
    pub capabilities: Capabilities,
    pub models: Vec<Model>,
    pub selected_model: Option<String>,
    pub reasoning: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<FastMode>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub permissions: Option<PermissionSettings>,
    #[serde(default)]
    pub permission_denials: Vec<PermissionDenial>,
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
    /// Why this snapshot shows saved history without an executable provider.
    /// Carried over IPC so every client explains the same read-only state.
    #[serde(default)]
    pub read_only_reason: Option<String>,
    pub exited: bool,
    pub has_earlier: bool,
    pub has_later: bool,
    pub composer: Option<String>,
    #[serde(default)]
    pub prompt_suggestion: Option<PromptSuggestion>,
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
    /// How the provider answered recent requests to remove a queued message,
    /// oldest first. A message leaving `queued_messages` does not say whether
    /// it was removed or delivered; this does.
    #[serde(default)]
    pub queue_removals: Vec<QueueRemoval>,
    /// A provider history request remains in flight after its command was
    /// acknowledged. Cached pages do not enter this state.
    #[serde(default)]
    pub history_loading: bool,
    #[serde(default)]
    pub interrupt_mode: Option<String>,
    #[serde(default)]
    pub branch_points: Vec<BranchPoint>,
    #[serde(default)]
    pub forked: Option<String>,
    /// Provider-reported edits in the owned cache, independent of transcript paging.
    #[serde(default)]
    pub file_changes: std::sync::Arc<FileChangesSummary>,
}

/// When a provider takes a message it is holding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueDelivery {
    /// At the provider's next delivery boundary inside the running turn.
    NextStep,
    /// As a turn of its own, once the running one has ended.
    #[default]
    NextTurn,
}

/// The provider's answer to a request to remove a queued message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRemoval {
    pub id: String,
    /// Whether the message left the queue without being delivered.
    pub removed: bool,
}

impl QueueRemoval {
    /// Answers kept for a client that reads snapshots rather than events.
    pub const LIMIT: usize = 32;
}

/// A bounded preview of pending provider input, independent of history pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
    /// Whether `text` and `attachments` are the whole message, so that taking
    /// it back puts in the composer exactly what was queued.
    #[serde(default)]
    pub editable: bool,
    #[serde(default)]
    pub delivery: QueueDelivery,
    /// The files queued with the message, where the provider names them.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchPoint {
    pub id: String,
    pub label: String,
}

/// A denied tool call is advisory evidence, never a pending approval to replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDenial {
    pub tool_use_id: String,
    pub tool_name: String,
    /// Exact JSON, represented as text for the postcard IPC codec.
    pub tool_input: Option<String>,
    pub reason: String,
    pub reason_type: Option<String>,
    pub agent_id: Option<String>,
}

impl PermissionDenial {
    pub const LIMIT: usize = 64;
    pub const INPUT_LIMIT: usize = 64 * 1024;
}

impl QueuedMessage {
    pub const LIMIT: usize = 128;
    pub fn new(mut id: String, mut text: String) -> Self {
        id.truncate(id.floor_char_boundary(256));
        const PREVIEW_BYTES: usize = 16 * 1024;
        let whole = text.len() <= PREVIEW_BYTES;
        if !whole {
            text.truncate(text.floor_char_boundary(PREVIEW_BYTES));
            text.push_str("\n[Queue preview truncated]");
        }
        Self {
            id,
            text,
            editable: whole,
            delivery: QueueDelivery::default(),
            attachments: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    /// The parent conversation's tool-use ID, which also keys child frames.
    pub id: String,
    pub kind: String,
    pub description: String,
    pub status: ToolStatus,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub waiting: bool,
    #[serde(default)]
    pub detail: String,
    /// Whether it runs apart from the turn that started it, so that the turn
    /// ending, or being cancelled, does not of itself end it.
    #[serde(default)]
    pub background: bool,
    pub items: Vec<std::sync::Arc<Item>>,
}

impl Subagent {
    /// Running delegated work that cancelling the parent's turn would also
    /// stop, where the provider's interrupt does not spare it.
    pub fn stopped_by_interrupt(&self, capabilities: &Capabilities) -> bool {
        self.status == ToolStatus::Running
            && self.background
            && capabilities.interrupt_stops_background
    }
}

/// Route transcript content without letting child status, identity or usage
/// overwrite the parent conversation. Adapters keep independent child state.
#[cfg(feature = "host")]
pub(crate) fn child_events(id: &str, events: Vec<Event>) -> Vec<Event> {
    events
        .into_iter()
        .flat_map(|event| match event {
            Event::Item(item) => vec![Event::SubagentItem {
                id: id.into(),
                item,
            }],
            Event::TextDelta {
                id: item_id,
                role,
                thinking,
                delta,
            } => vec![Event::SubagentTextDelta {
                id: id.into(),
                item_id,
                role,
                thinking,
                delta,
            }],
            Event::OutputDelta {
                id: item_id,
                offset,
                delta,
            } => vec![Event::SubagentOutputDelta {
                id: id.into(),
                item_id,
                offset,
                delta,
            }],
            Event::ItemsRemoved { ids } => vec![Event::SubagentItemsRemoved { id: id.into(), ids }],
            Event::History { items, .. } | Event::LatestHistory { items, .. } => items
                .into_iter()
                .map(|item| Event::SubagentItem {
                    id: id.into(),
                    item,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

impl Snapshot {
    /// Parent prompt controls use `busy`; card activity includes delegated work.
    pub fn is_working(&self) -> bool {
        self.busy
            || self
                .subagents
                .iter()
                .any(|agent| agent.status == ToolStatus::Running)
    }
    pub fn activity(&self) -> ActivityState {
        if self.exited {
            ActivityState::Unknown
        } else if self.waiting
            || !self.requests.is_empty()
            || self
                .subagents
                .iter()
                .any(|agent| agent.waiting && agent.status == ToolStatus::Running)
        {
            ActivityState::Waiting
        } else if self.is_working() {
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
    /// Verify login on the same live actor without replaying any failed prompt.
    ResumeAfterLogin {
        failure: String,
        expected: Option<crate::account::Identity>,
    },
    Prompt {
        text: String,
        attachments: Vec<Attachment>,
        mode: PromptMode,
    },
    Interrupt,
    /// Stops one subagent, addressed by [`Subagent::id`], and nothing else:
    /// the parent's turn and the other subagents carry on.
    StopSubagent {
        id: String,
    },
    Answer {
        request_id: String,
        action: String,
        answers: Vec<Answer>,
    },
    SetModel {
        model: String,
        reasoning: Option<String>,
    },
    SetFastMode {
        enabled: bool,
    },
    SetPermissionMode {
        mode: String,
    },
    RefreshPermissions,
    UpdatePermissionSettings {
        update: PermissionSettingsUpdate,
    },
    DismissPermissionDenial {
        tool_use_id: String,
    },
    Compact,
    Refresh,
    LoadEarlier,
    /// Provider slash commands are sent through its own command dispatcher.
    Slash {
        command: String,
    },
    /// Mutate an authoritative provider queue entry, never a transcript item.
    RemoveQueued {
        id: String,
    },
    StartQueued {
        id: String,
    },
    SetInterruptMode {
        mode: String,
    },
    ListBranches,
    Branch {
        id: Option<String>,
    },
    /// Read the next bounded window of already cached transcript history.
    LoadLater,
    /// Read cached provider edit metadata; never asks the provider to do work.
    LoadFileChanges {
        request_id: u64,
        after: Option<FileChangeKey>,
    },
    LoadFileChangePatches {
        request_id: u64,
        key: FileChangeKey,
        cursor: Option<FilePatchCursor>,
        revision: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub field: String,
    pub values: Vec<String>,
}

/// Permission metadata supplied by the provider, without unrelated settings
/// such as environment variables, hooks, or account credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSettings {
    pub rules: Vec<PermissionRule>,
    pub environment: Vec<PermissionEnvironment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    Allow,
    Ask,
    Deny,
}

impl PermissionBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub behavior: PermissionBehavior,
    pub rule: String,
    /// Claude source identity. Only `flagSettings` is editable in this view.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEnvironment {
    pub text: String,
    pub source: String,
}

/// Changes affect this running conversation's flag settings, never files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionSettingsUpdate {
    AddRule {
        behavior: PermissionBehavior,
        rule: String,
    },
    RemoveRule {
        behavior: PermissionBehavior,
        rule: String,
    },
    AddEnvironment {
        text: String,
    },
    RemoveEnvironment {
        text: String,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    Denied,
    /// Ended before it finished because it was asked to stop: cancelled by
    /// the user, or interrupted with the work that started it.
    Stopped,
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
pub enum FileChangeReply {
    Files(FileChangesSummary),
    Patches(FileChangePage),
}

/// Changes belong to the conversation or to an explicitly named delegate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FileChangeSource {
    Conversation,
    Subagent { id: String },
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileChangeKey {
    pub source: FileChangeSource,
    pub path: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeSummary {
    pub ownership_unverified: bool,
    /// Absolute opening pathname from a known provider cwd (or absolute
    /// report). Parent traversal is preserved; this is not filesystem identity.
    pub resolved_path: Option<String>,
    pub revision: u64,
    pub key: FileChangeKey,
    pub kind: ChangeKind,
    pub succeeded: u64,
    pub pending: u64,
    pub failed: u64,
    /// Sum of reported successful patch edits, not a net repository diff.
    /// None when any successful patch lacks recognizable unified hunks.
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    /// Replayed contributions whose ownership this process did not observe.
    /// These may be inherited by a fork; they are not this run's edits.
    pub imported: u64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangesCoverage {
    pub has_unloaded_history: bool,
    pub has_imported_history: bool,
    pub has_incomplete_delegated_history: bool,
    pub has_uncached_changes: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangesSummary {
    pub request_id: u64,
    pub revision: u64,
    pub files: Vec<FileChangeSummary>,
    pub total_files: usize,
    pub applied_files: usize,
    pub has_more: bool,
    pub coverage: FileChangesCoverage,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePatchCursor {
    pub ordinal: i64,
    pub file_index: usize,
    pub offset: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangePatch {
    pub ownership_unverified: bool,
    pub item_id: String,
    pub kind: ChangeKind,
    pub status: ToolStatus,
    pub imported: bool,
    pub offset: usize,
    pub total_bytes: usize,
    pub diff: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangePage {
    pub request_id: u64,
    pub key: FileChangeKey,
    pub revision: u64,
    pub patches: Vec<FileChangePatch>,
    pub next: Option<FilePatchCursor>,
    pub error: Option<String>,
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
        /// What the command was applied to, or the provider's own summary of
        /// it: `src/app.rs` for a read, `TODO in src` for a search, `Run the
        /// tests` for a shell command the provider described. The view puts
        /// the verb `action` implies in front, so the label carries none.
        /// Empty when the provider offered nothing, in which case the view
        /// falls back to `command`.
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
        /// A one-line account of the delivery: the harness's own, such as
        /// `Agent "Review" finished`, or the one a teammate wrote for its
        /// message. Empty when there is none.
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

/// A delivery's heading as plain text. A named sender leads, followed by
/// the summary it wrote, as in `Message from tester: Suite passes`; an
/// unnamed one is described by the harness's own summary where it wrote one,
/// such as `Agent "Review" finished`, and otherwise by its identifier.
pub fn delivery_heading(kind: DeliveryKind, from: &str, sender: &str, summary: &str) -> String {
    match (kind, from.is_empty(), summary.is_empty()) {
        (DeliveryKind::Notice, _, false) | (_, true, false) => summary.into(),
        (DeliveryKind::Notice, _, true) => "Background work changed state".into(),
        (_, false, false) => format!("{} {from}: {summary}", kind.verb()),
        (_, _, true) => format!("{} {}", kind.verb(), delivery_sender(from, sender)),
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
    /// Unix milliseconds when this item was first observed. Unlike `at`,
    /// this remains stable while text and tool output stream. Replayed items
    /// without a known start retain `None`.
    #[serde(default)]
    pub started_at: Option<u64>,
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
/// What the provider is asking the reader to do, independent of its RPC envelope.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestKind {
    /// Authorize a tool action or additional permissions. Never submit on Enter.
    #[default]
    Approval,
    /// Answer an ordinary question, including a yes/no confirmation.
    Question,
    /// Provider-managed elicitation or consent. Explicit decisions remain required.
    Consent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Old JSON snapshots remain conservative; postcard peers use a matched version.
    #[serde(default)]
    pub kind: RequestKind,
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
    #[serde(default)]
    pub fast_mode: bool,
    pub permission_modes: Vec<String>,
    pub skill_reach: SkillReach,
    #[serde(default)]
    pub queue_remove: bool,
    #[serde(default)]
    pub queue_start: bool,
    #[serde(default)]
    pub interrupt_modes: Vec<String>,
    #[serde(default)]
    pub branch: bool,
    /// Whether [`Command::StopSubagent`] is carried by the provider.
    #[serde(default)]
    pub stop_subagent: bool,
    /// Whether the provider's interrupt also stops subagents running in the
    /// background, rather than only the turn and the work inside it.
    #[serde(default)]
    pub interrupt_stops_background: bool,
}
/// Provider-confirmed speed setting, independent of model reasoning effort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastMode {
    pub state: FastModeState,
    pub disabled_reason: Option<String>,
}
impl FastMode {
    pub fn enabled(&self) -> bool {
        matches!(self.state, FastModeState::On | FastModeState::Cooldown)
    }
    pub fn changeable(&self) -> bool {
        self.enabled()
            || self
                .disabled_reason
                .as_deref()
                .is_none_or(|reason| matches!(reason, "preference" | "sdk_opt_in_required"))
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastModeState {
    #[default]
    Off,
    On,
    Cooldown,
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
    /// API-equivalent estimate when the provider does not report a cost.
    #[serde(default)]
    pub cost_estimate: Option<pricing::CostEstimate>,
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
    /// Correlated account/read proof; runtime fences the captured auth failure.
    LoginRecovered {
        failure: String,
    },
    /// A failed verification is fenced to the same captured auth failure too.
    LoginVerificationFailed {
        failure: String,
        detail: String,
    },
    /// Providers correlate this to their latest completed turn before emitting
    /// it. The actor adds its monotonic turn generation at publication.
    PromptSuggestion {
        session_id: String,
        source: String,
        text: String,
    },
    PromptSuggestionCleared,

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
    PermissionSettings(PermissionSettings),
    PermissionDenied(PermissionDenial),
    PermissionDenialDismissed {
        tool_use_id: String,
    },
    Models {
        models: Vec<Model>,
        selected: Option<String>,
        reasoning: Option<String>,
    },
    FastMode(FastMode),
    Commands(Vec<SlashCommand>),
    Skills(Vec<Skill>),
    /// Upsert a completed/history item without duplicating a streamed item.
    Item(Item),
    /// An append to a tracked command/tool output, never an authoritative rewrite.
    OutputDelta {
        id: String,
        offset: usize,
        delta: String,
    },
    /// A sparse update: empty text and `None` leave what is known in place.
    Subagent {
        id: String,
        kind: String,
        description: String,
        status: Option<ToolStatus>,
        background: Option<bool>,
        model: Option<String>,
        waiting: Option<bool>,
        detail: Option<String>,
    },
    SubagentItem {
        id: String,
        item: Item,
    },
    SubagentOutputDelta {
        id: String,
        item_id: String,
        offset: usize,
        delta: String,
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
    /// The actor can page saved history but cannot execute provider commands.
    ReadOnlyHistory {
        reason: String,
    },
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
    QueueRemoval(QueueRemoval),
    /// Published until the provider has answered the asynchronous history
    /// request, independently of the command acknowledgement.
    HistoryLoading(bool),
    /// A reread of the newest provider page, which may contain entirely new
    /// items after missed live notifications. Disjoint older pages prepend;
    /// disjoint latest pages append.
    LatestHistory {
        items: Vec<Item>,
        has_earlier: bool,
    },
    /// Establish the cached/live boundary before an asynchronous newest-page
    /// request. Its response may arrive after newer live items.
    LatestHistoryStarted,
    /// A retryable history RPC failure, independent of turn failures.
    HistoryError(String),
    /// A successful cached/provider page can recover its own active error.
    HistoryRecovered,
    InterruptMode(String),
    BranchPoints(Vec<BranchPoint>),
    Forked(String),
    /// An explicit maintenance boundary, separate from prompt completion.
    /// A generic idle status cannot release accepted prompt admission.
    Compaction { active: bool },
}

#[cfg(test)]
mod tests {
    #[test]
    fn request_kind_roundtrips_and_legacy_json_requires_explicit_approval() {
        let legacy = serde_json::json!({"id":"request","title":"Prompt","description":"","fields":[],"actions":[]});
        let mut request: super::Request = serde_json::from_value(legacy).unwrap();
        assert_eq!(request.kind, super::RequestKind::Approval);
        for kind in [
            super::RequestKind::Approval,
            super::RequestKind::Question,
            super::RequestKind::Consent,
        ] {
            request.kind = kind;
            let bytes = postcard::to_stdvec(&request).unwrap();
            assert_eq!(
                postcard::from_bytes::<super::Request>(&bytes).unwrap(),
                request
            );
            let json = serde_json::to_value(&request).unwrap();
            assert_eq!(
                serde_json::from_value::<super::Request>(json).unwrap(),
                request
            );
        }
    }

    #[test]
    fn older_subagent_snapshots_default_to_unknown_model_and_child_waiting_is_visible() {
        let mut agent: super::Subagent = serde_json::from_value(serde_json::json!({
            "id":"child", "kind":"Codex agent", "description":"", "status":"Running", "items":[]
        }))
        .unwrap();
        assert_eq!(agent.model, None);
        assert!(!agent.waiting);
        assert!(agent.detail.is_empty());
        agent.waiting = true;
        let mut snapshot = super::Snapshot::default();
        snapshot.subagents.push(agent);
        assert_eq!(snapshot.activity(), crate::agent::ActivityState::Waiting);
        snapshot.subagents[0].waiting = false;
        assert_eq!(snapshot.activity(), crate::agent::ActivityState::Working);
        snapshot.subagents[0].status = super::ToolStatus::Succeeded;
        assert_eq!(snapshot.activity(), crate::agent::ActivityState::Idle);
    }

    #[test]
    fn delegated_work_keeps_card_active_without_blocking_parent_prompts() {
        let mut snapshot = super::Snapshot::default();
        snapshot.subagents.push(super::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: "child".into(),
            kind: String::new(),
            description: String::new(),
            status: super::ToolStatus::Running,
            background: false,
            items: Vec::new(),
        });
        assert!(!snapshot.busy);
        assert!(snapshot.is_working());
        assert_eq!(snapshot.activity(), super::ActivityState::Working);
        snapshot.subagents[0].status = super::ToolStatus::Succeeded;
        assert_eq!(snapshot.activity(), super::ActivityState::Idle);
    }

    use super::*;

    #[test]
    fn delivery_headings_name_the_sender_before_any_summary() {
        use DeliveryKind::*;
        assert_eq!(
            delivery_heading(Message, "tester", "", "CI is down"),
            "Message from tester: CI is down"
        );
        assert_eq!(
            delivery_heading(Report, "", "a17925ac2cee519e2", ""),
            "Report from agent a17925ac"
        );
        assert_eq!(
            delivery_heading(Report, "", "a1", "Agent \"Review\" finished"),
            "Agent \"Review\" finished"
        );
        assert_eq!(
            delivery_heading(Notice, "", "", ""),
            "Background work changed state"
        );
    }

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
    #[cfg(any(feature = "host", feature = "ui"))]
    fn managed_claude_accounts_follow_the_current_global_permission_default() {
        let directory = tempfile::tempdir().unwrap();
        let homes = crate::account::home::Homes::at(directory.path().join("accounts"));
        let account = homes.path(Provider::Claude, uuid::Uuid::new_v4());
        let system = directory.path().join("system");
        let project = directory.path().join("project");
        for path in [&account, &system, &project] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            system.join("settings.json"),
            r#"{"permissions":{"defaultMode":"auto"}}"#,
        )
        .unwrap();
        let mut recipe = LaunchRecipe {
            unset_environment: ["HOME".into()].into(),
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: project.clone(),
            environment: [("CLAUDE_CONFIG_DIR".into(), account.display().to_string())].into(),
        };
        let resolve = |recipe: &LaunchRecipe| {
            claude_configured_default_mode_in(recipe, Some(&homes), Some(&system))
        };
        assert_eq!(resolve(&recipe).as_deref(), Some("auto"));
        for settings in [
            r#"{"theme":"dark"}"#,
            r#"{"permissions":{"defaultMode":"plan"}}"#,
            r#"{"permissions":{"defaultMode":"default"}}"#,
            r#"{"permissions":{"defaultMode":"unknown"}}"#,
            r#"{"permissions":{"defaultMode":null}}"#,
            "malformed",
        ] {
            std::fs::write(account.join("settings.json"), settings).unwrap();
            assert_eq!(resolve(&recipe).as_deref(), Some("auto"), "{settings}");
        }
        for (settings, expected) in [
            (
                r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
                Some("bypassPermissions"),
            ),
            (r#"{"permissions":{"defaultMode":"plan"}}"#, Some("plan")),
            (
                r#"{"permissions":{"defaultMode":"default"}}"#,
                Some("manual"),
            ),
            ("{}", Some("manual")),
            (
                r#"{"permissions":{"defaultMode":"unknown"}}"#,
                Some("manual"),
            ),
            ("malformed", Some("manual")),
        ] {
            std::fs::write(system.join("settings.json"), settings).unwrap();
            assert_eq!(resolve(&recipe).as_deref(), expected, "{settings}");
        }
        std::fs::write(
            account.join("settings.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        std::fs::remove_file(system.join("settings.json")).unwrap();
        std::fs::create_dir(system.join("settings.json")).unwrap();
        assert_eq!(resolve(&recipe).as_deref(), Some("manual"));
        std::fs::remove_dir(system.join("settings.json")).unwrap();
        assert_eq!(resolve(&recipe).as_deref(), Some("manual"));
        recipe
            .environment
            .insert("CLAUDE_CODE_PERMISSION_MODE".into(), "auto".into());
        assert_eq!(resolve(&recipe), None);
        recipe
            .unset_environment
            .insert("CLAUDE_CODE_PERMISSION_MODE".into());
        assert_eq!(resolve(&recipe).as_deref(), Some("manual"));
        recipe.environment.remove("CLAUDE_CODE_PERMISSION_MODE");
        recipe
            .unset_environment
            .remove("CLAUDE_CODE_PERMISSION_MODE");
        std::fs::write(
            system.join("settings.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        std::fs::write(account.join("settings.json"), "{}").unwrap();
        std::fs::create_dir(project.join(".claude")).unwrap();
        std::fs::write(
            project.join(".claude/settings.local.json"),
            r#"{"permissions":{"defaultMode":"plan"}}"#,
        )
        .unwrap();
        assert_eq!(resolve(&recipe), None);
        std::fs::remove_file(project.join(".claude/settings.local.json")).unwrap();
        recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            directory.path().join("linked").display().to_string(),
        );
        assert_eq!(resolve(&recipe), None);
        let linked = directory.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(
            linked.join("settings.json"),
            r#"{"permissions":{"defaultMode":"plan"}}"#,
        )
        .unwrap();
        assert_eq!(resolve(&recipe).as_deref(), Some("plan"));
        recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            homes
                .path(Provider::Codex, uuid::Uuid::new_v4())
                .display()
                .to_string(),
        );
        assert_eq!(resolve(&recipe), None);
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
        && !(crate::device::is_remote()
            && matches!(account.home, crate::account::AccountHome::System))
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
            // configured user choice before the first native prompt. Managed
            // logins follow the current global default; independent homes
            // retain their own. Explicit environment choices and project
            // settings remain under Claude's own handling.
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
    if crate::device::is_remote() {
        return None;
    }
    let homes = crate::account::home::Homes::installation();
    let system_home = crate::account::Account::system(Provider::Claude).resolved_home();
    claude_configured_default_mode_in(recipe, homes.as_ref(), system_home.as_deref())
}

#[cfg(any(feature = "host", feature = "ui"))]
fn claude_configured_default_mode_in(
    recipe: &LaunchRecipe,
    homes: Option<&crate::account::home::Homes>,
    system_home: Option<&std::path::Path>,
) -> Option<String> {
    fn setting(path: &std::path::Path) -> Result<Option<String>, ()> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(()),
        };
        let settings: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
        let settings = settings.as_object().ok_or(())?;
        let Some(permissions) = settings.get("permissions") else {
            return Ok(None);
        };
        let Some(value) = permissions.as_object().ok_or(())?.get("defaultMode") else {
            return Ok(None);
        };
        let mode = value.as_str().ok_or(())?;
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
        .then(|| Some(mode.to_owned()))
        .ok_or(())
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
    if environment("CLAUDE_CODE_PERMISSION_MODE").is_some() {
        // This explicit choice outranks settings; do not pin a CLI default
        // during fresh creation or when converting an older launch.
        return None;
    }
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
                && setting(&directory.join(".claude/settings.local.json"))
                    .ok()
                    .flatten()
                    .is_some()
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
                setting(&path).ok().flatten().is_some()
            })
        {
            // A project-specific choice belongs to Claude's own settings
            // handling, not to a command-line override from another account.
            return None;
        }
    }

    // A managed home isolates a login, not the user's permission preference.
    // Its settings were copied when the account was created and may now be
    // stale. Read the current system choice; explicit launch and project
    // choices still take precedence. Linked homes keep independent settings.
    if homes.is_some_and(|homes| {
        homes
            .managed_identity(&root)
            .is_some_and(|(provider, _)| provider == Provider::Claude)
    }) {
        let mode = system_home
            .and_then(|home| setting(&home.join("settings.json")).ok())
            .flatten();
        // Pin Manual too when the global choice is absent/default, otherwise
        // Claude would fall back to a stale managed-home permission default.
        // Invalid/unreadable global settings must not resurrect stale YOLO.
        return Some(match mode {
            Some(mode) if mode != "default" => mode,
            _ => "manual".to_owned(),
        });
    }

    // Local settings belong to a project; a settings.local.json beside the
    // account's user settings is not a user configuration layer.
    setting(&root.join("settings.json"))
        .ok()?
        .filter(|mode| mode != "default")
}
