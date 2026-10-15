//! Exact retained Claude actor. Only bounded semantic publications cross this
//! socket; provider stdout and mutable reducer state never leave the helper.
mod journal;
pub(crate) mod restart;
pub(crate) use restart::{RestartCleanup, RestartFailure, RestartReceipt, RestartState};
pub(crate) mod final_snapshot;
mod deadline_io;
use super::{LocalHandle, Rejection, Update};
use crate::{
    chat::{
        self, Command, Snapshot,
        protocol::{ExecutionContext, ProcessSpec, TransportSpec},
    },
    daemon::holder_registry::{self as registry, Kind, Process, Record},
    model::HostSessionProof,
};
use journal::Journal;
pub(crate) use journal::{ContinuityLoss, InputIdentity, InputReceipt, InputState, OutboxEntry};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
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
const PROTOCOL: u32 = 1;
const CAPABILITY: &str = registry::RETAINED_CLAUDE_CAPABILITY;
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = 34 * 1024 * 1024;
fn payload_digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

#[derive(Serialize, Deserialize)]
enum InputPayload {
    Command {
        run: Option<crate::model::RunId>,
        command: Command,
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
    pub outcome: Option<crate::agent::ActivityOutcome>,
    pub usage: chat::Usage,
    pub native_continuity: chat::NativeContinuity,
    pub requests: Vec<chat::Request>,
    pub completed: bool,
    pub owns_work: bool,
    pub installed: bool,
    pub close: super::CloseObservation,
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
#[derive(Serialize, Deserialize)]
struct DirectoryEvidence {
    process: Process,
    identity: crate::agent_execution::DirectoryIdentity,
    path: PathBuf,
}
#[derive(Serialize, Deserialize)]
struct Startup {
    protocol: u32,
    source: chat::Session,
    program: String,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    working_directory: PathBuf,
    directory_fd: Option<RawFd>,
    directory_identity: Option<crate::agent_execution::DirectoryIdentity>,
    mailbox_key: Option<[u8; 32]>,
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
    identity: ActorIdentity,
    epoch: uuid::Uuid,
    resident: Resident,
    source: chat::Session,
    launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    recovery_environment: BTreeMap<String, String>,
}
#[derive(Clone)]
pub(super) struct RemoteActor(Arc<Remote>);
struct Pending {
    identity: InputIdentity,
    payload: Value,
    bytes: usize,
    reply: Option<crossbeam_channel::Sender<Result<(), String>>>,
}
struct Remote {
    identity: ActorIdentity,
    epoch: uuid::Uuid,
    proof: HostSessionProof,
    socket: PathBuf,
    state: Mutex<Arc<Resident>>,
    final_snapshot: Mutex<Option<Arc<Snapshot>>>,
    environment: Mutex<crate::terminal_ipc::ChatExecutionEnvironment>,
    launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    recovery_environment: BTreeMap<String, String>,
    desired_visible: Mutex<Option<bool>>,
    desired_observe: AtomicBool,
    input_sequence: Mutex<u64>,
    uncertain_input: Mutex<Option<InputIdentity>>,
    pending: Mutex<Vec<Pending>>,
    retiring_inputs: Mutex<Vec<InputIdentity>>,
    recovering_inputs: AtomicBool,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    stop: AtomicBool,
    installed: AtomicBool,
    preserved: AtomicBool,
    close_job: Mutex<Option<Arc<CloseJob>>>,
    session_stop_job: Mutex<Option<Arc<CloseJob>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    startup: Mutex<Option<UnixStream>>,
}
struct CloseJob {
    result: Mutex<Option<Result<(), String>>>,
    done: std::sync::Condvar,
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
impl RemoteActor {
    #[cfg(test)]
    pub(super) fn fixture() -> Self {
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
            outcome: None,
            usage: Default::default(),
            native_continuity: Default::default(),
            requests: Vec::new(),
            completed: false,
            owns_work: true,
            installed: true,
            close: super::CloseObservation {
                actor_id: uuid::Uuid::new_v4(),
                close_revision: 0,
                turn_generation: 0,
                activity: crate::agent::ActivityState::Idle,
                ready: true,
                exited: false,
                completed: false,
                closing: false,
                owns_work: true,
                working_directory: PathBuf::from("/var/tmp"),
                directory_identity: None,
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
            final_snapshot: Mutex::new(None),
            environment: Mutex::new(crate::terminal_ipc::ChatExecutionEnvironment(
                Default::default(),
            )),
            launch_account_scope: Err("fixture has no account scope".into()),
            recovery_environment: Default::default(),
            desired_visible: Mutex::new(None),
            desired_observe: AtomicBool::new(false),
            input_sequence: Mutex::new(1),
            uncertain_input: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            retiring_inputs: Mutex::new(Vec::new()),
            recovering_inputs: AtomicBool::new(false),
            notify: Arc::new(|_| {}),
            stop: AtomicBool::new(false),
            installed: AtomicBool::new(true),
            preserved: AtomicBool::new(false),
            close_job: Mutex::new(None),
            session_stop_job: Mutex::new(None),
            worker: Mutex::new(None),
            startup: Mutex::new(None),
        }))
    }
    #[cfg(test)]
    pub(super) fn completed_failure_fixture() -> Self {
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
    pub fn native_holder_record(&self)->&Record {&self.0.identity.record}
    pub fn identity(&self) -> ActorIdentity {
        self.0.identity.clone()
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
            self.call::<Option<Resident>>("state", json!([self.state().changed]))?
        {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.0.stop.load(Ordering::Acquire) {
                if !closed || !resident.close.completed {
                    return Err(
                        "Closed actor metadata changed provider ownership unexpectedly".into(),
                    );
                }
                resident.completed = true;
                resident.owns_work = false;
            }
            *state = Arc::new(resident);
        }
        Ok(())
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
            outcome: state.outcome.clone(),
            error: state.failure.clone(),
            exited: state.completed,
            ..Snapshot::default()
        }
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
        recovery_environment: BTreeMap<String, String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        std::hint::black_box(&SERVE);
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
            mailbox_key: spec.messages.as_ref().map(|seal| seal.export_private_key()),
            provider_directory_observed: observed,
            recovery_environment,
        };
        let (control, remote) = UnixStream::pair().map_err(|error| error.to_string())?;
        let fd = remote.as_raw_fd();
        let mut command = ChildCommand::new("/proc/self/exe");
        command
            .arg0("cagents-native")
            .arg(ARGUMENT)
            .arg(fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
        Self::inspect_until(record,proof,Instant::now()+TIMEOUT)
    }
    pub fn inspect_until(record:&Record,proof:HostSessionProof,deadline:Instant)->Result<ActorInspection,String> {
        let socket=registry::socket(&registry::directory(Kind::Native)?,record.terminal);
        Self::inspect_at_until(record,proof,&socket,deadline)
    }
    fn inspect_at_until(record:&Record,proof:HostSessionProof,socket:&std::path::Path,deadline:Instant)->Result<ActorInspection,String> {
        if Instant::now()>=deadline {return Err("Retained actor inspection deadline expired".into());}
        validate_record(record)?;
        let control=registry::connect_until(socket,deadline).map_err(|error|error.to_string())?;
        verify_peer(&control,record)?;
        deadline_io::send_until(&control,&Invocation {proof,epoch:None,method:"inspect".into(),args:Value::Null},deadline)?;
        let response:Result<ActorInspection,String>=deadline_io::receive_until(&control,deadline)?;
        let response=response?;
        if response.identity.record!=*record || response.source.id!=record.terminal {
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
        validate_record(record)?;
        let socket = registry::socket(&registry::directory(Kind::Native)?, record.terminal);
        let control = registry::connect(&socket).map_err(|error| error.to_string())?;
        verify_peer(&control, record)?;
        send(
            &control,
            &Invocation {
                proof: proof.clone(),
                epoch: None,
                method: "attach".into(),
                args: json!([source]),
            },
            TIMEOUT,
        )?;
        let attached: Result<Attached, String> = receive(&control, TIMEOUT)?;
        let attached = attached?;
        if &attached.identity.record != record {
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
        let recovering_inputs = attached.resident.acknowledged_input_count > 0;
        let remote = Self(Arc::new(Remote {
            identity: attached.identity,
            epoch: attached.epoch,
            proof,
            socket,
            desired_visible: Mutex::new(None),
            desired_observe: AtomicBool::new(false),
            input_sequence: Mutex::new(attached.resident.next_input),
            uncertain_input: Mutex::new(None),
            state: Mutex::new(Arc::new(attached.resident)),
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
            close_job: Mutex::new(None),
            session_stop_job: Mutex::new(None),
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
                        let changed = state.changed != old.changed;
                        let generation = state.generation;
                        let activity = state.activity;
                        let outcome = state.outcome.clone();
                        let activity_changed = activity != old.activity || outcome != old.outcome;
                        let ready = state.ready && !old.ready;
                        let identity_changed = state.confirmed_session != old.confirmed_session;
                        let outcomes = (state.outbox_latest > state.outbox_ack
                            && state.outbox_latest != old.outbox_latest)
                            || (state.continuity_loss != old.continuity_loss
                                && state
                                    .continuity_loss
                                    .as_ref()
                                    .is_some_and(|loss| !loss.acknowledged));
                        let usage: Vec<_> = state
                            .account_reports
                            .iter()
                            .filter(|report| !old.account_reports.contains(report))
                            .cloned()
                            .collect();
                        if !actor.publish_resident(state) {
                            break;
                        }
                        if changed {
                            (actor.0.notify)(Update::Observed { generation });
                            (actor.0.notify)(Update::Presentation);
                        }
                        if activity_changed {
                            (actor.0.notify)(Update::Activity {
                                state: activity,
                                generation,
                                outcome,
                            });
                        }
                        if identity_changed {
                            (actor.0.notify)(Update::Identity);
                        }
                        if ready {
                            (actor.0.notify)(Update::Ready);
                        }
                        if outcomes {
                            (actor.0.notify)(Update::RetainedOutcomes);
                        }
                        for report in usage {
                            (actor.0.notify)(report);
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
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if self.0.stop.load(Ordering::Acquire) {
            return false;
        }
        *state = Arc::new(resident);
        true
    }
    fn publish_closed(&self, snapshot: Snapshot) {
        *self
            .0
            .final_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(snapshot));
        self.0.stop.store(true, Ordering::Release);
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
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
        let dead = !self.0.identity.record.holder.is_current();
        let reason = if dead {
            "The retained Claude actor exited; exact provider cleanup and unfinished input outcomes are unconfirmed".to_owned()
        } else {
            format!("This retained actor attachment is no longer authoritative: {error}")
        };
        let generation;
        let outcome;
        {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            if self.0.stop.load(Ordering::Acquire) {
                return;
            }
            let state = Arc::make_mut(&mut state);
            if dead && state.working {
                state.outcome = Some(crate::agent::ActivityOutcome::Interrupted {
                    reason: reason.clone(),
                });
            }
            state.failure = Some(reason.clone());
            state.ready = false;
            state.idle = false;
            state.busy = false;
            state.waiting = false;
            state.working = false;
            state.activity = crate::agent::ActivityState::Unknown;
            state.owns_work = true;
            generation = state.generation;
            outcome = state.outcome.clone();
        }
        for pending in &mut *self.0.pending.lock().unwrap_or_else(|p| p.into_inner()) {
            if let Some(reply) = pending.reply.take() {
                let _ = reply.send(Err(format!(
                    "{reason}; input {} sequence {} remains unknown and must not be regenerated",
                    pending.identity.id, pending.identity.sequence
                )));
            }
        }
        (self.0.notify)(Update::Activity {
            state: crate::agent::ActivityState::Unknown,
            generation,
            outcome,
        });
        (self.0.notify)(Update::Observed { generation });
        (self.0.notify)(Update::Presentation);
        self.0.stop.store(true, Ordering::Release);
    }
    pub fn call<T: DeserializeOwned>(&self, method: &str, args: Value) -> Result<T, String> {
        let control = registry::connect(&self.0.socket).map_err(|error| error.to_string())?;
        verify_peer(&control, &self.0.identity.record)?;
        send(
            &control,
            &Invocation {
                proof: self.0.proof.clone(),
                epoch: Some(self.0.epoch),
                method: method.into(),
                args,
            },
            TIMEOUT,
        )?;
        let response: Result<Value, String> = receive(&control, TIMEOUT)?;
        serde_json::from_value(response?).map_err(|error| error.to_string())
    }
    pub fn enqueue(
        &self,
        run: Option<crate::model::RunId>,
        command: Command,
    ) -> Result<crossbeam_channel::Receiver<Result<(), String>>, String> {
        self.enqueue_payload(InputPayload::Command { run, command })
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
    ) -> Result<crossbeam_channel::Receiver<Result<(), String>>, String> {
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
                        let _ = reply.send(result.clone());
                    }
                    completed.push(pending.identity.clone());
                    false
                }
                InputState::Unknown(error) => {
                    if let Some(reply) = pending.reply.take() {
                        let _ = reply.send(Err(error.clone()));
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
        let receiver = self
            .enqueue(run, command)
            .map_err(Rejection::Unacknowledged)?;
        receiver.recv_timeout(Duration::from_secs(10)).map_err(|_|Rejection::Unacknowledged("Retained actor input acknowledgement is pending; use its stable receipt, never resend blindly".into()))?.map_err(Rejection::Refused)
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
    pub fn pinned_directory(
        &self,
        candidate: &std::path::Path,
    ) -> Result<Arc<ExecutionContext>, String> {
        let identity: crate::agent_execution::DirectoryIdentity =
            self.call("pinned_directory", Value::Null)?;
        let context = Arc::new(ExecutionContext::capture(candidate)?);
        if context.directory_identity()? != Some(identity) {
            return Err("The retained actor's original directory was replaced; approval cannot transfer to this folder".into());
        }
        Ok(context)
    }
    pub fn capture_directory(
        &self,
        candidate: &std::path::Path,
    ) -> Result<super::DirectoryProof, String> {
        let proof: DirectoryEvidence = self.call("capture_directory", json!([candidate]))?;
        if Some(&proof.process) != self.state().provider_process.as_ref() || proof.path != candidate
        {
            return Err("Retained provider directory evidence changed process or candidate".into());
        }
        super::DirectoryProof::capture_owned_process(
            &proof.process,
            candidate,
            Some(proof.identity),
        )
    }
    pub fn relocate_directory(&self, candidate: &std::path::Path) -> Result<(), String> {
        self.call("relocate_directory", json!([candidate]))
    }
    pub fn commit_installation(&self) -> Result<(), String> {
        self.call::<()>("commit", Value::Null)?;
        self.0.installed.store(true, Ordering::Release);
        {
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
    /// Keep one exact fence request owned while its caller's budget expires.
    /// A lost response never causes cleanup or a replacement actor close.
    pub fn fence_session_stop_until(
        &self,
        actor_id: uuid::Uuid,
        deadline: Instant,
    ) -> Result<(), String> {
        if actor_id != self.0.identity.instance {
            return Err("The provider actor was replaced; its replacement was preserved".into());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Session stop budget expired; its exact owner is retained")?;
        let job = {
            let mut pending = match self.0.session_stop_job.try_lock() {
                Ok(pending) => pending,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err(
                        "Session stop admission is already pending; retain the exact owner".into(),
                    );
                }
            };
            if let Some(job) = pending
                .as_ref()
                .filter(|job| {
                    job.result
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_ref()
                        .is_none_or(Result::is_ok)
                })
                .cloned()
            {
                job
            } else {
                let job = Arc::new(CloseJob {
                    result: Mutex::new(None),
                    done: std::sync::Condvar::new(),
                });
                let completion = job.clone();
                let actor = self.clone();
                let millis = u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .min(32_000);
                thread::Builder::new()
                    .name("cagents-session-stop".into())
                    .spawn(move || {
                        let result =
                            actor.call::<()>("fence_session_stop", json!([actor_id, millis]));
                        *completion.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                        completion.done.notify_all();
                    })
                    .map_err(|error| error.to_string())?;
                *pending = Some(job.clone());
                job
            }
        };
        let mut result = job.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = result.as_ref() {
                return result.clone();
            }
            let remaining=deadline.checked_duration_since(Instant::now()).ok_or("Session stop acknowledgement is pending; the exact helper and request remain retained")?;
            result = job
                .done
                .wait_timeout(result, remaining)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
    pub fn close_until(&self, deadline: Instant) -> Result<(), String> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Retained actor close budget expired; its exact owner is retained")?;
        let job = {
            let mut pending = match self.0.close_job.try_lock() {
                Ok(pending) => pending,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return Err(
                    "Another retained close is being admitted; its exact owner remains retained"
                        .into(),
                ),
            };
            let existing = pending
                .as_ref()
                .filter(|job| {
                    job.result
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_ref()
                        .is_none_or(Result::is_ok)
                })
                .cloned();
            if let Some(job) = existing {
                job
            } else {
                let job = Arc::new(CloseJob {
                    result: Mutex::new(None),
                    done: std::sync::Condvar::new(),
                });
                let completion = job.clone();
                let actor = self.clone();
                let millis = u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .min(32_000);
                thread::Builder::new()
                    .name("cagents-actor-close".into())
                    .spawn(move || {
                        let result = actor.close_method("close_until", json!([millis]));
                        *completion.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                        completion.done.notify_all();
                    })
                    .map_err(|error| error.to_string())?;
                *pending = Some(job.clone());
                job
            }
        };
        let mut result = job.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = result.as_ref() {
                return result.clone();
            }
            let remaining=deadline.checked_duration_since(Instant::now()).ok_or("Retained actor close is still pending; its exact helper and request owner remain retained")?;
            result = job
                .done
                .wait_timeout(result, remaining)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
    pub fn close_method(&self, method: &str, args: Value) -> Result<(), String> {
        let snapshot = match self.call::<ClosePublication>(method, args) {
            Ok(publication) => {
                // Keep the final semantic cursors visible to the broker even
                // though this proxy's provider is now closed.
                {
                    let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                    *state = Arc::new(publication.resident);
                }
                publication.snapshot
            }
            Err(error) => {
                if !chat::native_holder::actor_close_proved(&self.0.identity.record, &self.0.proof)?
                {
                    return Err(error);
                }
                final_snapshot::read(&self.0.identity,&self.0.proof).map_err(|detail|format!("Provider cleanup is proved, but the exact final publication is unavailable: {detail}"))?
            }
        };
        self.publish_closed(snapshot);
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
    mailbox_seal: Option<Arc<crate::agent_execution::mailbox::Seal>>,
    closed_snapshot: Option<Arc<Snapshot>>,
    session_stopping: bool,
    // A successful ownership handoff fences authorizing commands from the old
    // attachment. A newly authenticated attach may own the still-live actor.
    attachment_preserved: bool,
}
impl Server {
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
        let mut spec_and_adapter = chat::protocol::create(&operational)?;
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
            .mailbox_key
            .map(|key| Arc::new(crate::agent_execution::mailbox::Seal::new(key)));
        let environment = crate::terminal_ipc::ChatExecutionEnvironment(spec.environment.clone());
        spec_and_adapter
            .1
            .relocate_directory(&spec.working_directory)?;
        let initial = spec_and_adapter.1.initialize()?;
        let mut initial = initial;
        let store = super::preflight(spec, &mut initial)?;
        let journal = Arc::new(Mutex::new(Journal::open(store.directory())?));
        let changed = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let published = changed.clone();
        let outcomes = journal.clone();
        let account_reports = Arc::new(Mutex::new(Vec::<Update>::new()));
        let reported = account_reports.clone();
        if disconnected(control)? {
            return Err("Retained actor launch cancelled before process creation".into());
        }
        let launch_account_scope =
            super::capture_account_scope(&spec_and_adapter.0, crate::agent::Provider::Claude);
        let mailbox_seal = spec_and_adapter.0.messages.clone();
        let actor = LocalHandle::start_inner(
            spec_and_adapter.0,
            spec_and_adapter.1,
            initial,
            store,
            None,
            startup.source.launch.session_id.clone(),
            None,
            false,
            launch_account_scope,
            move |update| {
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
            instance: actor.close_observation().actor_id,
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
            pending: Vec::new(),
            account_reports,
            environment,
            recovery_environment: startup.recovery_environment,
            listener,
            socket,
            socket_identity,
            _directory: directory,
            mailbox_seal,
            closed_snapshot: None,
            session_stopping: false,
            attachment_preserved: false,
        })
    }
    fn notifier(&self) -> impl Fn(Update) + Send + Sync + 'static {
        let outcomes = self.journal.clone();
        let changed = self.changed.clone();
        let reports = self.account_reports.clone();
        move |update| {
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
        self.changed
            .load(Ordering::Acquire)
            .wrapping_add(self.actor.close_observation().close_revision)
    }
    fn resident(&self) -> Result<Resident, String> {
        self.resident_after_copy(|| {})
    }
    fn resident_after_copy(&self, after_copy: impl FnOnce()) -> Result<Resident, String> {
        // Sample before copying any metadata. A concurrent actor publication
        // may make this conservative token old, which forces the next poll;
        // stamping the newest token after copying could hide Ready forever.
        let changed = self.revision();
        // Clone resident metadata only. Do not hydrate the Store or serialize
        // any growing parent/child tool body to service periodic observation.
        let state = self.actor.0.state.lock().unwrap_or_else(|p| p.into_inner());
        let generation = state.generation;
        let session_id = state.session_id.clone();
        let title = state.title.clone();
        let ready = state.ready;
        let busy = state.busy;
        let waiting = state.waiting;
        let outcome = state.outcome.clone();
        let usage = state.usage.clone();
        let requests = state.requests.clone();
        drop(state);
        after_copy();
        let close = self.actor.close_observation();
        let activity = close.activity;
        let working = activity == crate::agent::ActivityState::Working;
        let busy = busy || working;
        let ready = ready && close.ready;
        let idle = activity == crate::agent::ActivityState::Idle && ready && !close.closing;
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
        let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        let continuity_loss = journal.continuity_loss()?;
        let failure = continuity_loss
            .as_ref()
            .map(|loss| loss.reason.clone())
            .or(actor_failure);
        let outcome = continuity_loss
            .as_ref()
            .map(|loss| crate::agent::ActivityOutcome::Interrupted {
                reason: loss.reason.clone(),
            })
            .or(outcome);
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
            outcome,
            usage,
            requests,
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
        for source in self.lineage.iter().chain(std::iter::once(&self.source)) {
            let projected = published_source(source, confirmed.as_deref(), &continuity);
            if projected != self.source && !sources.contains(&projected) {
                sources.push(projected);
            }
        }
        sources
    }
    fn attached(&self) -> Result<Attached, String> {
        Ok(Attached {
            protocol: PROTOCOL,
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
    fn invoke(&mut self, request: Invocation) -> Result<Value, String> {
        if request.method == "inspect" {
            return serde_json::to_value(ActorInspection {
                identity: self.identity.clone(),
                source: self.source.clone(),
                previous_sources: self.published_sources(),
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
        if request.method == "attach" {
            let (source,): (chat::Session,) =
                serde_json::from_value(request.args).map_err(|error| error.to_string())?;
            if source != self.source && !self.published_sources().contains(&source) {
                return Err(
                    "Retained actor source/account identity does not match this chat".into(),
                );
            }
            self.epoch = uuid::Uuid::new_v4();
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
        if self.attachment_preserved && matches!(request.method.as_str(),
            "restart" | "fence_session_stop" | "close" | "close_until" |
            "close_if_revision" | "close_if_idle" | "relocate_directory"
        ) {
            return Err("This attachment preserved provider ownership; authenticate a new attachment before changing it".into());
        }
        match request.method.as_str() {
            "execution_environment" => value!(self.environment.clone()),
            "restart" => {
                if self.session_stopping {
                    return Err(
                        "This actor lifetime is stopping; provider replacement is refused".into(),
                    );
                }
                let request = decode!(restart::Request);
                value!(self.restart(request)?)
            }
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
                    InputPayload::Command { run, command } => {
                        let state = Arc::new(std::sync::atomic::AtomicU8::new(0));
                        (
                            super::ActorCommand::RetainedProvider {
                                run,
                                command,
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
                        super::ActorCommand::Interrupt {
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
                    snapshot.outcome = Some(crate::agent::ActivityOutcome::Interrupted {
                        reason: loss.reason,
                    });
                }
                value!(snapshot)
            }
            "presentation" => {
                let (id, after) = decode!((crate::model::TerminalId, Option<u64>));
                if id != self.source.id {
                    return Err("Retained presentation names another chat".into());
                }
                value!(self.actor.presentation(id, after))
            }
            "file_query" => {
                let (command,) = decode!((Command,));
                value!(self.actor.file_query(&command)?)
            }
            "exit_output" => {
                let (id, offset) = decode!((String, u64));
                value!(self.actor.provider_exit_output(&id, offset)?)
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
            "pinned_directory" => value!(self.actor.pinned_directory_identity()?),
            "capture_directory" => {
                let (candidate,) = decode!((PathBuf,));
                let proof = self.actor.capture_execution_directory(&candidate)?;
                value!(DirectoryEvidence {
                    process: proof.process,
                    identity: proof.identity,
                    path: proof.path
                })
            }
            "relocate_directory" => {
                let (candidate,) = decode!((PathBuf,));
                self.actor.relocate_execution_directory(&candidate)?;
                self.changed.fetch_add(1, Ordering::Release);
                Ok(Value::Null)
            }
            "readiness" => value!(self.actor.check_update_readiness()?),
            "commit" => {
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
                let retained = serde_json::to_vec(&lineage)
                    .map_err(|error| error.to_string())?
                    .len();
                if lineage.len() > 1024 || retained > 4 * 1024 * 1024 {
                    return Err("Retained source lineage capacity reached".into());
                }
                self.lineage = lineage;
                self.source = source;
                self.execution_console = console;
                self.paired_holder = pair;
                Ok(Value::Null)
            }
            "fence_session_stop" => {
                let (actor_id, millis) = decode!((uuid::Uuid, u64));
                if actor_id != self.identity.instance {
                    return Err(
                        "The provider actor was replaced; its replacement was preserved".into(),
                    );
                }
                // Helper ownership survives daemon/proxy replacement. Retain
                // explicit actor-lifetime intent even if its ACK is lost, and
                // never allow a provider restart to escape that consent fence.
                self.session_stopping = true;
                self.changed.fetch_add(1, Ordering::Release);
                self.actor.fence_session_stop_until(
                    actor_id,
                    Instant::now() + Duration::from_millis(millis.min(32_000)),
                )?;
                Ok(Value::Null)
            }
            "close_until" => {
                let (millis,) = decode!((u64,));
                if self.closed_snapshot.is_none() {
                    self.actor
                        .close_until(Instant::now() + Duration::from_millis(millis.min(32_000)))?;
                    reap_orphans()?;
                    self.finish_inputs();
                }
                value!(self.publish_close()?)
            }
            "close_if_revision" => {
                let (revision,) = decode!((u64,));
                if self.closed_snapshot.is_none() {
                    self.actor.close_if_revision(revision)?;
                    reap_orphans()?;
                    self.finish_inputs();
                }
                value!(self.publish_close()?)
            }
            "close_if_idle" => {
                let (generation,) = decode!((u64,));
                if self.closed_snapshot.is_none() {
                    self.actor.close_if_idle(generation)?;
                    reap_orphans()?;
                    self.finish_inputs();
                }
                value!(self.publish_close()?)
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
                final_snapshot::publish_retirement(&self.identity, &self.proof)?;
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
        if self.installed
            && (journal.latest()? != journal.acknowledged()?
                || journal
                    .continuity_loss()?
                    .as_ref()
                    .is_some_and(|loss| !loss.acknowledged))
        {
            return Err(
                "Final retained outcomes must be durably acknowledged before helper retirement"
                    .into(),
            );
        }
        Ok(())
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
                snapshot.outcome = Some(crate::agent::ActivityOutcome::Interrupted {
                    reason: loss.reason,
                });
            }
            snapshot
        };
        // A closing actor may be adopted by a new authenticated daemon before
        // its last semantic ACK. Refresh receipt authority without changing
        // the exact immutable final publication.
        final_snapshot::save(&self.identity, &self.proof, &snapshot)?;
        chat::native_holder::publish_actor_close(
            &self.identity.record,
            &self.proof,
            &self.source,
            &self.published_sources(),
            self.paired_holder.as_ref(),
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
                    let mut finished = false;
                    let response = (|| {
                        let request: Invocation = receive(&control, TIMEOUT)?;
                        authorize(&control, &request.proof)?;
                        let close = request.method == "retire";
                        let result = self.invoke(request);
                        finished = close && result.is_ok();
                        result
                    })();
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
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME {
        return Err("Retained actor frame exceeds the bounded protocol capacity".into());
    }
    control
        .set_write_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    let mut writer = control;
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|()| writer.write_all(&bytes))
        .map_err(|error| error.to_string())
}
fn receive<T: DeserializeOwned>(control: &UnixStream, timeout: Duration) -> Result<T, String> {
    control
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    let mut reader = control;
    let mut size = [0; 4];
    reader
        .read_exact(&mut size)
        .map_err(|error| error.to_string())?;
    let size = u32::from_le_bytes(size) as usize;
    if size > MAX_FRAME {
        return Err("Retained actor frame exceeds the bounded protocol capacity".into());
    }
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
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
#[used]
#[unsafe(link_section = ".init_array")]
static SERVE: extern "C" fn() = serve_if_requested;
extern "C" fn serve_if_requested() {
    let Ok(arguments) = std::fs::read("/proc/self/cmdline") else {
        return;
    };
    let mut arguments = arguments.split(|byte| *byte == 0).skip(1);
    if arguments.next() != Some(ARGUMENT.as_bytes()) {
        return;
    }
    let fd = arguments
        .next()
        .and_then(|fd| std::str::from_utf8(fd).ok()?.parse::<RawFd>().ok())
        .filter(|fd| *fd > 2);
    let Some(fd) = fd else {
        std::process::exit(2);
    };
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        libc::prctl(libc::PR_SET_NAME, c"cagents-native".as_ptr());
    }
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        std::process::exit(2);
    }
    let control = unsafe { UnixStream::from_raw_fd(fd) };
    let result = (|| {
        // Startup control and the pinned directory survived helper exec;
        // seal them before spawning any provider or provider descendants.
        crate::daemon::seal_inherited_descriptors().map_err(|error|error.to_string())?;
        let startup: Startup = receive(&control, Duration::from_secs(30))?;
        authorize(
            &control,
            startup
                .source
                .proof
                .as_ref()
                .ok_or("Retained startup lacks proof")?,
        )?;
        let _ = crate::daemon::process_scope::enroll_holder();
        let server = Server::start(startup, &control)?;
        server.serve(control.try_clone().map_err(|error| error.to_string())?)
    })();
    if let Err(error) = result {
        let _ = send(&control, &Err::<Attached, _>(error), TIMEOUT);
    }
    while reap_orphans().is_err() {
        thread::sleep(Duration::from_millis(100));
    }
    std::process::exit(73);
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
        let store = super::super::preflight(&spec, &mut initial).unwrap();
        let actor = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !actor.0.state.lock().unwrap().ready {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        identity.instance = actor.close_observation().actor_id;
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
                pending: Vec::new(),
                account_reports: Arc::new(Mutex::new(Vec::new())),
                environment: crate::terminal_ipc::ChatExecutionEnvironment(Default::default()),
                recovery_environment: BTreeMap::new(),
                listener,
                socket,
                socket_identity: (metadata.dev(), metadata.ino()),
                _directory: None,
                mailbox_seal: None,
                closed_snapshot: None,
                session_stopping: false,
                attachment_preserved: false,
            },
            _root: root,
        }
    }
    #[test]
    fn helper_restores_descriptor_seal_before_any_provider_exec() {
        const CHILD:&str="CAGENTS_ACTOR_DESCRIPTOR_SEAL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let isolated=tempfile::tempdir_in("/var/tmp").unwrap();
            let test=thread::current().name().unwrap().to_owned();
            let output=ChildCommand::new(std::env::current_exe().unwrap()).args(["--exact",&test,"--nocapture"])
                .env(CHILD,"1").env("XDG_RUNTIME_DIR",isolated.path()).env("XDG_DATA_HOME",isolated.path().join("data"))
                .env("XDG_CONFIG_HOME",isolated.path().join("config")).env("XDG_CACHE_HOME",isolated.path().join("cache"))
                .env("HOME",isolated.path().join("home")).env("CAGENTS_LOCAL_DEBUG","0").output().unwrap();
            assert!(output.status.success(),"stdout={} stderr={}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
        }
        let root=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut source=chat::native_holder::fixture_source();
        source.launch.provider=crate::agent::Provider::Claude;source.launch.session_id=None;
        source.launch.recipe=crate::model::LaunchRecipe {program:"/bin/sleep".into(),arguments:vec!["600".into()],working_directory:root.path().into(),
            environment:BTreeMap::from([("HOME".into(),root.path().to_string_lossy().into_owned()),("CLAUDE_CONFIG_DIR".into(),root.path().join("claude").to_string_lossy().into_owned())]),unset_environment:Default::default()};
        source.owned_codex_listener=None;source.terminal=None;source.native_continuity=chat::NativeContinuity::ReconnectOnly;
        source.proof.as_mut().unwrap().boot_id=crate::daemon::read_boot_id().unwrap();
        let paths=crate::daemon::RuntimePaths::discover().unwrap();
        let _lease=crate::daemon::HostLease::acquire(&paths,source.proof.as_ref().unwrap()).unwrap();
        let context=Arc::new(ExecutionContext::capture(root.path()).unwrap());
        let mut spec=ProcessSpec::from(&source.launch.recipe);spec.execution=Some(context.clone());
        // This execs the real retained-helper entry and its real provider
        // spawning path. Omitting the entry seal leaks both inherited FDs.
        let actor=RemoteActor::spawn(spec,source,BTreeMap::new(),|_|{}).unwrap();
        struct Owner(RemoteActor);
        impl Drop for Owner {fn drop(&mut self) {let _=self.0.close();let _=self.0.finish_close();}}
        let owner=Owner(actor);let actor=&owner.0;actor.commit_installation().unwrap();
        let provider=actor.state().provider_process.clone().unwrap();
        assert!(provider.is_current());
        let inherited=std::fs::read_dir(format!("/proc/{}/fd",provider.pid)).unwrap().filter_map(|entry| {
            let path=entry.unwrap().path();match std::fs::read_link(path) {
                Ok(target)=>Some(target),
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>None,
                Err(error)=>panic!("Cannot inspect exact fixture provider FD: {error}"),
            }
        }).collect::<Vec<_>>();
        actor.close().unwrap();actor.finish_close().unwrap();
        assert!(!provider.is_current());
        assert!(!inherited.iter().any(|target|target.to_string_lossy().starts_with("socket:[")),"provider exec must not inherit the private helper startup control socket");
        assert!(!inherited.iter().any(|target|target==root.path()),"provider exec must not retain the helper's approved-directory FD");
        assert!(context.directory_descriptor().unwrap().metadata().unwrap().is_dir(),"the caller's approved pin remains usable");
    }


    #[test]
    fn explicit_restart_reuses_the_store_and_never_repeats_a_deduplicated_launch() {
        const CHILD: &str = "CAGENTS_RETAINED_RESTART_SUBREAPER_TEST";
        if std::env::var_os(CHILD).is_none() {
            let test = thread::current()
                .name()
                .expect("test harness name")
                .to_owned();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // Orphan reaping is process-wide. Only this isolated test process may
        // become a subreaper; never reap children belonging to parallel tests.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        let old_process = server.actor.provider_process().unwrap();
        let old_close = server.actor.close_observation();
        let store = server.actor.0.store.clone();
        store
            .lock()
            .unwrap()
            .apply(chat::Event::Item(chat::Item {
                id: "partial".into(),
                role: chat::Role::Assistant,
                started_at: None,
                at: None,
                content: chat::ItemContent::Text {
                    text: "answer before crash".into(),
                    thinking: false,
                    phase: Default::default(),
                },
            }))
            .unwrap();
        server.actor.close().unwrap();
        let id = uuid::Uuid::new_v4();
        let script = r#"printf 'spawn\n' >> spawns
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*) printf 'unexpected prompt\n' >> prompts ;;
  esac
done"#;
        let payload = json!({"id":id,"source":server.source,"program":"/bin/sh","arguments":["-c",script],"environment":{},"working_directory":fixture._root.path(),"recovery_environment":{}});
        let request: restart::Request = serde_json::from_value(payload.clone()).unwrap();
        let receipt = server.restart(request).unwrap();
        let restart::RestartState::Acknowledged(Ok(process)) = receipt.state else {
            panic!("replacement did not acknowledge its exact process");
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server.actor.0.state.lock().unwrap().ready {
            assert!(
                Instant::now() < deadline,
                "restarted fixture did not acknowledge initialization"
            );
            thread::yield_now();
        }
        assert_ne!(process, old_process);
        assert_eq!(server.identity.instance, old_close.actor_id);
        assert!(Arc::ptr_eq(&store, &server.actor.0.store));
        let window = store.lock().unwrap().window().unwrap();
        assert!(window.items.iter().any(|item|matches!(&item.content,chat::ItemContent::Text{text,..} if text=="answer before crash")));
        assert!(server.actor.close_observation().close_revision > old_close.close_revision);
        let repeated = server
            .restart(serde_json::from_value(payload.clone()).unwrap())
            .unwrap();
        assert_eq!(
            repeated.state,
            restart::RestartState::Acknowledged(Ok(process))
        );
        assert_eq!(
            std::fs::read_to_string(fixture._root.path().join("spawns")).unwrap(),
            "spawn\n"
        );
        assert!(
            !fixture._root.path().join("prompts").exists(),
            "restart never replays an accepted prompt"
        );
        let mut changed = payload;
        changed["arguments"] = json!(["-c", "exit 0"]);
        assert!(
            server
                .restart(serde_json::from_value(changed).unwrap())
                .is_err(),
            "same restart UUID cannot select a different launch"
        );
        let close = server.actor.close_observation();
        let expected = crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(fixture._root.path()).unwrap(),
        );
        assert_eq!(
            close.directory_identity,
            Some(expected),
            "prepared restart retains physical close scope"
        );
        assert_eq!(close.working_directory, fixture._root.path());
        assert!(
            server
                .actor
                .close_if_revision(close.close_revision)
                .unwrap_err()
                .contains("work is unchanged")
        );
        assert!(!server.actor.close_observation().closing);
        server
            .actor
            .close_until(Instant::now() + Duration::from_secs(5))
            .unwrap();
    }
    #[test]
    fn stopped_provider_keeps_its_semantic_journal_until_outcome_and_loss_ack() {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
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
    fn final_proxy_drop_never_joins_finished_packet_with_blocked_tls_cleanup() {
        let actor = RemoteActor::fixture();
        let (worker, release, finished) = super::super::tests::worker_blocked_in_tls_destruction();
        *actor.0.worker.lock().unwrap() = Some(worker);
        let started = Instant::now();
        drop(actor);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            finished.try_recv().is_err(),
            "frontend detach is not OS-thread or retained-provider cleanup proof"
        );
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn read_only_inspection_of_a_stalled_exact_peer_obeys_one_budget_without_actor_changes() {
        let fixture=resident_fixture();
        let mut record=fixture.server.identity.record.clone();
        record.boot=crate::daemon::read_boot_id().unwrap();
        record.build.as_mut().unwrap().capabilities.push(CAPABILITY.into());
        let socket=fixture._root.path().join("deadline-inspect.sock");
        let listener=UnixListener::bind(&socket).unwrap();
        let (release,wait)=crossbeam_channel::bounded(1);
        let peer=thread::spawn(move|| {
            let (control,_)=listener.accept().unwrap();
            let request:Invocation=receive(&control,TIMEOUT).unwrap();
            assert_eq!(request.method,"inspect");assert!(request.epoch.is_none());
            // SO_PEERCRED has identified this exact Record.holder process, but
            // its inspection response stalls. The client must still return.
            let _=wait.recv_timeout(Duration::from_secs(2));
        });
        let epoch=fixture.server.epoch;let source=fixture.server.source.clone();
        let started=Instant::now();
        let error=RemoteActor::inspect_at_until(&record,fixture.server.proof.clone(),&socket,started+Duration::from_millis(40)).err().expect("stalled inspection must time out");
        let elapsed=started.elapsed();let _=release.send(());peer.join().unwrap();
        assert!(error.contains("deadline"));assert!(elapsed<Duration::from_millis(250));
        assert_eq!(fixture.server.epoch,epoch);assert_eq!(fixture.server.source,source);
        assert!(!fixture.server.actor.0.stop.load(Ordering::Acquire));
        assert!(fixture.server.pending.is_empty());assert_eq!(fixture.server.journal.lock().unwrap().latest().unwrap(),0);
    }

    #[test]
    fn exact_session_stop_fence_survives_attachment_and_blocks_new_input_and_restart() {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        let invoke = |server: &mut Server, method: &str, args: Value| {
            server.invoke(Invocation {
                proof: server.proof.clone(),
                epoch: Some(server.epoch),
                method: method.into(),
                args,
            })
        };
        let actor_id = server.identity.instance;
        assert!(
            invoke(
                server,
                "fence_session_stop",
                json!([uuid::Uuid::new_v4(), 1000])
            )
            .is_err()
        );
        assert!(!server.session_stopping);
        assert!(!server.actor.0.stop.load(Ordering::Acquire));
        invoke(server, "fence_session_stop", json!([actor_id, 1000])).unwrap();
        assert!(
            server.actor.0.stop.load(Ordering::Acquire),
            "ACK must follow the actual actor command fence"
        );
        let old_epoch = server.epoch;
        let source = server.source.clone();
        invoke(server, "attach", json!([source])).unwrap();
        assert_ne!(server.epoch, old_epoch);
        assert!(
            server.session_stopping,
            "reattachment cannot reopen admission"
        );
        let payload = serde_json::to_value(InputPayload::Command {
            run: None,
            command: Command::Refresh,
        })
        .unwrap();
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: payload_digest(&serde_json::to_vec(&payload).unwrap()),
        };
        assert!(
            invoke(server, "input", json!([identity, payload]))
                .unwrap_err()
                .contains("stopping")
        );
        assert_eq!(
            server.journal.lock().unwrap().next_input().unwrap(),
            1,
            "refused admission must not consume an input identity"
        );
        assert!(
            invoke(server, "restart", Value::Null)
                .unwrap_err()
                .contains("stopping")
        );
        // Read/ACK pathways remain available for two-phase durable cleanup.
        assert!(invoke(server, "outbox_unacked", Value::Null).is_ok());
        assert!(
            invoke(
                server,
                "input_receipts",
                json!([Vec::<InputIdentity>::new()])
            )
            .is_ok()
        );
    }

    #[test]
    fn retained_preservation_and_session_stop_have_one_attachment_winner() {
        for preserve_first in [false, true] {
            let mut fixture = resident_fixture();
            let server = &mut fixture.server;
            let invoke = |server: &mut Server, method: &str, args: Value| {
                server.invoke(Invocation { proof:server.proof.clone(),epoch:Some(server.epoch),method:method.into(),args })
            };
            let actor_id = server.identity.instance;
            if preserve_first {
                invoke(server,"preserve",Value::Null).unwrap();
                invoke(server,"preserve",Value::Null).unwrap(); // Lost-ACK retry is idempotent.
                assert!(server.attachment_preserved);
                assert!(invoke(server,"fence_session_stop",json!([actor_id,1000])).unwrap_err().contains("preserved"));
                assert!(!server.session_stopping);
                assert!(!server.actor.0.stop.load(Ordering::Acquire));
                assert!(invoke(server,"restart",Value::Null).unwrap_err().contains("preserved"));
                assert!(invoke(server,"close_until",json!([1000])).unwrap_err().contains("preserved"));
                let payload=serde_json::to_value(InputPayload::Command {run:None,command:Command::Refresh}).unwrap();
                let identity=InputIdentity {sequence:1,id:uuid::Uuid::new_v4(),hash:payload_digest(&serde_json::to_vec(&payload).unwrap())};
                assert!(invoke(server,"input",json!([identity,payload])).unwrap_err().contains("preserved"));
                assert_eq!(server.journal.lock().unwrap().next_input().unwrap(),1);
                assert!(invoke(server,"outbox_unacked",Value::Null).is_ok());
                assert!(invoke(server,"input_receipts",json!([Vec::<InputIdentity>::new()])).is_ok());
                let epoch=server.epoch; let source=server.source.clone();
                invoke(server,"attach",json!([source])).unwrap();
                assert_ne!(server.epoch,epoch);
                assert!(!server.attachment_preserved);
                invoke(server,"fence_session_stop",json!([actor_id,1000])).unwrap();
            } else {
                invoke(server,"fence_session_stop",json!([actor_id,1000])).unwrap();
                assert!(invoke(server,"preserve",Value::Null).unwrap_err().contains("stopping"));
                assert!(!server.attachment_preserved);
                let source=server.source.clone();
                invoke(server,"attach",json!([source])).unwrap();
                assert!(server.session_stopping);
                assert!(invoke(server,"preserve",Value::Null).unwrap_err().contains("stopping"));
            }
        }
    }

    #[test]
    fn session_stop_metadata_contention_respects_local_and_helper_deadlines() {
        // Snapshot publication and diagnostics must never be prerequisites for
        // admitting this identity-only command. An expired budget touches none.
        let mut fixture = resident_fixture();
        let actor = fixture.server.actor.clone();
        let actor_id = fixture.server.identity.instance;
        let state = actor.0.state.lock().unwrap();
        let failure = actor.0.failure.lock().unwrap();
        let close = actor.0.close_state.lock().unwrap();
        let started = Instant::now();
        assert!(actor.fence_session_stop_until(actor_id, started).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        let started = Instant::now();
        let error = fixture
            .server
            .invoke(Invocation {
                proof: fixture.server.proof.clone(),
                epoch: Some(fixture.server.epoch),
                method: "fence_session_stop".into(),
                args: json!([actor_id, 20]),
            })
            .unwrap_err();
        assert!(error.contains("identity is being updated"));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "helper must not block behind close/publication metadata"
        );
        assert!(!actor.0.stop.load(Ordering::Acquire));
        drop(close);
        drop(failure);
        drop(state);
        actor
            .fence_session_stop_until(actor_id, Instant::now() + Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn remote_session_stop_deadline_keeps_one_exact_request_without_cleanup_claim() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("stop.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut actor = RemoteActor::fixture();
        Arc::get_mut(&mut actor.0).unwrap().socket = socket;
        let actor_id = actor.identity().instance;
        assert!(
            actor
                .fence_session_stop_until(
                    uuid::Uuid::new_v4(),
                    Instant::now() + Duration::from_secs(1)
                )
                .is_err()
        );
        assert!(actor.0.session_stop_job.lock().unwrap().is_none());
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        let peer = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            let request: Invocation = receive(&control, TIMEOUT).unwrap();
            assert_eq!(request.method, "fence_session_stop");
            let (actual, _): (uuid::Uuid, u64) = serde_json::from_value(request.args).unwrap();
            assert_eq!(actual, actor_id);
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            send(&control, &Ok::<_, String>(Value::Null), TIMEOUT).unwrap();
        });
        assert!(
            actor
                .fence_session_stop_until(actor_id, Instant::now() + Duration::from_millis(20))
                .is_err()
        );
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let first = actor
            .0
            .session_stop_job
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        assert!(
            actor
                .fence_session_stop_until(actor_id, Instant::now() + Duration::from_millis(10))
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &first,
            actor.0.session_stop_job.lock().unwrap().as_ref().unwrap()
        ));
        assert!(!actor.state().completed);
        assert!(actor.owns_work());
        release.send(()).unwrap();
        actor
            .fence_session_stop_until(actor_id, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(
            !actor.state().completed,
            "fencing alone never proves provider cleanup"
        );
        assert!(actor.owns_work());
        peer.join().unwrap();
    }

    #[test]
    fn bounded_remote_close_keeps_one_pending_rpc_and_its_exact_owner() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("bounded.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut actor = RemoteActor::fixture();
        Arc::get_mut(&mut actor.0).unwrap().socket = socket;
        let resident = actor.state().as_ref().clone();
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        let peer = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            let request: Invocation = receive(&control, TIMEOUT).unwrap();
            assert_eq!(request.method, "close_until");
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            send(
                &control,
                &Ok::<_, String>(ClosePublication {
                    snapshot: Snapshot {
                        generation: 42,
                        exited: true,
                        ..Default::default()
                    },
                    resident,
                }),
                TIMEOUT,
            )
            .unwrap();
        });
        assert!(
            actor
                .close_until(Instant::now() + Duration::from_millis(20))
                .is_err()
        );
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let first = actor.0.close_job.lock().unwrap().as_ref().unwrap().clone();
        assert!(
            actor
                .close_until(Instant::now() + Duration::from_millis(10))
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &first,
            actor.0.close_job.lock().unwrap().as_ref().unwrap()
        ));
        assert!(
            !actor.state().completed,
            "a caller deadline never proves provider teardown"
        );
        release.send(()).unwrap();
        assert!(
            actor
                .close_until(Instant::now() + Duration::from_secs(1))
                .is_ok()
        );
        peer.join().unwrap();
        assert!(actor.state().completed);
        assert!(!actor.owns_work());
        assert_eq!(actor.snapshot(None).unwrap().generation, 42);
    }
    #[test]
    fn live_helper_retirement_refusal_is_not_a_successful_cleanup() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("rpc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut actor = RemoteActor::fixture();
        Arc::get_mut(&mut actor.0).unwrap().socket = socket;
        let peer = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            let request: Invocation = receive(&control, TIMEOUT).unwrap();
            assert_eq!(request.method, "retire");
            send(
                &control,
                &Err::<Value, _>(
                    "Final retained outcomes must be durably acknowledged before helper retirement"
                        .to_owned(),
                ),
                TIMEOUT,
            )
            .unwrap();
        });
        let error = actor.finish_close().unwrap_err();
        peer.join().unwrap();
        assert!(error.contains("durably acknowledged"));
        assert!(actor.identity().record.holder.is_current());
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
        let mut source = chat::native_holder::fixture_source();
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
        let mut spec = ProcessSpec::from(&source.launch.recipe);
        spec.arguments = vec!["-c".into(), script.into()];
        let actor = RemoteActor::spawn(spec, source.clone(), BTreeMap::new(), |_| {}).unwrap();
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
        // Stop the old proxy before observing/retiring this helper receipt.
        // Its actual actor and provider continue under the retained helper.
        actor.preserve().unwrap();
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: payload_digest(
                &serde_json::to_vec(
                    &serde_json::to_value(InputPayload::Command {
                        run: None,
                        command: Command::Refresh,
                    })
                    .unwrap(),
                )
                .unwrap(),
            ),
        };
        let payload = serde_json::to_value(InputPayload::Command {
            run: None,
            command: Command::Refresh,
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
        let record = actor.identity().record;
        drop(_lease);
        let mut proof = source.proof.clone().unwrap();
        proof.host_incarnation = HostIncarnation::new(uuid::Uuid::new_v4().to_string());
        let _replacement_lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
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
        assert!(
            response
                .try_recv()
                .unwrap()
                .unwrap_err()
                .contains("must not be regenerated")
        );
        assert!(
            state.outcome.is_none(),
            "a superseded attachment does not prove provider interruption"
        );
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
