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
                title_source: Default::default(),
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

    /// Selected channels in creation order, whatever order the sidebar
    /// lists them in.
    fn selected(app: &Cagents) -> Vec<ChannelId> {
        let mut ids = app
            .sidebar_state
            .selected_rows(&app.visible_sidebar_rows())
            .into_iter()
            .filter_map(|row| match row {
                SelectionRow::Channel(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        ids.sort_by_key(|id| {
            app.durable
                .channels
                .iter()
                .position(|channel| channel.id == *id)
        });
        ids
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
    fn opening_a_row_is_navigation_and_never_mounts_a_selection() {
        let (mut app, _, ids) = fixture();
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: ids[0],
        })));
        assert!(!app.sidebar_state.has_row_selection());
        // A Shift-click afterwards ranges from the row that was opened.
        drop(app.update(Message::Sidebar(SidebarAction::SelectRows {
            row: SelectionRow::Channel(ids[0]),
            intent: SelectionIntent::Navigate,
        })));
        drop(app.update(Message::Sidebar(SidebarAction::SelectRows {
            row: SelectionRow::Channel(ids[1]),
            intent: SelectionIntent::Range { extend: false },
        })));
        assert_eq!(selected(&app), [ids[0], ids[1]]);
    }

    #[test]
    fn deleting_a_focused_row_outside_the_selection_targets_only_that_row() {
        let (mut app, _, ids) = fixture();
        select(&mut app, &ids[..2]);
        drop(app.update(Message::Sidebar(SidebarAction::DeleteRow(
            SelectionRow::Channel(ids[2]),
        ))));
        assert!(app.sidebar_selection_ui.dialog.is_none());
        assert!(
            app.channel_ui.modal_open(),
            "a keystroke asks first, even for an empty channel"
        );
        drop(app.handle_channel_ui(channel_ui::Action::ConfirmDelete));
        let remaining = app
            .durable
            .channels
            .iter()
            .map(|channel| channel.id)
            .collect::<Vec<_>>();
        assert_eq!(remaining, ids[..2]);
        assert_eq!(
            selected(&app),
            [ids[0], ids[1]],
            "the untouched selection survives"
        );
    }

    #[test]
    fn a_selection_of_one_uses_that_rows_own_confirmation() {
        let (mut app, project, _) = fixture();
        app.select_sidebar_rows(SelectionRow::Project(project), SelectionIntent::Toggle);
        drop(app.begin_sidebar_group_delete());
        assert!(app.sidebar_selection_ui.dialog.is_none());
        assert!(
            app.project_ui.modal_open(),
            "the project's own Remove confirmation, with its cascade, opens"
        );
    }

    #[test]
    fn a_blocked_member_holds_the_whole_group_inside_the_dialog() {
        let (mut app, project, _) = fixture();
        let other = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            other,
            RepositoryId::new(),
            "Other project".into(),
            "/other-project".into(),
            Vec::new(),
            Vec::new(),
        ));
        app.project_ui
            .removing
            .insert("/selection-project".into(), Vec::new());
        app.select_sidebar_rows(SelectionRow::Project(project), SelectionIntent::Toggle);
        app.select_sidebar_rows(SelectionRow::Project(other), SelectionIntent::Toggle);
        drop(app.begin_sidebar_group_delete());
        let dialog = app.sidebar_selection_ui.dialog.as_ref().unwrap();
        assert_eq!(dialog.blocked(), 1, "the blocker is shown, not toasted");
        assert!(app.sidebar_selection_dialog().is_some());
        drop(app.update_sidebar_selection(Action::Confirm));
        assert!(
            !app.project_ui
                .removing
                .contains_key(Path::new("/other-project")),
            "no member of a blocked group is removed"
        );
        assert!(app.sidebar_selection_ui.dialog.is_some());

        // The blocker clears in the background; the open dialog notices
        // without being reopened, and one Confirm removes the whole group.
        app.project_ui
            .removing
            .remove(Path::new("/selection-project"));
        drop(app.update_sidebar_selection(Action::Confirm));
        assert!(app.sidebar_selection_ui.dialog.is_none());
        assert!(
            app.project_ui
                .removing
                .contains_key(Path::new("/other-project")),
            "the confirmed group proceeds once nothing blocks it"
        );
    }

    #[test]
    fn model_refresh_prunes_removed_rows_without_another_sidebar_click() {
        let (mut app, _, ids) = fixture();
        select(&mut app, &ids[..2]);
        Arc::make_mut(&mut app.durable)
            .channels
            .retain(|channel| channel.id != ids[0]);
        drop(app.update(Message::GitUi(git_ui::Action::Changed)));
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
    /// Where the row lives: a path for projects and worktrees, the project
    /// for a channel. Labels alone repeat across projects.
    detail: String,
    /// Why this member cannot go yet. Any blocker holds the whole group, so
    /// the confirmation never deletes a subset the user did not choose.
    blocker: Option<String>,
}

#[derive(Debug)]
pub(super) struct Dialog {
    targets: Vec<Target>,
    error: Option<String>,
}

impl Dialog {
    fn kind(&self) -> SelectionRow {
        self.targets[0].row
    }

    /// Confirm, unless a blocker disables it; then Cancel, so focus never
    /// lands on a dead control.
    pub(super) fn focus_target(&self) -> widget::Id {
        if self.blocked() > 0 {
            close_focus_id()
        } else {
            focus_id()
        }
    }

    fn blocked(&self) -> usize {
        self.targets
            .iter()
            .filter(|target| target.blocker.is_some())
            .count()
    }
}

pub(super) fn focus_id() -> widget::Id {
    widget::Id::new("dialog.sidebar-selection.confirm")
}

fn close_focus_id() -> widget::Id {
    widget::Id::new("dialog.sidebar-selection.close")
}

impl Cagents {
    fn visible_sidebar_rows(&self) -> Vec<SelectionRow> {
        sidebar::visible_selection_rows(
            &self.sidebar_model,
            &self.sidebar_state,
            matches!(self.projects_sidebar_mode(), SidebarMode::Compact),
        )
    }

    pub(super) fn select_sidebar_rows(&mut self, row: SelectionRow, intent: SelectionIntent) {
        // Navigation only moves the anchor, so a plain click never pays for
        // the filtered row order.
        let visible = if intent == SelectionIntent::Navigate {
            Vec::new()
        } else {
            self.visible_sidebar_rows()
        };
        self.sidebar_state.select_rows(row, intent, &visible);
    }

    /// Drops selected rows a filter, collapse, or reload hid. Free while
    /// nothing is selected, which is the ordinary state.
    pub(super) fn prune_sidebar_row_selection(&mut self) {
        if !self.sidebar_state.has_row_selection() {
            return;
        }
        let visible = self.visible_sidebar_rows();
        self.sidebar_state.prune_row_selection(&visible);
    }

    /// Deletes one row through the confirmation its context menu opens, so a
    /// keyboard Delete and the menu can never ask different questions.
    pub(super) fn delete_sidebar_row(&mut self, row: SelectionRow) -> Task<Message> {
        match row {
            SelectionRow::Project(project_id) => {
                self.handle_project_action(project_ui::Action::Command {
                    project_id,
                    worktree_id: None,
                    command: sidebar::ProjectCommand::Remove,
                })
            }
            SelectionRow::Worktree(project_id, worktree_id) => {
                self.handle_project_action(project_ui::Action::Command {
                    project_id,
                    worktree_id: Some(worktree_id),
                    command: sidebar::ProjectCommand::Delete,
                })
            }
            SelectionRow::Channel(id) => {
                self.handle_channel_ui(channel_ui::Action::RequestDelete(id))
            }
        }
    }

    fn selected_row_target(&self, row: SelectionRow) -> Result<Target, String> {
        match row {
            SelectionRow::Project(id) => {
                let project = self
                    .sidebar_model
                    .project(id)
                    .ok_or("The selected project is no longer open.")?;
                Ok(Target {
                    row,
                    directory: project.repository.clone(),
                    label: project.name.clone(),
                    detail: project.repository.display().to_string(),
                    blocker: self.selected_project_removal_blocker(id),
                })
            }
            SelectionRow::Worktree(project, id) => {
                let worktree = self
                    .sidebar_model
                    .project(project)
                    .and_then(|project| project.worktrees.iter().find(|worktree| worktree.id == id))
                    .ok_or("The selected worktree is no longer open.")?;
                Ok(Target {
                    row,
                    directory: worktree.path.clone(),
                    label: worktree.label().to_owned(),
                    detail: worktree.path.display().to_string(),
                    blocker: self.selected_worktree_deletion_blocker(project, id),
                })
            }
            SelectionRow::Channel(id) => {
                let channel = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .ok_or("The selected channel is no longer open.")?;
                let project = self
                    .sidebar_model
                    .project(channel.project_id)
                    .ok_or("The selected channel's project is no longer open.")?;
                let blocker = if self.channel_ui.is_deleting(id) {
                    Some("Channel deletion is already in progress.".into())
                } else if channel.cards.iter().any(|card| matches!(self.cards.get(card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())) {
                    Some("Save or close this channel's unsaved editor buffers first.".into())
                } else {
                    None
                };
                Ok(Target {
                    row,
                    directory: project.repository.clone(),
                    label: channel_ui::channel_display_title(channel).to_owned(),
                    detail: project.name.clone(),
                    blocker,
                })
            }
        }
    }

    /// Deletes the explicit selection. One member is simply that row, so it
    /// gets that row's own confirmation; several get one group confirmation
    /// that names every member and what goes with them.
    pub(super) fn begin_sidebar_group_delete(&mut self) -> Task<Message> {
        let visible = self.visible_sidebar_rows();
        self.sidebar_state.prune_row_selection(&visible);
        let selected = self.sidebar_state.selected_rows(&visible);
        let first = match selected.as_slice() {
            [] => return Task::none(),
            [only] => return self.delete_sidebar_row(*only),
            [first, ..] => *first,
        };
        if selected
            .iter()
            .any(|row| std::mem::discriminant(row) != std::mem::discriminant(&first))
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
                let dialog = Dialog {
                    targets,
                    error: None,
                };
                let focus = dialog.focus_target();
                self.sidebar_selection_ui.dialog = Some(dialog);
                iced::widget::operation::focus(focus).map(cosmic::Action::App)
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
        // have changed a path, removed a row, or dirtied a worktree while
        // confirmation was open.
        dialog.error = None;
        for target in &mut dialog.targets {
            match self.selected_row_target(target.row) {
                Ok(current) if current.directory == target.directory => {
                    target.blocker = current.blocker;
                }
                Ok(_) => {
                    dialog.error = Some(
                        "A selected row changed location. Cancel and select it again.".to_owned(),
                    );
                }
                Err(error) => dialog.error = Some(error),
            }
        }
        if dialog.error.is_some() || dialog.blocked() > 0 {
            let focus = dialog.focus_target();
            self.sidebar_selection_ui.dialog = Some(dialog);
            return iced::widget::operation::focus(focus).map(cosmic::Action::App);
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

    /// The consequences of the whole group, in the terms each kind's own
    /// confirmation uses: what else closes, and what stays on disk.
    fn group_cascade(&self, dialog: &Dialog) -> (Option<String>, &'static str) {
        match dialog.kind() {
            SelectionRow::Project(_) => {
                let channels = dialog
                    .targets
                    .iter()
                    .filter_map(|target| match target.row {
                        SelectionRow::Project(id) => Some(id),
                        _ => None,
                    })
                    .map(|id| {
                        self.durable
                            .channels
                            .iter()
                            .filter(|channel| channel.project_id == id)
                            .count()
                    })
                    .sum::<usize>();
                (
                    (channels > 0).then(|| {
                        format!(
                            "Their {channels} channel{} will be removed from every window.",
                            if channels == 1 { "" } else { "s" }
                        )
                    }),
                    "Terminals and chats in these projects, including detached sessions, will \
                     close. Repositories, folders, worktrees, and branches stay on disk.",
                )
            }
            SelectionRow::Worktree(..) => (
                self.worktree_session_cascade(
                    &dialog
                        .targets
                        .iter()
                        .map(|target| target.directory.as_path())
                        .collect::<Vec<_>>(),
                    "these worktrees",
                ),
                "Git will remove these worktree directories, including uncommitted and untracked files. \
                 Unsaved editor changes are discarded. Their branches are retained. \
                 Terminals and chats running in them, including detached ones, are closed first.",
            ),
            SelectionRow::Channel(_) => {
                let conversations = dialog
                    .targets
                    .iter()
                    .filter_map(|target| match target.row {
                        SelectionRow::Channel(id) => self
                            .durable
                            .channels
                            .iter()
                            .find(|channel| channel.id == id),
                        _ => None,
                    })
                    .map(|channel| self.channel_conversation_count(channel))
                    .sum::<usize>();
                (
                    (conversations > 0).then(|| {
                        format!(
                            "Deleting them closes the {conversations} conversation{} inside them.",
                            if conversations == 1 { "" } else { "s" }
                        )
                    }),
                    "The channels and the conversations they hold are removed from this \
                     workspace. Nothing outside them is touched.",
                )
            }
        }
    }

    pub(super) fn sidebar_selection_dialog(&self) -> Option<Element<'_, Message>> {
        let dialog = self.sidebar_selection_ui.dialog.as_ref()?;
        let kind = dialog.kind();
        let count = dialog.targets.len();
        let title = match kind {
            SelectionRow::Project(_) => format!("Remove {count} projects from cagents?"),
            _ => format!("{}?", kind.deletion_label(count)),
        };
        // Blockers clear in the background (a removal finishes, a buffer is
        // saved), so read them live rather than from when the dialog opened.
        // Confirm revalidates again before anything is deleted.
        let blockers = dialog
            .targets
            .iter()
            .map(|target| match self.selected_row_target(target.row) {
                Ok(current) => current.blocker,
                Err(_) => target.blocker.clone(),
            })
            .collect::<Vec<_>>();
        let members = widget::column::with_children(
            dialog
                .targets
                .iter()
                .zip(blockers.iter().cloned())
                .map(|(target, blocker)| {
                    let detail: Element<'_, Message> = match blocker {
                        Some(blocker) => widget::text::caption(blocker)
                            .class(cosmic::theme::Text::Custom(design::warning_text))
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .width(Length::Fill)
                            .into(),
                        None => super::notice_detail(target.detail.as_str()),
                    };
                    widget::column::with_children(vec![
                        super::notice_body(target.label.as_str()),
                        detail,
                    ])
                    .into()
                })
                .collect::<Vec<_>>(),
        )
        .spacing(space::S);
        let (cascade, note) = self.group_cascade(dialog);
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title(title).into(),
            widget::container(widget::scrollable(members))
                .max_height(240)
                .into(),
        ];
        if let Some(cascade) = cascade {
            rows.push(super::notice_body(cascade));
        }
        rows.push(super::notice_detail(note));
        let blocked = blockers.iter().filter(|blocker| blocker.is_some()).count();
        if blocked > 0 {
            let verb = match kind {
                SelectionRow::Project(_) => "removed",
                _ => "deleted",
            };
            rows.push(super::notice_body(if blocked == 1 {
                format!("One of these can't be {verb} yet. Cancel, resolve or deselect it, and try again.")
            } else {
                format!(
                    "{blocked} of these can't be {verb} yet. Cancel, resolve or deselect them, and try again."
                )
            }));
        }
        if let Some(error) = &dialog.error {
            rows.push(super::notice_body(error.as_str()));
        }
        rows.push(super::dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(close_focus_id())
                .on_press(Message::SidebarSelection(Action::Dismiss)),
            crate::ui_controls::button::destructive(kind.deletion_label(count))
                .id(focus_id())
                .on_press_maybe(
                    (blocked == 0).then_some(Message::SidebarSelection(Action::Confirm)),
                ),
        ));
        Some(dialog_ui::surface(
            widget::column::with_children(rows).spacing(space::M),
            500.0,
        ))
    }
}
