//! An older private editor may launch its own daemon after the source exits.
//! It cannot be assumed to understand the destination's admission fence.
use super::*;
use std::os::unix::ffi::OsStrExt;

struct CompanionIdentity {
    protocol: u16,
    instance: String,
    daemon_qualified: bool,
}
#[cfg(test)]
impl From<u16> for CompanionIdentity {
    fn from(protocol: u16) -> Self {
        Self {
            protocol,
            instance: crate::instance::NAME.into(),
            daemon_qualified: true,
        }
    }
}

pub(crate) fn preflight_editor_companions(candidate_protocol: Option<u16>) -> io::Result<()> {
    let runtime = crate::instance::runtime_directory()?;
    preflight_in(Path::new("/proc"), &runtime, candidate_protocol, |cli| {
        let instance = crate::update::companion_namespace(cli).map_err(io::Error::other)?;
        let protocol = if instance == crate::instance::NAME {
            crate::update::companion_protocol(cli).map_err(io::Error::other)?
        } else {
            0
        };
        let daemon_qualified = instance == crate::instance::NAME
            && candidate_protocol == Some(crate::terminal_ipc::PROTOCOL_VERSION)
            && candidate_protocol == Some(protocol)
            && same_daemon_binary(
                &crate::companion::Product::Daemon.beside(cli)?,
                &crate::companion::Product::Daemon.beside(&std::env::current_exe()?)?,
            )?;
        Ok(CompanionIdentity {
            instance,
            protocol,
            daemon_qualified,
        })
    })
}

fn preflight_in(
    proc_root: &Path,
    runtime: &Path,
    candidate_protocol: Option<u16>,
    mut protocol: impl FnMut(&Path) -> io::Result<CompanionIdentity>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(proc_root)? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let process = entry.path();
        if !std::fs::metadata(&process)
            .is_ok_and(|metadata| metadata.uid() == unsafe { libc::geteuid() })
        {
            continue;
        }
        let Some(executable) =
            crate::instance::editor_executable(&process, std::fs::read_link(process.join("exe")))?
        else {
            continue;
        };
        // Read only for namespace comparison, never include environment bytes
        // in an error, log or update preview.
        let mut environment = Vec::new();
        match File::open(process.join("environ")) {
            Ok(file) => {
                file.take(1024 * 1024 + 1).read_to_end(&mut environment)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "Cannot verify Ceditor process {pid}'s service namespace; close it before upgrading"
                    ),
                ));
            }
        }
        if environment.len() > 1024 * 1024 {
            return Err(io::Error::other(format!(
                "Cannot verify Ceditor process {pid}'s service namespace; close it before upgrading"
            )));
        }
        let editor_runtime = environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"XDG_RUNTIME_DIR="));
        let Some(editor_runtime) = editor_runtime else {
            return Err(io::Error::other(format!(
                "Ceditor process {pid} has no verifiable service namespace; close it before upgrading"
            )));
        };
        if !same_namespace(
            runtime,
            Path::new(std::ffi::OsStr::from_bytes(editor_runtime)),
        )? {
            continue;
        }
        let daemon = crate::companion::Product::Daemon.beside(&executable)?;
        let cli = crate::companion::Product::Cli.beside(&executable)?;
        // Older private sets may predate the .ceditor-companions marker. The
        // fixed sibling pair must still be checked before its owner stops.
        if executable
            .file_name()
            .is_some_and(|name| name.as_bytes().ends_with(b" (deleted)"))
        {
            return Err(io::Error::other(format!(
                "Ceditor process {pid} is running a removed executable; close or restart it before upgrading"
            )));
        }
        if !daemon.is_file() || !cli.is_file() {
            return Err(io::Error::other(format!(
                "Ceditor process {pid}'s terminal service has no matched CLI/daemon pair to verify its protocol; close or update Ceditor before upgrading"
            )));
        }
        let before = std::fs::read_to_string(process.join("stat"))?;
        let companion_before = std::fs::symlink_metadata(&cli)?;
        let actual = protocol(&cli).map_err(|_| io::Error::other(format!("Cannot verify Ceditor process {pid}'s private service protocol; close or update Ceditor before upgrading")))?;
        let companion_after = std::fs::symlink_metadata(&cli)?;
        let after = std::fs::read_to_string(process.join("stat"))?;
        if process_start(&before) != process_start(&after)
            || process_start(&before).is_none()
            || std::fs::read_link(process.join("exe"))? != executable
            || companion_stamp(&companion_before) != companion_stamp(&companion_after)
        {
            return Err(io::Error::other(
                "Ceditor ownership changed during update preflight; retry without closing the application",
            ));
        }
        if actual.instance != crate::instance::NAME {
            continue;
        }
        let Some(expected) = candidate_protocol else {
            return Err(io::Error::other(format!(
                "Ceditor process {pid} has a private terminal service. Close it before installing, or use the application's reviewed update to check compatibility"
            )));
        };
        if actual.protocol != expected {
            return Err(io::Error::other(format!(
                "Ceditor process {pid}'s private service uses protocol {}, but the update requires {expected}. Close or update Ceditor first; application windows and conversations were kept open",
                actual.protocol
            )));
        }
        if !actual.daemon_qualified {
            return Err(io::Error::other(format!(
                "Ceditor process {pid}'s private daemon cannot be qualified as the matched destination build. Close or update Ceditor before upgrading; application windows and conversations were kept open"
            )));
        }
    }
    Ok(())
}

pub(crate) fn trusted_executable_owner(owner: u32) -> bool {
    owner == 0 || owner == unsafe { libc::geteuid() }
}

/// Never execute an unknown older daemon to ask its version: historical
/// entrypoints may ignore informational flags and start a service. Only exact
/// bytes from the coordinator's already trusted matched daemon qualify.
fn same_daemon_binary(candidate: &Path, expected: &Path) -> io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt;
    fn open(path: &Path) -> io::Result<(File, std::fs::Metadata)> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || !trusted_executable_owner(metadata.uid())
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
            || metadata.len() == 0
            || metadata.len() > 512 * 1024 * 1024
        {
            return Err(io::Error::other(
                "The editor daemon is not an owned bounded executable",
            ));
        }
        Ok((file, metadata))
    }
    let (mut candidate_file, before_candidate) = open(candidate)?;
    let (mut expected_file, before_expected) = open(expected)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut equal = before_candidate.len() == before_expected.len();
    if equal {
        let mut left = [0; 64 * 1024];
        let mut right = [0; 64 * 1024];
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Editor daemon qualification exceeded its bound",
                ));
            }
            let count = candidate_file.read(&mut left)?;
            expected_file.read_exact(&mut right[..count])?;
            if left[..count] != right[..count] {
                equal = false;
                break;
            }
            if count == 0 {
                break;
            }
        }
    }
    for (path, file, before) in [
        (candidate, &candidate_file, &before_candidate),
        (expected, &expected_file, &before_expected),
    ] {
        if companion_stamp(before) != companion_stamp(&file.metadata()?)
            || companion_stamp(before) != companion_stamp(&std::fs::symlink_metadata(path)?)
        {
            return Err(io::Error::other(
                "The matched daemon changed during qualification",
            ));
        }
    }
    Ok(equal)
}

fn companion_stamp(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64, u32, u32) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.uid(),
        metadata.mode(),
    )
}

fn same_namespace(left: &Path, right: &Path) -> io::Result<bool> {
    let left = std::fs::metadata(left)?;
    let right = std::fs::metadata(right)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

fn process_start(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        fs::OpenOptionsExt,
        fs::{PermissionsExt, symlink},
    };

    fn editor(proc_root: &Path, bin: &Path, runtime: &Path) {
        std::fs::create_dir_all(proc_root.join("42")).unwrap();
        std::fs::create_dir_all(bin).unwrap();
        std::fs::create_dir_all(runtime).unwrap();
        for name in ["ceditor", "cagents", "cagentsd"] {
            let path = bin.join(name);
            std::fs::write(&path, "fixture").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(bin.join(".ceditor-companions"), b"ceditor companions v1\n").unwrap();
        symlink(bin.join("ceditor"), proc_root.join("42/exe")).unwrap();
        let mut environment = b"SECRET=must-not-be-printed\0XDG_RUNTIME_DIR=".to_vec();
        environment.extend_from_slice(runtime.as_os_str().as_bytes());
        environment.push(0);
        std::fs::write(proc_root.join("42/environ"), environment).unwrap();
        let mut fields = vec!["0"; 20];
        fields[19] = "100";
        std::fs::write(
            proc_root.join("42/stat"),
            format!("42 (ceditor fixture) {}", fields.join(" ")),
        )
        .unwrap();
    }

    #[test]
    fn incompatible_editor_defers_before_any_window_or_daemon_is_stopped() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let bin = root.path().join("editor/bin");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &bin, &runtime);
        let error = preflight_in(&proc_root, &runtime, Some(77), |_| Ok(56.into())).unwrap_err();
        assert!(error.to_string().contains("protocol 56"));
        assert!(error.to_string().contains("kept open"));
        assert!(!error.to_string().contains("must-not-be-printed"));
        assert!(preflight_in(&proc_root, &runtime, Some(77), |_| Ok(77.into())).is_ok());
        assert!(preflight_in(&proc_root, &runtime, None, |_| Ok(77.into())).is_err());
        let other_runtime = root.path().join("other-runtime");
        std::fs::create_dir(&other_runtime).unwrap();
        assert!(
            preflight_in(&proc_root, &other_runtime, Some(77), |_| panic!(
                "another service namespace"
            ))
            .is_ok()
        );
        let alias = root.path().join("runtime-alias");
        symlink(&runtime, &alias).unwrap();
        assert!(preflight_in(&proc_root, &alias, Some(77), |_| Ok(56.into())).is_err());
        std::fs::remove_file(bin.join(".ceditor-companions")).unwrap();
        assert!(
            preflight_in(&proc_root, &runtime, Some(77), |_| Ok(56.into())).is_err(),
            "unmarked older private companions must be checked too"
        );
        std::fs::remove_file(bin.join("cagents")).unwrap();
        assert!(
            preflight_in(&proc_root, &runtime, Some(77), |_| panic!(
                "missing CLI cannot qualify an existing daemon"
            ))
            .unwrap_err()
            .to_string()
            .contains("no matched CLI")
        );
        std::fs::write(bin.join("cagents"), "fixture").unwrap();
        std::fs::remove_file(proc_root.join("42/exe")).unwrap();
        symlink(bin.join("ceditor (deleted)"), proc_root.join("42/exe")).unwrap();
        assert!(
            preflight_in(&proc_root, &runtime, Some(77), |_| panic!(
                "removed executable cannot be qualified"
            ))
            .is_err()
        );
        std::fs::remove_file(bin.join("cagents")).unwrap();
        std::fs::remove_file(bin.join("cagentsd")).unwrap();
        assert!(
            preflight_in(&proc_root, &runtime, Some(77), |_| panic!(
                "deleted editor and removed private release cannot be qualified"
            ))
            .unwrap_err()
            .to_string()
            .contains("removed executable")
        );
    }

    #[test]
    fn matching_cli_does_not_qualify_a_mixed_generation_daemon() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        let error = preflight_in(&proc_root, &runtime, Some(77), |_| {
            Ok(CompanionIdentity {
                instance: crate::instance::NAME.into(),
                protocol: 77,
                daemon_qualified: false,
            })
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("private daemon cannot be qualified")
        );
        assert!(error.to_string().contains("kept open"));
    }

    #[test]
    fn executable_ownership_accepts_root_or_current_user_only() {
        assert!(trusted_executable_owner(0));
        assert!(trusted_executable_owner(unsafe { libc::geteuid() }));
        let other = if unsafe { libc::geteuid() } == 1 {
            2
        } else {
            1
        };
        assert!(!trusted_executable_owner(other));
    }

    #[test]
    fn exact_daemon_bytes_qualify_without_executing_either_file() {
        let root = tempfile::tempdir().unwrap();
        let expected = root.path().join("matched-daemon");
        let candidate = root.path().join("editor-daemon");
        for path in [&expected, &candidate] {
            std::fs::write(path, b"same native daemon bytes").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(same_daemon_binary(&candidate, &expected).unwrap());
        std::fs::write(&candidate, b"different daemon build!").unwrap();
        assert!(!same_daemon_binary(&candidate, &expected).unwrap());
        let alias = root.path().join("daemon-alias");
        symlink(&expected, &alias).unwrap();
        assert!(same_daemon_binary(&alias, &expected).is_err());
    }

    #[test]
    fn another_compiled_instance_sharing_the_xdg_root_cannot_block_this_upgrade() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        let different = if crate::instance::NAME == "development" {
            "production"
        } else {
            "development"
        };
        assert!(
            preflight_in(&proc_root, &runtime, Some(77), |_| Ok(CompanionIdentity {
                protocol: 56,
                instance: different.into(),
                daemon_qualified: false
            }))
            .is_ok()
        );
        assert!(
            preflight_in(&proc_root, &runtime, None, |_| Ok(CompanionIdentity {
                protocol: 56,
                instance: different.into(),
                daemon_qualified: false
            }))
            .is_ok()
        );
    }

    #[test]
    fn changed_process_identity_cannot_authorize_the_editor_companion() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        let error = preflight_in(&proc_root, &runtime, Some(77), |_| {
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(proc_root.join("42/stat"))
                .unwrap()
                .write_all(b"42 (replaced) invalid")
                .unwrap();
            Ok(77.into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("ownership changed"));
    }

    #[test]
    fn opaque_editor_executable_is_not_interpreted_as_no_editor() {
        let process = tempfile::tempdir().unwrap();
        std::fs::write(process.path().join("comm"), "ceditor\n").unwrap();
        assert!(
            crate::instance::editor_executable(
                process.path(),
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            )
            .is_err()
        );
    }
}
