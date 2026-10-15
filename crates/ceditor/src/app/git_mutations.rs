//! Host adapters for immutable, ordered Git intentions.

use super::*;
use cagents::repository::{ChangeArea, DiscardRequest};
use std::{collections::VecDeque, os::unix::fs::MetadataExt};

#[derive(Debug, Clone)]
pub struct GroupOrigin {
    card: CardId,
    key: ActiveWorktreeKey,
    source_revision: u64,
    selection_revision: u64,
    root_identity: (u64, u64),
    operation: u64,
}

#[derive(Debug, Clone)]
pub struct GroupRequest {
    origin: GroupOrigin,
    area: ChangeArea,
    command: sidebar::ChangeGroupCommand,
    selected: Option<Arc<Vec<PathSelection>>>,
}

#[derive(Debug, Clone)]
pub struct GroupSelection {
    paths: Vec<PathSelection>,
    untracked: BTreeSet<RepoPath>,
}

#[derive(Default)]
pub(super) struct GroupQueue {
    pending: VecDeque<GroupRequest>,
    active: Option<u64>,
}

#[cfg(test)]
impl GroupQueue {
    pub(super) fn pending_len(&self) -> usize { self.pending.len() }
}

fn allowed(area: ChangeArea, command: sidebar::ChangeGroupCommand) -> bool {
    match command {
        sidebar::ChangeGroupCommand::Stage => area != ChangeArea::Staged,
        sidebar::ChangeGroupCommand::Unstage => area == ChangeArea::Staged,
        sidebar::ChangeGroupCommand::Discard => matches!(area, ChangeArea::Unstaged | ChangeArea::Untracked),
    }
}

fn root_identity(root: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(root).ok()?;
    metadata.is_dir().then(|| (metadata.dev(), metadata.ino()))
}

impl Editor {
    fn group_origin_current(&self, request: &GroupRequest) -> bool {
        let origin = &request.origin;
        !self.exiting && self.tab(origin.card).is_some_and(|tab| {
            tab.git && tab.runtime.descriptor.worktree == origin.key.canonical_path
                && root_identity(&origin.key.canonical_path) == Some(origin.root_identity)
                && tab.change_source_revision == origin.source_revision
                && tab.navigation.active_worktree() == Some(&origin.key)
                && tab.navigation.change_selection_revision() == origin.selection_revision
                && tab.sidebar.source_control.as_ref().is_some_and(|source| {
                    source.key == origin.key
                        && source.group(request.area).is_some_and(|group| !group.is_empty())
                })
        })
    }

    pub(super) fn enqueue_change_group(
        &mut self,
        key: ActiveWorktreeKey,
        area: ChangeArea,
        command: sidebar::ChangeGroupCommand,
    ) -> Task<Message> {
        if self.modal_open() || !allowed(area, command) { return Task::none(); }
        let Some(tab) = self.current().filter(|tab| {
            tab.git && tab.navigation.active_worktree() == Some(&key)
                && tab.runtime.descriptor.worktree == key.canonical_path
                && !tab.snapshot_loading
        }) else { return Task::none(); };
        let Some(source) = tab.sidebar.source_control.as_ref().filter(|source| {
            source.key == key && !source.is_reloading()
                && source.group(area).is_some_and(|group| !group.is_empty())
                && !(source.busy() && command == sidebar::ChangeGroupCommand::Discard)
        }) else { return Task::none(); };
        // Resolve selected paths only on activation. Ordinary whole-area
        // availability and its captured button contain no path vector.
        let selected = if let Some(row) = sidebar::selected_change_group_anchor(source, &tab.navigation, area) {
            let Some(action) = sidebar::resolve_change_command(source, &tab.navigation, &key, &row, command) else {
                return Task::none();
            };
            let selection = match action {
                SidebarAction::Stage { selection, .. } | SidebarAction::Unstage { selection, .. } => selection,
                SidebarAction::RequestDiscard { request, .. } => {
                    let mut selection = request.tracked;
                    selection.extend(request.untracked.into_iter().map(PathSelection::new));
                    selection
                }
                _ => return Task::none(),
            };
            Some(Arc::new(selection))
        } else { None };
        let Some(identity) = root_identity(&key.canonical_path) else { return Task::none(); };
        let origin = GroupOrigin {
            card: tab.id,
            key: key.clone(),
            source_revision: tab.change_source_revision,
            selection_revision: tab.navigation.change_selection_revision(),
            root_identity: identity,
            operation: self.next_group_operation,
        };
        self.next_group_operation = self.next_group_operation.wrapping_add(1);
        self.git_groups.entry(key.canonical_path.clone()).or_default().pending.push_back(GroupRequest { origin, area, command, selected });
        self.start_next_change_group(&key.canonical_path).unwrap_or_else(Task::none)
    }

    pub(super) fn start_next_change_group(&mut self, root: &Path) -> Option<Task<Message>> {
        if self.modal_open() || self.exiting || self.tabs.iter().any(|tab| tab.runtime.descriptor.worktree == root && tab.mutation.is_some()) {
            return None;
        }
        loop {
            let queue = self.git_groups.get_mut(root)?;
            if queue.active.is_some() { return None; }
            let Some(request) = queue.pending.pop_front() else {
                self.git_groups.remove(root);
                return None;
            };
            if !self.group_origin_current(&request) {
                self.notice = Some("The original change group or selection is no longer available.".into());
                continue;
            }
            if self.begin_mutation(request.origin.card, MutationKind::Other).is_none() { continue; }
            self.git_groups.get_mut(root).unwrap().active = Some(request.origin.operation);
            return Some(cosmic::task::future(async move {
                let captured = request.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let service = RepositoryService::system("git");
                    // This is intentionally uncapped, unlike every display snapshot.
                    let status = service.status(&captured.origin.key.canonical_path, &CancellationToken::default())
                        .map_err(|error| error.to_string())?;
                    let selected = captured.selected.as_ref().map(|selection| {
                        cagents::repository::selection_pathspecs(selection).into_iter().collect::<BTreeSet<_>>()
                    });
                    let entries = status.entries.iter().filter(|entry| {
                        allowed(entry.area, captured.command) && match &selected {
                            Some(paths) => paths.contains(&entry.path) || entry.old_path.as_ref().is_some_and(|path| paths.contains(path)),
                            None => entry.area == captured.area,
                        }
                    }).collect::<Vec<_>>();
                    Ok(GroupSelection {
                        paths: entries.iter().map(|entry| PathSelection::from_changed_path(entry)).collect(),
                        untracked: entries.iter().filter(|entry| entry.area == ChangeArea::Untracked).map(|entry| entry.path.clone()).collect(),
                    })
                }).await.map_err(|error| format!("Could not read the change group: {error}"))
                    .and_then(|result| result);
                Message::ChangeGroupResolved { request, result }
            }));
        }
    }

    pub(super) fn resume_change_groups(&mut self) -> Task<Message> {
        let roots = self.git_groups.keys().cloned().collect::<Vec<_>>();
        Task::batch(roots.iter().filter_map(|root| self.start_next_change_group(root)).collect::<Vec<_>>())
    }

    pub(super) fn finish_change_group(
        &mut self,
        request: GroupRequest,
        result: Result<GroupSelection, String>,
    ) -> Task<Message> {
        let root = request.origin.key.canonical_path.clone();
        let Some(queue) = self.git_groups.get_mut(&root).filter(|queue| queue.active == Some(request.origin.operation)) else {
            return Task::none();
        };
        queue.active = None;
        if self.modal_open() || !self.group_origin_current(&request) {
            return self.finish_mutation_root(root.clone(), Some("The original change group or selection changed before the action could run.".into()));
        }
        let selection = match result {
            Ok(selection) if !selection.paths.is_empty() => selection,
            Ok(_) => return self.finish_mutation_root(root.clone(), None),
            Err(error) => return self.finish_mutation_root(root.clone(), Some(error)),
        };
        // Transfer the reserved read turn to the existing mutation or exact
        // confirmation pipeline; no other queued intention can run in between.
        for tab in &mut self.tabs {
            if tab.runtime.descriptor.worktree == root {
                tab.mutation = None;
                if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_mutation(None); }
            }
        }
        match request.command {
            sidebar::ChangeGroupCommand::Stage => self.stage_selection(request.origin.card, true, selection.paths),
            sidebar::ChangeGroupCommand::Unstage => self.stage_selection(request.origin.card, false, selection.paths),
            sidebar::ChangeGroupCommand::Discard => {
                if self.active != Some(request.origin.card) {
                    return self.finish_mutation_root(root.clone(), Some("Return to the original tab and retry Discard.".into()));
                }
                let mut tracked = Vec::new();
                let mut untracked = Vec::new();
                // Classification comes from the same complete worker read,
                // never from the capped display or a later widened query.
                for entry in selection.paths {
                    if selection.untracked.contains(&entry.path) { untracked.push(entry.path); }
                    else { tracked.push(entry); }
                }
                self.update(Message::Sidebar(SidebarAction::RequestDiscard {
                    key: request.origin.key,
                    request: DiscardRequest { worktree: root.clone(), tracked, untracked, confirmed: false },
                }))
            }
        }
    }
}
