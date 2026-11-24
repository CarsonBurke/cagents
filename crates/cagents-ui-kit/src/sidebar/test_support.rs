//! Fixtures the application's tests share with this module's.

use super::*;
use crate::repository::{
    ChangeArea, ChangeKind, ChangedPath, HeadState, LocalRefRecord, RemoteHead, RemoteRecord,
    RemoteRefRecord, RepoPath, WorkingFiles, WorktreeLock,
};
use uuid::Uuid;

pub fn project_id(value: u128) -> ProjectId {
    ProjectId::from_uuid(Uuid::from_u128(value))
}

pub fn repo_path(value: &str) -> RepoPath {
    RepoPath::parse(value).expect("valid repository path")
}

pub fn ref_name(value: &str) -> ValidatedRefName {
    ValidatedRefName::parse(value).expect("valid ref name")
}

pub fn oid_of(seed: u8) -> Oid {
    Oid::parse(format!("{seed:02x}").repeat(20)).expect("valid object id")
}

pub fn main_key() -> ActiveWorktreeKey {
    ActiveWorktreeKey {
        project_id: project_id(1),
        worktree_id: WorktreeId::from_uuid(Uuid::from_u128(2)),
        canonical_path: PathBuf::from("/repo/main"),
    }
}

pub fn fix_key() -> ActiveWorktreeKey {
    ActiveWorktreeKey {
        project_id: project_id(1),
        worktree_id: WorktreeId::from_uuid(Uuid::from_u128(3)),
        canonical_path: PathBuf::from("/repo/fix-sidebar"),
    }
}

pub fn changed(
    path: RepoPath,
    area: ChangeArea,
    kind: ChangeKind,
    counts: Option<(u64, u64)>,
) -> ChangedPath {
    ChangedPath {
        path,
        old_path: None,
        area,
        kind,
        conflict: None,
        submodule: false,
        added_lines: counts.map(|(added, _)| added),
        removed_lines: counts.map(|(_, removed)| removed),
    }
}

pub fn panel_remote(name: &str, url: &str) -> RemoteRecord {
    RemoteRecord {
        name: name.into(),
        fetch_url: Some(url.into()),
        push_url: None,
        gh_resolved: None,
    }
}

pub fn panel_remote_ref(name: &str) -> RemoteRefRecord {
    let reference = ref_name(name);
    let rest = name.strip_prefix("refs/remotes/").unwrap_or(name);
    let (remote, branch) = rest.split_once('/').unwrap_or((rest, ""));
    RemoteRefRecord {
        name: reference,
        oid: oid_of(0xaa),
        remote: remote.into(),
        branch: branch.into(),
    }
}

/// A worktree on `main`, ahead of `origin/main`, with one entry in each of
/// the four change areas and one unmeasured (binary) count.
pub fn snapshot() -> RepositorySnapshot {
    RepositorySnapshot {
        status: StatusSnapshot {
            untracked_omitted: false,
            head: HeadState::Branch {
                name: "main".into(),
                oid: oid_of(0x11).to_string(),
            },
            tracking: TrackingState::Compared {
                upstream: "refs/remotes/origin/main".into(),
                ahead: 2,
                behind: 0,
            },
            dirty: DirtyState {
                staged: 1,
                unstaged: 1,
                untracked: 1,
                conflicted: 1,
                submodule: 0,
            },
            entries: vec![
                changed(
                    repo_path("src/app.rs"),
                    ChangeArea::Staged,
                    ChangeKind::Modified,
                    Some((12, 3)),
                ),
                changed(
                    repo_path("assets/logo.png"),
                    ChangeArea::Unstaged,
                    ChangeKind::Modified,
                    None,
                ),
                changed(
                    repo_path("notes.txt"),
                    ChangeArea::Untracked,
                    ChangeKind::Untracked,
                    None,
                ),
                ChangedPath {
                    conflict: Some(ConflictKind::BothModified),
                    ..changed(
                        repo_path("src/sidebar.rs"),
                        ChangeArea::Conflict,
                        ChangeKind::Unmerged,
                        None,
                    )
                },
            ],
        },
        files: WorkingFiles {
            paths: vec![
                repo_path("README.md"),
                repo_path("src/app.rs"),
                repo_path("src/sidebar.rs"),
            ],
            tracked_count: 3,
            ..WorkingFiles::default()
        },
        remotes: vec![panel_remote(
            "origin",
            "https://github.com/example/cagents.git",
        )],
        local_refs: vec![LocalRefRecord {
            name: ref_name("refs/heads/main"),
            oid: oid_of(0x11),
            upstream: Some(ref_name("refs/remotes/origin/main")),
            head: true,
        }],
        remote_refs: vec![panel_remote_ref("refs/remotes/origin/main")],
        remote_heads: vec![RemoteHead {
            remote: "origin".into(),
            target: ref_name("refs/remotes/origin/main"),
        }],
    }
}

pub fn source_control() -> SourceControlModel {
    SourceControlModel::ready(main_key(), &snapshot())
}

pub fn panel_worktree(key: &ActiveWorktreeKey, branch: &str) -> SidebarWorktree {
    SidebarWorktree::from_repository(
        &WorktreeRecord {
            id: key.worktree_id,
            path: key.canonical_path.clone(),
            created_at_unix_ms: None,
            head_oid: None,
            branch: Some(format!("refs/heads/{branch}")),
            detached: false,
            bare: false,
            lock: WorktreeLock::default(),
            prunable_reason: None,
        },
        None,
    )
}

/// A project holding both fixture worktrees, so the panel has somewhere to
/// bind and somewhere to rebind to.
pub fn panel_model() -> SidebarModel {
    SidebarModel::ready(
        SidebarMode::Expanded,
        vec![SidebarProject::new(
            project_id(1),
            RepositoryId(Uuid::from_u128(9)),
            "cagents".into(),
            "/repo/cagents".into(),
            vec![
                panel_worktree(&main_key(), "main"),
                panel_worktree(&fix_key(), "fix-sidebar"),
            ],
            Vec::new(),
        )],
    )
    .with_source_control(Some(source_control()))
}

pub fn focus_on(key: &ActiveWorktreeKey) -> SidebarFocus {
    SidebarFocus::Repository {
        worktree: key.canonical_path.clone(),
    }
}

/// A pane bound to `main` with every panel section open.
pub fn panel_state(model: &SidebarModel) -> SidebarState {
    let mut state = SidebarState::default();
    assert!(state.sync_active_worktree(model, &focus_on(&main_key())));
    for section in PanelSection::ALL {
        if !state.section_expanded(section) {
            state.apply(&SidebarAction::ToggleSection {
                key: main_key(),
                section,
            });
        }
    }
    state
}
