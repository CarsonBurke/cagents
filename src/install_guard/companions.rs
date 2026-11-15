//! Informational census of editors sharing this application's terminal service.
use super::*;
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditorCompanion {
    pub name: String,
    /// The version reported by the editor's matched cagents companion.
    pub version: Option<String>,
    pub protocol: Option<u16>,
    pub pid: u32,
}

struct CompanionIdentity {
    instance: String,
    version: Option<String>,
    protocol: Option<u16>,
}

/// Missing or changing process evidence cannot veto an update. Unknown
/// companion details remain unknown rather than implying compatibility.
pub(crate) fn editor_companions() -> Vec<EditorCompanion> {
    let Ok(runtime) = crate::instance::runtime_directory() else {
        return Vec::new();
    };
    editors_in(Path::new("/proc"), &runtime, |cli| {
        let (instance, version) =
            crate::update::companion_identity(cli).map_err(io::Error::other)?;
        let protocol = if instance == crate::instance::NAME {
            crate::update::companion_protocol(cli).ok()
        } else {
            None
        };
        Ok(CompanionIdentity {
            instance,
            version,
            protocol,
        })
    })
}

fn editors_in(
    proc_root: &Path,
    runtime: &Path,
    mut inspect: impl FnMut(&Path) -> io::Result<CompanionIdentity>,
) -> Vec<EditorCompanion> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut editors = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let process = entry.path();
        if !std::fs::metadata(&process)
            .is_ok_and(|metadata| metadata.uid() == unsafe { libc::geteuid() })
        {
            continue;
        }
        let executable = match crate::instance::editor_executable(
            &process,
            std::fs::read_link(process.join("exe")),
        ) {
            Ok(Some(executable)) => Some(executable),
            Ok(None) => continue,
            Err(_) => {
                if !std::fs::read_to_string(process.join("comm"))
                    .is_ok_and(|name| matches!(name.trim_end(), "ceditor" | "ceditor-develop"))
                {
                    continue;
                }
                None
            }
        };
        let mut editor = EditorCompanion {
            name: "Ceditor".into(),
            version: None,
            protocol: None,
            pid,
        };
        let Some(executable) = executable else {
            editors.push(editor);
            continue;
        };
        // Never probe a companion without a stable editor identity and a
        // confirmed shared runtime. Keep an observed editor with unknown
        // details if those reads fail; the census is informational only.
        let details = (|| -> io::Result<Option<CompanionIdentity>> {
            let before = std::fs::read_to_string(process.join("stat"))?;
            verify_editor_identity(&process, &before, &executable)?;
            let mut environment = Vec::new();
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(process.join("environ"))?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut environment)?;
            verify_editor_identity(&process, &before, &executable)?;
            if environment.len() > 1024 * 1024 {
                return Err(io::Error::other("editor environment exceeds census limit"));
            }
            let editor_runtime = environment
                .split(|byte| *byte == 0)
                .find_map(|entry| entry.strip_prefix(b"XDG_RUNTIME_DIR="))
                .ok_or_else(|| io::Error::other("editor runtime is unknown"))?;
            if !same_namespace(
                runtime,
                Path::new(std::ffi::OsStr::from_bytes(editor_runtime)),
            )? {
                return Ok(None);
            }
            if executable
                .file_name()
                .is_some_and(|name| name.as_bytes().ends_with(b" (deleted)"))
            {
                return Err(io::Error::other("editor executable was removed"));
            }
            let cli = crate::companion::Product::Cli.beside(&executable)?;
            let companion_before = std::fs::symlink_metadata(&cli)?;
            let identity = inspect(&cli)?;
            verify_editor_identity(&process, &before, &executable)?;
            if companion_stamp(&companion_before)
                != companion_stamp(&std::fs::symlink_metadata(&cli)?)
            {
                return Err(io::Error::other("editor companion changed during census"));
            }
            Ok((identity.instance == crate::instance::NAME).then_some(identity))
        })();
        match details {
            Ok(None) => continue,
            Ok(Some(identity)) => {
                editor.version = identity.version;
                editor.protocol = identity.protocol;
            }
            Err(_) => {}
        }
        editors.push(editor);
    }
    editors.sort_by_key(|editor| editor.pid);
    editors
}

fn verify_editor_identity(process: &Path, before: &str, executable: &Path) -> io::Result<()> {
    let after = std::fs::read_to_string(process.join("stat"))?;
    let current = std::fs::read_link(process.join("exe"))?;
    if process_start(before).is_none()
        || process_start(before) != process_start(&after)
        || current.as_path() != executable
    {
        return Err(io::Error::other("editor ownership changed during census"));
    }
    Ok(())
}

pub(crate) fn trusted_executable_owner(owner: u32) -> bool {
    owner == 0 || owner == unsafe { libc::geteuid() }
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
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn editor(proc_root: &Path, bin: &Path, runtime: &Path) {
        std::fs::create_dir_all(proc_root.join("42")).unwrap();
        std::fs::create_dir_all(bin).unwrap();
        std::fs::create_dir_all(runtime).unwrap();
        for name in ["ceditor", "cagents", "cagentsd"] {
            let path = bin.join(name);
            std::fs::write(&path, "fixture").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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

    fn identity(protocol: u16) -> CompanionIdentity {
        CompanionIdentity {
            instance: crate::instance::NAME.into(),
            version: Some("0.1.3+local.07e7efc5".into()),
            protocol: Some(protocol),
        }
    }

    #[test]
    fn older_and_matching_editors_are_informational_including_unmarked_releases() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        for protocol in [127, crate::terminal_ipc::PROTOCOL_VERSION] {
            let found = editors_in(&proc_root, &runtime, |_| Ok(identity(protocol)));
            assert_eq!(
                found,
                vec![EditorCompanion {
                    name: "Ceditor".into(),
                    version: Some("0.1.3+local.07e7efc5".into()),
                    protocol: Some(protocol),
                    pid: 42,
                }]
            );
            assert!(
                !serde_json::to_string(&found)
                    .unwrap()
                    .contains("must-not-be-printed")
            );
        }
        let alias = root.path().join("runtime-alias");
        symlink(&runtime, &alias).unwrap();
        assert_eq!(
            editors_in(&proc_root, &alias, |_| Ok(identity(127))).len(),
            1
        );
    }

    #[test]
    fn another_runtime_or_compiled_instance_is_omitted() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        let other_runtime = root.path().join("other-runtime");
        std::fs::create_dir(&other_runtime).unwrap();
        assert!(editors_in(&proc_root, &other_runtime, |_| panic!("another runtime")).is_empty());
        assert!(
            editors_in(&proc_root, &runtime, |_| {
                let mut other = identity(127);
                other.instance = "another-instance".into();
                Ok(other)
            })
            .is_empty()
        );
    }

    #[test]
    fn missing_or_changed_evidence_keeps_an_unknown_editor_without_refusing() {
        for missing in ["environ", "stat", "companion", "removed-executable"] {
            let root = tempfile::tempdir().unwrap();
            let proc_root = root.path().join("proc");
            let runtime = root.path().join("runtime");
            let bin = root.path().join("editor/bin");
            editor(&proc_root, &bin, &runtime);
            match missing {
                "companion" => std::fs::remove_file(bin.join("cagents")).unwrap(),
                "removed-executable" => {
                    std::fs::remove_file(proc_root.join("42/exe")).unwrap();
                    symlink(bin.join("ceditor (deleted)"), proc_root.join("42/exe")).unwrap();
                }
                path => std::fs::remove_file(proc_root.join("42").join(path)).unwrap(),
            }
            let found = editors_in(&proc_root, &runtime, |_| panic!("unverified editor"));
            assert_eq!(found.len(), 1, "{missing}");
            assert_eq!(found[0].protocol, None);
            assert_eq!(found[0].version, None);
        }
    }

    #[test]
    fn changed_identity_discards_probe_details() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        let found = editors_in(&proc_root, &runtime, |_| {
            std::fs::write(proc_root.join("42/stat"), b"42 (replaced) invalid").unwrap();
            Ok(identity(127))
        });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].protocol, None);
        assert_eq!(found[0].version, None);
    }

    #[test]
    fn vanished_process_is_not_reported() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        editor(&proc_root, &root.path().join("editor/bin"), &runtime);
        std::fs::remove_dir_all(proc_root.join("42")).unwrap();
        assert!(editors_in(&proc_root, &runtime, |_| panic!("no editor")).is_empty());
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
}
