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
        for path in ["home","run","data","config","cache","tmp","bin","project"] {fs::create_dir(root.path().join(path)).unwrap();}
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
    fn output(&self,args:&[&str])->Output {
        let mut child=Self::command(self.root.path(),Path::new(env!("CARGO_BIN_EXE_cagents"))).arg("--json").args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
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
    let fixture=Fixture::new();let path=fixture.create("feat-one");
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
    let plan=cagents::worktree_setup::load(&project).unwrap().unwrap();
    cagents::worktree_setup::trust(&project,&plan,&cagents::repository::CancellationToken::new()).unwrap();
    let created=fixture.cli(&["worktree","create","trusted"]);let path=Path::new(created["worktree"]["path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(path.join(".env")).unwrap(),"fixture secret\n");
    assert!(created["setup"]["terminal"]["id"].is_string(),"{created}");
    let deadline=Instant::now()+Duration::from_secs(10);
    while !path.join("setup-result").exists() {assert!(Instant::now()<deadline,"setup terminal did not run: {created}");thread::sleep(Duration::from_millis(20));}
    assert_eq!(fs::read_to_string(path.join("setup-result")).unwrap(),"ready");
    assert_eq!(created["setup"]["terminal"]["launch"]["environment"]["CAGENTS_WORKTREE_SETUP"],"1");
}
