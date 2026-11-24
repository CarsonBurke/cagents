//! The target worker's per-item preservation policy and observed outcomes.
use serde::{Deserialize, Serialize};

use super::impact::Policy;
use crate::model::{HostSessionProof, TerminalId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Item {
    Terminal(TerminalId),
    NativeChat(TerminalId),
    WindowState(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Continuity {
    Carry,
    Resume,
    Restart,
    SetAside(Reason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Carry,
    Resume,
    Restart,
    SetAside(Reason),
    End(EndLoss),
}

impl From<Continuity> for Disposition {
    fn from(value: Continuity) -> Self {
        match value {
            Continuity::Carry => Self::Carry,
            Continuity::Resume => Self::Resume,
            Continuity::Restart => Self::Restart,
            Continuity::SetAside(reason) => Self::SetAside(reason),
        }
    }
}

impl Disposition {
    fn rank(self) -> Option<u8> {
        match self {
            Self::Carry => Some(3),
            Self::Resume => Some(2),
            Self::Restart => Some(1),
            Self::SetAside(_) => Some(0),
            Self::End(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    CheckpointOmitted,
    CheckpointChanged,
    VerificationChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndLoss {
    InterruptTurn,
    StopProcess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ending {
    Exited,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Carried,
    Resumed,
    Restarted,
    SetAside(Reason),
    Ended,
    Gone(Ending),
}

impl Outcome {
    fn continuity(self) -> Option<Disposition> {
        match self {
            Self::Carried => Some(Disposition::Carry),
            Self::Resumed => Some(Disposition::Resume),
            Self::Restarted => Some(Disposition::Restart),
            Self::SetAside(reason) => Some(Disposition::SetAside(reason)),
            Self::Ended | Self::Gone(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Plan {
    Ready(Continuity),
    Waiting { then: Continuity },
}

impl Plan {
    fn continuity(self) -> Continuity {
        match self {
            Self::Ready(continuity) | Self::Waiting { then: continuity } => continuity,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Observation {
    Source,
    LateSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Plan,
    Drain,
    Release,
    Verify,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResultAtStage {
    Note(Reason),
    AcceptedEnd(EndRequest),
    EndNotNeeded(EndRequest),
    Outcome(Outcome),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StageResult {
    stage: Stage,
    result: ResultAtStage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Row {
    item: Item,
    observed: Observation,
    plan: Plan,
    stages: Vec<StageResult>,
}

impl Row {
    pub(crate) fn item(&self) -> &Item {
        &self.item
    }

    pub(crate) fn disposition(&self) -> Disposition {
        self.stages
            .iter()
            .rev()
            .find_map(|entry| match &entry.result {
                ResultAtStage::AcceptedEnd(request) => self
                    .allows_end(request)
                    .then_some(Disposition::End(request.loss)),
                ResultAtStage::Outcome(outcome) => outcome.continuity(),
                ResultAtStage::EndNotNeeded(_) | ResultAtStage::Note(_) => None,
            })
            .unwrap_or_else(|| self.plan.continuity().into())
    }

    fn allows_end(&self, request: &EndRequest) -> bool {
        self.stages
            .iter()
            .rev()
            .find_map(|entry| match &entry.result {
                ResultAtStage::AcceptedEnd(accepted) if accepted.covers(request) => Some(true),
                ResultAtStage::EndNotNeeded(unused) if unused.covers(request) => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }

    pub(crate) fn outcome(&self) -> Option<Outcome> {
        self.stages
            .iter()
            .rev()
            .find_map(|entry| match &entry.result {
                ResultAtStage::Outcome(outcome) => Some(*outcome),
                _ => None,
            })
    }

    #[cfg(test)]
    pub(crate) fn has_note(&self, stage: Stage, reason: Reason) -> bool {
        self.stages.contains(&StageResult {
            stage,
            result: ResultAtStage::Note(reason),
        })
    }

    fn append(&mut self, stage: Stage, result: ResultAtStage) {
        self.stages.push(StageResult { stage, result });
    }
}

/// An End decision supplies an opaque owner witness. Only a decision that
/// would stop work needs it, and it never applies across source daemons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EndRequest {
    item: Item,
    source: HostSessionProof,
    owner: String,
    loss: EndLoss,
    subagents: Vec<String>,
}

impl EndRequest {
    pub(crate) fn item(&self) -> &Item {
        &self.item
    }

    pub(crate) fn covers(&self, observed: &Self) -> bool {
        self.item == observed.item
            && self.source == observed.source
            && self.source.is_complete()
            && !self.owner.is_empty()
            && self.owner == observed.owner
            && self.loss == observed.loss
            && observed
                .subagents
                .iter()
                .all(|subagent| self.subagents.contains(subagent))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Planning,
    Frozen,
    Released,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ledger {
    job: String,
    from: Option<HostSessionProof>,
    to: String,
    policy: Policy,
    phase: Phase,
    rows: Vec<Row>,
}

impl Ledger {
    pub(crate) fn new(
        job: String,
        from: Option<HostSessionProof>,
        to: String,
        policy: Policy,
    ) -> Self {
        Self {
            job,
            from,
            to,
            policy,
            phase: Phase::Planning,
            rows: Vec::new(),
        }
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn attention_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| match row.outcome() {
                Some(outcome) => matches!(outcome, Outcome::SetAside(_)),
                None => matches!(row.disposition(), Disposition::SetAside(_)),
            })
            .count()
    }

    pub(crate) fn observe_late(&mut self, item: Item, continuity: Continuity) {
        self.observe(item, Observation::LateSource, Plan::Ready(continuity));
    }

    fn observe(&mut self, item: Item, observed: Observation, plan: Plan) {
        let planning = self.phase == Phase::Planning;
        if let Some(row) = self.row_mut(&item) {
            if planning
                && observed == Observation::Source
                && matches!(row.plan, Plan::Waiting { .. })
            {
                row.plan = plan;
            }
            return;
        }
        self.rows.push(Row {
            item,
            observed: if planning {
                observed
            } else {
                Observation::LateSource
            },
            plan,
            stages: Vec::new(),
        });
    }

    pub(crate) fn freeze(&mut self) {
        if self.phase == Phase::Planning {
            self.phase = Phase::Frozen;
        }
    }

    pub(crate) fn release(&mut self) {
        self.phase = Phase::Released;
    }

    pub(crate) fn accept_end(&mut self, request: EndRequest) {
        if self.phase == Phase::Released
            || self.from.as_ref() != Some(&request.source)
            || !request.source.is_complete()
            || request.owner.is_empty()
            || self.allows_end(&request)
        {
            return;
        }
        let item = request.item.clone();
        self.observe_late(item.clone(), Continuity::Resume);
        self.append(&item, Stage::Plan, ResultAtStage::AcceptedEnd(request));
    }

    pub(crate) fn allows_end(&self, request: &EndRequest) -> bool {
        self.rows
            .iter()
            .find(|row| row.item == request.item)
            .is_some_and(|row| row.allows_end(request))
    }

    pub(crate) fn settle_unused_end(&mut self, request: &EndRequest, stage: Stage) {
        if self.allows_end(request)
            && let Some(row) = self.row_mut(&request.item)
            && !matches!(row.outcome(), Some(Outcome::Ended | Outcome::Gone(_)))
            && let Some(accepted) = row
                .stages
                .iter()
                .rev()
                .find_map(|entry| match &entry.result {
                    ResultAtStage::AcceptedEnd(accepted) if accepted.covers(request) => {
                        Some(accepted.clone())
                    }
                    _ => None,
                })
        {
            row.append(stage, ResultAtStage::EndNotNeeded(accepted));
        }
    }

    pub(crate) fn record_ended(&mut self, request: &EndRequest, stage: Stage) {
        if self.allows_end(request) {
            self.record_terminal_outcome(&request.item, stage, Outcome::Ended);
        }
    }

    pub(crate) fn mark_gone(&mut self, item: &Item, stage: Stage, ending: Ending) {
        self.record_terminal_outcome(item, stage, Outcome::Gone(ending));
    }

    fn record_terminal_outcome(&mut self, item: &Item, stage: Stage, outcome: Outcome) {
        if let Some(row) = self.row_mut(item)
            && !matches!(row.outcome(), Some(Outcome::Ended | Outcome::Gone(_)))
        {
            row.append(stage, ResultAtStage::Outcome(outcome));
        }
    }

    pub(crate) fn record_outcome(&mut self, item: &Item, stage: Stage, outcome: Outcome) {
        if let Some(row) = self.row_mut(item)
            && !matches!(row.outcome(), Some(Outcome::Ended | Outcome::Gone(_)))
            && let Some(before) = row.disposition().rank()
            && let Some(after) = outcome.continuity().and_then(Disposition::rank)
            && after <= before
        {
            row.append(stage, ResultAtStage::Outcome(outcome));
        }
    }

    pub(crate) fn set_aside(&mut self, item: &Item, stage: Stage, reason: Reason) {
        self.record_outcome(item, stage, Outcome::SetAside(reason));
    }

    pub(crate) fn note(&mut self, item: &Item, stage: Stage, reason: Reason) {
        self.append(item, stage, ResultAtStage::Note(reason));
    }

    fn row_mut(&mut self, item: &Item) -> Option<&mut Row> {
        self.rows.iter_mut().find(|row| row.item() == item)
    }

    fn append(&mut self, item: &Item, stage: Stage, result: ResultAtStage) {
        if let Some(row) = self.row_mut(item) {
            row.append(stage, result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        }
    }

    fn ledger() -> Ledger {
        Ledger::new(
            "job".into(),
            Some(proof()),
            "target".into(),
            Policy::WhenIdle,
        )
    }

    fn request(item: Item, loss: EndLoss) -> EndRequest {
        EndRequest {
            item,
            source: proof(),
            owner: "observed owner".into(),
            loss,
            subagents: vec!["shown task".into()],
        }
    }

    #[test]
    fn continuity_is_monotone_after_release_for_every_stage_sequence() {
        let initial = [
            Continuity::Carry,
            Continuity::Resume,
            Continuity::Restart,
            Continuity::SetAside(Reason::VerificationChanged),
        ];
        let outcomes = [
            Outcome::Carried,
            Outcome::Resumed,
            Outcome::Restarted,
            Outcome::SetAside(Reason::VerificationChanged),
        ];
        let item = Item::Terminal(TerminalId::new());
        for continuity in initial {
            for mut sequence in 0..outcomes.len().pow(4) {
                let mut ledger = ledger();
                ledger.observe_late(item.clone(), continuity);
                ledger.release();
                for stage in [Stage::Drain, Stage::Release, Stage::Verify, Stage::Verify] {
                    let before = ledger.rows()[0].disposition().rank().unwrap();
                    let outcome = outcomes[sequence % outcomes.len()];
                    let after = outcome.continuity().unwrap().rank().unwrap();
                    ledger.record_outcome(&item, stage, outcome);
                    assert_eq!(
                        ledger.rows()[0].disposition().rank(),
                        Some(before.min(after))
                    );
                    assert_eq!(ledger.rows().len(), 1);
                    assert_eq!(ledger.rows()[0].item(), &item);
                    sequence /= outcomes.len();
                }
            }
        }
    }

    #[test]
    fn reobservation_keeps_frozen_rows_and_appends_late_items() {
        let mut ledger = ledger();
        let item = Item::NativeChat(TerminalId::new());
        ledger.observe(
            item.clone(),
            Observation::Source,
            Plan::Waiting {
                then: Continuity::Resume,
            },
        );
        ledger.observe(
            item.clone(),
            Observation::Source,
            Plan::Ready(Continuity::Carry),
        );
        assert_eq!(ledger.rows()[0].disposition(), Disposition::Carry);
        ledger.freeze();
        let frozen = ledger.rows().to_vec();
        ledger.observe(
            item,
            Observation::Source,
            Plan::Ready(Continuity::SetAside(Reason::CheckpointChanged)),
        );
        ledger.observe_late(Item::WindowState("primary".into()), Continuity::Carry);
        ledger.release();
        ledger.observe_late(Item::Terminal(TerminalId::new()), Continuity::Restart);
        assert_eq!(&ledger.rows()[..frozen.len()], frozen.as_slice());
        assert_eq!(ledger.rows().len(), 3);
        assert_eq!(ledger.rows()[1].observed, Observation::LateSource);
        let bytes = serde_json::to_vec(&ledger).unwrap();
        assert_eq!(serde_json::from_slice::<Ledger>(&bytes).unwrap(), ledger);
    }

    #[test]
    fn only_a_decision_for_the_same_owner_and_tasks_allows_end() {
        for loss in [EndLoss::InterruptTurn, EndLoss::StopProcess] {
            let mut ledger = Ledger::new(
                "job".into(),
                Some(proof()),
                "target".into(),
                Policy::Interrupt,
            );
            let request = request(Item::NativeChat(TerminalId::new()), loss);
            ledger.observe_late(request.item().clone(), Continuity::Resume);
            ledger.record_ended(&request, Stage::Release);
            ledger.record_outcome(request.item(), Stage::Release, Outcome::Ended);
            assert_eq!(ledger.rows()[0].outcome(), None);
            ledger.freeze();
            ledger.accept_end(request.clone());
            assert_eq!(ledger.rows()[0].disposition(), Disposition::End(loss));
            let mut changed = request.clone();
            changed.subagents.clear();
            assert!(ledger.allows_end(&changed));
            changed.subagents.push("unseen task".into());
            assert!(!ledger.allows_end(&changed));
            changed = request.clone();
            changed.owner.push_str(" replacement");
            assert!(!ledger.allows_end(&changed));
            changed = request.clone();
            changed.source.host_incarnation = HostIncarnation::new("replacement");
            assert!(!ledger.allows_end(&changed));
            ledger.release();
            changed = request.clone();
            changed.item = Item::NativeChat(TerminalId::new());
            ledger.accept_end(changed.clone());
            assert!(!ledger.allows_end(&changed));
            ledger.record_ended(&request, Stage::Release);
            let ended = ledger.clone();
            ledger.record_outcome(request.item(), Stage::Verify, Outcome::Resumed);
            ledger.mark_gone(request.item(), Stage::Verify, Ending::Exited);
            assert_eq!(ledger, ended);
            assert_eq!(ledger.rows()[0].outcome(), Some(Outcome::Ended));
        }
    }

    #[test]
    fn observed_endings_are_gone_without_an_end_decision() {
        for ending in [Ending::Exited, Ending::Closed] {
            let mut ledger = ledger();
            let item = Item::Terminal(TerminalId::new());
            ledger.observe_late(item.clone(), Continuity::Carry);
            ledger.release();
            ledger.note(&item, Stage::Release, Reason::CheckpointOmitted);
            assert!(ledger.rows()[0].has_note(Stage::Release, Reason::CheckpointOmitted));
            ledger.mark_gone(&item, Stage::Verify, ending);
            ledger.record_outcome(&item, Stage::Verify, Outcome::Carried);
            assert_eq!(ledger.rows()[0].outcome(), Some(Outcome::Gone(ending)));
            assert_eq!(ledger.attention_count(), 0);
        }
    }

    #[test]
    fn unused_end_permission_preserves_prior_downgrades() {
        let mut ledger = ledger();
        let request = request(Item::NativeChat(TerminalId::new()), EndLoss::InterruptTurn);
        ledger.observe_late(request.item().clone(), Continuity::Carry);
        ledger.set_aside(request.item(), Stage::Release, Reason::VerificationChanged);
        ledger.accept_end(request.clone());
        ledger.release();
        ledger.settle_unused_end(&request, Stage::Release);
        assert!(!ledger.allows_end(&request));
        assert_eq!(
            ledger.rows()[0].disposition(),
            Disposition::SetAside(Reason::VerificationChanged)
        );
        ledger.record_outcome(request.item(), Stage::Verify, Outcome::Carried);
        assert_eq!(
            ledger.rows()[0].outcome(),
            Some(Outcome::SetAside(Reason::VerificationChanged))
        );
        assert_eq!(ledger.attention_count(), 1);
    }
}
