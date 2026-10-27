use super::*;
use crate::{
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{
        CardId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal_ipc::{ClientRequest, TerminalSpawnOptions},
    workspace_model::views::{Change, Pane, Scope},
};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

const TEST: &str = "host::workspace::pending_tests::pending_publication_serializes_a_new_recipe_without_blocking_host_reads";
const ROOT: &str = "CAGENTS_PENDING_PUBLICATION_TEST_ROOT";
const AUTHORITY_PRESENCE: &str = "CAGENTS_PENDING_PUBLICATION_AUTHORITY_PRESENCE";

struct Evidence(Option<tempfile::TempDir>);
impl Drop for Evidence {
    fn drop(&mut self) {
        if let Some(root) = self.0.take() {
            let _retained = root.keep();
        }
    }
}
pub(super) fn isolated(test: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT) {
        let expected = match std::env::var(AUTHORITY_PRESENCE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing fixture authority presence marker"),
        };
        assert_eq!(
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some(),
            expected,
            "the isolated fixture must retain the caller capability unchanged"
        );
        return Some(PathBuf::from(root).canonicalize().unwrap());
    }
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = Evidence(Some(directory));
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let executable = std::env::current_exe().unwrap();
    let mut command = CommandSpec::new(&executable);
    // Preserve the real inherited authority opaquely. Only predetermined
    // private fixture paths and its public presence boolean cross this seam.
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
            command = command.env_remove(key);
        }
    }
    command = command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .cwd(&root)
        .env(ROOT, &root)
        .env(
            AUTHORITY_PRESENCE,
            if std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some() {
                "present"
            } else {
                "absent"
            },
        )
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .timeout(Duration::from_secs(40))
        .output_limits(256 * 1024, 256 * 1024);
    let output = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let (stdout, stderr) = match &output {
        Ok(output) => (&output.stdout, &output.stderr),
        Err(error) => (&error.stdout, &error.stderr),
    };
    for (name, bytes) in [("stdout", stdout), ("stderr", stderr)] {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(name))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    let output = output
        .unwrap_or_else(|error| panic!("retained {}: {error}", root.display()))
        .require_success(&executable)
        .unwrap_or_else(|error| panic!("retained {}: {error}", root.display()));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("test {test} ... ok")), "{stdout}");
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "{stdout}"
    );
    // Original Child exit is not whole-family proof. Keep the runtime on BOTH
    // outcomes for the existing strict outer command guardian.
    None
}

struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(send) = self.0.take() {
            let _ = send.send(());
        }
    }
}
pub(super) struct Cleanup(pub(super) SharedHost);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let (handles, _) = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .graceful_shutdown();
        for handle in &handles {
            handle.request_shutdown();
        }
        for handle in handles {
            let _ = handle.shutdown();
        }
    }
}

#[test]
fn pending_publication_serializes_a_new_recipe_without_blocking_host_reads() {
    let Some(root) = isolated(TEST) else {
        return;
    };
    let mut registry = Registry::at(&root.join("model.sqlite3")).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.clone(),
            name: "fixture".into(),
            kind: Default::default(),
        })
        .unwrap();
    let scope = Scope::Worktree {
        repository: root.clone(),
        path: root.clone(),
    };
    let launch = LaunchRecipe {
        program: "/bin/sleep".into(),
        arguments: vec!["60".into()],
        working_directory: root.clone(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    };
    let pane = Pane {
        id: CardId::new(),
        title: "original".into(),
        user_title: None,
        pending_terminal_start: true,
        width_fraction: 0.5,
        kind: CardKind::Terminal(session.clone()),
        scope: None,
    };
    let applied = registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: Vec::new(),
        })
        .unwrap();
    let revision = applied
        .model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .revision;
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("pending-publication"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("pending-publication"),
    });
    state.enable_holder_recovery();
    state.install_model(registry);
    state.register_client(1);
    state.register_client(2);
    let host = Arc::new(HostLock::new(state));
    let _cleanup = Cleanup(host.clone());
    let mut changed = pane.clone();
    let CardKind::Terminal(next) = &mut changed.kind else {
        unreachable!()
    };
    next.launch.arguments = vec!["61".into()];
    next.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: next.launch.clone(),
        last_exit: None,
    };
    let request = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: revision,
        operation: uuid::Uuid::new_v4(),
        change: Change::Update {
            before: pane.clone(),
            pane: changed.clone(),
        },
    };
    thread::scope(|threads| {
        let (ready, admitted) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let release = Release(Some(release));
        let spawning = host.clone();
        let pane_id = pane.id;
        let terminal_id = session.id;
        let spawn = threads.spawn(move || {
            BEFORE_PENDING_PUBLICATION.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    ready.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                }))
            });
            crate::host::client::process_request(
                &spawning,
                1,
                ClientRequest::CompletePendingTerminal {
                    pane: pane_id,
                    terminal_id,
                    options: TerminalSpawnOptions::default(),
                },
            )
        });
        admitted.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            writer(&host).unwrap().try_lock().is_err(),
            "pending publication lost its writer admission"
        );
        let (started, entered) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let editing = host.clone();
        let edit = threads.spawn(move || {
            started.send(()).unwrap();
            let answer = crate::host::client::process_request(
                &editing,
                2,
                ClientRequest::EditWorkspaceModel { edit: request },
            );
            let _ = finished.send(answer);
        });
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            matches!(
                result.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "new recipe committed inside pending publication"
        );
        let (read, answer) = mpsc::channel();
        let reading = host.clone();
        let reader = threads.spawn(move || {
            let _ = read.send(crate::host::client::process_request(
                &reading,
                1,
                ClientRequest::List,
            ));
        });
        assert!(
            answer
                .recv_timeout(Duration::from_millis(500))
                .unwrap()
                .is_ok(),
            "writer admission blocked unrelated Host reads"
        );
        drop(release);
        assert!(spawn.join().unwrap().is_ok());
        let error = result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
        edit.join().unwrap();
        reader.join().unwrap();
    });
    let model = snapshot(&host, None).unwrap();
    let view = model.views.iter().find(|view| view.scope == scope).unwrap();
    let completed = view.panes.iter().find(|item| item.id == pane.id).unwrap();
    assert!(!completed.pending_terminal_start);
    assert!(matches!(&completed.kind,CardKind::Terminal(actual) if actual.launch==session.launch));
    let original = host.lock().unwrap().terminals[&session.id].handle.clone();
    let (registered, proof) = {
        let state = host.lock().unwrap();
        (
            state.terminals[&session.id].session.clone(),
            state.proof().clone(),
        )
    };
    // The holder control connection belongs to the live worker. Inspect is
    // an orphan/successor protocol, so do not open a competing live connection.
    let holder = original.holder_identity().expect("real pending holder");
    assert_eq!(holder.terminal, session.id);
    assert_eq!(holder.boot, proof.boot_id);
    assert!(holder.root.is_current());
    assert!(holder.holder.is_current());
    assert_eq!(
        host.lock()
            .unwrap()
            .descriptor(1, session.id)
            .unwrap()
            .session
            .continuity
            .live_proof(),
        Some(proof.clone())
    );
    assert!(matches!(
        registered.continuity,
        TerminalContinuity::DetachedSameBoot { .. }
    ));
    assert_eq!(registered.continuity.live_proof(), Some(proof));
    assert!(
        crate::host::state::pending_worker_source(
            &host.lock().unwrap(),
            pane.id,
            &session,
            &original
        )
        .is_none(),
        "a completed save must not retain a permissive relaunch-source alias"
    );

    assert!(
        crate::host::client::process_request(
            &host,
            2,
            ClientRequest::CompletePendingTerminal {
                pane: pane.id,
                terminal_id: session.id,
                options: TerminalSpawnOptions::default()
            }
        )
        .is_ok(),
        "same-source completion retry must attach the existing worker"
    );
    assert!(
        host.lock().unwrap().terminals[&session.id]
            .handle
            .same_worker(&original)
    );
    // The real UI may publish the actual Live descriptor into the model.
    // An idempotent completion still attaches that exact existing worker.
    let mut projected = completed.clone();
    projected.pending_terminal_start = true;
    projected.kind = CardKind::Terminal(
        host.lock()
            .unwrap()
            .descriptor(1, session.id)
            .unwrap()
            .session,
    );
    edit(
        &host,
        2,
        ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: view.revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: completed.clone(),
                pane: projected,
            },
        },
    )
    .unwrap();
    assert!(
        crate::host::client::process_request(
            &host,
            2,
            ClientRequest::CompletePendingTerminal {
                pane: pane.id,
                terminal_id: session.id,
                options: TerminalSpawnOptions::default()
            }
        )
        .is_ok(),
        "actual published source must remain idempotently attachable"
    );
    assert!(
        host.lock().unwrap().terminals[&session.id]
            .handle
            .same_worker(&original)
    );
    let model = snapshot(&host, None).unwrap();
    let view = model.views.iter().find(|view| view.scope == scope).unwrap();
    let before = view.panes.iter().find(|item| item.id == pane.id).unwrap();
    assert!(
        !before.pending_terminal_start,
        "same-worker projected retry must finish its reservation"
    );
    // A deliberate rebased edit remains authoritative after completion. No
    // delayed completion may overwrite it with the previous source.
    edit(
        &host,
        2,
        ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: view.revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: before.clone(),
                pane: changed.clone(),
            },
        },
    )
    .unwrap();
    let model = snapshot(&host, None).unwrap();
    assert_eq!(
        model
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap()
            .panes
            .iter()
            .find(|item| item.id == pane.id),
        Some(&changed)
    );
    let refused = crate::host::client::process_request(
        &host,
        2,
        ClientRequest::CompletePendingTerminal {
            pane: pane.id,
            terminal_id: session.id,
            options: TerminalSpawnOptions::default(),
        },
    )
    .unwrap_err();
    assert_eq!(refused.code, ProtocolErrorCode::TerminalNotFound);
    assert!(
        host.lock().unwrap().terminals[&session.id]
            .handle
            .same_worker(&original)
    );
    assert!(
        !original.is_completed(),
        "source refusal must not replace/stop the old owned worker"
    );
    original.shutdown().unwrap();
    assert!(original.is_completed());
}

#[test]
fn failed_pending_commit_retries_the_same_original_without_relaunching_or_retargeting() {
    let name = thread::current().name().unwrap().to_owned();
    let Some(root) = isolated(&name) else {
        return;
    };
    let path = root.join("model.sqlite3");
    let mut registry = Registry::at(&path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.clone(),
            name: "pending-retry".into(),
            kind: Default::default(),
        })
        .unwrap();
    let scope = Scope::Worktree {
        repository: root.clone(),
        path: root.clone(),
    };
    let launch = LaunchRecipe {
        program: "/usr/bin/python3".into(),
        arguments: vec!["-u".into(), "-c".into(), "import os,sys\nwith open('launches','a') as f: f.write(str(os.getpid())+'\\n')\nprint('ORIGINAL_READY',flush=True)\nfor line in sys.stdin: print('ORIGINAL_REPLY:'+line.strip(),flush=True)".into()],
        working_directory: root.clone(), environment: Default::default(),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    };
    let pane = Pane {
        id: CardId::new(),
        title: "original".into(),
        user_title: None,
        pending_terminal_start: true,
        width_fraction: 0.5,
        kind: CardKind::Terminal(session.clone()),
        scope: None,
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: Vec::new(),
        })
        .unwrap();
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("pending-retry"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("pending-retry"),
    });
    state.enable_holder_recovery();
    state.install_model(registry);
    state.register_client(1);
    let host = Arc::new(HostLock::new(state));
    let _cleanup = Cleanup(host.clone());
    let failing_path = path.clone();
    // The old hook is on the actual publication trajectory with the writer
    // admission held. Adding a real SQLite object makes the exact published
    // layout inadmissible. Its guard must reject completion before this
    // trigger can execute; startup and the original Handle remain real.
    BEFORE_PENDING_PUBLICATION.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            rusqlite::Connection::open(failing_path).unwrap().execute_batch(
                "CREATE TRIGGER reject_pending_commit BEFORE UPDATE ON shared_views BEGIN SELECT RAISE(ABORT, 'fixture pending persistence failure'); END;"
            ).unwrap();
        }));
    });
    let request = || ClientRequest::CompletePendingTerminal {
        pane: pane.id,
        terminal_id: session.id,
        options: TerminalSpawnOptions::default(),
    };
    let failure = crate::host::client::process_request(&host, 1, request()).unwrap_err();
    assert_eq!(failure.code, ProtocolErrorCode::Internal);
    assert!(!failure.retryable);
    assert_eq!(
        failure.message,
        "The original terminal worker is retained, but its exact pane completion was not confirmed: workspace registry layout does not match published schema 23"
    );
    let original = host.lock().unwrap().terminals[&session.id].handle.clone();
    assert!(!original.is_completed());
    assert!(
        !host.lock().unwrap().startup_blocks_shutdown(),
        "a non-adopted persistence failure must not invent retained adoption custody"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let launches = loop {
        if let Ok(launches) = fs::read_to_string(root.join("launches")) {
            if launches.ends_with('\n') && launches.trim().parse::<i32>().is_ok() {
                break launches;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the real original process did not run"
        );
        thread::sleep(Duration::from_millis(1));
    };
    let pid = launches.trim().parse::<i32>().unwrap();
    let original_process = crate::daemon::holder_registry::Process::read(pid).unwrap();
    let root_pidfd = original.original_root_pidfd_for_test().unwrap();
    use std::os::fd::AsRawFd;
    let fdinfo =
        fs::read_to_string(format!("/proc/self/fdinfo/{}", root_pidfd.as_raw_fd())).unwrap();
    let pinned = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(pinned, original_process.pid);
    // Repair only the fixture's extra object before using an authoritative
    // model read. The unchanged pending pane is checked before retrying.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TRIGGER reject_pending_commit")
        .unwrap();
    let unchanged = snapshot(&host, None).unwrap();
    assert_eq!(
        unchanged
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap()
            .panes,
        vec![pane.clone()]
    );
    let (registered, proof) = {
        let state = host.lock().unwrap();
        let registered = state.terminals[&session.id].session.clone();
        assert_eq!(
            crate::host::state::pending_worker_source(&state, pane.id, &session, &original),
            Some(registered.clone()),
            "SQL failure must retain the exact accepted card/source for this worker"
        );
        assert!(
            crate::host::state::pending_worker_source(&state, CardId::new(), &session, &original)
                .is_none()
        );
        (registered, state.proof().clone())
    };
    // The actual worker retains its authenticated holder connection; inspect
    // is only serviced after orphaning, not alongside this live connection.
    let holder = original.holder_identity().expect("actual pending holder");
    assert_eq!(holder.terminal, session.id);
    assert_eq!(holder.boot, proof.boot_id);
    assert_eq!(holder.root, original_process);
    assert!(holder.holder.is_current());
    assert!(matches!(
        registered.continuity,
        TerminalContinuity::DetachedSameBoot { .. }
    ));
    assert_eq!(registered.continuity.live_proof(), Some(proof.clone()));
    assert_eq!(
        host.lock()
            .unwrap()
            .descriptor(1, session.id)
            .unwrap()
            .session
            .continuity
            .live_proof(),
        Some(proof.clone())
    );
    assert!(matches_pending_issued(&session, &registered, &proof));
    for changed in 0..7 {
        let mut foreign = registered.clone();
        match changed {
            0 => {
                foreign.continuity = TerminalContinuity::DetachedSameBoot {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: HostIncarnation::new("foreign-pending-owner"),
                }
            }
            1 => foreign
                .launch
                .arguments
                .push("different-worker-recipe".into()),
            2 => foreign.predecessor = Some(TerminalId::new()),
            3 => foreign.recovery_origin = Some(TerminalId::new()),
            4 => foreign.transcript_path = Some(root.join("different-transcript")),
            5 => foreign.id = TerminalId::new(),
            _ => {
                foreign.continuity = TerminalContinuity::Live {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: proof.host_incarnation.clone(),
                }
            }
        }
        assert!(
            !matches_pending_issued(&session, &foreign, &proof),
            "changed issued source {changed}"
        );
    }
    // These are authoritative pane edits while the same real worker waits on
    // its failed save. A proof, embedded recipe, historical exit or lineage
    // change cannot borrow the retained original's accepted start intent.
    for changed in 0..5 {
        let model = snapshot(&host, None).unwrap();
        let view = model.views.iter().find(|view| view.scope == scope).unwrap();
        let before = view
            .panes
            .iter()
            .find(|item| item.id == pane.id)
            .unwrap()
            .clone();
        let mut replacement = before.clone();
        let CardKind::Terminal(source) = &mut replacement.kind else {
            unreachable!()
        };
        match changed {
            0 => {
                source.continuity = TerminalContinuity::DetachedSameBoot {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: HostIncarnation::new("foreign-pending-owner"),
                }
            }
            1 => {
                let TerminalContinuity::RelaunchAvailable { recipe, .. } = &mut source.continuity
                else {
                    unreachable!()
                };
                recipe.arguments.push("changed-embedded-recipe".into());
            }
            2 => {
                let TerminalContinuity::RelaunchAvailable { last_exit, .. } =
                    &mut source.continuity
                else {
                    unreachable!()
                };
                *last_exit = Some(crate::model::TerminalExit {
                    code: Some(23),
                    signal: None,
                    occurred_at_unix_ms: 1,
                });
            }
            3 => source.predecessor = Some(TerminalId::new()),
            _ => source.recovery_origin = Some(TerminalId::new()),
        }
        edit(
            &host,
            1,
            ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: view.revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before: before.clone(),
                    pane: replacement.clone(),
                },
            },
        )
        .unwrap();
        let refusal = crate::host::client::process_request(&host, 1, request()).unwrap_err();
        assert_eq!(refusal.code, ProtocolErrorCode::TerminalNotFound);
        assert!(
            host.lock().unwrap().terminals[&session.id]
                .handle
                .same_worker(&original)
        );
        assert!(original_process.is_current());
        assert_eq!(fs::read_to_string(root.join("launches")).unwrap(), launches);
        let model = snapshot(&host, None).unwrap();
        let view = model.views.iter().find(|view| view.scope == scope).unwrap();
        assert_eq!(view.panes, vec![replacement.clone()]);
        edit(
            &host,
            1,
            ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: view.revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before: replacement,
                    pane: before,
                },
            },
        )
        .unwrap();
    }
    assert!(crate::host::client::process_request(&host, 1, request()).is_ok());
    assert!(
        crate::host::state::pending_worker_source(
            &host.lock().unwrap(),
            pane.id,
            &session,
            &original
        )
        .is_none(),
        "successful retry must release only its captured pending intent"
    );

    assert!(
        host.lock().unwrap().terminals[&session.id]
            .handle
            .same_worker(&original)
    );
    assert!(original_process.is_current());
    assert_eq!(fs::read_to_string(root.join("launches")).unwrap(), launches);
    let model = snapshot(&host, None).unwrap();
    let view = model.views.iter().find(|view| view.scope == scope).unwrap();
    let completed = view.panes.iter().find(|item| item.id == pane.id).unwrap();
    assert!(!completed.pending_terminal_start);
    assert!(
        matches!(&completed.kind, CardKind::Terminal(actual) if actual.id == session.id && actual.launch == session.launch)
    );
    let mut replacement = completed.clone();
    let CardKind::Terminal(changed) = &mut replacement.kind else {
        unreachable!()
    };
    changed
        .launch
        .arguments
        .push("replacement-that-must-never-run".into());
    changed.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: changed.launch.clone(),
        last_exit: None,
    };
    replacement.pending_terminal_start = true;
    edit(
        &host,
        1,
        ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: view.revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: completed.clone(),
                pane: replacement.clone(),
            },
        },
    )
    .unwrap();
    let refused = crate::host::client::process_request(&host, 1, request()).unwrap_err();
    assert_eq!(refused.code, ProtocolErrorCode::TerminalNotFound);
    assert!(
        host.lock().unwrap().terminals[&session.id]
            .handle
            .same_worker(&original)
    );
    assert!(original_process.is_current());
    assert_eq!(fs::read_to_string(root.join("launches")).unwrap(), launches);
    assert_eq!(
        snapshot(&host, None)
            .unwrap()
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap()
            .panes,
        vec![replacement]
    );
    assert_eq!(
        original.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
    assert!(original.is_completed());
    assert!(
        !original_process.is_current(),
        "the same original epoch must be reaped"
    );
    let mut ready = libc::pollfd {
        fd: root_pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut ready, 1, 0) }, 1);
    assert_ne!(ready.revents & libc::POLLIN, 0);
}
