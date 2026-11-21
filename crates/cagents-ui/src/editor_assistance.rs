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

/// One buffer's language-server results, with the buffer version each describes.
///
/// A server answers the text it was given. Keeping the version beside the
/// answer is what lets a stale one be refused instead of applied to text that
/// has since moved: a caret jump to a line that no longer exists, or a
/// completion whose edit range describes characters the user has already
/// replaced, are both silent corruptions, and neither is recoverable by
/// looking at the answer alone.
#[derive(Debug, Default)]
pub(crate) struct AttachmentState {
    pub(crate) context: LspInspectorContext,
    /// The document, once opened. `None` while no file is open, or when no
    /// configured server claims this file's extension, or another independent
    /// buffer already owns its language-server attachment.
    pub(crate) document: Option<LspDocumentId>,
    /// The version the diagnostics were computed for, as the manager stamped
    /// them. Compared against the manager's document version and never against
    /// a count of this side's own: the manager re-opens a document on a
    /// restart, a crash-respawn, or a second buffer opening the same file, and
    /// none of those are edits this side made.
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

    /// Drops every answer that describes text the buffer no longer holds.
    ///
    /// Diagnostics are kept and re-dated instead of dropped: the server
    /// replaces them wholesale on its own schedule, and blanking them on each
    /// keystroke would make the list flicker empty while the user typed. They
    /// are refused at the point of use by version rather than erased here.
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
            self.clear_diagnostics();
        }
    }

    /// Forget the standing diagnostics, for a buffer whose server will
    /// republish from scratch or whose coordinates can no longer be mapped.
    pub(crate) fn clear_diagnostics(&mut self) {
        self.context.diagnostics.clear();
        self.diagnostics_version = None;
    }

    /// Forget everything shown for a closed document. Outstanding request
    /// identities stay; their responses no longer route to this buffer.
    pub(crate) fn clear_results(&mut self) {
        self.context = LspInspectorContext::default();
        self.diagnostics_version = None;
    }

    /// Record the completion request answering the caret at `at`.
    pub(crate) fn completion_requested(&mut self, request: LspRequestId, at: LspPosition) {
        self.completion_request = Some((request, at));
    }

    /// Retire the pending hover so the shell can cancel it before asking again.
    pub(crate) fn take_hover_request(&mut self) -> Option<LspRequestId> {
        self.editor_hover_request.take().map(|(request, _)| request)
    }

    /// Record the hover request answering the pointer at `at`.
    pub(crate) fn hover_requested(&mut self, request: LspRequestId, at: LspPosition) {
        self.editor_hover_request = Some((request, at));
    }

    /// Record the definition request whose answer may move the caret.
    pub(crate) fn navigation_requested(&mut self, request: LspRequestId) {
        self.editor_navigation = Some(request);
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

/// Whether a published diagnostics list still describes the buffer on screen.
///
/// `published` is the version the manager stamped the list with; `document` is
/// the version its snapshot holds now. Both must be known and equal: an
/// unknown document has no diagnostics worth jumping into, and an unstamped
/// list is one this side never received.
pub(crate) const fn diagnostics_are_current(published: Option<i32>, document: Option<i32>) -> bool {
    match (published, document) {
        (Some(published), Some(document)) => published == document,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
