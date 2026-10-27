//! Publish separate provider/run/message facts; historical failure never
//! overwrites a genuinely active or unattributed provider conversation.
use crate::agent_execution::{
    Run,
    directory::{ExecutionStatus, RunStatus, Status},
};

pub(super) fn run(run: &Run, may_read_private: bool) -> RunStatus {
    let mut diagnostic = may_read_private
        .then(|| run.result.diagnostic.clone())
        .flatten();
    if let Some(diagnostic) = &mut diagnostic {
        crate::agent_execution::truncate(diagnostic, crate::agent_execution::MAX_DIAGNOSTIC_BYTES);
    }
    RunStatus {
        id: run.id,
        state: run.state,
        request_recorded: run.started_ms.is_some(),
        diagnostic,
    }
}

pub(super) fn project(
    observed: Option<ExecutionStatus>,
    run: Option<RunStatus>,
    may_read_private: bool,
    pending_message: bool,
) -> Status {
    Status {
        observed,
        latest_run: run,
        pending_message: may_read_private.then_some(pending_message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::ActivityState,
        agent_execution::{ResultSummary, State},
        model::{AgentId, RunId},
    };

    fn failed() -> Run {
        Run {
            id: RunId::new(),
            agent: AgentId::new(),
            state: State::Failed,
            revision: 2,
            accepted_ms: 1,
            started_ms: None,
            finished_ms: Some(2),
            provider_key: None,
            result: ResultSummary {
                diagnostic: Some("private capacity diagnostic".into()),
                text: "private response body".into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn historical_failure_does_not_replace_current_request_or_background_activity() {
        let old = failed();
        let current = RunId::new();
        let sample = ExecutionStatus {
            activity: ActivityState::Working,
            generation: 10,
            turn_generation: 4,
            parent_busy: true,
            active_run: Some(current),
        };
        let status = project(Some(sample.clone()), Some(run(&old, true)), true, true);
        assert_eq!(status.observed, Some(sample));
        assert_eq!(status.latest_run.as_ref().unwrap().id, old.id);
        assert!(!status.latest_run.as_ref().unwrap().request_recorded);
        let mut background = status.observed.unwrap();
        background.active_run = None;
        background.parent_busy = false;
        let status = project(Some(background), Some(run(&old, true)), true, false);
        assert!(status.explanation()[0].contains("background work"));
        assert!(
            !status
                .explanation()
                .iter()
                .any(|line| line.contains("consumed") || line.contains("provider dead"))
        );
    }

    #[test]
    fn public_or_invalid_caller_projection_carries_no_diagnostic_body_or_queue() {
        let private = failed();
        let projected = project(None, Some(run(&private, false)), false, true);
        let encoded = serde_json::to_string(&projected).unwrap();
        assert!(!encoded.contains("private capacity") && !encoded.contains("private response"));
        assert!(projected.latest_run.unwrap().diagnostic.is_none());
        assert_eq!(projected.pending_message, None);
        let own = project(None, Some(run(&private, true)), true, false);
        assert!(
            own.observed.is_none(),
            "missing/old helper is unknown, never fabricated idle"
        );
        assert_eq!(own.pending_message, Some(false));
        assert!(
            !own.explanation()
                .iter()
                .any(|line| line.contains("consumed") || line.contains("execution confirmed"))
        );
    }
}
