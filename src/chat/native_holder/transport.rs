//! Bounded local control for the process owner. Provider JSON goes directly
//! between the daemon and app-server; this process never sees a prompt.

use super::Recovery;
use crate::{
    chat::{
        Session,
        protocol::{LaunchCleanup, LaunchOutcome, ProcessSpec, TransportSpec},
    },
    daemon::holder_registry::{self as registry, Kind, Process, Record},
    model::HostSessionProof,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            fs::MetadataExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const ARGUMENT: &str = "--cagents-native-holder";
const PROTOCOL: u32 = 4;
const TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 16 << 20;
// This status is emitted only after the helper's owned descendants have all
// been reaped. Signals, panics and ordinary setup exits are not cleanup proof.
const CLEANUP_PROVED: i32 = 73;

struct StartupOwner {
    control: UnixStream,
    child: std::sync::Mutex<Option<std::process::Child>>,
    outcome: std::sync::Arc<std::sync::Mutex<LaunchOutcome>>,
    receipt: std::sync::Arc<std::sync::Mutex<Option<super::cleanup::Startup>>>,
    retiring: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LaunchCleanup for StartupOwner {
    fn cleanup(&self) -> Result<LaunchOutcome, String> {
        self.retiring
            .store(true, std::sync::atomic::Ordering::Release);
        // A failed startup never became durable. EOF requests its teardown;
        // don't reuse a stream whose startup response may be partially read.
        let _ = self.control.shutdown(std::net::Shutdown::Both);
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(process) = child.as_mut() {
            // Only a stopped reaper before handoff leaves a Child here.
            // No Spawn request has been sent on that path.
            let _ = process.kill();
            if process
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                child.take();
                *self.outcome.lock().unwrap_or_else(|p| p.into_inner()) = LaunchOutcome::NoProvider;
            }
        }
        if let Some(receipt) = self
            .receipt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            && receipt.consume()?
        {
            *self.outcome.lock().unwrap_or_else(|p| p.into_inner()) = LaunchOutcome::TeardownProved;
        }
        Ok(*self.outcome.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

#[derive(Serialize, Deserialize)]
struct Spawn {
    program: String,
    arguments: Vec<String>,
    directory: PathBuf,
    environment: BTreeMap<String, String>,
    recovery: Recovery,
    directory_fd: Option<RawFd>,
}

impl Spawn {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .current_dir(&self.directory)
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0);
        crate::host::descriptor_limit::inherit(&mut command);
        // The helper's signalfd owns SIGCHLD; its provider must receive the
        // usual unblocked child-exit signals after exec.
        unsafe {
            command.pre_exec(|| {
                let mut signals = std::mem::zeroed();
                libc::sigemptyset(&mut signals);
                libc::sigaddset(&mut signals, libc::SIGCHLD);
                if libc::sigprocmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if let Some(fd) = self.directory_fd {
            // The dedicated holder keeps this validated directory alive. Only
            // async-signal-safe syscalls run between fork and provider exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::fchdir(fd) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command
    }
}

#[derive(Serialize, Deserialize)]
enum Request {
    Diagnostic,
    Spawn {
        protocol: u32,
        spawn: Spawn,
    },
    Confirm {
        thread: String,
        identity: crate::chat::protocol::ServerIdentity,
        process: Process,
    },
    UpdateSource {
        source: Session,
        execution_console: Option<crate::model::LaunchRecipe>,
        paired_holder: Option<crate::terminal::holder::Record>,
    },
    Inspect {
        proof: HostSessionProof,
    },
    Adopt {
        source: Session,
        proof: HostSessionProof,
    },
    Release,
}

#[derive(Serialize, Deserialize)]
enum Report {
    Diagnostic(ExitDiagnostic),
    Started {
        protocol: u32,
        record: Record,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope_error: Option<String>,
    },
    Confirmed,
    SourceUpdated,
    Released,
    Inspected {
        protocol: u32,
        record: Record,
        recovery: Recovery,
        exited: bool,
    },
    Adopted(Recovery),
    Failed(String),
}

/// Raw transport evidence. Presentation strips escape sequences and selects a
/// short reason; keeping this separate prevents transport errors hiding exits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExitDiagnostic {
    // Preserve the original wire keys for older clients, but name the owner
    // explicitly in Rust. A launcher can exit before its confirmed server.
    #[serde(rename = "code")]
    pub launcher_code: Option<i32>,
    #[serde(rename = "signal")]
    pub launcher_signal: Option<i32>,
    #[serde(default)]
    pub launcher: Option<Process>,
    #[serde(default)]
    pub confirmed_server: Option<ServerObservation>,
    pub stderr: String,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ServerObservation {
    pub process: Process,
    // This observes the exact /proc identity, not an exit status. An adopted
    // descendant's wait status may already have been consumed by tool reaping.
    pub identity_current: bool,
}

impl ExitDiagnostic {
    fn identify_launcher(mut self, launcher: &Process) -> Result<Self, String> {
        if self
            .launcher
            .as_ref()
            .is_some_and(|actual| actual != launcher)
        {
            return Err("Native holder diagnostic belongs to another launcher".into());
        }
        // Old v4 holders return only code/signal. Their known meaning is the
        // original child, so label that identity without inventing a server.
        self.launcher = Some(launcher.clone());
        Ok(self)
    }
}

/// Dropping an intentionally closed actor releases its server. An unexpected
/// daemon exit instead closes the socket without sending Release.
pub(crate) struct Holder {
    control: Option<UnixStream>,
    release_on_drop: bool,
    durable: bool,
    identity: super::Detached,
}

impl Holder {
    pub fn diagnostic(&mut self) -> Result<ExitDiagnostic, String> {
        if !self.identity.record.build.as_ref().is_some_and(|build| build.capabilities.iter().any(|capability| capability == "provider-exit-diagnostics")) {
            return Err("This older provider holder has no exit diagnostics; restart the session to update it".into());
        }
        let control = self.control.as_ref().ok_or("Native holder is detached")?;
        send(control, &Request::Diagnostic, TIMEOUT)?;
        match receive(control, TIMEOUT)? {
            Some(Report::Diagnostic(diagnostic)) => {
                diagnostic.identify_launcher(&self.identity.record.root)
            }
            Some(Report::Failed(error)) => Err(error),
            _ => Err("Native holder did not report provider exit diagnostics".into()),
        }
    }

    pub fn is_durable(&self) -> bool {
        self.durable
    }
    pub fn confirm(
        &mut self,
        thread: String,
        identity: crate::chat::protocol::ServerIdentity,
        process: Process,
    ) -> Result<(), String> {
        let control = self.control.as_ref().ok_or("Native holder is detached")?;
        send(
            control,
            &Request::Confirm {
                thread,
                identity,
                process,
            },
            TIMEOUT,
        )?;
        match receive(control, TIMEOUT)? {
            Some(Report::Confirmed) => {
                self.durable = true;
                Ok(())
            }
            Some(Report::Failed(error)) => Err(error),
            _ => Err("Native holder did not acknowledge provider identity".into()),
        }
    }

    pub fn preserve(mut self) -> super::Detached {
        self.release_on_drop = false;
        self.control.take();
        self.identity.clone()
    }

    pub fn update_source(
        &mut self,
        source: Session,
        execution_console: Option<crate::model::LaunchRecipe>,
        paired_holder: Option<crate::terminal::holder::Record>,
    ) -> Result<(), String> {
        let control = self.control.as_ref().ok_or("Native holder is detached")?;
        send(
            control,
            &Request::UpdateSource {
                source,
                execution_console,
                paired_holder,
            },
            TIMEOUT,
        )?;
        match receive(control, TIMEOUT)? {
            Some(Report::SourceUpdated) => Ok(()),
            Some(Report::Failed(error)) => Err(error),
            _ => Err("Native holder did not acknowledge paired view ownership".into()),
        }
    }

    pub fn close(&mut self) -> Result<(), String> {
        self.close_inner(false)
    }

    pub(in crate::chat) fn close_inner(&mut self, retain: bool) -> Result<(), String> {
        if self.identity.receipt(retain)? {
            self.control.take();
            self.release_on_drop = false;
            return Ok(());
        }
        let Some(control) = self.control.as_ref() else {
            return if retain {
                self.identity.close_retaining_receipt()
            } else {
                self.identity.close()
            };
        };
        send(control, &Request::Release, TIMEOUT)?;
        match receive(control, TIMEOUT)? {
            Some(Report::Released) => {
                if !self.identity.receipt(retain)? {
                    return Err("Native holder released without durable cleanup evidence".into());
                }
                self.control.take();
                self.release_on_drop = false;
                Ok(())
            }
            Some(Report::Failed(error)) => Err(error),
            _ => Err("Native holder did not acknowledge server teardown".into()),
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if self.release_on_drop {
            let _ = self.close();
        }
    }
}

pub(crate) fn spawn(mut spec: ProcessSpec, source: Session) -> Result<Holder, String> {
    std::hint::black_box(&SERVE);
    spec.remove_unset_environment();
    if source.native_continuity != crate::chat::NativeContinuity::ReconnectOnly {
        return Err(
            "A durable native source requires verified adoption, never another server launch"
                .into(),
        );
    }
    let TransportSpec::OwnedUnixWebSocket { socket } = &spec.transport else {
        return Err("Native holder requires an owned provider listener".into());
    };
    if source
        .owned_codex_listener
        .as_ref()
        .is_none_or(|owned| &owned.socket != socket)
    {
        return Err("Provider listener does not match its ownership evidence".into());
    }
    let retained_source = source.clone();
    let recovery = Recovery {
        source,
        execution_console: None,
        paired_holder: None,
        previous_sources: Vec::new(),
        confirmed_thread: None,
        server_version: None,
        server_identity: None,
        server_process: None,
        execution_environment: crate::terminal_ipc::ChatExecutionEnvironment(
            spec.environment.clone(),
        ),
    };
    recovery.validate(&crate::daemon::read_boot_id().map_err(|error| error.to_string())?)?;
    let source_proof = retained_source
        .proof
        .clone()
        .ok_or("Native holder source has no daemon proof")?;
    let execution = spec.execution.clone();
    let directory = execution
        .as_ref()
        .and_then(|context| context.directory.clone());
    let directory_fd = directory.as_ref().map(|file| file.as_raw_fd());
    let request = Spawn {
        directory_fd,
        program: spec.program,
        arguments: spec.arguments,
        directory: spec.working_directory,
        environment: spec.environment,
        recovery,
    };
    // Hold the account lease through the actual child creation and its ACK.
    // The holder receives this exact final environment, never an inherited one.
    let command = request.command();
    let _account =
        crate::account::registry::authorize_command(&command).map_err(|error| error.to_string())?;
    let (control, remote) = UnixStream::pair().map_err(|error| error.to_string())?;
    let fd = remote.as_raw_fd();
    let mut holder = Command::new("/proc/self/exe");
    holder
        .arg0("cagents-native")
        .arg(ARGUMENT)
        .arg(fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        holder.pre_exec(move || {
            crate::daemon::seal_inherited_descriptors()?;
            if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if let Some(directory) = directory_fd {
                if libc::fcntl(directory, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let owner = std::sync::Arc::new(StartupOwner {
        control: control.try_clone().map_err(|error| error.to_string())?,
        child: std::sync::Mutex::new(None),
        outcome: std::sync::Arc::new(std::sync::Mutex::new(LaunchOutcome::Uncertain)),
        receipt: Default::default(),
        retiring: Default::default(),
    });
    // Allocate the reaper before creating any child. Once the helper exists,
    // publishing its exact owner and handing it to this thread cannot require
    // another thread allocation.
    let (publish, incoming) = std::sync::mpsc::sync_channel(1);
    let (reaping, ready) = std::sync::mpsc::sync_channel(1);
    let watch = owner.clone();
    std::thread::Builder::new()
        .name("native-holder-reap".into())
        .spawn(move || {
            if incoming.recv().is_err() {
                return;
            }
            let child = watch.child.lock().unwrap_or_else(|p| p.into_inner()).take();
            let outcome = watch.outcome.clone();
            let receipt = watch.receipt.clone();
            let retiring = watch.retiring.clone();
            drop(watch); // Reaping must not keep the control connection alive.
            let _ = reaping.send(());
            if let Some(mut child) = child {
                let proved = child
                    .wait()
                    .is_ok_and(|status| status.code() == Some(CLEANUP_PROVED));
                let receipt_proved = retiring.load(std::sync::atomic::Ordering::Acquire)
                    && receipt
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .as_ref()
                        .is_some_and(|receipt| receipt.consume().unwrap_or(false));
                if proved || receipt_proved {
                    *outcome.lock().unwrap_or_else(|p| p.into_inner()) =
                        LaunchOutcome::TeardownProved;
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let child = holder.spawn().map_err(|error| error.to_string())?;
    let pid = child.id() as i32;
    *owner.child.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
    if let Some(context) = &execution {
        context.claim_owner(owner.clone());
    }
    if let Ok(holder) = Process::read(pid) {
        *owner.receipt.lock().unwrap_or_else(|p| p.into_inner()) = Some(super::cleanup::Startup {
            terminal: retained_source.id,
            holder,
            proof: source_proof.clone(),
        });
    }
    drop(remote);
    if publish.send(()).is_err() || ready.recv().is_err() {
        let _ = owner.cleanup();
        return Err("Native holder reaper stopped before provider startup".into());
    }
    let result = (|| {
        send(
            &control,
            &Request::Spawn {
                protocol: PROTOCOL,
                spawn: request,
            },
            START_TIMEOUT,
        )?;
        match receive(&control, START_TIMEOUT)? {
            Some(Report::Started {
                protocol,
                record,
                scope_error,
            }) if protocol == PROTOCOL
                && record.holder.pid == pid
                && record.holder.is_current() =>
            {
                if let Some(error) = scope_error {
                    tracing::warn!(
                        %error,
                        holder = record.holder.pid,
                        "holder scope enrollment failed; provider uses existing launch fallback"
                    );
                    // Execed holders have null stderr and no subscriber. Keep
                    // the refusal visible in staged owning-test failure output.
                    #[cfg(test)]
                    eprintln!("native holder scope enrollment failed: {error}");
                }
                crate::daemon::process_scope::warn_if_shared(
                    record.holder.pid as u32,
                    record.root.pid as u32,
                );
                Ok(record)
            }
            Some(Report::Failed(error)) => Err(error),
            _ => Err("Native holder did not acknowledge its server".into()),
        }
    })();
    match result {
        Ok(record) => Ok(Holder {
            control: Some(control),
            release_on_drop: true,
            durable: false,
            identity: super::Detached {
                record,
                proof: source_proof.clone(),
                source: retained_source,
            },
        }),
        Err(error) => {
            // The execution context may retain a control clone. Explicit
            // shutdown starts cleanup before a later Close reconciles it.
            let _ = owner.cleanup();
            drop(control);
            Err(error)
        }
    }
}

fn connect(record: &Record) -> Result<UnixStream, String> {
    let directory = registry::directory(Kind::Native)?;
    let socket = registry::socket(&directory, record.terminal);
    if registry::evidence(&socket, Kind::Native)?.as_ref() != Some(record)
        || !record.holder.is_current()
    {
        return Err("Native holder discovery evidence changed".into());
    }
    let control =
        crate::daemon::holder_registry::connect(&socket).map_err(|error| error.to_string())?;
    let credentials = peer(&control)?;
    if credentials.uid != crate::daemon::current_uid()
        || credentials.pid != record.holder.pid
        || !record.holder.is_current()
    {
        return Err("Native holder endpoint belongs to another process".into());
    }
    Ok(control)
}

pub(crate) fn inspect(
    record: &Record,
    proof: &HostSessionProof,
) -> Result<(Recovery, bool), String> {
    let control = connect(record)?;
    send(
        &control,
        &Request::Inspect {
            proof: proof.clone(),
        },
        TIMEOUT,
    )?;
    match receive(&control, TIMEOUT)? {
        Some(Report::Inspected {
            protocol,
            record: actual,
            recovery,
            exited,
        }) if matches!(protocol, 2..=PROTOCOL) && actual == *record => {
            recovery.validate(&record.boot)?;
            Ok((recovery, exited))
        }
        Some(Report::Failed(error)) => Err(error),
        _ => Err("Native holder did not return its exact discovery evidence".into()),
    }
}

pub(crate) fn adopt(
    record: &Record,
    source: &Session,
    proof: &HostSessionProof,
) -> Result<(Holder, Recovery), String> {
    let control = connect(record)?;
    if record.is_outdated() {
        tracing::warn!(terminal = %record.terminal, holder_pid = record.holder.pid,
            helper_version = ?record.build.as_ref().map(|build| &build.version),
            daemon_version = crate::VERSION,
            zombies = ?record.zombie_children().ok(),
            "adopting a native conversation running on an outdated helper");
    }
    send(
        &control,
        &Request::Adopt {
            source: source.clone(),
            proof: proof.clone(),
        },
        TIMEOUT,
    )?;
    match receive(&control, TIMEOUT)? {
        Some(Report::Adopted(recovery)) => {
            recovery.validate(&record.boot)?;
            if recovery.source.id != source.id
                || recovery.source.proof.as_ref() != Some(proof)
                || (&recovery.source != source && !recovery.previous_sources.contains(source))
            {
                return Err("Native holder did not preserve exact adoption lineage".into());
            }
            Ok((
                Holder {
                    control: Some(control),
                    release_on_drop: false,
                    durable: true,
                    identity: super::Detached {
                        record: record.clone(),
                        source: recovery.source.clone(),
                        proof: proof.clone(),
                    },
                },
                recovery,
            ))
        }
        Some(Report::Failed(error)) => Err(error),
        _ => Err("Native holder did not acknowledge adoption".into()),
    }
}

struct Residence {
    directory: PathBuf,
    listener: Listener,
    record: Record,
    recovery: Recovery,
    child: Server,
    child_events: ChildEvents,
    stderr: StderrCapture,
    released: bool,
    _working_directory: Option<std::fs::File>,
}

impl Residence {
    fn start(spawn: Spawn) -> Result<Self, String> {
        // Block before creating children so even a very short-lived tool cannot
        // disappear between the initial reap and the blocking poll.
        let child_events = ChildEvents::new().map_err(|error| error.to_string())?;
        let working_directory = spawn
            .directory_fd
            .map(|fd| {
                // The parent deliberately inherited this descriptor. Validate it
                // before taking ownership; never accept the control socket as cwd.
                let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
                if fd <= 2 || unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } == -1 {
                    return Err("Invalid retained execution directory".to_owned());
                }
                if unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err("Retained execution descriptor is not a directory".into());
                }
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                    return Err(io::Error::last_os_error().to_string());
                }
                Ok(unsafe { std::fs::File::from_raw_fd(fd) })
            })
            .transpose()?;
        let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
        spawn.recovery.validate(&boot)?;
        let directory = registry::directory(Kind::Native)?;
        crate::secure_file::prepare_private_directory(&directory)
            .map_err(|error| error.to_string())?;
        let socket = registry::socket(&directory, spawn.recovery.source.id);
        let listener = Listener::bind(socket)?;
        let mut child = match spawn.command().spawn() {
            Ok(child) => Server(crate::daemon::owned_children::OwnedChildren::new(child)),
            Err(error) => {
                return Err(error.to_string());
            }
        };
        let stderr = StderrCapture::new(child.take_stderr(), spawn.recovery.source.id)?;
        let record = Record {
            version: 4,
            build: Some(registry::BuildIdentity::current(Kind::Native)),
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot,
            terminal: spawn.recovery.source.id,
            holder: Process::read(std::process::id() as i32).map_err(|error| error.to_string())?,
            root: Process::read(child.id()).map_err(|error| error.to_string())?,
        };
        let residence = Self {
            directory,
            listener,
            record,
            recovery: spawn.recovery,
            child,
            child_events,
            stderr,
            released: false,
            _working_directory: working_directory,
        };
        residence
            .record
            .create(&residence.directory)
            .map_err(|error| error.to_string())?;
        Ok(residence)
    }

    fn exited(&self) -> bool {
        self.recovery
            .server_process
            .as_ref()
            .map_or_else(|| self.child.exited(), |server| !server.is_current())
    }

    fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        if !self
            .child
            .terminate(Duration::from_secs(2))
            .map_err(|error| error.to_string())?
        {
            return Err(
                "Provider descendants are still stopping; ownership remains with the native holder"
                    .into(),
            );
        }
        if let Some(owned) = &self.recovery.source.owned_codex_listener {
            let _ = std::fs::remove_file(&owned.socket);
            if let Some(parent) = owned.socket.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        self.listener.retire()?;
        super::cleanup::publish(
            &self.record,
            self.recovery
                .source
                .proof
                .as_ref()
                .ok_or("Native holder has no release proof")?,
            &self.recovery,
        )?;
        self.record
            .remove_record(&self.directory)
            .map_err(|error| error.to_string())?;
        self.released = true;
        Ok(())
    }

    fn attend(&mut self, control: UnixStream) -> bool {
        loop {
            if wait_for_control_with_stderr(control.as_raw_fd(), &self.child, &self.child_events, Some(&mut self.stderr)).is_err() {
                return self.recovery.is_durable();
            }
            let request = match receive(&control, TIMEOUT) {
                Ok(Some(request)) => request,
                _ => return self.recovery.is_durable(),
            };
            let response = match request {
                Request::Diagnostic => {
                    use std::os::unix::process::ExitStatusExt;
                    let result = self
                        .stderr
                        .drain()
                        .and_then(|()| self.child.exit_status())
                        .and_then(|status| {
                            if status.is_some() {
                                // Once the leader exited, drain its remaining pipe
                                // bytes before publishing the last meaningful line.
                                // Descendants may retain stderr; keep this bounded.
                                for _ in 0..16 {
                                    self.stderr.drain()?;
                                    if self.stderr.reader.is_none() {
                                        break;
                                    }
                                }
                            }
                            Ok(status)
                        });
                    match result {
                        Ok(status) => Report::Diagnostic(ExitDiagnostic {
                            launcher_code: status.and_then(|status| status.code()),
                            launcher_signal: status.and_then(|status| status.signal()),
                            launcher: Some(self.record.root.clone()),
                            confirmed_server: self.recovery.server_process.as_ref().map(|server| {
                                ServerObservation {
                                    process: server.clone(),
                                    identity_current: server.is_current(),
                                }
                            }),
                            stderr: String::from_utf8_lossy(self.stderr.tail.make_contiguous())
                                .into_owned(),
                            log_path: self.stderr.log_path.clone(),
                        }),
                        Err(error) => Report::Failed(error.to_string()),
                    }
                }
                Request::Release => match self.release() {
                    Ok(()) => {
                        let _ = send(&control, &Report::Released, TIMEOUT);
                        return false;
                    }
                    Err(error) => Report::Failed(error),
                },
                Request::Confirm {
                    thread,
                    identity,
                    process,
                } => {
                    match self
                        .verify_server(&process)
                        .and_then(|()| self.recovery.confirm(thread, identity, process))
                    {
                        Ok(()) => Report::Confirmed,
                        Err(error) => Report::Failed(error),
                    }
                }
                Request::UpdateSource {
                    source,
                    execution_console,
                    paired_holder,
                } => match self
                    .recovery
                    .update_source(source, execution_console, paired_holder)
                {
                    Ok(()) => Report::SourceUpdated,
                    Err(error) => Report::Failed(error),
                },
                _ => Report::Failed("Unexpected request on an owned native holder".into()),
            };
            if send(&control, &response, TIMEOUT).is_err() {
                return self.recovery.is_durable();
            }
        }
    }

    fn verify_server(&self, server: &Process) -> Result<(), String> {
        if self
            .recovery
            .server_process
            .as_ref()
            .is_some_and(|old| old != server)
        {
            return Err("The retained app-server process changed".into());
        }
        let mut current = server.clone();
        // A launcher may exec the server or spawn it. This dedicated holder
        // owns only that provider tree, including reparented descendants.
        for _ in 0..1024 {
            let stat = std::fs::read(format!("/proc/{}/stat", current.pid))
                .map_err(|error| error.to_string())?;
            let (_, start) = crate::model::agent_recovery::process_identity(&stat)
                .ok_or("Invalid app-server ancestry")?;
            let parent: i32 = std::str::from_utf8(&stat)
                .ok()
                .and_then(|stat| stat.rsplit_once(')'))
                .and_then(|(_, fields)| fields.split_whitespace().nth(1))
                .and_then(|parent| parent.parse().ok())
                .ok_or("Invalid app-server parent")?;
            if start != current.start {
                break;
            }
            if parent == self.record.holder.pid
                && self.record.holder.is_current()
                && server.is_current()
            {
                return Ok(());
            }
            if parent <= 1 {
                break;
            }
            current = Process::read(parent).map_err(|error| error.to_string())?;
        }
        Err("Codex socket peer is not owned by this native holder".into())
    }

    fn orphan(&mut self) -> bool {
        loop {
            if wait_for_control_with_stderr(self.listener.socket.as_raw_fd(), &self.child, &self.child_events, Some(&mut self.stderr)).is_err() {
                return false;
            }
            let (control, _) = match self.listener.socket.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            };
            let request = match receive(&control, TIMEOUT) {
                Ok(Some(request)) => request,
                _ => continue,
            };
            let proof = match &request {
                Request::Inspect { proof } | Request::Adopt { proof, .. } => proof,
                _ => continue,
            };
            if authorize(&control, proof).is_err() {
                continue;
            }
            match request {
                Request::Inspect { .. } => {
                    let _ = send(
                        &control,
                        &Report::Inspected {
                            protocol: PROTOCOL,
                            record: self.record.clone(),
                            recovery: self.recovery.clone(),
                            exited: self.exited(),
                        },
                        TIMEOUT,
                    );
                }
                Request::Adopt { source, proof } => match self.recovery.adopt(&source, proof) {
                    Ok(()) => {
                        if send(&control, &Report::Adopted(self.recovery.clone()), TIMEOUT).is_ok()
                        {
                            return self.attend(control);
                        }
                    }
                    Err(error) => {
                        let _ = send(&control, &Report::Failed(error), TIMEOUT);
                    }
                },
                _ => unreachable!(),
            }
        }
    }
}

/// One bounded ring, drained on the holder's existing poll loop. No per-agent
/// reader thread or periodic wakeup is needed. Full raw stderr is opt-in disk
/// debug output, just like the daemon's direct provider transport.
struct StderrCapture {
    reader: Option<std::process::ChildStderr>,
    tail: std::collections::VecDeque<u8>,
    log: Option<std::fs::File>,
    log_path: Option<PathBuf>,
}
impl StderrCapture {
    fn new(reader: Option<std::process::ChildStderr>, terminal: crate::model::TerminalId) -> Result<Self, String> {
        if let Some(reader) = &reader {
            let fd = reader.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
                return Err(io::Error::last_os_error().to_string());
            }
        }
        let log_path = (std::env::var_os("CAGENTS_LOCAL_DEBUG").as_deref() == Some(std::ffi::OsStr::new("1")))
            .then(|| crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join("provider-stderr").join(format!("{terminal}-{}.log", std::process::id())))).flatten();
        let log = log_path.as_ref().and_then(|path| {
            use std::os::unix::fs::OpenOptionsExt;
            let parent = path.parent()?;
            crate::secure_file::prepare_private_directory(parent).ok()?;
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).ok()
        });
        let log_path = log.is_some().then_some(log_path).flatten();
        Ok(Self { reader, tail: Default::default(), log, log_path })
    }
    fn drain(&mut self) -> io::Result<()> {
        let mut bytes = [0_u8; 8192];
        // Bound work per poll so a noisy provider cannot starve Interrupt or
        // ownership updates. A still-readable pipe immediately polls again.
        for _ in 0..8 {
            let Some(reader) = self.reader.as_mut() else { break; };
            match reader.read(&mut bytes) {
                Ok(0) => { self.reader.take(); break; }
                Ok(count) => {
                    if let Some(log) = self.log.as_mut() && log.write_all(&bytes[..count]).is_err() { self.log.take(); }
                    for byte in &bytes[..count] {
                        if self.tail.len() == 64 * 1024 { self.tail.pop_front(); }
                        self.tail.push_back(*byte);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// SIGCHLD is blocked only in the dedicated holder. The provider's pre-exec
/// hook restores the ordinary mask so its own child management is unaffected.
struct ChildEvents {
    fd: std::fs::File,
    previous: libc::sigset_t,
}

impl ChildEvents {
    fn new() -> io::Result<Self> {
        // SAFETY: both signal sets are live writable objects; this helper is
        // single-threaded and is the sole owner of child wait statuses.
        unsafe {
            let mut mask = std::mem::zeroed();
            let mut previous = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGCHLD);
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous);
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            let fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                let error = io::Error::last_os_error();
                libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
                return Err(error);
            }
            Ok(Self {
                fd: std::fs::File::from_raw_fd(fd),
                previous,
            })
        }
    }

    fn drain(&self) -> io::Result<()> {
        let mut event = [0_u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        loop {
            match (&self.fd).read(&mut event) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for ChildEvents {
    fn drop(&mut self) {
        // SAFETY: restore the mask on the same single holder thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

/// A child exit or control message is the only source of idle wakeups. Reap
/// once before blocking as well, including orphans already pending at startup.
#[cfg(test)]
fn wait_for_control(fd: RawFd, children: &crate::daemon::owned_children::OwnedChildren, child_events: &ChildEvents) -> io::Result<()> {
    wait_for_control_with_stderr(fd, children, child_events, None)
}

fn wait_for_control_with_stderr(
    fd: RawFd,
    children: &crate::daemon::owned_children::OwnedChildren,
    child_events: &ChildEvents,
    mut stderr: Option<&mut StderrCapture>,
) -> io::Result<()> {
    loop {
        if let Some(stderr) = stderr.as_mut() { stderr.drain()?; }
        if let Err(error) = children.reap_exited_descendants() {
            // A transient inspection failure must not tear down a durable server.
            eprintln!("Native holder could not reap exited tools: {error}");
        }
        let mut poll = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: child_events.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stderr.as_ref().and_then(|stderr| stderr.reader.as_ref()).map_or(-1, AsRawFd::as_raw_fd), events: libc::POLLIN, revents: 0 }];
        // SAFETY: the array and both owned descriptors remain live throughout
        // poll. No timer is armed: an idle holder sleeps indefinitely.
        match unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, -1) } {
            result if result > 0 => {
                if poll[1].revents != 0 {
                    child_events.drain()?;
                    // A simultaneous control event must not postpone reaping.
                    children.reap_exited_descendants()?;
                }
                if poll[0].revents != 0 {
                    return Ok(());
                }
            }
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

/// Removes even an unregistered startup endpoint, but never a replacement
/// created after this owner's endpoint was retired.
struct Listener {
    socket: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Listener {
    fn bind(path: PathBuf) -> Result<Self, String> {
        let socket =
            crate::daemon::holder_registry::bind(&path).map_err(|error| error.to_string())?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = std::fs::remove_file(path);
                return Err(error.to_string());
            }
        };
        Ok(Self {
            socket,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn retire(&self) -> Result<(), String> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.dev() == self.device && metadata.ino() == self.inode => {
                match std::fs::remove_file(&self.path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    result => result.map_err(|error| error.to_string())?,
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

impl Drop for Residence {
    fn drop(&mut self) {
        // No completion receipt or discovery retirement until every owned
        // descendant is reaped. A stuck child leaves this small owner alive.
        while self.release().is_err() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct Server(crate::daemon::owned_children::OwnedChildren);
impl std::ops::Deref for Server {
    type Target = crate::daemon::owned_children::OwnedChildren;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for Server {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        while !self.terminate(Duration::from_secs(2)).unwrap_or(false) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn authorize(control: &UnixStream, proof: &HostSessionProof) -> Result<(), String> {
    let credentials = peer(control)?;
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    let owner =
        crate::daemon::lease_owner(&paths.lock, proof).map_err(|error| error.to_string())?;
    if credentials.uid != crate::daemon::current_uid()
        || u32::try_from(credentials.pid).ok() != Some(owner)
    {
        return Err("Native holder caller does not own the daemon lease".into());
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
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(credentials)
}

fn send(control: &UnixStream, value: &impl Serialize, timeout: Duration) -> Result<(), String> {
    let data = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if data.len() > MAX_FRAME {
        return Err("Native holder control frame is too large".into());
    }
    let deadline = Instant::now() + timeout;
    for mut bytes in [&(data.len() as u32).to_be_bytes()[..], &data[..]] {
        while !bytes.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or("Native holder write timed out")?;
            control
                .set_write_timeout(Some(remaining))
                .map_err(|error| error.to_string())?;
            match (&*control).write(bytes) {
                Ok(0) => return Err("Native holder control closed".into()),
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(())
}

fn receive<T: DeserializeOwned>(
    control: &UnixStream,
    timeout: Duration,
) -> Result<Option<T>, String> {
    let deadline = Instant::now() + timeout;
    let mut header = [0; 4];
    if !read_exact(control, &mut header, deadline)? {
        return Ok(None);
    }
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("Invalid native holder control frame length".into());
    }
    let mut data = vec![0; length];
    if !read_exact(control, &mut data, deadline)? {
        return Err("Incomplete native holder control frame".into());
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn read_exact(
    control: &UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<bool, String> {
    let mut read = false;
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Native holder read timed out")?;
        control
            .set_read_timeout(Some(remaining))
            .map_err(|error| error.to_string())?;
        match (&*control).read(bytes) {
            Ok(0) if !read => return Ok(false),
            Ok(0) => return Err("Incomplete native holder control frame".into()),
            Ok(count) => {
                read = true;
                bytes = &mut bytes[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(true)
}

#[used]
#[unsafe(link_section = ".init_array")]
static SERVE: extern "C" fn() = serve_if_requested;

extern "C" fn serve_if_requested() {
    let Ok(arguments) = std::fs::read("/proc/self/cmdline") else {
        return;
    };
    let mut arguments = arguments.split(|&byte| byte == 0).skip(1);
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
    let _result = (|| {
        let Some(Request::Spawn {
            protocol: PROTOCOL,
            spawn,
        }) = receive(&control, START_TIMEOUT)?
        else {
            return Err("Missing native holder spawn request".to_owned());
        };
        // This is an execed holder, not a post-fork pre_exec hook. Complete
        // scope enrollment before the provider's first process can exist.
        let scope_error = crate::daemon::process_scope::enroll_holder()
            .err()
            .map(|error| error.to_string());
        let mut residence = Residence::start(spawn)?;
        send(
            &control,
            &Report::Started {
                protocol: PROTOCOL,
                record: residence.record.clone(),
                scope_error,
            },
            START_TIMEOUT,
        )?;
        if residence.attend(control.try_clone().map_err(|error| error.to_string())?) {
            drop(control);
            while residence.orphan() {}
        }
        Ok(())
    })();
    // Returning from the closure drops Residence/Server, whose teardown waits
    // for ECHILD. This is an explicit receipt, not an inference from PID death.
    std::process::exit(CLEANUP_PROVED);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_report_preserves_optional_scope_failure() {
        let record = Record {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: crate::model::BootId::new("fixture-boot"),
            terminal: crate::model::TerminalId::new(),
            holder: Process { pid: 42, start: 17 },
            root: Process { pid: 43, start: 18 },
        };
        let mut wire = serde_json::to_value(Report::Started {
            protocol: PROTOCOL,
            record: record.clone(),
            scope_error: None,
        })
        .unwrap();
        assert!(wire["Started"].get("scope_error").is_none());
        let Report::Started {
            protocol,
            record: decoded,
            scope_error,
        } = serde_json::from_value(wire.clone()).unwrap()
        else {
            panic!("legacy startup report changed type");
        };
        assert_eq!(protocol, PROTOCOL);
        assert_eq!(decoded, record);
        assert_eq!(scope_error, None);

        wire["Started"]["scope_error"] = serde_json::json!("fixture enrollment refusal");
        let Report::Started {
            protocol,
            record: decoded,
            scope_error,
        } = serde_json::from_value(wire).unwrap()
        else {
            panic!("diagnostic startup report changed type");
        };
        assert_eq!(protocol, PROTOCOL);
        assert_eq!(decoded, record);
        assert_eq!(scope_error.as_deref(), Some("fixture enrollment refusal"));
    }

    #[test]
    fn legacy_exit_diagnostics_identify_only_the_launcher() {
        let legacy = serde_json::json!({
            "code": 0,
            "signal": null,
            "stderr": "launcher finished\n",
            "log_path": null,
        });
        let launcher = Process { pid: 42, start: 17 };
        let diagnostic: ExitDiagnostic = serde_json::from_value(legacy).unwrap();
        let diagnostic = diagnostic.identify_launcher(&launcher).unwrap();
        assert_eq!(diagnostic.launcher.as_ref(), Some(&launcher));
        assert_eq!(diagnostic.launcher_code, Some(0));
        assert_eq!(diagnostic.launcher_signal, None);
        assert!(diagnostic.confirmed_server.is_none());
        let wire = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(wire["code"], 0);
        assert!(wire.get("launcher_code").is_none());
        assert!(
            diagnostic
                .identify_launcher(&Process { pid: 42, start: 18 })
                .is_err()
        );
    }

    #[test]
    fn launcher_success_is_distinct_from_confirmed_server_lifetime() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_IDENTITY_DIAGNOSTIC_TEST";
        if std::env::var_os(STAGE).is_none() {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "chat::native_holder::transport::tests::launcher_success_is_distinct_from_confirmed_server_lifetime",
                    "--nocapture",
                ])
                .env(STAGE, "1")
                .env("XDG_RUNTIME_DIR", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .env("XDG_CACHE_HOME", directory.path().join("cache"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let working = tempfile::tempdir_in("/var/tmp").unwrap();
        for name in ["launcher-gate", "server-gate"] {
            use std::os::unix::ffi::OsStrExt;
            let path =
                std::ffi::CString::new(working.path().join(name).as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let mut source = super::super::tests::recovery().source;
        let boot = crate::daemon::read_boot_id().unwrap();
        source.proof.as_mut().unwrap().boot_id = boot.clone();
        source
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = boot;
        // The launcher and confirmed server are distinct shell processes.
        // FIFOs retain both until the test has inspected each phase; no sleep
        // or unrelated long-lived tool supplies the server's identity.
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::OwnedUnixWebSocket {
                socket: source.owned_codex_listener.as_ref().unwrap().socket.clone(),
            },
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                r#"/bin/sh -c 'printf "%s\n" "$$" > "$1/server.pid"; IFS= read -r gate < "$1/server-gate"; printf "confirmed server failure\n" >&2; exit 7' server "$1" &
IFS= read -r gate < "$1/launcher-gate"
exit 0"#.into(),
                "launcher".into(),
                working.path().to_string_lossy().into_owned(),
            ],
            working_directory: working.path().to_owned(),
            environment: Default::default(),
        };
        let mut holder = spawn(spec, source).unwrap();
        let launcher = holder.identity.record.root.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        let server = loop {
            if let Ok(pid) = std::fs::read_to_string(working.path().join("server.pid"))
                && let Ok(pid) = pid.trim().parse()
                && let Ok(server) = Process::read(pid)
            {
                break server;
            }
            assert!(Instant::now() < deadline, "fixture server did not start");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_ne!(server, launcher);
        holder
            .confirm(
                uuid::Uuid::new_v4().to_string(),
                super::super::tests::server_identity(),
                server.clone(),
            )
            .unwrap();
        let initial = holder.diagnostic().unwrap();
        assert_eq!(initial.launcher.as_ref(), Some(&launcher));
        assert_eq!(initial.launcher_code, None);
        assert_eq!(initial.launcher_signal, None);
        let observed = initial.confirmed_server.unwrap();
        assert_eq!(observed.process, server);
        assert!(observed.identity_current);

        let release = |name| {
            use std::os::unix::fs::OpenOptionsExt;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                    .open(working.path().join(name))
                {
                    Ok(mut writer) => {
                        writer.write_all(b"continue\n").unwrap();
                        break;
                    }
                    Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                        assert!(
                            Instant::now() < deadline,
                            "fixture gate never opened: {name}"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture gate {name}: {error}"),
                }
            }
        };
        release("launcher-gate");
        let deadline = Instant::now() + Duration::from_secs(5);
        let diagnostic = loop {
            let diagnostic = holder.diagnostic().unwrap();
            if diagnostic.launcher_code == Some(0) {
                break diagnostic;
            }
            assert!(Instant::now() < deadline, "fixture launcher did not exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(diagnostic.launcher.as_ref(), Some(&launcher));
        assert_eq!(diagnostic.launcher_signal, None);
        let observed = diagnostic.confirmed_server.unwrap();
        assert_eq!(observed.process, server);
        assert!(
            observed.identity_current,
            "launcher exit is not server exit"
        );
        assert!(server.is_current());

        release("server-gate");
        let deadline = Instant::now() + Duration::from_secs(5);
        let diagnostic = loop {
            let diagnostic = holder.diagnostic().unwrap();
            if diagnostic
                .confirmed_server
                .as_ref()
                .is_some_and(|server| !server.identity_current)
            {
                break diagnostic;
            }
            assert!(Instant::now() < deadline, "fixture server was not reaped");
            std::thread::sleep(Duration::from_millis(5));
        };
        // The confirmed server exited 7, but its reaped status is unavailable.
        // The only retained code remains explicitly attributed to the launcher.
        assert_eq!(diagnostic.launcher_code, Some(0));
        assert_eq!(diagnostic.launcher_signal, None);
        assert_eq!(diagnostic.launcher.as_ref(), Some(&launcher));
        assert!(diagnostic.stderr.contains("confirmed server failure\n"));
        let wire = serde_json::to_vec(&Report::Diagnostic(diagnostic)).unwrap();
        let Report::Diagnostic(decoded) = serde_json::from_slice(&wire).unwrap() else {
            panic!("missing diagnostic control response");
        };
        let observed = decoded.confirmed_server.unwrap();
        assert_eq!(observed.process, server);
        assert!(!observed.identity_current);
        assert_eq!(decoded.launcher_code, Some(0));
        holder.close().unwrap();
        assert!(!server.is_current());
        assert!(!launcher.is_current());
    }

    #[test]
    fn holder_reports_exit_status_and_captured_stderr_over_control() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_STDERR_TEST";
        if std::env::var_os(STAGE).is_none() {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::native_holder::transport::tests::holder_reports_exit_status_and_captured_stderr_over_control", "--nocapture"])
                .env(STAGE,"1").env("XDG_RUNTIME_DIR",directory.path())
                .env("XDG_DATA_HOME",directory.path().join("data"))
                .env("XDG_CACHE_HOME",directory.path().join("cache"))
                .env("CAGENTS_LOCAL_DEBUG","1").output().unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut source = super::super::tests::recovery().source;
        let boot = crate::daemon::read_boot_id().unwrap();
        source.proof.as_mut().unwrap().boot_id = boot.clone();
        source
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = boot;
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::OwnedUnixWebSocket {
                socket: source.owned_codex_listener.as_ref().unwrap().socket.clone(),
            },
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                "printf 'first diagnostic\nlast failure\n' >&2; exit 3".into(),
            ],
            working_directory: std::env::temp_dir(),
            environment: Default::default(),
        };
        let mut holder = spawn(spec, source).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let diagnostic = loop {
            let diagnostic = holder.diagnostic().unwrap();
            if diagnostic.launcher_code.is_some() || diagnostic.launcher_signal.is_some() {
                break diagnostic;
            }
            assert!(Instant::now() < deadline, "fixture provider did not exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(diagnostic.launcher_code, Some(3));
        assert_eq!(diagnostic.launcher_signal, None);
        assert_eq!(
            diagnostic.launcher.as_ref(),
            Some(&holder.identity.record.root)
        );
        assert!(diagnostic.confirmed_server.is_none());
        assert_eq!(diagnostic.stderr, "first diagnostic\nlast failure\n");
        assert_eq!(
            std::fs::read_to_string(diagnostic.log_path.as_ref().unwrap()).unwrap(),
            diagnostic.stderr
        );
        let frame = serde_json::to_vec(&Report::Diagnostic(diagnostic)).unwrap();
        let Report::Diagnostic(decoded) = serde_json::from_slice(&frame).unwrap() else {
            panic!("missing diagnostic control response")
        };
        assert_eq!(decoded.launcher_code, Some(3));
        assert!(decoded.stderr.ends_with("last failure\n"));
        holder.close().unwrap();
    }

    #[test]
    fn idle_control_and_orphan_listener_reap_tools_without_reaping_or_killing_provider() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_IDLE_REAPER_TEST";
        if std::env::var_os(STAGE).is_none() {
            let mut fixture = Command::new(std::env::current_exe().unwrap());
            fixture.args([
                "--exact",
                "chat::native_holder::transport::tests::idle_control_and_orphan_listener_reap_tools_without_reaping_or_killing_provider",
                "--nocapture",
            ]).env(STAGE, "1");
            // All libtest threads must inherit the blocked signal. Blocking
            // only this test thread would let the harness consume SIGCHLD.
            unsafe {
                fixture.pre_exec(|| {
                    let mut mask = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGCHLD);
                    if libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let output = fixture.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        // Isolate subreaper state and wait ownership from the test harness's
        // other tests. The fixture launcher exits without waiting for its tools.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let child_events = ChildEvents::new().unwrap();
        for orphan in [false, true] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let child = Command::new("/bin/sh")
                .args([
                    "-c",
                    "sleep 60 & echo $! > \"$1/live\"; sleep 0.2 & echo $! > \"$1/tool\"",
                    "fixture",
                ])
                .arg(directory.path())
                .process_group(0)
                .spawn()
                .unwrap();
            let mut server = Server(crate::daemon::owned_children::OwnedChildren::new(child));
            let deadline = Instant::now() + Duration::from_secs(3);
            while !server.exited() {
                assert!(Instant::now() < deadline, "fixture launcher did not exit");
                std::thread::sleep(Duration::from_millis(5));
            }
            let read_pid = |name| {
                std::fs::read_to_string(directory.path().join(name))
                    .unwrap()
                    .trim()
                    .parse::<i32>()
                    .unwrap()
            };
            let live = Process::read(read_pid("live")).unwrap();
            let tool = read_pid("tool");
            let tool_identity = Process::read(tool).unwrap();

            let observed_tool = tool_identity.clone();
            if orphan {
                let path = directory.path().join("control.sock");
                let listener = UnixListener::bind(&path).unwrap();
                let connector = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(500));
                    let reaped = !observed_tool.is_current();
                    let stream = UnixStream::connect(path).unwrap();
                    assert!(reaped, "orphan was not reaped on SIGCHLD");
                    stream
                });
                wait_for_control(listener.as_raw_fd(), &server, &child_events).unwrap();
                let _peer = connector.join().unwrap();
                let _accepted = listener.accept().unwrap();
            } else {
                let (reader, mut writer) = UnixStream::pair().unwrap();
                let sender = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(500));
                    assert!(
                        !observed_tool.is_current(),
                        "tool was not reaped on SIGCHLD"
                    );
                    writer.write_all(b"x").unwrap();
                });
                wait_for_control(reader.as_raw_fd(), &server, &child_events).unwrap();
                sender.join().unwrap();
            }

            assert!(server.exited(), "provider leader status was consumed");
            assert!(live.is_current(), "idle reaping killed a live tool");
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        live.pid as u32,
                        &mut status,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                },
                0
            );
            assert_eq!(unsafe { status.si_pid() }, 0, "live tool exited");
            assert_eq!(
                unsafe { libc::waitpid(tool, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            assert!(!tool_identity.is_current());
            assert!(server.terminate(Duration::from_secs(2)).unwrap());
            assert!(!live.is_current());
        }
    }

    #[test]
    fn spawned_owner_keeps_exact_server_on_failed_adoption_then_acknowledges_close() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_TRANSPORT_TEST";
        if std::env::var_os(STAGE).is_none() {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::native_holder::transport::tests::spawned_owner_keeps_exact_server_on_failed_adoption_then_acknowledges_close", "--nocapture"])
                .env(STAGE, "1").env("XDG_RUNTIME_DIR", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut source = super::super::tests::recovery().source;
        let boot = crate::daemon::read_boot_id().unwrap();
        source.proof.as_mut().unwrap().boot_id = boot.clone();
        source
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = boot;
        let socket = source.owned_codex_listener.as_ref().unwrap().socket.clone();
        let working = tempfile::tempdir_in("/var/tmp").unwrap();
        let context = std::sync::Arc::new(crate::chat::protocol::ExecutionContext::new(Some(
            std::sync::Arc::new(std::fs::File::open(working.path()).unwrap()),
        )));
        let spec = ProcessSpec {
            execution: Some(context.clone()),
            messages: None,
            transport: TransportSpec::OwnedUnixWebSocket { socket },
            program: "/bin/sleep".into(),
            arguments: vec!["60".into()],
            working_directory: working.path().to_owned(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let first = source.proof.clone().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &first).unwrap();
        let mut holder = spawn(spec.clone(), source.clone()).unwrap();
        assert!(
            context.outcome() == LaunchOutcome::Uncertain,
            "holder creation owns the launch before acknowledgment"
        );
        context.installed(); // The fixture now owns the returned Holder.
        let record = registry::evidence(
            &registry::socket(&registry::directory(Kind::Native).unwrap(), source.id),
            Kind::Native,
        )
        .unwrap()
        .unwrap();
        assert!(record.root.is_current());
        crate::daemon::process_scope::tests::assert_separate(
            record.holder.pid as u32,
            record.root.pid as u32,
        );
        assert_eq!(
            std::fs::read_link(format!("/proc/{}/cwd", record.root.pid)).unwrap(),
            working.path()
        );
        let records = registry::discover(
            &registry::directory(Kind::Native).unwrap(),
            &first.boot_id,
            Kind::Native,
        )
        .unwrap();
        assert_eq!(
            records,
            vec![record.clone()],
            "discovery precedes startup acknowledgment"
        );
        let thread = uuid::Uuid::new_v4().to_string();
        holder
            .confirm(
                thread.clone(),
                super::super::tests::server_identity(),
                record.root.clone(),
            )
            .unwrap();
        holder.preserve();
        drop(lease);
        let mut successor = first;
        successor.host_incarnation = crate::model::HostIncarnation::new("native-successor");
        assert!(
            inspect(&record, &successor).is_err(),
            "same UID without daemon lease is insufficient"
        );
        let _lease = crate::daemon::HostLease::acquire(&paths, &successor).unwrap();
        let (saved, exited) = inspect(&record, &successor).unwrap();
        assert!(!exited);
        assert_eq!(saved.confirmed_thread.as_ref(), Some(&thread));
        let (uncommitted, _) = adopt(&record, &source, &successor).unwrap();
        drop(uncommitted); // Simulate a failed local actor allocation.
        assert!(record.root.is_current());
        let (adopted, saved) = adopt(&record, &source, &successor).unwrap();
        assert_eq!(saved.confirmed_thread.as_ref(), Some(&thread));
        let detached = adopted.preserve();
        assert!(record.root.is_current());
        let mut stale = detached.clone();
        stale.proof.host_incarnation = crate::model::HostIncarnation::new("stale-owner");
        assert!(stale.close().is_err());
        assert!(record.root.is_current());
        detached.close().unwrap();
        assert!(!record.root.is_current());
        assert!(
            registry::discover(
                &registry::directory(Kind::Native).unwrap(),
                &successor.boot_id,
                Kind::Native
            )
            .unwrap()
            .is_empty()
        );
        // Teardown may finish after the daemon loses the Release reply. Its
        // exact durable receipt must permit retry without guessing from EOF.
        source.proof = Some(successor);
        let mut lost_reply = spawn(spec.clone(), source.clone()).unwrap();
        context.installed();
        let root = lost_reply.identity.record.root.clone();
        let control = lost_reply.control.as_ref().unwrap();
        control.shutdown(std::net::Shutdown::Read).unwrap();
        send(control, &Request::Release, TIMEOUT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if lost_reply.close().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "lost Release reply discarded cleanup evidence"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.is_current());
        assert!(!lost_reply.identity.consume_receipt().unwrap());
        // Simulate losing local startup installation after the helper created
        // its record. The startup owner consumes its own late receipt too.
        let mut uninstalled = spawn(spec, source).unwrap();
        let root = uninstalled.identity.record.root.clone();
        uninstalled.release_on_drop = false;
        drop(uninstalled);
        let deadline = Instant::now() + Duration::from_secs(3);
        while context.reconcile_cleanup().unwrap() == LaunchOutcome::Uncertain {
            assert!(
                Instant::now() < deadline,
                "startup cleanup never became proved"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.is_current());
        assert!(
            std::fs::read_dir(registry::directory(Kind::Native).unwrap())
                .unwrap()
                .all(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "closed"))
        );
    }

    #[test]
    fn lost_startup_connection_remains_uncertain_without_cleanup_receipt() {
        let (control, peer) = UnixStream::pair().unwrap();
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(LaunchOutcome::Uncertain));
        let owner = StartupOwner {
            control,
            child: std::sync::Mutex::new(None),
            outcome: outcome.clone(),
            receipt: Default::default(),
            retiring: Default::default(),
        };
        drop(peer);
        assert_eq!(owner.cleanup().unwrap(), LaunchOutcome::Uncertain);
        assert_eq!(owner.cleanup().unwrap(), LaunchOutcome::Uncertain);
        // A late exact reaper receipt resolves the same retained owner.
        *outcome.lock().unwrap() = LaunchOutcome::TeardownProved;
        assert_eq!(owner.cleanup().unwrap(), LaunchOutcome::TeardownProved);
    }

    #[test]
    fn failed_provider_spawn_retains_a_proven_cleanup_receipt() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_FAILED_SPAWN_TEST";
        if std::env::var_os(STAGE).is_none() {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::native_holder::transport::tests::failed_provider_spawn_retains_a_proven_cleanup_receipt", "--nocapture"])
                .env(STAGE, "1")
                .env("XDG_RUNTIME_DIR", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut source = super::super::tests::recovery().source;
        let boot = crate::daemon::read_boot_id().unwrap();
        source.proof.as_mut().unwrap().boot_id = boot.clone();
        source
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = boot;
        let context = std::sync::Arc::new(crate::chat::protocol::ExecutionContext::default());
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let spec = ProcessSpec {
            execution: Some(context.clone()),
            messages: None,
            transport: TransportSpec::OwnedUnixWebSocket {
                socket: source.owned_codex_listener.as_ref().unwrap().socket.clone(),
            },
            program: directory
                .path()
                .join("missing-provider")
                .to_string_lossy()
                .into_owned(),
            arguments: vec![],
            working_directory: directory.path().to_owned(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        assert!(spawn(spec, source).is_err());
        let deadline = Instant::now() + Duration::from_secs(3);
        while context.reconcile_cleanup().unwrap() == LaunchOutcome::Uncertain {
            assert!(
                Instant::now() < deadline,
                "failed startup lost its exact cleanup owner"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(context.outcome(), LaunchOutcome::TeardownProved);
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
        assert!(
            registry::discover(
                &registry::directory(Kind::Native).unwrap(),
                &crate::daemon::read_boot_id().unwrap(),
                Kind::Native,
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn partial_control_frames_obey_one_deadline_and_reject_oversized_allocation() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(&10_u32.to_be_bytes()).unwrap();
        writer.write_all(b"{").unwrap();
        let began = Instant::now();
        assert!(receive::<Request>(&reader, Duration::from_millis(30)).is_err());
        assert!(began.elapsed() < Duration::from_secs(1));
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .unwrap();
        assert!(
            receive::<Request>(&reader, TIMEOUT)
                .err()
                .unwrap()
                .contains("length")
        );
    }

    #[test]
    fn subreaper_close_reaps_tools_that_left_the_original_group() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_SUBREAPER_TEST";
        if std::env::var_os(STAGE).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::native_holder::transport::tests::subreaper_close_reaps_tools_that_left_the_original_group", "--nocapture"])
                .env(STAGE, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "setsid sh -c 'echo $$ > \"$1\"; exec sleep 60' fixture \"$1\" & wait",
                "fixture",
            ])
            .arg(&ready)
            .process_group(0)
            .spawn()
            .unwrap();
        let mut server = Server(crate::daemon::owned_children::OwnedChildren::new(child));
        let deadline = Instant::now() + Duration::from_secs(3);
        let descendant = loop {
            if let Some(pid) = std::fs::read_to_string(&ready)
                .ok()
                .and_then(|raw| raw.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "provider tool never became ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let identity = Process::read(descendant).unwrap();
        assert!(server.terminate(Duration::from_secs(2)).unwrap());
        assert!(!identity.is_current());
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) }, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
