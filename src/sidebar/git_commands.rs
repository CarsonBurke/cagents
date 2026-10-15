//! Whole-worktree commands retain the source the palette was opened against.
use super::*;
use crate::repository::GitOperation;
use crate::workspace_widget::GlobalShortcut as Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTarget {
    pub key: ActiveWorktreeKey,
    branch: Option<ValidatedBranchName>,
    upstream: Option<PushTarget>,
}

impl GitTarget {
    pub fn capture(model: &SidebarModel, state: &SidebarState) -> Option<Self> {
        let source = model.source_control.as_ref()?;
        if model.project(source.key.project_id)?.kind != ProjectKind::Git { return None; }
        if state.active_worktree() != Some(&source.key) || !model.contains_worktree(&source.key) {
            return None;
        }
        Some(Self { key: source.key.clone(), branch: source.branch.clone(), upstream: source.upstream.clone() })
    }

    fn source<'a>(&self, model: &'a SidebarModel, state: &SidebarState) -> Option<&'a SourceControlModel> {
        let source = model.source_control.as_ref()?;
        (source.key == self.key && state.active_worktree() == Some(&self.key)
            && model.contains_worktree(&self.key) && source.branch == self.branch
            && source.upstream == self.upstream && !source.reloading
            && matches!(source.data_state, SidebarDataState::Ready)).then_some(source)
    }

    pub fn available(&self, model: &SidebarModel, state: &SidebarState, command: Command) -> bool {
        let Some(source) = self.source(model, state) else { return false; };
        match command {
            Command::GitApplyStash | Command::GitPopStash | Command::GitDropStash => !source.busy() && !source.stashes.is_empty(),
            _ => self.action(model, state, command).is_some(),
        }
    }

    pub fn action(&self, model: &SidebarModel, state: &SidebarState, command: Command) -> Option<SidebarAction> {
        let source = self.source(model, state)?;
        let key = self.key.clone();
        if command == Command::GitRefresh { return Some(SidebarAction::RefreshWorktree { key }); }
        if command == Command::GitStashes { return Some(SidebarAction::ToggleStashes { key }); }
        if command == Command::GitOutput && source.error.is_some() { return Some(SidebarAction::ToggleGitOutput { key }); }
        if source.busy() { return None; }
        let operation = match command {
            Command::GitFetch => GitOperation::Fetch { remote: source.upstream.as_ref().map(|upstream| upstream.remote()).or_else(|| source.remotes.first().map(String::as_str))?.to_owned(), prune: true },
            Command::GitSync | Command::GitSyncRebase if source.upstream.is_some() && source.branch.is_some() && !source.detached => GitOperation::Sync { rebase: command == Command::GitSyncRebase },
            Command::GitStash | Command::GitStashUntracked if [ChangeArea::Staged, ChangeArea::Unstaged].into_iter().chain((command == Command::GitStashUntracked).then_some(ChangeArea::Untracked)).any(|area| source.group(area).is_some_and(|group| !group.is_empty())) && source.group(ChangeArea::Conflict).is_none_or(ChangeGroup::is_empty) => GitOperation::StashPush { message: None, include_untracked: command == Command::GitStashUntracked },
            Command::GitPush if source.can_push() => return Some(SidebarAction::Push { key, branch: source.branch.clone()?, target: source.upstream.clone()? }),
            Command::GitPublish if source.can_publish() => return Some(SidebarAction::OpenPublish { key, branch: source.branch.clone()?, remotes: source.remotes.clone() }),
            Command::GitAmend if source.head_oid.is_some() => return Some(SidebarAction::AmendChanged { key, amend: !state.commit_draft.amend }),
            Command::GitCommit if !state.commit_draft.subject.trim().is_empty()
                && (source.staged_count > 0 || (state.commit_draft.amend && source.head_oid.is_some()) || source.operation.as_ref().is_some_and(|operation| operation.kind == crate::repository::GitOperationKind::Merge))
                && source.group(ChangeArea::Conflict).is_none_or(ChangeGroup::is_empty) => return Some(SidebarAction::Commit { key, message: state.commit_message().to_owned(), amend: state.commit_draft.amend }),
            Command::GitAbort => GitOperation::Abort { kind: source.operation.as_ref()?.kind },
            Command::GitContinue if source.group(ChangeArea::Conflict).is_none_or(ChangeGroup::is_empty) => GitOperation::Continue { kind: source.operation.as_ref()?.kind },
            _ => return None,
        };
        Some(SidebarAction::GitOperation { key, operation })
    }

    /// Both selector and object identity must still match. Reordering the stash
    /// list never substitutes another entry for the one shown in the picker.
    pub fn stash_action(&self, model: &SidebarModel, state: &SidebarState, command: Command, entry: &crate::repository::StashEntry) -> Option<SidebarAction> {
        let source = self.source(model, state)?;
        if source.busy() || !source.stashes.iter().any(|current| current.reference == entry.reference && current.oid == entry.oid) { return None; }
        let key = self.key.clone();
        Some(match command {
            Command::GitApplyStash | Command::GitPopStash => SidebarAction::GitOperation { key, operation: GitOperation::StashApply { reference: entry.reference.clone(), expected_oid: entry.oid.clone(), pop: command == Command::GitPopStash } },
            Command::GitDropStash => SidebarAction::RequestStashDrop { key, entry: Some(entry.clone()) },
            _ => return None,
        })
    }

    pub fn stashes<'a>(&self, model: &'a SidebarModel, state: &SidebarState) -> &'a [crate::repository::StashEntry] {
        self.source(model, state).filter(|source| !source.busy()).map_or(&[], |source| &source.stashes)
    }
}

pub fn is_stash_picker(command: Command) -> bool {
    matches!(command, Command::GitApplyStash | Command::GitPopStash | Command::GitDropStash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_worktree_commands_share_capabilities_and_reject_rebound_sources() {
        let mut model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        model.source_control.as_mut().unwrap().remotes = vec!["origin".into()];
        model.source_control.as_mut().unwrap().groups.retain(|group| group.area != ChangeArea::Conflict);
        let target = GitTarget::capture(&model, &state).unwrap();
        assert!(matches!(target.action(&model, &state, Command::GitFetch), Some(SidebarAction::GitOperation { operation: GitOperation::Fetch { remote, prune: true }, .. }) if remote == "origin"));
        assert!(!target.available(&model, &state, Command::GitCommit), "a commit needs a message");
        state.apply(&SidebarAction::CommitMessageChanged { key: target.key.clone(), message: "Keep subject and body\n\nDetails".into() });
        assert!(matches!(target.action(&model, &state, Command::GitCommit), Some(SidebarAction::Commit { message, .. }) if message == state.commit_message()));
        for descriptor in crate::workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS.iter().filter(|descriptor| crate::workspace_widget::git_command_bit(descriptor.action).is_some() && !is_stash_picker(descriptor.action)) {
            assert_eq!(target.available(&model, &state, descriptor.action), target.action(&model, &state, descriptor.action).is_some());
        }
        model.source_control.as_mut().unwrap().set_mutation(Some(MutationKind::Other));
        assert!(!target.available(&model, &state, Command::GitFetch));
        assert!(target.available(&model, &state, Command::GitRefresh));
        model.source_control.as_mut().unwrap().set_mutation(None);
        model.source_control.as_mut().unwrap().branch = Some(ValidatedBranchName::parse("replacement").unwrap());
        assert!(!target.available(&model, &state, Command::GitFetch), "a branch change cannot retarget an open palette");
    }

    #[test]
    fn stash_commands_preserve_selector_oid_and_confirmation_instead_of_defaulting() {
        let mut model = super::super::tests::panel_model();
        let state = super::super::tests::panel_state(&model);
        let entry = crate::repository::StashEntry { reference: "stash@{2}".into(), oid: Oid::parse(&"a".repeat(40)).unwrap(), message: "chosen stash".into() };
        model.source_control.as_mut().unwrap().stashes = vec![entry.clone()];
        let target = GitTarget::capture(&model, &state).unwrap();
        assert!(matches!(target.stash_action(&model, &state, Command::GitPopStash, &entry), Some(SidebarAction::GitOperation { operation: GitOperation::StashApply { reference, expected_oid, pop: true }, .. }) if reference == entry.reference && expected_oid == entry.oid));
        assert!(matches!(target.stash_action(&model, &state, Command::GitDropStash, &entry), Some(SidebarAction::RequestStashDrop { entry: Some(shown), .. }) if shown == entry));
        model.source_control.as_mut().unwrap().stashes[0].oid = Oid::parse(&"b".repeat(40)).unwrap();
        assert!(target.stash_action(&model, &state, Command::GitApplyStash, &entry).is_none());
        model.source_control.as_mut().unwrap().stashes.clear();
        assert!(!target.available(&model, &state, Command::GitApplyStash));
    }
}
