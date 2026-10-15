use super::Relocation;
mod files;
use files::{Directory,Edit};
use std::path::{Path,PathBuf};

// A Locate choice must not attach unrelated linked checkouts to this repo.
// Verify both the old marker and the moved admin directory's backlink before
// handing an explicit linked path to `git worktree repair`.
fn read_marker(path:&Path)->Option<String> {
    use std::{io::Read,os::unix::fs::OpenOptionsExt};
    let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    if !file.metadata().ok()?.is_file() { return None; }
    let mut text=String::new();
    file.take(4097).read_to_string(&mut text).ok()?;
    (text.len()<=4096).then_some(text)
}

fn normalized(path:&Path)->PathBuf {
    let mut result=PathBuf::new();for component in path.components(){match component{std::path::Component::CurDir=>{},std::path::Component::ParentDir=>{result.pop();},component=>result.push(component.as_os_str())}}result
}
fn previous_path(path:&Path,relocation:&Relocation)->PathBuf {
    path.strip_prefix(&relocation.new_root).map(|relative|relocation.old_root.join(relative)).unwrap_or_else(|_|path.to_owned())
}
fn linked_admin(path:&Path,marker:&str,relocation:&Relocation)->Option<PathBuf> {
    let raw=Path::new(marker.trim().strip_prefix("gitdir: ")?);
    let candidates=if raw.is_absolute(){vec![raw.to_owned()]}else{vec![path.join(raw),previous_path(path,relocation).join(raw)]};
    let parent=relocation.destination.join("worktrees");
    for candidate in candidates {
        let candidate=normalized(&candidate);
        // Retry may observe our first linking edit already committed. Prefer
        // the exact current admin before considering the old→new mapping.
        let current=if candidate.parent()==Some(parent.as_path()){candidate}else if candidate.starts_with(&relocation.repository){relocation.mapped(&candidate)}else{continue};
        if current.parent()==Some(parent.as_path()){return Some(current);}
    }
    None
}
fn backlink_matches(backlink:&str,admin:&Path,path:&Path,relocation:&Relocation)->bool {
    let raw=Path::new(backlink.trim());
    let old_admin=admin.strip_prefix(&relocation.destination).map(|relative|relocation.repository.join(relative)).unwrap_or_else(|_|admin.to_owned());
    let candidates=if raw.is_absolute(){vec![raw.to_owned()]}else{vec![admin.join(raw),old_admin.join(raw)]};
    candidates.into_iter().map(|path|normalized(&path)).any(|candidate|candidate==path.join(".git")||relocation.mapped(&candidate)==path.join(".git"))
}
pub(crate) fn belongs_to_relocated_worktree(path:&Path,relocation:&Relocation)->bool {
    let Some(marker)=read_marker(&path.join(".git")) else{return false;};
    let Some(admin)=linked_admin(path,&marker,relocation) else{return false;};
    read_marker(&admin.join("gitdir")).is_some_and(|backlink|backlink_matches(&backlink,&admin,path,relocation))
}

// `git worktree repair` does not update an external Git directory's
// core.worktree. Change only the exact previous checkout chosen by Locate.
pub(crate) fn repair_checkout_config(relocation: &Relocation) -> Result<(), String> {
    repair_config_with(relocation,||{})
}

fn repair_config_with(relocation:&Relocation,before_write:impl FnOnce())->Result<(),String> {
    let root=Directory::open(&relocation.new_root)?;
    let common=Directory::open(&relocation.destination)?;
    repair_config_pinned(relocation,&root,&common,before_write)
}

fn repair_config_pinned(relocation:&Relocation,root:&Directory,common:&Directory,before_write:impl FnOnce())->Result<(),String> {
    use crate::repository::{CommandSpec,ProcessExecutor,SystemProcessExecutor};
    root.require_current()?;common.require_current()?;
    let (admin,discovered)=crate::project_location::checkout_identity(&root.anchor()).ok_or("The selected checkout no longer has readable Git metadata")?;
    if discovered!=relocation.destination {return Err("The selected checkout changed repositories while locating it".into());}
    root.require_current()?;common.require_current()?;
    if admin!=common.path() {return Ok(());}
    let original=common.read("config",8*1024*1024)?;
    // Let Git parse and preserve its config syntax in a private working copy.
    // It never receives a mutable repository pathname. The final replace is
    // performed by us relative to the retained common-directory FD.
    let temporary=tempfile::tempdir().map_err(|error|error.to_string())?;
    let config=temporary.path().join("config");
    std::fs::write(&config,&original).map_err(|error|error.to_string())?;
    let command=||CommandSpec::new("git").args(["config","--no-includes","--file"]).arg(&config)
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_COMMON_DIR").env_remove("GIT_INDEX_FILE")
        .timeout(std::time::Duration::from_secs(15)).output_limits(64*1024,64*1024);
    let read=command().args(["--null","--get","core.worktree"]);
    let output=SystemProcessExecutor.execute(&read,&crate::repository::CancellationToken::default()).map_err(|error|error.to_string())?;
    if output.exit_code==Some(1) {return Ok(());}
    let output=output.require_success(&read.program).map_err(|error|error.to_string())?;
    let bytes=output.stdout.strip_suffix(&[0]).ok_or("Git returned an invalid checkout setting")?;
    let configured=PathBuf::from(String::from_utf8(bytes.to_vec()).map_err(|_|"Git checkout setting is not valid UTF-8")?);
    let absolute=if configured.is_absolute(){configured}else{relocation.destination.join(configured)};
    let mut normalized=PathBuf::new();
    for component in absolute.components(){match component {std::path::Component::CurDir=>{},std::path::Component::ParentDir=>{normalized.pop();},component=>normalized.push(component.as_os_str())}}
    if normalized==relocation.new_root {return Ok(());}
    if normalized!=relocation.old_root {return Err(format!("This Git directory names another checkout: {}. Its setting was preserved.",normalized.display()));}
    let write=command().arg("core.worktree").arg(&relocation.new_root);
    SystemProcessExecutor.execute(&write,&crate::repository::CancellationToken::default()).and_then(|output|output.require_success(&write.program)).map_err(|error|error.to_string())?;
    let after=std::fs::read(&config).map_err(|error|error.to_string())?;
    before_write();
    root.require_current()?;common.require_current()?;
    Edit::new(common.clone(),"config",original,after).apply()?;
    root.require_current()
}

/// The caller holds the exact project's scoped location reservation. Perform
/// repairs only after registry instance validation, never in a stale chooser.
pub(crate) fn repair(relocation: &Relocation, worktrees: &[PathBuf]) -> Result<(), String> {
    let root=Directory::open(&relocation.new_root)?;
    let common=Directory::open(&relocation.destination)?;
    repair_config_pinned(relocation,&root,&common,||{})?;
    let mut known=worktrees.to_vec();
    known.push(relocation.new_root.clone());
    if let Ok(entries)=std::fs::read_dir(common.anchor().join("worktrees")) {
        for entry in entries.take(4096).flatten() {
            if let Some(marker)=read_marker(&entry.path().join("gitdir")) {
                if let Some(path)=Path::new(marker.trim()).parent(){known.push(path.to_owned());}
            }
        }
    }
    known.sort();known.dedup();
    for path in known {
        let path=if path==relocation.new_root{path}else{relocation.mapped(&path)};
        if !belongs_to_relocated_worktree(&path,relocation){continue;}
        let checkout=Directory::open(&path)?;
        let before=checkout.read(".git",4096)?;
        let marker=std::str::from_utf8(&before).map_err(|error|error.to_string())?;
        let admin_path=linked_admin(&path,marker,relocation).ok_or("Linked checkout changed Git ownership during Locate")?;
        let admin=Directory::open(&admin_path)?;
        let backlink=admin.read("gitdir",4096)?;
        let old=std::str::from_utf8(&backlink).map_err(|error|error.to_string())?;
        if !backlink_matches(old,&admin_path,&path,relocation){return Err("Linked checkout backlink changed during Locate".into());}
        root.require_current()?;common.require_current()?;checkout.require_current()?;admin.require_current()?;
        // Git worktree repair canonicalizes paths internally before writes;
        // retained argument FDs alone cannot fence those later opens. Repair
        // these already verified linking records with directory-relative edits.
        Edit::new(checkout.clone(),".git",before,format!("gitdir: {}\n",admin_path.display()).into_bytes()).apply()?;
        root.require_current()?;common.require_current()?;checkout.require_current()?;
        Edit::new(admin,"gitdir",backlink,format!("{}\n",path.join(".git").display()).into_bytes()).apply()?;
        checkout.require_current()?;
    }
    root.require_current()?;common.require_current()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_repair_refuses_replaced_checkout_or_admin_after_read_without_touching_replacement() {
        for replace_admin in [false,true] {
            let fixture=tempfile::tempdir().unwrap();
            let old=fixture.path().join("old");let selected=fixture.path().join("selected");let common=fixture.path().join("repo.git");
            std::fs::create_dir(&selected).unwrap();std::fs::create_dir(&common).unwrap();
            std::fs::write(selected.join(".git"),format!("gitdir: {}\n",common.display())).unwrap();
            std::fs::write(common.join("config"),format!("[core]\nworktree = {}\n",old.display())).unwrap();
            let relocation=Relocation{repository:common.clone(),destination:common.clone(),old_root:old,new_root:selected.clone()};
            let replaced=if replace_admin {common.clone()}else{selected.clone()};
            let retained=fixture.path().join("original-inode");
            let result=repair_config_with(&relocation,|| {
                std::fs::rename(&replaced,&retained).unwrap();std::fs::create_dir(&replaced).unwrap();
                std::fs::write(replaced.join("config"),b"replacement must remain exact").unwrap();
            });
            assert!(result.unwrap_err().contains("replaced"));
            assert_eq!(std::fs::read(replaced.join("config")).unwrap(),b"replacement must remain exact");
            let original_config=if replace_admin {retained.join("config")}else{common.join("config")};
            assert!(String::from_utf8(std::fs::read(original_config).unwrap()).unwrap().contains("/old"));
            assert!(!common.join("config.lock").exists());
        }
    }
    #[test]
    fn selected_moved_linked_checkout_repairs_its_backlink_and_is_not_prunable() {
        let fixture=tempfile::tempdir().unwrap();
        let primary=fixture.path().join("primary");let old=fixture.path().join("linked");let moved=fixture.path().join("moved");
        let git=|directory:&Path,args:&[&str]| {
            let output=std::process::Command::new("git").current_dir(directory)
                .env_remove("GIT_DIR").env_remove("GIT_COMMON_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE")
                .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_NOSYSTEM","1")
                .args(["-c","core.fsmonitor=false","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","-c","user.name=Fixture","-c","user.email=fixture@example.invalid"]).args(args).output().unwrap();
            assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));output
        };
        git(fixture.path(),&["init","-b","main",primary.to_str().unwrap()]);
        git(&primary,&["commit","--allow-empty","-m","fixture"]);
        git(&primary,&["worktree","add","-b","linked",old.to_str().unwrap()]);
        let common=primary.join(".git");
        let admin=crate::project_location::checkout_identity(&old).unwrap().0;
        std::fs::rename(&old,&moved).unwrap();
        let relocation=Relocation{repository:common.clone(),destination:common,old_root:old.clone(),new_root:moved.clone()};
        repair(&relocation,&[]).unwrap();
        assert_eq!(std::fs::read_to_string(admin.join("gitdir")).unwrap().trim(),moved.join(".git").to_str().unwrap());
        let listed=git(&primary,&["worktree","list","--porcelain"]);
        let listed=String::from_utf8(listed.stdout).unwrap();
        assert!(listed.contains(moved.to_str().unwrap()));assert!(!listed.contains(&format!("worktree {}\n",old.display())));
        let prune=git(&primary,&["worktree","prune","--dry-run","--verbose"]);
        assert!(!String::from_utf8_lossy(&prune.stdout).contains("linked"));assert!(!String::from_utf8_lossy(&prune.stderr).contains("linked"));
    }
    #[test]
    fn retry_completes_a_link_whose_checkout_marker_already_names_the_new_admin() {
        let fixture=tempfile::tempdir().unwrap();
        let old=fixture.path().join("old");let moved=fixture.path().join("moved");
        let common=moved.join(".git");let admin=common.join("worktrees/linked");let linked=moved.join("linked");
        std::fs::create_dir_all(&admin).unwrap();std::fs::create_dir(&linked).unwrap();
        std::fs::write(common.join("config"),b"[core]\nbare = false\n").unwrap();
        std::fs::write(linked.join(".git"),format!("gitdir: {}\n",admin.display())).unwrap();
        std::fs::write(admin.join("gitdir"),format!("{}\n",old.join("linked/.git").display())).unwrap();
        let relocation=Relocation{repository:old.join(".git"),destination:common,old_root:old,new_root:moved.clone()};
        assert!(belongs_to_relocated_worktree(&linked,&relocation));
        repair(&relocation,&[]).unwrap();
        assert_eq!(std::fs::read_to_string(admin.join("gitdir")).unwrap().trim(),linked.join(".git").to_str().unwrap());
        repair(&relocation,&[]).unwrap();
    }

}
