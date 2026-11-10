//! A fake old console exits on daemon EOF while a real retained actor keeps
//! its slow fixture provider running. The replacement never resends its turn.
use super::*;
use crate::{
    agent_execution::{Destination, HostedSession, Origin, Spec, State, Submit, Surface, Target},
    model::{CardId, CardKind, HostIncarnation, HostSessionId, LaunchRecipe, ProjectKind},
    workspace_model::{
        ModelEdit,
        views::{Pane, Scope},
    },
};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
};

#[test]
fn daemon_eof_relaunches_only_the_console_and_keeps_its_running_turn() {
    run_case(None);
}

#[test]
fn console_replacement_recovers_a_crash_after_unpair_ack() {
    run_case(Some("unpair"));
}

#[test]
fn console_replacement_recovers_spawn_before_its_holder_receipt_is_saved() {
    run_case(Some("spawn"));
}

#[test]
fn console_replacement_recovers_pair_ack_before_registry_commit() {
    run_case(Some("pair"));
}

fn wait(mut ready: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn run_case(crash: Option<&'static str>) {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const ROOT: &str = "CAGENTS_CONSOLE_RECOVERY_FIXTURE_ROOT";
    if std::env::var_os(ROOT).is_none() {
        fs::create_dir_all("/var/tmp/cr").unwrap();
        let root = tempfile::Builder::new()
            .prefix("cvu")
            .tempdir_in("/var/tmp/cr")
            .unwrap();
        for directory in [
            "home/.claude",
            "home/.codex",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "old",
            "new",
            "project",
        ] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        let name = thread::current().name().unwrap().to_owned();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env_clear()
            .envs(crate::retained_fixture::owner_env())
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                super::retained_actor_tests::retained_helper_executable(),
            )
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env(ROOT, root.path())
            .env("HOME", root.path().join("home"))
            .env("CLAUDE_CONFIG_DIR", root.path().join("home/.claude"))
            .env("CODEX_HOME", root.path().join("home/.codex"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .env("CAGENTS_TEST_CONSOLE_CLI", root.path().join("new/cagents"));
        if let Some(authority) = std::env::var_os(crate::agent_execution::AUTHORITY_ENV) {
            command.env(crate::agent_execution::AUTHORITY_ENV, authority);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
    super::retained_actor_tests::install_provider(&root.join("bin/claude"));
    executable(
        &root.join("old/cagents"),
        r#"#!/usr/bin/python3
import os, pathlib, socket, sys
assert sys.argv[1:3]==['agent','console']
sock=socket.socket(socket.AF_UNIX)
sock.connect(str(pathlib.Path(os.environ['HOME']).parent/'old.sock'))
while sock.recv(1): pass
print('cagents: daemon connection closed', flush=True)
sys.exit(1)
"#,
    );
    executable(
        &root.join("new/cagents"),
        r#"#!/usr/bin/python3
import fcntl, json, os, pathlib, select, sys
assert sys.argv[1:3]==['agent','console']
root=pathlib.Path(os.environ['HOME']).parent
with (root/'new-views').open('a') as log: log.write(str(os.getpid())+'\n')
print('Console reconnected to the retained agent', flush=True)
owner=json.loads(os.environ['CAGENTS_CONSOLE_STARTUP_OWNER'])
lock=pathlib.Path(os.environ['XDG_RUNTIME_DIR'])/'cagents'/'cagentsd.lock'
while True:
    try:
        with lock.open() as lease:
            if json.load(lease)['proof'] != owner: sys.exit(1)
            try:
                fcntl.flock(lease, fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                fcntl.flock(lease, fcntl.LOCK_UN)
                sys.exit(1)
    except OSError:
        sys.exit(1)
    if select.select([sys.stdin],[],[],.02)[0] and not sys.stdin.buffer.read(1): break
"#,
    );
    fs::write(root.join("lease-third-frame-gate"), b"hold").unwrap();
    let listener = UnixListener::bind(root.join("old.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |name| HostSessionProof {
        host_session_id: HostSessionId::new("console-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(name),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let db = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&db).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Console fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: Some("console-fixture".into()),
                destination: Destination::New(Spec {
                    provider: Provider::Claude,
                    account: Some(crate::account::Account::system(Provider::Claude).id()),
                    target: Target {
                        repository: root.join("project"),
                        directory: root.join("project"),
                        placement: None,
                    },
                    surface: Surface::Terminal,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "One retained slow turn".into(),
                idempotency_key: "one-console-turn".into(),
            },
            1,
        )
        .unwrap();
    let mut state = HostState::new(first.clone());
    state.enable_holder_recovery();
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let first_notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(first_notifications.handle());
    let session = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = session.id;
    start_execution(
        &host,
        0,
        session,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let actor = host.lock().unwrap().chats[&id].handle.clone();
    let identity = actor.actor_identity().unwrap();
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let issued = host.lock().unwrap().chats[&id].session.clone();
    registry
        .lock()
        .unwrap()
        .bind_agent(accepted.agent.id, None, HostedSession::Chat(issued))
        .unwrap();
    let recipe = LaunchRecipe {
        program: root.join("old/cagents").display().to_string(),
        arguments: vec![
            "agent".into(),
            "console".into(),
            accepted.agent.id.to_string(),
        ],
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    host.lock()
        .unwrap()
        .chats
        .get_mut(&id)
        .unwrap()
        .console_recipe = Some(recipe.clone());
    let terminal =
        crate::terminal_ipc::TerminalDescriptor::relaunchable(id, recipe.clone(), None).session;
    let (descriptor, console) = super::super::state::spawn_terminal_view_owned(
        &host,
        0,
        terminal,
        Default::default(),
        None,
        true,
    )
    .unwrap();
    let pair = console.holder_identity().unwrap().clone();
    let mut connection = None;
    wait(
        || {
            connection = listener.accept().ok().map(|(stream, _)| stream);
            connection.is_some()
        },
        "fake console never connected to its old daemon",
    );
    let mut paired = Handle::inspect_remote(&identity.record, first.clone())
        .unwrap()
        .source;
    paired.terminal = Some(descriptor.session.clone());
    actor
        .update_native_source(paired.clone(), Some(recipe), Some(pair.clone()))
        .unwrap();
    {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&id).unwrap();
        record.session = paired;
        record.paired_terminal = Some(console.clone());
    }
    let agent_source = registry
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap();
    registry
        .lock()
        .unwrap()
        .bind_agent_console(accepted.agent.id, &agent_source, descriptor.session.clone())
        .unwrap();
    let scope = Scope::Channel(accepted.agent.channel().unwrap());
    let pane = Pane {
        id: CardId::from_uuid(accepted.agent.id.as_uuid()),
        title: "Working console".into(),
        user_title: Some("Keep this title".into()),
        pending_terminal_start: false,
        width_fraction: 0.4,
        kind: CardKind::Terminal(descriptor.session.clone()),
        scope: None,
    };
    registry
        .lock()
        .unwrap()
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
    super::super::agent_execution::await_provider_ready(&actor).unwrap();
    actor
        .command_for_run(
            accepted.run.id,
            chat::Command::Prompt {
                text: "One retained slow turn".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            },
        )
        .unwrap();
    wait(
        || {
            registry
                .lock()
                .unwrap()
                .agent_run(accepted.run.id)
                .unwrap()
                .state
                == State::Running
                && root.join("produced").exists()
        },
        "the accepted fixture turn never started",
    );
    let before = Handle::inspect_remote(&identity.record, first.clone())
        .unwrap()
        .source;
    console.preserve_for_restart().unwrap();
    actor.preserve_for_restart().unwrap();
    drop(connection);
    wait(
        || !pair.root.is_current(),
        "the old console did not fail on daemon EOF",
    );
    assert!(
        identity.record.root.is_current(),
        "console EOF must leave the provider alive"
    );
    drop((console, actor, first_notifications, host));
    drop(lease.take());
    let mut current = proof("second");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &current).unwrap());
    let make_host = |proof: HostSessionProof| {
        let mut state = HostState::new(proof);
        state.enable_holder_recovery();
        state.install_model(super::super::model::Registry::at(&db).unwrap());
        let host = Arc::new(HostLock::new(state));
        let notifications = super::super::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(notifications.handle());
        (host, notifications)
    };
    let (mut successor, mut notifications) = make_host(current.clone());
    super::super::state::restore_orphaned_holders(&successor).unwrap();
    if let Some(point) = crash {
        super::console_relaunch::fail_after_for_test(point);
        assert!(
            super::native_recovery::restore(&successor, 0, &before)
                .unwrap_err()
                .contains("fixture crash")
        );
        assert!(successor.lock().unwrap().startup_recovery().is_some());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .agent_run(accepted.run.id)
                .unwrap()
                .state,
            State::Running
        );
        assert!(identity.record.root.is_current());
        let previous_view = successor
            .lock()
            .unwrap()
            .terminals
            .get(&id)
            .filter(|record| record.handle.holder_identity() != Some(&pair))
            .and_then(|record| record.handle.holder_identity().cloned());
        if previous_view.is_some() {
            wait(
                || root.join("new-views").exists(),
                "the first replacement never entered its startup wait",
            );
        }
        if let Some(record) = successor.lock().unwrap().terminals.get(&id) {
            record.handle.preserve_for_restart().unwrap();
        }
        drop((notifications, successor));
        drop(lease.take());
        if let Some(view) = previous_view {
            wait(
                || !view.root.is_current(),
                "the pending console did not stop when its startup owner left",
            );
        }
        current = proof("third");
        lease = Some(crate::daemon::HostLease::acquire(&paths, &current).unwrap());
        (successor, notifications) = make_host(current.clone());
        super::super::state::restore_orphaned_holders(&successor).unwrap();
    }
    let request = Handle::inspect_remote(&identity.record, current.clone())
        .unwrap()
        .source;
    super::native_recovery::restore(&successor, 0, &request).unwrap();
    let retained = retained_actor::retained_runs(&successor);
    registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(2, &retained)
        .unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .state,
        State::Running
    );
    assert!(
        identity.record.root.is_current(),
        "the original provider process must survive replacement"
    );
    let state = successor.lock().unwrap();
    let replacement = &state.terminals[&id];
    assert!(!replacement.handle.is_completed());
    assert_ne!(replacement.handle.holder_identity(), Some(&pair));
    assert_eq!(
        replacement.session.launch.program,
        root.join("new/cagents").display().to_string()
    );
    assert!(
        state
            .descriptor(0, id)
            .unwrap()
            .upgrade_sources
            .iter()
            .any(|source| source.launch == descriptor.session.launch)
    );
    let new_actor = state.chats[&id].handle.clone();
    assert_eq!(new_actor.actor_identity(), Some(identity.clone()));
    drop(state);
    let model = registry.lock().unwrap().model().unwrap();
    let saved = &model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .panes[0];
    assert_eq!(
        (
            saved.id,
            &saved.title,
            &saved.user_title,
            saved.width_fraction
        ),
        (pane.id, &pane.title, &pane.user_title, pane.width_fraction)
    );
    assert!(
        matches!(&saved.kind, CardKind::Terminal(view) if view.id == id && view.launch.program == root.join("new/cagents").display().to_string())
    );
    assert!(!registry.lock().unwrap().has_console_relaunch(id).unwrap());
    wait(
        || root.join("new-views").exists(),
        "replacement view did not start",
    );
    assert_eq!(
        fs::read_to_string(root.join("new-views"))
            .unwrap()
            .lines()
            .count(),
        if matches!(crash, Some("spawn" | "pair")) {
            2
        } else {
            1
        },
        "each view replacement needs exact closure of the previous startup owner's view"
    );
    fs::write(root.join("lease-third-frame-release"), b"continue").unwrap();
    wait(
        || {
            registry
                .lock()
                .unwrap()
                .agent_run(accepted.run.id)
                .unwrap()
                .state
                == State::Succeeded
        },
        "the same retained turn never completed",
    );
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "console restoration must never replay a prompt"
    );
    assert_eq!(
        fs::read_to_string(root.join("launches"))
            .unwrap()
            .lines()
            .count(),
        1,
        "console restoration must never start another provider"
    );
    super::super::agent_execution::process(
        &successor,
        crate::agent_execution::Authority::default(),
        crate::agent_execution::Operation::Close(accepted.agent.id),
    )
    .unwrap();
    drop((new_actor, notifications, successor, lease));
}
