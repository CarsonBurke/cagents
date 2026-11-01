//! Terminal holders: one small process per terminal that owns its PTY and its
//! root process.
//!
//! The daemon keeps the emulator, but a holder is the root's parent and keeps
//! the PTY master open, which is what lets a terminal's life stop depending on
//! the daemon's. The daemon starts a holder by executing its own image with
//! [`ARGUMENT`]. The holder opens the PTY, spawns the root on it, and hands back
//! the master and a pidfd for the root over its control socket. From then on
//! the daemon reads, writes, resizes and signals through those descriptors
//! directly, so no terminal byte is ever relayed. The holder only reaps the
//! root, reports its wait status, and waits to be released.
//!
//! A discoverable holder that loses its daemon buffers output and waits for a
//! successor. Explicit release still hangs up the terminal. Older anonymous
//! holders retain their original release-on-disconnect behavior.
//!
//! A daemon that is being replaced can instead [detach](Holder::detach) a
//! holder. The holder then keeps its terminal running, holds the output that
//! arrives meanwhile, and listens on a socket for the replacement to
//! [`adopt`] the terminal: same PTY, same root, nothing restarted.
//!
//! Control messages are length-prefixed JSON, so a field added later is
//! ignored by a holder that predates it, and descriptors travel as
//! `SCM_RIGHTS` on a message's first byte.

mod watch;
use watch::Watch;

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossbeam_channel::{Receiver, Sender};
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{ChildExit, close_processes::signal};
use crate::terminal::{
    SPAWN_START_TIMEOUT as START_TIMEOUT,
    holder::{self as registry, Record, Recovery},
};

/// The argument that makes an image serve as a holder. Deliberately not a
/// word a test filter or a person would type.
const ARGUMENT: &str = "--cagents-terminal-holder";
/// Protocol 1 holders could not be detached.
const PROTOCOL: u32 = 5;
/// The holders a daemon can adopt: any that speak detaching.
const ADOPTABLE: std::ops::RangeInclusive<u32> = 2..=PROTOCOL;
/// A spawn request carries the root's whole environment; nothing legitimate
/// comes near this.
const MAX_FRAME: u32 = 16 << 20;
/// Descriptors any one message may carry.
const MAX_DESCRIPTORS: usize = 4;
/// How long a released holder has to exit before it is killed. It exits as
/// soon as it reads the release, so this only bounds a wedged one.
const RELEASE_GRACE: Duration = Duration::from_millis(500);

/// What process listings show for a holder, within the kernel's 15 bytes.
const PROCESS_NAME: &std::ffi::CStr = c"cagents-holder";
/// Grace period for anonymous legacy handoffs. Registered holders retain
/// their live processes or completed result until explicit release.
const ADOPTION_GRACE: Duration = Duration::from_secs(15 * 60);
/// Output a detached holder keeps for its successor. Past this it stops
/// reading, so the terminal's writers block rather than lose anything.
const DETACHED_OUTPUT: usize = 1 << 20;
/// How long either side of a detach or an adoption waits on the other.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// The most screen a detached holder keeps for its successor. With the
/// output it keeps, base64 in one frame, it stays within [`MAX_FRAME`].
pub(super) const MAX_SCREEN: usize = 8 << 20;

#[derive(Debug, Serialize, Deserialize)]
enum Request {
    Spawn {
        /// A postcard-encoded [`CommandBuilder`], base64 in the JSON. Its
        /// environment is keyed by `OsString`, which a JSON object cannot key
        /// by. Only a daemon sends this, and only to a holder it just started
        /// from its own image, so a binary encoding cannot straddle versions.
        command: String,
        size: PtySize,
        #[serde(default)]
        recovery: Option<Recovery>,
    },
    /// The daemon no longer needs the terminal. The holder exits, closing its
    /// master; any process still on the terminal is hung up.
    Release,
    /// Sent only after checked family termination or a drained completed PTY.
    ReleaseProved,
    /// Final bounded emulator image, retained only after the root exits.
    CacheScreen { screen: String, screen_lost: bool },
    /// The successor allocated its parser and can now own the replay bytes.
    CommitAdoption,
    /// The daemon is going away without ending the terminal. The holder
    /// listens on `socket` for a successor and answers [`Report::Detached`].
    Detach {
        socket: PathBuf,
        /// What the daemon last showed of the terminal, base64, for the
        /// successor to show again. Opaque to the holder.
        screen: String,
    },
    /// A successor, connected to a detached holder's socket, takes the
    /// terminal over. Answered with [`Report::Attached`].
    Inspect {
        proof: crate::model::HostSessionProof,
    },
    Attach {
        protocol: u32,
        #[serde(default)]
        recovery: Option<Recovery>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum Report {
    ScreenCached,
    AdoptionCommitted,
    /// Protocol 5 carries PTY master, root pidfd, and original launch directory.
    Started {
        protocol: u32,
        pid: libc::pid_t,
        #[serde(default)]
        launch_directory: Option<crate::agent_execution::DirectoryIdentity>,
    },
    Failed(String),
    /// The root's raw wait status, reported once, after the holder reaps it.
    Exited {
        wait_status: libc::c_int,
    },
    /// The holder listens for a successor; this connection ends here.
    Detached,
    Inspected {
        record: Record,
        recovery: Recovery,
        screen_lost: bool,
    },
    /// Carries the PTY master, then a pidfd for the root, to a successor.
    Attached {
        protocol: u32,
        pid: libc::pid_t,
        /// The root's raw wait status, when it exited while detached.
        exited: Option<libc::c_int>,
        /// The screen the previous daemon detached with, base64.
        screen: String,
        /// Output that arrived while detached, base64. Output after it is
        /// still unread on the terminal.
        output: String,
        #[serde(default)]
        screen_lost: bool,
        #[serde(default)]
        launch_directory: Option<crate::agent_execution::DirectoryIdentity>,
    },
}

/// A terminal whose PTY and root a holder owns.
pub(super) struct Held {
    pub(super) master: PtyMaster,
    pub(super) child: HeldChild,
    pub(super) holder: Holder,
    pub(super) launch_directory: Option<LaunchDirectoryPin>,
}

pub(super) type LaunchDirectoryPin = (crate::agent_execution::DirectoryIdentity, Arc<File>);

/// Validate the physical launch directory, never resolving a caller pathname.
pub(super) fn checked_launch_directory(
    identity: crate::agent_execution::DirectoryIdentity,
    file: Arc<File>,
) -> Result<LaunchDirectoryPin, String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect launch directory: {error}"))?;
    if !metadata.is_dir()
        || crate::agent_execution::DirectoryIdentity::from_metadata(&metadata) != identity
    {
        return Err(
            "terminal launch directory descriptor does not match its original identity".into(),
        );
    }
    // SAFETY: F_GETFD inspects this live, owned descriptor without mutation.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 || flags & libc::FD_CLOEXEC == 0 {
        return Err("terminal launch directory descriptor is not close-on-exec".into());
    }
    Ok((identity, file))
}

fn pin_launch_directory(command: &CommandBuilder) -> Result<LaunchDirectoryPin, String> {
    let directory = match command.get_cwd() {
        Some(directory) => PathBuf::from(directory),
        None => match command.get_env("HOME").and_then(|home| home.to_str()) {
            Some(home) => PathBuf::from(home),
            None => {
                // Match portable-pty's default-home selection in this execed
                // holder. Production recipes always set an explicit cwd.
                let user = unsafe { libc::getpwuid(libc::getuid()) };
                if user.is_null() {
                    PathBuf::from("/")
                } else {
                    PathBuf::from(
                        unsafe { std::ffi::CStr::from_ptr((*user).pw_dir) }
                            .to_str()
                            .map_err(|_| "terminal home is not UTF-8")?,
                    )
                }
            }
        },
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&directory)
        .map_err(|error| {
            format!(
                "pin terminal launch directory {}: {error}",
                directory.display()
            )
        })?;
    let identity = crate::agent_execution::DirectoryIdentity::from_metadata(
        &file.metadata().map_err(|error| error.to_string())?,
    );
    checked_launch_directory(identity, Arc::new(file))
}

/// Called only inside the dedicated execed holder, before it starts threads
/// or the PTY root. Relative executable and PATH lookups then use '.' rather
/// than /proc/self/fd/N, whose fd portable-pty closes before exec.
fn enter_launch_directory(
    command: &mut CommandBuilder,
    pin: &LaunchDirectoryPin,
) -> Result<(), String> {
    checked_launch_directory(pin.0, Arc::clone(&pin.1))?;
    // SAFETY: the directory File is pinned and this holder has its own cwd.
    if unsafe { libc::fchdir(pin.1.as_raw_fd()) } != 0 {
        return Err(format!(
            "enter pinned terminal launch directory: {}",
            io::Error::last_os_error()
        ));
    }
    command.cwd(".");
    Ok(())
}

/// Starts a holder that spawns `command` on a new PTY of `size`. The root's
/// exit is delivered to `exits`, once.
#[cfg(test)]
pub(super) fn spawn(
    command: CommandBuilder,
    size: PtySize,
    exits: Sender<ChildExit>,
) -> Result<Held, String> {
    spawn_recoverable(command, size, exits, None)
}

pub(super) fn spawn_recoverable(
    command: CommandBuilder,
    size: PtySize,
    exits: Sender<ChildExit>,
    recovery: Option<Recovery>,
) -> Result<Held, String> {
    let reservation = crate::daemon::child_reaper::reserve()
        .map_err(|error| format!("reserve terminal holder child custody: {error}"))?;
    spawn_reserved(command, size, exits, recovery, reservation)
}

fn spawn_reserved(
    command: CommandBuilder,
    size: PtySize,
    exits: Sender<ChildExit>,
    recovery: Option<Recovery>,
    reservation: crate::daemon::child_reaper::Reservation,
) -> Result<Held, String> {
    // Every image that can start a holder has to be able to serve as one.
    // Referencing the entry here keeps the linker from discarding it.
    std::hint::black_box(&SERVE);

    let (control, remote) =
        UnixStream::pair().map_err(|error| format!("create terminal holder socket: {error}"))?;
    let remote_fd = remote.as_raw_fd();
    let mut process = Command::new("/proc/self/exe");
    process
        .arg0(PROCESS_NAME.to_str().expect("the holder name is ASCII"))
        .arg(ARGUMENT)
        .arg(remote_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls run between fork and exec. The
    // holder leads its own session, so nothing aimed at the daemon's process
    // group or terminal reaches it, and it inherits exactly one descriptor:
    // a holder can outlive whatever else the daemon had open.
    unsafe {
        process.pre_exec(move || {
            crate::daemon::seal_inherited_descriptors()?;
            if libc::setsid() == -1 || libc::fcntl(remote_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The root inherits the holder's limit.
    crate::host::descriptor_limit::inherit(&mut process);
    // `/proc/self/exe` runs this image even after an upgrade has replaced the
    // file it was loaded from, so a holder always matches its daemon.
    let process = spawn_original(&mut process, reservation)
        .map_err(|error| format!("start terminal holder: {error}"))?;
    drop(remote);
    // The holder is this daemon's unreaped child until `Holder` reaps it.
    let pidfd = open_pidfd(process.child.id() as libc::pid_t);
    let mut holder = Holder::new(control, Some(process));
    // Preserve the captured descriptor in the cleanup owner before any other
    // fallible setup step can return through Holder::drop.
    holder.pidfd = Some(pidfd.map_err(|error| format!("watch terminal holder: {error}"))?);
    let registered_id = recovery.as_ref().map(|recovery| recovery.source.id);
    let owner = registry::Process::read(holder.process.as_ref().unwrap().child.id() as i32)
        .map_err(|error| error.to_string())?;
    holder.registered = recovery.is_some();

    let command = postcard::to_stdvec(&command)
        .map(|command| STANDARD.encode(command))
        .map_err(|error| format!("encode terminal command: {error}"))?;
    let deadline = Instant::now() + START_TIMEOUT;
    let started = crate::descriptor_frame::send_json_with_fds_until(
        &holder.control,
        &Request::Spawn {
            command,
            size,
            recovery,
        },
        &[],
        deadline,
        MAX_FRAME as usize,
        3,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .and_then(|()| {
        crate::descriptor_frame::receive_json_with_fds_until::<Report>(
            &holder.control,
            deadline,
            MAX_FRAME as usize,
            3,
            crate::descriptor_frame::FrameByteOrder::LittleEndian,
        )
    })
    .map_err(|error| format!("start terminal holder: {error}"))?;
    let (report, descriptors) = started;
    let (pid, launch_directory) = match report {
        Report::Started {
            protocol,
            pid,
            launch_directory,
        } if protocol == PROTOCOL => (pid, launch_directory),
        Report::Started { protocol, .. } => {
            return Err(format!(
                "terminal holder speaks protocol {protocol}, expected {PROTOCOL}"
            ));
        }
        Report::Failed(message) => return Err(message),
        _ => return Err("terminal holder answered out of turn".into()),
    };
    crate::daemon::process_scope::warn_if_shared(owner.pid as u32, pid as u32);
    holder.protocol = PROTOCOL;
    if let Some(id) = registered_id {
        let record = registry::evidence(&registry::socket(&registry::directory()?, id))?
            .ok_or("New terminal holder has no discovery evidence")?;
        if record.holder != owner || record.root.pid != pid {
            return Err("New terminal holder evidence does not match its exact child".into());
        }
        holder.record = Some(record);
    }
    watched(holder, pid, descriptors, None, exits, launch_directory)
}

/// A terminal taken over from a detached holder.
pub(super) struct Adopted {
    pub(super) held: Held,
    /// The screen its previous daemon detached with.
    pub(super) screen: Vec<u8>,
    /// Output that followed that screen, which precedes anything still
    /// unread on the terminal.
    pub(super) output: Vec<u8>,
    pub(super) screen_lost: bool,
}

/// Takes over the terminal of a holder a previous daemon
/// [detached](Holder::detach) onto `socket`. A root that exited meanwhile
/// has its exit delivered to `exits` straight away.
#[cfg(test)]
fn adopt(socket: &Path, exits: Sender<ChildExit>) -> Result<Adopted, String> {
    adopt_recoverable(socket, exits, None)
}

pub(super) fn adopt_recoverable(
    socket: &Path,
    exits: Sender<ChildExit>,
    recovery: Option<Recovery>,
) -> Result<Adopted, String> {
    let evidence = crate::daemon::holder_registry::recovery_candidate(
        socket,
        crate::daemon::holder_registry::Kind::Terminal,
    )?;
    let mut original_role = None;
    if let Some(record) = &evidence
        && record.application != crate::instance::APPLICATION_ID
    {
        let next = recovery
            .as_ref()
            .ok_or("Cross-role adoption requires issued recovery lineage")?;
        let proof = next
            .source
            .continuity
            .live_proof()
            .ok_or("Cross-role adoption has no successor proof")?;
        let (original, _) = inspect(socket, record, &proof)?;
        if next.execution_recipe != original.execution_recipe
            || next.execution_environment != original.execution_environment
            || next.source.launch != original.source.launch
            || (next.source != original.source && !next.previous_sources.contains(&original.source))
        {
            return Err(
                "Cross-role adoption does not retain the holder's exact issued source".into(),
            );
        }
        original_role = crate::daemon::holder_registry::OriginalRole::authenticated(
            record,
            &original
                .source
                .continuity
                .live_proof()
                .ok_or("Original holder source has no proof")?,
            &proof,
        )?;
    }
    let failed =
        |error: io::Error| format!("adopt terminal holder at {}: {error}", socket.display());
    let control = crate::daemon::holder_registry::connect(socket).map_err(failed)?;
    let peer = peer_credentials(&control).map_err(failed)?;
    if evidence.as_ref().is_some_and(|record| {
        peer.pid != record.holder.pid || !record.holder.is_current() || recovery.is_none()
    }) {
        return Err("holder adoption requires matching live process evidence and lineage".into());
    }
    // SAFETY: getuid cannot fail.
    if peer.uid != unsafe { libc::getuid() } {
        return Err(format!(
            "adopt terminal holder at {}: it belongs to another user",
            socket.display()
        ));
    }
    let pidfd = peer_pidfd(&control, peer.pid).map_err(failed)?;
    let mut holder = Holder::new(control, None);
    // A refused or malformed handshake grants no authority to release it.
    holder.detached = true;
    holder.pidfd = Some(pidfd);
    let deadline = Instant::now() + HANDOVER_TIMEOUT;
    crate::descriptor_frame::send_json_with_fds_until(
        &holder.control,
        &Request::Attach {
            protocol: PROTOCOL,
            recovery,
        },
        &[],
        deadline,
        MAX_FRAME as usize,
        3,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .map_err(|error| failed(io::Error::other(error)))?;
    let attached = crate::descriptor_frame::receive_json_with_fds_until::<Report>(
        &holder.control,
        deadline,
        MAX_FRAME as usize,
        3,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .map_err(|error| failed(io::Error::other(error)))?;
    let (report, descriptors) = attached;
    let Report::Attached {
        protocol,
        pid,
        exited,
        screen,
        output,
        screen_lost,
        launch_directory,
    } = report
    else {
        return Err(format!(
            "terminal holder at {} answered out of turn",
            socket.display()
        ));
    };
    if !ADOPTABLE.contains(&protocol) {
        return Err(format!(
            "terminal holder at {} speaks protocol {protocol}, which this daemon cannot adopt",
            socket.display()
        ));
    }
    if evidence.as_ref().is_some_and(|record| {
        record.root.pid != pid || (exited.is_none() && !record.root.is_current())
    }) {
        return Err("holder root process no longer matches discovery evidence".into());
    }
    let decode = |encoded: String| {
        STANDARD
            .decode(encoded)
            .map_err(|error| format!("decode held terminal: {error}"))
    };
    let (screen, output) = (decode(screen)?, decode(output)?);
    holder.protocol = protocol;
    holder.adoption_pending = evidence.is_some();
    holder.registered = evidence.is_some();
    holder.record = evidence;
    holder.original_role = original_role;
    holder.screen_lost = screen_lost;
    // No release/kill authority is acquired until descriptor provenance and
    // every fallible watch resource have been installed. Anonymous holders do
    // not have adoption_pending to protect a refused handoff in Drop.
    let mut held = watched(holder, pid, descriptors, exited, exits, launch_directory)?;
    held.holder.detached = false;
    Ok(Adopted {
        held,
        screen,
        output,
        screen_lost,
    })
}

/// Starts watching a holder that just handed over its terminal.
fn watched(
    mut holder: Holder,
    pid: libc::pid_t,
    descriptors: Vec<OwnedFd>,
    exited: Option<libc::c_int>,
    exits: Sender<ChildExit>,
    launch_directory: Option<crate::agent_execution::DirectoryIdentity>,
) -> Result<Held, String> {
    let (master, pidfd, launch_directory) =
        decode_held_descriptors(holder.protocol, launch_directory, descriptors)?;
    let root = File::from(pidfd);
    if let Some(wait_status) = exited {
        let _ = exits.try_send(Ok(exit_status(wait_status)));
    }
    let (acknowledge, acknowledged) = crossbeam_channel::bounded(1);
    holder.acknowledged = Some(acknowledged);
    let reader = holder
        .control
        .try_clone()
        .map_err(|error| format!("watch terminal holder: {error}"))?;
    let watched_root = root
        .try_clone()
        .map_err(|error| format!("watch terminal root: {error}"))?;
    let (released, release) =
        std::io::pipe().map_err(|error| format!("watch terminal release: {error}"))?;
    let watch = Watch::start(
        reader,
        watched_root,
        released,
        exits,
        acknowledge,
        exited.is_some(),
    )?;
    holder.release = Some(release);
    holder.watch = Some(watch);
    Ok(Held {
        master: PtyMaster(master),
        child: HeldChild { pid, pidfd: root },
        holder,
        launch_directory,
    })
}

fn decode_held_descriptors(
    protocol: u32,
    identity: Option<crate::agent_execution::DirectoryIdentity>,
    descriptors: Vec<OwnedFd>,
) -> Result<(OwnedFd, OwnedFd, Option<LaunchDirectoryPin>), String> {
    if !ADOPTABLE.contains(&protocol) {
        return Err("unsupported terminal launch-directory protocol".into());
    }
    if protocol >= 5 {
        let [master, pidfd, directory]: [OwnedFd; 3] = descriptors
            .try_into()
            .map_err(|_| "terminal holder sent the wrong launch-directory descriptors")?;
        let identity = identity.ok_or("terminal holder did not attest its launch directory")?;
        let pin = checked_launch_directory(identity, Arc::new(File::from(directory)))?;
        Ok((master, pidfd, Some(pin)))
    } else {
        if identity.is_some() {
            return Err("legacy holder cannot attest a launch directory".into());
        }
        let [master, pidfd]: [OwnedFd; 2] = descriptors
            .try_into()
            .map_err(|_| "legacy terminal holder sent the wrong descriptors")?;
        Ok((master, pidfd, None))
    }
}

fn exit_status(wait_status: libc::c_int) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from(std::process::ExitStatus::from_raw(wait_status))
}

/// Spawn-time credit and original wait authority must never be separated by a
/// fallible ownership transfer. Adopted holders do not have this authority.
struct OriginalChild {
    child: Child,
    reservation: crate::daemon::child_reaper::Reservation,
}

fn spawn_original(
    command: &mut Command,
    reservation: crate::daemon::child_reaper::Reservation,
) -> io::Result<OriginalChild> {
    Ok(OriginalChild {
        child: command.spawn()?,
        reservation,
    })
}

impl OriginalChild {
    fn retain(self, pidfd: Option<File>, reason: &str) {
        let Self { child, reservation } = self;
        reservation.quarantine();
        let Some(pidfd) = pidfd else {
            reservation.preserve_suspended(child, None, reason);
            return;
        };
        if let Err((error, child, pidfd, reservation)) = reservation.retain(child, pidfd) {
            // retain returns every original operand intact on validation,
            // wakeup or service failure. No second reaper or PID lookup.
            reservation.preserve_suspended(child, Some(pidfd), &format!("{reason}: {error}"));
        }
    }
}

/// Local acknowledgement identity. The wire reports stay distinct all the way
/// to the operation waiting for them; an earlier operation may have timed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acknowledgement {
    ScreenCached,
    AdoptionCommitted,
    Detached,
}

fn await_acknowledgement(
    answers: &Receiver<Result<Acknowledgement, String>>,
    expected: Acknowledgement,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        if Instant::now() >= deadline {
            return Err("terminal holder acknowledgement deadline elapsed".into());
        }
        let observed = answers
            .recv_deadline(deadline)
            .map_err(|error| error.to_string())??;
        if observed == expected {
            return Ok(());
        }
        // A late success for a different request cannot transfer custody.
        // Keep the original deadline rather than granting another full wait.
        tracing::warn!(
            ?expected,
            ?observed,
            "ignored a different terminal holder acknowledgement"
        );
    }
}

/// The daemon's end of a holder: its control socket and its process.
/// Dropping it releases the holder, unless the holder was detached.
pub(super) struct Holder {
    control: UnixStream,
    /// A holder this daemon started is its child, which it reaps. An adopted
    /// holder's parent is whoever inherited it from the daemon that started it.
    process: Option<OriginalChild>,
    pidfd: Option<File>,
    protocol: u32,
    watch: Option<Watch>,
    /// Dropping this ends a watch still waiting on a root its holder left.
    release: Option<std::io::PipeWriter>,
    /// The holder's typed answers, relayed by the original control watch.
    acknowledged: Option<Receiver<Result<Acknowledgement, String>>>,
    detached: bool,
    adoption_pending: bool,
    registered: bool,
    screen_lost: bool,
    cleanup_proved: bool,
    record: Option<Record>,
    original_role: Option<crate::daemon::holder_registry::OriginalRole>,
}

impl Holder {
    #[cfg(test)]
    pub(super) fn original_pidfd_for_test(&self) -> io::Result<File> {
        self.pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("original holder pidfd was not captured"))?
            .try_clone()
    }

    pub(super) fn original_role(&self) -> Option<&crate::daemon::holder_registry::OriginalRole> {
        self.original_role.as_ref()
    }
    pub(super) fn record(&self) -> Option<&Record> {
        self.record.as_ref()
    }
    pub(super) fn prove_cleanup(&mut self) {
        self.cleanup_proved = true;
    }

    /// Only a checked close of the exact original family authorizes discarding
    /// an unconfirmed startup replay. It supersedes provisional adoption; an
    /// ordinary Drop, failed inspection or timeout never calls this method.
    pub(super) fn prove_startup_cleanup(&mut self) {
        self.cleanup_proved = true;
        self.adoption_pending = false;
    }

    #[cfg(test)]
    pub(super) fn fail_after_actual_commit_for_test(&mut self) -> Result<(), String> {
        self.commit_adoption()?; // real original holder acknowledged and cleared replay
        // Inject the caller's uncertain outcome at precisely that irreversible
        // boundary, retaining the conservative phase of a lost acknowledgement.
        self.adoption_pending = true;
        Err("injected local failure AFTER actual holder commit acknowledgement".into())
    }

    #[cfg(test)]
    pub(super) fn startup_cleanup_state_for_test(&self) -> (bool, bool) {
        (self.adoption_pending, self.cleanup_proved)
    }
    pub(super) fn registered(&self) -> bool {
        self.registered
    }

    pub(super) fn leave_for_successor(&mut self) {
        self.detached = true;
        let _ = self.control.shutdown(std::net::Shutdown::Both);
    }

    pub(super) fn cache_screen(&mut self, screen: &[u8]) -> Result<(), String> {
        if !self.registered || screen.len() > MAX_SCREEN {
            return Err("completed holder screen is not retainable".into());
        }
        self.control
            .set_write_timeout(Some(HANDOVER_TIMEOUT))
            .and_then(|()| {
                send(
                    &self.control,
                    &Request::CacheScreen {
                        screen: STANDARD.encode(screen),
                        screen_lost: self.screen_lost,
                    },
                    &[],
                )
            })
            .and_then(|()| self.control.set_write_timeout(None))
            .map_err(|error| error.to_string())?;
        await_acknowledgement(
            self.acknowledged
                .as_ref()
                .ok_or("holder acknowledgement unavailable")?,
            Acknowledgement::ScreenCached,
            Instant::now() + HANDOVER_TIMEOUT,
        )
    }
    pub(super) fn commit_adoption(&mut self) -> Result<(), String> {
        if !self.adoption_pending {
            return Ok(());
        }
        self.control
            .set_write_timeout(Some(HANDOVER_TIMEOUT))
            .and_then(|()| send(&self.control, &Request::CommitAdoption, &[]))
            .and_then(|()| self.control.set_write_timeout(None))
            .map_err(|error| error.to_string())?;
        await_acknowledgement(
            self.acknowledged
                .as_ref()
                .ok_or("holder acknowledgement unavailable")?,
            Acknowledgement::AdoptionCommitted,
            Instant::now() + HANDOVER_TIMEOUT,
        )?;
        self.adoption_pending = false;
        Ok(())
    }
    fn new(control: UnixStream, process: Option<OriginalChild>) -> Self {
        Self {
            control,
            process,
            pidfd: None,
            protocol: 0,
            watch: None,
            release: None,
            acknowledged: None,
            detached: false,
            adoption_pending: false,
            registered: false,
            screen_lost: false,
            cleanup_proved: false,
            record: None,
            original_role: None,
        }
    }

    /// Leaves the terminal running under its holder, which listens on
    /// `socket` for a successor to [`adopt`] it, with `screen` and the
    /// terminal's output meanwhile. Nothing may read the terminal here once
    /// this succeeds: the holder reads it now, and dropping this leaves it be.
    /// On failure the holder is still attached.
    pub(super) fn detach(&mut self, socket: &Path, screen: &[u8]) -> Result<(), String> {
        if !ADOPTABLE.contains(&self.protocol) {
            return Err(format!(
                "terminal holder speaks protocol {}, which cannot be detached",
                self.protocol
            ));
        }
        if screen.len() > MAX_SCREEN {
            return Err("terminal screen is too large to detach".to_owned());
        }
        // A socket left by an earlier attempt would refuse the holder's bind.
        match if self.protocol < 3 {
            std::fs::remove_file(socket)
        } else {
            Ok(())
        } {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(format!("clear {}: {error}", socket.display()));
            }
            _ => {}
        }
        let request = Request::Detach {
            socket: socket.to_owned(),
            screen: STANDARD.encode(screen),
        };
        // A holder that stopped reading must not hold its daemon's shutdown.
        self.control
            .set_write_timeout(Some(HANDOVER_TIMEOUT))
            .and_then(|()| send(&self.control, &request, &[]))
            .and_then(|()| self.control.set_write_timeout(None))
            .map_err(|error| format!("detach terminal holder: {error}"))?;
        await_acknowledgement(
            self.acknowledged
                .as_ref()
                .ok_or("holder acknowledgement unavailable")?,
            Acknowledgement::Detached,
            Instant::now() + HANDOVER_TIMEOUT,
        )?;
        self.detached = true;
        Ok(())
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if self.adoption_pending {
            // A failed emulator/reader setup must leave the original process
            // adoptable. It never acquired authority to close that session.
            self.detached = true;
            let _ = self.control.shutdown(std::net::Shutdown::Both);
        }
        if !self.detached {
            // First, so the watch takes the holder's exit for the release it is.
            drop(self.release.take());
            let request = if self.cleanup_proved && self.protocol >= 4 {
                Request::ReleaseProved
            } else {
                Request::Release
            };
            let _ = send(&self.control, &request, &[]);
            let exited = self
                .pidfd
                .as_ref()
                .is_some_and(|pidfd| exits_within(pidfd, RELEASE_GRACE).unwrap_or(false));
            if !exited && self.cleanup_proved && self.protocol >= 4 {
                // A proved release may still be fsyncing its receipt. Keep
                // that exact owner alive; killing it would erase the only
                // authority for a retry after the daemon exits.
                if let Some(process) = self.process.take() {
                    process.retain(
                        self.pidfd.take(),
                        "Terminal receipt release remains pending",
                    );
                }
            } else if !exited {
                tracing::warn!("killing a terminal holder that did not exit when released");
                match (&mut self.process, &self.pidfd) {
                    (Some(process), _) => {
                        let _ = process.child.kill();
                    }
                    (None, Some(pidfd)) => {
                        let _ = signal(pidfd, libc::SIGKILL);
                    }
                    (None, None) => {}
                }
            }
            if let Some(mut process) = self.process.take() {
                if let Err(error) = process.child.wait() {
                    process.retain(
                        self.pidfd.take(),
                        &format!("Terminal child wait failed: {error}"),
                    );
                }
                // Successful exact wait drops its reservation normally.
            }
        } else if let Some(process) = self.process.take() {
            process.retain(
                self.pidfd.take(),
                "Detached terminal child custody transfer",
            );
        }
        // Ending a watch does not end original child custody. A detached
        // original is now owned by the bounded exact-child reaper.
        drop(self.release.take());
        if let Some(watch) = self.watch.take() {
            watch.stop();
        }
    }
}

/// A holder's PTY master, shared with the holder that opened it.
pub(super) struct PtyMaster(OwnedFd);

impl PtyMaster {
    /// Ask the kernel to notify this PTY's foreground group. A numeric
    /// tcgetpgrp/kill pair could instead signal a reused process-group ID.
    pub(super) fn request_redraw(&self) -> io::Result<()> {
        let mut original: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: the owned PTY and writable winsize outlive both ioctls.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCGWINSZ, &mut original) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut changed = original;
        changed.ws_xpixel ^= 1;
        // Pixel-only change keeps text rows/columns stable while forcing
        // SIGWINCH; restore the exact original size before returning.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &changed) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &original) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn resize(&self, size: PtySize) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: the descriptor is owned and `size` outlives the call.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A further descriptor for the same master, closed on exec.
    pub(super) fn duplicate(&self) -> io::Result<OwnedFd> {
        self.0.try_clone()
    }
}

impl AsRawFd for PtyMaster {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// The root process of a held terminal. Its holder is the parent that reaps
/// it; the daemon signals it through a pidfd, which can never reach a
/// process that reused the pid.
pub(super) struct HeldChild {
    pid: libc::pid_t,
    pidfd: File,
}

impl HeldChild {
    pub(super) fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Clone this exact root pidfd; File::try_clone uses close-on-exec custody.
    pub(super) fn duplicate_pidfd(&self) -> io::Result<File> {
        self.pidfd.try_clone()
    }

    /// Whether the root has not been reaped yet. Anything read about its pid
    /// before this answers true was read about the root itself.
    pub(super) fn unreaped(&self) -> bool {
        signal(&self.pidfd, 0).is_ok()
    }

    /// Hangs the root up, as closing its terminal would. A root that already
    /// exited counts as hung up; one that ignores the hangup keeps running.
    pub(super) fn kill(&self) -> io::Result<()> {
        match signal(&self.pidfd, libc::SIGHUP) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result,
        }
    }

    pub(super) fn exited_within(&self, grace: Duration) -> io::Result<bool> {
        exits_within(&self.pidfd, grace)
    }

    /// Escalates only this pinned root after a graceful hangup was ignored.
    /// The pidfd cannot acquire authority over a reused numeric process ID.
    pub(super) fn force_kill(&self) -> io::Result<()> {
        match signal(&self.pidfd, libc::SIGKILL) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result,
        }
    }
}

fn peer_credentials(socket: &UnixStream) -> io::Result<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` and `length` describe a live ucred buffer.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials)
}

/// A pidfd for the process at the other end of `socket`, whose pid is `pid`.
/// The kernel names that very process where it can; otherwise the pid names
/// it only while the holder lives, which it does until it has handed over.
fn peer_pidfd(socket: &UnixStream, pid: libc::pid_t) -> io::Result<File> {
    let mut fd: libc::c_int = -1;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` and `length` describe a live c_int buffer.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&raw mut fd).cast(),
            &mut length,
        )
    };
    if result == 0 {
        // SAFETY: a successful SO_PEERPIDFD returns a fresh owned descriptor.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    match io::Error::last_os_error() {
        // Before Linux 6.5.
        error if error.raw_os_error() == Some(libc::ENOPROTOOPT) => open_pidfd(pid),
        error => Err(error),
    }
}

fn open_pidfd(pid: libc::pid_t) -> io::Result<File> {
    // SAFETY: pidfd_open takes scalars; a success is a fresh owned descriptor.
    match unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } {
        -1 => Err(io::Error::last_os_error()),
        fd => Ok(unsafe { File::from_raw_fd(fd as RawFd) }),
    }
}

/// Whether a pidfd's process exits within `grace`: a pidfd becomes readable
/// when it does.
fn exits_within(pidfd: &File, grace: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + grace;
    let mut target = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // SAFETY: one live pollfd for the duration of the call.
        match unsafe { libc::poll(&mut target, 1, remaining.as_millis() as libc::c_int) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            -1 => return Err(io::Error::last_os_error()),
            ready => return Ok(ready > 0),
        }
    }
}

/// Serves as a holder when this image was executed as one.
///
/// It runs from `.init_array`, before `main`, so every image that can start a
/// holder can also be one: the daemon, and any test harness that links the
/// host library. None of them needs a hook in its own `main`, and a holder
/// never initializes anything it does not use. Any other invocation costs one
/// read of its own command line.
#[used]
#[unsafe(link_section = ".init_array")]
static SERVE: extern "C" fn() = serve_if_requested;

extern "C" fn serve_if_requested() {
    let Ok(command_line) = std::fs::read("/proc/self/cmdline") else {
        return;
    };
    let mut arguments = command_line.split(|&byte| byte == 0).skip(1);
    if arguments.next() != Some(ARGUMENT.as_bytes()) {
        return;
    }
    let control = arguments
        .next()
        .and_then(|fd| std::str::from_utf8(fd).ok()?.parse::<RawFd>().ok())
        .filter(|&fd| fd > libc::STDERR_FILENO);
    std::process::exit(match control {
        // SAFETY: the daemon passed exactly this descriptor, and nothing else
        // in this process has claimed it this early.
        Some(fd) => serve(unsafe { UnixStream::from_raw_fd(fd) }),
        None => 2,
    });
}

struct Residence {
    directory: PathBuf,
    listener: UnixListener,
    record: Record,
    socket_device: u64,
    socket_inode: u64,
}

impl Residence {
    fn create(recovery: &Recovery, pid: i32) -> Result<Self, String> {
        let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
        recovery.validate(&boot, recovery.source.id)?;
        let directory = registry::directory()?;
        crate::secure_file::prepare_private_directory(&directory)
            .map_err(|error| error.to_string())?;
        let record = Record {
            version: PROTOCOL,
            build: Some(crate::daemon::holder_registry::BuildIdentity::current(
                crate::daemon::holder_registry::Kind::Terminal,
            )),
            kind: crate::daemon::holder_registry::Kind::Terminal,
            application: crate::instance::APPLICATION_ID.into(),
            boot,
            terminal: recovery.source.id,
            holder: registry::Process::read(std::process::id() as i32)
                .map_err(|error| error.to_string())?,
            root: registry::Process::read(pid).map_err(|error| error.to_string())?,
        };
        let socket = registry::socket(&directory, record.terminal);
        let listener =
            crate::daemon::holder_registry::bind(&socket).map_err(|error| error.to_string())?;
        let metadata = std::fs::symlink_metadata(&socket).map_err(|error| error.to_string())?;
        if let Err(error) = record.create(&directory) {
            let _ = std::fs::remove_file(socket);
            return Err(error.to_string());
        }
        Ok(Self {
            directory,
            listener,
            record,
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        })
    }

    fn retire(&self) -> io::Result<()> {
        let path = registry::socket(&self.directory, self.record.terminal);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.dev() == self.socket_device && metadata.ino() == self.socket_inode =>
            {
                match std::fs::remove_file(path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    result => result?,
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.record.remove_record(&self.directory)
    }
}

impl Drop for Residence {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

fn serve(control: UnixStream) -> i32 {
    // SAFETY: plain process attribute changes, made before anything else
    // runs. A write to a daemon that went away must fail rather than kill the
    // holder, the holder has no terminal whose hangup could concern it, and
    // the name is a static C string.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        // Reaping the root is this holder's job, which an inherited
        // SIGCHLD disposition of "ignore" would do behind its back.
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        libc::fcntl(control.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        libc::prctl(libc::PR_SET_NAME, PROCESS_NAME.as_ptr());
    }
    let fail = |message: String| {
        let _ = send(&control, &Report::Failed(message), &[]);
        1
    };
    let (command, size, mut recovery) = match receive::<Request>(&control) {
        Ok(Some((
            Request::Spawn {
                command,
                size,
                recovery,
            },
            _,
        ))) => (command, size, recovery),
        _ => return 1,
    };
    if let Some(recovery) = &recovery {
        let authority = (|| {
            let boot = crate::daemon::read_boot_id()?;
            recovery
                .validate(&boot, recovery.source.id)
                .map_err(io::Error::other)?;
            let peer = peer_credentials(&control)?;
            if peer.uid != unsafe { libc::getuid() } {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            authorize_successor(
                peer.pid,
                &recovery
                    .source
                    .continuity
                    .live_proof()
                    .ok_or(io::ErrorKind::PermissionDenied)?,
            )
        })();
        if let Err(error) = authority {
            return fail(format!("authorize terminal holder: {error}"));
        }
    }
    let mut command = match STANDARD
        .decode(command)
        .map_err(|error| error.to_string())
        .and_then(|command| {
            postcard::from_bytes::<CommandBuilder>(&command).map_err(|error| error.to_string())
        }) {
        Ok(command) => command,
        Err(error) => return fail(format!("decode terminal command: {error}")),
    };
    let program = command
        .get_argv()
        .first()
        .map(|program| program.to_string_lossy().into_owned())
        .unwrap_or_default();
    let launch_directory = match pin_launch_directory(&command) {
        Ok(pin) => pin,
        Err(error) => return fail(error),
    };
    // Isolate this execed keeper before the PTY root and all of its tools
    // inherit its cgroup. A failed request retains the existing launch path.
    let _ = crate::daemon::process_scope::enroll_holder();
    let pair = match native_pty_system().openpty(size) {
        Ok(pair) => pair,
        Err(error) => return fail(format!("open PTY: {error:#}")),
    };
    if let Err(error) = enter_launch_directory(&mut command, &launch_directory) {
        return fail(error);
    }
    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(error) => return fail(format!("spawn {program}: {error:#}")),
    };
    drop(pair.slave);
    let Some(pid) = child.process_id().map(|pid| pid as libc::pid_t) else {
        return fail(format!("spawn {program}: no process id"));
    };
    // The root is this holder's unreaped child, so its pid is its own.
    let pidfd = match open_pidfd(pid) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            // SAFETY: as above; the root cannot be handed over, so it must
            // not outlive this failure.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            return fail(format!("open pidfd for {program}: {error}"));
        }
    };
    let Some(master) = pair.master.as_raw_fd() else {
        return fail("PTY descriptor unavailable".into());
    };
    let residence = match recovery
        .as_ref()
        .map(|recovery| Residence::create(recovery, pid))
        .transpose()
    {
        Ok(residence) => residence,
        Err(error) => {
            // This newly spawned child has never been handed to a daemon.
            // Closing the PTY alone cannot stop an HUP-ignoring startup job.
            // Its unreaped PID still identifies the session's process group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = signal(&pidfd, libc::SIGKILL);
            let _ = reap(pid);
            return fail(format!("register terminal holder: {error}"));
        }
    };
    let started = Report::Started {
        protocol: PROTOCOL,
        pid,
        launch_directory: Some(launch_directory.0),
    };
    // SAFETY: pair.master owns this exact live descriptor through the send.
    let master_fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(master) };
    let mut lost = crate::descriptor_frame::send_json_with_fds_until(
        &control,
        &started,
        &[master_fd, pidfd.as_fd(), launch_directory.1.as_fd()],
        Instant::now() + START_TIMEOUT,
        MAX_FRAME as usize,
        3,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .is_err();

    let mut exit = None;
    let mut cached_screen = None;
    let mut pending_replay: Option<(String, Vec<u8>, bool)> = None;
    let mut control = control;
    let mut cleanup_proved = false;
    loop {
        let disposition = if lost {
            Attendance::Lost
        } else {
            attend(
                &control,
                pid,
                &pidfd,
                &mut exit,
                &mut cached_screen,
                &mut pending_replay,
            )
        };
        let (socket, screen, output, screen_lost) = match disposition {
            Attendance::Release => break,
            Attendance::ReleaseProved => {
                cleanup_proved = true;
                break;
            }
            Attendance::Lost => match &residence {
                Some(residence) => {
                    let (screen, output, lost) = pending_replay.take().unwrap_or_else(|| {
                        (
                            cached_screen
                                .as_ref()
                                .map(|(screen, _)| screen.clone())
                                .unwrap_or_default(),
                            Vec::new(),
                            cached_screen.as_ref().is_none_or(|(_, lost)| *lost),
                        )
                    });
                    (
                        registry::socket(&residence.directory, residence.record.terminal),
                        screen,
                        output,
                        lost,
                    )
                }
                None => break,
            },
            Attendance::Detach { socket, screen } => (socket, screen, Vec::new(), false),
        };
        let listener = match &residence {
            Some(residence)
                if socket == registry::socket(&residence.directory, residence.record.terminal) =>
            {
                residence.listener.try_clone()
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "holder endpoint cannot change",
            )),
            None => crate::daemon::holder_registry::bind(&socket),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                let refusal = Report::Failed(format!("listen on {}: {error}", socket.display()));
                if send(&control, &refusal, &[]).is_err() {
                    break;
                }
                continue;
            }
        };
        if !screen_lost && send(&control, &Report::Detached, &[]).is_err() && residence.is_none() {
            let _ = std::fs::remove_file(&socket);
            break;
        }
        drop(control);
        let successor = await_successor(
            &listener,
            master,
            pid,
            &pidfd,
            &mut exit,
            &screen,
            residence.as_ref().map(|residence| &residence.record),
            &mut recovery,
            screen_lost,
            output,
            &launch_directory,
        );
        if residence.is_none() {
            let _ = std::fs::remove_file(&socket);
        }
        match successor {
            Some((successor, output)) => {
                if residence.is_some() {
                    pending_replay = Some((screen, output, screen_lost));
                }
                control = successor;
                lost = false;
            }
            None => break,
        }
    }
    drop(child);
    // The master closes with the holder, hanging up whatever is left on it.
    drop(pair.master);
    if cleanup_proved && let (Some(residence), Some(recovery)) = (&residence, &recovery) {
        // The checked worker has already stopped its exact process family;
        // independently require that this holder reaped the root too.
        if exit.is_none() && exits_within(&pidfd, Duration::ZERO).unwrap_or(false) {
            exit = reap(pid);
        }
        if exit.is_some() {
            loop {
                if registry::cleanup::publish(&residence.record, recovery).is_ok()
                    && residence.retire().is_ok()
                {
                    break;
                }
                // A failed durable write retains this exact small owner;
                // callers time out honestly and can retry its receipt.
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    0
}

/// Serves an attached daemon until it releases the terminal, which answers
/// `None`, or asks for it to be detached onto a socket, with its screen.
enum Attendance {
    Release,
    ReleaseProved,
    Lost,
    Detach { socket: PathBuf, screen: String },
}

fn attend(
    control: &UnixStream,
    pid: libc::pid_t,
    pidfd: &File,
    exit: &mut Option<libc::c_int>,
    cached_screen: &mut Option<(String, bool)>,
    pending_replay: &mut Option<(String, Vec<u8>, bool)>,
) -> Attendance {
    loop {
        let mut targets = [
            libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                // A negative descriptor is skipped once the root is reaped.
                fd: if exit.is_some() {
                    -1
                } else {
                    pidfd.as_raw_fd()
                },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both pollfds live for the duration of the call.
        if unsafe { libc::poll(targets.as_mut_ptr(), 2, -1) } == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Attendance::Lost;
        }
        if targets[1].revents != 0 {
            // A root this holder cannot reap leaves nothing to report: ending
            // the holder lets the daemon report that exit as unknown instead
            // of polling a pidfd that stays ready forever.
            let Some(wait_status) = reap(pid) else {
                return Attendance::Release;
            };
            *exit = Some(wait_status);
            let _ = send(control, &Report::Exited { wait_status }, &[]);
        }
        if targets[0].revents != 0 {
            let request = receive::<Request>(control);
            if matches!(&request, Ok(Some((Request::CommitAdoption, _)))) {
                if send(control, &Report::AdoptionCommitted, &[]).is_ok() {
                    *pending_replay = None;
                    continue;
                }
                return Attendance::Lost;
            }
            if let Ok(Some((
                Request::CacheScreen {
                    screen,
                    screen_lost,
                },
                _,
            ))) = &request
            {
                if exit.is_some() && screen.len() <= MAX_SCREEN.div_ceil(3) * 4 {
                    *cached_screen = Some((screen.clone(), *screen_lost));
                    if send(control, &Report::ScreenCached, &[]).is_ok() {
                        continue;
                    }
                    return Attendance::Lost;
                }
                if send(
                    control,
                    &Report::Failed("only a completed bounded screen can be retained".into()),
                    &[],
                )
                .is_ok()
                {
                    continue;
                }
                return Attendance::Lost;
            }
            return match request {
                Ok(Some((Request::Detach { socket, screen }, _))) => {
                    Attendance::Detach { socket, screen }
                }
                Ok(Some((Request::Release, _))) => Attendance::Release,
                Ok(Some((Request::ReleaseProved, _))) => Attendance::ReleaseProved,
                _ => Attendance::Lost,
            };
        }
    }
}

/// Registered holders retain live processes and completed results until an
/// authenticated successor attaches. Only legacy anonymous handoffs expire.
/// Output is bounded by backpressure and is delivered to the successor.
fn await_successor(
    listener: &UnixListener,
    master: RawFd,
    pid: libc::pid_t,
    pidfd: &File,
    exit: &mut Option<libc::c_int>,
    screen: &str,
    record: Option<&Record>,
    recovery: &mut Option<Recovery>,
    screen_lost: bool,
    mut output: Vec<u8>,
    launch_directory: &LaunchDirectoryPin,
) -> Option<(UnixStream, Vec<u8>)> {
    let deadline = orphan_deadline(record.is_some(), Instant::now());
    let mut buffer = vec![0u8; 64 << 10];
    // The master reads EIO once the terminal's last process has closed it.
    let mut open = true;
    loop {
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return None;
        }
        let reading = open && output.len() < DETACHED_OUTPUT;
        let mut targets = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if exit.is_some() {
                    -1
                } else {
                    pidfd.as_raw_fd()
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if reading { master } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = remaining.map_or(-1, |remaining| {
            remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int
        });
        // SAFETY: all three pollfds live for the duration of the call.
        if unsafe { libc::poll(targets.as_mut_ptr(), 3, timeout) } == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if targets[1].revents != 0 {
            // As when attended: nothing to hand over is better than a spin.
            *exit = Some(reap(pid)?);
        }
        if targets[2].revents != 0 {
            let room = (DETACHED_OUTPUT - output.len()).min(buffer.len());
            // SAFETY: `buffer` has at least `room` writable bytes.
            match unsafe { libc::read(master, buffer.as_mut_ptr().cast(), room) } {
                read if read > 0 => output.extend_from_slice(&buffer[..read as usize]),
                -1 if matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
                _ => open = false,
            }
        }
        if targets[0].revents != 0
            && let Ok((successor, _)) = listener.accept()
            && hand_over(
                &successor,
                master,
                pid,
                pidfd,
                *exit,
                screen,
                &output,
                record,
                recovery,
                screen_lost,
                launch_directory,
            )
            .is_ok_and(|attached| attached)
        {
            return Some((successor, output));
        }
    }
}

fn orphan_deadline(registered: bool, now: Instant) -> Option<Instant> {
    (!registered).then(|| now + ADOPTION_GRACE)
}

/// Gives a connecting successor the terminal, if it is this user's and asks
/// for it properly.
fn hand_over(
    successor: &UnixStream,
    master: RawFd,
    pid: libc::pid_t,
    pidfd: &File,
    exit: Option<libc::c_int>,
    screen: &str,
    output: &[u8],
    record: Option<&Record>,
    recovery: &mut Option<Recovery>,
    screen_lost: bool,
    launch_directory: &LaunchDirectoryPin,
) -> io::Result<bool> {
    // SAFETY: getuid cannot fail.
    let peer = peer_credentials(successor)?;
    if peer.uid != unsafe { libc::getuid() } {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let deadline = Instant::now() + HANDOVER_TIMEOUT;
    let request = crate::descriptor_frame::receive_json_with_fds_until::<Request>(
        successor,
        deadline,
        MAX_FRAME as usize,
        0,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    let proposed = match Some(request) {
        Some((Request::Inspect { proof }, _)) => {
            let (Some(record), Some(recovery)) = (record, recovery.as_ref()) else {
                return Err(io::ErrorKind::InvalidData.into());
            };
            authorize_successor(peer.pid, &proof)?;
            let mut recovery = recovery.clone();
            // Size belongs to the live PTY, not the original spawn recipe.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: master is this holder's live PTY and size is writable.
            if unsafe { libc::ioctl(master, libc::TIOCGWINSZ, &mut size) } == -1 {
                return Err(io::Error::last_os_error());
            }
            recovery.options.columns = size.ws_col.max(1);
            recovery.options.rows = size.ws_row.max(1);
            if size.ws_xpixel > 0 {
                recovery.options.cell_width_px =
                    u32::from(size.ws_xpixel / size.ws_col.max(1)).max(1);
            }
            if size.ws_ypixel > 0 {
                recovery.options.cell_height_px =
                    u32::from(size.ws_ypixel / size.ws_row.max(1)).max(1);
            }
            crate::descriptor_frame::send_json_with_fds_until(
                successor,
                &Report::Inspected {
                    record: record.clone(),
                    recovery,
                    screen_lost,
                },
                &[],
                deadline,
                MAX_FRAME as usize,
                0,
                crate::descriptor_frame::FrameByteOrder::LittleEndian,
            )
            .map_err(io::Error::other)?;
            return Ok(false);
        }
        Some((
            Request::Attach {
                protocol,
                recovery: proposed,
            },
            _,
        )) if protocol >= PROTOCOL => proposed,
        _ => return Err(io::ErrorKind::InvalidData.into()),
    };
    if let (Some(record), Some(current)) = (record, recovery.as_ref()) {
        let next = proposed.as_ref().ok_or(io::ErrorKind::PermissionDenied)?;
        next.validate(&record.boot, record.terminal)
            .map_err(io::Error::other)?;
        authorize_successor(
            peer.pid,
            &next
                .source
                .continuity
                .live_proof()
                .ok_or(io::ErrorKind::PermissionDenied)?,
        )?;
        if next.source.launch != current.source.launch
            || next.execution_recipe != current.execution_recipe
            || (next.source != current.source
                && !next.previous_sources.iter().any(|source| {
                    source.id == current.source.id
                        && source.launch == current.source.launch
                        && source.continuity.live_proof() == current.source.continuity.live_proof()
                }))
            || (next.source == current.source
                && !current
                    .previous_sources
                    .iter()
                    .all(|source| next.previous_sources.contains(source)))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "holder adoption does not preserve its issued lineage",
            ));
        }
    }
    let attached = Report::Attached {
        protocol: PROTOCOL,
        pid,
        exited: exit,
        screen: screen.to_owned(),
        output: STANDARD.encode(output),
        screen_lost,
        launch_directory: Some(launch_directory.0),
    };
    // SAFETY: the caller owns this live master for the whole bounded send.
    let master = unsafe { std::os::fd::BorrowedFd::borrow_raw(master) };
    crate::descriptor_frame::send_json_with_fds_until(
        successor,
        &attached,
        &[master, pidfd.as_fd(), launch_directory.1.as_fd()],
        deadline,
        MAX_FRAME as usize,
        3,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    if proposed.is_some() {
        *recovery = proposed;
    }
    Ok(true)
}

/// Reads only the memory of a proven orphan holder; does not adopt or release it.
pub(crate) fn inspect(
    socket: &Path,
    expected: &Record,
    proof: &crate::model::HostSessionProof,
) -> Result<(Recovery, bool), String> {
    let control =
        crate::daemon::holder_registry::connect(socket).map_err(|error| error.to_string())?;
    let peer = peer_credentials(&control).map_err(|error| error.to_string())?;
    if peer.uid != crate::daemon::current_uid()
        || peer.pid != expected.holder.pid
        || !expected.holder.is_current()
    {
        return Err("holder socket does not belong to the recorded process".into());
    }
    control
        .set_read_timeout(Some(HANDOVER_TIMEOUT))
        .map_err(|error| error.to_string())?;
    control
        .set_write_timeout(Some(HANDOVER_TIMEOUT))
        .map_err(|error| error.to_string())?;
    send(
        &control,
        &Request::Inspect {
            proof: proof.clone(),
        },
        &[],
    )
    .map_err(|error| error.to_string())?;
    let Some((
        Report::Inspected {
            record,
            recovery,
            screen_lost,
        },
        descriptors,
    )) = receive::<Report>(&control).map_err(|error| error.to_string())?
    else {
        return Err("holder did not return discovery evidence".into());
    };
    if !descriptors.is_empty() || record != *expected {
        return Err("holder discovery evidence changed".into());
    }
    recovery.validate(&record.boot, record.terminal)?;
    Ok((recovery, screen_lost))
}

fn authorize_successor(pid: i32, proof: &crate::model::HostSessionProof) -> io::Result<()> {
    let paths = crate::daemon::RuntimePaths::discover().map_err(io::Error::other)?;
    let owner = crate::daemon::lease_owner(&paths.lock, proof).map_err(io::Error::other)?;
    if u32::try_from(pid).ok() != Some(owner) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "holder caller does not own the daemon lease",
        ));
    }
    Ok(())
}

/// Reaps the root once its pidfd says it exited, returning its wait status.
fn reap(pid: libc::pid_t) -> Option<libc::c_int> {
    let mut wait_status = 0;
    loop {
        // SAFETY: the root is this holder's child, and its pidfd reported
        // its exit.
        match unsafe { libc::waitpid(pid, &mut wait_status, 0) } {
            reaped if reaped == pid => return Some(wait_status),
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

fn send(control: &UnixStream, message: &impl Serialize, descriptors: &[RawFd]) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let length = u32::try_from(body.len())
        .ok()
        .filter(|&length| length <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&body);
    let mut sent = 0;
    while sent < frame.len() {
        // Descriptors ride on the frame's first byte, so only the first call
        // carries them.
        let attached = if sent == 0 { descriptors } else { &[] };
        match send_with_descriptors(control, &frame[sent..], attached) {
            Ok(count) => sent += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads one message, or `None` when the peer hung up between messages.
fn receive<T: DeserializeOwned>(control: &UnixStream) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let mut header = [0u8; 4];
    let mut descriptors = Vec::new();
    let mut filled = 0;
    while filled < header.len() {
        match receive_with_descriptors(control, &mut header[filled..], &mut descriptors) {
            Ok(0) if filled == 0 && descriptors.is_empty() => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let length = u32::from_le_bytes(header);
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0; length as usize];
    (&*control).read_exact(&mut body)?;
    let message = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some((message, descriptors)))
}

fn send_with_descriptors(
    control: &UnixStream,
    bytes: &[u8],
    descriptors: &[RawFd],
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // u64 words keep the control buffer aligned for cmsghdr.
    let mut space = [0u64; 8];
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives
    // sendmsg, and the control message is written within `space`, which
    // CMSG_SPACE of at most MAX_DESCRIPTORS descriptors fits.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if !descriptors.is_empty() {
            assert!(descriptors.len() <= MAX_DESCRIPTORS);
            let payload = std::mem::size_of_val(descriptors) as libc::c_uint;
            message.msg_control = space.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(payload) as _;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(payload) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr(),
                libc::CMSG_DATA(header).cast::<RawFd>(),
                descriptors.len(),
            );
        }
        match libc::sendmsg(control.as_raw_fd(), &message, libc::MSG_NOSIGNAL) {
            -1 => Err(io::Error::last_os_error()),
            sent => Ok(sent as usize),
        }
    }
}

fn receive_with_descriptors(
    control: &UnixStream,
    bytes: &mut [u8],
    descriptors: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut space = [0u64; 8];
    // SAFETY: as in `send_with_descriptors`; the kernel writes at most
    // `msg_controllen` bytes of control data into `space`, and each
    // descriptor it reports is new to this process and owned from here on.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = space.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&space) as _;
        let received = libc::recvmsg(control.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
        if received == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for index in 0..count {
                    descriptors.push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control message truncated",
            ));
        }
        Ok(received as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_adoption_commit_acknowledgement_keeps_unconsumed_replay() {
        let (successor, holder) = UnixStream::pair().unwrap();
        successor.shutdown(std::net::Shutdown::Read).unwrap();
        send(&successor, &Request::CommitAdoption, &[]).unwrap();
        let mut replay = Some(("screen".to_owned(), b"orphan-output".to_vec(), false));
        let expected = replay.clone();
        // An already reaped child has no pollable pidfd; the descriptor is ignored.
        let placeholder = File::open("/dev/null").unwrap();
        let result = attend(
            &holder,
            0,
            &placeholder,
            &mut Some(0),
            &mut None,
            &mut replay,
        );
        assert!(matches!(result, Attendance::Lost));
        assert_eq!(replay, expected);

        let (successor, holder) = UnixStream::pair().unwrap();
        send(&successor, &Request::CommitAdoption, &[]).unwrap();
        send(&successor, &Request::Release, &[]).unwrap();
        let result = attend(
            &holder,
            0,
            &placeholder,
            &mut Some(0),
            &mut None,
            &mut replay,
        );
        assert!(matches!(result, Attendance::Release));
        assert!(matches!(
            receive::<Report>(&successor).unwrap(),
            Some((Report::AdoptionCommitted, _))
        ));
        assert!(replay.is_none());
    }

    #[test]
    fn registered_orphans_keep_live_and_completed_results_without_a_deadline() {
        let now = Instant::now();
        assert_eq!(orphan_deadline(false, now), Some(now + ADOPTION_GRACE));
        assert_eq!(orphan_deadline(true, now), None);
        assert_eq!(orphan_deadline(true, now + ADOPTION_GRACE * 100), None);
    }

    #[test]
    fn an_adopter_that_stops_reading_cannot_hold_the_output_loop_forever() {
        let (successor, holder) = UnixStream::pair().unwrap();
        let limit: libc::c_int = 4096;
        // Keep the kernel buffer much smaller than the bounded replay frame.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    holder.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&limit as *const libc::c_int).cast(),
                    std::mem::size_of_val(&limit) as _,
                )
            },
            0
        );
        send(
            &successor,
            &Request::Attach {
                protocol: PROTOCOL,
                recovery: None,
            },
            &[],
        )
        .unwrap();
        let descriptor = tempfile::tempfile().unwrap();
        let started = Instant::now();
        let error = hand_over(
            &holder,
            descriptor.as_raw_fd(),
            1,
            &descriptor,
            None,
            "",
            &vec![b'x'; DETACHED_OUTPUT],
            None,
            &mut None,
            false,
            &pin_launch_directory(&command("true")).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "descriptor frame deadline expired");
        assert!(started.elapsed() >= HANDOVER_TIMEOUT);
        assert!(started.elapsed() < HANDOVER_TIMEOUT * 2 + Duration::from_secs(1));
    }

    fn command(script: &str) -> CommandBuilder {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        command.cwd("/");
        command
    }

    fn size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn read_until(master: &PtyMaster, needle: &str) -> String {
        let mut file = File::from(master.duplicate().unwrap());
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !String::from_utf8_lossy(&output).contains(needle) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "did not receive {needle:?}: {}",
                String::from_utf8_lossy(&output)
            );
            let mut ready = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Bound test fixture waits even when a shell defers its signal trap.
            let polled = unsafe {
                libc::poll(
                    &mut ready,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if polled == 0 {
                continue;
            }
            if polled < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("poll fixture: {}", io::Error::last_os_error());
            }
            match file.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => output.extend_from_slice(&buffer[..read]),
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    fn holder_pid(held: &Held) -> u32 {
        held.holder.process.as_ref().unwrap().child.id()
    }

    fn alive(pid: libc::pid_t) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let state = stat
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .next()?
                    .to_owned();
                Some(state != "Z" && state != "X")
            })
            .unwrap_or(false)
    }

    fn wait_gone(pid: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(
                Instant::now() < deadline,
                "process {pid} outlived its terminal"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_message_carries_its_descriptors() {
        let (left, right) = UnixStream::pair().unwrap();
        let (read, write) = std::io::pipe().unwrap();
        send(
            &left,
            &Report::Started {
                protocol: PROTOCOL,
                pid: 7,
                launch_directory: None,
            },
            &[write.as_raw_fd()],
        )
        .unwrap();
        send(&left, &Report::Exited { wait_status: 3 }, &[]).unwrap();
        drop(write);

        let (report, descriptors) = receive::<Report>(&right).unwrap().unwrap();
        assert!(matches!(report, Report::Started { pid: 7, .. }));
        let [passed]: [OwnedFd; 1] = descriptors.try_into().unwrap();
        let mut passed = std::io::PipeWriter::from(passed);
        std::io::Write::write_all(&mut passed, b"through").unwrap();
        drop(passed);
        let mut text = String::new();
        (&read).read_to_string(&mut text).unwrap();
        assert_eq!(text, "through");

        let (report, descriptors) = receive::<Report>(&right).unwrap().unwrap();
        assert!(matches!(report, Report::Exited { wait_status: 3 }));
        assert!(descriptors.is_empty());
        drop(left);
        assert!(receive::<Report>(&right).unwrap().is_none());
    }

    #[test]
    fn pinned_directory_before_launch_survives_replacement_for_relative_program_and_path() {
        const CHILD: &str = "CAGENTS_PINNED_DIRECTORY_CASE";
        const PRESENCE: &str = "CAGENTS_PINNED_DIRECTORY_AUTH_PRESENT";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let stdout_path = directory.path().join("stdout.log");
            let stderr_path = directory.path().join("stderr.log");
            let present = std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::worker::holder::tests::pinned_directory_before_launch_survives_replacement_for_relative_program_and_path", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1").env(PRESENCE, present.to_string())
                .stdin(Stdio::null())
                .stdout(File::create(&stdout_path).unwrap())
                .stderr(File::create(&stderr_path).unwrap()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    break child.wait().unwrap();
                }
                thread::sleep(Duration::from_millis(10));
            };
            let stdout = std::fs::read_to_string(&stdout_path).unwrap();
            let stderr = std::fs::read_to_string(&stderr_path).unwrap();
            print!("{stdout}");
            eprint!("{stderr}");
            if !status.success() {
                eprintln!("saved exact child logs: {}", directory.keep().display());
            }
            assert!(status.success(), "isolated pinned launch control failed");
            return;
        }
        assert_eq!(
            std::env::var(PRESENCE).unwrap(),
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                .is_some()
                .to_string()
        );
        let original_cwd = File::open(".").unwrap();
        for relative_path in [false, true] {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().unwrap();
            let original = directory.path().join("launch");
            let moved = directory.path().join("moved");
            std::fs::create_dir_all(original.join("bin")).unwrap();
            std::fs::write(original.join("payload"), "ORIGINAL_PAYLOAD\n").unwrap();
            let program = original.join("bin/probe");
            std::fs::write(&program, "#!/bin/sh\n/usr/bin/stat -Lc 'PIN_ID:%d:%i' .\n/bin/cat payload\nprintf 'PIN_OK\\n'\n").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut command = CommandBuilder::new(if relative_path {
                "probe"
            } else {
                "./bin/probe"
            });
            command.cwd(&original);
            command.env("PATH", "bin:/usr/bin:/bin");
            let pin = pin_launch_directory(&command).unwrap();
            std::fs::rename(&original, &moved).unwrap();
            std::fs::create_dir_all(original.join("bin")).unwrap();
            std::fs::write(original.join("payload"), "REPLACEMENT_PAYLOAD\n").unwrap();
            std::fs::write(
                original.join("bin/probe"),
                "#!/bin/sh\nprintf 'REPLACEMENT_EXEC\\n'\n",
            )
            .unwrap();
            std::fs::set_permissions(
                original.join("bin/probe"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            // This is a dedicated exact self-exec case; never a UI/daemon cwd.
            enter_launch_directory(&mut command, &pin).unwrap();
            let pair = native_pty_system().openpty(size()).unwrap();
            let mut child = pair.slave.spawn_command(command).unwrap();
            drop(pair.slave);
            let master = pair.master.as_raw_fd().unwrap();
            let duplicate = unsafe { libc::fcntl(master, libc::F_DUPFD_CLOEXEC, 0) };
            assert!(duplicate >= 0);
            let master = PtyMaster(unsafe { OwnedFd::from_raw_fd(duplicate) });
            let output = read_until(&master, "PIN_OK");
            assert!(
                output.contains(&format!("PIN_ID:{}:{}", pin.0.device, pin.0.inode)),
                "{output}"
            );
            assert!(output.contains("ORIGINAL_PAYLOAD"), "{output}");
            assert!(!output.contains("REPLACEMENT"), "{output}");
            assert_eq!(child.wait().unwrap().exit_code(), 0);
            assert_eq!(
                crate::agent_execution::DirectoryIdentity::from_metadata(
                    &pin.1.metadata().unwrap()
                ),
                pin.0
            );
            assert_eq!(unsafe { libc::fchdir(original_cwd.as_raw_fd()) }, 0);
        }
    }

    #[test]
    fn adopted_terminal_transfers_the_same_original_launch_directory_and_child() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("launch");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("payload"), "ORIGINAL_PAYLOAD\n").unwrap();
        let mut command = command(
            "echo started >> launches; /usr/bin/stat -Lc 'PIN_ID:%d:%i' .; echo ready; read line; /bin/cat payload; echo PIN_DONE",
        );
        command.cwd(&original);
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let mut held = spawn(command, size(), exits).unwrap();
        let pin = held.launch_directory.as_ref().unwrap().clone();
        let child_pid = held.child.pid();
        assert!(
            read_until(&held.master, "ready")
                .contains(&format!("PIN_ID:{}:{}", pin.0.device, pin.0.inode))
        );
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("payload"), "REPLACEMENT_PAYLOAD\n").unwrap();
        let socket = directory.path().join("holder.sock");
        held.holder.detach(&socket, b"original screen").unwrap();
        drop(held);
        let (exits, exited) = crossbeam_channel::bounded(1);
        let adopted = adopt(&socket, exits).unwrap();
        assert_eq!(adopted.held.child.pid(), child_pid);
        let adopted_pin = adopted.held.launch_directory.as_ref().unwrap();
        assert_eq!(adopted_pin.0, pin.0);
        assert_eq!(
            crate::agent_execution::DirectoryIdentity::from_metadata(
                &adopted_pin.1.metadata().unwrap()
            ),
            pin.0
        );
        assert_ne!(
            crate::agent_execution::DirectoryIdentity::from_metadata(
                &std::fs::metadata(&original).unwrap()
            ),
            pin.0
        );
        assert_ne!(
            unsafe { libc::fcntl(adopted_pin.1.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut writer = File::from(adopted.held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"continue\n").unwrap();
        let output = read_until(&adopted.held.master, "PIN_DONE");
        assert!(output.contains("ORIGINAL_PAYLOAD"), "{output}");
        assert!(!output.contains("REPLACEMENT_PAYLOAD"));
        assert_eq!(
            exited
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .exit_code(),
            0
        );
        drop(adopted);
        assert_eq!(
            std::fs::read_to_string(moved.join("launches")).unwrap(),
            "started\n"
        );
        assert_eq!(
            crate::agent_execution::DirectoryIdentity::from_metadata(&pin.1.metadata().unwrap()),
            pin.0
        );
    }

    #[test]
    fn anonymous_malformed_pin_refusal_never_releases_the_original() {
        const CHILD: &str = "CAGENTS_ANONYMOUS_PIN_REFUSAL_CASE";
        const PRESENCE: &str = "CAGENTS_ANONYMOUS_PIN_REFUSAL_AUTH_PRESENT";
        if std::env::var_os(CHILD).is_none() {
            // A pre-fix refusal could signal the socket peer. Keep that peer
            // in an exact self-exec child, never the parent test process.
            let directory = tempfile::tempdir().unwrap();
            let stdout_path = directory.path().join("stdout.log");
            let stderr_path = directory.path().join("stderr.log");
            let present = std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::worker::holder::tests::anonymous_malformed_pin_refusal_never_releases_the_original", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1").env(PRESENCE, present.to_string())
                .stdin(Stdio::null())
                .stdout(File::create(&stdout_path).unwrap())
                .stderr(File::create(&stderr_path).unwrap()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    break child.wait().unwrap();
                }
                thread::sleep(Duration::from_millis(10));
            };
            let stdout = std::fs::read_to_string(&stdout_path).unwrap();
            let stderr = std::fs::read_to_string(&stderr_path).unwrap();
            print!("{stdout}");
            eprint!("{stderr}");
            if !status.success() {
                eprintln!("saved exact child logs: {}", directory.keep().display());
            }
            assert!(status.success(), "anonymous pin refusal control failed");
            return;
        }
        assert_eq!(
            std::env::var(PRESENCE).unwrap(),
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                .is_some()
                .to_string()
        );
        let directory = tempfile::tempdir().unwrap();
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(
            command("echo ready; read line; echo ORIGINAL_SURVIVED"),
            size(),
            exits,
        )
        .unwrap();
        assert!(read_until(&held.master, "ready").contains("ready"));
        let original_pid = held.child.pid();
        let master = held.master.duplicate().unwrap();
        let pidfd = held.child.pidfd.try_clone().unwrap();
        let invalid_directory = tempfile::tempfile().unwrap();
        let expected = pin_launch_directory(&command("true")).unwrap().0;
        let socket = directory.path().join("anonymous.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (request, rights) = receive::<Request>(&control).unwrap().unwrap();
            assert!(matches!(
                request,
                Request::Attach {
                    protocol: PROTOCOL,
                    recovery: None
                }
            ));
            assert!(rights.is_empty());
            crate::descriptor_frame::send_json_with_fds_until(
                &control,
                &Report::Attached {
                    protocol: PROTOCOL,
                    pid: original_pid,
                    exited: None,
                    screen: String::new(),
                    output: String::new(),
                    screen_lost: false,
                    launch_directory: Some(expected),
                },
                &[master.as_fd(), pidfd.as_fd(), invalid_directory.as_fd()],
                Instant::now() + Duration::from_secs(5),
                MAX_FRAME as usize,
                3,
                crate::descriptor_frame::FrameByteOrder::LittleEndian,
            )
            .unwrap();
            // EOF proves the refused setup dropped the connection without
            // sending Release/ReleaseProved or any other control request.
            assert!(receive::<Request>(&control).unwrap().is_none());
        });
        let (refused_exits, _refused_exited) = crossbeam_channel::bounded(1);
        let error = match adopt(&socket, refused_exits) {
            Ok(_) => panic!("regular file accepted as a launch directory"),
            Err(error) => error,
        };
        assert!(error.contains("launch directory"), "{error}");
        server.join().unwrap();
        assert!(alive(original_pid));
        assert!(!exits_within(&held.child.pidfd, Duration::ZERO).unwrap());
        let mut input = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut input, b"continue\n").unwrap();
        assert!(read_until(&held.master, "ORIGINAL_SURVIVED").contains("ORIGINAL_SURVIVED"));
        assert_eq!(
            exited
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .exit_code(),
            0
        );
        drop(held);
    }

    #[test]
    fn legacy_and_malformed_launch_directory_descriptors_never_become_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let file = File::open(directory.path()).unwrap();
        let identity =
            crate::agent_execution::DirectoryIdentity::from_metadata(&file.metadata().unwrap());
        let fds = |with_directory: bool| {
            let mut descriptors = vec![
                OwnedFd::from(tempfile::tempfile().unwrap()),
                OwnedFd::from(tempfile::tempfile().unwrap()),
            ];
            if with_directory {
                descriptors.push(OwnedFd::from(file.try_clone().unwrap()));
            }
            descriptors
        };
        for protocol in 2..=4 {
            assert!(
                decode_held_descriptors(protocol, None, fds(false))
                    .unwrap()
                    .2
                    .is_none()
            );
            assert!(decode_held_descriptors(protocol, Some(identity), fds(true)).is_err());
        }
        for unsupported in [0, 1, 6] {
            assert!(decode_held_descriptors(unsupported, Some(identity), fds(true)).is_err());
        }
        assert!(decode_held_descriptors(5, None, fds(true)).is_err());
        assert!(decode_held_descriptors(5, Some(identity), fds(false)).is_err());
        let wrong = crate::agent_execution::DirectoryIdentity {
            inode: identity.inode.wrapping_add(1),
            ..identity
        };
        assert!(decode_held_descriptors(5, Some(wrong), fds(true)).is_err());
        let mut regular = fds(false);
        regular.push(OwnedFd::from(tempfile::tempfile().unwrap()));
        assert!(decode_held_descriptors(5, Some(identity), regular).is_err());
        assert_eq!(
            decode_held_descriptors(5, Some(identity), fds(true))
                .unwrap()
                .2
                .unwrap()
                .0,
            identity
        );
        let not_cloexec = File::open(directory.path()).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(not_cloexec.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        assert!(checked_launch_directory(identity, Arc::new(not_cloexec)).is_err());
    }

    #[test]
    fn an_invalid_explicit_launch_directory_refuses_before_any_root_starts() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started");
        for cwd in [
            directory.path().join("missing"),
            directory.path().join("regular"),
        ] {
            if cwd.ends_with("regular") {
                std::fs::write(&cwd, b"not a directory").unwrap();
            }
            let mut command = command(r#"printf started > "$PIN_STARTED""#);
            command.env("PIN_STARTED", &marker);
            command.cwd(&cwd);
            let (exits, _exited) = crossbeam_channel::bounded(1);
            let error = match spawn(command, size(), exits) {
                Ok(_) => panic!("invalid cwd launched a root"),
                Err(error) => error,
            };
            assert!(error.contains("pin terminal launch directory"), "{error}");
            assert!(!marker.exists());
        }
        // Truly absent cwd retains portable-pty's selected HOME fallback.
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "echo HOME_PIN_READY; read line"]);
        command.env("HOME", directory.path());
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(command, size(), exits).unwrap();
        assert_eq!(
            held.launch_directory.as_ref().unwrap().0,
            crate::agent_execution::DirectoryIdentity::from_metadata(
                &std::fs::metadata(directory.path()).unwrap()
            )
        );
        read_until(&held.master, "HOME_PIN_READY");
        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"\n").unwrap();
        assert_eq!(
            exited
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .exit_code(),
            0
        );
    }

    #[test]
    fn a_held_root_runs_on_the_passed_pty_and_reports_its_exit() {
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(command("stty size; read line; exit 7"), size(), exits).unwrap();
        assert!(held.child.unreaped());
        crate::daemon::process_scope::tests::assert_separate(
            holder_pid(&held),
            held.child.pid() as u32,
        );
        assert!(read_until(&held.master, "24 80").contains("24 80"));

        held.master
            .resize(PtySize {
                rows: 30,
                cols: 100,
                ..size()
            })
            .unwrap();
        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"\n").unwrap();

        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 7);
        assert!(!held.child.unreaped());
    }

    #[test]
    fn crash_redraw_notifies_the_live_foreground_without_changing_dimensions() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let held = spawn(
            command("trap 'echo redraw; stty size' WINCH; echo ready; while :; do read line; done"),
            size(),
            exits,
        )
        .unwrap();
        read_until(&held.master, "ready");
        held.master.request_redraw().unwrap();
        // Some shells defer traps while their read builtin waits for a whole
        // line. Release that wait; only SIGWINCH can print the redraw marker.
        let mut input = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut input, b"\n").unwrap();
        let output = read_until(&held.master, "24 80");
        assert!(output.contains("redraw"));
        let mut actual: libc::winsize = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::ioctl(held.master.as_raw_fd(), libc::TIOCGWINSZ, &mut actual) },
            0
        );
        assert_eq!(
            (
                actual.ws_col,
                actual.ws_row,
                actual.ws_xpixel,
                actual.ws_ypixel
            ),
            (
                size().cols,
                size().rows,
                size().pixel_width,
                size().pixel_height
            )
        );
    }

    #[test]
    fn the_root_leads_its_own_session_apart_from_its_holder() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let held = spawn(command("read line"), size(), exits).unwrap();
        let session = |pid: u32| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            (
                fields[1].parse::<i32>().unwrap(),
                fields[3].parse::<i32>().unwrap(),
            )
        };
        let holder = holder_pid(&held);
        let (holder_parent, holder_session) = session(holder);
        assert_eq!(holder_parent, std::process::id() as i32);
        assert_eq!(holder_session, holder as i32);
        let root = held.child.pid();
        let (root_parent, root_session) = session(root as u32);
        assert_eq!(root_parent, holder as i32);
        assert_eq!(root_session, root);
    }

    #[test]
    fn same_parent_detach_adopt_close_reaps_the_original_holder_without_other_waits() {
        let reaper = crate::daemon::child_reaper::TestService::new(1);
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        struct Unrelated(Child);
        impl Drop for Unrelated {
            fn drop(&mut self) {
                if !matches!(self.0.try_wait(), Ok(Some(_))) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let mut unrelated = Unrelated(
            Command::new("/bin/sh")
                .args(["-c", "exit 23"])
                .spawn()
                .unwrap(),
        );
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let mut held = spawn_reserved(
            command("echo ready; read line"),
            size(),
            exits,
            None,
            reaper.reserve().unwrap(),
        )
        .unwrap();
        read_until(&held.master, "ready");
        let owner = registry::Process::read(holder_pid(&held) as i32).unwrap();
        let root = registry::Process::read(held.child.pid()).unwrap();
        assert_eq!(reaper.free(), 0);
        let socket = directory.path().join("custody.sock");
        held.holder.detach(&socket, b"screen").unwrap();
        drop(held);
        assert!(owner.is_current());
        assert!(root.is_current());
        assert_eq!(reaper.retained_custody_error(), None);
        assert_eq!(
            reaper.free(),
            0,
            "live original child still owns its spawn credit"
        );
        let (exits, exited) = crossbeam_channel::bounded(1);
        let adopted = adopt(&socket, exits).unwrap().held;
        assert!(
            adopted.holder.process.is_none(),
            "adoption must not invent original wait authority"
        );
        assert_eq!(adopted.child.pid(), root.pid);
        adopted.child.kill().unwrap();
        exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        drop(adopted);
        reaper.await_free(1);
        assert!(
            !owner.is_current(),
            "same-parent holder was observed but not reaped"
        );
        assert!(
            !root.is_current(),
            "the terminal holder did not reap its own root"
        );
        assert_eq!(
            unrelated.0.wait().unwrap().code(),
            Some(23),
            "another owner's child status was consumed"
        );
    }

    #[test]
    fn detached_holder_transfer_failure_preserves_custody_until_explicit_exact_retry() {
        let reaper = crate::daemon::child_reaper::TestService::new(1);
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let mut held = spawn_reserved(
            command("echo ready; read line"),
            size(),
            exits,
            None,
            reaper.reserve().unwrap(),
        )
        .unwrap();
        read_until(&held.master, "ready");
        let owner = registry::Process::read(holder_pid(&held) as i32).unwrap();
        let root = registry::Process::read(held.child.pid()).unwrap();
        let socket = directory.path().join("retry.sock");
        held.holder.detach(&socket, b"screen").unwrap();
        reaper.fail_wake(true);
        drop(held);
        assert!(reaper.is_suspended(owner.pid as u32));
        assert!(
            reaper
                .retained_custody_error()
                .unwrap()
                .contains("injected")
        );
        assert_eq!(reaper.free(), 0);
        assert!(
            reaper.reserve().is_err(),
            "failed handoff must not release admission credit"
        );
        assert!(owner.is_current() && root.is_current());
        // The failure preserves the helper without signalling it. A real
        // successor can still use its socket, close its root, and release it.
        let (exits, exited) = crossbeam_channel::bounded(1);
        let adopted = adopt(&socket, exits).unwrap().held;
        assert!(adopted.holder.process.is_none());
        let exact_exit = adopted.holder.pidfd.as_ref().unwrap().try_clone().unwrap();
        adopted.child.kill().unwrap();
        exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        drop(adopted);
        assert!(exits_within(&exact_exit, Duration::from_secs(5)).unwrap());
        assert!(
            owner.is_current(),
            "suspended service must not silently reap before recovery"
        );
        assert!(reaper.is_suspended(owner.pid as u32));
        reaper.fail_wake(false);
        let retry = reaper.retry_preserved().unwrap();
        assert_eq!((retry.rearmed, retry.retained), (1, 0));
        assert_eq!(reaper.retained_custody_error(), None);
        reaper.await_free(1);
        assert!(!owner.is_current() && !root.is_current());
        assert!(reaper.reserve().is_ok());
    }

    #[test]
    fn failed_terminal_holder_spawn_returns_its_preallocated_child_credit() {
        let reaper = crate::daemon::child_reaper::TestService::new(1);
        let directory = tempfile::tempdir().unwrap();
        let reservation = reaper.reserve().unwrap();
        assert_eq!(reaper.free(), 0);
        let result = spawn_original(
            &mut Command::new(directory.path().join("missing-holder")),
            reservation,
        );
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::NotFound));
        assert_eq!(reaper.free(), 1);
        assert!(reaper.reserve().is_ok());
    }

    #[test]
    fn normal_terminal_holder_close_returns_its_original_child_credit() {
        let reaper = crate::daemon::child_reaper::TestService::new(1);
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn_reserved(
            command("echo ready; read line"),
            size(),
            exits,
            None,
            reaper.reserve().unwrap(),
        )
        .unwrap();
        read_until(&held.master, "ready");
        let owner = registry::Process::read(holder_pid(&held) as i32).unwrap();
        let root = registry::Process::read(held.child.pid()).unwrap();
        assert_eq!(reaper.free(), 0);
        // Normal worker shutdown waits for the holder's post-reap root report
        // before releasing the holder. Raw Held drop only promises a hangup.
        let root_identity = super::super::RootIdentity::capture(root.pid, None).unwrap();
        super::super::shutdown_child_exit(&held.child, Some(&root_identity), &exited, false)
            .expect("normal shutdown must confirm the exact root was reaped");
        assert!(!held.child.unreaped());
        assert_eq!(
            reaper.free(),
            0,
            "the original holder still owns its credit"
        );
        drop(held);
        assert_eq!(reaper.free(), 1);
        assert!(!owner.is_current());
        let deadline = Instant::now() + Duration::from_secs(5);
        while root.is_current() {
            assert!(
                Instant::now() < deadline,
                "normal holder close left its root"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn releasing_a_holder_hangs_up_its_terminal() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let Held {
            master,
            child,
            holder,
            ..
        } = spawn(command("while :; do sleep 1; done"), size(), exits).unwrap();
        let root = child.pid();
        let holder_pid = holder.process.as_ref().unwrap().child.id() as libc::pid_t;
        // The terminal hangs up once its last master closes.
        drop(master);
        drop(holder);
        assert!(!alive(holder_pid));
        wait_gone(root);
    }

    #[test]
    fn a_holder_whose_daemon_vanishes_releases_itself() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let Held {
            master,
            child,
            mut holder,
            ..
        } = spawn(command("while :; do sleep 1; done"), size(), exits).unwrap();
        let root = child.pid();
        let mut process = holder.process.take().unwrap();
        // A daemon that dies sends nothing; its descriptors simply close.
        drop(master);
        let _ = holder.control.shutdown(std::net::Shutdown::Both);
        drop(holder);
        let status = process.child.wait().unwrap();
        assert_eq!(status.code(), Some(0));
        wait_gone(root);
    }

    #[test]
    fn registered_holder_survives_sigkill_and_repeated_adoption_without_restarting_root() {
        const CHILD: &str = "CAGENTS_HOLDER_CRASH_FIXTURE";
        const COORDINATOR: &str = "CAGENTS_HOLDER_CRASH_COORDINATOR";
        if std::env::var_os(CHILD).is_none() && std::env::var_os(COORDINATOR).is_none() {
            // Receipt validation deliberately discovers the caller's runtime
            // and actual exclusive lease. Isolate the whole coordinator rather
            // than changing process-global environment in a parallel test.
            let directory = tempfile::Builder::new()
                .prefix("cagents-holder-close-long-runtime-xxxxxxxxxx-")
                .tempdir_in("/var/tmp")
                .unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::worker::holder::tests::registered_holder_survives_sigkill_and_repeated_adoption_without_restarting_root", "--nocapture"])
                .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", directory.path())
                .env("XDG_RUNTIME_DIR", directory.path()).env(COORDINATOR, directory.path())
                .status().unwrap();
            assert!(
                status.success(),
                "isolated holder coordinator failed: {status}"
            );
            return;
        }
        if let Some(ready) = std::env::var_os(CHILD) {
            let proof = crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: crate::model::HostIncarnation::new(
                    uuid::Uuid::new_v4().to_string(),
                ),
            };
            let paths = crate::daemon::RuntimePaths::discover().unwrap();
            let recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
            let source = crate::model::TerminalSession {
                id: crate::model::TerminalId::new(),
                launch: recipe,
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                transcript_path: None,
                resume_hint: None,
                continuity: crate::model::TerminalContinuity::DetachedSameBoot {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: proof.host_incarnation.clone(),
                },
            };
            let recovery = Recovery {
                source,
                previous_sources: Vec::new(),
                relaunches: Vec::new(),
                options: crate::terminal::TerminalSpawnOptions::default(),
                execution_environment: None,
                execution_recipe: None,
            };
            let (exits, _exited) = crossbeam_channel::bounded(1);
            assert!(
                spawn_recoverable(command("exit 0"), size(), exits, Some(recovery.clone()))
                    .is_err(),
                "a complete proof without the daemon lease cannot register a holder"
            );
            let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
            let (exits, _exited) = crossbeam_channel::bounded(1);
            let held = spawn_recoverable(
                command("echo ready; while read line; do if [ \"$line\" = finish ]; then echo completed-result; exit 7; fi; echo still-running:$line; done"),
                size(),
                exits,
                Some(recovery),
            )
            .unwrap();
            read_until(&held.master, "ready");
            crate::daemon::process_scope::tests::assert_separate(
                holder_pid(&held),
                held.child.pid() as u32,
            );
            held.master
                .resize(PtySize {
                    cols: 120,
                    rows: 40,
                    ..size()
                })
                .unwrap();
            std::fs::write(ready, "ready").unwrap();
            loop {
                thread::park();
            }
        }

        struct Cleanup {
            daemon: Child,
            holder: Option<File>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.daemon.kill();
                let _ = self.daemon.wait();
                if let Some(holder) = &self.holder {
                    let _ = signal(holder, libc::SIGKILL);
                }
            }
        }
        let directory = PathBuf::from(std::env::var_os(COORDINATOR).expect("isolated coordinator"));
        let ready = directory.as_path().join("ready");
        let daemon = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "terminal::worker::holder::tests::registered_holder_survives_sigkill_and_repeated_adoption_without_restarting_root", "--nocapture"])
            .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", directory.as_path())
            .env("XDG_RUNTIME_DIR", directory.as_path()).env(CHILD, &ready)
            .spawn().unwrap();
        let mut cleanup = Cleanup {
            daemon,
            holder: None,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                cleanup.daemon.try_wait().unwrap().is_none(),
                "fixture exited before acknowledgement"
            );
            assert!(
                Instant::now() < deadline,
                "fixture did not acknowledge its session"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let holders = directory
            .as_path()
            .join(crate::instance::RUNTIME_SUBDIRECTORY)
            .join("holders");
        let boot = crate::daemon::read_boot_id().unwrap();
        let records = registry::discover(&holders, &boot).unwrap();
        assert_eq!(
            records.len(),
            1,
            "discovery must exist before startup is acknowledged"
        );
        let record = records[0].clone();
        cleanup.holder = Some(open_pidfd(record.holder.pid).unwrap());
        cleanup.daemon.kill().unwrap();
        cleanup.daemon.wait().unwrap();
        assert!(record.holder.is_current());
        assert!(record.root.is_current());
        let runtime = directory
            .as_path()
            .join(crate::instance::RUNTIME_SUBDIRECTORY);
        let paths = crate::daemon::RuntimePaths {
            directory: runtime.clone(),
            socket: runtime.join(crate::terminal::ipc::SOCKET_FILE_NAME),
            lock: runtime.join("cagentsd.lock"),
        };
        let replacement = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("replacement"),
            boot_id: boot.clone(),
            host_incarnation: crate::model::HostIncarnation::new("replacement"),
        };
        let socket = registry::socket(&holders, record.terminal);
        assert!(
            inspect(&socket, &record, &replacement).is_err(),
            "same uid without the daemon lease is not adoption authority"
        );
        let lease = crate::daemon::HostLease::acquire(&paths, &replacement).unwrap();
        let mut reused = record.clone();
        reused.holder.start += 1;
        assert!(
            inspect(&socket, &reused, &replacement).is_err(),
            "a reused PID is not the recorded holder"
        );
        let (mut recovery, screen_lost) = inspect(&socket, &record, &replacement).unwrap();
        assert!(screen_lost);
        assert_eq!((recovery.options.columns, recovery.options.rows), (120, 40));
        let original = recovery.source.clone();
        recovery.previous_sources.push(original.clone());
        recovery.source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: crate::model::HostSessionId::new("replacement"),
            boot_id: boot.clone(),
            host_incarnation: crate::model::HostIncarnation::new("replacement"),
        };
        let mut changed = recovery.clone();
        changed.source.launch.arguments.push("different".into());
        let (exits, _exited) = crossbeam_channel::bounded(1);
        assert!(adopt_recoverable(&socket, exits, Some(changed)).is_err());
        assert!(
            record.root.is_current(),
            "refusing a changed launch must not release the holder"
        );
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let adopted = adopt_recoverable(&socket, exits, Some(recovery.clone())).unwrap();
        assert_eq!(adopted.held.child.pid(), record.root.pid);
        assert!(adopted.screen_lost);
        let Held {
            master,
            child,
            mut holder,
            ..
        } = adopted.held;
        holder.commit_adoption().unwrap();
        let mut writer = File::from(master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"first\n").unwrap();
        assert!(read_until(&master, "still-running:first").contains("still-running:first"));
        drop(master);
        drop(child);
        // Simulate the next daemon's abrupt descriptor loss without Release.
        holder.detached = true;
        holder.control.shutdown(std::net::Shutdown::Both).unwrap();
        drop(holder);
        std::io::Write::write_all(&mut writer, b"rollback-replay\n").unwrap();
        drop(writer);
        drop(lease);
        let third = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("third"),
            boot_id: boot.clone(),
            host_incarnation: crate::model::HostIncarnation::new("third"),
        };
        let lease = crate::daemon::HostLease::acquire(&paths, &third).unwrap();
        let (mut next, _) = inspect(&socket, &record, &third).unwrap();
        assert_eq!(next.previous_sources, vec![original.clone()]);
        next.previous_sources.push(next.source.clone());
        next.source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: crate::model::HostSessionId::new("third"),
            boot_id: boot.clone(),
            host_incarnation: crate::model::HostIncarnation::new("third"),
        };
        // The same successor can retry the identical allocation under its
        // existing lease, without fabricating a self-alias after a lost ACK.
        let retry_source = next.source.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (exits, _) = crossbeam_channel::bounded(1);
            let attempt = adopt_recoverable(&socket, exits, Some(next.clone())).unwrap();
            let replayed =
                String::from_utf8_lossy(&attempt.output).contains("still-running:rollback-replay");
            // No commit: dropping failed parser setup must preserve every byte
            // already returned in Attached as well as the surviving process.
            drop(attempt);
            assert_eq!(next.source, retry_source);
            assert_eq!(
                crate::daemon::holder_registry::recovery_candidate(
                    &socket,
                    crate::daemon::holder_registry::Kind::Terminal,
                )
                .unwrap(),
                Some(record.clone()),
                "adoption rewrote the original holder role/identity"
            );
            if replayed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "orphan never buffered the fixture output"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let (exits, exited) = crossbeam_channel::bounded(1);
        let mut adopted = adopt_recoverable(&socket, exits, Some(next)).unwrap();
        assert!(String::from_utf8_lossy(&adopted.output).contains("still-running:rollback-replay"));
        assert_eq!(adopted.held.child.pid(), record.root.pid);
        adopted.held.holder.commit_adoption().unwrap();
        let mut writer = File::from(adopted.held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"finish\n").unwrap();
        assert_eq!(
            exited
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .exit_code(),
            7
        );
        drop(writer);
        adopted.held.holder.detached = true;
        adopted
            .held
            .holder
            .control
            .shutdown(std::net::Shutdown::Both)
            .unwrap();
        drop(adopted);
        drop(lease);
        let final_proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("completed"),
            boot_id: boot.clone(),
            host_incarnation: crate::model::HostIncarnation::new("completed"),
        };
        let _lease = crate::daemon::HostLease::acquire(&paths, &final_proof).unwrap();
        let (mut completed, _) = inspect(&socket, &record, &final_proof).unwrap();
        completed.previous_sources.push(completed.source.clone());
        completed.source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: final_proof.host_session_id.clone(),
            boot_id: boot,
            host_incarnation: final_proof.host_incarnation.clone(),
        };
        let (exits, exited) = crossbeam_channel::bounded(1);
        let mut restored = adopt_recoverable(&socket, exits, Some(completed)).unwrap();
        assert_eq!(restored.held.child.pid(), record.root.pid);
        assert_eq!(
            exited
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .exit_code(),
            7
        );
        assert!(String::from_utf8_lossy(&restored.output).contains("completed-result"));
        restored.held.holder.commit_adoption().unwrap();
        // This fixture has observed the exact root exit and drained retained
        // output, equivalent to the completed worker's close disposition.
        restored.held.holder.prove_cleanup();
        drop(restored);
        wait_gone(record.root.pid);
        wait_gone(record.holder.pid);
        assert!(
            registry::discover(&holders, &crate::daemon::read_boot_id().unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(!socket.exists(), "explicit release removes its endpoint");
        assert!(registry::cleanup::confirmed(&original, &record, &final_proof).unwrap());
        let mut replacement = record.clone();
        replacement.holder.start += 1;
        assert!(
            !registry::cleanup::confirmed(&original, &replacement, &final_proof).unwrap(),
            "same terminal descriptor does not prove a different holder's cleanup"
        );
        let mut wrong = original.clone();
        wrong.launch.arguments.push("different".into());
        assert!(!registry::cleanup::confirmed(&wrong, &record, &final_proof).unwrap());
        drop(_lease);
        let retry = crate::model::HostSessionProof {
            host_incarnation: crate::model::HostIncarnation::new("close-retry"),
            ..final_proof
        };
        let _lease = crate::daemon::HostLease::acquire(&paths, &retry).unwrap();
        assert!(
            registry::cleanup::confirmed(&original, &record, &retry).unwrap(),
            "terminal proof survives the daemon loss between paired cleanup and native receipt confirmation"
        );
    }

    #[test]
    fn killing_a_held_root_hangs_it_up() {
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(
            command("trap 'exit 3' HUP; echo ready; while :; do sleep 1; done"),
            size(),
            exits,
        )
        .unwrap();
        read_until(&held.master, "ready");
        held.child.kill().unwrap();
        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 3);
        // A root that is already gone counts as hung up.
        held.child.kill().unwrap();
    }

    #[test]
    fn a_root_outliving_its_holder_reports_only_its_own_exit() {
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(command("echo ready; read line; exit 4"), size(), exits).unwrap();
        read_until(&held.master, "ready");
        let holder = holder_pid(&held) as libc::pid_t;
        // SAFETY: signals a child this test spawned.
        assert_eq!(unsafe { libc::kill(holder, libc::SIGKILL) }, 0);
        // The root keeps its terminal, so its exit is not the holder's.
        assert!(exited.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(held.child.unreaped());

        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"\n").unwrap();
        let error = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("exit status is unknown"), "{error}");
        drop(held);
    }

    /// Detaches a fresh terminal running `script` onto a socket in `directory`,
    /// once it printed "ready". Returns the socket and the root's pid.
    fn detached(script: &str, directory: &Path) -> (PathBuf, libc::pid_t) {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let Held {
            master,
            child,
            mut holder,
            ..
        } = spawn(command(script), size(), exits).unwrap();
        read_until(&master, "ready");
        let socket = directory.join("holder.sock");
        if let Err(error) = holder.detach(&socket, b"screen") {
            panic!("{error}");
        }
        // The daemon's descriptors close with it.
        (socket, child.pid())
    }

    #[test]
    fn a_detached_terminal_keeps_running_for_a_successor_with_its_output() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let gate = directory.path().join("detached-ready");
        let ready = directory.path().join("while-away-ready");
        let script = format!(
            "echo ready; while [ ! -f {} ]; do sleep 0.01; done; echo while-away; : > {}; read line; echo got:$line; exit 6",
            gate.display(),
            ready.display()
        );
        let (socket, root) = detached(&script, directory.path());
        // The initial PTY read cannot accidentally consume while-away output.
        std::fs::write(gate, b"").unwrap();
        // The marker follows the PTY write. The holder drains readable output
        // before answering adoption; elapsed wall time is not readiness.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture did not emit detached output"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(alive(root));

        let (exits, exited) = crossbeam_channel::bounded(1);
        let Adopted {
            held,
            screen,
            output,
            screen_lost,
        } = adopt(&socket, exits).unwrap();
        assert!(!screen_lost);
        assert_eq!(held.child.pid(), root);
        assert!(held.child.unreaped());
        assert_eq!(screen, b"screen");
        assert!(
            String::from_utf8_lossy(&output).contains("while-away"),
            "{output:?}"
        );
        // The holder no longer listens once adopted. It removes its endpoint
        // after answering, so its successor may look first.
        let deadline = Instant::now() + Duration::from_secs(5);
        while socket.exists() {
            assert!(
                Instant::now() < deadline,
                "the adopted holder kept its endpoint"
            );
            thread::sleep(Duration::from_millis(5));
        }

        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"hello\n").unwrap();
        assert!(read_until(&held.master, "got:hello").contains("got:hello"));
        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 6);
    }

    #[test]
    fn a_root_that_exits_while_detached_reports_to_its_successor() {
        let directory = tempfile::tempdir().unwrap();
        let (socket, root) = detached("echo ready; sleep 0.2; exit 9", directory.path());
        wait_gone(root);

        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = adopt(&socket, exits).unwrap().held;
        let status = exited
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 9);
        // Reported once: the holder does not report it again.
        assert!(exited.recv_timeout(Duration::from_millis(200)).is_err());
        drop(held);
    }

    #[test]
    fn releasing_an_adopted_holder_hangs_up_its_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let (socket, root) = detached("echo ready; while :; do sleep 1; done", directory.path());
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let held = adopt(&socket, exits).unwrap().held;
        let holder = held.holder.pidfd.as_ref().map(|pidfd| pidfd.as_raw_fd());
        assert!(holder.is_some());
        drop(held);
        wait_gone(root);
    }

    #[test]
    fn a_holder_can_be_detached_again_by_its_successor() {
        let mut directory = tempfile::tempdir().unwrap();
        // Preserve the exact detached socket/checkpoint namespace on success
        // and panic until the outer original-family guardian has settled it.
        directory.disable_cleanup(true);
        eprintln!(
            "Retained terminal carry fixture: {}",
            directory.path().display()
        );
        let (socket, root) = detached("echo ready; read line; exit 2", directory.path());
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let held = adopt(&socket, exits).unwrap().held;
        let Held { mut holder, .. } = held;
        if let Err(error) = holder.detach(&socket, b"again") {
            panic!("{error}");
        }
        let (exits, exited) = crossbeam_channel::bounded(1);
        let Adopted { held, screen, .. } = adopt(&socket, exits).unwrap();
        assert_eq!(held.child.pid(), root);
        // Each detach carries its own daemon's screen.
        assert_eq!(screen, b"again");
        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"\n").unwrap();
        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 2);
    }

    #[test]
    fn nothing_adopts_a_holder_that_is_not_listening() {
        let directory = tempfile::tempdir().unwrap();
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let error = match adopt(&directory.path().join("absent.sock"), exits) {
            Ok(_) => panic!("an absent holder must not be adopted"),
            Err(error) => error,
        };
        assert!(error.contains("absent.sock"), "{error}");
    }

    #[test]
    fn a_root_that_cannot_start_reports_why() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let error = match spawn(
            CommandBuilder::new("/nonexistent/cagents-holder-test"),
            size(),
            exits,
        ) {
            Ok(_) => panic!("a missing program must not start"),
            Err(error) => error,
        };
        assert!(
            error.starts_with("spawn /nonexistent/cagents-holder-test:"),
            "{error}"
        );
    }
}
