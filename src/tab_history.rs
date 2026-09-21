//! Most-recently-used navigation, with a stable order while Control is held.
use cosmic::iced::keyboard::{self, Key, key::Named};

#[derive(Debug)]
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

impl<T: Copy + Eq> TabHistory<T> {
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

    pub fn step(&mut self, available: &[T], active: Option<T>, backwards: bool) -> Option<T> {
        self.recent.retain(|id| available.contains(id));
        if available.is_empty() {
            self.cycle = None;
            return None;
        }
        if self
            .cycle
            .as_ref()
            .is_some_and(|(order, _)| order.iter().any(|id| !available.contains(id)))
        {
            self.finish(active);
        }
        if self.cycle.is_none() {
            self.observe(active);
            let mut order = self.recent.clone();
            for id in available {
                if !order.contains(id) {
                    order.push(*id);
                }
            }
            let index = active
                .and_then(|id| order.iter().position(|item| *item == id))
                .unwrap_or(0);
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
    match event {
        keyboard::Event::ModifiersChanged(modifiers) => !modifiers.control(),
        keyboard::Event::KeyReleased {
            key: Key::Named(Named::Control),
            ..
        } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
