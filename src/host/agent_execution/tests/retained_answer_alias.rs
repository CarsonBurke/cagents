//! A person's answer must reach a real retained provider. The answer path asks
//! the helper to recognize the daemon's descriptor, so the two must agree.
use super::*;

const MODE: &str = "CAGENTS_RETAINED_ANSWER_ALIAS_TEST";

fn run_isolated(test: &str, surface: &str) {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path();
    for path in [
        "bin",
        "home/.claude",
        "data",
        "config",
        "cache",
        "tmp",
        "run",
        "project",
    ] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    install_claude(&root.join("bin/claude"));
    let helper = std::path::PathBuf::from(
        std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            .expect("this regression requires the same-head cagentsd executable"),
    );
    assert!(helper.is_absolute());
    // The console's matched CLI is found beside the running executable, so this
    // run links the test binary into a directory of its own. Other tests rely on
    // that CLI being absent beside the shared test binary.
    let exe = std::env::current_exe().unwrap();
    let products = tempfile::tempdir_in(exe.parent().unwrap()).unwrap();
    let linked = products.path().join(exe.file_name().unwrap());
    std::fs::hard_link(&exe, &linked).unwrap();
    let cli = products.path().join("cagents");
    std::fs::write(
        &cli,
        "#!/bin/sh\nwhile [ ! -e \"$HOME/../end-console\" ]; do sleep 0.05; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::hard_link(&helper, products.path().join("cagentsd")).unwrap();
    let output = std::process::Command::new(&linked)
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env(MODE, surface)
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_TEST_RETAINED_ACTOR_HELPER", helper)
        .env("CAGENTS_EXECUTION_FIXTURE", root)
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .output()
        .unwrap();
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "isolated answer fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Has the provider name its conversation as soon as it initializes, so the
/// identity is confirmed before the daemon pairs the agent's console.
fn confirm_identity_at_start() {
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap());
    let provider = root.join("bin/claude");
    let script = std::fs::read_to_string(&provider).unwrap();
    let response = r#""request_id":"%s","response":{}}}\n' "$id""#;
    assert_eq!(script.matches(response).count(), 1);
    let script = script.replace(
        response,
        &format!(
            "{response}\n      printf '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"%s\",\"uuid\":\"early-init\"}}\\n' \"$session\""
        ),
    );
    std::fs::write(&provider, script).unwrap();
}

#[derive(Clone, Copy, PartialEq)]
enum Restart {
    Never,
    /// Adopted by a new daemon, then asks.
    BeforeAsking,
    /// Asks, then a new daemon adopts the helper with the request pending.
    WhileAsking,
    /// Both, as two successive daemons.
    Twice,
}

struct Daemon {
    host: SharedHost,
    lease: crate::daemon::HostLease,
    _notifications: crate::host::notifications::Broker,
}

fn serve(host: SharedHost) -> Daemon {
    let proof = host.lock().unwrap().proof().clone();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let notifications = crate::host::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    Daemon {
        host,
        lease,
        _notifications: notifications,
    }
}

/// Replaces the daemon the way a restart does: the helper and console holder
/// survive, and a new incarnation runs the production startup order.
fn restart(daemon: Daemon, source: crate::model::TerminalId, generation: usize) -> Daemon {
    let registry_path =
        std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap())
            .join("registry.sqlite3");
    let boot = {
        let state = daemon.host.lock().unwrap();
        if let Some(console) = state.chats[&source].paired_terminal.as_ref() {
            console.preserve_for_restart().unwrap();
        }
        state.proof().boot_id.clone()
    };
    drop(daemon);
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("execution-fixture"),
        boot_id: boot,
        host_incarnation: HostIncarnation::new(format!("successor-{generation}")),
    });
    state.install_model(Registry::at(&registry_path).unwrap());
    state.enable_holder_recovery();
    let daemon = serve(Arc::new(crate::host::state::HostLock::new(state)));
    crate::host::state::restore_orphaned_holders(&daemon.host).unwrap();
    crate::host::chat::restore_orphaned_native(&daemon.host, &[]).unwrap();
    crate::host::chat::retained_actor::reconcile_retired_actors(&daemon.host).unwrap();
    crate::host::chat::retained_actor::reconcile_startup_losses(&daemon.host).unwrap();
    daemon
}

fn field_differences(left: &chat::Session, right: &chat::Session) -> Vec<String> {
    let (serde_json::Value::Object(left), serde_json::Value::Object(right)) = (
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap(),
    ) else {
        unreachable!()
    };
    left.iter()
        .filter(|(name, value)| right.get(*name) != Some(value))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Which of the two descriptors the answer path presents the helper knows, and
/// how each differs from the helper's current source.
fn alias_report(daemon: &Daemon, id: crate::model::TerminalId) -> String {
    let state = daemon.host.lock().unwrap();
    let record = &state.chats[&id];
    let session = record.session.clone();
    let proof = state.proof().clone();
    let identity = record.handle.actor_identity().unwrap();
    drop(state);
    let owner = registry(&daemon.host)
        .unwrap()
        .lock()
        .unwrap()
        .native_actor_ownership(identity.instance)
        .unwrap()
        .unwrap();
    let helper = chat::runtime::Handle::inspect_remote(&identity.record, proof).unwrap();
    let known = |source: &chat::Session| {
        source == &helper.source || helper.previous_sources.contains(source)
    };
    format!(
        "owner issued={} differs from helper source in {:?}; record issued={} differs in {:?}; {} earlier aliases",
        known(owner.source()),
        field_differences(owner.source(), &helper.source),
        known(&session),
        field_differences(&session, &helper.source),
        helper.previous_sources.len(),
    )
}

fn answer_case(
    surface: Surface,
    later_turn: bool,
    early_identity: bool,
    when: Restart,
    console_ends: bool,
) {
    if early_identity {
        confirm_identity_at_start();
    }
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    host.lock().unwrap().enable_holder_recovery();
    let mut daemon = serve(host);
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.surface = surface;
    let asks_first = !later_turn && matches!(when, Restart::Never | Restart::WhileAsking);
    if asks_first {
        request.prompt = "ASK_PERMISSION before running".into();
    }
    let owner = Authority::default();
    let accepted = submit(&daemon.host, &owner, request).unwrap();
    let agent = accepted.agent.id;
    if asks_first {
        pending_requests(&daemon.host, agent);
    } else {
        // The provider has confirmed its identity by the time it asks.
        assert_eq!(
            settled(&daemon.host, accepted.run.id).state,
            State::Succeeded
        );
    }
    let source = match registry(&daemon.host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap()
        .session
    {
        Some(HostedSession::Chat(source)) => source.id,
        session => panic!("the agent has no native source: {session:?}"),
    };
    let identity = daemon.host.lock().unwrap().chats[&source]
        .handle
        .actor_identity()
        .expect("this regression must execute the real retained helper");
    struct Cleanup(crate::daemon::holder_registry::Record);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for process in [&self.0.root, &self.0.holder] {
                if process.is_current() {
                    unsafe {
                        libc::kill(process.pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup(identity.record.clone());
    if console_ends {
        // The console's process ends on its own, with the provider still running.
        std::fs::write(root.join("end-console"), b"").unwrap();
        let console = daemon.host.lock().unwrap().chats[&source]
            .paired_terminal
            .clone()
            .expect("the agent has a console");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !console.is_completed() {
            assert!(Instant::now() < deadline, "the console never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if matches!(when, Restart::BeforeAsking | Restart::Twice) {
        daemon = restart(daemon, source, 1);
    }
    if !asks_first {
        submit(
            &daemon.host,
            &owner,
            Submit {
                name: None,
                destination: Destination::Existing(agent),
                prompt: "ASK_PERMISSION before running".into(),
                idempotency_key: "second".into(),
            },
        )
        .unwrap();
    }
    let asked = pending_requests(&daemon.host, agent);
    let [asked] = asked.as_slice() else {
        panic!("{asked:?}")
    };
    let request_id = asked.id.clone();
    if matches!(when, Restart::WhileAsking | Restart::Twice) {
        daemon = restart(daemon, source, 2);
        let preserved = pending_requests(&daemon.host, agent);
        assert_eq!(
            preserved.len(),
            1,
            "the pending request must survive adoption"
        );
        assert_eq!(preserved[0].id, request_id);
    }
    let (handle, record_session) = {
        let state = daemon.host.lock().unwrap();
        let record = &state.chats[&source];
        (record.handle.clone(), record.session.clone())
    };
    assert_eq!(handle.actor_identity(), Some(identity.clone()));

    // A descriptor the helper never issued stays refused, and refusing it
    // leaves the request pending.
    let mut forged = record_session.clone();
    forged.launch.recipe.arguments.push("--forged".into());
    assert_eq!(
        handle.recognize_retained_sources(&[forged]).unwrap_err(),
        "Retained execution source is not an exact issued alias"
    );
    assert_eq!(pending_requests(&daemon.host, agent).len(), 1);
    assert!(!root.join("decisions").exists());

    let answer = || {
        process(
            &daemon.host,
            owner.clone(),
            Operation::Control {
                agent,
                command: Control::Answer {
                    request_id: request_id.clone(),
                    action: "deny".into(),
                    answers: Vec::new(),
                },
            },
        )
    };
    let remaining = answer().unwrap_or_else(|error| {
        assert!(
            !root.join("decisions").exists(),
            "a refused answer must not reach the provider"
        );
        assert_eq!(pending_requests(&daemon.host, agent).len(), 1);
        panic!("{error}\nALIAS {}", alias_report(&daemon, source))
    });
    assert!(matches!(remaining, Reply::Requests(requests) if requests.is_empty()));
    eprintln!(
        "ALIAS confirmed={:?} record={:?} pair={} {}",
        handle.confirmed_session_id(),
        record_session.launch.session_id,
        record_session.terminal.is_some(),
        alias_report(&daemon, source)
    );
    // A repeat finds nothing to answer, and the provider heard exactly one.
    let repeated = answer().unwrap_err();
    assert!(repeated.contains("No pending request"), "{repeated}");
    assert_eq!(
        std::fs::read_to_string(root.join("decisions")).unwrap(),
        "deny\n"
    );
    let _ = daemon.lease;
}

macro_rules! answer_cases {
    ($($name:ident: $surface:expr, $mode:literal, $later:expr, $early:expr, $when:expr, $ends:expr;)+) => {$(
        #[test]
        fn $name() {
            if std::env::var_os(MODE).is_none() {
                return run_isolated(
                    concat!(
                        "host::agent_execution::tests::retained_answer_alias::",
                        stringify!($name)
                    ),
                    $mode,
                );
            }
            answer_case($surface, $later, $early, $when, $ends);
        }
    )+};
}

answer_cases! {
    chat_first_turn: Surface::Chat, "chat", false, false, Restart::Never, false;
    chat_later_turn: Surface::Chat, "chat", true, false, Restart::Never, false;
    chat_identity_at_start: Surface::Chat, "chat", false, true, Restart::Never, false;
    chat_adopted_then_asks: Surface::Chat, "chat", true, false, Restart::BeforeAsking, false;
    chat_asks_then_adopted: Surface::Chat, "chat", false, false, Restart::WhileAsking, false;
    chat_later_turn_asks_then_adopted: Surface::Chat, "chat", true, false, Restart::WhileAsking, false;
    chat_adopted_twice: Surface::Chat, "chat", true, false, Restart::Twice, false;
    console_first_turn: Surface::Terminal, "terminal", false, false, Restart::Never, false;
    console_later_turn: Surface::Terminal, "terminal", true, false, Restart::Never, false;
    console_identity_at_start: Surface::Terminal, "terminal", false, true, Restart::Never, false;
    console_adopted_then_asks: Surface::Terminal, "terminal", true, false, Restart::BeforeAsking, false;
    console_asks_then_adopted: Surface::Terminal, "terminal", false, false, Restart::WhileAsking, false;
    console_later_turn_asks_then_adopted: Surface::Terminal, "terminal", true, false, Restart::WhileAsking, false;
    console_adopted_twice: Surface::Terminal, "terminal", true, false, Restart::Twice, false;
    console_ended: Surface::Terminal, "terminal", true, false, Restart::Never, true;
}
