//! Provider JSON protocols used by daemon-owned pipe and Unix WebSocket transports.
use super::{Command, Event, Launch};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

pub mod claude;
mod claude_edits;
mod claude_permissions;
mod claude_titles;
pub mod codex;
mod image_references;
pub mod omp;
mod terminal_output;

#[derive(Debug, Clone)]
pub enum TransportSpec {
    /// Page an exact provider transcript without starting a provider process.
    History,
    JsonLines,
    /// A supervised provider server that both native and CLI views can join.
    OwnedUnixWebSocket {
        socket: PathBuf,
    },
    /// Join only the exact socket checked by the provider adapter.
    UnixWebSocket {
        socket: PathBuf,
        device: u64,
        inode: u64,
    },
}

#[derive(Clone)]
pub struct ProcessSpec {
    /// Private spawn evidence and an optional caller-validated directory. Never
    /// serialized into launch recipes or provider configuration.
    pub(crate) execution: Option<std::sync::Arc<ExecutionContext>>,
    /// Recognizes the messages the daemon delivered among the prompts the
    /// transcript records, so they are shown as messages rather than as the
    /// user's own words.
    pub(crate) messages: Option<std::sync::Arc<crate::agent_execution::mailbox::Recognizer>>,
    pub transport: TransportSpec,
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub unset_environment: std::collections::BTreeSet<String>,
}
impl std::fmt::Debug for ProcessSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessSpec")
            .field("transport", &self.transport)
            .field("program", &self.program)
            .field("arguments", &self.arguments)
            .field("working_directory", &self.working_directory)
            .field("environment", &"<redacted>")
            .finish()
    }
}
impl From<&crate::model::LaunchRecipe> for ProcessSpec {
    fn from(recipe: &crate::model::LaunchRecipe) -> Self {
        Self {
            execution: None,
            messages: None,
            transport: TransportSpec::JsonLines,
            program: recipe.program.clone(),
            arguments: recipe.arguments.clone(),
            working_directory: recipe.working_directory.clone(),
            environment: recipe.environment.clone(),
            unset_environment: crate::account::launch_credential_overrides(recipe),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LaunchOutcome {
    #[default]
    NoProvider,
    TeardownProved,
    Uncertain,
}

/// An exact owner, retained until startup is installed or cleanup is proved.
/// Implementations must be bounded and idempotent; a caller timeout or an
/// unverified process exit is not a teardown receipt.
pub(crate) trait LaunchCleanup: Send + Sync {
    fn cleanup(&self) -> Result<LaunchOutcome, String>;
}

#[derive(Default)]
struct LaunchOwnership {
    outcome: LaunchOutcome,
    owner: Option<std::sync::Arc<dyn LaunchCleanup>>,
}

#[derive(Default)]
pub(crate) struct ExecutionContext {
    pub directory: Option<std::sync::Arc<std::fs::File>>,
    expected_directory: Option<crate::agent_execution::DirectoryIdentity>,
    // Only a once-claimed fresh acceptance may retain the holder's original
    // spawn FD. These private cells are never reconstructed from a pathname.
    fresh_native_session: std::sync::OnceLock<crate::model::TerminalId>,
    fresh_spawn_claimed: std::sync::OnceLock<Result<(), String>>,
    fresh_spawn_directory: std::sync::OnceLock<std::sync::Arc<std::fs::File>>,
    ownership: std::sync::Mutex<LaunchOwnership>,
    provider_directory: std::sync::Mutex<Option<PathBuf>>,
}

impl ExecutionContext {
    /// Pin the selected directory before an ordinary native provider starts.
    /// The descriptor remains authoritative after a same-filesystem move;
    /// opening a replacement at the old spelling never changes its identity.
    pub(crate) fn capture(path: &std::path::Path) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let relative = path
            .strip_prefix("/")
            .map_err(|_| "The selected provider directory must be absolute".to_owned())?;
        let root = crate::fs_safety::Root::open("/".as_ref()).map_err(|error| error.to_string())?;
        let directory = if relative.as_os_str().is_empty() {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")
                .map_err(|error| error.to_string())
        } else {
            crate::fs_safety::open_beneath(&root, relative).map_err(|error| error.to_string())
        }
        .map_err(|error| {
            format!(
                "Cannot pin the selected provider directory {}: {error}",
                path.display()
            )
        })?;
        if !directory
            .metadata()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            return Err("The selected provider directory is not a folder".into());
        }
        let context = Self::new(Some(std::sync::Arc::new(directory)));
        context.verify_directory(path)?;
        Ok(context)
    }

    pub(crate) fn new(directory: Option<std::sync::Arc<std::fs::File>>) -> Self {
        Self {
            directory,
            ..Default::default()
        }
    }
    pub(crate) fn restored(identity: crate::agent_execution::DirectoryIdentity) -> Self {
        Self {
            expected_directory: Some(identity),
            ..Default::default()
        }
    }
    pub(crate) fn directory_identity(
        &self,
    ) -> Result<Option<crate::agent_execution::DirectoryIdentity>, String> {
        match self.directory_descriptor() {
            Some(directory) => directory
                .metadata()
                .map(|metadata| {
                    Some(crate::agent_execution::DirectoryIdentity::from_metadata(
                        &metadata,
                    ))
                })
                .map_err(|error| error.to_string()),
            None => Ok(self.expected_directory),
        }
    }
    pub(crate) fn directory_descriptor(&self) -> Option<std::sync::Arc<std::fs::File>> {
        self.directory
            .clone()
            .or_else(|| self.fresh_spawn_directory.get().cloned())
    }
    /// Called only after the accepted fresh session has been allocated and its
    /// once-only dispatch claim committed. A new helper is not itself admission.
    pub(crate) fn admit_fresh_native_directory(
        &self,
        source: &super::Session,
    ) -> Result<(), String> {
        if source.launch.provider != crate::agent::Provider::Codex
            || source.launch.session_id.is_some()
            || source.native_continuity != super::NativeContinuity::ReconnectOnly
            || self.expected_directory.is_some()
        {
            return Err("Only a fresh accepted conversation may pin a new spawn directory".into());
        }
        self.fresh_native_session
            .set(source.id)
            .map_err(|_| "Fresh native directory admission was already issued".into())
    }
    /// Every marked native spawn, including a context that already has a pin,
    /// must claim this exact session once before selecting a directory or child.
    pub(crate) fn claim_fresh_native_spawn(&self, source: &super::Session) -> Result<bool, String> {
        let Some(session) = self.fresh_native_session.get() else {
            return Ok(false);
        };
        let admission = if *session != source.id
            || source.launch.session_id.is_some()
            || source.launch.provider != crate::agent::Provider::Codex
            || self.expected_directory.is_some()
        {
            Err("The native spawn does not belong to this fresh acceptance".to_owned())
        } else {
            Ok(())
        };
        self.fresh_spawn_claimed
            .set(admission.clone())
            .map_err(|_| "The fresh native spawn attempt was already consumed".to_owned())?;
        admission.map(|()| true)
    }
    /// Retain the actual FD already selected by this claimed native spawn.
    /// Legacy/recovered contexts cannot mint authority from a current pathname.
    pub(crate) fn retain_fresh_spawn_directory(
        &self,
        source: &super::Session,
        directory: std::sync::Arc<std::fs::File>,
    ) -> Result<(), String> {
        if self.fresh_native_session.get() != Some(&source.id)
            || !matches!(self.fresh_spawn_claimed.get(), Some(Ok(())))
            || source.launch.session_id.is_some()
            || source.launch.provider != crate::agent::Provider::Codex
            || self.expected_directory.is_some()
            || self.directory.is_some()
        {
            return Err(
                "The original spawn directory does not belong to this fresh acceptance".into(),
            );
        }
        let identity = crate::agent_execution::DirectoryIdentity::from_metadata(
            &directory.metadata().map_err(|error| error.to_string())?,
        );
        validate_launch_directory(&directory, identity)?;
        self.fresh_spawn_directory
            .set(directory)
            .map_err(|_| "The original native spawn directory is already pinned".to_owned())?;
        self.verify_directory(&source.launch.recipe.working_directory)
    }
    /// Return the descriptor used by spawn, never a new lookup of its pathname.
    pub(crate) fn pinned_launch_directory(
        &self,
    ) -> Result<
        (
            crate::agent_execution::DirectoryIdentity,
            std::sync::Arc<std::fs::File>,
        ),
        String,
    > {
        let directory = self
            .directory_descriptor()
            .ok_or("This provider has no original launch-directory descriptor")?;
        let identity = directory.metadata().map_err(|error| error.to_string())?;
        let identity = crate::agent_execution::DirectoryIdentity::from_metadata(&identity);
        validate_launch_directory(&directory, identity)?;
        if self
            .expected_directory
            .is_some_and(|expected| expected != identity)
        {
            return Err("Original launch-directory identity changed".into());
        }
        Ok((identity, directory))
    }
    /// Claim before asynchronous work may spawn, or synchronously at child
    /// creation when the caller itself performs the entire spawn attempt.
    pub(crate) fn claim_owner(&self, owner: std::sync::Arc<dyn LaunchCleanup>) {
        *self.ownership.lock().unwrap_or_else(|p| p.into_inner()) = LaunchOwnership {
            outcome: LaunchOutcome::Uncertain,
            owner: Some(owner),
        };
    }
    pub(crate) fn outcome(&self) -> LaunchOutcome {
        self.ownership
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .outcome
    }
    /// The host's exact actor record now owns cleanup; do not retain a second
    /// strong Handle through the actor's own execution context.
    pub(crate) fn installed(&self) {
        self.ownership
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .owner = None;
    }
    pub(crate) fn reconcile_cleanup(&self) -> Result<LaunchOutcome, String> {
        let owner = {
            let state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            match &state.owner {
                Some(owner) => owner.clone(),
                None => return Ok(state.outcome),
            }
        };
        let outcome = owner.cleanup()?;
        let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
        // A worker may have transferred ownership to a helper while the caller
        // was joining it. Its receipt cannot prove that new owner's teardown.
        if state
            .owner
            .as_ref()
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, &owner))
            && outcome != LaunchOutcome::Uncertain
        {
            state.outcome = outcome;
            state.owner = None;
        }
        Ok(state.outcome)
    }
    /// Minted only after typed preparation has pinned the private launch and
    /// updated the adapter before initialize. A process cwd alone is no proof
    /// of a resumed provider thread's operational directory.
    pub(crate) fn observe_provider_directory(&self, path: &std::path::Path) -> Result<(), String> {
        if self.directory_descriptor().is_none() {
            return Err("Provider scope requires its admitted directory descriptor".into());
        }
        self.verify_directory(path)?;
        *self
            .provider_directory
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(path.to_owned());
        Ok(())
    }
    pub(crate) fn provider_directory_observed(
        &self,
        path: &std::path::Path,
    ) -> Result<bool, String> {
        if self
            .provider_directory
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_deref()
            != Some(path)
        {
            return Ok(false);
        }
        self.verify_directory(path)?;
        Ok(true)
    }
    pub(crate) fn verify_directory(&self, path: &std::path::Path) -> Result<(), String> {
        let Some(expected) = self.directory_identity()? else {
            return Ok(());
        };
        let current = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !current.is_dir()
            || expected != crate::agent_execution::DirectoryIdentity::from_metadata(&current)
        {
            return Err("The approved agent directory changed before provider dispatch".into());
        }
        Ok(())
    }
}
/// Validate both local and received pins without reopening a filesystem path.
pub(crate) fn validate_launch_directory(
    directory: &std::fs::File,
    expected: crate::agent_execution::DirectoryIdentity,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let metadata = directory.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || crate::agent_execution::DirectoryIdentity::from_metadata(&metadata) != expected
    {
        return Err("Launch-directory descriptor does not match its original identity".into());
    }
    // Received descriptors must not escape through a subsequent provider exec.
    let flags = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 || flags & libc::FD_CLOEXEC == 0 {
        return Err("Launch-directory descriptor is not close-on-exec".into());
    }
    Ok(())
}

impl ProcessSpec {
    pub(crate) fn remove_unset_environment(&mut self) {
        self.environment
            .retain(|name, _| !self.unset_environment.contains(name));
    }
}
#[derive(Debug, Default)]
pub struct Step {
    /// The decoder has not qualified this frame as the current turn. Its
    /// transcript and exact managed execution evidence remain valid, but it
    /// cannot replace the current conversation's notification context/error.
    /// This transport-local flag never enters the shared wire codec.
    pub suppress_current_notification: bool,
    /// Bounded pre-acknowledgement context selected by the exact accepted
    /// provider turn. Restores only notification metadata, never transcript
    /// deltas that would otherwise be appended a second time.
    pub notification_preview: Option<super::notification::Preview>,
    /// Written in order before the next incoming frame is processed.
    pub writes: Vec<Value>,
    pub events: Vec<Event>,
}
/// Account identity verified during initialize on the actual server connection.
/// A holder can retain it when the account directory is subsequently removed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerIdentity {
    pub user_agent: String,
    pub canonical_home: PathBuf,
    pub reported_home: String,
}

/// Advisory preparation for a native resume. The runtime rechecks authority at
/// the actual write boundary; this value never authorizes a write by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GoalResumeAdmission {
    OriginalScope,
    /// An original legacy actor may resume only after a side-effect-free read
    /// positively establishes that its goal cannot continue automatically.
    InactiveOnly,
    #[default]
    Unavailable,
}
/// An adapter is confined to one daemon worker. No UI state, subprocesses,
/// unbounded transcript storage, credentials, or global mutable state live here.
pub trait Adapter: Send {
    /// Wake an otherwise idle actor for bounded local intent bookkeeping.
    /// Expiry must not pretend an unresolved provider RPC stopped executing.
    fn next_deadline(&self) -> Option<std::time::Instant> {
        None
    }
    fn on_deadline(&mut self, _now: std::time::Instant) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn set_goal_resume_admission(&mut self, _admission: GoalResumeAdmission) {}
    /// Private launch selection, distinct from a provider-confirmed session.
    /// Only a provider with a retained bootstrap contract supplies this value.
    fn prepared_launch_identity(&self) -> Option<claude::PreparedIdentity> {
        None
    }
    fn initialize(&mut self) -> Result<Step, String>;
    fn command(&mut self, command: Command) -> Result<Step, String>;
    /// Whether this exact pending answer refuses work. Providers with opaque
    /// action IDs must inspect their retained decision, never its UI label.
    fn answer_declines(&self, _request_id: &str, action: &str) -> bool {
        super::declines(action)
    }
    fn command_for_run(
        &mut self,
        _run: crate::model::RunId,
        _command: Command,
    ) -> Result<Step, String> {
        Err("This provider transport does not support correlated agent runs".into())
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String>;
    /// Identity reported by the initialized server on this exact connection.
    /// Only a holder with a verified provider contract can use it for durable
    /// ownership; an executable's separate `--version` output is insufficient.
    fn server_identity(&self) -> Option<&ServerIdentity> {
        None
    }
}
pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    launch.provider.create_chat(launch)
}

/// Reconstruct a saved reader without granting permission to launch a provider.
/// Support is deliberately limited to adapters with a local history transport.
pub(crate) fn create_history(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    match launch.provider {
        crate::agent::Provider::Claude => claude::create_history(launch),
        _ => {
            Err("This provider does not support restoring a local read-only history reader".into())
        }
    }
}

/// Record a provider-confirmed conversation rotation in the launch recipe.
///
/// Upgrade checkpointing stamps the live confirmed identity onto
/// `session_id`, but the recipe still names the originally launched resume
/// ID — a checkpoint that neither validates nor restarts the final
/// conversation. This rewrites the provider's resume arguments to the
/// confirmed identity. Only call with an identity the live provider
/// reported, never user input: strict `create()` forgery checks stay intact
/// everywhere else. OMP needs nothing: it derives `--resume` from the
/// session ID when creating the process spec.
pub fn align_resume_identity(launch: &mut Launch) -> Result<(), String> {
    let Some(id) = launch.session_id.clone() else {
        return Ok(());
    };
    match launch.provider {
        crate::agent::Provider::Claude => {
            claude::align_resume_identity(&mut launch.recipe.arguments, &id)
        }
        crate::agent::Provider::Codex => {
            codex::align_resume_identity(&mut launch.recipe.arguments, &id)
        }
        crate::agent::Provider::Omp => Ok(()),
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Cleanup(Mutex<LaunchOutcome>);
    impl LaunchCleanup for Cleanup {
        fn cleanup(&self) -> Result<LaunchOutcome, String> {
            Ok(*self.0.lock().unwrap())
        }
    }

    #[test]
    fn provider_directory_observation_requires_a_pinned_exact_launch_directory() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let other = root.path().join("other");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&other).unwrap();
        let context = ExecutionContext::capture(&original).unwrap();
        let identity = context.directory_identity().unwrap().unwrap();
        assert!(!context.provider_directory_observed(&original).unwrap());
        assert!(context.observe_provider_directory(&other).is_err());
        assert!(!context.provider_directory_observed(&other).unwrap());
        context.observe_provider_directory(&original).unwrap();
        assert!(context.provider_directory_observed(&original).unwrap());
        assert!(!context.provider_directory_observed(&other).unwrap());
        assert!(
            ExecutionContext::default()
                .observe_provider_directory(&original)
                .is_err()
        );
        assert!(
            ExecutionContext::restored(identity)
                .observe_provider_directory(&original)
                .is_err()
        );

        // Renaming preserves the retained descriptor but never grants authority
        // to the replacement at the original spelling or observes a new path.
        let moved = root.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        assert_eq!(context.directory_identity().unwrap(), Some(identity));
        assert!(context.provider_directory_observed(&original).is_err());
        assert!(!context.provider_directory_observed(&moved).unwrap());
    }

    #[test]
    fn execution_directory_capture_refuses_relative_paths_files_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, b"fixture").unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();
        assert!(ExecutionContext::capture(std::path::Path::new("relative")).is_err());
        assert!(ExecutionContext::capture(&file).is_err());
        assert!(ExecutionContext::capture(&alias).is_err());
        assert!(ExecutionContext::capture(&alias.join("file")).is_err());
        assert!(ExecutionContext::capture(std::path::Path::new("/")).is_ok());
    }

    #[test]
    fn fresh_native_spawn_pin_is_once_only_and_preserves_existing_cleanup_custody() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("original");
        std::fs::create_dir(&path).unwrap();
        let source = super::super::Session {
            id: crate::model::TerminalId::new(),
            launch: super::super::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: crate::model::LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: path.clone(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let pin = ExecutionContext::capture(&path)
            .unwrap()
            .directory_descriptor()
            .unwrap();
        let identity =
            crate::agent_execution::DirectoryIdentity::from_metadata(&pin.metadata().unwrap());
        let legacy = ExecutionContext::new(None);
        assert!(!legacy.claim_fresh_native_spawn(&source).unwrap());
        assert!(
            legacy
                .retain_fresh_spawn_directory(&source, pin.clone())
                .is_err()
        );
        assert!(
            legacy.directory_descriptor().is_none(),
            "a new holder alone is not fresh admission"
        );
        let restored = ExecutionContext::restored(identity);
        assert!(restored.admit_fresh_native_directory(&source).is_err());
        assert!(!restored.claim_fresh_native_spawn(&source).unwrap());
        assert!(
            restored
                .retain_fresh_spawn_directory(&source, pin.clone())
                .is_err()
        );
        assert!(restored.directory_descriptor().is_none());
        let mut resumed = source.clone();
        resumed.launch.session_id = Some("original-thread".into());
        assert!(
            ExecutionContext::new(None)
                .admit_fresh_native_directory(&resumed)
                .is_err()
        );
        let context = Arc::new(ExecutionContext::new(None));
        let same_context = context.clone();
        let cleanup = Arc::new(Cleanup(Mutex::new(LaunchOutcome::Uncertain)));
        context.claim_owner(cleanup.clone());
        context.admit_fresh_native_directory(&source).unwrap();
        assert!(context.admit_fresh_native_directory(&source).is_err());
        assert!(context.claim_fresh_native_spawn(&source).unwrap());
        context
            .retain_fresh_spawn_directory(&source, pin.clone())
            .unwrap();
        assert!(Arc::ptr_eq(&context, &same_context));
        assert!(Arc::ptr_eq(&pin, &context.directory_descriptor().unwrap()));
        assert_eq!(context.pinned_launch_directory().unwrap().0, identity);
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::Uncertain
        );
        assert!(
            context.claim_fresh_native_spawn(&source).is_err(),
            "even a populated original pin cannot authorize another spawn"
        );
        let mut another_session = source.clone();
        another_session.id = crate::model::TerminalId::new();
        assert!(context.claim_fresh_native_spawn(&another_session).is_err());
        assert!(
            context
                .retain_fresh_spawn_directory(&source, pin.clone())
                .is_err()
        );
        // Renaming/replacement never changes the retained original FD.
        std::fs::rename(&path, root.path().join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(context.directory_identity().unwrap(), Some(identity));
        assert!(context.verify_directory(&path).is_err());
        assert!(
            context
                .retain_fresh_spawn_directory(
                    &source,
                    Arc::new(std::fs::File::open(&path).unwrap())
                )
                .is_err()
        );
        assert!(Arc::ptr_eq(&pin, &context.directory_descriptor().unwrap()));
        *cleanup.0.lock().unwrap() = LaunchOutcome::TeardownProved;
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
        assert_eq!(Arc::strong_count(&cleanup), 1);
        // A wrong session consumes its attempt, without installing any FD.
        let invalid = ExecutionContext::new(None);
        invalid.admit_fresh_native_directory(&source).unwrap();
        let mut other = source.clone();
        other.id = crate::model::TerminalId::new();
        assert!(invalid.claim_fresh_native_spawn(&other).is_err());
        assert!(invalid.claim_fresh_native_spawn(&source).is_err());
        assert!(invalid.retain_fresh_spawn_directory(&source, pin).is_err());
        assert!(invalid.directory_descriptor().is_none());
    }

    #[test]
    fn uncertain_cleanup_retains_owner_until_its_receipt_proves_teardown() {
        let context = ExecutionContext::default();
        assert_eq!(context.outcome(), LaunchOutcome::NoProvider);
        let owner = Arc::new(Cleanup(Mutex::new(LaunchOutcome::Uncertain)));
        context.claim_owner(owner.clone());
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::Uncertain
        );
        *owner.0.lock().unwrap() = LaunchOutcome::TeardownProved;
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
        assert_eq!(Arc::strong_count(&owner), 1);
    }

    #[test]
    fn old_worker_receipt_cannot_clear_transferred_helper_ownership() {
        struct Transfer(std::sync::Weak<ExecutionContext>, Arc<Cleanup>);
        impl LaunchCleanup for Transfer {
            fn cleanup(&self) -> Result<LaunchOutcome, String> {
                self.0.upgrade().unwrap().claim_owner(self.1.clone());
                Ok(LaunchOutcome::TeardownProved)
            }
        }
        let context = Arc::new(ExecutionContext::default());
        let helper = Arc::new(Cleanup(Mutex::new(LaunchOutcome::Uncertain)));
        context.claim_owner(Arc::new(Transfer(Arc::downgrade(&context), helper.clone())));
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::Uncertain
        );
        *helper.0.lock().unwrap() = LaunchOutcome::TeardownProved;
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
    }
}
