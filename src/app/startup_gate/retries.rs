//! Saved terminals whose startup attach or inspection ended without settling.
//!
//! Nothing else retries them while their cards stay unopened, so the gate
//! tries again through the same attach-only paths. Unchanged refusals are
//! reported once a window measured from the first of them has passed; a
//! different reason, a new descriptor or a new registry entry for the pane is
//! new evidence and restarts that window.
use super::*;

/// Paces retries so a refusal that returns at once does not spin.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long one unchanged refusal is retried before the gate reports it.
const RETRY_WINDOW: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(in crate::app) struct Retries {
    records: HashMap<CardId, Record>,
}

struct Record {
    basis: Basis,
    reason: String,
    attempts: u32,
    since: Instant,
    at: Instant,
}

/// What a refusal was about: the pane's exact descriptor, and the registry's
/// entry for that pane when a shared view projects it.
#[derive(PartialEq)]
struct Basis {
    session: TerminalSession,
    shared: Option<crate::workspace_model::views::Pane>,
}

impl Retries {
    fn record(&self, card: CardId, basis: &Basis) -> Option<&Record> {
        self.records
            .get(&card)
            .filter(|record| &record.basis == basis)
    }

    fn failed(&mut self, card: CardId, basis: Basis, reason: String) {
        let now = Instant::now();
        let (attempts, since) = self
            .record(card, &basis)
            .filter(|record| record.reason == reason)
            .map_or((1, now), |record| (record.attempts + 1, record.since));
        self.records.insert(
            card,
            Record {
                basis,
                reason,
                attempts,
                since,
                at: now,
            },
        );
    }

    fn exhausted(&self, card: CardId, basis: &Basis) -> Option<&str> {
        self.record(card, basis)
            .filter(|record| record.attempts > 1 && record.since.elapsed() >= RETRY_WINDOW)
            .map(|record| record.reason.as_str())
    }

    fn may_retry(&self, card: CardId, basis: &Basis) -> bool {
        self.record(card, basis).is_none_or(|record| {
            self.exhausted(card, basis).is_none() && record.at.elapsed() >= RETRY_DELAY
        })
    }

    /// Ends every pause between retries.
    #[cfg(test)]
    pub(in crate::app) fn expire_delay(&mut self) {
        for record in self.records.values_mut() {
            record.at -= RETRY_DELAY;
        }
    }

    /// Ends every pause and every retry window.
    #[cfg(test)]
    pub(in crate::app) fn expire_window(&mut self) {
        self.expire_delay();
        for record in self.records.values_mut() {
            record.since -= RETRY_WINDOW;
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
            .cloned();
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
        let basis = self
            .startup_attach_basis(card)
            .filter(|basis| basis.session.id == terminal);
        match basis {
            Some(basis) if self.startup_gate.opened.is_some() && !self.startup_gate.reported => {
                tracing::warn!(?card, %terminal, %reason, "startup terminal stays detached");
                self.startup_gate.retries.failed(card, basis, reason);
            }
            _ => tracing::debug!(?card, %terminal, %reason, "restored terminal stays detached"),
        }
    }

    /// The refusal to report once `card`'s current pane has used its retries.
    pub(super) fn startup_attach_exhausted(&self, card: CardId) -> Option<&str> {
        let basis = self.startup_attach_basis(card)?;
        self.startup_gate.retries.exhausted(card, &basis)
    }

    pub(super) fn startup_attach_may_retry(&self, card: CardId) -> bool {
        self.startup_attach_basis(card)
            .is_some_and(|basis| self.startup_gate.retries.may_retry(card, &basis))
    }
}
