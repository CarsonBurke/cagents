use super::*;
use crate::{
    repository::{BranchComparison, HeadState, Oid, ValidatedRefName},
    sidebar::{BranchChanges, BranchChangesModel},
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    key: ActiveWorktreeKey,
    head: Oid,
    base_ref: ValidatedRefName,
    base_oid: Option<Oid>,
}

#[derive(Debug)]
struct Pending {
    generation: u64,
    identity: Identity,
    cancellation: CancellationToken,
}

#[derive(Debug, Default)]
pub(super) struct State {
    generation: u64,
    pending: Option<Pending>,
    cached: Option<(Identity, Arc<BranchChangesModel>)>,
}

impl State {
    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Loaded {
        generation: u64,
        identity: Identity,
        result: Result<BranchComparison, String>,
    },
}

fn comparison_identity(
    key: ActiveWorktreeKey,
    snapshot: &RepositorySnapshot,
    primary_branch: Option<&str>,
) -> Result<Identity, &'static str> {
    let head = match &snapshot.status.head {
        HeadState::Branch { oid, .. } | HeadState::Detached { oid } => {
            Oid::parse_object(oid.clone()).map_err(|_| "HEAD could not be read.")?
        }
        HeadState::Unborn { .. } => return Err("No commits yet."),
        HeadState::Unknown => return Err("HEAD could not be read."),
    };
    let base_ref = snapshot
        .default_branch_base()
        .cloned()
        .or_else(|| {
            // Sidebar branch labels are shortened; only a matching real local ref
            // may turn a known primary-worktree branch back into a revision.
            let branch = primary_branch?;
            snapshot
                .local_refs
                .iter()
                .find(|record| record.name.short_name() == branch || record.name.as_str() == branch)
                .map(|record| record.name.clone())
        })
        .ok_or("No default or primary-worktree branch is known.")?;
    let base_oid = snapshot
        .local_refs
        .iter()
        .find(|record| record.name == base_ref)
        .map(|record| record.oid.clone())
        .or_else(|| {
            snapshot
                .remote_refs
                .iter()
                .find(|record| record.name == base_ref)
                .map(|record| record.oid.clone())
        });
    Ok(Identity {
        key,
        head,
        base_ref,
        base_oid,
    })
}

impl Cagents {
    /// Called as soon as the sidebar changes binding, before the new status
    /// load. No old worktree's Git process survives merely to be discarded.
    pub(super) fn cancel_branch_changes(&mut self) {
        self.branch_ui.cancel();
    }

    /// Local files/status are already published before this independent task
    /// starts. The single-entry cache retains immutable presentation rows across
    /// ordinary status refreshes without cloning the file list.
    pub(super) fn refresh_branch_changes(
        &mut self,
        snapshot: &RepositorySnapshot,
    ) -> Task<Message> {
        let Some(key) = self.sidebar_state.active_worktree().cloned() else {
            self.branch_ui.cancel();
            return Task::none();
        };
        if !self.source_control_is_bound(&key) {
            return Task::none();
        }
        let primary_branch = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.id == key.project_id)
            .and_then(|project| {
                project.worktrees.iter().find(|worktree| {
                    sidebar::is_primary_worktree(&project.repository, &worktree.path)
                })
            })
            .filter(|worktree| !worktree.detached)
            .and_then(|worktree| worktree.branch.as_deref());
        let identity = match comparison_identity(key, snapshot, primary_branch) {
            Ok(identity) => identity,
            Err(reason) => {
                self.branch_ui.cancel();
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    source.branch_changes = BranchChanges::Unavailable(reason);
                }
                return Task::none();
            }
        };
        if identity.base_oid.is_some()
            && let Some((cached, model)) = self.branch_ui.cached.as_ref()
            && cached == &identity
        {
            let model = Arc::clone(model);
            self.branch_ui.cancel();
            if let Some(source) = self.sidebar_model.source_control.as_mut() {
                source.branch_changes = BranchChanges::Ready(model);
            }
            return Task::none();
        }
        if let Some(source) = self.sidebar_model.source_control.as_mut() {
            source.branch_changes = BranchChanges::Loading(identity.base_ref.clone());
        }
        if self
            .branch_ui
            .pending
            .as_ref()
            .is_some_and(|pending| pending.identity == identity)
        {
            return Task::none();
        }
        self.branch_ui.cancel();
        self.branch_ui.generation = self.branch_ui.generation.wrapping_add(1);
        let generation = self.branch_ui.generation;
        let cancellation = CancellationToken::default();
        self.branch_ui.pending = Some(Pending {
            generation,
            identity: identity.clone(),
            cancellation: cancellation.clone(),
        });
        cosmic::task::future(async move {
            let worktree = identity.key.canonical_path.clone();
            let base_ref = identity.base_ref.clone();
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .branch_comparison(&worktree, &base_ref, &cancellation)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| format!("branch comparison reader join failed: {error}"))
            .and_then(|result| result);
            Message::BranchUi(Action::Loaded {
                generation,
                identity,
                result,
            })
        })
    }

    pub(super) fn update_branch_ui(&mut self, action: Action) -> Task<Message> {
        let Action::Loaded {
            generation,
            identity,
            result,
        } = action;
        let current = self.branch_ui.pending.as_ref().is_some_and(|pending| {
            pending.generation == generation && pending.identity == identity
        });
        if !current {
            return Task::none();
        }
        self.branch_ui.pending = None;
        let Some(source) = self.sidebar_model.source_control.as_mut().filter(|source| {
            source.key == identity.key
                && self.sidebar_state.active_worktree() == Some(&identity.key)
                && matches!(source.data_state, sidebar::SidebarDataState::Ready)
                && source.head_oid.as_deref() == Some(identity.head.as_str())
        }) else {
            return Task::none();
        };
        match result {
            Ok(comparison)
                if comparison.head == identity.head
                    && comparison.base_ref == identity.base_ref
                    && identity
                        .base_oid
                        .as_ref()
                        .is_none_or(|base| *base == comparison.base_oid) =>
            {
                let model = Arc::new(BranchChangesModel::new(comparison));
                source.branch_changes = BranchChanges::Ready(Arc::clone(&model));
                self.branch_ui.cached = Some((identity, model));
                Task::none()
            }
            Ok(_) => {
                // A ref moved between status and the independent Git request.
                // Never cache the new range under the old snapshot's identity.
                source.branch_changes = BranchChanges::Unavailable("Branch moved; refreshing…");
                self.reload_source_control(&identity.key)
            }
            Err(message) => {
                source.branch_changes = BranchChanges::Error {
                    base_ref: identity.base_ref,
                    message,
                };
                Task::none()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_branch_file(
        &mut self,
        key: ActiveWorktreeKey,
        path: RepoPath,
        old_path: Option<RepoPath>,
        base_ref: ValidatedRefName,
        head: Oid,
        merge_base: Oid,
    ) -> Task<Message> {
        if self.modal_is_open() || self.sidebar_state.active_worktree() != Some(&key) {
            return Task::none();
        }
        let current = self.sidebar_model.source_control.as_ref().is_some_and(|source| {
            source.key == key && matches!(&source.branch_changes, BranchChanges::Ready(branch)
                if branch.base_ref == base_ref && branch.head == head && branch.merge_base == merge_base
                    && branch.rows.iter().any(|row| row.entry.path == path && row.entry.old_path == old_path))
        });
        if !current {
            return Task::none();
        }
        self.open_committed_file(
            key,
            path,
            old_path,
            DiffBasis::MergeBaseToHead {
                base_ref,
                head,
                merge_base,
            },
        )
    }

    pub(super) fn open_committed_file(
        &mut self,
        key: ActiveWorktreeKey,
        path: RepoPath,
        old_path: Option<RepoPath>,
        basis: DiffBasis,
    ) -> Task<Message> {
        self.open_diff_card(key, move |card| RepositoryAction::OpenCommittedFile {
            card,
            path,
            old_path,
            basis,
        })
    }

    pub(super) fn open_working_change(
        &mut self,
        key: ActiveWorktreeKey,
        path: RepoPath,
        area: crate::repository::ChangeArea,
    ) -> Task<Message> {
        use crate::repository::ChangeArea;
        if self.modal_is_open() || !self.source_control_is_bound(&key) {
            return Task::none();
        }
        let Some(source) = self.sidebar_model.source_control.as_ref() else {
            return Task::none();
        };
        let find = |area| {
            source
                .group(area)
                .and_then(|group| group.rows().iter().find(|row| row.entry.path == path))
        };
        if find(area).is_none() {
            return Task::none();
        }
        let staged = find(ChangeArea::Staged);
        let unstaged = find(ChangeArea::Unstaged)
            .or_else(|| find(ChangeArea::Untracked))
            .or_else(|| find(ChangeArea::Conflict));
        let file = crate::repository_widget::RepositoryFile {
            path: path.as_path().to_owned(),
            staged: staged.is_some(),
            unstaged: unstaged.is_some(),
            staged_old_path: staged
                .and_then(|row| row.entry.old_path.as_ref())
                .map(|path| path.as_path().to_owned()),
            unstaged_old_path: unstaged
                .and_then(|row| row.entry.old_path.as_ref())
                .map(|path| path.as_path().to_owned()),
            ..Default::default()
        };
        let basis = if area == ChangeArea::Staged {
            DiffBasis::HeadToIndex
        } else {
            DiffBasis::IndexToWorktree
        };
        self.open_diff_card(key, move |card| RepositoryAction::OpenWorkingDiff {
            card,
            file,
            basis,
        })
    }

    fn open_diff_card(
        &mut self,
        key: ActiveWorktreeKey,
        open: impl FnOnce(CardId) -> RepositoryAction,
    ) -> Task<Message> {
        let overlay = if self.context_inspector_is_overlay() {
            self.set_context_inspector_open(false)
        } else {
            Task::none()
        };
        // A historical path may be deleted, renamed, or never exist in the
        // working file list. Do not park behind or emit OpenFile/RefreshFiles.
        self.pending_panel_open = None;
        let prepare = match self.repository_card_for(&key.canonical_path) {
            Some(card) => self
                .workspace
                .focus(card)
                .map(|update| self.apply_viewport(update, true, false))
                .unwrap_or_else(|_| Task::none()),
            None => self.add_repository_card(
                key.project_id,
                key.canonical_path.clone(),
                key.canonical_path.clone(),
            ),
        };
        let Some(card) = self.repository_card_for(&key.canonical_path) else {
            return overlay.chain(prepare);
        };
        let open = cosmic::task::message(Message::Repository(open(card)));
        Task::batch([overlay, prepare, open])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{LocalRefRecord, RemoteHead, RemoteRefRecord, WorkingFiles};

    fn key() -> ActiveWorktreeKey {
        ActiveWorktreeKey {
            project_id: crate::model::ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: PathBuf::from("/repo/feature"),
        }
    }

    fn snapshot() -> RepositorySnapshot {
        RepositorySnapshot {
            status: StatusSnapshot {
                listing_truncated: false,
                head: HeadState::Detached {
                    oid: "ab".repeat(20),
                },
                ..StatusSnapshot::default()
            },
            files: WorkingFiles::default(),
            remotes: Vec::new(),
            local_refs: vec![LocalRefRecord {
                name: ValidatedRefName::parse("refs/heads/trunk").unwrap(),
                oid: Oid::parse_object("cd".repeat(20)).unwrap(),
                upstream: None,
                head: false,
            }],
            remote_refs: Vec::new(),
            remote_heads: Vec::new(),
        }
    }

    #[test]
    fn known_primary_is_a_fallback_not_an_invented_default() {
        let snapshot = snapshot();
        assert!(comparison_identity(key(), &snapshot, None).is_err());
        assert!(comparison_identity(key(), &snapshot, Some("main")).is_err());
        assert_eq!(
            comparison_identity(key(), &snapshot, Some("trunk"))
                .unwrap()
                .base_ref
                .as_str(),
            "refs/heads/trunk",
        );
    }

    #[test]
    fn remote_default_takes_precedence_and_moving_base_invalidates_cache() {
        let key = key();
        let mut snapshot = snapshot();
        let reference = ValidatedRefName::parse("refs/remotes/origin/release").unwrap();
        snapshot.remote_heads.push(RemoteHead {
            remote: "origin".into(),
            target: reference.clone(),
        });
        snapshot.remote_refs.push(RemoteRefRecord {
            name: reference.clone(),
            oid: Oid::parse_object("ef".repeat(20)).unwrap(),
            remote: "origin".into(),
            branch: "release".into(),
        });
        let before = comparison_identity(key.clone(), &snapshot, Some("trunk")).unwrap();
        assert_eq!(before.base_ref, reference);
        snapshot.remote_refs[0].oid = Oid::parse_object("12".repeat(20)).unwrap();
        let after = comparison_identity(key, &snapshot, Some("trunk")).unwrap();
        assert_ne!(before, after);
    }
}
