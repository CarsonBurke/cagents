//! Original UI peer -> one existing configured-profile window journal.
//! This proves the original UI audience only, never a journal row, directory
//! grant or start authority. The authenticated UI must answer a host challenge
//! using its existing original operation-store connection.
//! Call on a blocking source worker, outside HostState. No caller slot or
//! database pathname is accepted; this module never opens a StateStore.
use super::*;
use std::{os::fd::FromRawFd, time::Instant};

pub(crate) struct OriginalUiJournalPeer {
    image: File,
    slot: String,
    peer: File, // SO_PEERPIDFD: the ORIGINAL socket peer, never pidfd_open(pid)
    owner: VerifiedWindow,
    database: File,
    database_path: PathBuf,
}

impl OriginalUiJournalPeer {
    pub(crate) fn capture(stream: &UnixStream, deadline: Instant) -> io::Result<Self> {
        let runtime = runtime_directory()?;
        let image = crate::companion::Product::Ui.beside_current()?;
        Self::capture_in(stream, &runtime, &image, window_database_path, deadline)
    }
    fn capture_in(
        stream: &UnixStream,
        runtime: &Path,
        image: &Path,
        database_path: impl Fn(&str) -> io::Result<PathBuf>,
        deadline: Instant,
    ) -> io::Result<Self> {
        check_deadline(deadline)?;
        let peer = original_peer(stream)?;
        let pid = peer_pid(&peer)?;
        require_live(&peer)?;
        let process = Path::new("/proc").join(pid.to_string());
        let image = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(image)?;
        let expected = image.metadata()?;
        require_ui_role(&process)?;
        let actual = fs::metadata(process.join("exe"))?;
        if !expected.is_file() || identity(&expected) != identity(&actual) {
            return Err(refused(
                "The original journal peer is not this installation's UI executable",
            ));
        }
        // Existing launch fence excludes window-slot admission while matching
        // current ownership. It is released before returning, never retained
        // for the lifetime of a deferred directory grant.
        let _launch = owned_lock(
            &runtime.join(format!("{APPLICATION_LOCK_FILE}.update")),
            libc::LOCK_EX | libc::LOCK_NB,
        )?;
        require_owned_runtime_directory(runtime)?;
        let prefix = format!("{APPLICATION_LOCK_FILE}.");
        let mut matched = None;
        for (count, entry) in fs::read_dir(runtime)?.enumerate() {
            check_deadline(deadline)?;
            if count >= 4096 {
                return Err(refused("Window journal audience census exceeds its bound"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(slot) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
                continue;
            };
            if slot == "update" || validate_window_slot(slot).is_err() {
                continue;
            }
            let lease_path = entry.path();
            let lease = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&lease_path)?;
            let metadata = lease.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != current_uid()
                || metadata.nlink() != 1
                || metadata.mode() & 0o077 != 0
            {
                return Err(refused("Unsafe original UI slot lease"));
            }
            if !owns_exact_slot(&process, pid, &metadata, deadline)? {
                continue;
            }
            if matched.is_some() {
                return Err(refused("The original peer owns multiple window slots"));
            }
            matched = Some((
                slot.to_owned(),
                VerifiedWindow {
                    executable: crate::companion::Product::Ui.beside_current()?,
                    pidfd: peer.try_clone()?,
                    pid,
                    lease,
                    lease_path,
                },
            ));
        }
        let (slot, owner) =
            matched.ok_or_else(|| refused("The original peer has no authenticated window slot"))?;
        let database_path = database_path(&slot)?;
        let database = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&database_path)?;
        let metadata = database.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != current_uid()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err(refused(
                "The original window journal is not a private regular file",
            ));
        }
        // The slot role alone does not prove that this profile's database is
        // the peer's actual live store. Require its exact open DB inode too.
        if !owns_open_file(&process, &metadata, deadline)? {
            return Err(refused(
                "The original window peer does not hold this configured-profile journal",
            ));
        }
        let audience = Self {
            image,
            slot,
            peer,
            owner,
            database,
            database_path,
        };
        audience.validate(deadline)?;
        Ok(audience)
    }
    pub(crate) fn validate(&self, deadline: Instant) -> io::Result<()> {
        check_deadline(deadline)?;
        require_live(&self.peer)?;
        let pid = peer_pid(&self.peer)?;
        if pid != self.owner.pid {
            return Err(refused("Original journal peer identity changed"));
        }
        let process = Path::new("/proc").join(pid.to_string());
        let expected = self.owner.lease.metadata()?;
        require_private_regular(&expected)?;
        let named = fs::symlink_metadata(&self.owner.lease_path)?;
        if !named.is_file()
            || identity(&expected) != identity(&named)
            || !owns_exact_slot(&process, pid, &expected, deadline)?
        {
            return Err(refused("Original UI slot ownership changed"));
        }
        require_ui_role(&process)?;
        let image = self.image.metadata()?;
        let actual = fs::metadata(process.join("exe"))?;
        if identity(&image) != identity(&actual) {
            return Err(refused("Original UI peer executable changed"));
        }
        let named = fs::symlink_metadata(&self.database_path)?;
        let expected = self.database.metadata()?;
        require_private_regular(&expected)?;
        if !named.is_file()
            || identity(&named) != identity(&expected)
            || !owns_open_file(&process, &expected, deadline)?
        {
            return Err(refused(
                "Original configured-profile journal identity changed",
            ));
        }
        require_live(&self.peer)?;
        check_deadline(deadline)
    }
    /// This proves the original process/window audience only. It grants no
    /// journal row read: SQLite pathname probes cannot bind another connection
    /// to the held database. The UI must answer from its original live store.
    pub(crate) fn slot(&self) -> &str {
        &self.slot
    }
    pub(crate) fn database_identity(&self) -> io::Result<(u64, u64)> {
        self.database.metadata().map(|metadata| identity(&metadata))
    }
}
fn original_peer(stream: &UnixStream) -> io::Result<File> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<libc::ucred>()
        || credentials.uid != current_uid()
        || credentials.pid <= 1
    {
        return Err(refused(
            "Original UI journal peer credentials are unavailable",
        ));
    }
    let mut raw = -1i32;
    let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut raw as *mut i32).cast(),
            &mut size,
        )
    };
    let peer = (raw >= 0).then(|| unsafe { File::from_raw_fd(raw) });
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let peer = peer.ok_or_else(|| refused("Kernel did not supply the original peer pidfd"))?;
    if size as usize != std::mem::size_of::<i32>()
        || !descriptor_cloexec(&peer)?
        || peer_pid(&peer)? != credentials.pid as u32
    {
        return Err(refused("Original UI journal peer pidfd is invalid"));
    }
    require_live(&peer)?;
    Ok(peer)
}
fn peer_pid(peer: &File) -> io::Result<u32> {
    let bytes = read_small(&Path::new("/proc/self/fdinfo").join(peer.as_raw_fd().to_string()))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| refused("Invalid original peer pidfd metadata"))?;
    text.lines()
        .find_map(|line| {
            line.strip_prefix("Pid:")
                .and_then(|pid| pid.trim().parse::<u32>().ok())
        })
        .filter(|pid| *pid > 1)
        .ok_or_else(|| refused("Original peer pidfd no longer names a live process"))
}
fn require_live(peer: &File) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: peer.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    match unsafe { libc::poll(&mut poll, 1, 0) } {
        0 => Ok(()),
        -1 => Err(io::Error::last_os_error()),
        _ => Err(refused(
            "Original UI journal peer exited or its observation is unavailable",
        )),
    }
}
fn owns_exact_slot(
    process: &Path,
    pid: u32,
    expected: &fs::Metadata,
    deadline: Instant,
) -> io::Result<bool> {
    let mut found = false;
    scan_fds(process, deadline, |path, metadata, name| {
        if identity(metadata) == identity(expected) {
            let bytes = read_small(&process.join("fdinfo").join(name))?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| refused("Invalid original window lock metadata"))?;
            if fdinfo_owns_slot(text, pid, expected)
                && fs::metadata(path).is_ok_and(|m| identity(&m) == identity(expected))
            {
                found = true;
            }
        }
        Ok(())
    })?;
    Ok(found)
}
fn owns_open_file(process: &Path, expected: &fs::Metadata, deadline: Instant) -> io::Result<bool> {
    let mut found = false;
    scan_fds(process, deadline, |_, metadata, _| {
        found |= identity(metadata) == identity(expected);
        Ok(())
    })?;
    Ok(found)
}
fn scan_fds(
    process: &Path,
    deadline: Instant,
    mut visit: impl FnMut(&Path, &fs::Metadata, &std::ffi::OsStr) -> io::Result<()>,
) -> io::Result<()> {
    for (count, entry) in fs::read_dir(process.join("fd"))?.enumerate() {
        check_deadline(deadline)?;
        if count >= 4096 {
            return Err(refused("Original UI descriptor census exceeds its bound"));
        }
        let entry = entry?;
        let path = entry.path();
        match fs::metadata(&path) {
            Ok(metadata) => visit(&path, &metadata, &entry.file_name())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn read_small(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(refused("Original UI process metadata exceeds its bound"));
    }
    Ok(bytes)
}
fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}
fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}
fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn descriptor_cloexec(file: &File) -> io::Result<bool> {
    // SAFETY: F_GETFD takes no variadic argument and file owns a live fd.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & libc::FD_CLOEXEC != 0)
}
fn require_ui_role(process: &Path) -> io::Result<()> {
    let comm = read_small(&process.join("comm"))?;
    if comm != format!("{LEASE_CAPABLE_PROCESS_NAME}\n").as_bytes() {
        return Err(refused(
            "The original peer has no current UI-v2 window role",
        ));
    }
    Ok(())
}

fn require_private_regular(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(refused(
            "The original UI ownership file is no longer private",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
    };

    #[test]
    fn original_peer_pidfd_is_kernel_bound_and_close_on_exec() {
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = original_peer(&a).unwrap();
        assert_eq!(peer_pid(&peer).unwrap(), std::process::id());
        assert!(descriptor_cloexec(&peer).unwrap());
        require_live(&peer).unwrap();
    }

    #[test]
    fn original_ui_fixture_child() {
        let Some(root) = std::env::var_os("CAGENTS_TEST_ORIGINAL_UI_JOURNAL") else {
            return;
        };
        let root = PathBuf::from(root);
        let _lease = owned_lock(
            &root.join(slot_lock_name("fixture")),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let _database = OpenOptions::new()
            .read(true)
            .open(root.join("journal.sqlite3"))
            .unwrap();
        mark_lease_capable_process().unwrap();
        let stream = UnixStream::connect(root.join("peer.socket")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.take(32))
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "exit\n");
    }

    #[test]
    fn audience_uses_original_slot_and_held_db_then_refuses_peer_exit() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = directory.path().join("journal.sqlite3");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&database)
            .unwrap();
        let listener = UnixListener::bind(directory.path().join("peer.socket")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let cancellation = CancellationToken::new();
        struct CancelOnDrop(CancellationToken);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel = CancelOnDrop(cancellation.clone());
        let spec = CommandSpec::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance::ui_journal::tests::original_ui_fixture_child",
                "--nocapture",
            ])
            .env("CAGENTS_TEST_ORIGINAL_UI_JOURNAL", directory.path())
            .timeout(Duration::from_secs(5))
            .output_limits(16384, 16384);
        std::thread::scope(|scope| {
            let child = scope.spawn(|| SystemProcessExecutor.execute(&spec, &cancellation));
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::yield_now()
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let audience = OriginalUiJournalPeer::capture_in(
                &stream,
                directory.path(),
                &std::env::current_exe().unwrap(),
                |slot| {
                    assert_eq!(slot, "fixture");
                    Ok(database.clone())
                },
                deadline,
            )
            .unwrap();
            assert_eq!(audience.slot(), "fixture");
            assert_eq!(
                audience.database_identity().unwrap(),
                identity(&fs::metadata(&database).unwrap())
            );
            audience.validate(deadline).unwrap();
            stream.write_all(b"exit\n").unwrap();
            let output = child.join().unwrap().unwrap();
            assert!(output.success, "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
            assert!(audience.validate(deadline).is_err());
        });
    }
}
