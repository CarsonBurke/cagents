//! Native repository actions use the source that produced their displayed bytes.
use super::*;
use crate::repository::GitReadSource;

impl Cagents {
    pub(super) fn original_git_source_for_view(&self, view: &RepositoryView) -> Option<GitReadSource> {
        self.refresh_ui.source_binding.as_ref()
            .filter(|binding| binding.key.project_id == view.project
                && binding.key.canonical_path == view.worktree)
            .and_then(|binding| binding.read_source.as_ref())
            .filter(|source| source.is_available() && source.authority().path() == view.worktree)
            .cloned()
            .or_else(|| self.cards.values().find_map(|runtime| {
                let CardRuntime::Repository(runtime) = runtime else { return None; };
                (runtime.descriptor.project == view.project && runtime.descriptor.worktree == view.worktree)
                    .then(|| runtime.git_read_source()).flatten()
                    .filter(|source| source.is_available() && source.authority().path() == view.worktree).cloned()
            }))
    }

    /// Only a still-mounted original reader or active queue keeps this actor
    /// alive off strip. There is no permanent per-path authority cache.
    pub(super) fn retained_runtime_git_binding(&self, key: &ActiveWorktreeKey) -> Option<git_binding::Binding> {
        self.cards.values().find_map(|runtime| {
            let CardRuntime::Repository(runtime) = runtime else { return None; };
            if runtime.descriptor.project != key.project_id || runtime.descriptor.worktree != key.canonical_path { return None; }
            git_binding::Binding::from_read_source(key.clone(), runtime.git_read_source()?.clone())
        })
    }

    pub(super) fn runtime_git_source_is_current(&self, runtime: &RepositoryRuntime) -> bool {
        let Some(source) = runtime.git_read_source() else { return false; };
        self.refresh_ui.source_binding.as_ref().is_some_and(|binding|
            binding.key.project_id == runtime.descriptor.project
                && binding.key.canonical_path == runtime.descriptor.worktree
                && self.sidebar_model.source_control.as_ref().is_some_and(|model|
                    model.key == binding.key && model.command_binding_id() == source.id())
                && source.is_available()
                && source.authority().path() == runtime.descriptor.worktree
                && binding.read_source.as_ref().is_some_and(|current| source.same_source(current)))
    }

    /// A project ownership change permanently revokes its old native readers.
    /// Ordinary verified refresh/navigation does not call this retirement.
    pub(super) fn retire_project_git_readers(&mut self, project: ProjectId) {
        for runtime in self.cards.values() {
            if let CardRuntime::Repository(runtime) = runtime
                && runtime.descriptor.project == project
                && let Some(source) = runtime.git_read_source()
            { source.invalidate(); }
        }
        if let Some(binding) = self.refresh_ui.source_binding.as_ref()
            && binding.key.project_id == project
        {
            if let Some(source) = &binding.read_source { source.invalidate(); }
            self.cancel_source_read();
            // Old queue workers retain their own original (revoked) source;
            // a new actor cannot join or receive their completion projection.
            self.refresh_ui.source_binding = None;
        }
        for active in self.source_control_mutations.values_mut().filter(|active| active.binding.key.project_id == project) {
            if let Some(source) = &active.binding.read_source { source.invalidate(); }
            active.cancellation.cancel();
            active.pending.clear();
        }
    }
}
