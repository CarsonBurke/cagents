//! The target worker's record of what it intends to preserve and what it observed.
use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::{JOB_STATE_LIMIT, read_private};
use super::{
    UpdateError, floor_display::StopItem, impact, invalid, private_create, recovery,
    source_inspection,
};
use crate::model::{HostSessionProof, TerminalId};

pub(super) const FILE_NAME: &str = "ledger.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Item {
    Terminal(TerminalId),
    NativeChat(TerminalId),
    WindowState(String),
}

impl Item {
    pub(crate) fn session(kind: impact::SessionKind, id: TerminalId) -> Self {
        match kind {
            impact::SessionKind::Terminal => Self::Terminal(id),
            impact::SessionKind::NativeChat => Self::NativeChat(id),
        }
    }
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
    SourceBlocked,
    SavedUnavailable,
    CheckpointOmitted,
    CheckpointChanged,
    VerificationMissing,
    VerificationChanged,
    VerificationUnavailable,
    Paused,
    WindowRecoveryPending,
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
    Observed,
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
            Self::Observed | Self::Ended | Self::Gone(_) => None,
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
    Saved,
    LateSource,
    LateSaved,
    LateSuccessor,
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
    InterruptionSettled(EndRequest),
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

    pub(crate) fn expects_owner(&self) -> bool {
        matches!(self.observed, Observation::Source | Observation::LateSource)
    }

    pub(crate) fn disposition(&self) -> Disposition {
        self.stages
            .iter()
            .rev()
            .find_map(|entry| match &entry.result {
                ResultAtStage::AcceptedEnd(request) if request.loss == EndLoss::StopProcess => self
                    .allows_end(request)
                    .then_some(Disposition::End(request.loss)),
                ResultAtStage::Outcome(outcome) => outcome.continuity(),
                _ => None,
            })
            .unwrap_or_else(|| self.plan.continuity().into())
    }

    fn allows_end(&self, request: &EndRequest) -> bool {
        self.active_end(request).is_some()
    }

    fn active_end(&self, request: &EndRequest) -> Option<&EndRequest> {
        self.stages
            .iter()
            .rev()
            .find_map(|entry| match &entry.result {
                ResultAtStage::AcceptedEnd(accepted) if accepted.covers(request) => {
                    Some(Some(accepted))
                }
                ResultAtStage::EndNotNeeded(settled)
                | ResultAtStage::InterruptionSettled(settled)
                    if settled.covers(request) =>
                {
                    Some(None)
                }
                _ => None,
            })
            .flatten()
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

/// The old review supplies an opaque owner witness. Only a decision that
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

    pub(crate) fn interruption(
        source: &HostSessionProof,
        item: &impact::SessionImpact,
    ) -> Option<Self> {
        (item.disposition == impact::Disposition::Interrupted).then(|| Self {
            item: Item::session(item.kind, item.id),
            source: source.clone(),
            owner: item.identity.clone(),
            loss: EndLoss::InterruptTurn,
            subagents: item.interrupted_subagents.clone(),
        })
    }

    pub(crate) fn interruptions(daemon: &impact::DaemonImpact) -> impl Iterator<Item = Self> + '_ {
        daemon
            .sessions
            .iter()
            .filter_map(|item| Self::interruption(&daemon.capability.proof, item))
    }

    pub(crate) fn stop(source: &HostSessionProof, item: &StopItem) -> Self {
        Self {
            item: Item::session(item.kind, item.id),
            source: source.clone(),
            owner: item.identity.clone(),
            loss: EndLoss::StopProcess,
            subagents: item.interrupted_subagents.clone(),
        }
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
struct SourceBuild {
    version: String,
    protocol: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ledger {
    job: String,
    from: Option<HostSessionProof>,
    source_build: Option<SourceBuild>,
    to: String,
    policy: impact::Policy,
    phase: Phase,
    rows: Vec<Row>,
}

impl Ledger {
    pub(crate) fn new(
        job: String,
        from: Option<HostSessionProof>,
        to: String,
        policy: impact::Policy,
    ) -> Self {
        Self {
            job,
            from,
            source_build: None,
            to,
            policy,
            phase: Phase::Planning,
            rows: Vec::new(),
        }
    }

    pub(crate) fn with_source_build(mut self, version: String, protocol: u16) -> Self {
        self.source_build = Some(SourceBuild { version, protocol });
        self
    }

    #[cfg(test)]
    pub(crate) fn load(path: &Path) -> Result<Self, UpdateError> {
        Ok(serde_json::from_slice(&read_private(
            path,
            JOB_STATE_LIMIT,
        )?)?)
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

    pub(crate) fn finish_saved_rows(&mut self, windows_complete: bool) {
        for row in &mut self.rows {
            if !matches!(row.observed, Observation::Saved | Observation::LateSaved)
                || row.outcome().is_some()
            {
                continue;
            }
            let outcome = match (row.disposition(), windows_complete) {
                (Disposition::End(_), _) => continue,
                (Disposition::SetAside(reason), _) => Outcome::SetAside(reason),
                (_, false) => Outcome::SetAside(Reason::WindowRecoveryPending),
                (Disposition::Carry, true) => Outcome::Carried,
                (Disposition::Resume, true) => Outcome::Resumed,
                (Disposition::Restart, true) => Outcome::Restarted,
            };
            row.append(Stage::Verify, ResultAtStage::Outcome(outcome));
        }
    }

    pub(crate) fn observe_impact(&mut self, daemon: &impact::DaemonImpact) {
        if self.from.as_ref() != Some(&daemon.capability.proof) {
            return;
        }
        for item in &daemon.sessions {
            use impact::Disposition::*;
            let plan = match item.disposition {
                Continues => Plan::Ready(Continuity::Carry),
                Resumes => Plan::Ready(Continuity::Resume),
                WaitsForIdle | Interrupted => Plan::Waiting {
                    then: Continuity::Resume,
                },
                ResumePicker => Plan::Ready(Continuity::Restart),
                Blocked => Plan::Ready(Continuity::SetAside(Reason::SourceBlocked)),
            };
            self.observe(Item::session(item.kind, item.id), Observation::Source, plan);
        }
    }

    pub(crate) fn observe_census(&mut self, census: &recovery::Census) {
        for terminal in &census.terminals {
            let item = Item::Terminal(terminal.id);
            let continuity = if terminal.continuity.live_proof().is_some() {
                Continuity::Carry
            } else {
                Continuity::Restart
            };
            self.observe_late(item.clone(), continuity);
            if matches!(
                terminal.continuity,
                crate::model::TerminalContinuity::Exited { .. }
            ) {
                self.mark_gone(&item, Stage::Plan, Ending::Exited);
            }
        }
        for chat in &census.chats {
            self.observe_late(
                Item::NativeChat(chat.id),
                if chat.native_continuity.retains_provider() {
                    Continuity::Carry
                } else {
                    Continuity::Resume
                },
            );
        }
    }

    pub(crate) fn observe_preview(&mut self, preview: &impact::Preview) {
        if let Some(daemon) = &preview.daemon {
            self.observe_impact(daemon);
        }
        self.observe_saved_panes(&preview.labels.details, &preview.stopped);
    }

    pub(crate) fn observe_saved(&mut self, inspection: &source_inspection::SourceInspection) {
        self.observe_saved_panes(&inspection.details, &inspection.stopped);
    }

    fn observe_saved_panes(
        &mut self,
        details: &source_inspection::ReviewDetails,
        stopped: &[source_inspection::StoppedPane],
    ) {
        for pane in stopped {
            let kind = match pane.kind {
                source_inspection::StoppedKind::Chat => impact::SessionKind::NativeChat,
                source_inspection::StoppedKind::Terminal => impact::SessionKind::Terminal,
            };
            self.observe(
                Item::session(kind, pane.pane),
                Observation::Saved,
                Plan::Ready(Continuity::SetAside(Reason::SavedUnavailable)),
            );
        }
        for (id, pane) in &details.panes {
            let continuity = match pane.outcome {
                source_inspection::SavedOutcome::Resumes => Continuity::Resume,
                source_inspection::SavedOutcome::Unchanged
                | source_inspection::SavedOutcome::KeepsRunning => Continuity::Carry,
            };
            self.observe(
                Item::session(pane.kind, *id),
                Observation::Saved,
                Plan::Ready(continuity),
            );
        }
    }

    pub(crate) fn observe_window(&mut self, slot: String) {
        self.observe(
            Item::WindowState(slot),
            Observation::Saved,
            Plan::Ready(Continuity::Carry),
        );
    }

    pub(crate) fn observe_late(&mut self, item: Item, continuity: Continuity) {
        self.observe(item, Observation::LateSource, Plan::Ready(continuity));
    }

    /// A row first seen on the successor has no source continuity to verify.
    pub(crate) fn observe_successor(&mut self, item: Item) {
        if !self.rows.iter().any(|row| row.item() == &item) {
            self.rows.push(Row {
                item,
                observed: Observation::LateSuccessor,
                plan: Plan::Ready(Continuity::Carry),
                stages: vec![StageResult {
                    stage: Stage::Verify,
                    result: ResultAtStage::Outcome(Outcome::Observed),
                }],
            });
        }
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
            observed: match (planning, observed) {
                (false, Observation::Source) => Observation::LateSource,
                (false, Observation::Saved) => Observation::LateSaved,
                (_, observed) => observed,
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
        if let Some(row) = self.row_mut(&request.item)
            && !matches!(row.outcome(), Some(Outcome::Ended | Outcome::Gone(_)))
            && let Some(accepted) = row.active_end(request)
        {
            row.append(stage, ResultAtStage::EndNotNeeded(accepted.clone()));
        }
    }

    pub(crate) fn settle_unused_stops(
        &mut self,
        proof: &HostSessionProof,
        current: &[StopItem],
        stage: Stage,
    ) {
        for row in &mut self.rows {
            if matches!(row.outcome(), Some(Outcome::Ended | Outcome::Gone(_))) {
                continue;
            }
            for index in 0..row.stages.len() {
                let ResultAtStage::AcceptedEnd(request) = &row.stages[index].result else {
                    continue;
                };
                if request.loss == EndLoss::StopProcess
                    && &request.source == proof
                    && row.allows_end(request)
                    && !current
                        .iter()
                        .any(|item| request.covers(&EndRequest::stop(proof, item)))
                {
                    let unused = request.clone();
                    row.append(stage, ResultAtStage::EndNotNeeded(unused));
                }
            }
        }
    }

    pub(crate) fn record_ended(&mut self, request: &EndRequest, stage: Stage) {
        if let Some(row) = self.row_mut(&request.item)
            && !matches!(row.outcome(), Some(Outcome::Ended | Outcome::Gone(_)))
            && let Some(accepted) = row.active_end(request)
        {
            // The interruption receipt consumes the permission. The conversation
            // remains an item whose continuity still needs verification.
            let result = match request.loss {
                EndLoss::InterruptTurn => ResultAtStage::InterruptionSettled(accepted.clone()),
                EndLoss::StopProcess => ResultAtStage::Outcome(Outcome::Ended),
            };
            row.append(stage, result);
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

    pub(crate) fn save(&self, path: &Path) -> Result<(), UpdateError> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("The update ledger needs a private directory"))?;
        let temporary = parent.join(format!(".ledger-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = private_create(&temporary)?;
            serde_json::to_writer(&mut file, self)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    fn ledger(daemon: &impact::DaemonImpact) -> Ledger {
        Ledger::new(
            "job".into(),
            Some(daemon.capability.proof.clone()),
            "candidate".into(),
            daemon.policy,
        )
        .with_source_build("0.2.3".into(), 198)
    }

    #[test]
    fn continuity_never_improves_after_release() {
        let daemon = impact::tests::waiting();
        let initial = [
            Continuity::Carry,
            Continuity::Resume,
            Continuity::Restart,
            Continuity::SetAside(Reason::SourceBlocked),
        ];
        let outcomes = [
            Outcome::Carried,
            Outcome::Resumed,
            Outcome::Restarted,
            Outcome::SetAside(Reason::VerificationMissing),
        ];
        let item = Item::Terminal(TerminalId::new());
        for continuity in initial {
            // Every ordered sequence of four observations, with repetition.
            for mut sequence in 0..outcomes.len().pow(4) {
                let mut ledger = ledger(&daemon);
                ledger.observe_late(item.clone(), continuity);
                ledger.release();
                for stage in [Stage::Release, Stage::Release, Stage::Verify, Stage::Verify] {
                    let before = ledger.rows()[0].disposition().rank().unwrap();
                    let outcome = outcomes[sequence % outcomes.len()];
                    let rank = outcome.continuity().unwrap().rank().unwrap();
                    ledger.record_outcome(&item, stage, outcome);
                    assert_eq!(
                        ledger.rows()[0].disposition().rank(),
                        Some(before.min(rank))
                    );
                    assert_eq!(ledger.rows().len(), 1);
                    assert_eq!(ledger.rows()[0].item(), &item);
                    sequence /= outcomes.len();
                }
            }
        }
    }

    #[test]
    fn reobservation_can_add_rows_and_preserves_frozen_rows() {
        let mut daemon = impact::tests::waiting();
        let mut ledger = ledger(&daemon);
        ledger.observe_impact(&daemon);
        assert!(matches!(ledger.rows()[0].plan, Plan::Waiting { .. }));
        daemon.sessions[0].disposition = impact::Disposition::Continues;
        ledger.observe_impact(&daemon);
        assert_eq!(ledger.rows()[0].disposition(), Disposition::Carry);
        let original = ledger.rows().to_vec();
        ledger.freeze();
        // After a failed recovery restarted the source, a frozen card can
        // still hold the previous daemon's incarnation for this live item.
        let mut stale = source_inspection::SourceInspection::default();
        stale.stopped.push(source_inspection::StoppedPane {
            pane: daemon.sessions[0].id,
            kind: source_inspection::StoppedKind::Chat,
            label: "Saved conversation".into(),
            reason: "Could not confirm the saved owner".into(),
            recovery: "Reopen saved conversation".into(),
        });
        ledger.observe_saved(&stale);
        daemon.sessions[0].identity = "process changed since the review".into();
        daemon.sessions[0].disposition = impact::Disposition::Blocked;
        let mut late = daemon.sessions[0].clone();
        late.id = TerminalId::new();
        daemon.sessions.push(late);
        ledger.observe_impact(&daemon);
        assert_eq!(&ledger.rows()[..original.len()], original.as_slice());
        assert_eq!(ledger.rows()[1].observed, Observation::LateSource);
        assert_eq!(ledger.attention_count(), 1);

        ledger.release();
        daemon.sessions.clear();
        ledger.observe_impact(&daemon);
        assert_eq!(ledger.rows().len(), 2);
        assert_eq!(&ledger.rows()[..original.len()], original.as_slice());
    }

    #[test]
    fn a_saved_pane_does_not_create_a_daemon_owner_expectation() {
        let daemon = impact::tests::waiting();
        let mut ledger = ledger(&daemon);
        ledger.observe_impact(&daemon);
        ledger.freeze();
        let mut inspection = source_inspection::SourceInspection::default();
        let pane = TerminalId::new();
        inspection.details.panes.insert(
            pane,
            source_inspection::InspectedPane {
                kind: impact::SessionKind::NativeChat,
                outcome: source_inspection::SavedOutcome::Unchanged,
                detail: String::new(),
            },
        );
        inspection.stopped.push(source_inspection::StoppedPane {
            pane,
            kind: source_inspection::StoppedKind::Chat,
            label: "Saved pane".into(),
            reason: "Previously connected owner's incarnation has changed".into(),
            recovery: "Reopen saved history".into(),
        });
        ledger.observe_saved(&inspection);

        assert!(ledger.rows()[0].expects_owner());
        assert!(!ledger.rows()[1].expects_owner());
        assert_eq!(ledger.rows()[1].observed, Observation::LateSaved);
        assert_eq!(
            ledger.rows()[1].disposition(),
            Disposition::SetAside(Reason::SavedUnavailable)
        );
    }

    #[test]
    fn window_verdict_finishes_only_unfinished_saved_rows() {
        let daemon = impact::tests::waiting();
        let mut initial = ledger(&daemon);
        initial.observe_impact(&daemon);
        for continuity in [
            Continuity::Carry,
            Continuity::Resume,
            Continuity::Restart,
            Continuity::SetAside(Reason::SavedUnavailable),
        ] {
            initial.observe(
                Item::NativeChat(TerminalId::new()),
                Observation::Saved,
                Plan::Ready(continuity),
            );
        }
        let completed = Item::NativeChat(TerminalId::new());
        initial.observe(
            completed.clone(),
            Observation::Saved,
            Plan::Ready(Continuity::Carry),
        );
        initial.mark_gone(&completed, Stage::Verify, Ending::Closed);
        initial.release();

        for windows_complete in [false, true] {
            let mut ledger = initial.clone();
            ledger.finish_saved_rows(windows_complete);
            assert_eq!(ledger.rows().len(), initial.rows().len());
            assert_eq!(ledger.rows()[0], initial.rows()[0]);
            assert_eq!(ledger.rows()[5], initial.rows()[5]);
            let expected = if windows_complete {
                [Outcome::Carried, Outcome::Resumed, Outcome::Restarted]
            } else {
                [Outcome::SetAside(Reason::WindowRecoveryPending); 3]
            };
            for (row, expected) in ledger.rows()[1..4].iter().zip(expected) {
                assert_eq!(row.outcome(), Some(expected));
            }
            assert_eq!(
                ledger.rows()[4].outcome(),
                Some(Outcome::SetAside(Reason::SavedUnavailable))
            );
            let finished = ledger.clone();
            ledger.finish_saved_rows(!windows_complete);
            assert_eq!(ledger, finished);
        }
    }

    #[test]
    fn end_requires_a_decision_for_the_same_owner_and_subagents() {
        let mut daemon = impact::tests::waiting();
        let mut ledger = ledger(&daemon);
        daemon.sessions[0].disposition = impact::Disposition::Interrupted;
        daemon.sessions[0].interrupted_subagents = vec!["shown".into()];
        let request = EndRequest::interruptions(&daemon).next().unwrap();
        let item = request.item.clone();
        ledger.observe_impact(&daemon);
        ledger.record_ended(&request, Stage::Release);
        ledger.record_outcome(&item, Stage::Release, Outcome::Ended);
        assert_eq!(ledger.rows()[0].outcome(), None);
        assert_eq!(ledger.rows()[0].disposition(), Disposition::Resume);

        ledger.freeze();
        ledger.accept_end(request.clone());
        assert_eq!(ledger.rows()[0].disposition(), Disposition::Resume);
        let mut current = request.clone();
        current.subagents.clear();
        assert!(ledger.allows_end(&current));
        current.subagents.push("added".into());
        assert!(!ledger.allows_end(&current));
        current.subagents.clear();
        current.owner.push_str("changed");
        assert!(!ledger.allows_end(&current));
        current.owner = request.owner.clone();
        current.source.host_incarnation = crate::model::HostIncarnation::new("restarted");
        assert!(!ledger.allows_end(&current));

        ledger.release();
        current = request.clone();
        current.item = Item::NativeChat(TerminalId::new());
        ledger.accept_end(current.clone());
        assert!(!ledger.allows_end(&current));
        ledger.record_ended(&request, Stage::Release);
        assert!(!ledger.allows_end(&request));
        assert_eq!(ledger.rows()[0].outcome(), None);
        ledger.set_aside(&item, Stage::Verify, Reason::VerificationMissing);
        assert_eq!(
            ledger.rows()[0].outcome(),
            Some(Outcome::SetAside(Reason::VerificationMissing))
        );
        let bytes = serde_json::to_vec(&ledger).unwrap();
        assert_eq!(serde_json::from_slice::<Ledger>(&bytes).unwrap(), ledger);
    }

    #[test]
    fn an_observed_exit_is_gone_without_an_end_decision() {
        let daemon = impact::tests::waiting();
        let mut ledger = ledger(&daemon);
        let item = Item::Terminal(TerminalId::new());
        ledger.observe_late(item.clone(), Continuity::Carry);
        ledger.release();
        ledger.note(&item, Stage::Release, Reason::CheckpointOmitted);
        assert!(ledger.rows()[0].has_note(Stage::Release, Reason::CheckpointOmitted));
        assert_eq!(ledger.rows()[0].outcome(), None);

        ledger.mark_gone(&item, Stage::Verify, Ending::Exited);
        ledger.record_outcome(&item, Stage::Verify, Outcome::Carried);
        assert_eq!(
            ledger.rows()[0].outcome(),
            Some(Outcome::Gone(Ending::Exited))
        );
        assert_eq!(ledger.attention_count(), 0);
    }

    #[test]
    fn an_unused_end_preserves_continuity_and_existing_downgrades() {
        let mut daemon = impact::tests::waiting();
        daemon.sessions[0].disposition = impact::Disposition::Interrupted;
        let request = EndRequest::interruptions(&daemon).next().unwrap();
        for downgrade in [false, true] {
            let mut ledger = ledger(&daemon);
            ledger.observe_impact(&daemon);
            if downgrade {
                ledger.set_aside(&request.item, Stage::Release, Reason::VerificationChanged);
            }
            ledger.accept_end(request.clone());
            let accepted = ledger.clone();
            ledger.release();
            ledger.settle_unused_end(&request, Stage::Release);
            assert!(!ledger.allows_end(&request));
            assert_eq!(ledger.rows().len(), 1);
            assert_eq!(
                ledger.rows()[0].disposition(),
                if downgrade {
                    Disposition::SetAside(Reason::VerificationChanged)
                } else {
                    Disposition::Resume
                }
            );
            ledger.record_outcome(&request.item, Stage::Verify, Outcome::Resumed);
            assert_eq!(
                ledger.rows()[0].outcome(),
                if downgrade {
                    Some(Outcome::SetAside(Reason::VerificationChanged))
                } else {
                    Some(Outcome::Resumed)
                }
            );
            for terminal in [Outcome::Ended, Outcome::Gone(Ending::Exited)] {
                let mut ledger = accepted.clone();
                ledger.record_terminal_outcome(&request.item, Stage::Release, terminal);
                let finished = ledger.clone();
                ledger.settle_unused_end(&request, Stage::Release);
                assert_eq!(ledger, finished);
            }
        }
    }

    #[test]
    fn unused_stop_consent_does_not_revoke_the_current_owner() {
        let daemon = impact::tests::waiting();
        let proof = &daemon.capability.proof;
        let mut ledger = ledger(&daemon);
        ledger.observe_impact(&daemon);
        let mut stop = StopItem {
            id: daemon.sessions[0].id,
            kind: impact::SessionKind::NativeChat,
            label: String::new(),
            identity: "previous owner".into(),
            interrupted_subagents: Vec::new(),
            running: String::new(),
            recovery: String::new(),
        };
        let previous = EndRequest::stop(proof, &stop);
        ledger.accept_end(previous.clone());
        stop.identity = "current owner".into();
        let current = EndRequest::stop(proof, &stop);
        ledger.accept_end(current.clone());
        ledger.release();
        ledger.settle_unused_stops(proof, &[stop], Stage::Release);
        assert!(!ledger.allows_end(&previous));
        assert!(ledger.allows_end(&current));
        assert_eq!(
            ledger.rows()[0].disposition(),
            Disposition::End(EndLoss::StopProcess)
        );

        let mut ended = ledger.clone();
        ended.record_ended(&current, Stage::Release);
        let finished = ended.clone();
        ended.settle_unused_stops(proof, &[], Stage::Release);
        assert_eq!(ended, finished);

        ledger.settle_unused_stops(proof, &[], Stage::Release);
        assert!(!ledger.allows_end(&current));
        ledger.record_outcome(&current.item, Stage::Verify, Outcome::Resumed);
        assert_eq!(ledger.rows()[0].outcome(), Some(Outcome::Resumed));
    }
}

#[cfg(test)]
mod tests {
    use super::impact::Policy;
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
            let disposition = match loss {
                EndLoss::InterruptTurn => Disposition::Resume,
                EndLoss::StopProcess => Disposition::End(loss),
            };
            assert_eq!(ledger.rows()[0].disposition(), disposition);
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
            match loss {
                EndLoss::InterruptTurn => {
                    assert!(!ledger.allows_end(&request));
                    assert_eq!(ledger.rows()[0].outcome(), Some(Outcome::Resumed));
                }
                EndLoss::StopProcess => {
                    ledger.mark_gone(request.item(), Stage::Verify, Ending::Exited);
                    assert_eq!(ledger, ended);
                    assert_eq!(ledger.rows()[0].outcome(), Some(Outcome::Ended));
                }
            }
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
