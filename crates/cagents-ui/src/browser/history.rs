//! One browser card's back/forward history while it has a live webview.
//!
//! Servo keeps its own session history for each webview, and only that history
//! can be traversed natively: a page that is in it comes back through
//! [`servo::WebView::go_back`] and [`servo::WebView::go_forward`]. A card can
//! show more than Servo holds, though. A restored card starts Servo on its
//! current entry alone while it shows every entry it saved, and stepping
//! outside what Servo holds has to load the neighbouring entry as a fresh page.
//!
//! The card's history is therefore Servo's list framed by entries Servo does
//! not have: `back ++ servo[window_start..] ++ forward`. Entries in Servo before
//! `window_start` are pages the card's history has since replaced, which Servo
//! still holds because an embedder cannot remove them.

use url::Url;

/// How to carry out a traversal that [`NavigationState::traverse`] accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HistoryStep {
    /// Servo holds the target entry: traverse its own history by one entry.
    Back,
    Forward,
    /// Servo does not hold the target entry: load it as a new page.
    Load(Url),
}

#[derive(Debug)]
pub(super) struct NavigationState {
    /// The page the card shows, which Servo reports separately from history.
    url: Url,
    /// Entries before Servo's window, which Servo does not hold.
    back: Vec<Url>,
    /// Entries after Servo's list, which Servo does not hold. A new navigation
    /// discards them, as it discards Servo's own forward entries.
    forward: Vec<Url>,
    /// Servo's history as last reported, and its current entry. The index
    /// moves ahead of the report when this card asks Servo to traverse, so a
    /// second Back pressed before Servo answers counts from the first.
    servo: Vec<Url>,
    servo_index: usize,
    /// Where the card's history begins within `servo`.
    window_start: usize,
    /// A page this card asked Servo to load, shown at its place in the history
    /// until Servo reports the entry it committed for it.
    pending: Option<PendingLoad>,
    /// The saved scroll offset applies once, to the page the card restored.
    restore_scroll: bool,
}

#[derive(Debug)]
struct PendingLoad {
    url: Url,
    /// Set when the load continues from the current entry of Servo's window,
    /// which Servo then keeps in front of the new entry. Without it, Servo's
    /// report starts a new window at the committed entry.
    window_start: Option<usize>,
}

impl NavigationState {
    /// The history of a card whose webview is about to load entry `index`.
    pub(super) fn restored(history: &[Url], index: usize) -> Self {
        let index = index.min(history.len().saturating_sub(1));
        let url = history[index].clone();
        Self {
            back: history[..index].to_vec(),
            forward: history[index + 1..].to_vec(),
            servo: Vec::new(),
            servo_index: 0,
            window_start: 0,
            pending: Some(PendingLoad {
                url: url.clone(),
                window_start: None,
            }),
            url,
            restore_scroll: true,
        }
    }

    pub(super) fn url(&self) -> &Url {
        &self.url
    }

    pub(super) fn set_url(&mut self, url: Url) {
        self.url = url;
    }

    /// Every entry the card shows, and the index of the current one, bounded as
    /// durable state is.
    pub(super) fn entries(&self) -> (Vec<Url>, usize) {
        let (mut entries, mut current) = self.all_entries();
        crate::model::trim_browser_history(&mut entries, &mut current);
        (entries, current)
    }

    fn all_entries(&self) -> (Vec<Url>, usize) {
        let window: &[Url] = match &self.pending {
            Some(pending) => std::slice::from_ref(&pending.url),
            None => &self.servo[self.window_start..],
        };
        let current = self.back.len()
            + match self.pending {
                Some(_) => 0,
                None => self.servo_index - self.window_start,
            };
        let entries = self
            .back
            .iter()
            .chain(window)
            .chain(&self.forward)
            .cloned()
            .collect();
        (entries, current)
    }

    /// Moves one entry back (`-1`) or forward (`1`), or returns `None` at that
    /// end of the history.
    pub(super) fn traverse(&mut self, delta: isize) -> Option<HistoryStep> {
        let (entries, current) = self.all_entries();
        let target = current
            .checked_add_signed(delta)
            .filter(|target| *target < entries.len() && *target != current)?;
        self.url = entries[target].clone();
        if self.pending.is_none()
            && let Some(servo_target) = self.servo_index.checked_add_signed(delta)
            && (self.window_start..self.servo.len()).contains(&servo_target)
        {
            self.servo_index = servo_target;
            return Some(if delta < 0 {
                HistoryStep::Back
            } else {
                HistoryStep::Forward
            });
        }
        self.back = entries[..target].to_vec();
        self.forward = entries[target + 1..].to_vec();
        self.pending = Some(PendingLoad {
            url: self.url.clone(),
            window_start: None,
        });
        Some(HistoryStep::Load(self.url.clone()))
    }

    /// Records a page the card loads as a new entry after the current one.
    pub(super) fn navigate(&mut self, url: Url) {
        let (mut entries, current) = self.all_entries();
        entries.truncate(current + 1);
        let window_start = self.pending.is_none().then_some(self.window_start);
        self.back = entries;
        self.forward.clear();
        self.url = url.clone();
        self.pending = Some(PendingLoad { url, window_start });
    }

    /// Applies Servo's report of its history for this webview.
    pub(super) fn servo_history_changed(&mut self, entries: Vec<Url>, current: usize) {
        let Some(last) = entries.len().checked_sub(1) else {
            return;
        };
        let current = current.min(last);
        if let Some(pending) = self.pending.take() {
            // Servo committed the page this card loaded. What Servo holds in
            // front of it is the card's history only when the load continued
            // the window and Servo kept the window's entries up to it.
            self.window_start = current;
            if let Some(start) = pending.window_start
                && let Some(kept) = current.checked_sub(start)
                && let Some(back_len) = self.back.len().checked_sub(kept)
                && entries[start..current] == self.back[back_len..]
            {
                self.back.truncate(back_len);
                self.window_start = start;
            }
        } else {
            // A traversal leaves Servo's list as it was, and a redirect or
            // `history.replaceState` rewrites only the current entry. Anything
            // else is a new navigation from the page, such as a followed link,
            // and that discards every forward entry, including the card's own.
            let same_list = entries.len() == self.servo.len()
                && entries
                    .iter()
                    .zip(&self.servo)
                    .enumerate()
                    .all(|(index, (new, old))| {
                        new == old || (index == current && current == self.servo_index)
                    });
            if !same_list {
                self.forward.clear();
            }
            // Page script can traverse into the entries this card replaced.
            // They are part of its history again.
            self.window_start = self.window_start.min(current);
        }
        self.url = entries[current].clone();
        self.servo = entries;
        self.servo_index = current;
    }

    /// Whether the page that just finished loading is the restored one, whose
    /// saved scroll offset should now be applied. True once per webview.
    pub(super) fn take_restore_scroll(&mut self) -> bool {
        std::mem::take(&mut self.restore_scroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(path: &str) -> Url {
        Url::parse(&format!("https://example.test/{path}")).unwrap()
    }

    fn urls(paths: &[&str]) -> Vec<Url> {
        paths.iter().map(|path| url(path)).collect()
    }

    fn shown(state: &NavigationState) -> (Vec<Url>, usize) {
        state.entries()
    }

    /// A fresh card on `a`, with Servo's first report in.
    fn started_on_a() -> NavigationState {
        let mut state = NavigationState::restored(&urls(&["a"]), 0);
        state.servo_history_changed(urls(&["a"]), 0);
        state
    }

    #[test]
    fn a_followed_link_can_be_traversed_back_and_forward_in_servo() {
        let mut state = started_on_a();
        // The page navigates by itself: no command from the card announced it.
        state.servo_history_changed(urls(&["a", "b"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b"]), 1));

        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.url(), &url("a"));
        assert_eq!(shown(&state), (urls(&["a", "b"]), 0));
        state.servo_history_changed(urls(&["a", "b"]), 0);
        assert_eq!(shown(&state), (urls(&["a", "b"]), 0));
        assert_eq!(state.traverse(-1), None);

        assert_eq!(state.traverse(1), Some(HistoryStep::Forward));
        state.servo_history_changed(urls(&["a", "b"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b"]), 1));
        assert_eq!(state.url(), &url("b"));
        assert_eq!(state.traverse(1), None);
    }

    #[test]
    fn a_second_back_before_servo_answers_counts_from_the_first() {
        let mut state = started_on_a();
        state.servo_history_changed(urls(&["a", "b"]), 1);
        state.servo_history_changed(urls(&["a", "b", "c"]), 2);
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), None);
        state.servo_history_changed(urls(&["a", "b", "c"]), 1);
        state.servo_history_changed(urls(&["a", "b", "c"]), 0);
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 0));
    }

    #[test]
    fn a_restored_card_loads_entries_servo_does_not_hold() {
        let mut state = NavigationState::restored(&urls(&["a", "b", "c"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 1));
        // Servo knows only the page it was started on.
        state.servo_history_changed(urls(&["b"]), 0);
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 1));

        assert_eq!(state.traverse(-1), Some(HistoryStep::Load(url("a"))));
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 0));
        state.servo_history_changed(urls(&["b", "a"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 0));
        assert_eq!(state.traverse(-1), None);

        // `b` is in Servo, but before the page that replaced it: loading it
        // again keeps the card's order instead of Servo's.
        assert_eq!(state.traverse(1), Some(HistoryStep::Load(url("b"))));
        state.servo_history_changed(urls(&["b", "a", "b"]), 2);
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 1));
        assert_eq!(state.traverse(1), Some(HistoryStep::Load(url("c"))));
        state.servo_history_changed(urls(&["b", "a", "b", "c"]), 3);
        assert_eq!(shown(&state), (urls(&["a", "b", "c"]), 2));
        assert_eq!(state.traverse(1), None);
    }

    #[test]
    fn a_link_from_a_restored_entry_keeps_the_history_behind_it() {
        let mut state = NavigationState::restored(&urls(&["a", "b", "c"]), 1);
        state.servo_history_changed(urls(&["b"]), 0);
        state.servo_history_changed(urls(&["b", "d"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "d"]), 2));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Load(url("a"))));
    }

    #[test]
    fn a_redirect_of_the_restored_page_keeps_its_saved_neighbours() {
        let mut state = NavigationState::restored(&urls(&["a", "b", "c"]), 1);
        state.servo_history_changed(urls(&["b-redirected"]), 0);
        assert_eq!(shown(&state), (urls(&["a", "b-redirected", "c"]), 1));
        state.servo_history_changed(urls(&["b-replaced"]), 0);
        assert_eq!(shown(&state), (urls(&["a", "b-replaced", "c"]), 1));
    }

    #[test]
    fn navigating_after_back_discards_the_forward_entries() {
        let mut state = started_on_a();
        state.servo_history_changed(urls(&["a", "b"]), 1);
        state.traverse(-1);
        state.servo_history_changed(urls(&["a", "b"]), 0);

        state.navigate(url("typed"));
        assert_eq!(shown(&state), (urls(&["a", "typed"]), 1));
        state.servo_history_changed(urls(&["a", "typed"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "typed"]), 1));
        assert_eq!(state.traverse(1), None);
        // Servo kept `a` in front of the typed page, so Back traverses to it.
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
    }

    #[test]
    fn navigating_from_a_loaded_entry_keeps_the_entries_servo_lacks() {
        let mut state = NavigationState::restored(&urls(&["a", "b"]), 1);
        state.servo_history_changed(urls(&["b"]), 0);
        state.navigate(url("typed"));
        state.servo_history_changed(urls(&["b", "typed"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "typed"]), 2));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Load(url("a"))));
    }

    #[test]
    fn a_link_discards_restored_forward_entries() {
        let mut state = NavigationState::restored(&urls(&["a", "b", "c"]), 0);
        state.servo_history_changed(urls(&["a"]), 0);
        state.servo_history_changed(urls(&["a", "d"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "d"]), 1));
    }

    #[test]
    fn the_saved_scroll_offset_applies_to_the_restored_page_only() {
        let mut state = started_on_a();
        assert!(state.take_restore_scroll());
        assert!(!state.take_restore_scroll());
    }
}
