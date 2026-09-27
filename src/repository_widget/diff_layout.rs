//! Diff coordinates independent of mounted widgets. Uniform files allocate no
//! table; annotations materialize cumulative row offsets once per measurement.

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

#[cfg(test)]
mod tests {
    use super::*;

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
