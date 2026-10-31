//! Saved terminals the gate is waiting on without anything settling them.
//!
//! An attach-only attempt or a saved terminal's inspection that ends without
//! a surface has nothing else retrying it while its card stays unopened, so
//! the gate tries again through the same attach-only paths. A pane held back
//! from attaching, or attached under a different terminal service than its
//! record names, is observed on every check instead, and forgotten once the
//! condition no longer holds.
//!
//! Either is reported once it has stayed unsettled through a window measured
//! from its first occurrence. A new descriptor, or a change to the registry
//! entry's attach fields, is new evidence and restarts that window; a changed
//! reason alone does not. No pane is retried past an overall limit, however
//! its evidence churns.
use super::*;

/// Paces retries so a refusal that returns at once does not spin.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long one unchanged pane is left unsettled before the gate reports it.
pub(super) const RETRY_WINDOW: Duration = Duration::from_secs(30);
/// How long any pane is left unsettled before the gate reports it, even when
/// its descriptor or registry entry keeps changing.
const SETTLE_LIMIT: Duration = Duration::from_secs(90);

#[derive(Default)]
pub(in crate::app) struct Retries {
    /// Attach-only attempts and inspections that ended without settling.
    attempts: HashMap<CardId, Record>,
    /// Conditions seen on the latest check, dropped once one no longer holds.
    observed: HashMap<CardId, Record>,
}

struct Record {
    basis: Basis,
    reason: String,
    /// The evidence last logged with `reason`.
    detail: String,
    /// Occurrences since the basis last changed.
    occurrences: u32,
    /// When the basis last changed.
    since: Instant,
    /// When this pane was first unsettled, under any basis.
    first: Instant,
    /// The latest occurrence.
    at: Instant,
}

/// What an attach depends on: the pane's exact descriptor, and the fields of
/// the registry's entry for it that change what is attached. A rename or
/// resize from another window says nothing about the attachment.
#[derive(PartialEq)]
struct Basis {
    session: TerminalSession,
    shared: Option<SharedBasis>,
}

#[derive(PartialEq)]
struct SharedBasis {
    kind: CardKind,
    scope: Option<PathBuf>,
    pending_terminal_start: bool,
}

impl From<&crate::workspace_model::views::Pane> for SharedBasis {
    fn from(pane: &crate::workspace_model::views::Pane) -> Self {
        Self {
            kind: pane.kind.clone(),
            scope: pane.scope.clone(),
            pending_terminal_start: pane.pending_terminal_start,
        }
    }
}

impl Record {
    fn exhausted(&self, basis: &Basis) -> bool {
        self.first.elapsed() >= SETTLE_LIMIT
            || (self.basis == *basis
                && self.occurrences > 1
                && self.since.elapsed() >= RETRY_WINDOW)
    }

    fn new(basis: Basis, reason: String, detail: &str) -> Self {
        let now = Instant::now();
        Self {
            basis,
            reason,
            detail: detail.to_owned(),
            occurrences: 1,
            since: now,
            first: now,
            at: now,
        }
    }

    /// Records one more occurrence, and whether its reason or evidence is new.
    fn recur(&mut self, basis: Basis, reason: String, detail: &str) -> bool {
        let now = Instant::now();
        if self.basis == basis {
            self.occurrences += 1;
        } else {
            self.basis = basis;
            self.occurrences = 1;
            self.since = now;
        }
        self.at = now;
        let changed = self.reason != reason || self.detail != detail;
        self.reason = reason;
        if self.detail != detail {
            self.detail = detail.to_owned();
        }
        changed
    }
}

fn note(
    records: &mut HashMap<CardId, Record>,
    card: CardId,
    basis: Basis,
    reason: String,
    detail: &str,
) -> bool {
    match records.entry(card) {
        std::collections::hash_map::Entry::Occupied(mut record) => {
            record.get_mut().recur(basis, reason, detail)
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(Record::new(basis, reason, detail));
            true
        }
    }
}

impl Retries {
    fn failed(&mut self, card: CardId, basis: Basis, reason: String) {
        note(&mut self.attempts, card, basis, reason, "");
    }

    /// Records that a condition still holds, and whether its reason or
    /// evidence is new.
    fn observed(&mut self, card: CardId, basis: Basis, reason: String, detail: &str) -> bool {
        note(&mut self.observed, card, basis, reason, detail)
    }

    /// The condition `card` was observed in no longer holds.
    fn settled(&mut self, card: CardId) {
        self.observed.remove(&card);
    }

    fn exhausted(&self, card: CardId, basis: &Basis) -> Option<&str> {
        [&self.observed, &self.attempts]
            .into_iter()
            .filter_map(|records| records.get(&card))
            .find(|record| record.exhausted(basis))
            .map(|record| record.reason.as_str())
    }

    /// The latest reason `card` is not settled, if one is known.
    fn latest(&self, card: CardId) -> Option<&str> {
        self.observed
            .get(&card)
            .or_else(|| self.attempts.get(&card))
            .map(|record| record.reason.as_str())
    }

    /// Paced per pane, so a basis that keeps changing cannot speed it up.
    fn may_retry(&self, card: CardId, basis: &Basis) -> bool {
        self.attempts
            .get(&card)
            .is_none_or(|record| !record.exhausted(basis) && record.at.elapsed() >= RETRY_DELAY)
    }

    #[cfg(test)]
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

    /// Ends every pause and every window for an unchanged basis.
    #[cfg(test)]
    pub(in crate::app) fn expire_window(&mut self) {
        self.expire_delay();
        for record in self.records_mut() {
            record.since -= RETRY_WINDOW;
        }
    }

    /// Ends every pause, window and overall limit.
    #[cfg(test)]
    pub(in crate::app) fn expire_limit(&mut self) {
        self.expire_window();
        for record in self.records_mut() {
            record.first -= SETTLE_LIMIT;
        }
    }
}

impl Cagents {
    fn startup_attach_basis(&self, card: CardId) -> Option<Basis> {
        let CardKind::Terminal(session) = &self.card_state(card)?.kind else {
            return None;
        };
        let shared = self
            .workspace_model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .find(|pane| pane.id == card)
            .map(SharedBasis::from);
        Some(Basis {
            session: session.clone(),
            shared,
        })
    }

    /// An attach-only attempt for `terminal` ended without a surface. Only a
    /// gate waiting on that exact pane records it, and only then is it
    /// worth a warning: on an ordinary launch the pane just stays detached
    /// until it is opened.
    pub(in crate::app) fn startup_attach_failed(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        reason: String,
    ) {
        match self.startup_basis_for(card, terminal) {
            Some(basis) => {
                tracing::warn!(?card, %terminal, %reason, "startup terminal stays detached");
                self.startup_gate.retries.failed(card, basis, reason);
            }
            None => tracing::debug!(?card, %terminal, %reason, "restored terminal stays detached"),
        }
    }

    /// `card` is still not settled on this check, for `reason`, without an
    /// attempt having failed: it is held back from attaching, or attached
    /// under a different service than its record names. `condition` names
    /// which, and `detail` its evidence, for the log each time it changes.
    pub(in crate::app) fn startup_terminal_unsettled(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        condition: &'static str,
        reason: String,
        detail: &str,
    ) {
        if let Some(basis) = self.startup_basis_for(card, terminal)
            && self
                .startup_gate
                .retries
                .observed(card, basis, reason.clone(), detail)
        {
            tracing::debug!(?card, %terminal, condition, %reason, detail, "startup terminal is not settled");
        }
    }

    /// No condition holds `card` on this check.
    pub(in crate::app) fn startup_terminal_settled(&mut self, card: CardId) {
        self.startup_gate.retries.settled(card);
    }

    /// The latest reason `card` is not settled, if one is known.
    pub(super) fn startup_terminal_latest(&self, card: CardId) -> Option<&str> {
        self.startup_gate.retries.latest(card)
    }

    /// The basis to record `terminal`'s outcome under, while a gate waits on
    /// that exact pane.
    fn startup_basis_for(&self, card: CardId, terminal: TerminalId) -> Option<Basis> {
        if self.startup_gate.opened.is_none() || self.startup_gate.reported {
            return None;
        }
        self.startup_attach_basis(card)
            .filter(|basis| basis.session.id == terminal)
    }

    /// Why `card` is reported, once it has stayed unsettled too long.
    pub(super) fn startup_attach_exhausted(&self, card: CardId) -> Option<&str> {
        let basis = self.startup_attach_basis(card)?;
        self.startup_gate.retries.exhausted(card, &basis)
    }

    pub(super) fn startup_attach_may_retry(&self, card: CardId) -> bool {
        self.startup_attach_basis(card)
            .is_some_and(|basis| self.startup_gate.retries.may_retry(card, &basis))
    }
}
