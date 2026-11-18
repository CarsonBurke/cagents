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
    /// The history of a card whose webview is about to load entry `index`, or
    /// `url` alone when it saved no history.
    pub(super) fn restored(url: &Url, history: &[Url], index: usize) -> Self {
        let history = if history.is_empty() {
            std::slice::from_ref(url)
        } else {
            history
        };
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
    ///
    /// A load still waiting for Servo is superseded rather than kept: Servo
    /// abandons a navigation that has not committed when another begins, and
    /// one that did commit first is reported as an entry of its own.
    pub(super) fn navigate(&mut self, url: Url) {
        self.forward.clear();
        self.url = url.clone();
        if let Some(pending) = &mut self.pending {
            pending.url = url;
            return;
        }
        let (mut entries, current) = self.all_entries();
        entries.truncate(current + 1);
        self.back = entries;
        self.pending = Some(PendingLoad {
            url,
            window_start: Some(self.window_start),
        });
    }

    /// Applies Servo's report of its history for this webview.
    pub(super) fn servo_history_changed(&mut self, entries: Vec<Url>, current: usize) {
        let Some(last) = entries.len().checked_sub(1) else {
            return;
        };
        let current = current.min(last);
        // A traversal leaves Servo's list as it was, and a redirect or
        // `history.replaceState` rewrites only the current entry.
        let same_list = entries.len() == self.servo.len()
            && entries
                .iter()
                .zip(&self.servo)
                .enumerate()
                .all(|(index, (new, old))| {
                    new == old || (index == current && current == self.servo_index)
                });
        // While a load is pending, Servo still reports the page it replaces:
        // a Back that was already under way, or that page's own script. Only
        // a new entry, or the first report of a new webview, commits the load.
        let committed = match self.pending.take() {
            Some(pending) if self.servo.is_empty() || !same_list => Some(pending),
            pending => {
                self.pending = pending;
                None
            }
        };
        if let Some(pending) = committed {
            // What Servo holds in front of the committed page is the card's
            // history only when the load continued the window and Servo kept
            // the window's entries up to it.
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
            // Anything else is a new navigation from the page, such as a
            // followed link, and that discards every forward entry, including
            // the card's own.
            if !same_list {
                self.forward.clear();
            }
            // The page a typed load continues from rewrote its own entry, which
            // the card holds as the last entry behind the load.
            if same_list
                && current == self.servo_index
                && let Some(PendingLoad {
                    window_start: Some(_),
                    ..
                }) = self.pending
                && let Some(departed) = self.back.last_mut()
            {
                departed.clone_from(&entries[current]);
            }
            // Page script can traverse into the entries this card replaced.
            // They are part of its history again.
            self.window_start = self.window_start.min(current);
        }
        if self.pending.is_none() {
            self.url = entries[current].clone();
        }
        self.servo = entries;
        self.servo_index = current;
    }

    /// Whether the page that just finished loading is the restored one, whose
    /// saved scroll offset should now be applied. True once per webview.
    /// Servo's history as last reported.
    #[cfg(test)]
    pub(super) fn servo_entries(&self) -> &[Url] {
        &self.servo
    }

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

    fn restored(history: &[Url], index: usize) -> NavigationState {
        NavigationState::restored(&history[index], history, index)
    }

    fn shown(state: &NavigationState) -> (Vec<Url>, usize) {
        state.entries()
    }

    /// A fresh card on `a`, with Servo's first report in.
    fn started_on_a() -> NavigationState {
        let mut state = restored(&urls(&["a"]), 0);
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
        let mut state = restored(&urls(&["a", "b", "c"]), 1);
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
        let mut state = restored(&urls(&["a", "b", "c"]), 1);
        state.servo_history_changed(urls(&["b"]), 0);
        state.servo_history_changed(urls(&["b", "d"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "d"]), 2));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Load(url("a"))));
    }

    #[test]
    fn a_redirect_of_the_restored_page_keeps_its_saved_neighbours() {
        let mut state = restored(&urls(&["a", "b", "c"]), 1);
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
        let mut state = restored(&urls(&["a", "b"]), 1);
        state.servo_history_changed(urls(&["b"]), 0);
        state.navigate(url("typed"));
        state.servo_history_changed(urls(&["b", "typed"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "typed"]), 2));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Load(url("a"))));
    }

    #[test]
    fn a_link_discards_restored_forward_entries() {
        let mut state = restored(&urls(&["a", "b", "c"]), 0);
        state.servo_history_changed(urls(&["a"]), 0);
        state.servo_history_changed(urls(&["a", "d"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "d"]), 1));
    }

    #[test]
    fn a_back_servo_reports_while_a_load_is_pending_does_not_commit_it() {
        let mut state = restored(&urls(&["a", "b"]), 1);
        state.servo_history_changed(urls(&["b"]), 0);
        state.servo_history_changed(urls(&["b", "d"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "d"]), 2));
        // Two quick Backs: the first is Servo's, the second leaves its list.
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Load(url("a"))));

        // Servo reports the first Back only after the second was asked for.
        state.servo_history_changed(urls(&["b", "d"]), 0);
        assert_eq!(shown(&state), (urls(&["a", "b", "d"]), 0));
        assert_eq!(state.url(), &url("a"));
        state.servo_history_changed(urls(&["b", "a"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b", "d"]), 0));
        assert_eq!(state.traverse(1), Some(HistoryStep::Load(url("b"))));
    }

    #[test]
    fn a_page_rewriting_its_entry_while_a_typed_load_is_pending_keeps_servo_history() {
        let mut state = started_on_a();
        state.servo_history_changed(urls(&["a", "b"]), 1);
        state.navigate(url("x"));
        // The page being left calls `history.replaceState`.
        state.servo_history_changed(urls(&["a", "b2"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "b2", "x"]), 2));
        state.servo_history_changed(urls(&["a", "b2", "x"]), 2);
        assert_eq!(shown(&state), (urls(&["a", "b2", "x"]), 2));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
    }

    #[test]
    fn a_second_typed_load_supersedes_one_servo_has_not_committed() {
        let mut state = started_on_a();
        state.navigate(url("x"));
        state.navigate(url("y"));
        assert_eq!(shown(&state), (urls(&["a", "y"]), 1));
        state.servo_history_changed(urls(&["a", "y"]), 1);
        assert_eq!(shown(&state), (urls(&["a", "y"]), 1));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
    }

    #[test]
    fn a_superseded_typed_load_that_did_commit_stays_in_the_history() {
        let mut state = started_on_a();
        state.navigate(url("x"));
        state.navigate(url("y"));
        state.servo_history_changed(urls(&["a", "x"]), 1);
        state.servo_history_changed(urls(&["a", "x", "y"]), 2);
        assert_eq!(shown(&state), (urls(&["a", "x", "y"]), 2));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
        assert_eq!(state.traverse(-1), Some(HistoryStep::Back));
    }

    #[test]
    fn a_card_without_saved_history_starts_on_its_url() {
        let state = NavigationState::restored(&url("a"), &[], 3);
        assert_eq!(shown(&state), (urls(&["a"]), 0));
    }

    #[test]
    fn the_saved_scroll_offset_applies_to_the_restored_page_only() {
        let mut state = started_on_a();
        assert!(state.take_restore_scroll());
        assert!(!state.take_restore_scroll());
    }
}
