#![cfg(all(feature = "host", target_os = "linux"))]
//! Actual matched source inspection and daemon handoff in one private profile.
use cagents::{
    agent_execution::Authority,
    control::ControlClient,
    model::{
        CardId, CardKind, CardState, Existence, Exposure, LaunchRecipe, Residency, TerminalId,
        WorkspaceState,
    },
    persistence::{SnapshotBase, StateStore},
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    terminal::TerminalSpawnOptions,
    terminal_ipc::TerminalDescriptor,
};
use std::{
    collections::BTreeMap,
    fs,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, MetadataExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct ChildOwner(Child);
impl ChildOwner {
    fn wait_until(&mut self, deadline: Instant) -> Result<std::process::ExitStatus, String> {
        loop {
            if let Some(status) = self.0.try_wait().map_err(|e| e.to_string())? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err("private child deadline expired".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // An unreaped original child reserves its PID through this signal.
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let _ = self.wait_until(deadline);
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn daemon(root: &Path, name: &str) -> Result<ChildOwner, String> {
    Command::new(env!("CARGO_BIN_EXE_cagentsd"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.join(name)).map_err(|e| e.to_string())?)
        .spawn()
        .map(ChildOwner)
        .map_err(|e| e.to_string())
}
fn connected(
    socket: &Path,
    daemon: &mut ChildOwner,
    deadline: Instant,
) -> Result<ControlClient, String> {
    loop {
        if let Ok(mut client) = ControlClient::connect(socket) {
            client
                .authenticate(Authority::from_environment())
                .map_err(|e| e.to_string())?;
            return Ok(client);
        }
        if daemon.0.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Err("private daemon exited before readiness".into());
        }
        if Instant::now() >= deadline {
            return Err("private daemon readiness deadline expired".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn inspect(
    root: &Path,
    proof: &cagents::model::HostSessionProof,
    epoch: &str,
    deadline: Instant,
) -> Result<(), String> {
    let output = root.join(format!("inspect-{epoch}.json"));
    let guard = cagents::instance::UiUpdateGuard::acquire().map_err(|e| e.to_string())?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
    command
        .args([
            "--inspect-upgrade-source",
            &serde_json::json!({
                "protocol":cagents::terminal_ipc::PROTOCOL_VERSION,
                "proof":proof, "require_resumable":true
            })
            .to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(fs::File::create(&output).map_err(|e| e.to_string())?)
        .stderr(
            fs::File::create(root.join(format!("inspect-{epoch}.log")))
                .map_err(|e| e.to_string())?,
        );
    guard
        .inherit_for_install(&mut command)
        .map_err(|e| e.to_string())?;
    let mut child = ChildOwner(command.spawn().map_err(|e| e.to_string())?);
    let status = child.wait_until(deadline)?;
    drop(child);
    drop(guard);
    if !status.success() {
        return Err("actual private inspection helper failed".into());
    }
    if fs::metadata(&output).map_err(|e| e.to_string())?.len() > 16 * 1024 {
        return Err("inspection receipt exceeded limit".into());
    }
    serde_json::from_slice::<Result<(), String>>(&fs::read(output).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
}
fn birth(pid: u32) -> Result<String, String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .map(str::to_owned)
        .ok_or("missing original process birth".into())
}
struct ShellOwner(OwnedFd);
impl Drop for ShellOwner {
    fn drop(&mut self) {
        if live(&self.0) {
            // This fd still names only the fixture's original private shell.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while live(&self.0) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if live(&self.0) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
    }
}
fn live(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut poll, 1, 0) == 0 }
}

#[test]
fn actual_same_generation_inspection_and_upgrade_keep_the_original_saved_pty() {
    const CHILD: &str = "CAGENTS_UPGRADE_INSPECTION_FIXTURE";
    if std::env::var(CHILD).as_deref() != Ok("private-child") {
        let root = tempfile::Builder::new()
            .prefix("upgrade-inspect-")
            .tempdir_in("/var/tmp")
            .unwrap()
            .keep();
        let mut spec = CommandSpec::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "actual_same_generation_inspection_and_upgrade_keep_the_original_saved_pty",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "private-child")
            .env("CAGENTS_UPGRADE_INSPECTION_ROOT", &root)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .timeout(Duration::from_secs(90))
            .output_limits(128 * 1024, 128 * 1024);
        for (key, name) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
        ] {
            let path = root.join(name);
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            spec = spec.env(key, path);
        }
        // Inherited delegation stays intact; only this child receives the private profile.
        let output = SystemProcessExecutor
            .execute(&spec, &CancellationToken::new())
            .unwrap();
        fs::write(root.join("stdout.log"), &output.stdout).unwrap();
        fs::write(root.join("stderr.log"), &output.stderr).unwrap();
        let output = output.require_success(&spec.program).unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("running 1 test") && text.contains("1 passed; 0 failed; 0 ignored"));
        return;
    }
    let root = PathBuf::from(std::env::var_os("CAGENTS_UPGRADE_INSPECTION_ROOT").unwrap());
    let deadline = Instant::now() + Duration::from_secs(65);
    let socket = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut source = daemon(&root, "source.log").unwrap();
    let mut successor = None;
    let outcome = (|| -> Result<_, String> {
        let mut client = connected(&socket, &mut source, deadline)?;
        let original_proof = client.proof().clone();
        let pid_path = root.join("shell.pid");
        let recipe=LaunchRecipe {
            program:"/bin/sh".into(), arguments:vec!["-c".into(),
                "printf '%s\\n' \"$$\" > \"$1\"; while IFS= read -r line; do case \"$line\" in before) printf 'SOURCE-PTY-OK\\n';; after) printf 'SUCCESSOR-PTY-OK\\n';; esac; done".into(),
                "private-fixture".into(),pid_path.to_string_lossy().into_owned()],
            working_directory:root.clone(),environment:std::env::vars().filter(|(key,_)|
                ["HOME","PATH","XDG_CONFIG_HOME","XDG_DATA_HOME","XDG_STATE_HOME","XDG_CACHE_HOME"].contains(&key.as_str())).collect(),
            unset_environment:Default::default(),
        };
        let id = TerminalId::new();
        let saved = client
            .spawn(
                TerminalDescriptor::relaunchable(id, recipe, None).session,
                TerminalSpawnOptions::default(),
            )
            .map_err(|e| e.to_string())?
            .session;
        drop(client); // Inventory uses a fresh connection, not the spawn subscriber.
        let mut client = connected(&socket, &mut source, deadline)?;
        while !pid_path.exists() {
            if Instant::now() >= deadline {
                return Err("shell PID evidence missing".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(&pid_path)
            .map_err(|e| e.to_string())?
            .trim()
            .parse::<u32>()
            .map_err(|e| e.to_string())?;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err("cannot retain original private shell pidfd".into());
        }
        let pidfd = ShellOwner(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        let original_birth = birth(pid)?;
        let tty = fs::metadata(format!("/proc/{pid}/fd/0")).map_err(|e| e.to_string())?;
        let tty_stamp = (tty.dev(), tty.ino(), tty.rdev());
        let mut workspace = WorkspaceState::default();
        workspace.cards.push(CardState {
            id: CardId::new(),
            generation: 0,
            title: "Original shell".into(),
            width_fraction: 0.6,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Terminal(saved.clone()),
        });
        workspace.focused = Some(workspace.cards[0].id);
        let database = cagents::instance::window_database_path(cagents::instance::PRIMARY_WINDOW)
            .map_err(|e| e.to_string())?;
        let store = StateStore::open(&database, original_proof.boot_id.clone())
            .map_err(|e| e.to_string())?;
        store
            .save_workspace(
                &workspace,
                SnapshotBase {
                    workspace: 0,
                    cards: &BTreeMap::new(),
                },
            )
            .map_err(|e| e.to_string())?;
        drop(store);
        inspect(&root, &original_proof, "source", deadline)?;
        client
            .write(id, b"before\n".to_vec())
            .map_err(|e| e.to_string())?;
        let before = client
            .wait_for_text(
                id,
                "SOURCE-PTY-OK",
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(|e| e.to_string())?;
        drop(client);
        let mut client = connected(&socket, &mut source, deadline)?;
        let capability = client
            .shutdown_preflight()
            .map_err(|e| e.to_string())?
            .capability;
        client
            .prepare_upgrade(capability, false)
            .map_err(|e| e.to_string())?;
        drop(client);
        if !source.wait_until(deadline)?.success() {
            return Err("source upgrade exit failed".into());
        }
        if !live(&pidfd.0) {
            return Err("original shell exited during upgrade".into());
        }
        successor = Some(daemon(&root, "successor.log")?);
        let mut client = connected(&socket, successor.as_mut().unwrap(), deadline)?;
        let new_proof = client.proof().clone();
        let inventory = client.list().map_err(|e| e.to_string())?;
        let restored = inventory
            .iter()
            .find(|entry| entry.session.id == id)
            .ok_or("original terminal absent from successor")?;
        let exact_launch = restored.session.launch == saved.launch;
        inspect(&root, &new_proof, "successor", deadline)?;
        client
            .write(id, b"after\n".to_vec())
            .map_err(|e| e.to_string())?;
        let after = client
            .wait_for_text(
                id,
                "SUCCESSOR-PTY-OK",
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(|e| e.to_string())?;
        let tty = fs::metadata(format!("/proc/{pid}/fd/0")).map_err(|e| e.to_string())?;
        let same_root = live(&pidfd.0)
            && birth(pid)? == original_birth
            && (tty.dev(), tty.ino(), tty.rdev()) == tty_stamp;
        client.close_terminal(id).map_err(|e| e.to_string())?;
        while live(&pidfd.0) {
            if Instant::now() >= deadline {
                return Err("original shell did not settle after explicit close".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok((
            original_proof,
            new_proof,
            exact_launch,
            same_root,
            before,
            after,
        ))
    })();
    drop(successor);
    drop(source);
    let (old, new, exact_launch, same_root, _, _) = outcome.unwrap();
    assert_ne!(old, new);
    assert_eq!(old.boot_id, new.boot_id);
    assert!(exact_launch);
    assert!(same_root);
}
