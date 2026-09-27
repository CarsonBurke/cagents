use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Operation, Reply, Target},
    host::state::HostState,
    model::{HostIncarnation, HostSessionId, HostSessionProof, ProjectKind},
    workspace_model::ModelEdit,
};
use std::{os::unix::fs::PermissionsExt, path::Path};

// Environment isolation also proves the service does not depend on any user's
// account, project, provider installation, or running daemon.
fn isolated(name: &str) -> Option<tempfile::TempDir> {
    const CHILD: &str = "CAGENTS_EXECUTION_SERVICE_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return None;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for path in [
        "bin",
        "home/.claude",
        "home/.codex",
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
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("host::agent_execution::tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, name)
        .env("CAGENTS_EXECUTION_FIXTURE", root)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .status()
        .unwrap();
    assert!(result.success(), "isolated service fixture failed");
    Some(directory)
}

fn install_claude(path: &Path) {
    std::fs::write(path, r#"#!/bin/sh
set -eu
printf 'spawn\n' >> "$HOME/../spawns"
printf '%s\n' "$@" > "$HOME/../claude-arguments"
n=0
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) shift; session=$1 ;;
    --session-id=*|--resume=*) session=${1#*=} ;;
  esac
  shift
done
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}
      id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      printf 'prompt\n' >> "$HOME/../prompts"
      printf '{"type":"assistant","uuid":"message-%s","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"SERVICE_RESULT_%s"}],"usage":{"input_tokens":7,"output_tokens":3}}}\n' "$n" "$n" "$n"
      printf '{"type":"result","uuid":"result-%s","subtype":"success","is_error":false,"session_id":"%s","usage":{"input_tokens":7,"output_tokens":3}}\n' "$n" "$session"
      ;;
  esac
done
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn uncertain_private_startup_can_retry_exact_cleanup_without_losing_capacity() {
    if isolated("uncertain_private_startup_can_retry_exact_cleanup_without_losing_capacity")
        .is_some()
    {
        return;
    }
    use chat::protocol::{ExecutionContext, LaunchCleanup, LaunchOutcome};
    struct Owner(Mutex<LaunchOutcome>);
    impl LaunchCleanup for Owner {
        fn cleanup(&self) -> Result<LaunchOutcome, String> {
            Ok(*self.0.lock().unwrap())
        }
    }
    let (host, request, _) = fixture();
    let registry = registry(&host).unwrap();
    let accepted = registry
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    let mut session = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        accepted.agent.spec.target.directory.clone(),
        &Account::system(Provider::Claude),
    )
    .unwrap();
    session.proof = Some(host.lock().unwrap().proof().clone());
    let source = HostedSession::Chat(session);
    registry
        .lock()
        .unwrap()
        .reserve_agent_launch(accepted.agent.id, &source)
        .unwrap();
    let owner = Arc::new(Owner(Mutex::new(LaunchOutcome::Uncertain)));
    let context = Arc::new(ExecutionContext::default());
    context.claim_owner(owner.clone());
    host.lock()
        .unwrap()
        .execution_startups
        .insert(accepted.agent.id, (source.clone(), context));
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap_err()
            .contains("unconfirmed")
    );
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap(),
        Some(source)
    );
    assert!(
        !registry
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    *owner.0.lock().unwrap() = LaunchOutcome::TeardownProved;
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(host.lock().unwrap().execution_startups.is_empty());
}

#[test]
fn omp_service_waits_for_correlated_result_and_reuses_one_provider() {
    if isolated("omp_service_waits_for_correlated_result_and_reuses_one_provider").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap();
    let program = root.join("bin/omp");
    std::fs::write(&program, r#"#!/bin/sh
set -eu
printf 'spawn\n' >> "$HOME/../omp-spawns"
printf '%s\n' "$@" > "$HOME/../omp-arguments"
printf '{"type":"ready"}\n'
n=0
while IFS= read -r frame; do
  id=${frame#*'"id":"'}; id=${id%%'"'*}
  kind=${frame#*'"type":"'}; kind=${kind%%'"'*}
  data='{}'
  case "$kind" in
    negotiate_protocol) ;;
    get_state) data='{"sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","isStreaming":false}' ;;
    get_available_models) data='{"models":[]}' ;;
    get_available_commands) data='{"commands":[]}' ;;
    get_messages_page) data='{"messages":[],"nextCursor":null}' ;;
    get_session_stats) data='{"tokens":{"input":7,"output":3},"cost":0.0001}' ;;
    prompt)
      n=$((n + 1))
      printf 'prompt\n' >> "$HOME/../omp-prompts"
      printf '{"type":"agent_start"}\n'
      printf '{"type":"message_end","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"OMP_RESULT_%s"}],"stopReason":"stop","timestamp":%s}}\n' "$n" "$n" "$n"
      printf '{"type":"agent_end","isTerminal":false}\n'
      printf '{"type":"agent_end","isTerminal":true}\n'
      printf 'settled\n' > "$HOME/../omp-before-result"
      while [ ! -e "$HOME/../omp-release-result" ]; do /bin/sleep 0.01; done
      printf '{"type":"prompt_result","id":"%s","status":"completed","agentInvoked":true}\n' "$id"
      printf '{"type":"agent_end","isTerminal":true}\n'
      ;;
    abort) ;;
    *) exit 17 ;;
  esac
  printf '{"type":"response","id":"%s","command":"%s","success":true,"data":%s}\n' "$id" "$kind" "$data"
done
"#).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.provider = Provider::Omp;
    spec.model = Some("fixture/model".into());
    spec.permission_mode = Some("write".into());
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("omp-before-result").exists() {
        assert!(
            Instant::now() < deadline,
            "OMP did not settle its first turn: {:?}",
            registry(&host)
                .unwrap()
                .lock()
                .unwrap()
                .agent_run(accepted.run.id)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let before = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent_run(accepted.run.id)
        .unwrap();
    assert!(
        !before.state.is_terminal(),
        "agent_end is not correlated prompt completion"
    );
    std::fs::write(root.join("omp-release-result"), b"release").unwrap();
    let done = settled(&host, accepted.run.id);
    assert_eq!(done.state, State::Succeeded, "{done:?}");
    assert_eq!(done.result.text, "OMP_RESULT_1");
    assert_eq!(done.result.usage.input_tokens, 7);
    assert_eq!(done.result.usage.output_tokens, 3);
    let followup = submit(
        &host,
        &Authority::default(),
        Submit {
            destination: Destination::Existing(accepted.agent.id),
            prompt: "follow-up".into(),
            idempotency_key: "omp-followup".into(),
        },
    )
    .unwrap();
    let second = settled(&host, followup.run.id);
    assert_eq!(second.state, State::Succeeded);
    assert_eq!(second.result.text, "OMP_RESULT_2");
    assert_ne!(second.provider_key, done.provider_key);
    assert_eq!(
        std::fs::read_to_string(root.join("omp-spawns")).unwrap(),
        "spawn\n"
    );
    let arguments = std::fs::read_to_string(root.join("omp-arguments")).unwrap();
    assert!(arguments.contains("--model\nfixture/model\n"));
    assert!(arguments.contains("--approval-mode\nwrite\n"));
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
}

#[test]
fn automatic_cleanup_is_exact_run_fenced_and_confirms_provider_retirement() {
    if isolated("automatic_cleanup_is_exact_run_fenced_and_confirms_provider_retirement").is_some()
    {
        return;
    }
    let (host, request, _) = fixture();
    let automation = automation_fixture(&host, &request);
    let accepted = claim_fixture(&host, &automation);
    dispatch_accepted(&host, accepted.clone()).unwrap();
    let first = settled(&host, accepted.run.id);
    let session_id = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let handle = host.lock().unwrap().chats[&session_id].handle.clone();
    let stale = handle.snapshot(None).unwrap().generation;
    handle.command(Command::Refresh).unwrap();
    assert!(
        handle
            .close_if_idle(stale)
            .unwrap_err()
            .contains("state changed")
    );
    assert!(!handle.is_completed());
    assert_eq!(
        reconcile_agent_run(&host, &first, false).unwrap(),
        Reconciliation::Idle
    );
    release_fixture(&host, &first);
    let next = claim_fixture(&host, &automation);
    assert_eq!(next.agent.id, accepted.agent.id);
    assert!(
        reconcile_agent_run(&host, &first, true)
            .unwrap_err()
            .contains("newer run")
    );
    dispatch_accepted(&host, next.clone()).unwrap();
    let done = settled(&host, next.run.id);
    assert_eq!(
        reconcile_agent_run(&host, &done, true).unwrap(),
        Reconciliation::Closed
    );
    assert_eq!(
        reconcile_agent_run(&host, &done, true).unwrap(),
        Reconciliation::Closed
    );
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
}

#[test]
fn cancellation_reaches_only_the_exact_active_provider_run() {
    if isolated("cancellation_reaches_only_the_exact_active_provider_run").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    std::fs::write(path.parent().unwrap().join("bin/claude"), r#"#!/bin/sh
set -eu
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      case "$frame" in
        *'"subtype":"interrupt"'*)
          printf 'interrupt\n' >> "$HOME/../interrupts"
          printf '{"type":"result","uuid":"interrupted-result","is_error":false}\n'
          ;;
      esac
      id=${frame#*'"request_id":"'}
      id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
  esac
done
"#).unwrap();
    let automation = automation_fixture(&host, &request);
    let accepted = claim_fixture(&host, &automation);
    dispatch_accepted(&host, accepted.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let active = loop {
        let run = registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap();
        if run.state == State::Running {
            break run;
        }
        assert!(
            Instant::now() < deadline,
            "provider run did not start: {run:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let session_id = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let handle = host.lock().unwrap().chats[&session_id].handle.clone();
    assert!(
        handle
            .interrupt_run(RunId::new())
            .unwrap_err()
            .contains("active turn")
    );
    assert!(!path.parent().unwrap().join("interrupts").exists());
    cancel_agent_run(&host, &active).unwrap();
    let finished = settled(&host, active.id);
    assert_eq!(finished.state, State::Cancelled);
    assert!(cancel_agent_run(&host, &active).is_err());
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("interrupts")).unwrap(),
        "interrupt\n"
    );
    assert_eq!(
        reconcile_agent_run(&host, &finished, true).unwrap(),
        Reconciliation::Closed
    );
}

#[test]
fn interactive_work_claims_automation_agent_before_cleanup_or_reuse() {
    if isolated("interactive_work_claims_automation_agent_before_cleanup_or_reuse").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let Destination::New(spec) = request.destination.clone() else {
        unreachable!()
    };
    let automation = crate::host::automations::service::create(
        &host,
        crate::automations::Definition {
            name: "Interactive takeover fixture".into(),
            prompt: request.prompt.clone(),
            agent: spec,
            schedule: crate::automations::Schedule {
                pattern: crate::automations::SchedulePattern::Daily { hour: 3, minute: 0 },
                timezone: "UTC".into(),
            },
            session_mode: crate::automations::SessionMode::Reuse,
            precheck: None,
        },
    )
    .unwrap();
    let owner = Origin::Automation { id: automation.id };
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .claim_automation(
            automation.id,
            automation.execution_revision,
            None,
            crate::automations::Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            now(),
        )
        .unwrap()
        .accepted
        .unwrap();
    dispatch_accepted(&host, accepted.clone()).unwrap();
    let done = settled(&host, accepted.run.id);
    let HostedSession::Chat(session) = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap()
    else {
        unreachable!()
    };
    super::super::chat::process(
        &host,
        17,
        ClientRequest::ChatAttach {
            session: session.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        reconcile_agent_run(&host, &done, false).unwrap(),
        Reconciliation::Idle,
        "presentation alone is not takeover"
    );
    let gate = agent_gate(&host, accepted.agent.id);
    let held = gate.lock().unwrap();
    let other = host.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        sent.send(super::super::chat::process(
            &other,
            17,
            ClientRequest::ChatCommand {
                chat_id: session.id,
                command: Command::Prompt {
                    text: "interactive follow-up".into(),
                    mode: PromptMode::Send,
                    attachments: Vec::new(),
                },
            },
        ))
        .unwrap();
    });
    assert!(
        received.recv_timeout(Duration::from_millis(50)).is_err(),
        "UI commands share the cleanup gate"
    );
    drop(held);
    received
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(path.parent().unwrap().join("prompts")).unwrap()
        != "prompt\nprompt\n"
    {
        assert!(
            Instant::now() < deadline,
            "interactive prompt was not received"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        reconcile_agent_run(&host, &done, true).unwrap(),
        Reconciliation::UserOwned
    );
    let next = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(
            &owner,
            &Submit {
                destination: Destination::Existing(accepted.agent.id),
                prompt: "automatic prompt must not run".into(),
                idempotency_key: "after-takeover".into(),
            },
            now(),
        )
        .unwrap();
    // The automation coordinator now owns acceptance as well as dispatch.
    // A fabricated follow-up without its exact firing fails before the
    // provider-facing takeover guard is reached.
    assert!(
        dispatch_accepted(&host, next)
            .unwrap_err()
            .contains("owning firing")
    );
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("prompts")).unwrap(),
        "prompt\nprompt\n"
    );
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    super::super::chat::process(
        &host,
        17,
        ClientRequest::ChatClose {
            chat_id: session.id,
        },
    )
    .unwrap();
}

#[test]
fn removed_selected_account_refuses_a_queued_prompt_on_an_existing_actor() {
    if isolated("removed_selected_account_refuses_a_queued_prompt_on_an_existing_actor").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let key = uuid::Uuid::new_v4();
    let account_home = path.parent().unwrap().join("linked-claude");
    std::fs::create_dir(&account_home).unwrap();
    let store = Store::installation().unwrap();
    store
        .update(|accounts| {
            accounts
                .register(
                    crate::account::registry::Registered {
                        key,
                        provider: Provider::Claude,
                        linked_home: Some(account_home.clone()),
                        label: Some("Owned fixture".into()),
                        email: None,
                        organization: None,
                        added_at: 0,
                    },
                    &[],
                )
                .unwrap();
        })
        .unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.account = Some(Account::linked(Provider::Claude, key, account_home).id());
    let automation = automation_fixture(&host, &request);
    let accepted = claim_fixture(&host, &automation);
    dispatch_accepted(&host, accepted.clone()).unwrap();
    let first = settled(&host, accepted.run.id);
    assert_eq!(first.state, State::Succeeded);
    release_fixture(&host, &first);
    let next = claim_fixture(&host, &automation);
    assert_eq!(next.agent.id, accepted.agent.id);
    store
        .update(|accounts| {
            accounts.unregister(key);
        })
        .unwrap();
    dispatch_accepted(&host, next.clone()).unwrap();
    let refused = settled(&host, next.run.id);
    assert_eq!(refused.state, State::Failed);
    assert!(
        refused
            .result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("removed")
    );
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("prompts")).unwrap(),
        "prompt\n"
    );
    assert_eq!(
        reconcile_agent_run(&host, &refused, true).unwrap(),
        Reconciliation::Closed,
        "removal never blocks exact cleanup"
    );
}

fn automation_fixture(host: &SharedHost, request: &Submit) -> crate::automations::Automation {
    let Destination::New(spec) = request.destination.clone() else {
        unreachable!()
    };
    crate::host::automations::service::create(
        host,
        crate::automations::Definition {
            name: "Execution lifecycle fixture".into(),
            prompt: request.prompt.clone(),
            agent: spec,
            schedule: crate::automations::Schedule {
                pattern: crate::automations::SchedulePattern::Daily { hour: 3, minute: 0 },
                timezone: "UTC".into(),
            },
            session_mode: crate::automations::SessionMode::Reuse,
            precheck: None,
        },
    )
    .unwrap()
}

fn claim_fixture(host: &SharedHost, automation: &crate::automations::Automation) -> Accepted {
    registry(host)
        .unwrap()
        .lock()
        .unwrap()
        .claim_automation(
            automation.id,
            automation.execution_revision,
            None,
            crate::automations::Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            now(),
        )
        .unwrap()
        .accepted
        .unwrap()
}

fn release_fixture(host: &SharedHost, run: &Run) {
    assert_eq!(
        reconcile_agent_run(host, run, false).unwrap(),
        Reconciliation::Idle
    );
    let registry = registry(host).unwrap();
    let mut registry = registry.lock().unwrap();
    let firing = registry
        .reconcile_automation_run(run.id, now())
        .unwrap()
        .unwrap();
    let agent = registry.agent(run.agent).unwrap();
    registry
        .finish_automation_release(firing.id, &agent, now())
        .unwrap();
}

fn fixture() -> (SharedHost, Submit, std::path::PathBuf) {
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap());
    let path = root.join("registry.sqlite3");
    let mut registry = Registry::at(&path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Service fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("execution-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("incarnation"),
    });
    state.install_model(registry);
    let request = Submit {
        destination: Destination::New(Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository: root.join("project"),
                directory: root.join("project"),
                channel: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
        }),
        prompt: "Read the owned fixture".into(),
        idempotency_key: "first".into(),
    };
    (Arc::new(Mutex::new(state)), request, path)
}

fn settled(host: &SharedHost, run: RunId) -> Run {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let run = registry(host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(run)
            .unwrap();
        if run.state.is_terminal() {
            return run;
        }
        assert!(Instant::now() < deadline, "run never settled: {run:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn committed_native_dispatch_is_idempotent_and_children_have_real_scoped_authority() {
    if isolated("committed_native_dispatch_is_idempotent_and_children_have_real_scoped_authority")
        .is_some()
    {
        return;
    }
    let (host, mut request, path) = fixture();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.model = Some("fixture-model".into());
    spec.permission_mode = Some("plan".into());
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    // A second connection sees the accepted durable identity independently of
    // provider progress; no child is launched inside the acceptance transaction.
    assert_eq!(
        Registry::at(&path)
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .agent,
        accepted.agent.id
    );
    let result = settled(&host, accepted.run.id);
    assert_eq!(result.state, State::Succeeded, "{result:?}");
    assert_eq!(result.result.text, "SERVICE_RESULT_1");
    assert!(result.provider_key.is_some());
    let retry = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.run.id, accepted.run.id);
    let root = path.parent().unwrap();
    let arguments = std::fs::read_to_string(root.join("claude-arguments")).unwrap();
    assert!(arguments.contains("--model\nfixture-model\n"));
    assert!(arguments.contains("--permission-mode\nplan\n"));
    assert_eq!(
        std::fs::read_to_string(root.join("spawns")).unwrap(),
        "spawn\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("prompts")).unwrap(),
        "prompt\n"
    );

    let parent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(parent.session.as_ref().unwrap().id())
        .unwrap();
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &authority, child_request).unwrap();
    assert_eq!(
        child.agent.origin,
        Origin::Child {
            parent: parent.session.as_ref().unwrap().id()
        }
    );
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    assert!(status(&host, &Authority::default(), child.agent.id).is_err());
    assert!(close(&host, &Authority::default(), child.agent.id).is_err());
    assert!(close(&host, &authority, child.agent.id).unwrap().closed);

    // An ordinary UI ChatClose uses the same retirement transaction as the
    // execution CLI. Afterwards its capability cannot create another child.
    let parent_session = parent.session.as_ref().unwrap().id();
    super::super::chat::process(
        &host,
        EXECUTION_CLIENT,
        ClientRequest::ChatClose {
            chat_id: parent_session,
        },
    )
    .unwrap();
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(parent.id)
            .unwrap()
            .closed
    );
    assert!(origin(&host, &authority).is_err());
    let Reply::Listed(agents) = process(&host, Authority::default(), Operation::List).unwrap()
    else {
        panic!()
    };
    assert!(agents.is_empty());
}

#[test]
fn fresh_codex_execution_uses_owned_listener_and_correlated_provider_result() {
    if isolated("fresh_codex_execution_uses_owned_listener_and_correlated_provider_result")
        .is_some()
    {
        return;
    }
    use serde_json::{Value, json};
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    let program = root.join("bin/codex");
    std::fs::write(&program, "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"$HOME/../codex-arguments\"\nexec /bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.provider = Provider::Codex;
    spec.model = Some("fixture-model".into());
    spec.permission_mode = Some("on-request".into());
    let server_root = root.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            if let Ok(arguments) = std::fs::read_to_string(server_root.join("codex-arguments"))
                && let Some(socket) = arguments
                    .lines()
                    .find_map(|arg| arg.strip_prefix("unix://"))
            {
                assert!(arguments.lines().any(|arg| arg == "app-server"));
                assert!(arguments.lines().any(|arg| arg == "--listen"));
                break std::path::PathBuf::from(socket);
            }
            assert!(Instant::now() < deadline, "Codex server was not launched");
            std::thread::sleep(Duration::from_millis(10));
        };
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "native adapter did not join owned listener"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        let mut prompts = 0;
        loop {
            let message = match websocket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break;
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    break;
                }
                Err(tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                )) => break,
                Err(error) => panic!("fixture websocket: {error}"),
            };
            if message.is_close() {
                break;
            }
            let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let method = frame["method"].as_str().unwrap();
            let result = match method {
                "initialize" => {
                    json!({"userAgent":"codex/0.157.1", "codexHome":server_root.join("home/.codex")})
                }
                "thread/start" => {
                    assert_eq!(frame["params"]["model"], "fixture-model");
                    assert_eq!(frame["params"]["approvalPolicy"], "on-request");
                    json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"fixture-model"})
                }
                "model/list" | "skills/list" => json!({"data":[]}),
                "turn/start" => {
                    prompts += 1;
                    json!({"turn":{"id":"owned-turn"}})
                }
                "thread/read" => {
                    json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turns":[]}})
                }
                method => panic!("unexpected Codex request {method}"),
            };
            websocket
                .send(tungstenite::Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .unwrap();
            if method == "turn/start" {
                websocket.send(tungstenite::Message::Text(json!({"method":"turn/completed","params":{"threadId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turn":{"id":"owned-turn","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"OWNED_CODEX_RESULT"}]}}}).to_string().into())).unwrap();
            }
        }
        prompts
    });
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Succeeded, "{run:?}");
    assert_eq!(run.result.text, "OWNED_CODEX_RESULT");
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    let Some(HostedSession::Chat(session)) = agent.session else {
        panic!()
    };
    assert!(session.owned_codex_listener.is_some());
    assert!(session.owns_codex_listener());
    assert!(
        session
            .launch
            .recipe
            .arguments
            .iter()
            .any(|argument| argument.starts_with("unix://") && argument != "unix://")
    );
    assert!(
        !submit(&host, &Authority::default(), request)
            .unwrap()
            .created
    );
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
    assert_eq!(server.join().unwrap(), 1);
}

#[test]
fn failed_post_spawn_binding_retires_exact_provider_without_losing_launch_identity() {
    if isolated("failed_post_spawn_binding_retires_exact_provider_without_losing_launch_identity")
        .is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_binding BEFORE UPDATE OF record ON agents
         WHEN json_extract(NEW.record, '$.session') IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected descriptor write failure'); END;",
        )
        .unwrap();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Cancelled, "{run:?}");
    assert!(run.provider_key.is_none(), "no prompt reached the provider");
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("no prompt was submitted")
    );
    let registry = registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(
        host.lock().unwrap().chats.is_empty(),
        "exact provider teardown precedes retirement"
    );
    assert!(!path.parent().unwrap().join("prompts").exists());
}

#[test]
fn unavailable_console_keeps_failed_idle_provider_bound_and_explicitly_closeable() {
    if isolated("unavailable_console_keeps_failed_idle_provider_bound_and_explicitly_closeable")
        .is_some()
    {
        return;
    }
    // The isolated unit-test executable has no matched CLI sibling. Exercise
    // the actual allocation failure without spawning any user installation.
    assert!(
        !std::env::current_exe()
            .unwrap()
            .with_file_name("cagents")
            .exists()
    );
    let (host, mut request, path) = fixture();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.surface = Surface::Terminal;
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Failed, "{run:?}");
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("matched cagents CLI")
    );
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    assert!(agent.session.is_some());
    assert!(agent.console.is_none());
    assert!(!agent.closed);
    let repeated = submit(
        &host,
        &Authority::default(),
        Submit {
            destination: Destination::Existing(agent.id),
            prompt: "A follow-up".into(),
            idempotency_key: "second".into(),
        },
    )
    .unwrap();
    assert_eq!(settled(&host, repeated.run.id).state, State::Failed);
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("spawns")).unwrap(),
        "spawn\n",
        "retry keeps the same actor"
    );
    assert!(!path.parent().unwrap().join("prompts").exists());
    assert!(
        close(&host, &Authority::default(), agent.id)
            .unwrap()
            .closed
    );
    assert!(host.lock().unwrap().chats.is_empty());
}

#[test]
fn rejected_pre_spawn_launch_clears_private_intent_and_can_be_closed() {
    if isolated("rejected_pre_spawn_launch_clears_private_intent_and_can_be_closed").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    std::fs::remove_file(path.parent().unwrap().join("bin/claude")).unwrap();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Failed, "{run:?}");
    assert!(run.provider_key.is_none());
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(!path.parent().unwrap().join("spawns").exists());
}

#[test]
fn replaced_scheduled_directory_refuses_dispatch_before_spawning() {
    if isolated("replaced_scheduled_directory_refuses_dispatch_before_spawning").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let directory = path.parent().unwrap().join("project");
    let held = Arc::new(std::fs::File::open(&directory).unwrap());
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    std::fs::rename(&directory, directory.with_file_name("original-project")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    dispatch_accepted_in_directory(&host, accepted.clone(), held).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Failed);
    assert!(run.result.diagnostic.unwrap().contains("directory changed"));
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(!path.parent().unwrap().join("spawns").exists());
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
}

#[test]
fn changed_target_refuses_new_work_but_keeps_interrupt_and_close_available() {
    if isolated("changed_target_refuses_new_work_but_keeps_interrupt_and_close_available").is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let directory = path.parent().unwrap().join("project");
    let held = Arc::new(std::fs::File::open(&directory).unwrap());
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    dispatch_accepted_in_directory(&host, accepted.clone(), held).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    let handle = host
        .lock()
        .unwrap()
        .chats
        .get(&agent.session.as_ref().unwrap().id())
        .unwrap()
        .handle
        .clone();
    std::fs::rename(&directory, directory.with_file_name("original-project")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    let error = handle
        .command(Command::Prompt {
            text: "Must not run in replacement".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        })
        .unwrap_err();
    assert!(error.contains("directory changed"));
    handle.command(Command::Interrupt).unwrap();
    assert!(
        close(&host, &Authority::default(), agent.id)
            .unwrap()
            .closed
    );
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("prompts")).unwrap(),
        "prompt\n"
    );
}
