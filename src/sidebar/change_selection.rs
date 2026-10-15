//! Explicit Changes selection uses Git area plus byte-exact path identity.
//! Viewport rows are never the selection universe: ranges and bulk commands
//! keep working when most of a large group is not mounted.

use super::SelectionIntent;
use crate::repository::{ChangeArea, RepoPath};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Row {
    pub area: ChangeArea,
    pub path: RepoPath,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Selection {
    rows: BTreeSet<Row>,
    first_by_area: BTreeMap<ChangeArea, Row>,
    anchor: Option<Row>,
}

impl Selection {
    pub(super) fn clear(&mut self) -> bool {
        let changed = !self.rows.is_empty() || self.anchor.is_some();
        self.rows.clear();
        self.first_by_area.clear();
        self.anchor = None;
        changed
    }
    pub(super) fn contains(&self, row: &Row) -> bool {
        self.rows.contains(row)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
    pub(super) fn retain(&mut self, mut present: impl FnMut(&Row) -> bool) -> bool {
        let count = self.rows.len();
        let anchor = self.anchor.clone();
        self.rows.retain(&mut present);
        if self.anchor.as_ref().is_some_and(|row| !present(row)) {
            self.anchor = None;
        }
        let changed = count != self.rows.len() || anchor != self.anchor;
        if changed {
            self.update_areas();
        }
        changed
    }
    pub(super) fn first_in_area(&self, area: ChangeArea) -> Option<&Row> {
        self.first_by_area.get(&area)
    }
    fn update_areas(&mut self) {
        self.first_by_area.clear();
        for row in &self.rows {
            self.first_by_area
                .entry(row.area)
                .or_insert_with(|| row.clone());
        }
    }

    pub(super) fn reconcile(&mut self, order: &[Row]) {
        let present = order.iter().collect::<BTreeSet<_>>();
        self.rows.retain(|row| present.contains(row));
        if self
            .anchor
            .as_ref()
            .is_some_and(|row| !present.contains(row))
        {
            self.anchor = None;
        }
        self.update_areas();
    }

    pub(super) fn targets(&self, focused: &Row, order: &[Row]) -> Vec<Row> {
        if !self.contains(focused) {
            return order
                .iter()
                .find(|row| *row == focused)
                .cloned()
                .into_iter()
                .collect();
        }
        order
            .iter()
            .filter(|row| self.contains(row))
            .cloned()
            .collect()
    }

    pub(super) fn select(&mut self, row: Row, intent: SelectionIntent, order: &[Row]) {
        self.reconcile(order);
        let Some(end) = order.iter().position(|candidate| *candidate == row) else {
            return;
        };
        match intent {
            SelectionIntent::Navigate => {
                self.rows.clear();
                self.anchor = Some(row);
            }
            SelectionIntent::Toggle => {
                if !self.rows.remove(&row) {
                    self.rows.insert(row.clone());
                }
                self.anchor = Some(row);
            }
            SelectionIntent::Range { extend } => {
                let start = self
                    .anchor
                    .as_ref()
                    .and_then(|anchor| order.iter().position(|candidate| candidate == anchor))
                    .unwrap_or(end);
                if !extend {
                    self.rows.clear();
                }
                self.rows
                    .extend(order[start.min(end)..=start.max(end)].iter().cloned());
                self.anchor.get_or_insert(row);
            }
        }
        self.update_areas();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(index: usize, area: ChangeArea) -> Row {
        Row {
            area,
            path: RepoPath::parse(format!("src/m{index}.rs")).unwrap(),
        }
    }

    #[test]
    fn range_and_toggle_use_all_logical_rows_across_viewports() {
        let order = (0..40_000)
            .map(|index| row(index, ChangeArea::Unstaged))
            .collect::<Vec<_>>();
        let mut selection = Selection::default();
        selection.select(order[3].clone(), SelectionIntent::Navigate, &order);
        assert!(selection.is_empty());
        selection.select(
            order[35_000].clone(),
            SelectionIntent::Range { extend: false },
            &order,
        );
        assert_eq!(selection.targets(&order[35_000], &order), order[3..=35_000]);
        selection.select(order[5].clone(), SelectionIntent::Toggle, &order);
        assert!(!selection.contains(&order[5]));
        assert_eq!(selection.targets(&order[5], &order), vec![order[5].clone()]);
        selection.select(
            order[0].clone(),
            SelectionIntent::Range { extend: true },
            &order,
        );
        assert_eq!(selection.targets(&order[0], &order), order[..=35_000]);
    }

    #[test]
    fn the_same_path_in_two_git_areas_is_two_distinct_targets() {
        let staged = row(1, ChangeArea::Staged);
        let unstaged = row(1, ChangeArea::Unstaged);
        let order = [staged.clone(), unstaged.clone()];
        let mut selection = Selection::default();
        selection.select(staged.clone(), SelectionIntent::Toggle, &order);
        assert!(selection.contains(&staged));
        assert!(!selection.contains(&unstaged));
        selection.select(unstaged.clone(), SelectionIntent::Toggle, &order);
        assert_eq!(selection.targets(&staged, &order), order);
        selection.reconcile(&[unstaged.clone()]);
        assert_eq!(
            selection.targets(&unstaged, &[unstaged.clone()]),
            vec![unstaged]
        );
        assert!(!selection.contains(&staged));
        selection.clear();
        assert!(selection.is_empty());
    }

    #[test]
    fn removed_targets_and_anchors_cannot_survive_a_snapshot() {
        let order = (0..3)
            .map(|index| row(index, ChangeArea::Untracked))
            .collect::<Vec<_>>();
        let mut selection = Selection::default();
        selection.select(order[0].clone(), SelectionIntent::Toggle, &order);
        selection.select(order[1].clone(), SelectionIntent::Toggle, &order);
        selection.reconcile(&order[2..]);
        selection.select(
            order[2].clone(),
            SelectionIntent::Range { extend: false },
            &order[2..],
        );
        assert_eq!(selection.targets(&order[2], &order[2..]), order[2..]);
        assert!(selection.targets(&order[0], &order[2..]).is_empty());
    }
}
