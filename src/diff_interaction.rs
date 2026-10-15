//! Source-based diff selection and search, independent of virtual row mounting.
//!
//! A folded context run never removes its rows from the model. Selection and
//! search retain model coordinates, so unfolding and viewport virtualization
//! cannot omit text or move a result to a different source line.

use std::ops::Range;

use crate::diff_view::{DiffMode, DiffModel, DiffRowKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyText {
    Natural,
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub mode: DiffMode,
    pub side: Side,
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn new(mode: DiffMode, side: Side, row: usize) -> Self {
        Self {
            mode,
            side,
            anchor: row,
            head: row,
        }
    }

    pub fn rows(self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head).saturating_add(1)
    }

    pub fn contains(self, mode: DiffMode, side: Side, row: usize) -> bool {
        self.mode == mode
            && (mode == DiffMode::Inline || self.side == side)
            && self.rows().contains(&row)
    }

    /// Hunk starts are sorted model coordinates. Inspect the first row and
    /// binary-search the next start, instead of rescanning the full file for
    /// every mounted row's context menu.
    pub fn has_changes(self, model: &DiffModel) -> bool {
        let rows = self.rows();
        let kind = match self.mode {
            DiffMode::Inline => model.inline_rows.get(rows.start).map(|row| row.kind),
            DiffMode::Split => model.split_rows.get(rows.start).map(|row| row.kind),
        };
        if kind.is_some_and(|kind| kind != DiffRowKind::Context) {
            return true;
        }
        let starts = model.change_starts(self.mode);
        starts
            .get(starts.partition_point(|start| *start < rows.start))
            .is_some_and(|start| *start < rows.end)
    }

    /// Copy original source bytes, including CRLF and final-newline identity.
    /// Display rows may contain visible line-ending markers; those annotations
    /// must never leak into source copied into a composer or editor.
    pub fn text(self, model: &DiffModel, kind: CopyText) -> String {
        let old = model.before.split_inclusive('\n').collect::<Vec<_>>();
        let new = model.after.split_inclusive('\n').collect::<Vec<_>>();
        let mut text = String::new();
        let mut append = |side: Side, line: Option<u32>| {
            let lines = match side {
                Side::Old => &old,
                Side::New => &new,
            };
            if let Some(line) = line
                .and_then(|line| line.checked_sub(1))
                .and_then(|line| lines.get(line as usize))
            {
                text.push_str(line);
            }
        };
        for index in self
            .rows()
            .take(model.row_count(self.mode).saturating_sub(self.rows().start))
        {
            match self.mode {
                DiffMode::Inline => {
                    let Some(row) = model.inline_rows.get(index) else {
                        break;
                    };
                    match kind {
                        CopyText::Old => append(Side::Old, row.old_line),
                        CopyText::New => append(Side::New, row.new_line),
                        CopyText::Natural if row.kind == DiffRowKind::Deletion => {
                            append(Side::Old, row.old_line)
                        }
                        CopyText::Natural => append(Side::New, row.new_line),
                    }
                }
                DiffMode::Split => {
                    let Some(row) = model.split_rows.get(index) else {
                        break;
                    };
                    let side = match kind {
                        CopyText::Old => Side::Old,
                        CopyText::New => Side::New,
                        CopyText::Natural => self.side,
                    };
                    append(
                        side,
                        match side {
                            Side::Old => row.old_line,
                            Side::New => row.new_line,
                        },
                    );
                }
            }
        }
        text
    }

    pub fn line_reference(self, model: &DiffModel) -> Option<u32> {
        match self.mode {
            DiffMode::Inline => model
                .inline_rows
                .get(self.head)
                .and_then(|row| match self.side {
                    Side::Old => row.old_line.or(row.new_line),
                    Side::New => row.new_line.or(row.old_line),
                }),
            DiffMode::Split => model
                .split_rows
                .get(self.head)
                .and_then(|row| match self.side {
                    Side::Old => row.old_line,
                    Side::New => row.new_line,
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_view::build_diff;

    #[test]
    fn unified_selection_copies_natural_and_each_source_side() {
        let model = build_diff("same\r\nold\r\ntail", "same\r\nnew\r\ntail");
        let selection = Selection {
            mode: DiffMode::Inline,
            side: Side::New,
            anchor: 0,
            head: model.inline_rows.len() - 1,
        };
        assert_eq!(selection.text(&model, CopyText::Old), "same\r\nold\r\ntail");
        assert_eq!(selection.text(&model, CopyText::New), "same\r\nnew\r\ntail");
        assert_eq!(
            selection.text(&model, CopyText::Natural),
            "same\r\nold\r\nnew\r\ntail"
        );
    }

    #[test]
    fn split_selection_stays_on_the_chosen_side() {
        let model = build_diff("one\nremoved\ntwo\n", "one\ntwo\ninserted\n");
        let mut selection = Selection {
            mode: DiffMode::Split,
            side: Side::Old,
            anchor: 0,
            head: model.split_rows.len() - 1,
        };
        assert_eq!(
            selection.text(&model, CopyText::Natural),
            "one\nremoved\ntwo\n"
        );
        selection.side = Side::New;
        assert_eq!(
            selection.text(&model, CopyText::Natural),
            "one\ntwo\ninserted\n"
        );
        assert!(selection.contains(DiffMode::Split, Side::New, 1));
        assert!(!selection.contains(DiffMode::Split, Side::Old, 1));
    }

    #[test]
    fn line_reference_uses_the_clicked_side_after_line_number_divergence() {
        let model = build_diff("one\ntwo\nthree\n", "inserted\none\ntwo\nthree\n");
        let row = model
            .inline_rows
            .iter()
            .position(|row| row.old_line == Some(2))
            .unwrap();
        assert_eq!(
            Selection::new(DiffMode::Inline, Side::Old, row).line_reference(&model),
            Some(2)
        );
        assert_eq!(
            Selection::new(DiffMode::Inline, Side::New, row).line_reference(&model),
            Some(3)
        );
    }

    #[test]
    fn changed_selection_detects_hunk_interiors_and_context_boundaries() {
        let model = build_diff("a\nb\nc\nd\n", "a\nB\nC\nd\n");
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let first = model.change_starts(mode)[0];
            assert!(!Selection::new(mode, Side::New, 0).has_changes(&model));
            assert!(Selection::new(mode, Side::New, first + 1).has_changes(&model));
            assert!(
                !Selection::new(mode, Side::New, model.row_count(mode) - 1).has_changes(&model)
            );
            assert!(
                Selection {
                    mode,
                    side: Side::New,
                    anchor: 0,
                    head: first
                }
                .has_changes(&model)
            );
        }
    }

    #[test]
    fn reversed_and_oversized_selection_is_bounded_by_the_model() {
        let model = build_diff("old\n", "new\n");
        let selection = Selection {
            mode: DiffMode::Inline,
            side: Side::New,
            anchor: usize::MAX,
            head: 0,
        };
        assert_eq!(selection.text(&model, CopyText::New), "new\n");
    }
}

/// Source text searchable in a diff, including rows currently folded away.
/// Unified context appears once; split rows retain the identity of each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow<'a> {
    pub row: usize,
    pub side: Side,
    pub line: u32,
    pub text: &'a str,
}

pub fn source_rows(model: &DiffModel, mode: DiffMode) -> Vec<SourceRow<'_>> {
    let old = model.before.split_inclusive('\n').collect::<Vec<_>>();
    let new = model.after.split_inclusive('\n').collect::<Vec<_>>();
    let mut output = Vec::with_capacity(model.row_count(mode));
    let mut append = |row, side, line: Option<u32>| {
        let source = match side {
            Side::Old => &old,
            Side::New => &new,
        };
        if let Some((line, text)) = line.and_then(|line| {
            line.checked_sub(1)
                .and_then(|index| source.get(index as usize).map(|text| (line, *text)))
        }) {
            // Search literal code, never the view's line-ending annotations.
            let text = text.strip_suffix('\n').unwrap_or(text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            output.push(SourceRow {
                row,
                side,
                line,
                text,
            });
        }
    };
    match mode {
        DiffMode::Inline => {
            for (index, row) in model.inline_rows.iter().enumerate() {
                if row.new_line.is_some() {
                    append(index, Side::New, row.new_line);
                } else {
                    append(index, Side::Old, row.old_line);
                }
            }
        }
        DiffMode::Split => {
            for (index, row) in model.split_rows.iter().enumerate() {
                append(index, Side::Old, row.old_line);
                append(index, Side::New, row.new_line);
            }
        }
    }
    output
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::diff_view::build_diff;

    #[test]
    fn searchable_rows_include_long_unchanged_regions_on_both_sides() {
        let before = (0..200)
            .map(|line| format!("line_{line}\r\n"))
            .collect::<String>();
        let after = before
            .replace("line_0\r", "changed_0\r")
            .replace("line_199", "changed_199");
        let model = build_diff(&before, &after);
        let unified = source_rows(&model, DiffMode::Inline);
        let found = unified
            .iter()
            .filter(|row| row.text == "line_100")
            .collect::<Vec<_>>();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 101);
        assert_eq!(
            model.new_line_at_row(DiffMode::Inline, found[0].row),
            Some(101)
        );
        let split = source_rows(&model, DiffMode::Split);
        assert_eq!(split.iter().filter(|row| row.text == "line_100").count(), 2);
        assert!(split.iter().all(|row| !row.text.ends_with(['\n', '\r'])));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SelectAll,
    Copy,
    CopyOld,
    CopyNew,
    CopyPatch,
    CopyLineReference { row: usize, side: Side },
    OpenInEditor { row: usize, side: Side },
}

impl Command {
    /// Stable command-registry identity, independent of captured row context.
    pub const fn command_id(self) -> &'static str {
        match self {
            Self::SelectAll => "diff_select_all",
            Self::Copy => "diff_copy",
            Self::CopyOld => "diff_copy_old",
            Self::CopyNew => "diff_copy_new",
            Self::CopyPatch => "diff_copy_patch",
            Self::CopyLineReference { .. } => "diff_copy_line_reference",
            Self::OpenInEditor { .. } => "diff_open_in_editor",
        }
    }
}

/// Whole-file contextual patch for just the selected changed rows. Git's
/// contextual patch generator restores untouched lines around the selection,
/// so a whole-hunk selection can be applied with ordinary `git apply`.
pub fn selected_patch(
    model: &DiffModel,
    mode: DiffMode,
    selected: Range<usize>,
    path: &crate::repository::RepoPath,
    old_path: Option<&crate::repository::RepoPath>,
) -> Option<String> {
    let after = crate::diff_patch::selected_content(model, &model.after, mode, selected, false)?;
    let before = model.present[0].then_some(model.before.as_ref());
    let target = if !model.present[1] && after.is_empty() {
        None
    } else {
        Some(after.as_str())
    };
    String::from_utf8(crate::diff_patch::unified_patch_paths(
        old_path.unwrap_or(path),
        path,
        before,
        target,
    ))
    .ok()
}

/// Mouse selection remains separate from syntax rendering and discussion
/// gutters, whose own buttons retain first refusal of pointer events.
pub fn row_surface<'a, Message: Clone + 'static>(
    content: cosmic::Element<'a, Message>,
    selected: bool,
    press: Message,
    context_press: Message,
    drag_enter: Option<Message>,
) -> cosmic::Element<'a, Message> {
    use cosmic::{
        iced::{self, Length},
        widget,
    };
    let content = if selected {
        iced::widget::stack([
            content,
            widget::container(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .class(cosmic::theme::Container::custom(|theme| {
                let mut color = crate::design::accent_color(theme);
                color.a = 0.16;
                iced::widget::container::Style {
                    background: Some(iced::Background::Color(color)),
                    border: iced::Border {
                        color: crate::design::accent_color(theme),
                        width: 1.0,
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }))
            .into(),
        ])
        .into()
    } else {
        content
    };
    let area = iced::widget::mouse_area(content)
        .on_press(press)
        .on_right_press(context_press);
    match drag_enter {
        Some(message) => area.on_enter(message).into(),
        None => area.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub row: usize,
    pub side: Side,
    pub line: u32,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Coordinate {
    row: usize,
    side: Side,
    line: u32,
}

/// One immutable comparison snapshot is reused while find queries change.
/// Its byte storage is shared with the model; workers construct source slices
/// and run the same Unicode/options matcher as editable buffers.
#[derive(Debug)]
pub struct SearchSnapshot {
    model: std::sync::Arc<DiffModel>,
    mode: DiffMode,
    coordinates: std::sync::OnceLock<Vec<Coordinate>>,
}

impl SearchSnapshot {
    /// Capture immutable sources and rows without scanning the model on the UI
    /// thread. The first worker search builds the reusable coordinate map.
    pub fn new(model: std::sync::Arc<DiffModel>, mode: DiffMode) -> Self {
        Self { model, mode, coordinates: std::sync::OnceLock::new() }
    }

    fn coordinates(&self, cancelled: &impl Fn() -> bool) -> Option<&[Coordinate]> {
        if cancelled() { return None; }
        if self.coordinates.get().is_none() {
            let model = &self.model;
            let mut coordinates = Vec::with_capacity(model.row_count(self.mode));
            let mut append = |row, side, line: Option<u32>| {
                if let Some(line) = line { coordinates.push(Coordinate { row, side, line }); }
            };
            match self.mode {
                DiffMode::Inline => {
                    for (row, line) in model.inline_rows.iter().enumerate() {
                        if row % 256 == 0 && cancelled() { return None; }
                        if line.new_line.is_some() { append(row, Side::New, line.new_line); }
                        else { append(row, Side::Old, line.old_line); }
                    }
                }
                DiffMode::Split => {
                    for (row, line) in model.split_rows.iter().enumerate() {
                        if row % 256 == 0 && cancelled() { return None; }
                        append(row, Side::Old, line.old_line);
                        append(row, Side::New, line.new_line);
                    }
                }
            }
            if cancelled() { return None; }
            // At most one runtime search is active, but the snapshot is also
            // safe to share: another worker may win initialization harmlessly.
            let _ = self.coordinates.set(coordinates);
        }
        self.coordinates.get().map(Vec::as_slice)
    }

    pub fn find(
        &self,
        query: &str,
        options: crate::editor_search::FindOptions,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<Vec<Match>>, String> {
        let Some(coordinates) = self.coordinates(&cancelled) else { return Ok(None); };
        let old = self.model.before.lines().collect::<Vec<_>>();
        let new = self.model.after.lines().collect::<Vec<_>>();
        let text = coordinates.iter().map(|at| {
            let lines = match at.side {
                Side::Old => &old,
                Side::New => &new,
            };
            lines
                .get(at.line.saturating_sub(1) as usize)
                .copied()
                .unwrap_or_default()
        });
        crate::editor_search::find_in_lines(text, query, options, cancelled).map(|found| {
            found.map(|matches| {
                matches
                    .into_iter()
                    .filter_map(|(start, end)| {
                        let at = coordinates.get(start.line)?;
                        (start.line == end.line).then_some(Match {
                            row: at.row,
                            side: at.side,
                            line: at.line,
                            range: start.column..end.column,
                        })
                    })
                    .collect()
            })
        })
    }
}

#[cfg(test)]
mod find_tests {
    use super::*;
    #[test]
    fn shared_find_maps_unicode_and_folded_context_to_model_rows() {
        let before = (0..200)
            .map(|row| format!("line {row}\n"))
            .collect::<String>()
            .replace("line 100", "Straße needle");
        let after = before
            .replace("line 20", "changed20")
            .replace("line 180", "changed180");
        let model = std::sync::Arc::new(crate::diff_view::build_diff(&before, &after));
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let snapshot = SearchSnapshot::new(model.clone(), mode);
            assert!(std::sync::Arc::ptr_eq(&snapshot.model, &model));
            assert!(snapshot.coordinates.get().is_none(), "UI capture must not scan model rows");
            assert!(snapshot.find("needle", crate::editor_search::FindOptions::default(), || true).unwrap().is_none());
            assert!(snapshot.coordinates.get().is_none(), "cancelled search must not populate coordinates");
            let found = snapshot
                .find(
                    "STRASSE",
                    crate::editor_search::FindOptions::default(),
                    || false,
                )
                .unwrap()
                .unwrap();
            let coordinates = snapshot.coordinates.get().unwrap().as_ptr();
            assert_eq!(found.len(), if mode == DiffMode::Inline { 1 } else { 2 });
            for found in found {
                assert_eq!(found.line, 101);
                assert_eq!(found.range, 0..7);
                assert_eq!(model.new_line_at_row(mode, found.row), Some(101));
            }
            assert!(snapshot.find("needle", crate::editor_search::FindOptions::default(), || false).unwrap().is_some());
            assert_eq!(snapshot.coordinates.get().unwrap().as_ptr(), coordinates, "queries on the same model reuse worker coordinates");
            assert!(
                snapshot
                    .find(
                        "needle",
                        crate::editor_search::FindOptions::default(),
                        || true
                    )
                    .unwrap()
                    .is_none()
            );
        }
    }
}

/// Find emphasis splits existing syntax/change spans only at match boundaries;
/// each fragment retains its original font and foreground.
pub fn highlight_matches<'a>(
    text: &'a str,
    spans: Vec<cosmic::iced::widget::text::Span<'a>>,
    matches: &[Range<usize>],
    active: Option<&Range<usize>>,
    theme: &cosmic::Theme,
) -> Vec<cosmic::iced::widget::text::Span<'a>> {
    if matches.is_empty() {
        return spans;
    }
    let mut result = Vec::with_capacity(spans.len() + matches.len() * 2);
    let mut start = 0;
    for span in spans {
        let end = start + span.text.as_ref().len();
        if end == start {
            result.push(span);
            continue;
        }
        // Search results are source-ordered, including Unicode folds that map
        // multiple hits to the same source range. Partition monotonic bounds
        // instead of scanning every match for each syntax fragment.
        let first = matches.partition_point(|found| found.end <= start);
        let last = matches.partition_point(|found| found.start < end);
        let mut boundaries = vec![start, end];
        for found in &matches[first..last] {
            if start < found.start && found.start < end {
                boundaries.push(found.start);
            }
            if start < found.end && found.end < end {
                boundaries.push(found.end);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for bounds in boundaries.windows(2) {
            let (a, b) = (bounds[0], bounds[1]);
            let mut part = span.clone();
            part.text = (&text[a..b]).into();
            let candidate = matches.partition_point(|found| found.end <= a);
            if matches
                .get(candidate)
                .is_some_and(|found| found.contains(&a))
            {
                let mut color = crate::design::warning_color(theme);
                color.a = if active.is_some_and(|found| found.contains(&a)) {
                    0.65
                } else {
                    0.32
                };
                part = part.background(color);
            }
            result.push(part);
        }
        start = end;
    }
    result
}

#[cfg(test)]
mod highlight_tests {
    use super::*;
    #[test]
    fn empty_and_dense_search_emphasis_preserve_exact_source_text() {
        use cosmic::iced::widget::text::Span;
        let theme = cosmic::theme::active();
        let empty = highlight_matches("", vec![Span::new("")], &[0..0], None, &theme);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].highlight.is_none());
        let text = "a".repeat(5000);
        let matches = (0..text.len()).map(|at| at..at + 1).collect::<Vec<_>>();
        let result = highlight_matches(
            &text,
            vec![Span::new(text.as_str())],
            &matches,
            Some(&(2500..2501)),
            &theme,
        );
        assert_eq!(
            result
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            text
        );
        assert!(result.iter().all(|span| span.highlight.is_some()));
    }

    #[test]
    fn search_emphasis_retains_unicode_text_and_syntax_attributes() {
        use cosmic::iced::{Color, Font, widget::text::Span};
        let text = "alpha βeta tail";
        let original = Span::new(text)
            .font(Font::MONOSPACE)
            .color(Color::from_rgb(0.8, 0.2, 0.2));
        let result = highlight_matches(
            text,
            vec![original.clone()],
            &[6..11],
            Some(&(6..11)),
            &cosmic::theme::active(),
        );
        assert_eq!(
            result
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            text
        );
        assert_eq!(
            result
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<Vec<_>>(),
            ["alpha ", "βeta", " tail"]
        );
        assert!(
            result
                .iter()
                .all(|span| span.font == original.font && span.color == original.color)
        );
        assert!(result[0].highlight.is_none());
        assert!(result[1].highlight.is_some());
        assert!(result[2].highlight.is_none());
    }
}
