//! Capture immutable partial-change work without traversing rows on the UI thread.

use super::*;
use crate::git_patch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub identity: AsyncIdentity,
    pub basis: DiffBasis,
    pub mode: DiffMode,
    pub layout_revision: u64,
    pub rows: Range<usize>,
}

pub(crate) struct Capture {
    pub identity: AsyncIdentity,
    pub model: Arc<DiffModel>,
    pub kind: git_patch::Kind,
    pub mode: DiffMode,
    pub rows: Range<usize>,
    pub old_path: RepoPath,
    pub new_path: RepoPath,
    pub buffer_revision: Option<u64>,
    buffer_snapshot: Option<text_editor::TextSnapshot>,
}

impl Capture {
    pub fn build(self) -> Option<(AsyncIdentity, Option<u64>, git_patch::Patch)> {
        let mut patch = git_patch::selected_patch(&self.model, &self.old_path, &self.new_path,
            self.mode, self.rows, self.kind)?;
        if let Some(snapshot) = self.buffer_snapshot {
            if snapshot.text().as_ref() != self.model.after.as_ref() { return None; }
            let replacement = patch.working_text.take()?;
            patch.buffer_change = Some(Arc::new(buffer_change(&snapshot, replacement.as_str())?));
        }
        patch.working_text = None;
        Some((self.identity, self.buffer_revision, patch))
    }
}

impl RepositoryRuntime {
    /// A gutter action names the exact row coordinates that were rendered.
    pub fn diff_mutation_request(&self, card: CardId, rows: Range<usize>) -> Option<Arc<Request>> {
        let path = self.selected_path()?.to_owned();
        Some(Arc::new(Request { identity: self.current_identity(card, path), basis: self.diff_basis.clone(),
            mode: self.diff_mode, layout_revision: self.diff_layout_revision, rows }))
    }

    fn can_mutate_diff(&self, kind: git_patch::Kind) -> bool {
        if !self.shows_diff() || self.directory_only || self.mutation_refresh_pending
            || self.blocks_navigation() || self.basis_resolution.is_some()
            || self.editable_working_diff() && !self.working_annotations_current()
        { return false; }
        match (kind, &self.diff_basis) {
            (git_patch::Kind::Stage | git_patch::Kind::Revert, DiffBasis::IndexToWorktree)
            | (git_patch::Kind::Unstage, DiffBasis::HeadToIndex) => {},
            _ => return false,
        }
        let Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) = self.diff_state(&self.diff_basis) else { return false; };
        !model.is_empty() && (kind == git_patch::Kind::Unstage) == model.source_head.is_some()
            && model.present.into_iter().zip(model.modes).all(|(present, mode)| !present || matches!(mode, Some(0o100644 | 0o100755)))
    }

    pub(crate) fn capture_diff_mutation(
        &self,
        card: CardId,
        kind: git_patch::Kind,
        request: Option<&Request>,
    ) -> Option<Capture> {
        if !self.can_mutate_diff(kind) { return None; }
        if let Some(request) = request
            && (!self.accepts_selected(&request.identity) || request.identity.card != card
                || request.basis != self.diff_basis || request.mode != self.diff_mode
                || request.layout_revision != self.diff_layout_revision)
        { return None; }
        let model = self.current_diff_model_snapshot()?;
        let rows = match request {
            Some(request) => request.rows.clone(),
            None => self.selected_diff_range().or_else(|| {
                let starts = model.change_starts(self.diff_mode);
                let start = *starts.get(self.diff_change)?;
                Some(start..starts.get(self.diff_change + 1).copied().unwrap_or_else(|| model.row_count(self.diff_mode)))
            })?,
        };
        if rows.is_empty() || rows.end > model.row_count(self.diff_mode) { return None; }
        let path = self.selected_path()?.to_owned();
        let file = self.subject_file()?;
        let old_path = file.previous_path(&self.diff_basis).unwrap_or_else(|| path.clone());
        let (buffer_revision, buffer_snapshot) = if kind == git_patch::Kind::Revert && self.is_dirty() {
            match &self.document { DocumentState::Editable(document) => (Some(document.revision), Some(document.content.text_snapshot())), _ => return None }
        } else { (None, None) };
        Some(Capture { identity: self.current_identity(card, path.clone()), model, kind,
            mode: self.diff_mode, rows, old_path: RepoPath::parse(old_path).ok()?,
            new_path: RepoPath::parse(path).ok()?, buffer_revision, buffer_snapshot })
    }

    /// A delayed revert must not overwrite typing performed while its patch
    /// was prepared. The worker measured one native replacement; applying it
    /// touches only that span and records the original caret for one undo step.
    pub(crate) fn apply_diff_buffer_revert(
        &mut self,
        identity: &AsyncIdentity,
        revision: u64,
        change: Arc<TextChange>,
    ) -> Result<(), String> {
        if !self.accepts_selected(identity) || self.is_saving() { return Err("The file changed while Revert was prepared. Try again.".into()); }
        let DocumentState::Editable(document) = &mut self.document else { return Err("The editable file is no longer open.".into()); };
        if document.revision != revision { return Err("The buffer changed while Revert was prepared. Try again.".into()); }
        let cursor = document.content.cursor();
        let scroll = document.content.scroll_position();
        let mut buffer = ProtocolTracked { content: &mut document.content, edits: Vec::new() };
        if !buffer.replace(change.start, &change.removed, &change.inserted, &change.inserted_breaks) {
            return Err("The selected change no longer matches the buffer. Try again.".into());
        }
        let edits = buffer.edits;
        document.content.set_cursor(cursor);
        document.content.restore_scroll_position(scroll);
        document.history.record_group(vec![Arc::unwrap_or_clone(change)], cursor, document.content.cursor(), Instant::now());
        document.text_changed();
        self.push_lsp_edits(edits);
        Ok(())
    }
}

/// Preserve the native line map in the removed fragments, including adjacent
/// CR/LF endings retained by earlier edits. CPU scanning and text copies stay
/// on the worker; the application receives only the minimal replacement.
fn buffer_change(snapshot: &text_editor::TextSnapshot, replacement: &str) -> Option<TextChange> {
    let mut before = crate::editor_history::Excerpt::new(0);
    for index in 0..snapshot.line_count() {
        let line = snapshot.line(index)?;
        before.push_line(&line.text, line.ending.as_str());
    }
    crate::editor_history::replacement(&before, &crate::editor_history::Excerpt::document(replacement), usize::MAX)
}

pub(super) const GUTTER_WIDTH: f32 = 44.0;

pub(super) fn local_basis(runtime: &RepositoryRuntime) -> bool {
    !runtime.directory_only && matches!(runtime.diff_basis, DiffBasis::IndexToWorktree | DiffBasis::HeadToIndex)
}

pub(super) fn selection_controls<'a>(card: CardId, runtime: &'a RepositoryRuntime) -> Element<'a, RepositoryAction> {
    let selection = runtime.has_diff_selection();
    let staged = matches!(runtime.diff_basis, DiffBasis::HeadToIndex);
    let primary = if staged { git_patch::Kind::Unstage } else { git_patch::Kind::Stage };
    let label = match (staged, selection) {
        (true, true) => "Unstage selected lines", (true, false) => "Unstage current hunk",
        (false, true) => "Stage selected lines", (false, false) => "Stage current hunk",
    };
    let mut controls = vec![crate::ui_controls::button::secondary(label)
        .on_press_maybe(runtime.can_mutate_diff(primary).then_some(RepositoryAction::ApplyDiffSelection { card, kind: primary })).into()];
    if !staged {
        controls.push(crate::ui_controls::button::quiet(if selection { "Revert selected lines" } else { "Revert current hunk" })
            .on_press_maybe(runtime.can_mutate_diff(git_patch::Kind::Revert).then_some(RepositoryAction::ApplyDiffSelection { card, kind: git_patch::Kind::Revert })).into());
    }
    widget::row::with_children(controls).spacing(space::XS).padding([space::XS, space::S]).width(Length::Fill).into()
}

/// Hunk buttons occupy their own fixed gutter; line numbers stay readable and
/// no extra row changes the virtual list's measured offsets.
pub(super) fn gutter<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    model: &DiffModel,
    row: usize,
    height: f32,
) -> Element<'a, RepositoryAction> {
    let starts = model.change_starts(runtime.diff_mode);
    let Ok(hunk) = starts.binary_search(&row) else {
        return iced::widget::Space::new().width(GUTTER_WIDTH).height(height).into();
    };
    let range = row..starts.get(hunk + 1).copied().unwrap_or_else(|| model.row_count(runtime.diff_mode));
    let Some(request) = runtime.diff_mutation_request(card, range) else {
        return iced::widget::Space::new().width(GUTTER_WIDTH).height(height).into();
    };
    let staged = matches!(runtime.diff_basis, DiffBasis::HeadToIndex);
    let primary = if staged { git_patch::Kind::Unstage } else { git_patch::Kind::Stage };
    let button = |kind, icon: Icon, label: &'static str| {
        let enabled = runtime.can_mutate_diff(kind);
        let control = widget::button::custom(icon.handle().icon().size(12))
            .name(label).class(crate::ui_controls::control_style(false, None))
            .padding(0).width(20.0).height(height.min(20.0))
            .on_press_maybe(enabled.then(|| RepositoryAction::ApplyDiffHunk { card, kind, request: request.clone() }));
        crate::interaction::tooltip(control, label)
    };
    let mut controls = vec![button(primary, if staged { Icon::Minus } else { Icon::Plus }, if staged { "Unstage this hunk" } else { "Stage this hunk" })];
    if !staged { controls.push(button(git_patch::Kind::Revert, Icon::RevertFile, "Revert this hunk")); }
    widget::container(widget::row::with_children(controls).spacing(2.0))
        .center_y(height).width(GUTTER_WIDTH).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_change_buffer_revert_is_one_undo_step_with_exact_crlf_and_caret() {
        let card = CardId::new();
        let after = "first\r\nCHANGE\r\nkeep another hunk\r\nno final newline";
        let desired = "first\r\noriginal\r\nkeep another hunk\r\nno final newline";
        let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "file.txt", after);
        let DocumentState::Editable(document) = &mut runtime.document else { panic!("expected editable fixture"); };
        let cursor = text_editor::Cursor { position: text_editor::Position { line: 2, column: 4 }, selection: None };
        document.content.set_cursor(cursor);
        let revision = document.revision;
        let change = buffer_change(&document.content.text_snapshot(), desired).unwrap();
        assert_eq!(change.start.line, 1);
        assert!(!change.removed.contains("first") && !change.removed.contains("another hunk"));
        assert!(!document.history.can_undo());
        runtime.apply_diff_buffer_revert(&identity, revision, Arc::new(change)).unwrap();
        let DocumentState::Editable(document) = &mut runtime.document else { unreachable!(); };
        assert_eq!(document.content.text(), desired);
        assert_eq!(document.content.cursor(), cursor);
        assert!(document.step_history(false).is_some());
        assert_eq!(document.content.text(), after);
        assert_eq!(document.content.cursor(), cursor);
        assert!(!document.history.can_undo());
        assert!(document.step_history(true).is_some());
        assert_eq!(document.content.text(), desired);
    }

    #[test]
    fn partial_change_buffer_revert_refuses_later_typing_without_losing_it() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "file.txt", "changed\n");
        let DocumentState::Editable(document) = &mut runtime.document else { unreachable!(); };
        let revision = document.revision;
        let change = buffer_change(&document.content.text_snapshot(), "original\n").unwrap();
        document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('!')));
        let typed = document.content.text();
        assert!(runtime.apply_diff_buffer_revert(&identity, revision, Arc::new(change)).is_err());
        assert_eq!(runtime.editor_text().unwrap(), typed);
    }
}
