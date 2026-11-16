//! Saved terminals the gate is waiting on without anything settling them.
//!
//! An attach-only attempt or a saved terminal's inspection that ends without
//! a surface has nothing else retrying it while its card stays unopened, so
//! the gate tries again through the same attach-only paths. A pane held back
//! from attaching, or attached under a different terminal service than its
//! record names, is observed on every check instead, and forgotten once the
//! condition no longer holds.
//!
//! Either is reported once it has lasted a window measured from its first
//! occurrence. A replaced terminal starts that window again, and so does the
//! daemon saying it is still restoring.
use super::*;

/// Paces retries so a refusal that returns at once does not spin.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long a pane is left unsettled before the gate reports it.
pub(super) const RETRY_WINDOW: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(in crate::app) struct Retries {
    /// Attach-only attempts and inspections that ended without settling.
    attempts: HashMap<CardId, Record>,
    /// Conditions seen on the latest check, dropped once one no longer holds.
    observed: HashMap<CardId, Record>,
    /// Panes whose latest record was made while they could do nothing until
    /// the registry answered, so it says nothing once it has.
    awaiting_registry: HashSet<CardId>,
}

struct Record {
    terminal: TerminalId,
    reason: String,
    since: Instant,
    /// The latest occurrence.
    at: Instant,
}

impl Record {
    fn exhausted(&self, terminal: TerminalId) -> bool {
        self.terminal == terminal && self.since.elapsed() >= RETRY_WINDOW
    }
}

/// Records one occurrence, and returns whether its reason is new.
fn note(
    records: &mut HashMap<CardId, Record>,
    card: CardId,
    terminal: TerminalId,
    reason: String,
) -> bool {
    let now = Instant::now();
    match records.entry(card) {
        std::collections::hash_map::Entry::Occupied(mut record) => {
            let record = record.get_mut();
            if record.terminal != terminal {
                record.terminal = terminal;
                record.since = now;
            }
            record.at = now;
            let changed = record.reason != reason;
            record.reason = reason;
            changed
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(Record {
                terminal,
                reason,
                since: now,
                at: now,
            });
            true
        }
    }
}

impl Retries {
    fn exhausted(&self, card: CardId, terminal: TerminalId) -> Option<&str> {
        [&self.observed, &self.attempts]
            .into_iter()
            .filter_map(|records| records.get(&card))
            .find(|record| record.exhausted(terminal))
            .map(|record| record.reason.as_str())
    }

    /// The latest reason `card` is not settled, if one is known.
    fn latest(&self, card: CardId) -> Option<&str> {
        self.observed
            .get(&card)
            .or_else(|| self.attempts.get(&card))
            .map(|record| record.reason.as_str())
    }

    /// Paced per pane, so a terminal that keeps being replaced cannot speed it up.
    fn may_retry(&self, card: CardId, terminal: TerminalId) -> bool {
        self.attempts
            .get(&card)
            .is_none_or(|record| !record.exhausted(terminal) && record.at.elapsed() >= RETRY_DELAY)
    }

    /// Starts every pane's window again. A daemon still restoring is expected
    /// to refuse them, so that time does not count against them.
    pub(super) fn restart_windows(&mut self) {
        let now = Instant::now();
        for record in self.records_mut() {
            record.since = now;
        }
    }

    fn records_mut(&mut self) -> impl Iterator<Item = &mut Record> {
        self.attempts.values_mut().chain(self.observed.values_mut())
    }

    /// Ends every pause between retries.
    #[cfg(test)]
    pub(in crate::app) fn expire_delay(&mut self) {
        for record in self.records_mut() {
            record.at -= RETRY_DELAY;
        }
    }

    /// Ends every pause and every window.
    #[cfg(test)]
    pub(in crate::app) fn expire_window(&mut self) {
        self.expire_delay();
        for record in self.records_mut() {
            record.since -= RETRY_WINDOW;
        }
    }
}

impl Cagents {
    /// The saved terminal `card` names.
    fn saved_terminal_of(&self, card: CardId) -> Option<TerminalId> {
        match &self.card_state(card)?.kind {
            CardKind::Terminal(session) => Some(session.id),
            _ => None,
        }
    }

    /// The saved terminal `card` names, while a gate is waiting on it.
    fn startup_terminal(&self, card: CardId) -> Option<TerminalId> {
        if self.startup_gate.opened.is_none() || self.startup_gate.reported {
            return None;
        }
        self.saved_terminal_of(card)
    }

    /// An attach-only attempt for `terminal` ended without a surface. Only a
    /// gate for that exact pane records it, and only then is it worth a
    /// warning: on an ordinary launch the pane just stays detached until it is
    /// opened. An attempt that outlives the gate's report still counts, so
    /// its pane says why rather than spinning until someone opens it.
    pub(in crate::app) fn startup_attach_failed(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        reason: String,
    ) {
        if self.startup_gate.opened.is_some() && self.saved_terminal_of(card) == Some(terminal) {
            tracing::warn!(?card, %terminal, %reason, "startup terminal stays detached");
            self.startup_note_registry_wait(card);
            let retries = &mut self.startup_gate.retries;
            note(&mut retries.attempts, card, terminal, reason);
        } else {
            tracing::debug!(?card, %terminal, %reason, "restored terminal stays detached");
        }
    }

    /// `card` is still not settled on this check, for `reason`, without an
    /// attempt having failed: it is held back from attaching, or attached
    /// under a different service than its record names. `detail` is the
    /// evidence for the log each time the reason changes.
    pub(in crate::app) fn startup_terminal_unsettled(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        reason: &str,
        detail: &str,
    ) {
        if self.startup_terminal(card) == Some(terminal) {
            self.startup_note_registry_wait(card);
        }
        if self.startup_terminal(card) == Some(terminal)
            && note(
                &mut self.startup_gate.retries.observed,
                card,
                terminal,
                reason.to_owned(),
            )
        {
            tracing::debug!(?card, %terminal, %reason, detail, "startup terminal is not settled");
        }
    }

    fn startup_note_registry_wait(&mut self, card: CardId) {
        if self.startup_pane_needs_registry(card) {
            self.startup_gate.retries.awaiting_registry.insert(card);
        } else {
            self.startup_gate.retries.awaiting_registry.remove(&card);
        }
    }

    /// Once the gate has reported, nothing checks its records again. One made
    /// while a pane waited for the registry is stale when the registry
    /// answers, and would have the tab read as failed while opening it works.
    pub(in crate::app) fn retire_startup_registry_waits(&mut self) {
        if !self.startup_gate.reported {
            return;
        }
        let waits = std::mem::take(&mut self.startup_gate.retries.awaiting_registry);
        for card in waits {
            if self.startup_pane_needs_registry(card) {
                self.startup_gate.retries.awaiting_registry.insert(card);
            } else {
                self.startup_gate.retries.attempts.remove(&card);
                self.startup_gate.retries.observed.remove(&card);
            }
        }
    }

    /// No condition holds `card` on this check.
    pub(in crate::app) fn startup_terminal_settled(&mut self, card: CardId) {
        self.startup_gate.retries.observed.remove(&card);
    }

    /// `card` has a surface, so no attempt to attach it is outstanding.
    pub(in crate::app) fn startup_terminal_attached(&mut self, card: CardId) {
        self.startup_gate.retries.attempts.remove(&card);
    }

    /// The gate reported `card` as not reconnected and nothing is trying it
    /// now, so its tab must not read as still starting.
    pub(in crate::app) fn startup_terminal_gave_up(&self, card: CardId) -> bool {
        self.startup_gate.reported
            && !self.terminal_start_in_flight(card)
            && self.startup_gate.retries.latest(card).is_some()
    }

    /// What the tab of a loading terminal shows once the gate has reported, if
    /// the gate has a say: the label, and whether it is still under way. One
    /// that is waiting needs nothing done, and one left unopened with nothing
    /// holding it attaches when opened, so neither carries a warning.
    pub(in crate::app) fn startup_terminal_tab_status(
        &self,
        card: CardId,
    ) -> Option<Option<(&'static str, bool)>> {
        if self.startup_terminal_gave_up(card) {
            let waiting = self.startup_pane_held(card) && !self.startup_gate.retained;
            return Some(Some((self.startup_gave_up_title(card), waiting)));
        }
        let unopened = self.card_state(card).is_some()
            && !matches!(self.residency(card), Residency::Hydrating { .. });
        (self.startup_gate.reported
            && unopened
            && matches!(self.saved_terminal_of(card), Some(_))
            && !self.terminal_start_in_flight(card)
            && !self.startup_pane_needs_registry(card))
        .then_some(None)
    }

    /// The tab and pane title of a terminal that gave up: one held for the
    /// registry is still waiting, where any other needs something done.
    pub(in crate::app) fn startup_gave_up_title(&self, card: CardId) -> &'static str {
        if self.startup_pane_held(card) && !self.startup_gate.retained {
            "Waiting for the shared workspace"
        } else {
            "Reconnect unavailable"
        }
    }

    pub(in crate::app) fn startup_gave_up_caption(&self, card: CardId) -> String {
        let reason = self.startup_terminal_latest(card);
        if self.startup_gate.retained {
            super::RETAINED_TERMINAL_CAPTION.to_owned()
        } else if self.startup_pane_held(card) {
            super::held_terminal_caption(reason)
        } else {
            super::unavailable_terminal_caption(reason)
        }
    }

    /// The latest reason `card` is not settled, if one is known.
    pub(in crate::app) fn startup_terminal_latest(&self, card: CardId) -> Option<&str> {
        self.startup_gate.retries.latest(card)
    }

    /// Why `card` is reported, once it has stayed unsettled too long.
    pub(super) fn startup_attach_exhausted(&self, card: CardId) -> Option<&str> {
        let terminal = self.saved_terminal_of(card)?;
        self.startup_gate.retries.exhausted(card, terminal)
    }

    pub(super) fn startup_attach_may_retry(&self, card: CardId) -> bool {
        self.saved_terminal_of(card)
            .is_some_and(|terminal| self.startup_gate.retries.may_retry(card, terminal))
    }
}
