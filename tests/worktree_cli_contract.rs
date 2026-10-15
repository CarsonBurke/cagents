#![cfg(feature = "host")]

//! The shipped CLI and an isolated real daemon share the worktree journal,
//! settings, safety refusals and trusted setup terminal path.
use serde_json::Value;
use std::{fs,os::unix::fs::PermissionsExt,path::Path,process::{Child,Command,Output,Stdio},thread,time::{Duration,Instant}};

struct Fixture { root:tempfile::TempDir, daemon:Child }
impl Fixture {
    fn command(root:&Path,program:&Path)->Command {
        let mut command=Command::new(program);
        command.env_clear().env("HOME",root.join("home")).env("XDG_RUNTIME_DIR",root.join("run"))
            .env("XDG_DATA_HOME",root.join("data")).env("XDG_CONFIG_HOME",root.join("config"))
            .env("XDG_CACHE_HOME",root.join("cache")).env("TMPDIR",root.join("tmp"))
            .env("PATH",format!("{}:/usr/bin:/bin",root.join("bin").display()))
            .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_SYSTEM","/dev/null")
            .env("GIT_AUTHOR_NAME","Worktree Contract").env("GIT_AUTHOR_EMAIL","test@example.invalid")
            .env("GIT_COMMITTER_NAME","Worktree Contract").env("GIT_COMMITTER_EMAIL","test@example.invalid")
            .env("TERM","xterm-256color").current_dir(root.join("project")).stdin(Stdio::null());
        command
    }
    fn new()->Self {
        let root=tempfile::tempdir().unwrap();
        for path in ["home","home/.claude","run","data","config","cache","tmp","bin","project"] {fs::create_dir(root.path().join(path)).unwrap();}
        fs::set_permissions(root.path().join("run"),fs::Permissions::from_mode(0o700)).unwrap();
        let git=|args:&[&str]| {let output=Self::command(root.path(),Path::new("/usr/bin/git")).args(args).output().unwrap();assert!(output.status.success(),"{args:?}: {output:?}");};
        git(&["init","-q","-b","main"]);
        fs::write(root.path().join("project/tracked"),"initial\n").unwrap();
        git(&["add","tracked"]);git(&["commit","-qm","initial"]);
        git(&["init","--bare","-q",root.path().join("remote.git").to_str().unwrap()]);
        git(&["remote","add","origin",root.path().join("remote.git").to_str().unwrap()]);
        git(&["push","-qu","origin","main"]);
        let settings=root.path().join("config").join(cagents::instance::STORAGE_NAME).join("settings.toml");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let configuration=cagents::settings::AppSettings {managed_worktree_directory:"managed/".into(),..Default::default()};
        fs::write(settings,toml::to_string(&configuration).unwrap()).unwrap();
        let log=fs::File::create(root.path().join("daemon.log")).unwrap();
        let daemon=Self::command(root.path(),Path::new(env!("CARGO_BIN_EXE_cagentsd"))).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
        let mut fixture=Self {root,daemon};
        let deadline=Instant::now()+Duration::from_secs(15);
        loop {
            assert!(fixture.daemon.try_wait().unwrap().is_none(),"daemon exited: {}",fs::read_to_string(fixture.root.path().join("daemon.log")).unwrap());
            let output=fixture.output(&["daemon","status"]);
            if output.status.success()&&serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|value|value["data"]["daemon_version"].is_string()) {break;}
            assert!(Instant::now()<deadline,"daemon did not become ready");thread::sleep(Duration::from_millis(20));
        }
        fixture
    }
    fn output(&self,args:&[&str])->Output {self.output_with_env(args,&[])}
    fn output_with_env(&self,args:&[&str],environment:&[(&str,&std::ffi::OsStr)])->Output {
        let mut command=Self::command(self.root.path(),Path::new(env!("CARGO_BIN_EXE_cagents")));
        for (key,value) in environment {command.env(key,value);}
        let mut child=command.arg("--json").args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(35);
        while child.try_wait().unwrap().is_none() {if Instant::now()>=deadline {let _=child.kill();panic!("CLI timed out {args:?}: {:?}",child.wait_with_output());}thread::sleep(Duration::from_millis(10));}
        child.wait_with_output().unwrap()
    }
    fn cli(&self,args:&[&str])->Value {
        let output=self.output(args);assert!(output.status.success(),"{args:?}: {output:?}");
        let value:Value=serde_json::from_slice(&output.stdout).unwrap();assert_eq!(value["ok"],true,"{value}");value["data"].clone()
    }
    fn git(&self,path:&Path,args:&[&str])->String {
        let output=Self::command(self.root.path(),Path::new("/usr/bin/git")).current_dir(path).args(args).output().unwrap();
        assert!(output.status.success(),"{args:?}: {output:?}");String::from_utf8(output.stdout).unwrap()
    }
    fn create(&self,branch:&str)->std::path::PathBuf {
        let data=self.cli(&["worktree","create",branch,"--no-setup"]);
        assert_eq!(data["schema_version"],1);Path::new(data["worktree"]["path"].as_str().unwrap()).into()
    }
    fn setup_intent(&self,path:&Path)->Value {
        let journal=self.root.path().join("data").join(cagents::instance::STORAGE_NAME).join("worktree-operations.sqlite3");
        let connection=rusqlite::Connection::open_with_flags(journal,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY|rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW).unwrap();
        let payload:String=connection.query_row("SELECT payload_json FROM pending_operations WHERE action='create' AND json_extract(payload_json,'$.request.destination')=?1",[path.to_str().unwrap()],|row|row.get(0)).unwrap();
        serde_json::from_str::<Value>(&payload).unwrap()["request"]["setup_intent"].clone()
    }
    fn refusal(&self,target:&str,contains:&str) {
        let output=self.output(&["worktree","remove",target]);assert!(!output.status.success(),"{output:?}");
        let value:Value=serde_json::from_slice(&output.stdout).unwrap();assert_eq!(value["ok"],false);assert!(value.to_string().contains(contains),"{value}");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let preflight=self.output(&["daemon","preflight"]);
        if let Ok(value)=serde_json::from_slice::<Value>(&preflight.stdout)&&let Some(capability)=value["data"].get("capability") {
            let _=self.output(&["daemon","stop","--confirm",&capability.to_string()]);
        }
        let _=self.daemon.kill();let _=self.daemon.wait();
    }
}

#[test]
fn creates_lists_and_removes_in_configured_location_and_retains_branch_by_default() {
    let fixture=Fixture::new();
    let socket=fixture.root.path().join("run").join(cagents::instance::RUNTIME_SUBDIRECTORY).join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut observer=cagents::control::ControlClient::connect(&socket).unwrap();
    observer.watch_workspace_model().unwrap();
    let (send,receive)=std::sync::mpsc::channel();
    let watcher=thread::spawn(move || {
        loop {
            let event=observer.next_workspace_model_update();
            if matches!(&event,Ok(cagents::control::WorkspaceModelEvent::Edited {..})) {continue;}
            let _=send.send(event);break;
        }
    });
    let path=fixture.create("feat-one");
    let event=receive.recv_timeout(Duration::from_secs(5)).expect("create must publish a sidebar refresh immediately").unwrap();
    assert!(matches!(event,cagents::control::WorkspaceModelEvent::WorktreesChanged {repository} if repository==fixture.root.path().join("project/.git")));
    watcher.join().unwrap();
    assert_eq!(fixture.cli(&["project","list"])["projects"].as_array().unwrap().len(),1);
    assert_eq!(path,fixture.root.path().join("project/managed/feat-one"));
    assert!(path.join("tracked").is_file());
    let list=fixture.cli(&["worktree","list"]);
    assert_eq!(list["schema_version"],1);let entries=list["worktrees"].as_array().unwrap();assert_eq!(entries.len(),2);
    let entry=entries.iter().find(|entry|entry["branch"]=="feat-one").unwrap();
    assert_eq!(entry["dirty"],false);assert!(entry["live_sessions"].as_array().unwrap().is_empty());
    assert!(entry.get("ahead").is_some());assert!(entry.get("behind").is_some());
    fixture.cli(&["worktree","remove","feat-one"]);assert!(!path.exists());
    fixture.git(&fixture.root.path().join("project"),&["show-ref","--verify","refs/heads/feat-one"]);
    // Removal retires the exact creation receipt, so the retained branch can
    // be checked out again through the same journaled service.
    let recreated=fixture.create("feat-one");assert_eq!(recreated,path);
    fixture.cli(&["worktree","remove","feat-one"]);
    let second=fixture.create("feat-two");fixture.cli(&["worktree","remove","managed/feat-two","--delete-branch"]);
    assert!(!second.exists());
    assert!(!fixture.git(&fixture.root.path().join("project"),&["branch","--list","feat-two"]).contains("feat-two"));
}

#[test]
fn removal_refuses_modified_untracked_ignored_unpushed_and_detached_only_work() {
    let fixture=Fixture::new();let path=fixture.create("unsafe-work");
    fs::write(path.join("tracked"),"changed\n").unwrap();fixture.refusal("unsafe-work","modified");
    fixture.git(&path,&["restore","tracked"]);fs::write(path.join("untracked"),"keep").unwrap();fixture.refusal("unsafe-work","untracked");
    fs::remove_file(path.join("untracked")).unwrap();
    fixture.git(&path,&["config","--local","core.excludesFile",fixture.root.path().join("ignore").to_str().unwrap()]);
    fs::write(fixture.root.path().join("ignore"),".env\n").unwrap();fs::write(path.join(".env"),"SECRET=preserve\n").unwrap();fixture.refusal("unsafe-work","ignored");
    fs::remove_file(path.join(".env")).unwrap();fs::write(path.join("tracked"),"unique\n").unwrap();fixture.git(&path,&["commit","-am","unique"]);fixture.refusal("unsafe-work","unpushed");
    fixture.git(&path,&["checkout","--detach"]);fixture.git(&fixture.root.path().join("project"),&["branch","-D","unsafe-work"]);
    fixture.refusal(path.to_str().unwrap(),"detached-only");assert!(path.exists());
    fixture.cli(&["worktree","remove",path.to_str().unwrap(),"--force"]);assert!(!path.exists());
    assert!(fixture.git(&fixture.root.path().join("project"),&["branch","--list","rescue/*"]).contains("rescue/"));
}

#[test]
fn untrusted_setup_refuses_before_creation_no_setup_skips_and_trusted_copy_uses_terminal() {
    let fixture=Fixture::new();let project=fixture.root.path().join("project");
    fs::create_dir(project.join(".cagents")).unwrap();
    fs::write(project.join(".cagents/worktree.toml"),"copy = [\".env\"]\nsetup = \"printf ready > setup-result\"\n").unwrap();
    fs::write(project.join(".env"),"fixture secret\n").unwrap();
    let denied=fixture.output(&["worktree","create","untrusted"]);assert!(!denied.status.success());assert!(!project.join("managed/untrusted").exists());
    let skipped=fixture.create("skipped");assert!(!skipped.join(".env").exists());assert!(!skipped.join("setup-result").exists());
    let skipped_intent=fixture.setup_intent(&skipped);assert_eq!(skipped_intent["skip"],true);assert_eq!(skipped_intent["copy_done"],true);assert!(skipped_intent["plan"].is_null());
    let plan=cagents::worktree_setup::load(&project).unwrap().unwrap();
    cagents::worktree_setup::trust(&project,&plan,&cagents::repository::CancellationToken::new()).unwrap();
    let created=fixture.cli(&["worktree","create","trusted"]);let path=Path::new(created["worktree"]["path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(path.join(".env")).unwrap(),"fixture secret\n");
    let intent=fixture.setup_intent(path);assert_eq!(intent["copy_done"],true);assert_eq!(intent["skip"],false);
    assert_eq!(intent["plan"]["fingerprint"],plan.fingerprint);
    assert_eq!(intent["run_id"],created["setup"]["run_id"]);
    assert_eq!(intent["run_id"],created["setup"]["terminal"]["launch"]["environment"]["CAGENTS_WORKTREE_SETUP_ID"]);
    assert!(created["setup"]["terminal"]["id"].is_string(),"{created}");
    let deadline=Instant::now()+Duration::from_secs(10);
    while !path.join("setup-result").exists() {assert!(Instant::now()<deadline,"setup terminal did not run: {created}");thread::sleep(Duration::from_millis(20));}
    assert_eq!(fs::read_to_string(path.join("setup-result")).unwrap(),"ready");
    assert_eq!(created["setup"]["terminal"]["launch"]["environment"]["CAGENTS_WORKTREE_SETUP"],"1");
    assert!(created["setup"]["terminal"]["launch"]["environment"]["CAGENTS_WORKTREE_SETUP_STARTED_AT"].as_str().unwrap().parse::<u64>().unwrap()>0);
}

#[test]
fn project_ids_resolve_from_window_storage_without_migrating_it() {
    let fixture=Fixture::new();
    let id=cagents::model::ProjectId::new();
    let state=fixture.root.path().join("data").join(cagents::instance::STORAGE_NAME).join("sessions.sqlite3");
    let store=cagents::persistence::StateStore::open(&state,cagents::model::BootId::new("worktree-contract")).unwrap();
    store.save_project(&cagents::model::ProjectState {id,generation:0,name:"project".into(),repository:fixture.root.path().join("project/.git"),kind:cagents::model::ProjectKind::Git,existence:cagents::model::Existence::Open}).unwrap();
    drop(store);
    let data=fixture.cli(&["worktree","list","--project",&id.to_string()]);
    assert_eq!(data["worktrees"].as_array().unwrap().len(),1);
}
#[test]
fn linear_context_is_committed_with_the_created_worktree() {
    let fixture=Fixture::new();
    let denied=fixture.output(&["worktree","create","invalid-link","--linear","not-a-ticket","--no-setup"]);
    assert!(!denied.status.success());
    assert!(!fixture.root.path().join("project/managed/invalid-link").exists());
    assert!(fixture.git(&fixture.root.path().join("project"),&["branch","--list","invalid-link"]).is_empty());
    let data=fixture.cli(&["worktree","create","linked","--linear","ENG-123","--no-setup"]);
    let path=Path::new(data["worktree"]["path"].as_str().unwrap());
    assert_eq!(cagents::linear::context::resolve_at(path).unwrap().issue_id,"ENG-123");
}

#[test]
fn optional_agent_uses_new_worktree_and_waits_for_setup() {
    let fixture=Fixture::new();let project=fixture.root.path().join("project");
    fs::create_dir(project.join(".cagents")).unwrap();
    fs::write(project.join(".cagents/worktree.toml"),"setup = \"sleep 0.05; printf ready > setup-ready\"\n").unwrap();
    let plan=cagents::worktree_setup::load(&project).unwrap().unwrap();
    cagents::worktree_setup::trust(&project,&plan,&cagents::repository::CancellationToken::new()).unwrap();
    fs::create_dir_all(fixture.root.path().join("home/.claude")).unwrap();
    let provider=fixture.root.path().join("bin/claude");
    fs::write(&provider,format!("#!/bin/sh\nset -eu\n[ -f setup-ready ] || exit 18\nprintf '%s' \"$PWD\" > \"$HOME/../spawn-directory\"\n{}",include_str!("fixtures/automation-claude.sh"))).unwrap();
    fs::set_permissions(provider,fs::Permissions::from_mode(0o700)).unwrap();
    let data=fixture.cli(&["worktree","create","with-agent","--spawn-agent","claude","--model","haiku","--prompt","Reply ready"]);
    let path=Path::new(data["worktree"]["path"].as_str().unwrap());
    assert!(path.join("setup-ready").is_file());
    assert_eq!(data["agent"]["agent"]["spec"]["target"]["directory"],path.to_str().unwrap());
    let deadline=Instant::now()+Duration::from_secs(10);
    let marker=fixture.root.path().join("spawn-directory");
    while !marker.exists() {assert!(Instant::now()<deadline,"provider fixture did not start: {data}");thread::sleep(Duration::from_millis(20));}
    assert_eq!(fs::read_to_string(marker).unwrap(),path.to_str().unwrap());
    fixture.cli(&["agent","close",data["agent"]["agent"]["id"].as_str().unwrap()]);
    fixture.cli(&["worktree","remove","with-agent","--force"]);
    assert!(!path.exists());
}

#[test]
fn inherited_git_redirects_cannot_change_the_selected_worktree_project() {
    let fixture=Fixture::new();
    let unrelated=fixture.root.path().join("unrelated");fs::create_dir(&unrelated).unwrap();
    fixture.git(&unrelated,&["init","-q","-b","main"]);
    fs::write(unrelated.join("other"),"unrelated").unwrap();
    fixture.git(&unrelated,&["add","other"]);fixture.git(&unrelated,&["commit","-qm","unrelated"]);
    let git_dir=unrelated.join(".git");let index=git_dir.join("index");
    let output=fixture.output_with_env(&["worktree","create","authority","--no-setup"],&[
        ("GIT_DIR",git_dir.as_os_str()),("GIT_COMMON_DIR",git_dir.as_os_str()),
        ("GIT_WORK_TREE",unrelated.as_os_str()),("GIT_INDEX_FILE",index.as_os_str()),
    ]);
    assert!(output.status.success(),"{output:?}");
    let value:Value=serde_json::from_slice(&output.stdout).unwrap();
    let path=Path::new(value["data"]["worktree"]["path"].as_str().unwrap());
    assert_eq!(path,fixture.root.path().join("project/managed/authority"));
    assert!(path.join("tracked").is_file());assert!(!path.join("other").exists());
    assert!(!fixture.git(&unrelated,&["branch","--list","authority"]).contains("authority"));
}

#[test]
fn removal_refuses_a_canonical_path_that_cannot_be_encoded_exactly() {
    use std::os::unix::ffi::OsStringExt;
    let fixture=Fixture::new();
    let path=fixture.create("raw-path");
    let raw=fixture.root.path().join("project/managed").join(std::ffi::OsString::from_vec(b"raw-\xff".to_vec()));
    let output=Fixture::command(fixture.root.path(),Path::new("/usr/bin/git"))
        .args([std::ffi::OsStr::new("worktree"),std::ffi::OsStr::new("move"),path.as_os_str(),raw.as_os_str()]).output().unwrap();
    assert!(output.status.success(),"{output:?}");
    let alias=fixture.root.path().join("project/raw-alias");
    std::os::unix::fs::symlink(&raw,&alias).unwrap();
    let output=fixture.output(&["worktree","remove","raw-alias","--force"]);
    assert!(!output.status.success(),"{output:?}");
    let value:Value=serde_json::from_slice(&output.stdout).unwrap();
    assert!(value.to_string().contains("refusing a lossy removal target"),"{value}");
    assert!(raw.join("tracked").is_file());
}

#[test]
fn policy_read_and_write_do_not_depend_on_a_successful_safety_census() {
    use cagents::{device_service,worktree_control::{Operation,Reply}};
    let fixture=Fixture::new();let path=fixture.create("policy");
    let admin=fixture.git(&path,&["rev-parse","--absolute-git-dir"]);
    let index=Path::new(admin.trim()).join("index");
    fs::rename(&index,index.with_extension("fixture-backup")).unwrap();
    fs::create_dir(&index).unwrap();
    let socket=fixture.root.path().join("run").join(cagents::instance::RUNTIME_SUBDIRECTORY).join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut client=cagents::control::ControlClient::connect(&socket).unwrap();
    let repository=fixture.root.path().join("project/.git");
    assert!(client.device_operation(device_service::Operation::Worktree {operation:Operation::Inspect {repository:repository.clone(),path:path.clone()}}).is_err());
    let read=client.device_operation(device_service::Operation::Worktree {operation:Operation::GetDisposableIgnoredDirectories {repository:repository.clone(),path:path.clone()}}).unwrap();
    let device_service::Reply::Worktree(Reply::DisposableIgnoredDirectories {mut directories})=read else {panic!("wrong policy reply")};
    directories.push("private-cache".into());
    let saved=client.device_operation(device_service::Operation::Worktree {operation:Operation::SetDisposableIgnoredDirectories {repository:repository.clone(),path:path.clone(),directories:directories.clone()}}).unwrap();
    assert_eq!(saved,device_service::Reply::Worktree(Reply::DisposableIgnoredDirectories {directories:directories.clone()}));
    let read=client.device_operation(device_service::Operation::Worktree {operation:Operation::GetDisposableIgnoredDirectories {repository,path}}).unwrap();
    assert_eq!(read,saved);
}
