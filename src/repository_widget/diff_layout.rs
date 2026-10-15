//! Diff coordinates independent of mounted widgets. Uniform files allocate no
//! table; annotations materialize cumulative row offsets once per measurement.
//!
//! Long runs of unchanged rows are folded behind a single row. Folding maps
//! the model's rows to *display* rows, which are what [`RowOffsets`] measures
//! and what the view mounts; anchors that outlive a layout are kept in model
//! rows so that expanding a fold, or rebuilding the model, cannot move them.

use std::ops::Range;

use super::FoldExpansion;
use crate::diff_view::{DiffMode, DiffModel};

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RowOffsets {
    rows: usize,
    line_height: f32,
    leading: f32,
    annotations: std::collections::BTreeMap<usize, f32>,
    offsets: Vec<f64>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Anchor {
    Leading(f64),
    Row { row: usize, fraction: f64 },
}

#[cfg(test)]
impl Anchor {
    fn row(self) -> usize {
        match self {
            Self::Row { row, .. } => row,
            Self::Leading(_) => 0,
        }
    }
}

impl RowOffsets {
    pub fn uniform(rows: usize, line_height: f32) -> Self {
        assert!(line_height.is_finite() && line_height > 0.0);
        Self {
            rows,
            line_height,
            leading: 0.0,
            annotations: Default::default(),
            offsets: Vec::new(),
        }
    }

    pub fn annotations(&self) -> &std::collections::BTreeMap<usize, f32> {
        &self.annotations
    }

    pub fn offset(&self, row: usize) -> f32 {
        self.position(row) as f32
    }

    fn position(&self, row: usize) -> f64 {
        if self.offsets.is_empty() {
            f64::from(self.leading) + row as f64 * f64::from(self.line_height)
        } else if row <= self.rows {
            self.offsets[row]
        } else {
            self.offsets[self.rows] + (row - self.rows) as f64 * f64::from(self.line_height)
        }
    }

    pub fn height(&self, row: usize) -> f32 {
        if self.offsets.is_empty() || row >= self.rows {
            return self.line_height;
        }
        (self.position(row + 1) - self.position(row)) as f32
    }

    pub fn total_height(&self) -> f32 {
        self.offset(self.rows)
    }

    pub fn leading_height(&self) -> f32 {
        self.leading
    }

    fn row_at(&self, offset: f64) -> usize {
        if self.offsets.is_empty() {
            ((offset - f64::from(self.leading)).max(0.0) / f64::from(self.line_height)).floor()
                as usize
        } else if offset >= self.position(self.rows) {
            self.rows.saturating_add(
                ((offset - self.position(self.rows)) / f64::from(self.line_height)).floor()
                    as usize,
            )
        } else {
            self.offsets
                .partition_point(|position| *position <= offset)
                .saturating_sub(1)
        }
    }

    pub fn anchor(&self, offset: f32) -> Anchor {
        let offset = if offset.is_finite() {
            f64::from(offset.max(0.0))
        } else {
            0.0
        };
        if self.leading > 0.0 && offset < f64::from(self.leading) {
            return Anchor::Leading(offset / f64::from(self.leading));
        }
        let row = self.row_at(offset);
        Anchor::Row {
            row,
            fraction: (offset - self.position(row)) / f64::from(self.height(row)),
        }
    }

    pub fn restore(&self, anchor: Anchor) -> f32 {
        match anchor {
            Anchor::Leading(fraction) => (fraction * f64::from(self.leading)) as f32,
            Anchor::Row { row, fraction } => {
                (self.position(row) + fraction * f64::from(self.height(row))) as f32
            }
        }
    }

    pub fn visible(&self, offset: f32, height: f32, overscan: usize) -> std::ops::Range<usize> {
        let offset = if offset.is_finite() {
            f64::from(offset.max(0.0))
        } else {
            0.0
        };
        let first = self.row_at(offset);
        let last = self.row_at(offset + f64::from(height.max(0.0)));
        first.saturating_sub(overscan).min(self.rows)
            ..last
                .saturating_add(1)
                .saturating_add(overscan)
                .min(self.rows)
    }

    /// Replaces all measured annotation blocks, indexed by the code row they
    /// follow. Invalid measurements cannot poison scroll geometry. Duplicate
    /// rows use the last measurement; callers aggregate multiple threads there.
    pub fn annotate(&mut self, leading: f32, measurements: &[(usize, f32)]) -> bool {
        // A measurement is a physical widget extent. Refuse impossible values
        // rather than overflowing the f32 coordinates consumed by iced.
        let valid = |height: f32| height.is_finite() && (0.0..=1_000_000.0).contains(&height);
        let leading = if valid(leading) { leading } else { 0.0 };
        let mut extra = std::collections::BTreeMap::new();
        for &(row, height) in measurements {
            if row < self.rows && valid(height) {
                extra.insert(row, height);
            }
        }
        extra.retain(|_, height| *height > 0.0);
        if self.leading == leading && self.annotations == extra {
            return false;
        }
        let offsets = if extra.is_empty() {
            Vec::new()
        } else {
            let mut offsets = Vec::with_capacity(self.rows + 1);
            let mut sum = f64::from(leading);
            offsets.push(sum);
            let mut extra = extra.iter().peekable();
            for row in 0..self.rows {
                sum += f64::from(self.line_height);
                if extra.peek().is_some_and(|(index, _)| **index == row) {
                    sum += f64::from(*extra.next().expect("measured row").1);
                }
                offsets.push(sum);
            }
            offsets
        };
        self.leading = leading;
        self.annotations = extra;
        self.offsets = offsets;
        true
    }
}

/// Unchanged rows kept beside every change, as `git diff` does by default.
pub(super) const FOLD_CONTEXT: usize = 3;
/// A fold stands in for its rows with one row of its own, so hiding only a
/// handful would cost nearly as much height as it saves.
pub(super) const FOLD_MIN_HIDDEN: usize = 4;
/// How many rows one directional expansion reveals.
pub(super) const FOLD_STEP: usize = 20;

/// What the reader has revealed of each unchanged run, from its top and from
/// its bottom. Run `i` names the same lines in both layouts, so one record
/// serves inline and split alike and survives switching between them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FoldReveals(Vec<[usize; 2]>);

impl FoldReveals {
    pub fn expand(&mut self, fold: usize, expansion: FoldExpansion) {
        if self.0.len() <= fold {
            self.0.resize(fold + 1, [0; 2]);
        }
        let revealed = &mut self.0[fold];
        match expansion {
            FoldExpansion::Down => revealed[0] = revealed[0].saturating_add(FOLD_STEP),
            FoldExpansion::Up => revealed[1] = revealed[1].saturating_add(FOLD_STEP),
            FoldExpansion::All => *revealed = [usize::MAX; 2],
        }
    }

    fn get(&self, fold: usize) -> [usize; 2] {
        self.0.get(fold).copied().unwrap_or_default()
    }
}

/// One display row: a model row, or the fold standing in for hidden rows of
/// unchanged run `fold`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DisplayRow {
    Code(usize),
    Fold { fold: usize, rows: Range<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hidden {
    fold: usize,
    rows: Range<usize>,
    /// The display row the fold occupies.
    display: usize,
}

/// The model-to-display row mapping of one layout. It stores one entry per
/// collapsed fold, never one per row, so a 100k-row file costs a few bytes
/// per fold and every lookup is a binary search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FoldedRows {
    rows: usize,
    len: usize,
    hidden: Vec<Hidden>,
}

impl FoldedRows {
    /// Folds every run of unchanged rows that leaves at least
    /// [`FOLD_MIN_HIDDEN`] rows once [`FOLD_CONTEXT`] rows are kept beside
    /// each neighbouring change and the reader's reveals are shown. A run at
    /// either end of the file has no change on that side to keep context for.
    pub fn new(rows: usize, runs: &[Range<usize>], reveals: &FoldReveals) -> Self {
        let mut hidden = Vec::new();
        let mut removed = 0;
        for (fold, run) in runs.iter().enumerate() {
            let [top, bottom] = reveals.get(fold);
            let context = |edge: bool| if edge { 0 } else { FOLD_CONTEXT };
            let start = run
                .start
                .saturating_add(context(run.start == 0))
                .saturating_add(top);
            let end = run
                .end
                .min(rows)
                .saturating_sub(context(run.end >= rows))
                .saturating_sub(bottom);
            if end.saturating_sub(start) < FOLD_MIN_HIDDEN {
                continue;
            }
            hidden.push(Hidden {
                fold,
                rows: start..end,
                display: start - removed,
            });
            removed += end - start - 1;
        }
        Self {
            rows,
            len: rows - removed,
            hidden,
        }
    }

    /// The layout of a model with nothing folded.
    pub fn unfolded(rows: usize) -> Self {
        Self {
            rows,
            len: rows,
            hidden: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// What display row `display` shows. Rows past the end extend the last
    /// row linearly, as [`RowOffsets`] extends its geometry.
    pub fn row(&self, display: usize) -> DisplayRow {
        let index = self
            .hidden
            .partition_point(|hidden| hidden.display <= display);
        let Some(hidden) = index.checked_sub(1).map(|index| &self.hidden[index]) else {
            return DisplayRow::Code(display);
        };
        if hidden.display == display {
            DisplayRow::Fold {
                fold: hidden.fold,
                rows: hidden.rows.clone(),
            }
        } else {
            DisplayRow::Code(hidden.rows.end + (display - hidden.display - 1))
        }
    }

    /// The display row showing model row `row`: the row itself, or the fold
    /// hiding it.
    pub fn display(&self, row: usize) -> usize {
        let index = self
            .hidden
            .partition_point(|hidden| hidden.rows.start <= row);
        let Some(hidden) = index.checked_sub(1).map(|index| &self.hidden[index]) else {
            return row;
        };
        if row < hidden.rows.end {
            hidden.display
        } else {
            hidden.display + 1 + (row - hidden.rows.end)
        }
    }

    /// The model row a display row stands for: a fold stands for the first
    /// row it hides.
    fn model(&self, display: usize) -> usize {
        match self.row(display) {
            DisplayRow::Code(row) => row,
            DisplayRow::Fold { rows, .. } => rows.start,
        }
    }

    /// Re-expresses a display anchor in model rows, which folding cannot move.
    pub fn to_model(&self, anchor: Anchor) -> Anchor {
        match anchor {
            Anchor::Row { row, fraction } => Anchor::Row {
                row: self.model(row),
                fraction,
            },
            leading => leading,
        }
    }

    /// The inverse of [`FoldedRows::to_model`]. A row that is now hidden
    /// resolves to the fold that hides it.
    pub fn to_display(&self, anchor: Anchor) -> Anchor {
        match anchor {
            Anchor::Row { row, fraction } => Anchor::Row {
                row: self.display(row),
                fraction,
            },
            leading => leading,
        }
    }
}

/// Everything a comparison needs to place its rows: what the reader has
/// unfolded, and for each layout the folded mapping and its geometry.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DiffLayout {
    folding: bool,
    reveals: FoldReveals,
    folded: [FoldedRows; 2],
    offsets: [RowOffsets; 2],
    row_height: f32,
}

fn layout_index(mode: DiffMode) -> usize {
    usize::from(mode == DiffMode::Split)
}

impl DiffLayout {
    /// `model` is `None` for a comparison without text rows. A comparison
    /// without changes is not folded: there is nothing to fold towards.
    pub fn new(model: Option<&DiffModel>, row_height: f32) -> Self {
        let reveals = FoldReveals::default();
        let folded =
            [DiffMode::Inline, DiffMode::Split].map(|mode| Self::fold(model, mode, &reveals));
        let offsets = folded
            .each_ref()
            .map(|folded| RowOffsets::uniform(folded.len(), row_height));
        Self {
            folding: true,
            reveals,
            folded,
            offsets,
            row_height,
        }
    }

    fn fold(model: Option<&DiffModel>, mode: DiffMode, reveals: &FoldReveals) -> FoldedRows {
        match model {
            Some(model) if !model.is_empty() => {
                FoldedRows::new(model.row_count(mode), model.unchanged_runs(mode), reveals)
            }
            Some(model) => FoldedRows::unfolded(model.row_count(mode)),
            None => FoldedRows::unfolded(0),
        }
    }

    /// Carries `previous`'s reveals into this freshly built layout when the
    /// model was rebuilt in place with the same unchanged runs - a working
    /// diff rebuilt after an edit, including one that only changed lines
    /// inside existing changes - so what the reader unfolded stays unfolded.
    /// A reload through `request_diff` passes through `DiffState::Loading`
    /// and starts folded again. Runs of other lengths are different runs,
    /// and their reveals would open the wrong lines.
    pub fn carry_reveals(&mut self, model: &DiffModel, previous: (&DiffModel, &DiffLayout)) {
        let (previous_model, previous) = previous;
        let lengths = |model: &DiffModel| {
            model
                .unchanged_runs(DiffMode::Inline)
                .iter()
                .map(ExactSizeIterator::len)
                .collect::<Vec<_>>()
        };
        if previous.reveals == FoldReveals::default() || lengths(model) != lengths(previous_model) {
            return;
        }
        self.reveals = previous.reveals.clone();
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let folded = Self::fold(Some(model), mode, &self.reveals);
            self.offsets[layout_index(mode)] = RowOffsets::uniform(folded.len(), self.row_height);
            self.folded[layout_index(mode)] = folded;
        }
    }

    pub fn set_row_height(&mut self, height: f32) {
        if self.row_height == height { return; }
        self.row_height = height;
        for (index, rows) in self.offsets.iter_mut().enumerate() {
            let leading = rows.leading_height();
            let annotations = rows.annotations().iter().map(|(row, height)| (*row, *height)).collect::<Vec<_>>();
            *rows = RowOffsets::uniform(self.folded[index].len(), height);
            rows.annotate(leading, &annotations);
        }
    }

    pub fn rows(&self, mode: DiffMode) -> &RowOffsets {
        &self.offsets[layout_index(mode)]
    }

    pub fn rows_mut(&mut self, mode: DiffMode) -> &mut RowOffsets {
        &mut self.offsets[layout_index(mode)]
    }

    pub fn folded(&self, mode: DiffMode) -> &FoldedRows {
        &self.folded[layout_index(mode)]
    }

    pub fn set_folding(&mut self, model: &DiffModel, enabled: bool) -> bool {
        if self.folding == enabled {
            return false;
        }
        self.folding = enabled;
        self.rebuild(model)
    }

    /// Reveals part of fold `fold` in both layouts. Measured annotations
    /// follow the model rows they belong to, so the rows around the fold keep
    /// their review threads' heights instead of waiting to be measured again.
    pub fn expand(&mut self, model: &DiffModel, fold: usize, expansion: FoldExpansion) -> bool {
        if fold >= model.unchanged_runs(DiffMode::Inline).len() {
            return false;
        }
        self.reveals.expand(fold, expansion);
        self.rebuild(model)
    }

    fn rebuild(&mut self, model: &DiffModel) -> bool {
        let mut changed = false;
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let index = layout_index(mode);
            let folded = if self.folding {
                Self::fold(Some(model), mode, &self.reveals)
            } else {
                FoldedRows::unfolded(model.row_count(mode))
            };
            if folded == self.folded[index] {
                continue;
            }
            changed = true;
            let previous = &self.offsets[index];
            let annotations: Vec<(usize, f32)> = previous
                .annotations()
                .iter()
                .map(|(row, height)| (folded.display(self.folded[index].model(*row)), *height))
                .collect();
            let leading = previous.leading_height();
            let mut offsets = RowOffsets::uniform(folded.len(), self.row_height);
            offsets.annotate(leading, &annotations);
            self.folded[index] = folded;
            self.offsets[index] = offsets;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_preference_preserves_reveals_and_logical_annotation_rows() {
        let before = (0..100)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let after = before.replace("line 50\n", "changed\n");
        let model = crate::diff_view::build_diff(&before, &after);
        let mut layout = DiffLayout::new(Some(&model), 20.0);
        let initial_rows = layout.folded(DiffMode::Inline).len();
        assert!(initial_rows < model.row_count(DiffMode::Inline));
        let annotated = layout.folded(DiffMode::Inline).display(50);
        layout
            .rows_mut(DiffMode::Inline)
            .annotate(15.0, &[(annotated, 80.0)]);
        assert!(layout.set_folding(&model, false));
        assert_eq!(
            layout.folded(DiffMode::Inline).len(),
            model.row_count(DiffMode::Inline)
        );
        assert_eq!(
            layout.rows(DiffMode::Inline).annotations().get(&50),
            Some(&80.0)
        );
        assert_eq!(layout.rows(DiffMode::Inline).leading_height(), 15.0);
        assert!(layout.set_folding(&model, true));
        assert_eq!(layout.folded(DiffMode::Inline).len(), initial_rows);
        assert_eq!(
            layout.rows(DiffMode::Inline).annotations().get(&annotated),
            Some(&80.0)
        );
        assert!(layout.expand(&model, 0, FoldExpansion::All));
        let expanded_rows = layout.folded(DiffMode::Inline).len();
        layout.set_folding(&model, false);
        layout.set_folding(&model, true);
        assert_eq!(layout.folded(DiffMode::Inline).len(), expanded_rows);
    }

    /// Changes at rows 40..42 and 70 of a 100-row model.
    fn runs() -> Vec<Range<usize>> {
        vec![0..40, 42..70, 71..100]
    }

    fn fold(folded: &FoldedRows, display: usize) -> Option<Range<usize>> {
        match folded.row(display) {
            DisplayRow::Fold { rows, .. } => Some(rows),
            DisplayRow::Code(_) => None,
        }
    }

    #[test]
    fn folds_keep_context_beside_changes_and_none_past_the_file_edges() {
        let folded = FoldedRows::new(100, &runs(), &FoldReveals::default());
        // The leading run keeps only the context above the first change; the
        // trailing run only the context below the last.
        assert_eq!(fold(&folded, 0), Some(0..37));
        assert_eq!(folded.row(1), DisplayRow::Code(37));
        assert_eq!(folded.display(40), 4);
        assert_eq!(fold(&folded, folded.display(50)), Some(45..67));
        assert_eq!(fold(&folded, folded.display(99)), Some(74..100));
        assert_eq!(folded.len(), 100 - 36 - 21 - 25);
        assert_eq!(
            folded.row(folded.len() - 1),
            DisplayRow::Fold {
                fold: 2,
                rows: 74..100,
            }
        );

        // Three rows before the first change, six between two changes and
        // three after the last are all context; ten leave four to fold.
        let short = FoldedRows::new(20, &[0..3, 4..10, 17..20], &FoldReveals::default());
        assert_eq!(short.len(), 20);
        let long = FoldedRows::new(20, &[4..14], &FoldReveals::default());
        assert_eq!(long.len(), 17);
        assert_eq!(fold(&long, 7), Some(7..11));
    }

    #[test]
    fn display_and_model_rows_round_trip() {
        let mut reveals = FoldReveals::default();
        reveals.expand(1, FoldExpansion::Down);
        for reveals in [FoldReveals::default(), reveals] {
            let folded = FoldedRows::new(100, &runs(), &reveals);
            for display in 0..folded.len() {
                assert_eq!(folded.display(folded.model(display)), display);
            }
            for row in 0..100 {
                match folded.row(folded.display(row)) {
                    DisplayRow::Code(code) => assert_eq!(code, row),
                    DisplayRow::Fold { rows, .. } => assert!(rows.contains(&row)),
                }
            }
            // Past the end, both directions extend linearly.
            assert_eq!(folded.row(folded.len() + 2), DisplayRow::Code(102));
            assert_eq!(folded.display(102), folded.len() + 2);
        }
    }

    #[test]
    fn reveals_open_folds_from_either_side_and_close_them_when_little_remains() {
        let mut reveals = FoldReveals::default();
        reveals.expand(0, FoldExpansion::Up);
        let folded = FoldedRows::new(100, &runs(), &reveals);
        assert_eq!(fold(&folded, 0), Some(0..17));
        reveals.expand(1, FoldExpansion::Down);
        let folded = FoldedRows::new(100, &runs(), &reveals);
        // 45..67 less twenty from the top leaves two rows: not worth a fold.
        assert_eq!(folded.display(50) - folded.display(45), 5);
        assert!((45..67).all(|row| folded.row(folded.display(row)) == DisplayRow::Code(row)));
        reveals.expand(2, FoldExpansion::All);
        reveals.expand(0, FoldExpansion::All);
        assert_eq!(FoldedRows::new(100, &runs(), &reveals).len(), 100);
        // A reveal past the model's runs is inert.
        reveals.expand(9, FoldExpansion::All);
        assert_eq!(FoldedRows::new(100, &runs(), &reveals).len(), 100);
    }

    #[test]
    fn anchors_survive_folding_changes_in_model_rows() {
        let folded = FoldedRows::new(100, &runs(), &FoldReveals::default());
        let rows = RowOffsets::uniform(folded.len(), 30.0);
        // Halfway through model row 68, three rows below the second fold.
        let offset = rows.offset(folded.display(68)) + 15.0;
        let anchor = folded.to_model(rows.anchor(offset));
        assert!(matches!(anchor, Anchor::Row { row: 68, fraction } if fraction == 0.5));

        let mut reveals = FoldReveals::default();
        reveals.expand(1, FoldExpansion::Up);
        let unfolded = FoldedRows::new(100, &runs(), &reveals);
        let rows = RowOffsets::uniform(unfolded.len(), 30.0);
        let restored = rows.restore(unfolded.to_display(anchor));
        assert_eq!(restored, rows.offset(unfolded.display(68)) + 15.0);
        assert_eq!(
            unfolded.row(rows.anchor(restored).row()),
            DisplayRow::Code(68)
        );

        // A hidden row resolves to the fold hiding it, and a fold to its first
        // hidden row.
        let hidden = folded.to_display(Anchor::Row {
            row: 50,
            fraction: 0.0,
        });
        assert!(matches!(hidden, Anchor::Row { row, .. } if fold(&folded, row) == Some(45..67)));
        assert!(matches!(
            folded.to_model(hidden),
            Anchor::Row { row: 45, .. }
        ));
    }

    #[test]
    fn expanding_moves_measured_threads_with_their_rows() {
        let before: String = (1..=60).map(|line| format!("line {line}\n")).collect();
        let after = before.replace("line 30\n", "changed\n");
        let model = crate::diff_view::build_diff(&before, &after);
        let mut layout = DiffLayout::new(Some(&model), 30.0);
        let inline = layout.folded(DiffMode::Inline).clone();
        // The deleted row, which is never folded.
        let deleted = inline.display(29);
        layout
            .rows_mut(DiffMode::Inline)
            .annotate(12.0, &[(deleted, 90.0)]);
        assert!(layout.expand(&model, 0, FoldExpansion::All));
        let rows = layout.rows(DiffMode::Inline);
        let display = layout.folded(DiffMode::Inline).display(29);
        assert_eq!(display, 29);
        assert_eq!(
            rows.annotations().iter().collect::<Vec<_>>(),
            [(&29, &90.0)]
        );
        assert_eq!(rows.leading_height(), 12.0);
        // Both layouts share the reveal, and a no-op reveal reports itself.
        assert_eq!(layout.folded(DiffMode::Split).row(0), DisplayRow::Code(0));
        assert!(!layout.expand(&model, 0, FoldExpansion::All));
        assert!(!layout.expand(&model, 7, FoldExpansion::All));

        let identical = crate::diff_view::build_diff(&before, &before);
        let layout = DiffLayout::new(Some(&identical), 30.0);
        assert_eq!(
            layout.folded(DiffMode::Inline).len(),
            60,
            "nothing to fold towards"
        );
    }

    #[test]
    fn variable_rows_project_viewport_with_complete_spacer_geometry() {
        let mut rows = RowOffsets::uniform(10, 30.0);
        rows.annotate(0.0, &[(1, 150.0), (5, 60.0)]);
        assert_eq!(rows.visible(200.0, 80.0, 0), 1..5);
        assert_eq!(rows.visible(200.0, 80.0, 1), 0..6);
        assert_eq!(rows.offset(2), 210.0);
        assert_eq!(rows.total_height(), 510.0);
        let visible = rows.visible(270.0, 150.0, 1);
        let mounted: f32 = visible.clone().map(|row| rows.height(row)).sum();
        assert_eq!(
            rows.offset(visible.start) + mounted + rows.total_height() - rows.offset(visible.end),
            rows.total_height()
        );
    }

    #[test]
    fn height_changes_preserve_row_and_fraction_even_inside_annotation() {
        let mut rows = RowOffsets::uniform(10, 30.0);
        rows.annotate(0.0, &[(2, 90.0)]);
        let anchor = rows.anchor(120.0); // Halfway through row 2.
        rows.annotate(0.0, &[(0, 60.0), (2, 210.0)]);
        assert_eq!(rows.restore(anchor), 240.0);
        rows.annotate(0.0, &[]);
        assert_eq!(rows.restore(anchor), 75.0);
        assert_eq!(rows.restore(rows.anchor(900.0)), 900.0);
    }

    #[test]
    fn invalid_measurements_and_empty_diffs_keep_finite_coordinates() {
        let mut rows = RowOffsets::uniform(0, 30.0);
        assert!(!rows.annotate(0.0, &[(0, 50.0)]));
        assert_eq!(rows.visible(0.0, 900.0, 6), 0..0);
        let mut rows = RowOffsets::uniform(3, 30.0);
        assert!(!rows.annotate(0.0, &[(0, f32::NAN), (1, -3.0), (2, f32::INFINITY)]));
        assert_eq!(rows.restore(rows.anchor(f32::NAN)), 0.0);
        assert_eq!(rows.visible(f32::INFINITY, 30.0, 0), 0..2);
    }

    #[test]
    fn leading_threads_preserve_code_and_header_anchors_separately() {
        let mut rows = RowOffsets::uniform(10, 30.0);
        let code = rows.anchor(75.0);
        rows.annotate(120.0, &[]);
        assert_eq!(rows.restore(code), 195.0);
        let header = rows.anchor(60.0);
        rows.annotate(240.0, &[(0, 90.0)]);
        assert_eq!(rows.restore(code), 405.0);
        assert_eq!(rows.restore(header), 120.0);
        rows.annotate(0.0, &[]);
        assert_eq!(rows.restore(header), 0.0);
    }
}
