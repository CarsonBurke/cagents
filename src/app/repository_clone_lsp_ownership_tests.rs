use super::*;

const DISK_TEXT: &str = "baseline buffer\n";

struct CloneLspFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl CloneLspFixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("main.rs"), DISK_TEXT).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"clone-lsp-fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for arguments in [
            vec!["init", "-q", "--template="],
            vec!["add", "main.rs", "Cargo.toml"],
        ] {
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::process::Command::new("/usr/bin/git")
                    .env_clear()
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .current_dir(&root)
                    .args(["-c", "core.hooksPath=/dev/null"])
                    .args(arguments)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("private Git fixture finishes")
            .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        Self {
            _directory: directory,
            root,
        }
    }

    async fn app(&self, manager: LspManager, next_document: u64) -> (Cagents, CardId) {
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.settings.editor_auto_save = false;
        app.settings.language_servers = manager.servers().to_vec();
        app.lsp = manager;
        app.next_lsp_document = next_document;
        app.active_workspace = Some(self.root.clone());
        let card = app.workspace.focused().unwrap();
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: self.root.join(".git"),
            worktree: self.root.clone(),
            selected_file: None,
        };
        let state = app.card_state_mut(card).unwrap();
        state.kind = CardKind::Repository(view.clone());
        state.residency = Residency::Hot;
        let mut repository = RepositoryRuntime::new(view.clone());
        repository.set_auto_save(false);
        repository.restore_navigation(&crate::model::RepositoryNavigation {
            project: view.project,
            worktree: view.worktree,
            selected_file: Some("main.rs".into()),
            editor: None,
        });
        let load = repository_task(repository.initial_load(card));
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(repository)));
        settle_repository(&mut app, load).await;
        assert_eq!(
            runtime(&app, card).editor_text().as_deref(),
            Some(DISK_TEXT)
        );
        wait_for_language_ready(&mut app, card).await;
        (app, card)
    }
}

fn runtime(app: &Cagents, card: CardId) -> &RepositoryRuntime {
    match &app.cards[&card] {
        CardRuntime::Repository(runtime) => runtime,
        _ => panic!("actual repository editor"),
    }
}

async fn settle_repository(app: &mut Cagents, task: Task<Message>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut pending = std::collections::VecDeque::from([task]);
        let mut count = 0;
        while let Some(task) = pending.pop_front() {
            count += 1;
            assert!(count < 64, "repository callbacks must settle");
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                continue;
            };
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(
                    message @ Message::Repository(_),
                )) = action
                {
                    pending.push_back(app.update(message));
                }
            }
        }
    })
    .await
    .expect("actual repository workers and callbacks finish");
}

async fn wait_for_language_ready(app: &mut Cagents, card: CardId) {
    let events = app.lsp.events();
    tokio::time::timeout(Duration::from_secs(30), async {
        while app.lsp_context(card).unwrap().status != Some(LspStatus::Ready) {
            app.apply_lsp_event(events.recv().await.unwrap());
        }
    })
    .await
    .expect("real manager discovery and in-process server become ready");
}

fn prefix(app: &mut Cagents, card: CardId, text: &str) {
    let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
        panic!("actual editable repository runtime");
    };
    assert!(runtime.move_cursor_to(LspPosition::new(0, 0)));
    drop(
        app.update(Message::Repository(RepositoryAction::EditorAction {
            card,
            action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Paste(
                Arc::new(text.into()),
            )),
        })),
    );
}

/// Public coordinate conversion reads the manager's actual immutable source.
/// Distinct astral prefixes make a wrong buffer observable without adding a
/// test-only snapshot accessor or trusting App's stored refusal flag.
fn assert_manager_source(app: &Cagents, doc: LspDocumentId, text: &str) {
    let line = text.strip_suffix('\n').unwrap();
    let mut utf16 = 0;
    let mut scalar = 0;
    for character in line.chars() {
        assert_eq!(
            app.lsp
                .scalar_position(doc, LspPosition::new(0, utf16))
                .unwrap(),
            LspPosition::new(0, scalar)
        );
        utf16 += character.len_utf16() as u32;
        scalar += 1;
    }
    assert_eq!(
        app.lsp
            .scalar_position(doc, LspPosition::new(0, utf16))
            .unwrap(),
        LspPosition::new(0, scalar)
    );
    assert!(
        app.lsp
            .scalar_position(doc, LspPosition::new(0, utf16 + 1))
            .is_err()
    );
}

fn restart_action(app: &Cagents, card: CardId) -> InspectorAction {
    inspector::with_lsp(
        inspector::InspectorModel {
            card_id: card,
            title: "Independent cloned editor".into(),
            kind: "Repository",
            sections: Vec::new(),
            actions: Vec::new(),
        },
        card,
        app.lsp_context(card).unwrap(),
    )
    .actions
    .into_iter()
    .find(|action| action.message == InspectorAction::RestartLanguageServer(card))
    .expect("the ordinary inspector exposes recovery")
    .message
}

fn assert_blocked(app: &Cagents, card: CardId, owner: CardId, doc: LspDocumentId) {
    assert!(app.lsp_document(card).is_none());
    assert!(runtime(app, card).lsp_document().is_none());
    assert_eq!(app.lsp_document(owner), Some(doc));
    assert_eq!(runtime(app, owner).lsp_document(), Some(doc));
    assert_eq!(app.lsp_documents.get(&doc), Some(&owner));
    assert!(matches!(
        &app.lsp_context(card).unwrap().status,
        Some(LspStatus::Unavailable { reason }) if reason.contains("another pane")
    ));
    let _ = restart_action(app, card);
}

#[tokio::test]
async fn dirty_repository_clone_cannot_replace_original_language_source_on_load_edit_or_retry() {
    let fixture = CloneLspFixture::new().await;
    // This existing fixture supplies a finite in-process protocol transport;
    // the manager still performs real registration, discovery and versioning.
    let (manager, holder) = crate::lsp::tests::progress_fixture().await;
    let (mut app, source) = fixture.app(manager, holder.doc.0 + 1).await;
    let original_runtime = runtime(&app, source) as *const RepositoryRuntime;
    prefix(&mut app, source, "🌲🌲");
    let dirty = format!("🌲🌲{DISK_TEXT}");
    let doc = app.lsp_document(source).unwrap();
    let version = app.lsp.document_version(doc).unwrap();
    let next_document = app.next_lsp_document;
    assert_eq!(
        runtime(&app, source).editor_text().as_deref(),
        Some(dirty.as_str())
    );
    assert!(runtime(&app, source).is_dirty());
    assert_manager_source(&app, doc, &dirty);

    let task = app.clone_focused_pane();
    let clone = app.workspace.focused().unwrap();
    assert_ne!(clone, source);
    settle_repository(&mut app, task).await;
    assert_eq!(
        runtime(&app, clone).editor_text().as_deref(),
        Some(DISK_TEXT)
    );
    assert!(!runtime(&app, clone).is_dirty());
    assert_blocked(&app, clone, source, doc);
    assert_eq!(app.next_lsp_document, next_document);
    assert_eq!(app.lsp.document_version(doc), Some(version));
    assert_manager_source(&app, doc, &dirty);

    prefix(&mut app, clone, "🦀");
    let edited_clone = format!("🦀{DISK_TEXT}");
    for _ in 0..2 {
        let action = restart_action(&app, clone);
        drop(app.update(Message::ContextInspector(action)));
        assert_blocked(&app, clone, source, doc);
        assert_eq!(app.next_lsp_document, next_document);
        assert_eq!(app.lsp.document_version(doc), Some(version));
        assert_manager_source(&app, doc, &dirty);
    }
    app.apply_lsp_event(LspEvent::Resources { doc });
    assert_blocked(&app, clone, source, doc);
    assert_eq!(
        runtime(&app, source).editor_text().as_deref(),
        Some(dirty.as_str())
    );
    assert_eq!(
        runtime(&app, clone).editor_text().as_deref(),
        Some(edited_clone.as_str())
    );
    assert_eq!(
        runtime(&app, source) as *const RepositoryRuntime,
        original_runtime
    );
    assert!(runtime(&app, source).is_dirty() && runtime(&app, clone).is_dirty());
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("main.rs")).unwrap(),
        DISK_TEXT
    );
    app.lsp.begin_shutdown().await;
}

#[tokio::test]
async fn clean_clones_diverge_without_sharing_language_edits_and_retry_after_owner_release() {
    let fixture = CloneLspFixture::new().await;
    let (manager, holder) = crate::lsp::tests::progress_fixture().await;
    let (mut app, source) = fixture.app(manager, holder.doc.0 + 1).await;
    let doc = app.lsp_document(source).unwrap();
    let initial_version = app.lsp.document_version(doc).unwrap();
    let task = app.clone_focused_pane();
    let clone = app.workspace.focused().unwrap();
    settle_repository(&mut app, task).await;
    assert_blocked(&app, clone, source, doc);
    assert_eq!(app.lsp.document_version(doc), Some(initial_version));
    assert_manager_source(&app, doc, DISK_TEXT);

    prefix(&mut app, source, "🌲");
    let source_text = format!("🌲{DISK_TEXT}");
    let source_version = app.lsp.document_version(doc).unwrap();
    assert!(source_version > initial_version);
    assert_manager_source(&app, doc, &source_text);
    prefix(&mut app, clone, "🦀🦀");
    let clone_text = format!("🦀🦀{DISK_TEXT}");
    assert_eq!(app.lsp.document_version(doc), Some(source_version));
    assert_manager_source(&app, doc, &source_text);
    assert_eq!(
        runtime(&app, source).editor_text().as_deref(),
        Some(source_text.as_str())
    );
    assert_eq!(
        runtime(&app, clone).editor_text().as_deref(),
        Some(clone_text.as_str())
    );

    // This is the actual App release hook used when a buffer closes or
    // navigates away. Releasing LSP ownership need not discard its local text.
    app.close_lsp_document(source);
    assert!(app.lsp.document_version(doc).is_none());
    assert!(!app.lsp_documents.contains_key(&doc));
    let action = restart_action(&app, clone);
    drop(app.update(Message::ContextInspector(action)));
    let admitted = app
        .lsp_document(clone)
        .expect("ordinary Restart admits the current buffer");
    assert_ne!(admitted, doc);
    assert_eq!(runtime(&app, clone).lsp_document(), Some(admitted));
    assert_eq!(app.lsp_documents.get(&admitted), Some(&clone));
    wait_for_language_ready(&mut app, clone).await;
    assert_manager_source(&app, admitted, &clone_text);
    let admitted_version = app.lsp.document_version(admitted).unwrap();
    prefix(&mut app, clone, "🌳");
    let latest = format!("🌳{clone_text}");
    assert!(app.lsp.document_version(admitted).unwrap() > admitted_version);
    assert_manager_source(&app, admitted, &latest);
    assert_eq!(
        runtime(&app, source).editor_text().as_deref(),
        Some(source_text.as_str())
    );

    let next_document = app.next_lsp_document;
    let task = app.clone_focused_pane();
    let third = app.workspace.focused().unwrap();
    assert_ne!(third, clone);
    settle_repository(&mut app, task).await;
    assert_blocked(&app, third, clone, admitted);
    assert_eq!(app.next_lsp_document, next_document);
    assert_eq!(
        runtime(&app, third).editor_text().as_deref(),
        Some(DISK_TEXT)
    );
    assert_eq!(
        runtime(&app, clone).editor_text().as_deref(),
        Some(latest.as_str())
    );
    assert_manager_source(&app, admitted, &latest);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("main.rs")).unwrap(),
        DISK_TEXT
    );
    app.lsp.begin_shutdown().await;
}
