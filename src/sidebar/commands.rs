//! Registered sidebar templates carry their original row or file intent.
use super::*;
use crate::workspace_widget::GlobalShortcut as Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTarget {
    /// Focus belonged to a sidebar control that no longer resolves.
    Unavailable,
    Rows(Vec<SelectionRow>),
    File { key: ActiveWorktreeKey, path: RepoPath },
}

/// Menu parameters remain in their ordinary self-contained sidebar action.
/// Only the template identity is shared with palette/shortcut registration.
pub fn command_id(action: &SidebarAction) -> Option<Command> {
    Some(match action {
        SidebarAction::ProjectCommand { worktree_id, command, .. } => match command {
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
        _ => return None,
    })
}

pub fn label(command: Command) -> &'static str {
    crate::workspace_widget::shortcut_descriptor(command).label
}

pub fn registered(action: SidebarAction) -> SidebarAction {
    match command_id(&action) {
        Some(command) => SidebarAction::RunCommand { command, action: Box::new(action) },
        None => action,
    }
}

pub fn project_action(project_id: ProjectId, worktree_id: Option<WorktreeId>, command: ProjectCommand) -> SidebarAction {
    SidebarAction::ProjectCommand { project_id, worktree_id, command }
}

fn row_project(model: &SidebarModel, row: SelectionRow) -> Option<(&SidebarProject, Option<&SidebarWorktree>)> {
    match row {
        SelectionRow::Project(id) => model.project(id).map(|project| (project, None)),
        SelectionRow::Worktree(project, worktree) => {
            let project = model.project(project)?;
            Some((project, Some(project.worktrees.iter().find(|row| row.id == worktree)?)))
        }
        SelectionRow::Channel(id) => model.projects.iter().find(|project| project.channels.iter().any(|row| row.id == id)).map(|project| (project, None)),
    }
}

pub fn file_rows(source: &SourceControlModel) -> impl Iterator<Item = &FileRow> {
    source.files.iter().chain(match &source.file_search {
        FileSearch::Ready { files, .. } => files.as_slice(),
        _ => &[],
    }.iter())
}

impl CommandTarget {
    pub fn from_action(action: &SidebarAction) -> Option<Self> {
        Some(match action {
            SidebarAction::ProjectCommand { project_id, worktree_id, .. } => Self::Rows(vec![worktree_id.map_or(SelectionRow::Project(*project_id), |id| SelectionRow::Worktree(*project_id, id))]),
            SidebarAction::RenameChannel(id) | SidebarAction::MarkChannelRead(id) | SidebarAction::DeleteChannel { channel_id: id } => Self::Rows(vec![SelectionRow::Channel(*id)]),
            SidebarAction::MarkWorktreeRead(project, worktree) => Self::Rows(vec![SelectionRow::Worktree(*project, *worktree)]),
            SidebarAction::DeleteRows(rows) => Self::Rows(rows.clone()),
            SidebarAction::FileCommand { key, path, .. } | SidebarAction::OpenFileInEditor { key, path, .. } => Self::File { key: key.clone(), path: path.clone() },
            _ => return None,
        })
    }

    /// Validate the template and original parameters without substituting a new target.
    pub fn accepts(&self, model: &SidebarModel, command: Command, action: &SidebarAction) -> bool {
        if command_id(action) != Some(command) { return false; }
        let Some(resolved) = self.resolve(model, command) else { return false; };
        match (action, &resolved) {
            (SidebarAction::ProjectCommand { command: ProjectCommand::OpenEditor(id), .. }, SidebarAction::ProjectCommand { .. })
            | (SidebarAction::OpenFileInEditor { editor_id: id, .. }, SidebarAction::OpenFileInEditor { .. }) => model.external_editors.iter().any(|editor| editor.id == *id),
            _ => *action == resolved,
        }
    }

    pub fn resolve(&self, model: &SidebarModel, command: Command) -> Option<SidebarAction> {
        match self {
            Self::Unavailable => None,
            Self::File { key, path } => {
                let source = model.source_control.as_ref()?;
                if source.key != *key || !model.contains_worktree(key) || source.reloading || source.busy() { return None; }
                let row = file_rows(source).find(|row| row.path == *path)?;
                let file = match command {
                    Command::CopyPath => FileCommand::CopyPath,
                    Command::CopyRelativePath => FileCommand::CopyRelativePath,
                    Command::RevealInFileManager => FileCommand::RevealInFileManager,
                    // cagents intentionally does not implement standalone ceditor mutations.
                    Command::RenameFile | Command::DuplicateFile => return None,
                    Command::OpenInExternalEditor if !row.directory => return Some(SidebarAction::OpenFileInEditor { key: key.clone(), path: path.clone(), editor_id: model.external_editors.first()?.id.clone() }),
                    _ => return None,
                };
                Some(SidebarAction::FileCommand { key: key.clone(), path: path.clone(), command: file })
            }
            Self::Rows(rows) => {
                let first = *rows.first()?;
                if rows.len() > 1 {
                    if command != Command::DeleteSelectedRows || rows.iter().any(|row| std::mem::discriminant(row) != std::mem::discriminant(&first) || row_project(model, *row).is_none_or(|(project, worktree)| project.reloading || project.unreachable.is_some() || worktree.is_some_and(|row| row.deletion.is_some()) || matches!(row, SelectionRow::Channel(id) if project.channels.iter().any(|channel| channel.id == *id && channel.deletion.is_some())))) { return None; }
                    return Some(SidebarAction::DeleteRows(rows.clone()));
                }
                let (project, worktree) = row_project(model, first)?;
                if project.reloading || project.unreachable.is_some() { return None; }
                if let SelectionRow::Channel(id) = first {
                    let channel = project.channels.iter().find(|row| row.id == id)?;
                    if channel.deletion.is_some() { return None; }
                    return Some(match command {
                        Command::RenameChannel => SidebarAction::RenameChannel(id),
                        Command::MarkChannelRead => SidebarAction::MarkChannelRead(id),
                        Command::DeleteChannel => SidebarAction::DeleteChannel { channel_id: id },
                        Command::DeleteSelectedRows => SidebarAction::DeleteRows(rows.clone()),
                        _ => return None,
                    });
                }
                if worktree.is_some_and(|row| row.deletion.is_some()) { return None; }
                let git = project.kind == ProjectKind::Git;
                let branch = worktree.and_then(|row| row.branch.as_ref());
                let project_command = match command {
                    Command::CopyPath => ProjectCommand::CopyPath,
                    Command::CopyBranch if git && branch.is_some() => ProjectCommand::CopyBranch,
                    Command::OpenOrCreatePullRequest if git && branch.is_some() && project.github_ready => ProjectCommand::PullRequest,
                    Command::NewWorktreeFromBranch if git && branch.is_some() => ProjectCommand::NewWorktreeFromBranch,
                    Command::MoveProjectUp if worktree.is_none() && model.projects.first().is_some_and(|row| row.id != project.id) => ProjectCommand::MoveUp,
                    Command::MoveProjectDown if worktree.is_none() && model.projects.last().is_some_and(|row| row.id != project.id) => ProjectCommand::MoveDown,
                    Command::ResetSidebarOrder if worktree.is_none() => ProjectCommand::ResetSidebarOrder,
                    Command::RevealInFileManager => ProjectCommand::Reveal,
                    Command::OpenRepositoryUrl if project.github_ready => ProjectCommand::RepositoryUrl,
                    Command::OpenInExternalEditor => ProjectCommand::OpenEditor(model.external_editors.first()?.id.clone()),
                    Command::DeleteWorktree if worktree.is_some() => ProjectCommand::Delete,
                    Command::RemoveProject if worktree.is_none() => ProjectCommand::Remove,
                    Command::DeleteSelectedRows => return Some(SidebarAction::DeleteRows(rows.clone())),
                    Command::MarkWorktreeRead if worktree.is_some() => return Some(SidebarAction::MarkWorktreeRead(project.id, worktree?.id)),
                    Command::NewTerminal => ProjectCommand::NewTerminal,
                    Command::NewChannel if worktree.is_none() => ProjectCommand::NewChannel,
                    Command::NewThread if worktree.is_some() => ProjectCommand::NewChannel,
                    Command::NewWorktree if git => ProjectCommand::NewWorktree,
                    _ => return None,
                };
                Some(project_action(project.id, worktree.map(|row| row.id), project_command))
            }
        }
    }

    pub fn capabilities(&self, model: &SidebarModel) -> u64 {
        crate::workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS.iter().filter_map(|descriptor| {
            descriptor.action.sidebar_command_bit().filter(|_| self.resolve(model, descriptor.action).is_some())
        }).fold(0, |mask, bit| mask | bit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        model.external_editors = ["first", "chosen"].into_iter().map(|id| SidebarEditor {
            id: id.into(), name: id.into(), icon: "editor".into(),
        }).collect();
        let action = project_action(model.projects[0].id, Some(model.projects[0].worktrees[0].id), ProjectCommand::OpenEditor("chosen".into()));
        let target = CommandTarget::from_action(&action).unwrap();
        assert!(target.accepts(&model, Command::OpenInExternalEditor, &action));
        model.external_editors.pop();
        assert!(!target.accepts(&model, Command::OpenInExternalEditor, &action));
        assert!(!target.accepts(&model, Command::CopyPath, &action));
    }

    #[test]
    fn original_file_binding_and_supported_operations_are_required() {
        let mut model = super::super::tests::panel_model();
        let source = model.source_control.as_ref().unwrap();
        let row = file_rows(source).next().unwrap();
        let target = CommandTarget::File { key: source.key.clone(), path: row.path.clone() };
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
        for item in [ProjectCommand::CopyPath, ProjectCommand::CopyBranch, ProjectCommand::PullRequest,
            ProjectCommand::NewWorktreeFromBranch, ProjectCommand::MarkRead, ProjectCommand::MoveUp,
            ProjectCommand::MoveDown, ProjectCommand::ResetSidebarOrder, ProjectCommand::Reveal,
            ProjectCommand::NewTerminal, ProjectCommand::NewChannel, ProjectCommand::NewWorktree,
            ProjectCommand::RepositoryUrl, ProjectCommand::OpenEditor("exact".into()),
            ProjectCommand::Delete, ProjectCommand::Remove] {
            let action = project_action(project, Some(worktree), item);
            let command = command_id(&action).unwrap();
            assert_eq!(crate::workspace_widget::shortcut_descriptor(command).id(), command.id());
            assert!(matches!(registered(action), SidebarAction::RunCommand { .. }));
        }
        let key = ActiveWorktreeKey { project_id: project, worktree_id: worktree, canonical_path: "/repo".into() };
        for command in [FileCommand::Rename, FileCommand::CopyFile, FileCommand::CopyPath, FileCommand::CopyRelativePath, FileCommand::RevealInFileManager] {
            assert!(command_id(&SidebarAction::FileCommand { key: key.clone(), path: RepoPath::parse("file.rs").unwrap(), command }).is_some());
        }
    }
}
