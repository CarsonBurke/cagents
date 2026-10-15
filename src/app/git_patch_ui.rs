//! Worker-built partial changes use the same mutation queue as whole files.

use super::*;
use crate::{git_patch, repository_widget::DiffMutationRequest};

impl Cagents {
    pub(super) fn start_diff_patch(
        &mut self,
        card: CardId,
        kind: git_patch::Kind,
        request: Option<Arc<DiffMutationRequest>>,
    ) -> Task<Message> {
        if self.modal_is_open() { return Task::none(); }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else { return Task::none(); };
        let Some(source) = self.sidebar_model.source_control.as_ref()
            .filter(|source| source.key.canonical_path == runtime.descriptor.worktree)
        else { return Task::none(); };
        let Some(capture) = runtime.capture_diff_mutation(card, kind, request.as_deref()) else { return Task::none(); };
        let key = source.key.clone();
        let identity = capture.identity.clone();
        let buffer_revision = capture.buffer_revision;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || capture.build().map(|(_, _, patch)| patch))
                .await.map_err(|error| format!("Could not prepare the selected changes: {error}"));
            Message::DiffPatchPrepared { key, identity, buffer_revision, result }
        })
    }

    pub(super) fn finish_diff_patch(
        &mut self,
        key: ActiveWorktreeKey,
        identity: repository_widget::AsyncIdentity,
        buffer_revision: Option<u64>,
        result: Result<Option<git_patch::Patch>, String>,
    ) -> Task<Message> {
        if !self.sidebar_model.contains_worktree(&key) { return Task::none(); }
        if self.modal_is_open() {
            self.status = Some(TransientNotice::warning("Close the dialog and retry the selected change."));
            return Task::none();
        }
        let mut patch = match result {
            Ok(Some(patch)) => patch,
            Ok(None) => {
                self.status = Some(TransientNotice::warning("The selected range contains no applicable changed lines."));
                return Task::none();
            }
            Err(error) => { self.status = Some(TransientNotice::warning(error)); return Task::none(); }
        };
        if let Some(revision) = buffer_revision {
            let Some(change) = patch.buffer_change.take() else { return Task::none(); };
            let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&identity.card) else { return Task::none(); };
            if let Err(error) = runtime.apply_diff_buffer_revert(&identity, revision, change) {
                self.status = Some(TransientNotice::warning(error));
                return Task::none();
            }
            return self.drain_lsp_intents(identity.card);
        }
        self.update(Message::Sidebar(SidebarAction::GitOperation { key,
            operation: crate::repository::GitOperation::ApplyPatch { patch: patch.patch,
                cached: patch.cached, reverse: patch.reverse, expected_head: patch.expected_head },
        }))
    }
}
