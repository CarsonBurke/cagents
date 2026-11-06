//! Automatic retirement must exercise the production retained Claude owner.
use super::*;

const TEST: &str = "host::agent_execution::tests::retained_automation::completed_retained_automation_retires_only_after_exact_idle_close";
const MODE: &str = "CAGENTS_RETAINED_AUTOMATION_TEST";
const SCOPE: &str = "CAGENTS_RETAINED_AUTOMATION_AUTHORITY_PRESENCE";

fn wait_until(mut condition: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "{reason}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn completed_retained_automation_retires_only_after_exact_idle_close() {
    if std::env::var(MODE).as_deref() != Ok("fixture") {
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
                .expect("retained automation requires the same-head cagentsd executable"),
        );
        assert!(helper.is_absolute());
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        // Keep opaque delegation and the selected helper by normal inheritance;
        // every remaining input belongs to this private fixture.
        for (name, _) in std::env::vars_os() {
            if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
                && name != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            {
                command.env_remove(name);
            }
        }
        let output = command
            .args(["--exact", TEST, "--nocapture"])
            .env(MODE, "fixture")
            .env(
                SCOPE,
                if Authority::from_environment() != Authority::default() {
                    "present"
                } else {
                    "absent"
                },
            )
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
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
        assert!(
            output.status.success(),
            "isolated retained automation failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let inherited = match std::env::var(SCOPE).as_deref() {
        Ok("present") => true,
        Ok("absent") => false,
        _ => panic!("retained automation fixture lost its authority presence marker"),
    };
    assert_eq!(
        Authority::from_environment() != Authority::default(),
        inherited,
        "retained automation child lost its inherited authority scope"
    );
    let (host, request, path) = fixture();
    if inherited {
        assert!(
            matches!(
                process(&host, Authority::from_environment(), Operation::List),
                Err(error) if error == "This agent session authority is no longer registered"
            ),
            "an unrelated inherited delegation must not acquire this private registry's authority"
        );
    }
    host.lock().unwrap().native_chat_capacity = 1;
    let proof = host.lock().unwrap().proof().clone();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let notifications = crate::host::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let automation = automation_fixture(&host, &request);
    let accepted = claim_fixture(&host, &automation);
    assert_eq!(
        accepted.agent.origin,
        Origin::Automation { id: automation.id }
    );
    dispatch_accepted(&host, accepted.clone()).unwrap();
    let done = settled(&host, accepted.run.id);
    assert_eq!(done.state, State::Succeeded);
    let HostedSession::Chat(source) = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap()
    else {
        panic!("automation did not establish its native source");
    };
    let handle = host.lock().unwrap().chats[&source.id].handle.clone();
    let identity = handle
        .actor_identity()
        .expect("this regression must execute the real retained helper");
    struct Cleanup {
        host: SharedHost,
        id: crate::model::TerminalId,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if self.host.lock().unwrap().chats.contains_key(&self.id) {
                if let Err(error) = crate::host::chat::process(
                    &self.host,
                    EXECUTION_CLIENT,
                    ClientRequest::ChatClose { chat_id: self.id },
                ) {
                    eprintln!(
                        "retained automation fixture cleanup failed: {}",
                        error.message
                    );
                }
            }
        }
    }
    let _cleanup = Cleanup {
        host: host.clone(),
        id: source.id,
    };
    let capacity = || crate::host::chat::capacity::status(&host);
    assert_eq!(capacity().used, 1);
    assert!(identity.record.holder.is_current());
    assert!(identity.record.root.is_current());
    {
        use std::os::unix::fs::MetadataExt;
        let selected = std::fs::metadata(
            std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
                .expect("retained fixture lost its matched helper"),
        )
        .unwrap();
        let running =
            std::fs::metadata(format!("/proc/{}/exe", identity.record.holder.pid)).unwrap();
        assert_eq!(
            (running.dev(), running.ino()),
            (selected.dev(), selected.ino()),
            "retained holder must execute the selected matched helper"
        );
        assert!(identity.record.holder.is_current());
    }

    let idle = handle.snapshot(None).unwrap();
    assert!(idle.ready && !idle.busy && !idle.waiting && idle.requests.is_empty());
    assert!(
        !handle
            .try_close_if_idle(idle.generation.wrapping_add(1))
            .unwrap(),
        "a typed stale-generation response must retain this exact idle owner",
    );
    assert!(!handle.is_completed());
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert!(identity.record.root.is_current());
    assert_eq!(capacity().used, 1);
    assert_eq!(
        reconcile_agent_run(&host, &done, false).unwrap(),
        Reconciliation::Idle
    );

    // Provider work can arrive between the scheduler's completed run and its
    // cleanup decision. This is deliberately not an interactive takeover: it
    // checks the provider's own atomic busy refusal and reconciliation fence.
    handle
        .command(Command::Prompt {
            text: "HOLD_CLEANUP_BUSY".into(),
            mode: PromptMode::Send,
            attachments: Vec::new(),
        })
        .unwrap();
    let root = path.parent().unwrap();
    wait_until(
        || root.join("cleanup-busy").exists(),
        "private provider did not enter its busy gate",
    );
    wait_until(
        || {
            handle
                .snapshot(None)
                .is_some_and(|snapshot| snapshot.busy && !snapshot.waiting)
        },
        "retained busy state was not published",
    );
    assert!(
        handle
            .try_close_if_idle(handle.snapshot(None).unwrap().generation)
            .is_err()
    );
    assert_eq!(
        reconcile_agent_run(&host, &done, true).unwrap(),
        Reconciliation::Busy
    );
    assert!(!handle.is_completed());
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    assert!(identity.record.holder.is_current());
    assert!(identity.record.root.is_current());
    assert_eq!(capacity().used, 1);
    std::fs::write(root.join("cleanup-release"), b"continue").unwrap();
    wait_until(
        || {
            handle.snapshot(None).is_some_and(|snapshot| {
                snapshot.ready
                    && !snapshot.busy
                    && !snapshot.waiting
                    && snapshot.requests.is_empty()
            })
        },
        "private retained provider did not settle",
    );

    assert_eq!(
        reconcile_agent_run(&host, &done, true).unwrap(),
        Reconciliation::Closed
    );
    assert_eq!(
        reconcile_agent_run(&host, &done, true).unwrap(),
        Reconciliation::Closed
    );
    assert!(handle.is_completed());
    assert!(!identity.record.root.is_current());
    assert!(!identity.record.holder.is_current());
    let registry = registry(&host).unwrap();
    let registry = registry.lock().unwrap();
    assert!(registry.agent(accepted.agent.id).unwrap().closed);
    assert_eq!(registry.agent_run(done.id).unwrap(), done);
    let retirement = registry
        .native_actor_retirement(identity.instance)
        .unwrap()
        .expect("automatic Close must durably retire its exact helper");
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(retirement.source().id, source.id);
    drop(registry);
    assert_eq!(capacity().used, 0);
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    assert!(host.lock().unwrap().native_closing.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}
