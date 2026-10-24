//! Bounded private JSON frames with optional UNIX descriptor custody.
//!
//! Framing and descriptor transport only: callers validate the exact count,
//! descriptor types, identities, and protocol authority. No socket flags or
//! shared timeouts are changed, including when a deadline expires mid-frame.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Write};
use std::mem::{size_of, size_of_val, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Instant;

// Linux's SCM_MAX_FD. Receive enough space to own and close excess rights even
// when a caller permits fewer. MSG_CTRUNC is still an error, never a partial set.
const MAX_RIGHTS: usize = 253;
const CONTROL_WORDS: usize = 256;
// Linux UAPI SCM_PIDFD (not yet exported by our libc version). We reject this
// unexpected ancillary type, but must close its installed fd before returning.
const SCM_PIDFD: libc::c_int = 0x04;

#[derive(Clone, Copy, Debug)]
pub(crate) enum FrameByteOrder {
    LittleEndian,
    BigEndian,
}

impl FrameByteOrder {
    fn encode(self, length: u32) -> [u8; 4] {
        match self {
            Self::LittleEndian => length.to_le_bytes(),
            Self::BigEndian => length.to_be_bytes(),
        }
    }

    fn decode(self, bytes: [u8; 4]) -> u32 {
        match self {
            Self::LittleEndian => u32::from_le_bytes(bytes),
            Self::BigEndian => u32::from_be_bytes(bytes),
        }
    }
}

fn check_bounds(max_frame: usize, max_fds: usize) -> Result<(), String> {
    if max_frame == 0 || u32::try_from(max_frame).is_err() {
        return Err("invalid JSON frame bound".into());
    }
    if max_fds > MAX_RIGHTS {
        return Err("invalid descriptor frame bound".into());
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Result<std::time::Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "descriptor frame deadline expired".into())
}

fn ready(control: &UnixStream, events: libc::c_short, deadline: Instant) -> Result<(), String> {
    loop {
        let left = remaining(deadline)?;
        let milliseconds = left
            .as_millis()
            .saturating_add(u128::from(left.subsec_nanos() % 1_000_000 != 0));
        let timeout = milliseconds.min(libc::c_int::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: control.as_raw_fd(),
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll descriptor frame: {error}"));
        }
        remaining(deadline)?;
        if result == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err("invalid descriptor frame socket".into());
        }
        // HUP/ERR must reach recvmsg/sendmsg, which reports EOF/the actual error.
        if descriptor.revents & (events | libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(());
        }
    }
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("JSON frame exceeds bound"));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn send_json_with_fds_until(
    control: &UnixStream,
    value: &impl Serialize,
    descriptors: &[BorrowedFd<'_>],
    deadline: Instant,
    max_frame: usize,
    max_fds: usize,
    byte_order: FrameByteOrder,
) -> Result<(), String> {
    check_bounds(max_frame, max_fds)?;
    if descriptors.len() > max_fds {
        return Err("too many descriptors for JSON frame".into());
    }
    remaining(deadline)?;
    let mut json = BoundedJson {
        bytes: Vec::new(),
        limit: max_frame,
    };
    serde_json::to_writer(&mut json, value)
        .map_err(|error| format!("encode JSON frame: {error}"))?;
    remaining(deadline)?;
    if json.bytes.is_empty() {
        return Err("empty JSON frame".into());
    }
    let length = u32::try_from(json.bytes.len()).map_err(|_| "JSON frame exceeds u32")?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(json.bytes.len().checked_add(4).ok_or("frame overflow")?)
        .map_err(|error| format!("allocate JSON frame: {error}"))?;
    frame.extend_from_slice(&byte_order.encode(length));
    frame.extend_from_slice(&json.bytes);

    let mut ancillary = [0_u64; CONTROL_WORDS];
    let mut offset = 0;
    while offset < frame.len() {
        remaining(deadline)?;
        let mut vector = libc::iovec {
            iov_base: frame[offset..].as_ptr().cast_mut().cast(),
            iov_len: frame.len() - offset,
        };
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        if offset == 0 && !descriptors.is_empty() {
            let bytes = descriptors.len() * size_of::<RawFd>();
            let space = unsafe { libc::CMSG_SPACE(bytes as _) } as usize;
            debug_assert!(space <= size_of_val(&ancillary));
            message.msg_control = ancillary.as_mut_ptr().cast();
            message.msg_controllen = space;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(bytes as _) as _;
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                for (index, descriptor) in descriptors.iter().enumerate() {
                    data.add(index).write_unaligned(descriptor.as_raw_fd());
                }
            }
        }
        let count = unsafe {
            libc::sendmsg(
                control.as_raw_fd(),
                &message,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    ready(control, libc::POLLOUT, deadline)?;
                    continue;
                }
                _ => return Err(format!("send JSON frame: {error}")),
            }
        }
        if count == 0 {
            return Err("zero-byte JSON frame send".into());
        }
        // Rights accompany only the first successful byte, even after partial I/O.
        offset += count as usize;
    }
    remaining(deadline)?;
    Ok(())
}

// Consume kernel-installed fds before returning any ancillary error. Bounds are
// checked before every header/payload read; unknown data is never treated as fds.
fn take_rights(bytes: &[u8], descriptors: &mut Vec<OwnedFd>) -> Result<(), String> {
    let base_length = unsafe { libc::CMSG_LEN(0) } as usize;
    let alignment = size_of::<usize>();
    let mut offset = 0_usize;
    let mut invalid = false;
    while bytes.len().saturating_sub(offset) >= size_of::<libc::cmsghdr>() {
        let header = unsafe {
            bytes
                .as_ptr()
                .add(offset)
                .cast::<libc::cmsghdr>()
                .read_unaligned()
        };
        let length = header.cmsg_len as usize;
        if length < base_length || length > bytes.len() - offset {
            return Err("malformed descriptor frame ancillary header".into());
        }
        let payload = &bytes[offset + base_length..offset + length];
        let rights = header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_RIGHTS;
        let pidfd = header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == SCM_PIDFD;
        if rights || pidfd {
            invalid |= payload.len() % size_of::<RawFd>() != 0 || pidfd;
            for chunk in payload.chunks_exact(size_of::<RawFd>()) {
                let descriptor = unsafe { chunk.as_ptr().cast::<RawFd>().read_unaligned() };
                if descriptor < 0 {
                    invalid = true;
                } else {
                    // Only called on descriptors actually installed by recvmsg.
                    descriptors.push(unsafe { OwnedFd::from_raw_fd(descriptor) });
                }
            }
        } else {
            invalid = true;
        }
        let step = length
            .checked_add(alignment - 1)
            .map(|length| length & !(alignment - 1))
            .ok_or("ancillary alignment overflow")?;
        offset = offset
            .checked_add(step)
            .ok_or("ancillary offset overflow")?;
        if offset >= bytes.len() {
            break;
        }
    }
    if offset < bytes.len() {
        return Err("malformed descriptor frame ancillary tail".into());
    }
    if invalid {
        Err("unexpected descriptor frame ancillary data".into())
    } else {
        Ok(())
    }
}

fn receive_part(
    control: &UnixStream,
    target: &mut [u8],
    descriptors: &mut Vec<OwnedFd>,
    allow_rights: bool,
    max_fds: usize,
    deadline: Instant,
    ancillary_capacity: usize,
) -> Result<usize, String> {
    loop {
        remaining(deadline)?;
        let mut ancillary = [0_u64; CONTROL_WORDS];
        if ancillary_capacity > size_of_val(&ancillary) {
            return Err("invalid ancillary capacity".into());
        }
        let mut vector = libc::iovec {
            iov_base: target.as_mut_ptr().cast(),
            iov_len: target.len(),
        };
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = ancillary_capacity;
        let count = unsafe {
            libc::recvmsg(
                control.as_raw_fd(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    ready(control, libc::POLLIN, deadline)?;
                    continue;
                }
                _ => return Err(format!("receive JSON frame: {error}")),
            }
        }
        let before = descriptors.len();
        let control_length = message.msg_controllen.min(ancillary_capacity);
        let bytes =
            unsafe { std::slice::from_raw_parts(ancillary.as_ptr().cast::<u8>(), control_length) };
        take_rights(bytes, descriptors)?;
        if message.msg_controllen > ancillary_capacity {
            return Err("oversized descriptor frame ancillary data".into());
        }
        if message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
            return Err("truncated descriptor frame ancillary data".into());
        }
        if (!allow_rights && descriptors.len() != before) || descriptors.len() > max_fds {
            return Err("unexpected or excess JSON frame descriptors".into());
        }
        if count == 0 {
            return Err("EOF in JSON frame".into());
        }
        remaining(deadline)?;
        return Ok(count as usize);
    }
}

pub(crate) fn receive_json_with_fds_until<T: DeserializeOwned>(
    control: &UnixStream,
    deadline: Instant,
    max_frame: usize,
    max_fds: usize,
    byte_order: FrameByteOrder,
) -> Result<(T, Vec<OwnedFd>), String> {
    check_bounds(max_frame, max_fds)?;
    let mut descriptors = Vec::new();
    let mut header = [0_u8; 4];
    // One byte, rather than one header read: a stream recvmsg spanning later
    // bytes could otherwise accept rights attached after the frame's first byte.
    receive_part(
        control,
        &mut header[..1],
        &mut descriptors,
        true,
        max_fds,
        deadline,
        size_of::<[u64; CONTROL_WORDS]>(),
    )?;
    let mut offset = 1;
    while offset < header.len() {
        offset += receive_part(
            control,
            &mut header[offset..],
            &mut descriptors,
            false,
            max_fds,
            deadline,
            size_of::<[u64; CONTROL_WORDS]>(),
        )?;
    }
    let length = byte_order.decode(header) as usize;
    if length == 0 || length > max_frame {
        return Err("invalid or oversized JSON frame".into());
    }
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|error| format!("allocate JSON frame: {error}"))?;
    body.resize(length, 0);
    let mut offset = 0;
    while offset < body.len() {
        offset += receive_part(
            control,
            &mut body[offset..],
            &mut descriptors,
            false,
            max_fds,
            deadline,
            size_of::<[u64; CONTROL_WORDS]>(),
        )?;
    }
    let value =
        serde_json::from_slice(&body).map_err(|error| format!("decode JSON frame: {error}"))?;
    remaining(deadline)?;
    Ok((value, descriptors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::sync::mpsc;
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    fn socket_flags(socket: &UnixStream) -> libc::c_int {
        let result = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
        assert!(result >= 0);
        result
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        }
    }

    fn assert_pipe_closed(reader: &OwnedFd) {
        let mut byte = 0_u8;
        assert_eq!(
            unsafe { libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0,
            "a received write descriptor leaked: {}",
            io::Error::last_os_error()
        );
    }

    fn raw_send(socket: &UnixStream, bytes: &[u8], descriptors: &[BorrowedFd<'_>]) {
        let mut ancillary = [0_u64; CONTROL_WORDS];
        let mut vector = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        if !descriptors.is_empty() {
            let length = descriptors.len() * size_of::<RawFd>();
            message.msg_control = ancillary.as_mut_ptr().cast();
            message.msg_controllen = unsafe { libc::CMSG_SPACE(length as _) } as _;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(length as _) as _;
                for (index, descriptor) in descriptors.iter().enumerate() {
                    libc::CMSG_DATA(header)
                        .cast::<RawFd>()
                        .add(index)
                        .write_unaligned(descriptor.as_raw_fd());
                }
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }

    #[test]
    fn directory_roundtrip_preserves_original_inode_and_socket_configuration() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("identity"), "original").unwrap();
        let directory = File::open(&original).unwrap();
        let identity = directory.metadata().unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("identity"), "replacement").unwrap();

        for order in [FrameByteOrder::LittleEndian, FrameByteOrder::BigEndian] {
            let (sender, receiver) = UnixStream::pair().unwrap();
            sender
                .set_write_timeout(Some(Duration::from_secs(7)))
                .unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_secs(9)))
                .unwrap();
            let send_flags = socket_flags(&sender);
            let receive_flags = socket_flags(&receiver);
            send_json_with_fds_until(
                &sender,
                &json!({"directory": true}),
                &[directory.as_fd()],
                deadline(),
                128,
                3,
                order,
            )
            .unwrap();
            let (value, descriptors): (Value, _) =
                receive_json_with_fds_until(&receiver, deadline(), 128, 3, order).unwrap();
            assert_eq!(value, json!({"directory": true}));
            assert_eq!(descriptors.len(), 1);
            let file = File::from(descriptors.into_iter().next().unwrap());
            let received = file.metadata().unwrap();
            assert_eq!(
                (received.dev(), received.ino()),
                (identity.dev(), identity.ino())
            );
            assert_eq!(
                std::fs::read_to_string(format!("/proc/self/fd/{}/identity", file.as_raw_fd()))
                    .unwrap(),
                "original"
            );
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            assert_eq!(socket_flags(&sender), send_flags);
            assert_eq!(socket_flags(&receiver), receive_flags);
            assert_eq!(
                sender.write_timeout().unwrap(),
                Some(Duration::from_secs(7))
            );
            assert_eq!(
                receiver.read_timeout().unwrap(),
                Some(Duration::from_secs(9))
            );
        }
    }

    #[test]
    fn byte_orders_no_rights_and_legacy_fewer_rights_remain_explicit() {
        for order in [FrameByteOrder::LittleEndian, FrameByteOrder::BigEndian] {
            let (sender, mut receiver) = UnixStream::pair().unwrap();
            send_json_with_fds_until(&sender, &true, &[], deadline(), 64, 0, order).unwrap();
            let mut header = [0; 4];
            receiver.read_exact(&mut header).unwrap();
            assert_eq!(header, order.encode(4));
            let mut body = [0; 4];
            receiver.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"true");

            let (sender, receiver) = UnixStream::pair().unwrap();
            send_json_with_fds_until(&sender, &true, &[], deadline(), 64, 0, order).unwrap();
            let (value, descriptors): (bool, _) =
                receive_json_with_fds_until(&receiver, deadline(), 64, 0, order).unwrap();
            assert!(value);
            assert!(descriptors.is_empty());

            let (reader, writer) = pipe();
            send_json_with_fds_until(
                &sender,
                &2,
                &[writer.as_fd(), writer.as_fd()],
                deadline(),
                64,
                3,
                order,
            )
            .unwrap();
            drop(writer);
            let (value, descriptors): (u8, _) =
                receive_json_with_fds_until(&receiver, deadline(), 64, 3, order).unwrap();
            assert_eq!(value, 2);
            assert_eq!(
                descriptors.len(),
                2,
                "the caller owns exact protocol counts"
            );
            drop(descriptors);
            assert_pipe_closed(&reader);
        }
    }

    #[test]
    fn invalid_body_oversize_eof_and_late_rights_close_every_received_fd() {
        for case in [
            "invalid",
            "oversize",
            "eof",
            "late-header",
            "late-body",
            "excess",
        ] {
            let (mut sender, receiver) = UnixStream::pair().unwrap();
            let (reader, writer) = pipe();
            match case {
                "invalid" => raw_send(&sender, b"\x01\0\0\0?", &[writer.as_fd()]),
                "oversize" => raw_send(&sender, b"\xff\xff\xff\x7f", &[writer.as_fd()]),
                "eof" => raw_send(&sender, b"\x04", &[writer.as_fd()]),
                "late-header" => {
                    sender.write_all(b"\x04").unwrap();
                    raw_send(&sender, b"\0\0\0true", &[writer.as_fd()]);
                }
                "late-body" => {
                    sender.write_all(b"\x04\0\0\0").unwrap();
                    raw_send(&sender, b"true", &[writer.as_fd()]);
                }
                "excess" => raw_send(
                    &sender,
                    b"\x04\0\0\0true",
                    &[writer.as_fd(), writer.as_fd()],
                ),
                _ => unreachable!(),
            }
            drop(writer);
            drop(sender);
            let result = receive_json_with_fds_until::<Value>(
                &receiver,
                deadline(),
                64,
                1,
                FrameByteOrder::LittleEndian,
            );
            assert!(result.is_err(), "{case} must fail");
            assert_pipe_closed(&reader);
        }
    }

    #[test]
    fn truncated_or_unexpected_ancillary_closes_installed_rights() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (reader, writer) = pipe();
        raw_send(
            &sender,
            b"x",
            &[writer.as_fd(), writer.as_fd(), writer.as_fd()],
        );
        drop(writer);
        let mut descriptors = Vec::new();
        let error = receive_part(
            &receiver,
            &mut [0],
            &mut descriptors,
            true,
            3,
            deadline(),
            unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) } as usize,
        )
        .unwrap_err();
        assert!(error.contains("truncated"));
        drop(descriptors);
        assert_pipe_closed(&reader);

        let (sender, receiver) = UnixStream::pair().unwrap();
        let enabled: libc::c_int = 1;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    receiver.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PASSCRED,
                    (&enabled as *const libc::c_int).cast(),
                    size_of_val(&enabled) as _,
                )
            },
            0
        );
        let (reader, writer) = pipe();
        raw_send(&sender, b"\x04\0\0\0true", &[writer.as_fd()]);
        drop(writer);
        assert!(
            receive_json_with_fds_until::<Value>(
                &receiver,
                deadline(),
                64,
                1,
                FrameByteOrder::LittleEndian,
            )
            .unwrap_err()
            .contains("unexpected")
        );
        assert_pipe_closed(&reader);
    }

    #[test]
    fn malformed_ancillary_lengths_are_checked_before_payload_access() {
        assert!(take_rights(&[0], &mut Vec::new()).is_err());
        for length in [0, size_of::<libc::cmsghdr>() - 1, usize::MAX] {
            let header = libc::cmsghdr {
                cmsg_len: length as _,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: libc::SCM_RIGHTS,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&header as *const libc::cmsghdr).cast::<u8>(),
                    size_of_val(&header),
                )
            };
            let mut descriptors = Vec::new();
            assert!(take_rights(bytes, &mut descriptors).is_err());
            assert!(descriptors.is_empty());
        }
    }

    #[test]
    fn partial_receive_keeps_one_absolute_deadline_and_closes_early_rights() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (reader, writer) = pipe();
        let (release, released) = mpsc::channel();
        let (sent, first_sent) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            raw_send(&sender, b"\x04", &[writer.as_fd()]);
            drop(writer);
            sent.send(()).unwrap();
            // The complete valid frame would take 210ms. Progress continues
            // across the 100ms absolute deadline; a rearmed per-read timeout
            // would instead finish successfully. Keep the socket alive while
            // waiting, so the expected error cannot be inferred from EOF.
            for byte in b"\0\0\0true" {
                match released.recv_timeout(Duration::from_millis(30)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => raw_send(&sender, &[*byte], &[]),
                }
            }
        });
        first_sent.recv_timeout(Duration::from_secs(2)).unwrap();
        let flags = socket_flags(&receiver);
        let started = Instant::now();
        let result = receive_json_with_fds_until::<Value>(
            &receiver,
            started + Duration::from_millis(100),
            64,
            1,
            FrameByteOrder::LittleEndian,
        );
        assert!(result.unwrap_err().contains("deadline"));
        assert!(started.elapsed() < Duration::from_millis(180));
        assert_eq!(socket_flags(&receiver), flags);
        assert_pipe_closed(&reader);
        release.send(()).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn partial_send_delivers_rights_once_and_stalled_send_honors_deadline() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let buffer: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    sender.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&buffer as *const libc::c_int).cast(),
                    size_of_val(&buffer) as _,
                )
            },
            0
        );
        let flags = socket_flags(&sender);
        let directory = File::open(".").unwrap();
        let value = "x".repeat(1 << 20);
        let started = Instant::now();
        let error = send_json_with_fds_until(
            &sender,
            &value,
            &[directory.as_fd()],
            started + Duration::from_millis(200),
            2 << 20,
            1,
            FrameByteOrder::BigEndian,
        )
        .unwrap_err();
        assert!(error.contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(socket_flags(&sender), flags);
        assert_eq!(sender.write_timeout().unwrap(), None);

        // Inspect the actual partially delivered stream, not a send-call count.
        // At least one send succeeded, and every later send omitted the rights.
        let mut descriptors = Vec::new();
        let mut bytes = vec![0; 8192];
        let mut total = 0;
        sender.shutdown(std::net::Shutdown::Write).unwrap();
        loop {
            match receive_part(
                &receiver,
                &mut bytes,
                &mut descriptors,
                true,
                MAX_RIGHTS,
                deadline(),
                size_of::<[u64; CONTROL_WORDS]>(),
            ) {
                Ok(count) => total += count,
                Err(error) if error.contains("EOF") => break,
                Err(error) => panic!("{error}"),
            }
        }
        assert!(total > 4 && total < value.len());
        assert_eq!(descriptors.len(), 1);

        let (sender, receiver) = UnixStream::pair().unwrap();
        let expected = value.clone();
        let thread = std::thread::spawn(move || {
            let (actual, descriptors): (String, _) = receive_json_with_fds_until(
                &receiver,
                deadline(),
                2 << 20,
                1,
                FrameByteOrder::LittleEndian,
            )
            .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(descriptors.len(), 1);
        });
        send_json_with_fds_until(
            &sender,
            &value,
            &[directory.as_fd()],
            deadline(),
            2 << 20,
            1,
            FrameByteOrder::LittleEndian,
        )
        .unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn sender_rejects_oversize_or_excess_before_emitting_any_frame_bytes() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let directory = File::open(".").unwrap();
        assert!(
            send_json_with_fds_until(
                &sender,
                &"oversized",
                &[],
                deadline(),
                1,
                0,
                FrameByteOrder::LittleEndian,
            )
            .is_err()
        );
        assert!(
            send_json_with_fds_until(
                &sender,
                &true,
                &[directory.as_fd()],
                deadline(),
                64,
                0,
                FrameByteOrder::LittleEndian,
            )
            .is_err()
        );
        let mut byte = 0_u8;
        assert_eq!(
            unsafe {
                libc::recv(
                    receiver.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_DONTWAIT,
                )
            },
            -1
        );
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    }
}
