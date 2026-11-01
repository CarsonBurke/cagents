use super::*;

#[cfg(all(feature = "development", target_os = "linux"))]
pub(super) mod native_close_fixture;

use crate::repository::{CommandSpec, ProcessExecutor, RemovedWorktree, SystemProcessExecutor};
use crate::{
    github::avatar::{self, OwnerKey},
    model::{ChannelId, WorktreeId},
    sidebar::ProjectCommand,
};

#[derive(Debug, Clone, Copy)]
struct AutomaticAttempt {
    project: ProjectId,
    at: Instant,
}

/// Captured before the effect; an old path result cannot acquire new editor owners.
#[derive(Debug, Clone)]
pub struct WorktreeRemovalCapture {
    lsp: crate::lsp::LspWorktreeRemoval,
    cards: Vec<RemovalCard>,
    inventory: Option<u64>,
    device: Option<(String, String, String)>,
}

#[derive(Debug, Clone)]
struct RemovalCard {
    state: CardState,
    document: Option<LspDocumentId>,
    scope: Option<PathBuf>,
    channels: Vec<ChannelId>,
}

/// Terminal status and authenticated host evidence evolve while deletion waits
/// for sessions to close. The terminal ID identifies that source incarnation;
/// the card generation, document, scope and channel fences are checked separately.
/// Other descriptor contracts remain exact until they define a stable identity.
fn same_removal_source(captured: &CardKind, current: &CardKind) -> bool {
    match (captured, current) {
        (CardKind::Terminal(captured), CardKind::Terminal(current)) => captured.id == current.id,
        _ => captured == current,
    }
}

fn removal_device_key() -> Option<(String, String, String)> {
    crate::device::selected().map(|device| {
        (
            device.name.clone(),
            device.host.clone(),
            device.executable.clone(),
        )
    })
}

#[derive(Debug, Default)]
pub struct State {
    #[cfg(all(feature = "development", target_os = "linux"))]
    pub(in crate::app) native_fixture: Option<native_close_fixture::Fixture>,
    owners: HashMap<OwnerKey, Option<iced::advanced::image::Handle>>,
    projects: HashMap<ProjectId, OwnerKey>,
    dialog: Option<Dialog>,
    remove_dialog: Option<ProjectRemoval>,
    /// A confirmed removal awaiting the daemon's shared-model answer.
    pub(super) removing: HashMap<PathBuf, Vec<PathBuf>>,
    /// Projects closed by the shared model but still settling their channels.
    pub(super) closing: HashMap<ProjectId, PathBuf>,
    close_started: HashMap<ProjectId, Instant>,
    close_in_flight: HashSet<ProjectId>,
    close_failed: HashSet<ProjectId>,
    close_cancellations: HashMap<ProjectId, CancellationToken>,
    pub(super) close_generation: HashMap<ProjectId, u64>,
    next_close_generation: u64,
    /// Prevents an older model read or repository load from reopening a close.
    pub(super) closed: HashSet<PathBuf>,
    /// A deliberate reopen outranks a cached close until its model edit lands.
    pub(super) reopening: HashMap<ProjectId, PathBuf>,
    /// Deletions the user has confirmed, by worktree, until each resolves. A
    /// failed one stays so its row can say why and offer to run it again.
    deletions: HashMap<WorktreeId, Deletion>,
    automatic_attempts: HashMap<WorktreeId, AutomaticAttempt>,
    editors: Vec<crate::external_editor::Editor>,
}

/// A confirmed deletion. It runs behind the worktree's row, not a dialog.
#[derive(Debug, Clone)]
struct Deletion {
    target: Target,
    started_at: Instant,
    /// Why the last attempt failed; `None` while one is running.
    failure: Option<String>,
}

impl State {
    pub(super) fn is_reopening(&self, repository: &Path) -> bool {
        self.reopening.values().any(|path| path == repository)
    }

    pub fn modal_open(&self) -> bool {
        self.dialog.is_some() || self.remove_dialog.is_some()
    }

    fn running(&self) -> impl Iterator<Item = &Target> {
        self.deletions
            .values()
            .filter(|deletion| deletion.failure.is_none())
            .map(|deletion| &deletion.target)
    }

    /// Starts deleting the worktree at `path` as a confirmed deletion does,
    /// for a test of what that refuses.
    #[cfg(test)]
    pub(super) fn begin_deleting(&mut self, path: PathBuf) -> Target {
        let worktree = WorktreeId::new();
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(worktree),
            repository: path.clone(),
            label: String::new(),
            path,
        };
        self.deletions.insert(
            worktree,
            Deletion {
                target: target.clone(),
                started_at: Instant::now(),
                failure: None,
            },
        );
        target
    }

    /// Whether `directory` is inside a worktree whose deletion is running.
    ///
    /// Nothing may start there: its sessions have been or are being closed,
    /// and Git is about to remove the directory from under whatever did.
    pub(super) fn deleting_directory(&self, directory: &Path) -> bool {
        self.running()
            .any(|target| directory.starts_with(&target.path))
    }

    /// Whether `card` works inside a worktree whose deletion is running.
    pub(super) fn deleting_card(&self, card: &CardState) -> bool {
        self.running()
            .any(|target| card_belongs_to(card, &target.path))
    }

    fn row_deletion(&self, worktree: WorktreeId) -> Option<crate::sidebar::RowDeletion> {
        self.deletions
            .get(&worktree)
            .map(|deletion| match &deletion.failure {
                None => crate::sidebar::RowDeletion::Deleting,
                Some(reason) => crate::sidebar::RowDeletion::Failed(reason.as_str().into()),
            })
    }

    /// Whether the deletion of `worktree` is under way.
    fn is_deleting(&self, worktree: WorktreeId) -> bool {
        self.deletions
            .get(&worktree)
            .is_some_and(|deletion| deletion.failure.is_none())
    }

    /// Whether any worktree deletion is under way.
    pub(super) fn deletion_running(&self) -> bool {
        self.running().next().is_some()
    }

    pub(super) fn focus_target(&self) -> widget::Id {
        widget::Id::new(if self.remove_dialog.is_some() {
            "dialog.project.remove.cancel"
        } else {
            "dialog.project.close"
        })
    }
}

#[derive(Debug, Clone)]
struct ProjectRemoval {
    project: ProjectId,
    repository: PathBuf,
    kind: ProjectKind,
    name: String,
    worktrees: Vec<PathBuf>,
    channels: usize,
    project_sessions: ProjectSessionOwnership,
}

#[derive(Debug, Clone)]
struct Dialog {
    target: Target,
    /// A pre-flight refusal: deletion cannot proceed, so the confirm button
    /// stays disabled until the user resolves it outside the dialog.
    blocker: Option<String>,
    /// Why the last confirmed deletion of this worktree failed, when the
    /// dialog is opened again from its row. Confirming retries it.
    failure: Option<String>,
}

impl Dialog {
    fn message(&self) -> Option<&str> {
        self.blocker.as_deref().or(self.failure.as_deref())
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    project: ProjectId,
    worktree: Option<WorktreeId>,
    repository: PathBuf,
    path: PathBuf,
    label: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    Command {
        project_id: ProjectId,
        worktree_id: Option<WorktreeId>,
        command: ProjectCommand,
    },
    EditorsLoaded(Result<Vec<crate::external_editor::Editor>, String>),
    ConfirmDelete,
    ConfirmRemove,
    RemovedProject {
        repository: PathBuf,
        result: Result<crate::workspace_model::WorkspaceModel, String>,
    },
    AwaitClosedProject(ProjectId, u64),
    ProjectSessionsClosed(ProjectId, u64, Result<ProjectSessionClosure, String>),
    /// Runs a failed deletion again. The user confirmed it once already.
    RetryDelete(WorktreeId),
    AwaitSessions(Target, WorktreeRemovalCapture),
    AutomaticallyDeleted {
        target: Target,
        capture: WorktreeRemovalCapture,
        removed: Option<RemovedWorktree>,
    },
    Deleted {
        target: Target,
        capture: WorktreeRemovalCapture,
        result: Result<(), String>,
    },
    AvatarLoaded {
        owner: OwnerKey,
        image: Option<iced::advanced::image::Handle>,
    },
    Finished(Result<(), String>),
    Dismiss,
}

impl Cagents {
    /// Capture the original registrations before selection, waiting or worker I/O.
    pub(super) fn capture_worktree_removal(&self, target: &Target) -> WorktreeRemovalCapture {
        let channels = self.removal_card_channels();
        let cards = self
            .durable
            .cards
            .iter()
            .filter(|card| {
                card.existence.is_open()
                    && (card_belongs_to(card, &target.path)
                        || self
                            .durable
                            .card_scopes
                            .get(&card.id)
                            .is_some_and(|scope| scope.starts_with(&target.path)))
            })
            .map(|card| RemovalCard {
                state: card.clone(),
                document: self.lsp_document(card.id),
                scope: self.durable.card_scopes.get(&card.id).cloned(),
                channels: channels.get(&card.id).cloned().unwrap_or_default(),
            })
            .collect();
        WorktreeRemovalCapture {
            lsp: self.lsp.capture_worktree_removal(&target.path),
            cards,
            inventory: self.repository_generations.get(&target.project).copied(),
            device: removal_device_key(),
        }
    }

    fn removal_card_channels(&self) -> HashMap<CardId, Vec<ChannelId>> {
        let mut channels = HashMap::<CardId, Vec<ChannelId>>::new();
        for channel in &self.durable.channels {
            for &id in &channel.cards {
                channels.entry(id).or_default().push(channel.id);
            }
        }
        channels
    }

    fn original_removal_cards(
        &self,
        target: &Target,
        capture: &WorktreeRemovalCapture,
    ) -> Vec<CardId> {
        if capture.device != removal_device_key() {
            return Vec::new();
        }
        let channels = self.removal_card_channels();
        capture.cards.iter().filter(|owner| {
            self.card_state(owner.state.id).is_some_and(|current| {
                current.existence.is_open() && current.generation == owner.state.generation
                    && same_removal_source(&owner.state.kind, &current.kind)
            })
                && self.lsp_document(owner.state.id) == owner.document
                && self.durable.card_scopes.get(&owner.state.id) == owner.scope.as_ref()
                && channels.get(&owner.state.id).map(Vec::as_slice).unwrap_or_default() == owner.channels.as_slice()
                // A path-only empty repository descriptor is not a physical owner.
                && (!matches!(owner.state.kind, CardKind::Repository(_))
                    || self.repository_generations.get(&target.project).copied() == capture.inventory)
        }).map(|owner| owner.state.id).collect()
    }

    pub(super) fn sync_worktree_protections(&mut self) {
        #[cfg(test)]
        super::terminal_output_tests::record_protection();
        let mut paths = Vec::new();
        if let Some(path) = &self.active_workspace {
            paths.push(path.clone());
        }
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            // Drafts and parked sessions must not be discarded by an optional
            // cleanup, even before they acquire a daemon-hosted actor.
            if matches!(&card.kind, CardKind::Chat(_) | CardKind::Terminal(_))
                && let Some(scope) = self
                    .worktree_owners
                    .get(&card.id)
                    .or_else(|| self.durable.card_scopes.get(&card.id))
            {
                paths.push(scope.clone());
            }
            if let CardKind::Repository(view) = &card.kind {
                if view.selected_file.is_some() {
                    paths.push(view.worktree.clone());
                }
                if let Some(navigation) = self.durable.repository_navigation.get(&card.id)
                    && navigation.selected_file.is_some()
                {
                    paths.push(navigation.worktree.clone());
                }
            }
            if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card.id)
                && runtime.selected_path().is_some()
            {
                paths.push(runtime.descriptor.worktree.clone());
            }
        }
        if let Err(error) = self.worktree_protections.sync(paths) {
            tracing::debug!(%error, "automatic worktree cleanup unavailable until window scopes are protected");
        }
    }

    /// Sidebar discovery only proposes a candidate. The daemon fetches its PR
    /// again and independently authorizes the exact checkout under its fences.
    pub(super) fn cleanup_merged_worktree_task(
        &mut self,
        project_id: ProjectId,
        records: &[(WorktreeRecord, Option<StatusSnapshot>)],
        pull_requests: &[crate::github::PullRequest],
    ) -> Task<Message> {
        if !self.settings.auto_delete_merged_worktrees
            || crate::device::selected().is_some()
            || self.project_ui.deletion_running()
            || self.session.is_none()
        {
            return Task::none();
        }
        let Some(repository) = self.review_repositories.get(&project_id).cloned() else {
            return Task::none();
        };
        let current_ids = records
            .iter()
            .map(|(record, _)| record.id)
            .collect::<HashSet<_>>();
        self.project_ui
            .automatic_attempts
            .retain(|id, attempt| attempt.project != project_id || current_ids.contains(id));
        let candidate = records.iter().filter_map(|(record, _)| {
            let source = self.sidebar_model.project(project_id)?.worktrees.iter()
                .find(|row| row.id == record.id && row.path == record.path)?
                .head_repository.as_ref()?;
            let pr = matching_attached_pr(record, source, pull_requests)?;
            if pr.state.0 != "MERGED"
                || record.head_oid.as_deref() != Some(pr.head_ref_oid.as_str())
                || record.lock.locked
                || self
                    .active_workspace
                    .as_ref()
                    .is_some_and(|active| active.starts_with(&record.path))
                || self
                    .durable
                    .cards
                    .iter()
                    .any(|card| {
                        card.existence.is_open()
                            && card_belongs_to(card, &record.path)
                            && match &card.kind {
                                CardKind::Repository(view) => view.selected_file.is_some()
                                    || self.durable.repository_navigation.get(&card.id)
                                        .is_some_and(|navigation| navigation.selected_file.is_some())
                                    || matches!(self.cards.get(&card.id), Some(CardRuntime::Repository(runtime)) if runtime.selected_path().is_some() || runtime.blocks_navigation()),
                                _ => true,
                            }
                    })
                || self
                    .project_ui
                    .automatic_attempts
                    .get(&record.id)
                    .is_some_and(|attempt| attempt.at.elapsed() < Duration::from_secs(30))
            {
                return None;
            }
            let target = self.project_target(project_id, Some(record.id))?;
            if self.worktree_deletion_blocker(&target).is_some() {
                return None;
            }
            let branch = record
                .branch
                .as_deref()?
                .strip_prefix("refs/heads/")?
                .to_owned();
            Some((
                target,
                crate::worktree_cleanup::RemovalRequest {
                    repository: self.sidebar_model.project(project_id)?.repository.clone(),
                    path: record.path.clone(),
                    branch,
                    head_oid: record.head_oid.clone()?,
                    source_repository: source.clone(),
                    pull_request: crate::github::PullRequestKey {
                        repository: repository.clone(),
                        number: pr.number,
                    },
                },
            ))
        })
        // A refused candidate must not monopolize every later refresh. Prefer
        // unseen candidates, then those whose last attempt is oldest.
        .min_by_key(|(target, _)| target.worktree.and_then(|id| self.project_ui.automatic_attempts.get(&id).map(|attempt| attempt.at)));
        let Some((target, request)) = candidate else {
            return Task::none();
        };
        let id = target.worktree.unwrap();
        self.project_ui.automatic_attempts.insert(
            id,
            AutomaticAttempt {
                project: project_id,
                at: Instant::now(),
            },
        );
        self.project_ui.deletions.insert(
            id,
            Deletion {
                target: target.clone(),
                started_at: Instant::now(),
                failure: None,
            },
        );
        self.sync_worktree_operation_rows();
        let capture = self.capture_worktree_removal(&target);
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        cosmic::task::future(async move {
            let removed = tokio::task::spawn_blocking(move || {
                let reply = crate::device_service::request(crate::device_service::Operation::RemoveMergedWorktree { request }).ok()?;
                let crate::device_service::Reply::MergedWorktreeRemoved(Some(removed)) = reply else { return None; };
                if let Some(store) = store
                    && let Err(errors) = crate::repository::release_deleted_worktree_creations(&store, &removed) {
                    for error in errors { tracing::warn!(%error, "could not release removed worktree creation receipt"); }
                }
                Some(removed)
            }).await.ok().flatten();
            Message::ProjectUi(Action::AutomaticallyDeleted {
                target,
                capture,
                removed,
            })
        })
    }

    pub(super) fn selected_project_removal_blocker(&self, project: ProjectId) -> Option<String> {
        let Some(row) = self.sidebar_model.project(project) else {
            return Some("The project is no longer open.".into());
        };
        if self.project_ui.removing.contains_key(&row.repository)
            || self.project_ui.closing.contains_key(&project)
        {
            return Some("Project removal is already in progress.".into());
        }
        self.project_has_unsaved_editors(project)
            .then(|| "Save or close this project's unsaved editor buffers first.".into())
            .or_else(|| self.project_review_checkout_blocker(project))
    }

    pub(super) fn selected_worktree_deletion_blocker(
        &self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Option<String> {
        let Some(target) = self.project_target(project, Some(worktree)) else {
            return Some("The worktree is no longer open.".into());
        };
        if self.project_ui.is_deleting(worktree) || self.project_ui.deleting_directory(&target.path)
        {
            return Some("Worktree deletion is already in progress.".into());
        }
        self.worktree_deletion_blocker(&target)
    }

    pub(super) fn delete_selected_worktree(
        &mut self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Task<Message> {
        if let Some(reason) = self.selected_worktree_deletion_blocker(project, worktree) {
            self.status = Some(TransientNotice::warning(reason));
            return Task::none();
        }
        let Some(target) = self.project_target(project, Some(worktree)) else {
            return Task::none();
        };
        self.start_worktree_deletion(target)
    }
    /// Apply a daemon-owned project close in every window. Channels use their
    /// existing checked close path; the project stays visible until those
    /// closes settle, so a refused close remains reachable and retryable.
    pub(super) fn close_shared_project(&mut self, repository: &Path) -> Task<Message> {
        if self.project_ui.is_reopening(repository) {
            return Task::none();
        }
        self.project_ui.removing.remove(repository);
        if !self.project_ui.closed.insert(repository.to_path_buf()) {
            return self.complete_closed_projects();
        }
        let Some(project) = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == repository)
            .map(|project| project.id)
        else {
            return Task::none();
        };
        self.project_ui
            .closing
            .insert(project, repository.to_path_buf());
        self.project_ui
            .close_started
            .insert(project, Instant::now());
        self.project_ui.next_close_generation =
            self.project_ui.next_close_generation.wrapping_add(1);
        self.project_ui
            .close_generation
            .insert(project, self.project_ui.next_close_generation);
        self.repository_generations.remove(&project);
        if self.project_has_unsaved_editors(project) {
            self.status = Some(TransientNotice::warning(
                "Project closed in another window; save its open editor buffers before removing its local row.",
            ));
        }
        let channels = self
            .durable
            .channels
            .iter()
            .filter(|channel| channel.project_id == project)
            .map(|channel| channel.id)
            .collect::<Vec<_>>();
        for channel in Arc::make_mut(&mut self.durable)
            .channels
            .iter_mut()
            .filter(|channel| channel.project_id == project)
        {
            // Even an unacknowledged local channel is part of the explicit
            // close. Registry silence must not publish it back.
            channel.shared = true;
        }
        let close = self.delete_channels(channels);
        Task::batch([close, self.complete_closed_projects()])
    }

    pub(super) fn cancel_shared_project_close(&mut self, repository: &Path) {
        let Some(project) = self
            .project_ui
            .closing
            .iter()
            .find(|(_, path)| path.as_path() == repository)
            .map(|(id, _)| *id)
        else {
            return;
        };
        if let Some(cancellation) = self.project_ui.close_cancellations.remove(&project) {
            cancellation.cancel();
        }
        self.project_ui.closing.remove(&project);
        self.project_ui.close_started.remove(&project);
        self.project_ui.close_in_flight.remove(&project);
        self.project_ui.close_failed.remove(&project);
        self.project_ui.close_generation.remove(&project);
        self.project_ui.removing.remove(repository);
    }

    pub(super) fn complete_closed_projects(&mut self) -> Task<Message> {
        let ready = self
            .project_ui
            .closing
            .keys()
            .copied()
            .filter(|project| {
                !self.project_has_unsaved_editors(*project)
                    && self.project_review_checkout_blocker(*project).is_none()
                    && !self.project_ui.close_failed.contains(project)
                    && !self.project_ui.close_in_flight.contains(project)
                    && !self
                        .durable
                        .channels
                        .iter()
                        .any(|channel| channel.project_id == *project)
            })
            .collect::<Vec<_>>();
        Task::batch(ready.into_iter().map(|project| {
            let Some(row) = self.sidebar_model.project(project) else {
                return Task::none();
            };
            let generation = self.project_ui.close_generation[&project];
            let repository = row.repository.clone();
            let kind = row.kind;
            let roots = project_worktree_roots(row);
            let scope = self.project_session_scope(project);
            let project_sessions = self.project_session_ownership(project);
            let pending = self.durable.cards.iter().any(|card| {
                card.existence.is_open()
                    && self.project_owns_card(project, card, &scope)
                    && (self.pending_terminal_spawn(card.id) || self.chat_start_pending(card.id))
            });
            if pending {
                if self.project_ui.close_started[&project].elapsed() > Duration::from_secs(45) {
                    self.project_ui.close_failed.insert(project);
                    self.status = Some(TransientNotice::error(
                        "A project session did not finish starting. Retry removal after resolving its card.",
                    ));
                    return Task::none();
                }
                self.project_ui.close_in_flight.insert(project);
                return cosmic::task::future(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Message::ProjectUi(Action::AwaitClosedProject(project, generation))
                });
            }
            self.project_ui.close_in_flight.insert(project);
            let cancellation = CancellationToken::default();
            self.project_ui
                .close_cancellations
                .insert(project, cancellation.clone());
            cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let roots = project_session_roots(&repository, kind, roots);
                    close_sessions_in_scope(&roots, Some(&repository), &project_sessions, Some(&cancellation))
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                Message::ProjectUi(Action::ProjectSessionsClosed(project, generation, result))
            })
        }))
    }

    fn project_review_checkout_blocker(&self, project: ProjectId) -> Option<String> {
        let scope = self.project_session_scope(project);
        self.cards.iter().find_map(|(id, runtime)| {
            let belongs = matches!(runtime, CardRuntime::PullRequests(pane)
                if pane.project(&self.sidebar_model) == Some(project))
                || self
                    .card_state(*id)
                    .is_some_and(|card| self.project_owns_card(project, card, &scope));
            belongs.then(|| self.review_checkout_blocker(*id)).flatten()
        })
    }

    fn project_has_unsaved_editors(&self, project: ProjectId) -> bool {
        let scope = self.project_session_scope(project);
        self.cards.iter().any(|(id, runtime)| {
            matches!(runtime, CardRuntime::Repository(editor)
                if editor.blocks_navigation()
                    && self.card_state(*id).is_some_and(|card| self.project_owns_card(project, card, &scope)))
        })
    }

    fn project_session_scope(&self, project: ProjectId) -> ProjectSessionScope {
        let mut roots = Vec::new();
        let mut protected = Vec::new();
        for row in &self.sidebar_model.projects {
            if row.id == project {
                roots.extend(project_worktree_roots(row));
            } else {
                protected.extend(project_worktree_roots(row));
            }
        }
        ProjectSessionScope::new(roots, protected)
    }

    fn project_owns_card(
        &self,
        project: ProjectId,
        card: &CardState,
        scope: &ProjectSessionScope,
    ) -> bool {
        if let Some(channel) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&card.id))
        {
            return channel.project_id == project;
        }
        if let CardKind::Repository(view) = &card.kind {
            return view.project == project;
        }
        if let Some(directory) = self.durable.card_scopes.get(&card.id) {
            return scope.contains(directory);
        }
        match &card.kind {
            CardKind::Terminal(session) => scope.contains(&session.launch.working_directory),
            CardKind::Chat(session) => scope.contains(&session.launch.recipe.working_directory),
            _ => false,
        }
    }

    fn project_session_ownership(&self, project: ProjectId) -> ProjectSessionOwnership {
        let mut ownership = ProjectSessionOwnership {
            local_database: self
                .session
                .as_ref()
                .map(|session| session.database_path().to_path_buf()),
            ..Default::default()
        };
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            let channel = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card.id));
            let mut record = |id| {
                if let Some(channel) = channel {
                    if channel.project_id == project {
                        ownership.owned.insert(id);
                    } else {
                        ownership.protected.insert(id);
                    }
                } else if let Some(directory) = self.durable.card_scopes.get(&card.id) {
                    ownership
                        .directories
                        .entry(id)
                        .or_default()
                        .push(directory.clone());
                }
            };
            match &card.kind {
                CardKind::Terminal(session) => {
                    record(session.id);
                }
                CardKind::Chat(session) => {
                    record(session.id);
                    if let Some(terminal) = &session.terminal {
                        record(terminal.id);
                    }
                }
                _ => {}
            }
        }
        ownership
    }

    fn finish_closed_project(
        &mut self,
        id: ProjectId,
        closure: ProjectSessionClosure,
    ) -> Task<Message> {
        // Editors remain usable while session cleanup runs. Recheck their
        // navigation fence before retiring any local owner; saving or recovery
        // will resume the pending close through complete_closed_projects.
        if self.project_has_unsaved_editors(id) {
            self.status = Some(TransientNotice::warning(
                "Project closed in another window; save your edits or resolve file recovery before removing its local row.",
            ));
            return Task::none();
        }
        // An accepted checkout can begin after session census but before this
        // callback. Keep its owner and the committed close pending until it settles.
        if let Some(reason) = self.project_review_checkout_blocker(id) {
            self.status = Some(TransientNotice::warning(reason));
            return Task::none();
        }
        let Some(repository) = self.project_ui.closing.remove(&id) else {
            return Task::none();
        };
        self.project_ui.close_started.remove(&id);
        self.project_ui.close_in_flight.remove(&id);
        self.project_ui.close_failed.remove(&id);
        self.project_ui.close_cancellations.remove(&id);
        self.project_ui.close_generation.remove(&id);
        let Some(index) = self
            .sidebar_model
            .projects
            .iter()
            .position(|project| project.id == id)
        else {
            return Task::none();
        };
        let project = &self.sidebar_model.projects[index];
        let name = project.name.clone();
        let kind = project.kind;
        let worktrees = project.worktrees.clone();
        let roots = project_worktree_roots(project);
        let scope = &closure.scope;
        // The terminal host was closed before the shared edit. Retire this
        // window's saved cards too, so reopening the project does not revive
        // conversations whose processes were deliberately closed.
        let cards = self
            .durable
            .cards
            .iter()
            .filter(|card| {
                card.existence.is_open()
                    && match &card.kind {
                        CardKind::Terminal(session) => closure.ownership.contains(
                            session.id,
                            &session.launch.working_directory,
                            scope,
                        ),
                        CardKind::Chat(session) => closure.ownership.contains(
                            session.id,
                            &session.launch.recipe.working_directory,
                            scope,
                        ),
                        _ => self.project_owns_card(id, card, scope),
                    }
            })
            .map(|card| card.id)
            .collect::<Vec<_>>();
        for card in cards {
            self.retire_lsp_card(card);
            self.sidebar_state.forget_card_attention(card);
            self.chat_discard(card);
            self.cards.remove(&card);
            self.lifecycle.remove(card);
            if let Some(state) = self.card_state_mut(card) {
                state.existence = removed_existence("project removed by user");
            }
            Arc::make_mut(&mut self.durable).card_scopes.remove(&card);
        }
        let active_here = self
            .active_workspace
            .as_deref()
            .is_some_and(|active| scope.contains(active));
        self.sidebar_model.projects.remove(index);
        self.refresh_ui.git.forget(id);
        self.github_availability.remove(&id);
        self.review_repositories.remove(&id);
        self.project_ui.projects.remove(&id);
        self.repository_generations.remove(&id);
        if let Some(cancellation) = self.repository_pr_cancellations.remove(&id) {
            cancellation.cancel();
        }
        self.workspace_model_opens.remove(&repository);
        Arc::make_mut(&mut self.durable)
            .project_order
            .retain(|project| *project != id);
        if let Some(session) = &self.session {
            session.save_project(ProjectState {
                kind,
                id,
                generation: 0,
                name,
                repository: repository.clone(),
                existence: removed_existence("project removed by user"),
            });
            session.save_worktrees(worktrees.iter().map(|worktree| WorktreeState {
                id: worktree.id,
                project_id: id,
                generation: 0,
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                existence: removed_existence("project removed by user"),
            }));
        }
        for root in &roots {
            self.worktree_offsets.remove(root);
            self.worktree_focus.remove(root);
        }
        let select = if active_here {
            if let Some(next) = self.sidebar_model.projects.first() {
                let path = next.worktrees.first().map_or_else(
                    || {
                        next.repository
                            .parent()
                            .unwrap_or(&next.repository)
                            .to_path_buf()
                    },
                    |worktree| worktree.path.clone(),
                );
                self.select_worktree(path, false)
            } else {
                Arc::make_mut(&mut self.durable).active_channel = None;
                self.active_workspace = None;
                self.set_workspace_surfaces_visible(false);
                self.workspace = Self::rebuild_workspace(&WorkspaceState::default());
                Task::none()
            }
        } else {
            Task::none()
        };
        self.refresh_sidebar_channels();
        self.refresh_sidebar_worktree_panes();
        self.persist();
        Task::batch([select, self.lifecycle_sweep(), self.sync_chrome()])
    }

    pub(super) fn editor_discovery_task() -> Task<Message> {
        if crate::device::is_remote() {
            return Task::none();
        }
        cosmic::task::future(async {
            let result = tokio::task::spawn_blocking(crate::external_editor::installed)
                .await
                .map_err(|error| format!("Could not discover installed editors: {error}"));
            Message::ProjectUi(Action::EditorsLoaded(result))
        })
    }

    /// Opens `path` in the editor `editor_id` names, at `caret` when the
    /// file is open here and that editor can be told where.
    pub(super) fn open_file_in_editor(
        &mut self,
        path: PathBuf,
        caret: Option<crate::external_editor::Caret>,
        editor_id: String,
    ) -> Task<Message> {
        if self.modal_is_open() {
            return Task::none();
        }
        self.launch_editor(path, caret, &editor_id)
    }

    fn launch_editor(
        &mut self,
        path: PathBuf,
        caret: Option<crate::external_editor::Caret>,
        editor_id: &str,
    ) -> Task<Message> {
        let Some(editor) = self
            .project_ui
            .editors
            .iter()
            .find(|editor| editor.choice.id == editor_id)
            .cloned()
        else {
            self.status = Some(TransientNotice::error(
                "The selected editor is no longer available.",
            ));
            return Task::none();
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::external_editor::launch(&editor, &path, caret)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ProjectUi(Action::Finished(result))
        })
    }

    /// Run after installing RepositoryLoaded's new sidebar model, never in view.
    pub(super) fn project_avatar_task(&mut self, project: ProjectId) -> Task<Message> {
        let owner = self
            .github_availability
            .get(&project)
            .and_then(GitHubAvailability::repository_key)
            .and_then(OwnerKey::from_repository);
        let Some(owner) = owner.or_else(|| self.project_ui.projects.get(&project).cloned()) else {
            return Task::none();
        };
        self.project_ui.projects.insert(project, owner.clone());
        if let Some(image) = self.project_ui.owners.get(&owner) {
            if let Some(row) = self
                .sidebar_model
                .projects
                .iter_mut()
                .find(|row| row.id == project)
            {
                row.avatar.clone_from(image);
            }
            return Task::none();
        }
        // Insert before starting: None means loading or failed, both intentionally
        // deduplicated for this app lifetime, including repeated repository refreshes.
        self.project_ui.owners.insert(owner.clone(), None);
        cosmic::task::future(async move {
            let key = owner.clone();
            let image = tokio::task::spawn_blocking(move || avatar::load(&key))
                .await
                .ok()
                .flatten();
            Message::ProjectUi(Action::AvatarLoaded { owner, image })
        })
    }

    fn project_target(&self, project: ProjectId, worktree: Option<WorktreeId>) -> Option<Target> {
        let row = self
            .sidebar_model
            .projects
            .iter()
            .find(|row| row.id == project)?;
        let selected = match worktree {
            Some(id) => Some(row.worktrees.iter().find(|worktree| worktree.id == id)?),
            None => row.worktrees.first(),
        };
        Some(Target {
            project,
            worktree,
            repository: row.repository.clone(),
            path: selected.map_or_else(|| row.repository.clone(), |worktree| worktree.path.clone()),
            label: selected
                .filter(|_| worktree.is_some())
                .map_or_else(|| row.name.clone(), |worktree| worktree.label().to_owned()),
        })
    }

    pub(super) fn handle_project_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::AwaitClosedProject(project, generation) => {
                if self.project_ui.close_generation.get(&project) != Some(&generation) {
                    return Task::none();
                }
                self.project_ui.close_in_flight.remove(&project);
                return self.complete_closed_projects();
            }
            Action::ProjectSessionsClosed(project, generation, result) => {
                if self.project_ui.close_generation.get(&project) != Some(&generation) {
                    return Task::none();
                }
                self.project_ui.close_in_flight.remove(&project);
                self.project_ui.close_cancellations.remove(&project);
                if !self.project_ui.closing.contains_key(&project) {
                    return Task::none();
                }
                match result {
                    Ok(closure) => return self.finish_closed_project(project, closure),
                    Err(error) => {
                        self.project_ui.close_failed.insert(project);
                        self.status = Some(TransientNotice::error(format!(
                            "Could not finish removing project: {error}"
                        )));
                    }
                }
            }
            Action::AwaitSessions(target, capture) => {
                let running = target
                    .worktree
                    .and_then(|id| self.project_ui.deletions.get(&id))
                    .filter(|deletion| {
                        deletion.failure.is_none() && deletion.target.path == target.path
                    });
                if let Some(running) = running {
                    if running.started_at.elapsed() > Duration::from_secs(45) {
                        let reason = "A terminal or chat did not finish starting; close or retry its card, then retry deletion.".to_owned();
                        self.status = Some(TransientNotice::error(format!(
                            "Could not delete {}: {reason}",
                            target.label
                        )));
                        if let Some(id) = target.worktree
                            && let Some(deletion) = self.project_ui.deletions.get_mut(&id)
                        {
                            deletion.failure = Some(reason);
                        }
                        self.sync_worktree_operation_rows();
                        return Task::none();
                    }
                    return self.run_worktree_deletion(target, capture);
                }
            }
            Action::EditorsLoaded(result) => match result {
                Ok(editors) => {
                    let choices: Vec<_> =
                        editors.iter().map(|editor| editor.choice.clone()).collect();
                    self.sidebar_model.external_editors = choices.clone();
                    for runtime in self.cards.values_mut() {
                        if let CardRuntime::Repository(runtime) = runtime {
                            runtime.set_external_editors(choices.clone());
                        }
                    }
                    self.project_ui.editors = editors;
                }
                Err(error) => self.status = Some(TransientNotice::warning(error)),
            },
            Action::AvatarLoaded { owner, image } => {
                for project in &mut self.sidebar_model.projects {
                    if self.project_ui.projects.get(&project.id) == Some(&owner) {
                        project.avatar.clone_from(&image);
                    }
                }
                self.project_ui.owners.insert(owner, image);
            }
            Action::Command {
                project_id,
                worktree_id,
                command,
            } => {
                if self.modal_is_open() && !self.project_ui.modal_open() {
                    return Task::none();
                }
                let Some(target) = self.project_target(project_id, worktree_id) else {
                    return Task::none();
                };
                if self
                    .sidebar_model
                    .project(project_id)
                    .is_some_and(|project| project.kind == ProjectKind::Folder)
                    && matches!(
                        command,
                        ProjectCommand::NewWorktree
                            | ProjectCommand::Delete
                            | ProjectCommand::RepositoryUrl
                    )
                {
                    return Task::none();
                }
                // The row offers nothing while its worktree is being deleted;
                // a command that raced the deletion is refused the same way,
                // even once a read has moved the row to another path.
                if target
                    .worktree
                    .is_some_and(|id| self.project_ui.is_deleting(id))
                    || (target.worktree.is_some()
                        && self.project_ui.deleting_directory(&target.path))
                {
                    return Task::none();
                }
                match command {
                    ProjectCommand::CopyPath => {
                        chat_ui::invalidate_copy_intent();
                        return iced::clipboard::write(target.path.display().to_string());
                    }
                    ProjectCommand::CopyBranch
                    | ProjectCommand::NewWorktreeFromBranch
                    | ProjectCommand::PullRequest => {
                        let row = self.sidebar_model.project(project_id).and_then(|project| {
                            project
                                .worktrees
                                .iter()
                                .find(|worktree| Some(worktree.id) == worktree_id)
                        });
                        if matches!(command, ProjectCommand::PullRequest) {
                            if let Some(pr) = row.and_then(|row| row.pull_request()) {
                                return self.open_clicked_link(pr.url.clone());
                            }
                            return self.update(Message::Sidebar(
                                SidebarAction::CreatePullRequest {
                                    project_id,
                                    worktree: Some(target.path),
                                },
                            ));
                        }
                        let Some(branch) = row.and_then(|row| row.branch.clone()) else {
                            return Task::none();
                        };
                        if matches!(command, ProjectCommand::CopyBranch) {
                            chat_ui::invalidate_copy_intent();
                            return iced::clipboard::write(branch);
                        }
                        return self.open_worktree_dialog(
                            project_id,
                            target.repository,
                            WorktreeDraft {
                                base: branch.trim_start_matches("refs/heads/").to_owned(),
                                ..Default::default()
                            },
                            None,
                        );
                    }
                    ProjectCommand::ResetSidebarOrder => {
                        self.publish_model_edit(
                            crate::workspace_model::ModelEdit::ResetNavigation {
                                repository: target.repository,
                            },
                        );
                        return Task::none();
                    }
                    ProjectCommand::MoveUp | ProjectCommand::MoveDown => {
                        let order: Vec<_> = self
                            .sidebar_model
                            .projects
                            .iter()
                            .map(|project| project.id)
                            .collect();
                        let Some(index) = order.iter().position(|id| *id == project_id) else {
                            return Task::none();
                        };
                        let before = if matches!(command, ProjectCommand::MoveUp) {
                            if index == 0 {
                                return Task::none();
                            }
                            Some(order[index - 1])
                        } else {
                            if index + 1 >= order.len() {
                                return Task::none();
                            }
                            order.get(index + 2).copied()
                        };
                        return self.update(Message::Sidebar(SidebarAction::ReorderProject {
                            project: project_id,
                            before,
                        }));
                    }
                    ProjectCommand::OpenEditor(editor_id) => {
                        return self.launch_editor(target.path, None, &editor_id);
                    }
                    ProjectCommand::Delete => {
                        let blocker = self.worktree_deletion_blocker(&target);
                        let failure = target
                            .worktree
                            .and_then(|id| self.project_ui.deletions.get(&id))
                            .and_then(|deletion| deletion.failure.clone());
                        self.project_ui.dialog = Some(Dialog {
                            target,
                            blocker,
                            failure,
                        });
                        return iced::widget::operation::focus(self.project_ui.focus_target())
                            .map(cosmic::Action::App);
                    }
                    ProjectCommand::Remove => {
                        let Some(project) = self.sidebar_model.project(project_id) else {
                            return Task::none();
                        };
                        if self.project_ui.removing.contains_key(&project.repository) {
                            self.status = Some(TransientNotice::warning(
                                "Project removal is already in progress.",
                            ));
                            return Task::none();
                        }
                        let worktrees = project_worktree_roots(project);
                        self.project_ui.remove_dialog = Some(ProjectRemoval {
                            project: project_id,
                            repository: project.repository.clone(),
                            kind: project.kind,
                            name: project.name.clone(),
                            worktrees,
                            channels: self
                                .durable
                                .channels
                                .iter()
                                .filter(|channel| channel.project_id == project_id)
                                .count(),
                            project_sessions: ProjectSessionOwnership::default(),
                        });
                        return iced::widget::operation::focus(self.project_ui.focus_target())
                            .map(cosmic::Action::App);
                    }
                    ProjectCommand::Reveal => {
                        self.project_ui.dialog = None;
                        return launch_task(PathBuf::from("xdg-open"), target.path, false);
                    }
                    ProjectCommand::NewTerminal => {
                        self.project_ui.dialog = None;
                        let select = self.select_worktree(target.path.clone(), false);
                        let terminal = self.add_terminal(target.path);
                        return Task::batch([select, terminal]);
                    }
                    ProjectCommand::NewChannel => {
                        self.project_ui.dialog = None;
                        if target.worktree.is_some() {
                            let (selected, scope) =
                                self.select_worktree_checked(target.path, false);
                            if !selected {
                                return scope;
                            }
                            // Selection commits synchronously, including aliases;
                            // do not reconstruct the target from a later callback.
                            let directory =
                                self.active_workspace.clone().expect("selected worktree");
                            let thread = self.new_native_thread_at(directory);
                            return Task::batch([scope, thread]);
                        }
                        return cosmic::task::message(Message::Sidebar(
                            SidebarAction::NewChannel { project_id },
                        ));
                    }
                    ProjectCommand::NewWorktree => {
                        self.project_ui.dialog = None;
                        return cosmic::task::message(Message::Sidebar(
                            SidebarAction::NewWorktree {
                                project_id,
                                repository: target.repository,
                            },
                        ));
                    }
                    ProjectCommand::RepositoryUrl => {
                        let url = self
                            .github_availability
                            .get(&project_id)
                            .and_then(GitHubAvailability::repository_key)
                            .and_then(|key| {
                                Url::parse(&format!(
                                    "https://{}/{}/{}",
                                    key.authority(),
                                    key.owner,
                                    key.name
                                ))
                                .ok()
                            });
                        self.project_ui.dialog = None;
                        if let Some(url) = url {
                            return self.open_clicked_link(url);
                        }
                        self.status = Some(TransientNotice::warning(
                            "No GitHub repository URL is available. Refresh the repository after authenticating gh.",
                        ));
                    }
                }
            }
            Action::Dismiss => {
                self.project_ui.remove_dialog = None;
                let dismissed = self.project_ui.dialog.take();
                // Declining to run a failed deletion again is the answer to
                // it, so its row stops asking.
                if let Some(id) = dismissed.and_then(|dialog| dialog.target.worktree)
                    && self
                        .project_ui
                        .deletions
                        .get(&id)
                        .is_some_and(|deletion| deletion.failure.is_some())
                {
                    self.project_ui.deletions.remove(&id);
                    self.sync_worktree_operation_rows();
                }
                return self.project_dialog_closed();
            }
            Action::ConfirmRemove => {
                let Some(mut removal) = self.project_ui.remove_dialog.take() else {
                    return Task::none();
                };
                if self
                    .sidebar_model
                    .project(removal.project)
                    .is_none_or(|project| project.repository != removal.repository)
                {
                    self.status = Some(TransientNotice::warning(
                        "The project changed. Try removing it again.",
                    ));
                    return self.project_dialog_closed();
                }
                if let Some(reason) = self.project_review_checkout_blocker(removal.project) {
                    self.status = Some(TransientNotice::warning(reason));
                    return self.project_dialog_closed();
                }
                if self.project_has_unsaved_editors(removal.project) {
                    self.status = Some(TransientNotice::warning(
                        "Save or close this project's unsaved editor buffers before removing it.",
                    ));
                    return self.project_dialog_closed();
                }
                let repository = removal.repository.clone();
                removal.project_sessions = self.project_session_ownership(removal.project);
                self.project_ui.close_failed.remove(&removal.project);
                self.project_ui
                    .close_started
                    .insert(removal.project, Instant::now());
                self.project_ui
                    .removing
                    .insert(repository.clone(), removal.worktrees.clone());
                let close = cosmic::task::future(async move {
                    let result =
                        tokio::task::spawn_blocking(move || remove_project_from_cagents(&removal))
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result);
                    Message::ProjectUi(Action::RemovedProject { repository, result })
                });
                return Task::batch([close, self.project_dialog_closed()]);
            }
            Action::RemovedProject { repository, result } => match result {
                Ok(model) => {
                    if model.sequence <= self.workspace_model.sequence {
                        return if self.workspace_model.project(&repository).is_none() {
                            self.close_shared_project(&repository)
                        } else {
                            Task::none()
                        };
                    }
                    // The observer is a different client and may deliver this
                    // edit before or after the command's own answer.
                    return self.apply_workspace_model_update(
                        super::workspace_model_sync::ModelUpdate::Edited {
                            sequence: model.sequence,
                            edit: crate::workspace_model::ModelEdit::CloseProject { repository },
                        },
                    );
                }
                Err(error) => {
                    self.project_ui.removing.remove(&repository);
                    if let Some(project) = self
                        .sidebar_model
                        .projects
                        .iter()
                        .find(|project| project.repository == repository)
                        .map(|project| project.id)
                    {
                        self.project_ui.close_started.remove(&project);
                    }
                    self.status = Some(TransientNotice::error(format!(
                        "Could not remove project: {error}"
                    )));
                }
            },
            Action::ConfirmDelete => {
                let Some(dialog) = &self.project_ui.dialog else {
                    return Task::none();
                };
                let target = dialog.target.clone();
                if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                    let dialog = self.project_ui.dialog.as_mut().unwrap();
                    dialog.blocker = Some(blocker);
                    dialog.failure = None;
                    return Task::none();
                }
                // Confirmed is registered: the row reports the deletion from
                // here, so the dialog has nothing left to say.
                self.project_ui.dialog = None;
                let deletion = self.start_worktree_deletion(target);
                return Task::batch([deletion, self.project_dialog_closed()]);
            }
            Action::RetryDelete(id) => {
                let Some(deletion) = self.project_ui.deletions.get(&id) else {
                    return Task::none();
                };
                if deletion.failure.is_none() {
                    return Task::none();
                }
                // The row may have been read again since; retry what it lists
                // now, and let a row that is gone take its failure with it.
                let Some(target) = self.project_target(deletion.target.project, Some(id)) else {
                    self.project_ui.deletions.remove(&id);
                    self.sync_worktree_operation_rows();
                    return Task::none();
                };
                if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                    self.status = Some(TransientNotice::warning(format!(
                        "Could not delete {}: {blocker}",
                        target.label
                    )));
                    if let Some(deletion) = self.project_ui.deletions.get_mut(&id) {
                        deletion.failure = Some(blocker);
                    }
                    self.sync_worktree_operation_rows();
                    return Task::none();
                }
                return self.start_worktree_deletion(target);
            }
            Action::AutomaticallyDeleted {
                target,
                capture,
                removed,
            } => {
                if removed
                    .as_ref()
                    .is_some_and(|receipt| receipt.path == target.path)
                {
                    return self.handle_project_action(Action::Deleted {
                        target,
                        capture,
                        result: Ok(()),
                    });
                }
                if let Some(id) = target.worktree {
                    self.project_ui.deletions.remove(&id);
                }
                self.sync_worktree_operation_rows();
                return self.lifecycle_sweep();
            }
            Action::Deleted {
                target,
                capture,
                result,
            } => match result {
                Err(failure) => {
                    // A blocker names what to resolve, so it outranks Git's
                    // own account of the same refusal.
                    let reason = self.worktree_deletion_blocker(&target).unwrap_or(failure);
                    self.status = Some(TransientNotice::error(format!(
                        "Could not delete {}: {reason}",
                        target.label
                    )));
                    if let Some(deletion) = target
                        .worktree
                        .and_then(|id| self.project_ui.deletions.get_mut(&id))
                    {
                        deletion.failure = Some(reason);
                    }
                    self.sync_worktree_operation_rows();
                    // The worktree stays, so the cards the sweep held back
                    // while it was going can mount again, and the agents
                    // whose rows waited on it can be attached.
                    let sweep = self.lifecycle_sweep();
                    return Task::batch([sweep, self.attach_next_restored_agent()]);
                }
                Ok(()) => {
                    let current = self
                        .original_removal_cards(&target, &capture)
                        .into_iter()
                        .collect::<HashSet<_>>();
                    let ids = capture.cards.iter().filter(|owner| {
                        current.contains(&owner.state.id) && card_belongs_to(&owner.state, &target.path)
                            // Loaded and dirty buffers remain available for recovery.
                            && !matches!(self.cards.get(&owner.state.id), Some(CardRuntime::Repository(runtime))
                                if runtime.selected_path().is_some() || runtime.blocks_navigation())
                    }).map(|owner| owner.state.id).collect::<Vec<_>>();
                    if capture.device == removal_device_key() {
                        let closed = self.lsp.remove_captured_worktree(capture.lsp);
                        self.clear_removed_lsp_documents(&closed);
                    }
                    if let Some(id) = target.worktree {
                        self.project_ui.deletions.remove(&id);
                    }
                    // The daemon has already closed every terminal and chat in
                    // the directory, so their cards are tombstoned here without
                    // a second close request: dropping a runtime only detaches.
                    for &id in &ids {
                        self.sidebar_state.forget_card_attention(id);
                        self.chat_discard(id);
                        self.cards.remove(&id);
                        self.lifecycle.remove(id);
                        if let Some(card) = self.card_state_mut(id) {
                            card.existence = removed_existence("worktree removed");
                        }
                    }
                    // A card filed under the worktree without running in it -
                    // an adopted terminal or a provider sign-in, whose
                    // directories no worktree owns - outlives the deletion,
                    // so it moves to the repository rather than stay filed
                    // under a row that no longer exists, where no strip would
                    // show it again.
                    let durable = Arc::make_mut(&mut self.durable);
                    for id in &ids {
                        durable.card_scopes.remove(id);
                    }
                    for owner in &capture.cards {
                        if current.contains(&owner.state.id)
                            && let Some(scope) = durable.card_scopes.get_mut(&owner.state.id)
                            && Some(&*scope) == owner.scope.as_ref()
                            && scope.starts_with(&target.path)
                        {
                            *scope = target.repository.clone();
                        }
                    }
                    let emptied = self.close_emptied_channels(&ids);
                    self.worktree_offsets.remove(&target.path);
                    self.worktree_focus.remove(&target.path);
                    let removed = self
                        .sidebar_model
                        .projects
                        .iter_mut()
                        .find(|project| project.id == target.project)
                        .and_then(|project| {
                            let index = project
                                .worktrees
                                .iter()
                                .position(|worktree| Some(worktree.id) == target.worktree)?;
                            Some(project.worktrees.remove(index))
                        });
                    // Queued behind any save of the open worktree, so the
                    // tombstone is the catalog's last word on it.
                    if let Some(id) = target.worktree {
                        self.refresh_ui.git.forget_worktree(target.project, id);
                    }
                    if let (Some(id), Some(session)) = (target.worktree, &self.session) {
                        session.save_worktrees([WorktreeState {
                            id,
                            project_id: target.project,
                            generation: 0,
                            path: target.path.clone(),
                            branch: removed.and_then(|worktree| worktree.branch),
                            existence: removed_existence("worktree removed"),
                        }]);
                    }
                    self.forget_worktree_creation(&target.path);
                    // The window may have moved anywhere while Git worked, a
                    // channel included, so the scope it is on is re-selected
                    // to drop the retired cards rather than replaced.
                    let select = match self.durable.active_channel {
                        Some(channel) => self.select_channel(channel),
                        None => {
                            let scope = self
                                .active_workspace
                                .clone()
                                .filter(|active| !active.starts_with(&target.path))
                                .unwrap_or_else(|| target.repository.clone());
                            self.select_worktree(scope, false)
                        }
                    };
                    self.persist();
                    self.status = Some(TransientNotice::success(
                        "Worktree deleted; branch retained.",
                    ));
                    let refresh =
                        self.repository_load_task(target.repository, Some(target.project), false);
                    return Task::batch([
                        select,
                        emptied,
                        refresh,
                        self.lifecycle_sweep(),
                        self.sync_chrome(),
                    ]);
                }
            },
            Action::Finished(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                }
            }
        }
        Task::none()
    }

    /// Registers a confirmed deletion on its row and starts it.
    ///
    /// The workspace leaves the worktree now rather than once it is gone, so
    /// nothing the user does in the meantime lands in a directory whose
    /// sessions are being closed.
    fn start_worktree_deletion(&mut self, target: Target) -> Task<Message> {
        let Some(id) = target.worktree else {
            return Task::none();
        };
        let capture = self.capture_worktree_removal(&target);
        self.project_ui.deletions.insert(
            id,
            Deletion {
                target: target.clone(),
                started_at: Instant::now(),
                failure: None,
            },
        );
        self.sync_worktree_operation_rows();
        let leave = if self
            .active_workspace
            .as_deref()
            .is_some_and(|active| active.starts_with(&target.path))
        {
            self.select_worktree(target.repository.clone(), false)
        } else {
            Task::none()
        };
        Task::batch([leave, self.run_worktree_deletion(target, capture)])
    }

    /// Runs the deletion once no session in the worktree is still starting:
    /// one that is would come up after its directory had been emptied.
    fn run_worktree_deletion(
        &mut self,
        target: Target,
        capture: WorktreeRemovalCapture,
    ) -> Task<Message> {
        if self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && card_belongs_to(card, &target.path)
                && (self.pending_terminal_spawn(card.id) || self.chat_start_pending(card.id))
        }) {
            return cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Message::ProjectUi(Action::AwaitSessions(target, capture))
            });
        }
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        cosmic::task::future(async move {
            let request = target.clone();
            let result = tokio::task::spawn_blocking(move || {
                let removed = delete_worktree(&request)?;
                if let Some(store) = store
                    && let Some(removed) = removed
                    && let Err(errors) =
                        crate::repository::release_deleted_worktree_creations(&store, &removed)
                {
                    // The worktree is gone either way; what stays held is
                    // only the chance to create the same one again.
                    for error in errors {
                        tracing::warn!(
                            worktree = %request.path.display(),
                            %error,
                            "could not release the deleted worktree's creation"
                        );
                    }
                }
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ProjectUi(Action::Deleted {
                target,
                capture,
                result,
            })
        })
    }

    /// Refuses, and says so, to start anything in `directory` while the
    /// worktree it is in is being deleted.
    pub(super) fn refuse_in_deleting_worktree(&mut self, directory: &Path) -> bool {
        if self.sidebar_model.projects.iter().any(|project| {
            (self.project_ui.removing.contains_key(&project.repository)
                || self.project_ui.closing.contains_key(&project.id))
                && self.project_session_scope(project.id).contains(directory)
        }) {
            self.status = Some(TransientNotice::warning("That project is being removed."));
            return true;
        }
        if !self.project_ui.deleting_directory(directory) {
            return false;
        }
        self.status = Some(TransientNotice::warning("That worktree is being deleted."));
        true
    }

    /// Refuses, and says so, to start a session for `card` while the
    /// worktree it works in is being deleted.
    pub(super) fn refuse_for_deleting_card(&mut self, card: CardId) -> bool {
        if self.card_state(card).is_some_and(|state| {
            self.sidebar_model.projects.iter().any(|project| {
                (self.project_ui.removing.contains_key(&project.repository)
                    || self.project_ui.closing.contains_key(&project.id))
                    && self.project_owns_card(
                        project.id,
                        state,
                        &self.project_session_scope(project.id),
                    )
            })
        }) {
            self.status = Some(TransientNotice::warning("That project is being removed."));
            return true;
        }
        if !self
            .card_state(card)
            .is_some_and(|state| self.project_ui.deleting_card(state))
        {
            return false;
        }
        self.status = Some(TransientNotice::warning("That worktree is being deleted."));
        true
    }

    /// Hands focus back to whatever the dialog covered.
    fn project_dialog_closed(&mut self) -> Task<Message> {
        let focus = self.nested_modal_focus_target().unwrap_or_else(|| {
            self.workspace
                .focused()
                .map(|card| self.card_keyboard_focus(card))
                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
        });
        Task::batch([
            iced::widget::operation::focus(focus).map(cosmic::Action::App),
            self.lifecycle_sweep(),
            self.sync_chrome(),
        ])
    }

    /// Shows each deletion on its worktree's row. A read of the checkout
    /// rebuilds the rows, so this runs after every one; a failure whose
    /// worktree the read no longer lists has nothing left to retry.
    pub(super) fn sync_worktree_deletion_rows(&mut self) {
        let projects = &mut self.sidebar_model.projects;
        self.project_ui.deletions.retain(|id, deletion| {
            deletion.failure.is_none()
                || projects
                    .iter()
                    .flat_map(|project| &project.worktrees)
                    .any(|worktree| worktree.id == *id)
        });
        for worktree in projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
        {
            worktree.deletion = self.project_ui.row_deletion(worktree.id);
        }
    }

    fn worktree_deletion_blocker(&self, target: &Target) -> Option<String> {
        if self.session.is_none() {
            return Some(
                "Session storage is unavailable; deletion cannot be recorded safely.".into(),
            );
        }
        let Some(id) = target.worktree else {
            return Some("The primary repository cannot be deleted.".into());
        };
        let Some(project) = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.id == target.project)
        else {
            return Some("The project disappeared. Refresh before deleting.".into());
        };
        let Some(worktree) = project
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id && worktree.path == target.path)
        else {
            return Some("The worktree changed. Refresh before deleting.".into());
        };
        if worktree.path == project.repository
            || project.repository.parent() == Some(worktree.path.as_path())
        {
            return Some("The primary worktree cannot be deleted.".into());
        }
        if worktree.locked {
            return Some("This worktree is locked. Unlock it explicitly in Git first.".into());
        }
        if self.cards.values().any(|runtime| {
            matches!(runtime, CardRuntime::Repository(runtime)
            if runtime.descriptor.worktree.starts_with(&target.path) && runtime.is_saving())
        }) {
            return Some("Wait for this worktree’s pending file saves before deleting it.".into());
        }
        None
    }

    fn worktree_deletion_warning(&self, target: &Target) -> Option<String> {
        let dirty = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.id == target.project)
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| Some(worktree.id) == target.worktree)
            })
            .and_then(|worktree| worktree.dirty.as_ref())
            .is_some_and(crate::repository::DirtyState::is_dirty);
        let unsaved = self.cards.values().any(|runtime| {
            matches!(runtime, CardRuntime::Repository(runtime)
                if runtime.descriptor.worktree.starts_with(&target.path) && runtime.is_dirty())
        });
        match (dirty, unsaved) {
            (true, true) => Some("Uncommitted changes and unsaved editor changes in this worktree will be discarded.".into()),
            (true, false) => Some("Uncommitted changes in this worktree will be discarded.".into()),
            (false, true) => Some("Unsaved editor changes in this worktree will be discarded.".into()),
            (false, false) => None,
        }
    }

    /// What the deletion takes with it: the open terminal and chat cards
    /// inside the worktree, counted so the user knows before confirming.
    fn worktree_cascade_summary(&self, target: &Target) -> Option<String> {
        self.worktree_session_cascade(&[target.path.as_path()], "the worktree")
    }

    /// The open terminal and chat cards inside any of `worktrees`, as the
    /// sentence a deletion confirmation shows; `None` when nothing is open.
    /// `place` names the worktrees in that sentence.
    pub(super) fn worktree_session_cascade(
        &self,
        worktrees: &[&Path],
        place: &str,
    ) -> Option<String> {
        let (mut terminals, mut chats) = (0, 0);
        for card in &self.durable.cards {
            if !card.existence.is_open()
                || !worktrees.iter().any(|path| card_belongs_to(card, path))
            {
                continue;
            }
            match &card.kind {
                CardKind::Terminal(_) => terminals += 1,
                CardKind::Chat(_) => chats += 1,
                _ => {}
            }
        }
        let count =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let parts = match (terminals, chats) {
            (0, 0) => return None,
            (t, 0) => count(t, "terminal", "terminals"),
            (0, c) => count(c, "chat", "chats"),
            (t, c) => format!(
                "{} and {}",
                count(t, "terminal", "terminals"),
                count(c, "chat", "chats")
            ),
        };
        Some(format!(
            "This closes {parts} in {place} and ends their sessions."
        ))
    }

    pub(super) fn project_dialog_view(&self) -> Option<Element<'_, Message>> {
        if let Some(removal) = &self.project_ui.remove_dialog {
            let detail = if removal.channels == 0 {
                "No channels are attached to this project.".to_owned()
            } else {
                format!(
                    "Its {} channel{} will be removed from every window.",
                    removal.channels,
                    if removal.channels == 1 { "" } else { "s" }
                )
            };
            return Some(super::dialog_ui::surface(
                widget::column::with_children(vec![
                    design::title("Remove project from cagents?").into(),
                    super::notice_body(&removal.name),
                    super::notice_detail(removal.repository.display().to_string()),
                    super::notice_body(detail),
                    super::notice_detail(match removal.kind {
                        ProjectKind::Folder => "Terminals and chats in this folder, including detached sessions, will close. The folder and its files stay on disk.",
                        ProjectKind::Git => "Terminals and chats in its worktrees, including detached sessions, will close. The repository, worktrees, and branches stay on disk.",
                    }),
                    super::dialog_ui::confirmation_actions(
                        crate::ui_controls::button::secondary("Cancel")
                            .id(widget::Id::new("dialog.project.remove.cancel"))
                            .on_press(Message::ProjectUi(Action::Dismiss)),
                        crate::ui_controls::button::destructive("Remove project")
                            .id(widget::Id::new("dialog.project.remove.confirm"))
                            .on_press(Message::ProjectUi(Action::ConfirmRemove)),
                    ),
                ])
                .spacing(space::M),
                500.0,
            ));
        }
        let dialog = self.project_ui.dialog.as_ref()?;
        let target = &dialog.target;
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Delete worktree?").into(),
            super::notice_body(&target.label),
            super::notice_detail(target.path.display().to_string()),
            super::notice_detail(
                "Git will remove this worktree directory, including uncommitted and untracked files. The branch is retained. \
                 Terminals and chats running in it, including detached ones, are closed \
                 first.",
            ),
        ];
        if let Some(warning) = self.worktree_deletion_warning(target) {
            rows.push(super::notice_body(warning));
        }
        if let Some(cascade) = self.worktree_cascade_summary(target) {
            rows.push(super::notice_body(cascade));
        }
        if let Some(message) = dialog.message() {
            rows.push(super::notice_body(message));
        }
        // Reopened from a row whose deletion failed, the dialog says why and
        // confirming runs it again; `ConfirmDelete` re-checks every
        // precondition first either way.
        let confirm = dialog
            .blocker
            .is_none()
            .then_some(Message::ProjectUi(Action::ConfirmDelete));
        rows.push(super::dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("dialog.project.close"))
                .on_press(Message::ProjectUi(Action::Dismiss)),
            crate::ui_controls::button::destructive(if dialog.failure.is_some() {
                "Retry delete"
            } else {
                "Delete worktree"
            })
            .id(widget::Id::new("dialog.project.confirm"))
            .on_press_maybe(confirm),
        ));
        Some(super::dialog_ui::surface(
            widget::column::with_children(rows).spacing(space::M),
            480.0,
        ))
    }
}

fn launch_task(executable: PathBuf, path: PathBuf, editor: bool) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new(executable);
            if editor {
                command.arg("--");
            }
            let child = command
                .arg(&path)
                .current_dir(if path.is_file() {
                    path.parent().unwrap_or(&path)
                } else {
                    &path
                })
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
            // Reap the launcher without waiting for a long-lived editor on the UI thread.
            std::thread::spawn(move || {
                let _ = {
                    let mut child = child;
                    child.wait()
                };
            });
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        Message::ProjectUi(Action::Finished(result))
    })
}

/// The tombstone a card gets when the container it lived in is deleted
/// outright, rather than the card itself being closed.
pub(super) fn removed_existence(reason: &str) -> Existence {
    Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: unix_millis(),
            reason: Some(reason.into()),
        },
    }
}

/// Whether a card lives inside the worktree at `path`: its process runs there,
/// or it views files there.
fn project_worktree_roots(project: &SidebarProject) -> Vec<PathBuf> {
    if project.kind == ProjectKind::Folder {
        return vec![project.repository.clone()];
    }
    let mut roots = project
        .worktrees
        .iter()
        .map(|worktree| worktree.path.clone())
        .collect::<Vec<_>>();
    if let Some(primary) = project
        .repository
        .parent()
        .filter(|parent| sidebar::is_primary_worktree(&project.repository, parent))
        && !roots.iter().any(|root| root == primary)
    {
        roots.push(primary.to_path_buf());
    }
    roots
}

fn card_belongs_to(card: &CardState, path: &Path) -> bool {
    match &card.kind {
        CardKind::Terminal(session) => session.launch.working_directory.starts_with(path),
        CardKind::Chat(session) => session.launch.recipe.working_directory.starts_with(path),
        CardKind::Repository(view) => view.worktree.starts_with(path),
        _ => false,
    }
}

/// Ends every daemon terminal and chat running inside the worktree, the
/// detached and hidden ones included, so the directory can go.
///
/// Deleting a worktree is a decision about everything in it; a session that
/// survived would be a shell in a directory that no longer exists. The
/// terminals are closed through the checked path with active work allowed,
/// which is the same request the close dialog sends once the user has
/// confirmed. A close the daemon cannot confirm stops the deletion: the
/// directory is not removed from under a process whose fate is unknown.
fn close_worktree_sessions(target: &Target) -> Result<(), String> {
    close_sessions_in_worktrees(std::slice::from_ref(&target.path))
}

fn close_sessions_in_worktrees(worktrees: &[PathBuf]) -> Result<(), String> {
    close_sessions_in_scope(worktrees, None, &ProjectSessionOwnership::default(), None).map(|_| ())
}

fn close_sessions_in_scope(
    worktrees: &[PathBuf],
    project: Option<&Path>,
    project_sessions: &ProjectSessionOwnership,
    cancellation: Option<&CancellationToken>,
) -> Result<ProjectSessionClosure, String> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err("Project removal was cancelled.".into());
    }
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = match crate::control::ControlClient::connect(&socket) {
        Ok(client) => client,
        // No daemon means no sessions to close.
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return if project.is_some() {
                Err("Cannot verify project ownership while the daemon is unavailable.".into())
            } else {
                Ok(ProjectSessionClosure {
                    scope: ProjectSessionScope::new(worktrees.to_vec(), Vec::new()),
                    ownership: project_sessions.clone(),
                })
            };
        }
        Err(error) => {
            return Err(format!(
                "Cannot reach the daemon to close sessions: {error}"
            ));
        }
    };
    // Project removal leaves every directory in place, unlike deleting a
    // worktree. Independently registered descendant projects keep their live
    // sessions, including those opened in another window.
    let mut project_sessions = project_sessions.clone();
    let scope = if let Some(project) = project {
        let model = client.read_workspace_model().map_err(|error| {
            format!("Could not verify project ownership before closing sessions: {error}")
        })?;
        let scope = shared_project_session_scope(worktrees, project, &model)?;
        let paths = crate::instance::window_database_paths().map_err(|error| {
            format!("Could not locate other windows before closing sessions: {error}")
        })?;
        for path in paths {
            if project_sessions.local_database.as_ref() == Some(&path) {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "Could not inspect ownership in {}: {error}",
                        path.display()
                    ));
                }
                Ok(_) => {}
            }
            let owners =
                crate::persistence::read_project_session_owners(&path).map_err(|error| {
                    format!("Could not verify ownership in {}: {error}", path.display())
                })?;
            project_sessions.extend_saved(owners, project);
        }
        scope
    } else {
        ProjectSessionScope::new(worktrees.to_vec(), Vec::new())
    };
    for session in client
        .list_chats()
        .map_err(|error| format!("Cannot list the worktree's chats: {error}"))?
    {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("Project removal was cancelled.".into());
        }
        if !project_sessions.contains(session.id, &session.launch.recipe.working_directory, &scope)
        {
            continue;
        }
        let id = session.id;
        if let Err(error) = client.close_chat(session) {
            client = crate::control::ControlClient::connect(&socket)
                .map_err(|reconnect| format!("Could not verify chat {id}: {reconnect}"))?;
            if client
                .list_chats()
                .map_err(|list| format!("Could not verify chat {id}: {list}"))?
                .iter()
                .any(|live| live.id == id)
            {
                return Err(format!("Could not close chat {id}: {error}"));
            }
        }
    }
    // A native chat may retain its source PTY. Close the actor first, then
    // inventory terminals again because that close may have retired the PTY.
    for terminal in client
        .list()
        .map_err(|error| format!("Cannot list the worktree's terminals: {error}"))?
    {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("Project removal was cancelled.".into());
        }
        if !project_sessions.contains(
            terminal.session.id,
            &terminal.session.launch.working_directory,
            &scope,
        ) {
            continue;
        }
        let id = terminal.session.id;
        let result = if matches!(
            terminal.session.continuity,
            TerminalContinuity::Exited { .. }
                | TerminalContinuity::RelaunchAvailable {
                    last_exit: Some(_),
                    ..
                }
        ) {
            client
                .release_completed_terminal(terminal.session, true)
                .map_err(|error| error.to_string())
                .and_then(|outcome| match outcome {
                    crate::terminal::ipc::TerminalCloseResult::Closed => Ok(()),
                    other => Err(format!("terminal {id} did not confirm close: {other:?}")),
                })
        } else {
            client
                .close_terminal_checked(id, true)
                .map_err(|error| error.to_string())
                .and_then(|outcome| match outcome {
                    crate::terminal::ipc::TerminalCloseResult::Closed => Ok(()),
                    other => Err(format!("terminal {id} did not confirm close: {other:?}")),
                })
        };
        if let Err(error) = result {
            // Another window or a natural exit may have won the race. Query a
            // fresh connection; a failed request can desynchronize this one.
            client = crate::control::ControlClient::connect(&socket)
                .map_err(|reconnect| format!("Could not verify terminal {id}: {reconnect}"))?;
            if client
                .list()
                .map_err(|list| format!("Could not verify terminal {id}: {list}"))?
                .iter()
                .any(|live| live.session.id == id)
            {
                return Err(format!("Could not close terminal {id}: {error}"));
            }
        }
    }
    Ok(ProjectSessionClosure {
        scope,
        ownership: project_sessions,
    })
}

#[derive(Debug, Clone)]
pub struct ProjectSessionClosure {
    scope: ProjectSessionScope,
    ownership: ProjectSessionOwnership,
}

#[cfg(test)]
impl ProjectSessionClosure {
    pub(super) fn for_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            scope: ProjectSessionScope::new(roots, Vec::new()),
            ownership: ProjectSessionOwnership::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct ProjectSessionScope {
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct ProjectSessionOwnership {
    owned: HashSet<TerminalId>,
    protected: HashSet<TerminalId>,
    directories: std::collections::HashMap<TerminalId, Vec<PathBuf>>,
    local_database: Option<PathBuf>,
}

impl ProjectSessionOwnership {
    fn extend_saved(
        &mut self,
        owners: Vec<(TerminalId, crate::persistence::SavedSessionOwner)>,
        project: &Path,
    ) {
        for (id, owner) in owners {
            match owner {
                crate::persistence::SavedSessionOwner::Project(path) if path == project => {
                    self.owned.insert(id);
                }
                crate::persistence::SavedSessionOwner::Project(_) => {
                    self.protected.insert(id);
                }
                crate::persistence::SavedSessionOwner::Directory(path) => {
                    self.directories.entry(id).or_default().push(path);
                }
            }
        }
    }

    fn contains(&self, id: TerminalId, directory: &Path, scope: &ProjectSessionScope) -> bool {
        if self.protected.contains(&id) {
            return false;
        }
        if let Some(directories) = self.directories.get(&id) {
            // A retained view wins when windows disagree about ownership.
            if directories.iter().any(|path| !scope.contains(path)) {
                return false;
            }
            return true;
        }
        self.owned.contains(&id) || scope.contains(directory)
    }
}

impl ProjectSessionScope {
    fn new(roots: Vec<PathBuf>, protected: Vec<PathBuf>) -> Self {
        let canonical = |paths: Vec<PathBuf>| {
            paths
                .into_iter()
                .map(|path| path.canonicalize().unwrap_or(path))
                .collect()
        };
        Self {
            roots: canonical(roots),
            protected: canonical(protected),
        }
    }

    fn contains(&self, directory: &Path) -> bool {
        let directory = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.to_path_buf());
        let depth = |roots: &[PathBuf]| {
            roots
                .iter()
                .filter(|root| directory.starts_with(root))
                .map(|root| root.components().count())
                .max()
        };
        match (depth(&self.roots), depth(&self.protected)) {
            (Some(owned), Some(protected)) => owned > protected,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

fn shared_project_session_scope(
    roots: &[PathBuf],
    repository: &Path,
    model: &crate::workspace_model::WorkspaceModel,
) -> Result<ProjectSessionScope, String> {
    let mut protected = Vec::new();
    let service = RepositoryService::system("git");
    for project in &model.projects {
        if project.repository == repository {
            continue;
        }
        match project.kind {
            ProjectKind::Folder => protected.push(project.repository.clone()),
            ProjectKind::Git => {
                let records = service.list_worktrees(&project.repository, &CancellationToken::default())
                    .map_err(|error| format!("Could not verify worktrees owned by {} before removing this project: {error}", project.name))?;
                protected.extend(records.into_iter().map(|record| record.path));
            }
        }
    }
    Ok(ProjectSessionScope::new(roots.to_vec(), protected))
}

fn project_session_roots(
    repository: &Path,
    kind: ProjectKind,
    mut roots: Vec<PathBuf>,
) -> Vec<PathBuf> {
    if kind == ProjectKind::Folder {
        return roots;
    }
    let service = RepositoryService::system("git");
    // A missing checkout must still be removable using its saved roots.
    if let Ok(records) = service.list_worktrees(repository, &CancellationToken::default()) {
        for record in records {
            if !roots.contains(&record.path) {
                roots.push(record.path);
            }
        }
    }
    roots
}

/// The confirmation covers live and detached sessions in every listed
/// worktree. Close those first; the shared project edit is the final step and
/// leaves all Git files and worktree directories untouched.
fn remove_project_from_cagents(
    removal: &ProjectRemoval,
) -> Result<crate::workspace_model::WorkspaceModel, String> {
    let roots = project_session_roots(&removal.repository, removal.kind, removal.worktrees.clone());
    close_sessions_in_scope(
        &roots,
        Some(&removal.repository),
        &removal.project_sessions,
        None,
    )?;
    let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    client
        .edit_workspace_model(crate::workspace_model::ModelEdit::CloseProject {
            repository: removal.repository.clone(),
        })
        .map_err(|error| error.to_string())
}

/// Removes the worktree through Git once Git confirms it is the one the user
/// chose and is safe to remove.
///
/// Git's listing is the authority here, not the window's catalog: the catalog
/// is written behind the UI and may not have caught up with a worktree created
/// moments ago. The deletion is recorded in the catalog afterwards, on the same
/// ordered lane as every other catalog write.
///
/// Returns what was removed, by which its creation is released, or `None`
/// when Git could not say where the worktree's administrative files were.
fn delete_worktree(target: &Target) -> Result<Option<RemovedWorktree>, String> {
    let id = target
        .worktree
        .ok_or("The primary repository cannot be deleted.")?;
    let service = RepositoryService::system("git");
    let cancellation = CancellationToken::default();
    let records = service
        .list_worktrees(&target.repository, &cancellation)
        .map_err(|error| error.to_string())?;
    let record = records
        .iter()
        .find(|record| record.id == id && record.path == target.path)
        .ok_or("Worktree changed or disappeared; refresh before deleting.")?;
    if records.first().is_some_and(|primary| primary.id == id) || record.bare {
        return Err("The primary worktree cannot be deleted.".into());
    }
    if record.lock.locked {
        return Err("The worktree is locked; unlock it explicitly in Git first.".into());
    }
    let removed = service
        .common_dir(&target.repository, &cancellation)
        .and_then(|common_dir| {
            Ok(RemovedWorktree {
                common_dir,
                admin_dir: service.git_dir(&target.path, &cancellation)?,
                path: target.path.clone(),
                branch: record.branch.clone(),
            })
        })
        .inspect_err(|error| {
            tracing::warn!(
                worktree = %target.path.display(),
                %error,
                "could not identify a worktree being deleted"
            );
        })
        .ok();
    close_worktree_sessions(target)?;
    // Confirmation authorizes discarding this worktree's changes. Git owns
    // directory removal and still refuses locked worktrees with one --force.
    let command = worktree_removal_command(target);
    let result = SystemProcessExecutor
        .execute(&command, &cancellation)
        .and_then(|output| output.require_success(&command.program));
    if let Err(error) = result {
        // A timeout may happen after removal. Reconcile before saying it failed.
        // Git's own account is what the row and the notice report: a bare
        // exit status gives the user nothing to act on before a retry.
        let records = service
            .list_worktrees(&target.repository, &cancellation)
            .map_err(|_| error.reported_detail())?;
        if records.iter().any(|record| record.id == id) {
            return Err(error.reported_detail());
        }
    }
    Ok(removed)
}

fn worktree_removal_command(target: &Target) -> CommandSpec {
    CommandSpec::new("git")
        .cwd(&target.repository)
        .args(["worktree", "remove", "--force", "--"])
        .arg(target.path.as_os_str())
        .timeout(Duration::from_secs(30))
        .output_limits(64 * 1024, 64 * 1024)
}

#[cfg(all(test, unix))]
#[path = "project_ui/close_edit_regression.rs"]
mod close_edit_regression;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::RowDeletion;

    fn removal_callback_test(case: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move { case() });
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn removal_lsp_app() -> (Cagents, tempfile::TempDir, Target) {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let worktree = root.path().join("linked");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            worktree.clone(),
            Some("feature".into()),
        );
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            vec![primary, linked.clone()],
            vec![],
        ));
        let servers = vec![crate::settings::LanguageServerSettings {
            id: "removal-callback".into(),
            languages: vec![crate::settings::LanguageServerLanguage {
                language_id: "rust".into(),
                extensions: vec!["rs".into(), "txt".into()],
            }],
            // Discovery is deliberately not delivered in these synchronous
            // callback cases. The real pending registration is enough; this
            // command is never started. Framed session retirement is tested by LSP.
            command: vec!["unused-before-discovery-delivery".into()],
            root_markers: vec![],
            initialization_options: None,
        }];
        app.settings.language_servers = servers.clone();
        app.lsp = LspManager::new(servers);
        (
            app,
            root,
            Target {
                project,
                worktree: Some(linked.id),
                repository,
                path: worktree,
                label: "feature".into(),
            },
        )
    }

    fn removal_editor(app: &mut Cagents, target: &Target, dirty: bool) -> (CardId, LspDocumentId) {
        let card = CardId::new();
        let mut runtime = if dirty {
            repository_widget::RepositoryRuntime::dirty_fixture(card)
        } else {
            repository_widget::RepositoryRuntime::open_fixture(
                card,
                "main.rs",
                "replacement buffer\n",
            )
            .0
        };
        let selected = runtime.selected_path().unwrap().to_owned();
        let view = RepositoryView {
            project: target.project,
            repository: target.repository.clone(),
            worktree: target.path.clone(),
            selected_file: Some(selected.clone()),
        };
        runtime.descriptor = view.clone();
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(CardState {
            id: card,
            generation: durable.generation,
            title: "Editor".into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Repository(view),
        });
        durable.card_scopes.insert(card, target.path.clone());
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.open_lsp_document(
            card,
            &target.path.join(selected),
            &target.path,
            1,
            &"buffer\n".into(),
        ));
        let doc = app.lsp_document(card).unwrap();
        assert_eq!(app.lsp.status(doc), LspStatus::Starting);
        (card, doc)
    }

    #[test]
    fn worktree_removal_callback_retires_only_original_documents_and_preserves_same_path_successor()
    {
        removal_callback_test(|| {
            for automatic in [false, true] {
                let (mut app, _root, target) = removal_lsp_app();
                let (original, old_doc) = removal_editor(&mut app, &target, false);
                let empty = CardId::new();
                let view = RepositoryView {
                    project: target.project,
                    repository: target.repository.clone(),
                    worktree: target.path.clone(),
                    selected_file: None,
                };
                let mut empty_state = app.card_state(original).unwrap().clone();
                empty_state.id = empty;
                empty_state.kind = CardKind::Repository(view.clone());
                Arc::make_mut(&mut app.durable).cards.push(empty_state);
                app.cards.insert(
                    empty,
                    CardRuntime::Repository(Box::new(
                        repository_widget::RepositoryRuntime::file_list_fixture(view, &[]),
                    )),
                );
                let capture = app.capture_worktree_removal(&target);
                let (successor, new_doc) = removal_editor(&mut app, &target, false);
                let before = app.card_state(successor).unwrap().clone();
                let scope = app.durable.card_scopes[&successor].clone();
                let action = if automatic {
                    Action::AutomaticallyDeleted {
                        capture,
                        removed: Some(RemovedWorktree {
                            common_dir: target.repository.join(".git"),
                            admin_dir: target.repository.join(".git/worktrees/feature"),
                            path: target.path.clone(),
                            branch: Some("refs/heads/feature".into()),
                        }),
                        target,
                    }
                } else {
                    Action::Deleted {
                        target,
                        capture,
                        result: Ok(()),
                    }
                };
                drop(app.update(Message::ProjectUi(action)));
                assert_eq!(app.lsp.status(old_doc), LspStatus::Disabled);
                assert!(
                    !app.card_state(empty).unwrap().existence.is_open(),
                    "unchanged captured empty card is retired"
                );
                assert!(!app.cards.contains_key(&empty));
                assert_eq!(app.lsp_document(original), None);
                assert!(
                    app.card_state(original).unwrap().existence.is_open(),
                    "the original loaded buffer remains recoverable"
                );
                assert_eq!(app.lsp.status(new_doc), LspStatus::Starting);
                assert_eq!(app.lsp_document(successor), Some(new_doc));
                let current = app.card_state(successor).unwrap();
                assert_eq!(current.kind, before.kind);
                assert_eq!(current.generation, before.generation);
                assert!(current.existence.is_open());
                assert_eq!(app.durable.card_scopes.get(&successor), Some(&scope));
                let Some(CardRuntime::Repository(runtime)) = app.cards.get(&successor) else {
                    panic!("successor runtime")
                };
                assert_eq!(runtime.lsp_document(), Some(new_doc));
                assert_eq!(
                    runtime.editor_text().as_deref(),
                    Some("replacement buffer\n")
                );
            }
        });
    }

    #[test]
    fn worktree_removal_callback_retains_dirty_buffer_but_clears_original_language_state() {
        removal_callback_test(|| {
            let (mut app, _root, target) = removal_lsp_app();
            let (card, doc) = removal_editor(&mut app, &target, true);
            let capture = app.capture_worktree_removal(&target);
            app.lsp_contexts.get_mut(&card).unwrap().failure =
                Some("old attachment failure".into());
            drop(app.update(Message::ProjectUi(Action::Deleted {
                target,
                capture,
                result: Ok(()),
            })));
            assert_eq!(app.lsp.status(doc), LspStatus::Disabled);
            assert!(!app.lsp_documents.contains_key(&doc));
            assert!(app.lsp_contexts[&card].failure.is_none());
            assert!(app.card_state(card).unwrap().existence.is_open());
            let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
                panic!("retained buffer")
            };
            assert!(runtime.is_dirty());
            assert_eq!(runtime.editor_text().as_deref(), Some("unsaved\n"));
            assert_eq!(runtime.lsp_document(), None);
        });
    }

    #[test]
    fn worktree_removal_callback_failure_retains_owner_and_old_success_cannot_clear_reopened_card()
    {
        removal_callback_test(|| {
            let (mut app, _root, target) = removal_lsp_app();
            let (card, original_doc) = removal_editor(&mut app, &target, false);
            let capture = app.capture_worktree_removal(&target);
            drop(app.update(Message::ProjectUi(Action::Deleted {
                target: target.clone(),
                capture: capture.clone(),
                result: Err("removal refused".into()),
            })));
            assert_eq!(app.lsp.status(original_doc), LspStatus::Starting);
            assert_eq!(app.lsp_document(card), Some(original_doc));
            assert!(app.card_state(card).unwrap().existence.is_open());
            drop(app.open_lsp_document(
                card,
                &target.path.join("main.rs"),
                &target.path,
                2,
                &"new incarnation\n".into(),
            ));
            let reopened = app.lsp_document(card).unwrap();
            assert_ne!(original_doc, reopened);
            assert_eq!(app.lsp.status(original_doc), LspStatus::Disabled);
            drop(app.update(Message::ProjectUi(Action::Deleted {
                target,
                capture,
                result: Ok(()),
            })));
            assert_eq!(app.lsp.status(reopened), LspStatus::Starting);
            assert_eq!(app.lsp_document(card), Some(reopened));
            assert_eq!(app.lsp_documents.get(&reopened), Some(&card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
                panic!("reopened card")
            };
            assert_eq!(runtime.lsp_document(), Some(reopened));
        });
    }

    #[test]
    fn worktree_removal_callback_retires_original_terminal_after_normal_exit() {
        removal_callback_test(|| {
            for (changed_source, changed_generation) in
                [(false, false), (true, false), (false, true)]
            {
                let (mut app, _root, target) = removal_lsp_app();
                let mut card = super::super::tests::terminal_card_state();
                let id = card.id;
                let CardKind::Terminal(session) = &mut card.kind else {
                    panic!("terminal fixture")
                };
                session.launch.working_directory = target.path.clone();
                session.continuity = TerminalContinuity::Live {
                    host_session_id: "removal-fixture".into(),
                    boot_id: "removal-boot".into(),
                    host_incarnation: "removal-host".into(),
                };
                card.residency = Residency::Hot;
                let original = card.kind.clone();
                let durable = Arc::make_mut(&mut app.durable);
                durable.cards.push(card);
                durable.card_scopes.insert(id, target.path.clone());
                app.cards.insert(id, CardRuntime::Loading);
                app.workspace = Cagents::rebuild_workspace(&app.durable);
                let capture = app.capture_worktree_removal(&target);
                assert_eq!(capture.cards.len(), 1);

                if changed_source {
                    let CardKind::Terminal(session) = &mut app.card_state_mut(id).unwrap().kind
                    else {
                        unreachable!()
                    };
                    session.id = TerminalId::new();
                }
                if changed_generation {
                    app.card_state_mut(id).unwrap().generation += 1;
                }
                // This is the production transition used by Completed and
                // nonclean Exited events, before the queued Deleted callback.
                app.retire_ended_terminal(id, 0, None, true);
                assert_ne!(app.card_state(id).unwrap().kind, original);
                assert!(matches!(app.cards.get(&id), Some(CardRuntime::Loading)));
                assert!(matches!(
                    app.card_state(id).unwrap().kind,
                    CardKind::Terminal(TerminalSession {
                        continuity: TerminalContinuity::RelaunchAvailable { .. },
                        ..
                    })
                ));
                drop(app.update(Message::ProjectUi(Action::Deleted {
                    target: target.clone(),
                    capture,
                    result: Ok(()),
                })));
                let replacement = changed_source || changed_generation;
                assert_eq!(app.card_state(id).unwrap().existence.is_open(), replacement);
                assert_eq!(app.cards.contains_key(&id), replacement);
                assert_eq!(app.durable.card_scopes.contains_key(&id), replacement);
                if replacement {
                    assert_eq!(app.durable.card_scopes.get(&id), Some(&target.path));
                }
            }
        });
    }

    /// A window with session storage and one project at `repository` whose
    /// linked worktree is `linked`, both of them saved.
    fn deletable_app(
        repository: &Path,
        linked: &SidebarWorktree,
        primary: &SidebarWorktree,
    ) -> (Cagents, tempfile::TempDir, ProjectId) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("test"),
        )
        .unwrap();
        let project = ProjectId::new();
        session.save_project(ProjectState {
            kind: Default::default(),
            id: project,
            generation: 0,
            name: "repo".into(),
            repository: repository.to_path_buf(),
            existence: Existence::Open,
        });
        app.session = Some(session);
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.to_path_buf(),
            vec![primary.clone(), linked.clone()],
            Vec::new(),
        ));
        (app, directory, project)
    }

    fn row(app: &Cagents, id: WorktreeId) -> Option<&SidebarWorktree> {
        app.sidebar_model
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .find(|worktree| worktree.id == id)
    }

    #[test]
    fn project_and_worktree_deletion_start_on_cancel_and_enter_keeps_them() {
        for remove in [false, true] {
            let repository = PathBuf::from("/repo");
            let primary = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.clone(),
                Some("main".into()),
            );
            let linked = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.join("worktrees/topic"),
                Some("topic".into()),
            );
            let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
            let opening = app.handle_project_action(Action::Command {
                project_id: project,
                worktree_id: (!remove).then_some(linked.id),
                command: if remove {
                    ProjectCommand::Remove
                } else {
                    ProjectCommand::Delete
                },
            });
            let cancel = widget::Id::new(if remove {
                "dialog.project.remove.cancel"
            } else {
                "dialog.project.close"
            });
            assert_eq!(
                super::super::tests::focused_element(app.project_dialog_view().unwrap(), opening),
                Some(cancel.clone())
            );
            let message = super::super::dialog_ui::tests::enter_control(
                app.project_dialog_view().unwrap(),
                cancel,
            );
            assert!(matches!(message, Message::ProjectUi(Action::Dismiss)));
            drop(app.update(message));
            assert!(!app.project_ui.modal_open());
            assert!(app.sidebar_model.project(project).is_some());
            assert!(row(&app, linked.id).is_some());
            assert!(app.project_ui.removing.is_empty());
            assert!(app.project_ui.deletions.is_empty());
        }
    }

    #[test]
    fn worktree_row_thread_uses_successful_selected_scope_and_refuses_before_draft() {
        for blocked in [true, false] {
            let repository = tempfile::tempdir().unwrap();
            let linked_directory = tempfile::tempdir().unwrap();
            let runtime = tempfile::tempdir().unwrap();
            let primary = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.path().into(),
                Some("main".into()),
            );
            let linked = SidebarWorktree::from_durable(
                WorktreeId::new(),
                linked_directory.path().into(),
                Some("feature/login".into()),
            );
            let (mut app, _state, project) = deletable_app(repository.path(), &linked, &primary);
            app.worktree_protections
                .use_fixture_runtime(runtime.path().into());
            drop(app.select_worktree(repository.path().into(), false));
            let old_scope = app.active_workspace.clone();
            let old_order = app.workspace.order().collect::<Vec<_>>();
            let old_cards = app.durable.cards.clone();
            let old_scopes = app.durable.card_scopes.clone();
            let _exclusive = blocked.then(|| {
                crate::worktree_cleanup::fixture_protection(
                    runtime.path(),
                    linked_directory.path(),
                    true,
                )
                .unwrap()
            });
            let action = || Action::Command {
                project_id: project,
                worktree_id: Some(linked.id),
                command: ProjectCommand::NewChannel,
            };
            drop(app.handle_project_action(action()));
            if blocked {
                assert_eq!(app.active_workspace, old_scope);
                assert_eq!(app.workspace.order().collect::<Vec<_>>(), old_order);
                assert_eq!(
                    app.durable.cards, old_cards,
                    "refused physical protection cannot expose a draft"
                );
                assert_eq!(app.durable.card_scopes, old_scopes);
                assert!(
                    app.status
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("Cannot open this worktree")
                );
            } else {
                assert_eq!(
                    app.active_workspace.as_deref(),
                    Some(linked_directory.path())
                );
                assert!(app.durable.active_channel.is_none());
                assert!(
                    app.durable.channels.is_empty(),
                    "worktree row opens a thread, not a project channel"
                );
                let added = app
                    .durable
                    .cards
                    .iter()
                    .filter(|card| !old_cards.iter().any(|old| old.id == card.id))
                    .collect::<Vec<_>>();
                assert_eq!(added.len(), 1);
                assert!(
                    matches!(&added[0].kind, CardKind::Chat(session) if session.launch.recipe.working_directory == linked_directory.path())
                );
                assert_eq!(
                    crate::worktree_cleanup::fixture_protection(
                        runtime.path(),
                        linked_directory.path(),
                        true
                    )
                    .unwrap_err()
                    .kind(),
                    std::io::ErrorKind::WouldBlock
                );
                // A successful same-scope no-op still authorizes another thread.
                let count = app.durable.cards.len();
                drop(app.handle_project_action(action()));
                assert_eq!(app.durable.cards.len(), count + 1);
                let CardKind::Chat(session) = &app.durable.cards.last().unwrap().kind else {
                    panic!("native thread")
                };
                assert_eq!(
                    session.launch.recipe.working_directory,
                    linked_directory.path()
                );
            }
        }
    }

    #[test]
    fn project_row_channel_uses_primary_scope_and_branch_label_without_saving_a_fake_name() {
        let repository = tempfile::tempdir().unwrap();
        let linked_directory = tempfile::tempdir().unwrap();
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.path().into(),
            Some("main".into()),
        );
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            linked_directory.path().into(),
            Some("feature/login".into()),
        );
        let (mut app, _state, project) = deletable_app(repository.path(), &linked, &primary);
        app.settings.default_channel_pane = settings::DefaultPane::NativeChat;
        drop(app.select_worktree(linked_directory.path().into(), false));
        let task = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: None,
            command: ProjectCommand::NewChannel,
        });
        let message = super::super::tests::task_app_message(task);
        assert!(
            matches!(&message, Message::Sidebar(SidebarAction::NewChannel {project_id}) if *project_id == project)
        );
        drop(app.update(message));
        assert_eq!(app.active_workspace.as_deref(), Some(repository.path()));
        let channel = app.durable.channels.last().unwrap();
        assert!(
            channel.title.is_empty(),
            "the first actual conversation remains free to name it"
        );
        assert_eq!(
            channel_ui::channel_display_title(channel, app.sidebar_model.project(project)),
            "New channel · main"
        );
        assert_eq!(
            app.sidebar_model.project(project).unwrap().channels[0].title,
            "New channel · main"
        );
        assert!(channel.cards.iter().any(|id| matches!(&app.card_state(*id).unwrap().kind, CardKind::Chat(session) if session.launch.recipe.working_directory == repository.path())));
    }

    #[test]
    fn shared_project_close_removes_local_row_and_records_tombstones() {
        let repository = PathBuf::from("/repo");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        drop(app.close_shared_project(&repository));
        let generation = app.project_ui.close_generation[&project];
        drop(app.handle_project_action(Action::ProjectSessionsClosed(
            project,
            generation,
            Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
        )));
        assert!(app.sidebar_model.project(project).is_none());
        assert!(app.project_ui.closed.contains(&repository));

        let session = app.session.as_ref().unwrap();
        session.flush(Duration::from_secs(5)).unwrap();
        let store = session.operation_store();
        assert!(
            !store
                .load_project(project)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        for worktree in [primary, linked] {
            assert!(
                !store
                    .load_worktree(worktree.id)
                    .unwrap()
                    .unwrap()
                    .existence
                    .is_open()
            );
        }
    }

    #[test]
    fn pending_pr_checkout_survives_local_and_inflight_shared_project_removal() {
        for scoped in [false, true] {
            let repository = PathBuf::from("/repo");
            let primary = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.clone(),
                Some("main".into()),
            );
            let linked = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.join("linked"),
                Some("feature".into()),
            );
            let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
            app.active_workspace = Some(if scoped {
                linked.path.clone()
            } else {
                repository.clone()
            });
            let (card, task) = app.add_pull_requests_card(if scoped {
                linked.path.clone()
            } else {
                repository.clone()
            });
            drop(task);
            // Session closure may already have started before checkout acceptance.
            drop(app.close_shared_project(&repository));
            let generation = app.project_ui.close_generation[&project];
            let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
                panic!("review");
            };
            let ticket = pane.review.fixture_existing_checkout_pending();
            let review_generation = pane.review.selection_generation();
            app.project_ui.remove_dialog = Some(ProjectRemoval {
                project,
                repository: repository.clone(),
                kind: Default::default(),
                name: "repo".into(),
                worktrees: vec![repository.clone(), linked.path.clone()],
                channels: 0,
                project_sessions: ProjectSessionOwnership::default(),
            });
            drop(app.handle_project_action(Action::ConfirmRemove));
            assert!(!app.project_ui.removing.contains_key(&repository));
            assert!(app.cards.contains_key(&card));
            drop(app.handle_project_action(Action::ProjectSessionsClosed(
                project,
                generation,
                Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
            )));
            assert!(app.sidebar_model.project(project).is_some());
            assert!(app.project_ui.closing.contains_key(&project));
            assert!(app.cards.contains_key(&card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.window_update_blocker().is_some());
            drop(app.update(Message::PullRequestReview {
                card,
                action: pull_request_review::Action::ExistingCheckout(
                    pull_request_review::checkout_ui::Action::Finished {
                        generation: review_generation,
                        ticket,
                        result: Err("refused; no effect".into()),
                    },
                ),
            }));
            assert!(
                app.project_ui.close_in_flight.contains(&project),
                "settlement resumes the committed project close"
            );
            drop(app.handle_project_action(Action::ProjectSessionsClosed(
                project,
                generation,
                Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
            )));
            assert!(app.sidebar_model.project(project).is_none());
            assert!(!app.cards.contains_key(&card));
        }
    }

    #[test]
    fn stalled_worktree_deletion_becomes_retryable() {
        let repository = PathBuf::from("/repo");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let target = Target {
            project,
            worktree: Some(linked.id),
            repository,
            path: linked.path,
            label: "feature".into(),
        };
        app.project_ui.deletions.insert(
            linked.id,
            Deletion {
                target: target.clone(),
                started_at: Instant::now() - Duration::from_secs(46),
                failure: None,
            },
        );
        drop(app.handle_project_action(Action::AwaitSessions(
            target.clone(),
            app.capture_worktree_removal(&target),
        )));
        assert!(app.project_ui.deletions[&linked.id].failure.is_some());
        assert!(!app.project_ui.is_deleting(linked.id));
    }

    /// A worktree deleted before the catalog ever stored it is still recorded
    /// as deleted, and the tombstone lands after a save of it as open that was
    /// still queued (#245).
    #[test]
    fn deletion_records_its_tombstone_without_waiting_for_the_catalog() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let target = app.project_ui.dialog.as_ref().unwrap().target.clone();
        let _ = app.handle_project_action(Action::ConfirmDelete);
        assert!(app.project_ui.dialog.is_none());
        // A repository refresh that listed the worktree before Git removed it.
        app.session
            .as_ref()
            .unwrap()
            .save_worktrees([WorktreeState {
                id,
                project_id: project,
                generation: 0,
                path: linked.path.clone(),
                branch: Some("feature".into()),
                existence: Existence::Open,
            }]);
        let _ = app.handle_project_action(Action::Deleted {
            capture: app.capture_worktree_removal(&target),
            target,
            result: Ok(()),
        });
        assert!(app.project_ui.deletions.is_empty());
        assert!(
            !app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .any(|worktree| worktree.id == id)
        );

        let session = app.session.as_ref().unwrap();
        let store = session.operation_store();
        session.flush(Duration::from_secs(5)).unwrap();
        let deleted = store.load_worktree(id).unwrap().unwrap();
        assert!(!deleted.existence.is_open());
        assert_eq!(deleted.project_id, project);
        assert_eq!(deleted.path, linked.path);
        assert_eq!(deleted.branch.as_deref(), Some("feature"));
    }

    /// A card filed under the worktree without running in it, such as a
    /// provider sign-in started from its strip, outlives the deletion and
    /// moves to the repository instead of under a row that is gone.
    #[test]
    fn a_card_filed_under_a_deleted_worktree_moves_to_its_repository() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let open = |app: &mut Cagents, directory: &str| {
            let _ = app.add_terminal(PathBuf::from(directory));
            let card = app.durable.cards.last().unwrap().id;
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(card, linked.path.clone());
            card
        };
        let sign_in = open(&mut app, "/home/reader");
        let inside = open(&mut app, "/repo/worktrees/feature/src");

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let target = app.project_ui.dialog.as_ref().unwrap().target.clone();
        let _ = app.handle_project_action(Action::ConfirmDelete);
        let _ = app.handle_project_action(Action::Deleted {
            capture: app.capture_worktree_removal(&target),
            target,
            result: Ok(()),
        });

        assert!(app.card_state(sign_in).unwrap().existence.is_open());
        assert_eq!(app.durable.card_scopes.get(&sign_in), Some(&repository));
        assert!(!app.card_state(inside).unwrap().existence.is_open());
        assert!(!app.durable.card_scopes.contains_key(&inside));
    }

    /// The confirmation goes the moment it is answered, and the worktree's
    /// row carries the deletion from there (#264): nothing starts in the
    /// worktree meanwhile, a failure stays on the row through a re-read of
    /// the checkout, the row runs it again, and declining it clears the row.
    #[test]
    fn a_deletion_runs_behind_its_row_and_a_failure_is_retried_from_there() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let delete = Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        };

        let _ = app.handle_project_action(delete.clone());
        let _ = app.handle_project_action(Action::ConfirmDelete);
        assert!(!app.modal_is_open());
        assert_eq!(row(&app, id).unwrap().deletion, Some(RowDeletion::Deleting));

        let cards = app.durable.cards.len();
        let _ = app.add_terminal(linked.path.join("src"));
        assert_eq!(app.durable.cards.len(), cards, "a terminal started in it");
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("That worktree is being deleted.")
        );
        assert!(!app.project_ui.deleting_directory(&repository));

        let target = app.project_ui.deletions[&id].target.clone();
        let _ = app.handle_project_action(Action::Deleted {
            capture: app.capture_worktree_removal(&target),
            target: target.clone(),
            result: Err("fatal: cannot remove".into()),
        });
        let failed = Some(RowDeletion::Failed("fatal: cannot remove".into()));
        assert_eq!(row(&app, id).unwrap().deletion, failed);
        assert!(!app.project_ui.deleting_directory(&linked.path));
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.ends_with("fatal: cannot remove"))
        );

        // A read of the checkout replaces the rows; the failure is the
        // application's, so it is put back on the new one.
        app.sidebar_model.projects[0].worktrees = vec![
            primary.clone(),
            SidebarWorktree::from_durable(id, linked.path.clone(), Some("feature".into())),
        ];
        app.sync_worktree_operation_rows();
        assert_eq!(row(&app, id).unwrap().deletion, failed);

        let _ = app.handle_project_action(Action::RetryDelete(id));
        assert_eq!(row(&app, id).unwrap().deletion, Some(RowDeletion::Deleting));
        let _ = app.handle_project_action(Action::Deleted {
            capture: app.capture_worktree_removal(&target),
            target,
            result: Err("fatal: cannot remove".into()),
        });

        // Reopened from the row, the dialog says why; cancelling declines it.
        let _ = app.handle_project_action(delete);
        assert_eq!(
            app.project_ui.dialog.as_ref().unwrap().failure.as_deref(),
            Some("fatal: cannot remove")
        );
        let _ = app.handle_project_action(Action::Dismiss);
        assert_eq!(row(&app, id).unwrap().deletion, None);
        assert!(app.project_ui.deletions.is_empty());
    }

    /// The window moves off a worktree whose deletion it confirms and cannot
    /// be brought back while Git works, by the row or by cycling workspaces,
    /// nor restarted underneath it. Wherever the user goes meanwhile, a
    /// channel included, is where the window stays when the deletion lands.
    #[test]
    fn the_window_leaves_a_worktree_being_deleted_and_stays_where_the_user_went() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let _ = app.select_worktree(linked.path.clone(), false);
        assert_eq!(app.active_workspace.as_ref(), Some(&linked.path));

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let _ = app.handle_project_action(Action::ConfirmDelete);
        assert_eq!(app.active_workspace.as_ref(), Some(&repository));
        assert!(
            app.window_update_blocker().is_some(),
            "an update would restart the window halfway through the deletion"
        );

        let open = SidebarAction::OpenWorktree {
            project_id: project,
            worktree_id: id,
            path: linked.path.clone(),
        };
        assert!(
            !crate::sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state)
                .contains(&open),
            "cycling workspaces lands on the worktree being deleted"
        );
        let sidebar = app.sidebar_state.clone();
        let _ = app.update(Message::Sidebar(open));
        assert_eq!(app.active_workspace.as_ref(), Some(&repository));
        assert_eq!(
            app.sidebar_state, sidebar,
            "the refused open still selected the row"
        );
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("That worktree is being deleted.")
        );

        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: project,
                title: "Investigation".into(),
                ordinal: 0,
                shared: false,
                cards: Vec::new(),
                focused: None,
            });
        let _ = app.select_channel(channel);
        let target = app.project_ui.deletions[&id].target.clone();
        let _ = app.handle_project_action(Action::Deleted {
            capture: app.capture_worktree_removal(&target),
            target,
            result: Ok(()),
        });
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(app.window_update_blocker().is_none());
    }

    /// A load that reached this repository under another project entry, with
    /// a listing taken before the deletion, lands after it. A load of the
    /// project begun since speaks for the repository instead.
    #[test]
    fn removing_a_parent_folder_preserves_child_project_cards_channels_and_selection() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().canonicalize().unwrap();
        let child_path = parent_path.join("child");
        std::fs::create_dir(&child_path).unwrap();
        let parent = ProjectId::new();
        let child = ProjectId::new();
        let mut app = super::super::tests::context_inspector_test_app(false);
        for (id, path) in [(parent, parent_path.clone()), (child, child_path.clone())] {
            app.sidebar_model.projects.push(
                SidebarProject::new(
                    id,
                    RepositoryId::new(),
                    "Folder".into(),
                    path,
                    Vec::new(),
                    Vec::new(),
                )
                .with_kind(ProjectKind::Folder),
            );
        }
        drop(app.add_terminal(parent_path.clone()));
        let parent_card = app.durable.cards.last().unwrap().id;
        drop(app.add_terminal(child_path.clone()));
        let child_card = app.durable.cards.last().unwrap().id;
        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: child,
                title: "Child".into(),
                ordinal: 0,
                shared: false,
                cards: vec![child_card],
                focused: Some(child_card),
            });
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        let CardKind::Terminal(session) = &mut app.card_state_mut(child_card).unwrap().kind else {
            unreachable!()
        };
        session.launch.working_directory = parent_path.clone();
        let child_session = session.id;
        assert!(
            app.project_session_ownership(parent)
                .protected
                .contains(&child_session),
            "explicit channel ownership outranks a launch directory"
        );
        let outside = directory.path().parent().unwrap().to_path_buf();
        let CardKind::Terminal(session) = &mut app.card_state_mut(parent_card).unwrap().kind else {
            unreachable!()
        };
        session.launch.working_directory = outside.clone();
        let parent_session = session.id;
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(parent_card, parent_path.clone());
        let scope = app.project_session_scope(parent);
        let ownership = app.project_session_ownership(parent);
        assert!(
            ownership.contains(parent_session, &outside, &scope),
            "an explicitly owned session outside the roots must close before its card retires"
        );
        assert!(!ownership.contains(child_session, &parent_path, &scope));
        app.active_workspace = Some(child_path.clone());
        app.project_ui.closing.insert(parent, parent_path.clone());
        assert!(!app.refuse_in_deleting_worktree(&child_path));
        assert!(!app.refuse_for_deleting_card(child_card));
        assert!(app.refuse_in_deleting_worktree(&parent_path));
        drop(app.finish_closed_project(parent, ProjectSessionClosure { scope, ownership }));
        assert!(!app.card_state(parent_card).unwrap().existence.is_open());
        assert!(app.card_state(child_card).unwrap().existence.is_open());
        assert!(app.sidebar_model.project(child).is_some());
        assert_eq!(app.durable.active_channel, Some(channel));
        assert_eq!(app.active_workspace, Some(child_path));
        assert!(app.durable.channels.iter().any(|entry| entry.id == channel));
    }

    #[test]
    fn project_retirement_uses_shared_closure_scope_when_child_row_is_not_loaded() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let parent = ProjectId::new();
        let root = PathBuf::from("/parent");
        let child = root.join("child");
        app.sidebar_model.projects.push(
            SidebarProject::new(
                parent,
                RepositoryId::new(),
                "Parent".into(),
                root.clone(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        drop(app.add_terminal(child.clone()));
        let card = app.durable.cards.last().unwrap().id;
        app.project_ui.closing.insert(parent, root.clone());
        let closure = ProjectSessionClosure {
            scope: ProjectSessionScope::new(vec![root], vec![child.clone()]),
            ownership: app.project_session_ownership(parent),
        };
        // The shared child was known to the close worker but its asynchronous
        // load has not supplied a local sidebar row, even at completion.
        drop(app.finish_closed_project(parent, closure));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.sidebar_model.project(parent).is_none());
    }

    #[test]
    fn session_ownership_defers_directories_to_shared_roots_and_preserves_foreign_views() {
        use crate::persistence::SavedSessionOwner;
        let parent = PathBuf::from("/projects");
        let child = parent.join("child");
        let scope = ProjectSessionScope::new(vec![parent.clone()], vec![child.clone()]);
        let outside = Path::new("/elsewhere");
        let id = TerminalId::new();
        let mut ownership = ProjectSessionOwnership::default();
        assert!(
            !ownership.contains(id, &child, &scope),
            "cwd must use shared exclusions even before the local sidebar adopts the child"
        );
        ownership.extend_saved(
            vec![(id, SavedSessionOwner::Directory(child.clone()))],
            &parent,
        );
        assert!(
            !ownership.contains(id, &parent, &scope),
            "saved directory scope must also use shared exclusions"
        );
        let owned = TerminalId::new();
        ownership.extend_saved(
            vec![(owned, SavedSessionOwner::Project(parent.clone()))],
            &parent,
        );
        assert!(
            ownership.contains(owned, outside, &scope),
            "explicit project ownership covers sessions launched outside roots"
        );
        ownership.extend_saved(vec![(owned, SavedSessionOwner::Project(child))], &parent);
        assert!(
            !ownership.contains(owned, outside, &scope),
            "a retained window's explicit ownership wins over another view"
        );
    }

    #[test]
    fn shared_removal_scope_preserves_nested_folders_and_linked_git_worktrees() {
        use crate::workspace_model::{ModelProject, WorkspaceModel};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let folder = root.join("folder");
        let child = folder.join("child");
        std::fs::create_dir_all(&child).unwrap();
        git(&root, &["init", "-b", "main", "outside"]);
        let git_root = root.join("outside");
        git(&git_root, &["commit", "--allow-empty", "-m", "initial"]);
        let linked = folder.join("linked");
        git(
            &git_root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let model = WorkspaceModel {
            projects: vec![
                ModelProject {
                    repository: child.clone(),
                    name: "Child folder".into(),
                    kind: ProjectKind::Folder,
                    ordinal: 1,
                },
                ModelProject {
                    repository: git_root.join(".git"),
                    name: "Other Git".into(),
                    kind: ProjectKind::Git,
                    ordinal: 2,
                },
            ],
            ..WorkspaceModel::default()
        };
        let scope =
            shared_project_session_scope(std::slice::from_ref(&folder), &folder, &model).unwrap();
        assert!(scope.contains(&folder.join("owned")));
        assert!(!scope.contains(&child.join("work")));
        assert!(!scope.contains(&linked));
        let child_scope = ProjectSessionScope::new(vec![child.clone()], vec![folder.clone()]);
        assert!(
            child_scope.contains(&child.join("work")),
            "removing a child still owns its scope beneath another project's root"
        );
        let mut missing = model;
        missing.projects[1].repository = root.join("missing.git");
        assert!(shared_project_session_scope(&[folder.clone()], &folder, &missing).is_err());
    }

    #[test]
    fn closing_a_folder_does_not_discover_git_worktrees_created_after_it_opened() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "-b", "main", "folder"]);
        let folder = root.join("folder");
        git(&folder, &["commit", "--allow-empty", "-m", "initial"]);
        let linked = root.join("linked");
        git(
            &folder,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let saved = vec![folder.clone()];
        assert_eq!(
            project_session_roots(&folder, ProjectKind::Folder, saved.clone()),
            saved
        );
        assert!(project_session_roots(&folder, ProjectKind::Git, saved).contains(&linked));
    }

    #[test]
    fn focus_regain_does_not_hide_a_new_worktree_opened_through_the_picker() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "-b", "main", "checkout"]);
        let checkout = root.join("checkout");
        git(&checkout, &["commit", "--allow-empty", "-m", "initial"]);
        let mut app = super::super::tests::context_inspector_test_app(false);
        let first =
            super::super::load_repository(&checkout, None, CancellationToken::default()).unwrap();
        let project = first.project.id;
        app.repository_generations.insert(project, 0);
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation: 0,
            result: Ok(Box::new(first)),
        }));
        drop(app.select_worktree(checkout.clone(), false));
        let linked = root.join("linked");
        git(
            &checkout,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let picked = ProjectId::new();
        drop(app.repository_load_task(linked.clone(), Some(picked), true));
        let generation = app.repository_generations[&picked];
        let listing =
            super::super::load_repository(&linked, Some(picked), CancellationToken::default())
                .unwrap();
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        assert_eq!(
            app.repository_generations[&project], 0,
            "focus must not start a newer canonical read"
        );
        drop(app.update(Message::RepositoryLoaded {
            open_cards: true,
            project_id: picked,
            generation,
            result: Ok(Box::new(listing)),
        }));
        assert_eq!(app.sidebar_model.projects.len(), 1);
        assert!(
            app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .any(|worktree| worktree.path == linked)
        );
        assert_eq!(app.active_workspace.as_ref(), Some(&linked));
        assert!(!app.refresh_ui.projects.contains_key(&picked));
    }

    #[test]
    fn a_listing_older_than_a_deletion_cannot_bring_the_worktree_back() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("checkout");
        let linked_path = root.join("linked");
        git(&root, &["init", "-b", "main", "checkout"]);
        git(&repository, &["commit", "--allow-empty", "-m", "initial"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                linked_path.to_str().unwrap(),
            ],
        );
        let mut app = super::super::tests::context_inspector_test_app(false);
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("test"),
        )
        .unwrap();
        app.session = Some(session);
        let load_as = |project| {
            super::super::load_repository(&repository, project, CancellationToken::default())
                .unwrap()
        };
        let load = || load_as(None);
        let loaded = |app: &mut Cagents, project_id, repository, open_cards| {
            let generation = app.repository_generations[&project_id];
            drop(app.update(Message::RepositoryLoaded {
                open_cards,
                project_id,
                generation,
                result: Ok(Box::new(repository)),
            }));
        };

        let first = load();
        let project = first.project.id;
        let _ = app.repository_load_task(repository.clone(), Some(project), false);
        loaded(&mut app, project, first, false);
        let id = app.sidebar_model.projects[0]
            .worktrees
            .iter()
            .find(|worktree| worktree.path == linked_path)
            .unwrap()
            .id;

        // The worktree's own folder is picked to be opened, and that open
        // lands while the worktree is being deleted.
        let picked = ProjectId::new();
        let _ = app.repository_load_task(linked_path.clone(), Some(picked), true);
        let picked_listing =
            super::super::load_repository(&linked_path, None, CancellationToken::default())
                .unwrap();

        // Re-adding the repository starts a load under a new entry, and its
        // listing is taken while the worktree still exists, alongside one
        // created outside the app that the project has not listed yet.
        let second_path = root.join("second");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "second",
                second_path.to_str().unwrap(),
            ],
        );
        let alias = ProjectId::new();
        let _ = app.repository_load_task(repository.clone(), Some(alias), false);
        let stale = load();

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let _ = app.handle_project_action(Action::ConfirmDelete);
        loaded(&mut app, picked, picked_listing, true);
        assert_ne!(app.active_workspace.as_ref(), Some(&linked_path));
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("That worktree is being deleted.")
        );
        git(
            &repository,
            &["worktree", "remove", linked_path.to_str().unwrap()],
        );
        let target = app.project_ui.deletions[&id].target.clone();
        let _ = app.handle_project_action(Action::Deleted {
            capture: app.capture_worktree_removal(&target),
            target,
            result: Ok(()),
        });
        // The deletion's follow-up load of the project has begun, so the
        // older listing lands superseded: it neither brings the worktree
        // back nor settles the row that follow-up is reloading.
        assert!(app.sidebar_model.projects[0].reloading);
        loaded(&mut app, alias, stale, false);
        assert!(
            row(&app, id).is_none(),
            "the stale listing re-added the row"
        );
        assert!(
            app.sidebar_model.projects[0].reloading,
            "the stale listing settled the follow-up load"
        );
        let session = app.session.as_ref().unwrap();
        session.flush(Duration::from_secs(5)).unwrap();
        assert!(
            !session
                .operation_store()
                .load_worktree(id)
                .unwrap()
                .unwrap()
                .existence
                .is_open(),
            "the stale listing reopened the catalog row"
        );
        // What that listing had that was new, the follow-up lists too.
        loaded(&mut app, project, load_as(Some(project)), false);
        assert!(row(&app, id).is_none());
        assert!(
            app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .any(|worktree| worktree.path == second_path)
        );

        // A worktree removed outside the app is caught the same way, and a
        // newer load's failure is not erased by the older success.
        let external = root.join("external");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "external",
                external.to_str().unwrap(),
            ],
        );
        let alias = ProjectId::new();
        let _ = app.repository_load_task(repository.clone(), Some(alias), false);
        let older = load();
        git(
            &repository,
            &["worktree", "remove", external.to_str().unwrap()],
        );
        let _ = app.repository_load_task(repository.clone(), Some(project), false);
        let generation = app.repository_generations[&project];
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation,
            result: Err("fixture: unreadable".into()),
        }));
        loaded(&mut app, alias, older, false);
        let row_state = &app.sidebar_model.projects[0];
        assert!(
            !row_state
                .worktrees
                .iter()
                .any(|worktree| worktree.path == external),
            "an older listing brought back a worktree removed outside the app"
        );
        assert!(
            row_state.unreachable.is_some(),
            "an older success erased a newer failure"
        );

        // A listing no load of the project has overtaken still adds what the
        // project has not listed yet.
        let later = root.join("later");
        git(
            &repository,
            &["worktree", "add", "-b", "later", later.to_str().unwrap()],
        );
        let alias = ProjectId::new();
        let _ = app.repository_load_task(repository.clone(), Some(alias), false);
        loaded(&mut app, alias, load(), false);
        let row_state = &app.sidebar_model.projects[0];
        assert!(
            row_state
                .worktrees
                .iter()
                .any(|worktree| worktree.path == later)
        );
        assert!(row_state.unreachable.is_none());
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// With the catalog no longer consulted, Git's listing is the only guard
    /// before the directory is removed. Each refusal leaves it in place. The
    /// removal itself is not exercised: it first closes sessions through the
    /// daemon, which a test must not reach.
    #[test]
    fn deletion_refuses_whatever_git_does_not_confirm_as_removable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("checkout");
        let linked = root.join("linked");
        git(&root, &["init", "-b", "main", "checkout"]);
        git(&repository, &["commit", "--allow-empty", "-m", "initial"]);
        git(
            &repository,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let records = RepositoryService::system("git")
            .list_worktrees(&repository, &CancellationToken::default())
            .unwrap();
        let id_of = |path: &Path| {
            records
                .iter()
                .find(|record| record.path == path)
                .unwrap()
                .id
        };
        let target = |worktree: Option<WorktreeId>, path: &Path| Target {
            project: ProjectId::new(),
            worktree,
            repository: repository.clone(),
            path: path.to_path_buf(),
            label: "linked".into(),
        };
        let linked_id = id_of(&linked);
        let refusal = |target: Target| delete_worktree(&target).unwrap_err();

        assert_eq!(
            refusal(target(None, &repository)),
            "The primary repository cannot be deleted."
        );
        assert_eq!(
            refusal(target(Some(id_of(&repository)), &repository)),
            "The primary worktree cannot be deleted."
        );
        assert_eq!(
            refusal(target(Some(WorktreeId::new()), &linked)),
            "Worktree changed or disappeared; refresh before deleting."
        );
        assert_eq!(
            refusal(target(Some(linked_id), &root.join("elsewhere"))),
            "Worktree changed or disappeared; refresh before deleting."
        );

        git(&repository, &["worktree", "lock", linked.to_str().unwrap()]);
        assert_eq!(
            refusal(target(Some(linked_id), &linked)),
            "The worktree is locked; unlock it explicitly in Git first."
        );
        assert!(linked.is_dir());

        // A bare repository lists itself first, as the primary it is.
        let bare = root.join("bare.git");
        git(&root, &["init", "--bare", "bare.git"]);
        let bare_id = RepositoryService::system("git")
            .list_worktrees(&bare, &CancellationToken::default())
            .unwrap()
            .into_iter()
            .find(|record| record.bare)
            .unwrap()
            .id;
        assert_eq!(
            delete_worktree(&Target {
                repository: bare.clone(),
                ..target(Some(bare_id), &bare)
            })
            .unwrap_err(),
            "The primary worktree cannot be deleted."
        );
        assert!(bare.is_dir());
    }

    #[test]
    fn dirty_worktree_confirmation_warns_without_blocking() {
        let repository = PathBuf::from("/repo");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let mut linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        linked.dirty = Some(crate::repository::DirtyState {
            unstaged: 1,
            untracked: 1,
            ..Default::default()
        });
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let editor = CardId::new();
        let mut runtime = RepositoryRuntime::dirty_fixture(editor);
        runtime.descriptor.worktree = linked.path.clone();
        app.cards
            .insert(editor, CardRuntime::Repository(Box::new(runtime)));
        drop(app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(linked.id),
            command: ProjectCommand::Delete,
        }));
        let dialog = app.project_ui.dialog.as_ref().unwrap();
        assert!(dialog.blocker.is_none());
        assert_eq!(
            app.worktree_deletion_warning(&dialog.target).as_deref(),
            Some(
                "Uncommitted changes and unsaved editor changes in this worktree will be discarded."
            )
        );
        drop(app.handle_project_action(Action::ConfirmDelete));
        assert!(app.project_ui.dialog.is_none());
        assert!(app.project_ui.is_deleting(linked.id));
    }

    #[test]
    fn confirmed_removal_discards_worktree_changes_and_retains_the_branch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("checkout");
        let linked = root.join("linked");
        git(&root, &["init", "-b", "main", "checkout"]);
        std::fs::write(repository.join("tracked.txt"), "initial\n").unwrap();
        git(&repository, &["add", "tracked.txt"]);
        git(&repository, &["commit", "-m", "initial"]);
        git(
            &repository,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::default();
        let record = service
            .list_worktrees(&repository, &cancellation)
            .unwrap()
            .into_iter()
            .find(|record| record.path == linked)
            .unwrap();
        let original_head = record.head_oid.clone();
        std::fs::write(linked.join("tracked.txt"), "staged\n").unwrap();
        git(&linked, &["add", "tracked.txt"]);
        std::fs::write(linked.join("tracked.txt"), "unstaged\n").unwrap();
        std::fs::write(linked.join("untracked.txt"), "untracked\n").unwrap();
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(record.id),
            repository: repository.clone(),
            path: linked.clone(),
            label: "feature".into(),
        };
        // Exercise the actual Git removal command without reaching a user's daemon.
        let command = worktree_removal_command(&target);
        SystemProcessExecutor
            .execute(&command, &cancellation)
            .unwrap()
            .require_success(&command.program)
            .unwrap();
        assert!(!linked.exists());
        assert!(
            !service
                .list_worktrees(&repository, &cancellation)
                .unwrap()
                .iter()
                .any(|record| record.id == target.worktree.unwrap())
        );
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", "feature"])
            .output()
            .unwrap();
        assert!(head.status.success());
        assert_eq!(
            String::from_utf8(head.stdout).unwrap().trim(),
            original_head.unwrap()
        );
        assert!(repository.join("tracked.txt").exists());
    }
}
