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

    /// Request ordinary cancellation without waiting for a worker or child.
    fn request_ordinary_cleanup(&self) {}

    /// Only implementations with synchronized real progress may report proof.
    fn poll_cleanup(&self) -> LaunchOutcome {
        LaunchOutcome::Uncertain
    }

    fn original_helper_wait_proof(&self) -> HelperWaitProof {
        HelperWaitProof::NoParentChild
    }

    fn retained_handle(&self) -> Option<super::runtime::Handle> {
        None
    }
    fn cleanup_diagnostic(&self) -> Option<String> {
        None
    }
    #[cfg(test)]
    fn cleanup_waiters_for_test(&self) -> usize {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LaunchOwnerToken(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchOwnerKind {
    None,
    Generic,
    Startup,
    RemoteActor {
        identity: super::runtime::ActorIdentity,
    },
}

/// A parent wait is distinct from authenticated semantic/execution retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelperWaitProof {
    NoParentChild,
    Pending,
    TeardownProved,
}

#[derive(Clone)]
pub(crate) struct LaunchOwnershipSnapshot {
    pub token: Option<LaunchOwnerToken>,
    pub kind: LaunchOwnerKind,
    pub outcome: LaunchOutcome,
    pub ordinary_requested: bool,
    pub original_helper: HelperWaitProof,
    pub remote: Option<super::runtime::Handle>,
    pub diagnostic: Option<String>,
}

#[derive(Default)]
struct LaunchOwnership {
    outcome: LaunchOutcome,
    owner: Option<std::sync::Arc<dyn LaunchCleanup>>,
    generation: u64,
    token: Option<LaunchOwnerToken>,
    kind: Option<LaunchOwnerKind>,
    ordinary_requested: bool,
    original_helper: Option<std::sync::Arc<dyn LaunchCleanup>>,
}

#[derive(Default)]
pub(crate) struct ExecutionContext {
    pub directory: Option<std::sync::Arc<std::fs::File>>,
    expected_directory: Option<crate::agent_execution::DirectoryIdentity>,
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
        match &self.directory {
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
        self.directory.clone()
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
    pub(crate) fn claim_owner(&self, owner: std::sync::Arc<dyn LaunchCleanup>) -> LaunchOwnerToken {
        self.claim_owner_kind(owner, LaunchOwnerKind::Generic)
    }
    pub(crate) fn claim_startup_owner(
        &self,
        owner: std::sync::Arc<dyn LaunchCleanup>,
    ) -> LaunchOwnerToken {
        self.claim_owner_kind(owner, LaunchOwnerKind::Startup)
    }
    fn claim_owner_kind(
        &self,
        owner: std::sync::Arc<dyn LaunchCleanup>,
        kind: LaunchOwnerKind,
    ) -> LaunchOwnerToken {
        let (token, requested) = {
            let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(&state.kind, Some(LaunchOwnerKind::RemoteActor { .. })) {
                // A transferred context never returns to raw startup cleanup,
                // even after installed() has dropped its strong attachment.
                return state.token.expect("transferred launch owner token");
            }
            state.generation = state
                .generation
                .checked_add(1)
                .expect("launch owner generation");
            let token = LaunchOwnerToken(state.generation);
            state.token = Some(token);
            state.kind = Some(kind);
            state.outcome = LaunchOutcome::Uncertain;
            state.original_helper = None;
            state.owner = Some(owner.clone());
            (token, state.ordinary_requested)
        };
        // A request before the first claim also fences a late spawn. Never
        // invoke an owner's callback while holding the context mutex.
        if requested {
            owner.request_ordinary_cleanup();
        }
        crate::daemon::wake();
        token
    }
    /// The latch and transfer share one mutex. A cancelled startup keeps its
    /// original owner; only the successful branch becomes semantic-required.
    pub(crate) fn transfer_startup_owner(
        &self,
        token: LaunchOwnerToken,
        owner: std::sync::Arc<dyn LaunchCleanup>,
        identity: super::runtime::ActorIdentity,
    ) -> bool {
        let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
        if state.token != Some(token)
            || state.kind != Some(LaunchOwnerKind::Startup)
            || state.ordinary_requested
        {
            return false;
        }
        state.original_helper = state.owner.clone();
        state.owner = Some(owner);
        state.kind = Some(LaunchOwnerKind::RemoteActor { identity });
        state.outcome = LaunchOutcome::Uncertain;
        drop(state);
        crate::daemon::wake();
        true
    }
    pub(crate) fn ownership_snapshot(&self) -> LaunchOwnershipSnapshot {
        let (token, kind, outcome, ordinary_requested, owner, original_helper) = {
            let state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            (
                state.token,
                state.kind.clone().unwrap_or(LaunchOwnerKind::None),
                state.outcome,
                state.ordinary_requested,
                state.owner.clone(),
                state.original_helper.clone(),
            )
        };
        let progress_owner = original_helper.as_ref().or(owner.as_ref());
        let original_helper = progress_owner.map_or(HelperWaitProof::NoParentChild, |owner| {
            owner.original_helper_wait_proof()
        });
        let diagnostic = progress_owner.and_then(|owner| owner.cleanup_diagnostic());
        LaunchOwnershipSnapshot {
            token,
            kind,
            outcome,
            ordinary_requested,
            original_helper,
            remote: owner.and_then(|owner| owner.retained_handle()),
            diagnostic,
        }
    }
    pub(crate) fn request_ordinary_cleanup(&self) -> LaunchOwnershipSnapshot {
        let owner = {
            let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            state.ordinary_requested = true;
            state.owner.clone()
        };
        if let Some(owner) = owner {
            owner.request_ordinary_cleanup();
        }
        crate::daemon::wake();
        self.ownership_snapshot()
    }
    #[cfg(test)]
    pub(crate) fn cleanup_waiters_for_test(&self) -> usize {
        let owner = self
            .ownership
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .owner
            .clone();
        owner.map_or(0, |owner| owner.cleanup_waiters_for_test())
    }
    /// Poll only the captured generation. In particular, an old startup wait
    /// cannot clear a transferred remote role or a replacement owner.
    pub(crate) fn poll_cleanup(
        &self,
        captured: &LaunchOwnershipSnapshot,
    ) -> LaunchOwnershipSnapshot {
        let owner = {
            let state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            if state.token != captured.token || state.kind.as_ref() != Some(&captured.kind) {
                drop(state);
                return self.ownership_snapshot();
            }
            match &state.kind {
                Some(LaunchOwnerKind::Startup | LaunchOwnerKind::Generic) => state.owner.clone(),
                _ => None,
            }
        };
        if let Some(owner) = owner {
            let outcome = owner.poll_cleanup();
            let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            if state.token == captured.token
                && state.kind.as_ref() == Some(&captured.kind)
                && state
                    .owner
                    .as_ref()
                    .is_some_and(|current| std::sync::Arc::ptr_eq(current, &owner))
                && outcome != LaunchOutcome::Uncertain
            {
                state.outcome = outcome;
                state.owner = None;
                crate::daemon::wake();
            }
        }
        self.ownership_snapshot()
    }
    /// Call only after exact HOST semantic settlement/ACK and verified helper
    /// retirement. A spawned-here owner additionally needs its sole wait73.
    pub(crate) fn complete_semantic_cleanup(
        &self,
        token: LaunchOwnerToken,
        identity: &super::runtime::ActorIdentity,
    ) -> bool {
        let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
        if state.token != Some(token)
            || !matches!(&state.kind, Some(LaunchOwnerKind::RemoteActor { identity: current }) if current == identity)
            || state.original_helper.as_ref().is_some_and(|origin| {
                origin.original_helper_wait_proof() != HelperWaitProof::TeardownProved
            })
        {
            return false;
        }
        state.outcome = LaunchOutcome::TeardownProved;
        state.owner = None;
        drop(state);
        crate::daemon::wake();
        true
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
        crate::daemon::wake();
    }
    pub(crate) fn reconcile_cleanup(&self) -> Result<LaunchOutcome, String> {
        let (owner, token, kind) = {
            let state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(state.kind, Some(LaunchOwnerKind::RemoteActor { .. })) {
                return Ok(state.outcome);
            }
            match &state.owner {
                Some(owner) => (owner.clone(), state.token, state.kind.clone()),
                None => return Ok(state.outcome),
            }
        };
        let outcome = owner.cleanup()?;
        let mut state = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
        // A worker may have transferred ownership to a helper while the caller
        // was joining it. Its receipt cannot prove that new owner's teardown.
        if state.token == token
            && state.kind == kind
            && state
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
/// An adapter is confined to one daemon worker. No UI state, subprocesses,
/// unbounded transcript storage, credentials, or global mutable state live here.
pub trait Adapter: Send {
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
