//! Confirm one explicit group action, then reuse each row's existing lifecycle.
use super::*;
use crate::sidebar::{SelectionIntent, SelectionRow};

#[derive(Debug, Default)]
pub(super) struct State {
    pub(super) dialog: Option<Dialog>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChannelId, ProjectChannel};

    fn fixture() -> (Cagents, ProjectId, [ChannelId; 3]) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Selected project".into(),
            "/selection-project".into(),
            Vec::new(),
            Vec::new(),
        ));
        let channels = [ChannelId::new(), ChannelId::new(), ChannelId::new()];
        let durable = Arc::make_mut(&mut app.durable);
        for (index, id) in channels.iter().enumerate() {
            durable.channels.push(ProjectChannel {
                id: *id,
                project_id: project,
                title: format!("Channel {index}"),
                ordinal: index as u32,
                shared: false,
                cards: Vec::new(),
                focused: None,
            });
        }
        durable.active_channel = Some(channels[2]);
        app.refresh_sidebar_channels();
        (app, project, channels)
    }

    fn select(app: &mut Cagents, ids: &[ChannelId]) {
        for id in ids {
            app.select_sidebar_rows(SelectionRow::Channel(*id), SelectionIntent::Toggle);
        }
    }

    #[test]
    fn group_confirmation_names_all_targets_and_preserves_unrelated_active_channel() {
        let (mut app, _, ids) = fixture();
        select(&mut app, &ids[..2]);
        drop(app.begin_sidebar_group_delete());
        let dialog = app.sidebar_selection_ui.dialog.as_ref().unwrap();
        assert_eq!(dialog.targets.len(), 2);
        assert!(
            dialog
                .targets
                .iter()
                .any(|target| target.label == "Channel 0")
        );
        assert!(app.modal_is_open());
        assert!(app.sidebar_selection_dialog().is_some());
        let focused = app.workspace.focused();
        drop(app.update_sidebar_selection(Action::Confirm));
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|channel| ids[..2].contains(&channel.id))
        );
        assert!(!app.channel_ui.is_deleting(ids[2]));
        assert_eq!(app.durable.active_channel, Some(ids[2]));
        assert_eq!(app.workspace.focused(), focused);
        assert!(!app.modal_is_open(), "no per-member confirmation");
    }

    #[test]
    fn changed_target_aborts_the_entire_confirmed_group_before_deletion() {
        let (mut app, _, ids) = fixture();
        select(&mut app, &ids[..2]);
        drop(app.begin_sidebar_group_delete());
        Arc::make_mut(&mut app.durable)
            .channels
            .retain(|channel| channel.id != ids[1]);
        drop(app.update_sidebar_selection(Action::Confirm));
        assert!(!app.channel_ui.deletion_running());
        assert!(
            app.sidebar_selection_ui
                .dialog
                .as_ref()
                .unwrap()
                .error
                .is_some()
        );
        drop(app.update_sidebar_selection(Action::Dismiss));
        assert!(!app.modal_is_open());
    }

    #[test]
    fn mixed_selection_cannot_delete_and_hidden_rows_are_pruned() {
        let (mut app, project, ids) = fixture();
        select(&mut app, &ids[..2]);
        app.select_sidebar_rows(SelectionRow::Project(project), SelectionIntent::Toggle);
        drop(app.begin_sidebar_group_delete());
        assert!(app.sidebar_selection_ui.dialog.is_none());
        assert!(!app.channel_ui.deletion_running());
        app.sidebar_state
            .apply(&SidebarAction::ToggleProject(project));
        app.prune_sidebar_row_selection();
        assert_eq!(
            app.sidebar_state.selected_rows(&app.visible_sidebar_rows()),
            vec![SelectionRow::Project(project)]
        );
    }

    #[test]
    fn model_refresh_prunes_removed_rows_without_another_sidebar_click() {
        let (mut app, _, ids) = fixture();
        select(&mut app, &ids[..2]);
        Arc::make_mut(&mut app.durable)
            .channels
            .retain(|channel| channel.id != ids[0]);
        drop(app.update(Message::WindowResized(1440.0, 900.0)));
        assert_eq!(
            app.sidebar_state.selected_rows(&app.visible_sidebar_rows()),
            vec![SelectionRow::Channel(ids[1])]
        );
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Confirm,
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    row: SelectionRow,
    directory: PathBuf,
    label: String,
}

#[derive(Debug)]
pub(super) struct Dialog {
    targets: Vec<Target>,
    error: Option<String>,
}

pub(super) fn focus_id() -> widget::Id {
    widget::Id::new("dialog.sidebar-selection.confirm")
}

impl Cagents {
    fn visible_sidebar_rows(&self) -> Vec<SelectionRow> {
        sidebar::visible_selection_rows(
            &self.sidebar_model,
            &self.sidebar_state,
            matches!(
                SidebarMode::for_width(self.window_width),
                SidebarMode::Compact
            ),
        )
    }

    pub(super) fn select_sidebar_rows(&mut self, row: SelectionRow, intent: SelectionIntent) {
        let visible = self.visible_sidebar_rows();
        self.sidebar_state.select_rows(row, intent, &visible);
    }

    pub(super) fn prune_sidebar_row_selection(&mut self) {
        if !self.sidebar_state.has_row_selection() {
            return;
        }
        let visible = self.visible_sidebar_rows();
        self.sidebar_state.prune_row_selection(&visible);
    }

    fn selected_row_target(&self, row: SelectionRow) -> Result<Target, String> {
        match row {
            SelectionRow::Project(id) => {
                let project = self
                    .sidebar_model
                    .project(id)
                    .ok_or("The selected project is no longer open.")?;
                if let Some(reason) = self.selected_project_removal_blocker(id) {
                    return Err(format!("{}: {reason}", project.name));
                }
                Ok(Target {
                    row,
                    directory: project.repository.clone(),
                    label: project.name.clone(),
                })
            }
            SelectionRow::Worktree(project, id) => {
                let worktree = self
                    .sidebar_model
                    .project(project)
                    .and_then(|project| project.worktrees.iter().find(|worktree| worktree.id == id))
                    .ok_or("The selected worktree is no longer open.")?;
                if let Some(reason) = self.selected_worktree_deletion_blocker(project, id) {
                    return Err(format!("{}: {reason}", worktree.path.display()));
                }
                Ok(Target {
                    row,
                    directory: worktree.path.clone(),
                    label: worktree.path.display().to_string(),
                })
            }
            SelectionRow::Channel(id) => {
                let channel = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .ok_or("The selected channel is no longer open.")?;
                if self.channel_ui.is_deleting(id) {
                    return Err("Channel deletion is already in progress.".into());
                }
                if channel.cards.iter().any(|card| matches!(self.cards.get(card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())) {
                    return Err("Save or close the selected channels' unsaved editor buffers first.".into());
                }
                let project = self
                    .sidebar_model
                    .project(channel.project_id)
                    .ok_or("The selected channel's project is no longer open.")?;
                Ok(Target {
                    row,
                    directory: project.repository.clone(),
                    label: channel_ui::channel_display_title(channel).to_owned(),
                })
            }
        }
    }

    pub(super) fn begin_sidebar_group_delete(&mut self) -> Task<Message> {
        let visible = self.visible_sidebar_rows();
        self.sidebar_state.prune_row_selection(&visible);
        let selected = self.sidebar_state.selected_rows(&visible);
        let Some(first) = selected.first() else {
            return Task::none();
        };
        if selected
            .iter()
            .any(|row| std::mem::discriminant(row) != std::mem::discriminant(first))
        {
            self.status = Some(TransientNotice::warning(
                "Select only projects, worktrees, or channels to delete together.",
            ));
            return Task::none();
        }
        match selected
            .into_iter()
            .map(|row| self.selected_row_target(row))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(targets) => {
                self.sidebar_selection_ui.dialog = Some(Dialog {
                    targets,
                    error: None,
                });
                iced::widget::operation::focus(focus_id()).map(cosmic::Action::App)
            }
            Err(error) => {
                self.status = Some(TransientNotice::warning(error));
                Task::none()
            }
        }
    }

    pub(super) fn update_sidebar_selection(&mut self, action: Action) -> Task<Message> {
        let Some(mut dialog) = self.sidebar_selection_ui.dialog.take() else {
            return Task::none();
        };
        if matches!(action, Action::Dismiss) {
            return dialog
                .targets
                .first()
                .filter(|target| self.visible_sidebar_rows().contains(&target.row))
                .map_or_else(Task::none, |target| {
                    iced::widget::operation::focus(sidebar::selection_row_focus_id(target.row))
                        .map(cosmic::Action::App)
                });
        }
        // Revalidate every target before starting any operation. A reload may
        // have changed a path or removed a row while confirmation was open.
        for target in &dialog.targets {
            let current = self.selected_row_target(target.row);
            let error = match current {
                Ok(current) if current.directory == target.directory => None,
                Ok(_) => {
                    Some("A selected row changed location. Cancel and select it again.".to_owned())
                }
                Err(error) => Some(error),
            };
            if let Some(error) = error {
                dialog.error = Some(error);
                self.sidebar_selection_ui.dialog = Some(dialog);
                return Task::none();
            }
        }
        let mut tasks = Vec::new();
        for target in dialog.targets {
            tasks.push(match target.row {
                SelectionRow::Channel(id) => self.delete_channel(id),
                SelectionRow::Worktree(project, id) => self.delete_selected_worktree(project, id),
                SelectionRow::Project(project_id) => {
                    // Prepare the same project removal used by its row, then
                    // consume the already-confirmed intent without displaying
                    // another modal for each member of the group.
                    drop(self.handle_project_action(project_ui::Action::Command {
                        project_id,
                        worktree_id: None,
                        command: sidebar::ProjectCommand::Remove,
                    }));
                    self.handle_project_action(project_ui::Action::ConfirmRemove)
                }
            });
        }
        self.sidebar_state.clear_row_selection();
        Task::batch(tasks)
    }

    pub(super) fn sidebar_selection_dialog(&self) -> Option<Element<'_, Message>> {
        let dialog = self.sidebar_selection_ui.dialog.as_ref()?;
        let first = dialog.targets.first()?;
        let (verb, kind, effect) = match first.row {
            SelectionRow::Project(_) => (
                "Remove",
                "projects",
                "Close their sessions and remove them from cagents. Folders and files stay on disk.",
            ),
            SelectionRow::Worktree(..) => (
                "Delete",
                "worktrees",
                "Close their sessions and delete these Git worktree directories.",
            ),
            SelectionRow::Channel(_) => (
                "Delete",
                "channels",
                "Close and remove the conversations in these channels.",
            ),
        };
        let mut rows = vec![
            widget::text::title3(format!("{verb} {} selected {kind}?", dialog.targets.len()))
                .into(),
            design::body(effect).into(),
        ];
        let labels = widget::column::with_children(
            dialog
                .targets
                .iter()
                .map(|target| design::body(&target.label).into())
                .collect::<Vec<_>>(),
        )
        .spacing(4);
        rows.push(
            widget::container(widget::scrollable(labels))
                .max_height(240)
                .into(),
        );
        if let Some(error) = &dialog.error {
            rows.push(design::body(error).into());
        }
        rows.push(
            widget::row::with_children(vec![
                widget::button::standard("Cancel")
                    .on_press(Message::SidebarSelection(Action::Dismiss))
                    .into(),
                widget::button::destructive(format!("{verb} {} {kind}", dialog.targets.len()))
                    .id(focus_id())
                    .on_press(Message::SidebarSelection(Action::Confirm))
                    .into(),
            ])
            .spacing(8)
            .into(),
        );
        Some(dialog_ui::surface(
            widget::column::with_children(rows).spacing(12),
            560.0,
        ))
    }
}
