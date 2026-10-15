use super::*;
use crate::{
    host::{model::Registry, state::{HostLock, HostState}},
    model::{CardId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal_ipc::{ClientRequest, TerminalSpawnOptions},
    workspace_model::views::{Change, Pane, Scope},
};
use std::{fs, io::Write, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::PathBuf, sync::{Arc,mpsc}, thread, time::Duration};

const TEST: &str = "host::workspace::pending_tests::pending_publication_serializes_a_new_recipe_without_blocking_host_reads";
const ROOT: &str = "CAGENTS_PENDING_PUBLICATION_TEST_ROOT";

struct Evidence(Option<tempfile::TempDir>);
impl Drop for Evidence {
    fn drop(&mut self) { if let Some(root) = self.0.take() { let _retained = root.keep(); } }
}
fn isolated() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT) { return Some(PathBuf::from(root).canonicalize().unwrap()); }
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = Evidence(Some(directory));
    for name in ["home","run","data","config","cache","tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name),fs::Permissions::from_mode(0o700)).unwrap();
    }
    let executable = std::env::current_exe().unwrap();
    let mut command = CommandSpec::new(&executable);
    for (key,_) in std::env::vars_os() { command = command.env_remove(key); }
    command = command.args(["--exact",TEST,"--nocapture","--test-threads=1"])
        .cwd(&root).env(ROOT,&root).env("HOME",root.join("home"))
        .env("XDG_RUNTIME_DIR",root.join("run")).env("XDG_DATA_HOME",root.join("data"))
        .env("XDG_CONFIG_HOME",root.join("config")).env("XDG_CACHE_HOME",root.join("cache"))
        .env("TMPDIR",root.join("tmp")).env("PATH","/usr/bin:/bin")
        .timeout(Duration::from_secs(40)).output_limits(256*1024,256*1024);
    let output = SystemProcessExecutor.execute(&command,&CancellationToken::new());
    let (stdout,stderr) = match &output { Ok(output) => (&output.stdout,&output.stderr), Err(error) => (&error.stdout,&error.stderr) };
    for (name,bytes) in [("stdout",stdout),("stderr",stderr)] {
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(root.join(name)).unwrap().write_all(bytes).unwrap();
    }
    let output = output.unwrap_or_else(|error|panic!("retained {}: {error}",root.display()))
        .require_success(&executable).unwrap_or_else(|error|panic!("retained {}: {error}",root.display()));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("test {TEST} ... ok")),"{stdout}");
    assert!(stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),"{stdout}");
    // Original Child exit is not whole-family proof. Keep the runtime on BOTH
    // outcomes for the existing strict outer command guardian.
    None
}

struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release { fn drop(&mut self) { if let Some(send) = self.0.take() { let _ = send.send(()); } } }
struct Cleanup(SharedHost);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let (handles,_) = self.0.lock().unwrap_or_else(|p|p.into_inner()).graceful_shutdown();
        for handle in &handles { handle.request_shutdown(); }
        for handle in handles { let _ = handle.shutdown(); }
    }
}

#[test]
fn pending_publication_serializes_a_new_recipe_without_blocking_host_reads() {
    let Some(root) = isolated() else { return; };
    let mut registry = Registry::at(&root.join("model.sqlite3")).unwrap();
    registry.apply(&ModelEdit::OpenProject { repository:root.clone(),name:"fixture".into(),kind:Default::default() }).unwrap();
    let scope = Scope::Worktree { repository:root.clone(),path:root.clone() };
    let launch = LaunchRecipe { program:"/bin/sleep".into(),arguments:vec!["60".into()],working_directory:root.clone(),environment:Default::default(),unset_environment:Default::default() };
    let session = TerminalSession { id:TerminalId::new(),launch:launch.clone(),transcript_path:None,resume_hint:None,owned_codex_listener:None,recovery_origin:None,predecessor:None,continuity:TerminalContinuity::RelaunchAvailable { recipe:launch,last_exit:None } };
    let pane = Pane { id:CardId::new(),title:"original".into(),user_title:None,pending_terminal_start:true,width_fraction:0.5,kind:CardKind::Terminal(session.clone()),scope:None };
    let applied = registry.apply(&ModelEdit::SeedView { scope:scope.clone(),panes:vec![pane.clone()],closed:Vec::new() }).unwrap();
    let revision = applied.model.views.iter().find(|view|view.scope==scope).unwrap().revision;
    let mut state = HostState::new(HostSessionProof { host_session_id:HostSessionId::new("pending-publication"),boot_id:crate::daemon::read_boot_id().unwrap(),host_incarnation:HostIncarnation::new("pending-publication") });
    state.install_model(registry);
    state.register_client(1);state.register_client(2);
    let host = Arc::new(HostLock::new(state));
    let _cleanup = Cleanup(host.clone());
    let mut changed = pane.clone();
    let CardKind::Terminal(next) = &mut changed.kind else { unreachable!() };
    next.launch.arguments = vec!["61".into()];
    next.continuity = TerminalContinuity::RelaunchAvailable { recipe:next.launch.clone(),last_exit:None };
    let request = ModelEdit::ChangeView { scope:scope.clone(),expected_revision:revision,operation:uuid::Uuid::new_v4(),change:Change::Update { before:pane.clone(),pane:changed.clone() } };
    thread::scope(|threads| {
        let (ready,admitted) = mpsc::channel();
        let (release,wait) = mpsc::channel();
        let release = Release(Some(release));
        let spawning = host.clone();
        let pane_id = pane.id;
        let terminal_id = session.id;
        let spawn = threads.spawn(move || {
            BEFORE_PENDING_PUBLICATION.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
                ready.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            })));
            crate::host::client::process_request(&spawning,1,ClientRequest::CompletePendingTerminal { pane:pane_id,terminal_id,options:TerminalSpawnOptions::default() })
        });
        admitted.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(writer(&host).unwrap().try_lock().is_err(),"pending publication lost its writer admission");
        let (started,entered) = mpsc::channel();
        let (finished,result) = mpsc::channel();
        let editing = host.clone();
        let edit = threads.spawn(move || {
            started.send(()).unwrap();
            let answer = crate::host::client::process_request(&editing,2,ClientRequest::EditWorkspaceModel { edit:request });
            let _ = finished.send(answer);
        });
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(result.recv_timeout(Duration::from_millis(50)),Err(mpsc::RecvTimeoutError::Timeout)),"new recipe committed inside pending publication");
        let (read,answer) = mpsc::channel();
        let reading = host.clone();
        let reader = threads.spawn(move || { let _ = read.send(crate::host::client::process_request(&reading,1,ClientRequest::List)); });
        assert!(answer.recv_timeout(Duration::from_millis(500)).unwrap().is_ok(),"writer admission blocked unrelated Host reads");
        drop(release);
        assert!(spawn.join().unwrap().is_ok());
        let error = result.recv_timeout(Duration::from_secs(5)).unwrap().unwrap_err();
        assert_eq!(error.code,ProtocolErrorCode::ViewConflict);
        edit.join().unwrap();reader.join().unwrap();
    });
    let model = snapshot(&host,None).unwrap();
    let view = model.views.iter().find(|view|view.scope==scope).unwrap();
    let completed = view.panes.iter().find(|item|item.id==pane.id).unwrap();
    assert!(!completed.pending_terminal_start);
    assert!(matches!(&completed.kind,CardKind::Terminal(actual) if actual.launch==session.launch));
    let original = host.lock().unwrap().terminals[&session.id].handle.clone();
    assert!(crate::host::client::process_request(&host,2,ClientRequest::CompletePendingTerminal {
        pane:pane.id,terminal_id:session.id,options:TerminalSpawnOptions::default()
    }).is_ok(),"same-source completion retry must attach the existing worker");
    assert!(host.lock().unwrap().terminals[&session.id].handle.same_worker(&original));
    // The real UI may publish the actual Live descriptor into the model.
    // An idempotent completion still attaches that exact existing worker.
    let mut projected = completed.clone();
    projected.pending_terminal_start = true;
    projected.kind = CardKind::Terminal(host.lock().unwrap().descriptor(1,session.id).unwrap().session);
    edit(&host,2,ModelEdit::ChangeView { scope:scope.clone(),expected_revision:view.revision,operation:uuid::Uuid::new_v4(),change:Change::Update { before:completed.clone(),pane:projected } }).unwrap();
    assert!(crate::host::client::process_request(&host,2,ClientRequest::CompletePendingTerminal {
        pane:pane.id,terminal_id:session.id,options:TerminalSpawnOptions::default()
    }).is_ok(),"actual published source must remain idempotently attachable");
    assert!(host.lock().unwrap().terminals[&session.id].handle.same_worker(&original));
    let model = snapshot(&host,None).unwrap();
    let view = model.views.iter().find(|view|view.scope==scope).unwrap();
    let before = view.panes.iter().find(|item|item.id==pane.id).unwrap();
    assert!(!before.pending_terminal_start,"same-worker projected retry must finish its reservation");
    // A deliberate rebased edit remains authoritative after completion. No
    // delayed completion may overwrite it with the previous source.
    edit(&host,2,ModelEdit::ChangeView { scope:scope.clone(),expected_revision:view.revision,operation:uuid::Uuid::new_v4(),change:Change::Update { before:before.clone(),pane:changed.clone() } }).unwrap();
    let model = snapshot(&host,None).unwrap();
    assert_eq!(model.views.iter().find(|view|view.scope==scope).unwrap().panes.iter().find(|item|item.id==pane.id),Some(&changed));
    let refused = crate::host::client::process_request(&host,2,ClientRequest::CompletePendingTerminal {
        pane:pane.id,terminal_id:session.id,options:TerminalSpawnOptions::default()
    }).unwrap_err();
    assert_eq!(refused.code,ProtocolErrorCode::TerminalNotFound);
    assert!(host.lock().unwrap().terminals[&session.id].handle.same_worker(&original));
    assert!(!original.is_completed(),"source refusal must not replace/stop the old owned worker");
    original.shutdown().unwrap();
    assert!(original.is_completed());
}
