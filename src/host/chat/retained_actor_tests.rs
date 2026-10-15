//! Real execed holder/provider and kernel daemon-lease qualification. This
//! isolates HOME/runtime/data; native UI qualification separately kills cagentsd.
use super::*;
use crate::{agent_execution::{Authority, Destination, Origin, Spec, Submit, Surface, Target as ExecutionTarget, State}, model::{HostIncarnation, HostSessionId, ProjectKind}, workspace_model::ModelEdit};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

const TEST: &str = "host::chat::retained_actor_tests::slow_turn_keeps_exact_provider_and_frames_across_daemon_lease_rotation";
const MODE: &str = "CAGENTS_CLAUDE_HOLDER_TEST";

#[test]
fn slow_turn_keeps_exact_provider_and_frames_across_daemon_lease_rotation() {
    if std::env::var(MODE).as_deref() != Ok("fixture") {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in ["home/.claude", "run", "data", "config", "cache", "bin", "project"] { fs::create_dir_all(root.join(path)).unwrap(); }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"]).env_clear()
            .env(MODE, "fixture").env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root).env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run")).env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config")).env("XDG_CACHE_HOME", root.join("cache"))
            .env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display())).output().unwrap();
        assert!(output.status.success(), "isolated retained actor qualification failed:\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_provider(&root.join("bin/claude"));
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |incarnation| HostSessionProof { host_session_id: HostSessionId::new("retained-fixture"), boot_id: crate::daemon::read_boot_id().unwrap(), host_incarnation: HostIncarnation::new(incarnation) };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&path).unwrap();
    registry.apply(&ModelEdit::OpenProject { repository: root.join("project"), name: "Retained actor fixture".into(), kind: ProjectKind::Folder }).unwrap();
    let accepted = registry.submit_agent(&Origin::User, &Submit { name: None, destination: Destination::New(Spec { provider: Provider::Claude, account: Some(crate::account::Account::system(Provider::Claude).id()), target: ExecutionTarget { repository: root.join("project"), directory: root.join("project"), placement: None }, surface: Surface::Chat, model: None, permission_mode: None, reasoning_effort: None }), prompt: "One exact slow turn".into(), idempotency_key: "one-slow-turn".into() }, 1).unwrap();
    let mut state = HostState::new(first.clone());
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let source = chat::fresh_session(Provider::Claude, TerminalId::new(), root.join("project"), &crate::account::Account::system(Provider::Claude)).unwrap();
    let id = source.id;
    let context = Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    start_execution(&host, 0, source, context).unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().expect("the test opts into a real retained actor");
    let active_proof = Arc::new(std::sync::Mutex::new(first.clone()));
    struct Cleanup { owner: crate::daemon::holder_registry::Record, proof: Arc<std::sync::Mutex<HostSessionProof>> }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let proof = self.proof.lock().unwrap().clone();
            if let Ok(inspection) = Handle::inspect_remote(&self.owner, proof.clone())
                && let Ok(handle) = Handle::adopt_remote(&self.owner, inspection.source, proof, |_| {})
                && handle.close().is_ok() { return; }
            // Only this fixture's exact process incarnations; no name/group sweep.
            for process in [&self.owner.root, &self.owner.holder] { if process.is_current() { unsafe { libc::kill(process.pid, libc::SIGKILL); } } }
        }
    }
    let _cleanup = Cleanup { owner: identity.record.clone(), proof: active_proof.clone() };
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let hosted_source = host.lock().unwrap().chats[&id].session.clone();
    registry.lock().unwrap().bind_agent(accepted.agent.id, None, crate::agent_execution::HostedSession::Chat(hosted_source)).unwrap();
    handle.command_for_run(accepted.run.id, chat::Command::Prompt { text: "One exact slow turn".into(), attachments: Vec::new(), mode: chat::PromptMode::Send }).unwrap();
    wait_until(|| fs::read_to_string(root.join("produced")).unwrap_or_default().lines().count() >= 3, "provider never began its slow turn");
    assert_eq!(registry.lock().unwrap().agent_run(accepted.run.id).unwrap().state, State::Running);
    drop(handle);
    drop(host);
    drop(lease.take());
    // The actual reducer, accepted command UUID, Store and protocol pipes stay
    // in the same helper while there is no daemon lease or attached proxy.
    wait_until(|| root.join("provider-done").exists(), "provider did not complete while detached");
    assert!(identity.record.root.is_current(), "the original provider must still own its stream");
    let next = proof("second");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    *active_proof.lock().unwrap() = next.clone();
    let mut state = HostState::new(next);
    state.install_model(super::super::model::Registry::at(&path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    restore_orphaned_native(&successor, &[]).unwrap();
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(handle.actor_identity(), Some(identity.clone()), "adoption must retain both helper and original provider incarnations");
    let retained = retained_actor::retained_runs(&successor);
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    registry.lock().unwrap().interrupt_unproven_agent_runs_except(2, &retained).unwrap();
    wait_until(|| registry.lock().unwrap().agent_run(accepted.run.id).unwrap().state == State::Succeeded, "retained final outcome did not commit as Succeeded");
    let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
    for index in 0..40 { assert_eq!(run.result.text.matches(&format!("FRAME_{index:03}|")).count(), 1, "missing or duplicate frame {index}"); }
    handle.set_visible(true);
    wait_until(|| handle.snapshot(None).is_some_and(|snapshot| snapshot.items.len() >= 40), "retained transcript failed to hydrate");
    let snapshot = handle.snapshot(None).unwrap();
    let transcript = snapshot.items.iter().filter_map(|item| match &item.content { chat::ItemContent::Text { text, thinking: false, .. } => Some(text.as_str()), _ => None }).collect::<Vec<_>>().join("\n");
    for index in 0..40 { assert_eq!(transcript.matches(&format!("FRAME_{index:03}|")).count(), 1); }
    assert_eq!(fs::read_to_string(root.join("prompts")).unwrap().lines().count(), 1, "adoption must never resend the accepted prompt");
    super::super::agent_execution::close(&successor, &Authority::default(), accepted.agent.id).unwrap();
    wait_until(|| !identity.record.holder.is_current() && !identity.record.root.is_current(), "explicit close left a native owner");
    drop(lease);
}

fn wait_until(mut condition: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() { assert!(Instant::now() < deadline, "{reason}"); thread::sleep(Duration::from_millis(20)); }
}

fn install_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
root=pathlib.Path(os.environ['HOME']).parent
session=next((arg.split('=',1)[1] for arg in sys.argv[1:] if arg.startswith('--session-id=')), 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa')
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
    frame=json.loads(line)
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
        for index in range(40):
            send({'type':'assistant','uuid':f'frame-{index}','message':{'id':f'frame-{index}','role':'assistant','content':[{'type':'text','text':f'FRAME_{index:03}|'}]}})
            with (root/'produced').open('a') as log: log.write(str(index)+'\n')
            time.sleep(0.04)
        send({'type':'result','uuid':'final','subtype':'success','is_error':False,'session_id':session,'usage':{'input_tokens':7,'output_tokens':40}})
        (root/'provider-done').write_text('done')
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
