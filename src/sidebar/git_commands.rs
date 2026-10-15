//! Whole-worktree commands retain the source the palette was opened against.
use super::*;
use crate::repository::GitOperation;
use crate::workspace_widget::GlobalShortcut as Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTarget {
    pub key: ActiveWorktreeKey,
    pub binding: crate::model::OperationId,
    branch: Option<ValidatedBranchName>,
    upstream: Option<PushTarget>,
}

// Availability is queried while menus and shortcut scopes are built. Keep it
// independent of the complete message; only activation constructs that payload.
pub(super) fn commit_available(source: &SourceControlModel, state: &SidebarState) -> bool {
    source.mutations_available()
        && !source.busy()
        && state.commit_draft.ready()
        && (!state.commit_draft.amend
            || (source.operation.is_none()
                && state
                    .commit_draft
                    .amend_head
                    .as_ref()
                    .is_some_and(|head| source.head_oid.as_deref() == Some(head.as_str()))))
        && !state.commit_draft.subject.trim().is_empty()
        && (source.staged_count > 0
            || (state.commit_draft.amend && source.head_oid.is_some())
            || source.operation.as_ref().is_some_and(|operation| {
                operation.kind == crate::repository::GitOperationKind::Merge
            }))
        && source
            .group(ChangeArea::Conflict)
            .is_none_or(ChangeGroup::is_empty)
}

impl GitTarget {
    pub fn capture(model: &SidebarModel, state: &SidebarState) -> Option<Self> {
        let source = model.source_control.as_ref()?;
        if model.project(source.key.project_id)?.kind != ProjectKind::Git {
            return None;
        }
        if state.active_worktree() != Some(&source.key) || !model.contains_worktree(&source.key) {
            return None;
        }
        Some(Self {
            key: source.key.clone(),
            binding: source.command_binding_id(),
            branch: source.branch.clone(),
            upstream: source.upstream.clone(),
        })
    }

    fn source<'a>(
        &self,
        model: &'a SidebarModel,
        state: &SidebarState,
    ) -> Option<&'a SourceControlModel> {
        let source = model.source_control.as_ref()?;
        (source.key == self.key
            && source.matches_command_binding(self.binding)
            && state.active_worktree() == Some(&self.key)
            && model.contains_worktree(&self.key)
            && source.branch == self.branch
            && source.upstream == self.upstream
            && !source.reloading
            && matches!(source.data_state, SidebarDataState::Ready))
        .then_some(source)
    }

    pub fn available(&self, model: &SidebarModel, state: &SidebarState, command: Command) -> bool {
        let Some(source) = self.source(model, state) else {
            return false;
        };
        match command {
            Command::GitCommit => commit_available(source, state),
            Command::GitApplyStash | Command::GitPopStash | Command::GitDropStash => {
                source.mutations_available() && !source.busy() && !source.stashes.is_empty()
            }
            _ => self.action(model, state, command).is_some(),
        }
    }

    pub fn action(
        &self,
        model: &SidebarModel,
        state: &SidebarState,
        command: Command,
    ) -> Option<SidebarAction> {
        self.action_unbound(model, state, command)
            .map(|action| super::commands::bound(self.binding, action))
    }

    fn action_unbound(
        &self,
        model: &SidebarModel,
        state: &SidebarState,
        command: Command,
    ) -> Option<SidebarAction> {
        let source = self.source(model, state)?;
        let key = self.key.clone();
        if command == Command::GitRefresh {
            return Some(SidebarAction::RefreshWorktree { key });
        }
        if command == Command::GitStashes {
            return Some(SidebarAction::ToggleStashes { key });
        }
        if command == Command::GitOutput && source.git_output().is_some() {
            return Some(SidebarAction::ToggleGitOutput { key });
        }
        if command == Command::GitAmend && source.can_toggle_amend(state.commit_draft.amend) {
            return Some(SidebarAction::AmendChanged {
                key,
                amend: !state.commit_draft.amend,
            });
        }
        if !source.mutations_available() || source.busy() {
            return None;
        }
        let operation = match command {
            Command::GitFetch => GitOperation::Fetch {
                remote: source
                    .upstream
                    .as_ref()
                    .map(|upstream| upstream.remote())
                    .or_else(|| source.remotes.first().map(String::as_str))?
                    .to_owned(),
                prune: true,
            },
            Command::GitSync | Command::GitSyncRebase
                if source.upstream.is_some() && source.branch.is_some() && !source.detached =>
            {
                GitOperation::Sync {
                    rebase: command == Command::GitSyncRebase,
                }
            }
            Command::GitStash | Command::GitStashUntracked
                if [ChangeArea::Staged, ChangeArea::Unstaged]
                    .into_iter()
                    .chain((command == Command::GitStashUntracked).then_some(ChangeArea::Untracked))
                    .any(|area| source.group(area).is_some_and(|group| !group.is_empty()))
                    && source
                        .group(ChangeArea::Conflict)
                        .is_none_or(ChangeGroup::is_empty) =>
            {
                GitOperation::StashPush {
                    message: None,
                    include_untracked: command == Command::GitStashUntracked,
                }
            }
            Command::GitPush if source.can_push() => {
                return Some(SidebarAction::Push {
                    key,
                    branch: source.branch.clone()?,
                    target: source.upstream.clone()?,
                });
            }
            Command::GitPublish if source.can_publish() => {
                return Some(SidebarAction::OpenPublish {
                    key,
                    branch: source.branch.clone()?,
                    remotes: source.remotes.clone(),
                });
            }
            Command::GitCommit if commit_available(source, state) => {
                return Some(SidebarAction::Commit {
                    key,
                    message: state.commit_message().to_owned(),
                    amend: state.commit_draft.amend,
                });
            }
            Command::GitAbort => GitOperation::Abort {
                kind: source.operation.as_ref()?.kind,
            },
            Command::GitContinue
                if source
                    .group(ChangeArea::Conflict)
                    .is_none_or(ChangeGroup::is_empty) =>
            {
                GitOperation::Continue {
                    kind: source.operation.as_ref()?.kind,
                }
            }
            _ => return None,
        };
        Some(SidebarAction::GitOperation { key, operation })
    }

    /// Both selector and object identity must still match. Reordering the stash
    /// list never substitutes another entry for the one shown in the picker.
    pub fn stash_action(
        &self,
        model: &SidebarModel,
        state: &SidebarState,
        command: Command,
        entry: &crate::repository::StashEntry,
    ) -> Option<SidebarAction> {
        self.stash_action_unbound(model, state, command, entry)
            .map(|action| super::commands::bound(self.binding, action))
    }

    fn stash_action_unbound(
        &self,
        model: &SidebarModel,
        state: &SidebarState,
        command: Command,
        entry: &crate::repository::StashEntry,
    ) -> Option<SidebarAction> {
        let source = self.source(model, state)?;
        if !source.mutations_available()
            || source.busy()
            || !source
                .stashes
                .iter()
                .any(|current| current.reference == entry.reference && current.oid == entry.oid)
        {
            return None;
        }
        let key = self.key.clone();
        Some(match command {
            Command::GitApplyStash | Command::GitPopStash => SidebarAction::GitOperation {
                key,
                operation: GitOperation::StashApply {
                    reference: entry.reference.clone(),
                    expected_oid: entry.oid.clone(),
                    pop: command == Command::GitPopStash,
                },
            },
            Command::GitDropStash => SidebarAction::RequestStashDrop {
                key,
                entry: Some(entry.clone()),
            },
            _ => return None,
        })
    }

    pub fn stashes<'a>(
        &self,
        model: &'a SidebarModel,
        state: &SidebarState,
    ) -> &'a [crate::repository::StashEntry] {
        self.source(model, state)
            .filter(|source| !source.busy())
            .map_or(&[], |source| &source.stashes)
    }
}

pub fn is_stash_picker(command: Command) -> bool {
    matches!(
        command,
        Command::GitApplyStash | Command::GitPopStash | Command::GitDropStash
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_host_capability_refuses_git_mutations_and_retains_read_commands() {
        let mut model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_mut().unwrap();
        source
            .groups
            .retain(|group| group.area != ChangeArea::Conflict);
        source.stashes = vec![crate::repository::StashEntry {
            reference: "stash@{0}".into(),
            oid: Oid::parse(&"a".repeat(40)).unwrap(),
            message: "Retained read-only stash".into(),
        }];
        source.error = Some("Failed\n\nGit output:\nactual output".into());
        let entry = source.stashes[0].clone();
        let target = GitTarget::capture(&model, &state).unwrap();
        state.apply(&SidebarAction::CommitMessageChanged {
            key: target.key.clone(),
            message: "Retain this draft".into(),
        });
        for command in [
            Command::GitFetch,
            Command::GitPush,
            Command::GitSync,
            Command::GitStash,
            Command::GitApplyStash,
            Command::GitPopStash,
            Command::GitDropStash,
            Command::GitCommit,
            Command::GitAmend,
        ] {
            assert!(
                target.available(&model, &state, command),
                "writable fixture positive: {command:?}"
            );
        }
        model
            .source_control
            .as_mut()
            .unwrap()
            .set_mutation_unavailable_reason(Some(Arc::from("Read-only Git host")));
        for descriptor in crate::workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS
            .iter()
            .filter(|descriptor| {
                crate::workspace_widget::git_command_bit(descriptor.action).is_some()
            })
        {
            let read = matches!(
                descriptor.action,
                Command::GitRefresh | Command::GitStashes | Command::GitOutput | Command::GitAmend
            );
            assert_eq!(
                target.available(&model, &state, descriptor.action),
                read,
                "capability must match actual operation: {:?}",
                descriptor.action
            );
        }
        for command in [
            Command::GitApplyStash,
            Command::GitPopStash,
            Command::GitDropStash,
        ] {
            assert!(
                target
                    .stash_action(&model, &state, command, &entry)
                    .is_none()
            );
        }
        assert_eq!(
            target.stashes(&model, &state),
            &[entry],
            "stash inspection remains readable"
        );
    }

    #[test]
    fn commit_palette_waits_for_amend_readiness_and_builds_only_an_activatable_message() {
        let mut model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_mut().unwrap();
        source
            .groups
            .retain(|group| group.area != ChangeArea::Conflict);
        let head = Oid::parse_object(source.head_oid.as_deref().unwrap()).unwrap();
        let target = GitTarget::capture(&model, &state).unwrap();
        let key = target.key.clone();
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "Retained subject\n\nPrior body".into(),
        });
        assert!(target.available(&model, &state, Command::GitCommit));
        state.apply(&SidebarAction::AmendChanged {
            key: key.clone(),
            amend: true,
        });
        let revision = state.amend_request_revision().unwrap();
        state.pin_amend_head(&key, revision, head.clone());
        assert!(!target.available(&model, &state, Command::GitCommit));
        assert!(target.action(&model, &state, Command::GitCommit).is_none());
        assert!(
            state
                .submitted_commit_revision(&key, state.commit_message(), true)
                .is_none()
        );
        assert!(state.finish_amend_read(&key, revision));
        assert!(
            !target.available(&model, &state, Command::GitCommit),
            "a failed read does not submit the old stashed message"
        );
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "Explicit replacement after failed read".into(),
        });
        assert!(
            target.available(&model, &state, Command::GitCommit),
            "an explicit edit remains a valid pinned amend"
        );
        state.apply(&SidebarAction::AmendChanged {
            key: key.clone(),
            amend: false,
        });
        state.apply(&SidebarAction::AmendChanged {
            key: key.clone(),
            amend: true,
        });
        let revision = state.amend_request_revision().unwrap();
        state.pin_amend_head(&key, revision, head);
        let loaded = "Loaded subject\n\nLoaded body\n";
        assert!(state.load_amend_message(&key, revision, loaded));
        assert!(target.available(&model, &state, Command::GitCommit));
        let action = target
            .action(&model, &state, Command::GitCommit)
            .expect("ready commit action");
        let SidebarAction::Commit {
            key: captured,
            message,
            amend,
        } = super::commands::bound_payload(&action)
        else {
            panic!("ready commit payload");
        };
        assert_eq!(*captured, key);
        assert_eq!(message, loaded);
        assert!(*amend);
        assert!(
            state
                .submitted_commit_revision(&key, message, *amend)
                .is_some()
        );
        let current_head = model.source_control.as_mut().unwrap().head_oid.take();
        assert!(
            !target.available(&model, &state, Command::GitCommit),
            "staged rows cannot authorize amend without HEAD"
        );
        assert!(target.action(&model, &state, Command::GitCommit).is_none());
        model.source_control.as_mut().unwrap().head_oid = Some("b".repeat(40).into());
        assert!(
            !target.available(&model, &state, Command::GitCommit),
            "a known changed HEAD cannot authorize the pinned amend"
        );
        assert!(target.action(&model, &state, Command::GitCommit).is_none());
        model.source_control.as_mut().unwrap().head_oid = current_head;
        let pinned_head = state.commit_draft.amend_head.take();
        assert!(
            !target.available(&model, &state, Command::GitCommit),
            "a retained message cannot authorize an unpinned amend"
        );
        assert!(target.action(&model, &state, Command::GitCommit).is_none());
        assert!(
            target.available(&model, &state, Command::GitAmend),
            "OFF recovers a stale checked draft"
        );
        state.commit_draft.amend_head = pinned_head;
        assert!(target.available(&model, &state, Command::GitCommit));
        model
            .source_control
            .as_mut()
            .unwrap()
            .set_mutation_unavailable_reason(Some(Arc::from("Read-only host")));
        assert!(!target.available(&model, &state, Command::GitCommit));
        assert!(target.action(&model, &state, Command::GitCommit).is_none());
        assert!(
            target.available(&model, &state, Command::GitAmend),
            "draft recovery remains available"
        );
    }

    #[test]
    fn amend_palette_recovers_checked_drafts_and_refuses_amend_commit_during_operations() {
        for head_present in [false, true] {
            for kind in [
                None,
                Some(crate::repository::GitOperationKind::Merge),
                Some(crate::repository::GitOperationKind::Rebase),
                Some(crate::repository::GitOperationKind::CherryPick),
                Some(crate::repository::GitOperationKind::Revert),
            ] {
                for checked in [false, true] {
                    let mut model = super::super::tests::panel_model();
                    let mut state = super::super::tests::panel_state(&model);
                    state.apply(&SidebarAction::CommitMessageChanged {
                        key: state.active_worktree().unwrap().clone(),
                        message: "Retained draft".into(),
                    });
                    state.apply(&SidebarAction::AmendChanged {
                        key: state.active_worktree().unwrap().clone(),
                        amend: checked,
                    });
                    let source = model.source_control.as_mut().unwrap();
                    source
                        .groups
                        .retain(|group| group.area != ChangeArea::Conflict);
                    if !head_present {
                        source.head_oid = None;
                    }
                    source.operation = kind.map(|kind| crate::repository::GitOperationState {
                        kind,
                        incoming_ref: None,
                        message: None,
                    });
                    let target = GitTarget::capture(&model, &state).unwrap();
                    assert_eq!(
                        target.available(&model, &state, Command::GitAmend),
                        checked || (head_present && kind.is_none())
                    );
                    if checked && kind.is_some() {
                        assert!(!target.available(&model, &state, Command::GitCommit));
                    }
                    model
                        .source_control
                        .as_mut()
                        .unwrap()
                        .set_mutation_unavailable_reason(Some(Arc::from("Read-only Git host")));
                    assert_eq!(
                        target.available(&model, &state, Command::GitAmend),
                        checked || (head_present && kind.is_none()),
                        "draft-only toggle survives write refusal"
                    );
                    assert!(!target.available(&model, &state, Command::GitCommit));
                }
            }
        }
    }

    #[test]
    fn whole_worktree_commands_share_capabilities_and_reject_rebound_sources() {
        let mut model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        model.source_control.as_mut().unwrap().remotes = vec!["origin".into()];
        model
            .source_control
            .as_mut()
            .unwrap()
            .groups
            .retain(|group| group.area != ChangeArea::Conflict);
        let target = GitTarget::capture(&model, &state).unwrap();
        assert!(
            matches!(target.action(&model, &state, Command::GitFetch).as_ref().map(super::commands::bound_payload), Some(SidebarAction::GitOperation { operation: GitOperation::Fetch { remote, prune: true }, .. }) if remote == "origin")
        );
        assert!(
            !target.available(&model, &state, Command::GitCommit),
            "a commit needs a message"
        );
        state.apply(&SidebarAction::CommitMessageChanged {
            key: target.key.clone(),
            message: "Keep subject and body\n\nDetails".into(),
        });
        assert!(
            matches!(target.action(&model, &state, Command::GitCommit).as_ref().map(super::commands::bound_payload), Some(SidebarAction::Commit { message, .. }) if message == state.commit_message())
        );
        for descriptor in crate::workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS
            .iter()
            .filter(|descriptor| {
                crate::workspace_widget::git_command_bit(descriptor.action).is_some()
                    && !is_stash_picker(descriptor.action)
            })
        {
            assert_eq!(
                target.available(&model, &state, descriptor.action),
                target.action(&model, &state, descriptor.action).is_some()
            );
        }
        model
            .source_control
            .as_mut()
            .unwrap()
            .set_mutation(Some(MutationKind::Other));
        assert!(!target.available(&model, &state, Command::GitFetch));
        assert!(target.available(&model, &state, Command::GitRefresh));
        model.source_control.as_mut().unwrap().set_mutation(None);
        model.source_control.as_mut().unwrap().branch =
            Some(ValidatedBranchName::parse("replacement").unwrap());
        assert!(
            !target.available(&model, &state, Command::GitFetch),
            "a branch change cannot retarget an open palette"
        );
    }

    #[test]
    fn output_command_only_applies_when_the_notices_have_captured_output_to_show() {
        let mut model = super::super::tests::panel_model();
        let state = super::super::tests::panel_state(&model);
        let target = GitTarget::capture(&model, &state).unwrap();
        for (message, expected) in [
            ("Reconciliation failed", false),
            ("Fetch failed\n\nGit output:\n", false),
            ("Fetch failed\n\nGit output:\nremote rejected", true),
        ] {
            model.source_control.as_mut().unwrap().error = Some(message.into());
            assert_eq!(
                target.available(&model, &state, Command::GitOutput),
                expected
            );
            assert_eq!(
                model
                    .source_control
                    .as_ref()
                    .unwrap()
                    .git_output()
                    .is_some(),
                expected
            );
        }
    }

    #[test]
    fn stash_commands_preserve_selector_oid_and_confirmation_instead_of_defaulting() {
        let mut model = super::super::tests::panel_model();
        let state = super::super::tests::panel_state(&model);
        let entry = crate::repository::StashEntry {
            reference: "stash@{2}".into(),
            oid: Oid::parse(&"a".repeat(40)).unwrap(),
            message: "chosen stash".into(),
        };
        model.source_control.as_mut().unwrap().stashes = vec![entry.clone()];
        let target = GitTarget::capture(&model, &state).unwrap();
        assert!(
            matches!(target.stash_action(&model, &state, Command::GitPopStash, &entry).as_ref().map(super::commands::bound_payload), Some(SidebarAction::GitOperation { operation: GitOperation::StashApply { reference, expected_oid, pop: true }, .. }) if *reference == entry.reference && *expected_oid == entry.oid)
        );
        assert!(
            matches!(target.stash_action(&model, &state, Command::GitDropStash, &entry).as_ref().map(super::commands::bound_payload), Some(SidebarAction::RequestStashDrop { entry: Some(shown), .. }) if *shown == entry)
        );
        model.source_control.as_mut().unwrap().stashes[0].oid =
            Oid::parse(&"b".repeat(40)).unwrap();
        assert!(
            target
                .stash_action(&model, &state, Command::GitApplyStash, &entry)
                .is_none()
        );
        model.source_control.as_mut().unwrap().stashes.clear();
        assert!(!target.available(&model, &state, Command::GitApplyStash));
    }
}
