//! Transient row selection, separate from the active workspace.

use std::collections::BTreeSet;

use crate::model::{ChannelId, ProjectId, WorktreeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Row {
    Project(ProjectId),
    Worktree(ProjectId, WorktreeId),
    Channel(ChannelId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Replace,
    Toggle,
    Range { extend: bool },
    Context,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Selection {
    rows: BTreeSet<Row>,
    anchor: Option<Row>,
}

impl Selection {
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
    pub(super) fn first(&self) -> Option<Row> {
        self.rows.first().copied()
    }
    pub(super) fn group_size(&self, row: Row) -> Option<usize> {
        if !self.contains(row) {
            return Some(1);
        }
        self.rows
            .iter()
            .all(|other| std::mem::discriminant(other) == std::mem::discriminant(&row))
            .then_some(self.rows.len())
    }
    pub(super) fn contains(&self, row: Row) -> bool {
        self.rows.contains(&row)
    }

    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.anchor = None;
    }

    pub(super) fn retain_visible(&mut self, visible: &[Row]) {
        let visible = visible.iter().copied().collect::<BTreeSet<_>>();
        self.rows.retain(|row| visible.contains(row));
        self.anchor = self.anchor.filter(|row| visible.contains(row));
    }

    pub(super) fn ordered(&self, visible: &[Row]) -> Vec<Row> {
        visible
            .iter()
            .copied()
            .filter(|row| self.contains(*row))
            .collect()
    }

    pub(super) fn select(&mut self, row: Row, intent: Intent, visible: &[Row]) {
        self.retain_visible(visible);
        let Some(end) = visible.iter().position(|candidate| *candidate == row) else {
            return;
        };
        match intent {
            Intent::Context if self.rows.contains(&row) => {}
            Intent::Replace | Intent::Context => {
                self.rows.clear();
                self.rows.insert(row);
                self.anchor = Some(row);
            }
            Intent::Toggle => {
                if !self.rows.remove(&row) {
                    self.rows.insert(row);
                }
                self.anchor = Some(row);
            }
            Intent::Range { extend } => {
                let start = self
                    .anchor
                    .and_then(|anchor| visible.iter().position(|row| *row == anchor))
                    .unwrap_or(end);
                if !extend {
                    self.rows.clear();
                }
                self.rows
                    .extend(visible[start.min(end)..=start.max(end)].iter().copied());
                self.anchor.get_or_insert(row);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_uses_visible_order_and_context_preserves_the_group() {
        let rows = (0..4)
            .map(|_| Row::Channel(ChannelId::new()))
            .collect::<Vec<_>>();
        let mut selection = Selection::default();
        selection.select(rows[1], Intent::Replace, &rows);
        selection.select(rows[3], Intent::Range { extend: false }, &rows);
        assert_eq!(selection.ordered(&rows), rows[1..]);
        selection.select(rows[2], Intent::Context, &rows);
        assert_eq!(selection.ordered(&rows), rows[1..]);
        selection.select(rows[0], Intent::Toggle, &rows);
        assert_eq!(selection.ordered(&rows), rows);
        selection.select(rows[1], Intent::Toggle, &rows);
        assert_eq!(selection.ordered(&rows), vec![rows[0], rows[2], rows[3]]);
    }

    #[test]
    fn removed_or_hidden_rows_cannot_be_targets_or_range_anchors() {
        let first = Row::Project(ProjectId::new());
        let second = Row::Project(ProjectId::new());
        let mut selection = Selection::default();
        selection.select(first, Intent::Replace, &[first, second]);
        selection.select(second, Intent::Range { extend: true }, &[second]);
        assert_eq!(selection.ordered(&[first, second]), vec![second]);
        assert_eq!(selection.anchor, Some(second));
    }
}
