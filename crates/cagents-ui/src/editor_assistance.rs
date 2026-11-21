//! One editor buffer's language-server attachment and result transitions.
//!
//! The shell supplies manager versions and editor intent/caret snapshots, then
//! runs the returned cancellation or presentation effect. Document lifecycle,
//! event routing, server policy and notices remain with the shell.

use crate::{
    inspector::LspInspectorContext,
    lsp::{
        LspDiagnostic, LspDocumentId, LspPosition, LspRequestId, LspTextEdit, shift_diagnostics,
    },
};

/// Answers and request identities belonging to one card or tab's buffer.
#[derive(Debug, Default)]
pub(crate) struct AttachmentState {
    pub(crate) context: LspInspectorContext,
    pub(crate) document: Option<LspDocumentId>,
    /// The manager's version, including reopen/restart transitions, rather
    /// than a local count of edits.
    pub(crate) diagnostics_version: Option<i32>,
    pub(crate) completion_request: Option<(LspRequestId, LspPosition)>,
    /// Last observed runtime intent; a request is captured against this value.
    pub(crate) completion_intent: u64,
    pub(crate) editor_hover_request: Option<(LspRequestId, LspPosition)>,
    pub(crate) editor_navigation: Option<LspRequestId>,
}

impl AttachmentState {
    /// Retire a request even when a new intent leaves the text/caret unchanged.
    /// The shell cancels the returned request at the manager boundary.
    pub(crate) fn reconcile_completion_intent(&mut self, intent: u64) -> Option<LspRequestId> {
        if self.completion_intent == intent {
            return None;
        }
        self.completion_intent = intent;
        self.context.completions.clear();
        self.completion_request.take().map(|(request, _)| request)
    }

    /// Drop position-addressed answers while retaining standing diagnostics.
    pub(crate) fn invalidate_answers(&mut self) {
        self.context.locations = None;
        self.context.completions.clear();
        self.completion_request = None;
        self.editor_hover_request = None;
        self.editor_navigation = None;
    }

    /// Rebase only a current standing set through exact edits. An inexact
    /// transition has no safe coordinate mapping, including for native marks.
    pub(crate) fn document_changed(
        &mut self,
        previous_version: Option<i32>,
        synchronized_version: Option<i32>,
        edits: &[LspTextEdit],
    ) {
        self.invalidate_answers();
        if !edits.is_empty() && self.diagnostics_are_current(previous_version) {
            shift_diagnostics(&mut self.context.diagnostics, edits);
            self.diagnostics_version = synchronized_version;
        } else {
            self.context.diagnostics.clear();
            self.diagnostics_version = None;
        }
    }

    /// Refuse a delayed publication without erasing a newer standing set.
    /// On acceptance the shell copies the set into native editor marks.
    pub(crate) fn set_diagnostics(
        &mut self,
        items: Vec<LspDiagnostic>,
        version: i32,
        document_version: Option<i32>,
    ) -> bool {
        if document_version != Some(version) {
            return false;
        }
        self.context.diagnostics = items;
        self.diagnostics_version = Some(version);
        true
    }

    pub(crate) const fn diagnostics_are_current(&self, document_version: Option<i32>) -> bool {
        diagnostics_are_current(self.diagnostics_version, document_version)
    }

    /// Admit only the pending request at its original caret. The shell then
    /// publishes the candidates through the editor and records its targets.
    pub(crate) fn accept_completion(
        &mut self,
        request: LspRequestId,
        caret: Option<LspPosition>,
    ) -> bool {
        if let Some((expected, at)) = self.completion_request
            && request == expected
            && caret == Some(at)
        {
            self.completion_request = None;
            return true;
        }
        false
    }

    /// The shell places a matching hover at the request's captured position.
    pub(crate) fn hover_position(&self, request: LspRequestId) -> Option<LspPosition> {
        self.editor_hover_request
            .filter(|(expected, _)| request == *expected)
            .map(|(_, at)| at)
    }

    /// A definition response can trigger navigation only once, for its request.
    pub(crate) fn accept_navigation(&mut self, request: LspRequestId) -> bool {
        if self.editor_navigation != Some(request) {
            return false;
        }
        self.editor_navigation = None;
        true
    }
}

/// Both versions must be known and equal before diagnostics can be used.
pub(crate) const fn diagnostics_are_current(published: Option<i32>, document: Option<i32>) -> bool {
    match (published, document) {
        (Some(published), Some(document)) => published == document,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
