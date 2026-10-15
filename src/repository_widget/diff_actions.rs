//! Selection and find state for immutable diff surfaces.

use super::*;
use crate::diff_interaction::{
    self as interaction, Command as DiffCommand, CopyText, Selection, Side,
};

#[derive(Clone, PartialEq, Eq)]
struct SourceKey {
    generation: u64,
    basis: DiffBasis,
    mode: DiffMode,
    sources: [(usize, usize); 2],
}

#[derive(Clone, PartialEq, Eq)]
struct FindKey {
    source: SourceKey,
    query: String,
    options: crate::editor_search::FindOptions,
}

struct FindJob {
    key: FindKey,
    ticket: uuid::Uuid,
    cancellation: CancellationToken,
}
impl Drop for FindJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) selection: Option<Selection>,
    pub(super) dragging: bool,
    pub(super) matches: Vec<interaction::Match>,
    snapshot: Option<(SourceKey, Arc<interaction::SearchSnapshot>)>,
    active: Option<FindJob>,
    cache: Option<FindKey>,
}

impl RepositoryRuntime {
    pub fn selected_diff_range(&self) -> Option<Range<usize>> {
        self.shows_diff()
            .then_some(self.diff_interaction.selection)
            .flatten()
            .filter(|selection| selection.mode == self.diff_mode)
            .map(Selection::rows)
    }

    pub fn selected_diff_hunk(&self) -> Option<usize> {
        self.selected_diff_range()?;
        let model = self.diff_model()?;
        let head = self.diff_interaction.selection?.head;
        model
            .change_starts(self.diff_mode)
            .partition_point(|start| *start <= head)
            .checked_sub(1)
    }

    pub fn has_diff_selection(&self) -> bool {
        self.selected_diff_range().is_some()
    }

    pub(super) fn clear_diff_selection(&mut self) {
        self.diff_interaction.selection = None;
        self.diff_interaction.dragging = false;
    }

    pub(super) fn select_diff_row(
        &mut self,
        identity: AsyncIdentity,
        basis: DiffBasis,
        mode: DiffMode,
        revision: u64,
        row: usize,
        side: Side,
        extend: bool,
        context: bool,
        drag: bool,
    ) {
        if !self.accepts_selected(&identity)
            || self.source_surface != SourceSurface::Diff
            || self.diff_basis != basis
            || self.diff_mode != mode
            || self.diff_layout_revision != revision
            || self
                .diff_model()
                .is_none_or(|model| row >= model.row_count(mode))
        {
            return;
        }
        if drag && !self.diff_interaction.dragging {
            return;
        }
        if context
            && self
                .diff_interaction
                .selection
                .is_some_and(|selection| selection.contains(mode, side, row))
        {
            return;
        }
        let selection = &mut self.diff_interaction.selection;
        if (extend || drag)
            && selection.is_some_and(|selection| selection.mode == mode && (mode == DiffMode::Inline || selection.side == side))
        {
            selection.as_mut().unwrap().head = row;
        } else {
            *selection = Some(Selection::new(mode, side, row));
        }
        if !drag {
            self.diff_interaction.dragging = !context;
        }
        if let Some(hunk) = self.selected_diff_hunk() {
            self.diff_change = hunk;
        }
    }

    pub(super) fn move_diff_selection(
        &mut self,
        delta: i32,
        extend: bool,
    ) -> Task<RepositoryAction> {
        let Some(model) = self.diff_model() else {
            return Task::none();
        };
        let count = model.row_count(self.diff_mode);
        if count == 0 {
            return Task::none();
        }
        let origin = self.diff_interaction.selection.map_or_else(
            || match self.current_diff_viewport().row {
                Some(diff_layout::Anchor::Row { row, .. }) => row,
                _ => 0,
            },
            |selection| selection.head,
        );
        let row = origin.saturating_add_signed(delta as isize).min(count - 1);
        let side = self
            .diff_interaction
            .selection
            .map_or(Side::New, |selection| selection.side);
        if extend {
            self.diff_interaction
                .selection
                .get_or_insert(Selection::new(self.diff_mode, side, origin))
                .head = row;
        } else {
            self.diff_interaction.selection = Some(Selection::new(self.diff_mode, side, row));
        }
        self.reveal_diff_row(row);
        self.restore_visible_scroll()
    }

    pub(super) fn select_all_diff(&mut self) {
        if let Some(last) = self
            .diff_model()
            .and_then(|model| model.row_count(self.diff_mode).checked_sub(1))
        {
            let side = self
                .diff_interaction
                .selection
                .map_or(Side::New, |selection| selection.side);
            self.diff_interaction.selection = Some(Selection {
                mode: self.diff_mode,
                side,
                anchor: 0,
                head: last,
            });
        }
    }

    pub(super) fn copy_diff(
        &mut self,
        card: CardId,
        command: DiffCommand,
    ) -> Task<RepositoryAction> {
        let Some(model) = self.diff_model() else {
            return Task::none();
        };
        match command {
            DiffCommand::Copy | DiffCommand::CopyOld | DiffCommand::CopyNew => {
                let Some(selection) = self.diff_interaction.selection else {
                    return Task::none();
                };
                let kind = match command {
                    DiffCommand::CopyOld => CopyText::Old,
                    DiffCommand::CopyNew => CopyText::New,
                    _ => CopyText::Natural,
                };
                iced::clipboard::write(selection.text(model, kind))
            }
            DiffCommand::CopyPatch => {
                let Some(selection) = self.diff_interaction.selection else {
                    return Task::none();
                };
                let Some(path) = self
                    .selected_path()
                    .and_then(|path| RepoPath::parse(path).ok())
                else {
                    return Task::none();
                };
                let old_path = self
                    .comparison_old_path()
                    .and_then(|old| RepoPath::parse(old).ok());
                interaction::selected_patch(
                    model,
                    self.diff_mode,
                    selection.rows(),
                    &path,
                    old_path.as_ref(),
                )
                .map_or_else(Task::none, iced::clipboard::write)
            }
            DiffCommand::CopyLineReference { row, side } => {
                let selection = Selection::new(self.diff_mode, side, row);
                let Some(line) = selection.line_reference(model) else {
                    return Task::none();
                };
                let path = if side == Side::Old {
                    self.comparison_old_path()
                        .or_else(|| self.selected_path().map(Path::to_path_buf))
                } else {
                    self.selected_path().map(Path::to_path_buf)
                };
                path.map_or_else(Task::none, |path| {
                    iced::clipboard::write(format!("{}:{line}", path.display()))
                })
            }
            DiffCommand::OpenInEditor { row, side } => {
                if self.is_saving() { return Task::none(); }
                let Some(line) = model
                    .new_line_at_row(self.diff_mode, row)
                    .or_else(|| Selection::new(self.diff_mode, side, row).line_reference(model))
                else {
                    return Task::none();
                };
                if !matches!(self.document, DocumentState::Editable(_)) {
                    let Some(path) = self.selected_path().map(Path::to_path_buf) else {
                        return Task::none();
                    };
                    return cosmic::task::message(RepositoryAction::OpenWorkingFileLocation {
                        identity: self.current_identity(card, path),
                        line: line.saturating_sub(1) as usize,
                    });
                }
                self.viewports.editor = text_editor::ScrollPosition {
                    line: self.editor_line_for_diff_source(line),
                    vertical: 0.0,
                    horizontal: 0.0,
                };
                self.source_surface = SourceSurface::Editor;
                self.find_cache = None;
                self.clear_diff_find();
                self.markdown_mode = MarkdownMode::Edit;
                self.restore_native_viewport();
                Task::batch([self.restore_visible_scroll(), self.focus_editor(card)])
            }
        }
    }

    fn diff_source_key(&self) -> Option<SourceKey> {
        let model = self.diff_model()?;
        Some(SourceKey {
            generation: self.generation,
            basis: self.diff_basis.clone(),
            mode: self.diff_mode,
            sources: [
                (model.before.as_ptr() as usize, model.before.len()),
                (model.after.as_ptr() as usize, model.after.len()),
            ],
        })
    }

    fn wanted_diff_find(&self) -> Option<FindKey> {
        if !self.find_open || self.source_surface != SourceSurface::Diff {
            return None;
        }
        Some(FindKey {
            source: self.diff_source_key()?,
            query: self.find_query.clone(),
            options: self.find_options,
        })
    }

    pub(super) fn diff_find_pending(&self) -> bool {
        matches!(
            self.diff_state(&self.diff_basis),
            Some(DiffState::Loading { .. })
        ) || matches!(self.basis_resolution, Some(BasisResolution::Resolving(_)))
            || self.diff_interaction.cache.as_ref() != self.wanted_diff_find().as_ref()
    }

    pub(super) fn prepare_diff_find(&mut self) {
        let wanted = self.wanted_diff_find();
        if let Some(active) = &self.diff_interaction.active {
            if Some(&active.key) != wanted.as_ref() {
                active.cancellation.cancel();
            }
        }
        if self.diff_interaction.cache.as_ref() != wanted.as_ref() {
            self.diff_interaction.matches.clear();
            self.find_total = 0;
            self.find_error = None;
        }
    }

    pub(super) fn diff_find_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.prepare_diff_find();
        let Some(key) = self.wanted_diff_find() else {
            return Task::none();
        };
        if self.diff_interaction.cache.as_ref() == Some(&key)
            || self.diff_interaction.active.is_some()
        {
            return Task::none();
        }
        if key.query.is_empty() {
            self.diff_interaction.cache = Some(key);
            self.find_total = 0;
            return Task::none();
        }
        if self
            .diff_interaction
            .snapshot
            .as_ref()
            .is_none_or(|(source, _)| source != &key.source)
        {
            let Some(model) = self.diff_model() else {
                return Task::none();
            };
            self.diff_interaction.snapshot = Some((
                key.source.clone(),
                Arc::new(interaction::SearchSnapshot::new(model, self.diff_mode)),
            ));
        }
        let snapshot = Arc::clone(&self.diff_interaction.snapshot.as_ref().unwrap().1);
        let ticket = uuid::Uuid::new_v4();
        let cancellation = CancellationToken::new();
        self.diff_interaction.active = Some(FindJob {
            key: key.clone(),
            ticket,
            cancellation: cancellation.clone(),
        });
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                snapshot.find(&key.query, key.options, || cancellation.is_cancelled())
            })
            .await
            .unwrap_or_else(|error| Err(format!("Diff search worker failed: {error}")));
            RepositoryAction::DiffFindLoaded {
                card,
                ticket,
                result,
            }
        })
    }

    pub(super) fn finish_diff_find(
        &mut self,
        ticket: uuid::Uuid,
        result: Result<Option<Vec<interaction::Match>>, String>,
    ) -> Task<RepositoryAction> {
        if self
            .diff_interaction
            .active
            .as_ref()
            .is_none_or(|active| active.ticket != ticket)
        {
            return Task::none();
        }
        let active = self.diff_interaction.active.take().unwrap();
        if active.cancellation.is_cancelled()
            || self.wanted_diff_find().as_ref() != Some(&active.key)
        {
            return Task::none();
        }
        match result {
            Ok(Some(matches)) => self.diff_interaction.matches = matches,
            Ok(None) => return Task::none(),
            Err(error) => self.find_error = Some(error),
        }
        self.diff_interaction.cache = Some(active.key.clone());
        self.find_total = self.diff_interaction.matches.len();
        self.select_diff_match(self.find_index);
        self.restore_visible_scroll()
    }

    pub(super) fn select_diff_match(&mut self, occurrence: usize) {
        self.prepare_diff_find();
        self.find_index = occurrence;
        if self.diff_find_pending() {
            return;
        }
        self.find_total = self.diff_interaction.matches.len();
        if self.find_total == 0 {
            self.find_index = 0;
            return;
        }
        self.find_index %= self.find_total;
        let row = self.diff_interaction.matches[self.find_index].row;
        self.reveal_diff_row(row);
    }

    fn reveal_diff_row(&mut self, row: usize) {
        self.pending_diff_source = None;
        let fold = self.diff_layout().and_then(|layout| {
            let folded = layout.folded(self.diff_mode);
            match folded.row(folded.display(row)) {
                DisplayRow::Fold { fold, .. } => Some(fold),
                _ => None,
            }
        });
        if let Some(fold) = fold {
            drop(self.expand_diff_fold(fold, FoldExpansion::All));
        }
        self.apply_diff_viewport(DiffViewportAnchor {
            row: Some(diff_layout::Anchor::Row { row, fraction: 0.0 }),
            first_change: false,
            ..self.current_diff_viewport()
        });
        self.capture_diff_viewport();
    }

    pub(super) fn diff_match_ranges(&self, row: usize, side: Side) -> Vec<Range<usize>> {
        let matches = &self.diff_interaction.matches;
        let start = matches.partition_point(|found| found.row < row);
        matches[start..]
            .iter()
            .take_while(|found| found.row == row)
            .filter(|found| found.side == side)
            .map(|found| found.range.clone())
            .collect()
    }

    pub(super) fn diff_active_match(&self, row: usize, side: Side) -> Option<Range<usize>> {
        self.diff_interaction
            .matches
            .get(self.find_index)
            .filter(|found| found.row == row && found.side == side)
            .map(|found| found.range.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuCommand(CardId, u64, u64, DiffMode, [(usize, usize); 2], DiffCommand);
impl widget::menu::Action for MenuCommand {
    type Message = RepositoryAction;
    fn message(&self) -> Self::Message {
        RepositoryAction::DiffCommand {
            card: self.0,
            generation: self.1,
            revision: self.2,
            mode: self.3,
            sources: self.4,
            command: self.5,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RowContext<'a> {
    pub(super) card: CardId,
    pub(super) runtime: &'a RepositoryRuntime,
    pub(super) row: usize,
}

impl<'a> RowContext<'a> {
    pub(super) fn wrap(
        self,
        side: Side,
        content: Element<'a, RepositoryAction>,
    ) -> Element<'a, RepositoryAction> {
        let runtime = self.runtime;
        let Some(path) = runtime.selected_path() else {
            return content;
        };
        let identity = runtime.current_identity(self.card, path.to_path_buf());
        let select = |extend, context, drag| RepositoryAction::SelectDiffRow {
            identity: identity.clone(),
            basis: runtime.diff_basis.clone(),
            mode: runtime.diff_mode,
            revision: runtime.diff_layout_revision,
            row: self.row,
            side,
            extend,
            context,
            drag,
        };
        let selected = runtime
            .diff_interaction
            .selection
            .is_some_and(|selection| selection.contains(runtime.diff_mode, side, self.row));
        let content = interaction::row_surface(
            content,
            selected,
            select(false, false, false),
            select(false, true, false),
            runtime
                .diff_interaction
                .dragging
                .then(|| select(true, false, true)),
        );
        let card = self.card;
        let row = self.row;
        let unified = runtime.diff_mode == DiffMode::Inline;
        let editable = matches!(runtime.document, DocumentState::Editable(_));
        let can_open = !runtime.is_saving() && (editable || !runtime.blocks_navigation());
        let changed = runtime.diff_interaction.selection.zip(runtime.diff_model())
            .is_some_and(|(selection,model)| selection.has_changes(model));
        let Some(source) = runtime.diff_source_key() else {
            return content;
        };
        let generation = runtime.generation;
        let revision = runtime.diff_layout_revision;
        let mode = runtime.diff_mode;
        crate::interaction::context_menu_lazy(content, move || {
            let command =
                |command| MenuCommand(card, generation, revision, mode, source.sources, command);
            use widget::menu::{Item, KeyBind, key_bind::Modifier};
            let copy = command(DiffCommand::Copy);
            let bindings = HashMap::from([(
                KeyBind {
                    modifiers: vec![Modifier::Ctrl],
                    key: iced::keyboard::Key::Character("c".into()),
                },
                copy,
            )]);
            let mut items = vec![Item::Button("Copy", None, copy)];
            if unified {
                items.push(Item::Button(
                    "Copy old side",
                    None,
                    command(DiffCommand::CopyOld),
                ));
                items.push(Item::Button(
                    "Copy new side",
                    None,
                    command(DiffCommand::CopyNew),
                ));
            }
            items.push(if changed {
                Item::Button("Copy as patch", None, command(DiffCommand::CopyPatch))
            } else {
                Item::ButtonDisabled("Copy as patch", None, command(DiffCommand::CopyPatch))
            });
            items.push(Item::Divider);
            items.push(Item::Button(
                "Copy line reference",
                None,
                command(DiffCommand::CopyLineReference { row, side }),
            ));
            let open = command(DiffCommand::OpenInEditor { row, side });
            let label = if editable {
                "Open in editor at line"
            } else {
                "Open working file in editor at line"
            };
            items.push(if can_open {
                Item::Button(label, None, open)
            } else {
                Item::ButtonDisabled(label, None, open)
            });
            widget::menu::items(&bindings, items)
        })
    }
}

impl RepositoryRuntime {
    pub(super) fn clear_diff_find(&mut self) {
        self.diff_interaction.cache = None;
        self.diff_interaction.snapshot = None;
        self.diff_interaction.matches.clear();
        if let Some(active) = &self.diff_interaction.active {
            active.cancellation.cancel();
        }
    }

    pub(super) fn open_diff_find(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.find_open = true;
        self.replace_open = false;
        self.find_origin = None;
        self.find_index = 0;
        self.set_search_mode(true);
        self.prepare_diff_find();
        iced::widget::operation::focus(find_focus_id(card))
    }
}

impl RepositoryRuntime {
    pub(super) fn run_diff_command(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        mode: DiffMode,
        sources: [(usize, usize); 2],
        command: DiffCommand,
    ) -> Task<RepositoryAction> {
        if self.generation != generation
            || self.diff_layout_revision != revision
            || self.diff_mode != mode
            || self
                .diff_source_key()
                .is_none_or(|key| key.sources != sources)
        {
            return Task::none();
        }
        self.copy_diff(card, command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(mode: DiffMode) -> (RepositoryRuntime, CardId, AsyncIdentity) {
        let card = CardId::new();
        let before = (0..200)
            .map(|row| format!("line {row}\n"))
            .collect::<String>()
            .replace("line 100\n", "Straße needle\n");
        let after = before
            .replace("line 20\n", "changed 20\n")
            .replace("line 180\n", "changed 180\n");
        let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "source.rs", &after);
        runtime.source_surface = SourceSurface::Diff;
        runtime.diff_basis = DiffBasis::HeadToIndex;
        runtime.diff_mode = mode;
        runtime.diff_viewport.height = 100.0;
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(crate::diff_view::build_diff(&before, &after)),
                old_label: "HEAD".into(),
                new_label: "Index".into(),
            }),
        );
        (runtime, card, identity)
    }

    async fn drive(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
        use cosmic::iced::futures::StreamExt;
        let mut pending = VecDeque::from([task]);
        while let Some(task) = pending.pop_front() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(action) = action {
                        pending.push_back(runtime.update(action));
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn diff_find_reveals_folded_context_in_both_layouts() {
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let (mut runtime, card, _) = fixture(mode);
            let row = runtime
                .diff_model()
                .unwrap()
                .row_at_new_line(mode, 101)
                .unwrap();
            let folded = runtime.diff_layout().unwrap().folded(mode);
            assert!(matches!(
                folded.row(folded.display(row)),
                DisplayRow::Fold { .. }
            ));
            runtime.find_query = "STRASSE".into();
            drop(runtime.open_find(card));
            let task = runtime.diff_find_task(card);
            drive(&mut runtime, task).await;
            assert_eq!(
                runtime.find_total,
                if mode == DiffMode::Inline { 1 } else { 2 }
            );
            assert!(!runtime.diff_find_pending());
            let found = &runtime.diff_interaction.matches[runtime.find_index];
            assert_eq!(found.line, 101);
            let folded = runtime.diff_layout().unwrap().folded(mode);
            assert!(matches!(
                folded.row(folded.display(found.row)),
                DisplayRow::Code(_)
            ));
            assert_eq!(
                runtime
                    .diff_model()
                    .unwrap()
                    .new_line_at_row(mode, found.row),
                Some(101)
            );
            let text = match &runtime.document {
                DocumentState::Editable(document) => document.content.text(),
                _ => panic!("editable backing source"),
            };
            assert_eq!(
                text,
                runtime.diff_model().unwrap().after.as_ref(),
                "search must not edit the backing source"
            );
        }
    }

    #[test]
    fn unified_selection_extends_across_old_and_new_rows_but_split_stays_per_side() {
        for mode in [DiffMode::Inline,DiffMode::Split] {
            let (mut runtime,card,identity) = fixture(mode);
            let model = runtime.diff_model().unwrap();
            let start = model.change_starts(mode)[0];
            let end = start+1;
            let revision = runtime.diff_layout_revision;
            let basis = runtime.diff_basis.clone();
            runtime.select_diff_row(identity.clone(),basis.clone(),mode,revision,start,Side::Old,false,false,false);
            runtime.select_diff_row(identity,basis,mode,revision,end,Side::New,true,false,false);
            let selection = runtime.diff_interaction.selection.unwrap();
            assert_eq!(selection.rows(),if mode == DiffMode::Inline { start..end+1 } else { end..end+1 });
            drop(runtime.update(RepositoryAction::EndDiffSelection { card }));
            assert!(!runtime.diff_interaction.dragging);
        }
    }

    #[tokio::test]
    async fn historical_diff_open_editor_requests_its_actual_working_path_and_line() {
        use cosmic::iced::futures::StreamExt;
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::diff_snapshot_fixture(
            card,
            "source.rs",
            "old\nkept\n",
            "new\nkept\n",
        );
        let row = runtime
            .diff_model()
            .unwrap()
            .row_at_new_line(DiffMode::Inline, 2)
            .unwrap();
        let task = runtime.copy_diff(
            card,
            DiffCommand::OpenInEditor {
                row,
                side: Side::New,
            },
        );
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        let iced::runtime::Action::Output(RepositoryAction::OpenWorkingFileLocation {
            identity: request,
            line,
        }) = stream.next().await.unwrap()
        else {
            panic!("working file request")
        };
        assert_eq!(request, identity);
        assert_eq!(line, 1);
        assert!(
            runtime.shows_diff(),
            "the app chooses the working file owner without replacing the snapshot first"
        );
        assert!(runtime.diff_working_file_target(&request).is_some());
        runtime.generation += 1;
        assert!(runtime.diff_working_file_target(&request).is_none());
    }

    #[test]
    fn worker_model_capture_shares_rows_and_excludes_a_stale_display_fallback() {
        let (mut runtime,_,_) = fixture(DiffMode::Inline);
        let first = runtime.current_diff_model_snapshot().unwrap();
        let second = runtime.current_diff_model_snapshot().unwrap();
        assert!(Arc::ptr_eq(&first,&second));
        let basis = runtime.diff_basis.clone();
        let index = runtime.diffs.iter().position(|(candidate,_)| *candidate == basis).unwrap();
        let (_,state) = runtime.diffs.remove(index);
        runtime.stale_diff = Some((runtime.selected_path().unwrap().to_path_buf(),basis,state));
        assert!(runtime.diff_model().is_some());
        assert!(runtime.current_diff_model_snapshot().is_none());
        assert!(first.inline_rows.len()>1);
    }

    #[test]
    fn empty_diff_query_does_not_snapshot_rows_or_start_a_worker() {
        let (mut runtime, card, _) = fixture(DiffMode::Inline);
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        assert!(!runtime.diff_find_pending());
        assert!(runtime.diff_interaction.snapshot.is_none());
        assert!(runtime.diff_interaction.active.is_none());
    }

    #[test]
    fn unavailable_diff_search_does_not_report_a_permanent_pending_job() {
        let (mut runtime, card, _) = fixture(DiffMode::Inline);
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::ready(RenderedDiff::Error("File cannot be read".into())),
        );
        runtime.find_query = "needle".into();
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        assert!(!runtime.diff_find_pending());
        assert_eq!(runtime.find_total, 0);
        assert!(runtime.diff_interaction.active.is_none());
    }

    #[tokio::test]
    async fn closing_diff_find_releases_cancelled_worker_without_installing_results() {
        let (mut runtime, card, _) = fixture(DiffMode::Inline);
        runtime.find_query = "line".into();
        drop(runtime.open_find(card));
        let task = runtime.diff_find_task(card);
        assert!(runtime.diff_interaction.active.is_some());
        runtime.clear_find();
        drive(&mut runtime, task).await;
        assert!(runtime.diff_interaction.active.is_none());
        assert!(runtime.diff_interaction.matches.is_empty());
        assert!(!runtime.find_open);
    }

    #[tokio::test]
    async fn superseded_diff_search_releases_slot_and_installs_only_latest_query() {
        let (mut runtime, card, _) = fixture(DiffMode::Inline);
        runtime.find_query = "line".into();
        drop(runtime.open_find(card));
        let old = runtime.diff_find_task(card);
        runtime.find_query = "needle".into();
        drop(runtime.diff_find_task(card));
        drive(&mut runtime, old).await;
        assert_eq!(runtime.find_query, "needle");
        assert_eq!(runtime.find_total, 1);
        assert_eq!(runtime.diff_interaction.matches[0].range, 8..14);
        assert!(runtime.diff_interaction.active.is_none());
    }

    #[tokio::test]
    async fn invalid_regex_in_diff_is_reported_and_does_not_poison_next_search() {
        let (mut runtime, card, _) = fixture(DiffMode::Inline);
        runtime.find_query = "(".into();
        runtime.find_options.regex = true;
        drop(runtime.open_find(card));
        let task = runtime.diff_find_task(card);
        drive(&mut runtime, task).await;
        assert!(runtime.find_error.is_some());
        assert_eq!(runtime.find_total, 0);
        runtime.find_options.regex = false;
        runtime.find_query = "needle".into();
        let task = runtime.diff_find_task(card);
        drive(&mut runtime, task).await;
        assert!(runtime.find_error.is_none());
        assert_eq!(runtime.find_total, 1);
    }

    #[test]
    fn diff_row_widget_routes_shift_click_and_ends_drag_on_release() {
        use cosmic::iced::{
            Event, Pixels, Point, Rectangle, Size,
            advanced::{Layout, Shell, layout, renderer::Headless, widget::Tree},
        };
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                Font::MONOSPACE,
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let (runtime, card, _) = fixture(DiffMode::Split);
        let row = RowContext {
            card,
            runtime: &runtime,
            row: 10,
        };
        let content = row.wrap(
            Side::Old,
            iced::widget::Space::new().width(300.0).height(24.0).into(),
        );
        let mut element = Element::new(DiffScrollObserver {
            content,
            card: Some(card),
        });
        let mut tree = Tree::new(element.as_widget());
        let bounds = Rectangle::with_size(Size::new(300.0, 24.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let mut messages = Vec::new();
        for event in [
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::SHIFT,
            )),
            Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Available(Point::new(20.0, 12.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        assert!(messages.iter().any(|action| matches!(
            action,
            RepositoryAction::SelectDiffRow {
                row: 10,
                side: Side::Old,
                extend: true,
                context: false,
                drag: false,
                ..
            }
        )));
        assert!(messages.iter().any(|action| matches!(action, RepositoryAction::EndDiffSelection { card:actual } if *actual == card)));
    }

    #[test]
    fn diff_selection_is_side_specific_and_rejects_stale_row_events() {
        let (mut runtime, _, identity) = fixture(DiffMode::Split);
        let revision = runtime.diff_layout_revision;
        runtime.select_diff_row(
            identity.clone(),
            DiffBasis::HeadToIndex,
            DiffMode::Split,
            revision,
            10,
            Side::Old,
            false,
            false,
            false,
        );
        runtime.select_diff_row(
            identity.clone(),
            DiffBasis::HeadToIndex,
            DiffMode::Split,
            revision,
            12,
            Side::Old,
            true,
            false,
            false,
        );
        assert_eq!(runtime.selected_diff_range(), Some(10..13));
        assert!(runtime.diff_interaction.selection.unwrap().contains(
            DiffMode::Split,
            Side::Old,
            11
        ));
        assert!(!runtime.diff_interaction.selection.unwrap().contains(
            DiffMode::Split,
            Side::New,
            11
        ));
        runtime.generation += 1;
        runtime.select_diff_row(
            identity,
            DiffBasis::HeadToIndex,
            DiffMode::Split,
            revision,
            50,
            Side::New,
            false,
            false,
            false,
        );
        assert_eq!(runtime.selected_diff_range(), Some(10..13));
        drop(runtime.move_diff_selection(1, true));
        assert_eq!(runtime.selected_diff_range(), Some(10..14));
    }
}
