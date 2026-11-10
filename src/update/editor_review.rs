//! Read-only editor companion versions for the review. Compatibility and lease
//! handoff remain with the update coordinator; this never admits or stops one.
use super::*;
use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;

pub(super) fn capture() -> source_inspection::ReviewDetails {
    let mut details = source_inspection::ReviewDetails::default();
    let Ok(runtime) = crate::instance::runtime_directory() else {
        return details;
    };
    match versions_in(Path::new("/proc"), &runtime, |cli| {
        let isolated = std::env::temp_dir().join(format!(
            "cagents-editor-review-{}", uuid::Uuid::new_v4()
        ));
        let response = companion_read(
            cli, &["--json", "daemon", "status"], Some(&isolated),
            CEDITOR_COMPANION, "editor companion version check",
        ).map_err(io::Error::other)?;
        let data = response.get("data").ok_or_else(|| io::Error::other("Missing companion status"))?;
        let instance = data.get("instance").and_then(serde_json::Value::as_str)
            .ok_or_else(|| io::Error::other("Missing companion instance"))?;
        let version = data.get("cli_version").and_then(serde_json::Value::as_str)
            .filter(|version| Version::parse(version).is_ok())
            .ok_or_else(|| io::Error::other("Missing companion version"))?;
        Ok((instance.to_owned(), version.to_owned(), companion_protocol(cli).map_err(io::Error::other)?))
    }) {
        Ok(versions) => details.editor_companions = versions,
        Err(_) => details.notices.push("Ceditor's cagents version could not be checked; its agent features may need Ceditor to update.".into()),
    }
    details
}

fn versions_in(
    processes: &Path,
    runtime: &Path,
    mut inspect: impl FnMut(&Path) -> io::Result<(String, String, u16)>,
) -> io::Result<Vec<String>> {
    let mut versions = BTreeSet::new();
    for entry in fs::read_dir(processes)? {
        let process = entry?.path();
        if process
            .file_name()
            .is_none_or(|name| name.to_string_lossy().parse::<u32>().is_err())
            || !fs::metadata(&process)
                .is_ok_and(|metadata| metadata.uid() == unsafe { libc::geteuid() })
        {
            continue;
        }
        let Some(executable) =
            crate::instance::editor_executable(&process, fs::read_link(process.join("exe")))?
        else {
            continue;
        };
        let before = fs::read_to_string(process.join("stat"))?;
        let mut environment = Vec::new();
        File::open(process.join("environ"))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut environment)?;
        if environment.len() > 1024 * 1024 {
            return Err(io::Error::other(
                "Editor environment exceeds its inspection limit",
            ));
        }
        let Some(editor_runtime) = environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"XDG_RUNTIME_DIR="))
        else {
            continue;
        };
        let editor_runtime = fs::metadata(Path::new(std::ffi::OsStr::from_bytes(editor_runtime)))?;
        let expected_runtime = fs::metadata(runtime)?;
        if (editor_runtime.dev(), editor_runtime.ino())
            != (expected_runtime.dev(), expected_runtime.ino())
        {
            continue;
        }
        let cli = crate::companion::Product::Cli.beside(&executable)?;
        let (instance, version, protocol) = inspect(&cli)?;
        // A reused pid or replaced executable cannot supply this editor's version.
        if start(&before).is_none()
            || start(&fs::read_to_string(process.join("stat"))?) != start(&before)
            || fs::read_link(process.join("exe"))? != executable
        {
            return Err(io::Error::other("Editor changed during inspection"));
        }
        if instance == crate::instance::NAME && protocol < crate::terminal_ipc::PROTOCOL_VERSION {
            versions.insert(version);
        }
    }
    Ok(versions.into_iter().collect())
}

fn start(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn reports_the_companion_version_only_for_older_matching_instances() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let processes = root.path().join("proc");
        let process = processes.join("42");
        let bin = root.path().join("editor/bin");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&process).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(bin.join("ceditor"), "fixture").unwrap();
        symlink(bin.join("ceditor"), process.join("exe")).unwrap();
        fs::write(
            process.join("environ"),
            format!(
                "PRIVATE_TOKEN=secret\0XDG_RUNTIME_DIR={}\0",
                runtime.display()
            ),
        )
        .unwrap();
        let mut fields = vec!["0"; 20];
        fields[19] = "123";
        fs::write(
            process.join("stat"),
            format!("42 (ceditor) {}", fields.join(" ")),
        )
        .unwrap();
        let companion_version = "0.1.3+local.07e7efc5";
        let run = |instance: &str, protocol| {
            versions_in(&processes, &runtime, |cli| {
                assert_eq!(cli, bin.join("cagents"));
                Ok((instance.into(), companion_version.into(), protocol))
            })
            .unwrap()
        };
        assert_eq!(run(crate::instance::NAME, 127), [companion_version]);
        assert!(run("another-instance", 127).is_empty());
        assert!(run(crate::instance::NAME, crate::terminal_ipc::PROTOCOL_VERSION).is_empty());
        let error = versions_in(&processes, &runtime, |_| {
            fields[19] = "456";
            fs::write(
                process.join("stat"),
                format!("42 (ceditor) {}", fields.join(" ")),
            )?;
            Ok((crate::instance::NAME.into(), companion_version.into(), 127))
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed"));
        assert!(!error.to_string().contains("secret"));
    }
}
