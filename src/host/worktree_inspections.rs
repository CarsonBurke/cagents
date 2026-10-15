//! Explicit Stop consent retains the objects inspected, through the effect.
//! A fresh path or a later Remove fingerprint cannot replace this authority.
use super::{delivery::DeliveryQueue, state::{ClientId, SharedHost}};
use crate::{fs_safety::Root, repository::{CancellationToken, RepositoryService}, worktree_control::Reply};
use std::{os::unix::fs::MetadataExt, path::{Path, PathBuf}, sync::{Arc, Weak}, time::{Duration, Instant}};

const TTL: Duration = Duration::from_secs(300);
const PER_CLIENT: usize = 8;
const TOTAL: usize = 128;
const REFRESH: &str = "Session-stop inspection expired or changed; refresh and review the worktree again";

struct Pin { path: PathBuf, root: Root }
impl Pin {
    fn open(path: PathBuf) -> Result<Self, String> {
        let root = Root::open(&path).map_err(|error| error.to_string())?;
        let pin = Self { path, root };
        pin.validate()?;
        Ok(pin)
    }
    fn validate(&self) -> Result<(), String> {
        let retained = self.root.metadata().map_err(|error| error.to_string())?;
        let current = std::fs::symlink_metadata(&self.path).map_err(|_| REFRESH.to_owned())?;
        if !current.is_dir() || (current.dev(), current.ino()) != (retained.dev(), retained.ino()) {
            return Err(REFRESH.into());
        }
        Ok(())
    }
}
struct Physical {
    repository: Pin,
    checkout: Pin,
    admin: Pin,
    common: Pin,
}
impl Physical {
    fn capture(repository: &Path, path: &Path) -> Result<Self, String> {
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let checkout = super::worktree_sessions::selected_worktree(repository, path)?;
        let repository = Pin::open(repository.canonicalize().map_err(|error| error.to_string())?)?;
        let checkout = Pin::open(checkout)?;
        let common = Pin::open(service.common_dir(&repository.path, &cancel).map_err(|error| error.to_string())?)?;
        let admin = Pin::open(service.git_dir(&checkout.path, &cancel).map_err(|error| error.to_string())?)?;
        let physical = Self { repository, checkout, admin, common };
        physical.validate()?;
        Ok(physical)
    }
    fn validate(&self) -> Result<(), String> {
        self.repository.validate()?;
        self.checkout.validate()?;
        self.admin.validate()?;
        self.common.validate()?;
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        if service.common_dir(&self.repository.path, &cancel).map_err(|error| error.to_string())? != self.common.path
            || service.common_dir(&self.checkout.path, &cancel).map_err(|error| error.to_string())? != self.common.path
            || service.git_dir(&self.checkout.path, &cancel).map_err(|error| error.to_string())? != self.admin.path {
            return Err(REFRESH.into());
        }
        // Git resolved names above; repeat descriptor identity checks afterward.
        self.repository.validate()?;
        self.checkout.validate()?;
        self.admin.validate()?;
        self.common.validate()
    }
}

pub(super) struct Inspection {
    pub(super) client: ClientId,
    connection: Weak<DeliveryQueue>,
    repository_request: PathBuf,
    path_request: PathBuf,
    physical: Physical,
    fingerprint: String,
    expires: Instant,
}
impl Inspection {
    pub(super) fn captured_scope(&self) -> (&Path, &Path) {
        (&self.physical.repository.path, &self.physical.checkout.path)
    }
    fn validate_requested_names(&self) -> Result<(), String> {
        if self.repository_request.canonicalize().map_err(|_| REFRESH)? != self.physical.repository.path
            || self.path_request.canonicalize().map_err(|_| REFRESH)? != self.physical.checkout.path {
            return Err(REFRESH.into());
        }
        Ok(())
    }
    pub(super) fn validate(&self) -> Result<(), String> {
        self.validate_with_clock(Instant::now)
    }
    fn validate_with_clock(&self, now: impl Fn() -> Instant) -> Result<(), String> {
        if now() >= self.expires || self.connection.upgrade().is_none() { return Err(REFRESH.into()); }
        self.validate_requested_names()?;
        self.physical.validate()?;
        let snapshot = RepositoryService::system("git")
            .worktree_safety(&self.physical.checkout.path, &CancellationToken::new())
            .map_err(|error| error.to_string())?;
        if snapshot.fingerprint != self.fingerprint { return Err(REFRESH.into()); }
        self.physical.validate()?;
        self.validate_requested_names()?;
        // Slow Git/filesystem inspection cannot extend the original authority.
        if now() >= self.expires || self.connection.upgrade().is_none() { return Err(REFRESH.into()); }
        Ok(())
    }
}

pub(super) fn inspect(host: &SharedHost, client: ClientId, repository: &Path, path: &Path) -> Result<Reply, String> {
    // Capture before inspecting, and validate after it. No HostLock spans any
    // filesystem or Git read. The original descriptor set remains retained.
    let physical = Physical::capture(repository, path)?;
    let service = RepositoryService::system("git");
    let cancel = CancellationToken::new();
    let snapshot = service.worktree_safety(&physical.checkout.path, &cancel).map_err(|error| error.to_string())?;
    let directories = service.disposable_ignored_directories(&physical.checkout.path, &cancel).map_err(|error| error.to_string())?;
    physical.validate()?;
    let fingerprint = snapshot.fingerprint.clone();
    let snapshot = serde_json::to_string(&snapshot).map_err(|error| error.to_string())?;
    let token = uuid::Uuid::new_v4();
    let now = Instant::now();
    let mut state = host.lock().unwrap_or_else(|poison| poison.into_inner());
    let connection = state.clients.get(&client).ok_or(REFRESH)?;
    let connection = Arc::downgrade(connection);
    state.stop_inspections.retain(|_, entry| now < entry.expires && entry.connection.upgrade().is_some());
    if state.stop_inspections.len() >= TOTAL || state.stop_inspections.values().filter(|entry| entry.client == client).count() >= PER_CLIENT {
        return Err("Too many outstanding session-stop inspections; close old dialogs or refresh after they expire".into());
    }
    state.stop_inspections.insert(token, Inspection {
        client, connection, repository_request: repository.to_owned(), path_request: path.to_owned(),
        physical, fingerprint, expires: now + TTL,
    });
    Ok(Reply::Inspected { snapshot, disposable_ignored_directories: directories, stop_authority: Some(token) })
}

pub(super) fn take(host: &SharedHost, client: ClientId, token: uuid::Uuid, repository: &Path, path: &Path, expected_fingerprint: &str) -> Result<Inspection, String> {
    let mut state = host.lock().unwrap_or_else(|poison| poison.into_inner());
    let entry = state.stop_inspections.get(&token).ok_or(REFRESH)?;
    let connection = state.clients.get(&client).ok_or(REFRESH)?;
    if entry.client != client || !entry.connection.upgrade().is_some_and(|original| Arc::ptr_eq(&original, connection))
        || entry.repository_request != repository || entry.path_request != path
        || entry.fingerprint != expected_fingerprint || Instant::now() >= entry.expires {
        return Err(REFRESH.into());
    }
    // Single use: a failed stop also needs a new inspection and explicit review.
    state.stop_inspections.remove(&token).ok_or_else(|| REFRESH.into())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::{BootId, HostIncarnation, HostSessionId, HostSessionProof}, terminal_ipc::{ClientRequest, ServerResponse}, worktree_control::{Operation, SessionStopConsent}};

    fn host() -> SharedHost {
        let host = Arc::new(super::super::state::HostLock::new(super::super::state::HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("stop-inspection-test"), boot_id: BootId::new("stop-inspection-boot"), host_incarnation: HostIncarnation::new("stop-inspection-incarnation"),
        })));
        host.lock().unwrap().register_client(1);
        host.lock().unwrap().register_client(2);
        host
    }
    fn git(path: &Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.as_encoded_bytes().starts_with(b"GIT_") { command.env_remove(key); }
        }
        let output = command.arg("-C").arg(path)
            .args(["-c","core.hooksPath=/dev/null","-c","init.templateDir=","-c","user.name=Fixture","-c","user.email=fixture@example.invalid"])
            .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_SYSTEM","/dev/null").env("GIT_CONFIG_NOSYSTEM","1")
            .args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}",String::from_utf8_lossy(&output.stderr));
    }
    fn repository() -> (tempfile::TempDir, PathBuf) {
        let parent = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = parent.path().join("checkout"); std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        (parent, root)
    }
    fn request(host: &SharedHost, client: ClientId, operation: Operation) -> Result<Reply, String> {
        let (reply,_,_) = super::super::client::process_request(host,client,ClientRequest::DeviceOperation {
            operation: crate::device_service::Operation::Worktree { operation },
        }).map_err(|error| format!("{error:?}"))?;
        match reply { ServerResponse::DeviceOperation { reply: crate::device_service::Reply::Worktree(reply) } => Ok(reply), _ => panic!("expected worktree reply") }
    }
    fn inspect_target(host: &SharedHost, root: &Path) -> (String, uuid::Uuid) {
        match request(host,1,Operation::InspectForSessionStop { repository:root.into(),path:root.into() }).unwrap() {
            Reply::Inspected { snapshot,stop_authority:Some(token),.. } => {
                let snapshot: crate::repository::WorktreeSafetySnapshot = serde_json::from_str(&snapshot).unwrap();
                (snapshot.fingerprint,token)
            }, _ => panic!("original physical inspection required"),
        }
    }
    fn stop(root: &Path, fingerprint: String, token: uuid::Uuid) -> Operation {
        Operation::StopSessions {repository:root.into(),path:root.into(),consent:SessionStopConsent { sessions:vec![] },expected_fingerprint:fingerprint,stop_authority:token}
    }
    #[test]
    fn original_inspection_is_connection_bound_and_single_use_at_actual_stop_admission() {
        let (_parent,root)=repository();let host=host();let (fingerprint,token)=inspect_target(&host,&root);
        assert!(request(&host,2,stop(&root,fingerprint.clone(),token)).is_err());
        assert!(host.lock().unwrap().stop_inspections.contains_key(&token),"foreign connection cannot consume original consent");
        assert!(matches!(request(&host,1,stop(&root,fingerprint.clone(),token)).unwrap(),Reply::SessionsStopped { .. }));
        assert!(request(&host,1,stop(&root,fingerprint,token)).is_err());
    }
    #[test]
    fn changed_files_and_expected_fingerprint_refuse_before_stop_fences() {
        let (_parent,root)=repository();let host=host();let (fingerprint,token)=inspect_target(&host,&root);
        assert!(request(&host,1,stop(&root,"different reviewed fingerprint".into(),token)).is_err());
        std::fs::write(root.join("late.txt"),"keep ongoing work").unwrap();
        assert!(request(&host,1,stop(&root,fingerprint,token)).is_err());
        assert!(!host.lock().unwrap().shutting_down);
        assert_eq!(std::fs::read_to_string(root.join("late.txt")).unwrap(),"keep ongoing work");
    }
    #[test]
    fn identical_clean_replacement_cannot_reuse_original_inspected_root_authority() {
        let (parent,root)=repository();let host=host();let (fingerprint,token)=inspect_target(&host,&root);
        std::fs::rename(&root,parent.path().join("original-checkout")).unwrap();
        std::fs::create_dir(&root).unwrap();git(&root,&["init","--quiet","--initial-branch=main"]);
        let replacement = RepositoryService::system("git").worktree_safety(&root,&CancellationToken::new()).unwrap();
        assert_eq!(replacement.fingerprint,fingerprint,"snapshot equality alone cannot distinguish this actual clean replacement");
        assert!(request(&host,1,stop(&root,fingerprint,token)).is_err());
        assert!(root.join(".git").is_dir() && parent.path().join("original-checkout/.git").is_dir());
    }
    #[test]
    fn retained_git_administrative_directory_replacement_refuses_original_stop() {
        let (_parent,root)=repository();let host=host();let (fingerprint,token)=inspect_target(&host,&root);
        std::fs::rename(root.join(".git"),root.join("old-admin")).unwrap();
        git(&root,&["init","--quiet","--initial-branch=main"]);
        assert!(request(&host,1,stop(&root,fingerprint,token)).is_err());
        assert!(root.join("old-admin").is_dir() && root.join(".git").is_dir());
    }
    #[test]
    fn expired_or_disconnected_inspection_cannot_rebind_to_a_new_connection() {
        let (_parent,root)=repository();let host=host();let (fingerprint,token)=inspect_target(&host,&root);
        host.lock().unwrap().stop_inspections.get_mut(&token).unwrap().expires=Instant::now()-Duration::from_secs(1);
        assert!(request(&host,1,stop(&root,fingerprint,token)).is_err());
        let (fingerprint,token)=inspect_target(&host,&root);
        host.lock().unwrap().unregister_client(1);
        host.lock().unwrap().register_client(1);
        assert!(request(&host,1,stop(&root,fingerprint,token)).is_err());
    }
    #[test]
    fn inspection_pin_budget_refuses_growth_and_expired_entries_release_capacity() {
        let (_parent,root)=repository();let host=host();let mut tokens=vec![];
        for _ in 0..PER_CLIENT { tokens.push(inspect_target(&host,&root).1); }
        assert!(request(&host,1,Operation::InspectForSessionStop { repository:root.clone(),path:root.clone() }).is_err());
        assert_eq!(host.lock().unwrap().stop_inspections.len(),PER_CLIENT);
        host.lock().unwrap().stop_inspections.get_mut(&tokens[0]).unwrap().expires=Instant::now()-Duration::from_secs(1);
        inspect_target(&host,&root);
        assert_eq!(host.lock().unwrap().stop_inspections.len(),PER_CLIENT);
    }
    #[test]
    fn original_request_alias_cannot_select_a_sibling_registered_worktree() {
        use std::os::unix::fs::symlink;
        let (parent,root)=repository();let host=host();
        git(&root,&["commit","--quiet","--allow-empty","-m","base"]);
        let sibling=parent.path().join("sibling");
        git(&root,&["worktree","add","--quiet","--detach",sibling.to_str().unwrap(),"HEAD"]);
        let alias=parent.path().join("selected");symlink(&root,&alias).unwrap();
        let (fingerprint,token)=match request(&host,1,Operation::InspectForSessionStop {repository:root.clone(),path:alias.clone()}).unwrap() {
            Reply::Inspected {snapshot,stop_authority:Some(token),..} => {
                let snapshot:crate::repository::WorktreeSafetySnapshot=serde_json::from_str(&snapshot).unwrap();
                (snapshot.fingerprint,token)
            }, _ => panic!("original authority required"),
        };
        std::fs::remove_file(&alias).unwrap();symlink(&sibling,&alias).unwrap();
        let operation=Operation::StopSessions {repository:root.clone(),path:alias,consent:SessionStopConsent {sessions:vec![]},expected_fingerprint:fingerprint,stop_authority:token};
        assert!(request(&host,1,operation).is_err(),"A token must never report or stop B under a changed alias");
        assert!(root.join(".git").is_dir() && sibling.join(".git").is_file());
    }
    #[test]
    fn expiry_during_actual_git_validation_refuses_at_the_effect_boundary() {
        let (_parent,root)=repository();let host=host();let (fingerprint,token)=inspect_target(&host,&root);
        let inspection=take(&host,1,token,&root,&root,&fingerprint).unwrap();
        let calls=std::cell::Cell::new(0);
        let before=Instant::now();let after=inspection.expires+Duration::from_nanos(1);
        assert!(inspection.validate_with_clock(|| {
            let call=calls.get();calls.set(call+1);
            if call==0 {before} else {after}
        }).is_err());
        assert_eq!(calls.get(),2,"actual validation must recheck expiry after all Git/filesystem reads");
    }

}
