#![cfg(all(feature = "host", target_os = "linux"))]
//! Actual matched source inspection and daemon handoff in one private profile.
//! The upgrade is driven by an explicit synthetic fixture User protocol client,
//! not a CLI User execution. Inherited delegation is separately proved refused
//! by the unrelated private daemon before the fixture User starts any terminal.
use cagents::{
    agent_execution::Authority,
    control::{ControlClient, ControlError},
    model::{
        CardId, CardKind, CardState, Existence, Exposure, LaunchRecipe, Residency, TerminalId,
        WorkspaceState,
    },
    persistence::{SnapshotBase, StateStore},
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    terminal::TerminalSpawnOptions,
    terminal_ipc::{ProtocolErrorCode, TerminalDescriptor},
};
use std::{
    collections::BTreeMap,
    fs,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
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
        if let Ok(client) = ControlClient::connect(socket) {
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
fn fixture_user_connected(
    socket: &Path,
    daemon: &mut ChildOwner,
    deadline: Instant,
) -> Result<ControlClient, String> {
    // A distinct, explicitly declared fixture User connection. This never
    // changes the environment or retries a refused delegated connection.
    let mut client = connected(socket, daemon, deadline)?;
    client
        .authenticate(Authority::default())
        .map_err(|e| e.to_string())?;
    Ok(client)
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
    const SCOPE: &str = "CAGENTS_UPGRADE_INSPECTION_AUTHORITY_PRESENCE";
    // Select the contract before operations, using only public equality. Never
    // print, copy, remove or replace an inherited session capability.
    let delegated = Authority::from_environment() != Authority::default();
    if std::env::var(CHILD).as_deref() != Ok("private-child") {
        let root = tempfile::Builder::new()
            .prefix("up-")
            .tempdir()
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
            .env(SCOPE, if delegated { "present" } else { "absent" })
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
        let result = SystemProcessExecutor.execute(&spec, &CancellationToken::new());
        let (stdout, stderr) = match &result {
            Ok(output) => (&output.stdout, &output.stderr),
            Err(failure) => (&failure.stdout, &failure.stderr),
        };
        // Preserve bounded raw child output even on timeout/spawn failure,
        // before either executor or nonzero-exit assertion can unwind.
        fs::write(root.join("stdout.log"), stdout).unwrap();
        fs::write(root.join("stderr.log"), stderr).unwrap();
        let output = result.unwrap();
        let output = output.require_success(&spec.program).unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("running 1 test") && text.contains("1 passed; 0 failed; 0 ignored"));
        return;
    }
    let expected_scope = match std::env::var(SCOPE).as_deref() {
        Ok("present") => true,
        Ok("absent") => false,
        _ => panic!("missing or invalid inherited authority presence marker"),
    };
    assert_eq!(
        delegated, expected_scope,
        "private fixture child changed inherited authority scope"
    );
    let root = PathBuf::from(std::env::var_os("CAGENTS_UPGRADE_INSPECTION_ROOT").unwrap());
    let deadline = Instant::now() + Duration::from_secs(65);
    let socket = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut source = daemon(&root, "source.log").unwrap();
    let mut successor = None;
    let outcome = (|| -> Result<_, String> {
        // Observe the empty private daemon independently of the inherited
        // caller. No terminal or saved workspace exists before this check.
        let mut observer = fixture_user_connected(&socket, &mut source, deadline)?;
        let before = observer.list().map_err(|e| e.to_string())?;
        assert!(before.is_empty());
        let observer_proof = observer.proof().clone();
        if delegated {
            let mut inherited = connected(&socket, &mut source, deadline)?;
            let refusal = inherited.authenticate(Authority::from_environment());
            assert!(
                matches!(
                    refusal,
                    Err(ControlError::Remote {
                        code: ProtocolErrorCode::PermissionDenied,
                        ref message,
                        retryable: false,
                        details: None,
                        ..
                    }) if message == "This agent session authority is no longer registered"
                ),
                "unrelated private daemon must precisely refuse inherited delegation"
            );
            drop(inherited); // Never retry this connection as User.
            assert_eq!(observer.list().map_err(|e| e.to_string())?, before);
            assert_eq!(observer.proof(), &observer_proof);
            assert!(!root.join("shell.pid").exists());
            eprintln!(
                "coverage: inherited authority precisely refused with unchanged empty terminal inventory/proof; subsequent upgrade uses separate explicit fixture User RPC, not CLI User execution"
            );
        } else {
            eprintln!(
                "coverage: genuinely nondelegated environment; upgrade still uses explicit fixture User RPC, not CLI execution"
            );
        }
        drop(observer);
        let mut client = fixture_user_connected(&socket, &mut source, deadline)?;
        assert_eq!(client.proof(), &observer_proof);
        let original_proof = client.proof().clone();
        let pid_path = root.join("shell.pid");
        let recipe=LaunchRecipe {
            program:"/bin/sh".into(), arguments:vec!["-c".into(),
                "printf '%s\\n' \"$$\" > \"$1\"; while IFS= read -r line; do case \"$line\" in before) printf 'SOURCE-PTY-OK\\n';; after) printf 'SUCCESSOR-PTY-OK\\n';; esac; done".into(),
                "private-fixture".into(),pid_path.to_string_lossy().into_owned()],
            working_directory:root.clone(),environment:["HOME","PATH","XDG_CONFIG_HOME","XDG_DATA_HOME","XDG_STATE_HOME","XDG_CACHE_HOME"]
                .into_iter().filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(),value))).collect(),
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
        let mut client = fixture_user_connected(&socket, &mut source, deadline)?;
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
        #[derive(serde::Deserialize)]
        struct Epoch {
            pid: u32,
            start: u64,
        }
        #[derive(serde::Deserialize)]
        struct OriginalHolder {
            terminal: TerminalId,
            application: String,
            boot: cagents::model::BootId,
            root: Epoch,
            holder: Epoch,
        }
        let record_path = socket
            .parent()
            .unwrap()
            .join("holders")
            .join(format!("{id}.json"));
        let mut record_file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(record_path)
            .map_err(|e| e.to_string())?;
        let record_stat = record_file.metadata().map_err(|e| e.to_string())?;
        if !record_stat.is_file()
            || record_stat.uid() != unsafe { libc::geteuid() }
            || record_stat.mode() & 0o077 != 0
            || record_stat.len() > 16 * 1024
        {
            return Err("original private holder record is unsafe".into());
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut record_file, 16 * 1024 + 1),
            &mut bytes,
        )
        .map_err(|e| e.to_string())?;
        if bytes.len() > 16 * 1024 {
            return Err("original private holder record exceeds limit".into());
        }
        let record: OriginalHolder = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if record.terminal != id
            || record.application != cagents::instance::APPLICATION_ID
            || record.boot != original_proof.boot_id
            || record.root.pid != pid
            || pid <= 1
        {
            return Err("shell hint does not name the actual original holder root".into());
        }
        let original_birth = record.root.start.to_string();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err("cannot retain original private shell pidfd".into());
        }
        // This is read-only until the acquired epoch is positively bound below.
        let acquired = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u32>().ok());
        if !live(&acquired)
            || birth(pid)? != original_birth
            || parent != Some(record.holder.pid)
            || record.holder.pid <= 1
            || birth(record.holder.pid)? != record.holder.start.to_string()
        {
            return Err("acquired epoch does not belong to the original private holder".into());
        }
        let mut expected = saved.launch.program.as_bytes().to_vec();
        expected.push(0);
        for argument in &saved.launch.arguments {
            expected.extend_from_slice(argument.as_bytes());
            expected.push(0);
        }
        let mut argv = Vec::new();
        let mut command_line =
            fs::File::open(format!("/proc/{pid}/cmdline")).map_err(|e| e.to_string())?;
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut command_line, expected.len() as u64 + 1),
            &mut argv,
        )
        .map_err(|e| e.to_string())?;
        if fs::read_link(format!("/proc/{pid}/cwd")).map_err(|e| e.to_string())? != root
            || argv != expected
            || birth(pid)? != original_birth
            || !live(&acquired)
        {
            return Err("acquired process is not the original private fixture shell".into());
        }
        let pidfd = ShellOwner(acquired);
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
        let mut client = fixture_user_connected(&socket, &mut source, deadline)?;
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
        let mut client = fixture_user_connected(&socket, successor.as_mut().unwrap(), deadline)?;
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
