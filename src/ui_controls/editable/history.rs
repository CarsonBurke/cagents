//! Bounded undo history for opt-in form editors. The reducer records accepted
//! content after each native action, so one native batch can type and undo.
use cosmic::{Renderer, iced::widget::text_editor};
use std::sync::{Arc, Mutex};

/// Identity of one draft. Replace it whenever the whole draft is replaced;
/// queued history commands from the previous draft then have no effect.
#[derive(Clone, Debug, Default)]
pub struct EditorHistoryOwner(Arc<()>);
impl EditorHistoryOwner {
    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HistoryIntent {
    Observe,
    Undo,
    Redo,
}

/// Dispatched after the native edit, then applied by the consumer reducer.
/// Native UI.update batches events before reducers run; reading the view
/// Content while interpreting Undo would observe the previous batch instead.
#[derive(Clone)]
pub struct EditorHistoryCommand {
    owner: Arc<()>,
    history: Arc<Mutex<EditHistory>>,
    intent: HistoryIntent,
}
impl std::fmt::Debug for EditorHistoryCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorHistoryCommand")
            .field("intent", &self.intent)
            .finish_non_exhaustive()
    }
}
impl PartialEq for EditorHistoryCommand {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && Arc::ptr_eq(&self.history, &other.history)
            && self.intent == other.intent
    }
}
impl Eq for EditorHistoryCommand {}
impl EditorHistoryCommand {
    pub(super) fn new(
        owner: &EditorHistoryOwner,
        history: Arc<Mutex<EditHistory>>,
        intent: HistoryIntent,
    ) -> Self {
        Self {
            owner: owner.0.clone(),
            history,
            intent,
        }
    }
    pub fn changes_text(&self) -> bool {
        !matches!(self.intent, HistoryIntent::Observe)
    }
    /// Returns true only when an admitted Undo/Redo changed this same draft.
    pub fn apply(
        &self,
        content: &mut text_editor::Content<Renderer>,
        owner: &EditorHistoryOwner,
    ) -> bool {
        if !Arc::ptr_eq(&self.owner, &owner.0) {
            return false;
        }
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        history.observe(content.text(), content.cursor());
        let redo = match self.intent {
            HistoryIntent::Observe => return false,
            HistoryIntent::Undo => false,
            HistoryIntent::Redo => true,
        };
        let Some(snapshot) = history.restore(redo) else {
            return false;
        };
        content.perform(text_editor::Action::SelectAll);
        content.perform(text_editor::Action::Edit(text_editor::Edit::Paste(
            Arc::new(snapshot.text.clone()),
        )));
        content.move_to(snapshot.cursor);
        true
    }
}

/// Bounded per-control history supplies the undo/redo operations absent from
/// iced's form editor. Observe accepted edits and focused key input, never
/// redraw, so an idle retained form does no cloning or history work.
#[derive(Debug)]
struct EditSnapshot {
    text: String,
    cursor: text_editor::Cursor,
}
#[derive(Debug)]
pub(super) struct EditHistory {
    entries: Vec<EditSnapshot>,
    current: usize,
    bytes: usize,
}
impl EditHistory {
    pub(super) fn new(text: String, cursor: text_editor::Cursor) -> Self {
        let bytes = text.len();
        Self {
            entries: vec![EditSnapshot { text, cursor }],
            current: 0,
            bytes,
        }
    }
    fn observe(&mut self, text: String, cursor: text_editor::Cursor) {
        if text == self.entries[self.current].text {
            self.entries[self.current].cursor = cursor;
            return;
        }
        for entry in self.entries.drain(self.current + 1..) {
            self.bytes -= entry.text.len();
        }
        self.bytes += text.len();
        self.entries.push(EditSnapshot { text, cursor });
        while self.entries.len() > 128 || self.bytes > 1024 * 1024 && self.entries.len() > 1 {
            self.bytes -= self.entries.remove(0).text.len();
        }
        self.current = self.entries.len() - 1;
    }
    fn restore(&mut self, redo: bool) -> Option<&EditSnapshot> {
        let target = if redo {
            self.current
                .checked_add(1)
                .filter(|target| *target < self.entries.len())?
        } else {
            self.current.checked_sub(1)?
        };
        self.current = target;
        self.entries.get(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_history_discards_redo_after_new_text_and_bounds_retention() {
        let cursor = text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        };
        let mut history = EditHistory::new(String::new(), cursor);
        history.observe("a".into(), cursor);
        assert_eq!(history.restore(false).unwrap().text, "");
        history.observe("".into(), cursor);
        history.observe("b".into(), cursor);
        assert!(history.restore(true).is_none());
        for value in 0..200 {
            history.observe(format!("{value}{}", "x".repeat(20_000)), cursor);
        }
        assert!(history.bytes <= 1024 * 1024);
        assert!(history.entries.len() <= 128);
    }
}
