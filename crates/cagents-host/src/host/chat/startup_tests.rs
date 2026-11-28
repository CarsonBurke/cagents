use super::*;
use crate::chat::protocol::{ExecutionContext, LaunchCleanup, LaunchOutcome};
use crate::model::{HostIncarnation, HostSessionId, LaunchRecipe};
use std::{
    cell::RefCell,
    os::unix::fs::PermissionsExt,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct StartupFailure {
    provider: Mutex<Option<Handle>>,
    attempts: AtomicUsize,
    ready: AtomicBool,
}

impl LaunchCleanup for StartupFailure {
    fn cleanup(&self) -> Result<LaunchOutcome, String> {
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        if self.ready.load(Ordering::Relaxed) {
            self.provider.lock().unwrap().as_ref().unwrap().cleanup()
        } else if attempt == 0 {
            Ok(LaunchOutcome::Uncertain)
        } else {
            Err("fixture cleanup has not settled".into())
        }
    }
}

thread_local! {
    static FAIL_AFTER_SPAWN: RefCell<Option<Arc<StartupFailure>>> = const { RefCell::new(None) };
}

pub(super) fn fail_managed_start_after_spawn(
    execution: &Option<Arc<ExecutionContext>>,
    handle: Result<Handle, String>,
) -> Result<Handle, String> {
    let Some(failure) = FAIL_AFTER_SPAWN.take() else {
        return handle;
    };
    let handle = handle?;
    assert!(
        handle.provider_process().is_some(),
        "fixture must spawn a provider"
    );
    *failure.provider.lock().unwrap() = Some(handle);
    execution.as_ref().unwrap().claim_owner(failure);
    Err("fixture startup failed after spawning".into())
}

#[test]
fn managed_pane_retries_the_original_cleanup_before_replacing_its_provider() {
    use super::super::agent_execution;
    use crate::agent_execution::{HostedSession, Origin};

    if agent_execution::tests::isolated(
        "host::chat::startup_tests::managed_pane_retries_the_original_cleanup_before_replacing_its_provider",
    )
    .is_some()
    {
        return;
    }
    let (host, request, path) = agent_execution::tests::fixture();
    let registry = agent_execution::registry(&host).unwrap();
    let agent = registry
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, 0)
        .unwrap()
        .agent;
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        agent.spec.target.directory,
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(host.lock().unwrap().proof().clone());
    source.launch.recipe.program = path
        .parent()
        .unwrap()
        .join("bin/claude")
        .to_str()
        .unwrap()
        .into();
    registry
        .lock()
        .unwrap()
        .bind_agent(agent.id, None, HostedSession::Chat(source.clone()))
        .unwrap();
    let start = || {
        start(
            &host,
            1,
            source.clone(),
            None,
            false,
            None,
            Default::default(),
        )
    };
    let failure = Arc::new(StartupFailure::default());
    FAIL_AFTER_SPAWN.set(Some(failure.clone()));
    assert!(
        start()
            .unwrap_err()
            .message
            .contains("fixture startup failed after spawning")
    );
    assert!(
        start()
            .unwrap_err()
            .message
            .contains("fixture cleanup has not settled")
    );
    assert_eq!(failure.attempts.load(Ordering::Relaxed), 2);
    let original = failure.provider.lock().unwrap().as_ref().unwrap().clone();
    assert!(!original.is_completed());
    assert!(!host.lock().unwrap().chats.contains_key(&source.id));

    failure.ready.store(true, Ordering::Relaxed);
    assert!(matches!(
        start().unwrap(),
        ServerResponse::ChatAttached { .. }
    ));
    assert!(original.is_completed());
    assert_eq!(failure.attempts.load(Ordering::Relaxed), 3);
    let state = host.lock().unwrap();
    assert!(state.native_actor_startups.is_empty());
    let replacement = state.chats[&source.id].handle.clone();
    assert_ne!(original.provider_process(), replacement.provider_process());
    drop(state);
    replacement.close().unwrap();
}

#[test]
fn managed_codex_start_releases_bundle_lease_before_provider_spawn() {
    const CHILD: &str = "CAGENTS_MANAGED_CODEX_START_TEST";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::chat::startup_tests::managed_codex_start_releases_bundle_lease_before_provider_spawn",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("HOME", directory.path())
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("TMPDIR", directory.path())
            .env("PATH", "/usr/bin:/bin")
            .spawn()
            .unwrap();
        // A nested flock waits forever. Keep the regression bounded and reap
        // its isolated process so it cannot strand an account lock in the suite.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "managed Codex startup fixture failed");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("managed Codex startup blocked on the account lifecycle lock");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    let homes = crate::account::home::Homes::installation().unwrap();
    let store = crate::account::registry::Store::installation().unwrap();
    let (_, home) = store
        .create_pending(&homes, Provider::Codex, 1, || {
            homes.create(Provider::Codex, None)
        })
        .unwrap();
    let directory = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    // Pass executable validation, but fail exec in the holder because the
    // interpreter is absent. This reaches both account leases without a CLI.
    let program = directory.join("codex");
    std::fs::write(&program, "#!/cagents-test-missing-interpreter\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let session = Session {
        id: TerminalId::new(),
        native_continuity: Default::default(),
        owned_codex_listener: None,
        proof: None,
        terminal: None,
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: program.to_str().unwrap().into(),
                arguments: vec!["--remote".into(), "unix://".into()],
                working_directory: directory.clone(),
                environment: BTreeMap::from([("CODEX_HOME".into(), home.to_str().unwrap().into())]),
                unset_environment: Default::default(),
            },
            session_id: None,
            draft_model: None,
        },
    };
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        HostSessionProof {
            host_session_id: HostSessionId::new("managed-start"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("incarnation"),
        },
    )));
    let failure = start(&host, 1, session, None, false, None, Default::default()).unwrap_err();
    assert_eq!(
        failure.message,
        format!(
            "Native holder startup failed: {}",
            std::io::Error::from_raw_os_error(libc::ENOENT)
        ),
        "startup must reach the holder's provider-spawn attempt"
    );
    assert!(directory.join(".agents/skills/cagents/SKILL.md").is_file());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    // A failed native start must also leave the global lease available for
    // ordinary terminal starts and account operations.
    let environment = BTreeMap::from([("CODEX_HOME".into(), home.to_str().unwrap().into())]);
    assert!(
        crate::account::registry::authorize_spawn(&environment, None)
            .unwrap()
            .is_some()
    );
}
