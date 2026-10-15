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
    let notifications = super::super::notifications::Broker::start(&host, crate::notifications::Application::Cagents).unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let source = chat::fresh_session(Provider::Claude, TerminalId::new(), root.join("project"), &crate::account::Account::system(Provider::Claude)).unwrap();
    let id = source.id;
    let context = Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    start_execution(&host, 0, source, context).unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().expect("the test opts into a real retained actor");
    let active_proof = Arc::new(std::sync::Mutex::new(first.clone()));
    struct Cleanup { owner: crate::daemon::holder_registry::Record, pair: Option<crate::daemon::holder_registry::Record>, proof: Arc<std::sync::Mutex<HostSessionProof>> }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let proof = self.proof.lock().unwrap().clone();
            if let Ok(inspection) = Handle::inspect_remote(&self.owner, proof.clone())
                && let Ok(handle) = Handle::adopt_remote(&self.owner, inspection.source, proof, |_| {})
                && handle.close().is_ok() && handle.finish_retained_close().is_ok() { return; }
            // Only this fixture's exact process incarnations; no name/group sweep.
            for owner in std::iter::once(&self.owner).chain(self.pair.iter()) {
                for process in [&owner.root,&owner.holder] { if process.is_current() { unsafe {libc::kill(process.pid,libc::SIGKILL);} } }
            }
        }
    }
    let mut cleanup = Cleanup { owner: identity.record.clone(), pair:None, proof: active_proof.clone() };
    // A real holder-owned thin console shares this native actor's exact ID.
    // The raw terminal record will retain RelaunchAvailable on later adoption;
    // recovery must project the authenticated current worker, not trust that raw
    // saved continuity or replace its PTY process.
    host.lock().unwrap().enable_holder_recovery();
    let console_recipe=crate::model::LaunchRecipe {program:"/bin/cat".into(),arguments:Vec::new(),working_directory:root.join("project"),environment:Default::default(),unset_environment:Default::default()};
    host.lock().unwrap().chats.get_mut(&id).unwrap().console_recipe=Some(console_recipe.clone());
    let terminal=crate::terminal_ipc::TerminalDescriptor::relaunchable(id,console_recipe.clone(),None).session;
    let (descriptor,console)=super::super::state::spawn_terminal_view_owned(&host,0,terminal,crate::terminal_ipc::TerminalSpawnOptions::default(),None,true).unwrap();
    let pair=console.holder_identity().expect("fixture requires actual terminal holder").clone();
    cleanup.pair=Some(pair.clone());
    let mut paired_source=handle.actor_identity().and_then(|owner|Handle::inspect_remote(&owner.record,first.clone()).ok()).unwrap().source;
    paired_source.terminal=Some(descriptor.session.clone());
    handle.update_native_source(paired_source.clone(),Some(console_recipe.clone()),Some(pair.clone())).unwrap();
    {
        let mut state=host.lock().unwrap();let record=state.chats.get_mut(&id).unwrap();
        record.session=paired_source;record.paired_terminal=Some(console.clone());
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let hosted_source = host.lock().unwrap().chats[&id].session.clone();
    registry.lock().unwrap().bind_agent(accepted.agent.id, None, crate::agent_execution::HostedSession::Chat(hosted_source)).unwrap();
    handle.command_for_run(accepted.run.id, chat::Command::Prompt { text: "One exact slow turn".into(), attachments: Vec::new(), mode: chat::PromptMode::Send }).unwrap();
    wait_until(|| fs::read_to_string(root.join("produced")).unwrap_or_default().lines().count() >= 3, "provider never began its slow turn");
    assert_eq!(registry.lock().unwrap().agent_run(accepted.run.id).unwrap().state, State::Running);
    console.preserve_for_restart().unwrap();
    drop(console);
    drop(handle);
    drop(notifications);
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
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(&successor, crate::notifications::Application::Cagents).unwrap();
    successor.lock().unwrap().notifications = Some(_notifications.handle());
    super::super::state::restore_orphaned_holders(&successor).unwrap();
    assert!(matches!(successor.lock().unwrap().terminals[&id].session.continuity,crate::model::TerminalContinuity::RelaunchAvailable {..}),"raw restored console intentionally remains unpublished until native binding");
    restore_orphaned_native(&successor, &[]).unwrap();
    {
        let state=successor.lock().unwrap();let record=&state.chats[&id];
        assert_eq!(state.terminals[&id].handle.holder_identity(),Some(&pair));
        assert!(pair.root.is_current(),"the exact original console process must survive");
        assert_eq!(record.session.terminal.as_ref(),Some(&state.native_terminal_projection(id).unwrap()));
        assert!(record.paired_terminal.as_ref().unwrap().same_worker(&state.terminals[&id].handle));
    }
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
    wait_until(|| !identity.record.holder.is_current() && !identity.record.root.is_current() && !pair.holder.is_current() && !pair.root.is_current(), "explicit close left a native or paired owner");
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

/// This boundary kills a separate host owner process with SIGKILL. Unlike a
/// lease-rotation-only fixture, no destructor or old proxy can help recovery.
#[test]
fn slow_turn_survives_sigkill_of_the_host_owner_process() {
    const EXACT: &str = "host::chat::retained_actor_tests::slow_turn_survives_sigkill_of_the_host_owner_process";
    const ROLE: &str = "CAGENTS_RETAINED_HOST_PROCESS_ROLE";
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Receipt {
        identity: chat::runtime::ActorIdentity,
        agent: crate::model::AgentId,
        run: crate::model::RunId,
        session: TerminalId,
    }
    fn child(root: &Path, role: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", EXACT, "--nocapture"]).env_clear()
            .env(ROLE, role).env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root).env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run")).env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config")).env("XDG_CACHE_HOME", root.join("cache"))
            .env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()));
        command
    }
    if std::env::var_os(ROLE).is_none() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in ["home/.claude", "run", "data", "config", "cache", "bin", "project"] { fs::create_dir_all(root.join(path)).unwrap(); }
        install_provider(&root.join("bin/claude"));
        // Cleanup uses a separate private host lease; the parallel test runner
        // never changes its own HOME/runtime or acquires the production lease.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) { let _ = child(&self.0, "cleanup").output(); }
        }
        let _cleanup = Cleanup(root.to_owned());
        struct SourceOwner(std::process::Child);
        impl Drop for SourceOwner {
            fn drop(&mut self) {
                if self.0.try_wait().is_ok_and(|status| status.is_none()) { let _ = self.0.kill(); }
                let _ = self.0.wait();
            }
        }
        let mut owner = SourceOwner(child(root, "source")
            .stdout(fs::File::create(root.join("source.stdout")).unwrap())
            .stderr(fs::File::create(root.join("source.stderr")).unwrap()).spawn().unwrap());
        let owner_identity = crate::daemon::holder_registry::Process::read(owner.0.id() as i32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("owner-receipt.json").exists() {
            assert!(owner.0.try_wait().unwrap().is_none(), "source host failed: {}", fs::read_to_string(root.join("source.stderr")).unwrap_or_default());
            assert!(Instant::now() < deadline, "source host never published its exact owner receipt");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(owner_identity.is_current());
        owner.0.kill().unwrap();
        assert_eq!(owner.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(!owner_identity.is_current(), "the old host owner must actually be gone");
        wait_until(|| root.join("provider-done").exists(), "provider did not finish while no host owner existed");
        let output = child(root, "successor").output().unwrap();
        assert!(output.status.success(), "successor failed:\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let role = std::env::var(ROLE).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = HostSessionProof { host_session_id: HostSessionId::new("sigkill-host-fixture"), boot_id: crate::daemon::read_boot_id().unwrap(), host_incarnation: HostIncarnation::new(&role) };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    if role == "cleanup" {
        // Only exact holder records inside this fixture's private namespace.
        let directory = crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native).unwrap();
        if let Ok(records) = crate::daemon::holder_registry::discover(&directory, &proof.boot_id, crate::daemon::holder_registry::Kind::Native) {
            for record in records {
                if let Ok(inspection) = Handle::inspect_remote(&record, proof.clone())
                    && let Ok(handle) = Handle::adopt_remote(&record, inspection.source, proof.clone(), |_| {})
                    && handle.close().is_ok() && handle.finish_retained_close().is_ok() { continue; }
                // On a failing assertion there may be uncommitted outcomes;
                // never misreport that as semantic retirement. Kill only the
                // exact fixture-created process incarnations for test hygiene.
                for process in [&record.root, &record.holder] { if process.is_current() { unsafe { libc::kill(process.pid, libc::SIGKILL); } } }
            }
        }
        return;
    }
    let path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&path).unwrap();
    let accepted = if role == "source" {
        registry.apply(&ModelEdit::OpenProject { repository: root.join("project"), name: "Killed host fixture".into(), kind: ProjectKind::Folder }).unwrap();
        Some(registry.submit_agent(&Origin::User, &Submit { name: None, destination: Destination::New(Spec { provider: Provider::Claude, account: Some(crate::account::Account::system(Provider::Claude).id()), target: ExecutionTarget { repository: root.join("project"), directory: root.join("project"), placement: None }, surface: Surface::Chat, model: None, permission_mode: None, reasoning_effort: None }), prompt: "One exact killed-host turn".into(), idempotency_key: "one-killed-host-turn".into() }, 1).unwrap())
    } else { None };
    let mut state = HostState::new(proof);
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(&host, crate::notifications::Application::Cagents).unwrap();
    host.lock().unwrap().notifications = Some(_notifications.handle());
    if let Some(accepted) = accepted {
        let session = chat::fresh_session(Provider::Claude, TerminalId::new(), root.join("project"), &crate::account::Account::system(Provider::Claude)).unwrap();
        let id = session.id;
        start_execution(&host, 0, session, Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap())).unwrap();
        let (source, handle) = { let state = host.lock().unwrap(); (state.chats[&id].session.clone(), state.chats[&id].handle.clone()) };
        super::super::agent_execution::registry(&host).unwrap().lock().unwrap().bind_agent(accepted.agent.id, None, crate::agent_execution::HostedSession::Chat(source)).unwrap();
        handle.command_for_run(accepted.run.id, chat::Command::Prompt { text: "One exact killed-host turn".into(), attachments: Vec::new(), mode: chat::PromptMode::Send }).unwrap();
        wait_until(|| fs::read_to_string(root.join("produced")).unwrap_or_default().lines().count() >= 3, "provider never began");
        let receipt = Receipt { identity: handle.actor_identity().unwrap(), agent: accepted.agent.id, run: accepted.run.id, session: id };
        let temporary = root.join("owner-receipt.pending");
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary).unwrap();
        serde_json::to_writer(&file, &receipt).unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, root.join("owner-receipt.json")).unwrap();
        loop { thread::park(); }
    }
    let receipt: Receipt = serde_json::from_slice(&fs::read(root.join("owner-receipt.json")).unwrap()).unwrap();
    assert!(receipt.identity.record.root.is_current(), "the original provider must survive the host SIGKILL");
    restore_orphaned_native(&host, &[]).unwrap();
    let handle = host.lock().unwrap().chats[&receipt.session].handle.clone();
    assert_eq!(handle.actor_identity(), Some(receipt.identity.clone()));
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let retained = retained_actor::retained_runs(&host);
    registry.lock().unwrap().interrupt_unproven_agent_runs_except(2, &retained).unwrap();
    wait_until(|| registry.lock().unwrap().agent_run(receipt.run).unwrap().state == State::Succeeded, "exact retained result never committed");
    let run = registry.lock().unwrap().agent_run(receipt.run).unwrap();
    handle.set_visible(true);
    wait_until(|| handle.snapshot(None).is_some_and(|snapshot| snapshot.items.len() >= 40), "transcript did not hydrate after host SIGKILL");
    let snapshot = handle.snapshot(None).unwrap();
    let text = snapshot.items.iter().filter_map(|item| match &item.content { chat::ItemContent::Text { text, thinking: false, .. } => Some(text.as_str()), _ => None }).collect::<Vec<_>>().join("\n");
    for index in 0..40 {
        let marker = format!("FRAME_{index:03}|");
        assert_eq!(run.result.text.matches(&marker).count(), 1);
        assert_eq!(text.matches(&marker).count(), 1);
    }
    assert_eq!(fs::read_to_string(root.join("prompts")).unwrap().lines().count(), 1);
    super::super::agent_execution::close(&host, &Authority::default(), receipt.agent).unwrap();
    wait_until(|| !receipt.identity.record.holder.is_current() && !receipt.identity.record.root.is_current(), "explicit close did not retire exact native owners");
}
