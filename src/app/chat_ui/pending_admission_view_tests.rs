//! Real saved-pane widgets only. No provider, file/image load or connection task.
use super::*;
use cosmic::iced::{
    Size,
    advanced::{clipboard, mouse},
    runtime::user_interface::{Cache, UserInterface},
};

fn saved(text: &str) -> Runtime {
    let account = Account::system(Provider::Codex);
    let mut session = fresh_chat_session(
        Provider::Codex,
        TerminalId::new(),
        "/saved-view-fixture".into(),
        &account,
        false,
    )
    .unwrap();
    session.launch.session_id = Some("original-saved-thread".into());
    let mut runtime = Runtime::new(
        Target {
            card: CardId::new(),
            generation: 37,
        },
        session,
        Connection::AwaitingAdmission,
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    runtime.composer = text_editor::Content::with_text(text);
    runtime
}

fn attachment(number: u64, name: &str) -> Attachment {
    Attachment {
        // This nonexistent path is deliberately not an asset or readable file.
        path: format!("/never-open-saved-attachment/{number}.png").into(),
        reference: Some(chat::ImageReference {
            id: uuid::Uuid::new_v4(),
            number,
            device: uuid::Uuid::new_v4(),
            name: name.into(),
            sha256: "a".repeat(64),
            extension: "png".into(),
        }),
    }
}

/// Build/draw the real native widget tree and invoke its actual accessible
/// Copy button. No constructed CopyLiteral callback substitutes for that button.
fn frame(runtime: &Runtime, width: f32, copy: bool) -> (Vec<String>, Vec<Message>) {
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = UserInterface::build(
        runtime.pending_admission_view(),
        Size::new(width, 900.0),
        Cache::default(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        messages.is_empty(),
        "mounting must not connect or load an image"
    );
    ui.draw(
        &mut renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let all = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .collect::<Vec<_>>();
    let texts = all
        .iter()
        .flat_map(|node| [node.node().label(), node.node().value()])
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let buttons = all
        .iter()
        .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .collect::<Vec<_>>();
    assert!(
        buttons
            .iter()
            .all(|node| node.node().label() == Some("Copy saved draft"))
    );
    assert_eq!(
        buttons.len(),
        usize::from(!runtime.unsent_text().is_empty())
    );
    if copy {
        let button = buttons.first().expect("nonempty draft exposes actual Copy");
        assert!(!button.node().is_disabled() && !button.node().is_hidden());
        let bounds = button.node().bounds().expect("mounted Copy geometry");
        assert!(bounds.x0 >= 0.0 && bounds.x1 <= f64::from(width));
        assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
        assert!(bounds.y0 >= 0.0 && bounds.y1 <= 900.0);
        use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, TreeId};
        let target_node = NodeId::from(button.id().clone());
        ui.update(
            &[iced::Event::A11y(
                iced::advanced::widget::Id::from(u128::from(target_node.0) as u64),
                ActionRequest {
                    action: Action::Click,
                    target_tree: TreeId::ROOT,
                    target_node,
                    data: None,
                },
            )],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
    }
    assert!(texts.iter().any(|text| text == "Saved conversation"));
    assert!(texts.iter().any(|text| text == "Not connected"));
    assert!(!texts.iter().any(|text| {
        ["reconnecting", "Connecting…", "Starting", "Loading…"]
            .iter()
            .any(|claim| text.contains(claim))
    }));
    (texts, messages)
}

fn unchanged(runtime: &Runtime, inspect: impl FnOnce()) {
    let source = runtime.session.clone();
    let target = runtime.target;
    let snapshot = Arc::as_ptr(&runtime.snapshot);
    let composer = runtime.composer.text();
    let cursor = runtime.composer.cursor();
    let selection = runtime.composer.selection();
    let draft = runtime.unsent_text();
    let attachments = runtime.unsent_attachments().to_vec();
    let revision = runtime.revision;
    let next_command = runtime.next_command;
    let history_cursor = runtime.history_cursor;
    inspect();
    assert!(runtime.awaiting_admission() && runtime.handle().is_none());
    assert_eq!(runtime.session, source);
    assert_eq!(runtime.target, target);
    assert_eq!(Arc::as_ptr(&runtime.snapshot), snapshot);
    assert_eq!(runtime.composer.text(), composer);
    assert_eq!(runtime.composer.cursor(), cursor);
    assert_eq!(runtime.composer.selection(), selection);
    assert_eq!(runtime.unsent_text(), draft);
    assert_eq!(runtime.unsent_attachments(), attachments);
    assert_eq!(runtime.revision, revision);
    assert_eq!(runtime.next_command, next_command);
    assert_eq!(runtime.history_cursor, history_cursor);
    assert!(runtime.pending.is_none() && runtime.queued.is_none());
    assert!(runtime.attachment_jobs.is_empty());
    assert!(runtime.cache.is_empty() && runtime.child_cache.is_empty());
    assert!(!runtime.preparation.waiting());
}

#[test]
fn empty_saved_conversation_has_no_false_draft_copy_or_progress_at_narrow_and_wide_widths() {
    let runtime = saved("");
    for width in [280.0, 640.0, 1000.0] {
        unchanged(&runtime, || {
            let (texts, messages) = frame(&runtime, width, false);
            assert!(texts.iter().any(|text| text == "No unsent draft."));
            assert!(!texts.iter().any(|text| text == "Unsent draft"));
            assert!(messages.is_empty());
        });
    }
}

#[test]
fn saved_text_copy_comes_from_the_real_button_with_exact_unmodified_bytes() {
    let text = "  Keep 🦀 exactly\r\n[Image 41]\ntrailing space \n";
    let runtime = saved(text);
    // Content owns newline representation; Copy must use precisely its saved
    // unsent bytes, without trimming or reconstructing text from shaped labels.
    let expected = runtime.unsent_text();
    for width in [280.0, 640.0, 1000.0] {
        unchanged(&runtime, || {
            let (texts, messages) = frame(&runtime, width, true);
            assert!(texts.iter().any(|text| text == "Unsent draft"));
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::CopyLiteral(actual))] if actual == &expected)
            );
        });
    }
}

#[test]
fn image_only_saved_draft_keeps_original_labels_receipts_and_filenames_without_loading() {
    let mut runtime = saved("");
    runtime.attachments = vec![
        attachment(41, "First original.png"),
        attachment(7, "Second original.png"),
    ];
    let original = runtime.attachments.clone();
    for width in [280.0, 640.0, 1000.0] {
        unchanged(&runtime, || {
            let (texts, messages) = frame(&runtime, width, false);
            for label in [
                "Unsent draft",
                "Attachments (2)",
                "Image 41 · First original.png",
                "Image 7 · Second original.png",
            ] {
                assert!(texts.iter().any(|text| text == label), "{label}");
            }
            assert!(
                !texts
                    .iter()
                    .any(|text| text == "No unsent draft." || text.contains("never-open"))
            );
            assert!(messages.is_empty());
            assert_eq!(runtime.attachments, original);
        });
    }
}

#[test]
fn browsing_sent_history_displays_and_copies_the_unsent_stash_with_its_own_attachment() {
    let mut runtime = saved("SENT_PROMPT_MUST_NOT_REPLACE_DRAFT");
    runtime.history_cursor = Some(0);
    runtime.history_stash = Some(text_editor::Content::with_text(
        "Original unsent [Image 29]\n",
    ));
    runtime.history_attachment_stash = Some(vec![attachment(29, "Unsaved diagram.png")]);
    runtime.attachments = vec![attachment(3, "SENT_ATTACHMENT_MUST_NOT_APPEAR.png")];
    let expected = runtime.unsent_text();
    let sent = runtime.attachments.clone();
    for width in [280.0, 1000.0] {
        unchanged(&runtime, || {
            let (texts, messages) = frame(&runtime, width, true);
            assert!(
                texts
                    .iter()
                    .any(|text| text == "Image 29 · Unsaved diagram.png")
            );
            assert!(!texts.iter().any(|text| text.contains("SENT_")));
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::CopyLiteral(actual))] if actual == &expected)
            );
            assert_eq!(runtime.attachments, sent);
        });
    }
}
