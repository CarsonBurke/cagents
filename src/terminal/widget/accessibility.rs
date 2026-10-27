//! The assistive view is a bounded projection of the displayed grid. It never
//! reads history, a transcript, or a terminal command channel.
use super::super::RenderSnapshot;
use cosmic::iced::{
    Event,
    advanced::{mouse, widget::Id},
    keyboard,
};
use iced_accessibility::{
    A11yId,
    accesskit::{NodeId, TextPosition, TextSelection},
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

pub(super) const MAX_ROWS: usize = 128;
pub(super) const MAX_BYTES: usize = 16 * 1024;
const MAX_CELLS: usize = 8192;
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Default)]
pub(super) struct Row {
    pub text: String,
    pub lengths: Vec<u8>,
    // Terminal columns are not Unicode character offsets (wide and combining
    // characters differ). Keep that mapping with the actual projected text.
    pub columns: Vec<usize>,
    pub positions: Vec<f32>,
    pub widths: Vec<f32>,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Projection {
    pub rows: Vec<Row>,
    pub truncated: bool,
}
impl Projection {
    pub fn new(snapshot: &RenderSnapshot, rows: u16, columns: u16) -> Self {
        let mut result = Self::default();
        let mut bytes = 0;
        let mut cells = 0;
        let limit = usize::from(snapshot.rows.min(rows));
        let columns = usize::from(snapshot.columns.min(columns));
        for source in snapshot.lines.iter().take(limit.min(MAX_ROWS)) {
            let mut row = Row::default();
            let mut continuation_until = 0;
            let mut glyph_start = 0;
            for (column, cell) in source.cells.iter().take(columns).enumerate() {
                if cells == MAX_CELLS {
                    result.truncated = true;
                    break;
                }
                cells += 1;
                if cell.text.is_empty() && column < continuation_until {
                    row.columns.push(glyph_start);
                    continue;
                }
                glyph_start = row.lengths.len();
                row.columns.push(glyph_start);
                let value = if cell.style.invisible || cell.text.is_empty() {
                    " "
                } else {
                    cell.text.as_str()
                };
                let width = unicode_width::UnicodeWidthStr::width(value)
                    .max(1)
                    .min(columns - column);
                continuation_until = column + width;
                for cluster in value.graphemes(true) {
                    let Ok(length) = u8::try_from(cluster.len()) else {
                        result.truncated = true;
                        break;
                    };
                    if bytes + cluster.len() + 1 > MAX_BYTES {
                        result.truncated = true;
                        break;
                    }
                    row.text.push_str(cluster);
                    row.lengths.push(length);
                    row.positions.push(column as f32);
                    row.widths.push(width as f32);
                    bytes += cluster.len();
                }
                if result.truncated {
                    break;
                }
            }
            row.columns.push(row.lengths.len());
            // A run is one displayed row, with a real text boundary even for
            // soft-wrapped rows. Review never pretends to expose a logical line.
            if bytes < MAX_BYTES {
                row.text.push('\n');
                row.lengths.push(1);
                row.positions
                    .push(row.columns.len().saturating_sub(1) as f32);
                row.widths.push(0.0);
                bytes += 1;
            } else {
                result.truncated = true;
            }
            result.rows.push(row);
            if result.truncated {
                break;
            }
        }
        result.truncated |= limit > MAX_ROWS || snapshot.lines.len().min(limit) > result.rows.len();
        result
    }
}

pub(super) fn focus_report(last_sent: bool, model_focused: bool, assistive: bool) -> Option<bool> {
    (!assistive && last_sent != model_focused).then_some(model_focused)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct View {
    pub rows: u16,
    pub columns: u16,
    pub offset: u64,
    pub alternate: bool,
    pub focused: bool,
}
use cosmic::iced::runtime::core::keyboard::input::{
    PressCycle, Source, Transition, current_source,
};
struct OwnedKey {
    physical: keyboard::key::Physical,
    cycle: Option<Weak<PressCycle>>, // None is cfg(test)-only compatibility.
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionPhase {
    PendingQuery,
    Claimed,
}
struct ClaimedAction {
    physical: keyboard::key::Physical,
    cycle: Weak<PressCycle>,
    phase: ActionPhase,
}
#[derive(Default)]
struct ConsumedInputs {
    keys: Vec<OwnedKey>,
    actions: Vec<ClaimedAction>,
    buttons: Vec<mouse::Button>,
}
impl ConsumedInputs {
    fn prune(&mut self) {
        self.keys
            .retain(|k| k.cycle.as_ref().is_none_or(|w| w.strong_count() > 0));
        self.actions.retain(|a| a.cycle.strong_count() > 0);
    }
    fn physical(event: &keyboard::Event) -> Option<(&keyboard::key::Physical, Transition)> {
        match event {
            keyboard::Event::KeyPressed { physical_key, .. } => {
                Some((physical_key, Transition::Press))
            }
            keyboard::Event::KeyReleased { physical_key, .. } => {
                Some((physical_key, Transition::Release))
            }
            _ => None,
        }
    }
    fn cycle<'a>(
        key: &keyboard::key::Physical,
        role: Transition,
        source: &'a Source,
    ) -> Option<&'a Arc<PressCycle>> {
        source
            .cycle()
            .filter(|c| source.transition() == Some(role) && c.physical_key() == *key)
    }
    fn exact(
        &self,
        key: &keyboard::key::Physical,
        role: Transition,
        source: &Source,
    ) -> Option<usize> {
        if let Some(cycle) = Self::cycle(key, role, source) {
            let weak = Arc::downgrade(cycle);
            self.keys.iter().position(|k| {
                k.physical == *key && k.cycle.as_ref().is_some_and(|w| w.ptr_eq(&weak))
            })
        } else if source.cycle().is_none() {
            self.keys
                .iter()
                .position(|k| k.physical == *key && k.cycle.is_none())
        } else {
            None
        }
    }
    fn owns(&self, key: &keyboard::key::Physical, role: Transition, source: &Source) -> bool {
        self.exact(key, role, source).is_some()
            || (source.cycle().is_none() && self.keys.iter().any(|k| k.physical == *key))
    }
    fn record(&mut self, event: &keyboard::Event, source: &Source, from_app: bool) -> bool {
        self.prune();
        let Some((key, Transition::Press)) = Self::physical(event) else {
            return false;
        };
        if let Some(cycle) = Self::cycle(key, Transition::Press, source) {
            if from_app && !cycle.is_down() {
                return false;
            }
            if self.exact(key, Transition::Press, source).is_some() {
                return false;
            }
            if self
                .keys
                .iter()
                .any(|k| k.physical == *key && k.cycle.is_none())
            {
                return false;
            }
            self.keys.push(OwnedKey {
                physical: key.clone(),
                cycle: Some(Arc::downgrade(cycle)),
            });
            true
        } else if cfg!(test) && source.cycle().is_none() {
            if self.keys.iter().any(|k| k.physical == *key)
                || self.actions.iter().any(|a| a.physical == *key)
            {
                return false;
            }
            self.keys.push(OwnedKey {
                physical: key.clone(),
                cycle: None,
            });
            true
        } else {
            false
        }
    }
}
#[derive(Clone, Default)]
pub(crate) struct ReviewInputBarrier(Arc<Mutex<ConsumedInputs>>);
impl ReviewInputBarrier {
    /// Recording is idempotent; existing widget consumers may redeliver a press.
    pub fn capture_activation(&self, event: &keyboard::Event) -> bool {
        matches!(event, keyboard::Event::KeyPressed { repeat: false, .. })
            && self.capture_consumed_key(event)
    }
    /// Action admission is separate from accepted ownership recording.
    pub fn claim_native_activation(&self, event: &keyboard::Event) -> bool {
        self.capture_native_action(event, ActionPhase::Claimed)
    }
    pub fn capture_query_submit(&self, event: &keyboard::Event) -> bool {
        self.capture_native_action(event, ActionPhase::PendingQuery)
    }
    fn capture_native_action(&self, event: &keyboard::Event, phase: ActionPhase) -> bool {
        if !matches!(event, keyboard::Event::KeyPressed { repeat: false, .. }) {
            return false;
        }
        let source = current_source();
        let mut inputs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !inputs.record(event, &source, false) {
            return false;
        }
        if let Some(cycle) = source.cycle() {
            let weak = Arc::downgrade(cycle);
            if inputs.actions.iter().any(|a| a.cycle.ptr_eq(&weak)) {
                return false;
            }
            inputs.actions.push(ClaimedAction {
                physical: cycle.physical_key(),
                cycle: weak,
                phase,
            });
        }
        true
    }
    pub fn claim_query_submit(&self, event: &keyboard::Event, source: &Source) -> bool {
        let Some((key, Transition::Press)) = ConsumedInputs::physical(event) else {
            return false;
        };
        let Some(cycle) = ConsumedInputs::cycle(key, Transition::Press, source) else {
            return self.allow_legacy_app_activation(event, source);
        };
        let mut inputs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inputs.prune();
        let weak = Arc::downgrade(cycle);
        let Some(ticket) = inputs.actions.iter_mut().find(|a| a.cycle.ptr_eq(&weak)) else {
            return false;
        };
        if ticket.phase != ActionPhase::PendingQuery {
            return false;
        }
        ticket.phase = ActionPhase::Claimed;
        true
    }
    /// Recording result is NOT action validity. Quick P1/R1 may reach App up.
    pub fn capture_app_activation(&self, event: &keyboard::Event, source: &Source) -> bool {
        if !matches!(event, keyboard::Event::KeyPressed { repeat: false, .. }) {
            return false;
        }
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(event, source, true)
    }
    /// Real contextual activation is native-only with an immutable payload.
    /// Old direct KeyEvent helpers exist only for isolated component tests.
    pub fn allow_legacy_app_activation(&self, event: &keyboard::Event, source: &Source) -> bool {
        if !cfg!(test) || source.cycle().is_some() {
            return false;
        }
        let Some((key, Transition::Press)) = ConsumedInputs::physical(event) else {
            return false;
        };
        let mut inputs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inputs.prune();
        !inputs
            .keys
            .iter()
            .any(|k| k.physical == *key && k.cycle.is_some())
            && !inputs.actions.iter().any(|a| a.physical == *key)
    }
    pub fn capture_consumed_key(&self, event: &keyboard::Event) -> bool {
        let source = current_source();
        let mut inputs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inputs.prune();
        let already = ConsumedInputs::physical(event)
            .is_some_and(|(key, role)| inputs.exact(key, role, &source).is_some());
        already || inputs.record(event, &source, false)
    }
    pub fn consume_keyboard(&self, event: &keyboard::Event) -> bool {
        self.consume_keyboard_with_source(event, &current_source())
    }
    pub fn consume_keyboard_with_source(&self, event: &keyboard::Event, source: &Source) -> bool {
        let mut inputs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inputs.prune();
        let Some((key, role)) = ConsumedInputs::physical(event) else {
            return false;
        };
        if role == Transition::Release {
            if let Some(index) = inputs.exact(key, role, source) {
                inputs.keys.swap_remove(index);
                return true;
            }
        }
        // None can suppress ambiguous input but cannot retire a tracked record.
        inputs.owns(key, role, source)
    }
    pub fn owns_keyboard(&self, event: &Event) -> bool {
        let Event::Keyboard(event) = event else {
            return false;
        };
        let mut inputs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inputs.prune();
        ConsumedInputs::physical(event)
            .is_some_and(|(key, role)| inputs.owns(key, role, &current_source()))
    }
    pub fn owns_mouse_release(&self, event: &Event) -> bool {
        let Event::Mouse(mouse::Event::ButtonReleased(button)) = event else {
            return false;
        };
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buttons
            .contains(button)
    }
}

#[derive(Default)]
pub(super) struct State {
    identity: Option<(Id, crate::model::TerminalId)>,
    pub projection: Option<Projection>,
    pub row_ids: Vec<Id>,
    pub control_id: Option<Id>,
    generation: u64,
    view: Option<View>,
    published: Option<Instant>,
    pub review: bool,
    pub selection: Option<TextSelection>,
    pub barrier: ReviewInputBarrier,
}
impl State {
    pub fn bind(&mut self, parent: &Id, terminal: crate::model::TerminalId) {
        if self
            .identity
            .as_ref()
            .is_none_or(|(old_parent, old_terminal)| {
                old_parent != parent || *old_terminal != terminal
            })
        {
            let identity = (parent.clone(), terminal);
            let barrier = self.barrier.clone();
            *self = Self {
                identity: Some(identity),
                barrier,
                ..Self::default()
            };
        }
    }
    pub fn ensure_ids(&mut self, parent: &Id) {
        let prefix = self.identity.as_ref().map_or_else(
            || parent.to_string(),
            |(parent, terminal)| format!("{parent}-terminal-{terminal}"),
        );
        self.control_id
            .get_or_insert_with(|| Id::new(format!("{prefix}-review")));
        while self.row_ids.len() < self.projection.as_ref().map_or(0, |p| p.rows.len()) {
            self.row_ids.push(Id::new(format!(
                "{prefix}-visible-row-{}",
                self.row_ids.len()
            )));
        }
    }
    pub fn refresh(
        &mut self,
        snapshot: &Arc<RenderSnapshot>,
        view: View,
        now: Instant,
        activate: bool,
    ) -> Option<Instant> {
        if !activate && self.projection.is_none() {
            return None;
        }
        if !view.focused && self.view.is_some_and(|old| old.focused) {
            self.review = false;
            self.selection = None;
        }
        let force = self.view != Some(view) || self.projection.is_none();
        if force || self.generation != snapshot.generation {
            let due = self.published.map(|at| at + UPDATE_INTERVAL).unwrap_or(now);
            if force || now >= due {
                self.projection = Some(Projection::new(snapshot, view.rows, view.columns));
                self.generation = snapshot.generation;
                self.view = Some(view);
                self.published = Some(now);
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|s| !self.valid_selection(s))
                {
                    self.selection = None;
                }
            } else {
                return Some(due);
            }
        }
        None
    }
    fn row_index(&self, node: NodeId) -> Option<usize> {
        self.row_ids
            .iter()
            .position(|id| NodeId::from(A11yId::from(id.clone())) == node)
    }
    pub fn valid_selection(&self, selection: &TextSelection) -> bool {
        [&selection.anchor, &selection.focus].into_iter().all(|p| {
            self.row_index(p.node)
                .and_then(|i| self.projection.as_ref()?.rows.get(i))
                .is_some_and(|row| p.character_index <= row.lengths.len())
        })
    }
    pub fn position(&self, row: usize, character: usize) -> Option<TextPosition> {
        let value = self.projection.as_ref()?.rows.get(row)?;
        Some(TextPosition {
            node: A11yId::from(self.row_ids.get(row)?.clone()).into(),
            character_index: character.min(value.lengths.len().saturating_sub(1)),
        })
    }
    /// Return true before the widget can call any terminal input API. A key or
    /// button consumed during review keeps its release, even after mode exit.
    pub fn review_input(&mut self, event: &Event) -> bool {
        if !self.review
            && matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { .. }))
            && self.barrier.owns_keyboard(event)
        {
            return true;
        }
        if let Event::Keyboard(keyboard::Event::KeyReleased { .. }) = event
            && let Event::Keyboard(key) = event
            && self.barrier.consume_keyboard(key)
        {
            return true;
        }
        if let Event::Mouse(mouse::Event::ButtonReleased(button)) = event {
            let mut barrier = self.barrier.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = barrier.buttons.iter().position(|held| held == button) {
                barrier.buttons.swap_remove(i);
                return true;
            }
        }
        if !self.review {
            return false;
        }
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                let Event::Keyboard(key_event) = event else {
                    unreachable!()
                };
                if !self.barrier.capture_consumed_key(key_event) {
                    return true;
                }
                use keyboard::key::Named;
                match key.as_ref() {
                    keyboard::Key::Named(Named::Escape) => {
                        self.review = false;
                        self.selection = None;
                    }
                    keyboard::Key::Named(Named::ArrowUp) => {
                        self.move_review(-1, 0, modifiers.shift())
                    }
                    keyboard::Key::Named(Named::ArrowDown) => {
                        self.move_review(1, 0, modifiers.shift())
                    }
                    keyboard::Key::Named(Named::ArrowLeft) => {
                        self.move_review(0, -1, modifiers.shift())
                    }
                    keyboard::Key::Named(Named::ArrowRight) => {
                        self.move_review(0, 1, modifiers.shift())
                    }
                    _ => {}
                }
                true
            }
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                let mut barrier = self.barrier.0.lock().unwrap_or_else(|e| e.into_inner());
                if !barrier.buttons.contains(button) {
                    barrier.buttons.push(*button);
                }
                true
            }
            Event::Keyboard(_) | Event::InputMethod(_) | Event::Mouse(_) => true,
            _ => false,
        }
    }

    pub fn grid_selection(
        &self,
        start: (u64, u16),
        end: (u64, u16),
        offset: u64,
    ) -> Option<TextSelection> {
        let rows = &self.projection.as_ref()?.rows;
        let last = offset.checked_add(u64::try_from(rows.len()).ok()?.checked_sub(1)?)?;
        if end.0 < offset || start.0 > last {
            return None;
        }
        let start_row = usize::try_from(start.0.saturating_sub(offset)).ok()?;
        let end_row = usize::try_from(end.0.min(last).saturating_sub(offset)).ok()?;
        let column = |row: usize, col: usize| {
            let value = rows.get(row)?;
            value
                .columns
                .get(col.min(value.columns.len().saturating_sub(1)))
                .copied()
        };
        let anchor = self.position(
            start_row,
            if start.0 < offset {
                0
            } else {
                column(start_row, usize::from(start.1))?
            },
        )?;
        let focus = self.position(
            end_row,
            if end.0 > last {
                rows[end_row].lengths.len().saturating_sub(1)
            } else {
                let value = &rows[end_row];
                let end_column = usize::from(end.1).min(value.columns.len().saturating_sub(1));
                let start_index = column(end_row, end_column)?;
                value
                    .columns
                    .iter()
                    .skip(end_column + 1)
                    .copied()
                    .find(|index| *index > start_index)
                    .unwrap_or(value.lengths.len().saturating_sub(1))
            },
        )?;
        Some(TextSelection { anchor, focus })
    }

    pub fn cursor(&self, snapshot: &RenderSnapshot) -> Option<TextSelection> {
        let cursor = snapshot.cursor.filter(|c| c.visible)?;
        let row = self
            .projection
            .as_ref()?
            .rows
            .get(usize::from(cursor.row))?;
        let index = *row.columns.get(usize::from(cursor.column))?;
        let p = self.position(usize::from(cursor.row), index)?;
        Some(TextSelection {
            anchor: p,
            focus: p,
        })
    }
    pub fn move_review(&mut self, row_delta: isize, char_delta: isize, extend: bool) {
        let Some(projection) = &self.projection else {
            return;
        };
        if projection.rows.is_empty() {
            return;
        }
        let current = self.selection.as_ref().map(|s| s.focus);
        let row = current.and_then(|p| self.row_index(p.node)).unwrap_or(0);
        let next_row = row
            .saturating_add_signed(row_delta)
            .min(projection.rows.len() - 1);
        let next_char = current
            .map_or(0, |p| p.character_index)
            .saturating_add_signed(char_delta);
        if let Some(next) = self.position(next_row, next_char) {
            let anchor = if extend {
                self.selection.as_ref().map_or(next, |s| s.anchor)
            } else {
                next
            };
            self.selection = Some(TextSelection {
                anchor,
                focus: next,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{CellSnapshot, CellText, RowSnapshot};
    use super::*;
    #[test]
    fn weak_cycle_records_coexist_and_old_or_untagged_release_cannot_clear_successor() {
        use cosmic::iced::runtime::core::keyboard::input::{self, PressCycle, Source, Transition};
        let barrier = ReviewInputBarrier::default();
        let key = keyboard::Key::Named(keyboard::key::Named::Enter);
        let event = press(
            key.clone(),
            keyboard::key::Code::Enter,
            keyboard::Modifiers::empty(),
        );
        let Event::Keyboard(press) = event.clone() else {
            unreachable!()
        };
        let first = PressCycle::new(keyboard::key::Physical::Code(keyboard::key::Code::Enter));
        first.release();
        let second = PressCycle::new(keyboard::key::Physical::Code(keyboard::key::Code::Enter));
        let one = Source::tracked(first.clone(), Transition::Press);
        let two = Source::tracked(second, Transition::Press);
        {
            let _scope = input::scope(two.clone());
            assert!(barrier.claim_native_activation(&press));
        }
        {
            let _scope = input::scope(one.clone());
            assert!(barrier.claim_native_activation(&press));
        }
        assert_eq!(
            barrier.0.lock().unwrap().keys.len(),
            2,
            "late native P1 cannot overwrite P2"
        );
        let Event::Keyboard(release) = release(key, keyboard::key::Code::Enter) else {
            unreachable!()
        };
        assert!(
            barrier.consume_keyboard_with_source(
                &release,
                &Source::tracked(first, Transition::Release)
            )
        );
        assert!(
            !barrier.capture_app_activation(&press, &one),
            "late App capture never reacquires an up cycle"
        );
        assert!(
            barrier.consume_keyboard_with_source(&release, &Source::default()),
            "unknown release may suppress, never retire tracked ownership"
        );
        assert!(!barrier.allow_legacy_app_activation(&press, &Source::default()));
        let _scope = input::scope(two);
        assert!(barrier.owns_keyboard(&event));
    }

    #[test]
    fn weak_key_and_action_history_do_not_keep_released_cycle_alive() {
        use cosmic::iced::runtime::core::keyboard::input::{self, PressCycle, Source, Transition};
        let barrier = ReviewInputBarrier::default();
        let event = press(
            keyboard::Key::Named(keyboard::key::Named::Enter),
            keyboard::key::Code::Enter,
            keyboard::Modifiers::empty(),
        );
        let Event::Keyboard(key) = &event else {
            unreachable!()
        };
        let cycle = PressCycle::new(keyboard::key::Physical::Code(keyboard::key::Code::Enter));
        let weak = Arc::downgrade(&cycle);
        {
            let _scope = input::scope(Source::tracked(cycle.clone(), Transition::Press));
            assert!(barrier.claim_native_activation(key));
        }
        cycle.release();
        drop(cycle);
        assert!(
            weak.upgrade().is_none(),
            "barrier stores no strong cycle owner"
        );
        assert!(!barrier.owns_keyboard(&event));
        let inputs = barrier.0.lock().unwrap();
        assert!(inputs.keys.is_empty() && inputs.actions.is_empty());
    }

    #[test]
    fn review_consumes_first_seen_repeat_and_protects_its_exact_release_after_exit() {
        use cosmic::iced::runtime::core::keyboard::input::{self, PressCycle, Source, Transition};
        let mut state = State {
            review: true,
            ..State::default()
        };
        let key = keyboard::Key::Character("q".into());
        let mut event = press(
            key.clone(),
            keyboard::key::Code::KeyQ,
            keyboard::Modifiers::empty(),
        );
        if let Event::Keyboard(keyboard::Event::KeyPressed { repeat, .. }) = &mut event {
            *repeat = true;
        }
        let cycle = PressCycle::new(keyboard::key::Physical::Code(keyboard::key::Code::KeyQ));
        {
            let _scope = input::scope(Source::tracked(cycle.clone(), Transition::Press));
            assert!(state.review_input(&event));
        }
        state.review = false;
        cycle.release();
        let release = release(key, keyboard::key::Code::KeyQ);
        let _scope = input::scope(Source::tracked(cycle, Transition::Release));
        assert!(state.review_input(&release));
        assert!(!state.barrier.owns_keyboard(&release));
    }

    fn frame(text: &str, generation: u64) -> Arc<RenderSnapshot> {
        Arc::new(RenderSnapshot {
            generation,
            rows: 1,
            columns: 1,
            lines: vec![Arc::new(RowSnapshot {
                cells: vec![CellSnapshot {
                    text: CellText::new(text),
                    ..Default::default()
                }],
                ..Default::default()
            })]
            .into(),
            ..RenderSnapshot::empty()
        })
    }
    fn view() -> View {
        View {
            rows: 1,
            columns: 1,
            offset: 0,
            alternate: false,
            focused: true,
        }
    }
    #[test]
    fn visible_projection_preserves_graphemes_and_excludes_hidden_rows_cells_and_concealed_text() {
        let mut source = (*frame("e\u{301}👩‍💻", 1)).clone();
        let mut row = (*source.lines[0]).clone();
        row.cells.push(CellSnapshot {
            text: CellText::new("excluded column"),
            ..Default::default()
        });
        source.lines = vec![
            Arc::new(row),
            Arc::new(RowSnapshot {
                cells: vec![CellSnapshot {
                    text: CellText::new("excluded row"),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        ]
        .into();
        let p = Projection::new(&source, 1, 1);
        assert_eq!(p.rows[0].text, "e\u{301}👩‍💻\n");
        assert_eq!(p.rows[0].lengths, vec![3, 11, 1]);
        let mut row = (*source.lines[0]).clone();
        row.cells[0].style.invisible = true;
        source.lines = vec![Arc::new(row)].into();
        assert_eq!(Projection::new(&source, 1, 1).rows[0].text, " \n");
    }
    #[test]
    fn capped_text_never_splits_unicode_or_exceeds_the_byte_budget() {
        let p = Projection::new(&frame(&"👩‍💻".repeat(MAX_BYTES), 1), 1, 1);
        assert!(p.truncated);
        assert!(p.rows.iter().map(|r| r.text.len()).sum::<usize>() <= MAX_BYTES);
        assert!(
            p.rows
                .iter()
                .all(|r| r.lengths.iter().map(|n| usize::from(*n)).sum::<usize>() == r.text.len())
        );
        let p = Projection::new(&frame(&format!("e{}", "\u{301}".repeat(200)), 1), 1, 1);
        assert!(p.truncated);
        assert_eq!(p.rows[0].text, "\n");
    }
    #[test]
    fn coalescing_returns_a_deadline_and_eventually_publishes_the_latest_frame() {
        let now = Instant::now();
        let mut state = State::default();
        assert_eq!(state.refresh(&frame("first", 1), view(), now, false), None);
        assert!(state.projection.is_none());
        state.refresh(&frame("first", 1), view(), now, true);
        assert_eq!(
            state.refresh(
                &frame("next", 2),
                view(),
                now + Duration::from_millis(20),
                false
            ),
            Some(now + UPDATE_INTERVAL)
        );
        assert_eq!(state.projection.as_ref().unwrap().rows[0].text, "first\n");
        state.refresh(&frame("latest", 3), view(), now + UPDATE_INTERVAL, false);
        assert_eq!(state.projection.as_ref().unwrap().rows[0].text, "latest\n");
        let changed = View {
            columns: 0,
            ..view()
        };
        state.refresh(
            &frame("must disappear", 4),
            changed,
            now + UPDATE_INTERVAL + Duration::from_millis(1),
            false,
        );
        assert_eq!(state.projection.as_ref().unwrap().rows[0].text, "\n");
    }
    #[test]
    fn projected_selection_rejects_foreign_nodes_and_hidden_offsets_and_retains_row_ids() {
        let now = Instant::now();
        let mut state = State::default();
        let id = Id::new("test-terminal-card");
        state.refresh(&frame("text", 1), view(), now, true);
        state.ensure_ids(&id);
        let rows = state.row_ids.clone();
        let position = state.position(0, 1).unwrap();
        assert!(state.valid_selection(&TextSelection {
            anchor: position,
            focus: position
        }));
        let foreign = TextPosition {
            node: NodeId(u64::MAX),
            character_index: 0,
        };
        assert!(!state.valid_selection(&TextSelection {
            anchor: position,
            focus: foreign
        }));
        let hidden = TextPosition {
            character_index: 100,
            node: position.node,
        };
        assert!(!state.valid_selection(&TextSelection {
            anchor: hidden,
            focus: hidden
        }));
        state.refresh(&frame("changed", 2), view(), now + UPDATE_INTERVAL, false);
        state.ensure_ids(&id);
        assert_eq!(state.row_ids, rows);
    }
    #[test]
    fn rebinding_card_or_replaced_terminal_drops_old_content_selection_and_controls() {
        let now = Instant::now();
        let mut state = State::default();
        let card = Id::new("card-one");
        let terminal = crate::model::TerminalId::new();
        state.bind(&card, terminal);
        state.refresh(&frame("old", 1), view(), now, true);
        state.ensure_ids(&card);
        state.move_review(0, 1, false);
        state.review = true;
        let old = state.row_ids.clone();
        let old_control = state.control_id.clone();
        state.bind(&Id::new("card-one"), terminal);
        assert!(state.review);
        assert_eq!(state.row_ids, old);
        let replacement = crate::model::TerminalId::new();
        state.bind(&card, replacement);
        assert!(!state.review);
        assert!(state.projection.is_none());
        assert!(state.selection.is_none());
        state.refresh(&frame("new", 1), view(), now, true);
        state.ensure_ids(&card);
        assert_ne!(state.row_ids, old);
        assert_ne!(state.control_id, old_control);
        state.bind(&Id::new("card-two"), replacement);
        assert!(state.row_ids.is_empty());
        assert!(state.projection.is_none());
    }

    #[test]
    fn ordinary_selection_is_clipped_to_visible_rows_without_hidden_text_offsets() {
        let now = Instant::now();
        let mut state = State::default();
        state.refresh(
            &frame("visible", 1),
            View {
                offset: 40,
                ..view()
            },
            now,
            true,
        );
        state.ensure_ids(&Id::new("card"));
        assert!(state.grid_selection((0, 0), (39, 10), 40).is_none());
        let visible = state.grid_selection((0, 0), (100, 10), 40).unwrap();
        assert_eq!(visible.anchor.character_index, 0);
        assert_eq!(visible.focus.character_index, 7);
        assert!(state.valid_selection(&visible));
    }
    fn press(
        key: keyboard::Key,
        code: keyboard::key::Code,
        modifiers: keyboard::Modifiers,
    ) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }
    fn release(key: keyboard::Key, code: keyboard::key::Code) -> Event {
        Event::Keyboard(keyboard::Event::KeyReleased {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        })
    }
    #[test]
    fn review_gate_consumes_chords_ime_mouse_and_the_releases_after_exit_without_input_commands() {
        let now = Instant::now();
        let mut state = State::default();
        let parent = Id::new("card");
        state.bind(&parent, crate::model::TerminalId::new());
        state.refresh(&frame("visible", 1), view(), now, true);
        state.ensure_ids(&parent);
        state.review = true;
        let c = keyboard::Key::Character("c".into());
        let escape = keyboard::Key::Named(keyboard::key::Named::Escape);
        let events = [
            press(
                c.clone(),
                keyboard::key::Code::KeyC,
                keyboard::Modifiers::CTRL,
            ),
            Event::InputMethod(cosmic::iced::advanced::input_method::Event::Commit(
                "paste".into(),
            )),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
            press(
                escape.clone(),
                keyboard::key::Code::Escape,
                keyboard::Modifiers::empty(),
            ),
            release(escape, keyboard::key::Code::Escape),
            release(c, keyboard::key::Code::KeyC),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
        ];
        let mut input_commands = Vec::new();
        for event in events {
            if !state.review_input(&event) {
                input_commands.push(event);
            }
        }
        assert!(input_commands.is_empty());
        assert!(!state.review);
        assert!(!state.review_input(&press(
            keyboard::Key::Character("a".into()),
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty()
        )));
    }
    #[test]
    fn replacement_keeps_consumed_release_barrier_but_does_not_keep_review_or_content() {
        let mut state = State {
            review: true,
            ..State::default()
        };
        let escape = keyboard::Key::Named(keyboard::key::Named::Escape);
        assert!(state.review_input(&press(
            escape.clone(),
            keyboard::key::Code::Escape,
            keyboard::Modifiers::empty()
        )));
        state.bind(&Id::new("new-card"), crate::model::TerminalId::new());
        assert!(!state.review);
        assert!(state.projection.is_none());
        assert!(state.review_input(&release(escape, keyboard::key::Code::Escape)));
    }
    #[test]
    fn focus_loss_leaves_review_and_forces_fresh_semantics_without_a_second_card_keyboard_trap() {
        let now = Instant::now();
        let mut state = State::default();
        state.refresh(&frame("first", 1), view(), now, true);
        state.review = true;
        assert_eq!(
            state.refresh(
                &frame("latest", 2),
                View {
                    focused: false,
                    ..view()
                },
                now + Duration::from_millis(1),
                false
            ),
            None
        );
        assert!(!state.review);
        assert_eq!(state.projection.unwrap().rows[0].text, "latest\n");
    }
    #[test]
    fn wide_continuations_do_not_invent_spaces_but_real_blank_columns_remain_readable() {
        let cells = ["漢", "", "", "x"]
            .map(|text| CellSnapshot {
                text: CellText::new(text),
                ..Default::default()
            })
            .to_vec();
        let source = RenderSnapshot {
            rows: 1,
            columns: 4,
            lines: vec![Arc::new(RowSnapshot {
                cells,
                ..Default::default()
            })]
            .into(),
            ..RenderSnapshot::empty()
        };
        let p = Projection::new(&source, 1, 4);
        assert_eq!(p.rows[0].text, "漢 x\n");
        assert_eq!(p.rows[0].columns, vec![0, 0, 1, 2, 3]);
        assert_eq!(p.rows[0].positions, vec![0.0, 2.0, 3.0, 4.0]);
        assert_eq!(p.rows[0].widths, vec![2.0, 1.0, 1.0, 0.0]);
    }
    #[test]
    fn row_and_cell_caps_bound_large_visible_grids_and_mark_omitted_content() {
        let row = Arc::new(RowSnapshot {
            cells: vec![
                CellSnapshot {
                    text: CellText::new("a"),
                    ..Default::default()
                };
                512
            ],
            ..Default::default()
        });
        let source = RenderSnapshot {
            rows: 129,
            columns: 512,
            lines: vec![row; 129].into(),
            ..RenderSnapshot::empty()
        };
        let p = Projection::new(&source, 129, 512);
        assert!(p.truncated);
        assert!(p.rows.len() <= MAX_ROWS);
        assert!(
            p.rows
                .iter()
                .map(|row| row.columns.len().saturating_sub(1))
                .sum::<usize>()
                <= MAX_CELLS
        );
        assert!(p.rows.iter().map(|row| row.text.len()).sum::<usize>() <= MAX_BYTES);
    }
    #[test]
    fn shared_barrier_retires_blurred_keys_and_blocks_repeats_in_the_new_card() {
        let shared = ReviewInputBarrier::default();
        let mut old = State {
            barrier: shared.clone(),
            review: true,
            ..State::default()
        };
        let a = keyboard::Key::Character("a".into());
        assert!(old.review_input(&press(
            a.clone(),
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty()
        )));
        old.review = false;
        let mut new = State {
            barrier: shared.clone(),
            ..State::default()
        };
        assert!(shared.owns_keyboard(&press(
            a.clone(),
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty()
        )));
        assert!(new.review_input(&press(
            a.clone(),
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty()
        )));
        assert!(new.review_input(&release(a.clone(), keyboard::key::Code::KeyA)));
        assert!(!old.review_input(&press(
            a.clone(),
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty()
        )));
        assert!(!old.review_input(&release(a, keyboard::key::Code::KeyA)));
    }
    #[test]
    fn assistive_focus_handoff_never_reports_old_or_new_pty_focus_and_return_resynchronizes() {
        assert_eq!(focus_report(true, false, true), None);
        assert_eq!(focus_report(false, true, true), None);
        assert_eq!(focus_report(true, false, false), Some(false));
        assert_eq!(focus_report(false, true, false), Some(true));
        assert_eq!(focus_report(true, true, false), None);
    }
    #[test]
    fn inclusive_selection_of_either_wide_cell_contains_the_glyph_and_combining_cluster() {
        let mut state = State::default();
        let source = Arc::new(RenderSnapshot {
            rows: 1,
            columns: 3,
            lines: vec![Arc::new(RowSnapshot {
                cells: ["漢", "", "e\u{301}"]
                    .map(|text| CellSnapshot {
                        text: CellText::new(text),
                        ..Default::default()
                    })
                    .to_vec(),
                ..Default::default()
            })]
            .into(),
            ..RenderSnapshot::empty()
        });
        state.refresh(
            &source,
            View {
                columns: 3,
                ..view()
            },
            Instant::now(),
            true,
        );
        state.ensure_ids(&Id::new("wide"));
        for col in [0, 1] {
            let selection = state.grid_selection((0, col), (0, col), 0).unwrap();
            assert_eq!(selection.anchor.character_index, 0);
            assert_eq!(selection.focus.character_index, 1);
        }
        let selection = state.grid_selection((0, 2), (0, 2), 0).unwrap();
        assert_eq!(selection.anchor.character_index, 1);
        assert_eq!(selection.focus.character_index, 2);
    }
}
