use super::*;
use crate::model::{HostIncarnation, HostSessionId, LaunchRecipe};
use std::os::unix::fs::PermissionsExt;

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
        },
    };
    let host = Arc::new(Mutex::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("managed-start"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("incarnation"),
    })));
    let failure = start(&host, 1, session, None, false, None, Default::default()).unwrap_err();
    assert_eq!(
        failure.message, "Native holder did not acknowledge its server",
        "startup must reach the holder's provider-spawn attempt"
    );
    assert!(
        directory
            .join(".agents/skills/cagents-agents/SKILL.md")
            .is_file()
    );
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
