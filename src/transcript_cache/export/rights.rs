//! Bounded private transport primitive, NOT authenticated authority admission.
//! Only a codec holding an actual authenticated helper/daemon receipt may use
//! this framing. Public CLI transport never carries descriptors.
use super::{Error, Result, types::MAX_RECORD};
use std::{fs::File, io, mem, os::fd::{AsRawFd, BorrowedFd, FromRawFd},
    os::unix::net::UnixStream, time::Instant};

pub(super) struct Packet { pub(super) metadata: Vec<u8>, pub(super) files: [File; 4] }
fn wait(stream: &UnixStream, writing: bool, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or(Error::Deadline)?;
        let mut fd = libc::pollfd { fd: stream.as_raw_fd(), events: if writing { libc::POLLOUT } else { libc::POLLIN }, revents: 0 };
        let millis = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut fd, 1, millis) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted { continue; }
            return Err(error.into());
        }
        if Instant::now() >= deadline { return Err(Error::Deadline); }
        if result == 0 { continue; }
        if fd.revents & fd.events != 0 { return Ok(()); }
        return Err(Error::Incomplete("Private export descriptor transport disconnected"));
    }
}

pub(super) fn send(stream: &UnixStream, metadata: &[u8], files: &[BorrowedFd<'_>; 4], deadline: Instant) -> Result<()> {
    if metadata.is_empty() || metadata.len() > MAX_RECORD { return Err(Error::Capacity); }
    let mut framed = Vec::with_capacity(4 + metadata.len());
    framed.extend_from_slice(&(metadata.len() as u32).to_be_bytes()); framed.extend_from_slice(metadata);
    // usize storage gives cmsghdr alignment. The kernel copies rights during
    // successful sendmsg, so queued rights hold their actual SH flock even if
    // the issuer drops its descriptor before the receiver accepts the frame.
    let bytes = unsafe { libc::CMSG_SPACE((4 * mem::size_of::<libc::c_int>()) as u32) } as usize;
    let mut control = vec![0usize; bytes.div_ceil(mem::size_of::<usize>())];
    let mut offset = 0;
    while offset < framed.len() {
        wait(stream, true, deadline)?;
        let mut vector = libc::iovec { iov_base: framed[offset..].as_mut_ptr().cast(), iov_len: framed.len() - offset };
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut vector; message.msg_iovlen = 1;
        if offset == 0 {
            message.msg_control = control.as_mut_ptr().cast(); message.msg_controllen = bytes;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET; (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN((4 * mem::size_of::<libc::c_int>()) as u32) as usize;
                let descriptors = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for (index, file) in files.iter().enumerate() { descriptors.add(index).write(file.as_raw_fd()); }
            }
        }
        let written = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) { continue; }
            return Err(error.into());
        }
        if written == 0 { return Err(Error::Incomplete("Private export descriptor write made no progress")); }
        offset += written as usize;
    }
    if Instant::now() >= deadline { return Err(Error::Deadline); }
    Ok(())
}

fn receive_chunk(stream: &UnixStream, output: &mut [u8], deadline: Instant) -> Result<(usize, Vec<File>)> {
    // Extra capacity lets us close a malformed sender's extra rights before
    // refusing. MSG_CTRUNC automatically closes rights not delivered by Linux;
    // every delivered descriptor becomes an owned File before any error exit.
    let bytes = unsafe { libc::CMSG_SPACE((8 * mem::size_of::<libc::c_int>()) as u32) } as usize;
    let mut control = vec![0usize; bytes.div_ceil(mem::size_of::<usize>())];
    loop {
        wait(stream, false, deadline)?;
        let mut vector = libc::iovec { iov_base: output.as_mut_ptr().cast(), iov_len: output.len() };
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut vector; message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast(); message.msg_controllen = bytes;
        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
        if received < 0 {
            let error = io::Error::last_os_error();
            if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) { continue; }
            return Err(error.into());
        }
        let mut files = Vec::new(); let mut invalid = message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0;
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                let base = libc::CMSG_LEN(0) as usize;
                if (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS
                    || (*header).cmsg_len < base { invalid = true; }
                else {
                    let count_bytes = (*header).cmsg_len - base;
                    if count_bytes % mem::size_of::<libc::c_int>() != 0 { invalid = true; }
                    let descriptors = libc::CMSG_DATA(header).cast::<libc::c_int>();
                    for index in 0..count_bytes / mem::size_of::<libc::c_int>() {
                        let descriptor = descriptors.add(index).read();
                        if descriptor < 0 { invalid = true; } else { files.push(File::from_raw_fd(descriptor)); }
                    }
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        if invalid { return Err(Error::Incomplete("Private export ancillary data was unexpected or truncated")); }
        if received == 0 { return Err(Error::Incomplete("Private export descriptor frame ended early")); }
        if Instant::now() >= deadline { return Err(Error::Deadline); }
        return Ok((received as usize, files));
    }
}
pub(super) fn receive(stream: &UnixStream, deadline: Instant) -> Result<Packet> {
    let mut length = [0u8; 4];
    let (mut filled, files) = receive_chunk(stream, &mut length, deadline)?;
    let files: [File; 4] = files.try_into().map_err(|_: Vec<File>| Error::Incomplete("Private export needs exactly four descriptors"))?;
    while filled < length.len() {
        let (read, extra) = receive_chunk(stream, &mut length[filled..], deadline)?;
        if !extra.is_empty() { return Err(Error::Incomplete("Private export repeated descriptor rights")); }
        filled += read;
    }
    let size = u32::from_be_bytes(length) as usize;
    if size == 0 || size > MAX_RECORD { return Err(Error::Capacity); }
    let mut metadata = vec![0; size]; filled = 0;
    while filled < size {
        let (read, extra) = receive_chunk(stream, &mut metadata[filled..], deadline)?;
        if !extra.is_empty() { return Err(Error::Incomplete("Private export repeated descriptor rights")); }
        filled += read;
    }
    Ok(Packet { metadata, files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lifetime;
    use std::{os::fd::AsFd, time::Duration};
    fn deadline() -> Instant { Instant::now() + Duration::from_secs(2) }
    #[test]
    fn actual_rights_are_cloexec_and_queued_sh_survives_sender_drop() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap(); lifetime::initialize(root.path()).unwrap();
        let (sender, receiver) = UnixStream::pair().unwrap();
        let lock = File::open(root.path().join("readers.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) }, 0);
        let files = [lock.try_clone().unwrap(), lock.try_clone().unwrap(), lock.try_clone().unwrap(), lock.try_clone().unwrap()];
        send(&sender, b"private-metadata", &files.each_ref().map(File::as_fd), deadline()).unwrap();
        drop(files); drop(lock); drop(sender);
        assert!(matches!(lifetime::disposal(root.path(), Instant::now() + Duration::from_millis(20)), Err(Error::Deadline)));
        let packet = receive(&receiver, deadline()).unwrap();
        assert_eq!(packet.metadata, b"private-metadata");
        for file in &packet.files { assert_ne!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0); }
        assert!(matches!(lifetime::disposal(root.path(), Instant::now() + Duration::from_millis(20)), Err(Error::Deadline)));
        drop(packet); lifetime::disposal(root.path(), deadline()).unwrap();
    }
    #[test]
    fn absolute_deadline_and_oversized_metadata_refuse_without_rights_transfer() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let file = File::open("/dev/null").unwrap();
        let borrowed = [file.as_fd(), file.as_fd(), file.as_fd(), file.as_fd()];
        assert!(matches!(send(&sender, b"x", &borrowed, Instant::now()), Err(Error::Deadline)));
        assert!(matches!(send(&sender, &vec![0; MAX_RECORD + 1], &borrowed, deadline()), Err(Error::Capacity)));
        assert!(matches!(receive(&receiver, Instant::now()), Err(Error::Deadline)));
    }
}
