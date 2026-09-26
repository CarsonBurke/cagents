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
        let launch = LaunchRecipe {
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
        let (wake, wakes) = crossbeam_channel::unbounded();
        let terminal = TerminalHandle::spawn(
            TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                id: TerminalId::new(),
                launch: launch.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch,
                    last_exit: None,
                },
            },
            TerminalConfig::default(),
            move |_| {
                let _ = wake.send(());
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
