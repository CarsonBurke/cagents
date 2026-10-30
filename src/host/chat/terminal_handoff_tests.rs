//! Private real-holder fixtures. Inherited caller authority is never cleared or
//! replaced; direct HOST calls here do not qualify control-envelope admission.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

pub(super) struct PrivateHost {
    pub(super) host: SharedHost,
    _lease: crate::daemon::HostLease,
}

/// Reexecute exactly the current parent with private storage and an actual
/// same-boot lease. The surrounding qualification runner owns the full family;
/// this helper waits its original child and never signals or cleans strangers.
pub(super) fn isolated() -> Option<PrivateHost> {
    const CASE: &str = "CAGENTS_HANDOFF_FIXTURE_CASE";
    const ROOT: &str = "CAGENTS_HANDOFF_FIXTURE_ROOT";
    const NONCE: &str = "CAGENTS_HANDOFF_FIXTURE_NONCE";
    let name = thread::current()
        .name()
        .expect("named test parent")
        .to_owned();
    let root = if let Some(case) = std::env::var_os(CASE) {
        assert_eq!(
            case.to_str(),
            Some(name.as_str()),
            "foreign self-child selector"
        );
        let root = PathBuf::from(std::env::var_os(ROOT).expect("private fixture root"));
        let metadata = fs::symlink_metadata(&root).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(root.canonicalize().unwrap(), root);
        assert!(
            root.parent()
                .is_some_and(|parent| parent == std::path::Path::new("/var/tmp"))
        );
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("cagents-handoff-")
        );
        for (key, leaf) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "run"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("TMPDIR", "tmp"),
            ("CLAUDE_CONFIG_DIR", "home/.claude"),
            ("CODEX_HOME", "home/.codex"),
        ] {
            assert_eq!(
                std::env::var_os(key),
                Some(root.join(leaf).into_os_string()),
                "private path binding {key}"
            );
        }
        let nonce = std::env::var(NONCE).expect("issued self-child nonce");
        assert!(!uuid::Uuid::parse_str(&nonce).unwrap().is_nil());
        let entry = serde_json::json!({"selector":name,"nonce":nonce,
            "pid":std::process::id(),"parent":unsafe{libc::getppid()}});
        fs::write(
            root.join("entered.json"),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
        root
    } else {
        assert!(
            std::env::var_os(ROOT).is_none() && std::env::var_os(NONCE).is_none(),
            "inherited fixture operands refused"
        );
        let mut directory = tempfile::Builder::new()
            .prefix("cagents-handoff-")
            .tempdir_in("/var/tmp")
            .unwrap();
        // Keep exact private evidence even on failure. Removing an unproved
        // family namespace is not cleanup, and no user directory is touched.
        directory.disable_cleanup(true);
        let root = directory.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        for leaf in ["home", "run", "data", "config", "cache", "state", "tmp"] {
            fs::create_dir(root.join(leaf)).unwrap();
            fs::set_permissions(root.join(leaf), fs::Permissions::from_mode(0o700)).unwrap();
        }
        for leaf in ["home/.claude", "home/.codex"] {
            fs::create_dir(root.join(leaf)).unwrap();
            fs::set_permissions(root.join(leaf), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let nonce = uuid::Uuid::new_v4().to_string();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--test-threads=1", "--color=never"])
            .env(CASE, &name)
            .env(ROOT, root)
            .env(NONCE, &nonce)
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("TMPDIR", root.join("tmp"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("CODEX_HOME", root.join("home/.codex"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let original_child = child.id();
        let output = child.wait_with_output().unwrap();
        fs::write(root.join("stdout.log"), &output.stdout).unwrap();
        fs::write(root.join("stderr.log"), &output.stderr).unwrap();
        assert!(
            output.status.success(),
            "private self-child failed: {}\nstdout={}\nstderr={}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout
                .lines()
                .filter(|line| *line == "running 1 test")
                .count(),
            1
        );
        assert_eq!(
            stdout
                .lines()
                .filter(|line| *line == format!("test {name} ... ok"))
                .count(),
            1
        );
        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
                .count(),
            1
        );
        let entry: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("entered.json")).unwrap()).unwrap();
        assert_eq!(entry["selector"], name);
        assert_eq!(entry["nonce"], nonce);
        assert_eq!(entry["parent"], std::process::id());
        assert_eq!(entry["pid"], original_child);
        return None;
    };
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof);
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    Some(PrivateHost {
        host: Arc::new(HostLock::new(state)),
        _lease: lease,
    })
}

#[test]
fn returned_terminal_ack_keeps_real_retirement_proof_and_close_retry() {
    use crate::chat::protocol::{Adapter, Event, ProcessSpec, Step};
    let Some(fixture) = isolated() else {
        return;
    };
    let host = fixture.host.clone();
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("claude");
    fs::write(&cli, "#!/bin/sh\nexec cat\n").unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let proof = host.lock().unwrap().proof().clone();
    let chat_id = TerminalId::new();
    let conversation = "12345678-1234-4234-8234-123456789abc";
    let source = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: chat_id,
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: cli.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
            session_id: Some(conversation.into()),
        },
        proof: Some(proof.clone()),
        terminal: None,
    };
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec::from(&source.launch.recipe);
    let mut initial = Step::default();
    initial.events.push(Event::Session {
        id: conversation.into(),
        title: None,
    });
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(
        spec,
        Box::new(Quiet),
        initial,
        store,
        Some(conversation.into()),
        |_| {},
    )
    .unwrap();
    host.lock().unwrap().register_client(1);
    host.lock().unwrap().chats.insert(
        chat_id,
        Record {
            execution_instance: uuid::Uuid::new_v4(),
            console_recipe: None,
            paired_terminal: None,
            session: source.clone(),
            handle: handle.clone(),
            source_process: None,
            subscribers: BTreeMap::from([(1, false)]),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    let ServerResponse::ChatTerminal {
        session: terminal, ..
    } = process(&host, 1, ClientRequest::ChatReturnTerminal { chat_id }).unwrap()
    else {
        panic!("real terminal handoff was not published")
    };
    assert!(handle.is_completed());
    let worker = host.lock().unwrap().terminals[&terminal.id].handle.clone();
    let original = worker
        .holder_identity()
        .expect("actual registered original holder")
        .clone();
    assert_eq!(original.terminal, terminal.id);
    // ACK may arrive from another window's ordinary hydration/save before
    // that actual T is closed. It must preserve the original A recovery route.
    process(
        &host,
        2,
        ClientRequest::ChatCommitTerminal {
            terminal_id: terminal.id,
        },
    )
    .unwrap();
    assert!(host.lock().unwrap().chat_handoffs.contains_key(&chat_id));
    assert!(matches!(
        process(
            &host,
            3,
            ClientRequest::ChatAttach {
                session: source.clone()
            }
        )
        .unwrap(),
        ServerResponse::ChatAttached { .. }
    ));
    assert!(process(&host, 3, ClientRequest::ChatClose { chat_id }).is_err());
    // Exercise the genuine existing release operation and its private on-disk
    // receipt. No test creates a .closed file or substitutes absence for proof.
    worker.close_with_receipt(&terminal, &proof).unwrap();
    assert!(worker.is_completed());
    assert!(
        crate::terminal::holder::cleanup::confirmed_with_role(&terminal, &original, &proof, None)
            .unwrap()
    );
    host.lock().unwrap().retire_terminal(terminal.id);
    for _ in 0..2 {
        assert!(
            matches!(process(&host, 3, ClientRequest::ChatClose { chat_id }).unwrap(), ServerResponse::ChatClosed { chat_id: closed } if closed == chat_id)
        );
    }
    assert!(process(&host, 3, ClientRequest::ChatReturnTerminal { chat_id }).is_err());
    // A reconnect after a lost response still gets the same close-only route.
    assert!(matches!(
        process(&host, 4, ClientRequest::ChatAttach { session: source }).unwrap(),
        ServerResponse::ChatAttached { .. }
    ));
    assert!(matches!(
        process(&host, 4, ClientRequest::ChatClose { chat_id }).unwrap(),
        ServerResponse::ChatClosed { .. }
    ));
    let state = host.lock().unwrap();
    let receipt = &state.chat_handoffs[&chat_id];
    assert!(receipt.terminal_retired && !receipt.pending());
    assert!(state.chats.is_empty() && state.terminals.is_empty());
}
