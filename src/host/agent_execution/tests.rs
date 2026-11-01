#[cfg(target_os = "linux")]
mod owned_late_decision;
#[cfg(target_os = "linux")]
mod retained_automation;

use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Control, Operation, Reply, Target},
    host::state::HostState,
    model::{HostIncarnation, HostSessionId, HostSessionProof, ProjectKind},
    workspace_model::ModelEdit,
};
use std::{os::unix::fs::PermissionsExt, path::Path};

// Environment isolation also proves the service does not depend on any user's
// account, project, provider installation, or running daemon.
pub(in crate::host) fn isolated(name: &str) -> Option<tempfile::TempDir> {
    const CHILD: &str = "CAGENTS_EXECUTION_SERVICE_TEST";
    const INHERITED_SCOPE: &str = "CAGENTS_EXECUTION_TEST_AUTHORITY_PRESENCE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        // Only this helper sets the presence marker. The separate owned-late-
        // decision controller also uses CHILD, without this helper's spawn.
        if let Ok(expected) = std::env::var(INHERITED_SCOPE) {
            let expected = match expected.as_str() {
                "present" => true,
                "absent" => false,
                _ => panic!("invalid fixture authority presence marker"),
            };
            assert_eq!(
                Authority::from_environment() != Authority::default(),
                expected,
                "isolated child lost its inherited authority scope"
            );
        }
        return None;
    }
    let directory = tempfile::tempdir().unwrap();
    // Ordinary fixture paths must agree with canonical project keys even when
    // the caller's temporary directory has a symlinked parent.
    let root = directory.path().canonicalize().unwrap();
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
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    // Keep the caller's delegation inherited unchanged, without inspecting or
    // copying its value. Every other environment input is fixture-owned.
    for (name, _) in std::env::vars_os() {
        if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
            command.env_remove(name);
        }
    }
    let result = command
        .args([
            "--exact",
            &if name.contains("::") {
                name.to_owned()
            } else {
                format!("host::agent_execution::tests::{name}")
            },
            "--nocapture",
        ])
        .env(CHILD, name)
        .env(
            INHERITED_SCOPE,
            if Authority::from_environment() != Authority::default() {
                "present"
            } else {
                "absent"
            },
        )
        .env("CAGENTS_EXECUTION_FIXTURE", &root)
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
    *'"type":"control_response"'*)
      # The answer to a permission request raised below finishes its turn;
      # a background one is only recorded, since its turn already ended.
      case "$frame" in
        *'"background-'*)
          case "$frame" in
            *'"behavior":"allow"'*) printf 'allow\n' >> "$HOME/../decisions" ;;
            *) printf 'deny\n' >> "$HOME/../decisions" ;;
          esac
          continue
          ;;
      esac
      case "$frame" in
        *'"behavior":"allow"'*) printf 'allow\n' >> "$HOME/../decisions" ;;
        *) printf 'deny\n' >> "$HOME/../decisions" ;;
      esac
      printf '{"type":"assistant","uuid":"message-%s","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"SERVICE_RESULT_%s"}],"usage":{"input_tokens":7,"output_tokens":3}}}\n' "$n" "$n" "$n"
      printf '{"type":"result","uuid":"result-%s","subtype":"success","is_error":false,"session_id":"%s","usage":{"input_tokens":7,"output_tokens":3}}\n' "$n" "$session"
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      printf 'prompt\n' >> "$HOME/../prompts"
      case "$frame" in
        *HOLD_CLEANUP_BUSY*)
          : > "$HOME/../cleanup-busy"
          attempts=0
          while [ ! -e "$HOME/../cleanup-release" ]; do
            attempts=$((attempts + 1))
            [ "$attempts" -lt 1000 ] || exit 1
            sleep 0.01
          done
          ;;
        *COURIER_BACKGROUND*)
          printf '{"type":"assistant","uuid":"launch-child","message":{"id":"launch-child","role":"assistant","content":[{"type":"tool_use","id":"courier-child","name":"Agent","input":{"run_in_background":true}}]}}\n'
          printf '{"type":"user","uuid":"child-launch-result","tool_use_result":{"status":"async_launched","agentId":"courier-agent"},"message":{"content":[{"type":"tool_result","tool_use_id":"courier-child","content":"Launched"}]}}\n'
          printf '{"type":"system","subtype":"task_started","task_id":"courier-agent","tool_use_id":"courier-child","task_type":"local_agent"}\n'
          : > "$HOME/../courier-busy"
          attempts=0
          while [ ! -e "$HOME/../courier-release" ]; do
            attempts=$((attempts + 1))
            [ "$attempts" -lt 500 ] || exit 1
            sleep 0.01
          done
          ;;
      esac
      case "$frame" in
        *BACKGROUND_PERMISSION*)
          # A background subagent asks, then the main turn ends without it.
          printf '{"type":"control_request","request_id":"background-%s","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-%s","input":{"command":"true"}}}\n' "$n" "$n"
          ;;
      esac
      case "$frame" in
        *ASK_PERMISSION*)
          printf '{"type":"control_request","request_id":"permission-%s","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-%s","input":{"command":"true"}}}\n' "$n" "$n"
          continue
          ;;
      esac
      printf '{"type":"assistant","uuid":"message-%s","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"SERVICE_RESULT_%s"}],"usage":{"input_tokens":7,"output_tokens":3}}}\n' "$n" "$n" "$n"
      printf '{"type":"result","uuid":"result-%s","subtype":"success","is_error":false,"session_id":"%s","usage":{"input_tokens":7,"output_tokens":3}}\n' "$n" "$session"
      ;;
  esac
done
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn owning_close_retires_a_restored_chat_without_a_ui_subscriber() {
    if isolated("owning_close_retires_a_restored_chat_without_a_ui_subscriber").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let HostedSession::Chat(model_source) = agent.session.unwrap() else {
        panic!("fixture did not create a native chat");
    };
    // The fake provider confirms its selected UUID only after the first user
    // frame, after the fresh launch descriptor has been bound in the model.
    assert_eq!(model_source.launch.session_id, None);
    let id = model_source.id;
    let handle = {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&id).unwrap();
        // Startup restoration has no UI attachment. In particular, execution
        // ownership is not represented by a synthetic client-zero subscriber.
        record.subscribers.clear();
        let confirmed = record
            .handle
            .confirmed_session_id()
            .expect("settled provider UUID");
        record.session.launch.session_id = Some(confirmed);
        assert_ne!(
            record.session.launch.session_id,
            model_source.launch.session_id
        );
        assert!(crate::agent_execution::same_chat_process(
            &record.session,
            &model_source
        ));
        record.handle.clone()
    };
    let refusal = super::super::chat::process(&host, 0, ClientRequest::ChatClose { chat_id: id })
        .expect_err("unattached UI close unexpectedly succeeded");
    assert_eq!(
        refusal.code,
        crate::terminal_ipc::ProtocolErrorCode::PermissionDenied
    );
    assert!(refusal.message.contains("not attached"), "{refusal:?}");
    assert!(!handle.is_completed());
    assert!(host.lock().unwrap().chats[&id].subscribers.is_empty());
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );

    let reply = process(
        &host,
        Authority::default(),
        Operation::Close(accepted.agent.id),
    )
    .expect("authenticated owning Close must not require UI subscription");
    let Reply::Closed(closed) = reply else {
        panic!("unexpected close reply: {reply:?}")
    };
    assert!(closed.closed);
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(!host.lock().unwrap().chats.contains_key(&id));
    assert!(
        handle.is_completed(),
        "owning Close did not retire the exact provider"
    );
}

#[test]
fn owning_close_refuses_a_current_chat_outside_the_model_process_identity() {
    if isolated("owning_close_refuses_a_current_chat_outside_the_model_process_identity").is_some()
    {
        return;
    }
    let (host, request, _) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let id = agent.session.unwrap().id();
    let (original, handle) = {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&id).unwrap();
        record.subscribers.clear();
        let original = record.session.clone();
        record
            .session
            .launch
            .recipe
            .environment
            .insert("CAGENTS_FIXTURE_CHANGED_OWNER".into(), "different".into());
        (original, record.handle.clone())
    };
    let error = process(
        &host,
        Authority::default(),
        Operation::Close(accepted.agent.id),
    )
    .unwrap_err();
    assert!(error.contains("ownership changed"), "{error}");
    assert!(!handle.is_completed());
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(host.lock().unwrap().chats.contains_key(&id));
    handle.command(Command::Refresh).unwrap();
    host.lock().unwrap().chats.get_mut(&id).unwrap().session = original;
    assert!(
        matches!(process(&host, Authority::default(), Operation::Close(accepted.agent.id)).unwrap(), Reply::Closed(agent) if agent.closed)
    );
}

#[test]
fn captured_owning_close_refuses_a_distinct_proxy_for_the_same_retained_actor() {
    if isolated("captured_owning_close_refuses_a_distinct_proxy_for_the_same_retained_actor")
        .is_some()
    {
        return;
    }
    let (host, request, _) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let HostedSession::Chat(source) = agent.session.unwrap() else {
        panic!("not a chat")
    };
    let (first, second) = chat::runtime::Handle::fixture_remote_attachment_pair();
    assert!(first.same_actor(&second));
    assert_eq!(first.actor_identity(), second.actor_identity());
    assert!(!first.same_attachment(&second));
    let (target, real_handle, session, instance) = {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&source.id).unwrap();
        let real_handle = std::mem::replace(&mut record.handle, first);
        let session = record.session.clone();
        let instance = record.execution_instance;
        let target = super::super::chat::CloseTarget::for_execution(
            &state,
            &state.chats[&source.id],
            &source,
        )
        .unwrap();
        (target, real_handle, session, instance)
    };
    // This is an owning-entry counterfactual, not an actual helper adoption:
    // replacing only Arc allocation must revoke the captured close capability.
    host.lock()
        .unwrap()
        .chats
        .get_mut(&source.id)
        .unwrap()
        .handle = second.clone();
    let error = super::super::chat::close_captured(&host, target).unwrap_err();
    assert_eq!(
        error.code,
        crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
        "{error:?}"
    );
    {
        let state = host.lock().unwrap();
        let record = &state.chats[&source.id];
        assert_eq!(record.session, session);
        assert_eq!(record.execution_instance, instance);
        assert!(record.handle.same_attachment(&second));
    }
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(!real_handle.is_completed());
    real_handle.command(Command::Refresh).unwrap();
    host.lock()
        .unwrap()
        .chats
        .get_mut(&source.id)
        .unwrap()
        .handle = real_handle.clone();
    assert!(
        matches!(process(&host, Authority::default(), Operation::Close(accepted.agent.id)).unwrap(), Reply::Closed(agent) if agent.closed)
    );
    assert!(real_handle.is_completed());
}

#[test]
fn blocking_owning_close_preserves_a_replacement_installed_during_provider_join() {
    if isolated("blocking_owning_close_preserves_a_replacement_installed_during_provider_join")
        .is_some()
    {
        return;
    }
    use chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};
    struct BlockingDrop {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Adapter for BlockingDrop {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            // A disconnected test owner cancels this barrier. The deadline
            // also bounds cleanup if an assertion unwinds inside thread::scope.
            if self.entered.send(()).is_ok() {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
        }
    }
    struct ReleaseOnDrop(Option<std::sync::mpsc::SyncSender<()>>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.try_send(());
            }
        }
    }
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let HostedSession::Chat(source) = agent.session.unwrap() else {
        panic!("not a chat")
    };
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let spec = ProcessSpec {
        execution: None,
        messages: None,
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: path.parent().unwrap().join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let blocking = chat::runtime::Handle::start(
        spec,
        Box::new(BlockingDrop {
            entered: entered_tx,
            release: release_rx,
        }),
        initial,
        store,
        None,
        |_| {},
    )
    .unwrap();
    let (target, successor, original_source, original_instance) = {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&source.id).unwrap();
        let successor = std::mem::replace(&mut record.handle, blocking.clone());
        let original_source = record.session.clone();
        let original_instance = record.execution_instance;
        let target = super::super::chat::CloseTarget::for_execution(
            &state,
            &state.chats[&source.id],
            &source,
        )
        .unwrap();
        (target, successor, original_source, original_instance)
    };
    let replacement_instance = uuid::Uuid::new_v4();
    std::thread::scope(|scope| {
        // This guard drops on every assertion/receive failure before scope
        // joins its worker, so teardown need not wait for the fallback timeout.
        let mut release = ReleaseOnDrop(Some(release_tx));
        let worker = scope.spawn(|| super::super::chat::close_captured(&host, target));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("actor worker never entered adapter Drop during close join");
        assert!(
            !worker.is_finished(),
            "close returned before its actor worker joined"
        );
        assert!(
            blocking.is_completed(),
            "old actor did not finish its actual process teardown"
        );
        {
            let mut state = host.lock().unwrap();
            let record = state.chats.get_mut(&source.id).unwrap();
            record.handle = successor.clone();
            record.execution_instance = replacement_instance;
        }
        release.0.take().unwrap().send(()).unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.message.contains("thread changed"), "{error:?}");
    });
    {
        let state = host.lock().unwrap();
        let record = &state.chats[&source.id];
        assert_eq!(record.session, original_source);
        assert_eq!(record.execution_instance, replacement_instance);
        assert!(record.handle.same_attachment(&successor));
    }
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(!successor.is_completed());
    successor.command(Command::Refresh).unwrap();
    // Restore the fixture generation solely for its normal authenticated cleanup.
    host.lock()
        .unwrap()
        .chats
        .get_mut(&source.id)
        .unwrap()
        .execution_instance = original_instance;
    assert!(
        matches!(process(&host, Authority::default(), Operation::Close(accepted.agent.id)).unwrap(), Reply::Closed(agent) if agent.closed)
    );
    assert!(successor.is_completed());
}

#[test]
fn captured_owning_close_refuses_a_replaced_host_execution_instance() {
    if isolated("captured_owning_close_refuses_a_replaced_host_execution_instance").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let HostedSession::Chat(source) = agent.session.unwrap() else {
        panic!("not a chat")
    };
    let (target, handle, original_instance) = {
        let state = host.lock().unwrap();
        let record = &state.chats[&source.id];
        (
            super::super::chat::CloseTarget::for_execution(&state, record, &source).unwrap(),
            record.handle.clone(),
            record.execution_instance,
        )
    };
    // A deterministic generation cutpoint: keep the same ID and attachment so
    // the execution-instance fence, independently, must reject this capability.
    host.lock()
        .unwrap()
        .chats
        .get_mut(&source.id)
        .unwrap()
        .execution_instance = uuid::Uuid::new_v4();
    let error = super::super::chat::close_captured(&host, target).unwrap_err();
    assert_eq!(
        error.code,
        crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
        "{error:?}"
    );
    assert!(!handle.is_completed());
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    handle.command(Command::Refresh).unwrap();
    host.lock()
        .unwrap()
        .chats
        .get_mut(&source.id)
        .unwrap()
        .execution_instance = original_instance;
    assert!(
        matches!(process(&host, Authority::default(), Operation::Close(accepted.agent.id)).unwrap(), Reply::Closed(agent) if agent.closed)
    );
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
    super::super::finish_chat_shutdown(&host).unwrap();
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
    get_subagents) data='{"subagents":[]}' ;;
    set_subagent_subscription) data='{"level":"events"}' ;;
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
    spec.reasoning_effort = Some("low".into());
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
    assert_upgrade_environment(&host, accepted.agent.id);
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
            name: None,
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
    assert!(arguments.contains("--thinking\nlow\n"));
    assert_upgrade_environment(&host, accepted.agent.id);
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
}

fn assert_upgrade_environment(host: &SharedHost, agent: AgentId) {
    let session = registry(host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap()
        .session
        .unwrap()
        .id();
    let mut state = host.lock().unwrap();
    let record = state.chats.get(&session).unwrap();
    if record.session.launch.provider == Provider::Claude {
        let project: String = record
            .session
            .launch
            .recipe
            .working_directory
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let directory = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".claude/projects")
            .join(project);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!(
                "{}.jsonl",
                record.handle.confirmed_session_id().unwrap()
            )),
            b"",
        )
        .unwrap();
    }
    let execution = record.handle.execution_environment();
    assert!(
        execution
            .0
            .contains_key(crate::agent_execution::AUTHORITY_ENV)
    );
    super::super::chat::validate_upgrade_environment(&record.session.launch, false, &record.handle)
        .expect("daemon-owned authority and CLI PATH must not block recovery");
    for name in ["PATH", "ANTHROPIC_API_KEY"] {
        let mut changed = record.session.launch.clone();
        changed
            .recipe
            .environment
            .insert(name.into(), "different-selected-value".into());
        assert!(
            super::super::chat::validate_upgrade_environment(&changed, false, &record.handle)
                .is_err(),
            "selected {name} must still be compared"
        );
    }
    let (busy, _) = record.handle.update_activity();
    use crate::update::impact::{Disposition, Policy};
    for (policy, expected) in [
        (
            Policy::WhenIdle,
            if busy {
                Disposition::WaitsForIdle
            } else {
                Disposition::Resumes
            },
        ),
        (
            Policy::Interrupt,
            if busy {
                Disposition::Interrupted
            } else {
                Disposition::Resumes
            },
        ),
    ] {
        let impact = state.upgrade_impact(policy).unwrap();
        let impact = impact
            .sessions
            .iter()
            .find(|impact| impact.id == session)
            .unwrap();
        assert_eq!(impact.disposition, expected, "{}", impact.reason);
    }
}

#[test]
fn automatic_cleanup_is_exact_run_fenced_and_confirms_provider_retirement() {
    if isolated("automatic_cleanup_is_exact_run_fenced_and_confirms_provider_retirement").is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    // Observe consumption of the actual refresh response, not just command
    // admission: pending transport must still refuse an idle close.
    let provider = path.parent().unwrap().join("bin/claude");
    let script = std::fs::read_to_string(&provider).unwrap();
    let original = r#"      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id""#;
    let refreshed = r#"      case "$frame" in
        *'"subtype":"get_context_usage"'*)
          printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"totalTokens":424242,"maxTokens":1000000}}}\n' "$id"
          ;;
        *)
          printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
          ;;
      esac"#;
    assert_eq!(script.matches(original).count(), 1);
    std::fs::write(provider, script.replace(original, refreshed)).unwrap();
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
    assert_ne!(handle.usage().context_tokens, 424242);
    handle.command(Command::Refresh).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if handle.usage().context_tokens == 424242
            && handle.check_update_readiness().unwrap()
                == crate::chat::runtime::UpdateReadiness::Idle
        {
            break;
        }
        assert!(Instant::now() < deadline, "fixture refresh did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !handle.try_close_if_idle(stale).unwrap(),
        "idle metadata drift must defer without stopping the actor"
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
                name: None,
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

pub(in crate::host) fn fixture() -> (SharedHost, Submit, std::path::PathBuf) {
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
        name: None,
        destination: Destination::New(Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository: root.join("project"),
                directory: root.join("project"),
                placement: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        }),
        prompt: "Read the owned fixture".into(),
        idempotency_key: "first".into(),
    };
    (
        Arc::new(crate::host::state::HostLock::new(state)),
        request,
        path,
    )
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
fn legacy_open_console_preflight_refuses_before_provider_stop_and_keeps_followup_usable() {
    if isolated(
        "legacy_open_console_preflight_refuses_before_provider_stop_and_keeps_followup_usable",
    )
    .is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Succeeded);
    let store = registry(&host).unwrap();
    let (agent, pane, scope) = {
        let mut store = store.lock().unwrap();
        let agent = store.agent(accepted.agent.id).unwrap();
        let HostedSession::Chat(source) = agent.session.as_ref().unwrap() else {
            unreachable!()
        };
        // Persist the old UI's ended-console projection directly, as an old
        // store would contain it after console /quit or EOF (neither closes
        // the provider). The provider below is an actual running fixture.
        let mut console_recipe = source.launch.recipe.clone();
        console_recipe.program = "/bin/cat".into();
        console_recipe.arguments.clear();
        let terminal = crate::model::TerminalSession {
            id: source.id,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: console_recipe,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            },
        };
        let agent = store
            .bind_agent_console(agent.id, agent.session.as_ref().unwrap(), terminal.clone())
            .unwrap();
        let scope = agent.placement.clone();
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.id.as_uuid()),
            title: "Legacy ended console".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: crate::model::CardKind::Terminal(terminal),
            scope: None,
        };
        store
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        (agent, pane, scope)
    };
    let source = agent.session.as_ref().unwrap().id();
    let handle = host.lock().unwrap().chats[&source].handle.clone();
    let authority = store
        .lock()
        .unwrap()
        .retained_agent_authority_source(source)
        .unwrap();
    assert!(matches!(authority, Some(HostedSession::Chat(_))));
    // Exercise the public checked-close dispatcher with a real live PTY,
    // rather than only calling the actor helper after terminal teardown.
    let console = agent.console.as_ref().unwrap().clone();
    {
        let mut state = host.lock().unwrap();
        state.register_client(29);
        assert_eq!(
            state.reserve_spawn(source).unwrap_err().code,
            crate::terminal_ipc::ProtocolErrorCode::TerminalAlreadyExists
        );
        let record = state.chats.get_mut(&source).unwrap();
        record.console_recipe = Some(console.launch.clone());
    }
    // Follow the execution service's private recipe admission. An unpaired
    // spawn must still refuse while the native provider owns this session.
    let (descriptor, terminal) = super::super::state::spawn_terminal_view_owned(
        &host,
        29,
        console,
        Default::default(),
        None,
        true,
    )
    .unwrap();
    {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&source).unwrap();
        record.session.terminal = Some(descriptor.session);
        record.paired_terminal = Some(terminal.clone());
    }
    assert!(!terminal.is_completed());
    assert_eq!(
        store
            .lock()
            .unwrap()
            .retained_agent_authority_source(source)
            .unwrap(),
        authority,
        "a paired console must retain the native provider's authority"
    );
    let before = store.lock().unwrap().model().unwrap();
    let prompts = std::fs::read(path.parent().unwrap().join("prompts")).unwrap();
    assert!(
        close(&host, &Authority::default(), agent.id)
            .unwrap_err()
            .contains("historical generation proof")
    );
    assert!(
        crate::host::chat::process(
            &host,
            EXECUTION_CLIENT,
            ClientRequest::ChatClose { chat_id: source }
        )
        .unwrap_err()
        .message
        .contains("historical generation proof")
    );
    assert!(
        crate::host::chat::close_terminal_actor(&host, source, &handle)
            .unwrap_err()
            .message
            .contains("historical generation proof")
    );
    let HostedSession::Chat(retained) = agent.session.as_ref().unwrap() else {
        unreachable!()
    };
    assert!(
        crate::host::chat::close_retained_native(&host, retained)
            .unwrap_err()
            .contains("historical generation proof")
    );
    let checked = crate::host::client::process_request(
        &host,
        29,
        ClientRequest::CloseTerminalChecked {
            terminal_id: source,
            allow_active: true,
        },
    );
    assert!(
        checked
            .unwrap_err()
            .message
            .contains("historical generation proof")
    );
    assert!(
        !terminal.is_completed(),
        "checked-close refusal must precede PTY teardown"
    );
    assert!(
        host.lock().unwrap().terminals[&source]
            .handle
            .same_worker(&terminal)
    );
    assert!(
        !handle.is_completed(),
        "refusal must precede provider freeze/stop"
    );
    assert!(
        host.lock().unwrap().chats[&source]
            .handle
            .same_actor(&handle)
    );
    assert_eq!(store.lock().unwrap().model().unwrap(), before);
    assert_eq!(store.lock().unwrap().agent(agent.id).unwrap(), agent);
    assert_eq!(store.lock().unwrap().agent_run(run.id).unwrap(), run);
    assert_eq!(
        std::fs::read(path.parent().unwrap().join("prompts")).unwrap(),
        prompts
    );
    let followup = submit(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::Existing(agent.id),
            prompt: "Still usable after refused close".into(),
            idempotency_key: "after-refusal".into(),
        },
    )
    .unwrap();
    assert_eq!(
        settled(&host, followup.run.id).result.text,
        "SERVICE_RESULT_2"
    );
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("spawns")).unwrap(),
        "spawn\n"
    );
    // Test-owned cleanup after proving refusal had no teardown side effect.
    {
        let mut store = store.lock().unwrap();
        let revision = store.view_revision(&scope).unwrap();
        store
            .apply(&ModelEdit::ChangeView {
                scope,
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: crate::workspace_model::views::Change::Remove { id: pane.id },
            })
            .unwrap();
    }
    terminal.shutdown().unwrap();
    close(&host, &Authority::default(), agent.id).unwrap();
}

#[test]
fn retained_old_alias_preflights_current_agent_before_real_holder_release() {
    if isolated("retained_old_alias_preflights_current_agent_before_real_holder_release").is_some()
    {
        return;
    }
    use crate::daemon::holder_registry::{self as holders, Kind};
    use chat::protocol::{ExecutionContext, ProcessSpec, TransportSpec};
    let (host, mut request, path) = fixture();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.provider = Provider::Codex;
    let store = registry(&host).unwrap();
    let accepted = store
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    let current_proof = host.lock().unwrap().proof().clone();
    let mut original_proof = current_proof.clone();
    original_proof.host_incarnation = HostIncarnation::new("before-holder-adoption");
    let directory = path.parent().unwrap();
    let id = TerminalId::new();
    let mut original = chat::fresh_session(
        Provider::Codex,
        id,
        directory.to_owned(),
        &Account::system(Provider::Codex),
    )
    .unwrap();
    original.proof = Some(original_proof.clone());
    original.claim_owned_codex_listener(Path::new("/var/tmp"), &original_proof);
    let socket = original
        .owned_codex_listener
        .as_ref()
        .unwrap()
        .socket
        .clone();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &original_proof).unwrap();
    let context = Arc::new(ExecutionContext::new(Some(Arc::new(
        std::fs::File::open(directory).unwrap(),
    ))));
    let process = ProcessSpec {
        execution: Some(context.clone()),
        messages: None,
        transport: TransportSpec::OwnedUnixWebSocket { socket },
        program: "/bin/sleep".into(),
        arguments: vec!["60".into()],
        working_directory: directory.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut holder = chat::native_holder::spawn(process, original.clone()).unwrap();
    context.installed();
    let owner = holders::evidence(
        &holders::socket(&holders::directory(Kind::Native).unwrap(), id),
        Kind::Native,
    )
    .unwrap()
    .unwrap();
    holder
        .confirm(
            uuid::Uuid::new_v4().to_string(),
            chat::protocol::ServerIdentity {
                user_agent: "cagents/0.157.1 (Linux)".into(),
                canonical_home: directory.to_owned(),
                reported_home: directory.display().to_string(),
            },
            owner.root.clone(),
        )
        .unwrap();
    holder.preserve();
    store
        .lock()
        .unwrap()
        .bind_agent(
            accepted.agent.id,
            None,
            HostedSession::Chat(original.clone()),
        )
        .unwrap();
    drop(lease);
    let _lease = crate::daemon::HostLease::acquire(&paths, &current_proof).unwrap();
    let (adopted, recovery) =
        chat::native_holder::adopt(&owner, &original, &current_proof).unwrap();
    let detached = adopted.preserve();
    assert_ne!(recovery.source.proof, original.proof);
    assert!(recovery.previous_sources.contains(&original));
    let current = HostedSession::Chat(recovery.source.clone());
    store
        .lock()
        .unwrap()
        .adopt_authenticated_agent_session(
            &recovery
                .previous_sources
                .iter()
                .cloned()
                .map(HostedSession::Chat)
                .collect::<Vec<_>>(),
            &current,
        )
        .unwrap();
    let (agent, scope, pane) = {
        let mut store = store.lock().unwrap();
        let mut recipe = recovery.source.launch.recipe.clone();
        recipe.program = "/bin/cat".into();
        recipe.arguments.clear();
        let terminal = crate::model::TerminalSession {
            id,
            launch: recipe,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            },
        };
        let agent = store
            .bind_agent_console(accepted.agent.id, &current, terminal.clone())
            .unwrap();
        let scope = agent.placement.clone();
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.id.as_uuid()),
            title: "Legacy adopted console".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: crate::model::CardKind::Terminal(terminal),
            scope: None,
        };
        store
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        (agent, scope, pane)
    };
    let before = store.lock().unwrap().model().unwrap();
    let run = store.lock().unwrap().agent_run(accepted.run.id).unwrap();
    // This old alias genuinely passes the original preflight predicate. The
    // cleanup service must additionally check the actual current owner set.
    preflight_retirement(&host, &original).unwrap();
    let error = crate::host::chat::close_retained_native(&host, &original).unwrap_err();
    assert!(error.contains("historical generation proof"), "{error}");
    assert!(
        owner.root.is_current(),
        "preflight refusal must not Release the real retained server"
    );
    let (still_owned, exited) = chat::native_holder::inspect(&owner, &current_proof).unwrap();
    assert!(!exited);
    assert_eq!(still_owned.source, recovery.source);
    assert_eq!(store.lock().unwrap().model().unwrap(), before);
    assert_eq!(store.lock().unwrap().agent(agent.id).unwrap(), agent);
    assert_eq!(store.lock().unwrap().agent_run(run.id).unwrap(), run);
    assert!(!host.lock().unwrap().native_closing.contains_key(&id));
    // Test-owned cleanup, only after all no-side-effect assertions.
    {
        let mut store = store.lock().unwrap();
        let revision = store.view_revision(&scope).unwrap();
        store
            .apply(&ModelEdit::ChangeView {
                scope,
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: crate::workspace_model::views::Change::Remove { id: pane.id },
            })
            .unwrap();
    }
    detached.close().unwrap();
    assert!(!owner.root.is_current());
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
    spec.reasoning_effort = Some("high".into());
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
    assert_upgrade_environment(&host, accepted.agent.id);
    assert!(result.provider_key.is_some());
    let retry = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.run.id, accepted.run.id);
    let root = path.parent().unwrap();
    let arguments = std::fs::read_to_string(root.join("claude-arguments")).unwrap();
    assert!(arguments.contains("--model\nfixture-model\n"));
    assert!(arguments.contains("--permission-mode\nplan\n"));
    assert!(arguments.contains("--effort\nhigh\n"));
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
            parent: crate::agent_execution::Lineage(parent.session.as_ref().unwrap().id())
        }
    );
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    assert!(status(&host, &Authority::default(), child.agent.id).is_err());
    assert!(close(&host, &Authority::default(), child.agent.id).is_err());
    assert!(close(&host, &authority, child.agent.id).unwrap().closed);

    // An ordinary UI ChatClose must publish the pane tombstone in its first
    // retirement transaction, without requiring a later execution Close call.
    let scope = crate::workspace_model::views::Scope::Channel(parent.channel().unwrap());
    let pane_id = crate::model::CardId::from_uuid(parent.id.as_uuid());
    let HostedSession::Chat(parent_chat) = parent.session.as_ref().unwrap() else {
        unreachable!()
    };
    registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![crate::workspace_model::views::Pane {
                id: pane_id,
                title: "Review".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 0.5,
                kind: crate::model::CardKind::Chat(parent_chat.clone()),
                scope: None,
            }],
            closed: vec![],
        })
        .unwrap();
    let retained_run = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent_run(accepted.run.id)
        .unwrap();
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
    // Read a separate database connection, before any service-level Close, to
    // verify the committed state that subscribed windows receive.
    let stored = Registry::at(&path).unwrap();
    let model = stored.model().unwrap();
    let view = model.views.iter().find(|view| view.scope == scope).unwrap();
    assert!(view.panes.iter().all(|pane| pane.id != pane_id));
    assert!(view.retired.contains(&pane_id));
    assert_eq!(stored.agent_run(accepted.run.id).unwrap(), retained_run);
    assert!(origin(&host, &authority).is_err());
    let Reply::Listed(agents) = process(&host, Authority::default(), Operation::List).unwrap()
    else {
        panic!()
    };
    assert!(agents.is_empty());
}

#[test]
fn graceful_daemon_restart_retires_completed_delegated_agents_before_ui_recovery() {
    if isolated("graceful_daemon_restart_retires_completed_delegated_agents_before_ui_recovery")
        .is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let source = agent.session.as_ref().unwrap().clone();
    let token = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(source.id())
        .unwrap();
    assert!(!agent.closed);

    super::super::finish_chat_shutdown(&host).unwrap();
    assert!(!host.lock().unwrap().chats[&source.id()].handle.owns_work());
    drop(host);
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("restarted-execution-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("restarted-incarnation"),
    });
    state.install_model(Registry::at(&path).unwrap());
    let restarted = Arc::new(crate::host::state::HostLock::new(state));
    // The close command remains idempotent even though the old actor is gone
    // and any UI resume would carry the new daemon's proof.
    assert!(
        close(&restarted, &Authority::default(), agent.id)
            .unwrap()
            .closed
    );
    assert!(
        registry(&restarted)
            .unwrap()
            .lock()
            .unwrap()
            .agents(None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        status(&restarted, &Authority::default(), agent.id)
            .unwrap()
            .1
            .state,
        State::Succeeded
    );
    assert!(origin(&restarted, &token).is_err());
    let root = path.parent().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("spawns")).unwrap(),
        "spawn\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("prompts")).unwrap(),
        "prompt\n"
    );
}

#[test]
fn failed_shutdown_retirement_preserves_the_agent_until_exact_cleanup_can_be_saved() {
    if isolated("failed_shutdown_retirement_preserves_the_agent_until_exact_cleanup_can_be_saved")
        .is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let fault = rusqlite::Connection::open(path).unwrap();
    fault.execute_batch("CREATE TRIGGER refuse_shutdown_retirement BEFORE UPDATE OF closed ON agents BEGIN SELECT RAISE(FAIL, 'fixture retirement unavailable'); END;").unwrap();
    assert!(
        super::super::finish_chat_shutdown(&host)
            .unwrap_err()
            .contains("fixture retirement unavailable")
    );
    let retained = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    assert!(!retained.closed);
    let source = retained.session.as_ref().unwrap();
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_authority_source(source.id())
            .unwrap()
            .is_some()
    );
    fault
        .execute_batch("DROP TRIGGER refuse_shutdown_retirement")
        .unwrap();
    super::super::finish_chat_shutdown(&host).unwrap();
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
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
    spec.reasoning_effort = Some("low".into());
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
                    assert_eq!(
                        frame["params"]["config"],
                        json!({"model_reasoning_effort": "low"})
                    );
                    json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"fixture-model"})
                }
                "model/list" | "skills/list" => json!({"data":[]}),
                "thread/queue/list" | "thread/list" => json!({"data":[],"nextCursor":null}),
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
    super::super::finish_chat_shutdown(&host).unwrap();
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_authority_source(session.id)
            .unwrap()
            .is_none()
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
        !crate::companion::Product::Cli
            .beside_current()
            .unwrap()
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
            name: None,
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

/// Registers a workspace-model watcher, as a window's observer does.
fn model_watcher(host: &SharedHost) -> Arc<crate::host::delivery::DeliveryQueue> {
    const WATCHER: u64 = 7;
    let mut state = host.lock().unwrap();
    let delivery = state.register_client(WATCHER);
    state.watch_model(WATCHER).unwrap();
    delivery
}

/// Model edges delivered to `watcher` until one reaches `sequence`.
fn edges_through(
    watcher: &Arc<crate::host::delivery::DeliveryQueue>,
    sequence: u64,
) -> Vec<(u64, ModelEdit)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let queue = Arc::clone(watcher);
    std::thread::spawn(move || {
        while let Some(queued) = queue.pop() {
            if let crate::terminal_ipc::ServerMessage::WorkspaceModelEdited { sequence, edit } =
                queued.message
                && sender.send((sequence, edit)).is_err()
            {
                return;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut edges = Vec::new();
    while edges
        .last()
        .is_none_or(|(observed, _): &(u64, ModelEdit)| *observed < sequence)
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(edge) => edges.push(edge),
            Err(_) => panic!("no model edge reached sequence {sequence}: {edges:?}"),
        }
    }
    edges
}

#[test]
fn provider_run_transitions_announce_agents_changed_to_model_watchers() {
    if isolated("provider_run_transitions_announce_agents_changed_to_model_watchers").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let watcher = model_watcher(&host);
    let before = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .execution_sequence()
        .unwrap();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Succeeded, "{run:?}");
    let after = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .execution_sequence()
        .unwrap();
    // Acceptance and binding are two edges; the provider's start and its
    // settlement must each be another, so a watcher can stop treating the run
    // as active without polling.
    assert!(after >= before + 4, "{before} -> {after}");
    let edges = edges_through(&watcher, after);
    watcher.close_after_drain();
    assert!(
        edges
            .iter()
            .all(|(_, edit)| matches!(edit, ModelEdit::AgentsChanged))
    );
    assert!(
        edges.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "edges arrive in sequence order: {edges:?}"
    );
    assert_eq!(edges.last().unwrap().0, after, "settlement was announced");
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
}

#[test]
fn failed_dispatch_announces_its_settlement_exactly_once() {
    if isolated("failed_dispatch_announces_its_settlement_exactly_once").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let registry = registry(&host).unwrap();
    let accepted = registry
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    let watcher = model_watcher(&host);
    let before = registry.lock().unwrap().execution_sequence().unwrap();
    finish_failure(&host, accepted.run.id, State::Failed, "fixture".into());
    // Repeating a failure for a settled run records nothing and stays silent.
    finish_failure(&host, accepted.run.id, State::Failed, "again".into());
    watcher.close_after_drain();
    let edges = std::iter::from_fn(|| watcher.pop())
        .filter_map(|queued| match queued.message {
            crate::terminal_ipc::ServerMessage::WorkspaceModelEdited { sequence, edit } => {
                Some((sequence, edit))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(edges, vec![(before + 1, ModelEdit::AgentsChanged)]);
    let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
    assert_eq!(run.state, State::Failed);
    assert_eq!(run.result.diagnostic.as_deref(), Some("fixture"));
}

#[test]
fn authenticated_worktree_caller_places_child_in_its_exact_visible_view() {
    use crate::{
        agent_execution::{Placement, directory::Scope},
        model::{CardId, CardKind},
        workspace_model::views::Pane,
    };
    if isolated("authenticated_worktree_caller_places_child_in_its_exact_visible_view").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let parent = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert_eq!(settled(&host, parent.run.id).state, State::Succeeded);
    let (authority, scope, channels) = {
        let owned = registry(&host).unwrap();
        let mut model = owned.lock().unwrap();
        let session = model.agent(parent.agent.id).unwrap().session.unwrap();
        let scope = Placement::Worktree {
            repository: path.parent().unwrap().join("project"),
            path: path.parent().unwrap().join("project"),
        };
        let crate::agent_execution::HostedSession::Chat(chat) = session.clone() else {
            panic!("chat caller")
        };
        model
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                closed: vec![],
                panes: vec![Pane {
                    id: CardId::from_uuid(parent.agent.id.as_uuid()),
                    title: "Caller".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.0,
                    kind: CardKind::Chat(chat),
                    scope: Some(path.parent().unwrap().join("project")),
                }],
            })
            .unwrap();
        (
            model.retained_agent_authority(session.id()).unwrap(),
            scope,
            model.model().unwrap().channels.len(),
        )
    };
    let Reply::Directory(directory) =
        process(&host, authority.clone(), Operation::Directory(Scope::All)).unwrap()
    else {
        panic!("directory")
    };
    let caller = directory.caller.unwrap();
    assert!(caller.channel.is_none());
    assert_eq!(caller.view_scope, Some(scope.clone()));
    let mut child_request = request;
    child_request.idempotency_key = "visible-child".into();
    let Destination::New(spec) = &mut child_request.destination else {
        unreachable!()
    };
    spec.target.placement = caller.view_scope;
    // Requested path spelling is retained for retry; ownership is checked
    // after the host resolves it to the registered project key.
    let alias = path.parent().unwrap().join("project-alias");
    std::os::unix::fs::symlink(&spec.target.repository, &alias).unwrap();
    spec.target.repository = alias;
    let child = submit(&host, &authority, child_request).unwrap();
    assert_eq!(child.agent.placement, scope);
    assert!(child.agent.channel().is_none());
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .model()
            .unwrap()
            .channels
            .len(),
        channels
    );
}

#[test]
fn directory_lists_live_sessions_with_relations_and_without_granting_control() {
    use crate::agent_execution::directory::{Owner, Relation, Scope};
    if isolated("directory_lists_live_sessions_with_relations_and_without_granting_control")
        .is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let project = path.parent().unwrap().join("project");
    let parent = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert_eq!(settled(&host, parent.run.id).state, State::Succeeded);
    let parent_session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(parent.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let parent_authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(parent_session)
        .unwrap();
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &parent_authority, child_request).unwrap();
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    let child_session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(child.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let child_authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(child_session)
        .unwrap();

    let read = |authority: &Authority, scope: Scope| {
        let Reply::Directory(directory) =
            process(&host, authority.clone(), Operation::Directory(scope)).unwrap()
        else {
            panic!("directory reply")
        };
        directory
    };
    let key = project.canonicalize().unwrap();

    let directory = read(&child_authority, Scope::Containing(project.join(".")));
    assert_eq!(directory.scope, Scope::Project(key.clone()));
    let me = directory.caller.expect("the caller is a live session");
    assert_eq!((me.id, me.relation), (child_session, Relation::Myself));
    assert_eq!(me.project.as_deref(), Some(key.as_path()));
    assert_eq!(me.project_name.as_deref(), Some("Service fixture"));
    assert_eq!(me.channel, child.agent.channel());
    assert_eq!(me.channel_repository.as_deref(), Some(key.as_path()));
    let delegation = me.delegation.unwrap();
    assert_eq!(
        delegation.owner,
        Owner::Session(crate::agent_execution::Lineage(parent_session))
    );
    assert_eq!(delegation.run_state, Some(State::Succeeded));
    assert_eq!(
        directory
            .sessions
            .iter()
            .map(|entry| (entry.id, entry.relation))
            .collect::<Vec<_>>(),
        vec![(parent_session, Relation::Parent)]
    );

    // The parent sees its child; a user CLI sees both and owns only the parent.
    let directory = read(&parent_authority, Scope::All);
    assert_eq!(directory.caller.unwrap().id, parent_session);
    assert_eq!(directory.sessions[0].id, child_session);
    assert_eq!(directory.sessions[0].relation, Relation::Child);
    let directory = read(&Authority::default(), Scope::Project(project.clone()));
    assert!(directory.caller.is_none());
    let relations: std::collections::BTreeMap<_, _> = directory
        .sessions
        .iter()
        .map(|entry| (entry.id, entry.relation))
        .collect();
    assert_eq!(relations[&parent_session], Relation::Child);
    assert_eq!(relations[&child_session], Relation::Peer);

    // Outside every open project, the read widens rather than coming back empty.
    let elsewhere = path.parent().unwrap().join("tmp");
    let directory = read(&Authority::default(), Scope::Containing(elsewhere.clone()));
    assert_eq!(directory.scope, Scope::All);
    assert_eq!(directory.sessions.len(), 2);
    assert!(
        process(
            &host,
            Authority::default(),
            Operation::Directory(Scope::Project(elsewhere))
        )
        .is_err()
    );

    // Shared pane membership overrides the original delegated placement.
    // The registry refuses a duplicate placement of this same resource.
    let HostedSession::Chat(parent_source) = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(parent.agent.id)
        .unwrap()
        .session
        .unwrap()
    else {
        panic!("chat");
    };
    let pane = |id| crate::workspace_model::views::Pane {
        id,
        title: "Parent".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: crate::model::CardKind::Chat(parent_source.clone()),
        scope: None,
    };
    registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .apply(&ModelEdit::SeedView {
            scope: child.agent.placement.clone(),
            panes: vec![pane(crate::model::CardId::new())],
            closed: vec![],
        })
        .unwrap();
    assert_eq!(
        read(&parent_authority, Scope::All).caller.unwrap().channel,
        child.agent.channel()
    );
    registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .apply(&ModelEdit::SeedView {
            scope: parent.agent.placement.clone(),
            panes: vec![pane(crate::model::CardId::new())],
            closed: vec![],
        })
        .unwrap();
    assert_eq!(
        read(&parent_authority, Scope::All).caller.unwrap().channel,
        child.agent.channel()
    );

    // Seeing a session confers no control over it.
    assert!(status(&host, &child_authority, parent.agent.id).is_err());
    assert!(close(&host, &Authority::default(), child.agent.id).is_err());
}

#[test]
fn only_an_agents_owner_reads_its_requests_and_controls_it() {
    if isolated("only_an_agents_owner_reads_its_requests_and_controls_it").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let owner = Authority::default();
    let agent = accepted.agent.id;
    let requests = |authority: &Authority, operation| -> Result<Vec<chat::Request>, String> {
        match process(&host, authority.clone(), operation)? {
            Reply::Requests(requests) => Ok(requests),
            reply => panic!("{reply:?}"),
        }
    };
    assert!(
        requests(&owner, Operation::Requests(agent))
            .unwrap()
            .is_empty()
    );
    let error = requests(
        &owner,
        Operation::Control {
            agent,
            command: Control::Interrupt {
                including_subagents: false,
            },
        },
    )
    .unwrap_err();
    assert!(error.contains("no active run"), "{error}");
    // Controlling on its owner's behalf is not a person taking the agent over.
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_is_interactive(agent)
            .unwrap()
    );

    let parent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap();
    let child_authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(parent.session.as_ref().unwrap().id())
        .unwrap();
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &child_authority, child_request).unwrap();
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    for operation in [
        Operation::Requests(child.agent.id),
        Operation::Control {
            agent: child.agent.id,
            command: Control::Interrupt {
                including_subagents: false,
            },
        },
    ] {
        let error = requests(&owner, operation).unwrap_err();
        assert!(error.contains("does not own"), "{error}");
    }
    assert!(
        close(&host, &child_authority, child.agent.id)
            .unwrap()
            .closed
    );
    assert!(requests(&child_authority, Operation::Requests(child.agent.id)).is_err());
}

/// Polls until the agent's latest run waits on a request, and returns them.
fn pending_requests(host: &SharedHost, agent: AgentId) -> Vec<chat::Request> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = process(host, Authority::default(), Operation::Requests(agent));
        if let Ok(Reply::Requests(requests)) = &reply
            && !requests.is_empty()
        {
            return requests.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the agent never asked: {reply:?} {:?}",
            status(host, &Authority::default(), agent)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_owner_declines_a_background_request_its_settled_run_left_but_cannot_approve_it() {
    if isolated("an_owner_declines_a_background_request_its_settled_run_left_but_cannot_approve_it")
        .is_some()
    {
        return;
    }
    let (host, mut request, path) = fixture();
    let owner = Authority::default();
    request.prompt = "BACKGROUND_PERMISSION while the turn ends".into();
    let accepted = submit(&host, &owner, request).unwrap();
    let agent = accepted.agent.id;
    let control = |command| -> Result<Vec<chat::Request>, String> {
        match process(&host, owner.clone(), Operation::Control { agent, command })? {
            Reply::Requests(requests) => Ok(requests),
            reply => panic!("{reply:?}"),
        }
    };
    let answer = |request_id: &str, action: &str| Control::Answer {
        request_id: request_id.into(),
        action: action.into(),
        answers: Vec::new(),
    };
    let asked = pending_requests(&host, agent);
    let [asked] = asked.as_slice() else {
        panic!("{asked:?}")
    };
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    assert_eq!(
        pending_requests(&host, agent).len(),
        1,
        "the decision outlives the turn"
    );

    let session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap()
        .session
        .unwrap()
        .id();
    let handle = host.lock().unwrap().chats[&session].handle.clone();
    let stale = handle.snapshot(None).unwrap().generation.wrapping_sub(1);
    assert!(
        handle.try_close_if_idle(stale).is_err(),
        "stale generation must not hide a pending permission request"
    );
    assert!(!handle.is_completed());

    let error = control(answer(&asked.id, "allow")).unwrap_err();
    assert!(error.contains("only decline"), "{error}");
    let error = control(Control::Interrupt {
        including_subagents: true,
    })
    .unwrap_err();
    assert!(error.contains("no active run"), "{error}");
    let root = path.parent().unwrap();
    assert!(!root.join("decisions").exists(), "nothing was approved");

    assert!(control(answer(&asked.id, "deny")).unwrap().is_empty());
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(root.join("decisions")).unwrap_or_default() != "deny\n" {
        assert!(
            Instant::now() < deadline,
            "the denial never reached the provider"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    // Resolved, the session is idle again and takes its owner's next prompt.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(
        process(&host, owner.clone(), Operation::Requests(agent)),
        Ok(Reply::Requests(requests)) if requests.is_empty()
    ) {
        assert!(Instant::now() < deadline, "the request was never resolved");
        std::thread::sleep(Duration::from_millis(10));
    }
    let followup = submit(
        &host,
        &owner,
        Submit {
            name: None,
            destination: Destination::Existing(agent),
            prompt: "after the denial".into(),
            idempotency_key: "after-denial".into(),
        },
    )
    .unwrap();
    let after = settled(&host, followup.run.id);
    assert_eq!(after.state, State::Succeeded, "{after:?}");
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_is_interactive(agent)
            .unwrap(),
        "the owner keeps the agent"
    );
}

#[test]
fn a_late_between_turn_interrupt_ack_cannot_cancel_the_next_owned_run() {
    if isolated("a_late_between_turn_interrupt_ack_cannot_cancel_the_next_owned_run").is_some() {
        return;
    }
    between_turn_interrupt_ack_fixture(false);
}

#[cfg(target_os = "linux")]
#[test]
fn a_request_arriving_after_result_cannot_lend_its_interrupt_ack_to_the_next_owned_run() {
    owned_late_decision::run(
        "a_request_arriving_after_result_cannot_lend_its_interrupt_ack_to_the_next_owned_run",
        || between_turn_interrupt_ack_fixture(true),
    );
}

fn between_turn_interrupt_ack_fixture(request_after_result: bool) {
    let (host, request, path) = fixture();
    let root = path.parent().unwrap();
    // A real private provider process withholds the idle interrupt response
    // until the next prompt has returned its result and awaits its own ACK.
    let script = r#"#!/bin/sh
set -eu
printf 'spawn\n' >> "$HOME/../spawns"
n=0
old=
current=
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      case "$frame" in
        *'"subtype":"interrupt"'*)
          if [ -z "$old" ]; then
            old=$id
            printf 'received\n' > "$HOME/../idle-interrupt"
          else
            current=$id
            printf '{"type":"result","uuid":"next-result","is_error":false}\n'
            printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$old"
            # Observing this request proves the reader consumed the preceding
            # result and stale ACK; a filesystem marker alone would not.
            printf '{"type":"control_request","request_id":"ack-barrier","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"barrier","input":{"command":"true"}}}\n'
          fi
          ;;
        *) printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id" ;;
      esac
      ;;
    *'"type":"control_response"'*)
      case "$frame" in
        *'"ack-barrier"'*)
          printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"already settled"}}\n' "$current"
          ;;
      esac
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      if [ "$n" -eq 1 ]; then
        printf '{"type":"control_request","request_id":"background-first","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"first","input":{"command":"true"}}}\n'
        printf '{"type":"result","uuid":"first-result","is_error":false}\n'
      else
        printf 'started\n' > "$HOME/../next-prompt"
      fi
      ;;
  esac
done
"#;
    let script = if request_after_result {
        let request = "        printf '{\"type\":\"control_request\",\"request_id\":\"background-first\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"tool_use_id\":\"first\",\"input\":{\"command\":\"true\"}}}\\n'\n";
        let result = "        printf '{\"type\":\"result\",\"uuid\":\"first-result\",\"is_error\":false}\\n'\n";
        let original = format!("{request}{result}");
        assert!(
            script.contains(&original),
            "fixture must reverse the actual decoder frames"
        );
        script.replacen(&original, &format!("{result}{request}"), 1)
    } else {
        script.to_owned()
    };
    std::fs::write(root.join("bin/claude"), script).unwrap();
    let owner = Authority::default();
    let accepted = submit(&host, &owner, request).unwrap();
    let agent = accepted.agent.id;
    assert_eq!(pending_requests(&host, agent)[0].id, "background-first");
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let (record, _) = status(&host, &owner, agent).unwrap();
    let original_session = record.session.clone();
    let handle = host
        .lock()
        .unwrap()
        .chats
        .get(&record.session.as_ref().unwrap().id())
        .unwrap()
        .handle
        .clone();
    // The native provider controls can interrupt background work between
    // managed turns. The owner API correctly refuses general idle interrupts.
    handle.command(Command::Interrupt).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("idle-interrupt").exists() {
        assert!(
            Instant::now() < deadline,
            "the provider never received the idle interrupt"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let answer = |request_id: &str| Control::Answer {
        request_id: request_id.into(),
        action: "deny".into(),
        answers: vec![],
    };
    process(
        &host,
        owner.clone(),
        Operation::Control {
            agent,
            command: answer("background-first"),
        },
    )
    .unwrap();
    assert!(
        handle.requests().is_empty(),
        "the settled decision must retire before the next prompt"
    );
    let followup = submit(
        &host,
        &owner,
        Submit {
            name: None,
            destination: Destination::Existing(agent),
            prompt: "next owned prompt".into(),
            idempotency_key: "next-after-idle-interrupt".into(),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("next-prompt").exists() {
        assert!(
            Instant::now() < deadline,
            "the provider never received the next prompt"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    process(
        &host,
        owner.clone(),
        Operation::Control {
            agent,
            command: Control::Interrupt {
                including_subagents: false,
            },
        },
    )
    .unwrap();
    let barrier = pending_requests(&host, agent);
    assert_eq!(barrier[0].id, "ack-barrier");
    let (_, waiting) = status(&host, &owner, agent).unwrap();
    assert_eq!(waiting.id, followup.run.id);
    assert!(
        !waiting.state.is_terminal(),
        "the stale ACK settled the next run: {waiting:?}"
    );
    process(
        &host,
        owner.clone(),
        Operation::Control {
            agent,
            command: answer("ack-barrier"),
        },
    )
    .unwrap();
    assert_eq!(settled(&host, followup.run.id).state, State::Succeeded);
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let record = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap();
    assert_eq!(record.session, original_session);
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_is_interactive(agent)
            .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("spawns")).unwrap(),
        "spawn\n"
    );
    assert!(close(&host, &owner, agent).unwrap().closed);
}

#[test]
fn an_owner_answers_and_interrupts_only_its_own_active_run() {
    if isolated("an_owner_answers_and_interrupts_only_its_own_active_run").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let owner = Authority::default();
    request.prompt = "ASK_PERMISSION before running".into();
    let accepted = submit(&host, &owner, request).unwrap();
    let agent = accepted.agent.id;
    let control = |command| -> Result<Vec<chat::Request>, String> {
        match process(&host, owner.clone(), Operation::Control { agent, command })? {
            Reply::Requests(requests) => Ok(requests),
            reply => panic!("{reply:?}"),
        }
    };
    let answer = |request_id: &str, action: &str, answers| Control::Answer {
        request_id: request_id.into(),
        action: action.into(),
        answers,
    };

    let asked = pending_requests(&host, agent);
    let [asked] = asked.as_slice() else {
        panic!("{asked:?}")
    };
    assert_eq!(
        status(&host, &owner, agent).unwrap().1.state,
        State::Waiting
    );
    let error = control(answer(&asked.id, "always", Vec::new())).unwrap_err();
    assert!(error.contains("accepts the actions allow, deny"), "{error}");
    let error = control(answer(
        &asked.id,
        "allow",
        vec![chat::Answer {
            field: "reason".into(),
            values: vec!["because".into()],
        }],
    ))
    .unwrap_err();
    assert!(error.contains("has no field reason"), "{error}");
    assert!(
        control(answer(&asked.id, "allow", Vec::new()))
            .unwrap()
            .is_empty()
    );
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let root = path.parent().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("decisions")).unwrap(),
        "allow\n"
    );
    let error = control(Control::Interrupt {
        including_subagents: false,
    })
    .unwrap_err();
    assert!(error.contains("no active run"), "{error}");

    // Once a person takes the agent over, the question its next turn raises
    // is theirs, though its owner may still see it.
    let followup = submit(
        &host,
        &owner,
        Submit {
            name: None,
            destination: Destination::Existing(agent),
            prompt: "ASK_PERMISSION again".into(),
            idempotency_key: "second".into(),
        },
    )
    .unwrap();
    let asked = pending_requests(&host, agent);
    registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .claim_agent_interaction(agent, None)
        .unwrap();
    for command in [
        answer(&asked[0].id, "deny", Vec::new()),
        Control::Interrupt {
            including_subagents: false,
        },
    ] {
        let error = control(command).unwrap_err();
        assert!(error.contains("took this agent over"), "{error}");
    }
    assert_eq!(pending_requests(&host, agent), asked);
    assert_eq!(status(&host, &owner, agent).unwrap().1.id, followup.run.id);
    // The person answers it at the agent's own controls.
    let (agent_record, _) = status(&host, &owner, agent).unwrap();
    let handle = host
        .lock()
        .unwrap()
        .chats
        .get(&agent_record.session.as_ref().unwrap().id())
        .unwrap()
        .handle
        .clone();
    handle
        .command(Command::Answer {
            request_id: asked[0].id.clone(),
            action: "deny".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert_eq!(settled(&host, followup.run.id).state, State::Succeeded);
    assert_eq!(
        std::fs::read_to_string(root.join("decisions")).unwrap(),
        "allow\ndeny\n"
    );
}

#[test]
fn a_project_is_found_by_its_name() {
    if isolated("a_project_is_found_by_its_name").is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let project = path.parent().unwrap().join("project");
    let read = |scope| match process(&host, Authority::default(), Operation::Directory(scope)) {
        Ok(Reply::Directory(directory)) => Ok(directory.scope),
        Ok(reply) => panic!("{reply:?}"),
        Err(error) => Err(error),
    };
    use crate::agent_execution::directory::Scope;
    assert_eq!(
        read(Scope::Project("service FIXTURE".into())).unwrap(),
        Scope::Project(project.clone())
    );
    let error = read(Scope::Project("elsewhere".into())).unwrap_err();
    assert!(
        error.contains("No open project is named elsewhere"),
        "{error}"
    );
    assert_eq!(
        read(Scope::Project(project.clone())).unwrap(),
        Scope::Project(project)
    );
}

#[test]
fn a_relaunched_parent_keeps_its_agents_and_a_rival_keeps_its_own() {
    use crate::agent_execution::{
        Lineage,
        directory::{Relation, Scope},
    };
    use crate::model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession};
    if isolated("a_relaunched_parent_keeps_its_agents_and_a_rival_keeps_its_own").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    host.lock().unwrap().register_client(1);
    // Each shell runs until its marker file appears.
    let spawn =
        |id: TerminalId, recovery_origin: Option<TerminalId>, predecessor: Option<TerminalId>| {
            let marker = root.join(format!("exit-{id}"));
            let recipe = LaunchRecipe {
                unset_environment: Default::default(),
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    format!("while [ ! -e '{}' ]; do sleep 0.02; done", marker.display()),
                ],
                working_directory: root.join("project"),
                environment: Default::default(),
            };
            crate::host::state::spawn_terminal(
                &host,
                1,
                TerminalSession {
                    owned_codex_listener: None,
                    recovery_origin,
                    predecessor,
                    id,
                    launch: recipe.clone(),
                    transcript_path: None,
                    resume_hint: None,
                    continuity: TerminalContinuity::RelaunchAvailable {
                        recipe,
                        last_exit: None,
                    },
                },
                Default::default(),
            )
            .unwrap();
            registry(&host)
                .unwrap()
                .lock()
                .unwrap()
                .retained_agent_authority(id)
                .unwrap()
        };
    let exit = |id: TerminalId| {
        std::fs::write(root.join(format!("exit-{id}")), b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.lock().unwrap().any_live(&[id]) {
            assert!(Instant::now() < deadline, "the shell never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let list = |authority: &Authority| {
        let Reply::Listed(listed) = process(&host, authority.clone(), Operation::List).unwrap()
        else {
            panic!("list reply")
        };
        listed
            .into_iter()
            .map(|summary| summary.agent.id)
            .collect::<Vec<_>>()
    };

    let first = TerminalId::new();
    let first_authority = spawn(first, None, None);
    let mut rival_request = request.clone();
    rival_request.idempotency_key = "rival child".into();
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &first_authority, child_request).unwrap();
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);

    // A relaunch while the original still runs starts a lineage of its own.
    // This one was saved before predecessors were recorded, so it names only
    // its root.
    let rival = TerminalId::new();
    let rival_authority = spawn(rival, Some(first), None);
    assert!(list(&rival_authority).is_empty());
    assert!(status(&host, &rival_authority, child.agent.id).is_err());

    // Once it has exited, the next relaunch takes its agents over.
    exit(first);
    // The exited original can no longer act at all.
    assert!(process(&host, first_authority.clone(), Operation::List).is_err());
    let second = TerminalId::new();
    let second_authority = spawn(second, Some(first), Some(first));
    assert_eq!(list(&second_authority), vec![child.agent.id]);
    assert!(status(&host, &second_authority, child.agent.id).is_ok());

    let child_session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(child.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let child_authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(child_session)
        .unwrap();
    let Reply::Directory(directory) =
        process(&host, child_authority, Operation::Directory(Scope::All)).unwrap()
    else {
        panic!("directory reply")
    };
    let relation = |id: TerminalId| {
        directory
            .sessions
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| (entry.relation, entry.lineage))
    };
    assert_eq!(relation(second), Some((Relation::Parent, Lineage(first))));
    // The rival is a plain shell that owns nothing, so it is not listed.
    assert_eq!(relation(rival), None);

    // The rival's own lineage passes to its successor, though the chain's
    // root is the original.
    let rival_child = submit(&host, &rival_authority, rival_request).unwrap();
    assert_eq!(settled(&host, rival_child.run.id).state, State::Succeeded);
    exit(rival);
    let rival_next = TerminalId::new();
    let rival_next_authority = spawn(rival_next, Some(first), Some(rival));
    assert_eq!(list(&rival_next_authority), vec![rival_child.agent.id]);
    assert_eq!(list(&second_authority), vec![child.agent.id]);

    close(&host, &second_authority, child.agent.id).unwrap();
    close(&host, &rival_next_authority, rival_child.agent.id).unwrap();
    exit(second);
    exit(rival_next);
}

#[test]
fn full_mailbox_replies_are_retryable_without_reclassifying_invalid_requests() {
    use crate::{
        agent_execution::{
            AcceptanceError, Lineage,
            mailbox::{
                Inbox, MAX_PENDING, Message, Outgoing, Party, Receipt, State as Delivery, To,
            },
        },
        model::MessageId,
        terminal::ipc::{ClientRequest, ProtocolErrorCode, ServerResponse},
    };

    let directory = tempfile::tempdir().unwrap();
    let host = crate::host::service::test_host();
    host.lock()
        .unwrap()
        .install_model(Registry::at(&directory.path().join("mailbox.sqlite3")).unwrap());
    let store = registry(&host).unwrap();
    let recipient = Party {
        lineage: Some(Lineage(TerminalId::new())),
        session: None,
        title: Some("Busy recipient".into()),
    };
    let message = |from, to, created_ms| Message {
        id: MessageId::new(),
        from,
        to,
        reply_to: None,
        body: "Retain this fixture message".into(),
        wait_until_ms: None,
        state: Delivery::Pending,
        created_ms,
    };
    let incoming = message(recipient.clone(), Party::user(), 0);
    store.lock().unwrap().post_message(&incoming).unwrap();
    for at in 0..MAX_PENDING {
        store
            .lock()
            .unwrap()
            .post_message(&message(Party::user(), recipient.clone(), at as u64))
            .unwrap();
    }
    let query = Inbox {
        all: false,
        replies_to: None,
    };
    let pending = || {
        store
            .lock()
            .unwrap()
            .peek_messages(recipient.lineage, &query)
            .unwrap()
    };
    let before = pending();
    let outgoing = Outgoing {
        to: To::Sender(incoming.id.to_string()),
        body: "A reply must wait for an actual mailbox slot".into(),
        wait_ms: None,
    };
    // Synthetic User scope resolves normally in this temporary registry. This
    // fixture neither removes inherited authority nor proves a deployed caller.
    assert!(matches!(
        process_request(
            &host,
            Authority::default(),
            Operation::SendMessage(outgoing.clone())
        ),
        Err(AcceptanceError::MailboxFull(_))
    ));
    let invoke = |authority, outgoing| {
        crate::host::client::process_request(
            &host,
            1,
            ClientRequest::Agent {
                authority,
                operation: Operation::SendMessage(outgoing),
            },
        )
        .map(|(reply, _, _)| reply)
    };
    let refusal = invoke(Authority::default(), outgoing.clone()).unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::QueueFull);
    assert!(refusal.retryable);
    assert!(refusal.message.contains(&recipient.short().unwrap()));
    assert!(refusal.message.contains(&MAX_PENDING.to_string()));
    assert!(refusal.message.contains("once it has read them"));

    let mut empty = outgoing.clone();
    empty.body.clear();
    let mut unknown = outgoing.clone();
    unknown.to = To::Session(TerminalId::new().to_string());
    let mut foreign_reply = outgoing.clone();
    foreign_reply.to = To::Sender(MessageId::new().to_string());
    for (authority, outgoing) in [
        (Authority::default(), empty),
        (Authority::default(), unknown),
        (Authority::default(), foreign_reply),
        // A fresh unregistered fixture token must not disclose queue capacity.
        (
            Authority::session(uuid::Uuid::new_v4().to_string()),
            outgoing.clone(),
        ),
    ] {
        let refusal = invoke(authority, outgoing).unwrap_err();
        assert_eq!(refusal.code, ProtocolErrorCode::InvalidRequest);
        assert!(!refusal.retryable);
    }
    assert_eq!(pending(), before, "no refusal inserts or consumes mail");
    assert_eq!(
        store.lock().unwrap().message(incoming.id).unwrap(),
        Some(incoming.clone())
    );
    // Only an exact recipient receipt frees a slot. The service has no courier
    // or provider here; explicit resubmission must durably fill just that slot.
    store
        .lock()
        .unwrap()
        .acknowledge_messages(
            recipient.lineage,
            None,
            &Receipt {
                ids: vec![before[0].id],
            },
            now(),
        )
        .unwrap();
    let ServerResponse::Agent(Reply::Sent(sent)) =
        invoke(Authority::default(), outgoing.clone()).unwrap()
    else {
        panic!("one exact read must permit one explicit reply");
    };
    assert_eq!(sent.message.reply_to, Some(incoming.id));
    let after = pending();
    assert_eq!(after.len(), MAX_PENDING);
    assert!(after.contains(&sent.message));
    assert!(before[1..].iter().all(|message| after.contains(message)));
    let refusal = invoke(Authority::default(), outgoing.clone()).unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::QueueFull);
    assert!(refusal.retryable);
    assert_eq!(pending(), after);

    host.lock().unwrap().execution_registry = None;
    let refusal = invoke(Authority::default(), outgoing).unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::InvalidRequest);
    assert!(!refusal.retryable);
}

#[test]
fn disconnected_blocked_inbox_poll_keeps_mail_pending() {
    use crate::{
        agent_execution::{
            Lineage,
            mailbox::{Inbox, Message, Party, Receipt, State as Delivery},
        },
        model::MessageId,
        terminal_ipc::{read_frame, write_frame},
    };
    use std::{os::unix::net::UnixStream, sync::mpsc};

    if isolated("disconnected_blocked_inbox_poll_keeps_mail_pending").is_some() {
        return;
    }
    let (host, _, _) = fixture();
    let registry = registry(&host).unwrap();
    let message = Message {
        id: MessageId::new(),
        from: Party {
            lineage: Some(Lineage(TerminalId::new())),
            session: None,
            title: None,
        },
        to: Party::user(),
        reply_to: None,
        body: "Do not lose this delayed reply".into(),
        wait_until_ms: None,
        state: Delivery::Pending,
        created_ms: now(),
    };
    let mut blocked = registry.lock().unwrap();
    blocked.post_message(&message).unwrap();
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let server_host = Arc::clone(&host);
    let handler = std::thread::spawn(move || {
        let operation: Operation = read_frame(&mut server).unwrap().unwrap();
        started_tx.send(()).unwrap();
        // The registry lock above delays processing until after the polling
        // client disconnects, as a slow daemon request would.
        let reply = process(&server_host, Authority::default(), operation).unwrap();
        assert!(write_frame(&mut server, &reply).is_err());
    });
    let query = Inbox {
        all: false,
        replies_to: None,
    };
    write_frame(&mut client, &Operation::PeekInbox(query.clone())).unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(client);
    drop(blocked);
    handler.join().unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .message(message.id)
            .unwrap()
            .unwrap()
            .state,
        Delivery::Pending
    );
    let Reply::Inbox(retried) =
        process(&host, Authority::default(), Operation::PeekInbox(query)).unwrap()
    else {
        panic!("inbox reply");
    };
    assert_eq!(retried, vec![message.clone()]);
    assert_eq!(
        process(
            &host,
            Authority::default(),
            Operation::AcknowledgeInbox(Receipt {
                ids: vec![message.id]
            })
        )
        .unwrap(),
        Reply::InboxAcknowledged
    );
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .message(message.id)
            .unwrap()
            .unwrap()
            .state,
        Delivery::Read
    );
}

#[test]
fn queued_mail_wakes_when_parent_settles_with_a_running_background_child() {
    use crate::agent_execution::mailbox::{Outgoing, State as Delivery, To};
    if isolated("queued_mail_wakes_when_parent_settles_with_a_running_background_child").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap();
    struct Release(std::path::PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "release");
        }
    }
    let release = Release(root.join("courier-release"));
    request.prompt = "COURIER_BACKGROUND".into();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (session, handle) = loop {
        let session = registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .session;
        if let Some(HostedSession::Chat(session)) = session
            && let Some(handle) = host
                .lock()
                .unwrap()
                .chats
                .get(&session.id)
                .map(|record| record.handle.clone())
            && root.join("courier-busy").exists()
            && handle.background_subagents().len() == 1
        {
            assert!(!handle.is_idle());
            break (session.id, handle);
        }
        assert!(
            Instant::now() < deadline,
            "fixture never reached its gated busy parent"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let Reply::Sent(sent) = process(
        &host,
        Authority::default(),
        Operation::SendMessage(Outgoing {
            to: To::Session(session.to_string()),
            body: "Exact courier delivery after parent result".into(),
            wait_ms: None,
        }),
    )
    .unwrap() else {
        panic!("sent receipt");
    };
    let stored = || {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .message(sent.message.id)
            .unwrap()
            .unwrap()
    };
    let courier = mailbox::Courier::start(&host).unwrap();
    courier.wait_for_scan(0); // Completes before result; never wakes the courier.
    assert_eq!(stored().state, Delivery::Pending);
    assert_eq!(handle.activity(), crate::agent::ActivityState::Working);
    std::fs::write(&release.0, "release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = handle.snapshot(None).unwrap();
        let deliveries = snapshot
            .items
            .iter()
            .filter(|item| {
                matches!(&item.content,
            chat::ItemContent::Delivery { body, .. } if body == &sent.message.body)
            })
            .count();
        let provider_prompts = std::fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count();
        if deliveries == 1 && provider_prompts == 2 && handle.is_idle() {
            assert_eq!(snapshot.activity(), crate::agent::ActivityState::Working);
            assert_eq!(
                snapshot
                    .subagents
                    .iter()
                    .filter(|child| child.status == chat::ToolStatus::Running)
                    .count(),
                1
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "eligible mail missed its parent-ready wake: {} visible deliveries",
            deliveries
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let delivered = stored();
    assert_eq!(delivered.state, Delivery::Delivered);
    assert_eq!(delivered.to.session, Some(session));
    assert_eq!(delivered.to.lineage, sent.message.to.lineage);
    courier.wake_and_wait_for_scan();
    assert_eq!(stored(), delivered);
    assert_eq!(
        std::fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        2,
        "one original prompt and one courier prompt, including after another scan"
    );
    drop(courier);
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
}

#[test]
fn sessions_message_their_parent_peers_and_the_user_and_replies_find_their_askers() {
    use crate::agent_execution::{
        Lineage,
        mailbox::{Inbox, Message, Outgoing, Reach, Sent, State as Delivery, To, short},
    };
    if isolated("sessions_message_their_parent_peers_and_the_user_and_replies_find_their_askers")
        .is_some()
    {
        return;
    }
    let (host, request, _) = fixture();
    let _courier = mailbox::Courier::start(&host).unwrap();
    let session_of = |agent: AgentId| {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(agent)
            .unwrap()
            .session
            .unwrap()
            .id()
    };
    let authority_of = |session: TerminalId| {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_authority(session)
            .unwrap()
    };
    let parent = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert_eq!(settled(&host, parent.run.id).state, State::Succeeded);
    let parent_session = session_of(parent.agent.id);
    let parent_authority = authority_of(parent_session);
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &parent_authority, child_request).unwrap();
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    let child_session = session_of(child.agent.id);
    let child_authority = authority_of(child_session);

    let send = |authority: &Authority, to: To, body: &str, wait_ms: Option<u64>| match process(
        &host,
        authority.clone(),
        Operation::SendMessage(Outgoing {
            to,
            body: body.into(),
            wait_ms,
        }),
    )? {
        Reply::Sent(sent) => Ok::<Sent, String>(sent),
        reply => panic!("unexpected reply {reply:?}"),
    };
    let read = |authority: &Authority, replies_to| {
        let Reply::Inbox(messages) = process(
            &host,
            authority.clone(),
            Operation::Inbox(Inbox {
                all: false,
                replies_to,
            }),
        )
        .unwrap() else {
            panic!("inbox reply")
        };
        messages
    };
    let stored = |message: &Message| {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .message(message.id)
            .unwrap()
            .unwrap()
    };
    let delivered = |message: &Message| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let current = stored(message);
            if current.state == Delivery::Delivered {
                return current;
            }
            assert!(Instant::now() < deadline, "never delivered: {current:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let transcript = |session: TerminalId, expected_sender: &str, expected_body: &str| {
        let handle = host.lock().unwrap().chats[&session].handle.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = handle.snapshot(None).unwrap();
            // The mailbox records its at-most-once delivery claim before
            // enqueueing the actor command. Its old idle snapshot can still
            // be visible then; wait for this exact delivery as well as idle.
            let visible = snapshot.items.iter().any(|item| {
                item.role == chat::Role::User
                    && matches!(&item.content, chat::ItemContent::Delivery {
                        kind: chat::DeliveryKind::Message,
                        sender,
                        body,
                        ..
                    } if sender == expected_sender && body == expected_body)
            });
            if visible && !snapshot.busy && snapshot.ready {
                return snapshot.items;
            }
            assert!(
                Instant::now() < deadline,
                "delivery never became visible in a settled conversation"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // A child reaches the session that started it, which sees who wrote.
    let sent = send(&child_authority, To::Parent, "Tests pass on my side", None).unwrap();
    assert_eq!(sent.reach, Reach::Conversation);
    assert_eq!(sent.message.to.lineage, Some(Lineage(parent_session)));
    assert_eq!(sent.message.from.session, Some(child_session));
    // A delegated agent without a title of its own goes by its channel's.
    assert!(sent.message.from.title.is_some());
    assert_eq!(delivered(&sent.message).to.session, Some(parent_session));
    let shown = transcript(parent_session, &short(child_session), &sent.message.body)
        .into_iter()
        .find_map(|item| match item.content.clone() {
            chat::ItemContent::Delivery {
                kind: chat::DeliveryKind::Message,
                sender,
                body,
                ..
            } => Some((item.role, sender, body)),
            _ => None,
        });
    assert_eq!(
        shown,
        Some((
            chat::Role::User,
            short(child_session),
            "Tests pass on my side".to_owned()
        ))
    );

    // The parent answers by the message's short ID; the reply finds the child.
    let reply = send(
        &parent_authority,
        To::Sender(short(sent.message.id)),
        "Thanks; merge it",
        None,
    )
    .unwrap();
    assert_eq!(reply.message.reply_to, Some(sent.message.id));
    assert_eq!(reply.message.to.lineage, Some(Lineage(child_session)));
    delivered(&reply.message);
    // Only the recipient may answer a message.
    assert!(
        send(
            &parent_authority,
            To::Sender(short(reply.message.id)),
            "x",
            None
        )
        .is_err()
    );

    // A user-started agent's parent is the user, who reads it from the CLI.
    let to_user = send(&parent_authority, To::Parent, "Done", None).unwrap();
    assert_eq!(to_user.message.to.lineage, None);
    assert_eq!(to_user.reach, Reach::Inbox);
    let inbox = read(&Authority::default(), None);
    assert_eq!(
        inbox.iter().map(|m| m.id).collect::<Vec<_>>(),
        [to_user.message.id]
    );
    assert_eq!(inbox[0].state, Delivery::Read);
    assert!(read(&Authority::default(), None).is_empty());

    // The user names a session by a prefix of its ID, its lineage or its agent.
    let by_agent = send(
        &Authority::default(),
        To::Session(child.agent.id.to_string()[..8].to_owned()),
        "Status?",
        None,
    )
    .unwrap();
    assert_eq!(by_agent.message.to.lineage, Some(Lineage(child_session)));
    assert_eq!(by_agent.message.from.lineage, None);
    delivered(&by_agent.message);
    for refused in [
        send(
            &parent_authority,
            To::Session(parent_session.to_string()),
            "me",
            None,
        ),
        send(
            &Authority::default(),
            To::Session("abc".into()),
            "short",
            None,
        ),
        send(
            &Authority::default(),
            To::Session("ffffffff-none".into()),
            "nobody",
            None,
        ),
        send(&Authority::default(), To::Parent, "orphan", None),
    ] {
        assert!(refused.is_err(), "{refused:?}");
    }

    // A reply to a waiting asker is held for it, not given to its conversation.
    let question = send(&child_authority, To::Parent, "Which branch?", Some(60_000)).unwrap();
    delivered(&question.message);
    let answer = send(
        &parent_authority,
        To::Sender(question.message.id.to_string()),
        "main",
        None,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(stored(&answer.message).state, Delivery::Pending);
    assert!(read(&parent_authority, Some(question.message.id)).is_empty());
    let answers = read(&child_authority, Some(question.message.id));
    assert_eq!(
        answers.iter().map(|m| m.id).collect::<Vec<_>>(),
        [answer.message.id]
    );
    assert_eq!(stored(&answer.message).state, Delivery::Read);

    close(&host, &parent_authority, child.agent.id).unwrap();
    close(&host, &Authority::default(), parent.agent.id).unwrap();
}

#[test]
fn an_agents_connection_changes_only_the_terminals_its_lineage_controls() {
    use crate::{
        host::client::process_request,
        model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
        terminal_ipc::{ProtocolErrorCode, TerminalSpawnOptions},
    };
    if isolated("an_agents_connection_changes_only_the_terminals_its_lineage_controls").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    let shell = |id: TerminalId| {
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "cat".into()],
            working_directory: root.join("project"),
            environment: Default::default(),
        };
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        }
    };
    const USER: u64 = 1;
    const AGENT: u64 = 2;
    const PEER: u64 = 3;
    for client in [USER, AGENT, PEER] {
        host.lock().unwrap().register_client(client);
    }
    let request_as = |client: u64, request: ClientRequest| {
        process_request(&host, client, request).map(|(response, _, _)| response)
    };
    let spawn_as = |client: u64, session: TerminalSession| {
        request_as(
            client,
            ClientRequest::Spawn {
                session,
                options: TerminalSpawnOptions::default(),
            },
        )
    };
    let attach = |client: u64, terminal_id: TerminalId| {
        request_as(
            client,
            ClientRequest::Attach {
                terminal_id,
                after_generation: 0,
                after_event_sequence: 0,
            },
        )
        .unwrap();
    };
    let input = |client: u64, terminal_id: TerminalId| {
        request_as(
            client,
            ClientRequest::Input {
                terminal_id,
                bytes: b"x".to_vec(),
            },
        )
    };
    let denied = |result: Result<ServerResponse, crate::terminal_ipc::ProtocolError>| {
        assert_eq!(
            result.unwrap_err().code,
            ProtocolErrorCode::PermissionDenied
        );
    };
    let authority = |session: TerminalId| {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_authority(session)
            .unwrap()
    };

    // The user starts two agent sessions; each connection acts for one.
    let (agent, peer) = (TerminalId::new(), TerminalId::new());
    for session in [agent, peer] {
        spawn_as(USER, shell(session)).unwrap();
    }
    let authenticate = |client: u64, session: TerminalId| {
        request_as(
            client,
            ClientRequest::Authenticate {
                authority: authority(session),
            },
        )
    };
    authenticate(AGENT, agent).unwrap();
    authenticate(PEER, peer).unwrap();
    denied(authenticate(AGENT, peer));

    // An agent changes its own session and the terminals it started.
    attach(AGENT, agent);
    input(AGENT, agent).unwrap();
    let started = TerminalId::new();
    spawn_as(AGENT, shell(started)).unwrap();
    input(AGENT, started).unwrap();
    request_as(
        AGENT,
        ClientRequest::Resize {
            terminal_id: started,
            size: crate::terminal_ipc::WireTerminalSize {
                columns: 100,
                rows: 30,
                cell_width_px: 9,
                cell_height_px: 18,
            },
        },
    )
    .unwrap();

    // It reads a peer's terminal but cannot type into, resize or close it.
    attach(AGENT, peer);
    assert!(matches!(
        request_as(
            AGENT,
            ClientRequest::Snapshot {
                terminal_id: peer,
                after_generation: 0,
            },
        )
        .unwrap(),
        ServerResponse::Snapshot { .. }
    ));
    denied(input(AGENT, peer));
    denied(request_as(
        AGENT,
        ClientRequest::ShutdownTerminal { terminal_id: peer },
    ));
    denied(request_as(
        AGENT,
        ClientRequest::CloseTerminalChecked {
            terminal_id: peer,
            allow_active: true,
        },
    ));
    // Nor what the peer started.
    attach(PEER, started);
    denied(input(PEER, started));

    // An agent may start a terminal, not take over another lineage by naming
    // a predecessor or reusing a session's ID.
    let mut relaunch = shell(TerminalId::new());
    relaunch.predecessor = Some(peer);
    denied(spawn_as(AGENT, relaunch));
    assert_eq!(
        spawn_as(AGENT, shell(peer)).unwrap_err().code,
        ProtocolErrorCode::TerminalAlreadyExists
    );
    // Everything outside the terminal surface stays with the user.
    denied(request_as(AGENT, ClientRequest::ChatList));
    denied(request_as(AGENT, ClientRequest::Shutdown));
    // Including a request that would carry the user's own authority.
    denied(request_as(
        AGENT,
        ClientRequest::Agent {
            authority: Authority::default(),
            operation: Operation::Directory(crate::agent_execution::directory::Scope::All),
        },
    ));

    // A delegated agent's session is its owner's to change, not the reverse.
    let mut delegated = request;
    delegated.idempotency_key = "delegated".into();
    let child = submit(&host, &authority(agent), delegated).unwrap();
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    let child_session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(child.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let change = |client: u64, terminal_id: TerminalId| {
        super::terminals::admit(
            &host,
            client,
            &ClientRequest::Input {
                terminal_id,
                bytes: Vec::new(),
            },
        )
    };
    change(AGENT, child_session).unwrap();
    change(PEER, child_session).unwrap_err();
    const CHILD: u64 = 4;
    host.lock().unwrap().register_client(CHILD);
    request_as(
        CHILD,
        ClientRequest::Authenticate {
            authority: authority(child_session),
        },
    )
    .unwrap();
    change(CHILD, child_session).unwrap();
    assert_eq!(
        change(CHILD, agent).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );

    // The user changes every terminal, and an agent's connection forgets
    // whom it acted for once it disconnects.
    for terminal in [agent, peer, started] {
        attach(USER, terminal);
        input(USER, terminal).unwrap();
    }
    host.lock().unwrap().unregister_client(AGENT);
    assert!(!host.lock().unwrap().agent_clients.contains_key(&AGENT));
    close(&host, &authority(agent), child.agent.id).unwrap();
    for terminal in [agent, peer, started] {
        request_as(
            USER,
            ClientRequest::ShutdownTerminal {
                terminal_id: terminal,
            },
        )
        .unwrap();
    }
}

#[test]
fn unavailable_retained_startup_snapshot_preserves_owner_and_returns_interrupted() {
    let (handle, _) = chat::runtime::Handle::fixture_remote_attachment_pair();
    let identity = handle.actor_identity().unwrap();
    let result = provider_launch_snapshot(&handle);
    assert!(
        matches!(result, Err((State::Interrupted, reason)) if reason.contains("ownership was preserved"))
    );
    assert_eq!(handle.actor_identity(), Some(identity));
    assert!(handle.owns_work());
    assert!(!handle.is_completed());
}

#[test]
fn execution_gates_reclaim_failed_and_repeated_close_churn() {
    if isolated("execution_gates_reclaim_failed_and_repeated_close_churn").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    // Accept without dispatch: closing this agent exercises durable retirement
    // without starting a provider, and subsequent closes use the repair path.
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
    for _ in 0..256 {
        assert!(close(&host, &Authority::default(), AgentId::new()).is_err());
        assert!(
            close(&host, &Authority::default(), accepted.agent.id)
                .unwrap()
                .closed
        );
        assert!(host.lock().unwrap().execution_gates.len() <= 1);
    }
    assert!(
        host.lock()
            .unwrap()
            .execution_gates
            .values()
            .all(|gate| gate.upgrade().is_none())
    );
    assert!(!path.parent().unwrap().join("spawns").exists());
}

#[test]
fn execution_gates_preserve_waiters_during_reclamation() {
    if isolated("execution_gates_preserve_waiters_during_reclamation").is_some() {
        return;
    }
    let (host, _, _) = fixture();
    let agent = AgentId::new();
    let holder = agent_gate(&host, agent);
    let held = holder.lock().unwrap();
    let (ready, waiting) = std::sync::mpsc::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let other = host.clone();
    let thread = std::thread::spawn(move || {
        let gate = agent_gate(&other, agent);
        ready.send(Arc::downgrade(&gate)).unwrap();
        let _held = gate.lock().unwrap();
        finished.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let waiter = waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    for _ in 0..256 {
        drop(agent_gate(&host, AgentId::new()));
        let current = agent_gate(&host, agent);
        assert!(Arc::ptr_eq(&holder, &current));
        assert!(current.try_lock().is_err());
        assert!(host.lock().unwrap().execution_gates.len() <= 2);
    }
    drop(held);
    drop(holder);
    // Only the waiter now owns the gate. A fresh lookup must still join it.
    let current = agent_gate(&host, agent);
    assert!(Arc::ptr_eq(&waiter.upgrade().unwrap(), &current));
    finish.send(()).unwrap();
    thread.join().unwrap();
    drop(current);
    assert!(waiter.upgrade().is_none());
    drop(agent_gate(&host, AgentId::new()));
    assert!(!host.lock().unwrap().execution_gates.contains_key(&agent));
}

#[test]
fn execution_gates_keep_existing_callers_after_successful_close() {
    if isolated("execution_gates_keep_existing_callers_after_successful_close").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    let pending = agent_gate(&host, accepted.agent.id);
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
    let next = agent_gate(&host, accepted.agent.id);
    assert!(Arc::ptr_eq(&pending, &next));
    let held = pending.lock().unwrap();
    assert!(next.try_lock().is_err());
    drop(held);
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(Arc::ptr_eq(&pending, &agent_gate(&host, accepted.agent.id)));
}

/// Seeds an agent's pane as a console over its running native provider, the
/// way `--surface terminal` shows it.
fn shown_on_console(host: &SharedHost, agent: AgentId) -> crate::workspace_model::views::Pane {
    let store = registry(host).unwrap();
    let mut store = store.lock().unwrap();
    let agent = store.agent(agent).unwrap();
    let HostedSession::Chat(source) = agent.session.as_ref().unwrap() else {
        unreachable!()
    };
    let proof = source.proof.clone().unwrap();
    let mut recipe = source.launch.recipe.clone();
    recipe.program = "/bin/cat".into();
    recipe.arguments = vec!["agent".into(), "console".into(), agent.id.to_string()];
    let console = crate::model::TerminalSession {
        id: source.id,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: recipe,
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        },
    };
    let agent = store
        .bind_agent_console(agent.id, agent.session.as_ref().unwrap(), console.clone())
        .unwrap();
    let pane = crate::workspace_model::views::Pane {
        id: crate::model::CardId::from_uuid(agent.id.as_uuid()),
        title: "Console".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: crate::model::CardKind::Terminal(console),
        scope: None,
    };
    store
        .apply(&ModelEdit::SeedView {
            scope: agent.placement.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
    pane
}

fn shown_kind(host: &SharedHost, pane: crate::model::CardId) -> crate::model::CardKind {
    registry(host)
        .unwrap()
        .lock()
        .unwrap()
        .model()
        .unwrap()
        .views
        .into_iter()
        .flat_map(|view| view.panes)
        .find(|shown| shown.id == pane)
        .unwrap()
        .kind
}

/// Presenting an agent as its chat and back only changes its pane: the same
/// provider answers the next prompt, and nothing is spawned for it.
#[test]
fn presenting_an_agent_keeps_its_provider_and_spawns_nothing() {
    if isolated("presenting_an_agent_keeps_its_provider_and_spawns_nothing").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let owner = Authority::default();
    let accepted = submit(&host, &owner, request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = accepted.agent.id;
    let pane = shown_on_console(&host, agent);
    let before = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap();
    let source = before.session.as_ref().unwrap().id();
    let actor = host.lock().unwrap().chats[&source].handle.clone();
    let spawns = std::fs::read_to_string(path.parent().unwrap().join("spawns")).unwrap();

    let present =
        |surface| match process(&host, owner.clone(), Operation::Present { agent, surface })
            .unwrap()
        {
            Reply::Status { agent, .. } => agent,
            reply => panic!("{reply:?}"),
        };
    assert_eq!(present(Surface::Chat), before);
    let crate::model::CardKind::Chat(chat) = shown_kind(&host, pane.id) else {
        panic!("shown as chat");
    };
    assert!(crate::agent_execution::same_chat_process(
        &chat,
        match before.session.as_ref().unwrap() {
            HostedSession::Chat(session) => session,
            HostedSession::Terminal(_) => unreachable!(),
        }
    ));
    // Presenting again changes nothing.
    assert_eq!(present(Surface::Chat), before);
    // Showing the agent is not taking it over.
    assert!(
        !registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_is_interactive(agent)
            .unwrap()
    );

    let followup = submit(
        &host,
        &owner,
        Submit {
            name: None,
            destination: Destination::Existing(agent),
            prompt: "Answer while shown as chat".into(),
            idempotency_key: "after-present".into(),
        },
    )
    .unwrap();
    assert_eq!(
        settled(&host, followup.run.id).result.text,
        "SERVICE_RESULT_2"
    );
    assert_eq!(present(Surface::Terminal), before);
    assert_eq!(shown_kind(&host, pane.id), pane.kind);
    assert!(
        host.lock().unwrap().chats[&source]
            .handle
            .same_actor(&actor),
        "the same provider stays attached"
    );
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("spawns")).unwrap(),
        spawns,
        "presenting spawned a provider"
    );
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(agent)
            .unwrap(),
        before
    );
}

/// Only an agent's owner chooses how it is shown; the user's CLI cannot
/// present another session's child.
#[test]
fn only_an_agents_owner_presents_it() {
    if isolated("only_an_agents_owner_presents_it").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let user = Authority::default();
    let accepted = submit(&host, &user, request.clone()).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let parent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let parent_authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(parent.session.as_ref().unwrap().id())
        .unwrap();
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &parent_authority, child_request).unwrap();
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    let pane = shown_on_console(&host, child.agent.id);
    let operation = Operation::Present {
        agent: child.agent.id,
        surface: Surface::Chat,
    };
    let error = process(&host, user.clone(), operation.clone()).unwrap_err();
    assert!(error.contains("does not own"), "{error}");
    assert_eq!(shown_kind(&host, pane.id), pane.kind);
    let error = process(
        &host,
        user,
        Operation::Present {
            agent: AgentId::new(),
            surface: Surface::Chat,
        },
    )
    .unwrap_err();
    assert!(!error.is_empty());
    assert!(matches!(
        process(&host, parent_authority.clone(), operation).unwrap(),
        Reply::Status { .. }
    ));
    assert!(matches!(
        shown_kind(&host, pane.id),
        crate::model::CardKind::Chat(_)
    ));
    assert!(
        close(&host, &parent_authority, child.agent.id)
            .unwrap()
            .closed
    );
}
