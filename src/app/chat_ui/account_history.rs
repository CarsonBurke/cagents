//! Account-file deletion invalidates local presentation, never provider work.
//! Association comes from the attached backend, not an issued launch recipe.
use super::*;
use std::os::unix::fs::MetadataExt;

const MAX_PENDING_DELETIONS: usize = 64;

#[derive(Clone, PartialEq, Eq)]
struct Deletion {
    id: AccountId,
    provider: Provider,
    physical: Option<(PathBuf, u64, u64)>,
}

#[derive(Default)]
pub(super) struct State {
    pending: HashMap<AccountId, Deletion>,
    /// Only starts already in flight at intent are fenced. A subsequently
    /// authenticated new handle gets its own local cache and provenance.
    inflight: HashMap<Target, Vec<Deletion>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision { Keep, Revoke, UnverifiedLive }

fn decide(target: &Deletion, provider: Provider, home: Option<&account::HomeIdentity>, read_only: bool, has_handle: bool) -> Decision {
    if let Some(home) = home {
        if home.purge_ids.contains(&target.id) || target.physical.as_ref().is_some_and(|(path, device, inode)| {
            home.canonical_home == *path && (home.device,home.inode) == (*device,*inode)
        }) { Decision::Revoke } else { Decision::Keep }
    } else if provider != target.provider { Decision::Keep }
    else if read_only { Decision::Revoke }
    else if has_handle { Decision::UnverifiedLive }
    else { Decision::Keep } // Unstarted draft/composer is not retained history.
}

fn decision(target: &Deletion, runtime: &Runtime) -> Decision {
    decide(target,runtime.session.launch.provider,runtime.local_history_account_home(),
        runtime.local_history_is_read_only(),runtime.has_local_history_handle())
}

impl Cagents {
    /// Called synchronously at explicit DeleteFiles intent, before its RPC.
    /// Archive and Move retain history and never enter this path.
    pub(in crate::app) fn prepare_account_history_deletion(&mut self, id: &AccountId) -> Result<(),String> {
        let state = self.account_ui.account(id).ok_or("The account changed; reload Accounts before deleting its files")?;
        let physical = state.account.resolved_home().and_then(|home| {
            let path = home.canonicalize().ok()?;
            let metadata = std::fs::metadata(&path).ok()?;
            metadata.is_dir().then(|| (path,metadata.dev(),metadata.ino()))
        });
        let deletion = Deletion { id:id.clone(), provider:state.account.provider, physical };
        if self.cards.values().filter_map(|runtime| match runtime { CardRuntime::Chat(runtime)=>Some(runtime.as_ref()),_=>None })
            .chain(self.chat_ui.parked.values().map(Box::as_ref))
            .any(|runtime| decision(&deletion,runtime)==Decision::UnverifiedLive) {
            return Err(format!("Close the live {} conversation whose account cannot be verified before deleting account files.",deletion.provider.label()));
        }
        let pending = &self.chat_ui.history_deletions.pending;
        if !pending.contains_key(id) && pending.len() >= MAX_PENDING_DELETIONS {
            return Err("Wait for current account deletions before deleting more files.".into());
        }
        let targets: HashSet<_> = self.chat_ui.phases.values().filter_map(|phase| match phase { Phase::Starting(attempt)=>Some(attempt.target),_=>None })
            .chain(self.chat_ui.abandoned.keys().copied()).collect();
        let fences = &mut self.chat_ui.history_deletions.inflight;
        fences.retain(|target,_| targets.contains(target));
        if targets.iter().any(|target| fences.get(target).is_some_and(|entries| entries.len()>=MAX_PENDING_DELETIONS && !entries.contains(&deletion))) {
            return Err("A previous conversation is still reconnecting. Let it finish or close it before deleting more account files.".into());
        }
        for target in targets {
            let entries = fences.entry(target).or_default();
            if !entries.contains(&deletion) { entries.push(deletion.clone()); }
        }
        self.revoke_account_history(&deletion);
        self.chat_ui.history_deletions.pending.insert(id.clone(),deletion);
        Ok(())
    }

    fn revoke_account_history(&mut self, deletion: &Deletion) {
        for runtime in self.cards.values_mut().filter_map(|runtime| match runtime { CardRuntime::Chat(runtime)=>Some(runtime.as_mut()),_=>None })
            .chain(self.chat_ui.parked.values_mut().map(Box::as_mut)) {
            if decision(deletion,runtime)==Decision::Revoke { runtime.revoke_local_history(); }
        }
    }

    /// Repeat before adopting the result, including errors/partial deletion:
    /// intent already revoked these local caches; a late attachment cannot
    /// reinstall one while provider logout or file sweeping was pending.
    pub(in crate::app) fn finish_account_history_deletion(&mut self, id: &AccountId) {
        if let Some(deletion)=self.chat_ui.history_deletions.pending.remove(id) {
            self.revoke_account_history(&deletion);
        }
    }

    pub(super) fn fence_account_history_ready(&mut self, target: Target, result: &Result<Arc<ClientHandle>,chat::StartFailure>) {
        let Some(deletions)=self.chat_ui.history_deletions.inflight.remove(&target) else {return;};
        if let Ok(handle)=result {
            for deletion in deletions {
                if decide(&deletion,handle.session().launch.provider,handle.account_home(),handle.local_history_is_read_only(),true)==Decision::Revoke {
                    handle.revoke_local_history();
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deletion_association_preserves_other_accounts_and_gates_unknown_live_history() {
        let id:AccountId="claude/11111111-1111-4111-8111-111111111111".parse().unwrap();
        let deletion=Deletion{id:id.clone(),provider:Provider::Claude,physical:Some(("/fixture/a".into(),1,2))};
        let known=|path:&str,inode|account::HomeIdentity{provider:Provider::Claude,canonical_home:path.into(),device:1,inode,purge_ids:Vec::new()};
        let a=known("/fixture/a",2);let b=known("/fixture/b",3);
        assert_eq!(decide(&deletion,Provider::Claude,Some(&a),false,true),Decision::Revoke);
        assert_eq!(decide(&deletion,Provider::Claude,Some(&b),true,true),Decision::Keep);
        let mut alias=known("/fixture/previous-a",4);alias.purge_ids.push(id);
        assert_eq!(decide(&deletion,Provider::Claude,Some(&alias),true,true),Decision::Revoke,"validated old registration remains purge-only association");
        assert_eq!(decide(&deletion,Provider::Claude,None,true,true),Decision::Revoke);
        assert_eq!(decide(&deletion,Provider::Claude,None,false,true),Decision::UnverifiedLive);
        assert_eq!(decide(&deletion,Provider::Codex,None,true,true),Decision::Keep);
        assert_eq!(decide(&deletion,Provider::Claude,None,false,false),Decision::Keep,"drafts and unsent text survive");
        assert_eq!(decide(&deletion,Provider::Claude,Some(&known("/fixture/a",5)),true,true),Decision::Keep,"a replaced path alone never transfers physical association");
    }
}
