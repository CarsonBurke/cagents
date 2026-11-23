//! Dispatch for the sidebar's actions: navigation, project, channel and
//! worktree rows, and the confirmations they open.
use super::*;
use std::ops::ControlFlow;

impl Cagents {
    pub(super) fn update_sidebar(&mut self, action: SidebarAction) -> Handled {
        // A control in the pane can still be activated while a dialog
        // is up: the modal captures the pointer, and the focus
        // operation the dialog issues on open cannot reach a pane
        // widget to unfocus it, so a focused control keeps its focus
        // and answers to Enter, and an assistive technology's own
        // activation is not captured at all. Acting on one would
        // replace the dialog the user is filling in with a fresh one,
        // discarding what they had typed. Nothing is recorded either:
        // the selection an open would set is a claim that a card
        // exists, and refusing the open would make it a false one.
        //
        // The one exception is the discard confirmation's own answer.
        // That dialog is itself a modal, and its Cancel and Discard are
        // `SidebarAction`s that arrive right here, so refusing
        // everything while it is up would strand it with no way out - a
        // worse fault than the one this guard exists for.
        if self.modal_is_open() && !answers_discard_confirmation(&action) {
            return ControlFlow::Break(Task::none());
        }
        // Folding takes the rows, and any keyboard focus on one, out
        // of the tree; ask the tree drawn now before it does.
        if action == SidebarAction::ToggleInProgress {
            if !self.sidebar_state.toggle_in_progress() {
                return ControlFlow::Break(Task::none());
            }
            return ControlFlow::Break(
                sidebar::in_progress_holds_focus(
                    &self.sidebar_model,
                    &self.sidebar_state,
                    self.projects_sidebar_mode() == SidebarMode::Compact,
                )
                .map(|refocus| cosmic::Action::App(Message::FoldInProgress { refocus })),
            );
        }
        if matches!(&action, SidebarAction::FilesPage { key, .. }
            | SidebarAction::FilesQueryChanged { key, .. }
            | SidebarAction::ToggleFilesFilter { key }
            | SidebarAction::CommitMessageChanged { key, .. }
            | SidebarAction::CommitSubjectEditorCursor { key, .. }
            | SidebarAction::CommitEditorCursor { key, .. }
            | SidebarAction::CommitSubjectEditorAction { key, .. }
            | SidebarAction::CommitEditorAction { key, .. }
            | SidebarAction::AmendChanged { key, .. } if !self.source_control_is_bound(key))
        {
            return ControlFlow::Break(Task::none());
        }
        let action = if matches!(action, SidebarAction::ChangeGroup { .. }) {
            let Some(source) = &self.sidebar_model.source_control else {
                return ControlFlow::Break(Task::none());
            };
            let Some(resolved) = sidebar::resolve_group_command(source, &action) else {
                return ControlFlow::Break(Task::none());
            };
            resolved
        } else {
            action
        };
        // Entering a worktree that is being deleted is refused, and
        // for the same reason before the selection would claim it.
        if self
            .sidebar_entered_directory(&action)
            .is_some_and(|directory| self.refuse_in_deleting_worktree(&directory))
        {
            return ControlFlow::Break(Task::none());
        }
        if let SidebarAction::OpenWorktree {
            project_id,
            worktree_id,
            ..
        } = &action
            && let Some(project) = self.sidebar_model.project(*project_id)
        {
            self.sidebar_state
                .reveal_worktree(project, *worktree_id, true);
        }
        self.sidebar_state.apply(&action);
        // Opening a row, by pointer or keyboard, is navigation: it
        // ends any explicit selection and anchors a later range, but
        // selects nothing.
        let opened = match &action {
            SidebarAction::OpenRepository { project_id, .. } => {
                Some(sidebar::SelectionRow::Project(*project_id))
            }
            SidebarAction::OpenWorktree {
                project_id,
                worktree_id,
                ..
            } => Some(sidebar::SelectionRow::Worktree(*project_id, *worktree_id)),
            SidebarAction::OpenChannel { channel_id } => {
                Some(sidebar::SelectionRow::Channel(*channel_id))
            }
            SidebarAction::OpenCard { card } => Some(sidebar::SelectionRow::Card(*card)),
            _ => None,
        };
        if let Some(row) = opened {
            self.select_sidebar_rows(row, sidebar::SelectionIntent::Navigate);
        }
        let task = match action {
            SidebarAction::ToggleProjects => self.update_chrome(chrome_ui::Action::ToggleProjects),
            SidebarAction::RenameChannel(id) => self.update_chrome(chrome_ui::Action::RenameChannel(id)),
            SidebarAction::MarkChannelRead(id) => {
                let cards = self.durable.channels.iter().find(|channel| channel.id == id)
                    .map(|channel| channel.cards.clone()).unwrap_or_default();
                self.mark_cards_read(cards)
            }
            SidebarAction::ScrollProjects(y) => iced::widget::operation::scroll_by(
                widget::Id::new("sidebar.projects.scroll"),
                iced::widget::operation::AbsoluteOffset { x: 0.0, y },
            ).map(cosmic::Action::App),
            SidebarAction::OpenFirstSearchResult => {
                let rows = self.visible_sidebar_rows();
                let row = rows.iter().find(|row| !matches!(row, sidebar::SelectionRow::Project(_)))
                    .or_else(|| rows.first()).copied();
                row.map_or_else(Task::none, |row| self.open_sidebar_navigation_row(row))
            }
            SidebarAction::FocusFirstSearchResult => {
                let rows = self.visible_sidebar_rows();
                let row = rows.iter().find(|row| !matches!(row, sidebar::SelectionRow::Project(_)))
                    .or_else(|| rows.first()).copied();
                row.map_or_else(Task::none, |row| sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row)).map(cosmic::Action::App))
            }
            SidebarAction::NavigateRow { row, key } => {
                use keyboard::key::Named;
                if key == Named::Enter {
                    return ControlFlow::Break(self.open_sidebar_navigation_row(row));
                }
                if matches!(key, Named::ArrowLeft | Named::ArrowRight) {
                    if let sidebar::SelectionRow::Project(project) = row {
                        let expanded = self.sidebar_state.project_expanded(project);
                        if expanded == (key == Named::ArrowLeft) {
                            return ControlFlow::Break(self.dispatch(Message::Sidebar(SidebarAction::ToggleProject(project))));
                        }
                    }
                    return ControlFlow::Break(Task::none());
                }
                self.sidebar_keyboard_neighbor(row, key).map_or_else(Task::none, |row| sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row)).map(cosmic::Action::App))
            }
            SidebarAction::SelectRows { row, intent } => {
                self.select_sidebar_rows(row, intent);
                if matches!(intent, sidebar::SelectionIntent::Toggle | sidebar::SelectionIntent::Range { .. }) {
                    sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row)).map(cosmic::Action::App)
                } else { Task::none() }
            }
            SidebarAction::DeleteRow(row) => self.delete_sidebar_row(row),
            SidebarAction::DeleteSelectedRows => self.begin_sidebar_group_delete(),
            SidebarAction::ClearRowSelection => {
                self.sidebar_state.clear_row_selection();
                Task::none()
            }
            SidebarAction::DragWindow => Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Drag)),
            SidebarAction::MaximizeWindow => Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Maximize)),
            SidebarAction::WindowMenu => Task::done(cosmic::Action::Cosmic(cosmic::app::Action::ShowWindowMenu)),
            SidebarAction::ReorderMember { project, source, before } => self.reorder_sidebar_member(project, source, before),
            SidebarAction::ReorderProject { project, before } => {
                if self.sidebar_model.reorder_project(project, before) {
                    Arc::make_mut(&mut self.durable).project_order =
                        self.sidebar_model.projects.iter().map(|project| project.id).collect();
                    self.persist();
                }
                Task::none()
            }
            SidebarAction::CancelWorktreeOperation(id) => self.cancel_worktree_operation(id),
            SidebarAction::DismissWorktreeOperation(id) => self.dismiss_worktree_operation(id),
            SidebarAction::EditWorktreeOperation(id) => self.reopen_worktree_operation(id, false),
            SidebarAction::RetryWorktreeOperation(id) => self.reopen_worktree_operation(id, true),
            SidebarAction::RetryWorktreeDeletion { worktree_id, .. } => {
                self.handle_project_action(project_ui::Action::RetryDelete(worktree_id))
            }
            SidebarAction::RetryChannelDeletion { channel_id } => {
                self.handle_channel_ui(channel_ui::Action::RetryFailed(channel_id))
            }
            SidebarAction::OpenRepositoryPicker => {
                cosmic::task::message(Message::OpenRepository)
            }
            SidebarAction::ProjectCommand { project_id, worktree_id, command } => {
                self.handle_project_action(project_ui::Action::Command { project_id, worktree_id, command })
            }
            SidebarAction::OpenRepository {
                project_id,
                repository,
            } => {
                let directory = self.project_entry_directory(project_id, &repository);
                self.navigate_to_worktree(directory, true)
            }
            SidebarAction::OpenWorktree { path, .. } => self.navigate_to_worktree(path, true),
            SidebarAction::BrowsePullRequests { project_id } => self.browse_pull_requests(project_id),
            SidebarAction::OpenPullRequest {
                project_id,
                number,
                url,
            } => match self.settings.pull_request_open_target {
                PullRequestOpenTarget::Review
                    if self.pull_request_target(project_id, number).is_some() =>
                {
                    self.review_pull_request(project_id, number)
                }
                // A pull request the project's GitHub state no longer
                // lists can still be read on GitHub.
                PullRequestOpenTarget::Review | PullRequestOpenTarget::Link => {
                    self.open_clicked_link(url)
                }
            },
            SidebarAction::NewWorktree {
                project_id,
                repository,
            } => {
                if self.sidebar_model.project(project_id).is_some_and(|project| project.kind == ProjectKind::Folder) {
                    return ControlFlow::Break(Task::none());
                }
                self.open_worktree_dialog(
                    project_id,
                    repository,
                    WorktreeDraft::default(),
                    None,
                )
            }
            SidebarAction::Refresh => {
                let targets = repository_refresh_targets(&self.sidebar_model.projects);
                if targets.is_empty() {
                    return ControlFlow::Break(cosmic::task::message(Message::OpenRepository));
                }
                self.sidebar_model.data_state = SidebarDataState::Loading;
                Task::batch(targets.into_iter().map(|(repository, id)| {
                    self.repository_load_task(repository, Some(id), false)
                }))
            }
            SidebarAction::RefreshProject {
                project_id,
                repository,
            } => self.repository_load_task(repository, Some(project_id), false),
            SidebarAction::CreatePullRequest {
                project_id,
                worktree,
            } => {
                let Some(repository) = self.github_availability.get(&project_id).and_then(GitHubAvailability::repository_key).cloned()
                else {
                    self.status = Some(TransientNotice::warning(
                        "This repository is not associated with an authenticated GitHub remote",
                    ));
                    return ControlFlow::Break(Task::none());
                };
                let head = pull_request_head(
                    &self.sidebar_model.projects,
                    project_id,
                    worktree.as_deref(),
                );
                let base = self
                    .sidebar_model
                    .source_control
                    .as_ref()
                    .filter(|source| {
                        source.key.project_id == project_id
                            && worktree.as_ref().is_none_or(|path| {
                                source.key.canonical_path == *path
                            })
                    })
                    .map(|source| {
                        proven_pull_request_base(
                            &source.branch_changes,
                            &source.remotes,
                        )
                    })
                    .unwrap_or_default();
                self.pull_request_dialog_generation =
                    self.pull_request_dialog_generation.wrapping_add(1);
                self.pull_request_dialog = Some(PullRequestDialog {
                    project_id,
                    repository,
                    worktree,
                    base,
                    head,
                    title: String::new(),
                    body: String::new().into(),
                    draft: false,
                    retry_confirmation: None,
                });
                iced::widget::operation::focus(pull_request_dialog_focus_id())
                    .map(cosmic::Action::App)
            }
            SidebarAction::NewChannel { project_id } => {
                self.handle_channel_ui(channel_ui::Action::Create(project_id))
            }
            SidebarAction::OpenChannel { channel_id } => {
                self.handle_channel_ui(channel_ui::Action::Open(channel_id))
            }
            SidebarAction::OpenCard { card } => {
                // Every card of the conversation counts on the one row.
                let aliases = self
                    .sidebar_state
                    .attention_cards_shown_on(card)
                    .into_iter()
                    .filter(|alias| *alias != card)
                    .collect::<Vec<_>>();
                let focus = self.focus_attention_card(card);
                let read = if self.workspace.focused() == Some(card) && !aliases.is_empty() {
                    self.mark_cards_read(aliases)
                } else {
                    Task::none()
                };
                Task::batch([focus, read])
            }
            SidebarAction::DeleteChannel { channel_id } => {
                self.handle_channel_ui(channel_ui::Action::Delete(channel_id))
            }
            SidebarAction::FilesPage { key, .. } => Self::reset_files_scroll(&key),
            SidebarAction::FilesQueryChanged { key, .. } => {
                let reload = if self.folder_scope(&key) {
                    self.reload_source_control(&key)
                } else {
                    Task::none()
                };
                Task::batch([Self::reset_files_scroll(&key), reload])
            }
            SidebarAction::ToggleFilesFilter { key } => {
                let followup = if self.sidebar_state.files_filter_open() {
                    widget::text_input::focus(sidebar::files_filter_focus_id(&key))
                        .map(cosmic::Action::App)
                } else if self.folder_scope(&key) {
                    // Closing the filter clears its query. Folder rows
                    // must be read again with that cleared query.
                    self.reload_source_control(&key)
                } else {
                    Task::none()
                };
                Task::batch([Self::reset_files_scroll(&key), followup])
            }
            SidebarAction::OpenSettings => self.open_utility_card(CardKind::Settings),
            SidebarAction::OpenAutomations => self.open_automations(),
            SidebarAction::OpenWork => {
                self.work_ui.show(false);
                self.open_utility_card(CardKind::Work)
            }
            // The handle clamps to its own rendered bounds, and the
            // settings file accepts a narrower range, so the width is
            // clamped again before it can reach a rejected save.
            SidebarAction::Resize { width } => {
                self.settings.sidebar_width =
                    width.clamp(sidebar::EXPANDED_WIDTH_MIN, sidebar::EXPANDED_WIDTH_MAX);
                Task::none()
            }
            SidebarAction::ResizeFinished => self.save_settings_task(false),
            SidebarAction::OpenBranchFile { key, path, old_path, base_ref, head, merge_base } => {
                self.open_branch_file(key, path, old_path, base_ref, head, merge_base)
            }
            // Disclosing `Untracked` is what decides whether the
            // snapshot walks for untracked paths at all, so the read
            // is reissued with the group's new state.
            SidebarAction::ToggleSection { key, section: PanelSection::Untracked } => self.reload_source_control(&key),
            SidebarAction::OpenCommit { key, oid } => self.open_history_commit(key, oid),
            SidebarAction::CloseCommit { key } => {
                if self.source_control_is_bound(&key) {
                    self.history_ui.cancel();
                    if let Some(source) = self.sidebar_model.source_control.as_mut() { source.commit_changes = None; }
                }
                Task::none()
            }
            SidebarAction::ShowMoreCommitFiles { key, oid } => {
                if self.source_control_is_bound(&key)
                    && let Some(changes) = self.sidebar_model.source_control.as_mut().and_then(|source| source.commit_changes.as_mut())
                    && changes.commit.oid == oid {
                    changes.visible_files = changes.visible_files.saturating_add(200);
                }
                Task::none()
            }
            SidebarAction::OpenCommitFile { key, oid, path } => self.open_commit_file(key, oid, path),
            SidebarAction::ToggleDirectory { key, path } if self.sidebar_model.source_control.as_ref().is_some_and(|source| source.files.iter().any(|row| row.path == path && row.ignored)) => {
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    source.retry_ignored_directory(&path);
                }
                self.reload_source_control(&key)
            }
            SidebarAction::ToggleDirectory { key, .. } if self.folder_scope(&key) => {
                self.reload_source_control(&key)
            }
            SidebarAction::ToggleSection { .. }
            | SidebarAction::SetChangesTab { .. }
            | SidebarAction::ToggleSourceControlPanel { .. }
            | SidebarAction::SetChangeListMode { .. }
            | SidebarAction::ChangePage { .. }
            | SidebarAction::ChangeGroup { .. }
            | SidebarAction::SetWorktreePanel { .. }
            | SidebarAction::ToggleDirectory { .. }
            | SidebarAction::ToggleChangeDirectory { .. }
            | SidebarAction::CommitMessageChanged { .. }
            | SidebarAction::CommitSubjectEditorCursor { .. }
            | SidebarAction::CommitEditorCursor { .. }
            | SidebarAction::CommitSubjectEditorAction { .. }
            | SidebarAction::CommitEditorAction { .. }
            | SidebarAction::AmendChanged { .. }
            | SidebarAction::PublishRemoteChanged { .. }
            | SidebarAction::PublishBranchChanged { .. }
            | SidebarAction::CancelPublish
            // `apply` already cleared the pending discard, and the
            // dialog it was driving is gone with it.
            | SidebarAction::CancelDiscard => Task::none(),
            SidebarAction::RequestDiscard { .. } => {
                if self.sidebar_state.pending_discard().is_some() {
                    dialog_ui::Question::DiscardChanges.focus(
                        sidebar::discard_cancel_id(),
                        sidebar::discard_confirm_id(),
                    )
                } else {
                    Task::none()
                }
            }
            SidebarAction::RefreshWorktree { key } => {
                if self.source_control_is_bound(&key)
                    && let Some(source) = self.sidebar_model.source_control.as_mut() {
                        source.retry_ignored_directories();
                    }
                self.reload_source_control(&key)
            },
            SidebarAction::OpenFile { key, path } => {
                let opener = self.panel_opener();
                self.route_panel_open(&key, &path, opener)
            }
            SidebarAction::OpenFileInEditor { key, path, editor_id } => {
                if !self.source_control_is_bound(&key) {
                    return ControlFlow::Break(Task::none());
                }
                self.open_file_in_editor(
                    key.canonical_path.join(path.as_path()),
                    None,
                    editor_id,
                )
            }
            SidebarAction::OpenChange { key, path, area } => {
                // The card has two diff bases. A conflict has no staged
                // side to show, so it is opened against the working
                // tree like any other unstaged change.
                let opener = self.panel_opener();
                self.open_working_change(key, path, area, opener)
            }
            SidebarAction::Stage { key, selection } => {
                let Some(operation) = self.begin_mutation(&key, MutationKind::Stage, None) else {
                    return ControlFlow::Break(Task::none());
                };
                let request = StageRequest {
                    worktree: key.canonical_path.clone(),
                    selection,
                };
                Self::mutation_task(key, operation, move |service, cancel| {
                    (
                        mutation_report("Stage", service.stage(&request, cancel)),
                        false,
                    )
                })
            }
            SidebarAction::Unstage { key, selection } => {
                let Some(operation) = self.begin_mutation(&key, MutationKind::Unstage, None) else {
                    return ControlFlow::Break(Task::none());
                };
                let request = UnstageRequest {
                    worktree: key.canonical_path.clone(),
                    selection,
                };
                Self::mutation_task(key, operation, move |service, cancel| {
                    (
                        mutation_report("Unstage", service.unstage(&request, cancel)),
                        false,
                    )
                })
            }
            // The only arm that destroys work, and the only one the
            // dialog can raise: `RequestDiscard` builds the request
            // with `confirmed: false`, the dialog rebuilds it with
            // `confirmed: true`, and the service refuses an
            // unconfirmed one outright.
            SidebarAction::ConfirmDiscard { key, request } => {
                let Some(operation) = self.begin_mutation(&key, MutationKind::Discard, None) else {
                    return ControlFlow::Break(Task::none());
                };
                Self::mutation_task(key, operation, move |service, cancel| {
                    (
                        mutation_report("Discard", service.discard(&request, cancel)),
                        false,
                    )
                })
            }
            SidebarAction::Commit { key, message, amend } => {
                let commit_revision = self.sidebar_state.submitted_commit_revision(&key, &message, amend);
                let Some(operation) = self.begin_mutation(&key, MutationKind::Commit, commit_revision) else {
                    return ControlFlow::Break(Task::none());
                };
                let request = CommitRequest {
                    worktree: key.canonical_path.clone(),
                    message,
                };
                Self::mutation_task(key, operation, move |service, cancel| {
                    commit_report(service.commit_with_amend(&request, amend, cancel))
                })
            }
            SidebarAction::Push {
                key,
                branch,
                target,
            } => {
                let Some(operation) = self.begin_mutation(&key, MutationKind::Push, None) else {
                    return ControlFlow::Break(Task::none());
                };
                let request = PushRequest {
                    worktree: key.canonical_path.clone(),
                    branch,
                    target,
                };
                Self::mutation_task(key, operation, move |service, cancel| {
                    (push_report(service.push(&request, cancel)), false)
                })
            }
            SidebarAction::OpenPublish { .. } => Task::none(),
            SidebarAction::LoadMoreHistory { key, page } => {
                if !self.source_control_is_bound(&key) {
                    return ControlFlow::Break(Task::none());
                }
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    if source.history_loading || source.next_history_page != page { return ControlFlow::Break(Task::none()); }
                    source.mark_history_requested();
                }
                Self::source_control_history_task(self.history_ui.next_page_request(), key, self.sidebar_model.source_control.as_ref().and_then(|source| source.head_oid.as_deref()).and_then(|oid| crate::repository::Oid::parse_object(oid).ok()), page)
            }
            SidebarAction::ToggleControls => {
                let target = if self.sidebar_state.controls_expanded() {
                    sidebar::search_focus_id()
                } else {
                    sidebar::search_toggle_focus_id()
                };
                iced::widget::operation::focus(target).map(cosmic::Action::App)
            }
            // Renaming and duplicating belong to standalone ceditor;
            // this model never enables those menu items. The two copy
            // commands are pure clipboard work that a worktree browser
            // needs regardless of file management.
            SidebarAction::FileCommand { key, path, command } => match command {
                sidebar::FileCommand::RevealInFileManager => {
                    if crate::device::is_remote() {
                        self.status = Some(TransientNotice::warning(
                            "Remote files cannot be revealed in the local file manager",
                        ));
                        return ControlFlow::Break(Task::none());
                    }
                    if !self.source_control_is_bound(&key) {
                        return ControlFlow::Break(Task::none());
                    }
                    let path = key.canonical_path.join(path.as_path());
                    cosmic::task::future(async move {
                        Message::FileRevealed(file_manager::reveal(&path).await)
                    })
                }
                sidebar::FileCommand::CopyPath => {
                    chat_ui::invalidate_copy_intent();
                    iced::clipboard::write(
                        key.canonical_path.join(path.as_path()).display().to_string(),
                    ).map(cosmic::Action::App)
                },
                sidebar::FileCommand::CopyRelativePath => {
                    chat_ui::invalidate_copy_intent();
                    iced::clipboard::write(path.display().into_owned())
                        .map(cosmic::Action::App)
                }
                sidebar::FileCommand::Rename | sidebar::FileCommand::CopyFile => {
                    Task::none()
                }
            },
            // Hiding removes the control that did it.
            SidebarAction::HideReadAttention => {
                iced::widget::operation::focus(sidebar::attention_filter_focus_id())
                    .map(cosmic::Action::App)
            }
            SidebarAction::SearchChanged(_)
            | SidebarAction::SetFilter(_)
            | SidebarAction::ToggleProject(_)
            | SidebarAction::ToggleWorktrees(_)
            | SidebarAction::ToggleInProgress
            => Task::none(),
            // The settings page edits the same default, so this goes
            // through its path: the page's draft, the save and the
            // sidebar all follow.
            SidebarAction::ToggleCheckouts => self.apply_setting_input(
                SettingsField::SidebarWorktrees,
                self.settings.toggled_sidebar_worktrees().key().to_owned(),
            ),
            SidebarAction::SetWorktreeDisplay { project_id, mode } => {
                if let Some(mode) = mode {
                    self.settings.sidebar_worktree_overrides.insert(project_id, mode);
                } else {
                    self.settings.sidebar_worktree_overrides.remove(&project_id);
                }
                self.save_settings_task(false)
            },
        };
        ControlFlow::Continue(task)
    }
}
