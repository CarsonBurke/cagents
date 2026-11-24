//! Applying a project's loaded repository, worktrees and GitHub state to the
//! sidebar, the open cards and the refresh bookkeeping.
use super::*;
use std::ops::ControlFlow;

impl Cagents {
    pub(super) fn repository_loaded(
        &mut self,
        open_cards: bool,
        project_id: ProjectId,
        generation: u64,
        result: Result<Box<LoadedRepository>, String>,
    ) -> Handled {
        if self.repository_generations.get(&project_id) != Some(&generation) {
            return ControlFlow::Break(Task::none());
        }
        self.refresh_ui.projects.remove(&project_id);
        self.refresh_ui.finished(
            project_id,
            result
                .as_ref()
                .is_ok_and(|loaded| loaded.github.refresh_succeeded()),
        );
        let mut task = Task::none();
        match result {
            Ok(loaded) => 'loaded: {
                if self.project_ui.closed.contains(&loaded.project.repository) {
                    self.repository_generations.remove(&project_id);
                    return ControlFlow::Break(Task::none());
                }
                let pr_request = if let GitHubAvailability::Ready {
                    repository_key,
                    pull_requests,
                    ..
                } = &loaded.github
                {
                    let key = repository_key.clone();
                    let worktrees = loaded
                        .worktrees
                        .iter()
                        .map(|(record, _)| (record.clone(), None))
                        .collect::<Vec<_>>();
                    let recent = pull_requests
                        .iter()
                        .map(|(pr, _)| pr.clone())
                        .collect::<Vec<_>>();
                    Some((key, worktrees, recent, loaded.head_repositories.clone()))
                } else {
                    None
                };
                let worktree = loaded
                    .repository
                    .as_ref()
                    .and_then(|repository| repository.worktree_root.clone())
                    .or_else(|| {
                        loaded
                            .worktrees
                            .first()
                            .map(|(record, _)| record.path.clone())
                    })
                    .unwrap_or_else(|| loaded.project.repository.clone());
                // Reconcile before publishing either durable records or
                // widget IDs. Concurrent adds have independent request
                // IDs but still describe the same repository.
                if let Some(existing) = self.sidebar_model.projects.iter_mut().find(|project| {
                    project.id != project_id
                        && (project.repository == loaded.project.repository
                            || loaded
                                .repository
                                .as_ref()
                                .is_some_and(|repository| project.repository_id == repository.id)
                            || project.worktrees.iter().any(|known| {
                                loaded
                                    .worktrees
                                    .iter()
                                    .any(|(record, _)| record.id == known.id)
                            }))
                }) {
                    // A load of that project which began after this
                    // one lists the repository more recently, whether
                    // it has landed yet or not, and it is also how a
                    // deletion is followed up. This listing may still
                    // name a worktree removed since, by this window or
                    // anyone else, so adding from it could bring one
                    // back, open; and the row's reloading or failed
                    // state is that newer load's to settle.
                    let superseded = self
                        .repository_generations
                        .get(&existing.id)
                        .is_some_and(|&latest| latest > generation);
                    let listed = if superseded {
                        &[][..]
                    } else {
                        // This read reached the same repository, so
                        // whatever an earlier failure recorded about
                        // the row it landed on no longer holds.
                        existing.unreachable = None;
                        existing.reloading = false;
                        &loaded.worktrees[..]
                    };
                    for (record, status) in listed {
                        if existing.worktrees.iter().any(|known| known.id == record.id) {
                            continue;
                        }
                        // A newly created linked worktree can be added
                        // without replacing fresher status/PR data,
                        // pending operations, or cached project chrome.
                        if let Some(session) = &self.session {
                            session.save_worktrees([WorktreeState {
                                id: record.id,
                                project_id: existing.id,
                                generation: 0,
                                path: record.path.clone(),
                                branch: record.branch.as_deref().map(|branch| {
                                    branch
                                        .strip_prefix("refs/heads/")
                                        .unwrap_or(branch)
                                        .to_owned()
                                }),
                                existence: Existence::Open,
                            }]);
                        }
                        existing
                            .worktrees
                            .push(SidebarWorktree::from_repository(record, status.as_ref()));
                    }
                    let existing_id = existing.id;
                    self.repository_generations.remove(&project_id);
                    if let Some(previous) = self.repository_pr_cancellations.remove(&project_id) {
                        previous.cancel();
                    }
                    self.github_availability.remove(&project_id);
                    self.review_repositories.remove(&project_id);
                    self.refresh_ui
                        .review_watch_repositories
                        .remove(&project_id);
                    self.sync_worktree_operation_rows();
                    self.sidebar_model.data_state = SidebarDataState::Ready;
                    if open_cards && !self.refuse_in_deleting_worktree(&worktree) {
                        task = self.open_checkout(worktree, true, DurableChange::Content).1;
                    }
                    // Publishing the reopened project below reads the
                    // rows before the projection after this update,
                    // and the new row may already hold the pane just
                    // opened.
                    self.refresh_sidebar_channels();
                    self.refresh_sidebar_worktree_panes();
                    if open_cards {
                        // The picker can resolve to a project already in this
                        // window. Finish the explicit reopen against that row;
                        // the provisional id never gets a row of its own.
                        if let Some(repository) = self.project_ui.reopening.remove(&project_id) {
                            self.project_ui.reopening.insert(existing_id, repository);
                            self.publish_project_opened(existing_id);
                        }
                    }
                    // What the project adds, such as its new worktree
                    // rows and the selection moving to one, is
                    // projected after this update like any other.
                    break 'loaded;
                }
                if let Some(session) = &self.session {
                    session.save_project(loaded.project.clone());
                    session.save_worktrees(loaded.worktrees.iter().map(|(record, _)| {
                        worktree_ui::worktree_state(loaded.project.id, record, Existence::Open)
                    }));
                }
                let worktrees = loaded
                    .worktrees
                    .iter()
                    .map(|(record, status)| {
                        let mut row = SidebarWorktree::from_repository(record, status.as_ref());
                        row.head_repository = loaded.head_repositories.get(&record.id).cloned();
                        row
                    })
                    .collect();
                let pull_requests = match &loaded.github {
                    GitHubAvailability::Ready { pull_requests, .. } => pull_requests
                        .iter()
                        .map(|(pull_request, checks)| {
                            SidebarPullRequest::from_github(loaded.project.id, pull_request, checks)
                        })
                        .collect(),
                    _ => self
                        .sidebar_model
                        .projects
                        .iter()
                        .find(|project| project.id == project_id)
                        .map(|project| project.display_pull_requests().to_vec())
                        .unwrap_or_default(),
                };
                let mut sidebar_project = SidebarProject::new(
                    loaded.project.id,
                    loaded
                        .repository
                        .as_ref()
                        .map_or(RepositoryId(loaded.project.id.as_uuid()), |repository| {
                            repository.id
                        }),
                    loaded.project.name.clone(),
                    loaded.project.repository.clone(),
                    worktrees,
                    pull_requests,
                )
                .with_kind(loaded.project.kind);
                if let GitHubAvailability::Ready {
                    pull_requests: recent,
                    ..
                } = &loaded.github
                {
                    let recent = recent.iter().map(|(pr, _)| pr.clone()).collect::<Vec<_>>();
                    apply_attached_worktree_prs(
                        &mut sidebar_project,
                        &loaded.worktrees,
                        &recent,
                        &[],
                    );
                }
                sidebar_project.github_ready = loaded.github.repository_key().is_some();
                if !sidebar_project.github_ready {
                    sidebar_project.revoke_github();
                }
                sidebar_project.github_status = if loaded.project.kind == ProjectKind::Folder {
                    "Folder project".into()
                } else {
                    loaded.github.label().into()
                };
                if let Some(key) = loaded.review_repository.clone() {
                    self.review_repositories.insert(project_id, key);
                } else {
                    self.review_repositories.remove(&project_id);
                }
                self.sync_review_watch(project_id, &loaded.github);
                self.github_availability
                    .insert(project_id, loaded.github.clone());
                if let Some(existing) = self
                    .sidebar_model
                    .projects
                    .iter_mut()
                    .find(|project| project.id == loaded.project.id)
                {
                    *existing = sidebar_project;
                } else {
                    self.sidebar_model.projects.push(sidebar_project);
                }
                self.sidebar_model
                    .apply_project_order(&self.durable.project_order);
                self.sync_worktree_operation_rows();
                self.sidebar_model.data_state = SidebarDataState::Ready;
                let pr_enrichment =
                    if let Some((key, worktrees, recent, head_repositories)) = pr_request {
                        let expected = self
                            .sidebar_model
                            .projects
                            .iter()
                            .find(|project| project.id == project_id)
                            .map(|project| {
                                project
                                    .worktrees
                                    .iter()
                                    .map(|row| (row.id, row.pull_request().cloned()))
                                    .collect::<HashMap<_, _>>()
                            })
                            .unwrap_or_default();
                        let cancellation = self
                            .repository_pr_cancellations
                            .get(&project_id)
                            .cloned()
                            .unwrap_or_default();
                        let request_worktrees = worktrees.clone();
                        let request_recent = recent.clone();
                        cosmic::task::future(async move {
                            let archived = tokio::task::spawn_blocking(move || {
                                attached_worktree_pull_requests(
                                    &GitHubService::system("gh"),
                                    &key,
                                    &request_worktrees,
                                    &request_recent,
                                    &head_repositories,
                                    &cancellation,
                                )
                            })
                            .await
                            .unwrap_or_default();
                            Message::AttachedWorktreePrsResolved {
                                project_id,
                                generation,
                                worktrees,
                                recent,
                                archived,
                                expected,
                            }
                        })
                    } else {
                        Task::none()
                    };
                // Restored pull request cards could not name their
                // pull request until this project's GitHub state and
                // sidebar entry both existed.
                let pending = Task::batch([
                    self.observe_git_repository(&loaded, generation),
                    self.open_pending_pull_requests(project_id),
                    self.prune_pull_request_refs(project_id),
                    self.update_review_requests(review_request_ui::Action::Tick),
                ]);
                if open_cards {
                    if !self.refuse_in_deleting_worktree(&worktree) {
                        task = self.open_checkout(worktree, true, DurableChange::Content).1;
                    }
                } else if let Some(active) = self.active_workspace.clone()
                    && self.project_owns_active_scope(project_id, &active)
                {
                    // Discovery can resolve legacy launch directories
                    // into distinct worktrees after session restoration.
                    // Only the project holding the scope can move it,
                    // and re-selecting resolves every card's owner.
                    task = if let Some(channel) = self.durable.active_channel {
                        self.select_channel(channel)
                    } else {
                        self.select_worktree(active, false)
                    };
                }
                // Every window shares which projects exist, and a
                // channel the shared model named in this repository
                // could not be adopted until the project existed here.
                self.publish_project_opened(project_id);
                task = Task::batch([
                    task,
                    pending,
                    pr_enrichment,
                    self.project_avatar_task(project_id),
                    if open_cards {
                        self.reconcile_workspace_model_for_repository(&loaded.project.repository)
                    } else {
                        self.defer_repository_reconciliation(loaded.project.repository.clone())
                    },
                ]);
            }
            Err(error) => {
                self.project_ui.reopening.remove(&project_id);
                let unavailable = GitHubAvailability::Failed {
                    detail: error.clone(),
                };
                if let Some(project) = self
                    .sidebar_model
                    .projects
                    .iter_mut()
                    .find(|project| project.id == project_id)
                {
                    project.github_status = unavailable.label().into();
                }
                self.github_availability.insert(project_id, unavailable);
                self.review_repositories.remove(&project_id);
                self.status = Some(TransientNotice::error(error.clone()));
                finish_repository_load_failure(&mut self.sidebar_model, project_id, error);
                task = self.apply_closed_project_tombstones();
            }
        }
        ControlFlow::Continue(task)
    }
}
