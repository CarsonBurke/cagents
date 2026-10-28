//! Admission regressions through the public host startup path. Every helper,
//! provider and discovery endpoint belongs to this test's isolated subprocess.
use super::*;
use crate::daemon::holder_registry::{self as holders, Kind};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn isolated(name: &str) -> Option<PathBuf> {
    const MODE: &str = "CAGENTS_RETAINED_ADMISSION_TEST";
    if std::env::var(MODE).as_deref() == Ok(name) {
        return Some(PathBuf::from(
            std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap(),
        ));
    }
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path();
    for child in [
        "home/.claude",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "project",
    ] {
        fs::create_dir_all(root.join(child)).unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("host::chat::retained_admission_tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(MODE, name)
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", root)
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        );
    if let Some(helper) = std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER") {
        command.env("CAGENTS_TEST_RETAINED_ACTOR_HELPER", helper);
    }
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    command
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Failed assertions or a hung child must not retain these private helpers.
    cleanup_discoveries(&root.join("run"));
    assert!(
        status.is_some_and(|status| status.success()),
        "isolated admission fixture failed or exceeded 90 seconds:\n{}\n{}",
        fs::read_to_string(stdout).unwrap(),
        fs::read_to_string(stderr).unwrap()
    );
    None
}

fn cleanup_owner(owner: &holders::Record) {
    for process in [&owner.root, &owner.holder] {
        if process.pid != std::process::id() as i32 && process.is_current() {
            unsafe {
                libc::kill(process.pid, libc::SIGKILL);
            }
        }
    }
}

fn cleanup_discoveries(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            cleanup_discoveries(&entry.path());
        } else if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            if let Ok(bytes) = fs::read(entry.path())
                && let Ok(owner) = serde_json::from_slice::<holders::Record>(&bytes)
            {
                cleanup_owner(&owner);
            }
        }
    }
}

fn setup(root: &Path, label: &str) -> (SharedHost, HostSessionProof) {
    let proof = HostSessionProof {
        host_session_id: label.into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "current".into(),
    };
    let mut state = HostState::new(proof.clone());
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    (Arc::new(HostLock::new(state)), proof)
}

fn start(host: &SharedHost, session: Session) -> Result<ServerResponse, ProtocolError> {
    let context = Arc::new(
        chat::protocol::ExecutionContext::capture(&session.launch.recipe.working_directory)
            .unwrap(),
    );
    start_with_execution(
        host,
        0,
        session,
        None,
        false,
        None,
        Default::default(),
        Some(context),
        None,
    )
}

#[test]
fn unsupported_live_owner_fences_reconnect_only_original_alias_before_rpc() {
    let Some(root) =
        isolated("unsupported_live_owner_fences_reconnect_only_original_alias_before_rpc")
    else {
        return;
    };
    let provider = root.join("bin/claude");
    fs::write(&provider, "#!/bin/sh\nprintf spawned > \"$0.spawned\"\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let (host, proof) = setup(&root, "unsupported-admission");
    let mut requested = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    requested.proof = Some(proof.clone());
    requested.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let directory = holders::directory(Kind::Native).unwrap();
    crate::secure_file::prepare_private_directory(&directory).unwrap();
    let socket = holders::socket(&directory, requested.id);
    let listener = holders::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let process = holders::Process::read(std::process::id() as i32).unwrap();
    let mut build = holders::BuildIdentity::current(Kind::Native);
    build.capabilities.push("claude-actor-v1".into());
    let owner = holders::Record {
        version: 4,
        build: Some(build),
        kind: Kind::Native,
        application: crate::instance::APPLICATION_ID.into(),
        boot: proof.boot_id,
        terminal: requested.id,
        holder: process.clone(),
        root: process,
    };
    owner.create(&directory).unwrap();
    assert!(!Handle::is_remote_record(&owner));
    let failure = start(&host, requested.clone()).unwrap_err();
    assert!(
        failure.message.contains("unsupported private protocol"),
        "{}",
        failure.message
    );
    assert!(
        !provider.with_extension("spawned").exists(),
        "discovery must fence recipe execution"
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "unsupported protocol must refuse before RPC"
    );
    assert_eq!(
        holders::recovery_candidate(&socket, Kind::Native).unwrap(),
        Some(owner.clone())
    );
    assert!(owner.holder.is_current() && owner.root.is_current());
    let state = host.lock().unwrap();
    assert!(
        state.chat_spawning.is_empty()
            && state.chats.is_empty()
            && state.native_actor_startups.is_empty()
    );
}

#[test]
fn installed_startup_with_uncommitted_attention_adopts_before_cleanup_retry() {
    let Some(root) =
        isolated("installed_startup_with_uncommitted_attention_adopts_before_cleanup_retry")
    else {
        return;
    };
    let _helper = super::retained_actor_tests::retained_helper_executable();
    let provider = root.join("bin/claude");
    super::retained_actor_tests::install_provider(&provider);
    let original = fs::read_to_string(&provider).unwrap();
    let initialize = "with (root/'spawns').open('a') as log: log.write('spawn\\n')\nsend({'type':'result','uuid':'startup-attention','subtype':'success','is_error':False,'session_id':session})\nfor line in sys.stdin:";
    assert!(original.contains("for line in sys.stdin:"));
    fs::write(
        &provider,
        original.replacen("for line in sys.stdin:", initialize, 1),
    )
    .unwrap();
    let (host, proof) = setup(&root, "post-install-admission");
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let mut requested = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    requested.proof = Some(proof.clone());
    let fault = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    fault.execute_batch("CREATE TRIGGER fail_initial_actor_ownership BEFORE INSERT ON native_actor_ownerships BEGIN SELECT RAISE(ABORT, 'fixture initial ownership failure'); END;").unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            cleanup_discoveries(&self.0);
        }
    }
    let _cleanup = Cleanup(root.join("run"));
    let failure = start(&host, requested.clone()).unwrap_err();
    assert!(
        failure
            .message
            .contains("fixture initial ownership failure"),
        "{}",
        failure.message
    );
    let pending = host.lock().unwrap().native_actor_startups[&requested.id].clone();
    let directory = holders::directory(Kind::Native).unwrap();
    let owner =
        holders::recovery_candidate(&holders::socket(&directory, requested.id), Kind::Native)
            .unwrap()
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let before = loop {
        let inspection = Handle::inspect_remote(&owner, proof.clone()).unwrap();
        if inspection.state.outbox_latest > inspection.state.outbox_ack {
            break inspection;
        }
        assert!(
            Instant::now() < deadline,
            "installed helper never retained startup attention"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(before.state.provider_process.as_ref().unwrap().is_current());
    assert!(host.lock().unwrap().chats.is_empty());
    fault
        .execute_batch("DROP TRIGGER fail_initial_actor_ownership;")
        .unwrap();
    let response = start(&host, requested.clone()).unwrap();
    let ServerResponse::ChatAttached { session, .. } = response else {
        panic!("retry did not attach");
    };
    assert_eq!(session.id, requested.id);
    let handle = host.lock().unwrap().chats[&requested.id].handle.clone();
    assert_eq!(handle.actor_identity().unwrap(), before.identity);
    assert_eq!(handle.provider_process(), before.state.provider_process);
    assert_eq!(
        fs::read_to_string(root.join("spawns"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(
        !root.join("prompts").exists(),
        "recovery must not invent or replay a prompt"
    );
    assert!(host.lock().unwrap().native_actor_startups.is_empty());
    assert_eq!(pending.requested, requested);
    assert_eq!(pending.source.id, requested.id);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspection = Handle::inspect_remote(&owner, proof.clone()).unwrap();
        if inspection.state.outbox_ack >= before.state.outbox_latest {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "adopted helper attention was not committed and acknowledged"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_cursor_for_test(before.identity.instance)
            .unwrap()
            .is_some()
    );
    super::native_cleanup::close_retained(&host, &session).unwrap();
    assert!(!owner.root.is_current() && !owner.holder.is_current());
}
