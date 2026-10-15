//! Host adapters for immutable, ordered Git intentions.

use super::*;
use cagents::repository::{ChangeArea, DiscardRequest};
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct GroupOrigin {
    card: CardId,
    key: ActiveWorktreeKey,
    source_revision: u64,
    selection_revision: u64,
    authority: Option<WorktreeAuthority>,
    operation: u64,
    mutation_revision: u64,
    cancellation: CancellationToken,
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

#[derive(Debug, Clone)]
pub struct PatchRequest {
    origin: GroupOrigin,
    prepared: Option<Arc<repository_widget::PreparedDiffMutation>>,
}

pub(super) enum QueuedIntent {
    Group(GroupRequest),
    Patch(PatchRequest),
}

#[derive(Default)]
pub(super) struct GroupQueue {
    pub(super) pending: VecDeque<QueuedIntent>,
    pub(super) active: Option<u64>,
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


impl Editor {
    fn origin_current(&self, origin: &GroupOrigin) -> bool {
        !self.exiting && self.tab(origin.card).is_some_and(|tab| {
            tab.git && tab.runtime.descriptor.worktree == origin.key.canonical_path
                && origin.authority.as_ref().is_none_or(|authority| authority.path() == origin.key.canonical_path)
                && tab.change_source_revision == origin.source_revision
                && tab.navigation.active_worktree() == Some(&origin.key)
                && tab.navigation.change_selection_revision() == origin.selection_revision
                && tab.sidebar.source_control.as_ref().is_some_and(|source| {
                    source.key == origin.key

                })
        })
    }

    fn group_origin_current(&self, request: &GroupRequest) -> bool {
        self.origin_current(&request.origin) && (request.selected.as_ref().is_some_and(|selection| !selection.is_empty()) || self.tab(request.origin.card).and_then(|tab| tab.sidebar.source_control.as_ref())
            .is_some_and(|source| source.group(request.area).is_some_and(|group| !group.is_empty())))
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
        if !source.mutations_available() { return self.refuse_git_mutation(tab.id); }
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
        let Some(authority) = tab.worktree_authority.clone() else { return self.refuse_git_mutation(tab.id); };
        let origin = GroupOrigin {
            card: tab.id,
            key: key.clone(),
            source_revision: tab.change_source_revision,
            selection_revision: tab.navigation.change_selection_revision(),
            authority: Some(authority),
            operation: self.next_group_operation,
            mutation_revision: tab.git_mutation_revision,
            cancellation: CancellationToken::default(),
        };
        self.next_group_operation = self.next_group_operation.wrapping_add(1);
        self.git_groups.entry(key.canonical_path.clone()).or_default().pending.push_back(QueuedIntent::Group(GroupRequest { origin, area, command, selected }));
        self.start_next_change_group(&key.canonical_path).unwrap_or_else(Task::none)
    }

    pub(super) fn start_next_change_group(&mut self, root: &Path) -> Option<Task<Message>> {
        if self.modal_open() || self.exiting || self.tabs.iter().any(|tab| tab.runtime.descriptor.worktree == root && (tab.mutation.is_some() || tab.sidebar.source_control.as_ref().is_some_and(|source| source.pending_marker_stage.is_some()))) {
            return None;
        }
        loop {
            let queue = self.git_groups.get_mut(root)?;
            if queue.active.is_some() { return None; }
            if matches!(queue.pending.front(), Some(QueuedIntent::Patch(PatchRequest { prepared: None, .. }))) {
                for tab in &mut self.tabs {
                    if tab.runtime.descriptor.worktree == root {
                        if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_mutation(Some(MutationKind::Other)); }
                    }
                }
                return None;
            }
            let Some(intent) = queue.pending.pop_front() else {
                self.git_groups.remove(root);
                for tab in &mut self.tabs {
                    if tab.runtime.descriptor.worktree == root {
                        if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_mutation(tab.mutation); }
                    }
                }
                return None;
            };
            let request = match intent {
                QueuedIntent::Group(request) => request,
                QueuedIntent::Patch(request) => {
                    queue.active = Some(request.origin.operation);
                    return Some(self.run_prepared_patch(request));
                },
            };
            if !self.group_origin_current(&request) {
                self.notice = Some("The original change group or selection is no longer available.".into());
                continue;
            }
            self.git_groups.get_mut(root).unwrap().active = Some(request.origin.operation);
            if self.begin_mutation_with_buffer_policy(request.origin.card, MutationKind::Other, false, Some(request.origin.operation)).is_none() {
                self.git_groups.get_mut(root).unwrap().active = None;
                continue;
            }
            let cancellation = self.mutation_cancellation(request.origin.card).expect("admitted group read has cancellation");
            return Some(cosmic::task::future(async move {
                let captured = request.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let service = RepositoryService::system("git").with_worktree_authority(captured.origin.authority.clone().ok_or("The original worktree binding is unavailable.")?).map_err(|error| error.to_string())?;
                    // This is intentionally uncapped, unlike every display snapshot.
                    let status = service.status(&captured.origin.key.canonical_path, &cancellation)
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
        // A successful read may already be queued for delivery when Cancel
        // is clicked. Cancellation owns the read-to-effect handoff too.
        if self.mutation_cancellation(request.origin.card).is_some_and(|cancellation| cancellation.is_cancelled()) {
            let notice = match result { Err(error) => error, Ok(_) => "Git action cancelled before applying the resolved selection.".into() };
            return self.finish_mutation_root(root, Some(notice));
        }
        if self.modal_open() || !self.group_origin_current(&request) {
            return self.finish_mutation_root(root.clone(), Some("The original change group or selection changed before the action could run.".into()));
        }
        let selection = match result {
            Ok(selection) if !selection.paths.is_empty() => selection,
            Ok(_) => return self.finish_mutation_root(root.clone(), None),
            Err(error) => return self.finish_mutation_root(root.clone(), Some(error)),
        };
        if let Some(reason) = self.tab(request.origin.card).and_then(|tab| tab.sidebar.source_control.as_ref()).and_then(SourceControlModel::mutation_unavailable_reason).map(str::to_owned) {
            return self.finish_mutation_root(root, Some(reason));
        }
        // Transfer the reserved read turn to the existing mutation or exact
        // confirmation pipeline; no other queued intention can run in between.
        for tab in &mut self.tabs {
            if tab.runtime.descriptor.worktree == root {
                tab.mutation = None;
                tab.mutation_cancel = None;
                if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_mutation(None); }
            }
        }
        match request.command {
            sidebar::ChangeGroupCommand::Stage => self.stage_selection_with_approval(request.origin.card, true, selection.paths, BTreeMap::new(), Some(request.origin.operation)),
            sidebar::ChangeGroupCommand::Unstage => self.stage_selection_with_approval(request.origin.card, false, selection.paths, BTreeMap::new(), Some(request.origin.operation)),
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
                let card = request.origin.card;
                let operation = request.origin.operation;
                let task = self.update(Message::Sidebar(SidebarAction::RequestDiscard {
                    key: request.origin.key,
                    request: DiscardRequest { worktree: root.clone(), tracked, untracked, confirmed: false },
                }));
                if let Some(tab) = self.tab_mut(card).filter(|tab| tab.navigation.pending_discard().is_some()) {
                    tab.discard_queue_turn = Some(operation);
                    task
                } else {
                    if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
                    Task::batch([task, self.resume_change_groups()])
                }
            }
        }
    }
    pub(super) fn enqueue_explicit_stage(&mut self, card: CardId, stage: bool, selection: Vec<PathSelection>) -> Task<Message> {
        if self.modal_open() || self.exiting || selection.is_empty() { return Task::none(); }
        let Some(tab) = self.tab(card).filter(|tab| tab.git && !tab.snapshot_loading) else { return Task::none(); };
        let Some(key) = tab.navigation.active_worktree().cloned() else { return Task::none(); };
        if !tab.sidebar.source_control.as_ref().is_some_and(|source| source.key == key && !source.is_reloading()) { return Task::none(); }
        if !tab.sidebar.source_control.as_ref().is_some_and(SourceControlModel::mutations_available) { return self.refuse_git_mutation(card); }
        let Some(authority) = tab.worktree_authority.clone() else { return self.refuse_git_mutation(card); };
        let origin = GroupOrigin { card, key: key.clone(), source_revision: tab.change_source_revision,
            selection_revision: tab.navigation.change_selection_revision(), authority: Some(authority),
            operation: self.next_group_operation, mutation_revision: tab.git_mutation_revision, cancellation: CancellationToken::default() };
        self.next_group_operation = self.next_group_operation.wrapping_add(1);
        let command = if stage { sidebar::ChangeGroupCommand::Stage } else { sidebar::ChangeGroupCommand::Unstage };
        let area = if stage { ChangeArea::Unstaged } else { ChangeArea::Staged };
        self.git_groups.entry(key.canonical_path.clone()).or_default().pending.push_back(QueuedIntent::Group(GroupRequest { origin, area, command, selected: Some(Arc::new(selection)) }));
        self.start_next_change_group(&key.canonical_path).unwrap_or_else(Task::none)
    }

    pub(super) fn cancel_git_mutations(&mut self, card: CardId) -> Task<Message> {
        let Some(root) = self.tab(card).map(|tab| tab.runtime.descriptor.worktree.clone()) else { return Task::none(); };
        if let Some(queue) = self.git_groups.get_mut(&root) {
            for intent in &queue.pending {
                match intent { QueuedIntent::Group(request) => request.origin.cancellation.cancel(), QueuedIntent::Patch(request) => request.origin.cancellation.cancel() }
            }
            queue.pending.clear();
        }
        let mut running = false;
        for tab in &mut self.tabs {
            if tab.runtime.descriptor.worktree == root {
                running |= tab.mutation.is_some();
                if let Some(cancellation) = &tab.mutation_cancel { cancellation.cancel(); }
                if let Some(source) = tab.sidebar.source_control.as_mut() {
                    source.pending_marker_stage = None;
                    source.pending_marker_approval.clear();
                }
                tab.marker_authority = None;
            }
        }
        if running {
            self.notice = Some("Cancelling Git; waiting for the operation's actual result.".into());
            Task::none()
        } else {
            self.git_groups.remove(&root);
            for tab in &mut self.tabs {
                if tab.runtime.descriptor.worktree == root {
                    if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_mutation(None); }
                }
            }
            self.notice = Some("Queued Git actions cancelled.".into());
            Task::none()
        }
    }

    pub(super) fn reserve_marker_turn(&mut self, card: CardId) -> Option<u64> {
        let root = self.tab(card)?.runtime.descriptor.worktree.clone();
        let queue = self.git_groups.entry(root).or_default();
        if queue.active.is_some() { return None; }
        let operation = self.next_group_operation;
        self.next_group_operation = self.next_group_operation.wrapping_add(1);
        queue.active = Some(operation);
        Some(operation)
    }

    pub(super) fn start_diff_patch(&mut self, card: CardId, kind: cagents::git_patch::Kind, request: Arc<repository_widget::DiffMutationRequest>) -> Task<Message> {
        if self.modal_open() || self.exiting { return Task::none(); }
        let Some(tab) = self.tab(card) else { return Task::none(); };
        let Some(key) = tab.navigation.active_worktree().cloned() else { return Task::none(); };
        let Some(capture) = tab.runtime.capture_diff_mutation(card, kind, Some(&request)) else { return Task::none(); };
        let buffer_only = capture.applies_to_buffer();
        if !buffer_only && (tab.worktree_authority.is_none() || !tab.sidebar.source_control.as_ref().is_some_and(SourceControlModel::mutations_available)) { return self.refuse_git_mutation(card); }
        let authority = tab.worktree_authority.clone();
        let origin = GroupOrigin {
            card, key: key.clone(), source_revision: tab.change_source_revision,
            selection_revision: tab.navigation.change_selection_revision(), authority,
            operation: self.next_group_operation, mutation_revision: tab.git_mutation_revision,
            cancellation: CancellationToken::default(),
        };
        self.next_group_operation = self.next_group_operation.wrapping_add(1);
        // Reserve the intention's position before preparation; a later cheap
        // group read cannot overtake an earlier large patch preparation.
        self.git_groups.entry(key.canonical_path.clone()).or_default().pending.push_back(QueuedIntent::Patch(PatchRequest { origin: origin.clone(), prepared: None }));
        for tab in &mut self.tabs {
            if tab.runtime.descriptor.worktree == key.canonical_path {
                if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_mutation(Some(MutationKind::Other)); }
            }
        }
        cosmic::task::future(async move {
            let authority = origin.authority.clone();
            let cancellation = origin.cancellation.clone();
            let result = tokio::task::spawn_blocking(move || {
                if cancellation.is_cancelled() { return Err("Preparing the selected change was cancelled.".to_string()); }
                if !buffer_only {
                    authority.as_ref().ok_or("The original worktree binding is unavailable.")?.validate().map_err(|error| error.to_string())?;
                }
                let prepared = capture.prepare();
                if cancellation.is_cancelled() { return Err("Preparing the selected change was cancelled.".to_string()); }
                Ok(prepared)
            }).await.map_err(|error| format!("Could not prepare the selected changes: {error}")).and_then(|result| result);
            Message::DiffPatchPrepared { origin, result }
        })
    }

    pub(super) fn finish_diff_patch(&mut self, origin: GroupOrigin, result: Result<Option<repository_widget::PreparedDiffMutation>, String>) -> Task<Message> {
        let root = origin.key.canonical_path.clone();
        let Some(queue) = self.git_groups.get_mut(&root) else { return Task::none(); };
        let Some(index) = queue.pending.iter().position(|intent| matches!(intent, QueuedIntent::Patch(request) if request.origin.operation == origin.operation)) else { return Task::none(); };
        match result {
            Ok(Some(prepared)) => {
                if let Some(QueuedIntent::Patch(request)) = queue.pending.get_mut(index) { request.prepared = Some(Arc::new(prepared)); }
            }
            other => {
                queue.pending.remove(index);
                self.notice = Some(match other { Err(error) => error, _ => "The selected range contains no applicable changed lines.".into() });
            }
        }
        self.start_next_change_group(&root).unwrap_or_else(Task::none)
    }

    fn run_prepared_patch(&mut self, request: PatchRequest) -> Task<Message> {
        let origin = request.origin;
        let root = origin.key.canonical_path.clone();
        if !self.origin_current(&origin) || !self.tab(origin.card).is_some_and(|tab| tab.git_mutation_revision == origin.mutation_revision) {
            self.notice = Some("The original diff or worktree changed before the selected action could run.".into());
            if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
            return self.start_next_change_group(&root).unwrap_or_else(Task::none);
        }
        let Some(prepared) = request.prepared else { return Task::none(); };
        let applies_to_buffer = prepared.applies_to_buffer();
        let index_only_stage = prepared.stages_live_buffer();
        if !applies_to_buffer && self.begin_mutation_with_buffer_policy(origin.card, MutationKind::Other, index_only_stage, Some(origin.operation)).is_none() {
            if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
            return self.start_next_change_group(&root).unwrap_or_else(Task::none);
        }
        let operation = match Arc::unwrap_or_clone(prepared).apply(&mut self.tab_mut(origin.card).unwrap().runtime) {
            Ok(operation) => operation,
            Err(error) => {
                if applies_to_buffer {
                    self.notice = Some(error);
                    if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
                    return self.start_next_change_group(&root).unwrap_or_else(Task::none);
                }
                return self.finish_mutation_root(root, Some(error));
            }
        };
        let Some(operation) = operation else {
            if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
            return Task::batch([self.drain(origin.card), self.start_next_change_group(&root).unwrap_or_else(Task::none)]);
        };
        let cancellation = self.mutation_cancellation(origin.card).expect("admitted patch has cancellation");
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git").with_worktree_authority(origin.authority.expect("admitted disk patch retains its authority"))?;
                match operation {
                    cagents::repository::GitOperation::ApplyPatch { patch, cached: false, reverse, expected_head } => {
                        let result = service.apply_patch_recoverable(&root, &patch, reverse, expected_head.as_ref(), &cancellation)?;
                        Ok((result.outcome, Some(result.recovery)))
                    }
                    other => service.git_operation(&root, &other, &cancellation).map(|outcome| (outcome, None)),
                }
            }).await.map_err(|error| error.to_string()).and_then(|result: Result<_, cagents::repository::RepositoryError>| result.map_err(|error| error.to_string()));
            Message::PatchMutationFinished { card: origin.card, result }
        })
    }

}
