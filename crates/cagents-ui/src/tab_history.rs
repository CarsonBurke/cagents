//! Most-recently-used navigation, with a stable order while a modifier is held.
use std::{collections::HashSet, hash::Hash};

use cosmic::iced::keyboard::{self, Key, key::Named};

#[derive(Clone, Debug)]
pub struct TabHistory<T> {
    recent: Vec<T>,
    cycle: Option<(Vec<T>, usize)>,
}

impl<T> Default for TabHistory<T> {
    fn default() -> Self {
        Self {
            recent: Vec::new(),
            cycle: None,
        }
    }
}

impl<T: Copy + Eq + Hash> TabHistory<T> {
    pub fn from_recent(recent: impl IntoIterator<Item = T>) -> Self {
        let mut history = Self::default();
        let mut seen = HashSet::new();
        for id in recent {
            if seen.insert(id) {
                history.recent.push(id);
            }
        }
        history
    }

    pub fn truncate_recent(&mut self, limit: usize) {
        self.recent.truncate(limit);
    }

    pub fn recent(&self) -> &[T] {
        &self.recent
    }

    pub fn cycle_index(&self) -> Option<usize> {
        self.cycle.as_ref().map(|(_, index)| *index)
    }

    /// Move inside an already frozen cycle. The owner validates the chosen
    /// destination at commit, rather than rescanning all destinations per key.
    pub fn advance(&mut self, backwards: bool) -> Option<T> {
        let (order, index) = self.cycle.as_mut()?;
        *index = if backwards {
            (*index + order.len() - 1) % order.len()
        } else {
            (*index + 1) % order.len()
        };
        Some(order[*index])
    }

    pub fn select_index(&mut self, selected: usize) -> Option<T> {
        let (order, index) = self.cycle.as_mut()?;
        let destination = *order.get(selected)?;
        *index = selected;
        Some(destination)
    }

    pub fn cycle_order(&self) -> Option<&[T]> {
        self.cycle.as_ref().map(|(order, _)| order.as_slice())
    }
    pub fn observe(&mut self, active: Option<T>) {
        let Some(active) = active else { return };
        if let Some((order, index)) = &self.cycle {
            if order.get(*index) == Some(&active) {
                return;
            }
            self.cycle = None;
        }
        if self.recent.first() == Some(&active) {
            return;
        }
        self.recent.retain(|id| *id != active);
        self.recent.insert(0, active);
    }

    pub fn finish(&mut self, active: Option<T>) {
        self.cycle = None;
        self.observe(active);
    }

    /// Retire closed identities without forgetting history in another scope.
    pub fn retain(&mut self, mut live: impl FnMut(T) -> bool) {
        self.recent.retain(|id| live(*id));
    }

    pub fn step(&mut self, available: &[T], active: Option<T>, backwards: bool) -> Option<T> {
        if available.is_empty() {
            self.cycle = None;
            return None;
        }
        let live: HashSet<_> = available.iter().copied().collect();
        if let Some((order, index)) = &mut self.cycle {
            let selected = order.get(*index).copied();
            order.retain(|id| live.contains(id));
            if order.is_empty() {
                self.cycle = None;
            } else {
                *index = selected
                    .filter(|id| order.contains(id))
                    .or(active)
                    .and_then(|id| order.iter().position(|item| *item == id))
                    .unwrap_or(0);
            }
        }
        if self.cycle.is_none() {
            let active = active.filter(|id| live.contains(id));
            self.observe(active);
            let mut order: Vec<_> = self
                .recent
                .iter()
                .copied()
                .filter(|id| live.contains(id))
                .collect();
            let mut seen: HashSet<_> = order.iter().copied().collect();
            for id in available {
                if seen.insert(*id) {
                    order.push(*id);
                }
            }
            let current = active.and_then(|id| order.iter().position(|item| *item == id));
            let index = current.unwrap_or(if backwards { 0 } else { order.len() - 1 });
            self.cycle = Some((order, index));
        }
        let (order, index) = self.cycle.as_mut()?;
        *index = if backwards {
            (*index + order.len() - 1) % order.len()
        } else {
            (*index + 1) % order.len()
        };
        Some(order[*index])
    }
}

pub fn is_chord(key: &Key, modifiers: keyboard::Modifiers) -> bool {
    *key == Key::Named(Named::Tab) && modifiers.control() && !modifiers.alt() && !modifiers.logo()
}

pub fn step_direction(event: &keyboard::Event) -> Option<bool> {
    match event {
        keyboard::Event::KeyPressed { key, modifiers, .. } if is_chord(key, *modifiers) => {
            Some(modifiers.shift())
        }
        _ => None,
    }
}

pub fn released(event: &keyboard::Event) -> bool {
    modifiers_released(event, keyboard::Modifiers::CTRL)
}

pub fn modifiers_released(event: &keyboard::Event, held: keyboard::Modifiers) -> bool {
    match event {
        keyboard::Event::ModifiersChanged(modifiers) => !modifiers.contains(held),
        keyboard::Event::KeyReleased {
            key: Key::Named(key),
            ..
        } => match key {
            Named::Control => held.control(),
            Named::Alt => held.alt(),
            Named::Super => held.logo(),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restored_history_deduplicates_and_frozen_selection_commits_without_preview_visits() {
        let mut history = TabHistory::from_recent([3, 2, 3, 99, 1]);
        history.retain(|id| id != 99);
        assert_eq!(history.step(&[1, 2, 3, 4], Some(3), false), Some(2));
        assert_eq!(history.select_index(3), Some(4));
        assert_eq!(history.advance(true), Some(1));
        assert_eq!(history.recent(), &[3, 2, 1]);
        assert_eq!(history.select_index(8), None);
        history.finish(Some(1));
        let mut restarted = TabHistory::from_recent(history.recent().iter().copied());
        assert_eq!(restarted.step(&[1, 2, 3, 4], Some(1), false), Some(3));
    }

    #[test]
    fn held_control_freezes_order_and_release_commits_selection() {
        let mut history = TabHistory::default();
        for id in [1, 2, 3] {
            history.observe(Some(id));
        }
        assert_eq!(history.step(&[1, 2, 3], Some(3), false), Some(2));
        history.observe(Some(2));
        assert_eq!(history.step(&[1, 2, 3], Some(2), false), Some(1));
        assert_eq!(history.step(&[1, 2, 3], Some(1), true), Some(2));
        assert_eq!(history.recent, vec![3, 2, 1]);
        history.finish(Some(2));
        assert_eq!(history.step(&[1, 2, 3], Some(2), false), Some(3));
    }
    #[test]
    fn closed_tabs_are_removed_and_mouse_navigation_ends_cycle() {
        let mut history = TabHistory::default();
        for id in [1, 2, 3] {
            history.observe(Some(id));
        }
        assert_eq!(history.step(&[1, 2, 3], Some(3), false), Some(2));
        assert_eq!(history.step(&[1, 3], Some(3), false), Some(1));
        history.observe(Some(3));
        assert_eq!(history.step(&[1, 3], Some(3), false), Some(1));
        assert_eq!(history.step(&[], None, false), None);
    }

    #[test]
    fn closing_an_unselected_candidate_preserves_the_frozen_order() {
        let mut history = TabHistory::default();
        for id in [1, 2, 3, 4] {
            history.observe(Some(id));
        }
        assert_eq!(history.step(&[1, 2, 3, 4], Some(4), false), Some(3));
        history.observe(Some(3));
        history.retain(|id| id != 2);
        assert_eq!(history.step(&[1, 3, 4], Some(3), false), Some(1));
        assert_eq!(history.recent, vec![4, 3, 1]);
        history.finish(Some(1));
        assert_eq!(history.step(&[1, 3, 4], Some(1), false), Some(4));
    }

    #[test]
    fn filtering_candidates_preserves_other_scopes_history() {
        let mut history = TabHistory::default();
        for id in [1, 3, 2, 4, 6, 5] {
            history.observe(Some(id));
        }
        assert_eq!(history.step(&[4, 5, 6], Some(5), false), Some(6));
        history.finish(Some(6));
        history.observe(Some(2));
        assert_eq!(history.step(&[1, 2, 3], Some(2), false), Some(3));
        history.finish(Some(3));
        history.observe(Some(6));
        assert_eq!(history.step(&[4, 5, 6], Some(6), false), Some(5));
    }

    #[test]
    fn no_active_identity_starts_at_the_requested_end() {
        assert_eq!(TabHistory::default().step(&[1, 2, 3], None, false), Some(1));
        assert_eq!(TabHistory::default().step(&[1, 2, 3], None, true), Some(3));
    }

    #[test]
    fn only_releasing_the_initiating_modifier_finishes_navigation() {
        use keyboard::{Event, Modifiers};
        assert!(!modifiers_released(
            &Event::ModifiersChanged(Modifiers::ALT | Modifiers::SHIFT),
            Modifiers::ALT
        ));
        assert!(!modifiers_released(
            &Event::ModifiersChanged(Modifiers::ALT),
            Modifiers::ALT
        ));
        assert!(modifiers_released(
            &Event::ModifiersChanged(Modifiers::CTRL),
            Modifiers::ALT
        ));
        assert!(!modifiers_released(
            &Event::ModifiersChanged(Modifiers::CTRL | Modifiers::SHIFT),
            Modifiers::CTRL
        ));
        assert!(modifiers_released(
            &Event::ModifiersChanged(Modifiers::SHIFT),
            Modifiers::CTRL
        ));
    }
}
