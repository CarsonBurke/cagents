//! What Darwin tells one process about another, where Linux reads `/proc`:
//! its argument vector and environment, and the files its descriptors name.
//!
//! Every answer describes whichever process holds the pid at that instant.
//! A caller that must know it describes a particular process pins it first
//! (`crate::pinned_process::PinnedProcess`) and checks the pin afterwards.
//! The process list itself is [`super::table`].

use std::{ffi::OsString, io, os::unix::ffi::OsStringExt, path::PathBuf};

/// `PROC_PIDFDVNODEPATHINFO` from `<sys/proc_info.h>`, which libc omits.
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
/// `FWRITE` from `<sys/fcntl.h>`: the open file allows writing. A file's
/// recorded flags are the kernel's `F*` flags, not the `O_*` ones it was
/// opened with.
const FWRITE: u32 = 0x0002;
/// `VREG` from `<sys/vnode.h>`: a regular file.
const VREG: libc::c_int = 1;

/// `struct proc_fileinfo` from `<sys/proc_info.h>`.
#[repr(C)]
pub(crate) struct ProcFileInfo {
    pub(crate) fi_openflags: u32,
    pub(crate) fi_status: u32,
    pub(crate) fi_offset: libc::off_t,
    pub(crate) fi_type: i32,
    pub(crate) fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` from `<sys/proc_info.h>`.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

/// A process's argument vector and environment, as it was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arguments {
    pub arguments: Vec<Vec<u8>>,
    pub environment: Vec<Vec<u8>>,
}

impl Arguments {
    /// The arguments each followed by a NUL, as Linux's `/proc/<pid>/cmdline`
    /// spells them.
    #[must_use]
    pub fn command_line(&self) -> Vec<u8> {
        nul_terminated(&self.arguments)
    }

    /// The environment each entry followed by a NUL, as Linux's
    /// `/proc/<pid>/environ` spells it.
    #[must_use]
    pub fn environ(&self) -> Vec<u8> {
        nul_terminated(&self.environment)
    }
}

fn nul_terminated(entries: &[Vec<u8>]) -> Vec<u8> {
    entries
        .iter()
        .flat_map(|entry| entry.iter().copied().chain([0]))
        .collect()
}

/// The arguments and environment of the process that holds `pid`. Darwin only
/// shows them for a process of the same user (or to root), and not for a
/// zombie. The executable path's padding cannot be told from an empty first
/// argument, so, as `ps` does, an empty first argument is lost and the rest
/// shift by one into the environment.
///
/// # Errors
///
/// Returns the underlying error when the kernel refuses, and `InvalidData`
/// when its record is malformed.
pub fn arguments(pid: u32) -> io::Result<Arguments> {
    // Reserved, not zeroed: most records are far smaller than the limit, so
    // only the pages the kernel writes are touched.
    let mut buffer: Vec<u8> = Vec::with_capacity(argument_limit()?);
    let mut length = buffer.capacity();
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, kernel_pid(pid)?];
    // SAFETY: the name has the three entries declared, and the buffer is
    // writable for `length` bytes, which the kernel updates to what it wrote.
    if unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.capacity() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process argument record filled its buffer",
        ));
    }
    // SAFETY: the kernel initialized the first `length` bytes and the
    // preceding check confirms they are within the allocation.
    unsafe { buffer.set_len(length) };
    parse_arguments(&buffer)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed process arguments"))
}

/// `pid` as the kernel's interfaces take it.
fn kernel_pid(pid: u32) -> io::Result<libc::c_int> {
    libc::c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"))
}

/// The working directory reported for a process. This is a kernel path
/// observation, not a retained directory or proof it cannot change.
#[cfg(feature = "host")]
pub(crate) fn working_directory(pid: u32) -> io::Result<PathBuf> {
    let pid = kernel_pid(pid)?;
    // PROC_FLAG_THCWD in <sys/proc_info.h>. A single process-wide path
    // cannot describe a process with a thread-specific working directory.
    const PROC_FLAG_THCWD: u32 = 0x100;
    if super::table::record(pid)?.bsd.pbsi_flags & PROC_FLAG_THCWD != 0 {
        return Err(io::Error::other(
            "process has a thread-specific working directory",
        ));
    }
    // SAFETY: zero is valid for the integer and path-buffer fields.
    let mut paths: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&paths) as libc::c_int;
    // SAFETY: paths is writable for the declared buffer length.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut paths).cast(),
            size,
        )
    };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    if written != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short working directory record",
        ));
    }
    // SAFETY: vip_path contains only bytes and its bindings lay them out in
    // contiguous arrays. Inspect at most the size actually in the record.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            paths.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
            std::mem::size_of_val(&paths.pvi_cdir.vip_path),
        )
    };
    let path = std::ffi::CStr::from_bytes_until_nul(bytes).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "unterminated working directory")
    })?;
    let path = PathBuf::from(OsString::from_vec(path.to_bytes().to_vec()));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "working directory has no absolute name",
        ));
    }
    let metadata = std::fs::metadata(&path)?;
    use std::os::unix::fs::MetadataExt;
    let vnode = &paths.pvi_cdir.vip_vi.vi_stat;
    if !metadata.is_dir()
        || metadata.dev() != vnode.vst_dev as i32 as u64
        || metadata.ino() != vnode.vst_ino
    {
        return Err(io::Error::other(
            "working directory no longer names the observed vnode",
        ));
    }
    Ok(path)
}

/// The largest argument record the kernel produces.
fn argument_limit() -> io::Result<usize> {
    let mut limit: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>();
    let mut name = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: the name has the two entries declared, and `limit` is writable
    // for exactly `length` bytes.
    if unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            2,
            (&raw mut limit).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(limit)
        .ok()
        .filter(|limit| *limit > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid argument limit"))
}

/// `KERN_PROCARGS2` is the argument count, the executable's path, NUL
/// padding, then `argc` NUL-terminated arguments and the NUL-terminated
/// environment, which ends at an empty entry or the end of the record.
fn parse_arguments(record: &[u8]) -> Option<Arguments> {
    let count = usize::try_from(i32::from_ne_bytes(record.get(..4)?.try_into().ok()?)).ok()?;
    let rest = record.get(4..)?;
    let rest = match rest.iter().position(|byte| *byte == 0) {
        Some(end) => &rest[end..],
        // A record of a process with no arguments may end at its count.
        None if count == 0 => &[],
        None => return None,
    };
    let rest = &rest[rest
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(rest.len())..];
    let mut entries = rest.split(|byte| *byte == 0);
    let arguments = entries
        .by_ref()
        .take(count)
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if arguments.len() != count {
        return None;
    }
    let environment = entries
        .take_while(|entry| !entry.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    Some(Arguments {
        arguments,
        environment,
    })
}

/// A file one of a process's descriptors names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFile {
    pub descriptor: i32,
    /// The path the kernel records for the file. It is a name, not the file:
    /// open it and compare `device` and `inode` before trusting it.
    pub path: PathBuf,
    /// Whether the descriptor was opened for writing.
    pub writable: bool,
    pub regular: bool,
    /// As `MetadataExt::dev` reports it.
    pub device: u64,
    pub inode: u64,
}

/// The files behind `pid`'s descriptors, at most `limit` descriptors read.
///
/// A descriptor closed after the list was read has nothing to report and is
/// left out, as Linux leaves out a `/proc/<pid>/fd` link that no longer
/// reads. Any other failure to read one is an error, so a census never takes
/// a file it could not see for one that is not open.
///
/// # Errors
///
/// Returns `NotFound` when no such process exists, and the underlying error
/// when the kernel refuses, as it does for another user's process.
pub fn open_files(pid: u32, limit: usize) -> io::Result<Vec<OpenFile>> {
    let mut files = Vec::new();
    for descriptor in descriptors(pid)?
        .into_iter()
        .filter(|descriptor| descriptor.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .take(limit)
    {
        match open_file(pid, descriptor.proc_fd) {
            Ok(file) => files.push(file),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EBADF | libc::ENOENT)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(files)
}

/// A bounded, complete observation for a caller making an identity claim.
/// A changed descriptor is uncertainty, even if a later scan could retry.
#[cfg(feature = "host")]
pub(crate) fn observed_open_files(pid: u32, limit: usize) -> io::Result<Vec<OpenFile>> {
    let mut files = Vec::new();
    for descriptor in descriptors(pid)?
        .into_iter()
        .filter(|descriptor| descriptor.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
    {
        if files.len() == limit {
            return Err(io::Error::other(
                "process holds too many files to establish identity",
            ));
        }
        files.push(open_file(pid, descriptor.proc_fd)?);
    }
    Ok(files)
}

fn open_file(pid: u32, descriptor: i32) -> io::Result<OpenFile> {
    let pid = kernel_pid(pid)?;
    // SAFETY: all-zero bytes are a valid value of this plain C struct.
    let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<VnodeFdInfoWithPath>())
        .expect("vnode_fdinfowithpath fits c_int");
    // SAFETY: the buffer is writable and exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidfdinfo(
            pid,
            descriptor,
            PROC_PIDFDVNODEPATHINFO,
            (&raw mut info).cast(),
            size,
        )
    };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    if written != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the kernel described an open file in an unexpected size",
        ));
    }
    // SAFETY: `vip_path` is `MAXPATHLEN` bytes of plain data, laid out as
    // nested arrays only for libc's sake.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.pvip.vip_path.as_ptr().cast::<u8>(),
            std::mem::size_of_val(&info.pvip.vip_path),
        )
    };
    let path = &bytes[..bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len())];
    let stat = &info.pvip.vip_vi.vi_stat;
    Ok(OpenFile {
        descriptor,
        path: PathBuf::from(OsString::from_vec(path.to_vec())),
        writable: info.pfi.fi_openflags & FWRITE != 0,
        regular: info.pvip.vip_vi.vi_type == VREG,
        // `vst_dev` is the same `dev_t` `stat` reports, widened as
        // `MetadataExt::dev` widens it.
        device: stat.vst_dev as i32 as u64,
        inode: stat.vst_ino,
    })
}

/// Every descriptor `pid` holds, as `PROC_PIDLISTFDS` reports them.
pub(crate) fn descriptors(pid: u32) -> io::Result<Vec<libc::proc_fdinfo>> {
    let pid = kernel_pid(pid)?;
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // The list can grow before it is read, so leave room and retry if it
    // filled the buffer.
    let mut capacity = 64;
    loop {
        let mut list: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
        let bytes = libc::c_int::try_from(capacity * entry)
            .map_err(|_| io::Error::other("descriptor list too large"))?;
        // SAFETY: the buffer has room for `capacity` records, and the kernel
        // writes at most `bytes` bytes of them.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                list.as_mut_ptr().cast(),
                bytes,
            )
        };
        if written <= 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(0) | None => io::Error::from(io::ErrorKind::NotFound),
                Some(libc::ESRCH) => io::Error::from(io::ErrorKind::NotFound),
                _ => error,
            });
        }
        let count = written as usize / entry;
        if count < capacity {
            // SAFETY: the kernel initialized the first `count` records.
            unsafe { list.set_len(count) };
            return Ok(list);
        }
        capacity *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    };

    #[test]
    fn an_argument_record_splits_into_arguments_and_environment() {
        let mut record = 2_i32.to_ne_bytes().to_vec();
        record.extend_from_slice(b"/bin/program\0\0\0\0program\0\0A=1\0B=2\0\0\0");
        assert_eq!(
            parse_arguments(&record),
            Some(Arguments {
                arguments: vec![b"program".to_vec(), Vec::new()],
                environment: vec![b"A=1".to_vec(), b"B=2".to_vec()],
            })
        );
        assert_eq!(parse_arguments(&record[..10]), None);
        assert_eq!(
            parse_arguments(&0_i32.to_ne_bytes()),
            Some(Arguments {
                arguments: Vec::new(),
                environment: Vec::new(),
            })
        );
    }

    #[test]
    fn this_process_reads_its_own_arguments_and_environment() {
        let read = arguments(std::process::id()).unwrap();
        let expected: Vec<Vec<u8>> = std::env::args_os()
            .map(|argument| argument.as_bytes().to_vec())
            .collect();
        assert_eq!(read.arguments, expected);
        assert!(
            read.environment
                .iter()
                .any(|entry| entry.starts_with(b"PATH="))
        );
    }

    #[test]
    fn an_open_file_names_its_path_identity_and_access() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("written"), b"").unwrap();
        // The kernel records the path through /private, not the /var link.
        let path = directory.path().join("written").canonicalize().unwrap();
        let written = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let read = std::fs::File::open(&path).unwrap();
        let metadata = written.metadata().unwrap();
        let files = open_files(std::process::id(), usize::MAX).unwrap();
        let find = |descriptor: i32| {
            files
                .iter()
                .find(|file| file.descriptor == descriptor)
                .unwrap()
                .clone()
        };
        let (written, read) = (find(written.as_raw_fd()), find(read.as_raw_fd()));
        assert_eq!(written.path, path);
        assert!(written.writable && written.regular);
        assert!(!read.writable);
        assert_eq!(
            (written.device, written.inode),
            (metadata.dev(), metadata.ino())
        );
    }
}
