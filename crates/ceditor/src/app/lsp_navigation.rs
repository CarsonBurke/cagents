use super::*;
use cagents::lsp::{LspCoordinates, LspLocation, LspRequestId, LspTarget, LocationKind};
use cagents::repository_widget::EditorLspAction;
use std::collections::HashMap;

const RESULT_LIMIT: usize = 512;
const LABEL_CHAR_LIMIT: usize = 256;

// The typed target remains intact for activation. Only bounded prefixes and
// suffixes enter native text layout, even for a very large non-file URI.
fn location_label_path(text: &str) -> String {
    let mut prefix = text.chars().take(LABEL_CHAR_LIMIT + 1).collect::<Vec<_>>();
    if prefix.len() <= LABEL_CHAR_LIMIT {
        return prefix.into_iter().map(|ch| if ch.is_control() { ' ' } else { ch }).collect();
    }
    prefix.truncate(LABEL_CHAR_LIMIT / 2);
    let suffix = text.chars().rev().take(LABEL_CHAR_LIMIT / 2).collect::<Vec<_>>();
    prefix.into_iter().chain(std::iter::once('…')).chain(suffix.into_iter().rev())
        .map(|ch| if ch.is_control() { ' ' } else { ch }).collect()
}

#[derive(Debug, Clone)]
pub enum Action {
    Select { card: CardId, request: LspRequestId, index: usize },
    Dismiss(CardId),
    Resolved { card: CardId, request: LspRequestId, result: Result<Resolved, String> },
}

#[derive(Debug, Clone)]
pub struct Resolved {
    path: PathBuf,
    external: bool,
    at: LspPosition,
    coordinates: LspCoordinates,
}

#[derive(Clone)]
pub(super) struct Origin {
    doc: LspDocumentId,
    request: LspRequestId,
    version: i32,
    at: LspPosition,
    epoch: u64,
}

pub(super) struct Results { origin: Origin, kind: LocationKind, items: Vec<LspLocation>, withheld: usize }
struct PendingJump { path: PathBuf, at: LspPosition, coordinates: LspCoordinates, epoch: u64 }

#[derive(Default)]
pub(super) struct State {
    pub(super) requests: HashMap<CardId, Origin>,
    pub(super) results: HashMap<CardId, Results>,
    jumps: HashMap<CardId, PendingJump>,
}

impl State {
    pub(super) fn remove_tab(&mut self, card: CardId) {
        self.requests.remove(&card); self.results.remove(&card); self.jumps.remove(&card);
    }
}

impl Editor {
    fn navigation_origin_current(&self, card: CardId, origin: &Origin) -> bool {
        self.active == Some(card) && self.selection_epoch == origin.epoch
            && self.tab(card).is_some_and(|tab| tab.runtime.lsp_document() == Some(origin.doc)
                && tab.runtime.cursor_position() == Some(origin.at))
            && self.lsp.document_version(origin.doc) == Some(origin.version)
    }

    pub(super) fn request_navigation(&mut self, card: CardId, action: EditorLspAction) -> Task<Message> {
        if self.modal_open() || self.active != Some(card) { return Task::none(); }
        let Some(tab) = self.tab(card) else { return Task::none(); };
        let (Some(doc), Some(at)) = (tab.runtime.lsp_document(), tab.runtime.cursor_position()) else { return Task::none(); };
        let (kind, at) = match action {
            EditorLspAction::Definition(position) => (LocationKind::Definition, position.unwrap_or(at)),
            EditorLspAction::References => (LocationKind::References, at),
            _ => return Task::none(),
        };
        let Some(version) = self.lsp.document_version(doc) else { return Task::none(); };
        if let Some(old) = self.lsp_navigation.requests.remove(&card) { self.lsp.cancel(old.request); }
        self.lsp_navigation.results.remove(&card);
        let request = match kind {
            LocationKind::Definition => self.lsp.request_definition(doc, at),
            LocationKind::References => self.lsp.request_references(doc, at),
        };
        // Click-definition may explicitly name another position without moving
        // the native caret. Fence delivery to the actual initiating caret.
        let caret = self.tab(card).and_then(|tab| tab.runtime.cursor_position()).unwrap();
        self.lsp_navigation.requests.insert(card, Origin { doc, request, version, at: caret, epoch: self.selection_epoch });
        Task::none()
    }

    pub(super) fn navigation_event(&mut self, event: &LspEvent) -> Option<Task<Message>> {
        let LspEvent::Locations { doc, request, kind, items } = event else {
            if let LspEvent::RequestFailed { request, .. } = event {
                self.lsp_navigation.requests.retain(|_, origin| origin.request != *request);
            }
            return None;
        };
        let card = self.lsp_navigation.requests.iter().find_map(|(card, origin)|
            (origin.doc == *doc && origin.request == *request).then_some(*card))?;
        let origin = self.lsp_navigation.requests.remove(&card)?;
        if !self.navigation_origin_current(card, &origin) { return Some(Task::none()); }
        let withheld = items.len().saturating_sub(RESULT_LIMIT);
        let items = items.iter().take(RESULT_LIMIT).cloned().collect::<Vec<_>>();
        let single = *kind == LocationKind::Definition && items.len() == 1 && withheld == 0;
        self.lsp_navigation.results.insert(card, Results { origin, kind: *kind, items, withheld });
        if single { Some(self.update_navigation(Action::Select { card, request: *request, index: 0 })) }
        else { Some(Task::none()) }
    }

    pub(super) fn update_navigation(&mut self, action: Action) -> Task<Message> {
        if self.modal_open() || self.exiting { return Task::none(); }
        match action {
            Action::Dismiss(card) => {
                if let Some(origin) = self.lsp_navigation.requests.remove(&card) { self.lsp.cancel(origin.request); }
                self.lsp_navigation.results.remove(&card);
                Task::none()
            }
            Action::Select { card, request, index } => {
                let Some(results) = self.lsp_navigation.results.get(&card) else { return Task::none(); };
                if results.origin.request != request || !self.navigation_origin_current(card, &results.origin) { return Task::none(); }
                let Some(item) = results.items.get(index).cloned() else { return Task::none(); };
                let Some(tab) = self.tab(card) else { return Task::none(); };
                let root = tab.runtime.descriptor.worktree.clone();
                let (path, force_external) = match item.target {
                    LspTarget::Workspace { path } => (if path.is_absolute() { path } else { root.join(path) }, false),
                    LspTarget::External { path } => (path, true),
                    LspTarget::Uri { .. } => { self.notice = Some("This location is not a local file and cannot be opened as source.".into()); return Task::none(); }
                };
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let path = path.canonicalize().map_err(|error| format!("Cannot open source: {error}"))?;
                        let root = root.canonicalize().map_err(|error| format!("Cannot resolve source workspace: {error}"))?;
                        if !path.is_file() { return Err("The source location is not a regular file.".into()); }
                        let external = force_external || !path.starts_with(&root);
                        Ok(Resolved { path, external, at: item.range.start, coordinates: item.coordinates })
                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::Navigation(Action::Resolved { card, request, result })
                })
            }
            Action::Resolved { card, request, result } => {
                let Some(results) = self.lsp_navigation.results.get(&card) else { return Task::none(); };
                if results.origin.request != request || !self.navigation_origin_current(card, &results.origin) { return Task::none(); }
                let resolved = match result { Ok(resolved) => resolved, Err(error) => { self.notice = Some(error); return Task::none(); } };
                self.open_navigation_target(card, resolved)
            }
        }
    }

    fn open_navigation_target(&mut self, origin: CardId, resolved: Resolved) -> Task<Message> {
        let Some(tab) = self.tab(origin) else { return Task::none(); };
        let root = tab.runtime.descriptor.worktree.clone();
        let existing = self.tabs.iter().find(|tab| tab.runtime.source_tab_path().as_ref() == Some(&resolved.path)
            && tab.runtime.is_read_only_source() == resolved.external).map(|tab| tab.id);
        let mut tasks = Vec::new();
        let target = if let Some(target) = existing { target } else {
            let relative = if resolved.external { None } else { resolved.path.strip_prefix(&root).ok().map(Path::to_path_buf) };
            if !resolved.external && relative.is_none() { return Task::none(); }
            tasks.push(self.add_with_source(Opened { root, file: relative, snapshot: None }, true,
                resolved.external.then(|| resolved.path.clone())));
            let Some(target) = self.active else { return Task::none(); };
            if resolved.external && let Some(tab) = self.tab_mut(target) {
                tab.title = format!("{} · read-only", resolved.path.file_name().unwrap_or_default().to_string_lossy());
            }
            target
        };
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.active = Some(target);
        self.lsp_navigation.jumps.insert(target, PendingJump { path: resolved.path, at: resolved.at, coordinates: resolved.coordinates, epoch: self.selection_epoch });
        tasks.push(self.finish_navigation_jump(target));
        Task::batch(tasks)
    }

    pub(super) fn finish_navigation_jump(&mut self, card: CardId) -> Task<Message> {
        let may_focus = !self.modal_open() && !self.exiting;
        let Some(jump) = self.lsp_navigation.jumps.get(&card) else { return Task::none(); };
        if self.selection_epoch != jump.epoch || self.active != Some(card) {
            self.lsp_navigation.jumps.remove(&card); return Task::none();
        }
        let Some(tab) = self.tab(card) else { self.lsp_navigation.jumps.remove(&card); return Task::none(); };
        if tab.runtime.source_tab_path().as_ref() != Some(&jump.path) { return Task::none(); }
        if tab.runtime.editor_text().is_none() { return Task::none(); }
        let jump = self.lsp_navigation.jumps.remove(&card).unwrap();
        let tab = self.tab_mut(card).unwrap();
        if !tab.runtime.move_cursor_to_coordinates(jump.at, jump.coordinates) {
            self.notice = Some("The source position is no longer valid.".into()); return Task::none();
        }
        if may_focus { tab.runtime.focus_editor(card).map(cosmic::Action::App) } else { Task::none() }
    }

    pub(super) fn navigation_results_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let results = self.lsp_navigation.results.get(&card)?;
        let title = match results.kind { LocationKind::Definition => "Definitions", LocationKind::References => "References" };
        let mut rows = vec![widget::row::with_children(vec![
            design::caption(format!("{title} ({})", results.items.len())).into(),
            widget::button::text("Dismiss").on_press(Message::Navigation(Action::Dismiss(card))).into(),
        ]).spacing(8).into()];
        if results.items.is_empty() { rows.push(design::caption("No locations found").into()); }
        for (index, item) in results.items.iter().enumerate() {
            let (path, local) = match &item.target {
                LspTarget::Workspace { path } | LspTarget::External { path } => (location_label_path(&path.to_string_lossy()), true),
                LspTarget::Uri { uri } => (location_label_path(uri), false),
            };
            let label = format!("{}:{}:{}", path, item.range.start.line.saturating_add(1), item.range.start.character.saturating_add(1));
            rows.push(widget::button::text(label).on_press_maybe(local.then_some(Message::Navigation(Action::Select { card, request: results.origin.request, index }))).into());
        }
        if results.withheld != 0 { rows.push(design::caption(format!("{} additional locations withheld", results.withheld)).into()); }
        Some(widget::scrollable(widget::column::with_children(rows).spacing(4)).height(Length::Fixed(180.0)).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{editor, run_tasks};

    fn open_file(app: &mut Editor, root: &Path, path: &str) -> CardId {
        let task = app.add(Opened { root: root.to_path_buf(), file: Some(path.into()), snapshot: None }, true);
        run_tasks(app, task);
        app.active.unwrap()
    }

    #[test]
    fn location_labels_bound_unicode_layout_without_copying_the_full_target() {
        let text = format!("file:/{}/tail😀.rs", "a😀é\n".repeat(300_000));
        let label = location_label_path(&text);
        assert!(label.chars().count() <= LABEL_CHAR_LIMIT + 1);
        assert!(label.starts_with("file:/"));
        assert!(label.ends_with("/tail😀.rs"));
        assert!(label.contains('…'));
        assert!(!label.contains('\n'));
        assert_eq!(location_label_path("short/é.rs"), "short/é.rs");
        assert_eq!(location_label_path(&"é".repeat(LABEL_CHAR_LIMIT)).chars().count(), LABEL_CHAR_LIMIT);
    }

    #[test]
    fn external_definition_uses_actual_loaded_utf16_and_refuses_edits_without_polluting_session() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let source = external.path().join("stdlib.rs");
        let text = "head\na😀éX\n";
        std::fs::write(&source, text).unwrap();
        let mut app = editor();
        let origin = open_file(&mut app, root.path(), "main.rs");
        let task = app.open_navigation_target(origin, Resolved { path: source.clone(), external: true, at: LspPosition::new(1, 4), coordinates: LspCoordinates::Utf16 });
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        assert_ne!(target, origin);
        let tab = app.tab(target).unwrap();
        assert!(tab.runtime.is_read_only_source());
        assert_eq!(tab.runtime.editor_text().as_deref(), Some(text));
        assert_eq!(tab.runtime.cursor_position(), Some(LspPosition::new(1, 3)));
        assert!(!tab.runtime.can_save());
        let task = app.update(Message::Repository(RepositoryAction::EditorAction { card: target, action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')) }));
        run_tasks(&mut app, task);
        assert_eq!(app.tab(target).unwrap().runtime.editor_text().as_deref(), Some(text));
        assert_eq!(std::fs::read_to_string(source).unwrap(), text);
        assert_eq!(app.session().paths, vec![root.path().join("main.rs")]);
        assert_eq!(app.session().active, None);
        assert_eq!(app.tab(origin).unwrap().runtime.editor_text().as_deref(), Some("fn main() {}\n"));
    }

    #[test]
    fn workspace_definition_opens_another_tab_without_replacing_dirty_origin_and_reuses_target() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "main();\n").unwrap();
        std::fs::write(root.path().join("target.rs"), "a😀éX\n").unwrap();
        let mut app = editor();
        let origin = open_file(&mut app, root.path(), "main.rs");
        app.tab_mut(origin).unwrap().runtime.set_auto_save(false);
        let task = app.update(Message::Repository(RepositoryAction::EditorAction { card: origin, action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')) }));
        run_tasks(&mut app, task);
        let before = app.tab(origin).unwrap().runtime.editor_text().unwrap();
        assert!(app.tab(origin).unwrap().runtime.is_dirty());
        let resolved = Resolved { path: root.path().join("target.rs"), external: false, at: LspPosition::new(0, 4), coordinates: LspCoordinates::Utf16 };
        let task = app.open_navigation_target(origin, resolved.clone());
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        assert_ne!(target, origin);
        assert!(!app.tab(target).unwrap().runtime.is_read_only_source());
        assert_eq!(app.tab(target).unwrap().runtime.cursor_position(), Some(LspPosition::new(0, 3)));
        assert_eq!(app.tab(origin).unwrap().runtime.editor_text().unwrap(), before);
        assert!(app.tab(origin).unwrap().runtime.is_dirty());
        let count = app.tabs.len();
        let task = app.open_navigation_target(origin, resolved);
        run_tasks(&mut app, task);
        assert_eq!(app.active, Some(target));
        assert_eq!(app.tabs.len(), count);
    }

    #[test]
    fn loaded_navigation_applies_position_without_focusing_behind_a_modal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "a😀éX\n").unwrap();
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "main.rs");
        app.lsp_navigation.jumps.insert(card, PendingJump { path: root.path().join("main.rs"), at: LspPosition::new(0, 4), coordinates: LspCoordinates::Utf16, epoch: app.selection_epoch });
        app.settings_open = true;
        let task = app.finish_navigation_jump(card);
        assert!(iced::runtime::task::into_stream(task).is_none());
        assert_eq!(app.tab(card).unwrap().runtime.cursor_position(), Some(LspPosition::new(0, 3)));
        assert!(!app.lsp_navigation.jumps.contains_key(&card));
    }

    #[test]
    fn dismissed_or_unowned_location_callbacks_cannot_open_tabs() {
        let mut app = editor();
        let card = CardId::new();
        let request = LspRequestId(17);
        drop(app.update_navigation(Action::Resolved { card, request, result: Ok(Resolved { path: "/tmp/unrequested.rs".into(), external: true, at: LspPosition::new(0, 0), coordinates: LspCoordinates::Scalar }) }));
        assert!(app.tabs.is_empty());
        let event = LspEvent::Locations { doc: LspDocumentId(3), request, kind: LocationKind::Definition, items: vec![] };
        assert!(app.navigation_event(&event).is_none());
        assert!(app.lsp_navigation.results.is_empty());
    }
}
