use super::*;
use cagents::lsp::{LocationKind, LspCoordinates, LspLocation, LspRequestId, LspTarget};
use cagents::repository_widget::EditorLspAction;
use std::collections::HashMap;

const RESULT_LIMIT: usize = 512;
const LABEL_CHAR_LIMIT: usize = 256;

// The typed target remains intact for activation. Only bounded prefixes and
// suffixes enter native text layout, even for a very large non-file URI.
fn location_label_path(text: &str) -> String {
    let mut prefix = text.chars().take(LABEL_CHAR_LIMIT + 1).collect::<Vec<_>>();
    if prefix.len() <= LABEL_CHAR_LIMIT {
        return prefix
            .into_iter()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
    }
    prefix.truncate(LABEL_CHAR_LIMIT / 2);
    let suffix = text
        .chars()
        .rev()
        .take(LABEL_CHAR_LIMIT / 2)
        .collect::<Vec<_>>();
    prefix
        .into_iter()
        .chain(std::iter::once('…'))
        .chain(suffix.into_iter().rev())
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[derive(Debug, Clone)]
pub enum Action {
    Select {
        card: CardId,
        request: LspRequestId,
        index: usize,
    },
    Dismiss(CardId),
    Resolved {
        card: CardId,
        request: LspRequestId,
        ticket: u64,
        result: Result<Resolved, String>,
    },
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

pub(super) struct Results {
    origin: Origin,
    kind: LocationKind,
    items: Vec<LspLocation>,
    withheld: usize,
}
struct PendingJump {
    path: PathBuf,
    at: LspPosition,
    coordinates: LspCoordinates,
    epoch: u64,
    origin: Option<cagents::editor_locations::Location>,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) requests: HashMap<CardId, Origin>,
    pub(super) results: HashMap<CardId, Results>,
    jumps: HashMap<CardId, PendingJump>,
    pub(super) resolving: HashMap<CardId, (LspRequestId, u64)>,
    next_resolution: u64,
    pub(super) history: cagents::editor_locations::History,
    history_origins: HashMap<CardId, cagents::editor_locations::Location>,
    history_restores: HashMap<
        CardId,
        (
            cagents::editor_locations::Location,
            cagents::editor_locations::Target,
            u64,
        ),
    >,
}

impl State {
    pub(super) fn remove_tab(&mut self, card: CardId) {
        self.requests.remove(&card);
        self.results.remove(&card);
        self.jumps.remove(&card);
        self.resolving.remove(&card);
        self.history_origins.remove(&card);
        self.history_restores.remove(&card);
    }
}

impl Editor {
    fn navigation_origin_current(&self, card: CardId, origin: &Origin) -> bool {
        self.active == Some(card)
            && self.selection_epoch == origin.epoch
            && self.tab(card).is_some_and(|tab| {
                tab.runtime.lsp_document() == Some(origin.doc)
                    && tab.runtime.cursor_position() == Some(origin.at)
            })
            && self.lsp.document_version(origin.doc) == Some(origin.version)
    }

    pub(super) fn request_navigation(
        &mut self,
        card: CardId,
        action: EditorLspAction,
    ) -> Task<Message> {
        if self.modal_open() || self.active != Some(card) {
            return Task::none();
        }
        let Some(tab) = self.tab(card) else {
            return Task::none();
        };
        let (Some(doc), Some(at)) = (tab.runtime.lsp_document(), tab.runtime.cursor_position())
        else {
            return Task::none();
        };
        let (kind, at) = match action {
            EditorLspAction::Definition(position) => {
                (LocationKind::Definition, position.unwrap_or(at))
            }
            EditorLspAction::References => (LocationKind::References, at),
            _ => return Task::none(),
        };
        let Some(version) = self.lsp.document_version(doc) else {
            return Task::none();
        };
        if let Some(old) = self.lsp_navigation.requests.remove(&card) {
            self.lsp.cancel(old.request);
        }
        self.lsp_navigation.results.remove(&card);
        self.lsp_navigation.resolving.remove(&card);
        let request = match kind {
            LocationKind::Definition => self.lsp.request_definition(doc, at),
            LocationKind::References => self.lsp.request_references(doc, at),
        };
        // Click-definition may explicitly name another position without moving
        // the native caret. Fence delivery to the actual initiating caret.
        let caret = self
            .tab(card)
            .and_then(|tab| tab.runtime.cursor_position())
            .unwrap();
        self.lsp_navigation.requests.insert(
            card,
            Origin {
                doc,
                request,
                version,
                at: caret,
                epoch: self.selection_epoch,
            },
        );
        Task::none()
    }

    pub(super) fn navigation_event(&mut self, event: &LspEvent) -> Option<Task<Message>> {
        let LspEvent::Locations {
            doc,
            request,
            kind,
            items,
        } = event
        else {
            if let LspEvent::RequestFailed { request, .. } = event {
                self.lsp_navigation
                    .requests
                    .retain(|_, origin| origin.request != *request);
            }
            return None;
        };
        let card = self
            .lsp_navigation
            .requests
            .iter()
            .find_map(|(card, origin)| {
                (origin.doc == *doc && origin.request == *request).then_some(*card)
            })?;
        let origin = self.lsp_navigation.requests.remove(&card)?;
        if !self.navigation_origin_current(card, &origin) {
            return Some(Task::none());
        }
        let withheld = items.len().saturating_sub(RESULT_LIMIT);
        let items = items.iter().take(RESULT_LIMIT).cloned().collect::<Vec<_>>();
        let single = *kind == LocationKind::Definition && items.len() == 1 && withheld == 0;
        self.lsp_navigation.results.insert(
            card,
            Results {
                origin,
                kind: *kind,
                items,
                withheld,
            },
        );
        if single {
            Some(self.update_navigation(Action::Select {
                card,
                request: *request,
                index: 0,
            }))
        } else {
            Some(Task::none())
        }
    }

    pub(super) fn update_navigation(&mut self, action: Action) -> Task<Message> {
        if self.modal_open() || self.exiting {
            return Task::none();
        }
        match action {
            Action::Dismiss(card) => {
                if let Some(origin) = self.lsp_navigation.requests.remove(&card) {
                    self.lsp.cancel(origin.request);
                }
                self.lsp_navigation.results.remove(&card);
                self.lsp_navigation.resolving.remove(&card);
                Task::none()
            }
            Action::Select {
                card,
                request,
                index,
            } => {
                let Some(results) = self.lsp_navigation.results.get(&card) else {
                    return Task::none();
                };
                if results.origin.request != request
                    || !self.navigation_origin_current(card, &results.origin)
                {
                    return Task::none();
                }
                let Some(item) = results.items.get(index).cloned() else {
                    return Task::none();
                };
                let Some(tab) = self.tab(card) else {
                    return Task::none();
                };
                let root = tab.runtime.descriptor.worktree.clone();
                let (path, force_external) = match item.target {
                    LspTarget::Workspace { path } => (
                        if path.is_absolute() {
                            path
                        } else {
                            root.join(path)
                        },
                        false,
                    ),
                    LspTarget::External { path } => (path, true),
                    LspTarget::Uri { .. } => {
                        self.notice = Some(
                            "This location is not a local file and cannot be opened as source."
                                .into(),
                        );
                        return Task::none();
                    }
                };
                self.lsp_navigation.next_resolution =
                    self.lsp_navigation.next_resolution.wrapping_add(1).max(1);
                let ticket = self.lsp_navigation.next_resolution;
                self.lsp_navigation
                    .resolving
                    .insert(card, (request, ticket));
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let path = path
                            .canonicalize()
                            .map_err(|error| format!("Cannot open source: {error}"))?;
                        let root = root
                            .canonicalize()
                            .map_err(|error| format!("Cannot resolve source workspace: {error}"))?;
                        if !path.is_file() {
                            return Err("The source location is not a regular file.".into());
                        }
                        let external = force_external || !path.starts_with(&root);
                        Ok(Resolved {
                            path,
                            external,
                            at: item.range.start,
                            coordinates: item.coordinates,
                        })
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::Navigation(Action::Resolved {
                        card,
                        request,
                        ticket,
                        result,
                    })
                })
            }
            Action::Resolved {
                card,
                request,
                ticket,
                result,
            } => {
                if self.lsp_navigation.resolving.get(&card) != Some(&(request, ticket)) {
                    return Task::none();
                }
                self.lsp_navigation.resolving.remove(&card);
                let Some(results) = self.lsp_navigation.results.get(&card) else {
                    return Task::none();
                };
                if results.origin.request != request
                    || !self.navigation_origin_current(card, &results.origin)
                {
                    return Task::none();
                }
                let resolved = match result {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        self.notice = Some(error);
                        return Task::none();
                    }
                };
                self.open_navigation_target(card, resolved)
            }
        }
    }

    fn open_navigation_target(&mut self, origin: CardId, resolved: Resolved) -> Task<Message> {
        let Some(tab) = self.tab(origin) else {
            return Task::none();
        };
        let root = tab.runtime.descriptor.worktree.clone();
        let authority = tab.worktree_authority.clone();
        let git = tab.git_context;
        let origin_location = self.capture_editor_location(origin);
        let existing = self
            .tabs
            .iter()
            .find(|tab| {
                tab.runtime.source_tab_path().as_ref() == Some(&resolved.path)
                    && tab.runtime.is_read_only_source() == resolved.external
            })
            .map(|tab| tab.id);
        let mut tasks = Vec::new();
        let target = if let Some(target) = existing {
            target
        } else {
            let relative = if resolved.external {
                None
            } else {
                resolved
                    .path
                    .strip_prefix(&root)
                    .ok()
                    .map(Path::to_path_buf)
            };
            if !resolved.external && relative.is_none() {
                return Task::none();
            }
            tasks.push(self.add_with_source_context(
                Opened {
                    root,
                    file: relative,
                    snapshot: None,
                    authority,
                },
                true,
                resolved.external.then(|| resolved.path.clone()),
                git,
            ));
            let Some(target) = self.active else {
                return Task::none();
            };
            if resolved.external
                && let Some(tab) = self.tab_mut(target)
            {
                tab.title = format!(
                    "{} · read-only",
                    resolved
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                );
            }
            target
        };
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.active = Some(target);
        self.lsp_navigation.jumps.insert(
            target,
            PendingJump {
                path: resolved.path,
                at: resolved.at,
                coordinates: resolved.coordinates,
                epoch: self.selection_epoch,
                origin: origin_location,
            },
        );
        tasks.push(self.finish_navigation_jump(target));
        Task::batch(tasks)
    }

    pub(super) fn finish_navigation_jump(&mut self, card: CardId) -> Task<Message> {
        let may_focus = !self.modal_open() && !self.exiting;
        let Some(jump) = self.lsp_navigation.jumps.get(&card) else {
            return Task::none();
        };
        if self.selection_epoch != jump.epoch || self.active != Some(card) {
            self.lsp_navigation.jumps.remove(&card);
            return Task::none();
        }
        let Some(tab) = self.tab(card) else {
            self.lsp_navigation.jumps.remove(&card);
            return Task::none();
        };
        if tab.runtime.source_tab_path().as_ref() != Some(&jump.path) {
            return Task::none();
        }
        if tab.runtime.editor_text().is_none() {
            return Task::none();
        }
        let jump = self.lsp_navigation.jumps.remove(&card).unwrap();
        let tab = self.tab_mut(card).unwrap();
        if !tab
            .runtime
            .move_cursor_to_coordinates(jump.at, jump.coordinates)
        {
            self.notice = Some("The source position is no longer valid.".into());
            return Task::none();
        }
        let focus = if may_focus {
            tab.runtime.focus_editor(card).map(cosmic::Action::App)
        } else {
            Task::none()
        };
        if let Some(origin) = jump.origin
            && let Some(target) = self.capture_editor_location(card)
        {
            self.lsp_navigation.history.record_jump(origin, target);
            self.lsp_navigation.history_origins.remove(&card);
            if let Some(tab) = self.tab_mut(card) {
                tab.runtime.take_location_jump_origin(card);
            }
        }
        focus
    }

    pub(super) fn collect_location_jump(&mut self, card: CardId) {
        if let Some(tab) = self.tab_mut(card)
            && let Some(origin) = tab.runtime.take_location_jump_origin(card)
        {
            self.lsp_navigation
                .history_origins
                .entry(card)
                .or_insert(origin);
        }
        if self.active != Some(card)
            || self.lsp_navigation.jumps.contains_key(&card)
            || self.lsp_navigation.history_restores.contains_key(&card)
        {
            return;
        }
        if let Some(target) = self.capture_editor_location(card)
            && let Some(origin) = self.lsp_navigation.history_origins.remove(&card)
        {
            self.lsp_navigation.history.record_jump(origin, target);
        }
    }

    fn capture_editor_location(&self, card: CardId) -> Option<cagents::editor_locations::Location> {
        let tab = self.tab(card)?;
        Some(cagents::editor_locations::Location {
            card,
            source: tab.runtime.source_tab_path()?,
            anchor: tab.runtime.editor_navigation_anchor()?,
        })
    }

    pub(super) fn navigate_cross_card_location(
        &mut self,
        card: CardId,
        forward: bool,
    ) -> Option<Task<Message>> {
        if self.modal_open() || self.exiting || self.active != Some(card) {
            return None;
        }
        self.collect_location_jump(card);
        let from = self.capture_editor_location(card)?;
        self.lsp_navigation.history.observe_origin(&from);
        let target = self.lsp_navigation.history.peek(forward, |entry| {
            self.tab(entry.card)
                .is_some_and(|tab| !tab.runtime.is_saving())
        })?;
        let same_source = self.tab(target.location.card).is_some_and(|tab| {
            tab.runtime.source_tab_path().as_ref() == Some(&target.location.source)
                && tab.runtime.editor_navigation_anchor().is_some()
        });
        if !same_source {
            let tab = self.tab(target.location.card)?;
            let root = tab.runtime.descriptor.worktree.clone();
            let authority = tab.worktree_authority.clone();
            let git = tab.git_context;
            let relative = target
                .location
                .source
                .strip_prefix(&root)
                .ok()
                .map(Path::to_path_buf);
            let external = relative.is_none().then(|| target.location.source.clone());
            let task = self.add_with_source_context(
                Opened {
                    root,
                    file: relative,
                    snapshot: None,
                    authority,
                },
                true,
                external,
                git,
            );
            let opened = self.active?;
            let admitted = self.tab(opened).is_some_and(|tab| {
                tab.runtime.source_tab_path().as_ref() == Some(&target.location.source)
                    || tab
                        .pending_file
                        .as_ref()
                        .and_then(|pending| pending.canonical.as_ref())
                        == Some(&target.location.source)
            });
            if !admitted {
                return Some(task);
            }
            self.selection_epoch = self.selection_epoch.wrapping_add(1);
            self.lsp_navigation
                .history_restores
                .insert(opened, (from, target, self.selection_epoch));
            self.finish_editor_history_restore(opened);
            return Some(task);
        }
        let previous = {
            let tab = self.tab_mut(target.location.card)?;
            let previous = tab.runtime.editor_navigation_anchor()?;
            if !tab
                .runtime
                .restore_editor_navigation_anchor(target.location.anchor)
            {
                return None;
            }
            previous
        };
        if !self.lsp_navigation.history.navigate(from, &target) {
            if let Some(tab) = self.tab_mut(target.location.card) {
                tab.runtime.restore_editor_navigation_anchor(previous);
            }
            return None;
        }
        let focus = self
            .tab(target.location.card)?
            .runtime
            .focus_editor(target.location.card)
            .map(cosmic::Action::App);
        self.active = Some(target.location.card);
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        Some(focus)
    }

    pub(super) fn finish_editor_history_restore(&mut self, card: CardId) {
        let Some((_, target, epoch)) = self.lsp_navigation.history_restores.get(&card) else {
            return;
        };
        if self.active != Some(card)
            || self.selection_epoch != *epoch
            || !self.lsp_navigation.history.is_current_target(target)
        {
            self.lsp_navigation.history_restores.remove(&card);
            return;
        }
        // Deduplication can reuse a tab that still displays its previous
        // buffer while a canonical open waits for Save. That loaded buffer is
        // not a refusal of the exact pending source admitted above.
        if self.tab(card).is_some_and(|tab| {
            tab.pending_file
                .as_ref()
                .and_then(|pending| pending.canonical.as_ref())
                == Some(&target.location.source)
        }) {
            return;
        }
        let Some(loaded) = self.capture_editor_location(card) else {
            return;
        };
        if loaded.source != target.location.source {
            self.lsp_navigation.history_restores.remove(&card);
            return;
        }
        let (from, target, _) = self.lsp_navigation.history_restores.remove(&card).unwrap();
        if let Some(tab) = self.tab_mut(card)
            && tab
                .runtime
                .restore_editor_navigation_anchor(target.location.anchor)
        {
            tab.runtime.take_location_jump_origin(card);
            if self.lsp_navigation.history.navigate(from, &target)
                && let Some(current) = self.capture_editor_location(card)
            {
                self.lsp_navigation.history.replace_current(current);
            }
        }
        self.lsp_navigation.history_origins.remove(&card);
    }

    pub(super) fn navigation_results_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let results = self.lsp_navigation.results.get(&card)?;
        let title = match results.kind {
            LocationKind::Definition => "Definitions",
            LocationKind::References => "References",
        };
        let mut rows = vec![
            widget::row::with_children(vec![
                design::caption(format!("{title} ({})", results.items.len())).into(),
                widget::button::text("Dismiss")
                    .on_press(Message::Navigation(Action::Dismiss(card)))
                    .into(),
            ])
            .spacing(8)
            .into(),
        ];
        if results.items.is_empty() {
            rows.push(design::caption("No locations found").into());
        }
        for (index, item) in results.items.iter().enumerate() {
            let (path, local) = match &item.target {
                LspTarget::Workspace { path } | LspTarget::External { path } => {
                    (location_label_path(&path.to_string_lossy()), true)
                }
                LspTarget::Uri { uri } => (location_label_path(uri), false),
            };
            let label = format!(
                "{}:{}:{}",
                path,
                item.range.start.line.saturating_add(1),
                item.range.start.character.saturating_add(1)
            );
            rows.push(
                widget::button::text(label)
                    .on_press_maybe(local.then_some(Message::Navigation(Action::Select {
                        card,
                        request: results.origin.request,
                        index,
                    })))
                    .into(),
            );
        }
        if results.withheld != 0 {
            rows.push(
                design::caption(format!(
                    "{} additional locations withheld",
                    results.withheld
                ))
                .into(),
            );
        }
        Some(
            widget::scrollable(widget::column::with_children(rows).spacing(4))
                .height(Length::Fixed(180.0))
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{editor, git_fixture, open_real_git, run_tasks};
    use cosmic::iced::futures::StreamExt;

    fn open_file(app: &mut Editor, root: &Path, path: &str) -> CardId {
        let task = app.add(
            Opened {
                root: root.to_path_buf(),
                file: Some(path.into()),
                snapshot: None,
                authority: None,
            },
            true,
        );
        run_tasks(app, task);
        app.active.unwrap()
    }

    #[test]
    fn internal_definition_tab_retains_its_original_git_authority_and_controls() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("target.rs"), "fn target() {}\n").unwrap();
        let mut app = editor();
        let origin = open_real_git(&mut app, root, Some("tracked.txt"));
        let task = app.open_navigation_target(
            origin,
            Resolved {
                path: root.join("target.rs"),
                external: false,
                at: LspPosition::new(0, 3),
                coordinates: LspCoordinates::Utf16,
            },
        );
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        assert_ne!(target, origin);
        let tab = app.tab(target).unwrap();
        assert!(tab.git, "an internal source remains a Git tab");
        assert_eq!(tab.worktree_authority.as_ref().unwrap().path(), root);
        assert!(
            tab.sidebar
                .source_control
                .as_ref()
                .is_some_and(|source| !source.is_reloading())
        );
        assert!(tab.runtime.editor_text().is_some());
        assert!(
            tab.runtime
                .diff_presentation_action(
                    target,
                    repository_widget::DiffPresentationCommand::StageFile
                )
                .is_some(),
            "the untracked target retains Git toolbar capabilities"
        );
    }

    #[test]
    fn internal_navigation_keeps_git_provenance_without_optional_mutation_authority() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("target.rs"), "fn target() {}\n").unwrap();
        let mut app = editor();
        let origin = open_real_git(&mut app, root, Some("tracked.txt"));
        app.tab_mut(origin).unwrap().worktree_authority = None;
        // Linux correctly refuses a missing strong authority rather than
        // falling back to path mutation. This fixture checks classification;
        // portable read-only snapshot execution is qualified in its own lane.
        drop(app.open_navigation_target(
            origin,
            Resolved {
                path: root.join("target.rs"),
                external: false,
                at: LspPosition::new(0, 3),
                coordinates: LspCoordinates::Utf16,
            },
        ));
        let target = app.active.unwrap();
        assert_ne!(target, origin);
        assert!(app.tab(target).unwrap().git);
        assert!(app.tab(target).unwrap().worktree_authority.is_none());
    }

    #[test]
    fn external_definition_tab_preserves_underlying_git_context_for_project_return() {
        let directory = git_fixture();
        let root = directory.path();
        let dependency = tempfile::tempdir().unwrap();
        let external = dependency.path().join("library.rs");
        std::fs::write(&external, "fn library() {}\n").unwrap();
        std::fs::write(root.join("target.rs"), "fn target() {}\n").unwrap();
        let mut app = editor();
        let origin = open_real_git(&mut app, root, Some("tracked.txt"));
        let task = app.open_navigation_target(
            origin,
            Resolved {
                path: external,
                external: true,
                at: LspPosition::new(0, 3),
                coordinates: LspCoordinates::Utf16,
            },
        );
        run_tasks(&mut app, task);
        let outside = app.active.unwrap();
        assert!(!app.tab(outside).unwrap().git);
        assert!(app.tab(outside).unwrap().git_context);
        assert!(app.tab(outside).unwrap().runtime.is_read_only_source());
        let task = app.open_navigation_target(
            outside,
            Resolved {
                path: root.join("target.rs"),
                external: false,
                at: LspPosition::new(0, 3),
                coordinates: LspCoordinates::Utf16,
            },
        );
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        assert_ne!(target, outside);
        assert!(app.tab(target).unwrap().git);
        assert!(!app.tab(target).unwrap().runtime.is_read_only_source());
        assert!(app.tab(target).unwrap().worktree_authority.is_some());
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
        assert_eq!(
            location_label_path(&"é".repeat(LABEL_CHAR_LIMIT))
                .chars()
                .count(),
            LABEL_CHAR_LIMIT
        );
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
        let task = app.open_navigation_target(
            origin,
            Resolved {
                path: source.clone(),
                external: true,
                at: LspPosition::new(1, 4),
                coordinates: LspCoordinates::Utf16,
            },
        );
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        assert_ne!(target, origin);
        let tab = app.tab(target).unwrap();
        assert!(tab.runtime.is_read_only_source());
        assert_eq!(tab.runtime.editor_text().as_deref(), Some(text));
        assert_eq!(tab.runtime.cursor_position(), Some(LspPosition::new(1, 3)));
        assert!(!tab.runtime.can_save());
        let task = app.update(Message::Repository(RepositoryAction::EditorAction {
            card: target,
            action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')),
        }));
        run_tasks(&mut app, task);
        assert_eq!(
            app.tab(target).unwrap().runtime.editor_text().as_deref(),
            Some(text)
        );
        assert_eq!(std::fs::read_to_string(source).unwrap(), text);
        assert_eq!(app.session().paths, vec![root.path().join("main.rs")]);
        assert_eq!(app.session().active, None);
        assert_eq!(
            app.tab(origin).unwrap().runtime.editor_text().as_deref(),
            Some("fn main() {}\n")
        );
    }

    #[test]
    fn workspace_definition_opens_another_tab_without_replacing_dirty_origin_and_reuses_target() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "main();\n").unwrap();
        std::fs::write(root.path().join("target.rs"), "a😀éX\n").unwrap();
        let mut app = editor();
        let origin = open_file(&mut app, root.path(), "main.rs");
        app.tab_mut(origin).unwrap().runtime.set_auto_save(false);
        let task = app.update(Message::Repository(RepositoryAction::EditorAction {
            card: origin,
            action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')),
        }));
        run_tasks(&mut app, task);
        let before = app.tab(origin).unwrap().runtime.editor_text().unwrap();
        assert!(app.tab(origin).unwrap().runtime.is_dirty());
        let resolved = Resolved {
            path: root.path().join("target.rs"),
            external: false,
            at: LspPosition::new(0, 4),
            coordinates: LspCoordinates::Utf16,
        };
        let task = app.open_navigation_target(origin, resolved.clone());
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        assert_ne!(target, origin);
        assert!(!app.tab(target).unwrap().runtime.is_read_only_source());
        assert_eq!(
            app.tab(target).unwrap().runtime.cursor_position(),
            Some(LspPosition::new(0, 3))
        );
        assert_eq!(
            app.tab(origin).unwrap().runtime.editor_text().unwrap(),
            before
        );
        assert!(app.tab(origin).unwrap().runtime.is_dirty());
        let count = app.tabs.len();
        let task = app.open_navigation_target(origin, resolved);
        run_tasks(&mut app, task);
        assert_eq!(app.active, Some(target));
        assert_eq!(app.tabs.len(), count);
    }

    #[cfg(unix)]
    #[test]
    fn an_alias_open_reuses_its_canonical_source_for_definition_navigation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real.rs"), "head\na😀éX\n").unwrap();
        std::os::unix::fs::symlink("real.rs", root.path().join("alias.rs")).unwrap();
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "alias.rs");
        assert_eq!(
            app.tab(card).unwrap().runtime.source_tab_path(),
            Some(root.path().join("real.rs"))
        );
        let task = app.open_navigation_target(
            card,
            Resolved {
                path: root.path().join("real.rs"),
                external: false,
                at: LspPosition::new(1, 4),
                coordinates: LspCoordinates::Utf16,
            },
        );
        run_tasks(&mut app, task);
        assert_eq!(app.active, Some(card));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(
            app.tab(card).unwrap().runtime.cursor_position(),
            Some(LspPosition::new(1, 3))
        );
        assert!(!app.lsp_navigation.jumps.contains_key(&card));
        assert_eq!(
            app.capture_editor_location(card).unwrap().source,
            root.path().join("real.rs")
        );
    }

    #[test]
    fn native_navigation_messages_round_trip_dirty_and_readonly_source_tabs() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "origin α\nsecond\n").unwrap();
        let path = external.path().join("std.rs");
        std::fs::write(&path, "a😀éX\n").unwrap();
        let mut app = editor();
        let origin = open_file(&mut app, root.path(), "main.rs");
        app.tab_mut(origin).unwrap().runtime.set_auto_save(false);
        let task = app.update(Message::Repository(RepositoryAction::EditorAction {
            card: origin,
            action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')),
        }));
        run_tasks(&mut app, task);
        let anchor = cagents::model::EditorNavigation {
            caret: cagents::model::EditorPosition { line: 1, column: 3 },
            selection: Some(cagents::model::EditorPosition { line: 0, column: 2 }),
            scroll_line: 1,
            scroll_vertical: 3.0,
            scroll_horizontal: 4.0,
        };
        assert!(
            app.tab_mut(origin)
                .unwrap()
                .runtime
                .restore_editor_navigation_anchor(anchor)
        );
        let original_text = app.tab(origin).unwrap().runtime.editor_text();
        let task = app.open_navigation_target(
            origin,
            Resolved {
                path: path.clone(),
                external: true,
                at: LspPosition::new(0, 4),
                coordinates: LspCoordinates::Utf16,
            },
        );
        run_tasks(&mut app, task);
        let target = app.active.unwrap();
        let target_anchor = app.capture_editor_location(target).unwrap().anchor;
        for action in [
            RepositoryAction::OpenGoToLine { card: target },
            RepositoryAction::GoToLineChanged {
                card: target,
                value: "1:1".into(),
            },
            RepositoryAction::SubmitGoToLine { card: target },
        ] {
            let task = app.update(Message::Repository(action));
            run_tasks(&mut app, task);
        }
        assert_eq!(
            app.capture_editor_location(target)
                .unwrap()
                .anchor
                .caret
                .column,
            0
        );
        let local_back = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: target,
            forward: false,
        }));
        run_tasks(&mut app, local_back);
        assert_eq!(app.active, Some(target));
        assert_eq!(
            app.capture_editor_location(target).unwrap().anchor,
            target_anchor
        );
        let back = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: target,
            forward: false,
        }));
        run_tasks(&mut app, back);
        assert_eq!(app.active, Some(origin));
        assert!(app.tab(origin).unwrap().runtime.is_dirty());
        assert_eq!(app.capture_editor_location(origin).unwrap().anchor, anchor);
        let forward = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: origin,
            forward: true,
        }));
        run_tasks(&mut app, forward);
        assert_eq!(app.active, Some(target));
        // An actual unpolled Save makes the dirty origin temporarily
        // unavailable. Completion must leave the same Back entry retryable.
        let save = app.update(Message::Repository(RepositoryAction::Save {
            card: origin,
            overwrite: false,
        }));
        assert!(app.tab(origin).unwrap().runtime.is_saving());
        assert!(app.navigate_cross_card_location(target, false).is_none());
        assert_eq!(app.active, Some(target));
        run_tasks(&mut app, save);
        let task = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: target,
            forward: false,
        }));
        run_tasks(&mut app, task);
        assert_eq!(app.active, Some(origin));
        assert_eq!(app.capture_editor_location(origin).unwrap().anchor, anchor);
        assert_eq!(
            app.tab(origin).unwrap().runtime.editor_text(),
            original_text
        );
        assert!(!app.tab(origin).unwrap().runtime.is_dirty());
        let task = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: origin,
            forward: true,
        }));
        run_tasks(&mut app, task);
        assert_eq!(app.active, Some(target));
        assert_eq!(
            app.capture_editor_location(target).unwrap().anchor,
            target_anchor
        );
        assert!(app.tab(target).unwrap().runtime.is_read_only_source());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "a😀éX\n");
        app.remove_tab(origin);
        assert!(app.navigate_cross_card_location(target, false).is_none());
    }

    #[test]
    fn window_history_exhaustion_never_reenters_the_runtime_stack() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "zero\none\ntwo\n").unwrap();
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "a.rs");
        for line in ["2", "3"] {
            for action in [
                RepositoryAction::OpenGoToLine { card },
                RepositoryAction::GoToLineChanged {
                    card,
                    value: line.into(),
                },
                RepositoryAction::SubmitGoToLine { card },
            ] {
                let task = app.update(Message::Repository(action));
                run_tasks(&mut app, task);
            }
        }
        for (forward, lines) in [(false, [1, 0, 0]), (true, [1, 2, 2])] {
            for line in lines {
                let task = app.update(Message::Repository(RepositoryAction::NavigateLocation {
                    card,
                    forward,
                }));
                run_tasks(&mut app, task);
                assert_eq!(
                    app.capture_editor_location(card).unwrap().anchor.caret.line,
                    line
                );
            }
        }
    }

    #[test]
    fn history_reopens_a_reused_tab_source_without_replacing_its_dirty_buffer() {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("a.rs", "first\na😀éX\n"),
            ("b.rs", "destination\n"),
            ("c.rs", "keep C\n"),
        ] {
            std::fs::write(root.path().join(path), text).unwrap();
        }
        let mut app = editor();
        let a = open_file(&mut app, root.path(), "a.rs");
        assert!(
            app.tab_mut(a)
                .unwrap()
                .runtime
                .move_cursor_to(LspPosition::new(1, 3))
        );
        let origin = app.capture_editor_location(a).unwrap();
        let b = open_file(&mut app, root.path(), "b.rs");
        let destination = app.capture_editor_location(b).unwrap();
        app.lsp_navigation
            .history
            .record_jump(origin.clone(), destination);
        let replace = app.request_file(a, "c.rs".into());
        run_tasks(&mut app, replace);
        app.tab_mut(a).unwrap().runtime.set_auto_save(false);
        let edit = app.update(Message::Repository(RepositoryAction::EditorAction {
            card: a,
            action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')),
        }));
        run_tasks(&mut app, edit);
        let retained = app.tab(a).unwrap().runtime.editor_text();
        app.active = Some(b);
        let back = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: b,
            forward: false,
        }));
        let restored = app.active.unwrap();
        assert_ne!(restored, a);
        assert_eq!(
            app.tab(restored)
                .unwrap()
                .pending_file
                .as_ref()
                .and_then(|pending| pending.canonical.as_ref()),
            Some(&origin.source)
        );
        assert!(
            app.lsp_navigation.history_restores.contains_key(&restored),
            "the anchor must be parked before the new tab's asynchronous file list/load"
        );
        run_tasks(&mut app, back);
        assert_eq!(
            app.capture_editor_location(restored).unwrap().source,
            origin.source
        );
        assert_eq!(
            app.capture_editor_location(restored).unwrap().anchor,
            origin.anchor
        );
        assert_eq!(app.tab(a).unwrap().runtime.editor_text(), retained);
        assert!(app.tab(a).unwrap().runtime.is_dirty());
        let forward = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: restored,
            forward: true,
        }));
        run_tasks(&mut app, forward);
        assert_eq!(app.active, Some(b));
    }

    #[test]
    fn history_anchor_waits_for_the_exact_pending_owner_to_save_its_previous_buffer() {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("a.rs", "first\na😀éX\n"),
            ("b.rs", "destination\n"),
            ("c.rs", "keep C\n"),
        ] {
            std::fs::write(root.path().join(path), text).unwrap();
        }
        let mut app = editor();
        let a = open_file(&mut app, root.path(), "a.rs");
        assert!(
            app.tab_mut(a)
                .unwrap()
                .runtime
                .move_cursor_to(LspPosition::new(1, 3))
        );
        let origin = app.capture_editor_location(a).unwrap();
        let b = open_file(&mut app, root.path(), "b.rs");
        let destination = app.capture_editor_location(b).unwrap();
        app.lsp_navigation
            .history
            .record_jump(origin.clone(), destination);
        let replace = app.request_file(a, "c.rs".into());
        run_tasks(&mut app, replace);
        app.tab_mut(a).unwrap().runtime.set_auto_save(false);
        let pending = app.request_file(a, "a.rs".into());
        let edited = app.update(Message::Repository(RepositoryAction::EditorAction {
            card: a,
            action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('!')),
        }));
        run_tasks(&mut app, edited);
        run_tasks(&mut app, pending);
        assert_eq!(
            app.tab(a)
                .unwrap()
                .pending_file
                .as_ref()
                .and_then(|pending| pending.canonical.as_ref()),
            Some(&origin.source)
        );
        let retained = app.tab(a).unwrap().runtime.editor_text().unwrap();
        app.active = Some(b);
        let count = app.tabs.len();
        let back = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: b,
            forward: false,
        }));
        run_tasks(&mut app, back);
        assert_eq!(app.active, Some(a));
        assert_eq!(app.tabs.len(), count);
        assert_eq!(
            app.tab(a).unwrap().runtime.editor_text().as_deref(),
            Some(retained.as_str())
        );
        assert!(app.lsp_navigation.history_restores.contains_key(&a));
        let save = app.update(Message::Repository(RepositoryAction::Save {
            card: a,
            overwrite: false,
        }));
        run_tasks(&mut app, save);
        assert_eq!(
            std::fs::read_to_string(root.path().join("c.rs")).unwrap(),
            retained
        );
        assert_eq!(
            app.capture_editor_location(a).unwrap().source,
            origin.source
        );
        assert_eq!(
            app.capture_editor_location(a).unwrap().anchor,
            origin.anchor
        );
        assert!(!app.lsp_navigation.history_restores.contains_key(&a));
    }

    #[test]
    fn reopening_history_does_not_apply_an_anchor_after_a_newer_tab_selection() {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("a.rs", "first\na😀éX\n"),
            ("b.rs", "destination\n"),
            ("c.rs", "keep C\n"),
        ] {
            std::fs::write(root.path().join(path), text).unwrap();
        }
        let mut app = editor();
        let a = open_file(&mut app, root.path(), "a.rs");
        assert!(
            app.tab_mut(a)
                .unwrap()
                .runtime
                .move_cursor_to(LspPosition::new(1, 3))
        );
        let origin = app.capture_editor_location(a).unwrap();
        let b = open_file(&mut app, root.path(), "b.rs");
        let destination = app.capture_editor_location(b).unwrap();
        app.lsp_navigation
            .history
            .record_jump(origin.clone(), destination);
        let replace = app.request_file(a, "c.rs".into());
        run_tasks(&mut app, replace);
        app.active = Some(b);
        let back = app.update(Message::Repository(RepositoryAction::NavigateLocation {
            card: b,
            forward: false,
        }));
        let opened = app.active.unwrap();
        let target = app
            .lsp_navigation
            .history_restores
            .get(&opened)
            .unwrap()
            .1
            .clone();
        drop(app.update(Message::Select(b)));
        drop(app.update(Message::Select(opened)));
        run_tasks(&mut app, back);
        assert_eq!(app.active, Some(opened));
        assert_eq!(
            app.capture_editor_location(opened).unwrap().source,
            origin.source
        );
        assert_ne!(
            app.capture_editor_location(opened).unwrap().anchor.caret,
            origin.anchor.caret
        );
        assert!(!app.lsp_navigation.history_restores.contains_key(&opened));
        assert!(
            app.lsp_navigation.history.is_current_target(&target),
            "an abandoned load must not consume the history entry"
        );
    }

    #[test]
    fn configured_file_completion_lands_while_preferences_are_open() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "first\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "second\n").unwrap();
        std::fs::write(
            root.path().join(".editorconfig"),
            "root = true\n[*.rs]\nindent_style = space\nindent_size = 2\n",
        )
        .unwrap();
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "a.rs");
        let read =
            app.tab_mut(card)
                .unwrap()
                .runtime
                .open_file_location(card, "b.rs".into(), 0, 0, 0);
        app.settings_open = true;
        run_tasks(&mut app, repository_task(read));
        assert!(app.settings_open);
        assert_eq!(
            app.tab(card).unwrap().runtime.editor_text().as_deref(),
            Some("second\n")
        );
        assert_eq!(
            app.tab(card).unwrap().runtime.editor_formatting_options(),
            (2, true)
        );
        app.settings_open = false;
        assert_eq!(
            app.tab(card).unwrap().runtime.editor_text().as_deref(),
            Some("second\n")
        );
    }

    #[test]
    fn configured_file_completion_consumes_a_fragment_and_requests_editor_focus() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "first\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "first\nsecond\n").unwrap();
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "a.rs");
        let tab = app.tab_mut(card).unwrap();
        let read = tab.runtime.open_file_location(card, "b.rs".into(), 0, 0, 0);
        tab.pending_fragment = Some("L2".into());
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let completion = executor.block_on(async {
            let mut stream = iced::runtime::task::into_stream(read).unwrap();
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(
                    action @ RepositoryAction::ConfiguredFileLoaded { .. },
                ) = action
                {
                    return action;
                }
            }
            panic!("the actual configured read did not complete");
        });
        let delivered = app.update(Message::Repository(completion));
        assert!(app.tab(card).unwrap().pending_fragment.is_none());
        assert_eq!(
            app.tab(card).unwrap().runtime.cursor_position(),
            Some(LspPosition::new(1, 0))
        );
        let focus_requests = executor.block_on(async {
            let mut count = 0;
            if let Some(mut stream) = iced::runtime::task::into_stream(delivered) {
                while let Some(action) = stream.next().await {
                    if matches!(action, iced::runtime::Action::Widget(_)) {
                        count += 1;
                    }
                }
            }
            count
        });
        assert!(
            focus_requests > 0,
            "the normal load must publish its native editor focus operation"
        );
    }

    #[test]
    fn loaded_navigation_applies_position_without_focusing_behind_a_modal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "a😀éX\n").unwrap();
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "main.rs");
        app.lsp_navigation.jumps.insert(
            card,
            PendingJump {
                path: root.path().join("main.rs"),
                at: LspPosition::new(0, 4),
                coordinates: LspCoordinates::Utf16,
                epoch: app.selection_epoch,
                origin: None,
            },
        );
        app.settings_open = true;
        let task = app.finish_navigation_jump(card);
        assert!(iced::runtime::task::into_stream(task).is_none());
        assert_eq!(
            app.tab(card).unwrap().runtime.cursor_position(),
            Some(LspPosition::new(0, 3))
        );
        assert!(!app.lsp_navigation.jumps.contains_key(&card));
    }

    #[test]
    fn two_message_route_selections_keep_the_latest_target_when_callbacks_arrive_reversed() {
        use cagents::lsp::{LspLanguage, LspRange};
        use cagents::settings::LanguageServerSettings;
        use cosmic::iced::futures::StreamExt;
        let root = tempfile::tempdir().unwrap();
        for name in ["main.rs", "first.rs", "latest.rs"] {
            std::fs::write(root.path().join(name), "a😀éX\n").unwrap();
        }
        let mut app = editor();
        let card = open_file(&mut app, root.path(), "main.rs");
        // Register a pending document snapshot without launching a server.
        // Protocol response production is tested at the LSP boundary; this
        // fixture tests the actual UI message admissions and worker callbacks.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        app.lsp = LspManager::new(vec![LanguageServerSettings {
            id: "fixture".into(),
            command: vec!["/bin/false".into()],
            ..Default::default()
        }]);
        let doc = LspDocumentId(91);
        assert_eq!(
            app.lsp.open(
                doc,
                &root.path().join("main.rs"),
                root.path(),
                LspLanguage {
                    server_id: "fixture".into(),
                    language_id: "rust".into()
                },
                7,
                "a😀éX\n"
            ),
            LspStatus::Starting
        );
        app.tab_mut(card)
            .unwrap()
            .runtime
            .set_lsp_document(Some(doc));
        drop(app.update(Message::Repository(RepositoryAction::Language {
            card,
            action: EditorLspAction::References,
        })));
        let request = app.lsp_navigation.requests[&card].request;
        let items = ["first.rs", "latest.rs"]
            .into_iter()
            .map(|name| LspLocation {
                target: LspTarget::Workspace {
                    path: root.path().join(name),
                },
                range: LspRange::new(LspPosition::new(0, 4), LspPosition::new(0, 5)),
                coordinates: LspCoordinates::Utf16,
            })
            .collect();
        drop(app.update(Message::Lsp(
            app.lsp.generation(),
            Some(LspEvent::Locations {
                doc,
                request,
                kind: LocationKind::References,
                items,
            }),
        )));
        let first = app.update(Message::Navigation(Action::Select {
            card,
            request,
            index: 0,
        }));
        let latest = app.update(Message::Navigation(Action::Select {
            card,
            request,
            index: 1,
        }));
        drop(entered);
        let collect_callback = |task| {
            runtime.block_on(async {
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::Navigation(Action::Resolved { .. }),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("canonicalization did not return its callback");
            })
        };
        let latest_callback = collect_callback(latest);
        let first_callback = collect_callback(first);
        let load_latest = app.update(latest_callback);
        let target = app.active.unwrap();
        assert_ne!(target, card);
        drop(app.update(first_callback));
        assert_eq!(app.active, Some(target));
        run_tasks(&mut app, load_latest);
        assert_eq!(
            app.tab(target).unwrap().runtime.source_tab_path(),
            Some(root.path().join("latest.rs"))
        );
        assert_eq!(
            app.tab(target).unwrap().runtime.cursor_position(),
            Some(LspPosition::new(0, 3))
        );
        assert_eq!(app.tabs.len(), 2);
    }

    #[test]
    fn an_older_resolution_cannot_consume_the_latest_explicit_choice() {
        let mut app = editor();
        let card = CardId::new();
        let request = LspRequestId(17);
        app.lsp_navigation.resolving.insert(card, (request, 2));
        let stale = Action::Resolved {
            card,
            request,
            ticket: 1,
            result: Err("old choice failed".into()),
        };
        drop(app.update_navigation(stale));
        assert_eq!(app.lsp_navigation.resolving.get(&card), Some(&(request, 2)));
        assert!(app.notice.is_none());
        assert!(app.tabs.is_empty());
        drop(app.update_navigation(Action::Dismiss(card)));
        assert!(!app.lsp_navigation.resolving.contains_key(&card));
        drop(app.update_navigation(Action::Resolved {
            card,
            request,
            ticket: 2,
            result: Err("dismissed choice failed".into()),
        }));
        assert!(app.notice.is_none());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn dismissed_or_unowned_location_callbacks_cannot_open_tabs() {
        let mut app = editor();
        let card = CardId::new();
        let request = LspRequestId(17);
        drop(app.update_navigation(Action::Resolved {
            card,
            request,
            ticket: 1,
            result: Ok(Resolved {
                path: "/tmp/unrequested.rs".into(),
                external: true,
                at: LspPosition::new(0, 0),
                coordinates: LspCoordinates::Scalar,
            }),
        }));
        assert!(app.tabs.is_empty());
        let event = LspEvent::Locations {
            doc: LspDocumentId(3),
            request,
            kind: LocationKind::Definition,
            items: vec![],
        };
        assert!(app.navigation_event(&event).is_none());
        assert!(app.lsp_navigation.results.is_empty());
    }
}
