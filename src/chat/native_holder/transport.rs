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
        fd::{AsFd, AsRawFd, FromRawFd, RawFd},
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
const PROTOCOL: u32 = 3;
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
            .stderr(Stdio::null())
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
        #[serde(default)]
        application: Option<String>,
    },
    PinnedLaunchDirectory,
    Release,
}

#[derive(Serialize, Deserialize)]
enum Report {
    Started {
        protocol: u32,
        record: Record,
        #[serde(default)]
        directory_pin: bool,
    },
    Confirmed,
    SourceUpdated,
    Released,
    Inspected {
        protocol: u32,
        record: Record,
        recovery: Recovery,
        exited: bool,
        #[serde(default)]
        directory_pin: bool,
    },
    Adopted(Recovery),
    Failed(String),
}

/// Dropping an intentionally closed actor releases its server. An unexpected
/// daemon exit instead closes the socket without sending Release.
pub(crate) struct Holder {
    control: Option<UnixStream>,
    release_on_drop: bool,
    durable: bool,
    directory_pin: bool,
    control_peer: Process,
    control_poisoned: bool,
    identity: super::Detached,
}

impl Holder {
    pub(crate) fn pinned_launch_directory(
        &mut self,
    ) -> Result<
        (
            crate::agent_execution::DirectoryIdentity,
            std::sync::Arc<std::fs::File>,
        ),
        String,
    > {
        if self.control_poisoned {
            return Err(
                "Native holder control framing is uncertain; only exact cleanup is available"
                    .into(),
            );
        }
        if !self.directory_pin {
            return Err(
                "This native holder does not support original launch-directory descriptors".into(),
            );
        }
        if !self.identity.record.holder.is_current() {
            return Err("Original native holder is no longer live".into());
        }
        let control = self.control.as_ref().ok_or("Native holder is detached")?;
        let credentials = peer(control)?;
        if credentials.uid != crate::daemon::current_uid()
            || credentials.pid != self.control_peer.pid
            || !self.control_peer.is_current()
        {
            return Err("Native holder directory endpoint belongs to another process".into());
        }
        let deadline = Instant::now() + TIMEOUT;
        let exchange = (|| {
            crate::descriptor_frame::send_json_with_fds_until(
                control,
                &Request::PinnedLaunchDirectory,
                &[],
                deadline,
                MAX_FRAME,
                0,
                crate::descriptor_frame::FrameByteOrder::BigEndian,
            )?;
            crate::descriptor_frame::receive_json_with_fds_until::<
                Result<crate::agent_execution::DirectoryIdentity, String>,
            >(
                control,
                deadline,
                MAX_FRAME,
                1,
                crate::descriptor_frame::FrameByteOrder::BigEndian,
            )
        })();
        let (response, mut descriptors) = match exchange {
            Ok(response) => response,
            Err(error) => {
                // Keep the original provider owned. A partial frame cannot be
                // reused for another lifecycle acknowledgement; explicit Close
                // reconciles through the existing exact detached-owner path.
                self.control_poisoned = true;
                return Err(error);
            }
        };
        let identity = response?;
        if descriptors.len() != 1 || !self.identity.record.holder.is_current() {
            return Err(
                "Native holder did not transfer one live original directory descriptor".into(),
            );
        }
        let directory = std::sync::Arc::new(std::fs::File::from(descriptors.remove(0)));
        crate::chat::protocol::validate_launch_directory(&directory, identity)?;
        Ok((identity, directory))
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
        if self.control_poisoned {
            return Err(
                "Native holder control framing is uncertain; only exact cleanup is available"
                    .into(),
            );
        }
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
        if self.control_poisoned {
            return Err(
                "Native holder control framing is uncertain; only exact cleanup is available"
                    .into(),
            );
        }
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
        if self.control_poisoned {
            self.control.take();
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

#[cfg(test)]
pub(crate) fn spawn(spec: ProcessSpec, source: Session) -> Result<Holder, String> {
    // Existing holder fixtures model legacy holders with no goal scope. The
    // production actor supplies its once-captured original tuple explicitly.
    spawn_with_account_scope(spec, source, None)
}

pub(crate) fn spawn_with_account_scope(
    mut spec: ProcessSpec,
    source: Session,
    goal_account_scope: Option<crate::account::scope::LaunchAccountScope>,
) -> Result<Holder, String> {
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
        goal_account_scope,
    };
    recovery.validate(&crate::daemon::read_boot_id().map_err(|error| error.to_string())?)?;
    let source_proof = retained_source
        .proof
        .clone()
        .ok_or("Native holder source has no daemon proof")?;
    let execution = spec.execution.clone();
    let original = execution
        .as_ref()
        .map(|context| {
            let descriptor = context.directory_descriptor();
            if descriptor.is_none() && context.directory_identity()?.is_some() {
                return Err(
                    "Admitted native context has no original directory descriptor".to_owned(),
                );
            }
            Ok(descriptor)
        })
        .transpose()?
        .flatten();
    let directory = match original {
        Some(directory) => directory,
        None => {
            use std::os::unix::fs::OpenOptionsExt;
            std::sync::Arc::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
                    .open(&spec.working_directory)
                    .map_err(|error| error.to_string())?,
            )
        }
    };
    let directory_fd = Some(directory.as_raw_fd());
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
    // SO_PEERCRED on a socketpair remains its creator, including after exec.
    let control_peer =
        Process::read(std::process::id() as i32).map_err(|error| error.to_string())?;
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
                directory_pin,
            }) if protocol == PROTOCOL
                && record.holder.pid == pid
                && record.holder.is_current() =>
            {
                crate::daemon::process_scope::warn_if_shared(
                    record.holder.pid as u32,
                    record.root.pid as u32,
                );
                Ok((record, directory_pin))
            }
            Some(Report::Failed(error)) => Err(error),
            _ => Err("Native holder did not acknowledge its server".into()),
        }
    })();
    match result {
        Ok((record, directory_pin)) => Ok(Holder {
            control: Some(control),
            release_on_drop: true,
            durable: false,
            directory_pin,
            control_peer,
            control_poisoned: false,
            identity: super::Detached {
                record,
                proof: source_proof.clone(),
                source: retained_source,
                original_role: None,
                receipt_proof: source_proof.clone(),
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
    if registry::recovery_candidate(&socket, Kind::Native)?.as_ref() != Some(record)
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
    inspect_with_directory_pin(record, proof).map(|(recovery, exited, _)| (recovery, exited))
}

fn inspect_with_directory_pin(
    record: &Record,
    proof: &HostSessionProof,
) -> Result<(Recovery, bool, bool), String> {
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
            directory_pin,
        }) if matches!(protocol, 2 | PROTOCOL) && actual == *record => {
            recovery.validate_original_role(&record.boot, &record.application)?;
            Ok((recovery, exited, directory_pin))
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
    // Inspect is read-only. A candidate role does not authorize Adopt: first
    // authenticate this live holder's complete issued source on its own lease-
    // checked channel, and retain the exact original Record through cleanup.
    let (original, _, directory_pin) = inspect_with_directory_pin(record, proof)?;
    if &original.source != source && !original.previous_sources.contains(source) {
        return Err("Native role transition does not recognize the exact issued source".into());
    }
    let original_role = registry::OriginalRole::authenticated(
        record,
        source.proof.as_ref().ok_or("Native source has no proof")?,
        proof,
    )?;
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
            application: Some(crate::instance::APPLICATION_ID.into()),
        },
        TIMEOUT,
    )?;
    match receive(&control, TIMEOUT)? {
        Some(Report::Adopted(recovery)) => {
            recovery.validate_original_role(&record.boot, &record.application)?;
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
                    directory_pin,
                    control_peer: record.holder.clone(),
                    control_poisoned: false,
                    identity: super::Detached {
                        record: record.clone(),
                        source: recovery.source.clone(),
                        proof: proof.clone(),
                        original_role,
                        receipt_proof: proof.clone(),
                    },
                },
                recovery,
            ))
        }
        Some(Report::Failed(error)) => Err(error),
        _ => Err("Native holder did not acknowledge adoption".into()),
    }
}

/// Bind the exact already-validated provider incarnation, never its cwd path.
/// An exited zombie can still pass Process::is_current; only the owned pidfd
/// establishes the kernel-live observation used by a directory-pin reply.
fn pin_provider_process(process: &Process) -> Result<std::fs::File, String> {
    if !process.is_current() {
        return Err("Original native provider identity changed".into());
    }
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
    if descriptor < 0 {
        return Err(format!(
            "Pin original native provider: {}",
            io::Error::last_os_error()
        ));
    }
    let descriptor = unsafe { std::fs::File::from_raw_fd(descriptor as RawFd) };
    if !process.is_current() {
        return Err("Original native provider identity changed during pinning".into());
    }
    Ok(descriptor)
}

fn ensure_provider_live(pidfd: &std::fs::File) -> Result<(), String> {
    let mut event = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    match unsafe { libc::poll(&mut event, 1, 0) } {
        0 if event.revents == 0 => Ok(()),
        result if result < 0 => Err(format!(
            "Observe original native provider: {}",
            io::Error::last_os_error()
        )),
        _ => Err("Original native provider has exited or its observation is unavailable".into()),
    }
}

struct Residence {
    directory: PathBuf,
    listener: Listener,
    record: Record,
    recovery: Recovery,
    child: Server,
    child_events: ChildEvents,
    released: bool,
    _working_directory: Option<std::fs::File>,
    provider_pidfd: std::fs::File,
    owner_application: String,
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
        let child = match spawn.command().spawn() {
            Ok(child) => Server(crate::daemon::owned_children::OwnedChildren::new(child)),
            Err(error) => {
                return Err(error.to_string());
            }
        };
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
        let provider_pidfd = pin_provider_process(&record.root)?;
        let residence = Self {
            provider_pidfd,
            owner_application: crate::instance::APPLICATION_ID.into(),
            directory,
            listener,
            record,
            recovery: spawn.recovery,
            child,
            child_events,
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

    fn pinned_launch_directory(
        &self,
    ) -> Result<(crate::agent_execution::DirectoryIdentity, &std::fs::File), String> {
        if self.released {
            return Err("Original native provider was released".into());
        }
        ensure_provider_live(&self.provider_pidfd)?;
        let directory = self
            ._working_directory
            .as_ref()
            .ok_or("Native provider has no original launch-directory descriptor")?;
        let identity = crate::agent_execution::DirectoryIdentity::from_metadata(
            &directory.metadata().map_err(|error| error.to_string())?,
        );
        crate::chat::protocol::validate_launch_directory(directory, identity)?;
        Ok((identity, directory))
    }
    fn attend(&mut self, control: UnixStream) -> bool {
        loop {
            if wait_for_control(control.as_raw_fd(), &self.child, &self.child_events).is_err() {
                return self.recovery.is_durable();
            }
            let request = match receive(&control, TIMEOUT) {
                Ok(Some(request)) => request,
                _ => return self.recovery.is_durable(),
            };
            if matches!(&request, Request::PinnedLaunchDirectory) {
                let pin = self.pinned_launch_directory();
                let response = pin
                    .as_ref()
                    .map(|(identity, _)| *identity)
                    .map_err(Clone::clone);
                let descriptors: Vec<_> = pin
                    .as_ref()
                    .ok()
                    .map(|(_, file)| vec![file.as_fd()])
                    .unwrap_or_default();
                if crate::descriptor_frame::send_json_with_fds_until(
                    &control,
                    &response,
                    &descriptors,
                    Instant::now() + TIMEOUT,
                    MAX_FRAME,
                    1,
                    crate::descriptor_frame::FrameByteOrder::BigEndian,
                )
                .is_err()
                {
                    return self.recovery.is_durable();
                }
                continue;
            }
            let response = match request {
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
                        .and_then(|()| pin_provider_process(&process))
                        .and_then(|pidfd| {
                            self.recovery.confirm(thread, identity, process)?;
                            self.provider_pidfd = pidfd;
                            Ok(())
                        }) {
                        Ok(()) => Report::Confirmed,
                        Err(error) => Report::Failed(error),
                    }
                }
                Request::UpdateSource {
                    source,
                    execution_console,
                    paired_holder,
                } => match self.recovery.update_source_for_role(
                    source,
                    execution_console,
                    paired_holder,
                    &self.owner_application,
                ) {
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
            if wait_for_control(
                self.listener.socket.as_raw_fd(),
                &self.child,
                &self.child_events,
            )
            .is_err()
            {
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
                            directory_pin: true,
                        },
                        TIMEOUT,
                    );
                }
                Request::Adopt {
                    source,
                    proof,
                    application,
                } => {
                    let application =
                        application.unwrap_or_else(|| self.record.application.clone());
                    if application != self.record.application
                        && !registry::production_role_pair(&self.record.application, &application)
                    {
                        let _ = send(
                            &control,
                            &Report::Failed("Unsupported original holder role transition".into()),
                            TIMEOUT,
                        );
                        continue;
                    }
                    match self.recovery.adopt(&source, proof) {
                        Ok(()) => {
                            self.owner_application = application;
                            if send(&control, &Report::Adopted(self.recovery.clone()), TIMEOUT)
                                .is_ok()
                            {
                                return self.attend(control);
                            }
                        }
                        Err(error) => {
                            let _ = send(&control, &Report::Failed(error), TIMEOUT);
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
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
fn wait_for_control(
    fd: RawFd,
    children: &crate::daemon::owned_children::OwnedChildren,
    child_events: &ChildEvents,
) -> io::Result<()> {
    wait_for_control_with_reaper(fd, child_events, || children.reap_exited_descendants())
}

fn wait_for_control_with_reaper(
    fd: RawFd,
    child_events: &ChildEvents,
    mut reap: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut reported = false;
    let mut inspect_children = || match reap() {
        Ok(()) => reported = false,
        Err(error) => {
            // Inspection grants no release authority. Treat the same transient
            // failure identically before poll and after a child wakeup, and
            // report a continuing failure only once until inspection recovers.
            if !reported {
                eprintln!("Native holder could not reap exited tools: {error}");
                reported = true;
            }
        }
    };
    loop {
        inspect_children();
        let mut poll = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: child_events.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: the array and both owned descriptors remain live throughout
        // poll. No timer is armed: an idle holder sleeps indefinitely.
        match unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, -1) } {
            result if result > 0 => {
                if poll[1].revents != 0 {
                    child_events.drain()?;
                    // A simultaneous control event must not postpone reaping.
                    inspect_children();
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
        let _ = crate::daemon::process_scope::enroll_holder();
        let mut residence = match Residence::start(spawn) {
            Ok(residence) => residence,
            Err(error) => {
                // Only startup has no Started response yet. Preserve the
                // original failure on its existing control channel; sending
                // diagnostics grants no ownership or cleanup authority.
                let _ = send(
                    &control,
                    &Report::Failed(format!("Native holder startup failed: {error}")),
                    START_TIMEOUT,
                );
                return Err(error);
            }
        };
        send(
            &control,
            &Report::Started {
                protocol: PROTOCOL,
                record: residence.record.clone(),
                directory_pin: true,
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
    #[test]
    fn legacy_adopt_json_keeps_exact_source_and_proof_without_claiming_a_new_role() {
        let source = super::super::tests::recovery().source;
        let proof = source.proof.clone().unwrap();
        let legacy = serde_json::json!({"Adopt": {"source": source, "proof": proof}});
        let decoded: Request = serde_json::from_value(legacy.clone()).unwrap();
        let Request::Adopt {
            source: actual,
            proof: actual_proof,
            application,
        } = decoded
        else {
            panic!("legacy request changed operation");
        };
        assert_eq!(actual, source);
        assert_eq!(actual_proof, proof);
        assert!(
            application.is_none(),
            "legacy request invented successor-role authority"
        );
        let mut current = legacy.clone();
        current["Adopt"]["application"] = serde_json::json!(crate::instance::APPLICATION_ID);
        let Request::Adopt {
            source: actual,
            proof: actual_proof,
            application,
        } = serde_json::from_value::<Request>(current).unwrap()
        else {
            panic!("current request changed operation");
        };
        assert_eq!(actual, source);
        assert_eq!(actual_proof, proof);
        assert_eq!(
            application.as_deref(),
            Some(crate::instance::APPLICATION_ID)
        );
        let mut missing_proof = legacy;
        missing_proof["Adopt"]
            .as_object_mut()
            .unwrap()
            .remove("proof");
        assert!(serde_json::from_value::<Request>(missing_proof).is_err());
    }

    use super::*;

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
    fn idle_reaping_preserves_a_live_provider_and_its_live_adopted_tool() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_LIVE_REAPER_TEST";
        if std::env::var_os(STAGE).is_none() {
            let mut fixture = Command::new(std::env::current_exe().unwrap());
            fixture
                .args([
                    "--exact",
                    "chat::native_holder::transport::tests::idle_reaping_preserves_a_live_provider_and_its_live_adopted_tool",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(STAGE, "1");
            // Isolate subreaper/wait ownership and block SIGCHLD in every
            // libtest thread, as in the existing idle-reaper fixture.
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
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let child_events = ChildEvents::new().unwrap();
        for orphan in [false, true] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            // The intermediate shell exits, adopting both tools into this
            // holder. The actual provider leader stays alive. Only the short
            // tool reads the test-owned pipe; no duration determines its exit.
            let script = r#"
while IFS=: read -r field value; do
    if [ "$field" = SigBlk ]; then printf '%s\n' "$value" > "$1/provider-mask"; fi
done < "/proc/$$/status"
/bin/sh -c '
    /bin/sleep 60 </dev/null &
    echo $! > "$1/live"
    /bin/sh -c "read -r token" <&0 &
    echo $! > "$1/tool"
' fixture "$1"
echo ready > "$1/ready"
exec /bin/sleep 60
"#;
            // Exercise the production pre-exec mask restoration, not a second
            // test-only unblocking implementation. Other Spawn fields are not
            // consulted by command(); no native registry/provider is opened.
            let spawn = Spawn {
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    script.into(),
                    "fixture".into(),
                    directory.path().to_str().unwrap().into(),
                ],
                directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
                recovery: super::super::tests::recovery(),
                directory_fd: None,
            };
            let mut child = spawn.command().stdin(Stdio::piped()).spawn().unwrap();
            let mut tool_gate = child.stdin.take().unwrap();
            let mut server = Server(crate::daemon::owned_children::OwnedChildren::new(child));
            let provider = Process::read(server.id()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !directory.path().join("ready").exists() {
                assert!(!server.exited(), "provider exited during fixture startup");
                assert!(Instant::now() < deadline, "provider fixture did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
            let read_pid = |name| {
                std::fs::read_to_string(directory.path().join(name))
                    .unwrap()
                    .trim()
                    .parse::<i32>()
                    .unwrap()
            };
            let tool = Process::read(read_pid("tool")).unwrap();
            let live = Process::read(read_pid("live")).unwrap();
            for process in [&tool, &live] {
                let stat = std::fs::read_to_string(format!("/proc/{}/stat", process.pid)).unwrap();
                let parent = stat
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                assert_eq!(parent.parse::<u32>().unwrap(), std::process::id());
            }
            let mask = std::fs::read_to_string(directory.path().join("provider-mask")).unwrap();
            let mask = u64::from_str_radix(mask.trim(), 16).unwrap();
            assert_eq!(mask & (1_u64 << (libc::SIGCHLD - 1)), 0);
            assert!(!server.exited(), "provider must be live before idle wait");

            let observed_tool = tool.clone();
            let release_and_observe = move || {
                let released = tool_gate.write_all(b"exit\n").is_ok();
                drop(tool_gate);
                if !released {
                    return false;
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                while observed_tool.is_current() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                !observed_tool.is_current()
            };
            let reaped = if orphan {
                let path = directory.path().join("control.sock");
                let listener = UnixListener::bind(&path).unwrap();
                let connector = std::thread::spawn(move || {
                    let reaped = release_and_observe();
                    // Always wake the real wait path, even on a failed oracle.
                    let stream = UnixStream::connect(path).unwrap();
                    (stream, reaped)
                });
                wait_for_control(listener.as_raw_fd(), &server, &child_events).unwrap();
                let (_peer, reaped) = connector.join().unwrap();
                let _accepted = listener.accept().unwrap();
                reaped
            } else {
                let (reader, mut writer) = UnixStream::pair().unwrap();
                let sender = std::thread::spawn(move || {
                    let reaped = release_and_observe();
                    writer.write_all(b"x").unwrap();
                    reaped
                });
                wait_for_control(reader.as_raw_fd(), &server, &child_events).unwrap();
                sender.join().unwrap()
            };
            assert!(
                reaped,
                "adopted tool was not reaped before control activity"
            );
            assert!(!server.exited(), "idle wait consumed the live provider");
            for process in [&provider, &live] {
                assert!(process.is_current(), "idle reaping changed a live process");
                let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe {
                        libc::waitid(
                            libc::P_PID,
                            process.pid as u32,
                            &mut status,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    },
                    0
                );
                assert_eq!(unsafe { status.si_pid() }, 0, "live process exited");
            }
            assert_eq!(
                unsafe { libc::waitpid(tool.pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            assert!(server.terminate(Duration::from_secs(2)).unwrap());
            assert!(!provider.is_current());
            assert!(!live.is_current());
            assert_eq!(
                unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }

    #[test]
    fn post_child_census_failure_preserves_live_ownership_and_services_control() {
        const STAGE: &str = "CAGENTS_NATIVE_HOLDER_CENSUS_FAILURE_TEST";
        if std::env::var_os(STAGE).is_none() {
            let mut fixture = Command::new(std::env::current_exe().unwrap());
            fixture
                .args([
                    "--exact",
                    "chat::native_holder::transport::tests::post_child_census_failure_preserves_live_ownership_and_services_control",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(STAGE, "1");
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
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let child_events = ChildEvents::new().unwrap();
        for orphan in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let spawn = Spawn {
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    r#"/bin/sh -c '
                        /bin/sleep 60 </dev/null & echo $! > "$1/live"
                        /bin/sh -c "read -r token" <&0 & echo $! > "$1/tool"
                    ' fixture "$1"
                    echo ready > "$1/ready"
                    exec /bin/sleep 60"#
                        .into(),
                    "fixture".into(),
                    directory.path().to_str().unwrap().into(),
                ],
                directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
                recovery: super::super::tests::recovery(),
                directory_fd: None,
            };
            let mut child = spawn.command().stdin(Stdio::piped()).spawn().unwrap();
            let gate = child.stdin.take().unwrap();
            let mut server = Server(crate::daemon::owned_children::OwnedChildren::new(child));
            let provider = Process::read(server.id()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !directory.path().join("ready").exists() {
                assert!(!server.exited());
                assert!(Instant::now() < deadline, "provider fixture did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
            let identity = |name| {
                let pid = std::fs::read_to_string(directory.path().join(name))
                    .unwrap()
                    .trim()
                    .parse::<i32>()
                    .unwrap();
                Process::read(pid).unwrap()
            };
            let tool = identity("tool");
            let live = identity("live");
            // Clear setup notifications before inducing the exact child-exit
            // boundary. The short tool still waits for this original pipe.
            child_events.drain().unwrap();
            let listener =
                orphan.then(|| UnixListener::bind(directory.path().join("control.sock")).unwrap());
            let (reader, peer) = if let Some(listener) = &listener {
                (
                    None,
                    UnixStream::connect(listener.local_addr().unwrap().as_pathname().unwrap())
                        .unwrap(),
                )
            } else {
                let (reader, peer) = UnixStream::pair().unwrap();
                (Some(reader), peer)
            };
            let fd = listener
                .as_ref()
                .map_or_else(|| reader.as_ref().unwrap().as_raw_fd(), AsRawFd::as_raw_fd);
            let proof = spawn.recovery.source.proof.clone().unwrap();
            send(
                &peer,
                &Request::Inspect {
                    proof: proof.clone(),
                },
                TIMEOUT,
            )
            .unwrap();
            let mut calls = 0;
            let mut released = false;
            let mut observed_exit = false;
            let mut gate = Some(gate);
            let waited = wait_for_control_with_reaper(fd, &child_events, || {
                calls += 1;
                if calls == 1 {
                    server.reap_exited_descendants()?;
                    let mut original = gate.take().unwrap();
                    released = original.write_all(b"exit\n").is_ok();
                    drop(original);
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while released && Instant::now() < deadline {
                        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                        let result = unsafe {
                            libc::waitid(
                                libc::P_PID,
                                tool.pid as u32,
                                &mut status,
                                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                            )
                        };
                        if result != 0 {
                            break;
                        }
                        if unsafe { status.si_pid() } == tool.pid {
                            observed_exit = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return Ok(());
                }
                if calls == 2 {
                    // This is the post-SIGCHLD census, never a control,
                    // authentication, child-event FD or teardown failure.
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                server.reap_exited_descendants()
            });
            assert!(
                waited.is_ok(),
                "inspection failure abandoned control: {waited:?}"
            );
            assert!(released && observed_exit);
            assert_eq!(calls, 2, "fault must occur after the real SIGCHLD wake");
            let control = if let Some(listener) = listener {
                listener.accept().unwrap().0
            } else {
                reader.unwrap()
            };
            let Some(Request::Inspect { proof: actual }) = receive(&control, TIMEOUT).unwrap()
            else {
                panic!("the original framed control request was lost");
            };
            assert_eq!(actual, proof);
            let mut stopped: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        tool.pid as u32,
                        &mut stopped,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                },
                0
            );
            assert_eq!(unsafe { stopped.si_pid() }, tool.pid);
            assert!(!server.exited(), "inspection failure consumed the provider");
            for process in [&provider, &live] {
                assert!(
                    process.is_current(),
                    "inspection failure changed a live owner"
                );
                let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe {
                        libc::waitid(
                            libc::P_PID,
                            process.pid as u32,
                            &mut status,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    },
                    0
                );
                assert_eq!(unsafe { status.si_pid() }, 0);
            }
            // The next genuine control wake retries ordinary reaping, without
            // replacing any owner or manufacturing a completion receipt.
            send(&peer, &Request::Inspect { proof }, TIMEOUT).unwrap();
            wait_for_control(control.as_raw_fd(), &server, &child_events).unwrap();
            assert!(matches!(
                receive(&control, TIMEOUT).unwrap(),
                Some(Request::Inspect { .. })
            ));
            assert_eq!(
                unsafe { libc::waitpid(tool.pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            assert!(!tool.is_current());
            assert!(provider.is_current() && live.is_current());
            assert!(server.terminate(Duration::from_secs(2)).unwrap());
            assert!(!provider.is_current() && !live.is_current());
            assert_eq!(
                unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
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
    fn original_directory_pin_survives_native_adoption_and_path_replacement() {
        const STAGE: &str = "CAGENTS_NATIVE_DIRECTORY_PIN_TEST";
        if std::env::var_os(STAGE).is_none() {
            let root = tempfile::tempdir().unwrap();
            let name = std::thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(STAGE, "1")
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data"))
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
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("marker"), "original").unwrap();
        std::fs::write(
            cwd.join("provider"),
            "#!/bin/sh\n/bin/cat marker > observed\nexec /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(cwd.join("provider"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        struct RestorePermissions([PathBuf; 2]);
        impl Drop for RestorePermissions {
            fn drop(&mut self) {
                for path in &self.0 {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
                }
            }
        }
        let _permissions = RestorePermissions([cwd.clone(), root.path().join("moved")]);
        // Existing executable/file access needs traversal, not list permission.
        std::fs::write(cwd.join("observed"), "").unwrap();
        std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o111)).unwrap();
        let expected = crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(&cwd).unwrap(),
        );
        let mut source = super::super::tests::recovery().source;
        let boot = crate::daemon::read_boot_id().unwrap();
        source.proof.as_mut().unwrap().boot_id = boot.clone();
        source
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = boot;
        source.launch.recipe.program = "./provider".into();
        source.launch.recipe.working_directory = cwd.clone();
        // The published source keeps its exact issued --remote ownership;
        // only the executable ProcessSpec below is the local test fixture.
        assert!(source.owns_codex_listener());
        let mut stripped = source.clone();
        stripped.launch.recipe.arguments.clear();
        assert!(!stripped.owns_codex_listener());
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            transport: TransportSpec::OwnedUnixWebSocket {
                socket: source.owned_codex_listener.as_ref().unwrap().socket.clone(),
            },
            program: "./provider".into(),
            arguments: Vec::new(),
            working_directory: cwd.clone(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let proof = source.proof.clone().unwrap();
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        assert_eq!(
            spawn(spec.clone(), stripped).err().as_deref(),
            Some("Native holder has inconsistent ownership evidence"),
            "a source without the issued listener must still fail before provider creation"
        );
        let mut holder = spawn(spec, source.clone()).unwrap();
        let record = holder.identity.record.clone();
        let deadline = Instant::now() + TIMEOUT;
        while std::fs::read_to_string(cwd.join("observed"))
            .ok()
            .as_deref()
            != Some("original")
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::rename(&cwd, root.path().join("moved")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("marker"), "replacement").unwrap();
        // Missing additive capability fields decode as unsupported for old peers.
        let mut legacy = serde_json::to_value(Report::Started {
            protocol: PROTOCOL,
            record: record.clone(),
            directory_pin: true,
        })
        .unwrap();
        legacy["Started"]
            .as_object_mut()
            .unwrap()
            .remove("directory_pin");
        assert!(matches!(
            serde_json::from_value::<Report>(legacy).unwrap(),
            Report::Started {
                directory_pin: false,
                ..
            }
        ));
        // A legacy capability refuses locally; the original control remains usable.
        holder.directory_pin = false;
        assert!(holder.pinned_launch_directory().is_err());
        holder.directory_pin = true;
        assert_eq!(holder.pinned_launch_directory().unwrap().0, expected);
        let thread = uuid::Uuid::new_v4().to_string();
        holder
            .confirm(
                thread.clone(),
                super::super::tests::server_identity(),
                record.root.clone(),
            )
            .unwrap();
        let detached = holder.preserve();
        struct Cleanup(super::super::Detached);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.close();
            }
        }
        let _cleanup = Cleanup(detached);
        let (mut adopted, _) = adopt(&record, &source, &proof).unwrap();
        let (identity, pin) = adopted.pinned_launch_directory().unwrap();
        assert_eq!(identity, expected);
        assert_ne!(
            unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) } & libc::O_PATH,
            0
        );
        let fd = unsafe {
            libc::openat(
                pin.as_raw_fd(),
                c"marker".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        assert!(fd >= 0);
        let mut text = String::new();
        unsafe { std::fs::File::from_raw_fd(fd) }
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "original");
        assert_eq!(
            std::fs::read_to_string(cwd.join("marker")).unwrap(),
            "replacement"
        );
        // Relay one real holder pin reply, then deliberately expose only a
        // partial header to the client. Timeout must poison this stream without
        // sending Release or losing the real provider's original owner.
        let (control, mut partial) = UnixStream::pair().unwrap();
        let original = adopted.control.replace(control).unwrap();
        adopted.control_peer = Process::read(std::process::id() as i32).unwrap();
        let (finish, release) = std::sync::mpsc::sync_channel(1);
        let relay = std::thread::spawn(move || {
            assert!(matches!(
                receive::<Request>(&partial, TIMEOUT).unwrap(),
                Some(Request::PinnedLaunchDirectory)
            ));
            send(&original, &Request::PinnedLaunchDirectory, TIMEOUT).unwrap();
            let (reply, rights) = crate::descriptor_frame::receive_json_with_fds_until::<
                Result<crate::agent_execution::DirectoryIdentity, String>,
            >(
                &original,
                Instant::now() + TIMEOUT,
                MAX_FRAME,
                1,
                crate::descriptor_frame::FrameByteOrder::BigEndian,
            )
            .unwrap();
            assert_eq!(reply.unwrap(), expected);
            assert_eq!(rights.len(), 1);
            partial.write_all(&20_u32.to_be_bytes()[..2]).unwrap();
            release
                .recv_timeout(TIMEOUT + Duration::from_secs(5))
                .unwrap();
            let mut byte = 0_u8;
            assert_eq!(
                unsafe {
                    libc::recv(
                        partial.as_raw_fd(),
                        (&raw mut byte).cast(),
                        1,
                        libc::MSG_DONTWAIT,
                    )
                },
                -1
            );
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
            // Dropping the original control preserves the confirmed holder.
        });
        assert!(adopted.pinned_launch_directory().is_err());
        assert!(adopted.control_poisoned);
        assert!(record.root.is_current());
        assert!(
            adopted
                .confirm(
                    thread,
                    super::super::tests::server_identity(),
                    record.root.clone()
                )
                .unwrap_err()
                .contains("framing")
        );
        assert!(
            adopted
                .update_source(source, None, None)
                .unwrap_err()
                .contains("framing")
        );
        finish.send(()).unwrap();
        relay.join().unwrap();
        assert!(record.root.is_current());
        adopted.close().unwrap();
        assert!(adopted.pinned_launch_directory().is_err());
        assert!(!record.root.is_current());
    }

    #[test]
    fn native_directory_pin_refuses_an_exited_original_before_reap() {
        const STAGE: &str = "CAGENTS_NATIVE_DIRECTORY_EXIT_TEST";
        if std::env::var_os(STAGE).is_none() {
            let root = tempfile::tempdir().unwrap();
            let name = std::thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(STAGE, "1")
                .env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data"))
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
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("finish");
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
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
            transport: TransportSpec::OwnedUnixWebSocket {
                socket: source.owned_codex_listener.as_ref().unwrap().socket.clone(),
            },
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read line < finish".into()],
            working_directory: directory.path().into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease =
            crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
        let mut holder = spawn(spec, source).unwrap();
        let root = holder.identity.record.root.clone();
        holder
            .confirm(
                uuid::Uuid::new_v4().to_string(),
                super::super::tests::server_identity(),
                root.clone(),
            )
            .unwrap();
        assert!(holder.pinned_launch_directory().is_ok());
        let original = pin_provider_process(&root).unwrap();
        ensure_provider_live(&original).unwrap();
        // Nonblocking opens only once the actual original shell is reading.
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = Instant::now() + TIMEOUT;
        let mut writer = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&fifo)
            {
                Ok(writer) => break writer,
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        writer.write_all(b"finish\n").unwrap();
        drop(writer);
        while ensure_provider_live(&original).is_ok() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            root.is_current(),
            "the original zombie is retained by its sole holder"
        );
        assert!(
            holder.pinned_launch_directory().is_err(),
            "process identity alone must not pass a kernel-exited root"
        );
        assert!(
            !holder.control_poisoned,
            "fully consumed semantic refusal is not a framing failure"
        );
        holder.close().unwrap();
        assert!(!root.is_current());
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
        let startup_error = spawn(spec, source)
            .err()
            .expect("the missing provider must fail in the actual helper");
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
        assert_eq!(
            startup_error,
            format!(
                "Native holder startup failed: {}",
                io::Error::from_raw_os_error(libc::ENOENT)
            ),
            "the real helper must expose its original spawn failure, not an EOF acknowledgement"
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
