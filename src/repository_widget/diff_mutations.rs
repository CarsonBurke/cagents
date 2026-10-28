//! Capture immutable partial-change work without traversing rows on the UI thread.

use super::*;
use crate::git_patch;

#[derive(Clone)]
pub struct Request {
    pub identity: AsyncIdentity,
    pub basis: DiffBasis,
    pub mode: DiffMode,
    pub layout_revision: u64,
    pub rows: Range<usize>,
    model: Arc<DiffModel>,
    selection: Option<crate::diff_interaction::Selection>,
    buffer_revision: Option<u64>,
    old_path: RepoPath,
    new_path: RepoPath,
}

// Retain the source allocation so it cannot be reused while a callback or held
// gesture names it. Event equality must not traverse the document's diff rows.
impl PartialEq for Request {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.model, &other.model)
            && self.identity == other.identity
            && self.basis == other.basis
            && self.mode == other.mode
            && self.layout_revision == other.layout_revision
            && self.rows == other.rows
            && self.selection == other.selection
            && self.buffer_revision == other.buffer_revision
            && self.old_path == other.old_path
            && self.new_path == other.new_path
    }
}

impl Eq for Request {}

impl std::fmt::Debug for Request {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("identity", &self.identity)
            .field("basis", &self.basis)
            .field("mode", &self.mode)
            .field("layout_revision", &self.layout_revision)
            .field("rows", &self.rows)
            .field("model", &Arc::as_ptr(&self.model))
            .field("selection", &self.selection)
            .field("buffer_revision", &self.buffer_revision)
            .field("old_path", &self.old_path)
            .field("new_path", &self.new_path)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ControlOwner {
    kind: git_patch::Kind,
    request: Arc<Request>,
}

fn control_owner(action: &Option<RepositoryAction>) -> Option<ControlOwner> {
    match action {
        Some(RepositoryAction::ApplyDiffHunk { kind, request, .. }) => Some(ControlOwner {
            kind: *kind,
            request: Arc::clone(request),
        }),
        _ => None,
    }
}

pub struct Capture {
    pub(crate) identity: AsyncIdentity,
    pub(crate) model: Arc<DiffModel>,
    pub(crate) kind: git_patch::Kind,
    pub(crate) mode: DiffMode,
    pub(crate) rows: Range<usize>,
    pub(crate) old_path: RepoPath,
    pub(crate) new_path: RepoPath,
    pub(crate) buffer_revision: Option<u64>,
    basis: DiffBasis,
    layout_revision: u64,
    selection: Option<crate::diff_interaction::Selection>,
    buffer_snapshot: Option<text_editor::TextSnapshot>,
}

/// A host receives an opaque prepared edit. Its source model, selection,
/// buffer revision and native Undo metadata remain owned by this runtime.
#[derive(Debug, Clone)]
pub struct Prepared {
    identity: AsyncIdentity,
    model: Arc<DiffModel>,
    kind: git_patch::Kind,
    basis: DiffBasis,
    mode: DiffMode,
    layout_revision: u64,
    selection: Option<crate::diff_interaction::Selection>,
    buffer_revision: Option<u64>,
    patch: git_patch::Patch,
}

impl Prepared {
    pub fn identity(&self) -> &AsyncIdentity { &self.identity }

    pub fn applies_to_buffer(&self) -> bool { self.buffer_revision.is_some() }

    /// Cached Stage reads captured live text and leaves disk/buffers intact.
    pub fn stages_live_buffer(&self) -> bool { self.kind == git_patch::Kind::Stage }

    /// At the owned queue turn, refuse any change of the rendered source.
    /// Buffer Revert is one native Undo transaction; disk effects are returned
    /// to the host's existing ordered Git/recovery pipeline.
    pub fn apply(self, runtime: &mut RepositoryRuntime) -> Result<Option<crate::repository::GitOperation>, String> {
        let current = runtime.current_diff_model_snapshot();
        if !runtime.accepts_selected(&self.identity)
            || runtime.diff_basis != self.basis || runtime.diff_mode != self.mode
            || runtime.diff_layout_revision != self.layout_revision
            || runtime.diff_interaction.selection != self.selection
            || !runtime.can_mutate_diff(self.kind)
            || !current.is_some_and(|model| Arc::ptr_eq(&model, &self.model))
        { return Err("The selected diff changed while the action was prepared. Try again.".into()); }
        let mut patch = self.patch;
        if let Some(revision) = self.buffer_revision {
            let present = patch.working_present.ok_or("The selected change has no working-file presence state")?;
            runtime.apply_diff_buffer_revert_with_presence(&self.identity, revision, patch.buffer_change.take(), present, patch.working_mode)?;
            Ok(None)
        } else {
            Ok(Some(crate::repository::GitOperation::ApplyPatch {
                patch: patch.patch, cached: patch.cached, reverse: patch.reverse, expected_head: patch.expected_head,
            }))
        }
    }
}

impl Capture {
    /// This capture edits only the open document through native Undo.
    pub fn applies_to_buffer(&self) -> bool { self.buffer_revision.is_some() }

    /// Build patch bytes and any minimal native buffer replacement off thread.
    pub fn prepare(self) -> Option<Prepared> {
        let model = self.model.clone();
        let kind = self.kind;
        let basis = self.basis.clone();
        let mode = self.mode;
        let layout_revision = self.layout_revision;
        let selection = self.selection;
        let (identity, buffer_revision, patch) = self.build()?;
        Some(Prepared { identity, model, kind, basis, mode, layout_revision, selection, buffer_revision, patch })
    }

    pub fn build(self) -> Option<(AsyncIdentity, Option<u64>, git_patch::Patch)> {
        let mut patch = git_patch::selected_patch(
            &self.model,
            &self.old_path,
            &self.new_path,
            self.mode,
            self.rows,
            self.kind,
        )?;
        if let Some(snapshot) = self.buffer_snapshot {
            let before = snapshot.text();
            if before.as_ref() != self.model.after.as_ref() {
                return None;
            }
            let replacement = patch.working_text.take()?;
            patch.buffer_change =
                buffer_change(&snapshot, &before, replacement.as_str()).map(Arc::new);
            if patch.buffer_change.is_none() && before.as_ref() != replacement.as_str() {
                return None;
            }
        }
        patch.working_text = None;
        Some((self.identity, self.buffer_revision, patch))
    }
}

impl RepositoryRuntime {
    fn diff_mutation_paths(&self) -> Option<(RepoPath, RepoPath)> {
        let file = self.subject_file()?;
        let old_path = file
            .previous_path(&self.diff_basis)
            .unwrap_or_else(|| file.path.clone());
        Some((
            RepoPath::parse(old_path).ok()?,
            RepoPath::parse(file.path).ok()?,
        ))
    }

    /// A gutter action names the exact source and row coordinates rendered.
    pub fn diff_mutation_request(&self, card: CardId, rows: Range<usize>) -> Option<Arc<Request>> {
        let path = self.selected_path()?.to_owned();
        let model = self.current_diff_model_snapshot()?;
        let (old_path, new_path) = self.diff_mutation_paths()?;
        let buffer_revision = match &self.document {
            DocumentState::Editable(document) => Some(document.revision),
            _ => None,
        };
        Some(Arc::new(Request {
            identity: self.current_identity(card, path),
            basis: self.diff_basis.clone(),
            mode: self.diff_mode,
            layout_revision: self.diff_layout_revision,
            rows,
            model,
            selection: self.diff_interaction.selection,
            buffer_revision,
            old_path,
            new_path,
        }))
    }

    #[cfg(test)]
    pub(crate) fn selected_diff_fixture(card: CardId) -> Self {
        let mut runtime = tests::selected_mutation_fixture(card);
        runtime.diff_interaction.selection = Some(crate::diff_interaction::Selection::new(
            runtime.diff_mode,
            crate::diff_interaction::Side::New,
            0,
        ));
        runtime
    }

    /// Selection commands require actual selected rows. Their captured request
    /// keeps palette activation bound to the rendered source and coordinates.
    pub fn can_apply_diff_selection(&self, kind: git_patch::Kind) -> bool {
        self.selected_mutation_rows(kind).is_some()
    }

    pub fn diff_selection_mutation_action(
        &self,
        card: CardId,
        kind: git_patch::Kind,
    ) -> Option<RepositoryAction> {
        let rows = self.selected_mutation_rows(kind)?;
        let request = self.diff_mutation_request(card, rows)?;
        Some(RepositoryAction::ApplyDiffHunk {
            card,
            kind,
            request,
        })
    }

    fn diff_mutation_control_action(
        &self,
        card: CardId,
        kind: git_patch::Kind,
    ) -> Option<RepositoryAction> {
        if self.has_diff_selection() {
            return self.diff_selection_mutation_action(card, kind);
        }
        self.diff_hunk_mutation_action(card, kind)
    }

    /// Current-hunk commands never substitute for an explicit selected range.
    pub fn can_apply_diff_hunk(&self, kind: git_patch::Kind) -> bool {
        self.current_hunk_mutation_rows(kind).is_some()
    }

    pub fn diff_hunk_mutation_action(
        &self,
        card: CardId,
        kind: git_patch::Kind,
    ) -> Option<RepositoryAction> {
        let rows = self.current_hunk_mutation_rows(kind)?;
        let request = self.diff_mutation_request(card, rows)?;
        Some(RepositoryAction::ApplyDiffHunk {
            card,
            kind,
            request,
        })
    }

    fn current_hunk_mutation_rows(&self, kind: git_patch::Kind) -> Option<Range<usize>> {
        if self.has_diff_selection() || !self.can_mutate_diff(kind) {
            return None;
        }
        let model = self.current_diff_model_snapshot()?;
        let starts = model.change_starts(self.diff_mode);
        let start = *starts.get(self.diff_change)?;
        let end = starts
            .get(self.diff_change + 1)
            .copied()
            .unwrap_or_else(|| model.row_count(self.diff_mode));
        (start < end).then_some(start..end)
    }

    fn selected_mutation_rows(&self, kind: git_patch::Kind) -> Option<Range<usize>> {
        if !self.can_mutate_diff(kind) {
            return None;
        }
        let rows = self.selected_diff_range()?;
        let model = self.current_diff_model_snapshot()?;
        (!rows.is_empty()
            && rows.end <= model.row_count(self.diff_mode)
            && self.diff_interaction.selection?.has_changes(&model))
        .then_some(rows)
    }

    fn can_mutate_diff(&self, kind: git_patch::Kind) -> bool {
        // A hunk edit keeps the current document open. Unsaved working
        // text is a valid source while its annotations match that revision;
        // saving and unresolved durability still fence the operation.
        let buffer_revert = kind == git_patch::Kind::Revert && self.editable_working_diff();
        if self.git_mutation_unavailable_reason.is_some() && !buffer_revert
            || !self.shows_diff()
            || self.directory_only
            || self.mutation_refresh_pending
            || self.is_saving()
            || self.has_durability_warning()
            || self.basis_resolution.is_some()
            || kind == git_patch::Kind::Unstage && self.is_dirty()
            || self.editable_working_diff() && !self.working_annotations_current()
        {
            return false;
        }
        match (kind, &self.diff_basis) {
            (git_patch::Kind::Stage | git_patch::Kind::Revert, DiffBasis::IndexToWorktree)
            | (git_patch::Kind::Unstage, DiffBasis::HeadToIndex) => {}
            _ => return false,
        }
        let Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) =
            self.diff_state(&self.diff_basis)
        else {
            return false;
        };
        !model.is_empty()
            && (kind == git_patch::Kind::Unstage) == model.source_head.is_some()
            && model
                .present
                .into_iter()
                .zip(model.modes)
                .all(|(present, mode)| !present || matches!(mode, Some(0o100644 | 0o100755)))
    }

    pub fn capture_diff_mutation(
        &self,
        card: CardId,
        kind: git_patch::Kind,
        request: Option<&Request>,
    ) -> Option<Capture> {
        if !self.can_mutate_diff(kind) {
            return None;
        }
        let model = self.current_diff_model_snapshot()?;
        let (old_path, new_path) = self.diff_mutation_paths()?;
        let buffer_revision = match &self.document {
            DocumentState::Editable(document) => Some(document.revision),
            _ => None,
        };
        if let Some(request) = request
            && (!self.accepts_selected(&request.identity)
                || request.identity.card != card
                || request.basis != self.diff_basis
                || request.mode != self.diff_mode
                || request.layout_revision != self.diff_layout_revision
                || !Arc::ptr_eq(&request.model, &model)
                || request.selection != self.diff_interaction.selection
                || request.buffer_revision != buffer_revision
                || request.old_path != old_path
                || request.new_path != new_path)
        {
            return None;
        }
        let rows = match request {
            Some(request) => request.rows.clone(),
            None => self.selected_diff_range().or_else(|| {
                let starts = model.change_starts(self.diff_mode);
                let start = *starts.get(self.diff_change)?;
                Some(
                    start
                        ..starts
                            .get(self.diff_change + 1)
                            .copied()
                            .unwrap_or_else(|| model.row_count(self.diff_mode)),
                )
            })?,
        };
        if rows.is_empty() || rows.end > model.row_count(self.diff_mode) {
            return None;
        }
        let path = self.selected_path()?.to_owned();
        let (old_path, new_path) = match request {
            Some(request) => (request.old_path.clone(), request.new_path.clone()),
            None => (old_path, new_path),
        };
        let (buffer_revision, buffer_snapshot) = if kind == git_patch::Kind::Revert {
            match &self.document {
                DocumentState::Editable(document) => (
                    Some(document.revision),
                    Some(document.content.text_snapshot()),
                ),
                _ => (None, None),
            }
        } else {
            (None, None)
        };
        Some(Capture {
            identity: self.current_identity(card, path.clone()),
            model,
            kind,
            mode: self.diff_mode,
            rows,
            old_path,
            new_path,
            buffer_revision,
            buffer_snapshot,
            basis: self.diff_basis.clone(),
            layout_revision: self.diff_layout_revision,
            selection: self.diff_interaction.selection,
        })
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
        self.apply_diff_buffer_revert_with_presence(identity, revision, Some(change), true, None)
    }

    pub(crate) fn apply_diff_buffer_revert_with_presence(
        &mut self,
        identity: &AsyncIdentity,
        revision: u64,
        change: Option<Arc<TextChange>>,
        present: bool,
        mode: Option<u32>,
    ) -> Result<(), String> {
        if !self.accepts_selected(identity) || self.is_saving() {
            return Err("The file changed while Revert was prepared. Try again.".into());
        }
        let DocumentState::Editable(document) = &mut self.document else {
            return Err("The editable file is no longer open.".into());
        };
        if document.revision != revision {
            return Err("The buffer changed while Revert was prepared. Try again.".into());
        }
        if !present {
            let replaces_all = change.as_ref().is_some_and(|change| {
                change.start == (text_editor::Position { line: 0, column: 0 })
                    && change.inserted.is_empty()
                    && document.content.text_len() == change.removed.len()
            });
            if !matches!(mode, Some(0o100644 | 0o100755))
                || (!document.content.is_empty() && !replaces_all)
            {
                return Err(
                    "The complete current file and its mode are required before removing it."
                        .into(),
                );
            }
        }
        let presence_id = if !present {
            let id = document
                .next_presence_id
                .checked_add(1)
                .ok_or("The file's Undo identity is exhausted. Reload before reverting.")?;
            document.next_presence_id = id;
            id
        } else {
            document.presence_id
        };
        let cursor = document.content.cursor();
        let scroll = document.content.scroll_position();
        let mut buffer = ProtocolTracked {
            content: &mut document.content,
            edits: Vec::new(),
        };
        if let Some(change) = &change
            && !buffer.replace(
                change.start,
                &change.removed,
                &change.inserted,
                &change.inserted_breaks,
            )
        {
            return Err("The selected change no longer matches the buffer. Try again.".into());
        }
        let edits = buffer.edits;
        document.content.set_cursor(cursor);
        document.content.restore_scroll_position(scroll);
        document.history.record_group_with_presence_identities(
            change.into_iter().map(Arc::unwrap_or_clone).collect(),
            cursor,
            document.content.cursor(),
            Instant::now(),
            Some(((document.present, presence_id), (present, presence_id))),
        );
        document.present = present;
        document.presence_id = presence_id;
        if !present {
            document.deletion_mode = mode;
        }
        document.text_changed();
        self.push_lsp_edits(edits);
        Ok(())
    }
}

/// Preserve the native line map in the removed fragments, including adjacent
/// CR/LF endings retained by earlier edits. CPU scanning and text copies stay
/// on the worker; the application receives only the minimal replacement.
fn buffer_change(
    snapshot: &text_editor::TextSnapshot,
    old: &str,
    replacement: &str,
) -> Option<TextChange> {
    if old == replacement {
        return None;
    }
    let mut prefix = old
        .bytes()
        .zip(replacement.bytes())
        .take_while(|(old, new)| old == new)
        .count();
    while !old.is_char_boundary(prefix) || !replacement.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let suffix = old[prefix..]
        .bytes()
        .rev()
        .zip(replacement[prefix..].bytes().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let offset = |line| snapshot.byte_offset(text_editor::Position { line, column: 0 });
    let line_at = |byte| {
        let (mut low, mut high) = (0, snapshot.line_count());
        while low < high {
            let mid = low + (high - low) / 2;
            if offset(mid)? <= byte {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Some(low.saturating_sub(1))
    };
    // Keep one neighbouring native line on either side. In particular, a
    // preceding CR must not be accidentally paired with a newly inserted LF.
    // The editor's noncanonical-ending fallback then stays local to this edit.
    let first = line_at(prefix)?.saturating_sub(1);
    let end = line_at(old.len() - suffix)?
        .saturating_add(2)
        .min(snapshot.line_count());
    let start_byte = offset(first)?;
    let end_byte = if end == snapshot.line_count() {
        old.len()
    } else {
        offset(end)?
    };
    let new_end = replacement.len().checked_sub(old.len() - end_byte)?;
    let mut before = crate::editor_history::Excerpt::new(0);
    for index in first..end {
        let line = snapshot.line(index)?;
        before.push_line(&line.text, line.ending.as_str());
    }
    let after = crate::editor_history::Excerpt::document(replacement.get(start_byte..new_end)?);
    let mut change = crate::editor_history::replacement(&before, &after, usize::MAX)?;
    change.start.line += first;
    Some(change)
}

pub(super) const GUTTER_WIDTH: f32 = 44.0;

pub(super) fn local_basis(runtime: &RepositoryRuntime) -> bool {
    !runtime.directory_only
        && matches!(
            runtime.diff_basis,
            DiffBasis::IndexToWorktree | DiffBasis::HeadToIndex
        )
}

pub(super) fn selection_controls<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
) -> Element<'a, RepositoryAction> {
    let selection = runtime.has_diff_selection();
    let staged = matches!(runtime.diff_basis, DiffBasis::HeadToIndex);
    let primary = if staged {
        git_patch::Kind::Unstage
    } else {
        git_patch::Kind::Stage
    };
    let label = match (staged, selection) {
        (true, true) => "Unstage selected lines",
        (true, false) => "Unstage current hunk",
        (false, true) => "Stage selected lines",
        (false, false) => "Stage current hunk",
    };
    let action = runtime.diff_mutation_control_action(card, primary);
    let owner = control_owner(&action);
    let button = crate::ui_controls::owned(
        owner,
        crate::ui_controls::button::secondary(label).on_press_maybe(action),
    );
    let command = crate::workspace_widget::diff_mutation_registry_command(primary, selection);
    let mut controls = vec![crate::interaction::tooltip(
        button,
        registered_command_hint(runtime, command, label),
    )];
    if !staged {
        let label = if selection {
            "Revert selected lines"
        } else {
            "Revert current hunk"
        };
        let action = runtime.diff_mutation_control_action(card, git_patch::Kind::Revert);
        let owner = control_owner(&action);
        let button = crate::ui_controls::owned(
            owner,
            crate::ui_controls::button::quiet(label).on_press_maybe(action),
        );
        let command = crate::workspace_widget::diff_mutation_registry_command(
            git_patch::Kind::Revert,
            selection,
        );
        controls.push(crate::interaction::tooltip(
            button,
            registered_command_hint(runtime, command, label),
        ));
    }
    widget::row::with_children(controls)
        .spacing(space::XS)
        .padding([space::XS, space::S])
        .width(Length::Fill)
        .into()
}

/// Hunk buttons occupy their own fixed gutter; line numbers stay readable and
/// no extra row changes the virtual list's measured offsets.
pub(super) fn gutter<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    offer: Option<&Arc<Request>>,
    model: &DiffModel,
    row: usize,
    height: f32,
) -> Element<'a, RepositoryAction> {
    let starts = model.change_starts(runtime.diff_mode);
    let Ok(hunk) = starts.binary_search(&row) else {
        return iced::widget::Space::new()
            .width(GUTTER_WIDTH)
            .height(height)
            .into();
    };
    let range = row..starts
        .get(hunk + 1)
        .copied()
        .unwrap_or_else(|| model.row_count(runtime.diff_mode));
    let Some(offer) = offer.filter(|offer| std::ptr::eq(model, offer.model.as_ref())) else {
        return iced::widget::Space::new()
            .width(GUTTER_WIDTH)
            .height(height)
            .into();
    };
    // One render-pass offer resolves the followed Git target once. Each
    // visible hunk only clones bounded metadata and its immutable source Arc.
    let mut request = (**offer).clone();
    request.rows = range;
    let request = Arc::new(request);
    let staged = matches!(runtime.diff_basis, DiffBasis::HeadToIndex);
    let primary = if staged {
        git_patch::Kind::Unstage
    } else {
        git_patch::Kind::Stage
    };
    let button = |kind, icon: Icon, label: &'static str| {
        let action = runtime
            .can_mutate_diff(kind)
            .then(|| RepositoryAction::ApplyDiffHunk {
                card,
                kind,
                request: request.clone(),
            });
        let owner = control_owner(&action);
        let control = widget::button::custom(icon.handle().icon().size(12))
            .name(label)
            .class(crate::ui_controls::control_style(false, None))
            .padding(0)
            .width(20.0)
            .height(height.min(20.0))
            .on_press_maybe(action);
        crate::interaction::tooltip(crate::ui_controls::owned(owner, control), label)
    };
    let mut controls = vec![button(
        primary,
        if staged { Icon::Minus } else { Icon::Plus },
        if staged {
            "Unstage this hunk"
        } else {
            "Stage this hunk"
        },
    )];
    if !staged {
        controls.push(button(
            git_patch::Kind::Revert,
            Icon::RevertFile,
            "Revert this hunk",
        ));
    }
    widget::container(widget::row::with_children(controls).spacing(2.0))
        .center_y(height)
        .width(GUTTER_WIDTH)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn finish_presence_save(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
        use cosmic::iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("save task");
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(action) = action {
                drop(runtime.update(action));
            }
        }
    }

    fn added_file_runtime(
        card: CardId,
        root: &Path,
        text: &str,
    ) -> (RepositoryRuntime, AsyncIdentity) {
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "new.sh", text);
        runtime.descriptor.worktree = root.to_owned();
        runtime.source_surface = SourceSurface::Diff;
        let mut model = crate::diff_view::build_diff("", text);
        model.present = [false, true];
        model.modes = [None, Some(0o100755)];
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(model),
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            }),
        );
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
        runtime.working_diff.applied_rich = true;
        runtime.working_diff.desired_rich = true;
        let identity = runtime.current_identity(card, "new.sh".into());
        (runtime, identity)
    }

    fn revert_complete_added_buffer(runtime: &mut RepositoryRuntime, card: CardId, mode: u32) {
        runtime.source_surface = SourceSurface::Diff;
        let text = runtime.editor_text().unwrap();
        let mut model = crate::diff_view::build_diff("", &text);
        model.present = [false, true];
        model.modes = [None, Some(mode)];
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(model),
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            }),
        );
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
        runtime.working_diff.applied_rich = true;
        runtime.working_diff.desired_rich = true;
        let model = runtime.current_diff_model_snapshot().unwrap();
        let request = runtime
            .diff_mutation_request(card, 0..model.row_count(runtime.diff_mode))
            .unwrap();
        let (identity, revision, mut patch) = runtime
            .capture_diff_mutation(card, git_patch::Kind::Revert, Some(&request))
            .unwrap()
            .build()
            .unwrap();
        runtime
            .apply_diff_buffer_revert_with_presence(
                &identity,
                revision.unwrap(),
                patch.buffer_change.take(),
                patch.working_present.unwrap(),
                patch.working_mode,
            )
            .unwrap();
    }

    fn initialize_presence_fixture(root: &Path) {
        let initialized = std::process::Command::new("git")
            .args([
                "-c",
                "core.fsmonitor=false",
                "init",
                "--template=",
                "-b",
                "main",
            ])
            .arg(&root)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .output()
            .unwrap();
        assert!(
            initialized.status.success(),
            "{}",
            String::from_utf8_lossy(&initialized.stderr)
        );
    }

    async fn finish_lineage_save(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
        use cosmic::iced::futures::StreamExt;
        let mut pending = std::collections::VecDeque::from([task]);
        let mut completions = 0;
        while let Some(task) = pending.pop_front() {
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                continue;
            };
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(action) = action {
                    completions += 1;
                    assert!(completions < 100, "fixture refresh must settle");
                    // Settle the real file-list/Git refresh produced by the
                    // save. Autosave timing belongs to its separate contract.
                    pending.push_back(runtime.update_inner(action));
                }
            }
        }
    }

    #[tokio::test]
    async fn partial_change_multiple_presence_undos_restore_their_own_access_metadata() {
        use std::os::unix::fs::PermissionsExt;
        for (save_between_undos, late_completion) in [(true, false), (false, false), (false, true)]
        {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            initialize_presence_fixture(&root);
            std::fs::write(root.join("new.sh"), "synthetic alpha\n").unwrap();
            std::fs::set_permissions(root.join("new.sh"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let card = CardId::new();
            let (mut runtime, _) = added_file_runtime(card, &root, "synthetic alpha\n");
            revert_complete_added_buffer(&mut runtime, card, 0o100644);
            let task = runtime.request_save_inner(card, false, false);
            finish_lineage_save(&mut runtime, task).await;
            assert!(!root.join("new.sh").exists());
            let DocumentState::Editable(document) = &mut runtime.document else {
                unreachable!()
            };
            document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new("synthetic beta\n".into()),
            )));
            let task = runtime.request_save_inner(card, false, false);
            finish_lineage_save(&mut runtime, task).await;
            std::fs::set_permissions(root.join("new.sh"), std::fs::Permissions::from_mode(0o644))
                .unwrap();
            revert_complete_added_buffer(&mut runtime, card, 0o100644);
            let task = runtime.request_save_inner(card, false, false);
            if late_completion {
                // The captured deletion belongs to B even after Undo has
                // moved the current history back to A's absence identity.
                drop(runtime.update(RepositoryAction::Undo { card }));
                drop(runtime.update(RepositoryAction::Undo { card }));
                finish_lineage_save(&mut runtime, task).await;
                let DocumentState::Editable(document) = &runtime.document else {
                    unreachable!()
                };
                assert_eq!(document.restorations.len(), 2);
                assert_ne!(document.presence_id, document.saved_presence_id);
                assert!(runtime.is_dirty());
            } else {
                finish_lineage_save(&mut runtime, task).await;
                drop(runtime.update(RepositoryAction::Undo { card }));
                let task = runtime.request_save_inner(card, false, false);
                finish_lineage_save(&mut runtime, task).await;
                assert_eq!(
                    std::fs::read_to_string(root.join("new.sh")).unwrap(),
                    "synthetic beta\n"
                );
                assert_eq!(
                    std::fs::metadata(root.join("new.sh"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o644
                );
                drop(runtime.update(RepositoryAction::Undo { card }));
            }
            assert!(!runtime.desired_file_present());
            if save_between_undos {
                let task = runtime.request_save_inner(card, false, false);
                finish_lineage_save(&mut runtime, task).await;
                assert!(!root.join("new.sh").exists());
            }
            drop(runtime.update(RepositoryAction::Undo { card }));
            assert_eq!(runtime.editor_text().unwrap(), "synthetic alpha\n");
            let task = runtime.request_save_inner(card, false, false);
            finish_lineage_save(&mut runtime, task).await;
            assert_eq!(
                std::fs::read_to_string(root.join("new.sh")).unwrap(),
                "synthetic alpha\n"
            );
            assert_eq!(
                std::fs::metadata(root.join("new.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "older synthetic contents must not acquire the later broader permissions"
            );
            assert!(!runtime.is_dirty());
        }
        eprintln!("PRESENCE_LINEAGE_VERIFIED");
    }

    #[tokio::test]
    async fn partial_change_presence_capacity_refuses_before_disk_effects_and_preserves_undo() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        initialize_presence_fixture(&root);
        std::fs::write(root.join("new.sh"), "synthetic initial\n").unwrap();
        std::fs::set_permissions(root.join("new.sh"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let card = CardId::new();
        let (mut runtime, _) = added_file_runtime(card, &root, "synthetic initial\n");
        let mut latest = String::new();
        for index in 1..=64 {
            revert_complete_added_buffer(&mut runtime, card, 0o100644);
            let task = runtime.request_save_inner(card, false, false);
            finish_lineage_save(&mut runtime, task).await;
            assert!(!root.join("new.sh").exists());
            latest = format!("synthetic cycle {index}\n");
            let DocumentState::Editable(document) = &mut runtime.document else {
                unreachable!()
            };
            document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new(latest.clone()),
            )));
            let task = runtime.request_save_inner(card, false, false);
            finish_lineage_save(&mut runtime, task).await;
            assert_eq!(
                std::fs::read_to_string(root.join("new.sh")).unwrap(),
                latest
            );
        }
        revert_complete_added_buffer(&mut runtime, card, 0o100644);
        let refused = runtime.request_save_inner(card, false, false);
        assert!(iced::runtime::task::into_stream(refused).is_none());
        assert!(
            runtime
                .save_error()
                .unwrap()
                .contains("retained deletion snapshots")
        );
        assert_eq!(
            runtime.source_surface,
            SourceSurface::Editor,
            "the recovery message must be visible"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.sh")).unwrap(),
            latest
        );
        assert_eq!(
            std::fs::metadata(root.join("new.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), latest);
        assert!(!runtime.is_dirty());
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!()
        };
        assert_eq!(document.restorations.len(), 64);
        document.history.clear();
        document.prune_restorations();
        assert_eq!(
            document.restorations.len(),
            1,
            "only the current/saved lineage remains after history is dropped"
        );
    }

    #[tokio::test]
    async fn partial_change_full_added_file_revert_saves_absence_and_single_undo_restores_bytes_and_mode()
     {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let text = "#!/bin/sh\r\necho native\r\nno final newline";
        std::fs::write(root.join("new.sh"), text).unwrap();
        std::fs::set_permissions(root.join("new.sh"), std::fs::Permissions::from_mode(0o751))
            .unwrap();
        let card = CardId::new();
        let (mut runtime, identity) = added_file_runtime(card, &root, text);
        let model = runtime.current_diff_model_snapshot().unwrap();
        let request = runtime
            .diff_mutation_request(card, 0..model.row_count(runtime.diff_mode))
            .unwrap();
        let (_, revision, mut patch) = runtime
            .capture_diff_mutation(card, git_patch::Kind::Revert, Some(&request))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(patch.working_present, Some(false));
        runtime
            .apply_diff_buffer_revert_with_presence(
                &identity,
                revision.unwrap(),
                patch.buffer_change.take(),
                false,
                patch.working_mode,
            )
            .unwrap();
        assert!(!runtime.desired_file_present());
        assert!(runtime.is_dirty());
        let task = runtime.request_save_inner(card, false, false);
        finish_presence_save(&mut runtime, task).await;
        assert!(!root.join("new.sh").exists());
        assert!(!runtime.saved_file_present());
        assert!(!runtime.is_dirty());
        assert!(
            runtime.auto_save_revision().is_none(),
            "old timers cannot recreate the absent empty buffer"
        );
        drop(runtime.refresh_selected_status(card));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("absence keeps native Undo mounted");
        };
        assert!(document.history.can_undo());
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), text);
        assert!(runtime.desired_file_present());
        assert!(runtime.is_dirty());
        let task = runtime.request_save_inner(card, false, false);
        finish_presence_save(&mut runtime, task).await;
        assert_eq!(std::fs::read(root.join("new.sh")).unwrap(), text.as_bytes());
        assert_eq!(
            std::fs::metadata(root.join("new.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert!(
            !document.history.can_undo(),
            "the entire presence/content revert is one step"
        );
    }

    #[tokio::test]
    async fn partial_change_late_absent_save_keeps_newer_typing_dirty_and_recreates_it_safely() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("new.sh"), "original\n").unwrap();
        std::fs::set_permissions(root.join("new.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let card = CardId::new();
        let (mut runtime, identity) = added_file_runtime(card, &root, "original\n");
        let model = runtime.current_diff_model_snapshot().unwrap();
        let request = runtime
            .diff_mutation_request(card, 0..model.row_count(runtime.diff_mode))
            .unwrap();
        let (_, revision, mut patch) = runtime
            .capture_diff_mutation(card, git_patch::Kind::Revert, Some(&request))
            .unwrap()
            .build()
            .unwrap();
        runtime
            .apply_diff_buffer_revert_with_presence(
                &identity,
                revision.unwrap(),
                patch.buffer_change.take(),
                false,
                patch.working_mode,
            )
            .unwrap();
        let deletion = runtime.request_save_inner(card, false, false);
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
            Arc::new("newer typing\n".into()),
        )));
        assert!(document.present);
        finish_presence_save(&mut runtime, deletion).await;
        assert!(!root.join("new.sh").exists());
        assert!(runtime.is_dirty());
        assert!(runtime.desired_file_present());
        assert!(!runtime.saved_file_present());
        assert_eq!(runtime.editor_text().unwrap(), "newer typing\n");
        let recreation = runtime.request_save_inner(card, false, false);
        finish_presence_save(&mut runtime, recreation).await;
        assert_eq!(
            std::fs::read_to_string(root.join("new.sh")).unwrap(),
            "newer typing\n"
        );
        assert!(!runtime.is_dirty());
    }

    #[test]
    fn partial_change_patch_targets_the_followed_file_while_identity_stays_on_the_selected_link() {
        let card = CardId::new();
        let (mut runtime, identity) = added_file_runtime(card, Path::new("/repo"), "new\n");
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.source = "target.sh".into();
        let FileListState::Ready(files) = &mut runtime.files else {
            unreachable!();
        };
        files.push(RepositoryFile {
            path: "target.sh".into(),
            unstaged: true,
            ..RepositoryFile::default()
        });
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
        let request = runtime.diff_mutation_request(card, 0..1).unwrap();
        let capture = runtime
            .capture_diff_mutation(card, git_patch::Kind::Stage, Some(&request))
            .unwrap();
        assert_eq!(capture.identity, identity);
        assert_eq!(capture.new_path.as_path(), Path::new("target.sh"));
        assert_eq!(capture.old_path.as_path(), Path::new("target.sh"));
        let (_, _, patch) = capture.build().unwrap();
        let text = std::str::from_utf8(&patch.patch).unwrap();
        assert!(text.contains("b/target.sh"));
        assert!(!text.contains("new.sh"));
    }

    #[test]
    fn partial_change_selected_lines_of_added_file_keep_presence_and_one_undo() {
        let card = CardId::new();
        let (mut runtime, identity) =
            added_file_runtime(card, Path::new("/repo"), "one\ntwo\nthree\n");
        let request = runtime.diff_mutation_request(card, 0..1).unwrap();
        let (_, revision, mut patch) = runtime
            .capture_diff_mutation(card, git_patch::Kind::Revert, Some(&request))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(patch.working_present, Some(true));
        runtime
            .apply_diff_buffer_revert_with_presence(
                &identity,
                revision.unwrap(),
                patch.buffer_change.take(),
                true,
                patch.working_mode,
            )
            .unwrap();
        assert!(runtime.desired_file_present());
        assert_eq!(runtime.editor_text().unwrap(), "two\nthree\n");
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), "one\ntwo\nthree\n");
        assert!(runtime.desired_file_present());
    }

    #[test]
    fn partial_change_presence_only_edit_is_dirty_even_when_saved_and_current_text_are_empty() {
        let card = CardId::new();
        let (mut runtime, identity) = added_file_runtime(card, Path::new("/repo"), "");
        runtime
            .apply_diff_buffer_revert_with_presence(&identity, 0, None, false, Some(0o100755))
            .unwrap();
        assert!(runtime.is_dirty());
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        assert!(document.step_history(false).is_some());
        assert!(document.present);
        assert!(!document.dirty);
        assert!(!document.history.can_undo());
    }

    pub(super) fn selected_mutation_fixture(card: CardId) -> RepositoryRuntime {
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "file.txt", "changed\nkeep\n");
        runtime.source_surface = SourceSurface::Diff;
        let mut model = crate::diff_view::build_diff("original\nkeep\n", "changed\nkeep\n");
        model.present = [true; 2];
        model.modes = [Some(0o100644); 2];
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(model),
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            }),
        );
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
        runtime.working_diff.applied_rich = true;
        runtime.working_diff.desired_rich = true;
        runtime
    }

    fn partial_controls_fixture(
        card: CardId,
        mode: DiffMode,
        kind: git_patch::Kind,
    ) -> RepositoryRuntime {
        let before =
            "old first\nkeep 1\nkeep 2\nkeep 3\nkeep 4\nkeep 5\nkeep 6\nkeep 7\nkeep 8\nold last\n";
        let after =
            "new first\nkeep 1\nkeep 2\nkeep 3\nkeep 4\nkeep 5\nkeep 6\nkeep 7\nkeep 8\nnew last\n";
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "file.txt", after);
        runtime.source_surface = SourceSurface::Diff;
        runtime.diff_mode = mode;
        let mut model = crate::diff_view::build_diff(before, after);
        model.present = [true; 2];
        model.modes = [Some(0o100644); 2];
        if kind == git_patch::Kind::Unstage {
            model.source_head = Some(crate::repository::GitHeadGuard::Commit(
                crate::repository::Oid::parse_object("11".repeat(20)).unwrap(),
            ));
            runtime.diff_basis = DiffBasis::HeadToIndex;
        }
        runtime.set_diff_state(
            runtime.diff_basis.clone(),
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(model),
                old_label: "Before".into(),
                new_label: "After".into(),
            }),
        );
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
        runtime.working_diff.applied_rich = true;
        runtime.working_diff.desired_rich = true;
        runtime
    }

    fn follow_partial_target(runtime: &mut RepositoryRuntime) {
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!()
        };
        document.source = "target.txt".into();
        let FileListState::Ready(files) = &mut runtime.files else {
            unreachable!()
        };
        files.push(RepositoryFile {
            path: "target.txt".into(),
            staged_old_path: Some("old-a.txt".into()),
            unstaged_old_path: Some("old-a.txt".into()),
            staged: true,
            unstaged: true,
            status: Some("RR".into()),
            directory: false,
        });
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
    }

    fn refresh_partial_target_rename(runtime: &mut RepositoryRuntime, card: CardId) {
        let mut files = runtime.listed_files().to_vec();
        let target = files
            .iter_mut()
            .find(|file| file.path == Path::new("target.txt"))
            .unwrap();
        if runtime.diff_basis == DiffBasis::HeadToIndex {
            target.staged_old_path = Some("old-b.txt".into());
        } else {
            target.unstaged_old_path = Some("old-b.txt".into());
        }
        runtime.background_file_refresh = true;
        let identity = AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: ".".into(),
            generation: runtime.file_list_generation,
        };
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity,
            result: Ok(files),
        }));
    }

    #[test]
    fn partial_change_requests_refuse_a_followed_target_rename_offer_changed_by_files_loaded() {
        for mode in [DiffMode::Inline, DiffMode::Split] {
            for kind in [
                git_patch::Kind::Stage,
                git_patch::Kind::Unstage,
                git_patch::Kind::Revert,
            ] {
                let card = CardId::new();
                let mut runtime = partial_controls_fixture(card, mode, kind);
                follow_partial_target(&mut runtime);
                let original = runtime.diff_mutation_request(card, 0..1).unwrap();
                assert_eq!(original.identity.path, Path::new("file.txt"));
                assert_eq!(original.old_path.as_path(), Path::new("old-a.txt"));
                assert_eq!(original.new_path.as_path(), Path::new("target.txt"));
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&original))
                        .is_some()
                );
                refresh_partial_target_rename(&mut runtime, card);
                let fresh = runtime.diff_mutation_request(card, 0..1).unwrap();
                assert_eq!(fresh.identity, original.identity);
                assert_eq!(fresh.layout_revision, original.layout_revision);
                assert_eq!(fresh.selection, original.selection);
                assert_eq!(fresh.buffer_revision, original.buffer_revision);
                assert!(
                    Arc::ptr_eq(&fresh.model, &original.model),
                    "the actual list update only changed target metadata, leaving the alias and model ready"
                );
                assert!(runtime.can_mutate_diff(kind));
                assert_ne!(
                    fresh, original,
                    "native press ownership includes the actual rename target offer"
                );
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&original))
                        .is_none()
                );
                let capture = runtime
                    .capture_diff_mutation(card, kind, Some(&fresh))
                    .expect("a fresh offer remains usable");
                assert_eq!(capture.identity.path, Path::new("file.txt"));
                assert_eq!(capture.old_path.as_path(), Path::new("old-b.txt"));
                assert_eq!(capture.new_path.as_path(), Path::new("target.txt"));
            }
        }
    }

    #[test]
    fn partial_change_requests_retain_source_identity_selection_and_input_revision() {
        for mode in [DiffMode::Inline, DiffMode::Split] {
            for kind in [
                git_patch::Kind::Stage,
                git_patch::Kind::Unstage,
                git_patch::Kind::Revert,
            ] {
                let card = CardId::new();
                let mut runtime = partial_controls_fixture(card, mode, kind);
                let original = runtime.diff_mutation_request(card, 0..1).unwrap();
                let same = runtime.diff_mutation_request(card, 0..1).unwrap();
                assert_eq!(
                    original, same,
                    "equal source captures keep an ordinary held press"
                );
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&original))
                        .is_some()
                );
                let retained_source = Arc::downgrade(&original.model);
                let mut replacement = (*original).clone();
                replacement.model = Arc::new((*original.model).clone());
                assert_ne!(
                    *original, replacement,
                    "same-content allocations are distinct immutable offers"
                );
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&replacement))
                        .is_none()
                );
                runtime.diff_interaction.selection = Some(crate::diff_interaction::Selection::new(
                    mode,
                    crate::diff_interaction::Side::New,
                    0,
                ));
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&original))
                        .is_none()
                );
                let selected = runtime.diff_mutation_request(card, 0..1).unwrap();
                assert_ne!(original, selected);
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&selected))
                        .is_some()
                );
                let DocumentState::Editable(document) = &mut runtime.document else {
                    unreachable!()
                };
                document.revision += 1;
                // Retain a ready, same-content model to isolate the request's
                // input-revision fence from ordinary annotation invalidation.
                runtime.working_diff.applied = runtime.wanted_working_diff();
                runtime.working_diff.desired = runtime.wanted_working_diff();
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&selected))
                        .is_none()
                );
                let fresh = runtime.diff_mutation_request(card, 0..1).unwrap();
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&fresh))
                        .is_some()
                );
                let basis = runtime.diff_basis.clone();
                runtime.set_diff_state(
                    basis,
                    DiffState::ready(RenderedDiff::Text {
                        model: replacement.model.clone(),
                        old_label: "Before".into(),
                        new_label: "After".into(),
                    }),
                );
                assert!(
                    runtime
                        .capture_diff_mutation(card, kind, Some(&fresh))
                        .is_none()
                );
                drop(same);
                drop(selected);
                drop(fresh);
                drop(runtime);
                assert!(
                    retained_source.upgrade().is_some(),
                    "the original request retains its allocation against pointer reuse"
                );
                drop(original);
                assert!(retained_source.upgrade().is_none());
            }
        }
    }

    #[test]
    fn partial_change_controls_keep_held_native_press_with_the_original_offer() {
        use iced::advanced::clipboard;
        use iced::runtime::{UserInterface, user_interface::Cache};
        use iced::{Event, Point, Size, mouse};

        fn controls(
            card: CardId,
            runtime: &RepositoryRuntime,
            icons: bool,
        ) -> Element<'_, RepositoryAction> {
            if icons {
                let model = runtime.current_diff_model_snapshot().unwrap();
                let row = model.change_starts(runtime.diff_mode)[runtime.diff_change];
                let offer = runtime.diff_mutation_request(card, 0..0);
                gutter(card, runtime, offer.as_ref(), &model, row, 24.0)
            } else {
                selection_controls(card, runtime)
            }
        }
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let viewport = Size::new(1000.0, 600.0);
        for mode in [DiffMode::Inline, DiffMode::Split] {
            for icons in [false, true] {
                for selected in [false, true] {
                    for kind in [
                        git_patch::Kind::Stage,
                        git_patch::Kind::Unstage,
                        git_patch::Kind::Revert,
                    ] {
                        for transition in [
                            "unchanged",
                            "file",
                            "model",
                            "selection",
                            "rename_target",
                            "hunk",
                            "disabled_then_enabled",
                        ] {
                            if selected && transition == "hunk" {
                                continue;
                            }
                            let card = CardId::new();
                            let mut runtime = partial_controls_fixture(card, mode, kind);
                            if transition == "rename_target" {
                                follow_partial_target(&mut runtime);
                            }
                            if selected {
                                runtime.diff_interaction.selection =
                                    Some(crate::diff_interaction::Selection::new(
                                        mode,
                                        crate::diff_interaction::Side::New,
                                        0,
                                    ));
                            }
                            let before_text = runtime.editor_text().unwrap();
                            let (mut cache, pointer) = {
                                let mut ui = UserInterface::build(
                                    controls(card, &runtime, icons),
                                    viewport,
                                    Cache::new(),
                                    &mut renderer,
                                );
                                let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
                                let prefix = match kind {
                                    git_patch::Kind::Stage => "Stage ",
                                    git_patch::Kind::Unstage => "Unstage ",
                                    git_patch::Kind::Revert => "Revert ",
                                };
                                let bounds = nodes
                                    .root()
                                    .iter()
                                    .chain(nodes.children())
                                    .find(|node| {
                                        node.node().role()
                                            == iced_accessibility::accesskit::Role::Button
                                            && node
                                                .node()
                                                .label()
                                                .is_some_and(|name| name.starts_with(prefix))
                                            && !node.node().is_disabled()
                                    })
                                    .expect("the actual partial-change control is enabled")
                                    .node()
                                    .bounds()
                                    .unwrap();
                                let pointer = Point::new(
                                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                                );
                                let mut messages = Vec::new();
                                ui.update(
                                    &[Event::Mouse(mouse::Event::ButtonPressed(
                                        mouse::Button::Left,
                                    ))],
                                    mouse::Cursor::Available(pointer),
                                    &mut renderer,
                                    &mut clipboard::Null,
                                    &mut messages,
                                );
                                assert!(messages.is_empty());
                                (ui.into_cache(), pointer)
                            };
                            match transition {
                                "unchanged" => {}
                                "file" => {
                                    runtime.selected.as_mut().unwrap().path = "other.txt".into();
                                    let FileListState::Ready(files) = &mut runtime.files else {
                                        unreachable!()
                                    };
                                    files[0].path = "other.txt".into();
                                    let DocumentState::Editable(document) = &mut runtime.document
                                    else {
                                        unreachable!()
                                    };
                                    document.path = "other.txt".into();
                                    document.source = "other.txt".into();
                                    runtime.generation += 1;
                                    runtime.working_diff.applied = runtime.wanted_working_diff();
                                    runtime.working_diff.desired = runtime.wanted_working_diff();
                                }
                                "model" => {
                                    let model = runtime.current_diff_model_snapshot().unwrap();
                                    runtime.set_diff_state(
                                        runtime.diff_basis.clone(),
                                        DiffState::ready(RenderedDiff::Text {
                                            model: Arc::new((*model).clone()),
                                            old_label: "Before".into(),
                                            new_label: "After".into(),
                                        }),
                                    );
                                }
                                "selection" => {
                                    let mut selection = crate::diff_interaction::Selection::new(
                                        mode,
                                        crate::diff_interaction::Side::New,
                                        0,
                                    );
                                    selection.head = 1;
                                    runtime.diff_interaction.selection = Some(selection);
                                }
                                "rename_target" => {
                                    refresh_partial_target_rename(&mut runtime, card)
                                }
                                "hunk" => runtime.diff_change = 1,
                                "disabled_then_enabled" => {
                                    runtime.mutation_refresh_pending = true;
                                    let ui = UserInterface::build(
                                        controls(card, &runtime, icons),
                                        viewport,
                                        cache,
                                        &mut renderer,
                                    );
                                    cache = ui.into_cache();
                                    runtime.mutation_refresh_pending = false;
                                }
                                _ => unreachable!(),
                            }
                            let mut ui = UserInterface::build(
                                controls(card, &runtime, icons),
                                viewport,
                                cache,
                                &mut renderer,
                            );
                            let cursor = mouse::Cursor::Available(pointer);
                            let mut messages = Vec::new();
                            ui.update(
                                &[Event::Mouse(mouse::Event::ButtonReleased(
                                    mouse::Button::Left,
                                ))],
                                cursor,
                                &mut renderer,
                                &mut clipboard::Null,
                                &mut messages,
                            );
                            assert_eq!(
                                messages.len(),
                                usize::from(transition == "unchanged"),
                                "held press cannot migrate: {mode:?}, icons={icons}, selected={selected}, {kind:?}, {transition}"
                            );
                            assert_eq!(
                                runtime.editor_text().as_deref(),
                                Some(before_text.as_str()),
                                "release on a replacement offer cannot mutate its buffer"
                            );
                            messages.clear();
                            ui.update(
                                &[
                                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                                ],
                                cursor,
                                &mut renderer,
                                &mut clipboard::Null,
                                &mut messages,
                            );
                            let [
                                RepositoryAction::ApplyDiffHunk {
                                    card: actual_card,
                                    kind: actual_kind,
                                    request,
                                },
                            ] = &messages[..]
                            else {
                                panic!("a fresh native press activates the current partial offer")
                            };
                            assert_eq!(*actual_card, card);
                            assert_eq!(*actual_kind, kind);
                            let capture = runtime
                                .capture_diff_mutation(card, kind, Some(request))
                                .expect(
                                    "the fresh callback keeps its captured source and selection",
                                );
                            let prepared = capture
                                .prepare()
                                .expect("the current captured rows prepare a real patch");
                            drop(ui);
                            if kind == git_patch::Kind::Revert {
                                assert!(prepared.apply(&mut runtime).unwrap().is_none());
                                assert_ne!(runtime.editor_text().unwrap(), before_text);
                                drop(runtime.update(RepositoryAction::Undo { card }));
                                assert_eq!(runtime.editor_text().unwrap(), before_text);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn read_only_git_runtime_disables_git_writes_but_keeps_buffer_revert_edit_and_undo() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        runtime.set_git_mutation_unavailable_reason(Some(Arc::from("Git writes unsupported")));
        assert!(!runtime.can_apply_diff_hunk(git_patch::Kind::Stage));
        assert!(runtime.can_apply_diff_hunk(git_patch::Kind::Revert));
        let revert = runtime.capture_diff_mutation(card, git_patch::Kind::Revert, None).unwrap();
        assert!(revert.applies_to_buffer());
        assert!(revert.prepare().unwrap().apply(&mut runtime).unwrap().is_none());
        assert_eq!(runtime.editor_text().unwrap(), "original\nkeep\n");
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), "changed\nkeep\n");
        assert!(runtime.selected_staging_selection(true).is_none());
        let request = runtime.diff_mutation_request(card, 0..2).unwrap();
        assert!(runtime.capture_diff_mutation(card, git_patch::Kind::Stage, Some(&request)).is_none());
        runtime.source_surface = SourceSurface::Editor;
        drop(runtime.update(RepositoryAction::EditorAction { card, action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("editable ".into()))) }));
        assert!(runtime.editor_text().unwrap().contains("editable"));
        assert!(runtime.can_step_editor_history(false));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), "changed\nkeep\n");
    }

    #[test]
    fn opaque_prepared_diff_refuses_selection_and_same_content_model_replacement() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        let request = runtime.diff_mutation_request(card, 0..1).unwrap();
        let prepared = runtime.capture_diff_mutation(card, git_patch::Kind::Stage, Some(&request)).unwrap().prepare().unwrap();
        runtime.diff_interaction.selection = Some(crate::diff_interaction::Selection::new(runtime.diff_mode, crate::diff_interaction::Side::New, 0));
        assert!(prepared.apply(&mut runtime).is_err());
        runtime.diff_interaction.selection = None;
        let prepared = runtime.capture_diff_mutation(card, git_patch::Kind::Stage, Some(&request)).unwrap().prepare().unwrap();
        let model = runtime.current_diff_model_snapshot().unwrap();
        let replacement = Arc::new((*model).clone());
        runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(RenderedDiff::Text { model: replacement, old_label: "Index".into(), new_label: "Working tree".into() }));
        assert!(prepared.apply(&mut runtime).is_err());
    }

    #[test]
    fn opaque_prepared_revert_uses_one_native_undo_transaction() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        let request = runtime.diff_mutation_request(card, 0..2).unwrap();
        let prepared = runtime.capture_diff_mutation(card, git_patch::Kind::Revert, Some(&request)).unwrap().prepare().unwrap();
        assert!(prepared.applies_to_buffer());
        assert!(prepared.apply(&mut runtime).unwrap().is_none());
        assert_eq!(runtime.editor_text().unwrap(), "original\nkeep\n");
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), "changed\nkeep\n");
    }

    #[test]
    fn selection_mutation_commands_require_selected_rows_and_capture_their_source() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        assert!(
            runtime
                .diff_selection_mutation_action(card, git_patch::Kind::Revert)
                .is_none()
        );
        runtime.diff_interaction.selection = Some(crate::diff_interaction::Selection::new(
            runtime.diff_mode,
            crate::diff_interaction::Side::New,
            0,
        ));
        assert!(runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        assert!(runtime.can_apply_diff_selection(git_patch::Kind::Revert));
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Unstage));
        let RepositoryAction::ApplyDiffHunk {
            card: actual,
            kind,
            request,
        } = runtime
            .diff_selection_mutation_action(card, git_patch::Kind::Stage)
            .unwrap()
        else {
            panic!("selection must dispatch a source-bound request");
        };
        assert_eq!(actual, card);
        assert_eq!(kind, git_patch::Kind::Stage);
        assert_eq!(request.rows, 0..1);
        runtime.diff_interaction.selection.as_mut().unwrap().head = 1;
        assert_eq!(
            request.rows,
            0..1,
            "later selection changes cannot retarget the action"
        );
        assert!(
            runtime
                .capture_diff_mutation(card, kind, Some(&request))
                .is_none(),
            "the consumed callback cannot adopt a newer selection"
        );
        let fresh = runtime.diff_selection_mutation_action(card, kind).unwrap();
        let RepositoryAction::ApplyDiffHunk { request, .. } = fresh else {
            unreachable!()
        };
        assert!(
            runtime
                .capture_diff_mutation(card, kind, Some(&request))
                .is_some()
        );
        runtime.diff_layout_revision = runtime.diff_layout_revision.wrapping_add(1);
        assert!(
            runtime
                .capture_diff_mutation(card, kind, Some(&request))
                .is_none()
        );
    }

    #[test]
    fn selection_mutation_commands_disable_unchanged_context_only_ranges() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        let context_row = runtime
            .current_diff_model_snapshot()
            .unwrap()
            .row_count(runtime.diff_mode)
            - 1;
        runtime.diff_interaction.selection = Some(crate::diff_interaction::Selection::new(
            runtime.diff_mode,
            crate::diff_interaction::Side::New,
            context_row,
        ));
        for kind in [git_patch::Kind::Stage, git_patch::Kind::Revert] {
            assert!(!runtime.can_apply_diff_selection(kind));
            assert!(runtime.diff_selection_mutation_action(card, kind).is_none());
        }
        runtime.diff_interaction.selection.as_mut().unwrap().anchor = 0;
        assert!(runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        assert!(runtime.can_apply_diff_selection(git_patch::Kind::Revert));
    }

    #[test]
    fn diff_mutation_controls_capture_the_rendered_hunk_and_disable_context_only_selections() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        assert!(runtime.can_apply_diff_hunk(git_patch::Kind::Stage));
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        let RepositoryAction::ApplyDiffHunk { request, .. } = runtime
            .diff_mutation_control_action(card, git_patch::Kind::Stage)
            .unwrap()
        else {
            panic!("toolbar must capture its rendered hunk");
        };
        let rows = request.rows.clone();
        runtime.diff_change = usize::MAX;
        assert!(!runtime.can_apply_diff_hunk(git_patch::Kind::Stage));
        assert!(
            runtime
                .diff_hunk_mutation_action(card, git_patch::Kind::Stage)
                .is_none()
        );
        assert_eq!(request.rows, rows);
        runtime.diff_change = 0;
        let context_row = runtime
            .current_diff_model_snapshot()
            .unwrap()
            .row_count(runtime.diff_mode)
            - 1;
        runtime.diff_interaction.selection = Some(crate::diff_interaction::Selection::new(
            runtime.diff_mode,
            crate::diff_interaction::Side::New,
            context_row,
        ));
        assert!(
            runtime
                .diff_mutation_control_action(card, git_patch::Kind::Stage)
                .is_none()
        );
        assert!(
            runtime
                .diff_mutation_control_action(card, git_patch::Kind::Revert)
                .is_none()
        );
        assert!(!runtime.can_apply_diff_hunk(git_patch::Kind::Stage));
        assert!(
            runtime
                .diff_hunk_mutation_action(card, git_patch::Kind::Stage)
                .is_none()
        );
        runtime.diff_interaction.selection.as_mut().unwrap().anchor = 0;
        assert!(runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        assert!(!runtime.can_apply_diff_hunk(git_patch::Kind::Stage));
    }

    #[test]
    fn selection_mutation_commands_share_basis_mode_and_current_annotation_guards() {
        let card = CardId::new();
        let mut runtime = selected_mutation_fixture(card);
        let selection = crate::diff_interaction::Selection::new(
            runtime.diff_mode,
            crate::diff_interaction::Side::New,
            0,
        );
        runtime.diff_interaction.selection = Some(selection);
        runtime.mutation_refresh_pending = true;
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        runtime.mutation_refresh_pending = false;
        runtime.working_diff.applied = None;
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        runtime.working_diff.applied = runtime.wanted_working_diff();
        let mut model = crate::diff_view::build_diff("original\n", "changed\n");
        model.present = [true; 2];
        model.modes = [Some(0o100644); 2];
        model.source_head = Some(crate::repository::GitHeadGuard::Unborn);
        runtime.diff_basis = DiffBasis::HeadToIndex;
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(model),
                old_label: "HEAD".into(),
                new_label: "Index".into(),
            }),
        );
        runtime.diff_interaction.selection = Some(selection);
        assert!(runtime.can_apply_diff_selection(git_patch::Kind::Unstage));
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Stage));
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Revert));
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!()
        };
        document.dirty = true;
        assert!(!runtime.can_apply_diff_selection(git_patch::Kind::Unstage));
    }

    #[test]
    fn partial_change_revert_captures_clean_and_dirty_live_buffers_for_one_undo() {
        for dirty in [false, true] {
            let card = CardId::new();
            let before = "original\r\nkeep\r\n";
            let after = "changed\r\nkeep\r\n";
            let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "file.txt", after);
            runtime.source_surface = SourceSurface::Diff;
            let mut model = crate::diff_view::build_diff(before, after);
            model.present = [true; 2];
            model.modes = [Some(0o100644); 2];
            let rows = 0..model.row_count(runtime.diff_mode);
            runtime.set_diff_state(
                DiffBasis::IndexToWorktree,
                DiffState::ready(RenderedDiff::Text {
                    model: Arc::new(model),
                    old_label: "Index".into(),
                    new_label: "Working tree".into(),
                }),
            );
            let DocumentState::Editable(document) = &mut runtime.document else {
                unreachable!();
            };
            document.dirty = dirty;
            runtime.working_diff.applied = runtime.wanted_working_diff();
            runtime.working_diff.desired = runtime.wanted_working_diff();
            runtime.working_diff.applied_rich = true;
            runtime.working_diff.desired_rich = true;
            let request = runtime.diff_mutation_request(card, rows).unwrap();
            assert!(runtime.can_mutate_diff(git_patch::Kind::Stage));
            let capture = runtime
                .capture_diff_mutation(card, git_patch::Kind::Revert, Some(&request))
                .unwrap();
            let (_, revision, mut patch) = capture.build().unwrap();
            runtime
                .apply_diff_buffer_revert(
                    &identity,
                    revision.unwrap(),
                    patch.buffer_change.take().unwrap(),
                )
                .unwrap();
            let DocumentState::Editable(document) = &mut runtime.document else {
                unreachable!();
            };
            assert_eq!(document.content.text(), before);
            assert!(document.dirty);
            assert!(document.step_history(false).is_some());
            assert_eq!(document.content.text(), after);
            assert!(!document.history.can_undo());
        }
    }

    #[test]
    fn partial_change_buffer_revert_is_one_undo_step_with_exact_crlf_and_caret() {
        let card = CardId::new();
        let after = "first\r\nCHANGE\r\nkeep another hunk\r\nno final newline";
        let desired = "first\r\noriginal\r\nkeep another hunk\r\nno final newline";
        let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "file.txt", after);
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!("expected editable fixture");
        };
        let cursor = text_editor::Cursor {
            position: text_editor::Position { line: 2, column: 4 },
            selection: None,
        };
        document.content.set_cursor(cursor);
        let revision = document.revision;
        let change = buffer_change(
            &document.content.text_snapshot(),
            &document.content.text(),
            desired,
        )
        .unwrap();
        assert_eq!(change.start.line, 1);
        assert!(!change.removed.contains("first") && !change.removed.contains("another hunk"));
        assert!(!document.history.can_undo());
        runtime
            .apply_diff_buffer_revert(&identity, revision, Arc::new(change))
            .unwrap();
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
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
        let (mut runtime, identity) =
            RepositoryRuntime::open_fixture(card, "file.txt", "changed\n");
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        let revision = document.revision;
        let change = buffer_change(
            &document.content.text_snapshot(),
            &document.content.text(),
            "original\n",
        )
        .unwrap();
        document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('!')));
        let typed = document.content.text();
        assert!(
            runtime
                .apply_diff_buffer_revert(&identity, revision, Arc::new(change))
                .is_err()
        );
        assert_eq!(runtime.editor_text().unwrap(), typed);
    }
    #[test]
    fn partial_change_buffer_revert_keeps_distant_noncanonical_breaks_out_of_the_transaction() {
        let card = CardId::new();
        let source = (0..200)
            .map(|line| format!("row {line}\n"))
            .collect::<String>();
        let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "file.txt", &source);
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        let at = text_editor::Position { line: 5, column: 0 };
        document
            .content
            .replace_range_with_breaks(at, at, "\n\r", &[1, 2]);
        let snapshot = document.content.text_snapshot();
        let before = snapshot.text();
        let desired = before.replace("row 100\n", "original\n");
        let revision = document.revision;
        let change = buffer_change(&snapshot, &before, &desired).unwrap();
        assert!(change.start.line >= 100);
        assert!(change.removed.len() < 100 && change.inserted.len() < 100);
        runtime
            .apply_diff_buffer_revert(&identity, revision, Arc::new(change))
            .unwrap();
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        assert_eq!(document.content.text(), desired);
        let after = document.content.text_snapshot();
        for line in 0..10 {
            assert_eq!(
                after.line(line).unwrap().ending,
                snapshot.line(line).unwrap().ending
            );
        }
        assert!(document.step_history(false).is_some());
        assert_eq!(document.content.text(), before.as_ref());
        let undone = document.content.text_snapshot();
        assert_eq!(undone.line_count(), snapshot.line_count());
        for line in 0..snapshot.line_count() {
            assert_eq!(
                undone.line(line).unwrap().ending,
                snapshot.line(line).unwrap().ending
            );
        }
    }

    #[test]
    fn partial_change_buffer_revert_measures_utf8_at_character_boundaries() {
        let content: text_editor::Content<cosmic::Renderer> =
            text_editor::Content::with_text("α\ncafé\nend");
        let snapshot = content.text_snapshot();
        let change = buffer_change(&snapshot, &snapshot.text(), "α\ncafê\nend").unwrap();
        assert_eq!(change.start, text_editor::Position { line: 1, column: 3 });
        assert_eq!(change.removed, "é");
        assert_eq!(change.inserted, "ê");
    }
}
