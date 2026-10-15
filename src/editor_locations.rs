//! Bounded window-local source-jump history shared by ADE and ceditor.
use crate::model::{CardId, EditorNavigation};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub card: CardId,
    pub source: PathBuf,
    pub anchor: EditorNavigation,
}

#[derive(Default)]
pub struct History {
    entries: Vec<Location>,
    current: usize,
    revision: u64,
}

#[derive(Debug, Clone)]
pub struct Target { pub location: Location, revision: u64, index: usize }

impl History {
    pub fn has_entries(&self) -> bool { !self.entries.is_empty() }

    pub fn record_jump(&mut self, from: Location, to: Location) {
        if from.card == to.card && from.source == to.source
            && from.anchor.caret == to.anchor.caret && from.anchor.selection == to.anchor.selection { return; }
        if self.entries.is_empty() { self.entries.push(from); self.current = 0; }
        else {
            self.entries.truncate(self.current + 1);
            let current = &self.entries[self.current];
            if current.card == from.card && current.source == from.source { self.entries[self.current] = from; }
            else { self.entries.push(from); self.current += 1; }
        }
        self.entries.push(to);
        self.current += 1;
        while self.entries.len() > 128 { self.entries.remove(0); self.current -= 1; }
        self.revision = self.revision.wrapping_add(1);
    }

    /// Manual tab/file focus is an origin too; do not overwrite the previous
    /// completed destination when navigating from another mounted source.
    pub fn observe_origin(&mut self, from: &Location) {
        if let Some(current) = self.entries.get(self.current)
            && (current.card != from.card || current.source != from.source) {
            self.entries.truncate(self.current + 1);
            self.entries.push(from.clone()); self.current += 1;
            while self.entries.len() > 128 { self.entries.remove(0); self.current -= 1; }
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn is_current_target(&self, target: &Target) -> bool {
        target.revision == self.revision && self.entries.get(target.index) == Some(&target.location)
    }

    pub fn peek(&self, forward: bool, mut available: impl FnMut(&Location) -> bool) -> Option<Target> {
        let index = if forward {
            (self.current + 1..self.entries.len()).find(|index| available(&self.entries[*index]))
        } else {
            (0..self.current).rev().find(|index| available(&self.entries[*index]))
        }?;
        Some(Target { location: self.entries[index].clone(), revision: self.revision, index })
    }

    /// Call only after the frontend has admitted a target load. A failed or
    /// rejected navigation does not consume the Back/Forward entry.
    pub fn navigate(&mut self, from: Location, target: &Target) -> bool {
        if target.revision != self.revision || self.entries.get(target.index) != Some(&target.location) { return false; }
        self.entries[self.current] = from;
        self.current = target.index;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// A dirty source can require opening a dedicated card. Keep the same
    /// source-history position while adopting the card that actually owns it.
    pub fn replace_current(&mut self, current: Location) {
        if let Some(entry) = self.entries.get_mut(self.current) { *entry = current; self.revision = self.revision.wrapping_add(1); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EditorPosition;
    fn location(card: CardId, name: &str, line: usize) -> Location {
        Location { card, source: name.into(), anchor: EditorNavigation { caret: EditorPosition { line, column: 0 }, selection: None, scroll_line: line, scroll_vertical: 0.0, scroll_horizontal: 0.0 } }
    }
    #[test]
    fn cross_card_history_keeps_live_positions_and_a_new_jump_retires_forward() {
        let a = CardId::new(); let b = CardId::new(); let c = CardId::new();
        let mut history = History::default();
        history.record_jump(location(a, "a", 2), location(b, "b", 4));
        let target = history.peek(false, |_| true).unwrap().clone();
        assert!(history.navigate(location(b, "b", 8), &target));
        assert_eq!(history.peek(true, |_| true).unwrap().location.anchor.caret.line, 8);
        history.replace_current(location(c, "a", 2));
        history.record_jump(location(c, "a", 3), location(a, "next", 9));
        assert!(history.peek(true, |_| true).is_none());
        assert_eq!(history.peek(false, |_| true).unwrap().location.card, c);
    }
    #[test]
    fn a_new_origin_in_another_card_keeps_the_previous_completed_destination() {
        let a = CardId::new(); let b = CardId::new(); let c = CardId::new(); let d = CardId::new();
        let mut history = History::default();
        history.record_jump(location(a, "a", 1), location(b, "b", 2));
        history.record_jump(location(c, "c", 3), location(d, "d", 4));
        let c_target = history.peek(false, |_| true).unwrap();
        assert_eq!(c_target.location.card, c);
        assert!(history.navigate(location(d, "d", 4), &c_target));
        let b_target = history.peek(false, |_| true).unwrap();
        assert_eq!(b_target.location.card, b);
        assert!(history.navigate(location(c, "c", 3), &b_target));
        assert_eq!(history.peek(false, |_| true).unwrap().location.card, a);
    }

    #[test]
    fn manual_focus_before_back_keeps_the_completed_destination() {
        let a = CardId::new(); let b = CardId::new(); let c = CardId::new();
        let mut history = History::default();
        history.record_jump(location(a, "a", 1), location(b, "b", 2));
        history.observe_origin(&location(c, "c", 3));
        let target = history.peek(false, |_| true).unwrap();
        assert_eq!(target.location.card, b);
        assert!(history.navigate(location(c, "c", 3), &target));
        assert_eq!(history.peek(false, |_| true).unwrap().location.card, a);
    }

    #[test]
    fn closed_cards_are_skipped_and_history_is_bounded() {
        let a = CardId::new(); let b = CardId::new(); let c = CardId::new();
        let mut history = History::default();
        history.record_jump(location(a, "a", 0), location(b, "b", 0));
        history.record_jump(location(b, "b", 1), location(c, "c", 0));
        assert_eq!(history.peek(false, |entry| entry.card != b).unwrap().location.card, a);
        for index in 0..200 { history.record_jump(location(a, "a", index), location(c, "c", index)); }
        assert_eq!(history.entries.len(), 128);
    }
}
