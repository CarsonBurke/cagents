//! Explicit editor operations. Responses are version fenced before preview;
//! publication freezes every participating buffer before writing unopened files.
use super::*;
use crate::{editor_workspace_edit as workspace, lsp::*};

#[derive(Debug, Clone)]
pub enum Action {
    Rename(CardId),
    RenameDraft(CardId, String),
    SubmitRename(CardId, String),
    CancelRename(CardId),
    Format { card: CardId, selection: bool, save_after: bool },
    CodeActions(CardId),
    ChooseCodeAction(CardId, usize),
    Signature(CardId, Option<String>),
    Dismiss(CardId),
    ResolvedTargets { ticket: u64, result: Result<Arc<workspace::ResolvedTargets>, String> },
    Prepared { ticket: u64, result: Result<Arc<workspace::Plan>, String> },
    SelectPreviewFile(usize),
    AnnotationsPage(usize),
    PreviewDiff { ticket: u64, index: usize, result: Result<Option<Arc<crate::diff_view::DiffModel>>, String> },
    PreviewRepository { ticket: u64, action: RepositoryAction },
    ApplyPreview,
    CancelPreview,
    ExpireServerNotice { id: u64, ticket: u64 },
    Published { ticket: u64, result: Result<workspace::PublicationReport, String> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    fn app() -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.settings.language_servers.clear();
        app.lsp = LspManager::new(Vec::new());
        app
    }

    fn buffer(app: &mut Cagents, root: &Path, name: &str, text: &str) -> CardId {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, name, text);
        runtime.descriptor.worktree = root.into();
        runtime.descriptor.repository = root.into();
        app.cards.insert(card, CardRuntime::Repository(Box::new(runtime)));
        card
    }

    fn operation(app: &Cagents, card: CardId) -> Operation {
        Operation { card, doc: LspDocumentId(44), version: 1, kind: LspEditKind::Rename, save_after: false, command: None, server_request: None, annotations: Vec::new(), open_files: app.editor_open_files() }
    }

    fn plan(app: &Cagents, root: &Path, names: &[&str]) -> Arc<workspace::Plan> {
        let open = app.cards.values().filter_map(|runtime| match runtime {
            CardRuntime::Repository(runtime) => runtime.workspace_edit_snapshot(), _ => None,
        }).collect();
        let documents = names.iter().map(|name| workspace::DocumentEdits {
            path: (*name).into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar,
            edits: vec![LspTextEdit { range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)), new_text: "renamed".into() }],
        }).collect();
        Arc::new(workspace::prepare(root, documents, &open).unwrap())
    }

    async fn finish(app: &mut Cagents, task: Task<Message>) {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::EditorFeatures(action))) = action {
                    drop(app.update_editor_features(action));
                }
            }
        }
    }

    fn text(app: &Cagents, card: CardId) -> String {
        match &app.cards[&card] { CardRuntime::Repository(runtime) => runtime.editor_text().unwrap(), _ => unreachable!() }
    }

    #[tokio::test]
    async fn three_file_publication_updates_unopened_disk_and_retains_independent_buffer_undo() {
        let root = tempfile::tempdir().unwrap();
        let original = "fn old() {}\n";
        for name in ["a.rs", "b.rs", "c.rs"] { std::fs::write(root.path().join(name), original).unwrap(); }
        let mut app = app();
        let a = buffer(&mut app, root.path(), "a.rs", original);
        let b = buffer(&mut app, root.path(), "b.rs", original);
        let plan = plan(&app, root.path(), &["a.rs", "b.rs", "c.rs"]);
        let operation = operation(&app, a);
        let task = app.publish_editor_workspace_edit(10, operation, plan);
        assert!(app.editor_features.modal_open());
        for card in [a, b] {
            if let CardRuntime::Repository(runtime) = app.cards.get_mut(&card).unwrap() {
                assert!(runtime.workspace_edit_pending());
                drop(runtime.update(RepositoryAction::EditorAction { card, action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert('x')) }));
            }
        }
        finish(&mut app, task).await;
        assert!(!app.editor_features.modal_open());
        assert_eq!(text(&app, a), "fn renamed() {}\n");
        assert_eq!(text(&app, b), "fn renamed() {}\n");
        assert_eq!(std::fs::read_to_string(root.path().join("a.rs")).unwrap(), original);
        assert_eq!(std::fs::read_to_string(root.path().join("b.rs")).unwrap(), original);
        assert_eq!(std::fs::read_to_string(root.path().join("c.rs")).unwrap(), "fn renamed() {}\n");
        if let CardRuntime::Repository(runtime) = app.cards.get_mut(&a).unwrap() { drop(runtime.update(RepositoryAction::Undo { card: a })); }
        assert_eq!(text(&app, a), original);
        assert_eq!(text(&app, b), "fn renamed() {}\n");
        if let CardRuntime::Repository(runtime) = app.cards.get_mut(&b).unwrap() { drop(runtime.update(RepositoryAction::Undo { card: b })); }
        assert_eq!(text(&app, b), original);
    }

    #[tokio::test]
    async fn replacing_preview_root_preserves_open_buffers_and_releases_their_freeze() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("worktree");
        let moved = parent.path().join("original");
        std::fs::create_dir(&root).unwrap();
        let original = "fn old() {}\n";
        for name in ["a.rs", "b.rs"] { std::fs::write(root.join(name), original).unwrap(); }
        let mut app = app();
        let card = buffer(&mut app, &root, "a.rs", original);
        let plan = plan(&app, &root, &["a.rs", "b.rs"]);
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        for name in ["a.rs", "b.rs"] { std::fs::write(root.join(name), original).unwrap(); }
        let operation = operation(&app, card);
        let task = app.publish_editor_workspace_edit(20, operation, plan);
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if runtime.workspace_edit_pending()));
        finish(&mut app, task).await;
        assert_eq!(text(&app, card), original);
        for tree in [&root, &moved] {
            for name in ["a.rs", "b.rs"] { assert_eq!(std::fs::read_to_string(tree.join(name)).unwrap(), original); }
        }
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if !runtime.workspace_edit_pending()));
        assert!(!app.editor_features.publishing());
    }

    #[test]
    fn changed_protocol_version_refuses_publication_before_buffer_or_disk_mutation() {
        let root = tempfile::tempdir().unwrap();
        let original = "fn old() {}\n";
        for name in ["a.rs", "b.rs"] { std::fs::write(root.path().join(name), original).unwrap(); }
        let mut app = app();
        let card = buffer(&mut app, root.path(), "a.rs", original);
        let mut plan = plan(&app, root.path(), &["a.rs", "b.rs"]);
        // The preview observed a manager-owned URI version which has since
        // retired. Its local editor version/text are still unchanged.
        Arc::make_mut(&mut plan).documents.iter_mut().find(|document| document.open).unwrap().protocol_version = Some(7);
        let operation = operation(&app, card);
        drop(app.publish_editor_workspace_edit(19, operation, plan));
        assert_eq!(text(&app, card), original);
        assert_eq!(std::fs::read_to_string(root.path().join("b.rs")).unwrap(), original);
        assert!(!app.editor_features.publishing());
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if !runtime.workspace_edit_pending()));
    }

    #[tokio::test]
    async fn failed_unopened_compare_and_swap_preserves_buffers_and_releases_every_freeze() {
        let root = tempfile::tempdir().unwrap();
        let original = "fn old() {}\n";
        for name in ["a.rs", "b.rs", "c.rs"] { std::fs::write(root.path().join(name), original).unwrap(); }
        let mut app = app();
        let a = buffer(&mut app, root.path(), "a.rs", original);
        let plan = plan(&app, root.path(), &["a.rs", "b.rs", "c.rs"]);
        std::fs::write(root.path().join("c.rs"), "outside edit").unwrap();
        let operation = operation(&app, a);
        let task = app.publish_editor_workspace_edit(11, operation, plan);
        finish(&mut app, task).await;
        assert_eq!(text(&app, a), original);
        assert_eq!(std::fs::read_to_string(root.path().join("b.rs")).unwrap(), original);
        assert_eq!(std::fs::read_to_string(root.path().join("c.rs")).unwrap(), "outside edit");
        assert!(!app.editor_features.modal_open());
        assert!(matches!(&app.cards[&a], CardRuntime::Repository(runtime) if !runtime.workspace_edit_pending()));
    }

    #[test]
    fn server_errors_publish_immediately_and_info_expiry_does_not_dismiss_errors_or_newer_repeats() {
        let mut app = app();
        let card = CardId::new();
        let doc = LspDocumentId(1);
        app.lsp_documents.insert(doc, card);
        drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Error, message: "server failed".into() })));
        assert!(app.notices.recent().any(|entry| entry.notice.message == "server failed"));
        drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Info, message: "indexing complete".into() })));
        let (id, previous) = app.editor_features.notice_expiry.iter().map(|(id, ticket)| (*id, *ticket)).next().unwrap();
        drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Info, message: "indexing complete".into() })));
        let current = app.editor_features.notice_expiry[&id];
        drop(app.update_editor_features(Action::ExpireServerNotice { id, ticket: previous }));
        assert!(app.notices.recent().any(|entry| entry.notice.message == "indexing complete"));
        drop(app.update_editor_features(Action::ExpireServerNotice { id, ticket: current }));
        assert!(!app.notices.recent().any(|entry| entry.notice.message == "indexing complete"));
        assert!(app.notices.recent().any(|entry| entry.notice.message == "server failed"));
    }

    #[test]
    fn server_info_flood_cannot_evict_the_unresolved_server_error() {
        let mut app = app();
        let card = CardId::new();
        let doc = LspDocumentId(1);
        app.lsp_documents.insert(doc, card);
        drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Error, message: "server unavailable".into() })));
        for index in 0..NOTICE_LOG_CAPACITY * 2 {
            drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Info, message: format!("Indexed batch {index}") })));
        }
        assert!(app.notices.len() <= NOTICE_LOG_CAPACITY);
        assert!(app.notices.recent().any(|entry| entry.notice.message == "server unavailable"));
    }

    #[test]
    fn a_full_persistent_error_log_may_refuse_transient_info_without_panicking() {
        let mut app = app();
        let card = CardId::new();
        let doc = LspDocumentId(1);
        app.lsp_documents.insert(doc, card);
        for index in 0..NOTICE_LOG_CAPACITY {
            drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Error, message: format!("Persistent error {index}") })));
        }
        drop(app.update(Message::Lsp(LspEvent::ServerMessage { doc, level: LspMessageLevel::Info, message: "Background indexing done".into() })));
        assert_eq!(app.notices.len(), NOTICE_LOG_CAPACITY);
        assert!(app.notices.recent().all(|entry| entry.notice.severity == NoticeSeverity::Error));
        assert!(app.editor_features.notice_expiry.is_empty());
    }

    #[test]
    fn code_action_command_failures_reach_the_notice_and_success_retires_the_request() {
        let mut app = app();
        let card = CardId::new();
        let doc = LspDocumentId(44);
        let request = LspRequestId(71);
        let operation = operation(&app, card);
        app.editor_features.requests.insert(request, operation.clone());
        assert!(app.editor_feature_event(&LspEvent::RequestFailed { request, doc, error: LspError::Unsupported("Quick fix command failed".into()) }).is_some());
        assert!(app.editor_features.requests.is_empty());
        assert_eq!(app.status.as_ref().unwrap().severity, NoticeSeverity::Error);
        assert!(app.status.as_ref().unwrap().message.contains("Quick fix command failed"));
        app.editor_features.requests.insert(request, operation);
        drop(app.editor_feature_event(&LspEvent::CommandExecuted { request, doc }));
        assert!(app.editor_features.requests.is_empty());
    }

    #[test]
    fn retired_manager_response_cannot_consume_a_new_documents_reused_request_id() {
        let mut app = app();
        let request = LspRequestId(7);
        let current = Operation { card: CardId::new(), doc: LspDocumentId(2), version: 1, kind: LspEditKind::Rename, save_after: false, command: None, server_request: None, annotations: Vec::new(), open_files: Vec::new() };
        app.editor_features.requests.insert(request, current);
        drop(app.editor_feature_event(&LspEvent::WorkspaceEdit { request, doc: LspDocumentId(1), kind: LspEditKind::Rename, edit: LspWorkspaceEdit::default() }));
        assert_eq!(app.editor_features.requests.get(&request).unwrap().doc, LspDocumentId(2));
    }

    #[tokio::test]
    async fn signature_response_after_caret_movement_cannot_reopen_the_old_call() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("call.rs");
        std::fs::write(&path, "call(").unwrap();
        let mut app = app();
        app.lsp = LspManager::new(AppSettings::default().language_servers);
        let card = buffer(&mut app, root.path(), "call.rs", "call(");
        let doc = LspDocumentId(44);
        app.lsp_documents.insert(doc, card);
        app.lsp_contexts.entry(card).or_default().document = Some(doc);
        let language = app.lsp.language_for(&path).unwrap();
        app.lsp.open(doc, &path, root.path(), language, 1, "call(");
        if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) {
            drop(runtime.take_lsp_intents());
            runtime.set_lsp_document(Some(doc));
            runtime.move_cursor_to(LspPosition::new(0, 5));
        }
        drop(app.update_editor_features(Action::Signature(card, None)));
        let (request, _) = app.editor_features.signature_requests[&card];
        if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) { runtime.move_cursor_to(LspPosition::new(0, 0)); }
        drop(app.editor_feature_event(&LspEvent::SignatureHelp { request, doc, help: Some(LspSignatureHelp { signatures: Vec::new(), active_signature: 0, active_parameter: None }) }));
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if !runtime.editor_signature_visible()));
        assert!(!app.editor_features.signature_requests.contains_key(&card));
    }

    #[tokio::test]
    async fn application_change_drain_keeps_signature_until_close_character_or_undo() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("call.rs");
        std::fs::write(&path, "call(").unwrap();
        let mut app = app();
        app.lsp = LspManager::new(AppSettings::default().language_servers);
        let card = buffer(&mut app, root.path(), "call.rs", "call(");
        let doc = LspDocumentId(44);
        app.lsp_documents.insert(doc, card);
        app.lsp_contexts.entry(card).or_default().document = Some(doc);
        let language = app.lsp.language_for(&path).unwrap();
        app.lsp.open(doc, &path, root.path(), language, 1, "call(");
        let help = LspSignatureHelp { signatures: Vec::new(), active_signature: 0, active_parameter: None };
        if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) {
            drop(runtime.take_lsp_intents());
            runtime.set_lsp_document(Some(doc));
            runtime.move_cursor_to(LspPosition::new(0, 5));
            runtime.set_editor_signature(Some(help.clone()));
        }
        drop(app.update(Message::Repository(RepositoryAction::EditorAction { card, action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert('1')) })));
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if runtime.editor_signature_visible()));
        drop(app.update(Message::Repository(RepositoryAction::EditorAction { card, action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert(')')) })));
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if !runtime.editor_signature_visible()));
        if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) { runtime.set_editor_signature(Some(help)); }
        drop(app.update(Message::Repository(RepositoryAction::Undo { card })));
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if !runtime.editor_signature_visible()));
    }

    #[tokio::test]
    async fn server_edit_anchored_to_read_only_source_can_change_a_writable_worktree_target() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let original = "fn old() {}\n";
        for name in ["a.rs", "b.rs"] { std::fs::write(root.path().join(name), original).unwrap(); }
        let external_path = external.path().join("stdlib.rs");
        std::fs::write(&external_path, original).unwrap();
        let mut app = app();
        app.lsp = LspManager::new(AppSettings::default().language_servers);
        let a = buffer(&mut app, root.path(), "a.rs", original);
        let read_only = buffer(&mut app, root.path(), external_path.to_str().unwrap(), original);
        if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&read_only) { runtime.set_external_source(external_path.clone()); }
        let b = buffer(&mut app, root.path(), "b.rs", original);
        // Registration stays pending in this controller fixture: discovery
        // events are not consumed, so no external server process can launch.
        for (card, doc, path) in [(a, LspDocumentId(1), root.path().join("a.rs")), (read_only, LspDocumentId(2), external_path.clone()), (b, LspDocumentId(3), root.path().join("b.rs"))] {
            app.lsp_documents.insert(doc, card);
            app.lsp_contexts.entry(card).or_default().document = Some(doc);
            if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) {
                drop(runtime.take_lsp_intents());
                drop(runtime.take_lsp_edits());
                runtime.set_lsp_document(Some(doc));
            }
            let language = app.lsp.language_for(&path).unwrap();
            app.lsp.open(doc, &path, root.path(), language, 1, original);
        }
        app.close_lsp_document(a);
        app.cards.remove(&a);
        let task = app.editor_feature_event(&LspEvent::ApplyWorkspaceEdit { doc: LspDocumentId(2), id: LspServerEditId(700), label: Some("Import quick fix".into()), edit: LspWorkspaceEdit { documents: vec![LspDocumentEdits { path: root.path().join("b.rs"), version: Some(1), observed_version: Some(1), coordinates: LspCoordinates::Scalar, edits: vec![LspTextEdit { range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)), new_text: "renamed".into() }] }], ..Default::default() } }).unwrap();
        let mut tasks = vec![task];
        while let Some(task) = tasks.pop() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::EditorFeatures(action))) = action { tasks.push(app.update_editor_features(action)); }
                }
            }
        }
        assert_eq!(text(&app, b), "fn renamed() {}\n");
        assert_eq!(text(&app, read_only), original);
        assert_eq!(std::fs::read_to_string(external_path).unwrap(), original);
        assert!(!app.editor_features.modal_open());
        if let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&b) { drop(runtime.update(RepositoryAction::Undo { card: b })); }
        assert_eq!(text(&app, b), original);
    }

    #[test]
    fn save_and_close_may_format_only_the_buffer_named_by_its_confirmation() {
        let mut app = app();
        let card = CardId::new();
        app.dirty_repository_close = Some(DirtyRepositoryClose { card, remaining: VecDeque::new(), intent: DirtyCloseIntent::Card(card), focused: DirtyCloseControl::Save, restore_card: Some(card) });
        assert!(app.modal_is_open());
        assert!(!app.editor_feature_action_blocked(&Action::Format { card, selection: false, save_after: true }));
        assert!(app.editor_feature_action_blocked(&Action::Format { card: CardId::new(), selection: false, save_after: true }));
        assert!(app.editor_feature_action_blocked(&Action::Format { card, selection: false, save_after: false }));
        assert!(app.editor_feature_action_blocked(&Action::Rename(card)));
    }

    #[test]
    fn window_close_waits_for_unopened_only_publication_without_exiting_mid_write() {
        let mut app = app();
        let card = CardId::new();
        let operation = operation(&app, card);
        app.editor_features.publication = Some(Publication { ticket: 8, operation, cards: Vec::new() });
        assert!(app.window_close_transient_blocker().is_some());
        let window = window::Id::unique();
        drop(app.update(Message::WindowCloseRequested(window)));
        assert_eq!(app.transient_work_close, Some(window));
        assert!(app.exit.is_none());
        drop(app.update_editor_features(Action::Published { ticket: 8, result: Ok(workspace::PublicationReport::default()) }));
        assert!(!app.editor_features.publishing());
        assert!(app.exit.is_none());
    }

    #[test]
    fn a_selected_target_still_loading_is_not_treated_as_an_unopened_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn old() {}\n").unwrap();
        let mut app = app();
        let card = buffer(&mut app, root.path(), "a.rs", "fn old() {}\n");
        if let CardRuntime::Repository(runtime) = app.cards.get_mut(&card).unwrap() { drop(runtime.update(RepositoryAction::Reload { card })); }
        let source = root.path().join("a.rs");
        assert!(app.editor_open_files().iter().any(|(id, path)| *id == card && path == &source));
        assert!(app.capture_workspace_edit_targets(&[source]).is_err());
    }

    #[test]
    fn opening_another_target_after_preview_refuses_before_any_buffer_or_disk_write() {
        let root = tempfile::tempdir().unwrap();
        let original = "fn old() {}\n";
        for name in ["a.rs", "b.rs"] { std::fs::write(root.path().join(name), original).unwrap(); }
        let mut app = app();
        let a = buffer(&mut app, root.path(), "a.rs", original);
        let operation = operation(&app, a);
        let plan = plan(&app, root.path(), &["a.rs", "b.rs"]);
        let b = buffer(&mut app, root.path(), "b.rs", original);
        drop(app.publish_editor_workspace_edit(12, operation, plan));
        assert_eq!(text(&app, a), original);
        assert_eq!(text(&app, b), original);
        assert!(app.editor_features.publication.is_none());
    }
}

#[derive(Clone)]
struct Operation {
    card: CardId,
    doc: LspDocumentId,
    version: i32,
    kind: LspEditKind,
    save_after: bool,
    command: Option<LspCommand>,
    server_request: Option<LspServerEditId>,
    annotations: Vec<LspChangeAnnotation>,
    open_files: Vec<(CardId, PathBuf)>,
}

struct Rename {
    doc: LspDocumentId,
    version: i32,
    at: LspPosition,
    draft: String,
}

struct Preview {
    ticket: u64,
    diff_ticket: u64,
    operation: Operation,
    plan: Arc<workspace::Plan>,
    selected: usize,
    annotation_page: usize,
    card: CardId,
    runtime: Option<RepositoryRuntime>,
    cancellation: crate::repository::CancellationToken,
    error: Option<String>,
}

struct Publication {
    ticket: u64,
    operation: Operation,
    cards: Vec<CardId>,
}

#[derive(Default)]
pub(super) struct State {
    next_ticket: u64,
    notice_expiry: HashMap<u64, u64>,
    requests: HashMap<LspRequestId, Operation>,
    preparing: HashMap<u64, Operation>,
    renames: HashMap<CardId, Rename>,
    code_actions: HashMap<CardId, (i32, Vec<LspCodeAction>)>,
    signatures: HashSet<CardId>,
    signature_requests: HashMap<CardId, (LspRequestId, LspPosition)>,
    preview: Option<Preview>,
    publication: Option<Publication>,
}

impl State {
    pub fn modal_open(&self) -> bool { self.preview.is_some() || self.publication.is_some() }
    pub fn publishing(&self) -> bool { self.publication.is_some() }
    fn next(&mut self) -> u64 { self.next_ticket = self.next_ticket.wrapping_add(1).max(1); self.next_ticket }
}

impl Cagents {
    pub(super) fn open_lsp_source_tab(&mut self, mut view: RepositoryView, path: PathBuf, at: LspPosition, coordinates: LspCoordinates, external: bool) -> Task<Message> {
        if crate::device::is_remote() { return Task::none(); }
        let existing = self.cards.iter().find_map(|(card, runtime)| match runtime {
            CardRuntime::Repository(runtime) if runtime.descriptor.worktree == view.worktree
                && runtime.is_read_only_source() == external
                && runtime.source_tab_path().as_ref() == Some(&path) => Some(*card),
            _ => None,
        });
        if let Some(card) = existing {
            let update = self.workspace.focus(card).unwrap_or_else(|_| ViewportUpdate::unchanged());
            let focus = self.apply_viewport(update, true, false);
            if self.lsp_document(card).is_none() {
                self.lsp_pending_jumps.insert(card, PendingLspJump { path, at, coordinates });
                return focus;
            }
            return Task::batch([focus, self.jump_to_lsp_location(card, path, at, coordinates)]);
        }
        view.selected_file = if external { None } else { path.strip_prefix(&view.worktree).ok().map(Path::to_path_buf) };
        if !external && view.selected_file.is_none() { return Task::none(); }
        let card = CardId::new();
        let title = path.file_name().and_then(|name| normalize_card_name(&name.to_string_lossy())).unwrap_or_else(|| "Source".into());
        let navigation = crate::model::RepositoryNavigation { project: view.project, worktree: view.worktree.clone(), selected_file: view.selected_file.clone(), external_source: external.then(|| path.clone()), editor: None };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(CardState { id: card, generation: durable.generation, title: title.clone(), width_fraction: self.settings.default_native_pane_fraction,
            existence: Existence::Open, exposure: Exposure::Visible, residency: Residency::Hot, kind: CardKind::Repository(view.clone()) });
        durable.repository_navigation.insert(card, navigation);
        durable.focused = Some(card);
        self.register_channel_card(card);
        if external { self.retain_local_pane(card); }
        self.lsp_pending_jumps.insert(card, PendingLspJump { path, at, coordinates });
        let load = self.attach_repository_pane(card, view);
        let index = self.workspace.cards().len();
        let update = self.workspace.add_card(WorkspaceCard::new(card, title, CardWidth::Fraction(self.settings.default_native_pane_fraction)).expect("source tab has a valid width"), index, true)
            .unwrap_or_else(|error| { self.status = Some(TransientNotice::error(error.to_string())); ViewportUpdate::unchanged() });
        self.persist();
        Task::batch([load, self.apply_viewport(update, true, false)])
    }

    pub(super) fn format_on_save_enabled(&self, card: CardId) -> bool {
        self.lsp_document(card).is_some_and(|doc| self.lsp.formatting_settings(doc).is_some_and(|(enabled, _)| enabled) && self.lsp.capabilities(doc).format_document)
    }

    pub(super) fn maybe_format_before_save(&mut self, card: CardId) -> Option<Task<Message>> {
        if !self.format_on_save_enabled(card) { return None; }
        if self.editor_features.requests.values().any(|operation| operation.card == card && operation.save_after)
            || self.editor_features.preparing.values().any(|operation| operation.card == card && operation.save_after) { return Some(Task::none()); }
        Some(self.update_editor_features(Action::Format { card, selection: false, save_after: true }))
    }

    pub(super) fn editor_signature_character(&mut self, card: CardId, character: char) -> Task<Message> {
        if character == ')' {
            return self.update_editor_features(Action::Dismiss(card));
        }
        let Some(doc) = self.lsp_document(card) else { return Task::none(); };
        let visible = matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.editor_signature_visible());
        if self.lsp.signature_trigger_with_context(doc, character, visible) {
            self.update_editor_features(Action::Signature(card, Some(character.to_string())))
        } else { Task::none() }
    }

    pub(super) fn editor_feature_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if !is_escape_press(event) { return None; }
        if self.editor_features.modal_open() { return Some(self.update_editor_features(Action::CancelPreview)); }
        let card = self.workspace.focused()?;
        self.editor_features.renames.contains_key(&card).then(|| self.update_editor_features(Action::CancelRename(card)))
    }

    pub(super) fn retire_editor_features(&mut self, card: CardId) {
        self.editor_features.renames.remove(&card);
        self.editor_features.code_actions.remove(&card);
        self.editor_features.signatures.remove(&card);
        if let Some((request, _)) = self.editor_features.signature_requests.remove(&card) { self.lsp.cancel(request); }
        let retired = self.editor_features.requests.iter().filter_map(|(request, operation)| (operation.card == card).then_some(*request)).collect::<Vec<_>>();
        for request in retired { self.editor_features.requests.remove(&request); self.lsp.cancel(request); }
    }

    fn take_editor_feature_request(&mut self, request: &LspRequestId, doc: &LspDocumentId) -> Option<Operation> {
        if self.editor_features.requests.get(request)?.doc != *doc { return None; }
        self.editor_features.requests.remove(request)
    }

    fn execute_editor_feature_command(&mut self, operation: &Operation, command: LspCommand) {
        let request = self.lsp.execute_command(operation.doc, command);
        let mut completion = operation.clone();
        // An executeCommand result has no edit coordinates. Its failure still
        // belongs to this invocation even if workspace/applyEdit advanced the
        // buffer version before the command acknowledged completion.
        completion.command = None;
        completion.server_request = None;
        completion.save_after = false;
        self.editor_features.requests.insert(request, completion);
    }

    pub(super) fn editor_feature_event(&mut self, event: &LspEvent) -> Option<Task<Message>> {
        match event {
            LspEvent::PrepareRename { request, doc, range, placeholder } => {
                let Some(operation) = self.take_editor_feature_request(request, doc) else { return Some(Task::none()); };
                if !self.editor_operation_current(&operation) { return Some(Task::none()); }
                let Some(range) = range else { self.feature_failure(operation.card, "This symbol cannot be renamed"); return Some(Task::none()); };
                let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&operation.card) else { return Some(Task::none()); };
                let draft = placeholder.clone().or_else(|| runtime.editor_text_in_range(*range)).unwrap_or_default();
                runtime.set_editor_rename(Some(draft.clone()));
                self.editor_features.renames.insert(operation.card, Rename { doc: *doc, version: operation.version, at: range.start, draft });
                Some(iced::widget::operation::focus(RepositoryRuntime::rename_focus_id(operation.card)).map(cosmic::Action::App))
            }
            LspEvent::WorkspaceEdit { request, doc, edit, .. } => {
                let Some(operation) = self.take_editor_feature_request(request, doc) else { return Some(Task::none()); };
                if !self.editor_operation_current(&operation) { return Some(self.save_after_feature(&operation)); }
                Some(self.prepare_editor_workspace_edit(operation, edit.clone()))
            }
            LspEvent::CodeActions { request, doc, items, .. } => {
                let Some(operation) = self.take_editor_feature_request(request, doc) else { return Some(Task::none()); };
                if !self.editor_operation_current(&operation) || self.editor_features.renames.contains_key(&operation.card) { return Some(Task::none()); }
                self.editor_features.code_actions.insert(operation.card, (operation.version, items.clone()));
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&operation.card) { runtime.set_editor_code_actions(items); }
                if items.is_empty() { self.status = Some(TransientNotice::success("No code actions are available here")); }
                Some(Task::none())
            }
            LspEvent::CodeActionResolved { request, doc, item, .. } => {
                let Some(mut operation) = self.take_editor_feature_request(request, doc) else { return Some(Task::none()); };
                if !self.editor_operation_current(&operation) { return Some(Task::none()); }
                if let Some(reason) = &item.disabled { self.feature_failure(operation.card, reason.clone()); return Some(Task::none()); }
                operation.command = item.command.clone();
                if let Some(edit) = &item.edit { Some(self.prepare_editor_workspace_edit(operation, edit.clone())) }
                else {
                    if let Some(command) = operation.command.clone() { self.execute_editor_feature_command(&operation, command); }
                    else { self.feature_failure(operation.card, "The server returned no edits or command for this code action"); }
                    Some(Task::none())
                }
            }
            LspEvent::SignatureHelp { request, doc, help, .. } => {
                let Some(operation) = self.take_editor_feature_request(request, doc) else { return Some(Task::none()); };
                let expected = self.editor_features.signature_requests.remove(&operation.card);
                if !self.editor_operation_current(&operation) || expected.is_none_or(|(expected, at)| expected != *request || !matches!(self.cards.get(&operation.card), Some(CardRuntime::Repository(runtime)) if runtime.cursor_position() == Some(at))) { return Some(Task::none()); }
                if help.is_some() { self.editor_features.signatures.insert(operation.card); } else { self.editor_features.signatures.remove(&operation.card); }
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&operation.card) { runtime.set_editor_signature(help.clone()); }
                Some(Task::none())
            }
            LspEvent::ServerMessage { doc, level, message } => {
                let Some(card) = self.lsp_documents.get(doc).copied() else { return Some(Task::none()); };
                match level {
                    LspMessageLevel::Log => tracing::info!(target: "cagents::lsp::server", ?doc, message),
                    LspMessageLevel::Error => self.feature_failure(card, message.clone()),
                    LspMessageLevel::Warning => self.status = Some(TransientNotice::warning(message.clone())),
                    LspMessageLevel::Info => {
                        let notice = TransientNotice::success(message.clone());
                        self.notices.record(notice.clone());
                        // A log filled with persistent failures may choose to
                        // discard this lower-severity transient immediately.
                        let Some(id) = self.notices.recent().find(|entry| entry.notice == notice).map(|entry| entry.id) else { return Some(Task::none()); };
                        self.editor_features.notice_expiry.retain(|id, _| self.notices.entries.iter().any(|entry| entry.id == *id));
                        let ticket = self.editor_features.next();
                        self.editor_features.notice_expiry.insert(id, ticket);
                        return Some(cosmic::task::future(async move {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            Message::EditorFeatures(Action::ExpireServerNotice { id, ticket })
                        }));
                    },
                }
                Some(Task::none())
            }
            LspEvent::ApplyWorkspaceEdit { doc, id, edit, .. } => {
                let operation = self.server_workspace_edit_operation(*doc);
                if let Some(mut operation) = operation {
                    operation.server_request = Some(*id);
                    Some(self.prepare_editor_workspace_edit(operation, edit.clone()))
                } else { self.lsp.respond_workspace_edit(*id, false, Some("The source document was closed".into())); Some(Task::none()) }
            }
            LspEvent::CommandExecuted { request, doc } => {
                self.take_editor_feature_request(request, doc);
                Some(Task::none())
            },
            LspEvent::RequestFailed { request, doc, error, .. } => {
                let operation = self.take_editor_feature_request(request, doc)?;
                if self.editor_features.signature_requests.get(&operation.card).is_some_and(|(expected, _)| expected == request) { self.editor_features.signature_requests.remove(&operation.card); }
                if !matches!(error, LspError::Cancelled) { self.feature_failure(operation.card, error.to_string()); }
                Some(self.save_after_feature(&operation))
            }
            _ => None,
        }
    }

    fn server_workspace_edit_operation(&self, doc: LspDocumentId) -> Option<Operation> {
        let card = *self.lsp_documents.get(&doc)?;
        if self.lsp_document(card) != Some(doc) || !matches!(self.cards.get(&card), Some(CardRuntime::Repository(_))) { return None; }
        // A server chooses a session document as its root/context anchor. It
        // can be a read-only std tab; writability is checked independently for
        // every actual target by the workspace planner and publication fence.
        Some(Operation { card, doc, version: self.lsp.document_version(doc)?, kind: LspEditKind::CodeAction,
            save_after: false, command: None, server_request: None, annotations: Vec::new(), open_files: Vec::new() })
    }

    fn editor_operation(&self, card: CardId, kind: LspEditKind) -> Option<Operation> {
        if matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.is_read_only_source()) { return None; }
        let doc = self.lsp_document(card)?;
        Some(Operation { card, doc, version: self.lsp.document_version(doc)?, kind, save_after: false, command: None, server_request: None, annotations: Vec::new(), open_files: Vec::new() })
    }

    fn editor_operation_current(&self, operation: &Operation) -> bool {
        self.lsp_document(operation.card) == Some(operation.doc)
            && self.lsp.document_version(operation.doc) == Some(operation.version)
    }

    fn feature_failure(&mut self, card: CardId, message: impl Into<String>) {
        let message = message.into();
        self.status = Some(TransientNotice::error(message.clone()));
        let state = self.lsp_contexts.entry(card).or_default();
        state.failure = Some(message);
        state.refresh_notice();
    }

    fn save_after_feature(&mut self, operation: &Operation) -> Task<Message> {
        if !operation.save_after || self.lsp_document(operation.card) != Some(operation.doc) { return Task::none(); }
        match self.cards.get_mut(&operation.card) {
            Some(CardRuntime::Repository(runtime)) => repository_task(runtime.request_save(operation.card, false)),
            _ => Task::none(),
        }
    }

    fn editor_feature_action_blocked(&self, action: &Action) -> bool {
        let user_action = matches!(action,
            Action::Rename(_) | Action::RenameDraft(..) | Action::SubmitRename(..) | Action::Format { .. }
            | Action::CodeActions(_) | Action::ChooseCodeAction(..) | Action::Signature(..));
        if !user_action { return false; }
        if crate::device::is_remote() { return true; }
        // Save-and-close is an explicit answer to the dirty-buffer dialog.
        // Its format phase must not be refused by that same dialog's hold.
        let close_save = matches!(action, Action::Format { card, save_after: true, .. }
            if self.dirty_repository_close.as_ref().is_some_and(|close| close.card == *card));
        self.modal_is_open() && !close_save
    }

    pub(super) fn update_editor_features(&mut self, action: Action) -> Task<Message> {
        if self.editor_feature_action_blocked(&action) { return Task::none(); }
        match action {
            Action::Rename(card) => {
                let Some(operation) = self.editor_operation(card, LspEditKind::Rename) else { return Task::none(); };
                if !self.lsp.capabilities(operation.doc).rename {
                    self.feature_failure(card, "This language server does not support rename");
                    return Task::none();
                }
                let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else { return Task::none(); };
                let Some(at) = runtime.cursor_position() else { return Task::none(); };
                if runtime.is_saving() { return Task::none(); }
                let request = self.lsp.request_prepare_rename(operation.doc, at);
                self.editor_features.requests.insert(request, operation);
                Task::none()
            }
            Action::RenameDraft(card, draft) => {
                if let Some(rename) = self.editor_features.renames.get_mut(&card) {
                    rename.draft = draft.clone();
                    if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) { runtime.set_editor_rename(Some(draft)); }
                }
                Task::none()
            }
            Action::SubmitRename(card, submitted) => {
                let Some(mut rename) = self.editor_features.renames.remove(&card) else { return Task::none(); };
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) { runtime.set_editor_rename(None); }
                rename.draft = submitted;
                if rename.draft.trim().is_empty() || rename.draft.contains(['\r', '\n', '\0']) {
                    self.feature_failure(card, "Enter a nonempty symbol name");
                    return Task::none();
                }
                let Some(operation) = self.editor_operation(card, LspEditKind::Rename) else { return Task::none(); };
                if operation.doc != rename.doc || operation.version != rename.version {
                    self.feature_failure(card, "The buffer changed; start rename again at the current symbol");
                    return Task::none();
                }
                let request = self.lsp.request_rename(rename.doc, rename.at, rename.draft);
                self.editor_features.requests.insert(request, operation);
                Task::none()
            }
            Action::CancelRename(card) => {
                self.editor_features.renames.remove(&card);
                match self.cards.get_mut(&card) {
                    Some(CardRuntime::Repository(runtime)) => { runtime.set_editor_rename(None); repository_task(runtime.focus_editor(card)) }
                    _ => Task::none(),
                }
            }
            Action::Format { card, selection, save_after } => {
                let Some(mut operation) = self.editor_operation(card, if selection { LspEditKind::RangeFormatting } else { LspEditKind::Formatting }) else { return Task::none(); };
                operation.save_after = save_after;
                let capabilities = self.lsp.capabilities(operation.doc);
                if !(if selection { capabilities.format_selection } else { capabilities.format_document }) {
                    if save_after { return self.save_after_feature(&operation); }
                    self.feature_failure(card, "This language server does not support this formatting operation");
                    return Task::none();
                }
                let timeout = self.lsp.formatting_settings(operation.doc).map_or(Duration::from_millis(1000), |(_, timeout)| timeout);
                let options = match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) => { let (tab_size, insert_spaces) = runtime.editor_formatting_options(); LspFormattingOptions { tab_size, insert_spaces } },
                    _ => LspFormattingOptions::default(),
                };
                let request = if selection {
                    let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else { return Task::none(); };
                    let Some(range) = runtime.editor_selection_range() else {
                        self.feature_failure(card, "Select the source to format first");
                        return Task::none();
                    };
                    self.lsp.request_range_formatting(operation.doc, range, options, timeout)
                } else { self.lsp.request_formatting(operation.doc, options, timeout) };
                self.editor_features.requests.insert(request, operation);
                Task::none()
            }
            Action::CodeActions(card) => {
                let Some(operation) = self.editor_operation(card, LspEditKind::CodeAction) else { return Task::none(); };
                if !self.lsp.capabilities(operation.doc).code_actions { self.feature_failure(card, "This language server does not provide code actions"); return Task::none(); }
                let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else { return Task::none(); };
                let Some(at) = runtime.cursor_position() else { return Task::none(); };
                let range = runtime.editor_selection_range().unwrap_or(LspRange::new(at, at));
                let request = self.lsp.request_code_actions(operation.doc, range, runtime.editor_diagnostics());
                self.editor_features.requests.insert(request, operation);
                Task::none()
            }
            Action::ChooseCodeAction(card, index) => {
                let Some((version, items)) = self.editor_features.code_actions.remove(&card) else { return Task::none(); };
                let Some(item) = items.get(index).cloned() else { return Task::none(); };
                let Some(mut operation) = self.editor_operation(card, LspEditKind::CodeAction) else { return Task::none(); };
                if operation.version != version || item.disabled.is_some() { self.feature_failure(card, "This code action is no longer available"); return Task::none(); }
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) { runtime.set_editor_code_actions(&[]); }
                if item.needs_resolve && self.lsp.capabilities(operation.doc).code_action_resolve {
                    let request = self.lsp.request_resolve_code_action(operation.doc, item);
                    self.editor_features.requests.insert(request, operation);
                    return Task::none();
                }
                operation.command = item.command;
                if let Some(edit) = item.edit { self.prepare_editor_workspace_edit(operation, edit) }
                else if let Some(command) = operation.command.clone() { self.execute_editor_feature_command(&operation, command); Task::none() }
                else { self.feature_failure(card, "The server returned no edits or command for this code action"); Task::none() }
            }
            Action::Signature(card, trigger) => {
                let Some(doc) = self.lsp_document(card) else { return Task::none(); };
                let Some(version) = self.lsp.document_version(doc) else { return Task::none(); };
                let operation = Operation { card, doc, version, kind: LspEditKind::CodeAction, save_after: false, command: None, server_request: None, annotations: Vec::new(), open_files: Vec::new() };
                let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else { return Task::none(); };
                let Some(at) = runtime.cursor_position() else { return Task::none(); };
                let active = runtime.editor_signature_help().cloned();
                if let Some((previous, _)) = self.editor_features.signature_requests.remove(&card) {
                    self.editor_features.requests.remove(&previous);
                    self.lsp.cancel(previous);
                }
                let request = self.lsp.request_signature_help_retrigger(operation.doc, at, trigger, active.is_some(), active);
                self.editor_features.signature_requests.insert(card, (request, at));
                self.editor_features.requests.insert(request, operation);
                Task::none()
            }
            Action::Dismiss(card) => {
                self.editor_features.signatures.remove(&card);
                self.editor_features.code_actions.remove(&card);
                if let Some((previous, _)) = self.editor_features.signature_requests.remove(&card) { self.editor_features.requests.remove(&previous); self.lsp.cancel(previous); }
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) { runtime.dismiss_editor_results(); }
                Task::none()
            }
            Action::ResolvedTargets { ticket, result } => {
                let Some(operation) = self.editor_features.preparing.get(&ticket).cloned() else { return Task::none(); };
                let resolved = match result {
                    Ok(resolved) if operation.open_files == self.editor_open_files() && self.editor_operation_current(&operation) => Arc::unwrap_or_clone(resolved),
                    Ok(_) => { return self.update_editor_features(Action::Prepared { ticket, result: Err("The open buffers changed before preparation completed".into()) }); }
                    Err(error) => { return self.update_editor_features(Action::Prepared { ticket, result: Err(error) }); }
                };
                let open = match self.capture_workspace_edit_targets(&resolved.open_paths) {
                    Ok(open) => open,
                    Err(error) => return self.update_editor_features(Action::Prepared { ticket, result: Err(error) }),
                };
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || workspace::prepare_resolved(resolved, &open).map(Arc::new)).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::EditorFeatures(Action::Prepared { ticket, result })
                })
            }
            Action::Prepared { ticket, result } => {
                let Some(operation) = self.editor_features.preparing.remove(&ticket) else { return Task::none(); };
                if !self.editor_operation_current(&operation) {
                    self.reject_server_edit(&operation, "The source buffer changed before the edit was prepared");
                    return self.save_after_feature(&operation);
                }
                match result {
                    Err(error) => { self.reject_server_edit(&operation, &error); self.feature_failure(operation.card, error); self.save_after_feature(&operation) }
                    Ok(plan) => {
                        if plan.documents.iter().all(|document| document.before == document.after) {
                            if let Some(id) = operation.server_request { self.lsp.respond_workspace_edit(id, true, None); }
                            if let Some(command) = operation.command.clone() { self.execute_editor_feature_command(&operation, command); }
                            if !operation.save_after { self.status = Some(TransientNotice::success("No source changes were needed")); }
                            return self.save_after_feature(&operation);
                        }
                        let preview = operation.kind == LspEditKind::Rename || plan.documents.len() > 1;
                        if preview {
                            if self.editor_features.modal_open() { self.reject_server_edit(&operation, "Another workspace edit is already open"); return Task::none(); }
                            let diff_ticket = self.editor_features.next();
                            self.editor_features.preview = Some(Preview { ticket, diff_ticket, operation, plan, selected: 0, annotation_page: 0, card: CardId::new(), runtime: None, cancellation: crate::repository::CancellationToken::new(), error: None });
                            self.update_editor_features(Action::SelectPreviewFile(0))
                        } else { self.publish_editor_workspace_edit(ticket, operation, plan) }
                    }
                }
            }
            Action::AnnotationsPage(page) => {
                if let Some(preview) = self.editor_features.preview.as_mut() {
                    preview.annotation_page = page.min(preview.operation.annotations.len().saturating_sub(1) / 8);
                }
                Task::none()
            }
            Action::SelectPreviewFile(index) => {
                let diff_ticket = self.editor_features.next();
                let Some(preview) = self.editor_features.preview.as_mut() else { return Task::none(); };
                let Some(document) = preview.plan.documents.get(index) else { return Task::none(); };
                preview.selected = index;
                preview.runtime = None;
                preview.error = None;
                preview.cancellation.cancel();
                preview.cancellation = crate::repository::CancellationToken::new();
                let cancellation = preview.cancellation.clone();
                preview.diff_ticket = diff_ticket;
                let ticket = preview.diff_ticket;
                let document = document.clone();
                let theme = self.settings.editor_syntax_theme;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || crate::diff_view::build_document_diff(document.before, &document.after, &document.relative, theme, &cancellation).map(Arc::new)).await.map_err(|error| error.to_string());
                    Message::EditorFeatures(Action::PreviewDiff { ticket, index, result })
                })
            }
            Action::PreviewDiff { ticket, index, result } => {
                let Some(preview) = self.editor_features.preview.as_mut().filter(|preview| preview.diff_ticket == ticket && preview.selected == index) else { return Task::none(); };
                match result {
                    Ok(Some(model)) => {
                        let Some(CardRuntime::Repository(owner)) = self.cards.get(&preview.operation.card) else { return Task::none(); };
                        let mut runtime = RepositoryRuntime::new(owner.descriptor.clone());
                        runtime.set_workspace_preview(preview.plan.documents[index].relative.clone(), Arc::unwrap_or_clone(model));
                        preview.runtime = Some(runtime);
                    }
                    Ok(None) => preview.error = Some("The comparison was canceled or could not be rendered".into()),
                    Err(error) => preview.error = Some(error),
                }
                Task::none()
            }
            Action::PreviewRepository { ticket, action } => {
                let Some(preview) = self.editor_features.preview.as_mut().filter(|preview| preview.diff_ticket == ticket) else { return Task::none(); };
                match preview.runtime.as_mut() {
                    Some(runtime) => runtime.update(action).map(move |action| Message::EditorFeatures(Action::PreviewRepository { ticket, action })).map(cosmic::Action::App),
                    None => Task::none(),
                }
            }
            Action::ApplyPreview => {
                let Some(preview) = self.editor_features.preview.take() else { return Task::none(); };
                preview.cancellation.cancel();
                self.publish_editor_workspace_edit(preview.ticket, preview.operation, preview.plan)
            }
            Action::CancelPreview => {
                if let Some(preview) = self.editor_features.preview.take() {
                    preview.cancellation.cancel();
                    self.reject_server_edit(&preview.operation, "The user canceled the preview");
                }
                Task::none()
            }
            Action::ExpireServerNotice { id, ticket } => {
                if self.editor_features.notice_expiry.get(&id) == Some(&ticket) {
                    self.editor_features.notice_expiry.remove(&id);
                    self.notices.dismiss(id);
                }
                Task::none()
            }
            Action::Published { ticket, result } => {
                let Some(publication) = self.editor_features.publication.take() else { return Task::none(); };
                if publication.ticket != ticket { self.editor_features.publication = Some(publication); return Task::none(); }
                let report = match result {
                    Err(error) => {
                        for card in publication.cards { if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) { runtime.cancel_workspace_edit(ticket); } }
                        self.reject_server_edit(&publication.operation, &error);
                        self.feature_failure(publication.operation.card, error);
                        return self.save_after_feature(&publication.operation);
                    }
                    Ok(report) => report,
                };
                if !report.durability_warnings.is_empty() { self.feature_failure(publication.operation.card, report.durability_warnings.join("\n")); }
                let mut tasks = Vec::new();
                let mut failure = None;
                for card in publication.cards {
                    if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                        if let Err(error) = runtime.finish_workspace_edit(ticket) { failure = Some(error); }
                        tasks.push(self.drain_lsp_intents(card));
                    } else { failure = Some("An edited buffer was closed during publication".into()); }
                }
                if let Some(error) = failure {
                    self.reject_server_edit(&publication.operation, &error);
                    self.feature_failure(publication.operation.card, error);
                } else {
                    if let Some(id) = publication.operation.server_request { self.lsp.respond_workspace_edit(id, true, None); }
                    if let Some(command) = publication.operation.command.clone() { self.execute_editor_feature_command(&publication.operation, command); }
                    tasks.push(self.save_after_feature(&publication.operation));
                }
                Task::batch(tasks)
            }
        }
    }

    fn reject_server_edit(&mut self, operation: &Operation, reason: &str) {
        if let Some(id) = operation.server_request { self.lsp.respond_workspace_edit(id, false, Some(reason.into())); }
    }

    fn prepare_editor_workspace_edit(&mut self, mut operation: Operation, edit: LspWorkspaceEdit) -> Task<Message> {
        if !self.editor_features.preparing.is_empty() || self.editor_features.modal_open() {
            self.reject_server_edit(&operation, "Finish the current workspace edit first");
            self.feature_failure(operation.card, "Finish the current workspace edit first");
            return Task::none();
        }
        if edit.requires_confirmation { operation.kind = LspEditKind::Rename; }
        operation.annotations = edit.annotations;
        let Some(CardRuntime::Repository(owner)) = self.cards.get(&operation.card) else { return Task::none(); };
        let worktree = owner.descriptor.worktree.clone();
        operation.open_files = self.editor_open_files();
        let open_paths = operation.open_files.iter().map(|(_, path)| path.clone()).collect();
        let documents = edit.documents.into_iter().map(|document| workspace::DocumentEdits { path: document.path, version: document.version, observed_version: document.observed_version, coordinates: document.coordinates, edits: document.edits }).collect();
        let ticket = self.editor_features.next();
        self.editor_features.preparing.insert(ticket, operation);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || workspace::resolve_targets(worktree, documents, open_paths).map(Arc::new)).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::EditorFeatures(Action::ResolvedTargets { ticket, result })
        })
    }

    fn capture_workspace_edit_targets(&self, paths: &[PathBuf]) -> Result<HashMap<PathBuf, workspace::OpenSnapshot>, String> {
        let mut open: HashMap<PathBuf, workspace::OpenSnapshot> = HashMap::new();
        let mut captured = 0usize;
        for (card, runtime) in &self.cards {
            if let CardRuntime::Repository(runtime) = runtime {
                if runtime.workspace_edit_candidate_source().is_some_and(|path| paths.contains(&path)) {
                    captured = captured.saturating_add(runtime.workspace_edit_snapshot_size());
                    if captured > 64 * 1024 * 1024 { return Err("The open buffers exceed the 64 MiB preview budget".into()); }
                    let Some((path, mut snapshot)) = runtime.workspace_edit_snapshot() else { return Err("Wait for the selected target to finish loading, or close its read-only view, before applying edits".into()); };
                    snapshot.protocol_version = self.lsp_document(*card).and_then(|doc| self.lsp.document_version(doc));
                        if let Some(previous) = open.insert(path, snapshot.clone()) {
                            if previous.text != snapshot.text || previous.version != snapshot.version || previous.protocol_version != snapshot.protocol_version || previous.writable != snapshot.writable {
                                return Err("Two open views of the same file disagree".into());
                            }
                        }
                }
            }
        }
        Ok(open)
    }

    fn editor_open_files(&self) -> Vec<(CardId, PathBuf)> {
        let mut files = self.cards.iter().filter_map(|(card, runtime)| {
            if let CardRuntime::Repository(runtime) = runtime { Some((*card, runtime.workspace_edit_candidate_source()?)) } else { None }
        }).collect::<Vec<_>>();
        files.sort_by_key(|(card, _)| card.to_string());
        files
    }

    fn publish_editor_workspace_edit(&mut self, ticket: u64, operation: Operation, plan: Arc<workspace::Plan>) -> Task<Message> {
        if self.editor_features.publication.is_some() { self.reject_server_edit(&operation, "Another edit is being applied"); return Task::none(); }
        if operation.open_files != self.editor_open_files() {
            self.reject_server_edit(&operation, "The open file tabs changed; prepare the edit again");
            self.feature_failure(operation.card, "The open file tabs changed; prepare the edit again");
            return Task::none();
        }
        let protocol_versions: HashMap<CardId, Option<i32>> = self.cards.keys().map(|card| (*card, self.lsp_document(*card).and_then(|doc| self.lsp.document_version(doc)))).collect();
        let mut matched_documents = HashSet::new();
        let mut cards = Vec::new();
        let mut failure = None;
        for (card, runtime) in &mut self.cards {
            if let CardRuntime::Repository(runtime) = runtime {
                if let Some(path) = runtime.workspace_edit_candidate_source() {
                    // The planner records canonical target names. Matching the
                    // selected source uses the original path captured below;
                    // descriptor-relative aliases are reconciled by the worker.
                    let document = plan.documents.iter().enumerate().find(|(_, document)| document.open_paths.contains(&path) || document.path == path);
                    if let Some((index, document)) = document {
                        matched_documents.insert(index);
                        if !document.open { failure = Some("A target file was opened after this preview was prepared".into()); break; }
                        if protocol_versions[card] != document.protocol_version { failure = Some("A target language-server version changed after this preview was prepared".into()); break; }
                        match runtime.begin_workspace_edit(ticket, document) { Ok(()) => cards.push(*card), Err(error) => { failure = Some(error); break; } }
                    }
                }
            }
        }
        if plan.documents.iter().enumerate().any(|(index, document)| document.open && !matched_documents.contains(&index)) { failure = Some("An edited buffer was closed after the preview was prepared".into()); }
        if let Some(error) = failure {
            for card in cards { if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) { runtime.cancel_workspace_edit(ticket); } }
            self.reject_server_edit(&operation, &error);
            self.feature_failure(operation.card, error);
            return Task::none();
        }
        self.editor_features.publication = Some(Publication { ticket, operation, cards });
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || workspace::publish_unopened(&plan)).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::EditorFeatures(Action::Published { ticket, result })
        })
    }

    pub(super) fn editor_feature_dialog(&self) -> Option<Element<'_, Message>> {
        if self.editor_features.publication.is_some() {
            return Some(dialog_ui::overlay(dialog_ui::surface(design::body("Applying workspace edit…"), 480.0), None, 1.0));
        }
        let preview = self.editor_features.preview.as_ref()?;
        let files = widget::column::with_children(preview.plan.documents.iter().enumerate().map(|(index, document)| {
            crate::ui_controls::button::secondary(format!("{}{}", document.relative.display(), if document.open { " · open buffer" } else { " · file on disk" }))
                .on_press(Message::EditorFeatures(Action::SelectPreviewFile(index))).into()
        }).collect::<Vec<_>>()).spacing(space::XS);
        let diff = match &preview.runtime {
            Some(runtime) => repository_widget::workspace_preview_view(preview.card, runtime).map(move |action| Message::EditorFeatures(Action::PreviewRepository { ticket: preview.diff_ticket, action })),
            None => design::body(preview.error.as_deref().unwrap_or("Preparing comparison…")).into(),
        };
        let mut rows = vec![
            design::title(format!("Preview changes · {} files", preview.plan.documents.len())).into(),
            design::caption("Open buffers retain one Undo step each. Unopened files are written after conflict checks.").into(),
        ];
        let annotations = &preview.operation.annotations;
        if !annotations.is_empty() {
            let start = preview.annotation_page * 8;
            let end = (start + 8).min(annotations.len());
            let metadata = widget::column::with_children(annotations[start..end].iter().map(|annotation| {
                let mut fields = vec![design::body(format!("{}{}", if annotation.needs_confirmation { "Confirmation required · " } else { "" }, annotation.label)).into()];
                if let Some(description) = &annotation.description { fields.push(design::caption(description).into()); }
                widget::column::with_children(fields).spacing(space::XS).into()
            }).collect::<Vec<_>>()).spacing(space::S);
            rows.push(widget::row::with_children(vec![
                design::caption(format!("Server annotations · {}–{} of {}", start + 1, end, annotations.len())).width(Length::Fill).into(),
                crate::ui_controls::button::secondary("Previous annotations").on_press_maybe((start > 0).then_some(Message::EditorFeatures(Action::AnnotationsPage(preview.annotation_page.saturating_sub(1))))).into(),
                crate::ui_controls::button::secondary("Next annotations").on_press_maybe((end < annotations.len()).then_some(Message::EditorFeatures(Action::AnnotationsPage(preview.annotation_page + 1)))).into(),
            ]).spacing(space::S).into());
            rows.push(widget::scrollable(metadata).height(120).into());
        }
        rows.push(widget::row::with_children(vec![widget::scrollable(files).width(260).height(Length::Fill).into(), widget::container(diff).width(Length::Fill).height(Length::Fill).into()]).spacing(space::M).height(if annotations.is_empty() { 480 } else { 360 }).into());
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel").on_press(Message::EditorFeatures(Action::CancelPreview)),
            crate::ui_controls::button::primary("Apply changes").on_press(Message::EditorFeatures(Action::ApplyPreview)),
        ));
        let surface = widget::column::with_children(rows).spacing(space::M);
        Some(dialog_ui::overlay(dialog_ui::surface(surface, 1100.0), Some(Message::EditorFeatures(Action::CancelPreview)), 1.0))
    }
}
