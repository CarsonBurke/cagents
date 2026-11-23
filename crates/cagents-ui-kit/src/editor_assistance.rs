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
pub struct AttachmentState {
    pub context: LspInspectorContext,
    /// The document, once opened. `None` while no file is open, or when no
    /// configured server claims this file's extension, or another independent
    /// buffer already owns its language-server attachment.
    pub document: Option<LspDocumentId>,
    /// The version the diagnostics were computed for, as the manager stamped
    /// them. Compared against the manager's document version and never against
    /// a count of this side's own: the manager re-opens a document on a
    /// restart, a crash-respawn, or a second buffer opening the same file, and
    /// none of those are edits this side made.
    diagnostics_version: Option<i32>,
    completion_request: Option<(LspRequestId, LspPosition)>,
    /// Last observed runtime intent; a request is captured against this value.
    completion_intent: u64,
    hover_request: Option<(LspRequestId, LspPosition)>,
    navigation_request: Option<LspRequestId>,
}

impl AttachmentState {
    /// Retire a request even when a new intent leaves the text/caret unchanged.
    /// The shell cancels the returned request at the manager boundary.
    pub fn reconcile_completion_intent(&mut self, intent: u64) -> Option<LspRequestId> {
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
    pub fn invalidate_answers(&mut self) {
        self.context.locations = None;
        self.context.completions.clear();
        self.completion_request = None;
        self.hover_request = None;
        self.navigation_request = None;
    }

    /// Rebase only a current standing set through exact edits. An inexact
    /// transition has no safe coordinate mapping, including for native marks.
    pub fn document_changed(
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
    pub fn clear_diagnostics(&mut self) {
        self.context.diagnostics.clear();
        self.diagnostics_version = None;
    }

    /// Forget everything shown for a closed document. Outstanding request
    /// identities stay; their responses no longer route to this buffer.
    pub fn clear_results(&mut self) {
        self.context = LspInspectorContext::default();
        self.diagnostics_version = None;
    }

    /// Record the completion request answering the caret at `at`.
    pub fn completion_requested(&mut self, request: LspRequestId, at: LspPosition) {
        self.completion_request = Some((request, at));
    }

    /// Retire the pending hover so the shell can cancel it before asking again.
    pub fn take_hover_request(&mut self) -> Option<LspRequestId> {
        self.hover_request.take().map(|(request, _)| request)
    }

    /// Record the hover request answering the pointer at `at`.
    pub fn hover_requested(&mut self, request: LspRequestId, at: LspPosition) {
        self.hover_request = Some((request, at));
    }

    /// Record the definition request whose answer may move the caret.
    pub fn navigation_requested(&mut self, request: LspRequestId) {
        self.navigation_request = Some(request);
    }

    /// Refuse a delayed publication without erasing a newer standing set.
    /// On acceptance the shell copies the set into native editor marks.
    #[must_use]
    pub fn accept_diagnostics(
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

    #[cfg(any(test, feature = "test-support"))]
    pub const fn diagnostics_version(&self) -> Option<i32> {
        self.diagnostics_version
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn completion_request(&self) -> Option<(LspRequestId, LspPosition)> {
        self.completion_request
    }

    pub const fn diagnostics_are_current(&self, document_version: Option<i32>) -> bool {
        diagnostics_are_current(self.diagnostics_version, document_version)
    }

    /// Admit only the pending request at its original caret. The shell then
    /// publishes the candidates through the editor and records its targets.
    #[must_use]
    pub fn accept_completion(&mut self, request: LspRequestId, caret: Option<LspPosition>) -> bool {
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
    pub fn hover_position(&self, request: LspRequestId) -> Option<LspPosition> {
        self.hover_request
            .filter(|(expected, _)| request == *expected)
            .map(|(_, at)| at)
    }

    /// A definition response can trigger navigation only once, for its request.
    #[must_use]
    pub fn accept_navigation(&mut self, request: LspRequestId) -> bool {
        if self.navigation_request != Some(request) {
            return false;
        }
        self.navigation_request = None;
        true
    }
}

/// Whether a published diagnostics list still describes the buffer on screen.
///
/// `published` is the version the manager stamped the list with; `document` is
/// the version its snapshot holds now. Both must be known and equal: an
/// unknown document has no diagnostics worth jumping into, and an unstamped
/// list is one this side never received.
const fn diagnostics_are_current(published: Option<i32>, document: Option<i32>) -> bool {
    match (published, document) {
        (Some(published), Some(document)) => published == document,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
