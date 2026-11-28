//! Prove native presentation before changing daemon ownership, and retain the
//! window slots separately from daemon recovery.
use super::*;

pub const PROBE_ARGUMENT: &str = "--internal-first-frame-probe";
pub const PROBE_RECEIPT: &str = "first-frame-v1\n";
const PROBE_LIMIT: u64 = 512 * 1024 * 1024;

const PRIVATE_PATHS: &[(&str, &str)] = &[
    ("HOME", "home"),
    ("XDG_RUNTIME_DIR", "runtime"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_CONFIG_DIRS", "system-config"),
    ("XDG_DATA_DIRS", "system-data"),
    ("CLAUDE_CONFIG_DIR", "claude"),
    ("CODEX_HOME", "codex"),
    ("TMPDIR", "tmp"),
    ("PATH", "launchers"),
];

#[derive(Serialize, Deserialize)]
struct SavedWindows {
    version: String,
    slots: Vec<String>,
    #[serde(default)]
    restore_failed: bool,
}

pub(super) fn verify_candidate(
    state: &Path,
    job: &mut UpdateJob,
    has_windows: bool,
    cancellation: &CancellationToken,
    executable: impl FnOnce() -> Result<File, UpdateError>,
) -> Result<bool, UpdateError> {
    // Even a compatible daemon may exit before the saved windows reopen,
    // leaving the new installation responsible for the first startup.
    if !has_windows {
        return Ok(false);
    }
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Checking that the new version can open a window",
    )?;
    match probe(state, executable, cancellation, Duration::from_secs(30)) {
        Ok(()) => (),
        Err(UpdateError::Cancelled) => return Err(UpdateError::Cancelled),
        Err(error) => {
            return Err(invalid(format!(
                "cagents {} could not present its first window frame: {error}. The current version and its saved workspace are still available.",
                job.version
            )));
        }
    }
    Ok(true)
}

pub(super) fn remember_slots(
    state: &Path,
    job: &UpdateJob,
    slots: Vec<String>,
) -> Result<(), UpdateError> {
    if slots.is_empty() {
        return Ok(());
    }
    write_state_file(
        state,
        &artifact(&job.id),
        &SavedWindows {
            version: job.version.clone(),
            slots,
            restore_failed: false,
        },
    )
}

fn probe(
    state: &Path,
    executable: impl FnOnce() -> Result<File, UpdateError>,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<(), UpdateError> {
    // Keep Unix socket paths short, and put the executable in the installation's
    // filesystem so a noexec temporary mount does not disable updates. With no
    // companions or search PATH, the probe has no daemon or provider launcher.
    let temp_base = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/var/tmp"
    };
    let workspace = tempfile::Builder::new()
        .prefix("cgp-")
        .tempdir_in(temp_base)?;
    let root = workspace.path().canonicalize()?;
    let image = tempfile::Builder::new()
        .prefix("window-probe-")
        .tempdir_in(state)?;
    let ui = image.path().canonicalize()?.join("cagents-ui");
    {
        let source = executable()?;
        let before = source.metadata()?;
        let mut copy = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&ui)?;
        if io::copy(&mut (&source).take(PROBE_LIMIT + 1), &mut copy)? > PROBE_LIMIT {
            return Err(invalid(
                "The candidate UI exceeds the executable size limit",
            ));
        }
        if !same_companion_executable(&before, &source.metadata()?) {
            return Err(invalid(
                "The candidate UI changed while preparing the startup check",
            ));
        }
    }
    let mut command = Command::new(ui);
    command.env_clear().current_dir(&root).arg(PROBE_ARGUMENT);
    for (key, name) in PRIVATE_PATHS {
        let path = root.join(name);
        fs::create_dir(&path)?;
        command.env(key, path);
    }
    // Preserve the desktop and renderer selection without sharing its runtime
    // directory, session bus, activation token or inherited descriptors.
    for key in [
        "DISPLAY",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "WGPU_BACKEND",
        "WGPU_ADAPTER_NAME",
        "WGPU_POWER_PREF",
        "XCURSOR_SIZE",
        "XCURSOR_THEME",
        "XDG_SESSION_TYPE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(target_os = "linux")]
    configure_display(&mut command)?;
    // SAFETY: Only closes descriptors in the forked child.
    unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
    let mut output = bounded_command_with_stderr(
        &mut command,
        "native window startup check",
        crate::control::MAX_UI_LAUNCH_HANDSHAKE_BYTES as u64,
        timeout,
        cancellation,
        Stdio::inherit(),
    )?;
    let mut receipt = String::new();
    output.read_to_string(&mut receipt)?;
    if receipt != PROBE_RECEIPT {
        return Err(invalid(
            "the candidate exited without confirming native presentation",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_display(command: &mut Command) -> io::Result<()> {
    if let Some(display) = std::env::var_os("WAYLAND_DISPLAY").filter(|name| !name.is_empty()) {
        // libwayland accepts absolute socket names as well as names relative
        // to XDG_RUNTIME_DIR. The child has its own runtime directory.
        let display = PathBuf::from(display);
        let socket = if display.is_absolute() {
            display
        } else {
            crate::instance::runtime_directory()?.join(display)
        };
        command.env("WAYLAND_DISPLAY", socket);
    }
    if let Some(authority) = std::env::var_os("XAUTHORITY")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".Xauthority")))
    {
        command.env("XAUTHORITY", std::path::absolute(authority)?);
    }
    Ok(())
}

/// Refuse direct invocations against an installed copy and its real data.
#[cfg(feature = "ui")]
pub fn validate_probe_environment() -> io::Result<()> {
    let root = std::env::current_dir()?;
    let executable = crate::process::current_executable()?;
    let image = executable
        .parent()
        .ok_or_else(|| io::Error::other("probe image has no directory"))?;
    let private = |path: &Path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_dir()
                && metadata.uid() == crate::daemon::current_uid()
                && metadata.mode() & 0o077 == 0
        })
    };
    if !private(&root)
        || !private(image)
        || executable
            .file_name()
            .is_none_or(|name| name != "cagents-ui")
        || ["cagents", "cagentsd"]
            .iter()
            .any(|name| image.join(name).exists())
        || PRIVATE_PATHS.iter().any(|(key, name)| {
            let path = root.join(name);
            std::env::var_os(key).as_deref() != Some(path.as_os_str())
                || !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        })
    {
        return Err(io::Error::other(
            "native startup check requires a private workspace",
        ));
    }
    Ok(())
}

fn artifact(id: &str) -> String {
    format!("{id}.saved-windows.json")
}

fn load(state: &Path, id: &str) -> Result<SavedWindows, UpdateError> {
    if uuid::Uuid::parse_str(id)
        .ok()
        .is_none_or(|parsed| parsed.to_string() != id)
    {
        return Err(invalid("Invalid update job identifier"));
    }
    let saved: SavedWindows =
        serde_json::from_slice(&read_private(&state.join(artifact(id)), JOB_STATE_LIMIT)?)?;
    let mut names = std::collections::BTreeSet::new();
    for slot in &saved.slots {
        crate::instance::validate_window_slot(slot)?;
        if !names.insert(slot) {
            return Err(invalid(
                "The update window record contains a duplicate slot",
            ));
        }
    }
    if saved.slots.is_empty() {
        return Err(invalid("The update has no saved window slots"));
    }
    Ok(saved)
}

pub(super) fn reopen_failure(
    state: &Path,
    job: &UpdateJob,
    error: impl std::fmt::Display,
) -> UpdateError {
    let selected = state.parent().is_some_and(|prefix| {
        prefix
            .join("current")
            .canonicalize()
            .is_ok_and(|root| root == prefix.join("versions").join(&job.version))
    });
    if selected
        && let Ok(mut saved) = load(state, &job.id)
        && saved.version == job.version
    {
        saved.restore_failed = true;
        match write_state_file(state, &artifact(&job.id), &saved) {
            Ok(()) => {
                let start = if recovery::Census::load(&recovery::census_path(state, &job.id))
                    .is_ok_and(|census| census.verify_absence().is_ok())
                {
                    "cagents daemon start && "
                } else {
                    ""
                };
                return invalid(format!(
                    "Some saved windows did not reopen: {error}. Run `{start}cagents daemon reopen-update-windows --job {}` from a system terminal to retry those windows with the selected version.",
                    job.id
                ));
            }
            Err(record_error) => {
                eprintln!("Could not record the failed window restore: {record_error}")
            }
        }
    }
    unrecorded_reopen_failure(error)
}

pub(super) fn unrecorded_reopen_failure(error: impl std::fmt::Display) -> UpdateError {
    invalid(format!(
        "Some saved windows did not reopen: {error}. Run `cagents ui` to reopen the saved workspace."
    ))
}

pub(crate) fn reopen(id: &str) -> Result<Option<String>, UpdateError> {
    let layout = locate(&crate::process::current_executable()?)?;
    let _worker = exclude_update_jobs(&layout.prefix)?;
    let state = state_dir(&layout.prefix)?;
    let mut job = read_job(&state)?
        .filter(|job| job.id == id && job.phase == UpdatePhase::Failed)
        .ok_or_else(|| invalid("That update is not the current failed update"))?;
    let saved = load(&state, id)?;
    if !saved.restore_failed {
        return Err(invalid(
            "This update has no recorded window restore failure",
        ));
    }
    let census = recovery::Census::load(&recovery::census_path(&state, id))?;
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    let selected = served_version_root(&layout)?;
    if saved.version != job.version
        || selected.file_name().and_then(|name| name.to_str()) != Some(saved.version.as_str())
    {
        return Err(invalid("The update's version is no longer selected"));
    }
    if census.verify_absence().is_ok() {
        return Err(invalid(
            "Start the selected terminal service with `cagents daemon start`, then retry the window command",
        ));
    }
    let daemon = selected.join("bin/cagentsd");
    let timeout = crate::install_guard::daemon_start_timeout()?;
    let confirm_owner = || installed::confirm_daemon_ready(&daemon, None, timeout);
    confirm_owner()?;
    guard.set_recovery_restore_only();
    let ui = selected.join("bin/cagents-ui");
    let launched = guard.relaunch_missing_windows_with_notes(&saved.slots, &ui);
    let guard = crate::instance::UiUpdateGuard::acquire_for_cutover()?;
    let verified = guard.verify_cutover_windows(&saved.slots, &ui);
    let notes = match (launched, verified) {
        (Ok(notes), Ok(())) => notes,
        (Err(_), Ok(())) => None,
        (Err(error), Err(_)) | (Ok(_), Err(error)) => return Err(error.into()),
    };
    confirm_owner()?;
    let receipt = census.verify_with_target_retiring(&selected, &state, id)?;
    if !receipt.usable() {
        return Err(receipt.failure());
    }
    receipt.save(&recovery::outcome_path(&state, id))?;
    job.phase = UpdatePhase::Complete;
    job.message = match &notes {
        Some(notes) => format!("Update installed; saved windows reopened after retry\n\n{notes}"),
        None => "Update installed; saved windows reopened after retry".into(),
    };
    write_job(&state, &job)?;
    retention::remember_success(&state, id, &job.phase);
    Ok(notes)
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    fn candidate(script: &str) -> File {
        let mut file = tempfile::tempfile().unwrap();
        writeln!(file, "#!/bin/sh\n{script}").unwrap();
        file.rewind().unwrap();
        file
    }

    fn job() -> UpdateJob {
        UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "0.2.5".into(),
            log_path: PathBuf::new(),
            phase: UpdatePhase::AwaitingConfirmation,
            message: String::new(),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        }
    }

    #[test]
    fn first_frame_proof_requires_a_successful_exit_and_the_exact_receipt() {
        let state = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(3);
        let cancellation = CancellationToken::new();
        for (script, success) in [
            ("exit 1", false),
            ("exit 0", false),
            ("printf 'ready\\n'", false),
            ("printf 'first-frame-v1\\n'; exit 1", false),
            ("printf 'first-frame-v1\\n'", true),
        ] {
            assert_eq!(
                probe(
                    state.path(),
                    || Ok(candidate(script)),
                    &cancellation,
                    timeout
                )
                .is_ok(),
                success,
                "{script}"
            );
        }
    }

    #[test]
    fn the_probe_has_private_data_and_no_launcher_or_provider_on_its_path() {
        let script = r#"
set -eu
test "$1" = --internal-first-frame-probe
test "${0##*/}" = cagents-ui
for dir in "$HOME" "$XDG_RUNTIME_DIR" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$CLAUDE_CONFIG_DIR" "$CODEX_HOME" "$PATH"; do
    test -d "$dir"
    test "${dir%/*}" = "$PWD"
done
test ! -e "${0%/*}/cagentsd"
! command -v claude
! command -v codex
printf 'first-frame-v1\n'
"#;
        let state = tempfile::tempdir().unwrap();
        probe(
            state.path(),
            || Ok(candidate(script)),
            &CancellationToken::new(),
            Duration::from_secs(3),
        )
        .unwrap();
    }

    #[test]
    fn timeout_is_a_failed_startup_check_even_if_the_child_printed_a_receipt() {
        let state = tempfile::tempdir().unwrap();
        let result = probe(
            state.path(),
            || Ok(candidate("printf 'first-frame-v1\\n'; exec /bin/sleep 5")),
            &CancellationToken::new(),
            Duration::from_millis(100),
        );
        assert!(matches!(result, Err(UpdateError::Timeout(_))));
    }

    #[test]
    fn headless_updates_skip_the_check_and_failures_keep_windows_uncommitted() {
        let state = tempfile::tempdir().unwrap();
        let mut job = job();
        let cancellation = CancellationToken::new();
        let slots = vec!["main".into(), "second".into()];
        assert!(
            !verify_candidate(state.path(), &mut job, false, &cancellation, || {
                panic!("a headless update must not launch the candidate UI");
            })
            .unwrap()
        );
        assert_eq!(job.phase, UpdatePhase::AwaitingConfirmation);
        let failure = verify_candidate(state.path(), &mut job, true, &cancellation, || {
            Ok(candidate("exit 1"))
        })
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("could not present its first window frame")
        );
        assert!(!state.path().join(artifact(&job.id)).exists());

        verify_candidate(state.path(), &mut job, true, &cancellation, || {
            Ok(candidate("printf 'first-frame-v1\\n'"))
        })
        .unwrap();
        remember_slots(state.path(), &job, slots.clone()).unwrap();
        let saved = load(state.path(), &job.id).unwrap();
        assert_eq!(saved.version, job.version);
        assert!(!saved.restore_failed);
        assert_eq!(saved.slots, slots);
    }

    #[test]
    fn workspace_failure_names_the_job_only_for_the_selected_version() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().canonicalize().unwrap();
        let state = prefix.join(".update");
        fs::create_dir(&state).unwrap();
        let job = job();
        remember_slots(&state, &job, vec!["main".into()]).unwrap();
        let error = || reopen_failure(&state, &job, "workspace initialization failed").to_string();
        let target = Path::new("versions").join(&job.version);
        fs::create_dir_all(prefix.join(&target)).unwrap();
        assert!(!error().contains("reopen-update-windows"));
        std::os::unix::fs::symlink(&target, prefix.join("current")).unwrap();
        assert!(error().contains(&format!("daemon reopen-update-windows --job {}", job.id)));
        assert!(load(&state, &job.id).unwrap().restore_failed);

        let mut job = job;
        assert!(
            finish_reopening_windows(
                &state,
                &mut job,
                Ok(()),
                Err(invalid("daemon inspection was busy")),
                Err(invalid("workspace initialization failed")),
            )
            .is_err()
        );
        assert_eq!(job.phase, UpdatePhase::Failed);
        assert!(job.message.contains("workspace initialization failed"));
        assert!(job.message.contains("daemon inspection was busy"));
        assert!(job.message.contains("daemon reopen-update-windows"));
    }
}
