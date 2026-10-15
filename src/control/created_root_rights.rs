//! Distinct original-directory descriptor roles, in a separate rights phase AFTER
//! the authenticated request frame. Ordinary FrameCodec must never read this
//! phase. This codec supplies owned descriptors, NOT creation/start authority.
//! The host must validate the actual settled carrier, current proof, required
//! marker and target before installing a grant or admitting any first effect.
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd},
        unix::net::UnixStream,
    },
    time::Instant,
};

pub(crate) const ROOT_COUNT: usize = 6;
const PHASE: u8 = 0xc1;
// Source checkout/admin/common, then destination checkout/admin/common. No
// role is inferred from a pathname and this is unrelated to holder rights.
pub(crate) struct ReceivedRoots<const N: usize>(pub(crate) [File; N]);
pub(crate) type Roots = ReceivedRoots<ROOT_COUNT>;

fn wait(stream: &UnixStream, events: libc::c_short, deadline: Instant, cancellation: Option<&crate::process::CancellationToken>) -> io::Result<()> {
    loop {
        check_active(deadline, cancellation)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        let millis = remaining
            .as_millis()
            .saturating_add(1)
            .min(if cancellation.is_some() { 50 } else { i32::MAX as u128 }) as i32;
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, millis) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The caller serializes normal-frame + rights-phase + correlated reply on
/// the SAME already authenticated connection. Any phase error poisons that
/// connection: never retry a new request on its remaining bytes.
pub(crate) fn send(
    stream: &UnixStream,
    files: &[BorrowedFd<'_>; ROOT_COUNT],
    deadline: Instant,
) -> io::Result<()> {
    send_exact(stream, files, PHASE, deadline, None)
}
pub(crate) fn send_cancellable(
    stream: &UnixStream, files: &[BorrowedFd<'_>; ROOT_COUNT], deadline: Instant,
    cancellation: &crate::process::CancellationToken,
) -> io::Result<()> {
    send_exact(stream, files, PHASE, deadline, Some(cancellation))
}
fn send_exact<const N: usize>(
    stream: &UnixStream,
    files: &[BorrowedFd<'_>; N],
    phase: u8,
    deadline: Instant,
    cancellation: Option<&crate::process::CancellationToken>,
) -> io::Result<()> {
    let mut byte = phase;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    // usize storage guarantees cmsghdr alignment; exactly one rights cmsg.
    let mut control = [0usize; 16];
    let space =
        unsafe { libc::CMSG_SPACE((N * std::mem::size_of::<i32>()) as u32) } as usize;
    if space > std::mem::size_of_val(&control) {
        return Err(io::Error::other("creation rights buffer is insufficient"));
    }
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space;
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null() {
        return Err(io::Error::other("creation rights header is unavailable"));
    }
    unsafe {
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len =
            libc::CMSG_LEN((N * std::mem::size_of::<i32>()) as u32) as usize;
        let data = libc::CMSG_DATA(header).cast::<i32>();
        for (index, file) in files.iter().enumerate() {
            data.add(index).write(file.as_raw_fd());
        }
    }
    loop {
        check_active(deadline, cancellation)?;
        let result = unsafe {
            libc::sendmsg(
                stream.as_raw_fd(),
                &message,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if result == 1 {
            return check_active(deadline, cancellation);
        }
        if result >= 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "creation rights phase was not sent",
            ));
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => wait(stream, libc::POLLOUT, deadline, cancellation)?,
            _ => return Err(error),
        }
    }
}

/// Called only after the host decoded and authenticated the matching NEW
/// request header and user audience. The six owned FDs still grant no source
/// identity: typed grant admission validates their original scope separately.
pub(crate) fn receive(stream: &UnixStream, deadline: Instant) -> io::Result<Roots> {
    receive_exact(stream, PHASE, deadline, None)
}
pub(crate) fn receive_cancellable(stream: &UnixStream, deadline: Instant,
    cancellation: &crate::process::CancellationToken) -> io::Result<Roots> {
    receive_exact(stream, PHASE, deadline, Some(cancellation))
}
fn receive_exact<const N: usize>(stream: &UnixStream, phase: u8, deadline: Instant,
    cancellation: Option<&crate::process::CancellationToken>) -> io::Result<ReceivedRoots<N>> {
    loop {
        check_active(deadline, cancellation)?;
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 16];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control);
        let result = unsafe {
            libc::recvmsg(
                stream.as_raw_fd(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    wait(stream, libc::POLLIN, deadline, cancellation)?;
                    continue;
                }
                _ => return Err(error),
            }
        }
        // Collect EVERY delivered FD into RAII before inspecting any failure.
        // On CTRUNC the kernel disposes undelivered FDs; delivered FDs are ours.
        let mut files = Vec::new();
        let mut cmsgs = 0usize;
        let mut invalid = false;
        let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        while !header.is_null() {
            cmsgs += 1;
            let length = unsafe { (*header).cmsg_len };
            let base = unsafe { libc::CMSG_LEN(0) } as usize;
            if length < base {
                invalid = true;
                break;
            }
            if unsafe {
                (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
            } {
                let size = length - base;
                if size % std::mem::size_of::<i32>() != 0 {
                    invalid = true;
                }
                let count = size / std::mem::size_of::<i32>();
                let data = unsafe { libc::CMSG_DATA(header).cast::<i32>() };
                for index in 0..count {
                    let raw = unsafe { data.add(index).read_unaligned() };
                    if raw < 0 {
                        invalid = true;
                    } else {
                        files.push(unsafe { File::from_raw_fd(raw) });
                    }
                }
            } else {
                invalid = true;
            }
            header = unsafe { libc::CMSG_NXTHDR(&message, header) };
        }
        if result != 1
            || byte != phase
            || cmsgs != 1
            || invalid
            || message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0
            || files.len() != N
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "creation rights phase is missing, malformed or truncated",
            ));
        }
        let files: [File; N] = files
            .try_into()
            .map_err(|_| io::Error::other("creation rights count changed"))?;
        // All delivered rights are owned before cancellation can reject them.
        check_active(deadline, cancellation)?;
        return Ok(ReceivedRoots(files));
    }
}

fn check_active(deadline: Instant, cancellation: Option<&crate::process::CancellationToken>) -> io::Result<()> {
    if cancellation.is_some_and(|token| token.is_cancelled()) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if Instant::now() >= deadline { return Err(io::ErrorKind::TimedOut.into()); }
    Ok(())
}

/// Negative recovery uses DISTINCT operation tags and exact role counts.
/// These payloads authorize no launch. The separate catalog/audience consumer
/// must authenticate its registry/lease/revision and retain the original pins.
pub(crate) mod negative {
    use super::*;
    // Reservation: original source + original holding parent.
    pub(crate) fn send_reservation(stream: &UnixStream, files: &[BorrowedFd<'_>; 2], deadline: Instant) -> io::Result<()> {
        send_exact(stream, files, 0xc2, deadline, None)
    }
    pub(crate) fn receive_reservation(stream: &UnixStream, deadline: Instant) -> io::Result<ReceivedRoots<2>> {
        receive_exact(stream, 0xc2, deadline, None)
    }
    // Later actual H publication, distinct from an edited/adopted root.
    pub(crate) fn send_published_payload(stream: &UnixStream, file: BorrowedFd<'_>, deadline: Instant) -> io::Result<()> {
        send_exact(stream, &[file], 0xc3, deadline, None)
    }
    pub(crate) fn receive_published_payload(stream: &UnixStream, deadline: Instant) -> io::Result<ReceivedRoots<1>> {
        receive_exact(stream, 0xc3, deadline, None)
    }
    pub(crate) fn send_edited_root(stream: &UnixStream, file: BorrowedFd<'_>, deadline: Instant) -> io::Result<()> {
        send_exact(stream, &[file], 0xc4, deadline, None)
    }
    pub(crate) fn receive_edited_root(stream: &UnixStream, deadline: Instant) -> io::Result<ReceivedRoots<1>> {
        receive_exact(stream, 0xc4, deadline, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::{
            fd::AsFd,
            unix::fs::{MetadataExt, PermissionsExt},
        },
        time::Duration,
    };
    #[test]
    fn frame_boundary_preserves_six_original_roots_after_public_name_replacement() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        let files: [File; ROOT_COUNT] = std::array::from_fn(|_| File::open(&original).unwrap());
        let expected = files[0].metadata().unwrap();
        std::fs::rename(&original, root.path().join("moved")).unwrap();
        std::fs::create_dir(&original).unwrap();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        crate::terminal_ipc::write_frame(&mut client, &"normal request frame boundary").unwrap();
        send(&client, &files.each_ref().map(AsFd::as_fd), deadline).unwrap();
        let header: String = crate::terminal_ipc::read_frame(&mut server)
            .unwrap()
            .unwrap();
        assert_eq!(header, "normal request frame boundary");
        let received = receive(&server, deadline).unwrap();
        for file in &received.0 {
            let actual = file.metadata().unwrap();
            assert_eq!(
                (actual.dev(), actual.ino()),
                (expected.dev(), expected.ino())
            );
            assert_ne!(actual.ino(), std::fs::metadata(&original).unwrap().ino());
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        client.write_all(b"next").unwrap();
        let mut next = [0; 4];
        server.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"next");
    }
    #[test]
    fn rejected_count_and_truncation_release_every_kernel_queued_right() {
        for count in [ROOT_COUNT + 1, 40] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("queued-lock");
            let owner = File::create(&path).unwrap();
            assert_eq!(
                unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            let (client, server) = UnixStream::pair().unwrap();
            let mut byte = PHASE;
            let mut iov = libc::iovec {
                iov_base: (&mut byte as *mut u8).cast(),
                iov_len: 1,
            };
            let mut control = [0usize; 64];
            let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = unsafe { libc::CMSG_SPACE((count * 4) as u32) } as usize;
            let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
            assert!(!header.is_null());
            unsafe {
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN((count * 4) as u32) as usize;
                let data = libc::CMSG_DATA(header).cast::<i32>();
                for index in 0..count {
                    data.add(index).write(owner.as_raw_fd());
                }
            }
            assert_eq!(
                unsafe {
                    libc::sendmsg(
                        client.as_raw_fd(),
                        &message,
                        libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    )
                },
                1
            );
            drop(owner);
            let probe = File::open(&path).unwrap();
            assert_eq!(
                unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                -1,
                "queued rights still own the original open-file-description lock"
            );
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
            assert_eq!(
                receive(&server, Instant::now() + Duration::from_secs(1))
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0,
                "rejected delivered AND truncated rights must all be closed"
            );
        }
    }
    #[test]
    fn distinct_negative_roles_preserve_original_pins_and_refuse_cross_phase_payloads() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("source");
        let second = root.path().join("holding");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let source = File::open(&first).unwrap();
        let holding = File::open(&second).unwrap();
        let deadline = Instant::now()+Duration::from_secs(2);
        let (client, server) = UnixStream::pair().unwrap();
        negative::send_reservation(&client, &[source.as_fd(), holding.as_fd()], deadline).unwrap();
        let received = negative::receive_reservation(&server, deadline).unwrap();
        assert_eq!(received.0[0].metadata().unwrap().ino(), source.metadata().unwrap().ino());
        assert_eq!(received.0[1].metadata().unwrap().ino(), holding.metadata().unwrap().ino());
        for wrong in [false, true] {
            let (client, server) = UnixStream::pair().unwrap();
            negative::send_published_payload(&client, source.as_fd(), deadline).unwrap();
            let result = if wrong { negative::receive_edited_root(&server, deadline) }
                else { negative::receive_published_payload(&server, deadline) };
            assert_eq!(result.is_err(), wrong, "one-FD roles must never substitute for each other");
        }
        let (client, server) = UnixStream::pair().unwrap();
        negative::send_edited_root(&client, holding.as_fd(), deadline).unwrap();
        assert_eq!(negative::receive_edited_root(&server, deadline).unwrap().0[0].metadata().unwrap().ino(), holding.metadata().unwrap().ino());
        let (client, server) = UnixStream::pair().unwrap();
        negative::send_reservation(&client, &[source.as_fd(),holding.as_fd()], deadline).unwrap();
        assert!(receive(&server, deadline).is_err(), "negative reservation cannot enter creation6 receiver");
    }
    #[test]
    fn missing_rights_or_stalled_phase_refuses_within_one_absolute_budget() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[PHASE]).unwrap();
        assert_eq!(
            receive(&server, Instant::now() + Duration::from_millis(50))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let (client, server) = UnixStream::pair().unwrap();
        let began = Instant::now();
        assert_eq!(
            receive(&server, began + Duration::from_millis(40))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(began.elapsed() < Duration::from_secs(1));
        drop(client);
    }
}
