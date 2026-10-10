//! A locally installed matched set uses the same review and checkpoint flow.
use super::*;

const BINARIES: [&str; 3] = ["cagents", "cagents-ui", "cagentsd"];

#[derive(PartialEq, Eq)]
struct Stamp(Vec<(u64, u64, u64, i64, i64, i64, i64)>);

fn stamp(root: &Path) -> Result<Stamp, UpdateError> {
    let mut entries = Vec::new();
    for binary in BINARIES {
        let metadata = fs::symlink_metadata(root.join("bin").join(binary))?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(invalid(
                "installed candidate binaries must be owned regular files without shared write permission",
            ));
        }
        entries.push((
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ));
    }
    Ok(Stamp(entries))
}

fn digest(root: &Path) -> Result<String, UpdateError> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    for binary in BINARIES {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("bin").join(binary))?;
        let size = file.metadata()?.len();
        if size > ARCHIVE_LIMIT {
            return Err(invalid("installed binary exceeds update size limit"));
        }
        hash.update(binary.as_bytes());
        hash.update(size.to_le_bytes());
        let mut consumed = 0u64;
        let mut limited = Read::by_ref(&mut file).take(size + 1);
        loop {
            let count = limited.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            consumed += count as u64;
            hash.update(&buffer[..count]);
        }
        if consumed != size {
            return Err(invalid("installed binary changed during verification"));
        }
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) fn perform(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    root: &Path,
) -> Result<(), UpdateError> {
    let layout = locate(&std::env::current_exe()?)?;
    if served_version_root(&layout)? != root || root == layout.version_root {
        return Err(invalid(
            "the installed candidate changed; inspect the installation again",
        ));
    }
    set_phase(
        state,
        job,
        UpdatePhase::Verifying,
        "Verifying the installed application set",
    )?;
    let before = stamp(root)?;
    let identity = digest(root)?;
    let mut executable = anonymous_file()?;
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join("bin/cagents"))?;
    io::copy(
        &mut Read::by_ref(&mut source).take(512 * 1024 * 1024 + 1),
        &mut executable,
    )?;
    if executable.metadata()?.len() > 512 * 1024 * 1024 {
        return Err(invalid("installed CLI exceeds update size limit"));
    }
    seal_file(&executable)?;
    let cancellation = CancellationToken::new();
    let staged =
        candidate::Staged::from_file(executable, job.version.clone(), identity, &cancellation)?;
    let observe = |policy| {
        if served_version_root(&layout)? != root || stamp(root)? != before {
            return Err(invalid(
                "the installed application changed; review the new build before updating",
            ));
        }
        staged.preview(policy)
    };
    if staged.identity.protocol == crate::terminal_ipc::PROTOCOL_VERSION {
        staged.verify_compatible_handshake(&cancellation)?;
    }
    let confirmed = confirmation::wait(state, job, observe)?;
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Saving and closing application windows",
    )?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    let windows = guard.prepare_and_close_windows(Duration::from_secs(45))?;
    let result = (|| {
        let current = observe(
            confirmed
                .daemon
                .as_ref()
                .map_or(impact::Policy::WhenIdle, |daemon| daemon.policy),
        )?;
        if !confirmed.preserves_consent(&current) || digest(root)? != staged.identity.digest {
            return Err(invalid(
                "the installed build or session ownership changed; review the update again",
            ));
        }
        if let Some(daemon) = current.daemon {
            let socket = crate::terminal_ipc::daemon_socket_path()?;
            let mut client = crate::control::ControlClient::connect(&socket)
                .map_err(|error| invalid(error.to_string()))?;
            client
                .prepare_confirmed_upgrade(daemon, false)
                .map_err(|error| invalid(error.to_string()))?;
            wait_daemon_stopped(&socket)?;
        }
        set_phase(
            state,
            job,
            UpdatePhase::Installing,
            "Starting the installed application",
        )?;
        let mut command = Command::new(root.join("bin/cagents"));
        command.args(["--json", "daemon", "start"]);
        guard.inherit_for_install(&mut command)?;
        let started = bounded_command(
            &mut command,
            "installed daemon startup",
            SMALL_ASSET_LIMIT,
            Duration::from_secs(30),
            &cancellation,
        );
        if let Err(error) = started {
            if !staged.daemon_running(&cancellation).unwrap_or(false) {
                return Err(error);
            }
        }
        Ok(())
    })();
    // Only the confirmed installation may be relaunched. If another installer
    // moved current during failure recovery, retain the old matched executable.
    let reopen = if result.is_ok() {
        root.to_owned()
    } else {
        installation
            .prefix
            .join("versions")
            .join(&installation.version)
    };
    let reopened = guard
        .relaunch_windows(&windows, &reopen.join("bin/cagents-ui"))
        .map_err(UpdateError::from);
    result?;
    reopened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_one_binary_invalidates_the_confirmed_matched_set() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("0.1.3+local.same-name");
        fs::create_dir_all(root.join("bin")).unwrap();
        for binary in BINARIES {
            fs::write(root.join("bin").join(binary), b"first build").unwrap();
        }
        let original_stamp = stamp(&root).unwrap();
        let original_identity = digest(&root).unwrap();
        let replacement = root.join("bin/replacement");
        fs::write(&replacement, b"other build").unwrap();
        fs::rename(replacement, root.join("bin/cagentsd")).unwrap();
        assert!(stamp(&root).unwrap() != original_stamp);
        assert_ne!(digest(&root).unwrap(), original_identity);
        // A different target behind an unchanged version name cannot borrow
        // the previous confirmation through a symlink either.
        fs::remove_file(root.join("bin/cagentsd")).unwrap();
        std::os::unix::fs::symlink("cagents", root.join("bin/cagentsd")).unwrap();
        assert!(stamp(&root).is_err());
    }
}
