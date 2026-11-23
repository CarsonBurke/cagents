use super::*;
use crate::lsp::{LocationKind, LspRange, LspSeverity};

fn diagnostic(message: &str) -> LspDiagnostic {
    LspDiagnostic::new(
        LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 3)),
        LspSeverity::Error,
        message.into(),
        None,
        None,
    )
}

fn standing_diagnostics() -> AttachmentState {
    let mut state = AttachmentState::default();
    assert!(state.accept_diagnostics(vec![diagnostic("standing")], 4, Some(4)));
    state
}

/// The two versions are equal only when both are known. An application
/// that kept a count of its own would satisfy this predicate while the
/// manager had silently moved on - a restart, a crash-respawn, or a second
/// buffer opening the same file all bump the manager's version without any
/// edit having been made.
#[test]
fn diagnostics_are_current_only_when_both_versions_are_known_and_equal() {
    assert!(diagnostics_are_current(Some(4), Some(4)));
    assert!(!diagnostics_are_current(Some(4), Some(5)));
    assert!(!diagnostics_are_current(Some(5), Some(4)));
    // No document: nothing published for it can still describe it.
    assert!(!diagnostics_are_current(Some(4), None));
    // Nothing published: there is no list to jump into.
    assert!(!diagnostics_are_current(None, Some(4)));
    assert!(!diagnostics_are_current(None, None));
}

#[test]
fn stale_diagnostics_do_not_replace_the_standing_set() {
    let mut state = standing_diagnostics();
    for version in [Some(5), Some(3), None] {
        assert!(!state.accept_diagnostics(vec![diagnostic("late")], 4, version));
        assert_eq!(state.context.diagnostics[0].message, "standing");
        assert_eq!(state.diagnostics_version, Some(4));
    }
    assert!(state.diagnostics_are_current(Some(4)));
    assert!(!state.diagnostics_are_current(Some(5)));
    assert!(!state.diagnostics_are_current(None));
}

#[test]
fn exact_edits_shift_and_redate_current_diagnostics_and_retire_answers() {
    let mut state = standing_diagnostics();
    let at = LspPosition::new(0, 0);
    state.completion_requested(LspRequestId(1), at);
    state.hover_requested(LspRequestId(2), at);
    state.navigation_requested(LspRequestId(3));
    state.context.locations = Some((LocationKind::References, Vec::new()));
    state.document_changed(
        Some(4),
        Some(5),
        &[LspTextEdit {
            range: LspRange::new(at, at),
            new_text: "\n".into(),
        }],
    );
    assert_eq!(
        state.context.diagnostics[0].range.start,
        LspPosition::new(2, 0)
    );
    assert_eq!(state.diagnostics_version, Some(5));
    assert!(state.completion_request.is_none());
    assert!(state.hover_request.is_none());
    assert!(state.navigation_request.is_none());
    assert!(state.context.locations.is_none());
}

#[test]
fn inexact_edits_and_stale_standing_coordinates_clear_diagnostics() {
    let at = LspPosition::new(0, 0);
    let exact = vec![LspTextEdit {
        range: LspRange::new(at, at),
        new_text: "\n".into(),
    }];
    for (previous, edits) in [(Some(4), vec![]), (Some(5), exact.clone()), (None, exact)] {
        let mut state = standing_diagnostics();
        state.document_changed(previous, Some(6), &edits);
        assert!(state.context.diagnostics.is_empty());
        assert_eq!(state.diagnostics_version, None);
    }
}

#[test]
fn changed_intent_retires_a_request_even_when_the_caret_returns() {
    let mut state = standing_diagnostics();
    let at = LspPosition::new(0, 0);
    state.completion_requested(LspRequestId(1), at);
    state.context.locations = Some((LocationKind::References, Vec::new()));
    assert_eq!(state.reconcile_completion_intent(0), None);
    assert_eq!(state.completion_request, Some((LspRequestId(1), at)));
    assert_eq!(state.reconcile_completion_intent(1), Some(LspRequestId(1)));
    assert_eq!(state.reconcile_completion_intent(2), None);
    assert!(!state.accept_completion(LspRequestId(1), Some(at)));
    assert!(state.context.locations.is_some());
    assert_eq!(state.context.diagnostics[0].message, "standing");
    assert_eq!(state.diagnostics_version, Some(4));
}

#[test]
fn completion_requires_the_pending_identity_and_caret() {
    let mut state = AttachmentState::default();
    let at = LspPosition::new(1, 2);
    let pending = (LspRequestId(7), at);
    state.completion_requested(pending.0, pending.1);
    assert!(!state.accept_completion(LspRequestId(6), Some(at)));
    assert!(!state.accept_completion(pending.0, Some(LspPosition::new(1, 3))));
    assert!(!state.accept_completion(pending.0, None));
    assert_eq!(state.completion_request, Some(pending));
    assert!(state.accept_completion(pending.0, Some(at)));
    assert!(state.completion_request.is_none());
    assert!(!state.accept_completion(pending.0, Some(at)));
}

#[test]
fn hover_and_navigation_require_their_own_response_identities() {
    let mut state = AttachmentState::default();
    let at = LspPosition::new(1, 2);
    state.hover_requested(LspRequestId(1), at);
    state.navigation_requested(LspRequestId(2));
    assert_eq!(state.hover_position(LspRequestId(2)), None);
    assert_eq!(state.hover_position(LspRequestId(1)), Some(at));
    assert_eq!(state.hover_request, Some((LspRequestId(1), at)));
    assert!(!state.accept_navigation(LspRequestId(1)));
    assert_eq!(state.navigation_request, Some(LspRequestId(2)));
    assert!(state.accept_navigation(LspRequestId(2)));
    assert!(!state.accept_navigation(LspRequestId(2)));
}
