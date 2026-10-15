use super::*;
use crate::repository::{
    CommandSpec, ProcessExecutor, RemovedWorktree, SystemProcessExecutor, WorktreeSafetySnapshot,
};
use crate::worktree_control::BusyAgent;
use crate::{
    github::avatar::{self, OwnerKey},
    model::WorktreeId,
    sidebar::ProjectCommand,
};

#[derive(Debug, Clone)]
struct AutomaticAttempt {
    project: ProjectId,
    at: Instant,
    reason: Option<String>,
    local: bool,
}

/// Original UI owners only; a path receipt cannot retire later same-path intent.
#[derive(Debug, Clone)]
struct RemovalProjection {
    request: crate::model::OperationId,
    repository_generation: Option<u64>,
    device: Option<(String, String, String)>,
    target: Target,
    owners: Vec<RemovalCardOwner>,
}

#[derive(Debug, Clone)]
struct RemovalCardOwner {
    runtime: RemovalRuntimeOwner,
    card: CardState,
    scope: Option<PathBuf>,
    channels: Vec<ChannelId>,
    user_title: Option<String>,
}

/// Weak allocation identity never keeps a retired UI reader alive. This only
/// fences UI projection cleanup; it grants no process or filesystem authority.
#[derive(Clone)]
enum RemovalRuntimeOwner {
    Descriptor,
    Chat {
        target: chat_ui::Target,
        client: std::sync::Weak<crate::chat::client::ClientHandle>,
    },
    Terminal(crate::terminal::TerminalSurfaceIdentity),
}
impl std::fmt::Debug for RemovalRuntimeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Descriptor => "Descriptor",
            Self::Chat { .. } => "Chat",
            Self::Terminal(_) => "Terminal",
        })
    }
}

impl AutomaticAttempt {
    fn retry_due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= Duration::from_secs(30)
    }
}

fn cleanup_kept_detail(reason: &str) -> String {
    reason
        .trim()
        .strip_prefix("Kept:")
        .unwrap_or(reason.trim())
        .trim()
        .to_owned()
}

#[derive(Debug, Default)]
pub struct State {
    owners: HashMap<OwnerKey, Option<iced::advanced::image::Handle>>,
    projects: HashMap<ProjectId, OwnerKey>,
    dialog: Option<Dialog>,
    cleanup_dialog: Option<CleanupPolicyDialog>,
    pending_delete: VecDeque<Target>,
    next_safety_request: u64,
    deletion_authorizations: HashMap<WorktreeId, Arc<WorktreeSafetySnapshot>>,
    deletion_stop_authorizations: HashMap<WorktreeId, StopAuthorization>,
    deletion_remove_authorizations: HashMap<WorktreeId, RemoveContinuation>,
    deletion_busy_agents: HashMap<WorktreeId, AgentCensus>,
    branch_deletions: HashMap<WorktreeId, BranchDeletion>,
    released_delete_protections: HashSet<WorktreeId>,
    deletion_warnings: HashMap<WorktreeId, Vec<String>>,
    branch_requests: HashSet<crate::model::OperationId>,
    force_branches: VecDeque<(BranchDeletion, String)>,
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
    deletion_projections: HashMap<WorktreeId, (Instant, RemovalProjection)>,
    automatic_attempts: HashMap<WorktreeId, AutomaticAttempt>,
    automatic_in_flight: HashMap<WorktreeId, RemovalProjection>,
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

    /// Exact opening identity for confirmation and the shared modal owner.
    pub(super) fn removal_request(&self) -> Option<crate::model::OperationId> {
        self.remove_dialog.as_ref().map(|removal| removal.request)
    }

    pub(super) fn removal_request_for(
        &self,
        project: ProjectId,
    ) -> Option<crate::model::OperationId> {
        self.remove_dialog
            .as_ref()
            .filter(|removal| removal.project == project)
            .map(|removal| removal.request)
    }

    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
            || self.remove_dialog.is_some()
            || self.cleanup_dialog.is_some()
            || !self.force_branches.is_empty()
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
        if let Some((request, _)) = self.force_branches.front() {
            return force_branch_control_id(request.id, "keep");
        }
        if let Some(dialog) = &self.cleanup_dialog {
            return widget::Id::new(if dialog.loading {
                "dialog.project.cleanup.cancel"
            } else {
                "dialog.project.cleanup.directories"
            });
        }
        if let Some(removal) = &self.remove_dialog {
            return removal_control_id(removal.request, "cancel");
        }
        if let Some(dialog) = &self.dialog {
            return deletion_control_id(dialog.request, "cancel");
        }
        widget::Id::new("dialog.project.close")
    }
}

fn removal_control_id(request: crate::model::OperationId, role: &str) -> widget::Id {
    widget::Id::new(format!("dialog.project.remove.{role}.{request}"))
}

fn deletion_control_id(request: u64, role: &str) -> widget::Id {
    widget::Id::new(format!("dialog.worktree.delete.{role}.{request}"))
}

fn force_branch_control_id(request: crate::model::OperationId, role: &str) -> widget::Id {
    widget::Id::new(format!("dialog.branch.{role}.{request}"))
}

#[derive(Debug, Clone)]
struct ProjectRemoval {
    request: crate::model::OperationId,
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
    safety: Option<Arc<WorktreeSafetySnapshot>>,
    stop_authorization: Option<StopAuthorization>,
    expected_head: Option<DeletionHead>,
    loading: bool,
    request: u64,
    options_loading: bool,
    branch_elsewhere: bool,
    delete_branch: bool,
    branch_choice_edited: bool,
    busy_agents: Vec<BusyAgent>,
    session_census: Vec<BusyAgent>,
    /// Explicit consent covers the complete observed roster, including idle
    /// sessions and all work they may admit before the guarded stop takes it.
    busy_confirmed: Option<Vec<BusyAgent>>,
    stale_lock: bool,
}

/// A post-merge cleanup offer is bound to the exact reviewed source and
/// head, never only to a path-derived worktree ID or a generic merged badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeletionHead {
    pub head_repository: GitHubRepoKey,
    /// Exact local branch name, without adding/removing a refs prefix.
    pub branch: String,
    pub head_oid: String,
    /// A verified post-merge offer carries its actual merged PR metadata, so
    /// the branch default does not depend on the sidebar cache refreshing.
    /// An expected head alone grants no merged-state authority.
    pub merged_pull_request: Option<Arc<crate::github::PullRequest>>,
}

impl DeletionHead {
    fn merged_receipt_matches(&self, snapshot: &WorktreeSafetySnapshot) -> bool {
        self.merged_pull_request.as_ref().is_some_and(|pr| {
            pr.state.0 == "MERGED"
                && pr.head_ref_name == self.branch
                && pr.head_ref_oid == self.head_oid
                && pr.belongs_to_head(&self.head_repository)
                && snapshot.branch.as_deref() == Some(self.branch.as_str())
                && snapshot.head_oid.as_deref() == Some(self.head_oid.as_str())
                && snapshot
                    .head_repository
                    .as_ref()
                    .is_some_and(|source| source.same_repository(&self.head_repository))
        })
    }
}

#[derive(Debug, Clone)]
pub struct BranchDeletion {
    id: crate::model::OperationId,
    repository: PathBuf,
    branch: String,
    expected_oid: String,
    warnings: Vec<String>,
    continuation: Option<BranchContinuation>,
}

#[derive(Debug, Clone)]
pub struct DeleteOptions {
    branch_elsewhere: bool,
    stale_lock: bool,
    lock_error: Option<String>,
}

#[derive(Debug)]
struct CleanupPolicyDialog {
    repository: PathBuf,
    name: String,
    directories: String,
    loading: bool,
    error: Option<String>,
    request: u64,
}

impl Dialog {
    fn sessions_consented(&self) -> bool {
        self.session_census.is_empty() || self.busy_confirmed.as_ref() == Some(&self.session_census)
    }

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
    ConfirmDelete(u64),
    DismissDelete(u64),
    DeleteOptionsRead {
        request: u64,
        result: Result<DeleteOptions, String>,
    },
    DeleteBranchChanged(u64, bool),
    BranchDeleted {
        request: BranchDeletion,
        result: Result<(), crate::worktree_branch::BranchDeleteFailure>,
    },
    ForceDeleteBranch(crate::model::OperationId),
    KeepBranch(crate::model::OperationId),
    BusyAgentsConfirmed(u64, bool),
    StaleUnlocked {
        request: u64,
        result: Result<(), String>,
    },
    SafetyRead {
        request: u64,
        confirming: bool,
        result: Result<SafetyInspection, String>,
    },
    OpenCleanupPolicy(ProjectId),
    CleanupPolicyRead {
        request: u64,
        result: Result<Vec<String>, String>,
    },
    CleanupPolicyChanged(u64, String),
    SaveCleanupPolicy(u64),
    CleanupPolicySaved {
        request: u64,
        result: Result<(), String>,
    },
    ConfirmRemove(crate::model::OperationId),
    DismissRemove(crate::model::OperationId),
    RemovedProject {
        repository: PathBuf,
        result: Result<crate::workspace_model::WorkspaceModel, String>,
    },
    AwaitClosedProject(ProjectId, u64),
    ProjectSessionsClosed(ProjectId, u64, Result<ProjectSessionClosure, String>),
    /// Runs a failed deletion again. The user confirmed it once already.
    RetryDelete(WorktreeId),
    AwaitSessions(Target),
    DeletionSessionsClosed {
        target: Target,
        started_at: Instant,
        result: Result<Option<RemoveContinuation>, String>,
    },
    DeletionProtectionsReleased {
        target: Target,
        started_at: Instant,
        result: Result<(), String>,
    },
    RemovalCompleted {
        target: Target,
        started_at: Instant,
        result: Result<RemovalOutcome, String>,
    },
    AutomaticallyDeleted {
        warnings: Vec<String>,
        target: Target,
        request: crate::model::OperationId,
        result: Result<RemovedWorktree, String>,
    },
    Deleted {
        target: Target,
        result: Result<(), String>,
    },
    AvatarLoaded {
        owner: OwnerKey,
        image: Option<iced::advanced::image::Handle>,
    },
    Finished(Result<(), String>),
    Dismiss,
}

impl Action {
    fn user_input(&self) -> bool {
        matches!(
            self,
            Self::Command { .. }
                | Self::ConfirmDelete(_)
                | Self::DismissDelete(_)
                | Self::DeleteBranchChanged(..)
                | Self::ForceDeleteBranch(_)
                | Self::KeepBranch(_)
                | Self::BusyAgentsConfirmed(..)
                | Self::OpenCleanupPolicy(_)
                | Self::CleanupPolicyChanged(..)
                | Self::SaveCleanupPolicy(_)
                | Self::ConfirmRemove(_)
                | Self::DismissRemove(_)
                | Self::RetryDelete(_)
                | Self::Dismiss
        )
    }
}

impl Cagents {
    #[cfg(test)]
    pub(super) fn confirm_worktree_delete_fixture(&mut self) {
        tests::confirm_deletion_fixture(self);
    }

    pub(super) fn sync_worktree_protections(&mut self) {
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
        paths.retain(|path| {
            !self.project_ui.deletions.iter().any(|(id, deletion)| {
                self.project_ui.released_delete_protections.contains(id)
                    && path.starts_with(&deletion.target.path)
            })
        });
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
            || !self.project_ui.automatic_in_flight.is_empty()
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
        let mut candidates = Vec::new();
        for (record, _) in records {
            let source = self
                .sidebar_model
                .project(project_id)
                .and_then(|project| {
                    project
                        .worktrees
                        .iter()
                        .find(|row| row.id == record.id && row.path == record.path)
                })
                .and_then(|row| row.head_repository.clone());
            let Some(source) = source else {
                continue;
            };
            let merged = matching_attached_pr(record, &source, pull_requests).filter(|pr| {
                pr.state.0 == "MERGED"
                    && record.head_oid.as_deref() == Some(pr.head_ref_oid.as_str())
            });
            let Some(pr) = merged else {
                self.project_ui.automatic_attempts.remove(&record.id);
                continue;
            };
            let Some(target) = self.project_target(project_id, Some(record.id)) else {
                continue;
            };
            if let Some(reason) = self.automatic_worktree_keep_reason(&target, record) {
                let unchanged = self
                    .project_ui
                    .automatic_attempts
                    .get(&record.id)
                    .is_some_and(|attempt| {
                        attempt.local && attempt.reason.as_deref() == Some(reason.as_str())
                    });
                if !unchanged {
                    self.project_ui.automatic_attempts.insert(
                        record.id,
                        AutomaticAttempt {
                            project: project_id,
                            at: Instant::now(),
                            reason: Some(reason),
                            local: true,
                        },
                    );
                }
                continue;
            }
            // A local pane/session closing is a relevant event: do not make
            // that worktree wait for its old local refusal's timer as well.
            if self
                .project_ui
                .automatic_attempts
                .get(&record.id)
                .is_some_and(|attempt| attempt.local)
            {
                self.project_ui.automatic_attempts.remove(&record.id);
            }
            if self
                .project_ui
                .automatic_attempts
                .get(&record.id)
                .is_some_and(|attempt| !attempt.retry_due(Instant::now()))
            {
                continue;
            }
            let Some(branch) = record
                .branch
                .as_deref()
                .and_then(|branch| branch.strip_prefix("refs/heads/"))
            else {
                continue;
            };
            let Some(head_oid) = record.head_oid.clone() else {
                continue;
            };
            candidates.push((
                target,
                crate::worktree_cleanup::RemovalRequest {
                    repository: self
                        .sidebar_model
                        .project(project_id)
                        .unwrap()
                        .repository
                        .clone(),
                    path: record.path.clone(),
                    branch: branch.to_owned(),
                    head_oid,
                    source_repository: source,
                    pull_request: crate::github::PullRequestKey {
                        repository: repository.clone(),
                        number: pr.number,
                    },
                },
            ));
        }
        // A declined worktree cannot monopolize the queue ahead of unseen ones.
        let candidate = candidates.into_iter().min_by_key(|(target, _)| {
            target.worktree.and_then(|id| {
                self.project_ui
                    .automatic_attempts
                    .get(&id)
                    .map(|attempt| attempt.at)
            })
        });
        let Some((target, request)) = candidate else {
            self.sync_worktree_deletion_rows();
            return Task::none();
        };
        let id = target.worktree.unwrap();
        self.project_ui
            .automatic_attempts
            .entry(id)
            .and_modify(|attempt| attempt.at = Instant::now())
            .or_insert(AutomaticAttempt {
                project: project_id,
                at: Instant::now(),
                reason: None,
                local: false,
            });
        // Checking with the daemon is not a deletion. Keep the last quiet
        // refusal on the row while it checks; accepted removals disappear.
        let custody = self.capture_removal_projections(target.clone());
        let request_id = custody.request;
        self.project_ui.automatic_in_flight.insert(id, custody);
        self.sync_worktree_deletion_rows();
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        let device = stop_device_key();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let retirements = creation_retirements_for_window(
                    store,
                    &request.repository,
                    &request.path,
                    &device,
                );
                if device != stop_device_key() {
                    return Err("The selected device changed before cleanup.".into());
                }
                let reply = crate::device_service::request(
                    crate::device_service::Operation::RemoveMergedWorktree { request },
                )
                .map_err(|error| error.to_string())?;
                let crate::device_service::Reply::MergedWorktreeRemoved(result) = reply else {
                    return Err("The daemon returned an unexpected cleanup response".into());
                };
                let removed = result.map_err(|refusal| refusal.to_string())?;
                let warnings = if device == stop_device_key() {
                    finish_creation_retirements(retirements, &removed)
                } else {
                    creation_retirement_warning(
                        "the selected device changed while cleanup completed".into(),
                    )
                };
                Ok((removed, warnings))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            let (result, warnings) = match result {
                Ok((removed, warnings)) => (Ok(removed), warnings),
                Err(error) => (Err(error), Vec::new()),
            };
            Message::ProjectUi(Action::AutomaticallyDeleted {
                target,
                request: request_id,
                result,
                warnings,
            })
        })
    }

    fn capture_removal_projections(&self, target: Target) -> RemovalProjection {
        let channels = self.removal_channel_owners();
        let owners = self
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
            .map(|card| RemovalCardOwner {
                card: card.clone(),
                runtime: match self.cards.get(&card.id) {
                    Some(CardRuntime::Chat(runtime)) => runtime
                        .handle()
                        .map(|client| RemovalRuntimeOwner::Chat {
                            target: runtime.target,
                            client: Arc::downgrade(client),
                        })
                        .unwrap_or(RemovalRuntimeOwner::Descriptor),
                    Some(CardRuntime::Terminal(surface)) => {
                        RemovalRuntimeOwner::Terminal(surface.client_identity())
                    }
                    _ => RemovalRuntimeOwner::Descriptor,
                },
                scope: self.durable.card_scopes.get(&card.id).cloned(),
                channels: channels.get(&card.id).cloned().unwrap_or_default(),
                user_title: self.chrome_ui.user_titles.get(&card.id).cloned(),
            })
            .collect();
        RemovalProjection {
            request: crate::model::OperationId::new(),
            repository_generation: self.repository_generations.get(&target.project).copied(),
            device: stop_device_key(),
            target,
            owners,
        }
    }

    fn complete_automatic_cleanup(&mut self, completed: RemovalProjection) -> Task<Message> {
        let task = self.complete_removal_projections(completed);
        self.status = Some(TransientNotice::success(
            "Worktree removed automatically. Refreshing its repository inventory.",
        ));
        task
    }

    fn complete_removal_projections(&mut self, completed: RemovalProjection) -> Task<Message> {
        let target = completed.target;
        // A Repository descriptor can retain its card/path through checkout
        // replacement. New inventory invalidates path-only custody; exact live
        // client/proof custody survives this removal's own inventory edge.
        let same_device = completed.device == stop_device_key();
        if !same_device {
            return Task::none();
        }
        let same_inventory = same_device
            && self.repository_generations.get(&target.project).copied()
                == completed.repository_generation;
        let channels = self.removal_channel_owners();
        let current = completed.owners.into_iter().filter(|owner| {
            (same_inventory || !matches!(owner.runtime, RemovalRuntimeOwner::Descriptor))
                && self.removal_owner_is_current(owner)
                && channels.get(&owner.card.id).map(Vec::as_slice).unwrap_or_default() == owner.channels.as_slice()
                && self.durable.card_scopes.get(&owner.card.id) == owner.scope.as_ref()
                && self.chrome_ui.user_titles.get(&owner.card.id) == owner.user_title.as_ref()
                && !matches!(self.cards.get(&owner.card.id), Some(CardRuntime::Repository(runtime))
                    if runtime.selected_path().is_some() || runtime.blocks_navigation())
        }).collect::<Vec<_>>();
        let mut updates = Vec::new();
        for owner in current {
            let id = owner.card.id;
            if card_belongs_to(&owner.card, &target.path) {
                // Another close owns its own handoff. Preserve this projection
                // rather than steal or cancel that operation's native focus.
                if self.workspace.card(id).is_some() {
                    if self.workspace.pending_close().is_some() {
                        continue;
                    }
                    if self.workspace.request_close(id).is_err() {
                        continue;
                    }
                    match self.workspace.commit_close(id) {
                        Ok(update) => updates.push(update),
                        Err(_) => {
                            let _ = self.workspace.cancel_close(id);
                            continue;
                        }
                    }
                }
                self.retire_lsp_card(id);
                self.sidebar_state.clear_agent_activity(id);
                self.sidebar_state.acknowledge_agent_update(id);
                self.chat_discard(id);
                self.cards.remove(&id);
                self.lifecycle.remove(id);
                if let Some(card) = self.card_state_mut(id) {
                    card.existence = removed_existence("worktree removed");
                }
                let durable = Arc::make_mut(&mut self.durable);
                durable.card_scopes.remove(&id);
                for channel in &mut durable.channels {
                    channel.cards.retain(|card| *card != id);
                    if channel.focused == Some(id) {
                        channel.focused = channel.cards.first().copied();
                    }
                }
                if durable.focused == Some(id) {
                    durable.focused = self.workspace.focused();
                }
            } else if owner
                .scope
                .as_ref()
                .is_some_and(|scope| scope.starts_with(&target.path))
            {
                Arc::make_mut(&mut self.durable)
                    .card_scopes
                    .insert(id, target.repository.clone());
            }
        }
        // No path-wide row/tombstone, creation, filing, camera or focus cleanup:
        // the original removal is not authority over a newer same-path checkout.
        self.refresh_sidebar_channels();
        self.refresh_sidebar_worktree_panes();
        self.persist();
        let mut tasks = updates
            .into_iter()
            .map(|update| self.apply_viewport(update, true, false))
            .collect::<Vec<_>>();
        if same_device
            && self
                .sidebar_model
                .project(target.project)
                .is_some_and(|project| project.repository == target.repository)
            && !self.project_ui.closing.contains_key(&target.project)
            && !self.project_ui.closed.contains(&target.repository)
        {
            tasks.push(self.repository_load_task(target.repository, Some(target.project), false));
        }
        tasks.push(self.lifecycle_sweep());
        tasks.push(self.sync_chrome());
        Task::batch(tasks)
    }

    fn removal_channel_owners(&self) -> HashMap<CardId, Vec<ChannelId>> {
        let mut owners = HashMap::<CardId, Vec<ChannelId>>::new();
        for channel in &self.durable.channels {
            for &card in &channel.cards {
                owners.entry(card).or_default().push(channel.id);
            }
        }
        owners
    }

    fn removal_owner_is_current(&self, owner: &RemovalCardOwner) -> bool {
        let Some(current) = self.card_state(owner.card.id) else {
            return false;
        };
        if !current.existence.is_open() || current.generation != owner.card.generation {
            return false;
        }
        match (
            &owner.runtime,
            &owner.card.kind,
            &current.kind,
            self.cards.get(&current.id),
        ) {
            (
                RemovalRuntimeOwner::Chat { target, client },
                CardKind::Chat(before),
                CardKind::Chat(after),
                Some(CardRuntime::Chat(runtime)),
            ) => {
                runtime.target == *target
                    && runtime
                        .handle()
                        .is_some_and(|handle| client.ptr_eq(&Arc::downgrade(handle)))
                    && before.id == after.id
                    && before
                        .proof
                        .as_ref()
                        .is_some_and(crate::model::HostSessionProof::is_complete)
                    && before.proof == after.proof
                    && before.launch.provider == after.launch.provider
                    && before.launch.recipe == after.launch.recipe
                    && runtime.session.id == after.id
                    && runtime.session.proof == after.proof
                    && runtime.session.launch.provider == after.launch.provider
                    && runtime.session.launch.recipe == after.launch.recipe
            }
            (
                RemovalRuntimeOwner::Terminal(client),
                CardKind::Terminal(before),
                CardKind::Terminal(after),
                Some(CardRuntime::Terminal(surface)),
            ) => {
                client.matches(surface)
                    && before.id == after.id
                    && before.launch == after.launch
                    && surface.terminal_id() == after.id
                    && surface
                        .host_proof()
                        .as_ref()
                        .is_some_and(crate::model::HostSessionProof::is_complete)
                    && before.continuity.live_proof() == surface.host_proof()
                    && after.continuity.live_proof() == surface.host_proof()
            }
            (RemovalRuntimeOwner::Descriptor, _, _, runtime) => {
                current == &owner.card
                    && (!matches!(current.kind, CardKind::Chat(_) | CardKind::Terminal(_))
                        || matches!(runtime, None | Some(CardRuntime::Loading)))
            }
            _ => false,
        }
    }

    fn automatic_worktree_keep_reason(
        &self,
        target: &Target,
        record: &WorktreeRecord,
    ) -> Option<String> {
        if record.lock.locked {
            return Some("worktree locked".into());
        }
        let sessions = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && card_belongs_to(card, &target.path))
            .collect::<Vec<_>>();
        if sessions.iter().any(|card| {
            self.sidebar_state
                .card_activity(card.id)
                .is_some_and(|activity| activity.state == crate::agent::ActivityState::Working)
        }) {
            return Some("agent running".into());
        }
        if sessions
            .iter()
            .any(|card| matches!(&card.kind, CardKind::Chat(_) | CardKind::Terminal(_)))
        {
            return Some("session open in this window".into());
        }
        if sessions.iter().any(|card| match &card.kind {
            CardKind::Repository(view) => view.selected_file.is_some()
                || self.durable.repository_navigation.get(&card.id).is_some_and(|navigation| navigation.selected_file.is_some())
                || matches!(self.cards.get(&card.id), Some(CardRuntime::Repository(runtime)) if runtime.selected_path().is_some() || runtime.blocks_navigation()),
            _ => false,
        }) { return Some("editor open in this window".into()); }
        if self
            .active_workspace
            .as_ref()
            .is_some_and(|active| active.starts_with(&target.path))
        {
            return Some("open in this window".into());
        }
        self.worktree_deletion_blocker(target)
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
        self.worktree_deletion_blocker_ignoring_lock(&target, true)
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
        if self.project_ui.dialog.is_some() {
            self.project_ui.pending_delete.push_back(target);
            Task::none()
        } else {
            self.open_worktree_delete_dialog(target)
        }
    }

    pub(super) fn open_worktree_delete_dialog_for_head(
        &mut self,
        project: ProjectId,
        worktree: WorktreeId,
        expected: DeletionHead,
    ) -> Task<Message> {
        if self.modal_is_open() || self.update_prepared {
            return Task::none();
        }
        let target = self.project_target(project, Some(worktree));
        if crate::repository::ValidatedBranchName::parse(expected.branch.clone()).is_err()
            || crate::repository::BaseRevision::object_id(expected.head_oid.clone()).is_err()
            || target
                .as_ref()
                .is_none_or(|target| !self.deletion_head_matches_row(target, &expected))
        {
            self.status = Some(TransientNotice::warning(
                "The merged pull request no longer matches this worktree. Refresh its current branch before offering cleanup.",
            ));
            return Task::none();
        }
        let task = self.open_worktree_delete_dialog(target.expect("target checked"));
        if let Some(dialog) = &mut self.project_ui.dialog {
            dialog.expected_head = Some(expected);
        }
        task
    }

    fn deletion_head_matches_row(&self, target: &Target, expected: &DeletionHead) -> bool {
        self.sidebar_model
            .project(target.project)
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|row| Some(row.id) == target.worktree && row.path == target.path)
            })
            .is_some_and(|row| {
                row.branch.as_deref() == Some(expected.branch.as_str())
                    && row
                        .head_repository
                        .as_ref()
                        .is_some_and(|source| source.same_repository(&expected.head_repository))
            })
    }

    fn open_worktree_delete_dialog(&mut self, target: Target) -> Task<Message> {
        self.project_ui.cleanup_dialog = None;
        self.project_ui.remove_dialog = None;
        self.project_ui.next_safety_request += 1;
        let request = self.project_ui.next_safety_request;
        let blocker = self.worktree_deletion_blocker_ignoring_lock(&target, true);
        let failure = target
            .worktree
            .and_then(|id| self.project_ui.deletions.get(&id))
            .and_then(|deletion| deletion.failure.clone());
        let loading = blocker.is_none();
        self.project_ui.dialog = Some(Dialog {
            target: target.clone(),
            blocker,
            failure,
            safety: None,
            stop_authorization: None,
            expected_head: None,
            loading,
            request,
            options_loading: loading,
            branch_elsewhere: false,
            delete_branch: false,
            branch_choice_edited: false,
            busy_agents: Vec::new(),
            session_census: Vec::new(),
            busy_confirmed: None,
            stale_lock: false,
        });
        let options =
            self.session.as_ref().filter(|_| loading).map(|session| {
                delete_options_task(target.clone(), request, session.operation_store())
            });
        let read = if loading {
            safety_read_task(target, request, false, None)
        } else {
            Task::none()
        };
        Task::batch([
            read,
            options.unwrap_or_else(Task::none),
            iced::widget::operation::focus(deletion_control_id(request, "cancel"))
                .map(cosmic::Action::App),
        ])
    }

    fn next_worktree_delete_dialog(&mut self) -> Task<Message> {
        if let Some(target) = self.project_ui.pending_delete.pop_front() {
            self.open_worktree_delete_dialog(target)
        } else {
            self.project_dialog_closed()
        }
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
            self.sidebar_state.clear_agent_activity(card);
            self.sidebar_state.acknowledge_agent_update(card);
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

    pub(super) fn open_file_in_editor(
        &mut self,
        path: PathBuf,
        editor_id: String,
    ) -> Task<Message> {
        if self.modal_is_open() {
            return Task::none();
        }
        self.launch_editor(path, &editor_id)
    }

    fn launch_editor(&mut self, path: PathBuf, editor_id: &str) -> Task<Message> {
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
            let result =
                tokio::task::spawn_blocking(move || crate::external_editor::launch(&editor, &path))
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

    fn project_dialog_on_top(&self) -> bool {
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() {
            return false;
        }
        self.project_ui.modal_open()
            && !self.update_prepared
            && self.update_ui.confirmation.is_none()
            && !self.session_recovery.open()
            && !self.daemon_health_ui.confirmation
            && !self.automation_approval_open()
            && self.transient_work_close.is_none()
            && !self.automation_ui.draft_dialog_open()
            && !self.device_ui.open
            && self.chat_ui.image_viewer.is_none()
            && self.terminal_paste.is_none()
            && !self.terminal_close_dialog_visible()
            && !self.settings_reset_confirm
            && self.sidebar_state.pending_discard().is_none()
            && self.dirty_repository_close.is_none()
            && self.sidebar_selection_ui.dialog.is_none()
    }

    fn worktree_delete_dialog_on_top(&self) -> bool {
        self.project_dialog_on_top()
            && self.project_ui.force_branches.is_empty()
            && self.project_ui.cleanup_dialog.is_none()
            && self.project_ui.remove_dialog.is_none()
            && self.project_ui.dialog.is_some()
    }

    pub(super) fn handle_project_action(&mut self, action: Action) -> Task<Message> {
        let hidden_control = match &action {
            Action::ForceDeleteBranch(_) | Action::KeepBranch(_) => {
                self.project_ui.force_branches.is_empty()
            }
            Action::CleanupPolicyChanged(..) | Action::SaveCleanupPolicy(_) => {
                !self.project_ui.force_branches.is_empty()
                    || self.project_ui.cleanup_dialog.is_none()
            }
            Action::ConfirmDelete(_)
            | Action::DeleteBranchChanged(..)
            | Action::BusyAgentsConfirmed(..) => !self.worktree_delete_dialog_on_top(),
            Action::DismissDelete(request) => {
                !self.worktree_delete_dialog_on_top()
                    || self.project_ui.dialog.as_ref().map(|dialog| dialog.request) != Some(*request)
            }
            Action::ConfirmRemove(request) | Action::DismissRemove(request) => {
                self.project_ui.removal_request() != Some(*request)
                    || !self.project_ui.force_branches.is_empty()
                    || self.project_ui.cleanup_dialog.is_some()
                    || self.project_ui.dialog.is_some()
            }
            Action::Dismiss => {
                !self.project_ui.force_branches.is_empty()
                    || ((self.project_ui.remove_dialog.is_some() || self.project_ui.dialog.is_some())
                        && self.project_ui.cleanup_dialog.is_none())
            }
            _ => false,
        };
        if hidden_control {
            return Task::none();
        }
        if action.user_input()
            && (self.update_prepared || (self.modal_is_open() && !self.project_dialog_on_top()))
        {
            return Task::none();
        }
        match action {
            Action::OpenCleanupPolicy(project) => {
                if self.modal_is_open() {
                    return Task::none();
                }
                let Some(project) = self
                    .sidebar_model
                    .project(project)
                    .filter(|project| project.kind == ProjectKind::Git)
                else {
                    return Task::none();
                };
                self.project_ui.next_safety_request += 1;
                let request = self.project_ui.next_safety_request;
                let repository = project.repository.clone();
                self.project_ui.dialog = None;
                self.project_ui.remove_dialog = None;
                self.project_ui.cleanup_dialog = Some(CleanupPolicyDialog {
                    repository: repository.clone(),
                    name: project.name.clone(),
                    directories: String::new(),
                    loading: true,
                    error: None,
                    request,
                });
                let read = cosmic::task::future(async move {
                    let result =
                        tokio::task::spawn_blocking(move || read_cleanup_policy(&repository))
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result);
                    Message::ProjectUi(Action::CleanupPolicyRead { request, result })
                });
                return Task::batch([
                    read,
                    iced::widget::operation::focus(self.project_ui.focus_target())
                        .map(cosmic::Action::App),
                ]);
            }
            Action::CleanupPolicyRead { request, result } => {
                let Some(dialog) = self
                    .project_ui
                    .cleanup_dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request)
                else {
                    return Task::none();
                };
                dialog.loading = false;
                match result {
                    Ok(directories) => {
                        dialog.directories = format_disposable_directories(&directories)
                    }
                    Err(error) => {
                        dialog.error =
                            Some(format!("Could not read project cleanup policy: {error}"))
                    }
                }
                return iced::widget::operation::focus(self.project_ui.focus_target())
                    .map(cosmic::Action::App);
            }
            Action::CleanupPolicyChanged(request, value) => {
                if let Some(dialog) = self
                    .project_ui
                    .cleanup_dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request && !dialog.loading)
                {
                    dialog.directories = value;
                    dialog.error = None;
                }
            }
            Action::SaveCleanupPolicy(request) => {
                let Some(dialog) = self
                    .project_ui
                    .cleanup_dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request && !dialog.loading)
                else {
                    return Task::none();
                };
                let directories = match parse_disposable_directories(&dialog.directories) {
                    Ok(directories) => directories,
                    Err(error) => {
                        dialog.error = Some(error);
                        return Task::none();
                    }
                };
                dialog.loading = true;
                dialog.error = None;
                let repository = dialog.repository.clone();
                let request = dialog.request;
                let save = cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let reply = crate::device_service::request(crate::device_service::Operation::Worktree {
                            operation: crate::worktree_control::Operation::SetDisposableIgnoredDirectories {
                                path: repository.clone(), repository, directories,
                            },
                        })?;
                        disposable_ignored_directories(reply).map(|_| ())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::ProjectUi(Action::CleanupPolicySaved { request, result })
                });
                return Task::batch([
                    save,
                    iced::widget::operation::focus(self.project_ui.focus_target())
                        .map(cosmic::Action::App),
                ]);
            }
            Action::CleanupPolicySaved { request, result } => {
                let Some(dialog) = self
                    .project_ui
                    .cleanup_dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request)
                else {
                    return Task::none();
                };
                match result {
                    Ok(()) => {
                        self.project_ui.cleanup_dialog = None;
                        self.status =
                            Some(TransientNotice::success("Project cleanup policy saved."));
                        return self.project_dialog_closed();
                    }
                    Err(error) => {
                        dialog.loading = false;
                        dialog.error =
                            Some(format!("Could not save project cleanup policy: {error}"));
                    }
                }
            }
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
            Action::AwaitSessions(target) => {
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
                    return self.run_worktree_deletion(target);
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
                        return self.launch_editor(target.path, &editor_id);
                    }
                    ProjectCommand::Delete => {
                        return self.open_worktree_delete_dialog(target);
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
                        self.project_ui.dialog = None;
                        self.project_ui.cleanup_dialog = None;
                        self.project_ui.remove_dialog = Some(ProjectRemoval {
                            request: crate::model::OperationId::new(),
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
            Action::DismissRemove(_) => {
                self.project_ui.remove_dialog = None;
                return self.project_dialog_closed();
            }
            Action::DismissDelete(_) => return self.dismiss_worktree_delete_dialog(),
            Action::Dismiss => {
                self.project_ui.cleanup_dialog = None;
                return self.dismiss_worktree_delete_dialog();
            }
            Action::ConfirmRemove(_) => {
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
            Action::BranchDeleted { request, result } => {
                if !self.project_ui.branch_requests.remove(&request.id) {
                    return Task::none();
                }
                match result {
                    Ok(()) => {
                        self.status = Some(if request.warnings.is_empty() {
                            TransientNotice::success(format!(
                                "Worktree and branch {} deleted.",
                                request.branch
                            ))
                        } else {
                            TransientNotice::warning(format!(
                                "Worktree and branch {} deleted; {}",
                                request.branch,
                                request.warnings.join("; ")
                            ))
                        })
                    }
                    Err(failure)
                        if failure.kind
                            == crate::worktree_branch::BranchDeleteFailureKind::Unmerged
                            && request.continuation.is_some() =>
                    {
                        self.project_ui
                            .force_branches
                            .push_back((request, failure.detail));
                        return iced::widget::operation::focus(self.project_ui.focus_target())
                        .map(cosmic::Action::App);
                    }
                    Err(failure)
                        if failure.kind
                            == crate::worktree_branch::BranchDeleteFailureKind::Other =>
                    {
                        self.status = Some(TransientNotice::warning(format!(
                            "Worktree deleted; deletion of branch {} could not be confirmed: {}. Refresh the branch list before continuing.",
                            request.branch, failure.detail
                        )))
                    }
                    Err(failure) => {
                        self.status = Some(TransientNotice::warning(format!(
                            "Worktree deleted; branch {} retained: {}",
                            request.branch, failure.detail
                        )))
                    }
                }
            }
            Action::KeepBranch(id) => {
                if !self
                    .project_ui
                    .force_branches
                    .front()
                    .is_some_and(|(request, _)| request.id == id)
                {
                    return Task::none();
                }
                self.project_ui.force_branches.pop_front();
                return self.project_dialog_closed();
            }
            Action::ForceDeleteBranch(id) => {
                if !self
                    .project_ui
                    .force_branches
                    .front()
                    .is_some_and(|(request, _)| request.id == id)
                {
                    return Task::none();
                }
                let Some((request, _)) = self.project_ui.force_branches.pop_front() else {
                    return Task::none();
                };
                self.project_ui.branch_requests.insert(request.id);
                return Task::batch([
                    branch_deletion_task(request, true),
                    self.project_dialog_closed(),
                ]);
            }
            Action::DeleteOptionsRead { request, result } => {
                let Some(dialog) = self
                    .project_ui
                    .dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request)
                else {
                    return Task::none();
                };
                dialog.options_loading = false;
                match result {
                    Ok(options) => {
                        dialog.branch_elsewhere = options.branch_elsewhere;
                        dialog.stale_lock = options.stale_lock;
                        if options.branch_elsewhere {
                            dialog.delete_branch = false;
                        }
                        if let Some(error) = options.lock_error {
                            dialog.blocker = Some(error);
                        }
                    }
                    Err(error) => {
                        dialog.blocker =
                            Some(format!("Could not verify worktree ownership: {error}"))
                    }
                }
            }
            Action::DeleteBranchChanged(request, value) => {
                if let Some(dialog) = self.project_ui.dialog.as_mut().filter(|dialog| {
                    dialog.request == request
                        && !dialog.branch_elsewhere
                        && !dialog.options_loading
                        && !dialog.loading
                }) {
                    dialog.delete_branch = value;
                    dialog.branch_choice_edited = true;
                }
            }
            Action::BusyAgentsConfirmed(request, value) => {
                if let Some(dialog) = self.project_ui.dialog.as_mut().filter(|dialog| {
                    dialog.request == request && !dialog.loading && !dialog.options_loading
                }) {
                    dialog.busy_confirmed = value.then(|| dialog.session_census.clone());
                }
            }
            Action::StaleUnlocked { request, result } => {
                let Some(dialog) = self
                    .project_ui
                    .dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request)
                else {
                    return Task::none();
                };
                match result {
                    Ok(()) => {
                        dialog.stale_lock = false;
                        let target = dialog.target.clone();
                        if let Some(row) = self
                            .sidebar_model
                            .projects
                            .iter_mut()
                            .flat_map(|project| &mut project.worktrees)
                            .find(|row| Some(row.id) == target.worktree && row.path == target.path)
                        {
                            row.locked = false;
                        }
                        return safety_read_task(
                            target,
                            request,
                            true,
                            dialog.stop_authorization.clone(),
                        );
                    }
                    Err(error) => {
                        dialog.loading = false;
                        dialog.blocker = Some(format!("Could not unlock safely: {error}"));
                    }
                }
            }
            Action::ConfirmDelete(request) => {
                let Some(dialog) = self
                    .project_ui
                    .dialog
                    .as_ref()
                    .filter(|dialog| dialog.request == request)
                else {
                    return Task::none();
                };
                let target = dialog.target.clone();
                if dialog.blocker.is_some()
                    || dialog.loading
                    || dialog.options_loading
                    || dialog.safety.is_none()
                    || dialog.stop_authorization.is_none()
                    || !dialog.sessions_consented()
                {
                    return Task::none();
                }
                if let Some(blocker) =
                    self.worktree_deletion_blocker_ignoring_lock(&target, dialog.stale_lock)
                {
                    let dialog = self.project_ui.dialog.as_mut().unwrap();
                    dialog.blocker = Some(blocker);
                    dialog.failure = None;
                    return Task::none();
                }
                let dialog = self.project_ui.dialog.as_mut().unwrap();
                dialog.loading = true;
                if dialog.stale_lock {
                    let request = dialog.request;
                    let store = self.session.as_ref().unwrap().operation_store();
                    let device = stop_device_key();
                    return cosmic::task::future(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            unlock_stale_worktree_for_window(
                                &store,
                                &target.repository,
                                &target.path,
                                &device,
                            )
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                        Message::ProjectUi(Action::StaleUnlocked { request, result })
                    });
                }
                return safety_read_task(
                    target,
                    dialog.request,
                    true,
                    dialog.stop_authorization.clone(),
                );
            }
            Action::SafetyRead {
                request,
                confirming,
                result,
            } => {
                let on_top = self.worktree_delete_dialog_on_top();
                let default_branch = self
                    .project_ui
                    .dialog
                    .as_ref()
                    .filter(|dialog| dialog.request == request)
                    .and_then(|dialog| {
                        result.as_ref().ok().map(|inspection| {
                            let snapshot = &inspection.snapshot;
                            self.delete_branch_default(&dialog.target, snapshot)
                                || dialog.expected_head.as_ref().is_some_and(|expected| {
                                    expected.merged_receipt_matches(snapshot)
                                })
                        })
                    })
                    .unwrap_or(false);
                let expected_source_matches = self
                    .project_ui
                    .dialog
                    .as_ref()
                    .filter(|dialog| dialog.request == request)
                    .is_none_or(|dialog| {
                        dialog.expected_head.as_ref().is_none_or(|expected| {
                            self.deletion_head_matches_row(&dialog.target, expected)
                        })
                    });
                let Some(dialog) = self
                    .project_ui
                    .dialog
                    .as_mut()
                    .filter(|dialog| dialog.request == request)
                else {
                    return Task::none();
                };
                dialog.loading = false;
                match result {
                    Err(error) => {
                        dialog.safety = None;
                        dialog.stop_authorization = None;
                        dialog.failure = Some(format!(
                            "Could not verify worktree safety: {error}. Cancel and try again."
                        ));
                    }
                    Ok(inspection) => {
                        let SafetyInspection {
                            snapshot,
                            agents,
                            authorization,
                        } = inspection;
                        if !confirming {
                            dialog.stop_authorization = authorization;
                            if !dialog.stop_authorization.as_ref().is_some_and(|authority| {
                                authority.matches(&dialog.target, &snapshot)
                            }) {
                                dialog.safety = None;
                                dialog.blocker = Some("The original worktree inspection is unavailable. Cancel and review this worktree again.".into());
                                return Task::none();
                            }
                        }

                        if !expected_source_matches
                            || dialog.expected_head.as_ref().is_some_and(|expected| {
                                snapshot.branch.as_deref() != Some(expected.branch.as_str())
                                    || snapshot.head_oid.as_deref()
                                        != Some(expected.head_oid.as_str())
                                    || snapshot.head_repository.as_ref().is_none_or(|source| {
                                        !source.same_repository(&expected.head_repository)
                                    })
                            })
                        {
                            dialog.safety = None;
                            dialog.stop_authorization = None;
                            dialog.busy_confirmed = None;
                            dialog.blocker = Some("The merged pull request's exact branch, source or head changed. Cancel and refresh before offering cleanup.".into());
                            return Task::none();
                        }
                        let agents_changed = dialog.busy_agents != agents.agents
                            || dialog.session_census != agents.census;
                        dialog.busy_agents = agents.agents;
                        dialog.session_census = agents.census;
                        if confirming && !on_top {
                            dialog.safety = Some(snapshot);
                            dialog.busy_confirmed = None;
                            dialog.failure = Some("Confirmation was interrupted. Review this worktree again when the dialog is exposed.".into());
                            return Task::none();
                        }
                        let unchanged = dialog.safety.as_ref() == Some(&snapshot);
                        if confirming
                            && (!unchanged
                                || !dialog.stop_authorization.as_ref().is_some_and(|authority| {
                                    authority.matches(&dialog.target, &snapshot)
                                }))
                        {
                            dialog.safety = Some(snapshot);
                            dialog.stop_authorization = None;
                            dialog.busy_confirmed = None;
                            dialog.blocker = Some("The worktree changed after its original inspection. Cancel and reopen deletion to review it again.".into());
                            return Task::none();
                        }

                        if !dialog.branch_choice_edited && !dialog.branch_elsewhere {
                            dialog.delete_branch = default_branch;
                        }
                        if confirming && (agents_changed || !dialog.sessions_consented()) {
                            dialog.safety = Some(snapshot);
                            dialog.busy_confirmed = None;
                            dialog.failure = Some("The sessions or their ongoing work changed. Review the current sessions and confirm stopping them again.".into());
                            return Task::none();
                        }
                        if confirming && unchanged {
                            let target = dialog.target.clone();
                            let branch = dialog
                                .delete_branch
                                .then(|| snapshot.branch.clone().zip(snapshot.head_oid.clone()))
                                .flatten();
                            let consented_agents = AgentCensus {
                                agents: dialog.busy_agents.clone(),
                                census: dialog.session_census.clone(),
                            };
                            let original_authority = dialog
                                .stop_authorization
                                .clone()
                                .expect("original inspection checked");
                            if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                                self.project_ui.dialog.as_mut().unwrap().blocker = Some(blocker);
                                return Task::none();
                            }
                            if let Some(id) = target.worktree {
                                if let Some((branch, expected_oid)) = branch {
                                    self.project_ui.branch_deletions.insert(
                                        id,
                                        BranchDeletion {
                                            id: crate::model::OperationId::new(),
                                            repository: target.repository.clone(),
                                            branch,
                                            expected_oid,
                                            warnings: Vec::new(),
                                            continuation: None,
                                        },
                                    );
                                } else {
                                    self.project_ui.branch_deletions.remove(&id);
                                }
                                self.project_ui
                                    .deletion_busy_agents
                                    .insert(id, consented_agents);
                                self.project_ui
                                    .deletion_stop_authorizations
                                    .insert(id, original_authority);
                                self.project_ui.deletion_authorizations.insert(id, snapshot);
                            }
                            self.project_ui.dialog = None;
                            let deletion = self.start_worktree_deletion(target);
                            let next = self.next_worktree_delete_dialog();
                            return Task::batch([deletion, next]);
                        }
                        dialog.safety = Some(snapshot);
                        if confirming {
                            dialog.failure = Some("This worktree changed while confirmation was open. Review the updated counts and confirm again.".into());
                        }
                    }
                }
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
                if let Some(blocker) = self.worktree_deletion_blocker_ignoring_lock(&target, true) {
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
                return self.open_worktree_delete_dialog(target);
            }
            Action::RemovalCompleted {
                target,
                started_at,
                result,
            } => {
                if !target
                    .worktree
                    .and_then(|id| self.project_ui.deletions.get(&id))
                    .is_some_and(|deletion| {
                        deletion.started_at == started_at
                            && deletion.failure.is_none()
                            && deletion.target.path == target.path
                    })
                {
                    return Task::none();
                }
                let result = result.map(|outcome| {
                    if let Some(id) = target.worktree {
                        self.project_ui.deletion_warnings.insert(id, outcome.warnings);
                        if let Some(branch) = self.project_ui.branch_deletions.get_mut(&id) {
                            branch.continuation = outcome.continuation;
                            if branch.continuation.is_none() {
                                self.project_ui.branch_deletions.remove(&id);
                                self.project_ui.deletion_warnings.entry(id).or_default().push(
                                    "The worktree was removed; its branch was retained because the original deletion authority is unavailable. Refresh before reviewing the branch.".into());
                            }
                        }
                    }
                });
                let unconfirmed = result.as_ref().err().cloned();
                let task = self.handle_project_action(Action::Deleted {
                    target: target.clone(),
                    result,
                });
                if let Some(detail) = unconfirmed {
                    // Do not let a newly cached row blocker turn a lost reply
                    // into a claim that the original checkout still exists.
                    self.status = Some(TransientNotice::warning(format!(
                        "Worktree removal could not be confirmed: {detail}. Refresh the worktree list before continuing; no removal or branch request was replayed."
                    )));
                    if let Some(deletion) = target
                        .worktree
                        .and_then(|id| self.project_ui.deletions.get_mut(&id))
                    {
                        deletion.failure = Some(detail);
                    }
                    self.sync_worktree_operation_rows();
                }
                return task;
            }
            Action::DeletionSessionsClosed {
                target,
                started_at,
                result,
            } => {
                let Some(id) = target.worktree else {
                    return Task::none();
                };
                if !self.project_ui.deletions.get(&id).is_some_and(|deletion| {
                    deletion.started_at == started_at
                        && deletion.failure.is_none()
                        && deletion.target.path == target.path
                }) {
                    return Task::none();
                }
                let continuation = match result {
                    Ok(Some(continuation)) => continuation,
                    Ok(None) => {
                        let mut detail = "The sessions stopped, but deletion authority could not be retained. Refresh the worktree list and review again; the worktree and branch were not removed by this step.".to_owned();
                        if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                            detail.push_str(&format!(" Current blocker: {blocker}"));
                        }
                        let task = self.handle_project_action(Action::Deleted {
                            target: target.clone(),
                            result: Err(detail.clone()),
                        });
                        // Generic refusal cleanup may prefer a newly observed
                        // blocker, but it must not erase the completed Stop.
                        self.status = Some(TransientNotice::warning(detail.clone()));
                        if let Some(deletion) = self.project_ui.deletions.get_mut(&id) {
                            deletion.failure = Some(detail);
                        }
                        self.sync_worktree_operation_rows();
                        return task;
                    }
                    Err(failure) => {
                        return self.handle_project_action(Action::Deleted {
                            target,
                            result: Err(failure),
                        });
                    }
                };
                self.project_ui
                    .deletion_remove_authorizations
                    .insert(id, continuation);
                self.project_ui.released_delete_protections.insert(id);
                // Sessions have stopped, and reopening this target is fenced by
                // the running deletion. Release only our own target leases so
                // the owning daemon can acquire an exclusive removal guard.
                self.sync_worktree_protections();
                return self.handle_project_action(Action::DeletionProtectionsReleased {
                    target,
                    started_at,
                    result: Ok(()),
                });
            }
            Action::DeletionProtectionsReleased {
                target,
                started_at,
                result,
            } => {
                let Some(id) = target.worktree else {
                    return Task::none();
                };
                if !self.project_ui.released_delete_protections.contains(&id)
                    || !self.project_ui.deletions.get(&id).is_some_and(|deletion| {
                        deletion.started_at == started_at
                            && deletion.failure.is_none()
                            && deletion.target.path == target.path
                    })
                {
                    return Task::none();
                }
                if let Err(failure) = result {
                    return self.handle_project_action(Action::Deleted {
                        target,
                        result: Err(failure),
                    });
                }
                return self.remove_worktree_after_sessions(target);
            }
            Action::AutomaticallyDeleted {
                warnings,
                target,
                request,
                result,
            } => {
                let Some(id) = target.worktree else {
                    return Task::none();
                };
                if !self
                    .project_ui
                    .automatic_in_flight
                    .get(&id)
                    .is_some_and(|pending| {
                        pending.request == request
                            && pending.target.project == target.project
                            && pending.target.repository == target.repository
                            && pending.target.path == target.path
                    })
                {
                    return Task::none();
                }
                let in_flight = self.project_ui.automatic_in_flight.remove(&id).unwrap();
                match result {
                    Ok(receipt) if receipt.path == target.path => {
                        self.project_ui.automatic_attempts.remove(&id);
                        let task = self.complete_automatic_cleanup(in_flight);
                        if !warnings.is_empty() {
                            self.status = Some(TransientNotice::warning(format!(
                                "Worktree removed automatically. {}",
                                warnings.join("; ")
                            )));
                        }
                        return task;
                    }
                    result => {
                        let reason = match result {
                            Err(reason) => cleanup_kept_detail(&reason),
                            Ok(_) => "Cleanup returned a different worktree identity".into(),
                        };
                        self.project_ui.automatic_attempts.insert(
                            id,
                            AutomaticAttempt {
                                project: target.project,
                                at: Instant::now(),
                                reason: Some(reason),
                                local: false,
                            },
                        );
                    }
                }
                self.sync_worktree_deletion_rows();
                return self.lifecycle_sweep();
            }
            Action::Deleted { target, result } => {
                if let Some(id) = target.worktree {
                    self.project_ui.released_delete_protections.remove(&id);
                }
                match result {
                    Err(failure) => {
                        if let Some(id) = target.worktree {
                            self.project_ui.deletion_projections.remove(&id);
                            self.project_ui.branch_deletions.remove(&id);
                            self.project_ui.deletion_authorizations.remove(&id);
                            self.project_ui.deletion_stop_authorizations.remove(&id);
                            self.project_ui.deletion_remove_authorizations.remove(&id);
                            self.project_ui.deletion_busy_agents.remove(&id);
                        }
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
                        let safety = target
                            .worktree
                            .and_then(|id| self.project_ui.deletion_authorizations.remove(&id));
                        let started_at = target
                            .worktree
                            .and_then(|id| self.project_ui.deletions.get(&id))
                            .map(|deletion| deletion.started_at);
                        if let Some(id) = target.worktree {
                            self.project_ui.deletions.remove(&id);
                            self.project_ui.deletion_stop_authorizations.remove(&id);
                            self.project_ui.deletion_remove_authorizations.remove(&id);
                            self.project_ui.deletion_busy_agents.remove(&id);
                        }
                        // A completed removal does not own newer same-path
                        // rows, actors, filing, focus, camera or creation state.
                        // Only the original pre-Stop projection may be retired.
                        let projections = target
                            .worktree
                            .and_then(|id| self.project_ui.deletion_projections.remove(&id))
                            .filter(|(captured_at, custody)| {
                                Some(*captured_at) == started_at
                                    && custody.target.project == target.project
                                    && custody.target.repository == target.repository
                                    && custody.target.path == target.path
                            });
                        let projection_task = projections
                            .map(|(_, custody)| self.complete_removal_projections(custody))
                            .unwrap_or_else(|| {
                                // Unknown original custody stays unknown. Refresh
                                // inventory without adopting current owners.
                                self.repository_load_task(
                                    target.repository.clone(),
                                    Some(target.project),
                                    false,
                                )
                            });
                        let detail = match safety.as_ref() {
                            Some(snapshot) if snapshot.detached_only_commits > 0 => {
                                "Worktree deleted; detached commits preserved in a rescue branch."
                            }
                            Some(snapshot) if snapshot.branch.is_none() => {
                                "Detached worktree deleted."
                            }
                            _ => "Worktree deleted; branch retained.",
                        };
                        let warnings = target
                            .worktree
                            .and_then(|id| self.project_ui.deletion_warnings.remove(&id))
                            .unwrap_or_default();
                        self.status = Some(if warnings.is_empty() {
                            TransientNotice::success(detail)
                        } else {
                            TransientNotice::warning(format!("{detail} {}", warnings.join("; ")))
                        });
                        let branch_delete = target
                            .worktree
                            .and_then(|id| self.project_ui.branch_deletions.remove(&id))
                            .map(|mut request| {
                                request.warnings = warnings.clone();
                                self.project_ui.branch_requests.insert(request.id);
                                branch_deletion_task(request, false)
                            })
                            .unwrap_or_else(Task::none);
                        self.sync_worktree_operation_rows();
                        return Task::batch([
                            projection_task,
                            branch_delete,
                            self.lifecycle_sweep(),
                            self.sync_chrome(),
                        ]);
                    }
                }
            }
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
        let started_at = Instant::now();
        let projections = self.capture_removal_projections(target.clone());
        self.project_ui
            .deletion_projections
            .insert(id, (started_at, projections));
        self.project_ui.deletions.insert(
            id,
            Deletion {
                target: target.clone(),
                started_at,
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
        Task::batch([leave, self.run_worktree_deletion(target)])
    }

    /// Runs the deletion once no session in the worktree is still starting:
    /// one that is would come up after its directory had been emptied.
    fn run_worktree_deletion(&mut self, target: Target) -> Task<Message> {
        if self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && card_belongs_to(card, &target.path)
                && (self.pending_terminal_spawn(card.id) || self.chat_start_pending(card.id))
        }) {
            return cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Message::ProjectUi(Action::AwaitSessions(target))
            });
        }
        let authorization = target
            .worktree
            .and_then(|id| self.project_ui.deletion_authorizations.get(&id))
            .cloned();
        let stop_authorization = target
            .worktree
            .and_then(|id| self.project_ui.deletion_stop_authorizations.get(&id))
            .cloned();
        let consented_agents = target
            .worktree
            .and_then(|id| self.project_ui.deletion_busy_agents.get(&id))
            .cloned();
        let Some(started_at) = target
            .worktree
            .and_then(|id| self.project_ui.deletions.get(&id))
            .map(|deletion| deletion.started_at)
        else {
            return Task::none();
        };
        cosmic::task::future(async move {
            let request = target.clone();
            let result = tokio::task::spawn_blocking(move || {
                let confirmed =
                    authorization.ok_or("Deletion safety must be checked and confirmed again.")?;
                let observed =
                    consented_agents.ok_or("Session stop consent must be checked again")?;
                let authority = stop_authorization.ok_or(
                    "The original worktree inspection is unavailable. Cancel and review it again.",
                )?;
                stop_confirmed_worktree_sessions(&request, &confirmed, &observed, &authority)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ProjectUi(Action::DeletionSessionsClosed {
                target,
                started_at,
                result,
            })
        })
    }

    fn remove_worktree_after_sessions(&mut self, target: Target) -> Task<Message> {
        let authorization = target
            .worktree
            .and_then(|id| self.project_ui.deletion_authorizations.get(&id))
            .cloned();
        let Some(started_at) = target
            .worktree
            .and_then(|id| self.project_ui.deletions.get(&id))
            .map(|deletion| deletion.started_at)
        else {
            return Task::none();
        };
        let continuation = target
            .worktree
            .and_then(|id| self.project_ui.deletion_remove_authorizations.remove(&id));
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        cosmic::task::future(async move {
            let request = target.clone();
            let result = tokio::task::spawn_blocking(move || {
                let authorization =
                    authorization.ok_or("Deletion safety must be checked and confirmed again.")?;
                let continuation = continuation.ok_or("The original stopped-worktree authority is unavailable. Refresh and review again.")?;
                let device = &continuation.0.connection.device;
                let retirements = creation_retirements_for_window(store, &request.repository, &request.path, device);
                let mut outcome = remove_confirmed_worktree(&request, &authorization, &continuation)?;
                match retirements {
                    Ok(retirements) => {
                        // The transport verified the actual Removed.path against
                        // request.path. Common/admin/branch remain the original
                        // descriptive capture; no post-effect Git discovery.
                        let original = retirements.planned().clone();
                        if original.path == request.path {
                            outcome.warnings.extend(finish_creation_retirements(Ok(retirements), &original));
                        } else {
                            outcome.warnings.push("Creation metadata retained: the original capture named a different worktree.".into());
                        }
                    }
                    Err(reason) => outcome.warnings.extend(creation_retirement_warning(reason)),
                }
                Ok(outcome)
            })
            .await
            .map_err(|error| format!("Worktree removal completion could not be confirmed: {error}. Refresh the worktree list before continuing."))
            .and_then(|result| result);
            Message::ProjectUi(Action::RemovalCompleted {
                target,
                started_at,
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

    /// Cancels this review and its not-yet-started queued deletions.
    fn dismiss_worktree_delete_dialog(&mut self) -> Task<Message> {
        self.project_ui.pending_delete.clear();
        let dismissed = self.project_ui.dialog.take();
        // Declining to run a failed deletion again is the answer to it,
        // so its row stops asking.
        if let Some(id) = dismissed.and_then(|dialog| dialog.target.worktree)
            && self.project_ui.deletions.get(&id)
                .is_some_and(|deletion| deletion.failure.is_some())
        {
            self.project_ui.deletions.remove(&id);
            self.sync_worktree_operation_rows();
        }
        self.project_dialog_closed()
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
        let deletions = &self.project_ui.deletions;
        self.project_ui
            .deletion_projections
            .retain(|id, (started_at, _)| {
                deletions.get(id).is_some_and(|deletion| {
                    deletion.failure.is_none() && deletion.started_at == *started_at
                })
            });
        for worktree in projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
        {
            worktree.deletion = self.project_ui.row_deletion(worktree.id);
            worktree.cleanup_kept = self
                .settings
                .auto_delete_merged_worktrees
                .then(|| {
                    self.project_ui
                        .automatic_attempts
                        .get(&worktree.id)
                        .and_then(|attempt| attempt.reason.clone())
                })
                .flatten();
        }
    }

    fn worktree_deletion_blocker(&self, target: &Target) -> Option<String> {
        self.worktree_deletion_blocker_ignoring_lock(target, false)
    }

    fn worktree_deletion_blocker_ignoring_lock(
        &self,
        target: &Target,
        allow_lock: bool,
    ) -> Option<String> {
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
        if worktree.locked && !allow_lock {
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

    fn worktree_has_unsaved_editors(&self, target: &Target) -> bool {
        self.cards.values().any(|runtime| {
            matches!(runtime, CardRuntime::Repository(runtime)
                if runtime.descriptor.worktree.starts_with(&target.path) && runtime.is_dirty())
        })
    }

    fn worktree_activity_counts(&self, target: &Target) -> (usize, usize) {
        self.project_ui
            .dialog
            .as_ref()
            .filter(|dialog| {
                dialog.target.worktree == target.worktree && dialog.target.path == target.path
            })
            .map_or((0, 0), |dialog| {
                let working = dialog
                    .busy_agents
                    .iter()
                    .filter(|agent| agent.working_agent)
                    .count();
                (working, dialog.busy_agents.len() - working)
            })
    }

    fn delete_branch_default(&self, target: &Target, snapshot: &WorktreeSafetySnapshot) -> bool {
        let Some(project) = self.sidebar_model.project(target.project) else {
            return false;
        };
        let Some(source) = project
            .worktrees
            .iter()
            .find(|row| Some(row.id) == target.worktree)
            .and_then(|row| row.head_repository.as_ref())
        else {
            return false;
        };
        if snapshot
            .head_repository
            .as_ref()
            .is_none_or(|current| !current.same_repository(source))
        {
            return false;
        }
        let Some(branch) = snapshot.branch.as_deref() else {
            return false;
        };
        project.pull_requests.iter().any(|pr| {
            pr.state == Some(crate::sidebar::WorktreePullRequestState::Merged)
                && pr.head_ref == branch
                && pr.head_oid.as_ref() == snapshot.head_oid.as_ref()
                && snapshot.head_oid.is_some()
                && pr
                    .head_repository
                    .as_ref()
                    .is_some_and(|head| head.same_repository(source))
        })
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

    pub(super) fn project_dialog_dismiss_action(&self) -> Action {
        if let Some((request, _)) = self.project_ui.force_branches.front() {
            return Action::KeepBranch(request.id);
        }
        if self.project_ui.force_branches.is_empty()
            && self.project_ui.cleanup_dialog.is_none()
            && let Some(request) = self.project_ui.removal_request()
        {
            return Action::DismissRemove(request);
        }
        if self.project_ui.force_branches.is_empty()
            && self.project_ui.cleanup_dialog.is_none()
            && let Some(dialog) = &self.project_ui.dialog
        {
            return Action::DismissDelete(dialog.request);
        }
        Action::Dismiss
    }

    pub(super) fn project_dialog_overlay(&self) -> Option<Element<'_, Message>> {
        let surface = self.project_dialog_view()?;
        let dismiss = self.project_dialog_dismiss_action();
        let overlay = super::dialog_ui::overlay(
            surface,
            Some(Message::ProjectUi(dismiss.clone())),
            1.0,
        );
        Some(match dismiss {
            Action::DismissRemove(request) => {
                // Replace unnamed native press state too. Named controls also
                // carry this request: iced restores them outside keyed diff.
                iced::widget::keyed::Column::with_children([(request, overlay)])
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            Action::DismissDelete(request) => {
                iced::widget::keyed::Column::with_children([(request, overlay)])
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            Action::KeepBranch(request) => {
                iced::widget::keyed::Column::with_children([(request, overlay)])
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }
            _ => overlay,
        })
    }

    pub(super) fn project_dialog_view(&self) -> Option<Element<'_, Message>> {
        if let Some((request, detail)) = self.project_ui.force_branches.front() {
            return Some(super::dialog_ui::surface(widget::column::with_children(vec![
                design::title("Delete unmerged branch?").into(),
                super::notice_body(format!("The worktree was removed. Branch {} is still retained.", request.branch)),
                super::notice_detail(detail),
                super::notice_body("Force deletion can lose commits that are not merged. This confirmation applies only to the commit shown below; a changed or checked-out branch is retained."),
                super::notice_detail(&request.expected_oid),
                super::dialog_ui::confirmation_actions(
                    crate::ui_controls::button::secondary("Keep branch").id(force_branch_control_id(request.id, "keep")).on_press(Message::ProjectUi(Action::KeepBranch(request.id))),
                    crate::ui_controls::button::destructive("Force delete branch").id(force_branch_control_id(request.id, "force")).on_press(Message::ProjectUi(Action::ForceDeleteBranch(request.id))),
                ),
            ]).spacing(space::M), 500.0));
        }
        if let Some(dialog) = &self.project_ui.cleanup_dialog {
            let request = dialog.request;
            let mut input = crate::ui_controls::field::text(
                "Directory names, separated by commas",
                &dialog.directories,
            )
            .id(widget::Id::new("dialog.project.cleanup.directories"))
            .width(Length::Fill);
            if !dialog.loading {
                input = input.on_input(move |value| {
                    Message::ProjectUi(Action::CleanupPolicyChanged(request, value))
                });
            }
            let mut rows = vec![
                design::title("Project cleanup policy").into(),
                super::notice_body(&dialog.name),
                super::notice_detail(dialog.repository.display().to_string()),
                super::notice_body(
                    "Only ignored files inside these directory names may be discarded by automatic cleanup. Names match at any depth in this project's worktrees. Other ignored files, including .env and local databases, block cleanup.",
                ),
                input.into(),
                super::notice_detail(
                    "Use exact directory names, not paths or patterns. Quote names containing commas or surrounding spaces. Leave empty to preserve all ignored files. This policy is stored in this clone's local Git configuration and shared by its worktrees.",
                ),
            ];
            if let Some(error) = &dialog.error {
                rows.push(super::notice_body(error));
            }
            if dialog.loading {
                rows.push(super::notice_detail("Reading or saving project policy…"));
            }
            rows.push(super::dialog_ui::confirmation_actions(
                crate::ui_controls::button::secondary("Cancel")
                    .id(widget::Id::new("dialog.project.cleanup.cancel"))
                    .on_press(Message::ProjectUi(Action::Dismiss)),
                crate::ui_controls::button::primary("Save policy").on_press_maybe(
                    (!dialog.loading)
                        .then_some(Message::ProjectUi(Action::SaveCleanupPolicy(request))),
                ),
            ));
            return Some(super::dialog_ui::surface(
                widget::column::with_children(rows).spacing(space::M),
                560.0,
            ));
        }
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
                            .id(removal_control_id(removal.request, "cancel"))
                            .on_press(Message::ProjectUi(Action::DismissRemove(removal.request))),
                        crate::ui_controls::button::destructive("Remove project")
                            .id(removal_control_id(removal.request, "confirm"))
                            .on_press(Message::ProjectUi(Action::ConfirmRemove(removal.request))),
                    ),
                ])
                .spacing(space::M),
                500.0,
            ));
        }
        let dialog = self.project_ui.dialog.as_ref()?;
        let request = dialog.request;
        let target = &dialog.target;
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Delete worktree?").into(),
            super::notice_body(&target.label),
            super::notice_detail(target.path.display().to_string()),
            super::notice_detail(
                "Git will remove this worktree directory. Terminals and chats running in it, including detached sessions, close first. Safety is checked again before removal.",
            ),
        ];
        if let Some(snapshot) = &dialog.safety {
            rows.push(super::notice_body(format!(
                "{} modified · {} untracked · {} notable ignored files",
                snapshot.modified,
                snapshot.untracked,
                snapshot.notable_ignored.len()
            )));
            if !snapshot.notable_ignored.is_empty() {
                let examples = snapshot
                    .notable_ignored
                    .iter()
                    .take(5)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                rows.push(super::notice_detail(format!(
                    "Ignored user data: {examples}{}",
                    if snapshot.notable_ignored.len() > 5 {
                        ", …"
                    } else {
                        ""
                    }
                )));
            }
            rows.push(super::notice_body(format!(
                "{} commits not on a remote · {} commits on no branch",
                snapshot.unpushed_commits, snapshot.detached_only_commits
            )));
            if let Some(branch) = &snapshot.branch {
                if dialog.branch_elsewhere {
                    rows.push(super::notice_detail(format!(
                        "Branch {branch} is checked out elsewhere and will be retained."
                    )));
                } else {
                    rows.push(
                        crate::settings_widgets::toggle(
                            "Delete branch too",
                            dialog.delete_branch,
                            move |value| {
                                Message::ProjectUi(Action::DeleteBranchChanged(request, value))
                            },
                        )
                        .into(),
                    );
                    rows.push(super::notice_detail(format!("Branch {branch} uses Git's safe deletion check. Unmerged commits require a separate confirmation.")));
                }
            } else if snapshot.detached_only_commits > 0 {
                let head = snapshot.head_oid.as_deref().unwrap_or("");
                rows.push(super::notice_body(format!(
                    "HEAD is detached. Create rescue/{} to preserve these commits before deleting.",
                    &head[..12.min(head.len())]
                )));
            } else {
                rows.push(super::notice_detail(
                    "HEAD is detached. No branch is attached to this worktree.",
                ));
            }
            if snapshot.files_to_discard() > 0 {
                rows.push(super::notice_body(format!(
                    "Explicitly discarding {} files cannot be undone.",
                    snapshot.files_to_discard()
                )));
            }
        }
        let (working, busy) = self.worktree_activity_counts(target);
        if working > 0 {
            rows.push(super::notice_body(format!(
                "{working} {} working in this worktree.",
                if working == 1 {
                    "agent is"
                } else {
                    "agents are"
                }
            )));
        }
        if busy > 0 {
            rows.push(super::notice_body(format!(
                "{busy} {} in this worktree. Agent turns and commands in these sessions will be stopped.",
                if busy == 1 { "session is busy or has unconfirmed activity" } else { "sessions are busy or have unconfirmed activity" }
            )));
        }
        let sessions = dialog.session_census.len();
        if sessions > 0 {
            rows.push(
                crate::settings_widgets::toggle(
                    &format!(
                        "Stop {sessions} session{} and all their ongoing work",
                        if sessions == 1 { "" } else { "s" }
                    ),
                    dialog.sessions_consented(),
                    move |value| Message::ProjectUi(Action::BusyAgentsConfirmed(request, value)),
                )
                .into(),
            );
        }
        if dialog.stale_lock {
            rows.push(super::notice_detail("The cagents creation has settled and its process lease is free. Unlocking is checked again before deletion."));
        }
        if dialog.loading || dialog.options_loading {
            rows.push(super::notice_body("Checking current files and commits…"));
        }
        if self.worktree_has_unsaved_editors(target) {
            rows.push(super::notice_body(
                "Unsaved editor changes in this worktree will be discarded.",
            ));
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
        let confirm = (dialog.blocker.is_none()
            && !dialog.loading
            && !dialog.options_loading
            && dialog.safety.is_some()
            && dialog.stop_authorization.is_some()
            && dialog.sessions_consented())
        .then_some(Message::ProjectUi(Action::ConfirmDelete(request)));
        let label = match dialog.safety.as_ref() {
            Some(snapshot) if snapshot.detached_only_commits > 0 => {
                if snapshot.files_to_discard() > 0 {
                    format!(
                        "Rescue, discard {} file{} and delete",
                        snapshot.files_to_discard(),
                        if snapshot.files_to_discard() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    )
                } else {
                    "Create rescue branch and delete".to_owned()
                }
            }
            Some(snapshot) if snapshot.files_to_discard() > 0 => {
                format!(
                    "Discard {} file{} and delete",
                    snapshot.files_to_discard(),
                    if snapshot.files_to_discard() == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            }
            _ => "Delete worktree".to_owned(),
        };
        let label = if dialog.stale_lock {
            format!("Unlock and {}", label.to_lowercase())
        } else {
            label
        };
        let actions = super::dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(deletion_control_id(request, "cancel"))
                .on_press(Message::ProjectUi(Action::DismissDelete(request))),
            crate::ui_controls::button::destructive(label)
                .id(deletion_control_id(request, "confirm"))
                .on_press_maybe(confirm),
        );
        let form = widget::container(
            widget::scrollable(widget::column::with_children(rows).spacing(space::M))
                .height(Length::Shrink),
        )
        .max_height((self.window_height - 180.0).max(120.0));

        Some(super::dialog_ui::surface(
            widget::column::with_children(vec![form.into(), actions]).spacing(space::M),
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
#[cfg(test)]
fn delete_worktree(
    target: &Target,
    confirmed: &WorktreeSafetySnapshot,
) -> Result<Option<RemovedWorktree>, String> {
    delete_worktree_inner(target, confirmed, true)
}

#[cfg(test)]
fn delete_worktree_inner(
    target: &Target,
    confirmed: &WorktreeSafetySnapshot,
    close_sessions: bool,
) -> Result<Option<RemovedWorktree>, String> {
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
    verify_worktree_safety(&service, &target.path, confirmed, &cancellation)?;
    if close_sessions {
        close_worktree_sessions(target)?;
    }
    // Processes may have written during shutdown; consent covers only the
    // inspected state, not changes produced after confirmation.
    verify_worktree_safety(&service, &target.path, confirmed, &cancellation)?;
    if confirmed.detached_only_commits > 0 {
        service
            .rescue_detached_worktree(&target.path, confirmed, &cancellation)
            .map_err(|error| format!("Could not preserve detached commits: {error}"))?;
    }
    let command = worktree_removal_command(target, confirmed);
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

#[cfg(test)]
fn worktree_removal_command(target: &Target, confirmed: &WorktreeSafetySnapshot) -> CommandSpec {
    let command = RepositoryService::system("git")
        .git_base(&target.repository, true)
        .args(["worktree", "remove"]);
    let command = if confirmed.files_to_discard() > 0 {
        command.arg("--force")
    } else {
        command
    };
    command
        .arg("--")
        .arg(target.path.as_os_str())
        .timeout(Duration::from_secs(30))
        .output_limits(64 * 1024, 64 * 1024)
}

#[cfg(test)]
fn verify_worktree_safety(
    service: &RepositoryService,
    path: &Path,
    confirmed: &WorktreeSafetySnapshot,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let current = service
        .worktree_safety(path, cancellation)
        .map_err(|error| error.to_string())?;
    if &current != confirmed {
        return Err("The worktree changed after confirmation. Review its current files and commits before deleting again.".into());
    }
    Ok(())
}

fn confirmed_removal_operation(
    target: &Target,
    confirmed: &WorktreeSafetySnapshot,
    continuation: uuid::Uuid,
) -> Result<crate::worktree_control::Operation, String> {
    // The control protocol currently represents removal targets as UTF-8.
    // A lossy path could name a different checkout and must never be sent.
    let path = target.path.to_str().ok_or("This worktree path cannot be represented by the daemon protocol; remove it explicitly in Git")?;
    Ok(crate::worktree_control::Operation::Remove {
        repository: target.repository.clone(),
        target: path.to_owned(),
        force: confirmed.files_to_discard() > 0,
        delete_branch: false,
        expected_fingerprint: Some(confirmed.fingerprint.clone()),
        resolved_path: None,
        continuation: Some(continuation),
    })
}

fn creation_retirements_for_window(
    store: Option<Arc<crate::persistence::StateStore>>,
    repository: &Path,
    path: &Path,
    device: &Option<(String, String, String)>,
) -> Result<crate::repository::WorktreeCreationRetirements, String> {
    if *device != stop_device_key() {
        return Err("the selected device changed before metadata capture".into());
    }
    if device.is_some() {
        return Err("remote creation metadata has no original local association".into());
    }
    let store = store.ok_or("the original window journal is unavailable")?;
    RepositoryService::system("git").creation_retirements_before_removal(
        store,
        repository,
        path,
        &CancellationToken::new(),
    )
}

fn creation_retirement_warning(reason: String) -> Vec<String> {
    tracing::warn!(%reason, "removed worktree creation metadata retained");
    vec![format!("Creation metadata retained: {reason}")]
}

fn finish_creation_retirements(
    capture: Result<crate::repository::WorktreeCreationRetirements, String>,
    removed: &RemovedWorktree,
) -> Vec<String> {
    match capture {
        Err(reason) => creation_retirement_warning(reason),
        Ok(capture) => capture
            .finish(removed)
            .err()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|error| creation_retirement_warning(error.to_string()))
            .collect(),
    }
}

fn remove_confirmed_worktree(
    target: &Target,
    confirmed: &WorktreeSafetySnapshot,
    authority: &RemoveContinuation,
) -> Result<RemovalOutcome, String> {
    if !authority.0.matches(target, confirmed) {
        return Err(
            "The original worktree removal authority changed. Refresh and review again.".into(),
        );
    }
    let crate::worktree_control::Reply::Removed { path, mut warnings, branch_error, continuation, .. } =
        authority.0.request(confirmed_removal_operation(target, confirmed, authority.0.token)?)
            .map_err(|error| format!("Worktree removal completion could not be confirmed: {error}. Refresh the worktree list; no request was replayed."))?
        else { return Err("Unexpected worktree removal response; refresh the worktree list before continuing".into()); };
    if path != target.path {
        return Err("The daemon reported a different removed worktree; refresh the repository before continuing".into());
    }
    if let Some(error) = branch_error {
        warnings.push(error);
    }
    Ok(RemovalOutcome {
        warnings,
        continuation: continuation
            .filter(|token| !token.is_nil())
            .map(|token| BranchContinuation(authority.0.with_token(token))),
    })
}

fn worktree_control_request(
    operation: crate::worktree_control::Operation,
) -> Result<crate::worktree_control::Reply, String> {
    match crate::device_service::request(crate::device_service::Operation::Worktree { operation })?
    {
        crate::device_service::Reply::Worktree(reply) => Ok(reply),
        _ => Err("Unexpected worktree control response".into()),
    }
}

fn unlock_stale_worktree_for_window(
    store: &crate::persistence::StateStore,
    repository: &Path,
    path: &Path,
    device: &Option<(String, String, String)>,
) -> Result<(), String> {
    stale_worktree_lock_for_window(store, repository, path, device, true)
}

fn stale_worktree_lock_for_window(
    store: &crate::persistence::StateStore,
    repository: &Path,
    path: &Path,
    device: &Option<(String, String, String)>,
    unlock: bool,
) -> Result<(), String> {
    if *device != stop_device_key() {
        return Err(
            "The selected device changed. Refresh and review the worktree lock again.".into(),
        );
    }
    if device.is_none() {
        // The durable creation receipt belongs to this original window store.
        // A refusal must not retry against a daemon's unrelated journal.
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::new();
        return if unlock {
            service.unlock_stale_worktree(store, repository, path, &cancellation)
        } else {
            service.stale_worktree_lock(store, repository, path, &cancellation)
        };
    }
    // A remote path is never handed to local Git. The remote service can
    // proceed only if it has genuine matching creation provenance of its own.
    match worktree_control_request(crate::worktree_control::Operation::StaleLock {
        repository: repository.to_path_buf(),
        path: path.to_path_buf(),
        unlock,
    })? {
        crate::worktree_control::Reply::StaleLock { unlocked } if unlocked == unlock => Ok(()),
        _ => Err("Unexpected stale-lock response".into()),
    }
}

fn delete_confirmed_worktree_branch(
    worker: BranchDeletion,
    force: bool,
) -> Result<(), crate::worktree_branch::BranchDeleteFailure> {
    let expected_branch = worker.branch.clone();
    let other = |detail| crate::worktree_branch::BranchDeleteFailure {
        kind: crate::worktree_branch::BranchDeleteFailureKind::Other,
        detail,
    };
    let invalid = |detail| crate::worktree_branch::BranchDeleteFailure {
        kind: crate::worktree_branch::BranchDeleteFailureKind::AuthorityChanged,
        detail,
    };
    let authority = worker.continuation.as_ref().ok_or_else(|| invalid(
                "The original branch deletion authority is unavailable. Refresh and review the branch again.".into()))?;
    if authority.0.repository != worker.repository {
        return Err(invalid(
            "The original branch repository changed. Refresh and review again.".into(),
        ));
    }
    match authority
        .0
        .request(crate::worktree_control::Operation::DeleteBranch {
            repository: worker.repository,
            branch: worker.branch,
            force,
            expected_oid: Some(worker.expected_oid),
            continuation: Some(authority.0.token),
        })
        .map_err(other)?
    {
        crate::worktree_control::Reply::BranchDeleted { branch } if branch == expected_branch => {
            Ok(())
        }
        crate::worktree_control::Reply::BranchDeleteRefused {
            failure,
            continuation,
        } if failure.kind == crate::worktree_branch::BranchDeleteFailureKind::Unmerged => {
            if !force && continuation == Some(authority.0.token) {
                Err(failure)
            } else {
                Err(invalid(format!(
                    "{}; the original force-confirmation authority is unavailable. Refresh before continuing.",
                    failure.detail
                )))
            }
        }
        crate::worktree_control::Reply::BranchDeleteRefused { failure, .. } => Err(failure),
        _ => Err(other("Unexpected branch deletion response".into())),
    }
}

fn branch_deletion_task(request: BranchDeletion, force: bool) -> Task<Message> {
    cosmic::task::future(async move {
        let worker = request.clone();
        let result =
            tokio::task::spawn_blocking(move || delete_confirmed_worktree_branch(worker, force))
                .await
                .unwrap_or_else(|error| {
                    Err(crate::worktree_branch::BranchDeleteFailure {
                        kind: crate::worktree_branch::BranchDeleteFailureKind::Other,
                        detail: error.to_string(),
                    })
                });
        Message::ProjectUi(Action::BranchDeleted { request, result })
    })
}

fn delete_options_task(
    target: Target,
    request: u64,
    store: Arc<crate::persistence::StateStore>,
) -> Task<Message> {
    let device = stop_device_key();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if device != stop_device_key() {
                return Err("The selected device changed. Refresh and review again.".into());
            }
            let crate::worktree_control::Reply::Listed { worktrees: records } =
                worktree_control_request(crate::worktree_control::Operation::List {
                    repository: target.repository.clone(),
                })?
            else {
                return Err("Unexpected worktree listing response".into());
            };
            let record = records
                .iter()
                .find(|record| record.path == target.path)
                .ok_or("Worktree no longer registered")?;
            let branch_elsewhere = record.branch.as_ref().is_some_and(|branch| {
                records
                    .iter()
                    .any(|other| other.path != record.path && other.branch.as_ref() == Some(branch))
            });
            let lock = record.locked.then(|| {
                stale_worktree_lock_for_window(
                    &store,
                    &target.repository,
                    &target.path,
                    &device,
                    false,
                )
            });
            if device != stop_device_key() {
                return Err("The selected device changed. Refresh and review again.".into());
            }
            Ok(DeleteOptions {
                branch_elsewhere,
                stale_lock: matches!(&lock, Some(Ok(()))),
                lock_error: lock.and_then(Result::err),
            })
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        Message::ProjectUi(Action::DeleteOptionsRead { request, result })
    })
}

/// All paths belong to the selected device. The UI never opens a remote
/// path with local Git or reads a different clone's cleanup policy.
fn inspect_worktree(
    repository: &Path,
    path: &Path,
) -> Result<(WorktreeSafetySnapshot, Vec<String>), String> {
    let reply = crate::device_service::request(crate::device_service::Operation::Worktree {
        operation: crate::worktree_control::Operation::Inspect {
            repository: repository.to_owned(),
            path: path.to_owned(),
        },
    })?;
    inspected_worktree(reply)
}

fn inspected_worktree(
    reply: crate::device_service::Reply,
) -> Result<(WorktreeSafetySnapshot, Vec<String>), String> {
    let crate::device_service::Reply::Worktree(crate::worktree_control::Reply::Inspected {
        snapshot,
        disposable_ignored_directories,
        ..
    }) = reply
    else {
        return Err("The selected device returned an unexpected worktree inspection reply.".into());
    };
    let safety = serde_json::from_str(&snapshot).map_err(|error| {
        format!("The selected device returned invalid worktree safety data: {error}")
    })?;
    Ok((safety, disposable_ignored_directories))
}

/// The stream is part of the authority: a new connection cannot inherit this
/// receipt. Network work and locking happen only in blocking workers.
struct StopConnection {
    client: std::sync::Mutex<crate::control::ControlClient>,
    device: Option<(String, String, String)>,
    #[cfg(test)]
    fixture_peer: Option<std::os::unix::net::UnixStream>,
}

#[derive(Clone)]
struct StopAuthorization {
    connection: Arc<StopConnection>,
    token: uuid::Uuid,
    inspected: Arc<WorktreeSafetySnapshot>,
    repository: PathBuf,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RemoveContinuation(StopAuthorization);
#[derive(Clone, Debug)]
struct BranchContinuation(StopAuthorization);
#[derive(Clone, Debug)]
pub struct RemovalOutcome {
    warnings: Vec<String>,
    continuation: Option<BranchContinuation>,
}

impl std::fmt::Debug for StopAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never lock a transport while rendering/logging UI state.
        formatter
            .debug_struct("StopAuthorization")
            .finish_non_exhaustive()
    }
}

fn stop_device_key() -> Option<(String, String, String)> {
    crate::device::selected().map(|device| {
        (
            device.name.clone(),
            device.host.clone(),
            device.executable.clone(),
        )
    })
}

fn request_on_stop_client(
    client: &mut crate::control::ControlClient,
    operation: crate::worktree_control::Operation,
) -> Result<crate::worktree_control::Reply, String> {
    match client
        .device_operation(crate::device_service::Operation::Worktree { operation })
        .map_err(|error| error.to_string())?
    {
        crate::device_service::Reply::Worktree(reply) => Ok(reply),
        _ => Err("Unexpected worktree control response".into()),
    }
}

impl StopAuthorization {
    fn with_token(&self, token: uuid::Uuid) -> Self {
        Self {
            token,
            ..self.clone()
        }
    }

    fn matches(&self, target: &Target, snapshot: &WorktreeSafetySnapshot) -> bool {
        self.repository == target.repository
            && self.path == target.path
            && *self.inspected == *snapshot
            && self.connection.device == stop_device_key()
    }

    fn request(
        &self,
        operation: crate::worktree_control::Operation,
    ) -> Result<crate::worktree_control::Reply, String> {
        if self.connection.device != stop_device_key() {
            return Err(
                "The selected device changed. Cancel and review the worktree again.".into(),
            );
        }
        let mut client = self.connection.client.lock().map_err(
            |_| "The original inspection connection is unavailable. Cancel and review again.",
        )?;
        if self.connection.device != stop_device_key() {
            return Err("The selected device changed while waiting for its connection. Cancel and review again.".into());
        }
        // No reconnect/retry: disconnect invalidates this connection's receipt.
        let reply = request_on_stop_client(&mut client, operation)?;
        if self.connection.device != stop_device_key() {
            return match reply {
                crate::worktree_control::Reply::SessionsStopped { path, .. } => {
                    Ok(crate::worktree_control::Reply::SessionsStopped {
                        path,
                        continuation: None,
                    })
                }
                crate::worktree_control::Reply::Removed {
                    path,
                    branch_deleted,
                    mut warnings,
                    branch_error,
                    ..
                } => {
                    warnings.push("The selected device changed after removal; branch authority was discarded. Refresh before continuing.".into());
                    Ok(crate::worktree_control::Reply::Removed {
                        path,
                        branch_deleted,
                        warnings,
                        branch_error,
                        continuation: None,
                    })
                }
                known @ crate::worktree_control::Reply::BranchDeleted { .. } => Ok(known),
                _ => Err(
                    "The selected device changed during the request. Refresh before continuing."
                        .into(),
                ),
            };
        }
        Ok(reply)
    }
}

#[derive(Debug, Clone)]
pub struct SafetyInspection {
    snapshot: Arc<WorktreeSafetySnapshot>,
    agents: AgentCensus,
    authorization: Option<StopAuthorization>,
}

impl SafetyInspection {
    fn initial(
        mut client: crate::control::ControlClient,
        target: &Target,
        device: Option<(String, String, String)>,
    ) -> Result<Self, String> {
        if device != stop_device_key() {
            return Err(
                "The selected device changed before inspection. Cancel and review again.".into(),
            );
        }
        let reply = request_on_stop_client(
            &mut client,
            crate::worktree_control::Operation::InspectForSessionStop {
                repository: target.repository.clone(),
                path: target.path.clone(),
            },
        )?;
        let token = match &reply {
            crate::worktree_control::Reply::Inspected { stop_authority: Some(token), .. }
                if !token.is_nil() => *token,
            _ => return Err("The daemon did not retain an original worktree inspection. Cancel and review again.".into()),
        };
        let (snapshot, _) = inspected_worktree(crate::device_service::Reply::Worktree(reply))?;
        let agents = checked_busy_worktree_agents(request_on_stop_client(
            &mut client,
            crate::worktree_control::Operation::BusySessions {
                repository: target.repository.clone(),
                path: target.path.clone(),
            },
        )?)?;
        if device != stop_device_key() {
            return Err(
                "The selected device changed during inspection. Cancel and review again.".into(),
            );
        }
        let snapshot = Arc::new(snapshot);
        let authorization = StopAuthorization {
            connection: Arc::new(StopConnection {
                client: std::sync::Mutex::new(client),
                device,
                #[cfg(test)]
                fixture_peer: None,
            }),
            token,
            inspected: Arc::clone(&snapshot),
            repository: target.repository.clone(),
            path: target.path.clone(),
        };
        Ok(Self {
            snapshot,
            agents,
            authorization: Some(authorization),
        })
    }
}

fn stop_confirmed_worktree_sessions(
    target: &Target,
    confirmed: &WorktreeSafetySnapshot,
    observed: &AgentCensus,
    authority: &StopAuthorization,
) -> Result<Option<RemoveContinuation>, String> {
    if !authority.matches(target, confirmed) {
        return Err("The original worktree inspection changed. Cancel and review it again.".into());
    }
    let crate::worktree_control::Reply::Listed { worktrees } =
        authority.request(crate::worktree_control::Operation::List {
            repository: target.repository.clone(),
        })?
    else {
        return Err("Unexpected worktree listing response".into());
    };
    let record = worktrees
        .iter()
        .find(|record| record.path == target.path)
        .ok_or("Worktree changed before closing sessions")?;
    if record.locked || record.primary {
        return Err("Worktree is locked or is the primary checkout".into());
    }
    let reply = authority.request(crate::worktree_control::Operation::Inspect {
        repository: target.repository.clone(),
        path: target.path.clone(),
    })?;
    let (current, _) = inspected_worktree(crate::device_service::Reply::Worktree(reply))?;
    if current != *confirmed {
        return Err("The worktree changed after confirmation; cancel and review its current files and commits again".into());
    }
    let consent = worktree_session_stop_consent(observed)?;
    // The host checks the ORIGINAL physical pins and fingerprint before first
    // teardown, independently of this adjacent read and full session consent.
    match authority.request(crate::worktree_control::Operation::StopSessions {
        repository: target.repository.clone(), path: target.path.clone(), consent,
        expected_fingerprint: authority.inspected.fingerprint.clone(), stop_authority: authority.token,
    }).map_err(|error| format!("Session stop completion could not be confirmed: {error}. No removal was requested; refresh and review again."))? {
        crate::worktree_control::Reply::SessionsStopped { path, continuation } if path == target.path =>
            Ok(continuation.filter(|token| !token.is_nil()).map(|token| RemoveContinuation(authority.with_token(token)))) ,
        _ => Err("Unexpected worktree session stop response".into()),
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentCensus {
    agents: Vec<BusyAgent>,
    census: Vec<BusyAgent>,
}

/// Derive only from the complete roster explicitly approved by the user.
/// Revisions still fence the UI's fresh confirmation; whole-session stop
/// intentionally authorizes all work within each retained actor lifetime.
fn worktree_session_stop_consent(
    observed: &AgentCensus,
) -> Result<crate::worktree_control::SessionStopConsent, String> {
    let mut sessions = observed
        .census
        .iter()
        .map(|actor| crate::worktree_control::SessionIdentity {
            session: actor.session,
            instance: actor.session_instance,
        })
        .collect::<Vec<_>>();
    sessions.sort();
    if sessions.iter().any(|session| session.instance.is_nil())
        || sessions
            .windows(2)
            .any(|pair| pair[0].session == pair[1].session)
    {
        return Err("The worktree session roster has unknown or ambiguous lifetime identities; review it again".into());
    }
    Ok(crate::worktree_control::SessionStopConsent { sessions })
}

fn busy_worktree_agents(repository: &Path, path: &Path) -> Result<AgentCensus, String> {
    let reply = worktree_control_request(crate::worktree_control::Operation::BusySessions {
        repository: repository.to_owned(),
        path: path.to_owned(),
    })?;
    checked_busy_worktree_agents(reply)
}

fn checked_busy_worktree_agents(
    reply: crate::worktree_control::Reply,
) -> Result<AgentCensus, String> {
    let crate::worktree_control::Reply::BusySessions {
        mut agents,
        mut census,
    } = reply
    else {
        return Err("Unexpected worktree agent census response".into());
    };
    agents.sort();
    census.sort();
    if [&agents, &census].into_iter().any(|rows| {
        rows.windows(2)
            .any(|pair| pair[0].session == pair[1].session)
    }) || agents
        .iter()
        .any(|agent| census.binary_search(agent).is_err())
        || census
            .iter()
            .any(|agent| agent.working_agent && agents.binary_search(agent).is_err())
    {
        return Err("The daemon returned ambiguous active agent identities".into());
    }
    Ok(AgentCensus { agents, census })
}

// Policy controls must stay usable even when deletion safety cannot inventory a
// protected directory. A save reply reports the policy readback, not a later census.
fn read_cleanup_policy(repository: &Path) -> Result<Vec<String>, String> {
    let reply = crate::device_service::request(crate::device_service::Operation::Worktree {
        operation: crate::worktree_control::Operation::GetDisposableIgnoredDirectories {
            repository: repository.to_owned(),
            path: repository.to_owned(),
        },
    })?;
    disposable_ignored_directories(reply)
}

fn disposable_ignored_directories(
    reply: crate::device_service::Reply,
) -> Result<Vec<String>, String> {
    match reply {
        crate::device_service::Reply::Worktree(
            crate::worktree_control::Reply::DisposableIgnoredDirectories { directories },
        ) => Ok(directories),
        _ => Err("The selected device returned an unexpected project cleanup policy reply.".into()),
    }
}

fn safety_read_task(
    target: Target,
    request: u64,
    confirming: bool,
    original: Option<StopAuthorization>,
) -> Task<Message> {
    // Capture before scheduling any connection or RPC work. Never relabel an
    // already-open stream with selection observed after a blocking operation.
    let device = stop_device_key();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if confirming {
                let authority = original.ok_or(
                    "The original worktree inspection is unavailable. Cancel and review it again.",
                )?;
                let reply = authority.request(crate::worktree_control::Operation::Inspect {
                    repository: target.repository.clone(),
                    path: target.path.clone(),
                })?;
                let (snapshot, _) =
                    inspected_worktree(crate::device_service::Reply::Worktree(reply))?;
                let agents = checked_busy_worktree_agents(authority.request(
                    crate::worktree_control::Operation::BusySessions {
                        repository: target.repository.clone(),
                        path: target.path.clone(),
                    },
                )?)?;
                Ok(SafetyInspection {
                    snapshot: Arc::new(snapshot),
                    agents,
                    authorization: None,
                })
            } else {
                if device != stop_device_key() {
                    return Err(
                        "The selected device changed before connecting. Cancel and review again."
                            .into(),
                    );
                }
                let socket =
                    crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
                let client = crate::control::ControlClient::connect(&socket)
                    .map_err(|error| error.to_string())?;
                // The selection is immutable after initialization in this
                // process. A concurrent initialization must invalidate this
                // opening, including one racing the transport connection.
                SafetyInspection::initial(client, &target, device)
            }
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        Message::ProjectUi(Action::SafetyRead {
            request,
            confirming,
            result,
        })
    })
}

fn format_disposable_directories(names: &[String]) -> String {
    names
        .iter()
        .map(|name| {
            if name.contains([',', '"']) || name.trim() != name {
                format!("\"{}\"", name.replace('"', "\"\""))
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_disposable_directories(value: &str) -> Result<Vec<String>, String> {
    let mut directories = Vec::new();
    let mut remaining = value.trim_start();
    while !remaining.is_empty() {
        let quoted = remaining.starts_with('"');
        let name = if quoted {
            let mut chars = remaining[1..].char_indices().peekable();
            let mut name = String::new();
            let mut closed = None;
            while let Some((index, ch)) = chars.next() {
                if ch == '"' {
                    if chars.peek().is_some_and(|(_, next)| *next == '"') {
                        chars.next();
                        name.push('"');
                    } else {
                        closed = Some(index + 2);
                        break;
                    }
                } else {
                    name.push(ch);
                }
            }
            let end = closed.ok_or("Close the quoted directory name with a double quote.")?;
            remaining = remaining[end..].trim_start();
            if !remaining.is_empty() && !remaining.starts_with(',') {
                return Err("Separate directory names with commas after a closing quote.".into());
            }
            name
        } else {
            let end = remaining.find(',').unwrap_or(remaining.len());
            let name = remaining[..end].trim().to_owned();
            remaining = &remaining[end..];
            if name.contains('"') {
                return Err(
                    "Quote the whole directory name and double any quotes inside it.".into(),
                );
            }
            name
        };
        remaining = remaining
            .strip_prefix(',')
            .unwrap_or(remaining)
            .trim_start();
        if name.is_empty() && !quoted {
            continue;
        }
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
        {
            return Err(
                "Enter directory names separated by commas, without paths or control characters."
                    .into(),
            );
        }
        if !directories.iter().any(|entry| entry == &name) {
            directories.push(name);
        }
    }
    Ok(directories)
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleanup_policy_reply_does_not_require_a_deletion_snapshot() {
        let directories = vec!["build,data".to_owned(), "cache ".to_owned()];
        let decoded = disposable_ignored_directories(crate::device_service::Reply::Worktree(
            crate::worktree_control::Reply::DisposableIgnoredDirectories {
                directories: directories.clone(),
            },
        ))
        .unwrap();
        assert_eq!(decoded, directories);
        assert!(
            disposable_ignored_directories(crate::device_service::Reply::Worktree(
                crate::worktree_control::Reply::Inspected {
                    snapshot: "{}".into(),
                    stop_authority: None,
                    disposable_ignored_directories: vec![]
                },
            ))
            .is_err()
        );
    }

    #[test]
    fn worktree_inspection_rejects_wrong_or_malformed_device_reply() {
        assert!(
            inspected_worktree(crate::device_service::Reply::Worktree(
                crate::worktree_control::Reply::Listed {
                    worktrees: Vec::new()
                }
            ))
            .is_err()
        );
        assert!(
            inspected_worktree(crate::device_service::Reply::Worktree(
                crate::worktree_control::Reply::Inspected {
                    snapshot: "{}".into(),
                    stop_authority: None,
                    disposable_ignored_directories: vec![]
                }
            ))
            .is_err()
        );
    }

    #[test]
    fn worktree_inspection_preserves_byte_exact_ignored_paths_and_policy_names() {
        let safety = WorktreeSafetySnapshot {
            head_repository: None,
            fingerprint: "fresh-device-snapshot".into(),
            modified: 0,
            untracked: 1,
            notable_ignored: vec![
                crate::repository::RepoPath::from_bytes(b"ignored-\xff").unwrap(),
            ],
            unpushed_commits: 2,
            detached_only_commits: 0,
            branch: Some("feature".into()),
            head_oid: Some("abcd".into()),
        };
        let directories = vec!["build,data".to_owned(), "cache ".to_owned()];
        let reply =
            crate::device_service::Reply::Worktree(crate::worktree_control::Reply::Inspected {
                snapshot: serde_json::to_string(&safety).unwrap(),
                stop_authority: None,
                disposable_ignored_directories: directories.clone(),
            });
        let (decoded, policy) = inspected_worktree(reply).unwrap();
        assert_eq!(decoded, safety);
        assert_eq!(policy, directories);
    }

    use super::*;
    use crate::sidebar::RowDeletion;

    fn fixture_safety_result(
        app: &Cagents,
        result: Result<(WorktreeSafetySnapshot, AgentCensus), String>,
        confirming: bool,
    ) -> Result<SafetyInspection, String> {
        let (snapshot, agents) = result?;
        if confirming || app.project_ui.dialog.is_none() {
            return Ok(SafetyInspection {
                snapshot: Arc::new(snapshot),
                agents,
                authorization: None,
            });
        }
        let target = app.project_ui.dialog.as_ref().unwrap().target.clone();
        let (client_stream, mut server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        let response_snapshot = serde_json::to_string(&snapshot).unwrap();
        let response_agents = agents.clone();
        let token = uuid::Uuid::new_v4();
        let peer = std::thread::spawn(move || {
            use crate::terminal::ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
                ServerResponse, read_frame, write_frame,
            };
            let proof = crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("stop-ui-fixture"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
            };
            let hello: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(hello.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                    },
                },
            )
            .unwrap();
            for phase in 0..2 {
                let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                let ClientRequest::DeviceOperation {
                    operation: crate::device_service::Operation::Worktree { operation },
                } = request.request
                else {
                    panic!("unexpected fixture request")
                };
                let reply = if phase == 0 {
                    assert!(matches!(
                        operation,
                        crate::worktree_control::Operation::InspectForSessionStop { .. }
                    ));
                    crate::worktree_control::Reply::Inspected {
                        snapshot: response_snapshot.clone(),
                        disposable_ignored_directories: vec![],
                        stop_authority: Some(token),
                    }
                } else {
                    assert!(matches!(
                        operation,
                        crate::worktree_control::Operation::BusySessions { .. }
                    ));
                    crate::worktree_control::Reply::BusySessions {
                        agents: response_agents.agents.clone(),
                        census: response_agents.census.clone(),
                    }
                };
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::DeviceOperation {
                                reply: crate::device_service::Reply::Worktree(reply),
                            },
                        },
                    },
                )
                .unwrap();
            }
            server_stream
        });
        let client = crate::control::ControlClient::from_stream(client_stream).unwrap();
        // This proves the real framed caller contract only. The separate host
        // fixture must prove physical pins; a fixture UUID is not that proof.
        let mut inspection = SafetyInspection::initial(client, &target, stop_device_key()).unwrap();
        Arc::get_mut(&mut inspection.authorization.as_mut().unwrap().connection)
            .unwrap()
            .fixture_peer = Some(peer.join().unwrap());
        Ok(inspection)
    }

    #[test]
    fn stop_remove_and_explicit_force_keep_the_same_stream_and_original_phase_tokens() {
        use crate::terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        };
        for mode in 0..3 {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let target = Target {
                project: ProjectId::new(),
                worktree: Some(WorktreeId::new()),
                repository: "/fixture-original-repository".into(),
                path: "/fixture-original-repository/linked".into(),
                label: "feature".into(),
            };
            drop(app.open_worktree_delete_dialog(target.clone()));
            let mut snapshot = clean_snapshot();
            snapshot.head_oid = Some("a".repeat(40));
            let mut inspection =
                fixture_safety_result(&app, Ok((snapshot.clone(), AgentCensus::default())), false)
                    .unwrap();
            let authority = inspection.authorization.as_mut().unwrap();
            let connection = Arc::get_mut(&mut authority.connection).unwrap();
            let mut peer = connection.fixture_peer.take().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let proof = connection.client.get_mut().unwrap().proof().clone();
            let initial = authority.token;
            let stopped = uuid::Uuid::new_v4();
            let removed = uuid::Uuid::new_v4();
            let path = target.path.clone();
            let original_snapshot = serde_json::to_string(&snapshot).unwrap();
            let fingerprint = snapshot.fingerprint.clone();
            let server = std::thread::spawn(move || {
                let count = if mode == 0 {
                    3
                } else if mode == 1 {
                    4
                } else {
                    6
                };
                for phase in 0..count {
                    let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    let ClientRequest::DeviceOperation {
                        operation: crate::device_service::Operation::Worktree { operation },
                    } = request.request
                    else {
                        panic!("changed original transport");
                    };
                    let reply = match phase {
                        0 => {
                            assert!(matches!(
                                operation,
                                crate::worktree_control::Operation::List { .. }
                            ));
                            crate::worktree_control::Reply::Listed {
                                worktrees: vec![crate::worktree_control::Entry {
                                    path: path.clone(),
                                    branch: Some("feature".into()),
                                    head_oid: Some("a".repeat(40)),
                                    primary: false,
                                    detached: false,
                                    locked: false,
                                    dirty: false,
                                    ahead: None,
                                    behind: None,
                                    live_sessions: Vec::new(),
                                }],
                            }
                        }
                        1 => {
                            assert!(matches!(
                                operation,
                                crate::worktree_control::Operation::Inspect { .. }
                            ));
                            crate::worktree_control::Reply::Inspected {
                                snapshot: original_snapshot.clone(),
                                disposable_ignored_directories: Vec::new(),
                                stop_authority: None,
                            }
                        }
                        2 => {
                            let crate::worktree_control::Operation::StopSessions {
                                stop_authority,
                                expected_fingerprint,
                                consent,
                                ..
                            } = operation
                            else {
                                panic!("expected original Stop");
                            };
                            assert_eq!(stop_authority, initial);
                            assert_eq!(expected_fingerprint, fingerprint);
                            assert!(consent.sessions.is_empty());
                            crate::worktree_control::Reply::SessionsStopped {
                                path: path.clone(),
                                continuation: (mode != 0).then_some(stopped),
                            }
                        }
                        3 => {
                            let crate::worktree_control::Operation::Remove {
                                continuation,
                                expected_fingerprint,
                                force,
                                delete_branch,
                                target,
                                ..
                            } = operation
                            else {
                                panic!("expected original Remove");
                            };
                            assert_eq!(continuation, Some(stopped));
                            assert_eq!(expected_fingerprint.as_ref(), Some(&fingerprint));
                            assert_eq!(target, path.to_str().unwrap());
                            assert!(!force);
                            assert!(!delete_branch);
                            crate::worktree_control::Reply::Removed {
                                path: path.clone(),
                                branch_deleted: None,
                                warnings: vec!["Known successful removal".into()],
                                branch_error: None,
                                continuation: (mode == 2).then_some(removed),
                            }
                        }
                        _ => {
                            let crate::worktree_control::Operation::DeleteBranch {
                                continuation,
                                branch,
                                expected_oid,
                                force,
                                ..
                            } = operation
                            else {
                                panic!("expected explicit branch phase");
                            };
                            assert_eq!(continuation, Some(removed));
                            assert_eq!(branch, "feature");
                            assert_eq!(expected_oid, Some("a".repeat(40)));
                            assert_eq!(force, phase == 5);
                            if force {
                                crate::worktree_control::Reply::BranchDeleted { branch }
                            } else {
                                crate::worktree_control::Reply::BranchDeleteRefused { failure: crate::worktree_branch::BranchDeleteFailure {
                                kind: crate::worktree_branch::BranchDeleteFailureKind::Unmerged, detail: "Unmerged".into() }, continuation: Some(removed) }
                            }
                        }
                    };
                    write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Response {
                                response: ServerResponse::DeviceOperation {
                                    reply: crate::device_service::Reply::Worktree(reply),
                                },
                            },
                        },
                    )
                    .unwrap();
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "No reconnect, token renewal, or replay after completion"
                );
            });
            let next = stop_confirmed_worktree_sessions(
                &target,
                &snapshot,
                &AgentCensus::default(),
                inspection.authorization.as_ref().unwrap(),
            )
            .unwrap();
            if mode == 0 {
                assert!(
                    next.is_none(),
                    "known Stop success does not invent Remove authority"
                );
            } else {
                let remove = next.unwrap();
                assert_eq!(remove.0.token, stopped);
                let outcome = remove_confirmed_worktree(&target, &snapshot, &remove).unwrap();
                assert_eq!(outcome.warnings, vec!["Known successful removal"]);
                if mode == 1 {
                    assert!(
                        outcome.continuation.is_none(),
                        "known Removed survives absent branch phase"
                    );
                } else {
                    let request = BranchDeletion {
                        id: crate::model::OperationId::new(),
                        repository: target.repository.clone(),
                        branch: "feature".into(),
                        expected_oid: "a".repeat(40),
                        warnings: Vec::new(),
                        continuation: outcome.continuation,
                    };
                    let refusal =
                        delete_confirmed_worktree_branch(request.clone(), false).unwrap_err();
                    assert_eq!(
                        refusal.kind,
                        crate::worktree_branch::BranchDeleteFailureKind::Unmerged
                    );
                    delete_confirmed_worktree_branch(request, true).unwrap();
                }
            }
            drop(inspection);
            server.join().unwrap();
        }
    }

    #[test]
    fn local_stale_lock_uses_the_original_window_journal_and_rechecks_its_live_lease() {
        use crate::repository::{BaseRevision, CreateWorktreeRequest, ValidatedBranchName};
        use std::os::fd::AsRawFd;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("repository");
        git(&root, &["init", "-b", "main", "repository"]);
        git(&repository, &["commit", "--allow-empty", "-m", "base"]);
        let (session, _) = SessionRuntime::open(
            &root.join("window.sqlite3"),
            crate::model::BootId::new("lock-window"),
        )
        .unwrap();
        let store = session.operation_store();
        let request = CreateWorktreeRequest {
            repository: repository.clone(),
            managed_root: root.join("managed"),
            destination: root.join("managed/feature"),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            base: BaseRevision::LocalBranch(ValidatedBranchName::parse("main").unwrap()),
            track: false,
            operation_id: crate::model::OperationId::new(),
            setup_intent: None,
        };
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::new();
        let (progress, _) = async_channel::bounded(8);
        let report =
            service.create_worktree_journaled(&store, &request, 7, &cancellation, &progress);
        assert_eq!(
            report.state,
            crate::repository::WorktreeOperationState::Created,
            "{report:?}"
        );
        let reason = format!("cagents:{}", request.operation_id);
        git(
            &repository,
            &[
                "worktree",
                "lock",
                "--reason",
                &reason,
                request.destination.to_str().unwrap(),
            ],
        );
        let locked = || {
            service
                .list_worktrees(&repository, &cancellation)
                .unwrap()
                .into_iter()
                .find(|record| record.path == request.destination)
                .unwrap()
                .lock
                .locked
        };
        stale_worktree_lock_for_window(&store, &repository, &request.destination, &None, false)
            .unwrap();
        assert!(locked(), "eligibility must not unlock");
        let unrelated = crate::persistence::StateStore::open(
            root.join("other.sqlite3"),
            crate::model::BootId::new("unrelated"),
        )
        .unwrap();
        for unlock in [false, true] {
            assert!(
                stale_worktree_lock_for_window(
                    &unrelated,
                    &repository,
                    &request.destination,
                    &None,
                    unlock
                )
                .unwrap_err()
                .contains("No matching cagents creation receipt")
            );
            assert!(
                locked(),
                "another store must not borrow the window's receipt"
            );
        }
        let common = service.common_dir(&repository, &cancellation).unwrap();
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(common.join(format!("cagents-operation-{}.lease", request.operation_id)))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(
            unlock_stale_worktree_for_window(&store, &repository, &request.destination, &None)
                .unwrap_err()
                .contains("lease")
        );
        assert!(locked());
        drop(lease);
        unlock_stale_worktree_for_window(&store, &repository, &request.destination, &None).unwrap();
        assert!(!locked());
        let wrong_device = Some(("other".into(), "example.invalid".into(), "cagents".into()));
        assert!(
            stale_worktree_lock_for_window(
                &store,
                &repository,
                &request.destination,
                &wrong_device,
                true
            )
            .unwrap_err()
            .contains("selected device changed")
        );
    }

    #[test]
    fn lost_removal_reply_remains_unconfirmed_on_the_loaded_row_after_a_new_blocker() {
        let repository = PathBuf::from("/repo");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let mut linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("linked"),
            Some("feature".into()),
        );
        linked.locked = true;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let target = app.project_target(project, Some(linked.id)).unwrap();
        assert!(app.worktree_deletion_blocker(&target).is_some());
        let started_at = Instant::now();
        app.project_ui.deletions.insert(
            linked.id,
            Deletion {
                target: target.clone(),
                started_at,
                failure: None,
            },
        );
        let detail = "Removal completion could not be confirmed: original connection closed";
        drop(app.handle_project_action(Action::RemovalCompleted {
            target,
            started_at,
            result: Err(detail.into()),
        }));
        assert_eq!(
            app.project_ui.deletions[&linked.id].failure.as_deref(),
            Some(detail)
        );
        assert!(matches!(&row(&app, linked.id).unwrap().deletion,
            Some(crate::sidebar::RowDeletion::Failed(reason)) if reason.as_ref() == detail));
        assert!(app.status.as_ref().unwrap().message.contains(detail));
        assert!(app.project_ui.branch_requests.is_empty());
        assert!(app.project_ui.force_branches.is_empty());
    }

    #[test]
    fn known_stopped_or_removed_without_next_authority_keeps_the_completed_outcome() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = app.project_ui.begin_deleting("/fixture/linked".into());
        let id = target.worktree.unwrap();
        let started_at = app.project_ui.deletions[&id].started_at;
        assert!(
            app.worktree_deletion_blocker(&target).is_some(),
            "a later blocker must not erase the known Stop outcome"
        );
        drop(app.handle_project_action(Action::DeletionSessionsClosed {
            target: target.clone(),
            started_at,
            result: Ok(None),
        }));
        assert!(
            app.project_ui.deletions[&id]
                .failure
                .as_ref()
                .unwrap()
                .contains("The sessions stopped")
        );
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("The sessions stopped")
        );
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("Current blocker:")
        );
        assert!(app.project_ui.deletion_remove_authorizations.is_empty());
        assert!(app.project_ui.released_delete_protections.is_empty());
        let target = app.project_ui.begin_deleting("/fixture/removed".into());
        let id = target.worktree.unwrap();
        let started_at = app.project_ui.deletions[&id].started_at;
        app.project_ui.branch_deletions.insert(
            id,
            BranchDeletion {
                id: crate::model::OperationId::new(),
                repository: target.repository.clone(),
                branch: "feature".into(),
                expected_oid: "a".repeat(40),
                warnings: Vec::new(),
                continuation: None,
            },
        );
        drop(app.handle_project_action(Action::RemovalCompleted {
            target,
            started_at,
            result: Ok(RemovalOutcome {
                warnings: Vec::new(),
                continuation: None,
            }),
        }));
        assert!(
            !app.project_ui.deletions.contains_key(&id),
            "known Removed must not become an error"
        );
        assert!(app.project_ui.branch_deletions.is_empty());
        assert!(app.project_ui.branch_requests.is_empty());
        assert!(
            app.project_ui.force_branches.is_empty(),
            "missing authority never grants Force"
        );
    }

    #[test]
    fn stop_uses_the_original_inspection_connection_and_receipt_without_retrying_refusal() {
        use crate::terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        };
        for refuse in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            // Only the private paired fixture stream is used by this test.
            let target = Target {
                project: ProjectId::new(),
                worktree: Some(WorktreeId::new()),
                repository: "/cagents-fixture-no-such-repository".into(),
                path: "/cagents-fixture-no-such-repository/linked".into(),
                label: "fixture".into(),
            };
            drop(app.open_worktree_delete_dialog(target.clone()));
            let snapshot = clean_snapshot();
            let observed = AgentCensus::default();
            let mut inspection =
                fixture_safety_result(&app, Ok((snapshot.clone(), observed.clone())), false)
                    .unwrap();
            let authority = inspection.authorization.as_mut().unwrap();
            let connection = Arc::get_mut(&mut authority.connection).unwrap();
            let mut peer = connection.fixture_peer.take().unwrap();
            let proof = connection.client.get_mut().unwrap().proof().clone();
            let token = authority.token;
            let expected_fingerprint = snapshot.fingerprint.clone();
            let expected_path = target.path.clone();
            let server_snapshot = serde_json::to_string(&snapshot).unwrap();
            let server = std::thread::spawn(move || {
                for phase in 0..3 {
                    let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    let ClientRequest::DeviceOperation {
                        operation: crate::device_service::Operation::Worktree { operation },
                    } = request.request
                    else {
                        panic!("caller reconnected or used a different protocol")
                    };
                    let reply = match phase {
                        0 => {
                            assert!(matches!(
                                operation,
                                crate::worktree_control::Operation::List { .. }
                            ));
                            crate::worktree_control::Reply::Listed {
                                worktrees: vec![crate::worktree_control::Entry {
                                    path: expected_path.clone(),
                                    branch: Some("feature".into()),
                                    head_oid: None,
                                    primary: false,
                                    detached: false,
                                    locked: false,
                                    dirty: false,
                                    ahead: None,
                                    behind: None,
                                    live_sessions: vec![],
                                }],
                            }
                        }
                        1 => {
                            assert!(
                                matches!(
                                    operation,
                                    crate::worktree_control::Operation::Inspect { .. }
                                ),
                                "confirmation never renews physical authority"
                            );
                            crate::worktree_control::Reply::Inspected {
                                snapshot: server_snapshot.clone(),
                                disposable_ignored_directories: vec![],
                                stop_authority: None,
                            }
                        }
                        _ => {
                            let crate::worktree_control::Operation::StopSessions {
                                path,
                                stop_authority,
                                expected_fingerprint: original,
                                consent,
                                ..
                            } = operation
                            else {
                                panic!("missing guarded Stop")
                            };
                            assert_eq!(path, expected_path);
                            assert_eq!(stop_authority, token);
                            assert_eq!(original, expected_fingerprint);
                            assert!(consent.sessions.is_empty());
                            if refuse {
                                write_frame(&mut peer, &ServerEnvelope {version: PROTOCOL_VERSION, request_id: Some(request.request_id), proof: proof.clone(), message: ServerMessage::Error(ProtocolError::new(ProtocolErrorCode::InvalidRequest, "Original inspection expired or was replaced; review again", false))}).unwrap();
                                // The caller must not request another inspection,
                                // stop, removal or transparent retry on refusal.
                                let next: Option<ClientEnvelope> = read_frame(&mut peer).unwrap();
                                assert!(next.is_none());
                                return;
                            }
                            crate::worktree_control::Reply::SessionsStopped {
                                path: expected_path.clone(),
                                continuation: Some(uuid::Uuid::new_v4()),
                            }
                        }
                    };
                    write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Response {
                                response: ServerResponse::DeviceOperation {
                                    reply: crate::device_service::Reply::Worktree(reply),
                                },
                            },
                        },
                    )
                    .unwrap();
                }
            });
            let result = stop_confirmed_worktree_sessions(
                &target,
                &snapshot,
                &observed,
                inspection.authorization.as_ref().unwrap(),
            );
            assert_eq!(result.is_err(), refuse);
            if refuse {
                assert!(result.unwrap_err().contains("Original inspection"));
            }
            drop(inspection);
            server.join().unwrap();
        }
    }

    #[test]
    fn inspection_from_another_device_is_not_relabelled_on_delivery() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(WorktreeId::new()),
            repository: "/cagents-fixture-no-such-repository".into(),
            path: "/cagents-fixture-no-such-repository/linked".into(),
            label: "fixture".into(),
        };
        drop(app.open_worktree_delete_dialog(target.clone()));
        let mut inspection =
            fixture_safety_result(&app, Ok((clean_snapshot(), AgentCensus::default())), false)
                .unwrap();
        let connection =
            Arc::get_mut(&mut inspection.authorization.as_mut().unwrap().connection).unwrap();
        // Keep the actual acquired connection/receipt, but model delivery after
        // selection changed away from its captured endpoint. Do not mutate the
        // process-wide OnceLock used by unrelated tests.
        connection.device = Some(("other-device".into(), "other-host".into(), "cagents".into()));
        assert_ne!(connection.device, stop_device_key());
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: false,
            result: Ok(inspection),
        }));
        let dialog = app.project_ui.dialog.as_ref().unwrap();
        assert!(dialog.safety.is_none());
        assert!(dialog.blocker.is_some());
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        assert!(!app.project_ui.is_deleting(target.worktree.unwrap()));
    }

    #[test]
    fn missing_or_disconnected_original_inspection_does_not_gain_stop_authority() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(WorktreeId::new()),
            repository: "/cagents-fixture-no-such-repository".into(),
            path: "/cagents-fixture-no-such-repository/linked".into(),
            label: "fixture".into(),
        };
        drop(app.open_worktree_delete_dialog(target.clone()));
        let snapshot = clean_snapshot();
        let mut inspection =
            fixture_safety_result(&app, Ok((snapshot.clone(), AgentCensus::default())), false)
                .unwrap();
        // Real peer disconnect; the original client must fail without opening
        // another connection or substituting a newly issued receipt.
        Arc::get_mut(&mut inspection.authorization.as_mut().unwrap().connection)
            .unwrap()
            .fixture_peer
            .take();
        assert!(
            stop_confirmed_worktree_sessions(
                &target,
                &snapshot,
                &AgentCensus::default(),
                inspection.authorization.as_ref().unwrap()
            )
            .is_err()
        );
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: false,
            result: Ok(SafetyInspection {
                snapshot: Arc::new(snapshot),
                agents: AgentCensus::default(),
                authorization: None,
            }),
        }));
        let dialog = app.project_ui.dialog.as_ref().unwrap();
        assert!(dialog.stop_authorization.is_none());
        assert!(dialog.blocker.is_some());
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        assert!(!app.project_ui.is_deleting(target.worktree.unwrap()));
    }

    fn clean_snapshot() -> WorktreeSafetySnapshot {
        WorktreeSafetySnapshot {
            head_repository: None,
            fingerprint: "fixture".into(),
            modified: 0,
            untracked: 0,
            notable_ignored: Vec::new(),
            unpushed_commits: 0,
            detached_only_commits: 0,
            branch: Some("feature".into()),
            head_oid: None,
        }
    }

    /// Lifecycle tests use virtual paths and inject the worker's checked reply.
    /// Fresh filesystem inspection is covered separately with real repositories.
    pub(super) fn confirm_deletion_fixture(app: &mut Cagents) {
        let Some(dialog) = &app.project_ui.dialog else {
            return;
        };
        let request = dialog.request;
        let agents = AgentCensus {
            agents: dialog.busy_agents.clone(),
            census: dialog.session_census.clone(),
        };
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request,
            result: Ok(DeleteOptions {
                branch_elsewhere: false,
                stale_lock: false,
                lock_error: None,
            }),
        }));
        let snapshot = clean_snapshot();
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: false,
            result: fixture_safety_result(&app, Ok((snapshot.clone(), agents.clone())), false),
        }));
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: true,
            result: fixture_safety_result(&app, Ok((snapshot, agents)), true),
        }));
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

    #[test]
    fn replaced_project_removal_refuses_the_old_confirmation_before_closing_sessions() {
        let repository = PathBuf::from("/request-repo");
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
        let open = || Action::Command {
            project_id: project,
            worktree_id: None,
            command: ProjectCommand::Remove,
        };
        drop(app.handle_project_action(open()));
        let old = app.project_ui.removal_request().unwrap();
        drop(app.handle_project_action(open()));
        let current = app.project_ui.removal_request().unwrap();
        assert_ne!(old, current);
        drop(app.handle_project_action(Action::ConfirmRemove(old)));
        assert_eq!(app.project_ui.removal_request(), Some(current));
        assert!(app.project_ui.removing.is_empty());
        assert!(app.project_ui.close_started.is_empty());
        let close = app.handle_project_action(Action::ConfirmRemove(current));
        assert!(app.project_ui.removal_request().is_none());
        assert!(app.project_ui.removing.contains_key(&repository));
        drop(close); // The production close worker is deliberately unpolled.
    }

    fn removal_fixture() -> (Cagents, tempfile::TempDir, ProjectId) {
        let repository = PathBuf::from("/request-repo");
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
        deletable_app(&repository, &linked, &primary)
    }

    fn open_removal(app: &mut Cagents, project: ProjectId) -> crate::model::OperationId {
        drop(app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: None,
            command: ProjectCommand::Remove,
        }));
        app.project_ui.removal_request().unwrap()
    }

    fn removal_native_frame(
        element: &mut Element<'_, Message>,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
    ) -> (
        cosmic::iced::advanced::layout::Node,
        HashMap<String, cosmic::iced::Rectangle>,
    ) {
        use cosmic::iced::advanced::{
            Layout, layout,
            widget::{Operation, operation::Focusable},
        };
        struct Bounds(HashMap<String, cosmic::iced::Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: cosmic::iced::Rectangle,
                _: &mut dyn Focusable,
            ) {
                if let Some(id) = id {
                    self.0.insert(id.to_string(), bounds);
                }
            }
        }
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(
                cosmic::iced::Size::ZERO,
                cosmic::iced::Size::new(1000.0, 900.0),
            ),
        );
        let mut bounds = Bounds(HashMap::new());
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut bounds);
        (node, bounds.0)
    }

    fn removal_pointer_event(
        element: &mut Element<'_, Message>,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        node: &cosmic::iced::advanced::layout::Node,
        point: cosmic::iced::Point,
        event: cosmic::iced::mouse::Event,
    ) -> Vec<Message> {
        use cosmic::iced::{
            Event, Rectangle, Size,
            advanced::{Layout, Shell, clipboard},
            mouse,
        };
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &Event::Mouse(event),
            Layout::new(node),
            mouse::Cursor::Available(point),
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(1000.0, 900.0)),
        );
        messages
    }

    #[test]
    fn removal_old_cancel_and_backdrop_cannot_dismiss_the_reopened_request() {
        use cosmic::iced::{Point, advanced::widget::Tree, mouse};
        for role in [None, Some("cancel")] {
            let (mut app, _directory, project) = removal_fixture();
            let old = open_removal(&mut app, project);
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.dialog().unwrap();
            let mut tree = Tree::new(element.as_widget());
            let (node, bounds) = removal_native_frame(&mut element, &mut tree, &renderer);
            let point = role
                .map(|role| bounds[&removal_control_id(old, role).to_string()].center())
                .unwrap_or(Point::new(4.0, 4.0));
            let mut messages = removal_pointer_event(
                &mut element,
                &mut tree,
                &renderer,
                &node,
                point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            );
            messages.extend(removal_pointer_event(
                &mut element,
                &mut tree,
                &renderer,
                &node,
                point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ));
            assert_eq!(messages.len(), 1);
            assert!(
                matches!(&messages[0], Message::ProjectUi(Action::DismissRemove(request)) if *request == old)
            );
            drop(element);
            let current = open_removal(&mut app, project);
            assert_ne!(old, current);
            for message in messages {
                drop(app.update(message));
            }
            drop(app.handle_project_action(Action::Dismiss)); // old untyped controls are not new consent
            assert_eq!(app.project_ui.removal_request(), Some(current));
            assert!(app.project_ui.removing.is_empty());
            assert!(app.project_ui.close_started.is_empty());
            drop(app.handle_project_action(Action::DismissRemove(current)));
            assert!(app.project_ui.removal_request().is_none());
        }
    }

    #[test]
    fn removal_native_press_does_not_transfer_to_a_reopened_request() {
        use cosmic::iced::{
            advanced::widget::{Tree, tree},
            mouse,
        };
        for role in ["cancel", "confirm"] {
            let (mut app, _directory, project) = removal_fixture();
            let old = open_removal(&mut app, project);
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.dialog().unwrap();
            let mut state = Tree::new(element.as_widget());
            let (node, bounds) = removal_native_frame(&mut element, &mut state, &renderer);
            let point = bounds[&removal_control_id(old, role).to_string()].center();
            assert!(
                removal_pointer_event(
                    &mut element,
                    &mut state,
                    &renderer,
                    &node,
                    point,
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                )
                .is_empty()
            );
            drop(element);
            let current = open_removal(&mut app, project);
            let mut element = app.dialog().unwrap();
            // Match the pinned iced runtime: named widget state is lifted before
            // keyed diff and restored afterward. A key alone is insufficient.
            tree::NAMED.with(|named| *named.borrow_mut() = state.take_all_named());
            state.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut state);
            tree::NAMED.with(|named| named.borrow_mut().clear());
            let (node, bounds) = removal_native_frame(&mut element, &mut state, &renderer);
            let current_point = bounds[&removal_control_id(current, role).to_string()].center();
            assert_eq!(point, current_point);
            assert!(
                removal_pointer_event(
                    &mut element,
                    &mut state,
                    &renderer,
                    &node,
                    current_point,
                    mouse::Event::ButtonReleased(mouse::Button::Left)
                )
                .is_empty()
            );
            assert!(
                removal_pointer_event(
                    &mut element,
                    &mut state,
                    &renderer,
                    &node,
                    current_point,
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                )
                .is_empty()
            );
            let messages = removal_pointer_event(
                &mut element,
                &mut state,
                &renderer,
                &node,
                current_point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            );
            assert_eq!(messages.len(), 1, "the actual current button still works");
            assert!(match &messages[0] {
                Message::ProjectUi(Action::DismissRemove(request)) if role == "cancel" =>
                    *request == current,
                Message::ProjectUi(Action::ConfirmRemove(request)) if role == "confirm" =>
                    *request == current,
                _ => false,
            });
        }
    }

    fn deletion_review_fixture() -> (Cagents, tempfile::TempDir, WorktreeId) {
        let repository = PathBuf::from("/delete-native-owner");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(), repository.clone(), Some("main".into()),
        );
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(), repository.join("linked"), Some("feature".into()),
        );
        let (mut app, directory, project) = deletable_app(&repository, &linked, &primary);
        let target = app.project_target(project, Some(linked.id)).unwrap();
        app.project_ui.deletions.insert(linked.id, Deletion {
            target, started_at: Instant::now(), failure: Some("owned earlier attempt failed".into()),
        });
        ready_deletion_retry(&mut app, linked.id);
        (app, directory, linked.id)
    }

    fn ready_deletion_retry(app: &mut Cagents, worktree: WorktreeId) -> u64 {
        // Use the actual failed-row retry route, including its replacement of
        // an existing review; worker tasks stay unpolled in this UI fixture.
        drop(app.handle_project_action(Action::RetryDelete(worktree)));
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request,
            result: Ok(DeleteOptions {
                branch_elsewhere: false, stale_lock: false, lock_error: None,
            }),
        }));
        let result = fixture_safety_result(app, Ok((clean_snapshot(), AgentCensus::default())), false);
        drop(app.handle_project_action(Action::SafetyRead { request, confirming: false, result }));
        let review = app.project_ui.dialog.as_ref().unwrap();
        assert!(review.safety.is_some() && review.stop_authorization.is_some());
        assert!(!review.loading && !review.options_loading && review.blocker.is_none());
        request
    }

    #[test]
    fn deletion_old_cancel_and_backdrop_preserve_the_replacement_review() {
        use cosmic::iced::{Point, advanced::widget::Tree, mouse};
        for role in [None, Some("cancel")] {
            let (mut app, _directory, worktree) = deletion_review_fixture();
            let old = app.project_ui.dialog.as_ref().unwrap().request;
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.dialog().unwrap();
            let mut tree = Tree::new(element.as_widget());
            let (node, bounds) = removal_native_frame(&mut element, &mut tree, &renderer);
            let point = role.map(|role| bounds[&deletion_control_id(old, role).to_string()].center())
                .unwrap_or(Point::new(4.0, 4.0));
            let mut messages = removal_pointer_event(
                &mut element, &mut tree, &renderer, &node, point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            );
            messages.extend(removal_pointer_event(
                &mut element, &mut tree, &renderer, &node, point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ));
            assert!(matches!(messages.as_slice(), [Message::ProjectUi(Action::DismissDelete(request))] if *request == old));
            drop(element);
            let current = ready_deletion_retry(&mut app, worktree);
            assert_ne!(old, current);
            for message in messages { drop(app.update(message)); }
            drop(app.handle_project_action(Action::Dismiss));
            assert_eq!(app.project_ui.dialog.as_ref().unwrap().request, current);
            assert!(!app.project_ui.is_deleting(worktree));
            drop(app.handle_project_action(Action::DismissDelete(current)));
            assert!(app.project_ui.dialog.is_none());
            assert!(app.project_ui.pending_delete.is_empty());
        }
    }

    #[test]
    fn deletion_native_press_does_not_transfer_to_a_replacement_review() {
        use cosmic::iced::{advanced::widget::{Tree, tree}, mouse};
        for role in ["cancel", "confirm"] {
            let (mut app, _directory, worktree) = deletion_review_fixture();
            let old = app.project_ui.dialog.as_ref().unwrap().request;
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.dialog().unwrap();
            let mut state = Tree::new(element.as_widget());
            let (node, bounds) = removal_native_frame(&mut element, &mut state, &renderer);
            let point = bounds[&deletion_control_id(old, role).to_string()].center();
            assert!(removal_pointer_event(
                &mut element, &mut state, &renderer, &node, point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            ).is_empty());
            drop(element);
            let current = ready_deletion_retry(&mut app, worktree);
            let mut element = app.dialog().unwrap();
            // Actual pinned runtime ordering: named state is lifted before
            // keyed diff and restored afterward, with no empty frame between.
            tree::NAMED.with(|named| *named.borrow_mut() = state.take_all_named());
            state.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut state);
            tree::NAMED.with(|named| named.borrow_mut().clear());
            let (node, bounds) = removal_native_frame(&mut element, &mut state, &renderer);
            let current_point = bounds[&deletion_control_id(current, role).to_string()].center();
            assert_ne!(old, current);
            assert_eq!(point, current_point);
            assert!(removal_pointer_event(
                &mut element, &mut state, &renderer, &node, current_point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ).is_empty());
            assert!(removal_pointer_event(
                &mut element, &mut state, &renderer, &node, current_point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            ).is_empty());
            let messages = removal_pointer_event(
                &mut element, &mut state, &renderer, &node, current_point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            );
            assert_eq!(messages.len(), 1, "the current review remains usable");
            assert!(match &messages[0] {
                Message::ProjectUi(Action::DismissDelete(request)) if role == "cancel" => *request == current,
                Message::ProjectUi(Action::ConfirmDelete(request)) if role == "confirm" => *request == current,
                _ => false,
            });
            drop(element);
            for message in messages { drop(app.update(message)); }
            if role == "cancel" {
                assert!(app.project_ui.dialog.is_none());
            } else {
                let review = app.project_ui.dialog.as_ref().unwrap();
                assert_eq!(review.request, current);
                assert!(review.loading, "only a fresh current press starts its recheck");
            }
            assert!(!app.project_ui.is_deleting(worktree), "no worker was polled");
        }
    }

    fn queued_force_reviews() -> (Cagents, crate::model::OperationId, crate::model::OperationId) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let mut ids = Vec::new();
        for branch in ["branch-a", "branch-b"] {
            let request = BranchDeletion {
                id: crate::model::OperationId::new(), repository: "/repo".into(),
                branch: branch.into(), expected_oid: "a".repeat(40), warnings: Vec::new(),
                continuation: Some(fixture_branch_continuation()),
            };
            ids.push(request.id);
            app.project_ui.branch_requests.insert(request.id);
            drop(app.handle_project_action(Action::BranchDeleted {
                request,
                result: Err(crate::worktree_branch::BranchDeleteFailure {
                    kind: crate::worktree_branch::BranchDeleteFailureKind::Unmerged,
                    detail: "Original checked branch was not merged".into(),
                }),
            }));
        }
        assert_eq!(app.project_ui.force_branches.len(), 2);
        (app, ids[0], ids[1])
    }

    fn escape_project_review(app: &mut Cagents) {
        use cosmic::iced::keyboard::{self, key::{Code, Named}};
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Escape),
                modified_key: keyboard::Key::Named(Named::Escape),
                physical_key: keyboard::key::Physical::Code(Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(), text: None, repeat: false,
            },
            captured: false,
        }));
    }

    #[test]
    fn queued_force_old_keep_and_backdrop_preserve_the_next_branch() {
        use cosmic::iced::{Point, advanced::widget::Tree, mouse};
        for role in [None, Some("keep")] {
            let (mut app, old, current) = queued_force_reviews();
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.dialog().unwrap();
            let mut tree = Tree::new(element.as_widget());
            let (node, bounds) = removal_native_frame(&mut element, &mut tree, &renderer);
            let point = role.map(|role| bounds[&force_branch_control_id(old, role).to_string()].center())
                .unwrap_or(Point::new(4.0, 4.0));
            let mut messages = removal_pointer_event(
                &mut element, &mut tree, &renderer, &node, point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            );
            messages.extend(removal_pointer_event(
                &mut element, &mut tree, &renderer, &node, point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ));
            assert!(matches!(messages.as_slice(), [Message::ProjectUi(Action::KeepBranch(id))] if *id == old));
            drop(element);
            escape_project_review(&mut app);
            assert_eq!(app.project_ui.force_branches.front().unwrap().0.id, current);
            for message in messages { drop(app.update(message)); }
            drop(app.handle_project_action(Action::Dismiss));
            assert_eq!(app.project_ui.force_branches.front().unwrap().0.id, current);
            assert!(app.project_ui.branch_requests.is_empty());
            drop(app.handle_project_action(Action::KeepBranch(current)));
            assert!(app.project_ui.force_branches.is_empty());
        }
    }

    #[test]
    fn queued_force_native_press_does_not_transfer_after_escape() {
        use cosmic::iced::{advanced::widget::{Tree, tree}, mouse};
        for role in ["keep", "force"] {
            let (mut app, old, current) = queued_force_reviews();
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.dialog().unwrap();
            let mut state = Tree::new(element.as_widget());
            let (node, bounds) = removal_native_frame(&mut element, &mut state, &renderer);
            let point = bounds[&force_branch_control_id(old, role).to_string()].center();
            assert!(removal_pointer_event(
                &mut element, &mut state, &renderer, &node, point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            ).is_empty());
            drop(element);
            escape_project_review(&mut app);
            assert_eq!(app.project_ui.force_branches.front().unwrap().0.id, current);
            let mut element = app.dialog().unwrap();
            tree::NAMED.with(|named| *named.borrow_mut() = state.take_all_named());
            state.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut state);
            tree::NAMED.with(|named| named.borrow_mut().clear());
            let (node, bounds) = removal_native_frame(&mut element, &mut state, &renderer);
            let current_point = bounds[&force_branch_control_id(current, role).to_string()].center();
            assert_eq!(point, current_point);
            assert!(removal_pointer_event(
                &mut element, &mut state, &renderer, &node, current_point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ).is_empty());
            assert!(removal_pointer_event(
                &mut element, &mut state, &renderer, &node, current_point,
                mouse::Event::ButtonPressed(mouse::Button::Left),
            ).is_empty());
            let messages = removal_pointer_event(
                &mut element, &mut state, &renderer, &node, current_point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            );
            assert_eq!(messages.len(), 1);
            assert!(match &messages[0] {
                Message::ProjectUi(Action::KeepBranch(id)) if role == "keep" => *id == current,
                Message::ProjectUi(Action::ForceDeleteBranch(id)) if role == "force" => *id == current,
                _ => false,
            });
            drop(element);
            for message in messages { drop(app.update(message)); }
            assert!(app.project_ui.force_branches.is_empty());
            assert_eq!(app.project_ui.branch_requests.contains(&current), role == "force");
            assert!(!app.project_ui.branch_requests.contains(&old));
            // The returned force-delete task is deliberately never polled.
        }
    }

    #[test]
    fn opened_project_and_worktree_removal_focus_the_cancel_button() {
        let repository = PathBuf::from("/focus-repo");
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
        let task = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: None,
            command: ProjectCommand::Remove,
        });
        assert_eq!(
            super::super::tests::focused_element(app.project_dialog_view().unwrap(), task),
            Some(removal_control_id(
                app.project_ui.removal_request().unwrap(),
                "cancel"
            )),
        );
        // The primary worktree refusal is synchronous, so the actual opening
        // task contains only focus operations and cannot start a daemon.
        let task = app.open_worktree_delete_dialog(Target {
            project,
            worktree: Some(primary.id),
            repository: repository.clone(),
            path: repository,
            label: "main".into(),
        });
        assert!(app.project_ui.remove_dialog.is_none());
        assert!(app.project_ui.dialog.as_ref().unwrap().blocker.is_some());
        assert_eq!(
            super::super::tests::focused_element(app.project_dialog_view().unwrap(), task),
            Some(deletion_control_id(app.project_ui.dialog.as_ref().unwrap().request, "cancel")),
        );
    }

    #[test]
    fn loaded_cleanup_policy_focuses_its_actual_editable_field() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.project_ui.cleanup_dialog = Some(CleanupPolicyDialog {
            repository: PathBuf::from("/focus-repo"),
            name: "repo".into(),
            directories: String::new(),
            loading: true,
            error: None,
            request: 19,
        });
        let loading_focus =
            iced::widget::operation::focus(app.project_ui.focus_target()).map(cosmic::Action::App);
        assert_eq!(
            super::super::tests::focused_element(app.project_dialog_view().unwrap(), loading_focus),
            Some(widget::Id::new("dialog.project.cleanup.cancel")),
            "the disabled loading field must not consume initial focus",
        );
        let task = app.handle_project_action(Action::CleanupPolicyRead {
            request: 19,
            result: Ok(vec!["target".into()]),
        });
        assert_eq!(
            super::super::tests::focused_element(app.project_dialog_view().unwrap(), task),
            Some(widget::Id::new("dialog.project.cleanup.directories")),
        );
    }

    fn row(app: &Cagents, id: WorktreeId) -> Option<&SidebarWorktree> {
        app.sidebar_model
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .find(|worktree| worktree.id == id)
    }

    #[test]
    fn branch_delete_defaults_require_exact_merged_head_and_repository() {
        let repository = PathBuf::from("/repo");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let mut linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("linked"),
            Some("feature".into()),
        );
        let source =
            crate::github::GitHubRepoKey::new("github.com", None, "example", "repo").unwrap();
        linked.head_repository = Some(source.clone());
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let target = app.project_target(project, Some(linked.id)).unwrap();
        let mut snapshot = clean_snapshot();
        snapshot.head_oid = Some("a".repeat(40));
        let mut pr = crate::sidebar::SidebarPullRequest::new(
            project,
            1,
            "Merged feature".into(),
            Url::parse("https://github.com/example/repo/pull/1").unwrap(),
            "feature".into(),
            None,
            false,
            crate::sidebar::CheckStatus::Passing,
        );
        pr.state = Some(crate::sidebar::WorktreePullRequestState::Merged);
        pr.head_oid = snapshot.head_oid.clone();
        pr.head_repository = Some(source.clone());
        snapshot.head_repository = Some(source);
        app.sidebar_model.projects[0].pull_requests.push(pr);
        assert!(app.delete_branch_default(&target, &snapshot));
        snapshot.head_repository =
            Some(GitHubRepoKey::new("github.com", None, "EXAMPLE", "Repo").unwrap());
        app.sidebar_model.projects[0].pull_requests[0].head_repository =
            Some(GitHubRepoKey::new("github.com", None, "Example", "REPO").unwrap());
        assert!(
            app.delete_branch_default(&target, &snapshot),
            "GitHub API and remote URL spelling do not change repository identity"
        );
        // The safety snapshot already contains a local name, including a
        // literal name that itself begins with refs/heads/.
        snapshot.branch = Some("refs/heads/feature".into());
        assert!(!app.delete_branch_default(&target, &snapshot));
        app.sidebar_model.projects[0].pull_requests[0].head_ref = "refs/heads/feature".into();
        assert!(app.delete_branch_default(&target, &snapshot));
        snapshot.branch = Some("feature".into());
        app.sidebar_model.projects[0].pull_requests[0].head_ref = "feature".into();
        snapshot.head_oid = Some("b".repeat(40));
        assert!(!app.delete_branch_default(&target, &snapshot));
        snapshot.head_oid = Some("a".repeat(40));
        app.sidebar_model.projects[0].pull_requests[0].state =
            Some(crate::sidebar::WorktreePullRequestState::Open);
        assert!(!app.delete_branch_default(&target, &snapshot));
        app.sidebar_model.projects[0].pull_requests[0].state =
            Some(crate::sidebar::WorktreePullRequestState::Merged);
        snapshot.head_repository =
            Some(GitHubRepoKey::new("github.com", None, "different", "repo").unwrap());
        assert!(!app.delete_branch_default(&target, &snapshot));
        snapshot.head_repository = None;
        assert!(!app.delete_branch_default(&target, &snapshot));
        app.sidebar_model.projects[0].pull_requests[0].head_repository = None;
        assert!(!app.delete_branch_default(&target, &snapshot));
    }

    #[test]
    fn post_merge_receipt_defaults_branch_without_waiting_for_sidebar_cache() {
        for changed in 0..9 {
            let repository = PathBuf::from("/repo");
            let primary = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.clone(),
                Some("main".into()),
            );
            let source = GitHubRepoKey::new("github.com", None, "example", "repo").unwrap();
            let mut linked = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.join("linked"),
                Some("feature".into()),
            );
            linked.head_repository = Some(source.clone());
            let (mut app, _state, project) = deletable_app(&repository, &linked, &primary);
            assert!(app.sidebar_model.projects[0].pull_requests.is_empty());
            let mut pr: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
                "number": 7, "title": "Merged feature", "state": "MERGED",
                "headRefName": "feature", "headRefOid": "a".repeat(40),
                "headRepository": {"name": "repo"}, "headRepositoryOwner": {"login": "example"},
                "baseRefName": "main", "updatedAt": "now", "url": "https://github.com/example/repo/pull/7"
            })).unwrap();
            match changed {
                1 => pr.state.0 = "OPEN".into(),
                2 => pr.head_ref_name = "other".into(),
                3 => pr.head_ref_oid = "b".repeat(40),
                4 => pr.head_repository_owner.as_mut().unwrap().login = "fork".into(),
                _ => {}
            }
            let expected_source = if changed == 8 {
                GitHubRepoKey::new("github.com", None, "ExAmPlE", "Repo").unwrap()
            } else {
                source.clone()
            };
            drop(app.open_worktree_delete_dialog_for_head(
                project,
                linked.id,
                DeletionHead {
                    head_repository: expected_source,
                    branch: "feature".into(),
                    head_oid: "a".repeat(40),
                    merged_pull_request: (changed != 5).then(|| Arc::new(pr)),
                },
            ));
            let request = app.project_ui.dialog.as_ref().unwrap().request;
            drop(app.handle_project_action(Action::DeleteOptionsRead {
                request,
                result: Ok(DeleteOptions {
                    branch_elsewhere: changed == 6,
                    stale_lock: false,
                    lock_error: None,
                }),
            }));
            let mut snapshot = clean_snapshot();
            snapshot.head_oid = Some("a".repeat(40));
            snapshot.head_repository = Some(source);
            if changed == 7 {
                snapshot.head_repository = None;
            }
            if changed == 8 {
                snapshot.head_repository =
                    Some(GitHubRepoKey::new("github.com", None, "EXAMPLE", "REPO").unwrap());
            }
            drop(app.handle_project_action(Action::SafetyRead {
                request,
                confirming: false,
                result: fixture_safety_result(&app, Ok((snapshot, AgentCensus::default())), false),
            }));
            assert_eq!(
                app.project_ui.dialog.as_ref().unwrap().delete_branch,
                changed == 0 || changed == 8,
                "only the actual merged receipt, current source/head and available branch allow the default"
            );
        }
    }

    #[test]
    fn working_agents_need_separate_counted_consent() {
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
        let target = app.project_target(project, Some(linked.id)).unwrap();
        drop(app.open_worktree_delete_dialog(target.clone()));
        // No current-window card exists for this detached daemon agent.
        assert!(app.durable.cards.is_empty());
        let agent = BusyAgent {
            session: TerminalId::new(),
            instance: uuid::Uuid::new_v4(),
            close_revision: Some(1),
            turn_generation: Some(3),
            session_instance: uuid::Uuid::new_v4(),
            working_agent: true,
        };
        let mut uncertain = agent.clone();
        uncertain.session = TerminalId::new();
        uncertain.instance = uuid::Uuid::new_v4();
        uncertain.session_instance = uuid::Uuid::new_v4();
        uncertain.working_agent = false;
        uncertain.close_revision = None;
        uncertain.turn_generation = None;
        app.project_ui.dialog.as_mut().unwrap().busy_agents =
            vec![agent.clone(), uncertain.clone()];
        app.project_ui.dialog.as_mut().unwrap().session_census = vec![agent, uncertain];
        assert_eq!(
            app.worktree_activity_counts(&target),
            (1, 1),
            "unknown live shells must not inflate the positively working-agent count"
        );
        confirm_deletion_fixture(&mut app);
        assert!(!app.project_ui.is_deleting(linked.id));
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.handle_project_action(Action::BusyAgentsConfirmed(request, true)));
        confirm_deletion_fixture(&mut app);
        assert!(app.project_ui.is_deleting(linked.id));
    }

    #[test]
    fn replacing_an_agent_or_its_turn_invalidates_counted_consent() {
        for change in 0..6 {
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
            let target = app.project_target(project, Some(linked.id)).unwrap();
            drop(app.open_worktree_delete_dialog(target));
            let request = app.project_ui.dialog.as_ref().unwrap().request;
            let original = BusyAgent {
                session: TerminalId::new(),
                instance: uuid::Uuid::new_v4(),
                close_revision: Some(1),
                turn_generation: Some(3),
                session_instance: uuid::Uuid::new_v4(),
                working_agent: true,
            };
            app.project_ui.dialog.as_mut().unwrap().busy_agents = vec![original.clone()];
            app.project_ui.dialog.as_mut().unwrap().session_census = vec![original.clone()];
            confirm_deletion_fixture(&mut app);
            drop(app.handle_project_action(Action::BusyAgentsConfirmed(request, true)));
            drop(app.handle_project_action(Action::ConfirmDelete(request)));
            let mut changed = original.clone();
            match change {
                0 => changed.session = TerminalId::new(),
                1 => changed.instance = uuid::Uuid::new_v4(),
                2 => changed.close_revision = Some(2),
                3 => changed.turn_generation = Some(4),
                4 => changed.session_instance = uuid::Uuid::new_v4(),
                _ => changed.working_agent = false,
            }
            drop(app.handle_project_action(Action::SafetyRead {
                request,
                confirming: true,
                result: fixture_safety_result(
                    &app,
                    Ok((
                        clean_snapshot(),
                        AgentCensus {
                            agents: vec![changed.clone()],
                            census: vec![changed],
                        },
                    )),
                    true,
                ),
            }));
            assert!(!app.project_ui.is_deleting(linked.id));
            assert!(
                app.project_ui
                    .dialog
                    .as_ref()
                    .unwrap()
                    .busy_confirmed
                    .is_none()
            );
        }
    }

    #[test]
    fn an_idle_actor_change_requires_a_fresh_deletion_confirmation() {
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
        let target = app.project_target(project, Some(linked.id)).unwrap();
        drop(app.open_worktree_delete_dialog(target));
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        let mut actor = BusyAgent {
            session: TerminalId::new(),
            instance: uuid::Uuid::new_v4(),
            close_revision: Some(1),
            turn_generation: Some(0),
            session_instance: uuid::Uuid::new_v4(),
            working_agent: false,
        };
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request,
            result: Ok(DeleteOptions {
                branch_elsewhere: false,
                stale_lock: false,
                lock_error: None,
            }),
        }));
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: false,
            result: fixture_safety_result(
                &app,
                Ok((
                    clean_snapshot(),
                    AgentCensus {
                        agents: vec![],
                        census: vec![actor.clone()],
                    },
                )),
                false,
            ),
        }));
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        assert!(
            !app.project_ui.dialog.as_ref().unwrap().loading,
            "even idle sessions require explicit consent to stop all their ongoing work"
        );
        assert!(!app.project_ui.dialog.as_ref().unwrap().sessions_consented());
        drop(app.handle_project_action(Action::BusyAgentsConfirmed(request, true)));
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        assert!(
            app.project_ui.dialog.as_ref().unwrap().loading,
            "consent starts a fresh safety/census read before stopping sessions"
        );
        actor.close_revision = Some(2);
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: true,
            result: fixture_safety_result(
                &app,
                Ok((
                    clean_snapshot(),
                    AgentCensus {
                        agents: vec![],
                        census: vec![actor],
                    },
                )),
                true,
            ),
        }));
        assert!(!app.project_ui.is_deleting(linked.id));
        assert!(
            app.project_ui
                .dialog
                .as_ref()
                .unwrap()
                .failure
                .as_deref()
                .unwrap()
                .contains("changed")
        );
    }

    #[test]
    fn post_merge_cleanup_refuses_a_replaced_branch_source_or_head_before_stopping_sessions() {
        for changed in 0..5 {
            let repository = PathBuf::from("/repo");
            let primary = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.clone(),
                Some("main".into()),
            );
            let source = GitHubRepoKey::new("github.com", None, "example", "repo").unwrap();
            let mut linked = SidebarWorktree::from_durable(
                WorktreeId::new(),
                repository.join("linked"),
                Some("feature".into()),
            );
            linked.head_repository = Some(source.clone());
            let (mut app, _state, project) = deletable_app(&repository, &linked, &primary);
            let oid = "a".repeat(40);
            drop(app.open_worktree_delete_dialog_for_head(
                project,
                linked.id,
                DeletionHead {
                    head_repository: source.clone(),
                    branch: "feature".into(),
                    head_oid: oid.clone(),
                    merged_pull_request: None,
                },
            ));
            let request = app.project_ui.dialog.as_ref().unwrap().request;
            drop(app.handle_project_action(Action::DeleteOptionsRead {
                request,
                result: Ok(DeleteOptions {
                    branch_elsewhere: false,
                    stale_lock: false,
                    lock_error: None,
                }),
            }));
            let mut snapshot = clean_snapshot();
            snapshot.head_oid = Some(oid);
            snapshot.head_repository = Some(source);
            drop(app.handle_project_action(Action::SafetyRead {
                request,
                confirming: false,
                result: fixture_safety_result(
                    &app,
                    Ok((snapshot.clone(), AgentCensus::default())),
                    false,
                ),
            }));
            assert!(app.project_ui.dialog.as_ref().unwrap().safety.is_some());
            drop(app.handle_project_action(Action::ConfirmDelete(request)));
            match changed {
                0 => snapshot.branch = Some("replacement".into()),
                1 => snapshot.head_oid = Some("b".repeat(40)),
                2 => {
                    app.sidebar_model.projects[0]
                        .worktrees
                        .iter_mut()
                        .find(|row| row.id == linked.id)
                        .unwrap()
                        .head_repository =
                        Some(GitHubRepoKey::new("github.com", None, "different", "repo").unwrap())
                }
                3 => {
                    snapshot.head_repository = Some(
                        GitHubRepoKey::new("github.com", None, "fresh-source", "repo").unwrap(),
                    )
                }
                _ => snapshot.head_repository = None,
            }
            drop(app.handle_project_action(Action::SafetyRead {
                request,
                confirming: true,
                result: fixture_safety_result(&app, Ok((snapshot, AgentCensus::default())), true),
            }));
            let dialog = app.project_ui.dialog.as_ref().unwrap();
            assert!(dialog.safety.is_none());
            assert!(
                dialog
                    .blocker
                    .as_deref()
                    .is_some_and(|message| message.contains("head changed"))
            );
            assert!(!app.project_ui.is_deleting(linked.id));
            assert!(
                !app.project_ui
                    .deletion_authorizations
                    .contains_key(&linked.id)
            );
        }
    }

    #[test]
    fn whole_session_consent_retains_lifetimes_instead_of_turn_revisions() {
        let actor = BusyAgent {
            session: TerminalId::new(),
            instance: uuid::Uuid::new_v4(),
            close_revision: Some(1),
            turn_generation: Some(1),
            session_instance: uuid::Uuid::new_v4(),
            working_agent: false,
        };
        let original = AgentCensus {
            agents: vec![],
            census: vec![actor.clone()],
        };
        let expected = worktree_session_stop_consent(&original).unwrap();
        assert_eq!(
            expected.sessions.len(),
            1,
            "idle sessions are included in explicit stop consent"
        );
        assert_eq!(expected.sessions[0].instance, actor.session_instance);
        let mut next_turn = actor.clone();
        next_turn.close_revision = Some(2);
        next_turn.turn_generation = Some(2);
        assert_eq!(
            worktree_session_stop_consent(&AgentCensus {
                agents: vec![next_turn.clone()],
                census: vec![next_turn]
            })
            .unwrap(),
            expected,
            "the explicit checkbox covers all ongoing work within the same actor lifetime"
        );
        let mut replaced = actor.clone();
        replaced.session_instance = uuid::Uuid::new_v4();
        assert_ne!(
            worktree_session_stop_consent(&AgentCensus {
                agents: vec![],
                census: vec![replaced.clone()]
            })
            .unwrap(),
            expected
        );
        assert!(
            worktree_session_stop_consent(&AgentCensus {
                agents: vec![],
                census: vec![actor.clone(), replaced]
            })
            .is_err()
        );
        let mut unknown = actor;
        unknown.session_instance = uuid::Uuid::nil();
        assert!(
            worktree_session_stop_consent(&AgentCensus {
                agents: vec![],
                census: vec![unknown]
            })
            .is_err()
        );
    }

    #[test]
    fn malformed_or_duplicate_busy_census_is_never_treated_as_idle() {
        assert!(
            checked_busy_worktree_agents(crate::worktree_control::Reply::StaleLock {
                unlocked: false
            })
            .is_err()
        );
        let agent = BusyAgent {
            session: TerminalId::new(),
            instance: uuid::Uuid::new_v4(),
            close_revision: Some(1),
            turn_generation: Some(1),
            session_instance: uuid::Uuid::new_v4(),
            working_agent: true,
        };
        assert!(
            checked_busy_worktree_agents(crate::worktree_control::Reply::BusySessions {
                agents: vec![agent.clone()],
                census: vec![]
            })
            .is_err()
        );
        assert!(
            checked_busy_worktree_agents(crate::worktree_control::Reply::BusySessions {
                agents: vec![],
                census: vec![agent.clone()],
            })
            .is_err(),
            "positively working actors cannot disappear from the warning subset"
        );
        assert!(
            checked_busy_worktree_agents(crate::worktree_control::Reply::BusySessions {
                agents: vec![agent.clone(), agent.clone()],
                census: vec![agent]
            })
            .is_err()
        );
    }

    #[test]
    fn deletion_controls_and_late_confirmations_wait_for_their_exposed_dialog() {
        for update in [false, true] {
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
            let target = app.project_target(project, Some(linked.id)).unwrap();
            drop(app.open_worktree_delete_dialog(target));
            let request = app.project_ui.dialog.as_ref().unwrap().request;
            drop(app.handle_project_action(Action::DeleteOptionsRead {
                request,
                result: Ok(DeleteOptions {
                    branch_elsewhere: false,
                    stale_lock: false,
                    lock_error: None,
                }),
            }));
            drop(app.handle_project_action(Action::SafetyRead {
                request,
                confirming: false,
                result: fixture_safety_result(
                    &app,
                    Ok((clean_snapshot(), AgentCensus::default())),
                    false,
                ),
            }));
            app.update_prepared = update;
            app.settings_reset_confirm = !update;
            drop(app.handle_project_action(Action::DeleteBranchChanged(request, true)));
            assert!(!app.project_ui.dialog.as_ref().unwrap().delete_branch);
            drop(app.handle_project_action(Action::BusyAgentsConfirmed(request, true)));
            assert!(
                app.project_ui
                    .dialog
                    .as_ref()
                    .unwrap()
                    .busy_confirmed
                    .is_none()
            );
            drop(app.handle_project_action(Action::ConfirmDelete(request)));
            drop(app.handle_project_action(Action::SafetyRead {
                request,
                confirming: true,
                result: fixture_safety_result(
                    &app,
                    Ok((clean_snapshot(), AgentCensus::default())),
                    true,
                ),
            }));
            assert!(!app.project_ui.is_deleting(linked.id));
            assert!(app.project_ui.dialog.is_some());
        }
        assert!(user_message_blocked_during_update(&Message::ProjectUi(
            Action::ForceDeleteBranch(crate::model::OperationId::new())
        )));
        assert!(user_message_blocked_during_update(&Message::ProjectUi(
            Action::BusyAgentsConfirmed(0, true)
        )));
    }

    #[test]
    fn stale_same_role_delete_controls_cannot_authorize_replacement_dialog() {
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
        let target = app.project_target(project, Some(linked.id)).unwrap();
        drop(app.open_worktree_delete_dialog(target.clone()));
        let stale = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.open_worktree_delete_dialog(target));
        let current = app.project_ui.dialog.as_ref().unwrap().request;
        assert_ne!(stale, current);
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request: current,
            result: Ok(DeleteOptions {
                branch_elsewhere: false,
                stale_lock: false,
                lock_error: None,
            }),
        }));
        drop(app.handle_project_action(Action::SafetyRead {
            request: current,
            confirming: false,
            result: fixture_safety_result(
                &app,
                Ok((clean_snapshot(), AgentCensus::default())),
                false,
            ),
        }));
        drop(app.handle_project_action(Action::DeleteBranchChanged(stale, true)));
        assert!(!app.project_ui.dialog.as_ref().unwrap().delete_branch);
        let confirm = app.handle_project_action(Action::ConfirmDelete(stale));
        assert!(cosmic::iced::runtime::task::into_stream(confirm).is_none());
        assert!(!app.project_ui.dialog.as_ref().unwrap().loading);
        assert!(!app.project_ui.is_deleting(linked.id));
        app.project_ui.dialog.as_mut().unwrap().busy_agents = vec![BusyAgent {
            session: TerminalId::new(),
            instance: uuid::Uuid::new_v4(),
            close_revision: Some(1),
            turn_generation: Some(1),
            session_instance: uuid::Uuid::new_v4(),
            working_agent: true,
        }];
        drop(app.handle_project_action(Action::BusyAgentsConfirmed(stale, true)));
        assert!(
            app.project_ui
                .dialog
                .as_ref()
                .unwrap()
                .busy_confirmed
                .is_none()
        );
        drop(app.handle_project_action(Action::BusyAgentsConfirmed(current, true)));
        assert!(
            app.project_ui
                .dialog
                .as_ref()
                .unwrap()
                .busy_confirmed
                .is_some()
        );
    }

    #[test]
    fn stale_policy_edit_or_save_cannot_mutate_another_project_dialog() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.project_ui.cleanup_dialog = Some(CleanupPolicyDialog {
            repository: "/project-b".into(),
            name: "Project B".into(),
            directories: "target".into(),
            loading: false,
            error: None,
            request: 18,
        });
        drop(app.handle_project_action(Action::CleanupPolicyChanged(17, "private-data".into())));
        assert_eq!(
            app.project_ui.cleanup_dialog.as_ref().unwrap().directories,
            "target"
        );
        let save = app.handle_project_action(Action::SaveCleanupPolicy(17));
        assert!(cosmic::iced::runtime::task::into_stream(save).is_none());
        assert!(!app.project_ui.cleanup_dialog.as_ref().unwrap().loading);
        drop(app.handle_project_action(Action::CleanupPolicyChanged(18, "node_modules".into())));
        assert_eq!(
            app.project_ui.cleanup_dialog.as_ref().unwrap().directories,
            "node_modules"
        );
    }

    #[test]
    fn force_confirmation_blocks_retained_lower_delete_controls_and_async_confirmation() {
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
        let target = app.project_target(project, Some(linked.id)).unwrap();
        drop(app.open_worktree_delete_dialog(target));
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request,
            result: Ok(DeleteOptions {
                branch_elsewhere: false,
                stale_lock: false,
                lock_error: None,
            }),
        }));
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: false,
            result: fixture_safety_result(
                &app,
                Ok((clean_snapshot(), AgentCensus::default())),
                false,
            ),
        }));
        let force = BranchDeletion {
            id: crate::model::OperationId::new(),
            repository,
            branch: "other".into(),
            expected_oid: "a".repeat(40),
            warnings: vec![],
            continuation: None,
        };
        app.project_ui
            .force_branches
            .push_back((force, "Unmerged".into()));
        drop(app.handle_project_action(Action::DeleteBranchChanged(request, true)));
        assert!(!app.project_ui.dialog.as_ref().unwrap().delete_branch);
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: true,
            result: fixture_safety_result(
                &app,
                Ok((clean_snapshot(), AgentCensus::default())),
                true,
            ),
        }));
        assert!(!app.project_ui.is_deleting(linked.id));
        assert!(app.project_ui.dialog.is_some());
        let dismiss = app.project_dialog_dismiss_action();
        drop(app.handle_project_action(dismiss));
        assert!(app.project_ui.force_branches.is_empty());
        assert!(
            app.project_ui.dialog.is_some(),
            "dismissing the upper force dialog preserves lower deletion"
        );
    }

    fn fixture_branch_continuation() -> BranchContinuation {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(WorktreeId::new()),
            repository: "/repo".into(),
            path: "/repo/linked".into(),
            label: "fixture".into(),
        };
        drop(app.open_worktree_delete_dialog(target));
        BranchContinuation(
            fixture_safety_result(&app, Ok((clean_snapshot(), AgentCensus::default())), false)
                .unwrap()
                .authorization
                .unwrap(),
        )
    }

    #[test]
    fn unmerged_branch_refusal_requires_a_separate_force_confirmation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let request = BranchDeletion {
            id: crate::model::OperationId::new(),
            repository: "/repo".into(),
            branch: "feature".into(),
            expected_oid: "a".repeat(40),
            warnings: Vec::new(),
            continuation: Some(fixture_branch_continuation()),
        };
        app.project_ui.branch_requests.insert(request.id);
        drop(app.handle_project_action(Action::BranchDeleted {
            request: request.clone(),
            result: Err(crate::worktree_branch::BranchDeleteFailure {
                kind: crate::worktree_branch::BranchDeleteFailureKind::Unmerged,
                detail: "Branch is not merged".into(),
            }),
        }));
        assert_eq!(app.project_ui.force_branches.len(), 1);
        assert!(app.project_ui.branch_requests.is_empty());
        drop(app.handle_project_action(Action::KeepBranch(request.id)));
        assert!(app.project_ui.force_branches.is_empty());
        // Checked-out branches never offer the destructive fallback.
        app.project_ui.branch_requests.insert(request.id);
        drop(app.handle_project_action(Action::BranchDeleted {
            request,
            result: Err(crate::worktree_branch::BranchDeleteFailure {
                kind: crate::worktree_branch::BranchDeleteFailureKind::CheckedOutElsewhere,
                detail: "Checked out elsewhere".into(),
            }),
        }));
        assert!(app.project_ui.force_branches.is_empty());
    }

    #[test]
    fn unmerged_without_original_branch_authority_does_not_offer_force() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let request = BranchDeletion {
            id: crate::model::OperationId::new(),
            repository: "/repo".into(),
            branch: "feature".into(),
            expected_oid: "a".repeat(40),
            warnings: Vec::new(),
            continuation: None,
        };
        app.project_ui.branch_requests.insert(request.id);
        drop(app.handle_project_action(Action::BranchDeleted {
            request,
            result: Err(crate::worktree_branch::BranchDeleteFailure {
                kind: crate::worktree_branch::BranchDeleteFailureKind::Unmerged,
                detail: "Unmerged without authority".into(),
            }),
        }));
        assert!(app.project_ui.force_branches.is_empty());
    }

    #[test]
    fn stale_branch_buttons_cannot_confirm_a_later_branch() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let stale = crate::model::OperationId::new();
        let current = BranchDeletion {
            id: crate::model::OperationId::new(),
            repository: "/repo".into(),
            branch: "current".into(),
            expected_oid: "b".repeat(40),
            warnings: Vec::new(),
            continuation: None,
        };
        app.project_ui
            .force_branches
            .push_back((current.clone(), "Unmerged".into()));
        drop(app.handle_project_action(Action::ForceDeleteBranch(stale)));
        drop(app.handle_project_action(Action::KeepBranch(stale)));
        assert_eq!(
            app.project_ui.force_branches.front().unwrap().0.id,
            current.id
        );
        assert!(app.project_ui.branch_requests.is_empty());
        drop(app.handle_project_action(Action::KeepBranch(current.id)));
        assert!(app.project_ui.force_branches.is_empty());
    }

    #[test]
    fn confirmed_rpc_keeps_branch_and_file_discard_consent_separate() {
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(WorktreeId::new()),
            repository: "/repo".into(),
            path: "/repo/linked".into(),
            label: "feature".into(),
        };
        let mut snapshot = clean_snapshot();
        snapshot.unpushed_commits = 3;
        let crate::worktree_control::Operation::Remove {
            force,
            delete_branch,
            expected_fingerprint,
            ..
        } = confirmed_removal_operation(&target, &snapshot, uuid::Uuid::new_v4()).unwrap()
        else {
            panic!("wrong operation")
        };
        assert!(
            !force,
            "unpushed commits do not authorize force-removing files"
        );
        assert!(
            !delete_branch,
            "branch deletion gets its own typed result and second confirmation"
        );
        assert_eq!(expected_fingerprint.as_deref(), Some("fixture"));
        snapshot.untracked = 1;
        assert!(matches!(
            confirmed_removal_operation(&target, &snapshot, uuid::Uuid::new_v4()).unwrap(),
            crate::worktree_control::Operation::Remove { force: true, .. }
        ));
        use std::os::unix::ffi::OsStringExt;
        let invalid = Target {
            path: PathBuf::from(std::ffi::OsString::from_vec(
                b"/repo/non-utf8-\xff".to_vec(),
            )),
            ..target
        };
        assert!(confirmed_removal_operation(&invalid, &snapshot, uuid::Uuid::new_v4()).is_err());
    }

    #[test]
    fn late_session_closure_cannot_release_a_new_deletion_attempt() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = app.project_ui.begin_deleting("/repo/linked".into());
        let id = target.worktree.unwrap();
        let started_at = app.project_ui.deletions[&id].started_at;
        drop(app.handle_project_action(Action::DeletionSessionsClosed {
            target,
            started_at: started_at - Duration::from_secs(1),
            result: Ok(None),
        }));
        assert!(app.project_ui.released_delete_protections.is_empty());
        assert!(app.project_ui.is_deleting(id));
    }

    #[test]
    fn stale_removal_completion_cannot_retire_a_new_attempt() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = app.project_ui.begin_deleting("/repo/linked".into());
        let id = target.worktree.unwrap();
        let started_at = app.project_ui.deletions[&id].started_at;
        drop(app.handle_project_action(Action::RemovalCompleted {
            target,
            started_at: started_at - Duration::from_secs(1),
            result: Ok(RemovalOutcome {
                warnings: Vec::new(),
                continuation: None,
            }),
        }));
        assert!(app.project_ui.deletions.contains_key(&id));
        assert!(app.project_ui.deletion_warnings.is_empty());
    }

    #[test]
    fn stale_protection_release_cannot_start_a_new_deletion() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let target = app.project_ui.begin_deleting("/repo/linked".into());
        let id = target.worktree.unwrap();
        let started_at = app.project_ui.deletions[&id].started_at;
        app.project_ui.released_delete_protections.insert(id);
        let stale = app.handle_project_action(Action::DeletionProtectionsReleased {
            target: target.clone(),
            started_at: started_at - Duration::from_secs(1),
            result: Ok(()),
        });
        assert!(cosmic::iced::runtime::task::into_stream(stale).is_none());
        assert!(app.project_ui.is_deleting(id));
        app.project_ui.released_delete_protections.remove(&id);
        let unreleased = app.handle_project_action(Action::DeletionProtectionsReleased {
            target,
            started_at,
            result: Ok(()),
        });
        assert!(cosmic::iced::runtime::task::into_stream(unreleased).is_none());
    }

    #[test]
    fn delayed_automatic_success_preserves_new_terminal_intent_and_refreshed_same_path_row() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("repository");
        let linked = root.join("linked");
        git(&root, &["init", "-b", "main", "repository"]);
        git(&repository, &["commit", "--allow-empty", "-m", "base"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "original",
                linked.to_str().unwrap(),
            ],
        );
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::new();
        let receipt = crate::worktree_cleanup::RemovedWorktree {
            common_dir: service.common_dir(&repository, &cancellation).unwrap(),
            admin_dir: service.git_dir(&linked, &cancellation).unwrap(),
            path: linked.clone(),
            branch: Some("refs/heads/original".into()),
        };
        let mut app = super::super::tests::context_inspector_test_app(false);
        let (session, _) = SessionRuntime::open(
            root.join("window.sqlite3"),
            crate::model::BootId::new("automatic-window"),
        )
        .unwrap();
        app.session = Some(session);
        let first = super::super::load_repository(&repository, None, cancellation.clone()).unwrap();
        let project = first.project.id;
        drop(app.repository_load_task(repository.clone(), Some(project), false));
        let first_generation = app.repository_generations[&project];
        drop(app.update(Message::RepositoryLoaded {
            project_id: project,
            generation: first_generation,
            open_cards: false,
            result: Ok(Box::new(first)),
        }));
        let id = app
            .sidebar_model
            .project(project)
            .unwrap()
            .worktrees
            .iter()
            .find(|row| row.path == linked)
            .unwrap()
            .id;
        let target = app.project_target(project, Some(id)).unwrap();
        drop(app.add_repository_card(project, target.repository.clone(), linked.clone()));
        let original_repository = app.durable.cards.last().unwrap().id;
        let custody = app.capture_removal_projections(target.clone());
        let request = custody.request;
        assert_eq!(custody.owners.len(), 1);
        app.project_ui.automatic_in_flight.insert(id, custody);
        // Actual Git changes and the actual completed repository-read reducer
        // put a successor row at the same path before the old reply arrives.
        git(
            &repository,
            &["worktree", "remove", linked.to_str().unwrap()],
        );
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "successor",
                linked.to_str().unwrap(),
            ],
        );
        let newer =
            super::super::load_repository(&repository, Some(project), cancellation).unwrap();
        drop(app.repository_load_task(repository.clone(), Some(project), false));
        let newer_generation = app.repository_generations[&project];
        drop(app.update(Message::RepositoryLoaded {
            project_id: project,
            generation: newer_generation,
            open_cards: false,
            result: Ok(Box::new(newer)),
        }));
        assert_eq!(row(&app, id).unwrap().branch.as_deref(), Some("successor"));
        let before = app
            .durable
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<HashSet<_>>();
        // This real menu control inserts its Loading descriptor synchronously;
        // no provider/native start task is polled by this reducer fixture.
        drop(app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::NewTerminal,
        }));
        let fresh = app
            .durable
            .cards
            .iter()
            .find(|card| !before.contains(&card.id) && matches!(card.kind, CardKind::Terminal(_)))
            .unwrap()
            .clone();
        assert!(matches!(fresh.kind, CardKind::Terminal(_)));
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(fresh.id, linked.clone());
        app.worktree_offsets.insert(linked.clone(), 37.0);
        app.worktree_focus.insert(linked.clone(), fresh.id);
        let active = app.active_workspace.clone();
        let focused = app.workspace.focused();
        drop(app.handle_project_action(Action::AutomaticallyDeleted {
            warnings: Vec::new(),
            target: target.clone(),
            request,
            result: Ok(receipt.clone()),
        }));
        assert_eq!(app.card_state(fresh.id).unwrap().kind, fresh.kind);
        assert!(app.card_state(fresh.id).unwrap().existence.is_open());
        assert!(
            app.card_state(original_repository)
                .unwrap()
                .existence
                .is_open(),
            "new inventory supersedes old path-only repository projection custody"
        );
        assert_eq!(app.durable.card_scopes.get(&fresh.id), Some(&linked));
        assert_eq!(app.active_workspace, active);
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.worktree_offsets.get(&linked), Some(&37.0));
        assert_eq!(app.worktree_focus.get(&linked), Some(&fresh.id));
        assert_eq!(row(&app, id).unwrap().branch.as_deref(), Some("successor"));
        assert!(
            app.repository_generations[&project] > newer_generation,
            "completion requests actual inventory"
        );
        let session = app.session.as_ref().unwrap();
        session.flush(Duration::from_secs(5)).unwrap();
        assert!(
            session
                .operation_store()
                .load_worktree(id)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
        // A repeated old message cannot consume a later cleanup's custody.
        let next = app.capture_removal_projections(target.clone());
        let next_request = next.request;
        app.project_ui.automatic_in_flight.insert(id, next);
        drop(app.handle_project_action(Action::AutomaticallyDeleted {
            warnings: Vec::new(),
            target,
            request,
            result: Ok(receipt),
        }));
        assert_eq!(
            app.project_ui.automatic_in_flight[&id].request,
            next_request
        );
    }

    #[test]
    fn delayed_manual_success_preserves_adopted_channel_actor_and_successor_inventory() {
        use crate::agent_execution::{Agent, HostedSession, Origin, Placement, Spec, Surface};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("repository");
        let linked = root.join("linked");
        git(&root, &["init", "-b", "main", "repository"]);
        git(&repository, &["commit", "--allow-empty", "-m", "base"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "original",
                linked.to_str().unwrap(),
            ],
        );
        let mut app = super::super::tests::context_inspector_test_app(false);
        let (session, _) = SessionRuntime::open(
            root.join("window.sqlite3"),
            crate::model::BootId::new("manual-window"),
        )
        .unwrap();
        app.session = Some(session);
        let first =
            super::super::load_repository(&repository, None, CancellationToken::new()).unwrap();
        let project = first.project.id;
        drop(app.repository_load_task(repository.clone(), Some(project), false));
        let generation = app.repository_generations[&project];
        drop(app.update(Message::RepositoryLoaded {
            project_id: project,
            generation,
            open_cards: false,
            result: Ok(Box::new(first)),
        }));
        let id = app
            .sidebar_model
            .project(project)
            .unwrap()
            .worktrees
            .iter()
            .find(|row| row.path == linked)
            .unwrap()
            .id;
        let target = app.project_target(project, Some(id)).unwrap();
        drop(app.add_repository_card(project, repository.clone(), linked.clone()));
        let original = app.durable.cards.last().unwrap().id;
        // Actual intake captures original projections. Its async Stop task is
        // held unpolled; this fixture supplies the checked completion below.
        drop(app.start_worktree_deletion(target.clone()));
        let started_at = app.project_ui.deletions[&id].started_at;
        git(
            &repository,
            &["worktree", "remove", linked.to_str().unwrap()],
        );
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "successor",
                linked.to_str().unwrap(),
            ],
        );
        let successor =
            super::super::load_repository(&repository, Some(project), CancellationToken::new())
                .unwrap();
        drop(app.repository_load_task(repository.clone(), Some(project), false));
        let generation = app.repository_generations[&project];
        drop(app.update(Message::RepositoryLoaded {
            project_id: project,
            generation,
            open_cards: false,
            result: Ok(Box::new(successor)),
        }));
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: channel,
                project_id: project,
                title: "Successor conversation".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: Vec::new(),
                focused: None,
            });
        let hosted = crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("successor-host"),
                boot_id: crate::model::BootId::new("successor-boot"),
                host_incarnation: crate::model::HostIncarnation::new("successor-incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: linked.clone(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        };
        let actor = Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: crate::agent_execution::Target {
                    repository: repository.clone(),
                    directory: linked.clone(),
                    placement: Some(Placement::Channel(channel)),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Channel(channel),
            session: Some(HostedSession::Chat(hosted)),
            console: None,
            created_ms: 0,
            closed: false,
        };
        assert!(
            app.adopt_execution_agent(&actor).is_some(),
            "actual channel adoption can arrive while old Remove awaits its callback"
        );
        let fresh = CardId::from_uuid(actor.id.as_uuid());
        let descriptor = app.card_state(fresh).unwrap().clone();
        let scope = app.durable.card_scopes.get(&fresh).cloned();
        let channels = app.durable.channels.clone();
        let focused = app.workspace.focused();
        let active = app.active_workspace.clone();
        app.worktree_offsets.insert(linked.clone(), 41.0);
        app.worktree_focus.insert(linked.clone(), fresh);
        drop(app.handle_project_action(Action::RemovalCompleted {
            target,
            started_at,
            result: Ok(RemovalOutcome {
                warnings: Vec::new(),
                continuation: None,
            }),
        }));
        assert_eq!(app.card_state(fresh), Some(&descriptor));
        assert!(
            app.card_state(original).unwrap().existence.is_open(),
            "new inventory invalidates path-only original repository custody"
        );
        assert_eq!(app.durable.card_scopes.get(&fresh), scope.as_ref());
        assert_eq!(app.durable.channels, channels);
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.active_workspace, active);
        assert_eq!(app.worktree_offsets.get(&linked), Some(&41.0));
        assert_eq!(app.worktree_focus.get(&linked), Some(&fresh));
        assert_eq!(row(&app, id).unwrap().branch.as_deref(), Some("successor"));
        assert!(app.repository_generations[&project] > generation);
        assert!(app.project_ui.deletion_projections.is_empty());
        assert!(app.project_ui.force_branches.is_empty());
        let session = app.session.as_ref().unwrap();
        session.flush(Duration::from_secs(5)).unwrap();
        assert!(
            session
                .operation_store()
                .load_worktree(id)
                .unwrap()
                .unwrap()
                .existence
                .is_open()
        );
    }

    #[test]
    fn removal_live_chat_custody_uses_original_weak_client_not_mutable_presentation() {
        for mode in 0..3 {
            let replacement = mode == 1;
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
            let target = app.project_target(project, Some(linked.id)).unwrap();
            drop(app.new_native_chat(crate::agent::Provider::Claude, Some(linked.path.clone())));
            let card = app.durable.cards.last().unwrap().id;
            let CardKind::Chat(mut session) = app.card_state(card).unwrap().kind.clone() else {
                panic!("chat");
            };
            session.proof = Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("fixture-host"),
                boot_id: crate::model::BootId::new("fixture-boot"),
                host_incarnation: crate::model::HostIncarnation::new("fixture-incarnation"),
            });
            app.card_state_mut(card).unwrap().kind = CardKind::Chat(session.clone());
            let generation = match app.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) => runtime.target.generation,
                _ => panic!("chat runtime"),
            };
            let runtime_target = chat_ui::Target { card, generation };
            let handle = Arc::new(crate::chat::client::ClientHandle::fixture(
                session.clone(),
                crate::chat::Snapshot::default(),
            ));
            app.cards.insert(
                card,
                CardRuntime::Chat(Box::new(chat_ui::Runtime::removal_fixture(
                    runtime_target,
                    session.clone(),
                    Arc::clone(&handle),
                    app.settings.editor_syntax_theme,
                ))),
            );
            let custody = app.capture_removal_projections(target.clone());
            assert_eq!(
                Arc::strong_count(&handle),
                2,
                "custody retains only a Weak client allocation"
            );
            let old_generation = app.repository_generations.get(&project).copied();
            app.workspace_model.identity = uuid::Uuid::new_v4();
            // The host announces this edge before its correlated Removed
            // response; the original live UI owner must still retire.
            drop(app.apply_workspace_model_update(
                super::super::workspace_model_sync::ModelUpdate::WorktreesChanged {
                    registry: app.workspace_model.identity,
                    repository: repository.clone(),
                },
            ));
            assert_ne!(
                app.repository_generations.get(&project).copied(),
                old_generation
            );
            app.card_state_mut(card).unwrap().title = "Actor settled during Stop".into();
            if replacement {
                let next = Arc::new(crate::chat::client::ClientHandle::fixture(
                    session.clone(),
                    crate::chat::Snapshot::default(),
                ));
                app.cards.insert(
                    card,
                    CardRuntime::Chat(Box::new(chat_ui::Runtime::removal_fixture(
                        runtime_target,
                        session,
                        next,
                        app.settings.editor_syntax_theme,
                    ))),
                );
            }
            if mode == 2 {
                // Same live actor, but a later explicit channel filing owns its
                // presentation. The old removal cannot sweep that new intent.
                Arc::make_mut(&mut app.durable)
                    .channels
                    .push(ProjectChannel {
                        id: ChannelId::new(),
                        project_id: project,
                        title: "Later filing".into(),
                        title_source: Default::default(),
                        ordinal: 0,
                        shared: false,
                        cards: vec![card],
                        focused: Some(card),
                    });
            }
            drop(app.complete_removal_projections(custody));
            assert_eq!(
                app.card_state(card).unwrap().existence.is_open(),
                mode != 0,
                "new client or explicit filing survives; exact original live client retires across its own inventory edge"
            );
        }
    }

    #[test]
    fn terminal_removal_custody_requires_current_descriptor_proof_and_original_surface() {
        use crate::terminal::ipc::{
            self, ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
            TerminalDescriptor,
        };
        for mode in 0..4 {
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
            let (mut app, directory, project) = deletable_app(&repository, &linked, &primary);
            drop(app.add_terminal(linked.path.clone()));
            let card = app.durable.cards.last().unwrap().id;
            let CardKind::Terminal(mut session) = app.card_state(card).unwrap().kind.clone() else {
                panic!("terminal");
            };
            session.continuity = crate::model::TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("original"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("original-incarnation"),
            };
            let proof = session.continuity.live_proof().unwrap();
            app.card_state_mut(card).unwrap().kind = CardKind::Terminal(session.clone());
            // Actual attachment acknowledgement supplies surface proof; merely
            // giving connect_at an expected proof would not qualify this test.
            let socket = directory.path().join("terminal.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let handle = crate::terminal::HostedTerminalHandle::connect_at(
                socket,
                session.id,
                Some(proof.clone()),
                |_| {},
            )
            .unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < until, "terminal client did not connect");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("terminal accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let connect: ClientEnvelope = ipc::read_frame(&mut stream).unwrap().unwrap();
            ipc::write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: ipc::PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
            let attach: ClientEnvelope = ipc::read_frame(&mut stream).unwrap().unwrap();
            assert!(
                matches!(attach.request, ClientRequest::Attach {terminal_id, ..} if terminal_id == session.id)
            );
            let terminal = TerminalDescriptor {
                session,
                generation: 0,
                event_sequence: 0,
                oldest_event_sequence: 1,
                activity: None,
                agent_activity: None,
                upgrade_sources: Vec::new(),
                upgrade_relaunches: Vec::new(),
                attached: true,
                transcript: None,
                screen_lost_after_crash: false,
            };
            ipc::write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: ipc::PROTOCOL_VERSION,
                    request_id: Some(attach.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Attached { terminal },
                    },
                },
            )
            .unwrap();
            handle.wait_until_attached(Duration::from_secs(5)).unwrap();
            assert_eq!(handle.proof(), Some(proof));
            app.cards.insert(
                card,
                CardRuntime::Terminal(TerminalSurface::from(handle.clone())),
            );
            let target = app.project_target(project, Some(linked.id)).unwrap();
            let custody = app.capture_removal_projections(target);
            app.card_state_mut(card).unwrap().title = "Settled terminal".into();
            if let CardKind::Terminal(current) = &mut app.card_state_mut(card).unwrap().kind {
                if mode == 1 {
                    current.continuity = crate::model::TerminalContinuity::Live {
                        host_session_id: crate::model::HostSessionId::new("successor"),
                        boot_id: crate::model::BootId::new("boot"),
                        host_incarnation: crate::model::HostIncarnation::new(
                            "successor-incarnation",
                        ),
                    };
                } else if mode == 2 {
                    current.continuity = crate::model::TerminalContinuity::Exited {
                        last_exit: crate::model::TerminalExit {
                            code: Some(0),
                            signal: None,
                            occurred_at_unix_ms: 1,
                        },
                    };
                }
            }
            if mode == 3 {
                app.cards.insert(
                    card,
                    CardRuntime::Terminal(TerminalSurface::from(handle.clone())),
                );
            }
            drop(app.complete_removal_projections(custody));
            assert_eq!(
                app.card_state(card).unwrap().existence.is_open(),
                mode != 0,
                "only the still-proven original descriptor+surface retires; proof replacement, unknown exited proof and surface replacement survive"
            );
            // Both stream and original handle have bounded I/O/drop custody;
            // there is no native terminal or provider process in this fixture.
        }
    }

    #[test]
    fn automatic_success_retires_only_unchanged_captured_card_owners() {
        for replaced in [false, true] {
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
            drop(app.add_repository_card(project, repository.clone(), linked.path.clone()));
            let original = app.durable.cards.last().unwrap().clone();
            let target = app.project_target(project, Some(linked.id)).unwrap();
            let custody = app.capture_removal_projections(target.clone());
            let request = custody.request;
            assert_eq!(custody.owners.len(), 1);
            app.project_ui
                .automatic_in_flight
                .insert(linked.id, custody);
            if replaced {
                // An authoritative same-CardId descriptor replacement must not
                // be mistaken for the old repository owner merely by its ID.
                app.card_state_mut(original.id).unwrap().kind = CardKind::Terminal(
                    fresh_terminal_session(LaunchRecipe::shell(linked.path.clone()).unwrap()),
                );
                app.cards.insert(original.id, CardRuntime::Loading);
            }
            let current = app.card_state(original.id).unwrap().clone();
            drop(app.handle_project_action(Action::AutomaticallyDeleted {
                warnings: Vec::new(),
                target,
                request,
                result: Ok(crate::worktree_cleanup::RemovedWorktree {
                    common_dir: repository.join(".git"),
                    admin_dir: repository.join(".git/worktrees/feature"),
                    path: linked.path.clone(),
                    branch: Some("refs/heads/feature".into()),
                }),
            }));
            if replaced {
                assert_eq!(app.card_state(original.id).unwrap().kind, current.kind);
                assert!(app.card_state(original.id).unwrap().existence.is_open());
            } else {
                assert!(!app.card_state(original.id).unwrap().existence.is_open());
            }
            assert!(
                row(&app, linked.id).is_some(),
                "only the new inventory may remove a row"
            );
        }
    }

    #[test]
    fn removal_projection_from_other_device_context_cannot_retire_or_refresh_current_owners() {
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
        drop(app.add_repository_card(project, repository.clone(), linked.path.clone()));
        let original = app.durable.cards.last().unwrap().id;
        let target = app.project_target(project, Some(linked.id)).unwrap();
        let mut custody = app.capture_removal_projections(target);
        custody.device = Some(("other".into(), "other-host".into(), "cagents".into()));
        assert_ne!(custody.device, stop_device_key());
        let generations = app.repository_generations.clone();
        drop(app.complete_removal_projections(custody));
        assert!(app.card_state(original).unwrap().existence.is_open());
        assert_eq!(app.repository_generations, generations);
        assert!(row(&app, linked.id).is_some());
    }

    #[test]
    fn successful_automatic_removal_preserves_metadata_capture_refusal_as_warning() {
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
        let target = app.project_target(project, Some(linked.id)).unwrap();
        let custody = app.capture_removal_projections(target.clone());
        let request = custody.request;
        app.project_ui
            .automatic_in_flight
            .insert(linked.id, custody);
        let receipt = RemovedWorktree {
            common_dir: repository.join(".git"),
            admin_dir: repository.join(".git/worktrees/feature"),
            path: linked.path.clone(),
            branch: Some("refs/heads/feature".into()),
        };
        let warnings =
            finish_creation_retirements(Err("original journal read failed".into()), &receipt);
        drop(app.handle_project_action(Action::AutomaticallyDeleted {
            target,
            request,
            result: Ok(receipt),
            warnings,
        }));
        let notice = &app.status.as_ref().unwrap().message;
        assert!(notice.contains("Worktree removed automatically"));
        assert!(notice.contains("Creation metadata retained: original journal read failed"));
        assert!(!app.project_ui.automatic_in_flight.contains_key(&linked.id));
        assert!(!app.project_ui.automatic_attempts.contains_key(&linked.id));
    }

    #[test]
    fn automatic_cleanup_refusals_are_quiet_and_backed_off() {
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
        app.settings.auto_delete_merged_worktrees = true;
        let target = app.project_target(project, Some(linked.id)).unwrap();
        // Each refusal category, including Git safety text, must survive to
        // the row without putting it in manual deletion or retry state.
        for reason in [
            "Kept: ignored files .env",
            "agent running",
            "ownership uncertain",
            "daemon upgrading",
            "cleanup protection unavailable",
        ] {
            let custody = app.capture_removal_projections(target.clone());
            let request = custody.request;
            app.project_ui
                .automatic_in_flight
                .insert(linked.id, custody);
            drop(app.handle_project_action(Action::AutomaticallyDeleted {
                warnings: Vec::new(),
                target: target.clone(),
                request,
                result: Err(reason.into()),
            }));
            let attempt = &app.project_ui.automatic_attempts[&linked.id];
            assert!(!attempt.retry_due(attempt.at + Duration::from_secs(29)));
            assert!(attempt.retry_due(attempt.at + Duration::from_secs(30)));
            assert_eq!(
                row(&app, linked.id).unwrap().cleanup_kept.as_deref(),
                Some(cleanup_kept_detail(reason).as_str())
            );
            assert!(row(&app, linked.id).unwrap().deletion.is_none());
            assert!(!app.project_ui.is_deleting(linked.id));
            assert!(app.project_ui.automatic_in_flight.is_empty());
        }
        app.settings.auto_delete_merged_worktrees = false;
        app.sync_worktree_deletion_rows();
        assert!(row(&app, linked.id).unwrap().cleanup_kept.is_none());
    }

    #[test]
    fn automatic_cleanup_ignores_late_or_unrequested_refusal() {
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
        let target = app.project_target(project, Some(linked.id)).unwrap();
        drop(app.handle_project_action(Action::AutomaticallyDeleted {
            warnings: Vec::new(),
            target,
            request: crate::model::OperationId::new(),
            result: Err("obsolete refusal".into()),
        }));
        assert!(app.project_ui.automatic_attempts.is_empty());
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
        drop(app.handle_project_action(Action::AwaitSessions(target)));
        assert!(app.project_ui.deletions[&linked.id].failure.is_some());
        assert!(!app.project_ui.is_deleting(linked.id));
    }

    /// Completion refreshes actual inventory; a delayed path receipt must not
    /// blindly tombstone the current catalog association.
    #[test]
    fn deletion_waits_for_actual_inventory_before_retiring_the_catalog_row() {
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
        confirm_deletion_fixture(&mut app);
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
            target,
            result: Ok(()),
        });
        assert!(app.project_ui.deletions.is_empty());
        assert!(row(&app, id).is_some());
        assert!(app.sidebar_model.projects[0].reloading);

        let session = app.session.as_ref().unwrap();
        let store = session.operation_store();
        session.flush(Duration::from_secs(5)).unwrap();
        let deleted = store.load_worktree(id).unwrap().unwrap();
        assert!(
            deleted.existence.is_open(),
            "only actual inventory may retire the row association"
        );
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
        confirm_deletion_fixture(&mut app);
        let _ = app.handle_project_action(Action::Deleted {
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
        confirm_deletion_fixture(&mut app);
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
        assert!(
            app.project_ui.dialog.is_some(),
            "retry requires a fresh safety confirmation"
        );
        confirm_deletion_fixture(&mut app);
        assert_eq!(row(&app, id).unwrap().deletion, Some(RowDeletion::Deleting));
        let _ = app.handle_project_action(Action::Deleted {
            target,
            result: Err("fatal: cannot remove".into()),
        });

        // Reopened from the row, the dialog says why; cancelling declines it.
        let _ = app.handle_project_action(delete);
        assert_eq!(
            app.project_ui.dialog.as_ref().unwrap().failure.as_deref(),
            Some("fatal: cannot remove")
        );
        let dismiss = app.project_dialog_dismiss_action();
        let _ = app.handle_project_action(dismiss);
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
        confirm_deletion_fixture(&mut app);
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
        confirm_deletion_fixture(&mut app);
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
            target,
            result: Ok(()),
        });
        // The follow-up invalidates older alias reads, but only its actual
        // inventory can retire the old row; a same-path successor must survive.
        assert!(app.sidebar_model.projects[0].reloading);
        loaded(&mut app, alias, stale, false);
        assert!(row(&app, id).is_some());
        assert!(app.sidebar_model.projects[0].reloading);
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
        let snapshot = clean_snapshot();
        let refusal = |target: Target| delete_worktree(&target, &snapshot).unwrap_err();

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
            delete_worktree(
                &Target {
                    repository: bare.clone(),
                    ..target(Some(bare_id), &bare)
                },
                &snapshot
            )
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
        assert!(app.worktree_has_unsaved_editors(&dialog.target));
        assert!(
            dialog.safety.is_none(),
            "cached dirty state cannot authorize deletion"
        );
        confirm_deletion_fixture(&mut app);
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
        let snapshot = service.worktree_safety(&linked, &cancellation).unwrap();
        let command = worktree_removal_command(&target, &snapshot);
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

    #[test]
    fn deletion_waits_for_fresh_safety_and_changed_state_requires_new_consent() {
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
        drop(app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(linked.id),
            command: ProjectCommand::Delete,
        }));
        let request = app.project_ui.dialog.as_ref().unwrap().request;
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request,
            result: Ok(DeleteOptions {
                branch_elsewhere: false,
                stale_lock: false,
                lock_error: None,
            }),
        }));
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        assert!(!app.project_ui.is_deleting(linked.id));
        let mut observed = clean_snapshot();
        observed.untracked = 3;
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: false,
            result: fixture_safety_result(
                &app,
                Ok((observed.clone(), AgentCensus::default())),
                false,
            ),
        }));
        assert_eq!(
            app.project_ui.focus_target(),
            deletion_control_id(request, "cancel")
        );
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        assert!(app.project_ui.dialog.as_ref().unwrap().loading);
        let mut current = observed;
        current.untracked = 4;
        current.fingerprint = "changed".into();
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: true,
            result: fixture_safety_result(
                &app,
                Ok((current.clone(), AgentCensus::default())),
                true,
            ),
        }));
        assert!(!app.project_ui.is_deleting(linked.id));
        let dialog = app.project_ui.dialog.as_ref().unwrap();
        assert_eq!(dialog.safety.as_ref().unwrap().untracked, 4);
        assert!(dialog.blocker.as_deref().unwrap().contains("changed"));
        assert!(dialog.stop_authorization.is_none());
        drop(app.handle_project_action(Action::ConfirmDelete(request)));
        drop(app.handle_project_action(Action::SafetyRead {
            request,
            confirming: true,
            result: fixture_safety_result(
                &app,
                Ok((current.clone(), AgentCensus::default())),
                true,
            ),
        }));
        assert!(
            !app.project_ui.is_deleting(linked.id),
            "changed inspection cannot be rebound within the same dialog"
        );
        let dismiss = app.project_dialog_dismiss_action();
        drop(app.handle_project_action(dismiss));
        drop(app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(linked.id),
            command: ProjectCommand::Delete,
        }));
        let reopened = app.project_ui.dialog.as_ref().unwrap().request;
        assert_ne!(reopened, request);
        drop(app.handle_project_action(Action::DeleteOptionsRead {
            request: reopened,
            result: Ok(DeleteOptions {
                branch_elsewhere: false,
                stale_lock: false,
                lock_error: None,
            }),
        }));
        drop(app.handle_project_action(Action::SafetyRead {
            request: reopened,
            confirming: false,
            result: fixture_safety_result(
                &app,
                Ok((current.clone(), AgentCensus::default())),
                false,
            ),
        }));
        drop(app.handle_project_action(Action::ConfirmDelete(reopened)));
        drop(app.handle_project_action(Action::SafetyRead {
            request: reopened,
            confirming: true,
            result: fixture_safety_result(&app, Ok((current, AgentCensus::default())), true),
        }));
        assert!(app.project_ui.is_deleting(linked.id));
        assert!(app.project_ui.dialog.is_none());
    }

    #[test]
    fn stale_safety_reply_cannot_authorize_a_reopened_dialog() {
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
        let command = Action::Command {
            project_id: project,
            worktree_id: Some(linked.id),
            command: ProjectCommand::Delete,
        };
        drop(app.handle_project_action(command.clone()));
        let old_request = app.project_ui.dialog.as_ref().unwrap().request;
        let dismiss = app.project_dialog_dismiss_action();
        drop(app.handle_project_action(dismiss));
        drop(app.handle_project_action(command));
        drop(app.handle_project_action(Action::SafetyRead {
            request: old_request,
            confirming: true,
            result: fixture_safety_result(
                &app,
                Ok((clean_snapshot(), AgentCensus::default())),
                true,
            ),
        }));
        assert!(app.project_ui.dialog.as_ref().unwrap().safety.is_none());
        assert!(app.project_ui.dialog.as_ref().unwrap().loading);
        assert!(!app.project_ui.is_deleting(linked.id));
    }

    #[test]
    fn clean_git_removal_has_no_force_and_new_untracked_files_invalidate_consent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("checkout");
        let linked = root.join("linked");
        git(&root, &["init", "-b", "main", "checkout"]);
        git(&repository, &["commit", "--allow-empty", "-m", "initial"]);
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
        let target = Target {
            project: ProjectId::new(),
            worktree: Some(record.id),
            repository,
            path: linked.clone(),
            label: "feature".into(),
        };
        let clean = service.worktree_safety(&linked, &cancellation).unwrap();
        let command = worktree_removal_command(&target, &clean);
        assert!(!command.arguments.iter().any(|arg| arg == "--force"));
        std::fs::write(linked.join("agent-output.txt"), "new work").unwrap();
        let untracked = service.worktree_safety(&linked, &cancellation).unwrap();
        assert_eq!(untracked.untracked, 1);
        assert_eq!(untracked.modified, 0);
        assert!(untracked.notable_ignored.is_empty());
        assert!(
            verify_worktree_safety(&service, &linked, &clean, &cancellation)
                .unwrap_err()
                .contains("changed")
        );
        assert!(linked.join("agent-output.txt").exists());
        std::fs::remove_file(linked.join("agent-output.txt")).unwrap();
        let clean = service.worktree_safety(&linked, &cancellation).unwrap();
        let command = worktree_removal_command(&target, &clean);
        SystemProcessExecutor
            .execute(&command, &cancellation)
            .unwrap()
            .require_success(&command.program)
            .unwrap();
        assert!(!linked.exists());
    }

    #[test]
    fn disposable_directory_input_is_exact_and_can_preserve_all_ignored_files() {
        assert_eq!(
            parse_disposable_directories("target, node_modules, target").unwrap(),
            vec!["target", "node_modules"]
        );
        assert!(parse_disposable_directories("").unwrap().is_empty());
        for invalid in ["../data", "target/build", "..", "foo\\bar", "target\n.env"] {
            assert!(
                parse_disposable_directories(invalid).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn cleanup_policy_round_trips_names_without_expanding_the_allowlist() {
        let names: Vec<String> = [
            "target",
            "build,data",
            " cache ",
            "quote\"name",
            "缓存",
            "[literal]*",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let displayed = format_disposable_directories(&names);
        assert_eq!(parse_disposable_directories(&displayed).unwrap(), names);
        assert_eq!(
            parse_disposable_directories("\"build,data\"").unwrap(),
            vec!["build,data"]
        );
        for malformed in ["\"unfinished", "\"target\"data", "bad\"quote", "\"\""] {
            assert!(
                parse_disposable_directories(malformed).is_err(),
                "{malformed}"
            );
        }
    }

    #[test]
    fn bulk_worktree_deletion_requires_each_safety_confirmation() {
        let repository = PathBuf::from("/repo");
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let first = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("first"),
            Some("first".into()),
        );
        let second = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("second"),
            Some("second".into()),
        );
        let (mut app, _directory, project) = deletable_app(&repository, &first, &primary);
        app.sidebar_model.projects[0].worktrees.push(second.clone());
        drop(app.delete_selected_worktree(project, first.id));
        drop(app.delete_selected_worktree(project, second.id));
        assert_eq!(app.project_ui.pending_delete.len(), 1);
        confirm_deletion_fixture(&mut app);
        assert!(app.project_ui.is_deleting(first.id));
        assert!(!app.project_ui.is_deleting(second.id));
        assert_eq!(
            app.project_ui.dialog.as_ref().unwrap().target.worktree,
            Some(second.id)
        );
        let dismiss = app.project_dialog_dismiss_action();
        drop(app.handle_project_action(dismiss));
        assert!(!app.project_ui.is_deleting(second.id));
        assert!(app.project_ui.pending_delete.is_empty());
    }
}
