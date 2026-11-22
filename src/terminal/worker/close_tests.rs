use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tempfile::tempdir;

use super::*;
use crate::model::{TerminalContinuity, TerminalId};
use crate::terminal::ipc::TerminalCloseResult;

struct CloseSafetyShell {
    terminal: TerminalHandle,
    wakes: Receiver<()>,
    home: tempfile::TempDir,
}

impl CloseSafetyShell {
    fn spawn(program: &Path, arguments: &[&str]) -> Self {
        Self::spawn_with_proof(program, arguments, None)
    }

    fn spawn_with_proof(
        program: &Path,
        arguments: &[&str],
        proof: Option<&crate::model::HostSessionProof>,
    ) -> Self {
        Self::spawn_with_reservation(program, arguments, proof, None, None)
    }

    fn spawn_with_reservation(
        program: &Path,
        arguments: &[&str],
        proof: Option<&crate::model::HostSessionProof>,
        reservation: Option<crate::daemon::child_reaper::Reservation>,
        runtime: Option<&Path>,
    ) -> Self {
        let home = tempdir().unwrap();
        let config_home = home.path().join(".config");
        let cache_home = home.path().join(".cache");
        let data_home = home.path().join(".local/share");
        fs::create_dir_all(config_home.join("fish")).unwrap();
        // A fresh HOME looks like fish's first run, which starts a detached
        // man-page completion generator in the shell's session. That job is
        // real work a checked close must protect, and on a loaded host it
        // outlives any test deadline. Model an established user instead.
        fs::create_dir_all(cache_home.join("fish/generated_completions")).unwrap();
        fs::create_dir_all(&data_home).unwrap();
        fs::write(home.path().join(".inputrc"), "").unwrap();
        fs::write(home.path().join(".bashrc"), "PS1='CLOSE_READY> '\n").unwrap();
        fs::write(
            config_home.join("fish/config.fish"),
            "function fish_prompt\nprintf 'CLOSE_READY> '\nend\n",
        )
        .unwrap();
        let mut launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: program.to_string_lossy().into_owned(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            working_directory: home.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), home.path().to_string_lossy().into_owned()),
                (
                    "XDG_CONFIG_HOME".into(),
                    config_home.to_string_lossy().into_owned(),
                ),
                (
                    "XDG_CACHE_HOME".into(),
                    cache_home.to_string_lossy().into_owned(),
                ),
                (
                    "XDG_DATA_HOME".into(),
                    data_home.to_string_lossy().into_owned(),
                ),
                (
                    "INPUTRC".into(),
                    home.path().join(".inputrc").to_string_lossy().into_owned(),
                ),
            ]),
        };
        if let Some(runtime) = runtime {
            launch.environment.insert(
                "XDG_RUNTIME_DIR".into(),
                runtime.to_string_lossy().into_owned(),
            );
        }
        let (wake, wakes) = crossbeam_channel::unbounded();
        let mut session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let mut config = TerminalConfig::default();
        if let Some(proof) = proof {
            session.continuity = TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            };
            config.holder_recovery = Some(crate::terminal::holder::Recovery {
                source: session.clone(),
                previous_sources: Vec::new(),
                relaunches: Vec::new(),
                options: crate::terminal::TerminalSpawnOptions::default(),
                execution_environment: None,
                execution_recipe: None,
            });
        }
        let terminal = TerminalHandle::spawn_with_execution_installed(
            session,
            config,
            None,
            move |_| {
                let _ = wake.send(());
            },
            move |handle| {
                *handle
                    .inner
                    .published
                    .test_holder_reservation
                    .lock()
                    .unwrap() = reservation;
                Ok(())
            },
        )
        .unwrap();
        Self {
            terminal,
            wakes,
            home,
        }
    }

    fn shell(program: &Path) -> Self {
        let shell = Self::spawn(program, &[]);
        shell.wait_for_text("CLOSE_READY>");
        shell
    }

    fn wait_for(&self, description: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if ready() {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "Timed out waiting for {description}; screen:\n{}",
                self.screen()
            );
            // Output wakes accelerate observation. The timeout also polls
            // process state, which need not produce terminal output.
            let _ = self
                .wakes
                .recv_timeout(remaining.min(Duration::from_millis(10)));
        }
    }

    fn screen(&self) -> String {
        let snapshot = self.terminal.snapshot();
        let text = snapshot
            .lines
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.terminal.acknowledge_redraw(snapshot.generation);
        text
    }

    fn wait_for_text(&self, text: &str) {
        self.wait_for(text, || self.screen().contains(text));
    }

    fn assert_unconfirmed_close(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let outcome = self.terminal.close_checked(false);
            if outcome == TerminalCloseResult::Closed {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for unconfirmed shell close: {outcome:?}; screen:\n{}",
                self.screen()
            );
            let _ = self.wakes.recv_timeout(Duration::from_millis(10));
        }
    }

    fn assert_child_requires_confirmation(&self, command: &[u8], marker: &str) {
        self.terminal.input(command).unwrap();
        self.wait_for_text(marker);
        let pid: u32 = fs::read_to_string(self.home.path().join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // The marker precedes exec. Wait for the actual long-running child,
        // rather than mistaking shell setup or echoed input for live work.
        self.wait_for("child running sleep", || {
            fs::read_link(format!("/proc/{pid}/exe"))
                .is_ok_and(|path| path.file_name() == Some(OsStr::new("sleep")))
        });
        assert_eq!(
            self.terminal.close_checked(false),
            TerminalCloseResult::Active
        );
        assert_eq!(
            self.terminal.close_checked(true),
            TerminalCloseResult::Closed
        );
        self.wait_for(
            "confirmed close terminating the child",
            || match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => stat.rsplit_once(") ").is_some_and(|(_, fields)| {
                    matches!(fields.as_bytes().first(), Some(b'Z' | b'X'))
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => panic!("Cannot observe child termination: {error}"),
            },
        );
    }
}

impl Drop for CloseSafetyShell {
    fn drop(&mut self) {
        // Join teardown before the isolated HOME disappears, even on panic.
        let _ = self.terminal.close_checked(true);
        let _ = self.terminal.shutdown();
    }
}

fn installed_fish() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    [PathBuf::from("/bin"), PathBuf::from("/usr/bin")]
        .into_iter()
        .chain(std::env::split_paths(&path))
        .map(|directory| directory.join("fish"))
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

#[test]
fn an_idle_shell_closes_without_confirmation_from_its_daemon_owner() {
    CloseSafetyShell::shell(Path::new("/bin/bash")).assert_unconfirmed_close();
}

#[test]
fn an_idle_fish_closes_without_confirmation_from_its_daemon_owner() {
    let Some(fish) = installed_fish() else { return };
    CloseSafetyShell::shell(&fish).assert_unconfirmed_close();
}

#[test]
fn replacing_the_fish_package_does_not_make_its_idle_shell_active() {
    let Some(fish) = installed_fish() else { return };
    let package = tempdir().unwrap();
    let installed = package.path().join("fish");
    fs::copy(&fish, &installed).unwrap();
    let shell = CloseSafetyShell::shell(&installed);
    // Model a package manager replacing an executable while the old, now
    // unlinked image is still mapped by an interactive shell.
    let replacement = package.path().join("fish.new");
    fs::copy(&fish, &replacement).unwrap();
    fs::rename(replacement, &installed).unwrap();
    // A shell that is still finishing its own startup work is legitimately
    // active, so this waits for it to settle exactly as the test for an
    // untouched fish does. Asserting on the first inspection made the result
    // depend on how much a given fish release does after its first prompt.
    shell.assert_unconfirmed_close();
}

#[test]
fn an_exec_replacement_cannot_inherit_the_shells_idle_close_authority() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    shell.assert_child_requires_confirmation(
        b"printf '%s' \"$$\" > child.pid; printf 'READY_%s\\n' EXEC; exec sleep 120\n",
        "READY_EXEC",
    );
}

#[test]
fn checked_close_delivers_its_kill_provenance_before_reporting_completion() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    assert_eq!(
        shell.terminal.close_checked(false),
        TerminalCloseResult::Closed
    );
    // close_checked joins teardown; its Exited event is already observable
    // even though an application's async close-completion message is not.
    assert!(shell.terminal.events().try_iter().any(|event| matches!(
        event,
        TerminalEvent::Exited {
            intentional: true,
            signal: Some(_),
            ..
        }
    )));
}

#[test]
fn a_shell_builtin_read_closes_without_confirmation_under_process_only_detection() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    shell
        .terminal
        .input(b"printf 'READY_%s\\n' BUILTIN; read reply\n")
        .unwrap();
    shell.wait_for_text("READY_BUILTIN");
    // This deliberately does not attempt to classify in-shell execution.
    shell.assert_unconfirmed_close();
}

#[test]
fn a_foreground_child_requires_confirmation_and_confirmed_close_terminates_it() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    shell.assert_child_requires_confirmation(
        b"sh -c 'printf \"%s\" \"$$\" > child.pid; printf \"READY_%s\\n\" FOREGROUND; exec sleep 120'\n",
        "READY_FOREGROUND",
    );
}

#[test]
fn a_background_child_requires_confirmation_and_confirmed_close_terminates_it() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    shell.assert_child_requires_confirmation(
        b"sleep 120 & printf '%s' \"$!\" > child.pid; printf 'READY_%s\\n' BACKGROUND\n",
        "READY_BACKGROUND",
    );
}

#[test]
fn ordinary_shutdown_resumes_an_already_stopped_jobs_hangup_handler() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    let child = shutdown_job(
        &shell,
        b"trap ':' HUP; bash -c 'trap \"printf hup > stopped-hup; exit 0\" HUP; echo $$ > child.pid; while :; do sleep 0.05; done' &\n",
        "bash",
    );
    shell
        .terminal
        .input(b"kill -STOP $(cat child.pid)\n")
        .unwrap();
    let pid = fs::read_to_string(shell.home.path().join("child.pid")).unwrap();
    shell.wait_for("the already-stopped background job", || {
        fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('T'))
        })
    });
    let outcome = shell.terminal.shutdown();
    assert!(
        shell.home.path().join("stopped-hup").exists(),
        "an already-stopped job must run its HUP handler before shutdown completes: {outcome:?}"
    );
    assert!(outcome.is_ok(), "shutdown: {outcome:?}");
    assert_eq!(
        fs::read(shell.home.path().join("stopped-hup")).unwrap(),
        b"hup"
    );
    assert!(holder::exits_within(&child.0, Duration::ZERO).unwrap());
}

#[test]
fn ordinary_shutdown_reserves_hangup_grace_when_capture_exhausts_its_deadline() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    shell.terminal.input(b"trap 'sleep 0.1; printf hup > delayed-capture-hup' HUP; printf '%s' \"$$\" > root.pid; printf 'READY_%s\\n' DELAY; read reply\n").unwrap();
    shell.wait_for_text("READY_DELAY");
    let root = pin_shutdown_process(
        fs::read_to_string(shell.home.path().join("root.pid"))
            .unwrap()
            .parse()
            .unwrap(),
    );
    let (finished, capture_finished) = crossbeam_channel::bounded(1);
    *shell
        .terminal
        .inner
        .published
        .shutdown_census_delay
        .lock()
        .unwrap() = Some(finished);
    let terminal = shell.terminal.clone();
    let started = Instant::now();
    let shutdown = std::thread::spawn(move || terminal.shutdown());
    let captured_at = capture_finished.recv_timeout(WORKER_JOIN_TIMEOUT).unwrap();
    let killed_before_grace = holder::exits_within(
        &root.0,
        CHILD_HANGUP_GRACE.saturating_sub(captured_at.elapsed()),
    )
    .unwrap();
    let outcome = shutdown.join().unwrap();
    assert!(
        shell.home.path().join("delayed-capture-hup").exists(),
        "capture must leave time for the shell's HUP handler: {outcome:?}"
    );
    assert!(
        !killed_before_grace,
        "capture must not consume the shell's existing HUP grace"
    );
    assert!(
        outcome.is_ok(),
        "shutdown: {outcome:?} after {:?}, including the capture delay this test injects; {}",
        started.elapsed(),
        shutdown_evidence(&shell)
    );
    assert!(holder::exits_within(&root.0, Duration::ZERO).unwrap());
    assert_eq!(
        fs::read(shell.home.path().join("delayed-capture-hup")).unwrap(),
        b"hup"
    );
}

#[test]
fn ordinary_shutdown_allows_a_background_jobs_hangup_handler_to_finish() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    let child = shutdown_job(
        &shell,
        b"trap 'exit 3' HUP; bash -c 'trap \"printf hup > graceful-hup; exit 0\" HUP; echo $$ > child.pid; printf READY_GRACEFUL; while :; do sleep 0.05; done' &\n",
        "bash",
    );
    let started = Instant::now();
    let outcome = shell.terminal.shutdown();
    // A family that exits on hangup closes the PTY at once; shutdown must not
    // wait out the drain budget kept for jobs that linger.
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "a family that exits on hangup must not wait for the drain deadline: {elapsed:?}"
    );
    assert!(
        outcome.is_ok(),
        "shutdown: {outcome:?}; HUP marker: {:?}; events: {:?}",
        fs::read(shell.home.path().join("graceful-hup")),
        shell.terminal.events().try_iter().collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read(shell.home.path().join("graceful-hup")).unwrap(),
        b"hup",
        "ordinary shutdown must let the job run its HUP handler before escalation"
    );
    assert!(holder::exits_within(&child.0, Duration::ZERO).unwrap());
}

#[test]
fn ordinary_shutdown_stops_a_hup_ignoring_background_job_after_its_shell_exits() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    let child = shutdown_background_job(&shell);
    let started = Instant::now();
    assert!(shell.terminal.shutdown().is_ok());
    assert!(
        started.elapsed() >= CHILD_HANGUP_GRACE,
        "a HUP-ignoring job must receive the existing grace before escalation"
    );
    assert!(
        holder::exits_within(&child.0, Duration::from_secs(1)).unwrap(),
        "ordinary shutdown reported success but left its HUP-ignoring background job running"
    );
}

#[test]
fn ordinary_shutdown_leaves_a_process_that_left_the_session_holding_the_pty() {
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    // setsid -f forks into a new session and is reparented, keeping the PTY
    // as its stdio. It is not a job of this terminal: cleanup must neither
    // signal it nor wait on it to confirm the terminal's own family.
    let outside = shutdown_job(
        &shell,
        b"setsid -f sh -c 'trap \"\" HUP; echo $$ > child.pid; exec sleep 60'\n",
        "sleep",
    );
    let started = Instant::now();
    let outcome = shell.terminal.shutdown();
    let elapsed = started.elapsed();
    assert!(outcome.is_ok(), "shutdown: {outcome:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "a PTY held outside the family must not delay shutdown: {elapsed:?}"
    );
    assert!(
        !holder::exits_within(&outside.0, Duration::ZERO).unwrap(),
        "ordinary shutdown must not stop a process outside the terminal's family"
    );
}

#[test]
fn ordinary_shutdown_retains_the_cleanup_obligation_after_observing_shell_exit() {
    let proof = crate::model::HostSessionProof {
        host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
    };
    let runtime = tempfile::Builder::new().prefix("tr-").tempdir().unwrap();
    let directory = runtime.path().join(crate::instance::runtime_subdirectory());
    let paths = crate::daemon::RuntimePaths {
        socket: directory.join(crate::terminal::ipc::SOCKET_FILE_NAME),
        lock: directory.join("cagentsd.lock"),
        directory,
    };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let reaper = crate::daemon::child_reaper::TestService::new(2);
    let shell = CloseSafetyShell::spawn_with_reservation(
        Path::new("/bin/bash"),
        &[],
        Some(&proof),
        Some(reaper.reserve().unwrap()),
        Some(runtime.path()),
    );
    shell.wait_for_text("CLOSE_READY>");
    let owner = shell.terminal.holder_identity().unwrap().clone();
    assert!(owner.holder.is_current());
    let registered_holder = pin_shutdown_process(owner.holder.pid);
    let child = shutdown_background_job(&shell);
    shell.terminal.input(b"exit 3\n").unwrap();
    shell.wait_for("the worker's actual shell exit report", || {
        shell.terminal.events().try_iter().any(|event| {
            matches!(
                event,
                TerminalEvent::Exited {
                    exit_code: 3,
                    signal: None,
                    ..
                }
            )
        })
    });
    assert!(!holder::exits_within(&child.0, Duration::ZERO).unwrap());
    let unpublished = reaper.reserve().unwrap();
    let drain = reaper.begin_exit_drain();
    let started = Instant::now();
    assert!(shell.terminal.shutdown().is_err());
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT);
    assert!(!shell.terminal.is_completed());
    assert!(shell.terminal.worker_joined_for_test());
    let events = shell.terminal.events().try_iter().collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| matches!(
            event,
            TerminalEvent::Error {
                operation: "wait for child",
                message,
            } if message == "Terminal process cleanup remains unconfirmed"
        )),
        "shutdown took {:?}; events: {events:?}; one process census now: {}",
        started.elapsed(),
        census_cost()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Completed { .. }))
    );
    assert!(
        !drain.try_seal_absence(),
        "an unrelated unpublished exact-child ticket must still block ordinary shutdown"
    );
    drop(unpublished);
    assert!(
        drain.try_seal_absence(),
        "unconfirmed registered cleanup must release ordinary shutdown's wait: {}",
        drain.pending_description()
    );
    // Finish only this harness's exact job and holder after the shutdown
    // assertions; a live recoverable holder must not escape the test.
    drop(child);
    drop(registered_holder);
}

#[test]
fn ordinary_shutdown_force_kills_its_pinned_root_when_the_census_fails() {
    use std::os::unix::process::ExitStatusExt;
    let shell = CloseSafetyShell::shell(Path::new("/bin/bash"));
    shell
        .terminal
        .input(b"trap '' HUP; trap 'printf exited > unexpected-root-exit' EXIT; printf '%s' \"$$\" > root.pid; printf 'READY_%s\\n' CENSUS; read reply\n")
        .unwrap();
    shell.wait_for_text("READY_CENSUS");
    let pid = fs::read_to_string(shell.home.path().join("root.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let root = pin_shutdown_process(pid);
    shell
        .terminal
        .inner
        .published
        .shutdown_census_failure
        .store(true, Ordering::Release);
    let started = Instant::now();
    assert!(shell.terminal.shutdown().is_ok());
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT);
    assert!(holder::exits_within(&root.0, Duration::ZERO).unwrap());
    assert!(!shell.home.path().join("unexpected-root-exit").exists());
    assert!(shell.terminal.is_completed());
    let expected_signal = ExitStatus::from(std::process::ExitStatus::from_raw(libc::SIGKILL))
        .signal()
        .unwrap()
        .to_owned();
    let events = shell.terminal.events().try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        event,
        TerminalEvent::Exited { signal: Some(signal), .. } if signal == &expected_signal
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Error { .. }))
    );
}

struct ShutdownJob(File);

impl Drop for ShutdownJob {
    fn drop(&mut self) {
        let _ = close_processes::signal(&self.0, libc::SIGKILL);
        let _ = holder::exits_within(&self.0, Duration::from_secs(1));
    }
}

fn shutdown_background_job(shell: &CloseSafetyShell) -> ShutdownJob {
    shutdown_job(
        shell,
        b"trap 'exit 3' HUP; sh -c 'trap \"\" HUP; echo $$ > child.pid; exec sleep 60' &\n",
        "sleep",
    )
}

/// How one census of this host's processes goes now, and how long it takes.
/// Shutdown confirms cleanup through such censuses within fixed reserves, so
/// a host with many processes, or a loaded one, can run out of them.
fn census_cost() -> String {
    let started = Instant::now();
    let result = close_processes::session_is_empty(0, started + Duration::from_secs(30));
    format!("{result:?} in {:?}", started.elapsed())
}

/// What a failed shutdown assertion needs to tell its causes apart: the
/// worker's reported events and the host's census cost.
fn shutdown_evidence(shell: &CloseSafetyShell) -> String {
    format!(
        "events: {:?}; one process census now: {}",
        shell.terminal.events().try_iter().collect::<Vec<_>>(),
        census_cost()
    )
}

fn pin_shutdown_process(pid: libc::pid_t) -> ShutdownJob {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "pin the fixture's exact terminal process");
    ShutdownJob(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

fn shutdown_job(shell: &CloseSafetyShell, command: &[u8], executable: &str) -> ShutdownJob {
    shell.terminal.input(command).unwrap();
    let mut child = None;
    shell.wait_for("background job readiness", || {
        let Some(pid) = fs::read_to_string(shell.home.path().join("child.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
        else {
            return false;
        };
        if !fs::read_link(format!("/proc/{pid}/exe"))
            .is_ok_and(|path| path.file_name() == Some(OsStr::new(executable)))
        {
            return false;
        }
        child = Some(pin_shutdown_process(pid));
        true
    });
    child.unwrap()
}

#[test]
fn a_non_shell_root_command_requires_confirmation() {
    let command = CloseSafetyShell::spawn(Path::new("/bin/sleep"), &["120"]);
    assert_eq!(
        command.terminal.close_checked(false),
        TerminalCloseResult::Active
    );
    assert_eq!(
        command.terminal.close_checked(true),
        TerminalCloseResult::Closed
    );
}

#[test]
fn an_interactive_shell_command_requires_confirmation_even_without_child_jobs() {
    let command = CloseSafetyShell::spawn(
        Path::new("/bin/bash"),
        &["-ic", "printf 'READY_%s\\n' COMMAND; read reply"],
    );
    command.wait_for_text("READY_COMMAND");
    assert_eq!(
        command.terminal.close_checked(false),
        TerminalCloseResult::Active
    );
    assert_eq!(
        command.terminal.close_checked(true),
        TerminalCloseResult::Closed
    );
}

#[test]
fn a_shell_script_requires_confirmation_even_without_child_jobs() {
    let scripts = tempdir().unwrap();
    let script = scripts.path().join("read.sh");
    fs::write(&script, "printf 'READY_%s\\n' SCRIPT; read reply\n").unwrap();
    let command = CloseSafetyShell::spawn(Path::new("/bin/bash"), &[script.to_str().unwrap()]);
    command.wait_for_text("READY_SCRIPT");
    assert_eq!(
        command.terminal.close_checked(false),
        TerminalCloseResult::Active
    );
    assert_eq!(
        command.terminal.close_checked(true),
        TerminalCloseResult::Closed
    );
}
