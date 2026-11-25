//! This process's own executable image: where it is installed, how to start
//! another process from exactly it, and the command line it was started with.
//!
//! Two different questions get two different answers:
//!
//! - [`current_executable`] names the installed file this process runs,
//!   resolved through every symlink. Code that looks for companion
//!   executables beside it, or recognizes an installation layout, asks this.
//!   The file it names can be replaced afterwards, as an update does, so it
//!   never proves what an `exec` of it would run.
//! - [`own_image`] gives a path that executes exactly the image this process
//!   runs, even after an update replaced the installed file. A process that
//!   starts a copy of itself to serve in a different role, such as a holder,
//!   asks this so that both sides speak the same protocol.
//!
//! Linux answers both from `/proc/self/exe`, a link the kernel resolves to
//! the mapped image itself. Darwin has no such link:
//!
//! - `proc_pidpath` answers the first. Unlike `std::env::current_exe`, which
//!   on Darwin returns the path the process was started through, symlinks
//!   included, it names the file that was mapped. A process started through
//!   `~/.local/bin/cagents` therefore still finds its companions in the
//!   installation's own directory, and one started through an installation's
//!   `current` link keeps the version it started with when that link flips.
//! - The image is pinned by an open descriptor checked against the vnode the
//!   kernel mapped, and each start clones that descriptor into a private
//!   directory, executes the clone and then unlinks it. APFS clones share
//!   their blocks, so a clone costs no copy; elsewhere the image is copied.
use std::{io, path::PathBuf};

/// The installed executable this process runs, with every symlink resolved.
/// See the module documentation for how this differs from [`own_image`].
pub fn current_executable() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    return std::env::current_exe();
    #[cfg(target_os = "macos")]
    return darwin::mapped_path();
}

/// This process's command line, one entry per argument, the program name
/// first. Safe to call from an initializer that runs before `main`.
#[cfg(feature = "host")]
pub(crate) fn command_line() -> Option<Vec<Vec<u8>>> {
    #[cfg(target_os = "linux")]
    {
        let mut arguments: Vec<Vec<u8>> = std::fs::read("/proc/self/cmdline")
            .ok()?
            .split(|&byte| byte == 0)
            .map(<[u8]>::to_vec)
            .collect();
        // Each argument is NUL-terminated, so the split ends with an empty one.
        if arguments.last().is_some_and(Vec::is_empty) {
            arguments.pop();
        }
        Some(arguments)
    }
    #[cfg(target_os = "macos")]
    {
        Some(darwin::arguments())
    }
}

/// A path that executes exactly this process's image, valid while this value
/// lives. Keep it until the spawn that uses it has returned: the kernel has
/// then executed it.
#[cfg(feature = "host")]
pub(crate) struct OwnImage {
    #[cfg(target_os = "macos")]
    path: PathBuf,
}

#[cfg(feature = "host")]
impl OwnImage {
    pub(crate) fn path(&self) -> &std::path::Path {
        #[cfg(target_os = "linux")]
        return std::path::Path::new("/proc/self/exe");
        #[cfg(target_os = "macos")]
        return &self.path;
    }
}

#[cfg(all(feature = "host", target_os = "macos"))]
impl Drop for OwnImage {
    fn drop(&mut self) {
        // A process already executing the clone keeps it mapped.
        let _ = std::fs::remove_file(&self.path);
        if let Some(directory) = self.path.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

/// A path that executes exactly this process's image, for starting a copy of
/// this process in another role. See the module documentation.
///
/// Refused on Darwin when the installed file was replaced before this
/// process first pinned its image; [`pin_own_image`] early avoids that.
#[cfg(feature = "host")]
pub(crate) fn own_image() -> io::Result<OwnImage> {
    #[cfg(target_os = "linux")]
    return Ok(OwnImage {});
    #[cfg(target_os = "macos")]
    return darwin::clone_image().map(|path| OwnImage { path });
}

/// Pins this process's image now, so [`own_image`] keeps working after the
/// installed file is replaced. A long-lived process that starts copies of
/// itself calls this once at startup. Linux needs no pin.
#[cfg(feature = "host")]
pub fn pin_own_image() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    return Ok(());
    #[cfg(target_os = "macos")]
    return darwin::pinned_image().map(drop);
}

#[cfg(target_os = "macos")]
mod darwin {
    use std::{ffi::OsString, io, os::unix::ffi::OsStringExt, path::PathBuf};

    /// The path of the vnode the kernel executed for this process.
    pub(super) fn mapped_path() -> io::Result<PathBuf> {
        let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is writable and exactly as long as declared.
        let written = unsafe {
            libc::proc_pidpath(
                std::process::id() as libc::c_int,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if written <= 0 {
            return Err(io::Error::last_os_error());
        }
        buffer.truncate(written as usize);
        Ok(PathBuf::from(OsString::from_vec(buffer)))
    }

    /// Darwin's standard library reads the arguments from `_NSGetArgv`, which
    /// libSystem sets before any initializer of the program runs.
    #[cfg(feature = "host")]
    pub(super) fn arguments() -> Vec<Vec<u8>> {
        std::env::args_os().map(OsString::into_vec).collect()
    }

    #[cfg(all(feature = "host", test))]
    pub(super) use image::mapped_identity;
    #[cfg(feature = "host")]
    pub(super) use image::{clone_image, pinned_image};

    #[cfg(feature = "host")]
    mod image {
        use std::{
            ffi::{CString, OsStr},
            fs::{self, File},
            io,
            os::{
                fd::AsRawFd,
                unix::{
                    ffi::OsStrExt,
                    fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
                },
            },
            path::{Path, PathBuf},
            sync::{
                OnceLock,
                atomic::{AtomicU64, Ordering},
            },
        };

        /// `<sys/proc_info.h>`'s `PROC_PIDREGIONPATHINFO`, which libc omits.
        const PROC_PIDREGIONPATHINFO: libc::c_int = 8;

        /// `<sys/proc_info.h>`'s `struct proc_regioninfo`.
        #[repr(C)]
        struct RegionInfo {
            protection: u32,
            max_protection: u32,
            inheritance: u32,
            flags: u32,
            offset: u64,
            behavior: u32,
            user_wired_count: u32,
            user_tag: u32,
            pages_resident: u32,
            pages_shared_now_private: u32,
            pages_swapped_out: u32,
            pages_dirtied: u32,
            ref_count: u32,
            shadow_depth: u32,
            share_mode: u32,
            private_pages_resident: u32,
            shared_pages_resident: u32,
            obj_id: u32,
            depth: u32,
            address: u64,
            size: u64,
        }

        /// `<sys/proc_info.h>`'s `struct proc_regionwithpathinfo`.
        #[repr(C)]
        struct RegionWithPath {
            region: RegionInfo,
            vnode: libc::vnode_info_path,
        }

        /// The device and inode of the file mapped at this function's code,
        /// which the executable links in: the image the kernel executed.
        pub(crate) fn mapped_identity() -> io::Result<(u64, u64)> {
            let address = mapped_identity as usize as u64;
            // SAFETY: all-zero is a valid value of this plain C struct.
            let mut info: RegionWithPath = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<RegionWithPath>() as libc::c_int;
            // SAFETY: the buffer is writable and exactly as long as declared.
            let written = unsafe {
                libc::proc_pidinfo(
                    std::process::id() as libc::c_int,
                    PROC_PIDREGIONPATHINFO,
                    address,
                    (&raw mut info).cast(),
                    size,
                )
            };
            if written != size {
                return Err(io::Error::other(
                    "the kernel did not describe this process's mapped image",
                ));
            }
            // The kernel answers the region at or after the address.
            if !(info.region.address..info.region.address + info.region.size).contains(&address) {
                return Err(io::Error::other(
                    "this process's code is not mapped from a file",
                ));
            }
            let stat = &info.vnode.vip_vi.vi_stat;
            Ok((u64::from(stat.vst_dev), stat.vst_ino))
        }

        /// An open descriptor of this process's own image, opened by path
        /// and kept only when it is the vnode the kernel mapped.
        pub(crate) fn pinned_image() -> io::Result<&'static File> {
            static IMAGE: OnceLock<File> = OnceLock::new();
            if let Some(image) = IMAGE.get() {
                return Ok(image);
            }
            let path = super::mapped_path()?;
            let image = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&path)?;
            let metadata = image.metadata()?;
            // Darwin reports a device as a 32-bit number in both places.
            let opened = (u64::from(metadata.dev() as u32), metadata.ino());
            if opened != mapped_identity()? {
                return Err(io::Error::other(format!(
                    "{} was replaced after this process started, so it cannot start a copy of itself",
                    path.display()
                )));
            }
            Ok(IMAGE.get_or_init(|| image))
        }

        /// A new private file holding exactly this process's image, alone in
        /// its own directory under the executable's file name: Darwin names a
        /// process after the file it executed.
        pub(crate) fn clone_image() -> io::Result<PathBuf> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let image = pinned_image()?;
            let images = crate::instance::runtime_directory()?.join("images");
            crate::secure_file::prepare_private_directory(&images)?;
            sweep(&images);
            let directory = images.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // One a process that held this pid before left behind.
            let _ = fs::remove_dir_all(&directory);
            fs::DirBuilder::new().mode(0o700).create(&directory)?;
            let name = super::mapped_path()?
                .file_name()
                .map_or_else(|| "cagents".into(), ToOwned::to_owned);
            let path = directory.join(&name);
            let cloned = clone_into(image, &directory, &name).or_else(|error| {
                // A volume without clones, or a runtime directory on another one.
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EXDEV)
                ) {
                    copy(image, &path)
                } else {
                    Err(error)
                }
            });
            if let Err(error) = cloned {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
            Ok(path)
        }

        fn clone_into(image: &File, directory: &Path, name: &OsStr) -> io::Result<()> {
            let directory = File::open(directory)?;
            let name = CString::new(name.as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "image name has a NUL"))?;
            // SAFETY: both descriptors are open for the call and the name is
            // NUL-terminated. The clone is created anew, never followed.
            let cloned = unsafe {
                libc::fclonefileat(image.as_raw_fd(), directory.as_raw_fd(), name.as_ptr(), 0)
            };
            if cloned == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        /// Writes the whole image from its pinned descriptor, never sharing
        /// that descriptor's offset with another copy.
        fn copy(image: &File, path: &Path) -> io::Result<()> {
            use std::{io::Write, os::unix::fs::FileExt};
            let mut copy = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .custom_flags(libc::O_CLOEXEC)
                .open(path)?;
            let mut buffer = vec![0_u8; 1 << 20];
            let mut offset = 0;
            let result = loop {
                match image.read_at(&mut buffer, offset) {
                    Ok(0) => break Ok(()),
                    Ok(read) => {
                        if let Err(error) = copy.write_all(&buffer[..read]) {
                            break Err(error);
                        }
                        offset += read as u64;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => break Err(error),
                }
            };
            if result.is_err() {
                let _ = fs::remove_file(path);
            }
            result
        }

        /// Removes clones a process that no longer runs left behind, as one
        /// that crashed between cloning and starting would. Pid reuse can
        /// only keep a stale clone a while longer.
        fn sweep(directory: &Path) {
            let Ok(entries) = fs::read_dir(directory) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = std::str::from_utf8(name.as_bytes())
                    .ok()
                    .and_then(|name| name.split_once('-'))
                    .and_then(|(pid, _)| pid.parse::<libc::pid_t>().ok())
                    .filter(|&pid| pid > 0)
                else {
                    continue;
                };
                // SAFETY: signal 0 only checks that the process exists.
                let gone = unsafe { libc::kill(pid, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                if gone {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "host")]
    #[test]
    fn the_command_line_names_this_program_first() {
        let arguments = command_line().unwrap();
        let program = std::env::args_os().next().unwrap();
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            arguments.first().map(Vec::as_slice),
            Some(program.as_bytes())
        );
        assert_eq!(arguments.len(), std::env::args_os().count());
    }

    #[test]
    fn the_current_executable_is_this_image_without_symlinks() {
        use std::os::unix::fs::MetadataExt;
        let executable = current_executable().unwrap();
        assert_eq!(executable, executable.canonicalize().unwrap());
        let running = std::fs::metadata(&executable).unwrap();
        let started = std::fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(
            (running.dev(), running.ino()),
            (started.dev(), started.ino())
        );
    }

    /// Run through a symlink, a process still names its installed file:
    /// companions are found beside that, not beside the link.
    #[test]
    fn a_process_started_through_a_symlink_names_the_installed_file() {
        if std::env::var_os("CAGENTS_PRINT_CURRENT_EXECUTABLE").is_some() {
            // libtest prints the test's name on the same line before this.
            println!(
                "\ncurrent executable: {}",
                current_executable().unwrap().display()
            );
            return;
        }
        let executable = current_executable().unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("linked");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        let output = std::process::Command::new(&link)
            .args([
                "--exact",
                "process::image::tests::a_process_started_through_a_symlink_names_the_installed_file",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CAGENTS_PRINT_CURRENT_EXECUTABLE", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let printed = String::from_utf8(output.stdout).unwrap();
        assert!(
            printed
                .lines()
                .filter_map(|line| line.strip_prefix("current executable: "))
                .any(|line| std::path::Path::new(line) == executable),
            "{printed}"
        );
    }

    /// Runs `test` from this harness in a child, with its own runtime
    /// directory so clones never land in the installed product's.
    #[cfg(feature = "host")]
    fn run_test_in_child(
        program: &std::path::Path,
        test: &str,
        probe: &str,
        runtime: &std::path::Path,
    ) -> std::process::Output {
        std::process::Command::new(program)
            .args([
                "--exact",
                &format!("process::image::tests::{test}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(probe, "1")
            .env("XDG_RUNTIME_DIR", runtime)
            .output()
            .unwrap()
    }

    #[cfg(feature = "host")]
    #[test]
    fn its_own_image_runs_this_program() {
        const PROBE: &str = "CAGENTS_OWN_IMAGE_PROBE";
        if std::env::var_os("CAGENTS_OWN_IMAGE_RAN").is_some() {
            return;
        }
        if std::env::var_os(PROBE).is_none() {
            let runtime = tempfile::tempdir().unwrap();
            let output = run_test_in_child(
                &current_executable().unwrap(),
                "its_own_image_runs_this_program",
                PROBE,
                runtime.path(),
            );
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let image = own_image().unwrap();
        let status = std::process::Command::new(image.path())
            .args([
                "--exact",
                "process::image::tests::its_own_image_runs_this_program",
                "--test-threads=1",
            ])
            .env("CAGENTS_OWN_IMAGE_RAN", "1")
            .status()
            .unwrap();
        assert!(status.success());
        #[cfg(target_os = "macos")]
        {
            let path = image.path().to_owned();
            assert_eq!(path.file_name(), current_executable().unwrap().file_name());
            drop(image);
            assert!(!path.exists(), "a clone outlived its spawn");
            assert!(
                !path.parent().unwrap().exists(),
                "its directory outlived it"
            );
        }
    }

    /// The kernel's record of the mapped image agrees with the file
    /// [`current_executable`] names, which checks the hand-declared layout.
    #[cfg(all(feature = "host", target_os = "macos"))]
    #[test]
    fn the_mapped_image_is_the_current_executable() {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(current_executable().unwrap()).unwrap();
        assert_eq!(
            darwin::mapped_identity().unwrap(),
            (u64::from(metadata.dev() as u32), metadata.ino())
        );
    }

    /// An update replaces the installed file; a copy started afterwards still
    /// runs the image this process started from.
    #[cfg(all(feature = "host", target_os = "macos"))]
    #[test]
    fn its_own_image_outlives_replacing_the_installed_file() {
        const PROBE: &str = "CAGENTS_REPLACED_IMAGE_PROBE";
        if std::env::var_os(PROBE).is_none() {
            // Replace a copy of this harness, never the build's own file.
            let fixture = tempfile::tempdir().unwrap();
            let installed = fixture.path().join("installed");
            std::fs::copy(current_executable().unwrap(), &installed).unwrap();
            let output = run_test_in_child(
                &installed,
                "its_own_image_outlives_replacing_the_installed_file",
                PROBE,
                fixture.path(),
            );
            assert!(output.status.success(), "{output:?}");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        pin_own_image().unwrap();
        let installed = current_executable().unwrap();
        let replacement = installed.with_file_name("replacement");
        std::fs::write(&replacement, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&replacement, &installed).unwrap();
        let replaced = std::process::Command::new(&installed).status().unwrap();
        assert_eq!(
            replaced.code(),
            Some(3),
            "the installed file was not replaced"
        );
        let image = own_image().unwrap();
        let status = std::process::Command::new(image.path())
            .args([
                "--exact",
                "process::image::tests::the_command_line_names_this_program_first",
                "--test-threads=1",
            ])
            .status()
            .unwrap();
        assert!(status.success(), "the clone did not run this image");
    }
}
