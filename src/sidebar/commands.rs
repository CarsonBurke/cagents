//! Registered sidebar templates carry their original row or file intent.
use super::*;
use crate::workspace_widget::GlobalShortcut as Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTarget {
    History {
        key: ActiveWorktreeKey,
        binding: crate::model::OperationId,
        oid: Oid,
        mainline: Option<u32>,
        message: Arc<str>,
    },
    /// Focus belonged to a sidebar control that no longer resolves.
    Unavailable,
    Rows(Vec<SelectionRow>),
    File {
        key: ActiveWorktreeKey,
        binding: crate::model::OperationId,
        path: RepoPath,
    },
    Changes {
        key: ActiveWorktreeKey,
        binding: crate::model::OperationId,
        row: ChangeSelectionRow,
        selection_revision: u64,
    },
}

/// Whole-area intent is independent of the focused row, and refuses a later
/// explicit selection before the existing fresh-status backend is entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTarget {
    pub key: ActiveWorktreeKey,
    pub binding: crate::model::OperationId,
    pub selection_revision: u64,
}
impl GroupTarget {
    pub fn capture(model: &SidebarModel, state: &SidebarState) -> Option<Self> {
        let source = model.source_control.as_ref()?;
        (state.active_worktree() == Some(&source.key) && model.contains_worktree(&source.key))
            .then(|| Self { key: source.key.clone(), binding: source.command_binding_id(), selection_revision: state.change_selection_revision() })
    }
    pub fn available(&self, model: &SidebarModel, state: &SidebarState, command: Command, modal_open: bool) -> bool {
        let Some((area, command)) = crate::workspace_widget::git_group_command_target(command) else { return false; };
        state.active_worktree() == Some(&self.key)
            && model.source_control.as_ref().is_some_and(|source| source.key == self.key && source.matches_command_binding(self.binding))
            && state.change_selection_revision() == self.selection_revision
            && whole_change_group_available(model, state, area, command, modal_open)
    }
    pub fn action(&self, model: &SidebarModel, state: &SidebarState, command: Command, modal_open: bool) -> Option<SidebarAction> {
        let (area, command) = crate::workspace_widget::git_group_command_target(command)?;
        if !model.source_control.as_ref()?.matches_command_binding(self.binding) { return None; }
        resolve_captured_change_group(model, state, &self.key, area, command, self.selection_revision, modal_open)?;
        Some(bound(self.binding, SidebarAction::RunChangeGroup { key: self.key.clone(), area, command, selection_revision: self.selection_revision }))
    }
}

pub const fn whole_group_command_id(area: ChangeArea, command: ChangeGroupCommand) -> Option<Command> {
    Some(match (area, command) {
        (ChangeArea::Unstaged, ChangeGroupCommand::Stage) => Command::GitStageAllUnstaged,
        (ChangeArea::Untracked, ChangeGroupCommand::Stage) => Command::GitStageAllUntracked,
        (ChangeArea::Conflict, ChangeGroupCommand::Stage) => Command::GitStageAllConflicts,
        (ChangeArea::Staged, ChangeGroupCommand::Unstage) => Command::GitUnstageAllStaged,
        (ChangeArea::Unstaged, ChangeGroupCommand::Discard) => Command::GitDiscardAllUnstaged,
        (ChangeArea::Untracked, ChangeGroupCommand::Discard) => Command::GitDiscardAllUntracked,
        _ => return None,
    })
}

pub const fn history_command_id(command: HistoryCommand) -> Command {
    match command {
        HistoryCommand::CopySha => Command::GitHistoryCopySha,
        HistoryCommand::CopyMessage => Command::GitHistoryCopyMessage,
        HistoryCommand::NewWorktree => Command::GitHistoryNewWorktree,
        HistoryCommand::CreateBranch => Command::GitHistoryCreateBranch,
        HistoryCommand::Revert => Command::GitHistoryRevert,
        HistoryCommand::CherryPick => Command::GitHistoryCherryPick,
    }
}

pub const fn history_command(command: Command) -> Option<HistoryCommand> {
    Some(match command {
        Command::GitHistoryCopySha => HistoryCommand::CopySha,
        Command::GitHistoryCopyMessage => HistoryCommand::CopyMessage,
        Command::GitHistoryNewWorktree => HistoryCommand::NewWorktree,
        Command::GitHistoryCreateBranch => HistoryCommand::CreateBranch,
        Command::GitHistoryRevert => HistoryCommand::Revert,
        Command::GitHistoryCherryPick => HistoryCommand::CherryPick,
        _ => return None,
    })
}

/// Menu parameters remain in their ordinary self-contained sidebar action.
/// Only the template identity is shared with palette/shortcut registration.
pub fn command_id(action: &SidebarAction) -> Option<Command> {
    if let SidebarAction::RunBound { action, .. } = action { return command_id(action); }
    Some(match action {
        SidebarAction::HistoryCommand { command, .. } => history_command_id(*command),
        SidebarAction::ProjectCommand {
            worktree_id,
            command,
            ..
        } => match command {
            ProjectCommand::CopyPath => Command::CopyPath,
            ProjectCommand::CopyBranch => Command::CopyBranch,
            ProjectCommand::PullRequest => Command::OpenOrCreatePullRequest,
            ProjectCommand::NewWorktreeFromBranch => Command::NewWorktreeFromBranch,
            ProjectCommand::MarkRead => Command::MarkWorktreeRead,
            ProjectCommand::MoveUp => Command::MoveProjectUp,
            ProjectCommand::MoveDown => Command::MoveProjectDown,
            ProjectCommand::ResetSidebarOrder => Command::ResetSidebarOrder,
            ProjectCommand::Reveal => Command::RevealInFileManager,
            ProjectCommand::NewTerminal => Command::NewTerminal,
            ProjectCommand::NewChannel if worktree_id.is_some() => Command::NewThread,
            ProjectCommand::NewChannel => Command::NewChannel,
            ProjectCommand::NewWorktree => Command::NewWorktree,
            ProjectCommand::RepositoryUrl => Command::OpenRepositoryUrl,
            ProjectCommand::OpenEditor(_) => Command::OpenInExternalEditor,
            ProjectCommand::Delete => Command::DeleteWorktree,
            ProjectCommand::Remove => Command::RemoveProject,
        },
        SidebarAction::RenameChannel(_) => Command::RenameChannel,
        SidebarAction::MarkChannelRead(_) => Command::MarkChannelRead,
        SidebarAction::MarkWorktreeRead(..) => Command::MarkWorktreeRead,
        SidebarAction::DeleteChannel { .. } => Command::DeleteChannel,
        SidebarAction::DeleteRows(_) => Command::DeleteSelectedRows,
        SidebarAction::FileCommand { command, .. } => match command {
            FileCommand::Rename => Command::RenameFile,
            FileCommand::CopyFile => Command::DuplicateFile,
            FileCommand::CopyPath => Command::CopyPath,
            FileCommand::CopyRelativePath => Command::CopyRelativePath,
            FileCommand::RevealInFileManager => Command::RevealInFileManager,
        },
        SidebarAction::OpenFileInEditor { .. } => Command::OpenInExternalEditor,
        SidebarAction::OpenFile { .. } => Command::OpenFile,
        SidebarAction::RevealChangeInFiles { .. } => Command::RevealInFiles,
        SidebarAction::IgnoreChangePath { .. } => Command::IgnoreChangePath,
        _ => return None,
    })
}

pub const fn change_command_id(command: ChangeGroupCommand) -> Command {
    match command {
        ChangeGroupCommand::Stage => Command::StageSelectedChanges,
        ChangeGroupCommand::Unstage => Command::UnstageSelectedChanges,
        ChangeGroupCommand::Discard => Command::DiscardSelectedChanges,
    }
}

pub fn label(command: Command) -> &'static str {
    crate::workspace_widget::shortcut_descriptor(command).label
}

/// Preserve a captured owner through asynchronous message dispatch.
pub fn bound(binding: crate::model::OperationId, action: SidebarAction) -> SidebarAction {
    match action {
        // An existing capture is never rebound by a later rendering map.
        action @ SidebarAction::RunBound { .. } => action,
        SidebarAction::RunCommand { command, action } => SidebarAction::RunCommand {
            command, action: Box::new(bound(binding, *action)),
        },
        action => SidebarAction::RunBound { binding, action: Box::new(action) },
    }
}

/// Read the payload only after independently validating its captured binding.
pub fn bound_payload(action: &SidebarAction) -> &SidebarAction {
    match action { SidebarAction::RunBound { action, .. } => action, _ => action }
}

pub fn registered(action: SidebarAction) -> SidebarAction {
    match command_id(&action) {
        Some(command) => SidebarAction::RunCommand {
            command,
            action: Box::new(action),
        },
        None => action,
    }
}

pub fn project_action(
    project_id: ProjectId,
    worktree_id: Option<WorktreeId>,
    command: ProjectCommand,
) -> SidebarAction {
    SidebarAction::ProjectCommand {
        project_id,
        worktree_id,
        command,
    }
}

fn row_project(
    model: &SidebarModel,
    row: SelectionRow,
) -> Option<(&SidebarProject, Option<&SidebarWorktree>)> {
    match row {
        SelectionRow::Project(id) => model.project(id).map(|project| (project, None)),
        SelectionRow::Worktree(project, worktree) => {
            let project = model.project(project)?;
            Some((
                project,
                Some(project.worktrees.iter().find(|row| row.id == worktree)?),
            ))
        }
        SelectionRow::Channel(id) => model
            .projects
            .iter()
            .find(|project| project.channels.iter().any(|row| row.id == id))
            .map(|project| (project, None)),
    }
}

pub fn file_rows(source: &SourceControlModel) -> impl Iterator<Item = &FileRow> {
    source.files.iter().chain(
        match &source.file_search {
            FileSearch::Ready { files, .. } => files.as_slice(),
            _ => &[],
        }
        .iter(),
    )
}

impl CommandTarget {
    pub fn from_action(action: &SidebarAction) -> Option<Self> {
        if matches!(action, SidebarAction::RunBound { .. }) { return Self::from_bound_action(action); }
        Self::from_action_with_binding(action, None)
    }

    pub fn from_bound_action(action: &SidebarAction) -> Option<Self> {
        let SidebarAction::RunBound { binding, action } = action else { return None; };
        Self::from_action_with_binding(action, Some(*binding))
    }

    fn from_action_with_binding(action: &SidebarAction, binding: Option<crate::model::OperationId>) -> Option<Self> {
        Some(match action {
            SidebarAction::HistoryCommand { key, oid, mainline, message, .. } => Self::History {
                key: key.clone(), binding: binding?, oid: oid.clone(), mainline: *mainline, message: Arc::clone(message),
            },
            SidebarAction::ProjectCommand {
                project_id,
                worktree_id,
                ..
            } => Self::Rows(vec![
                worktree_id.map_or(SelectionRow::Project(*project_id), |id| {
                    SelectionRow::Worktree(*project_id, id)
                }),
            ]),
            SidebarAction::RenameChannel(id)
            | SidebarAction::MarkChannelRead(id)
            | SidebarAction::DeleteChannel { channel_id: id } => {
                Self::Rows(vec![SelectionRow::Channel(*id)])
            }
            SidebarAction::MarkWorktreeRead(project, worktree) => {
                Self::Rows(vec![SelectionRow::Worktree(*project, *worktree)])
            }
            SidebarAction::DeleteRows(rows) => Self::Rows(rows.clone()),
            SidebarAction::FileCommand { key, path, .. }
            | SidebarAction::OpenFileInEditor { key, path, .. }
            | SidebarAction::OpenFile { key, path } => Self::File {
                key: key.clone(),
                binding: binding?,
                path: path.clone(),
            },
            _ => return None,
        })
    }

    /// Validate the template and original parameters without substituting a new target.
    pub fn accepts(&self, model: &SidebarModel, command: Command, action: &SidebarAction) -> bool {
        if command_id(action) != Some(command)
            || self.binding().is_some_and(|binding| !matches!(action,
                SidebarAction::RunBound { binding: captured, .. } if *captured == binding))
        {
            return false;
        }
        let Some(resolved) = self.resolve(model, command) else {
            return false;
        };
        match (bound_payload(action), bound_payload(&resolved)) {
            (
                SidebarAction::ProjectCommand {
                    command: ProjectCommand::OpenEditor(id),
                    ..
                },
                SidebarAction::ProjectCommand { .. },
            )
            | (
                SidebarAction::OpenFileInEditor { editor_id: id, .. },
                SidebarAction::OpenFileInEditor { .. },
            ) => Self::from_action(action).as_ref() == Some(self)
                && model.external_editors.iter().any(|editor| editor.id == *id),
            _ => *action == resolved,
        }
    }

    pub fn binding(&self) -> Option<crate::model::OperationId> {
        match self { Self::History { binding, .. } | Self::File { binding, .. } | Self::Changes { binding, .. } => Some(*binding), _ => None }
    }

    fn bind_action(&self, action: SidebarAction) -> SidebarAction {
        match self.binding() { Some(binding) => bound(binding, action), None => action }
    }

    pub fn resolve(&self, model: &SidebarModel, command: Command) -> Option<SidebarAction> {
        self.resolve_unbound(model, command).map(|action| self.bind_action(action))
    }

    fn resolve_unbound(&self, model: &SidebarModel, command: Command) -> Option<SidebarAction> {
        if let Some(binding) = self.binding() { if !model.source_control.as_ref()?.matches_command_binding(binding) { return None; } }
        match self {
            Self::History { key, oid, mainline, message, .. } => {
                let source = model.source_control.as_ref()?;
                let command = history_command(command)?;
                (source.key == *key && model.contains_worktree(key)
                    && source.history_command_available(oid, *mainline, message, command))
                    .then(|| SidebarAction::HistoryCommand { key: key.clone(), oid: oid.clone(), command, mainline: *mainline, message: Arc::clone(message) })
            }
            Self::Unavailable | Self::Changes { .. } => None,
            Self::File { key, path, .. } => {
                let source = model.source_control.as_ref()?;
                if source.key != *key
                    || !model.contains_worktree(key)
                    || source.reloading
                    || source.busy()
                {
                    return None;
                }
                let row = file_rows(source).find(|row| row.path == *path)?;
                let file = match command {
                    Command::OpenFile
                        if !row.directory && row.status != Some(FileStatus::Deleted) =>
                    {
                        return Some(SidebarAction::OpenFile {
                            key: key.clone(),
                            path: path.clone(),
                        });
                    }
                    Command::CopyPath => FileCommand::CopyPath,
                    Command::CopyRelativePath => FileCommand::CopyRelativePath,
                    Command::RevealInFileManager => FileCommand::RevealInFileManager,
                    // cagents intentionally does not implement standalone ceditor mutations.
                    Command::RenameFile | Command::DuplicateFile => return None,
                    Command::OpenInExternalEditor if !row.directory => {
                        return Some(SidebarAction::OpenFileInEditor {
                            key: key.clone(),
                            path: path.clone(),
                            editor_id: model.external_editors.first()?.id.clone(),
                        });
                    }
                    _ => return None,
                };
                Some(SidebarAction::FileCommand {
                    key: key.clone(),
                    path: path.clone(),
                    command: file,
                })
            }
            Self::Rows(rows) => {
                let first = *rows.first()?;
                if rows.len() > 1 {
                    if command != Command::DeleteSelectedRows || rows.iter().any(|row| std::mem::discriminant(row) != std::mem::discriminant(&first) || row_project(model, *row).is_none_or(|(project, worktree)| project.reloading || project.unreachable.is_some() || worktree.is_some_and(|row| row.deletion.is_some()) || matches!(row, SelectionRow::Channel(id) if project.channels.iter().any(|channel| channel.id == *id && channel.deletion.is_some())))) { return None; }
                    return Some(SidebarAction::DeleteRows(rows.clone()));
                }
                let (project, worktree) = row_project(model, first)?;
                if project.reloading || project.unreachable.is_some() {
                    return None;
                }
                if let SelectionRow::Channel(id) = first {
                    let channel = project.channels.iter().find(|row| row.id == id)?;
                    if channel.deletion.is_some() {
                        return None;
                    }
                    return Some(match command {
                        Command::RenameChannel => SidebarAction::RenameChannel(id),
                        Command::MarkChannelRead => SidebarAction::MarkChannelRead(id),
                        Command::DeleteChannel => SidebarAction::DeleteChannel { channel_id: id },
                        Command::DeleteSelectedRows => SidebarAction::DeleteRows(rows.clone()),
                        _ => return None,
                    });
                }
                if worktree.is_some_and(|row| row.deletion.is_some()) {
                    return None;
                }
                let git = project.kind == ProjectKind::Git;
                let branch = worktree.and_then(|row| row.branch.as_ref());
                let project_command = match command {
                    Command::CopyPath => ProjectCommand::CopyPath,
                    Command::CopyBranch if git && branch.is_some() => ProjectCommand::CopyBranch,
                    Command::OpenOrCreatePullRequest
                        if git && branch.is_some() && project.github_ready =>
                    {
                        ProjectCommand::PullRequest
                    }
                    Command::NewWorktreeFromBranch if git && branch.is_some() => {
                        ProjectCommand::NewWorktreeFromBranch
                    }
                    Command::MoveProjectUp
                        if worktree.is_none()
                            && model
                                .projects
                                .first()
                                .is_some_and(|row| row.id != project.id) =>
                    {
                        ProjectCommand::MoveUp
                    }
                    Command::MoveProjectDown
                        if worktree.is_none()
                            && model
                                .projects
                                .last()
                                .is_some_and(|row| row.id != project.id) =>
                    {
                        ProjectCommand::MoveDown
                    }
                    Command::ResetSidebarOrder if worktree.is_none() => {
                        ProjectCommand::ResetSidebarOrder
                    }
                    Command::RevealInFileManager => ProjectCommand::Reveal,
                    Command::OpenRepositoryUrl if project.github_ready => {
                        ProjectCommand::RepositoryUrl
                    }
                    Command::OpenInExternalEditor => {
                        ProjectCommand::OpenEditor(model.external_editors.first()?.id.clone())
                    }
                    Command::DeleteWorktree if worktree.is_some() => ProjectCommand::Delete,
                    Command::RemoveProject if worktree.is_none() => ProjectCommand::Remove,
                    Command::DeleteSelectedRows => {
                        return Some(SidebarAction::DeleteRows(rows.clone()));
                    }
                    Command::MarkWorktreeRead if worktree.is_some() => {
                        return Some(SidebarAction::MarkWorktreeRead(project.id, worktree?.id));
                    }
                    Command::NewTerminal => ProjectCommand::NewTerminal,
                    Command::NewChannel if worktree.is_none() => ProjectCommand::NewChannel,
                    Command::NewThread if worktree.is_some() => ProjectCommand::NewChannel,
                    Command::NewWorktree if git => ProjectCommand::NewWorktree,
                    _ => return None,
                };
                Some(project_action(
                    project.id,
                    worktree.map(|row| row.id),
                    project_command,
                ))
            }
        }
    }

    /// Changes retain a selection generation as well as the worktree and
    /// focused path. Rebinding, clearing or replacing selection cannot retarget
    /// a palette entry that was captured before its search field took focus.
    pub fn resolve_in(&self, model: &SidebarModel, state: &SidebarState, command: Command) -> Option<SidebarAction> {
        if let Some(binding) = self.binding() { if !model.source_control.as_ref()?.matches_command_binding(binding) { return None; } }
        self.resolve_in_unbound(model, state, command).map(|action| self.bind_action(action))
    }

    fn resolve_in_unbound(
        &self,
        model: &SidebarModel,
        state: &SidebarState,
        command: Command,
    ) -> Option<SidebarAction> {
        if let Self::History { key, .. } = self {
            return (state.active_worktree() == Some(key) && state.changes_tab() == ChangesTab::History)
                .then(|| self.resolve_unbound(model, command)).flatten();
        }
        let Self::Changes {
            key,
            row,
            selection_revision,
            ..
        } = self
        else {
            return self.resolve_unbound(model, command);
        };
        let source = model.source_control.as_ref()?;
        if &source.key != key
            || !model.contains_worktree(key)
            || source.reloading
            || state.active_worktree() != Some(key)
            || !matches!(source.data_state, SidebarDataState::Ready)
            || state.change_selection_revision() != *selection_revision
        {
            return None;
        }
        let current = source.group(row.area)?.visible_path(&row.path)?;
        if !source.mutations_available() && matches!(command,
            Command::StageSelectedChanges | Command::UnstageSelectedChanges
            | Command::DiscardSelectedChanges | Command::IgnoreChangePath
            | Command::GitAcceptOurs | Command::GitAcceptTheirs)
        { return None; }
        match command {
            Command::CopyRelativePath => {
                return Some(SidebarAction::FileCommand {
                    key: key.clone(),
                    path: row.path.clone(),
                    command: FileCommand::CopyRelativePath,
                });
            }
            Command::OpenFile if current.entry.kind != ChangeKind::Deleted => {
                return Some(SidebarAction::OpenFile {
                    key: key.clone(),
                    path: row.path.clone(),
                });
            }
            Command::RevealInFiles if current.entry.kind != ChangeKind::Deleted => {
                return Some(SidebarAction::RevealChangeInFiles {
                    key: key.clone(),
                    path: row.path.clone(),
                });
            }
            Command::IgnoreChangePath if row.area == ChangeArea::Untracked && !source.busy() => {
                return Some(SidebarAction::IgnoreChangePath {
                    key: key.clone(),
                    path: row.path.clone(),
                });
            }
            Command::GitAcceptOurs | Command::GitAcceptTheirs
                if row.area == ChangeArea::Conflict
                    && !source.busy()
                    && current.entry.conflict.is_some() =>
            {
                return Some(SidebarAction::GitOperation {
                    key: key.clone(),
                    operation: GitOperation::ResolveConflict {
                        path: row.path.clone(),
                        ours: command == Command::GitAcceptOurs,
                    },
                });
            }
            _ => {}
        }
        // Header Stage/Unstage already queue work behind the active mutation.
        // Preserve that behavior for the same registered selection commands.
        if source.busy() && !matches!(command, Command::StageSelectedChanges | Command::UnstageSelectedChanges) {
            return None;
        }
        let operation = match command {
            Command::StageSelectedChanges => ChangeGroupCommand::Stage,
            Command::UnstageSelectedChanges => ChangeGroupCommand::Unstage,
            Command::DiscardSelectedChanges => ChangeGroupCommand::Discard,
            _ => return None,
        };
        change_command_available_for(source, state, row, operation).then(|| {
            SidebarAction::InvokeChangeRows {
                key: key.clone(),
                row: row.clone(),
                command: operation,
            }
        })
    }

    pub fn capabilities(&self, model: &SidebarModel) -> u64 {
        crate::workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS
            .iter()
            .filter_map(|descriptor| {
                descriptor
                    .action
                    .sidebar_command_bit()
                    .filter(|_| self.resolve(model, descriptor.action).is_some())
            })
            .fold(0, |mask, bit| mask | bit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_templates_keep_the_shown_commit_and_current_worktree_policy() {
        let mut model = super::super::tests::panel_model();
        let identity = CommitIdentity { name: "Author".into(), email: "author@example.invalid".into(), timestamp: 0, offset_minutes: 0 };
        let row = CommitRow::new(&CommitSummary { oid: Oid::parse("1234567890123456789012345678901234567890").unwrap(), parents: vec![], author: identity.clone(), committer: identity, subject: "Shown subject".into(), body: "Shown body".into() });
        let source = model.source_control.as_mut().unwrap();
        source.history = vec![row];
        let row = &source.history[0];
        let target = CommandTarget::History { key: source.key.clone(), binding: source.command_binding_id(), oid: row.oid.clone(), mainline: row.mainline, message: Arc::clone(&row.message) };
        let mut state = super::super::tests::panel_state(&model);
        state.changes_tab = ChangesTab::History;
        let commands = [HistoryCommand::CopySha, HistoryCommand::CopyMessage, HistoryCommand::NewWorktree, HistoryCommand::CreateBranch, HistoryCommand::Revert, HistoryCommand::CherryPick];
        for command in commands {
            let action = target.resolve_in(&model, &state, history_command_id(command)).unwrap();
            assert_eq!(command_id(&action), Some(history_command_id(command)));
            assert_eq!(CommandTarget::from_action(&action), Some(target.clone()));
        }
        model.source_control.as_mut().unwrap().set_mutation_unavailable_reason(Some(Arc::from("Read-only host")));
        for command in commands {
            assert_eq!(target.resolve_in(&model, &state, history_command_id(command)).is_some(), matches!(command, HistoryCommand::CopySha | HistoryCommand::CopyMessage));
        }
        let source = model.source_control.as_mut().unwrap();
        source.set_mutation_unavailable_reason(None);
        source.mutation = Some(MutationKind::Stage);
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopyMessage).is_some());
        assert!(target.resolve_in(&model, &state, Command::GitHistoryRevert).is_none());
        model.source_control.as_mut().unwrap().mutation = None;
        state.changes_tab = ChangesTab::Changes;
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopySha).is_none());
        state.changes_tab = ChangesTab::History;
        model.source_control.as_mut().unwrap().history[0].message = Arc::from("Replacement message at the same OID");
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopyMessage).is_none());
        if let CommandTarget::History { message, .. } = &target {
            model.source_control.as_mut().unwrap().history[0].message = Arc::clone(message);
        }
        model.source_control.as_mut().unwrap().history[0].mainline = Some(1);
        assert!(target.resolve_in(&model, &state, Command::GitHistoryRevert).is_none());
        model.source_control.as_mut().unwrap().history[0].mainline = None;
        assert!(target.resolve_in(&model, &state, Command::GitHistoryRevert).is_some());
        model.source_control.as_mut().unwrap().reloading = true;
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopySha).is_none());
        model.source_control.as_mut().unwrap().reloading = false;
        let old_binding = model.source_control.as_ref().unwrap().command_binding_id();
        model.source_control.as_mut().unwrap().set_command_binding_id(crate::model::OperationId::new());
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopySha).is_none(),
            "same commit and path from a replacement actor do not authorize an old history menu");
        model.source_control.as_mut().unwrap().set_command_binding_id(old_binding);
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopySha).is_some());
        model.source_control.as_mut().unwrap().key.worktree_id = WorktreeId::new();
        assert!(target.resolve_in(&model, &state, Command::GitHistoryCopySha).is_none());
    }

    #[test]
    fn captured_worktree_never_resolves_to_a_replacement_row() {
        let mut model = super::super::tests::panel_model();
        let project = model.projects[0].id;
        let original = model.projects[0].worktrees[0].id;
        let target = CommandTarget::Rows(vec![SelectionRow::Worktree(project, original)]);
        assert!(target.resolve(&model, Command::NewTerminal).is_some());
        model.projects[0].worktrees.remove(0);
        assert!(target.resolve(&model, Command::NewTerminal).is_none());
        assert_eq!(target.capabilities(&model), 0);
        assert_eq!(CommandTarget::Unavailable.capabilities(&model), 0);
    }

    #[test]
    fn menu_keeps_its_exact_external_editor_and_rejects_removed_editors() {
        let mut model = super::super::tests::panel_model();
        model.external_editors = ["first", "chosen"]
            .into_iter()
            .map(|id| SidebarEditor {
                id: id.into(),
                name: id.into(),
                icon: "editor".into(),
            })
            .collect();
        let action = project_action(
            model.projects[0].id,
            Some(model.projects[0].worktrees[0].id),
            ProjectCommand::OpenEditor("chosen".into()),
        );
        let target = CommandTarget::from_action(&action).unwrap();
        assert!(target.accepts(&model, Command::OpenInExternalEditor, &action));
        model.external_editors = model.external_editors.iter().take(1).cloned().collect();
        assert!(!target.accepts(&model, Command::OpenInExternalEditor, &action));
        assert!(!target.accepts(&model, Command::CopyPath, &action));
    }

    #[test]
    fn original_file_binding_and_supported_operations_are_required() {
        let mut model = super::super::tests::panel_model();
        let source = model.source_control.as_ref().unwrap();
        let row = file_rows(source).next().unwrap();
        let target = CommandTarget::File {
            key: source.key.clone(), binding: source.command_binding_id(),
            path: row.path.clone(),
        };
        assert!(target.resolve(&model, Command::CopyRelativePath).is_some());
        for command in [Command::RenameFile, Command::DuplicateFile] {
            assert!(target.resolve(&model, command).is_none());
        }
        model.source_control.as_mut().unwrap().key = ActiveWorktreeKey {
            project_id: model.projects[0].id,
            worktree_id: model.projects[0].worktrees[1].id,
            canonical_path: model.projects[0].worktrees[1].path.clone(),
        };
        assert_eq!(target.capabilities(&model), 0);
    }

    #[test]
    fn every_project_and_file_menu_variant_has_a_registered_template() {
        let project = ProjectId::new();
        let worktree = WorktreeId::new();
        for item in [
            ProjectCommand::CopyPath,
            ProjectCommand::CopyBranch,
            ProjectCommand::PullRequest,
            ProjectCommand::NewWorktreeFromBranch,
            ProjectCommand::MarkRead,
            ProjectCommand::MoveUp,
            ProjectCommand::MoveDown,
            ProjectCommand::ResetSidebarOrder,
            ProjectCommand::Reveal,
            ProjectCommand::NewTerminal,
            ProjectCommand::NewChannel,
            ProjectCommand::NewWorktree,
            ProjectCommand::RepositoryUrl,
            ProjectCommand::OpenEditor("exact".into()),
            ProjectCommand::Delete,
            ProjectCommand::Remove,
        ] {
            let action = project_action(project, Some(worktree), item);
            let command = command_id(&action).unwrap();
            assert_eq!(
                crate::workspace_widget::shortcut_descriptor(command).id(),
                command.id()
            );
            assert!(matches!(
                registered(action),
                SidebarAction::RunCommand { .. }
            ));
        }
        let key = ActiveWorktreeKey {
            project_id: project,
            worktree_id: worktree,
            canonical_path: "/repo".into(),
        };
        for command in [
            FileCommand::Rename,
            FileCommand::CopyFile,
            FileCommand::CopyPath,
            FileCommand::CopyRelativePath,
            FileCommand::RevealInFileManager,
        ] {
            assert!(
                command_id(&SidebarAction::FileCommand {
                    key: key.clone(),
                    path: RepoPath::parse("file.rs").unwrap(),
                    command
                })
                .is_some()
            );
        }
    }
}

#[cfg(test)]
mod change_target_tests {
    use super::*;
    #[test]
    fn captured_change_selection_rejects_replacement_and_rebound_worktree() {
        let mut model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_ref().unwrap();
        let rows = change_selection_order(source, &state);
        assert!(rows.len() > 1);
        let row = rows[0].clone();
        let command = if row.area == ChangeArea::Staged {
            Command::UnstageSelectedChanges
        } else {
            Command::StageSelectedChanges
        };
        state.select_change_rows(row.clone(), SelectionIntent::Toggle, &rows);
        let target = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(),
            row,
            selection_revision: state.change_selection_revision(),
        };
        assert!(matches!(
            target.resolve_in(&model, &state, command).as_ref().map(bound_payload),
            Some(SidebarAction::InvokeChangeRows { .. })
        ));
        state.select_change_rows(rows[0].clone(), SelectionIntent::Toggle, &rows);
        state.select_change_rows(rows[1].clone(), SelectionIntent::Toggle, &rows);
        assert_eq!(state.selected_change_count(), 1);
        assert!(
            target.resolve_in(&model, &state, command).is_none(),
            "equal-count replacement cannot retarget a captured command"
        );
        let current = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(),
            row: rows[1].clone(),
            selection_revision: state.change_selection_revision(),
        };
        model.source_control.as_mut().unwrap().reloading = true;
        assert!(
            current
                .resolve_in(&model, &state, Command::StageSelectedChanges)
                .is_none()
        );
        model.source_control = None;
        assert!(
            current
                .resolve_in(&model, &state, Command::StageSelectedChanges)
                .is_none()
        );
    }
}

#[cfg(test)]
mod change_navigation_target_tests {
    use super::*;
    #[test]
    fn read_only_git_invalidates_captured_mutations_without_hiding_open_or_copy() {
        let mut model = super::super::tests::panel_model();
        let state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_ref().unwrap();
        let row = source.group(ChangeArea::Untracked).unwrap().rows()[0].entry.clone();
        let target = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(), row: ChangeSelectionRow { area: row.area, path: row.path },
            selection_revision: state.change_selection_revision(),
        };
        let group = GroupTarget::capture(&model, &state).unwrap();
        for command in [Command::StageSelectedChanges, Command::DiscardSelectedChanges, Command::IgnoreChangePath] {
            assert!(target.resolve_in(&model, &state, command).is_some(), "writable positive: {command:?}");
        }
        for command in crate::workspace_widget::GIT_GROUP_COMMANDS {
            assert!(group.available(&model, &state, command, false), "writable whole area: {command:?}");
        }
        model.source_control.as_mut().unwrap().set_mutation_unavailable_reason(Some(Arc::from("Read-only Git host")));
        for command in [Command::StageSelectedChanges, Command::DiscardSelectedChanges, Command::IgnoreChangePath] {
            assert!(target.resolve_in(&model, &state, command).is_none(), "captured mutation must be revalidated: {command:?}");
        }
        for command in crate::workspace_widget::GIT_GROUP_COMMANDS {
            assert!(group.action(&model, &state, command, false).is_none(), "captured whole-area action must be revalidated: {command:?}");
        }
        for command in [Command::OpenFile, Command::RevealInFiles, Command::CopyRelativePath] {
            assert!(target.resolve_in(&model, &state, command).is_some(), "read-only operation remains available: {command:?}");
        }
    }

    #[test]
    fn every_change_navigation_template_uses_current_row_and_captured_revision() {
        let model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_ref().unwrap();
        let row = source.group(ChangeArea::Untracked).unwrap().rows()[0]
            .entry
            .clone();
        let target = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(),
            row: ChangeSelectionRow {
                area: row.area,
                path: row.path.clone(),
            },
            selection_revision: state.change_selection_revision(),
        };
        for command in [
            Command::OpenFile,
            Command::RevealInFiles,
            Command::CopyRelativePath,
            Command::IgnoreChangePath,
        ] {
            let resolved = target
                .resolve_in(&model, &state, command)
                .expect("live row operation");
            assert_eq!(command_id(&resolved), Some(command));
            assert!(matches!(bound_payload(&resolved),
                SidebarAction::OpenFile { key,path } | SidebarAction::RevealChangeInFiles { key,path }
                | SidebarAction::IgnoreChangePath { key,path } | SidebarAction::FileCommand { key,path,.. }
                if *key==source.key && *path==row.path));
        }
        let order = change_selection_order(source, &state);
        state.select_change_rows(order[0].clone(), SelectionIntent::Toggle, &order);
        for command in [
            Command::OpenFile,
            Command::RevealInFiles,
            Command::CopyRelativePath,
            Command::IgnoreChangePath,
        ] {
            assert!(
                target.resolve_in(&model, &state, command).is_none(),
                "stale intent must not follow another selection"
            );
        }
        let tracked = source
            .groups()
            .iter()
            .filter(|group| group.area != ChangeArea::Untracked)
            .flat_map(|group| group.rows())
            .next()
            .unwrap();
        let target = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(),
            row: ChangeSelectionRow {
                area: tracked.entry.area,
                path: tracked.entry.path.clone(),
            },
            selection_revision: state.change_selection_revision(),
        };
        assert!(
            target
                .resolve_in(&model, &state, Command::IgnoreChangePath)
                .is_none()
        );
    }
}

#[cfg(test)]
mod conflict_command_tests {
    use super::*;
    #[test]
    fn conflict_templates_preserve_captured_path_area_and_revision() {
        let mut model = super::super::tests::panel_model();
        let mut state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_ref().unwrap();
        let entry = source.group(ChangeArea::Conflict).unwrap().rows()[0]
            .entry
            .clone();
        let target = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(),
            row: ChangeSelectionRow {
                area: ChangeArea::Conflict,
                path: entry.path.clone(),
            },
            selection_revision: state.change_selection_revision(),
        };
        for (command, expected) in [
            (Command::GitAcceptOurs, true),
            (Command::GitAcceptTheirs, false),
        ] {
            assert!(
                matches!(target.resolve_in(&model, &state, command).as_ref().map(bound_payload), Some(SidebarAction::GitOperation { operation: GitOperation::ResolveConflict { path, ours }, .. }) if *path == entry.path && *ours == expected)
            );
        }
        model
            .source_control
            .as_mut()
            .unwrap()
            .set_mutation(Some(MutationKind::Other));
        assert!(
            target
                .resolve_in(&model, &state, Command::GitAcceptOurs)
                .is_none()
        );
        model.source_control.as_mut().unwrap().set_mutation(None);
        let order = change_selection_order(model.source_control.as_ref().unwrap(), &state);
        state.select_change_rows(order[0].clone(), SelectionIntent::Toggle, &order);
        assert!(
            target
                .resolve_in(&model, &state, Command::GitAcceptOurs)
                .is_none(),
            "a changed selection cannot substitute another conflict"
        );
    }
}


#[cfg(test)]
mod binding_tests {
    use super::*;

    #[test]
    fn same_named_replacement_retires_all_captured_source_targets() {
        let mut model = super::super::tests::panel_model();
        let state = super::super::tests::panel_state(&model);
        let source = model.source_control.as_ref().unwrap();
        let git = super::super::git_commands::GitTarget::capture(&model, &state).unwrap();
        let group = GroupTarget::capture(&model, &state).unwrap();
        let file = CommandTarget::File {
            key: source.key.clone(), binding: source.command_binding_id(),
            path: file_rows(source).next().unwrap().path.clone(),
        };
        let entry = source.groups().iter().flat_map(|group| group.rows()).next().unwrap();
        let changes = CommandTarget::Changes {
            key: source.key.clone(), binding: source.command_binding_id(),
            row: ChangeSelectionRow { area: entry.entry.area, path: entry.entry.path.clone() },
            selection_revision: state.change_selection_revision(),
        };
        let shown = file.resolve(&model, Command::CopyRelativePath).unwrap();
        assert_eq!(CommandTarget::from_action(&shown), Some(file.clone()));
        assert!(git.action(&model, &state, Command::GitRefresh).is_some());
        assert!(changes.resolve_in(&model, &state, Command::CopyRelativePath).is_some());
        // Identical keys, paths, rows, branch and selection are insufficient when
        // the retained actor was replaced (including a physical root replacement).
        let old = source.command_binding_id();
        model.source_control.as_mut().unwrap().set_command_binding_id(crate::model::OperationId::new());
        assert!(git.action(&model, &state, Command::GitRefresh).is_none());
        assert!(file.resolve(&model, Command::CopyRelativePath).is_none());
        assert!(changes.resolve_in(&model, &state, Command::CopyRelativePath).is_none());
        assert!(group.action(&model, &state, Command::GitStageAllUntracked, false).is_none());
        assert!(!file.accepts(&model, Command::CopyRelativePath, &shown));
        // Explicit actual-owner continuity, rather than a selection revision or
        // a key comparison, is the only way a refresh keeps a captured target.
        model.source_control.as_mut().unwrap().set_command_binding_id(old);
        assert_eq!(file.resolve(&model, Command::CopyRelativePath), Some(shown));
        assert!(git.action(&model, &state, Command::GitRefresh).is_some());
    }

    #[test]
    fn cached_registered_actions_keep_the_original_capture_and_refuse_raw_source_intent() {
        let model = super::super::tests::panel_model();
        let source = model.source_control.as_ref().unwrap();
        let path = file_rows(source).next().unwrap().path.clone();
        let raw = SidebarAction::FileCommand { key: source.key.clone(), path, command: FileCommand::CopyRelativePath };
        assert!(CommandTarget::from_action(&raw).is_none());
        let original = bound(source.command_binding_id(), raw);
        let registered = registered(original.clone());
        let refreshed = bound(crate::model::OperationId::new(), registered);
        let SidebarAction::RunCommand { action, .. } = refreshed else { panic!("registered template"); };
        assert_eq!(*action, original, "a fresh render must never rebind a cached menu payload");
        let target = CommandTarget::from_action(&original).unwrap();
        let forged = bound(crate::model::OperationId::new(), bound_payload(&original).clone());
        assert!(!target.accepts(&model, Command::CopyRelativePath, &forged));
    }

    #[test]
    fn configured_external_editor_cannot_bypass_binding_or_original_path() {
        let mut model = super::super::tests::panel_model();
        model.external_editors = vec![SidebarEditor { id: "editor".into(), name: "Editor".into(), icon: "editor".into() }].into();
        let source = model.source_control.as_ref().unwrap();
        let target = CommandTarget::File { key: source.key.clone(), binding: source.command_binding_id(), path: file_rows(source).next().unwrap().path.clone() };
        let action = target.resolve(&model, Command::OpenInExternalEditor).unwrap();
        assert!(target.accepts(&model, Command::OpenInExternalEditor, &action));
        let captured = bound_payload(&action).clone();
        assert!(!target.accepts(&model, Command::OpenInExternalEditor, &bound(crate::model::OperationId::new(), captured)));
        let SidebarAction::OpenFileInEditor { key, editor_id, .. } = bound_payload(&action) else { panic!("file editor"); };
        let changed = bound(source.command_binding_id(), SidebarAction::OpenFileInEditor { key: key.clone(), path: RepoPath::parse("different.rs").unwrap(), editor_id: editor_id.clone() });
        assert!(!target.accepts(&model, Command::OpenInExternalEditor, &changed));
    }
}
