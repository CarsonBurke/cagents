//! Presence/history acceptance through the actual retained Runtime save task.
use super::*;
use cosmic::iced::futures::StreamExt;

#[tokio::test]
async fn retained_absent_completion_uses_ticket_baseline_after_presence_undo() {
    if retained_save_tests::isolated_test(
        "repository_widget::presence_history_tests::retained_absent_completion_uses_ticket_baseline_after_presence_undo",
    ) {
        return;
    }
    for initial in ["", "prior saved body\n"] {
        for undo_before_completion in [false, true] {
            let card = CardId::new();
            // This fixture reads through read_file_at/FileLoadedAt, retains
            // the actual Root, and configures the window recovery owner.
            let (directory, mut runtime, identity) =
                tests::save_fixture(card, "notes.txt", initial);
            assert!(runtime.original_read_attested);
            let (revision, change) = match &runtime.document {
                DocumentState::Editable(document) => (
                    document.revision,
                    document.content.replacement_to_text("").map(Arc::new),
                ),
                _ => panic!("editable fixture"),
            };
            runtime
                .apply_diff_buffer_revert_with_presence(
                    &identity,
                    revision,
                    change,
                    false,
                    Some(0o100644),
                )
                .unwrap();
            let task = runtime.request_save_inner(card, false, SaveTrigger::Automatic);
            let ticket = match &runtime.document {
                DocumentState::Editable(document) => document.active_save.clone().unwrap(),
                _ => panic!("editable while saving"),
            };
            assert!(!ticket.present && ticket.snapshot.is_empty());
            if !initial.is_empty() {
                let DocumentState::Editable(document) = &runtime.document else {
                    unreachable!()
                };
                assert!(!ticket.snapshot_source.same_text(&document.saved_snapshot));
            }
            if undo_before_completion {
                drop(runtime.update(RepositoryAction::Undo { card }));
                assert!(runtime.desired_file_present());
                assert_eq!(runtime.editor_text().as_deref(), Some(initial));
            }
            let mut stream = iced::runtime::task::into_stream(task).expect("retained save task");
            let mut completions = 0;
            while let Some(effect) = stream.next().await {
                if let iced::runtime::Action::Output(action) = effect {
                    let RepositoryAction::RetainedSaveFinished { result, .. } = &action else {
                        panic!("save must keep its canonical retained completion route")
                    };
                    assert!(matches!(
                        result.as_ref().unwrap().outcome(),
                        SaveOutcome::Absent { .. }
                    ));
                    completions += 1;
                    // Only the owning save task is polled. Git/list refresh
                    // belongs to its separate contract and remains unpolled.
                    drop(runtime.update(action));
                }
            }
            assert_eq!(completions, 1);
            assert!(!directory.path().join("notes.txt").exists());
            let DocumentState::Editable(document) = &mut runtime.document else {
                panic!("absence preserves the open document and Undo")
            };
            assert!(document.active_save.is_none());
            assert!(!document.saved_present);
            assert_eq!(document.saved_presence_id, ticket.presence_id);
            assert_eq!(document.saved_history_revision, ticket.history_revision);
            assert!(document.saved_snapshot.same_text(&ticket.snapshot_source));
            assert!(document.captured_presence_ids.contains(&ticket.presence_id));
            assert!(document.restorations.contains_key(&ticket.presence_id));
            assert_eq!(document.dirty, undo_before_completion);
            if undo_before_completion {
                // For the empty original, text is exactly equal to the
                // absent baseline but the newer present intent stays dirty.
                assert!(document.present);
                assert_ne!(document.history.revision(), ticket.history_revision);
                document.step_history(true).expect("actual presence Redo");
                assert!(!document.present && !document.dirty);
                assert_eq!(document.history.revision(), ticket.history_revision);
            } else {
                assert!(!document.present && !document.dirty);
                document.step_history(false).expect("actual presence Undo");
                assert!(document.present && document.dirty);
                document.step_history(true).expect("actual presence Redo");
                assert!(!document.present && !document.dirty);
            }
        }
    }
}

#[test]
fn equal_empty_text_cannot_clean_a_presence_only_removal_or_changed_lineage() {
    let mut document = EditorDocument::unlinked(
        PathBuf::from("empty.txt"),
        String::new(),
        blake3::hash(b""),
        false,
    );
    assert!(document.present && document.saved_present);
    let saved = document.history.revision();
    let cursor = document.content.cursor();
    document.history.record_group_with_presence_identities(
        vec![],
        cursor,
        cursor,
        Instant::now(),
        Some(((true, document.saved_presence_id), (false, 42))),
    );
    document.present = false;
    document.presence_id = 42;
    document.refresh_dirty();
    let removed = document.history.revision();
    assert!(document.content.is_empty());
    assert!(
        document
            .content
            .text_snapshot()
            .same_text(&document.saved_snapshot)
    );
    assert!(
        document.dirty,
        "an existing empty file is not an absent file"
    );
    assert_ne!(removed, saved, "do not alias the removal to saved bytes");

    document.step_history(false).expect("real presence Undo");
    assert!(document.present);
    assert_eq!(document.presence_id, document.saved_presence_id);
    assert_eq!(document.history.revision(), saved);
    assert!(!document.dirty);
    document.step_history(true).expect("real presence Redo");
    assert!(!document.present);
    assert_eq!(document.presence_id, 42);
    assert_eq!(document.history.revision(), removed);
    assert!(document.dirty);

    // Matching text and existence cannot erase a different restoration
    // lineage, even when a byte-oriented caller adopted the saved revision.
    document.present = true;
    document.presence_id = 43;
    document.history.adopt_revision(saved);
    document.refresh_dirty();
    assert!(document.dirty);
    assert_eq!(document.history.revision(), saved);
}
