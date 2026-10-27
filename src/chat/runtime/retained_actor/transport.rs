//! Authenticated retained Claude helper and bounded attachment transport.
use super::super::{LocalHandle, Rejection, Update};
pub(crate) use super::final_snapshot::{RetirementInspection, VerifiedRetirement};
use super::journal::Journal;
use super::journal::{ContinuityLoss, InputIdentity, InputReceipt, InputState};
use super::{MAX_FRAME, deadline_io, final_snapshot, journal};
use crate::{
    chat::{
        self, Command, Snapshot,
        protocol::{ExecutionContext, ProcessSpec, TransportSpec},
    },
    daemon::holder_registry::{self as registry, Kind, Process, Record},
    model::HostSessionProof,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, RawFd},
        unix::{
            fs::MetadataExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Command as ChildCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const ARGUMENT: &str = "--cagents-native-actor";
pub(super) const PROTOCOL: u32 = 2;
const CAPABILITY: &str = registry::RETAINED_CLAUDE_CAPABILITY;
const TIMEOUT: Duration = Duration::from_secs(5);
fn payload_digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

#[derive(Serialize, Deserialize)]
enum InputPayload {
    IdlePrompt {
        text: String,
    },
    Command {
        run: Option<crate::model::RunId>,
        command: Command,
        decline_only: bool,
    },
    Interrupt {
        run: Option<crate::model::RunId>,
        including_subagents: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActorIdentity {
    pub instance: uuid::Uuid,
    pub record: Record,
}
/// Immutable retirement evidence, never current account read/delete authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RetirementReceipt {
    /// Legacy private receipts always projected installed capability.
    #[serde(default = "legacy_installed", skip_serializing_if = "is_installed")]
    pub installed: bool,
    pub identity: ActorIdentity,
    pub proof: HostSessionProof,
    pub source: chat::Session,
    pub previous_sources: Vec<chat::Session>,
    pub launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    pub confirmed_session: Option<String>,
    pub paired_holder: Option<crate::terminal::holder::Record>,
}
pub(super) fn legacy_installed() -> bool {
    true
}
pub(super) fn is_installed(value: &bool) -> bool {
    *value
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CloseObservation {
    pub actor_id: uuid::Uuid,
    pub turn_generation: u64,
    pub activity: crate::agent::ActivityState,
    pub ready: bool,
    pub exited: bool,
    pub completed: bool,
    pub closing: bool,
    pub owns_work: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Resident {
    pub provider_process: Option<Process>,
    pub generation: u64,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub ready: bool,
    pub busy: bool,
    pub waiting: bool,
    pub working: bool,
    pub activity: crate::agent::ActivityState,

    pub usage: chat::Usage,
    pub native_continuity: chat::NativeContinuity,
    pub requests: Vec<chat::Request>,
    pub notification_content: Vec<(crate::notifications::Kind, crate::notifications::Content)>,
    prompt_revision: u64,
    pub completed: bool,
    pub owns_work: bool,
    pub installed: bool,
    pub close: CloseObservation,
    pub idle: bool,
    pub failure: Option<String>,
    pub confirmed_session: Option<String>,
    pub background_subagents: Vec<String>,
    pub interrupt_subagents: Vec<String>,
    pub account_reports: Vec<Update>,
    next_input: u64,
    acknowledged_input_count: usize,
    pub continuity_loss: Option<ContinuityLoss>,
    pub retained_runs: Vec<crate::model::RunId>,
    pub outbox_ack: u64,
    pub outbox_latest: u64,
    changed: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ActorInspection {
    pub identity: ActorIdentity,
    pub source: chat::Session,
    pub previous_sources: Vec<chat::Session>,
    pub execution_console: Option<crate::model::LaunchRecipe>,
    pub paired_holder: Option<crate::terminal::holder::Record>,
    pub state: Resident,
    pub launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    pub execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    pub recovery_environment: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
struct ClosePublication {
    snapshot: Snapshot,
    resident: Resident,
}
/// Additive private operation: older protocol-2 helpers refuse the method.
/// A retryable generation mismatch must never be inferred from an error string.
#[derive(Serialize, Deserialize)]
enum ConditionalClosePublication {
    Changed,
    Closed(ClosePublication),
}
#[derive(Serialize, Deserialize)]
struct Startup {
    protocol: u32,
    source: chat::Session,
    program: String,
    arguments: Vec<String>,
    prepared_identity: chat::protocol::claude::PreparedIdentity,
    environment: BTreeMap<String, String>,
    working_directory: PathBuf,
    directory_fd: Option<RawFd>,
    directory_identity: Option<crate::agent_execution::DirectoryIdentity>,
    mailbox_recognition: Option<crate::agent_execution::mailbox::RetainedRecognition>,
    provider_directory_observed: bool,
    recovery_environment: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
struct Invocation {
    proof: HostSessionProof,
    epoch: Option<uuid::Uuid>,
    method: String,
    args: Value,
}
#[derive(Serialize, Deserialize)]
struct Attached {
    protocol: u32,
    close_only: bool,
    identity: ActorIdentity,
    epoch: uuid::Uuid,
    resident: Resident,
    source: chat::Session,
    launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    recovery_environment: BTreeMap<String, String>,
}
#[derive(Clone)]
pub(crate) struct RemoteActor(Arc<Remote>);
struct Pending {
    identity: InputIdentity,
    payload: Value,
    bytes: usize,
    reply: Option<crossbeam_channel::Sender<Result<(), Rejection>>>,
}
struct Remote {
    close_only: bool,
    source: chat::Session,
    identity: ActorIdentity,
    epoch: uuid::Uuid,
    proof: HostSessionProof,
    socket: PathBuf,
    state: Mutex<Arc<Resident>>,
    publication: Mutex<()>,
    final_snapshot: Mutex<Option<Arc<Snapshot>>>,
    environment: Mutex<crate::terminal_ipc::ChatExecutionEnvironment>,
    launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    recovery_environment: BTreeMap<String, String>,
    desired_visible: Mutex<Option<bool>>,
    desired_observe: AtomicBool,
    notified_prompt_revision: std::sync::atomic::AtomicU64,
    input_sequence: Mutex<u64>,
    uncertain_input: Mutex<Option<InputIdentity>>,
    pending: Mutex<Vec<Pending>>,
    retiring_inputs: Mutex<Vec<InputIdentity>>,
    recovering_inputs: AtomicBool,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    stop: AtomicBool,
    installed: AtomicBool,
    preserved: AtomicBool,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    startup: Mutex<Option<UnixStream>>,
}
struct StartupOwner {
    control: Mutex<Option<UnixStream>>,
    cancelled: AtomicBool,
    outcome: Mutex<chat::protocol::LaunchOutcome>,
    finished: std::sync::Condvar,
}
struct StartupAttempt {
    owner: Arc<StartupOwner>,
    handed_off: bool,
}
impl Drop for StartupAttempt {
    fn drop(&mut self) {
        if !self.handed_off {
            self.owner.cancelled.store(true, Ordering::Release);
            if let Some(control) = self
                .owner
                .control
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                let _ = control.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}
impl chat::protocol::LaunchCleanup for StartupOwner {
    fn cleanup(&self) -> Result<chat::protocol::LaunchOutcome, String> {
        self.cancelled.store(true, Ordering::Release);
        if let Some(control) = self
            .control
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = control.shutdown(std::net::Shutdown::Both);
        }
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut outcome = self.outcome.lock().unwrap_or_else(|p| p.into_inner());
        while *outcome == chat::protocol::LaunchOutcome::Uncertain {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Retained actor startup cleanup remains unconfirmed; its exact owner is retained".into());
            }
            outcome = self
                .finished
                .wait_timeout(outcome, remaining)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        Ok(*outcome)
    }
}
#[cfg(test)]
pub(super) fn fixture_source() -> chat::Session {
    let id = crate::model::TerminalId::new();
    chat::Session {
        id,
        launch: chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: "/bin/cat".into(),
                arguments: Vec::new(),
                working_directory: "/var/tmp".into(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(HostSessionProof {
            boot_id: crate::model::BootId::new("fixture-boot"),
            host_session_id: crate::model::HostSessionId::new("fixture"),
            host_incarnation: crate::model::HostIncarnation::new("fixture"),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    }
}
impl RemoteActor {
    #[cfg(test)]
    pub(in crate::chat::runtime) fn fixture() -> Self {
        use crate::model::{BootId, HostIncarnation, HostSessionId, TerminalId};
        let process = Process::read(std::process::id() as i32).unwrap();
        let boot = BootId::new("fixture-boot");
        let resident = Resident {
            provider_process: Some(process.clone()),
            generation: 1,
            session_id: None,
            title: None,
            ready: true,
            busy: false,
            waiting: false,
            working: false,
            activity: crate::agent::ActivityState::Idle,

            usage: Default::default(),
            native_continuity: Default::default(),
            requests: Vec::new(),
            notification_content: Vec::new(),
            prompt_revision: 0,
            completed: false,
            owns_work: true,
            installed: true,
            close: CloseObservation {
                actor_id: uuid::Uuid::new_v4(),
                turn_generation: 0,
                activity: crate::agent::ActivityState::Idle,
                ready: true,
                exited: false,
                completed: false,
                closing: false,
                owns_work: true,
            },
            idle: true,
            failure: None,
            confirmed_session: None,
            background_subagents: Vec::new(),
            interrupt_subagents: Vec::new(),
            account_reports: Vec::new(),
            next_input: 1,
            acknowledged_input_count: 0,
            continuity_loss: None,
            retained_runs: Vec::new(),
            outbox_ack: 0,
            outbox_latest: 0,
            changed: 1,
        };
        Self(Arc::new(Remote {
            close_only: false,
            source: fixture_source(),
            identity: ActorIdentity {
                instance: uuid::Uuid::new_v4(),
                record: Record {
                    version: 4,
                    build: Some(registry::BuildIdentity::current(Kind::Native)),
                    kind: Kind::Native,
                    application: crate::instance::APPLICATION_ID.into(),
                    boot: boot.clone(),
                    terminal: TerminalId::new(),
                    holder: process.clone(),
                    root: process,
                },
            },
            epoch: uuid::Uuid::new_v4(),
            proof: HostSessionProof {
                boot_id: boot,
                host_session_id: HostSessionId::new("fixture"),
                host_incarnation: HostIncarnation::new("fixture"),
            },
            socket: PathBuf::from("/nonexistent/cagents-actor-fixture"),
            state: Mutex::new(Arc::new(resident)),
            publication: Mutex::new(()),
            final_snapshot: Mutex::new(None),
            environment: Mutex::new(crate::terminal_ipc::ChatExecutionEnvironment(
                Default::default(),
            )),
            launch_account_scope: Err("fixture has no account scope".into()),
            recovery_environment: Default::default(),
            desired_visible: Mutex::new(None),
            desired_observe: AtomicBool::new(false),
            notified_prompt_revision: std::sync::atomic::AtomicU64::new(0),
            input_sequence: Mutex::new(1),
            uncertain_input: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            retiring_inputs: Mutex::new(Vec::new()),
            recovering_inputs: AtomicBool::new(false),
            notify: Arc::new(|_| {}),
            stop: AtomicBool::new(false),
            installed: AtomicBool::new(true),
            preserved: AtomicBool::new(false),

            worker: Mutex::new(None),
            startup: Mutex::new(None),
        }))
    }
    /// Complete the actual proxy's pending envelope without any external
    /// helper. Tests still use production enqueue, receipt and disconnection
    /// handling; only the authenticated peer's observed outcome is supplied.
    #[cfg(test)]
    pub(in crate::chat::runtime) fn settle_input_fixture(
        &self,
        outcome: super::super::handle::RetainedInputOutcome,
    ) -> u64 {
        use super::super::handle::RetainedInputOutcome;
        let deadline = Instant::now() + Duration::from_secs(3);
        let identity = loop {
            if let Some(identity) = self.pending_inputs().into_iter().next() {
                break identity;
            }
            assert!(
                Instant::now() < deadline,
                "proxy did not admit the fixture input"
            );
            thread::sleep(Duration::from_millis(1));
        };
        match outcome {
            RetainedInputOutcome::Acknowledged(result) => self.apply_receipts(&[InputReceipt {
                identity: identity.clone(),
                state: InputState::Acknowledged(result),
            }]),
            RetainedInputOutcome::Unknown(error) => self.apply_receipts(&[InputReceipt {
                identity: identity.clone(),
                state: InputState::Unknown(error),
            }]),
            RetainedInputOutcome::Unavailable => {
                self.publish_unavailable("Retained actor attachment was superseded");
            }
            RetainedInputOutcome::Disconnected => {
                // Closing the completion channel after admission gives no
                // information about whether the provider consumed the input.
                self.0
                    .pending
                    .lock()
                    .unwrap()
                    .iter_mut()
                    .find(|pending| pending.identity == identity)
                    .unwrap()
                    .reply
                    .take();
            }
        }
        identity.sequence
    }

    #[cfg(test)]
    pub(in crate::chat::runtime) fn distinct_attachment_fixture(&self) -> Self {
        let mut other = Self::fixture();
        let remote = Arc::get_mut(&mut other.0).unwrap();
        remote.identity = self.identity();
        remote.proof = self.0.proof.clone();
        remote.source = self.0.source.clone();
        other
    }
    #[cfg(test)]
    pub(in crate::chat::runtime) fn completed_failure_fixture() -> Self {
        let actor = Self::fixture();
        {
            let mut state = actor.0.state.lock().unwrap();
            let state = Arc::make_mut(&mut state);
            state.completed = true;
            state.failure = Some("Provider cleanup is unconfirmed".into());
            state.owns_work = true;
        }
        actor
    }
    pub fn close_proved(
        identity: &ActorIdentity,
        proof: &HostSessionProof,
        source: &chat::Session,
    ) -> Result<bool, String> {
        authorize_reader(proof)?;
        validate_evidence_record(&identity.record, proof)?;
        final_snapshot::close_proved(identity, proof, source)
    }
    /// Authenticate the new daemon separately from the saved attachment which
    /// committed retirement. The caller must compare the returned tuple with
    /// its durable roster before recording retirement.
    pub fn inspect_retained_retirement(
        record: &Record,
        proof: &HostSessionProof,
    ) -> Result<RetirementInspection, String> {
        authorize_reader(proof)?;
        validate_evidence_record(record, proof)?;
        final_snapshot::inspect_retirement(record, proof)
    }
    pub fn retired_actor_receipt(
        record: &Record,
        proof: &HostSessionProof,
    ) -> Result<Option<RetirementReceipt>, String> {
        authorize_reader(proof)?;
        validate_evidence_record(record, proof)?;
        final_snapshot::retired_actor_receipt(record, proof)
    }
    pub fn is_close_only(&self) -> bool {
        self.0.close_only
    }
    pub fn is_record(record: &Record) -> bool {
        record.kind == Kind::Native
            && record.build.as_ref().is_some_and(|build| {
                build
                    .capabilities
                    .iter()
                    .any(|capability| capability == CAPABILITY)
            })
    }
    pub fn wake_outcomes(&self) {
        let state = self.state();
        if state.outbox_latest > state.outbox_ack
            || state
                .continuity_loss
                .as_ref()
                .is_some_and(|loss| !loss.acknowledged)
        {
            (self.0.notify)(Update::RetainedOutcomes);
        }
    }
    pub fn native_holder_record(&self) -> &Record {
        &self.0.identity.record
    }
    #[cfg(test)]
    pub(super) fn proof(&self) -> HostSessionProof {
        self.0.proof.clone()
    }
    pub fn identity(&self) -> ActorIdentity {
        self.0.identity.clone()
    }
    pub fn same_attachment(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// Unlike snapshots, resolution always authenticates this live epoch.
    pub fn resolve_source(&self, source: &chat::Session) -> Result<chat::Session, String> {
        self.resolve_source_until(source, Instant::now() + TIMEOUT)
    }
    fn resolve_source_until(
        &self,
        source: &chat::Session,
        deadline: Instant,
    ) -> Result<chat::Session, String> {
        if self.0.close_only {
            return Err(close_only_refusal());
        }
        let control =
            registry::connect_until(&self.0.socket, deadline).map_err(|error| error.to_string())?;
        verify_peer(&control, &self.0.identity.record)?;
        deadline_io::send_until(
            &control,
            &Invocation {
                proof: self.0.proof.clone(),
                epoch: Some(self.0.epoch),
                method: "resolve_source".into(),
                args: json!([source]),
            },
            deadline,
        )?;
        let resolved: Result<chat::Session, String> =
            deadline_io::receive_until(&control, deadline)?;
        let resolved = resolved?;
        if resolved.id != self.0.identity.record.terminal
            || resolved.proof.as_ref() != Some(&self.0.proof)
            || resolved.launch.provider != crate::agent::Provider::Claude
            || resolved.launch.recipe != self.0.source.launch.recipe
            || resolved.owned_codex_listener.is_some()
            || !matches!(
                resolved.native_continuity,
                chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
            )
            || !resolved.has_valid_native_continuity()
        {
            return Err(
                "Retained source resolution changed its exact actor or issued account/recipe"
                    .into(),
            );
        }
        Ok(resolved)
    }
    pub fn owns_work(&self) -> bool {
        !self.0.preserved.load(Ordering::Acquire) && self.state().owns_work
    }
    pub fn state(&self) -> Arc<Resident> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn refresh_state(&self) -> Result<(), String> {
        self.refresh_state_until(Instant::now() + TIMEOUT)
    }
    fn refresh_state_until(&self, deadline: Instant) -> Result<(), String> {
        let closed = self
            .0
            .final_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some();
        if self.0.stop.load(Ordering::Acquire) && !closed {
            return Err("This retained actor attachment is unavailable; authenticate a new attachment before refreshing".into());
        }
        if let Some(mut resident) =
            self.call_until::<Option<Resident>>("state", json!([self.state().changed]), deadline)?
        {
            if self.0.stop.load(Ordering::Acquire) {
                if !closed || !resident.close.completed {
                    return Err(
                        "Closed actor metadata changed provider ownership unexpectedly".into(),
                    );
                }
                resident.completed = true;
                resident.owns_work = false;
                resident.ready = false;
                resident.idle = false;
                resident.busy = false;
                if !self.publish_resident_inner(resident, true) {
                    return Err("Closed actor metadata was superseded during refresh".into());
                }
            } else if !self.publish_resident(resident) && self.0.stop.load(Ordering::Acquire) {
                return Err("Retained actor attachment closed during refresh".into());
            }
        }
        Ok(())
    }
    pub fn outcome_clock_until(&self, deadline: Instant) -> Result<(u64, u64), String> {
        if self.0.stop.load(Ordering::Acquire) {
            return Err("This retained Answer attachment is no longer authoritative".into());
        }
        // Revision zero predates every helper publication. Consume the fresh
        // authenticated response itself: a concurrent observer can supersede
        // the cached UI projection, but cannot replace this causal watermark.
        let state = self
            .call_until::<Option<Resident>>("state", json!([0]), deadline)?
            .ok_or("The retained helper omitted its Answer semantic watermark")?;
        if state.continuity_loss.is_some() || state.outbox_ack > state.outbox_latest {
            return Err(
                "Retained semantic continuity is unconfirmed; the answer must not be repeated"
                    .into(),
            );
        }
        let clock = (state.outbox_ack, state.outbox_latest);
        self.publish_resident(state);
        if self.0.stop.load(Ordering::Acquire) {
            return Err(
                "The retained Answer attachment changed during semantic synchronization".into(),
            );
        }
        Ok(clock)
    }
    pub fn set_visible(&self, visible: bool) {
        *self
            .0
            .desired_visible
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(visible);
    }
    pub fn observe_next_update(&self) -> u64 {
        self.0.desired_observe.store(true, Ordering::Release);
        self.state().generation
    }
    pub fn attachment_snapshot(&self) -> Snapshot {
        let state = self.state();
        Snapshot {
            generation: state.generation,
            turn_generation: state.close.turn_generation,
            session_id: state.session_id.clone(),
            title: state.title.clone(),
            ready: state.ready,
            busy: state.busy,
            waiting: state.waiting,
            usage: state.usage.clone(),
            requests: state.requests.clone(),
            native_continuity: state.native_continuity.clone(),
            error: state.failure.clone(),
            exited: state.completed,
            ..Snapshot::default()
        }
    }
    /// Bounded resident projection; never performs socket I/O or reads transcript pages.
    pub fn notification_content(
        &self,
        kind: crate::notifications::Kind,
    ) -> crate::notifications::Content {
        self.state()
            .notification_content
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, content)| content.clone())
            .unwrap_or_default()
    }
    pub fn conversation_title(
        &self,
    ) -> Result<(Option<String>, Option<String>), super::super::TitleUnavailable> {
        let state = self.0.state.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => super::super::TitleUnavailable::Busy,
            std::sync::TryLockError::Poisoned(_) => super::super::TitleUnavailable::Poisoned,
        })?;
        Ok((state.session_id.clone(), state.title.clone()))
    }
    pub fn launch_account_scope(&self) -> Result<&crate::account::scope::LaunchAccountScope, &str> {
        self.0.launch_account_scope.as_ref().map_err(String::as_str)
    }
    pub fn execution_environment(&self) -> crate::terminal_ipc::ChatExecutionEnvironment {
        self.0
            .environment
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn restart_environment(&self) -> BTreeMap<String, String> {
        self.0.recovery_environment.clone()
    }
    pub fn recovery_environment_matches(&self, value: &BTreeMap<String, String>) -> bool {
        &self.0.recovery_environment == value
    }
    #[cfg(test)]
    pub fn resource_lifetime(&self) -> Box<dyn Fn() -> bool> {
        let weak = Arc::downgrade(&self.0);
        Box::new(move || weak.upgrade().is_some())
    }
    pub fn spawn(
        mut spec: ProcessSpec,
        source: chat::Session,
        prepared_identity: chat::protocol::claude::PreparedIdentity,
        recovery_environment: BTreeMap<String, String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        if source.launch.provider != crate::agent::Provider::Claude
            || !matches!(spec.transport, TransportSpec::JsonLines)
        {
            return Err("Retained actors require the Claude stream-json transport".into());
        }
        spec.remove_unset_environment();
        let proof = source
            .proof
            .clone()
            .ok_or("Retained actor launch lacks daemon ownership proof")?;
        let directory = spec
            .execution
            .as_ref()
            .and_then(|context| context.directory_descriptor());
        let directory_fd = directory.as_ref().map(|file| file.as_raw_fd());
        let observed = spec
            .execution
            .as_ref()
            .map(|context| context.provider_directory_observed(&spec.working_directory))
            .transpose()?
            .unwrap_or(false);
        let request = Startup {
            protocol: PROTOCOL,
            prepared_identity,
            source: source.clone(),
            program: spec.program,
            arguments: spec.arguments,
            environment: spec.environment,
            working_directory: spec.working_directory,
            directory_fd,
            directory_identity: spec
                .execution
                .as_ref()
                .map(|context| context.directory_identity())
                .transpose()?
                .flatten(),
            mailbox_recognition: spec
                .messages
                .as_ref()
                .map(|recognizer| recognizer.delegate_to_retained_owner()),
            provider_directory_observed: observed,
            recovery_environment,
        };
        let (control, remote) = UnixStream::pair().map_err(|error| error.to_string())?;
        let fd = remote.as_raw_fd();
        let mut command = ChildCommand::new(helper_executable()?);
        command
            .arg0("cagents-native")
            .arg(ARGUMENT)
            .arg(fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Exec resets the process-local saved limit. Restore it before the
        // helper starts, so its provider and tools inherit the caller's limit.
        crate::host::descriptor_limit::inherit(&mut command);
        unsafe {
            command.pre_exec(move || {
                crate::daemon::seal_inherited_descriptors()?;
                if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(directory) = directory_fd
                    && libc::fcntl(directory, libc::F_SETFD, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let owner = Arc::new(StartupOwner {
            control: Mutex::new(Some(
                control.try_clone().map_err(|error| error.to_string())?,
            )),
            cancelled: AtomicBool::new(false),
            outcome: Mutex::new(chat::protocol::LaunchOutcome::NoProvider),
            finished: std::sync::Condvar::new(),
        });
        let mut attempt = StartupAttempt {
            owner: owner.clone(),
            handed_off: false,
        };
        if let Some(context) = &spec.execution {
            context.claim_owner(owner.clone());
        }
        // Allocate the sole child reaper before creating the helper. A startup
        // failure keeps an exact cleanup owner rather than killing an assumed
        // process group or allowing the provider to appear after cancellation.
        let (publish, incoming) = std::sync::mpsc::sync_channel::<std::process::Child>(1);
        let watcher = owner.clone();
        thread::Builder::new()
            .name("cagents-actor-owner".into())
            .spawn(move || {
                if let Ok(mut child) = incoming.recv() {
                    let proved = child.wait().is_ok_and(|status| status.code() == Some(73));
                    if proved {
                        *watcher.outcome.lock().unwrap_or_else(|p| p.into_inner()) =
                            chat::protocol::LaunchOutcome::TeardownProved;
                    }
                    watcher.finished.notify_all();
                }
            })
            .map_err(|error| error.to_string())?;
        {
            let mut outcome = owner.outcome.lock().unwrap_or_else(|p| p.into_inner());
            if owner.cancelled.load(Ordering::Acquire) {
                return Err("Retained actor launch was cancelled before process creation".into());
            }
            let child = command.spawn().map_err(|error| error.to_string())?;
            *outcome = chat::protocol::LaunchOutcome::Uncertain;
            publish
                .send(child)
                .map_err(|_| "Retained actor startup lost its exact child reaper")?;
        }
        drop(remote);
        // Keep this channel through installation. EOF before commit asks the
        // helper to clean its still-private actor and every descendant.
        send(&control, &request, Duration::from_secs(30))?;
        let attached: Result<Attached, String> = receive(&control, Duration::from_secs(30))?;
        let attached = attached?;
        let handle = Self::from_attached(attached, proof, Some(control), notify)?;
        let record = handle.identity().record;
        crate::daemon::process_scope::warn_if_shared(
            record.holder.pid as u32,
            record.root.pid as u32,
        );
        if let Some(context) = spec.execution {
            context.claim_owner(Arc::new(handle.clone()));
        }
        owner
            .control
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        attempt.handed_off = true;
        Ok(handle)
    }
    pub fn inspect(record: &Record, proof: HostSessionProof) -> Result<ActorInspection, String> {
        Self::inspect_until(record, proof, Instant::now() + TIMEOUT)
    }
    pub fn inspect_until(
        record: &Record,
        proof: HostSessionProof,
        deadline: Instant,
    ) -> Result<ActorInspection, String> {
        let socket = registry::socket(&registry::directory(Kind::Native)?, record.terminal);
        Self::inspect_at_until(record, proof, &socket, deadline)
    }
    fn inspect_at_until(
        record: &Record,
        proof: HostSessionProof,
        socket: &std::path::Path,
        deadline: Instant,
    ) -> Result<ActorInspection, String> {
        if Instant::now() >= deadline {
            return Err("Retained actor inspection deadline expired".into());
        }
        validate_record(record)?;
        let control =
            registry::connect_until(socket, deadline).map_err(|error| error.to_string())?;
        verify_peer(&control, record)?;
        deadline_io::send_until(
            &control,
            &Invocation {
                proof,
                epoch: None,
                method: "inspect".into(),
                args: Value::Null,
            },
            deadline,
        )?;
        let response: Result<ActorInspection, String> =
            deadline_io::receive_until(&control, deadline)?;
        let response = response?;
        if response.identity.record != *record || response.source.id != record.terminal {
            return Err("Retained actor inspection returned another exact holder identity".into());
        }
        Ok(response)
    }
    pub fn adopt(
        record: &Record,
        source: chat::Session,
        proof: HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        Self::adopt_mode(record, source, proof, false, notify)
    }
    pub fn adopt_for_close(
        record: &Record,
        source: chat::Session,
        proof: HostSessionProof,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        Self::adopt_mode(record, source, proof, true, notify)
    }
    fn adopt_mode(
        record: &Record,
        source: chat::Session,
        proof: HostSessionProof,
        close_only: bool,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let socket = registry::socket(&registry::directory(Kind::Native)?, record.terminal);
        Self::adopt_at_until(
            record,
            source,
            proof,
            close_only,
            &socket,
            Instant::now() + TIMEOUT,
            notify,
        )
    }
    fn adopt_at_until(
        record: &Record,
        source: chat::Session,
        proof: HostSessionProof,
        close_only: bool,
        socket: &std::path::Path,
        deadline: Instant,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        validate_record(record)?;
        let control =
            registry::connect_until(socket, deadline).map_err(|error| error.to_string())?;
        verify_peer(&control, record)?;
        deadline_io::send_until(
            &control,
            &Invocation {
                proof: proof.clone(),
                epoch: None,
                method: if close_only { "attach_close" } else { "attach" }.into(),
                args: json!([source, record]),
            },
            deadline,
        )?;
        let attached: Result<Attached, String> = deadline_io::receive_until(&control, deadline)?;
        let attached = attached?;
        if &attached.identity.record != record || attached.close_only != close_only {
            return Err("Retained actor changed identity during attachment".into());
        }
        Self::from_attached(attached, proof, None, notify)
    }
    fn from_attached(
        attached: Attached,
        proof: HostSessionProof,
        startup: Option<UnixStream>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        if attached.protocol != PROTOCOL {
            return Err(
                "Retained Claude actor protocol is incompatible; no prompt was replayed".into(),
            );
        }
        validate_record(&attached.identity.record)?;
        let socket = registry::socket(
            &registry::directory(Kind::Native)?,
            attached.identity.record.terminal,
        );
        Self::from_attached_at(attached, proof, startup, socket, notify)
    }
    fn from_attached_at(
        attached: Attached,
        proof: HostSessionProof,
        startup: Option<UnixStream>,
        socket: PathBuf,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let recovering_inputs = attached.resident.acknowledged_input_count > 0;
        let remote = Self(Arc::new(Remote {
            close_only: attached.close_only,
            source: attached.source,
            identity: attached.identity,
            epoch: attached.epoch,
            proof,
            socket,
            desired_visible: Mutex::new(None),
            desired_observe: AtomicBool::new(false),
            notified_prompt_revision: std::sync::atomic::AtomicU64::new(0),
            input_sequence: Mutex::new(attached.resident.next_input),
            uncertain_input: Mutex::new(None),
            state: Mutex::new(Arc::new(attached.resident)),
            publication: Mutex::new(()),
            final_snapshot: Mutex::new(None),
            environment: Mutex::new(attached.execution_environment),
            launch_account_scope: attached.launch_account_scope,
            recovery_environment: attached.recovery_environment,
            pending: Mutex::new(Vec::new()),
            retiring_inputs: Mutex::new(Vec::new()),
            recovering_inputs: AtomicBool::new(recovering_inputs),
            notify: Arc::new(notify),
            stop: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            preserved: AtomicBool::new(false),

            worker: Mutex::new(None),
            startup: Mutex::new(startup),
        }));
        let weak = Arc::downgrade(&remote.0);
        let worker = thread::Builder::new()
            .name("cagents-actor-proxy".into())
            .spawn(move || {
                loop {
                    let Some(inner) = weak.upgrade() else {
                        break;
                    };
                    if inner.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let actor = Self(inner);
                    let desired = actor
                        .0
                        .desired_visible
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take();
                    if let Some(visible) = desired
                        && actor.call::<()>("visible", json!([visible])).is_err()
                    {
                        actor
                            .0
                            .desired_visible
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .get_or_insert(visible);
                    }
                    if actor.0.desired_observe.swap(false, Ordering::AcqRel)
                        && actor.call::<()>("observe", Value::Null).is_err()
                    {
                        actor.0.desired_observe.store(true, Ordering::Release);
                    }
                    let old = actor.state();
                    let response = actor.call::<Option<Resident>>("state", json!([old.changed]));
                    if actor.0.stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(error) = &response
                        && actor.endpoint_failed(error)
                    {
                        actor.publish_unavailable(error);
                        break;
                    }
                    if let Ok(Some(state)) = response {
                        if !actor.publish_resident(state) && actor.0.stop.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    let _ = actor.reconcile_acknowledged_inputs();
                    let identities: Vec<_> = actor
                        .0
                        .pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .iter()
                        .filter(|pending| pending.reply.is_some())
                        .map(|pending| pending.identity.clone())
                        .collect();
                    if !identities.is_empty()
                        && let Ok(receipts) =
                            actor.call::<Vec<InputReceipt>>("input_receipts", json!([identities]))
                    {
                        actor.apply_receipts(&receipts);
                    }
                    let _ = actor.flush_input_retirements();
                    drop(actor);
                    thread::sleep(Duration::from_millis(50));
                }
            })
            .map_err(|error| error.to_string())?;
        *remote.0.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        Ok(remote)
    }
    fn publish_resident(&self, resident: Resident) -> bool {
        self.publish_resident_inner(resident, false)
    }
    fn publish_resident_inner(&self, resident: Resident, allow_closed: bool) -> bool {
        // Serialize metadata admission and all notifications together. Notify
        // callbacks enqueue host updates; the resident lock is never held
        // while invoking them, so consumers may read the accepted projection.
        let _publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        let updates = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if (self.0.stop.load(Ordering::Acquire)
                && !(allow_closed
                    && state.completed
                    && resident.completed
                    && resident.close.completed))
                || resident.changed < state.changed
                || resident.generation < state.generation
                // Installation is acknowledged independently of observer
                // revisions. A state RPC issued before commit can arrive
                // after its ACK with equal changed/generation values.
                || (state.installed && !resident.installed)
                || (matches!(state.native_continuity,
                    chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL })
                    && resident.native_continuity != state.native_continuity)
            {
                return false;
            }
            let mut updates = Vec::new();
            let generation = resident.generation;
            let identity_changed = resident.confirmed_session != state.confirmed_session;
            if resident.changed != state.changed
                || resident.generation != state.generation
                || identity_changed
            {
                updates.push(Update::Observed { generation });
                updates.push(Update::Presentation);
            }
            if resident.activity != state.activity {
                updates.push(Update::Activity {
                    state: resident.activity,
                    generation,
                });
            }
            if resident.ready && !state.ready {
                updates.push(Update::Ready);
            }
            if resident.idle
                && self
                    .0
                    .notified_prompt_revision
                    .fetch_max(resident.prompt_revision, Ordering::AcqRel)
                    < resident.prompt_revision
            {
                updates.push(Update::PromptAvailable);
            }
            if (resident.outbox_latest > resident.outbox_ack
                && resident.outbox_latest != state.outbox_latest)
                || (resident.continuity_loss != state.continuity_loss
                    && resident
                        .continuity_loss
                        .as_ref()
                        .is_some_and(|loss| !loss.acknowledged))
            {
                updates.push(Update::RetainedOutcomes);
            }
            updates.extend(
                resident
                    .account_reports
                    .iter()
                    .filter(|report| !state.account_reports.contains(report))
                    .cloned(),
            );
            *state = Arc::new(resident);
            updates
        };
        for update in updates {
            (self.0.notify)(update);
        }
        true
    }
    fn publish_closed(&self, snapshot: Snapshot) {
        self.publish_closed_inner(snapshot, None);
    }
    fn publish_closed_inner(&self, snapshot: Snapshot, resident: Option<Resident>) {
        let _publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        *self
            .0
            .final_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(snapshot));
        self.0.stop.store(true, Ordering::Release);
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut resident) = resident {
            // A close response may also have been issued before installation
            // was acknowledged. Keep that actor-scoped fact monotonic while
            // retaining the exact final snapshot/receipt above unchanged.
            if state.installed {
                resident.installed = true;
                resident.native_continuity = state.native_continuity.clone();
            }
            *state = Arc::new(resident);
        }
        let state = Arc::make_mut(&mut state);
        state.completed = true;
        state.owns_work = false;
        state.idle = false;
        state.ready = false;
        state.failure = None;
        state.close.completed = true;
        state.close.owns_work = false;
        state.close.closing = true;
    }
    fn endpoint_failed(&self, error: &str) -> bool {
        terminal_endpoint_error(error, self.0.identity.record.holder.is_current())
    }
    fn publish_unavailable(&self, error: &str) {
        let _publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        let dead = !self.0.identity.record.holder.is_current();
        let reason = if dead {
            "The retained Claude actor exited; exact provider cleanup and unfinished input outcomes are unconfirmed".to_owned()
        } else {
            format!("This retained actor attachment is no longer authoritative: {error}")
        };
        let generation;
        {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.0.stop.load(Ordering::Acquire) {
                return;
            }
            let state = Arc::make_mut(&mut state);
            state.failure = Some(reason.clone());
            state.ready = false;
            state.idle = false;
            state.busy = false;
            state.waiting = false;
            state.working = false;
            state.activity = crate::agent::ActivityState::Unknown;
            state.owns_work = true;
            generation = state.generation;
        }
        for pending in &mut *self.0.pending.lock().unwrap_or_else(|p| p.into_inner()) {
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Err(Rejection::Unacknowledged(format!(
                    "{reason}; input {} sequence {} remains unknown and must not be regenerated",
                    pending.identity.id, pending.identity.sequence
                ))));
            }
        }
        (self.0.notify)(Update::Activity {
            state: crate::agent::ActivityState::Unknown,
            generation,
        });
        (self.0.notify)(Update::Observed { generation });
        (self.0.notify)(Update::Presentation);
        self.0.stop.store(true, Ordering::Release);
    }
    pub(in crate::chat::runtime) fn pinned_launch_directory(
        &self,
    ) -> Result<(crate::agent_execution::DirectoryIdentity, Arc<File>), String> {
        if self.0.close_only {
            return Err(close_only_refusal());
        }
        let deadline = Instant::now() + TIMEOUT;
        let control =
            registry::connect_until(&self.0.socket, deadline).map_err(|error| error.to_string())?;
        verify_peer(&control, &self.0.identity.record)?;
        deadline_io::send_until(
            &control,
            &Invocation {
                proof: self.0.proof.clone(),
                epoch: Some(self.0.epoch),
                method: "pinned_directory".into(),
                args: Value::Null,
            },
            deadline,
        )?;
        let (response, mut descriptors) = crate::descriptor_frame::receive_json_with_fds_until::<
            Result<crate::agent_execution::DirectoryIdentity, String>,
        >(
            &control,
            deadline,
            MAX_FRAME,
            1,
            crate::descriptor_frame::FrameByteOrder::LittleEndian,
        )?;
        verify_peer(&control, &self.0.identity.record)?;
        match response {
            Err(error) => Err(error),
            Ok(identity) => {
                if descriptors.len() != 1 {
                    return Err(
                        "Retained provider did not transfer exactly one directory descriptor"
                            .into(),
                    );
                }
                let directory = Arc::new(File::from(descriptors.remove(0)));
                crate::chat::protocol::validate_launch_directory(&directory, identity)?;
                Ok((identity, directory))
            }
        }
    }
    pub fn call<T: DeserializeOwned>(&self, method: &str, args: Value) -> Result<T, String> {
        self.call_until(method, args, Instant::now() + TIMEOUT)
    }
    pub(in crate::chat::runtime) fn call_until<T: DeserializeOwned>(
        &self,
        method: &str,
        args: Value,
        deadline: Instant,
    ) -> Result<T, String> {
        if self.0.close_only && !close_only_method(method) {
            return Err(close_only_refusal());
        }
        let control =
            registry::connect_until(&self.0.socket, deadline).map_err(|error| error.to_string())?;
        verify_peer(&control, &self.0.identity.record)?;
        deadline_io::send_until(
            &control,
            &Invocation {
                proof: self.0.proof.clone(),
                epoch: Some(self.0.epoch),
                method: method.into(),
                args,
            },
            deadline,
        )?;
        let response: Result<Value, String> = deadline_io::receive_until(&control, deadline)?;
        serde_json::from_value(response?).map_err(|error| error.to_string())
    }
    pub fn enqueue(
        &self,
        run: Option<crate::model::RunId>,
        command: Command,
    ) -> Result<crossbeam_channel::Receiver<Result<(), Rejection>>, String> {
        self.enqueue_payload(InputPayload::Command {
            run,
            command,
            decline_only: false,
        })
    }
    /// Preserve the actor's refusal-only check across the process boundary.
    pub fn decline_answer(&self, command: Command) -> Result<(), String> {
        self.enqueue_payload(InputPayload::Command {
            run: None,
            command,
            decline_only: true,
        })?
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| {
            "Retained refusal acknowledgement remains unknown; never resend automatically"
                .to_owned()
        })?
        .map_err(String::from)
    }
    pub fn pending_inputs(&self) -> Vec<InputIdentity> {
        self.0
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|pending| pending.identity.clone())
            .collect()
    }
    fn enqueue_payload(
        &self,
        payload: InputPayload,
    ) -> Result<crossbeam_channel::Receiver<Result<(), Rejection>>, String> {
        if self.0.close_only {
            return Err(close_only_refusal());
        }
        if self.0.stop.load(Ordering::Acquire) {
            return Err("Retained actor proxy is unavailable; no new input was admitted".into());
        }
        if self.0.recovering_inputs.load(Ordering::Acquire) {
            return Err("Previous command receipts are settling after reattachment; no new command was admitted".into());
        }
        let payload = serde_json::to_value(payload).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
        let identity;
        let response;
        {
            let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.len() >= 65 {
                return Err("Remote actor pending input capacity reached".into());
            }
            if pending.len().saturating_add(
                self.0
                    .retiring_inputs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .len(),
            ) >= journal::MAX_INPUTS
            {
                return Err(
                    "Input outcome retirement is pending; no new command was admitted".into(),
                );
            }
            if let Some(identity) = &*self
                .0
                .uncertain_input
                .lock()
                .unwrap_or_else(|p| p.into_inner())
            {
                return Err(format!(
                    "Resolve retained input {} sequence {} before admitting another input",
                    identity.id, identity.sequence
                ));
            }
            let retained: usize = pending
                .iter()
                .map(|pending| pending.bytes)
                .try_fold(0usize, usize::checked_add)
                .unwrap_or(usize::MAX);
            if retained.saturating_add(bytes.len()) > 64 * 1024 * 1024 {
                return Err("Remote actor pending input byte budget reached".into());
            }
            let mut sequence = self
                .0
                .input_sequence
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            identity = InputIdentity {
                sequence: *sequence,
                id: uuid::Uuid::new_v4(),
                hash: payload_digest(&bytes),
            };
            let wire = serde_json::to_vec(&Invocation {
                proof: self.0.proof.clone(),
                epoch: Some(self.0.epoch),
                method: "input".into(),
                args: json!([identity, payload]),
            })
            .map_err(|error| error.to_string())?;
            if wire.len() > MAX_FRAME {
                return Err(
                    "Retained input exceeds the bounded wire capacity; no input was admitted"
                        .into(),
                );
            }
            *sequence = sequence
                .checked_add(1)
                .ok_or("Remote actor input sequence exhausted")?;
            let (reply, receiver) = crossbeam_channel::bounded(1);
            response = receiver;
            *self
                .0
                .uncertain_input
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(identity.clone());
            pending.push(Pending {
                identity: identity.clone(),
                payload: payload.clone(),
                bytes: bytes.len(),
                reply: Some(reply),
            });
        }
        // Preserve the exact identity and envelope before crossing the socket.
        // An ambiguous write keeps the receipt pending and fences new input.
        if let Ok(receipt) = self.call::<InputReceipt>("input", json!([identity, payload])) {
            self.apply_receipts(&[receipt]);
        }
        Ok(response)
    }
    /// Explicit retransmission of the exact protocol envelope. Existing helper
    /// receipts return the original outcome without invoking the adapter again.
    /// This never replays stdin or generates a replacement prompt UUID.
    pub fn retry_input(&self, identity: &InputIdentity) -> Result<InputReceipt, String> {
        if self.0.stop.load(Ordering::Acquire) {
            return Err(
                "Retained actor proxy is unavailable; original input was not replayed".into(),
            );
        }
        let payload = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|pending| &pending.identity == identity)
            .map(|pending| pending.payload.clone())
            .ok_or("The original input envelope is unavailable; never reconstruct it")?;
        let receipt = self.call("input", json!([identity, payload]))?;
        self.apply_receipts(std::slice::from_ref(&receipt));
        Ok(receipt)
    }
    fn apply_receipts(&self, receipts: &[InputReceipt]) {
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        let mut uncertain = self
            .0
            .uncertain_input
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut completed = Vec::new();
        pending.retain_mut(|pending| {
            let Some(receipt) = receipts
                .iter()
                .find(|receipt| receipt.identity == pending.identity)
            else {
                return true;
            };
            if uncertain.as_ref() == Some(&receipt.identity) {
                *uncertain = None;
            }
            match &receipt.state {
                InputState::Acknowledged(result) => {
                    if let Some(reply) = pending.reply.take() {
                        let _ = reply.send(result.clone().map_err(Rejection::Refused));
                    }
                    completed.push(pending.identity.clone());
                    false
                }
                InputState::Unknown(error) => {
                    if let Some(reply) = pending.reply.take() {
                        let _ = reply.send(Err(Rejection::Unacknowledged(error.clone())));
                    }
                    true
                }
                InputState::Accepted => true,
            }
        });
        let mut retiring = self
            .0
            .retiring_inputs
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for identity in completed {
            if !retiring.contains(&identity) {
                retiring.push(identity);
            }
        }
    }

    fn reconcile_acknowledged_inputs(&self) -> Result<(), String> {
        if self.state().acknowledged_input_count == 0
            && !self.0.recovering_inputs.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let receipts: Vec<InputReceipt> = self.call("acknowledged_inputs", Value::Null)?;
        if receipts.len() > 64
            || receipts
                .iter()
                .any(|receipt| !matches!(&receipt.state, InputState::Acknowledged(_)))
        {
            return Err("Helper returned an invalid terminal receipt page".into());
        }
        // Deliver any still-live local waiter before retirement. Older daemon
        // waiters no longer exist; observing their durable terminal outcome
        // settles metadata without reconstructing or replaying their input.
        self.apply_receipts(&receipts);
        let mut retiring = self
            .0
            .retiring_inputs
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for receipt in &receipts {
            if !retiring.contains(&receipt.identity) {
                retiring.push(receipt.identity.clone());
            }
        }
        if receipts.is_empty() {
            self.0.recovering_inputs.store(false, Ordering::Release);
        }
        Ok(())
    }

    fn flush_input_retirements(&self) -> Result<(), String> {
        let identities: Vec<_> = self
            .0
            .retiring_inputs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .take(64)
            .cloned()
            .collect();
        if identities.is_empty() {
            return Ok(());
        }
        // No pending/receipt guard is held over the socket. On a lost reply,
        // keep this bounded metadata and repeat only this idempotent ACK.
        self.call::<()>("retire_acknowledged_inputs", json!([identities]))?;
        self.0
            .retiring_inputs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|identity| !identities.contains(identity));
        Ok(())
    }

    pub fn submit(
        &self,
        run: Option<crate::model::RunId>,
        command: Command,
    ) -> Result<(), Rejection> {
        let receiver = self.enqueue(run, command).map_err(Rejection::Refused)?;
        receiver.recv_timeout(Duration::from_secs(10)).map_err(|_|Rejection::Unacknowledged("Retained actor input acknowledgement is pending; use its stable receipt, never resend blindly".into()))?
    }
    pub fn try_idle_prompt(&self, text: String) -> Result<(), Rejection> {
        let receiver = self
            .enqueue_payload(InputPayload::IdlePrompt { text })
            .map_err(Rejection::Refused)?;
        receiver.recv_timeout(Duration::from_secs(10))
            .map_err(|_| Rejection::Unacknowledged("Retained idle prompt acknowledgement is pending; use its stable receipt, never resend blindly".into()))?
    }
    pub fn interrupt(
        &self,
        run: Option<crate::model::RunId>,
        including_subagents: bool,
    ) -> Result<(), String> {
        let receiver = self.enqueue_payload(InputPayload::Interrupt {
            run,
            including_subagents,
        })?;
        receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                "Retained interrupt acknowledgement remains unknown; never resend automatically"
                    .to_owned()
            })?
            .map_err(String::from)
    }

    pub fn snapshot(&self, after: Option<u64>) -> Option<Snapshot> {
        if let Some(snapshot) = self
            .0
            .final_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return (after != Some(snapshot.generation)).then(|| snapshot.as_ref().clone());
        }
        self.call("snapshot", json!([after])).ok().flatten()
    }
    pub fn commit_installation(&self) -> Result<(), String> {
        self.call::<()>("commit", Value::Null)?;
        // Use the same publication fence as the observer. No local revision
        // is invented: the next authentic helper publication owns its clocks.
        {
            let _publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
            self.0.installed.store(true, Ordering::Release);
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let state = Arc::make_mut(&mut state);
            state.installed = true;
            state.native_continuity =
                chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
        }
        self.0
            .startup
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        Ok(())
    }
    pub fn preserve(&self) -> Result<(), String> {
        self.call::<()>("preserve", Value::Null)?;
        self.0.preserved.store(true, Ordering::Release);
        self.0.stop.store(true, Ordering::Release);
        Ok(())
    }
    pub fn close_method(&self, method: &str, args: Value) -> Result<(), String> {
        self.publish_close_result(self.call(method, args))
    }
    pub fn try_close_if_idle(&self, generation: u64) -> Result<bool, String> {
        let publication = match self.call("try_close_if_idle", json!([generation])) {
            Ok(ConditionalClosePublication::Changed) => return Ok(false),
            Ok(ConditionalClosePublication::Closed(publication)) => Ok(publication),
            Err(error) => Err(error),
        };
        // A lost reply remains uncertain unless the existing exact durable
        // final publication independently proves this provider's cleanup.
        self.publish_close_result(publication).map(|()| true)
    }
    fn publish_close_result(&self, result: Result<ClosePublication, String>) -> Result<(), String> {
        let (snapshot, resident) = match result {
            Ok(publication) => (publication.snapshot, Some(publication.resident)),
            Err(error) => {
                if !final_snapshot::close_proved(&self.0.identity, &self.0.proof, &self.0.source)? {
                    return Err(error);
                }
                (final_snapshot::read(&self.0.identity, &self.0.proof, &self.0.source)
                    .map_err(|detail| format!("Provider cleanup is proved, but the exact final publication is unavailable: {detail}"))?, None)
            }
        };
        // Publish the final cursors and stop fence atomically with respect to
        // observer/refresh notifications; no stale live response can intervene.
        self.publish_closed_inner(snapshot, resident);
        (self.0.notify)(Update::RetainedOutcomes);
        Ok(())
    }
    pub fn finish_close(&self) -> Result<(), String> {
        if let Err(error) = self.call::<()>("retire", Value::Null) {
            if !final_snapshot::retirement_proved(&self.0.identity, &self.0.proof)? {
                return Err(error);
            }
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if final_snapshot::retirement_proved(&self.0.identity, &self.0.proof)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("Final outcomes are acknowledged, but exact helper retirement has not completed; retain its cleanup owner".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn close(&self) -> Result<(), String> {
        self.close_method("close", Value::Null)
    }
}
impl chat::protocol::LaunchCleanup for RemoteActor {
    fn cleanup(&self) -> Result<chat::protocol::LaunchOutcome, String> {
        self.close()?;
        self.finish_close()?;
        Ok(chat::protocol::LaunchOutcome::TeardownProved)
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            // is_finished can precede OS thread/TLS destruction. This worker
            // only observes retained metadata; detach never proves cleanup.
            drop(worker);
        }
        // A committed proxy disappearing is a detach, including daemon death.
        // Startup EOF, held until commit, owns uninstalled launch rollback.
    }
}

fn close_only_terminal_projection(
    previous: &chat::Session,
    next: &chat::Session,
    proof: &HostSessionProof,
    no_pair_or_console: bool,
) -> bool {
    match (&previous.terminal, &next.terminal) {
        (None, None) => true,
        // A conversion startup may still carry its issued historical terminal
        // before the HOST's unpair ACK. The helper has never owned a pair or
        // console in this state; cleanup may discard only that stale view.
        (Some(_), None) => no_pair_or_console,
        (Some(old), Some(new)) => {
            if new.continuity.live_proof().as_ref() != Some(proof) {
                return false;
            }
            let mut normalized = new.clone();
            normalized.continuity = old.continuity.clone();
            &normalized == old
        }
        _ => false,
    }
}
fn close_only_refusal() -> String {
    "This retained actor attachment is close-only; provider input and ownership preservation are refused".into()
}
fn close_only_method(method: &str) -> bool {
    matches!(
        method,
        "state"
            | "execution_environment"
            | "recognize_sources"
            | "snapshot"
            | "presentation"
            | "visible"
            | "observe"
            | "readiness"
            | "outbox"
            | "outbox_unacked"
            | "ack_loss"
            | "ack_outbox"
            | "input_receipt"
            | "input_receipts"
            | "acknowledged_inputs"
            | "retire_acknowledged_inputs"
            | "ack_inputs"
            | "update_source"
            | "close_if_idle"
            | "try_close_if_idle"
            | "close"
            | "retire"
    )
}

fn published_source(
    source: &chat::Session,
    confirmed: Option<&str>,
    continuity: &chat::NativeContinuity,
) -> chat::Session {
    let mut projected = source.clone();
    if let Some(confirmed) = confirmed {
        projected.launch.session_id = Some(confirmed.to_owned());
    }
    projected.native_continuity = continuity.clone();
    projected
}
fn validate_source_lineage(lineage: &[chat::Session]) -> Result<(), String> {
    let retained = serde_json::to_vec(lineage)
        .map_err(|error| error.to_string())?
        .len();
    if lineage.len() > 1024 || retained > 4 * 1024 * 1024 {
        return Err("Retained source lineage capacity reached".into());
    }
    Ok(())
}

fn validate_pair(
    source: &chat::Session,
    console: Option<&crate::model::LaunchRecipe>,
    pair: Option<&crate::terminal::holder::Record>,
    previous: Option<&crate::terminal::holder::Record>,
) -> Result<(), String> {
    if source.terminal.is_some() != pair.is_some()
        || pair.is_some_and(|record| {
            record.terminal != source.id
                || record.kind != Kind::Terminal
                || !registry::record_application_matches(record)
                || source
                    .proof
                    .as_ref()
                    .is_none_or(|proof| proof.boot_id != record.boot)
                || previous.is_some_and(|old| old != record)
        })
    {
        return Err("Paired view does not retain the exact original terminal holder".into());
    }
    if (console.is_some() && source.terminal.is_none())
        || source.terminal.as_ref().is_some_and(|terminal| {
            terminal.id != source.id
                || terminal.continuity.live_proof().as_ref() != source.proof.as_ref()
                || match console {
                    Some(recipe) => {
                        &terminal.launch != recipe || terminal.owned_codex_listener.is_some()
                    }
                    None => terminal.owned_codex_listener != source.owned_codex_listener,
                }
        })
    {
        return Err("Paired view changed issued console or native conversation ownership".into());
    }
    Ok(())
}

struct Server {
    actor: LocalHandle,
    identity: ActorIdentity,
    source: chat::Session,
    lineage: Vec<chat::Session>,
    execution_console: Option<crate::model::LaunchRecipe>,
    paired_holder: Option<crate::terminal::holder::Record>,
    proof: HostSessionProof,
    epoch: uuid::Uuid,
    installed: bool,
    journal: Arc<Mutex<Journal>>,
    changed: Arc<std::sync::atomic::AtomicU64>,
    prompt_revision: Arc<std::sync::atomic::AtomicU64>,
    pending: Vec<(
        InputIdentity,
        crossbeam_channel::Receiver<Result<(), String>>,
        Option<(crate::model::RunId, Arc<std::sync::atomic::AtomicU8>)>,
    )>,
    account_reports: Arc<Mutex<Vec<Update>>>,
    environment: crate::terminal_ipc::ChatExecutionEnvironment,
    recovery_environment: BTreeMap<String, String>,
    listener: UnixListener,
    socket: PathBuf,
    socket_identity: (u64, u64),
    _directory: Option<Arc<File>>,
    closed_snapshot: Option<Arc<Snapshot>>,
    session_stopping: bool,
    // A successful ownership handoff fences authorizing commands from the old
    // attachment. A newly authenticated attach may own the still-live actor.
    attachment_preserved: bool,
    close_only: bool,
}
impl Server {
    fn start_after_scope_enrollment(
        startup: Startup,
        control: &UnixStream,
        enroll: impl FnOnce() -> std::io::Result<()>,
    ) -> Result<Self, String> {
        // This is optional OOM/cgroup isolation, not launch authorization.
        // Keep the authenticated, descriptor-sealed process-group fallback
        // used by terminal/native holders when the user manager rejects it.
        let _ = enroll();
        Self::start(startup, control)
    }

    fn start(startup: Startup, control: &UnixStream) -> Result<Self, String> {
        if startup.protocol != PROTOCOL
            || startup.source.launch.provider != crate::agent::Provider::Claude
        {
            return Err("Incompatible retained Claude actor launch".into());
        }
        let proof = startup
            .source
            .proof
            .clone()
            .ok_or("Retained actor launch lacks daemon proof")?;
        let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
        if proof.boot_id != boot {
            return Err("Retained actor launch belongs to another boot".into());
        }
        let mut operational = startup.source.launch.clone();
        operational.recipe.working_directory = startup.working_directory.clone();
        operational.recipe.environment = startup.environment.clone();
        operational.recipe.unset_environment.clear();
        let mut spec_and_adapter =
            chat::protocol::claude::create_prepared(&operational, startup.prepared_identity)?;
        if !matches!(spec_and_adapter.0.transport, TransportSpec::JsonLines) {
            return Err("Retained actor requires stream-json transport".into());
        }
        let directory = startup
            .directory_fd
            .map(|fd| unsafe { Arc::new(File::from_raw_fd(fd)) });
        let mut execution = match startup.directory_identity {
            Some(identity) => ExecutionContext::restored(identity),
            None => ExecutionContext::new(None),
        };
        execution.directory = directory.clone();
        if startup.provider_directory_observed {
            execution.observe_provider_directory(&startup.working_directory)?;
        }
        let spec = &mut spec_and_adapter.0;
        spec.program = startup.program;
        spec.arguments = startup.arguments;
        spec.environment = startup.environment;
        spec.working_directory = startup.working_directory;
        spec.unset_environment.clear();
        spec.execution = Some(Arc::new(execution));
        spec.messages = startup
            .mailbox_recognition
            .map(|recognition| Arc::new(recognition.into_recognizer()));
        let environment = crate::terminal_ipc::ChatExecutionEnvironment(spec.environment.clone());
        let initial = spec_and_adapter.1.initialize()?;
        let mut initial = initial;
        let store = super::super::preflight(spec, &mut initial)?;
        let journal = Arc::new(Mutex::new(Journal::open(store.directory())?));
        let changed = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let published = changed.clone();
        let prompt_revision = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let available = prompt_revision.clone();
        let outcomes = journal.clone();
        let account_reports = Arc::new(Mutex::new(Vec::<Update>::new()));
        let reported = account_reports.clone();
        if disconnected(control)? {
            return Err("Retained actor launch cancelled before process creation".into());
        }
        let actor = LocalHandle::start_retained(
            spec_and_adapter.0,
            spec_and_adapter.1,
            initial,
            store,
            startup.source.clone(),
            move |update| {
                if matches!(update, Update::PromptAvailable) {
                    available.fetch_add(1, Ordering::Release);
                }
                let mut journal = outcomes.lock().unwrap_or_else(|p| p.into_inner());
                if let Err(error) = journal.append(&update) {
                    let _ = journal.record_loss(&format!(
                        "Retained actor outcome continuity is unconfirmed: {error}"
                    ));
                }
                drop(journal);
                if let Update::AccountUsage { report, .. } = &update {
                    let mut reports = reported.lock().unwrap_or_else(|p| p.into_inner());
                    reports.retain(|previous|!matches!(previous,Update::AccountUsage {report:previous,..} if std::mem::discriminant(previous)==std::mem::discriminant(report)));
                    if reports.len() < 16 {
                        reports.push(update);
                    }
                }
                published.fetch_add(1, Ordering::Release);
            },
        )?;
        let provider = actor
            .provider_process()
            .ok_or("Retained actor did not retain its exact provider process")?;
        let directory_path = registry::directory(Kind::Native)?;
        crate::secure_file::prepare_private_directory(&directory_path)
            .map_err(|error| error.to_string())?;
        let socket = registry::socket(&directory_path, startup.source.id);
        let listener = registry::bind(&socket).map_err(|error| error.to_string())?;
        let metadata = std::fs::symlink_metadata(&socket).map_err(|error| error.to_string())?;
        let socket_identity = (metadata.dev(), metadata.ino());
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let mut build = registry::BuildIdentity::current(Kind::Native);
        build.capabilities.push(CAPABILITY.into());
        let identity = ActorIdentity {
            instance: uuid::Uuid::new_v4(),
            record: Record {
                version: 4,
                build: Some(build),
                kind: Kind::Native,
                application: crate::instance::APPLICATION_ID.into(),
                boot,
                terminal: startup.source.id,
                holder: Process::read(std::process::id() as i32)
                    .map_err(|error| error.to_string())?,
                root: provider,
            },
        };
        identity
            .record
            .create(&directory_path)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            actor,
            identity,
            source: startup.source,
            lineage: Vec::new(),
            execution_console: None,
            paired_holder: None,
            proof,
            epoch: uuid::Uuid::new_v4(),
            installed: false,
            journal,
            changed,
            prompt_revision,
            pending: Vec::new(),
            account_reports,
            environment,
            recovery_environment: startup.recovery_environment,
            listener,
            socket,
            socket_identity,
            _directory: directory,
            closed_snapshot: None,
            session_stopping: false,
            attachment_preserved: false,
            close_only: false,
        })
    }
    fn notifier(&self) -> impl Fn(Update) + Send + Sync + 'static {
        let outcomes = self.journal.clone();
        let changed = self.changed.clone();
        let available = self.prompt_revision.clone();
        let reports = self.account_reports.clone();
        move |update| {
            if matches!(update, Update::PromptAvailable) {
                available.fetch_add(1, Ordering::Release);
            }
            let mut journal = outcomes.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(error) = journal.append(&update) {
                let _ = journal.record_loss(&format!(
                    "Retained actor outcome continuity is unconfirmed: {error}"
                ));
            }
            drop(journal);
            if let Update::AccountUsage { report, .. } = &update {
                let mut entries = reports.lock().unwrap_or_else(|p| p.into_inner());
                entries.retain(|previous|!matches!(previous,Update::AccountUsage {report:previous,..} if std::mem::discriminant(previous)==std::mem::discriminant(report)));
                if entries.len() < 16 {
                    entries.push(update);
                }
            }
            changed.fetch_add(1, Ordering::Release);
        }
    }
    fn revision(&self) -> u64 {
        self.changed.load(Ordering::Acquire)
    }
    fn resident(&self) -> Result<Resident, String> {
        self.resident_after_copy(|| {})
    }
    fn resident_after_copy(&self, after_copy: impl FnOnce()) -> Result<Resident, String> {
        // Sample before copying any metadata. A concurrent actor publication
        // may make this conservative token old, which forces the next poll;
        // stamping the newest token after copying could hide Ready forever.
        let changed = self.revision();
        let prompt_revision = self.prompt_revision.load(Ordering::Acquire);
        // Clone resident metadata only. Do not hydrate the Store or serialize
        // any growing parent/child tool body to service periodic observation.
        let state = self.actor.0.state.lock().unwrap_or_else(|p| p.into_inner());
        let generation = state.generation;
        let turn_generation = state.turn_generation;
        let exited = state.exited;
        let session_id = state.session_id.clone();
        let title = state.title.clone();
        let ready = state.ready;
        let busy = state.busy;
        let waiting = state.waiting;
        let usage = state.usage.clone();
        let requests = state.requests.clone();
        let sampled_activity = state.activity();
        let prompt_eligible =
            super::super::accepts_prompt(&state) && !self.actor.0.draining.load(Ordering::Acquire);
        use crate::notifications::Kind;
        let notification_content = [
            Kind::Succeeded,
            Kind::Failed,
            Kind::Cancelled,
            Kind::Waiting,
            Kind::ReviewRequested,
            Kind::Interrupted,
            Kind::MessageReceived,
        ]
        .into_iter()
        .map(|kind| (kind, state.notification_content(kind)))
        .collect();
        drop(state);
        after_copy();
        let completed = self.actor.is_completed();
        let close = CloseObservation {
            actor_id: self.identity.instance,
            turn_generation,
            activity: sampled_activity,
            ready,
            exited,
            completed,
            closing: self.session_stopping || completed,
            owns_work: self.actor.owns_work(),
        };
        let activity = close.activity;
        let working = activity == crate::agent::ActivityState::Working;
        let ready = ready && close.ready;
        let idle = prompt_eligible && !completed && !close.closing;
        let owns_work = close.owns_work;
        let completed = close.completed;
        let provider_process = self.actor.provider_process();
        let actor_failure = self.actor.failure();
        let confirmed_session = self.actor.confirmed_session_id();
        let background_subagents = self.actor.background_subagents();
        let interrupt_subagents = self.actor.interrupt_subagents();
        let account_reports = self
            .account_reports
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        // Metadata can become a public descriptor through ChatList without
        // hydrating a snapshot. Retain the exact sampled IDs before sending
        // them, even if the reducer concurrently advances to another thread.
        let continuity = if self.installed {
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
        } else {
            chat::NativeContinuity::ReconnectOnly
        };
        self.remember_publication(session_id.as_deref(), &continuity)?;
        self.remember_publication(confirmed_session.as_deref(), &continuity)?;
        let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        let continuity_loss = journal.continuity_loss()?;
        let failure = continuity_loss
            .as_ref()
            .map(|loss| loss.reason.clone())
            .or(actor_failure);
        Ok(Resident {
            provider_process,
            generation,
            session_id,
            title,
            ready,
            busy,
            waiting,
            working,
            activity,
            usage,
            requests,
            notification_content,
            prompt_revision,
            native_continuity: if self.installed {
                chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
            } else {
                chat::NativeContinuity::ReconnectOnly
            },
            close,
            installed: self.installed,
            owns_work: owns_work || failure.is_some(),
            completed,
            idle,
            failure,
            confirmed_session,
            background_subagents,
            interrupt_subagents,
            account_reports,
            next_input: journal.next_input()?,
            acknowledged_input_count: journal.acknowledged_input_count(),
            continuity_loss,
            retained_runs: journal.retained_runs()?,
            outbox_ack: journal.acknowledged()?,
            outbox_latest: journal.latest()?,
            changed,
        })
    }

    fn published_sources(&self) -> Vec<chat::Session> {
        let confirmed = self.actor.confirmed_session_id();
        let continuity = if self.installed {
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
        } else {
            chat::NativeContinuity::ReconnectOnly
        };
        let mut sources = self.lineage.clone();
        for source in self
            .journal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .published_sources()
        {
            if !sources.contains(source) {
                sources.push(source.clone());
            }
        }
        for source in self.lineage.iter().chain(std::iter::once(&self.source)) {
            let projected = published_source(source, confirmed.as_deref(), &continuity);
            if projected != self.source && !sources.contains(&projected) {
                sources.push(projected);
            }
        }
        sources
    }
    /// Journal commit precedes every public identity-bearing response. The
    /// journal skips exact duplicates, so ordinary observation does not write.
    fn remember_publication(
        &self,
        confirmed: Option<&str>,
        continuity: &chat::NativeContinuity,
    ) -> Result<chat::Session, String> {
        let source = published_source(&self.source, confirmed, continuity);
        self.remember_descriptor(&source)?;
        Ok(source)
    }
    fn remember_descriptor(&self, source: &chat::Session) -> Result<(), String> {
        if self
            .journal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .published_sources()
            .contains(source)
        {
            return Ok(());
        }
        let mut lineage = self.published_sources();
        if !lineage.contains(source) {
            lineage.push(source.clone());
        }
        validate_source_lineage(&lineage)?;
        self.journal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remember_published_source(source)
    }
    fn retained_sources_for_publication(&self) -> Result<Vec<chat::Session>, String> {
        // The reducer can advance between sampling fields of one response.
        // Commit this exact captured list, not a later current-ID projection.
        let sources = self.published_sources();
        for source in &sources {
            self.remember_descriptor(source)?;
        }
        Ok(sources)
    }
    fn attached(&self) -> Result<Attached, String> {
        self.remember_descriptor(&self.source)?;
        Ok(Attached {
            protocol: PROTOCOL,
            close_only: self.close_only,
            identity: self.identity.clone(),
            epoch: self.epoch,
            resident: self.resident()?,
            source: self.source.clone(),
            launch_account_scope: self
                .actor
                .launch_account_scope()
                .cloned()
                .map_err(str::to_owned),
            execution_environment: self.environment.clone(),
            recovery_environment: self.recovery_environment.clone(),
        })
    }
    fn finish_inputs(&mut self) {
        let journal = self.journal.clone();
        let completed = self.actor.is_completed();
        let changed = self.changed.clone();
        self.pending.retain(|(identity,response,effects)| {
            if !completed && effects.as_ref().is_some_and(|(_,state)|state.load(Ordering::Acquire)==0) {return true;}
            let result=match response.try_recv() {Ok(result)=>Some(InputState::Acknowledged(result)),Err(crossbeam_channel::TryRecvError::Disconnected)=>Some(InputState::Unknown("The retained actor stopped before acknowledging this input; never replay it automatically".into())),Err(crossbeam_channel::TryRecvError::Empty)=>None};
            if let Some(state)=result {
                let mut journal=journal.lock().unwrap_or_else(|p|p.into_inner());
                if matches!(&state,InputState::Acknowledged(Err(_))) && let Some((run,effects))=effects && effects.load(Ordering::Acquire)==1 {
                    if let Err(error)=journal.refuse_unexecuted_run(*run) {let _=journal.record_loss(&error);}
                }
                if let Err(error)=journal.finish(identity,state) {let _=journal.record_loss(&format!("Cannot persist retained input outcome: {error}"));}
                changed.fetch_add(1,Ordering::Release);
                false
            } else {true}
        });
    }
    fn pinned_launch_directory(
        &self,
        request: &Invocation,
    ) -> Result<(crate::agent_execution::DirectoryIdentity, Arc<File>), String> {
        if request.epoch != Some(self.epoch) || request.proof != self.proof {
            return Err(
                "Retained actor attachment was superseded; stale directory queries are refused"
                    .into(),
            );
        }
        if self.close_only {
            return Err(close_only_refusal());
        }
        if !self.installed || self.attachment_preserved || self.closed_snapshot.is_some() {
            return Err("Retained provider has no current installed directory owner".into());
        }
        self.actor.pinned_launch_directory()
    }
    fn invoke(&mut self, request: Invocation) -> Result<Value, String> {
        if request.method == "inspect" {
            self.remember_descriptor(&self.source)?;
            let previous_sources = self.retained_sources_for_publication()?;
            #[cfg(test)]
            tests::after_inspection_sources();
            return serde_json::to_value(ActorInspection {
                identity: self.identity.clone(),
                source: self.source.clone(),
                previous_sources,
                execution_console: self.execution_console.clone(),
                paired_holder: self.paired_holder.clone(),
                state: self.resident()?,
                launch_account_scope: self
                    .actor
                    .launch_account_scope()
                    .cloned()
                    .map_err(str::to_owned),
                execution_environment: self.environment.clone(),
                recovery_environment: self.recovery_environment.clone(),
            })
            .map_err(|error| error.to_string());
        }
        if matches!(request.method.as_str(), "attach" | "attach_close") {
            if request.epoch.is_some() {
                return Err("Attach must authenticate a new epoch; an existing attachment cannot upgrade its capability".into());
            }
            let close_only = request.method == "attach_close";
            if !close_only {
                self.actor
                    .launch_account_scope()
                    .map_err(str::to_owned)?
                    .revalidate()?;
            }
            let (source, expected): (chat::Session, Record) =
                serde_json::from_value(request.args).map_err(|error| error.to_string())?;
            if expected != self.identity.record {
                return Err(
                    "Retained attachment names another exact actor holder or provider".into(),
                );
            }
            if source != self.source && !self.published_sources().contains(&source) {
                return Err(
                    "Retained actor source/account identity does not match this chat".into(),
                );
            }
            self.epoch = uuid::Uuid::new_v4();
            self.close_only = close_only;
            self.attachment_preserved = false;
            self.proof = request.proof;
            if self.closed_snapshot.is_some() {
                self.publish_close()?;
            }
            return serde_json::to_value(self.attached()?).map_err(|error| error.to_string());
        }
        if request.epoch != Some(self.epoch) || request.proof != self.proof {
            return Err(
                "Retained actor attachment was superseded; stale input and ACKs are refused".into(),
            );
        }
        if self.close_only && !close_only_method(&request.method) {
            return Err(close_only_refusal());
        }
        macro_rules! decode {
            ($type:ty) => {
                serde_json::from_value::<$type>(request.args).map_err(|error| error.to_string())?
            };
        }
        macro_rules! value {
            ($expression:expr) => {
                serde_json::to_value($expression).map_err(|error| error.to_string())
            };
        }
        if self.attachment_preserved
            && matches!(
                request.method.as_str(),
                "restart"
                    | "fence_session_stop"
                    | "close"
                    | "close_until"
                    | "close_if_revision"
                    | "close_if_idle"
                    | "try_close_if_idle"
                    | "relocate_directory"
                    | "update_source"
            )
        {
            return Err("This attachment preserved provider ownership; authenticate a new attachment before changing it".into());
        }
        match request.method.as_str() {
            "account_switch_generation"
            | "completed_account_options"
            | "account_switch_notice"
            | "restore_account_options"
            | "close_for_account_switch"
            | "restart"
            | "pinned_directory"
            | "capture_directory"
            | "relocate_directory"
            | "exit_output" => {
                Err("This retained actor protocol does not support this optional operation".into())
            }
            "recognize_sources" => {
                if !self.installed {
                    return Err("Retained source recognition requires installed ownership".into());
                }
                let (sources,) = decode!((Vec<chat::Session>,));
                if sources.is_empty() || sources.len() > 2 {
                    return Err("Retained source recognition requires one exact source pair".into());
                }
                // The journal and authenticated update_source path record exact
                // issued aliases. No descriptor normalization grants authority.
                let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
                if !sources.iter().all(|source| {
                    source == &self.source
                        || self.lineage.contains(source)
                        || journal.published_sources().contains(source)
                }) {
                    return Err("Retained execution source is not an exact issued alias".into());
                }
                Ok(Value::Null)
            }
            "resolve_source" => {
                if !self.installed
                    || self.session_stopping
                    || self.closed_snapshot.is_some()
                    || self.actor.is_completed()
                {
                    return Err("This retained actor is not a live installed attachment".into());
                }
                let (source,) = decode!((chat::Session,));
                if source != self.source && !self.published_sources().contains(&source) {
                    return Err(
                        "Retained source resolution requires an exact issued descriptor".into(),
                    );
                }
                value!(self.remember_publication(
                    self.actor.confirmed_session_id().as_deref(),
                    &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
                )?)
            }
            "execution_environment" => value!(self.environment.clone()),
            "state" => {
                let (after,) = decode!((u64,));
                if after == self.revision() {
                    Ok(Value::Null)
                } else {
                    value!(Some(self.resident()?))
                }
            }
            "outbox_unacked" => {
                let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
                value!(journal.page(journal.acknowledged()?)?)
            }
            "outbox" => {
                let (after,) = decode!((u64,));
                value!(
                    self.journal
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .page(after)?
                )
            }
            "ack_loss" => {
                let (id,) = decode!((uuid::Uuid,));
                self.journal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .ack_loss(id)?;
                self.changed.fetch_add(1, Ordering::Release);
                Ok(Value::Null)
            }
            "ack_outbox" => {
                let (through,) = decode!((u64,));
                self.journal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .ack(through)?;
                self.changed.fetch_add(1, Ordering::Release);
                Ok(Value::Null)
            }
            "input_receipt" => {
                let (identity,) = decode!((InputIdentity,));
                value!(
                    self.journal
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .input(&identity)?
                )
            }
            "input_receipts" => {
                let (identities,) = decode!((Vec<InputIdentity>,));
                if identities.len() > 65 {
                    return Err("Input receipt batch exceeds capacity".into());
                }
                let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
                let mut receipts = Vec::new();
                for identity in identities {
                    if let Some(receipt) = journal.input(&identity)? {
                        receipts.push(receipt);
                    }
                }
                value!(receipts)
            }
            "acknowledged_inputs" => value!(
                self.journal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .acknowledged_input_page()?
            ),
            "retire_acknowledged_inputs" => {
                let (identities,) = decode!((Vec<InputIdentity>,));
                self.journal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retire_acknowledged_inputs(&identities)?;
                self.changed.fetch_add(1, Ordering::Release);
                Ok(Value::Null)
            }
            "ack_inputs" => {
                let (through,) = decode!((u64,));
                self.journal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retire_inputs(through)?;
                self.changed.fetch_add(1, Ordering::Release);
                Ok(Value::Null)
            }
            "input" => {
                if !self.installed {
                    return Err(
                        "Retained actor installation is not confirmed; provider input is refused"
                            .into(),
                    );
                }
                let (identity, payload) = decode!((InputIdentity, Value));
                if payload_digest(&serde_json::to_vec(&payload).map_err(|error| error.to_string())?)
                    != identity.hash
                {
                    return Err("Retained input hash differs from its envelope".into());
                }
                let mut journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(receipt) = journal.input(&identity)? {
                    return value!(receipt);
                }
                if self.session_stopping {
                    return Err(
                        "This actor lifetime is stopping; new provider input is refused".into(),
                    );
                }
                if self.attachment_preserved {
                    return Err("This attachment preserved provider ownership; new provider input is refused".into());
                }
                if self.pending.len() >= 65 {
                    return Err("Retained actor input completion capacity reached".into());
                }
                let (command, run, effects) = match serde_json::from_value::<InputPayload>(payload)
                    .map_err(|error| error.to_string())?
                {
                    InputPayload::IdlePrompt { text } => {
                        (super::super::ActorCommand::IdlePrompt(text), None, None)
                    }
                    InputPayload::Command {
                        run,
                        command,
                        decline_only,
                    } => {
                        let state = Arc::new(std::sync::atomic::AtomicU8::new(0));
                        (
                            super::super::ActorCommand::RetainedProvider {
                                run,
                                command,
                                decline_only,
                                effects: state.clone(),
                            },
                            run,
                            run.map(|run| (run, state)),
                        )
                    }
                    InputPayload::Interrupt {
                        run,
                        including_subagents,
                    } => (
                        super::super::ActorCommand::Interrupt {
                            run,
                            including_subagents,
                        },
                        None,
                        None,
                    ),
                };
                let receipt = journal.admit_with_run(&identity, run)?;
                self.changed.fetch_add(1, Ordering::Release);
                match self.actor.enqueue_actor(command) {
                    Ok(reply) => self.pending.push((identity, reply, effects)),
                    Err(error) => {
                        journal.finish(&identity, InputState::Acknowledged(Err(error)))?;
                        if let Some(run) = run {
                            journal.refuse_unexecuted_run(run)?;
                        }
                    }
                }
                value!(receipt)
            }
            "snapshot" => {
                let (after,) = decode!((Option<u64>,));
                let mut snapshot = self.actor.snapshot(after);
                if let Some(snapshot) = &mut snapshot
                    && let Some(loss) = self
                        .journal
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .continuity_loss()?
                {
                    snapshot.error = Some(loss.reason.clone());
                    snapshot.ready = false;
                }
                if let Some(snapshot) = &snapshot {
                    self.remember_publication(
                        snapshot.session_id.as_deref(),
                        &snapshot.native_continuity,
                    )?;
                }
                value!(snapshot)
            }
            "presentation" => {
                let (id, after) = decode!((crate::model::TerminalId, Option<u64>));
                if id != self.source.id {
                    return Err("Retained presentation names another chat".into());
                }
                // Loss is outside the reducer's transcript generation. Refuse
                // even an unchanged-generation read instead of hiding it as
                // an empty publication or inventing a transcript revision.
                if let Some(loss) = self
                    .journal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .continuity_loss()?
                {
                    return Err(loss.reason);
                }
                let response = self.actor.presentation(id, after);
                let snapshot = match &response {
                    Some(crate::terminal_ipc::ServerResponse::ChatSnapshot {
                        snapshot: Some(snapshot),
                        ..
                    }) => Some(snapshot),
                    Some(crate::terminal_ipc::ServerResponse::ChatUpdate { update, .. }) => {
                        Some(&update.metadata)
                    }
                    _ => None,
                };
                if let Some(snapshot) = snapshot {
                    self.remember_publication(
                        snapshot.session_id.as_deref(),
                        &snapshot.native_continuity,
                    )?;
                }
                value!(response)
            }
            "text_copy" => {
                let (source, offset) = decode!((chat::TextCopySource, u64));
                value!(self.actor.text_copy(&source, offset)?)
            }
            "file_query" => {
                let (command,) = decode!((Command,));
                value!(self.actor.file_query(&command)?)
            }
            "visible" => {
                let (visible,) = decode!((bool,));
                self.actor.set_visible(visible);
                Ok(Value::Null)
            }
            "observe" => {
                self.actor.observe_next_update();
                Ok(Value::Null)
            }
            "readiness" => value!(self.actor.check_update_readiness()?),
            "commit" => {
                // Installation publishes a real retained capability for the
                // exact issued descriptor, even if it still has no thread ID.
                // Seal that pre-ID alias before confirmation can replace its
                // current projection. This survives HOST loss in this helper;
                // it is not a disk-resume claim after the helper itself dies.
                let published = published_source(
                    &self.source,
                    None,
                    &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
                );
                let mut lineage = self.lineage.clone();
                if published != self.source && !lineage.contains(&published) {
                    lineage.push(published);
                }
                validate_source_lineage(&lineage)?;
                for source in &lineage {
                    self.remember_descriptor(source)?;
                }
                self.lineage = lineage;
                self.installed = true;
                confirm_installed_snapshot(
                    &mut self.actor.0.state.lock().unwrap_or_else(|p| p.into_inner()),
                );
                self.changed.fetch_add(1, Ordering::Release);
                Ok(Value::Null)
            }
            "preserve" => {
                if !self.installed {
                    return Err("Cannot preserve an uninstalled actor".into());
                }
                if self.session_stopping {
                    return Err("This actor lifetime is stopping; ownership cannot be preserved for a successor".into());
                }
                if !self.attachment_preserved {
                    self.attachment_preserved = true;
                    self.changed.fetch_add(1, Ordering::Release);
                }
                Ok(Value::Null)
            }
            "update_source" => {
                let (source, console, pair) = decode!((
                    chat::Session,
                    Option<crate::model::LaunchRecipe>,
                    Option<crate::terminal::holder::Record>
                ));
                if self.close_only
                    && (!close_only_terminal_projection(
                        &self.source,
                        &source,
                        &self.proof,
                        self.paired_holder.is_none() && self.execution_console.is_none(),
                    ) || console != self.execution_console
                        || pair != self.paired_holder)
                {
                    return Err(
                        "Close-only source projection cannot change paired ownership".into(),
                    );
                }
                let mut permitted = self.published_sources();
                permitted.push(self.source.clone());
                let matches = permitted.iter().any(|previous| {
                    let mut expected = previous.clone();
                    expected.proof = Some(self.proof.clone());
                    expected.terminal = source.terminal.clone();
                    expected == source
                });
                if !matches {
                    return Err(
                        "Retained source update changed provider/account/issued recipe identity"
                            .into(),
                    );
                }
                validate_pair(
                    &source,
                    console.as_ref(),
                    pair.as_ref(),
                    self.paired_holder.as_ref(),
                )?;
                let mut lineage = self.published_sources();
                if source != self.source && !lineage.contains(&self.source) {
                    lineage.push(self.source.clone());
                }
                validate_source_lineage(&lineage)?;
                for previous in &lineage {
                    self.remember_descriptor(previous)?;
                }
                self.remember_descriptor(&source)?;
                self.lineage = lineage;
                self.source = source;
                self.execution_console = console;
                self.paired_holder = pair;
                Ok(Value::Null)
            }
            "close_if_idle" => {
                let (generation,) = decode!((u64,));
                match self.try_close_if_idle(generation)? {
                    ConditionalClosePublication::Changed => {
                        Err("Provider state changed before conditional idle close".into())
                    }
                    ConditionalClosePublication::Closed(publication) => value!(publication),
                }
            }
            "try_close_if_idle" => {
                let (generation,) = decode!((u64,));
                value!(self.try_close_if_idle(generation)?)
            }
            "close" => {
                if self.closed_snapshot.is_none() {
                    self.actor.close()?;
                    reap_orphans()?;
                    self.finish_inputs();
                }
                value!(self.publish_close()?)
            }
            "retire" => {
                self.check_retirement()?;
                // The reducer and its final input completions have stopped.
                // Keep the journal locked/sealed across the exact retirement
                // receipt and cache disposition. A failed publication retains
                // both helper ownership and the non-disposable evidence.
                self.remember_publication(
                    self.actor.confirmed_session_id().as_deref(),
                    &if self.installed {
                        chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
                    } else {
                        chat::NativeContinuity::ReconnectOnly
                    },
                )?;
                self.session_stopping = true;
                // Publication is single-threaded in this helper and the actor
                // is already closed. Collect aliases before holding the journal
                // lock across the retirement receipt/cache disposition.
                self.remember_descriptor(&self.source)?;
                let mut previous_sources = self.retained_sources_for_publication()?;
                if !previous_sources.contains(&self.source) {
                    previous_sources.push(self.source.clone());
                }
                let confirmed = self.actor.confirmed_session_id();
                let mut journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
                journal.seal_for_retirement()?;
                let source = published_source(
                    &self.source,
                    confirmed.as_deref(),
                    &if self.installed {
                        chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
                    } else {
                        chat::NativeContinuity::ReconnectOnly
                    },
                );
                final_snapshot::publish_retirement(RetirementReceipt {
                    installed: self.installed,
                    identity: self.identity.clone(),
                    proof: self.proof.clone(),
                    source,
                    previous_sources,
                    launch_account_scope: self
                        .actor
                        .launch_account_scope()
                        .cloned()
                        .map_err(str::to_owned),
                    confirmed_session: confirmed,
                    paired_holder: self.paired_holder.clone(),
                })?;
                if self.installed {
                    journal.allow_retired_cache_removal()?;
                }
                Ok(Value::Null)
            }
            _ => Err("Unknown retained actor method/version".into()),
        }
    }
    fn check_retirement(&self) -> Result<(), String> {
        if self.closed_snapshot.is_none() {
            return Err("Stop the exact retained provider before retiring its helper".into());
        }
        let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        if journal.latest()? != journal.acknowledged()?
            || journal
                .continuity_loss()?
                .as_ref()
                .is_some_and(|loss| !loss.acknowledged)
        {
            return Err(
                "Final retained outcomes must be durably acknowledged before helper retirement"
                    .into(),
            );
        }
        Ok(())
    }
    fn try_close_if_idle(
        &mut self,
        generation: u64,
    ) -> Result<ConditionalClosePublication, String> {
        if self.closed_snapshot.is_none() {
            if !self.actor.try_close_if_idle(generation)? {
                return Ok(ConditionalClosePublication::Changed);
            }
            reap_orphans()?;
            self.finish_inputs();
        }
        self.publish_close()
            .map(ConditionalClosePublication::Closed)
    }
    fn publish_close(&mut self) -> Result<ClosePublication, String> {
        let snapshot = if let Some(snapshot) = &self.closed_snapshot {
            snapshot.as_ref().clone()
        } else {
            let mut snapshot = self
                .actor
                .snapshot(None)
                .ok_or("The actor has no final publication")?;
            if let Some(loss) = self
                .journal
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .continuity_loss()?
            {
                snapshot.error = Some(loss.reason.clone());
            }
            snapshot
        };
        self.remember_publication(snapshot.session_id.as_deref(), &snapshot.native_continuity)?;
        // A closing actor may be adopted by a new authenticated daemon before
        // its last semantic ACK. Refresh receipt authority without changing
        // the exact immutable final publication.
        let previous_sources = self.retained_sources_for_publication()?;
        final_snapshot::save(
            self.installed,
            &self.identity,
            &self.proof,
            &self.source,
            &previous_sources,
            self.paired_holder.as_ref(),
            self.actor.launch_account_scope(),
            self.actor.confirmed_session_id().as_deref(),
            &snapshot,
        )?;
        self.closed_snapshot = Some(Arc::new(snapshot.clone()));
        Ok(ClosePublication {
            snapshot,
            resident: self.resident()?,
        })
    }
    fn serve(mut self, startup: UnixStream) -> Result<(), String> {
        send(
            &startup,
            &Ok::<_, String>(self.attached()?),
            Duration::from_secs(30),
        )?;
        loop {
            self.finish_inputs();
            if !self.installed && disconnected(&startup)? {
                self.actor.close()?;
                reap_orphans()?;
                break;
            }
            match self.listener.accept() {
                Ok((control, _)) => {
                    let request = (|| {
                        let request: Invocation = receive(&control, TIMEOUT)?;
                        authorize(&control, &request.proof)?;
                        Ok::<_, String>(request)
                    })();
                    if let Ok(request) = &request
                        && request.method == "pinned_directory"
                    {
                        let pin = self.pinned_launch_directory(request);
                        let response = pin
                            .as_ref()
                            .map(|(identity, _)| *identity)
                            .map_err(Clone::clone);
                        let descriptors: Vec<_> = pin
                            .as_ref()
                            .ok()
                            .map(|(_, file)| vec![file.as_fd()])
                            .unwrap_or_default();
                        let _ = crate::descriptor_frame::send_json_with_fds_until(
                            &control,
                            &response,
                            &descriptors,
                            Instant::now() + TIMEOUT,
                            MAX_FRAME,
                            1,
                            crate::descriptor_frame::FrameByteOrder::LittleEndian,
                        );
                        continue;
                    }
                    let mut finished = false;
                    let response = request.and_then(|request| {
                        let close = request.method == "retire";
                        let result = self.invoke(request);
                        finished = close && result.is_ok();
                        result
                    });
                    // Authentication failures use the same result boundary. Only an
                    // explicit retirement after durable semantic ACK exits the
                    // helper; a lost provider-close reply retains its journal.
                    let _ = send(&control, &response, TIMEOUT);
                    if finished {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        if std::fs::symlink_metadata(&self.socket)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.socket_identity)
        {
            std::fs::remove_file(&self.socket).map_err(|error| error.to_string())?;
        }
        self.identity
            .record
            .remove_record(&registry::directory(Kind::Native)?)
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn validate_record(record: &Record) -> Result<(), String> {
    if record.kind != Kind::Native
        || !registry::record_application_matches(record)
        || !record.holder.is_current()
        || record.boot != crate::daemon::read_boot_id().map_err(|error| error.to_string())?
        || !record.build.as_ref().is_some_and(|build| {
            build
                .capabilities
                .iter()
                .any(|capability| capability == CAPABILITY)
        })
    {
        return Err("Native holder is incompatible with retained Claude actor protocol; no input was replayed".into());
    }
    Ok(())
}
fn verify_peer(control: &UnixStream, record: &Record) -> Result<(), String> {
    let credentials = peer(control)?;
    if credentials.uid != crate::daemon::current_uid()
        || credentials.pid != record.holder.pid
        || !record.holder.is_current()
    {
        return Err("Retained actor socket peer identity changed".into());
    }
    Ok(())
}
fn authorize_reader(proof: &HostSessionProof) -> Result<(), String> {
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    let owner = authority_result(crate::daemon::lease_owner(&paths.lock, proof))?;
    if owner != std::process::id() {
        return Err("Retained evidence reader does not own the current daemon lease".into());
    }
    Ok(())
}
fn validate_evidence_record(record: &Record, proof: &HostSessionProof) -> Result<(), String> {
    if !RemoteActor::is_record(record)
        || !registry::record_application_matches(record)
        || record.boot != proof.boot_id
        || !proof.is_complete()
        || record.boot != crate::daemon::read_boot_id().map_err(|error| error.to_string())?
    {
        return Err("Retained evidence belongs to another helper protocol, role, or boot".into());
    }
    Ok(())
}
fn authorize(control: &UnixStream, proof: &HostSessionProof) -> Result<(), String> {
    let credentials = peer(control)?;
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    let owner = authority_result(crate::daemon::lease_owner(&paths.lock, proof))?;
    if credentials.uid != crate::daemon::current_uid()
        || u32::try_from(credentials.pid).ok() != Some(owner)
    {
        return Err("CAGENTS_ACTOR_AUTHORITY_UNCONFIRMED: Retained actor caller does not own the current daemon lease".into());
    }
    Ok(())
}
fn peer(control: &UnixStream) -> Result<libc::ucred, String> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            control.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(credentials)
}
fn send(control: &UnixStream, value: &impl Serialize, timeout: Duration) -> Result<(), String> {
    deadline_io::send_until(control, value, Instant::now() + timeout)
}
fn receive<T: DeserializeOwned>(control: &UnixStream, timeout: Duration) -> Result<T, String> {
    deadline_io::receive_until(control, Instant::now() + timeout)
}
fn disconnected(control: &UnixStream) -> Result<bool, String> {
    let mut event = libc::pollfd {
        fd: control.as_raw_fd(),
        events: libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    if unsafe { libc::poll(&mut event, 1, 0) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(event.revents & (libc::POLLHUP | libc::POLLERR) != 0)
}
/// Only after the actor's ChildGuard has joined: no competing provider reaper
/// remains. A child PID cannot be recycled until this sole reaper consumes it.
fn reap_orphans() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let children =
            std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))
                .map_err(|error| error.to_string())?;
        for pid in children
            .split_whitespace()
            .filter_map(|pid| pid.parse::<i32>().ok())
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        loop {
            match unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } {
                pid if pid > 0 => continue,
                0 => break,
                _ if std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => {
                    return Ok(());
                }
                _ if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => {
                    continue;
                }
                _ => return Err(std::io::Error::last_os_error().to_string()),
            }
        }
        if Instant::now() >= deadline {
            return Err(
                "Retained actor descendants have not finished cleanup; ownership is retained"
                    .into(),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}
/// The daemon dispatches this after Rust runtime initialization. Ordinary
/// invocations do not inspect descriptors or alter process lifecycle state.
pub(crate) fn run_helper_if_requested() -> Option<Result<(), String>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_none_or(|argument| argument != ARGUMENT)
    {
        return None;
    }
    Some((|| {
        if arguments.len() != 2 {
            return Err("Retained actor helper requires exactly one control descriptor".into());
        }
        let fd = arguments[1]
            .to_str()
            .and_then(|text| {
                let fd = text.parse::<RawFd>().ok()?;
                (fd > 2 && fd.to_string() == text).then_some(fd)
            })
            .ok_or("Retained actor helper control descriptor is invalid")?;
        validate_control_descriptor(fd)?;
        // SAFETY: the exact helper-only descriptor has been checked as a
        // connected Unix stream socket and is now owned by this dispatch.
        let control = unsafe { UnixStream::from_raw_fd(fd) };
        let startup: Startup = receive(&control, Duration::from_secs(30))?;
        authorize(
            &control,
            startup
                .source
                .proof
                .as_ref()
                .ok_or("Retained startup lacks proof")?,
        )?;
        // No provider, signal policy, subreaper or enrollment work occurs
        // until the connected peer proves current daemon lease ownership.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::prctl(libc::PR_SET_NAME, c"cagents-native".as_ptr());
        }
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let result = (|| {
            crate::daemon::seal_inherited_descriptors().map_err(|error| error.to_string())?;
            let server = Server::start_after_scope_enrollment(
                startup,
                &control,
                crate::daemon::process_scope::enroll_holder,
            )?;
            server.serve(control.try_clone().map_err(|error| error.to_string())?)
        })();
        if let Err(error) = result {
            let _ = send(&control, &Err::<Attached, _>(error), TIMEOUT);
        }
        // Exit status 73 means cleanup is proved, even when startup itself was
        // refused. Failure to reap cannot be reported as successful teardown.
        reap_orphans()
    })())
}
fn validate_control_descriptor(fd: RawFd) -> Result<(), String> {
    fn option(fd: RawFd, name: libc::c_int) -> Result<libc::c_int, String> {
        let mut value = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&raw mut value).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(value)
    }
    if option(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
        || option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM
    {
        return Err("Retained helper control must be a connected Unix stream socket".into());
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    if unsafe { libc::getpeername(fd, (&raw mut address).cast(), &mut length) } != 0 {
        return Err("Retained helper control socket is not connected".into());
    }
    Ok(())
}
fn helper_executable() -> Result<PathBuf, String> {
    #[cfg(test)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            .map(PathBuf::from).ok_or("Actual retained-helper tests require CAGENTS_TEST_RETAINED_ACTOR_HELPER pointing to the same-head cagentsd binary")?;
        if !path.is_absolute() {
            return Err("Retained-helper test executable must be absolute".into());
        }
        let metadata = path.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err("Retained-helper test executable is not an executable file".into());
        }
        Ok(path)
    }
    #[cfg(not(test))]
    {
        std::env::current_exe().map_err(|error| error.to_string())
    }
}

fn confirm_installed_snapshot(state: &mut Snapshot) {
    let durable = chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
    if state.native_continuity != durable {
        state.native_continuity = durable;
        state.generation = state.generation.saturating_add(1);
    }
}
fn authority_result<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, String> {
    result.map_err(|error| format!("CAGENTS_ACTOR_AUTHORITY_UNCONFIRMED: {error}"))
}
fn terminal_endpoint_error(error: &str, holder_current: bool) -> bool {
    !holder_current
        || error.contains("attachment was superseded")
        || error.starts_with("CAGENTS_ACTOR_AUTHORITY_UNCONFIRMED:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_scope_request_still_starts_and_retires_a_real_retained_server() {
        const CHILD: &str = "CAGENTS_RETAINED_SCOPE_FALLBACK_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let test = thread::current().name().unwrap().to_owned();
            let output = ChildCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("HOME", root.path().join("home"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cwd = root.path().join("cwd");
        let home = root.path().join("claude");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&home).unwrap();
        let marker = root.path().join("initialized");
        let mut source = fixture_source();
        source.launch.recipe.working_directory = cwd.clone();
        source.launch.recipe.environment = BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                home.to_string_lossy().into_owned(),
            ),
        ]);
        source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
        let proof = source.proof.clone().unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let (control, parent) = UnixStream::pair().unwrap();
        // The real caller/lease gate is independent of optional cgroup setup.
        authorize(&control, &proof).unwrap();
        let mut wrong = proof.clone();
        wrong.host_incarnation = crate::model::HostIncarnation::new("another-owner");
        assert!(authorize(&control, &wrong).is_err());
        let startup = |protocol| {
            let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
            let session_id = spec
                .arguments
                .iter()
                .find_map(|argument| argument.strip_prefix("--session-id="))
                .expect("fresh Claude launch must issue a session ID")
                .to_owned();
            let prepared = adapter.prepared_launch_identity().unwrap();
            Startup {
                protocol,
                source: source.clone(),
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    r#"IFS= read -r frame || exit 2
id=${frame#*\"request_id\":\"}
id=${id%%\"*}
printf initialized > "$2"
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"current_permission_mode":"default"}}}\n' "$id"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$1"
printf '{"type":"assistant","uuid":"scope-output","message":{"id":"scope-output","role":"assistant","content":[{"type":"text","text":"scope fallback initialized"}]}}\n'
printf '{"type":"result","subtype":"success","is_error":false,"session_id":"%s"}\n' "$1"
"#
                    .into(),
                    "scope-fixture".into(),
                    session_id,
                    marker.to_string_lossy().into_owned(),
                ],
                prepared_identity: prepared,
                environment: source.launch.recipe.environment.clone(),
                working_directory: cwd.clone(),
                directory_fd: None,
                directory_identity: None,
                mailbox_recognition: None,
                provider_directory_observed: false,
                recovery_environment: BTreeMap::new(),
            }
        };
        let reject = || {
            assert!(
                !marker.exists(),
                "enrollment must precede provider creation"
            );
            let error = crate::daemon::process_scope::tests::rejected_scope_request().unwrap_err();
            assert_eq!(
                error.to_string(),
                "user scope request failed: exit status: 1"
            );
            Err(error)
        };
        let error = Server::start_after_scope_enrollment(startup(0), &control, reject)
            .err()
            .unwrap();
        assert!(error.contains("Incompatible retained Claude actor launch"));
        assert!(!marker.exists(), "required startup failures remain fatal");
        let server =
            Server::start_after_scope_enrollment(startup(PROTOCOL), &control, reject).unwrap();
        let socket = server.socket.clone();
        let record = server.identity.record.clone();
        let record_path = registry::directory(Kind::Native)
            .unwrap()
            .join(format!("{}.json", record.terminal));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = server.actor.snapshot(None).unwrap();
            if server.actor.is_completed() {
                assert!(snapshot.exited);
                assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
                assert!(snapshot.items.iter().any(|item| matches!(
                    &item.content,
                    chat::ItemContent::Text { text, .. } if text == "scope fallback initialized"
                )));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "finite native fixture did not exit"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&marker).unwrap(), b"initialized");
        assert!(
            !record.root.is_current(),
            "original provider must be reaped"
        );
        assert!(socket.exists() && record_path.exists());
        struct Serving {
            parent: Option<UnixStream>,
            worker: Option<thread::JoinHandle<Result<(), String>>>,
        }
        impl Drop for Serving {
            fn drop(&mut self) {
                self.parent.take();
                if let Some(worker) = self.worker.take() {
                    // The explicit success path checks the result below.
                    // Still join on unwind without causing a double panic.
                    let _ = worker.join();
                }
            }
        }
        let mut serving = Serving {
            parent: Some(parent),
            worker: Some(thread::spawn(move || server.serve(control))),
        };
        let attached: Result<Attached, String> =
            receive(serving.parent.as_ref().unwrap(), TIMEOUT).unwrap();
        assert_eq!(attached.unwrap().identity.record, record);
        // No installation was committed: startup EOF performs the existing
        // normal actor cleanup and removes only its exact socket/record.
        serving.parent.take();
        serving.worker.take().unwrap().join().unwrap().unwrap();
        assert!(!socket.exists() && !record_path.exists());
    }

    thread_local! {
        static INSPECT_SOURCES_CAPTURED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }
    pub(super) fn after_inspection_sources() {
        INSPECT_SOURCES_CAPTURED.with(|slot| {
            if let Some(cutpoint) = slot.borrow_mut().take() {
                cutpoint();
            }
        });
    }
    struct Fixture {
        server: Server,
        _root: tempfile::TempDir,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.server.actor.close();
        }
    }
    fn resident_fixture() -> Fixture {
        resident_fixture_with_body(None)
    }
    fn resident_fixture_with_body(body: Option<String>) -> Fixture {
        use crate::chat::protocol::{Adapter, Step};
        struct Idle;
        impl Adapter for Idle {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step {
                    events: vec![chat::Event::Ready(true)],
                    ..Default::default()
                })
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let remote = RemoteActor::fixture();
        let mut identity = remote.identity();
        let proof = remote.0.proof.clone();
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: root.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let source = chat::Session {
            id: identity.record.terminal,
            launch: chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: recipe.clone(),
                session_id: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        let spec = ProcessSpec::from(&recipe);
        let mut adapter: Box<dyn Adapter> = Box::new(Idle);
        let mut initial = adapter.initialize().unwrap();
        if let Some(text) = body {
            // Readiness is the fixture's startup barrier. Publish its body
            // first so a ready reader cannot race an empty Copy source list.
            initial.events.insert(
                0,
                chat::Event::Item(chat::Item {
                    id: "retained-body".into(),
                    role: chat::Role::Assistant,
                    content: chat::ItemContent::Text {
                        text,
                        thinking: false,
                        phase: Default::default(),
                    },
                    started_at: None,
                    at: None,
                }),
            );
        }
        let store = super::super::super::preflight(&spec, &mut initial).unwrap();
        let actor = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !actor.0.state.lock().unwrap().ready {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        identity.instance = uuid::Uuid::new_v4();
        let journal_directory = root.path().join("journal");
        std::fs::create_dir(&journal_directory).unwrap();
        let socket = root.path().join("fixture.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let metadata = std::fs::symlink_metadata(&socket).unwrap();
        Fixture {
            server: Server {
                actor,
                identity,
                source,
                lineage: Vec::new(),
                execution_console: None,
                paired_holder: None,
                proof,
                epoch: uuid::Uuid::new_v4(),
                installed: true,
                journal: Arc::new(Mutex::new(Journal::open(&journal_directory).unwrap())),
                changed: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                prompt_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                pending: Vec::new(),
                account_reports: Arc::new(Mutex::new(Vec::new())),
                environment: crate::terminal_ipc::ChatExecutionEnvironment(Default::default()),
                recovery_environment: BTreeMap::new(),
                listener,
                socket,
                socket_identity: (metadata.dev(), metadata.ino()),
                _directory: None,
                closed_snapshot: None,
                session_stopping: false,
                attachment_preserved: false,
                close_only: false,
            },
            _root: root,
        }
    }
    #[test]
    fn continuity_loss_refuses_unchanged_presentation_without_mutating_source_or_copy() {
        let body = "Retained answer λ\n  preserve source and copy";
        let mut fixture = resident_fixture_with_body(Some(body.into()));
        let server = &mut fixture.server;
        let source = server.source.clone();
        let proof = server.proof.clone();
        let epoch = server.epoch;
        let id = source.id;
        let request = |method: &str, args| Invocation {
            proof: proof.clone(),
            epoch: Some(epoch),
            method: method.into(),
            args,
        };
        let before: Snapshot =
            serde_json::from_value(server.invoke(request("snapshot", json!([null]))).unwrap())
                .unwrap();
        assert!(before.ready);
        let copy = before.text_copy_sources.conversation[0].clone().unwrap();
        let before_page: chat::TextCopyPage = serde_json::from_value(
            server
                .invoke(request("text_copy", json!([copy, 0])))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(before_page.text, body);
        let generation = before.generation;
        assert_eq!(
            server
                .invoke(request("presentation", json!([id, generation])))
                .unwrap(),
            Value::Null,
        );
        let loss = "Retained outcome journal commit failed; run outcome is unconfirmed";
        server.journal.lock().unwrap().record_loss(loss).unwrap();
        // FULL, matching, and lagged cursors all report the same loss before
        // the reducer can suppress reads or hydrate a transcript window.
        for after in [None, Some(generation), Some(generation.saturating_sub(1))] {
            assert_eq!(
                server
                    .invoke(request("presentation", json!([id, after])))
                    .unwrap_err(),
                loss,
            );
        }
        let after: Snapshot =
            serde_json::from_value(server.invoke(request("snapshot", json!([null]))).unwrap())
                .unwrap();
        assert_eq!(
            after.generation, generation,
            "loss invents no transcript revision"
        );
        assert_eq!(after.error.as_deref(), Some(loss));
        assert!(!after.ready);
        assert_eq!(after.items, before.items);
        assert_eq!(after.text_copy_sources, before.text_copy_sources);
        assert_eq!(after.session_id, before.session_id);
        assert_eq!(after.exited, before.exited);
        assert_eq!(server.source, source);
        assert!(server.actor.owns_work());
        assert!(!server.actor.is_completed());
        assert!(!server.session_stopping);
        assert!(server.closed_snapshot.is_none());
        let copied: chat::TextCopyPage = serde_json::from_value(
            server
                .invoke(request("text_copy", json!([copy, 0])))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(copied, before_page);
    }
    #[test]
    fn removed_or_replaced_home_can_only_adopt_the_exact_helper_for_close() {
        const CHILD: &str = "CAGENTS_RETAINED_CLOSE_ONLY_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let test = thread::current().name().unwrap().to_owned();
            let output = ChildCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("HOME", root.path().join("home"))
                .env("CAGENTS_LOCAL_DEBUG", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        struct Owner(RemoteActor);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.close();
                let _ = self.0.finish_close();
            }
        }
        fn bypass_proxy(actor: &RemoteActor, method: &str, args: Value) -> Result<Value, String> {
            let control = registry::connect(&actor.0.socket).map_err(|error| error.to_string())?;
            verify_peer(&control, &actor.identity().record)?;
            send(
                &control,
                &Invocation {
                    proof: actor.0.proof.clone(),
                    epoch: Some(actor.0.epoch),
                    method: method.into(),
                    args,
                },
                TIMEOUT,
            )?;
            receive::<Result<Value, String>>(&control, TIMEOUT)?
        }
        for replaced in [false, true] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let home = root.path().join("claude");
            let previous_home = root.path().join("previous-claude");
            let cwd = root.path().join("cwd");
            std::fs::create_dir(&home).unwrap();
            std::fs::create_dir(&cwd).unwrap();
            let mut source = fixture_source();
            source.launch.recipe = crate::model::LaunchRecipe {
                program: "/bin/sleep".into(),
                arguments: vec!["600".into()],
                working_directory: cwd.clone(),
                environment: BTreeMap::from([
                    ("HOME".into(), root.path().to_string_lossy().into_owned()),
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        home.to_string_lossy().into_owned(),
                    ),
                ]),
                unset_environment: Default::default(),
            };
            source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
            let proof = source.proof.clone().unwrap();
            let paths = crate::daemon::RuntimePaths::discover().unwrap();
            let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
            // This idle fake provider deliberately substitutes argv, while
            // obtaining its typed adapter identity from real preparation.
            // Fake provider argv is intentionally independent of Claude flags;
            // the adapter selection still comes from the real constructor.
            let prepared = chat::protocol::create(&source.launch)
                .unwrap()
                .1
                .prepared_launch_identity()
                .unwrap();
            let mut spec = ProcessSpec::from(&source.launch.recipe);
            spec.execution = Some(Arc::new(ExecutionContext::capture(&cwd).unwrap()));
            let original = Owner(
                RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {})
                    .unwrap(),
            );
            original.0.commit_installation().unwrap();
            let identity = original.0.identity();
            let captured_scope = original.0.launch_account_scope().unwrap().clone();
            if replaced {
                std::fs::rename(&home, &previous_home).unwrap();
                std::fs::create_dir(&home).unwrap();
            } else {
                std::fs::remove_dir(&home).unwrap();
            }
            std::fs::rename(&cwd, root.path().join("original-cwd")).unwrap();
            std::fs::create_dir(&cwd).unwrap();
            assert!(
                RemoteActor::adopt(&identity.record, source.clone(), proof.clone(), |_| {})
                    .is_err()
            );
            let mut wrong_source = source.clone();
            wrong_source.launch.recipe.environment.insert(
                "CLAUDE_CONFIG_DIR".into(),
                root.path().join("foreign").to_string_lossy().into_owned(),
            );
            assert!(
                RemoteActor::adopt_for_close(&identity.record, wrong_source, proof.clone(), |_| {})
                    .is_err()
            );
            let mut wrong_record = identity.record.clone();
            wrong_record.root.start = wrong_record.root.start.wrapping_add(1);
            assert!(
                RemoteActor::adopt_for_close(&wrong_record, source.clone(), proof.clone(), |_| {})
                    .is_err()
            );
            let mut wrong_proof = proof.clone();
            wrong_proof.host_incarnation =
                crate::model::HostIncarnation::new("not-the-current-lease");
            assert!(
                RemoteActor::adopt_for_close(&identity.record, source.clone(), wrong_proof, |_| {})
                    .is_err()
            );
            assert!(identity.record.root.is_current());
            let close = Owner(
                RemoteActor::adopt_for_close(
                    &identity.record,
                    source.clone(),
                    proof.clone(),
                    |_| {},
                )
                .unwrap(),
            );
            assert!(close.0.is_close_only());
            assert_eq!(close.0.identity(), identity);
            assert_eq!(close.0.launch_account_scope(), Ok(&captured_scope));
            assert!(
                original.0.call::<()>("preserve", Value::Null).is_err(),
                "close attachment supersedes the old normal epoch"
            );
            assert!(matches!(
                close.0.submit(
                    None,
                    Command::Prompt {
                        text: "must never reach provider".into(),
                        attachments: Vec::new(),
                        mode: chat::PromptMode::Send
                    }
                ),
                Err(Rejection::Refused(_))
            ));
            assert!(close.0.interrupt(None, true).is_err());
            assert!(close.0.preserve().is_err());
            assert!(close.0.commit_installation().is_err());
            assert!(
                close
                    .0
                    .call::<()>("file_query", json!([Command::Refresh]))
                    .is_err()
            );
            for method in [
                "input",
                "file_query",
                "text_copy",
                "preserve",
                "commit",
                "restart",
            ] {
                assert!(
                    bypass_proxy(&close.0, method, Value::Null)
                        .unwrap_err()
                        .contains("close-only"),
                    "server must fence {method} independently of proxy"
                );
            }
            assert!(bypass_proxy(&close.0, "attach", json!([source, identity.record])).is_err());
            assert!(close.0.pending_inputs().is_empty());
            assert_eq!(close.0.state().next_input, 1);
            assert!(close.0.snapshot(None).is_some());
            close.0.refresh_state().unwrap();
            if replaced {
                std::fs::remove_dir(&home).unwrap();
                std::fs::rename(&previous_home, &home).unwrap();
                let normal = Owner(
                    RemoteActor::adopt(&identity.record, source.clone(), proof.clone(), |_| {})
                        .unwrap(),
                );
                assert!(!normal.0.is_close_only());
                assert!(
                    close.0.call::<()>("observe", Value::Null).is_err(),
                    "normal adoption invalidates old close-only epoch"
                );
                assert_eq!(normal.0.identity(), identity);
                std::fs::rename(&home, &previous_home).unwrap();
                std::fs::create_dir(&home).unwrap();
                let final_close = Owner(
                    RemoteActor::adopt_for_close(
                        &identity.record,
                        source.clone(),
                        proof.clone(),
                        |_| {},
                    )
                    .unwrap(),
                );
                assert!(normal.0.call::<()>("preserve", Value::Null).is_err());
                final_close.0.close().unwrap();
                final_close.0.finish_close().unwrap();
            } else {
                close.0.close().unwrap();
                close.0.finish_close().unwrap();
            }
            assert!(!identity.record.root.is_current());
            assert!(!identity.record.holder.is_current());
            let retired = RemoteActor::retired_actor_receipt(&identity.record, &proof)
                .unwrap()
                .unwrap();
            assert_eq!(retired.identity, identity);
            assert_eq!(retired.launch_account_scope, Ok(captured_scope));
            assert!(retired.paired_holder.is_none());
        }
    }
    #[test]
    fn authenticated_directory_pin_keeps_original_after_replacement_and_epoch_change() {
        const CHILD: &str = "CAGENTS_RETAINED_DIRECTORY_PIN_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let name = thread::current().name().unwrap().to_owned();
            let output = ChildCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("HOME", root.path().join("home"))
                .env("CAGENTS_LOCAL_DEBUG", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(root.path().join("claude")).unwrap();
        std::fs::write(cwd.join("marker"), "original").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let provider = cwd.join("fixture-claude");
        std::fs::write(
            &provider,
            r#"#!/bin/sh
printf '%s\n' "$@" > arguments
session=
for argument do
    case "$argument" in --session-id=*) session=${argument#--session-id=} ;; esac
done
[ -n "$session" ] || exit 2
printf '%s' "$session" > session-id
/bin/cat marker > observed
IFS= read -r frame || exit 3
id=${frame#*\"request_id\":\"}
id=${id%%\"*}
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"current_permission_mode":"default"}}}\n' "$id"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$session"
exec /bin/sleep 60
"#,
        )
        .unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        let expected = crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(&cwd).unwrap(),
        );
        let mut source = fixture_source();
        source.launch.recipe = crate::model::LaunchRecipe {
            program: provider.to_str().unwrap().to_owned(),
            arguments: Vec::new(),
            working_directory: cwd.clone(),
            environment: BTreeMap::from([
                ("HOME".into(), root.path().to_string_lossy().into_owned()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    root.path().join("claude").to_string_lossy().into_owned(),
                ),
            ]),
            unset_environment: Default::default(),
        };
        source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
        let proof = source.proof.clone().unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        let issued_session = spec
            .arguments
            .iter()
            .find_map(|argument| argument.strip_prefix("--session-id="))
            .expect("real fresh Claude preparation must issue its exact session ID")
            .to_owned();
        uuid::Uuid::parse_str(&issued_session).unwrap();
        let public_arguments = spec.arguments.clone();
        assert!(spec.execution.is_none()); // ordinary, no inherited startup directory FD
        let prepared = adapter.prepared_launch_identity().unwrap();
        struct Owner(RemoteActor);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.close();
                let _ = self.0.finish_close();
            }
        }
        let original = Owner(
            RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {}).unwrap(),
        );
        original.0.commit_installation().unwrap();
        let record = original.0.identity().record;
        let deadline = Instant::now() + TIMEOUT;
        while std::fs::read_to_string(cwd.join("observed"))
            .ok()
            .as_deref()
            != Some("original")
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read_to_string(cwd.join("arguments")).unwrap(),
            public_arguments.join("\n") + "\n",
            "the private executable receives the real adapter's complete public argv"
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("session-id")).unwrap(),
            issued_session
        );
        while original.0.state().session_id.as_deref() != Some(issued_session.as_str()) {
            assert!(
                Instant::now() < deadline,
                "prepared session identity was not observed"
            );
            original.0.refresh_state().unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        std::fs::rename(&cwd, root.path().join("moved")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("marker"), "replacement").unwrap();
        assert_eq!(original.0.pinned_launch_directory().unwrap().0, expected);
        let control = registry::connect(&original.0.0.socket).unwrap();
        let mut wrong = proof.clone();
        wrong.host_incarnation = crate::model::HostIncarnation::new("wrong-directory-query");
        send(
            &control,
            &Invocation {
                proof: wrong,
                epoch: Some(original.0.0.epoch),
                method: "pinned_directory".into(),
                args: Value::Null,
            },
            TIMEOUT,
        )
        .unwrap();
        let (refused, rights) = crate::descriptor_frame::receive_json_with_fds_until::<
            Result<crate::agent_execution::DirectoryIdentity, String>,
        >(
            &control,
            Instant::now() + TIMEOUT,
            MAX_FRAME,
            1,
            crate::descriptor_frame::FrameByteOrder::LittleEndian,
        )
        .unwrap();
        assert!(refused.is_err());
        assert!(rights.is_empty());
        let adopted =
            Owner(RemoteActor::adopt(&record, source.clone(), proof.clone(), |_| {}).unwrap());
        assert!(
            original.0.pinned_launch_directory().is_err(),
            "stale epoch cannot receive directory authority"
        );
        let (identity, pin) = adopted.0.pinned_launch_directory().unwrap();
        assert_eq!(identity, expected);
        let fd = unsafe {
            libc::openat(
                pin.as_raw_fd(),
                c"marker".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        assert!(fd >= 0);
        let mut text = String::new();
        use std::io::Read;
        unsafe { File::from_raw_fd(fd) }
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "original");
        assert_eq!(
            std::fs::read_to_string(cwd.join("marker")).unwrap(),
            "replacement"
        );
        let close = Owner(RemoteActor::adopt_for_close(&record, source, proof, |_| {}).unwrap());
        assert!(close.0.pinned_launch_directory().is_err());
        assert!(adopted.0.pinned_launch_directory().is_err());
        close.0.close().unwrap();
        close.0.finish_close().unwrap();
        assert!(!record.root.is_current());
    }

    #[test]
    fn helper_control_rejects_regular_files_and_unconnected_listeners() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let file = File::create(root.path().join("ordinary-file")).unwrap();
        assert!(validate_control_descriptor(file.as_raw_fd()).is_err());
        let listener = UnixListener::bind(root.path().join("listener.sock")).unwrap();
        assert!(validate_control_descriptor(listener.as_raw_fd()).is_err());
        let (client, server) = UnixStream::pair().unwrap();
        assert!(validate_control_descriptor(client.as_raw_fd()).is_ok());
        assert!(validate_control_descriptor(server.as_raw_fd()).is_ok());
    }
    #[test]
    fn helper_restores_descriptor_seal_before_any_provider_exec() {
        const CHILD: &str = "CAGENTS_ACTOR_DESCRIPTOR_SEAL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let isolated = tempfile::tempdir_in("/var/tmp").unwrap();
            let test = thread::current().name().unwrap().to_owned();
            let output = ChildCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", isolated.path())
                .env("XDG_DATA_HOME", isolated.path().join("data"))
                .env("XDG_CONFIG_HOME", isolated.path().join("config"))
                .env("XDG_CACHE_HOME", isolated.path().join("cache"))
                .env("HOME", isolated.path().join("home"))
                .env("CAGENTS_LOCAL_DEBUG", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        std::fs::create_dir(root.path().join("claude")).unwrap();
        let mut source = fixture_source();
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.session_id = None;
        source.launch.recipe = crate::model::LaunchRecipe {
            program: "/bin/sleep".into(),
            arguments: vec!["600".into()],
            working_directory: root.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), root.path().to_string_lossy().into_owned()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    root.path().join("claude").to_string_lossy().into_owned(),
                ),
            ]),
            unset_environment: Default::default(),
        };
        source.owned_codex_listener = None;
        source.terminal = None;
        source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease =
            crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
        let context = Arc::new(ExecutionContext::capture(root.path()).unwrap());
        let prepared = chat::protocol::create(&source.launch)
            .unwrap()
            .1
            .prepared_launch_identity()
            .unwrap();
        let mut spec = ProcessSpec::from(&source.launch.recipe);
        spec.execution = Some(context.clone());
        let limit = crate::host::descriptor_limit::tests::raise_with_fixture_limit();
        // This execs the real retained-helper entry and its real provider
        // spawning path. Omitting the entry seal leaks both inherited FDs.
        let actor = RemoteActor::spawn(spec, source, prepared, BTreeMap::new(), |_| {}).unwrap();
        struct Owner(RemoteActor);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.close();
                let _ = self.0.finish_close();
            }
        }
        let owner = Owner(actor);
        let actor = &owner.0;
        actor.commit_installation().unwrap();
        let provider = actor.state().provider_process.clone().unwrap();
        assert!(provider.is_current());
        crate::host::descriptor_limit::tests::assert_process_limit(provider.pid as u32, &limit);
        crate::host::descriptor_limit::tests::assert_process_limit(
            actor.native_holder_record().holder.pid as u32,
            &limit,
        );
        crate::host::descriptor_limit::tests::assert_daemon_limit_remains_raised(&limit);
        let inherited = std::fs::read_dir(format!("/proc/{}/fd", provider.pid))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                match std::fs::read_link(path) {
                    Ok(target) => Some(target),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("Cannot inspect exact fixture provider FD: {error}"),
                }
            })
            .collect::<Vec<_>>();
        actor.close().unwrap();
        actor.finish_close().unwrap();
        assert!(!provider.is_current());
        assert!(
            !inherited
                .iter()
                .any(|target| target.to_string_lossy().starts_with("socket:[")),
            "provider exec must not inherit the private helper startup control socket"
        );
        assert!(
            !inherited.iter().any(|target| target == root.path()),
            "provider exec must not retain the helper's approved-directory FD"
        );
        assert!(
            context
                .directory_descriptor()
                .unwrap()
                .metadata()
                .unwrap()
                .is_dir(),
            "the caller's approved pin remains usable"
        );
    }

    #[test]
    fn stopped_provider_keeps_its_semantic_journal_until_outcome_and_loss_ack() {
        for installed in [false, true] {
            let mut fixture = resident_fixture();
            let server = &mut fixture.server;
            server.installed = installed;
            server.actor.close().unwrap();
            server.closed_snapshot = Some(Arc::new(server.actor.snapshot(None).unwrap()));
            server
                .journal
                .lock()
                .unwrap()
                .append(&Update::Attention(crate::notifications::Evidence {
                    key: "final-turn".into(),
                    kind: crate::notifications::Kind::Succeeded,
                }))
                .unwrap();
            assert!(server.check_retirement().is_err());
            assert_eq!(
                server.journal.lock().unwrap().page(0).unwrap().len(),
                1,
                "provider teardown must not discard the uncommitted final outcome"
            );
            server.journal.lock().unwrap().ack(1).unwrap();
            server
                .journal
                .lock()
                .unwrap()
                .record_loss("fixture loss")
                .unwrap();
            assert!(server.check_retirement().is_err());
            let id = server
                .journal
                .lock()
                .unwrap()
                .continuity_loss()
                .unwrap()
                .unwrap()
                .id;
            server.journal.lock().unwrap().ack_loss(id).unwrap();
            assert!(server.check_retirement().is_ok());
        }
    }
    #[test]
    fn input_refusal_and_storage_failure_publish_new_resident_revisions() {
        for storage_failure in [false, true] {
            let mut fixture = resident_fixture();
            let server = &mut fixture.server;
            let run = crate::model::RunId::new();
            let identity = InputIdentity {
                sequence: 1,
                id: uuid::Uuid::new_v4(),
                hash: [9; 32],
            };
            {
                let mut journal = server.journal.lock().unwrap();
                journal.admit_with_run(&identity, Some(run)).unwrap();
                if storage_failure {
                    journal.fail_input_storage();
                }
            }
            let before = server.resident().unwrap();
            assert_eq!(before.retained_runs, vec![run]);
            let (reply, response) = crossbeam_channel::bounded(1);
            reply.send(Err("fixture refusal".to_owned())).unwrap();
            server.pending.push((
                identity,
                response,
                Some((
                    run,
                    Arc::new(std::sync::atomic::AtomicU8::new(if storage_failure {
                        2
                    } else {
                        1
                    })),
                )),
            ));
            server.finish_inputs();
            let response = server
                .invoke(Invocation {
                    proof: server.proof.clone(),
                    epoch: Some(server.epoch),
                    method: "state".into(),
                    args: json!([before.changed]),
                })
                .unwrap();
            let next: Option<Resident> = serde_json::from_value(response).unwrap();
            let next =
                next.expect("consumed completions must invalidate an earlier resident token");
            if storage_failure {
                assert!(next.continuity_loss.is_some());
                assert_eq!(next.continuity_loss.unwrap().affected_runs, vec![run]);
            } else {
                assert!(next.retained_runs.is_empty());
                assert!(next.continuity_loss.is_none());
            }
        }
    }
    #[test]
    fn retained_parent_accepts_prompts_while_background_children_keep_working() {
        let fixture = resident_fixture();
        {
            let mut state = fixture.server.actor.0.state.lock().unwrap();
            state.busy = false;
            state.subagents.push(chat::Subagent {
                id: "background-child".into(),
                kind: "fixture".into(),
                description: "Still running".into(),
                status: chat::ToolStatus::Running,
                model: None,
                waiting: false,
                detail: String::new(),
                background: true,
                items: Vec::new(),
            });
        }
        let resident = fixture.server.resident().unwrap();
        assert!(resident.working);
        assert!(!resident.busy);
        assert!(
            resident.idle,
            "parent admission is independent of aggregate child activity"
        );
        {
            let mut state = fixture.server.actor.0.state.lock().unwrap();
            state.busy = true;
        }
        assert!(!fixture.server.resident().unwrap().idle);
        fixture.server.actor.0.state.lock().unwrap().busy = false;
        let notify = fixture.server.notifier();
        notify(Update::PromptAvailable);
        let next = fixture.server.resident().unwrap();
        assert!(next.idle);
        assert_ne!(
            resident.prompt_revision, next.prompt_revision,
            "a coalesced parent busy/ready edge must remain observable across proxy polls"
        );
        let before = next.prompt_revision;
        notify(Update::Presentation);
        assert_eq!(fixture.server.resident().unwrap().prompt_revision, before);
    }
    #[test]
    fn protocol_one_attachment_is_refused_without_starting_or_retiring_its_owner() {
        let original = RemoteActor::fixture();
        let identity = original.identity();
        let proof = original.proof();
        let result = RemoteActor::from_attached(
            Attached {
                protocol: 1,
                close_only: false,
                identity: identity.clone(),
                epoch: uuid::Uuid::new_v4(),
                resident: original.state().as_ref().clone(),
                source: original.0.source.clone(),
                launch_account_scope: Err("legacy fixture".into()),
                execution_environment: Default::default(),
                recovery_environment: BTreeMap::new(),
            },
            proof,
            None,
            |_| panic!("an incompatible attachment cannot publish updates"),
        );
        assert!(result.err().unwrap().contains("incompatible"));
        assert_eq!(original.identity(), identity);
        assert!(original.state().owns_work);
        assert!(!original.0.stop.load(Ordering::Acquire));
        assert!(identity.record.root.is_current());
        assert!(original.pending_inputs().is_empty());
    }
    #[test]
    fn inspection_commits_the_exact_captured_alias_before_a_concurrent_reset() {
        let mut fixture = resident_fixture();
        let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        {
            let mut state = fixture.server.actor.0.state.lock().unwrap();
            state.session_id = Some(a.into());
            confirm_installed_snapshot(&mut state);
        }
        *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(a.into());
        let expected_a = published_source(
            &fixture.server.source,
            Some(a),
            &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
        );
        let actor = fixture.server.actor.clone();
        INSPECT_SOURCES_CAPTURED.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                actor.0.state.lock().unwrap().session_id = Some(b.into());
                *actor.0.confirmed_session.lock().unwrap() = Some(b.into());
            }))
        });
        let response = fixture
            .server
            .invoke(Invocation {
                proof: fixture.server.proof.clone(),
                epoch: None,
                method: "inspect".into(),
                args: json!([]),
            })
            .unwrap();
        let inspection: ActorInspection = serde_json::from_value(response).unwrap();
        assert!(inspection.previous_sources.contains(&expected_a));
        assert_eq!(inspection.state.session_id.as_deref(), Some(b));
        let db = rusqlite::Connection::open_with_flags(
            fixture._root.path().join("journal/actor-journal.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let bytes: Vec<u8> = db
            .query_row(
                "SELECT value FROM metadata WHERE key='published_sources'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let durable: Vec<chat::Session> = serde_json::from_slice(&bytes).unwrap();
        assert!(
            durable.contains(&expected_a),
            "the exact outgoing alias A must commit before resident B is sampled"
        );
    }

    #[test]
    fn public_identity_responses_commit_aliases_before_returning_and_skip_unchanged_writes() {
        for method in [
            "inspect",
            "state",
            "snapshot",
            "presentation",
            "resolve_source",
        ] {
            let mut fixture = resident_fixture();
            let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
            let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
            {
                let mut state = fixture.server.actor.0.state.lock().unwrap();
                state.session_id = Some(a.into());
                confirm_installed_snapshot(&mut state);
            }
            *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(a.into());
            let source = fixture.server.source.clone();
            let expected_a = published_source(
                &source,
                Some(a),
                &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
            );
            let path = fixture._root.path().join("journal/actor-journal.sqlite");
            let fault = rusqlite::Connection::open(&path).unwrap();
            fault.execute_batch("CREATE TRIGGER refuse_publication BEFORE INSERT ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'fixture publication refusal'); END;").unwrap();
            let invoke = |server: &mut Server| {
                let args = match method {
                    "inspect" => json!([]),
                    "state" => json!([0]),
                    "snapshot" => json!([null]),
                    "presentation" => json!([source.id, null]),
                    "resolve_source" => json!([source]),
                    _ => unreachable!(),
                };
                server.invoke(Invocation {
                    proof: server.proof.clone(),
                    epoch: Some(server.epoch),
                    method: method.into(),
                    args,
                })
            };
            assert!(
                invoke(&mut fixture.server)
                    .unwrap_err()
                    .contains("fixture publication refusal"),
                "no identity-bearing {method} response may precede the journal commit"
            );
            assert!(
                fixture
                    .server
                    .journal
                    .lock()
                    .unwrap()
                    .published_sources()
                    .is_empty()
            );
            fault
                .execute_batch("DROP TRIGGER refuse_publication;")
                .unwrap();
            invoke(&mut fixture.server).unwrap();
            let bytes: Vec<u8> = fault
                .query_row(
                    "SELECT value FROM metadata WHERE key='published_sources'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let durable: Vec<chat::Session> = serde_json::from_slice(&bytes).unwrap();
            assert!(
                durable.contains(&expected_a),
                "the returned {method} identity must already be durable"
            );
            fault.execute_batch("CREATE TRIGGER refuse_publication BEFORE INSERT ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'unchanged publication wrote'); END; CREATE TRIGGER refuse_publication_update BEFORE UPDATE ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'unchanged publication updated'); END;").unwrap();
            invoke(&mut fixture.server).expect("unchanged public identities must not write again");
            fault
                .execute_batch(
                    "DROP TRIGGER refuse_publication; DROP TRIGGER refuse_publication_update;",
                )
                .unwrap();
            {
                let mut state = fixture.server.actor.0.state.lock().unwrap();
                state.session_id = Some(b.into());
                state.generation += 1;
            }
            *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(b.into());
            let resolved = fixture
                .server
                .invoke(Invocation {
                    proof: fixture.server.proof.clone(),
                    epoch: Some(fixture.server.epoch),
                    method: "resolve_source".into(),
                    args: json!([expected_a]),
                })
                .unwrap();
            let resolved: chat::Session = serde_json::from_value(resolved).unwrap();
            assert_eq!(resolved.launch.session_id.as_deref(), Some(b));
            assert!(fixture.server.published_sources().contains(&expected_a));
            assert!(fixture.server.published_sources().contains(&resolved));
        }
    }

    #[test]
    fn source_resolution_checks_issued_membership_before_returning_current_thread() {
        let mut fixture = resident_fixture();
        let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        fixture.server.source.launch.session_id = Some(a.into());
        let issued = fixture.server.source.clone();
        *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(b.into());
        let invoke = |server: &mut Server, source: &chat::Session| {
            server.invoke(Invocation {
                proof: server.proof.clone(),
                epoch: Some(server.epoch),
                method: "resolve_source".into(),
                args: json!([source]),
            })
        };
        let resolved: chat::Session =
            serde_json::from_value(invoke(&mut fixture.server, &issued).unwrap()).unwrap();
        assert_eq!(resolved.launch.session_id.as_deref(), Some(b));
        assert_eq!(resolved.launch.recipe, issued.launch.recipe);
        assert_eq!(resolved.proof, issued.proof);
        assert!(matches!(
            resolved.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
        ));
        // A current projection is accepted too, without changing the issued
        // source or turning ordinary Ready/idle completion into process death.
        assert!(invoke(&mut fixture.server, &resolved).is_ok());
        for change in ["account", "cwd", "uuid", "erased", "proof", "listener"] {
            let mut foreign = issued.clone();
            match change {
                "account" => {
                    foreign
                        .launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), "/foreign/account".into());
                }
                "cwd" => foreign.launch.recipe.working_directory = "/foreign/project".into(),
                "uuid" => {
                    foreign.launch.session_id = Some("cccccccc-cccc-4ccc-8ccc-cccccccccccc".into())
                }
                "erased" => foreign.launch.session_id = None,
                "proof" => {
                    foreign.proof.as_mut().unwrap().host_incarnation =
                        crate::model::HostIncarnation::new("foreign")
                }
                "listener" => {
                    foreign.owned_codex_listener = Some(chat::OwnedCodexListener {
                        socket: "/foreign/codex.sock".into(),
                        allocator: foreign.proof.clone().unwrap(),
                    })
                }
                _ => unreachable!(),
            }
            assert!(
                invoke(&mut fixture.server, &foreign)
                    .unwrap_err()
                    .contains("exact issued descriptor"),
                "{change}"
            );
        }
        assert_eq!(fixture.server.source, issued);
        assert!(fixture.server.lineage.is_empty());
        assert!(fixture.server.pending.is_empty());
        assert_eq!(fixture.server.journal.lock().unwrap().latest().unwrap(), 0);
    }
    #[test]
    fn source_resolution_refuses_stale_cleanup_pending_and_process_completion() {
        for refusal in [
            "epoch",
            "proof",
            "close_only",
            "pending",
            "stopping",
            "closed",
            "completed",
        ] {
            let mut fixture = resident_fixture();
            let source = fixture.server.source.clone();
            let mut request = Invocation {
                proof: fixture.server.proof.clone(),
                epoch: Some(fixture.server.epoch),
                method: "resolve_source".into(),
                args: json!([source]),
            };
            match refusal {
                "epoch" => request.epoch = Some(uuid::Uuid::new_v4()),
                "proof" => {
                    request.proof.host_incarnation = crate::model::HostIncarnation::new("old")
                }
                "close_only" => fixture.server.close_only = true,
                "pending" => fixture.server.installed = false,
                "stopping" => fixture.server.session_stopping = true,
                "closed" => fixture.server.closed_snapshot = Some(Arc::new(Snapshot::default())),
                "completed" => fixture.server.actor.close().unwrap(),
                _ => unreachable!(),
            }
            let epoch = fixture.server.epoch;
            let error = fixture.server.invoke(request).unwrap_err();
            assert!(
                if matches!(refusal, "epoch" | "proof") {
                    error.contains("superseded")
                } else if refusal == "close_only" {
                    error.contains("close-only")
                } else {
                    error.contains("live installed")
                },
                "{refusal}: {error}"
            );
            assert_eq!(fixture.server.epoch, epoch);
            assert_eq!(fixture.server.source, source);
            assert!(fixture.server.pending.is_empty());
        }
    }
    #[test]
    fn source_resolution_uses_one_deadline_and_never_returns_a_cached_final_snapshot() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("resolve.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut actor = RemoteActor::fixture();
        Arc::get_mut(&mut actor.0).unwrap().socket = socket;
        let source = actor.0.source.clone();
        let epoch = actor.0.epoch;
        *actor.0.final_snapshot.lock().unwrap() = Some(Arc::new(Snapshot {
            exited: true,
            ..Default::default()
        }));
        let (release, wait) = crossbeam_channel::bounded(1);
        let peer = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            let request: Invocation = receive(&control, TIMEOUT).unwrap();
            assert_eq!(request.method, "resolve_source");
            assert_eq!(request.epoch, Some(epoch));
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let started = Instant::now();
        let error = actor
            .resolve_source_until(&source, started + Duration::from_millis(40))
            .unwrap_err();
        let elapsed = started.elapsed();
        let _ = release.send(());
        peer.join().unwrap();
        assert!(error.contains("deadline"), "{error}");
        assert!(elapsed < Duration::from_millis(250));
        assert_eq!(actor.0.epoch, epoch);
        assert!(actor.pending_inputs().is_empty());
    }
    #[test]
    fn close_only_discards_only_an_unowned_historical_terminal_view() {
        for actual_owner in ["none", "pair", "console"] {
            let mut fixture = resident_fixture();
            fixture.server.close_only = true;
            fixture.server.source.terminal = Some(
                crate::terminal_ipc::TerminalDescriptor::relaunchable(
                    fixture.server.source.id,
                    fixture.server.source.launch.recipe.clone(),
                    None,
                )
                .session,
            );
            if actual_owner == "pair" {
                let mut pair = fixture.server.identity.record.clone();
                pair.kind = Kind::Terminal;
                fixture.server.paired_holder = Some(pair);
            } else if actual_owner == "console" {
                fixture.server.execution_console =
                    Some(fixture.server.source.launch.recipe.clone());
            }
            let issued = fixture.server.source.clone();
            let mut cleared = issued.clone();
            cleared.terminal = None;
            let result = fixture.server.invoke(Invocation {
                proof: fixture.server.proof.clone(),
                epoch: Some(fixture.server.epoch),
                method: "update_source".into(),
                args: json!([
                    cleared,
                    fixture.server.execution_console,
                    fixture.server.paired_holder
                ]),
            });
            if actual_owner == "none" {
                result.unwrap();
                assert!(fixture.server.source.terminal.is_none());
                assert!(fixture.server.lineage.contains(&issued));
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .contains("cannot change paired ownership")
                );
                assert_eq!(fixture.server.source, issued);
                assert!(fixture.server.lineage.is_empty());
            }
            assert!(!fixture.server.actor.is_completed());
        }
    }
    #[test]
    fn resident_naming_and_notification_projections_do_not_need_a_helper_socket() {
        let actor = RemoteActor::fixture();
        assert_eq!(actor.conversation_title().unwrap(), (None, None));
        assert_eq!(
            actor.notification_content(crate::notifications::Kind::Waiting),
            Default::default()
        );
        assert!(!actor.0.socket.exists());
    }
    #[test]
    fn concurrent_ready_publication_cannot_be_hidden_by_a_new_revision_on_old_metadata() {
        let mut fixture = resident_fixture();
        fixture.server.actor.0.state.lock().unwrap().ready = false;
        let first = fixture
            .server
            .resident_after_copy(|| {
                let mut state = fixture.server.actor.0.state.lock().unwrap();
                state.ready = true;
                state.generation += 1;
                drop(state);
                fixture.server.changed.fetch_add(1, Ordering::Release);
            })
            .unwrap();
        assert!(!first.ready);
        assert_ne!(first.changed, fixture.server.revision());
        let response = fixture
            .server
            .invoke(Invocation {
                proof: fixture.server.proof.clone(),
                epoch: Some(fixture.server.epoch),
                method: "state".into(),
                args: json!([first.changed]),
            })
            .unwrap();
        let next: Option<Resident> = serde_json::from_value(response).unwrap();
        assert!(
            next.unwrap().ready,
            "the next real state RPC must fetch the raced Ready transition"
        );
    }
    #[test]
    fn read_only_inspection_of_a_stalled_exact_peer_obeys_one_budget_without_actor_changes() {
        let fixture = resident_fixture();
        let mut record = fixture.server.identity.record.clone();
        record.boot = crate::daemon::read_boot_id().unwrap();
        record
            .build
            .as_mut()
            .unwrap()
            .capabilities
            .push(CAPABILITY.into());
        let socket = fixture._root.path().join("deadline-inspect.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (release, wait) = crossbeam_channel::bounded(1);
        let peer = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            let request: Invocation = receive(&control, TIMEOUT).unwrap();
            assert_eq!(request.method, "inspect");
            assert!(request.epoch.is_none());
            // SO_PEERCRED has identified this exact Record.holder process, but
            // its inspection response stalls. The client must still return.
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let epoch = fixture.server.epoch;
        let source = fixture.server.source.clone();
        let started = Instant::now();
        let error = RemoteActor::inspect_at_until(
            &record,
            fixture.server.proof.clone(),
            &socket,
            started + Duration::from_millis(40),
        )
        .err()
        .expect("stalled inspection must time out");
        let elapsed = started.elapsed();
        let _ = release.send(());
        peer.join().unwrap();
        assert!(error.contains("deadline"));
        assert!(elapsed < Duration::from_millis(250));
        assert_eq!(fixture.server.epoch, epoch);
        assert_eq!(fixture.server.source, source);
        assert!(!fixture.server.actor.is_completed());
        assert!(fixture.server.pending.is_empty());
        assert_eq!(fixture.server.journal.lock().unwrap().latest().unwrap(), 0);
    }

    #[test]
    fn proved_close_wins_over_delayed_live_metadata_and_disconnection_publications() {
        let actor = RemoteActor::fixture();
        let delayed = actor.state().as_ref().clone();
        let snapshot = Snapshot {
            generation: 42,
            exited: true,
            ..Default::default()
        };
        actor.publish_closed(snapshot.clone());
        assert!(
            !actor.publish_resident(delayed),
            "the actual worker publication rechecks the stop fence under the state lock"
        );
        actor.publish_unavailable("late socket failure");
        let state = actor.state();
        assert!(state.completed);
        assert!(!state.owns_work);
        assert!(state.failure.is_none());
        assert_eq!(actor.snapshot(None), Some(snapshot));
    }
    #[test]
    fn proved_close_keeps_exact_final_publication_without_a_live_socket() {
        let actor = RemoteActor::fixture();
        let snapshot = Snapshot {
            generation: 42,
            session_id: Some("final-session".into()),
            title: Some("final-title".into()),
            exited: true,
            ..Default::default()
        };
        *actor.0.final_snapshot.lock().unwrap() = Some(Arc::new(snapshot.clone()));
        assert_eq!(actor.snapshot(None), Some(snapshot));
        assert!(actor.snapshot(Some(42)).is_none());
        assert!(actor.owns_work());
        actor.0.preserved.store(true, Ordering::Release);
        assert!(!actor.owns_work());
        assert!(
            actor.state().owns_work,
            "detaching this proxy does not claim provider teardown"
        );
    }
    #[test]
    fn commit_seals_exact_pre_id_capability_before_confirmation_race() {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        server.installed = false;
        let published = published_source(
            &server.source,
            None,
            &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
        );
        *server.actor.0.confirmed_session.lock().unwrap() = Some(uuid::Uuid::new_v4().to_string());
        let commit = || Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "commit".into(),
            args: Value::Null,
        };
        let request = commit();
        server.invoke(request).unwrap();
        assert!(server.installed);
        assert!(server.lineage.contains(&published));
        assert!(published.launch.session_id.is_none());
        let before = serde_json::to_vec(&server.lineage).unwrap();
        let request = Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "commit".into(),
            args: Value::Null,
        };
        server.invoke(request).unwrap();
        assert_eq!(
            serde_json::to_vec(&server.lineage).unwrap(),
            before,
            "lost commit ACK must permit idempotent repeat"
        );
        let stale = Invocation {
            proof: server.proof.clone(),
            epoch: Some(uuid::Uuid::new_v4()),
            method: "commit".into(),
            args: Value::Null,
        };
        assert!(server.invoke(stale).unwrap_err().contains("superseded"));
        assert_eq!(serde_json::to_vec(&server.lineage).unwrap(), before);
        let request = Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "update_source".into(),
            args: json!([published, null, null]),
        };
        server.invoke(request).unwrap();
        assert_eq!(server.source, published);
        let mut forged = published;
        forged.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let request = Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "update_source".into(),
            args: json!([forged, null, null]),
        };
        assert!(
            server.invoke(request).is_err(),
            "commit must not admit invented thread IDs"
        );
    }
    #[test]
    fn conditional_idle_close_refuses_stale_or_preserved_attachment_without_stopping_provider() {
        for refusal in ["epoch", "proof", "preserved"] {
            let mut fixture = resident_fixture();
            let server = &mut fixture.server;
            let generation = server.actor.snapshot(None).unwrap().generation;
            let mut request = Invocation {
                proof: server.proof.clone(),
                epoch: Some(server.epoch),
                method: "try_close_if_idle".into(),
                args: json!([generation]),
            };
            match refusal {
                "epoch" => request.epoch = Some(uuid::Uuid::new_v4()),
                "proof" => {
                    request.proof.host_incarnation = crate::model::HostIncarnation::new("old")
                }
                "preserved" => server.attachment_preserved = true,
                _ => unreachable!(),
            }
            assert!(server.invoke(request).is_err(), "{refusal}");
            assert!(!server.actor.is_completed(), "{refusal}");
            assert!(server.closed_snapshot.is_none(), "{refusal}");
            assert_eq!(server.actor.snapshot(None).unwrap().generation, generation);
        }
    }

    #[test]
    fn commit_alias_capacity_refuses_before_installation_or_lineage_mutation() {
        for byte_capacity in [false, true] {
            let mut fixture = resident_fixture();
            let server = &mut fixture.server;
            server.installed = false;
            let mut alias = server.source.clone();
            if byte_capacity {
                alias
                    .launch
                    .recipe
                    .environment
                    .insert("FIXTURE_METADATA".into(), "x".repeat(4 * 1024 * 1024));
                server.lineage.push(alias);
            } else {
                for _ in 0..1024 {
                    alias.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
                    server.lineage.push(alias.clone());
                }
            }
            let before = serde_json::to_vec(&server.lineage).unwrap();
            let continuity = server
                .actor
                .0
                .state
                .lock()
                .unwrap()
                .native_continuity
                .clone();
            let request = Invocation {
                proof: server.proof.clone(),
                epoch: Some(server.epoch),
                method: "commit".into(),
                args: Value::Null,
            };
            assert!(
                server
                    .invoke(request)
                    .unwrap_err()
                    .contains("lineage capacity")
            );
            assert!(!server.installed);
            assert_eq!(
                server.actor.0.state.lock().unwrap().native_continuity,
                continuity
            );
            assert_eq!(serde_json::to_vec(&server.lineage).unwrap(), before);
        }
    }

    #[test]
    fn published_source_retains_issued_recipe_and_exact_account_identity() {
        let source = chat::Session {
            id: crate::model::TerminalId::new(),
            launch: chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec!["--session-id".into(), "issued".into()],
                    working_directory: "/var/tmp".into(),
                    environment: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        "/account-a".into(),
                    )]),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        let continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
        let published = published_source(&source, Some("confirmed"), &continuity);
        assert_eq!(published.launch.recipe, source.launch.recipe);
        assert_eq!(published.launch.session_id.as_deref(), Some("confirmed"));
        let mut invented = published.clone();
        invented.launch.session_id = Some("invented".into());
        assert_ne!(published, invented);
        invented = published.clone();
        invented
            .launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/account-b".into());
        assert_ne!(published, invented);
        assert!(validate_pair(&source, None, None, None).is_ok());
        assert!(validate_pair(&source, Some(&source.launch.recipe), None, None).is_err());
    }
    #[test]
    fn attachment_metadata_and_observation_are_local_and_body_free() {
        let actor = RemoteActor::fixture();
        actor.set_visible(false);
        actor.set_visible(true);
        assert_eq!(actor.observe_next_update(), 1);
        assert_eq!(*actor.0.desired_visible.lock().unwrap(), Some(true));
        assert!(actor.0.desired_observe.load(Ordering::Acquire));
        let snapshot = actor.attachment_snapshot();
        assert_eq!(snapshot.generation, 1);
        assert!(snapshot.ready);
        assert!(snapshot.items.is_empty());
    }
    #[test]
    fn confirmed_install_updates_full_publication_once_and_is_idempotent() {
        let mut snapshot = Snapshot::default();
        assert_eq!(
            snapshot.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        confirm_installed_snapshot(&mut snapshot);
        assert!(matches!(
            snapshot.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
        ));
        let generation = snapshot.generation;
        confirm_installed_snapshot(&mut snapshot);
        assert_eq!(snapshot.generation, generation);
    }
    #[test]
    fn installed_actor_publications_preserve_valid_continuity_before_provider_id() {
        let fixture = resident_fixture();
        let server = &fixture.server;
        let resident = server.resident().unwrap();
        assert_eq!(
            resident.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
        );
        let mut snapshot = server.actor.snapshot(None).unwrap();
        confirm_installed_snapshot(&mut snapshot);
        assert_eq!(snapshot.native_continuity, resident.native_continuity);
        let source = published_source(&server.source, None, &resident.native_continuity);
        assert!(source.launch.session_id.is_none());
        assert!(source.has_valid_native_continuity());
        assert!(
            !source.is_unstarted(),
            "installed pre-ID helper must never become an editable unstarted draft"
        );
        let mut invalid = source.clone();
        invalid.launch.provider = crate::agent::Provider::Codex;
        assert!(!invalid.has_valid_native_continuity());
        invalid = source;
        invalid.native_continuity = chat::NativeContinuity::RetainedClaudeActor {
            protocol: PROTOCOL + 1,
        };
        assert!(!invalid.has_valid_native_continuity());
    }

    #[test]
    fn missing_install_commit_cannot_advertise_durable_continuity() {
        let actor = RemoteActor::fixture();
        actor.0.installed.store(false, Ordering::Release);
        {
            let mut state = actor.0.state.lock().unwrap();
            let state = Arc::make_mut(&mut state);
            state.installed = false;
            state.native_continuity = chat::NativeContinuity::ReconnectOnly;
        }
        assert!(actor.commit_installation().is_err());
        assert!(!actor.state().installed);
        assert_eq!(
            actor.state().native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
    }
    #[test]
    fn acknowledged_install_cannot_be_reversed_by_an_inflight_precommit_observation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("install.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut actor = RemoteActor::fixture();
        Arc::get_mut(&mut actor.0).unwrap().socket = socket;
        actor.0.installed.store(false, Ordering::Release);
        let before = {
            let mut state = actor.0.state.lock().unwrap();
            let state = Arc::make_mut(&mut state);
            state.installed = false;
            state.native_continuity = chat::NativeContinuity::ReconnectOnly;
            state.clone()
        };
        let proof = actor.0.proof.clone();
        let epoch = actor.0.epoch;
        let peer = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            let request: Invocation = receive(&control, TIMEOUT).unwrap();
            assert_eq!(request.method, "commit");
            assert_eq!(request.proof, proof);
            assert_eq!(request.epoch, Some(epoch));
            send(&control, &Ok::<Value, String>(Value::Null), TIMEOUT).unwrap();
        });
        actor.commit_installation().unwrap();
        peer.join().unwrap();
        // This is the exact precommit response an already-issued observer
        // RPC can return. Equal revisions cannot revoke a successful ACK.
        assert!(!actor.publish_resident(before.clone()));
        let installed = actor.state();
        assert!(installed.installed);
        assert_eq!(installed.changed, before.changed);
        assert_eq!(installed.generation, before.generation);
        assert_eq!(installed.confirmed_session, before.confirmed_session);
        assert_eq!(
            installed.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
        );
        let mut contradictory = installed.as_ref().clone();
        contradictory.changed += 1;
        contradictory.generation += 1;
        contradictory.native_continuity = chat::NativeContinuity::ReconnectOnly;
        assert!(!actor.publish_resident(contradictory));
        let mut next = installed.as_ref().clone();
        next.changed += 1;
        next.generation += 1;
        assert!(actor.publish_resident(next));
        assert_eq!(actor.state().generation, before.generation + 1);
        let final_snapshot = Snapshot {
            exited: true,
            ..Default::default()
        };
        actor.publish_closed_inner(final_snapshot.clone(), Some(before));
        assert!(actor.state().completed);
        assert!(actor.state().installed);
        assert_eq!(actor.state().native_continuity, installed.native_continuity);
        assert_eq!(actor.snapshot(None), Some(final_snapshot));
    }
    #[test]
    fn synchronous_refresh_publishes_all_transitions_and_rejects_a_delayed_observer_response() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("refresh.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let fixture = RemoteActor::fixture();
        let mut old = fixture.state().as_ref().clone();
        old.ready = false;
        old.busy = true;
        old.idle = false;
        old.activity = crate::agent::ActivityState::Working;
        let mut next = old.clone();
        next.changed += 1;
        next.generation += 1;
        next.prompt_revision = 1;
        next.ready = true;
        next.busy = false;
        next.idle = true;
        next.activity = crate::agent::ActivityState::Idle;
        next.confirmed_session = Some("confirmed-after-refresh".into());
        next.outbox_latest = 1;
        let usage = Update::AccountUsage {
            report: crate::account::LiveReport::Claude(crate::account::claude::LiveUpdate {
                windows: Vec::new(),
                status: None,
            }),
            observed_at: 123,
        };
        next.account_reports.push(usage.clone());
        let delayed = old.clone();
        let expected = vec![
            Update::Observed {
                generation: next.generation,
            },
            Update::Presentation,
            Update::Activity {
                state: next.activity,
                generation: next.generation,
            },
            Update::Ready,
            Update::PromptAvailable,
            Update::RetainedOutcomes,
            usage,
        ];
        let (observed_poll, poll_started) = crossbeam_channel::bounded(1);
        let (release, wait_release) = crossbeam_channel::bounded(1);
        let (processed, poll_processed) = crossbeam_channel::bounded(1);
        let (stop, stopped) = crossbeam_channel::bounded(1);
        let peer = thread::spawn(move || {
            // Hold the observer's response to force refresh_state to publish
            // the new resident before that observer returns an unchanged poll.
            let (observer, _) = listener.accept().unwrap();
            let request: Invocation = receive(&observer, TIMEOUT).unwrap();
            assert_eq!(request.method, "state");
            observed_poll.send(()).unwrap();
            let (refresh, _) = listener.accept().unwrap();
            let request: Invocation = receive(&refresh, TIMEOUT).unwrap();
            assert_eq!(request.method, "state");
            send(
                &refresh,
                &Ok::<Value, String>(serde_json::to_value(Some(next)).unwrap()),
                TIMEOUT,
            )
            .unwrap();
            wait_release.recv_timeout(TIMEOUT).unwrap();
            send(
                &observer,
                &Ok::<Value, String>(serde_json::to_value(Some(delayed)).unwrap()),
                TIMEOUT,
            )
            .unwrap();
            let (observer, _) = listener.accept().unwrap();
            let request: Invocation = receive(&observer, TIMEOUT).unwrap();
            assert_eq!(request.method, "state");
            send(&observer, &Ok::<Value, String>(Value::Null), TIMEOUT).unwrap();
            processed.send(()).unwrap();
            let _ = stopped.recv_timeout(TIMEOUT);
        });
        let (published, publications) = crossbeam_channel::unbounded();
        let actor = RemoteActor::from_attached_at(
            Attached {
                protocol: PROTOCOL,
                close_only: false,
                identity: fixture.identity(),
                epoch: uuid::Uuid::new_v4(),
                resident: old,
                source: fixture.0.source.clone(),
                launch_account_scope: Err("fixture".into()),
                execution_environment: Default::default(),
                recovery_environment: BTreeMap::new(),
            },
            fixture.0.proof.clone(),
            None,
            socket,
            move |update| {
                let _ = published.send(update);
            },
        )
        .unwrap();
        poll_started.recv_timeout(TIMEOUT).unwrap();
        actor.refresh_state().unwrap();
        for expected in expected {
            assert_eq!(publications.recv_timeout(TIMEOUT).unwrap(), expected);
        }
        release.send(()).unwrap();
        poll_processed.recv_timeout(TIMEOUT).unwrap();
        assert!(
            publications.try_recv().is_err(),
            "a delayed stale observer response must publish no transitions"
        );
        assert!(actor.state().ready);
        assert_eq!(
            actor.state().confirmed_session.as_deref(),
            Some("confirmed-after-refresh")
        );
        assert_eq!(actor.state().activity, crate::agent::ActivityState::Idle);
        actor.0.stop.store(true, Ordering::Release);
        stop.send(()).unwrap();
        peer.join().unwrap();
    }
    #[test]
    fn oversized_input_refusal_keeps_the_next_normal_input_admissible() {
        let actor = RemoteActor::fixture();
        // More than a complete wire frame, while still below the aggregate
        // pending-byte budget: this must fail before publishing any receipt.
        let result = actor.submit(
            None,
            Command::Prompt {
                text: "x".repeat(35 * 1024 * 1024),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            },
        );
        assert!(matches!(result, Err(Rejection::Refused(_))));
        assert!(actor.pending_inputs().is_empty());
        assert!(actor.0.uncertain_input.lock().unwrap().is_none());
        assert_eq!(*actor.0.input_sequence.lock().unwrap(), 1);
        // Missing fixture socket makes this normal input uncertain, but it is
        // admitted as the first exact envelope, proving the oversized refusal
        // did not permanently fence this actor.
        let reply = actor.enqueue(None, Command::Refresh).unwrap();
        assert_eq!(actor.pending_inputs()[0].sequence, 1);
        assert!(matches!(
            reply.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
    }
    #[test]
    fn lost_input_reply_keeps_the_original_envelope_and_fences_new_input() {
        let actor = RemoteActor::fixture();
        let response = actor.enqueue(None, Command::Interrupt).unwrap();
        let identity = actor.pending_inputs().pop().unwrap();
        assert_eq!(identity.sequence, 1);
        assert!(actor.enqueue(None, Command::Interrupt).is_err());
        assert_eq!(actor.pending_inputs(), vec![identity.clone()]);
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut journal = Journal::open(directory.path()).unwrap();
        let receipt = journal.admit(&identity).unwrap();
        assert_eq!(journal.admit(&identity).unwrap().identity, identity);
        actor.apply_receipts(&[receipt]);
        assert!(actor.0.uncertain_input.lock().unwrap().is_none());
        journal
            .finish(&identity, InputState::Acknowledged(Ok(())))
            .unwrap();
        actor.apply_receipts(&[journal.input(&identity).unwrap().unwrap()]);
        assert_eq!(response.try_recv(), Ok(Ok(())));
        assert!(actor.pending_inputs().is_empty());
        assert_eq!(*actor.0.input_sequence.lock().unwrap(), 2);
    }
    #[test]
    fn adopted_real_helper_reclaims_prior_acknowledgements_before_more_than_capacity_commands() {
        const CHILD: &str = "CAGENTS_ACTOR_RECEIPT_RETIREMENT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let test = thread::current()
                .name()
                .expect("test harness name")
                .to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("HOME", root.path().join("home"))
                .env("CAGENTS_LOCAL_DEBUG", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        std::fs::create_dir(root.path().join("claude")).unwrap();
        let mut source = fixture_source();
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.session_id = None;
        source.launch.recipe = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: root.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), root.path().to_string_lossy().into_owned()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    root.path().join("claude").to_string_lossy().into_owned(),
                ),
            ]),
            unset_environment: Default::default(),
        };
        source.owned_codex_listener = None;
        source.terminal = None;
        source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease =
            crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
        let script = r#"while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*) printf 'unexpected prompt\n' >> prompts ;;
  esac
done"#;
        let prepared = chat::protocol::create(&source.launch)
            .unwrap()
            .1
            .prepared_launch_identity()
            .unwrap();
        let mut spec = ProcessSpec::from(&source.launch.recipe);
        spec.arguments = vec!["-c".into(), script.into()];
        let actor =
            RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {}).unwrap();
        struct Owner(RemoteActor);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.close();
                let _ = self.0.finish_close();
            }
        }
        let owner = Owner(actor);
        let actor = &owner.0;
        actor.commit_installation().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !actor.state().ready {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        // Suspend only this test's metadata pump before admitting a command.
        // Preserve itself fences new helper input, so manufacturing a command
        // after preserve would test a forbidden ordering rather than a lost ACK.
        actor.0.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(6);
        while actor
            .0
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            assert!(Instant::now() < deadline, "old metadata pump did not stop");
            thread::sleep(Duration::from_millis(5));
        }
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: payload_digest(
                &serde_json::to_vec(
                    &serde_json::to_value(InputPayload::Command {
                        run: None,
                        command: Command::Refresh,
                        decline_only: false,
                    })
                    .unwrap(),
                )
                .unwrap(),
            ),
        };
        let payload = serde_json::to_value(InputPayload::Command {
            run: None,
            command: Command::Refresh,
            decline_only: false,
        })
        .unwrap();
        let _: InputReceipt = actor.call("input", json!([identity, payload])).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let receipt: Option<InputReceipt> =
                actor.call("input_receipt", json!([identity])).unwrap();
            if receipt.is_some_and(|receipt| matches!(receipt.state, InputState::Acknowledged(_))) {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let orphaned: Vec<InputReceipt> = actor.call("acknowledged_inputs", Value::Null).unwrap();
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].identity, identity);
        actor.preserve().unwrap();
        let record = actor.identity().record;
        drop(_lease);
        let mut proof = source.proof.clone().unwrap();
        proof.host_incarnation =
            crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string());
        let _replacement_lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        // The private v1 helper contract is incompatible with this actor
        // generation. Refusal cannot adopt, restart, or dispose the owner.
        let mut legacy_record = record.clone();
        legacy_record
            .build
            .as_mut()
            .unwrap()
            .capabilities
            .retain(|capability| capability != CAPABILITY);
        legacy_record
            .build
            .as_mut()
            .unwrap()
            .capabilities
            .push("claude-actor-v1".into());
        assert!(!RemoteActor::is_record(&legacy_record));
        assert!(RemoteActor::adopt(&legacy_record, source.clone(), proof.clone(), |_| {}).is_err());
        assert!(record.root.is_current());
        let mut legacy_source = source.clone();
        legacy_source.native_continuity =
            chat::NativeContinuity::RetainedClaudeActor { protocol: 1 };
        assert!(RemoteActor::adopt(&record, legacy_source, proof.clone(), |_| {}).is_err());
        assert!(
            record.root.is_current(),
            "mixed-version refusal preserves the exact provider"
        );
        // A new daemon cannot silently rebind the retained provider's account
        // after the same pathname resolves to a different physical directory.
        let original_home = root.path().join("claude");
        let retained_home = root.path().join("retained-claude");
        std::fs::rename(&original_home, &retained_home).unwrap();
        std::fs::create_dir(&original_home).unwrap();
        let rejection = RemoteActor::adopt(&record, source.clone(), proof.clone(), |_| {})
            .err()
            .expect("replaced physical account home must refuse adoption");
        assert!(rejection.contains("replaced"));
        assert!(
            record.root.is_current(),
            "refusal preserves the original provider"
        );
        std::fs::remove_dir(&original_home).unwrap();
        std::fs::rename(&retained_home, &original_home).unwrap();
        let replacement =
            RemoteActor::adopt(&record, source.clone(), proof.clone(), |_| {}).unwrap();
        let replacement_owner = Owner(replacement);
        let actor = &replacement_owner.0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while actor.0.recovering_inputs.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let reclaimed: Vec<InputReceipt> = actor.call("acknowledged_inputs", Value::Null).unwrap();
        assert!(reclaimed.is_empty());
        assert!(
            actor
                .call::<InputReceipt>("input", json!([identity, payload]))
                .is_err(),
            "a retired old-proxy command cannot execute again"
        );
        source.proof = Some(proof);
        source.native_continuity = actor.state().native_continuity.clone();
        actor
            .call::<()>(
                "update_source",
                json!([
                    source,
                    Option::<crate::model::LaunchRecipe>::None,
                    Option::<crate::terminal::holder::Record>::None
                ]),
            )
            .unwrap();
        let mut total = 0usize;
        while total < journal::MAX_INPUTS + 33 {
            let count = 32.min(journal::MAX_INPUTS + 33 - total);
            let mut replies = Vec::new();
            for _ in 0..count {
                replies.push(actor.enqueue(None, Command::Refresh).unwrap());
            }
            for reply in replies {
                reply
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
            }
            total += count;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !actor.0.retiring_inputs.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(actor.pending_inputs().is_empty());
        assert_eq!(*actor.0.input_sequence.lock().unwrap(), total as u64 + 2);
        assert!(
            !root.path().join("prompts").exists(),
            "receipt retirement must never enqueue a provider prompt"
        );
        actor.close().unwrap();
        actor.finish_close().unwrap();
    }

    #[test]
    fn submitted_input_preserves_refusal_and_uncertain_completion_types() {
        use super::super::super::handle::RetainedInputOutcome;
        for outcome in [
            RetainedInputOutcome::Acknowledged(Err("explicit refusal".into())),
            RetainedInputOutcome::Unknown("provider effect is unknown".into()),
            RetainedInputOutcome::Unavailable,
            RetainedInputOutcome::Disconnected,
        ] {
            let refused = matches!(&outcome, RetainedInputOutcome::Acknowledged(Err(_)));
            let actor = RemoteActor::fixture();
            let submitter = actor.clone();
            let delivery = thread::spawn(move || submitter.submit(None, Command::Refresh));
            assert_eq!(actor.settle_input_fixture(outcome), 1);
            let result = delivery.join().unwrap();
            assert!(if refused {
                matches!(result, Err(Rejection::Refused(_)))
            } else {
                matches!(result, Err(Rejection::Unacknowledged(_)))
            });
            assert_eq!(*actor.0.input_sequence.lock().unwrap(), 2);
        }
    }

    #[test]
    fn lost_retirement_reply_keeps_bounded_ack_metadata_and_never_resends_input() {
        let actor = RemoteActor::fixture();
        let response = actor.enqueue(None, Command::Refresh).unwrap();
        let identity = actor.pending_inputs().pop().unwrap();
        actor.apply_receipts(&[InputReceipt {
            identity: identity.clone(),
            state: InputState::Acknowledged(Ok(())),
        }]);
        assert_eq!(response.try_recv(), Ok(Ok(())));
        assert!(actor.pending_inputs().is_empty());
        assert_eq!(
            *actor.0.retiring_inputs.lock().unwrap(),
            vec![identity.clone()]
        );
        assert!(actor.flush_input_retirements().is_err());
        assert_eq!(
            *actor.0.retiring_inputs.lock().unwrap(),
            vec![identity.clone()]
        );
        assert!(
            actor.pending_inputs().is_empty(),
            "a failed metadata ACK never restores a provider envelope"
        );
        // Accepted and Unknown receipts are not retirement candidates.
        let response = actor.enqueue(None, Command::Refresh).unwrap();
        let other = actor.pending_inputs().pop().unwrap();
        actor.apply_receipts(&[InputReceipt {
            identity: other.clone(),
            state: InputState::Accepted,
        }]);
        assert!(response.try_recv().is_err());
        actor.apply_receipts(&[InputReceipt {
            identity: other,
            state: InputState::Unknown("uncertain".into()),
        }]);
        assert!(response.try_recv().unwrap().is_err());
        assert_eq!(*actor.0.retiring_inputs.lock().unwrap(), vec![identity]);
        assert_eq!(actor.pending_inputs().len(), 1);
    }

    #[test]
    fn unavailable_proxy_settles_pending_input_and_stops_reporting_healthy_metadata() {
        let actor = RemoteActor::fixture();
        let response = actor.enqueue(None, Command::Interrupt).unwrap();
        actor.publish_unavailable("Retained actor attachment was superseded");
        let state = actor.state();
        assert!(!state.ready);
        assert!(!state.idle);
        assert!(state.failure.is_some());
        assert_eq!(state.activity, crate::agent::ActivityState::Unknown);
        assert!(
            state.owns_work,
            "disconnection is not provider cleanup proof"
        );
        assert!(matches!(
            response.try_recv().unwrap(),
            Err(Rejection::Unacknowledged(error)) if error.contains("must not be regenerated")
        ));
    }
    #[test]
    fn connection_loss_requires_death_or_positive_authority_loss_evidence() {
        assert!(terminal_endpoint_error("connection reset", false));
        assert!(terminal_endpoint_error(
            "Retained actor attachment was superseded; stale input and ACKs are refused",
            true
        ));
        for detail in [
            "daemon lease does not match its discovery proof",
            "daemon ownership is not held",
        ] {
            let error = authority_result::<(), _>(Err(std::io::Error::other(detail))).unwrap_err();
            assert!(error.contains(detail));
            assert!(terminal_endpoint_error(&error, true));
        }
        assert!(!terminal_endpoint_error(
            "Resource temporarily unavailable (os error 11)",
            true
        ));
        assert!(!terminal_endpoint_error("timed out", true));
    }
}

#[cfg(test)]
#[path = "recognition_tests.rs"]
mod recognition_tests;

#[cfg(test)]
#[path = "identity_tests.rs"]
mod identity_tests;

#[cfg(test)]
#[path = "transport_deadline_tests.rs"]
mod deadline_tests;
