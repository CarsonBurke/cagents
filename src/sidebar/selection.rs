//! Explicit multi-row selection, separate from the active workspace.
//!
//! Navigation and selection are different things: opening a row makes it the
//! active workspace and only moves the range anchor, while the selection holds
//! rows the user picked on purpose with Ctrl or Shift. The selection is
//! therefore empty almost all of the time. Shared context-menu actions and
//! keyboard deletion target the group only when the user explicitly selects it.

use std::{collections::BTreeSet, sync::{Arc, OnceLock}};

use crate::model::{ChannelId, ProjectId, WorktreeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Row {
    Project(ProjectId),
    Worktree(ProjectId, WorktreeId),
    Channel(ChannelId),
}

impl Row {
    /// Projects leave cagents and stay on disk, so they are removed; the other
    /// kinds are deleted.
    pub fn deletion_verb(self) -> &'static str {
        match self {
            Row::Project(_) => "Remove",
            Row::Worktree(..) | Row::Channel(_) => "Delete",
        }
    }

    pub fn noun(self, count: usize) -> &'static str {
        match (self, count == 1) {
            (Row::Project(_), true) => "project",
            (Row::Project(_), false) => "projects",
            (Row::Worktree(..), true) => "worktree",
            (Row::Worktree(..), false) => "worktrees",
            (Row::Channel(_), true) => "channel",
            (Row::Channel(_), false) => "channels",
        }
    }

    /// The command deleting `count` rows of this kind, as buttons name it:
    /// "Remove 2 projects", "Delete 1 worktree".
    pub fn deletion_label(self, count: usize) -> String {
        format!("{} {count} {}", self.deletion_verb(), self.noun(count))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// A plain click: clears any explicit selection and anchors a later
    /// Shift-click range at this row, without selecting it.
    Navigate,
    /// Ctrl-click: adds or removes one row.
    Toggle,
    /// Shift-click: selects from the anchor to this row, keeping the existing
    /// selection when Ctrl is also held.
    Range { extend: bool },
}

#[derive(Debug, Clone, Default)]
pub(super) struct Selection {
    rows: BTreeSet<Row>,
    anchor: Option<Row>,
    context_snapshot: OnceLock<Arc<[Row]>>,
}

impl PartialEq for Selection {
    fn eq(&self, other: &Self) -> bool { self.rows == other.rows && self.anchor == other.anchor }
}
impl Eq for Selection {}

impl Selection {
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub(super) fn context_rows(&self, row: Row) -> Option<Arc<[Row]>> {
        // Single-row menus already carry the row ID. Share the empty marker
        // instead of allocating for every visible row, and share group intent
        // across all of its menus until the explicit selection changes.
        static SINGLE: OnceLock<Arc<[Row]>> = OnceLock::new();
        if !self.contains(row) || self.rows.len() == 1 {
            return Some(Arc::clone(SINGLE.get_or_init(Arc::default)));
        }
        self.group_size(row).map(|_| Arc::clone(self.context_snapshot.get_or_init(|| self.rows.iter().copied().collect())))
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
        self.context_snapshot.take();
        self.rows.clear();
        self.anchor = None;
    }

    pub(super) fn retain_visible(&mut self, visible: &[Row]) {
        self.context_snapshot.take();
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

    /// Applies one selection gesture. `visible` is the drawn row order; a
    /// plain click reads none of it, so a caller may pass an empty slice for
    /// [`Intent::Navigate`] rather than derive it.
    pub(super) fn select(&mut self, row: Row, intent: Intent, visible: &[Row]) {
        let extend = match intent {
            Intent::Navigate => {
                // A stale anchor is harmless: a later range resolves it
                // against the rows visible then, or starts at its own row.
                self.clear();
                self.anchor = Some(row);
                return;
            }
            Intent::Toggle => None,
            Intent::Range { extend } => Some(extend),
        };
        self.retain_visible(visible);
        let Some(end) = visible.iter().position(|candidate| *candidate == row) else {
            return;
        };
        match extend {
            None => {
                if !self.rows.remove(&row) {
                    self.rows.insert(row);
                }
                self.anchor = Some(row);
            }
            Some(extend) => {
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
    fn navigation_anchors_ranges_without_selecting_and_toggles_are_explicit() {
        let rows = (0..4)
            .map(|_| Row::Channel(ChannelId::new()))
            .collect::<Vec<_>>();
        let mut selection = Selection::default();
        selection.select(rows[1], Intent::Navigate, &[]);
        assert!(selection.is_empty(), "opening a row selects nothing");
        selection.select(rows[3], Intent::Range { extend: false }, &rows);
        assert_eq!(
            selection.ordered(&rows),
            rows[1..],
            "Shift-click ranges from the row last opened"
        );
        selection.select(rows[0], Intent::Toggle, &rows);
        assert_eq!(selection.ordered(&rows), rows);
        selection.select(rows[1], Intent::Toggle, &rows);
        assert_eq!(selection.ordered(&rows), vec![rows[0], rows[2], rows[3]]);
        selection.select(rows[2], Intent::Navigate, &[]);
        assert!(
            selection.is_empty(),
            "a plain click ends the multi-selection"
        );
        selection.select(rows[0], Intent::Toggle, &rows);
        assert_eq!(
            selection.ordered(&rows),
            vec![rows[0]],
            "Ctrl-click adds only the clicked row, never the active one"
        );
    }

    #[test]
    fn removed_or_hidden_rows_cannot_be_targets_or_range_anchors() {
        let first = Row::Project(ProjectId::new());
        let second = Row::Project(ProjectId::new());
        let mut selection = Selection::default();
        selection.select(first, Intent::Toggle, &[first, second]);
        selection.select(second, Intent::Range { extend: true }, &[second]);
        assert_eq!(selection.ordered(&[first, second]), vec![second]);
        assert_eq!(selection.anchor, Some(second));
    }
}
