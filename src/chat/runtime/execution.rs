//! Bounded prompt results, frozen before a deferred provider decision arrives.

use std::collections::BTreeMap;

use crate::{
    agent_execution::{MAX_RESULT_BYTES, ResultSummary, State, Update},
    chat::{Event, ItemContent, Role, Usage},
    model::RunId,
    notifications::Kind,
};

#[derive(Default)]
pub(super) struct Tracker {
    active: Option<RunId>,
    runs: BTreeMap<RunId, Capture>,
}

#[derive(Default)]
struct Capture {
    order: Vec<String>,
    text: BTreeMap<String, String>,
    bytes: usize,
    truncated: bool,
    diagnostic: Option<String>,
    result: Option<ResultSummary>,
    waiting: bool,
}

impl Tracker {
    pub fn has_pending(&self) -> bool { !self.runs.is_empty() }
    pub fn active(&self) -> Option<RunId> {
        self.active
    }
    pub fn activity(&mut self, state: crate::agent::ActivityState) -> Option<(RunId, Update)> {
        let run = self.active?;
        let capture = self.runs.get_mut(&run)?;
        let waiting = state == crate::agent::ActivityState::Waiting;
        if capture.waiting == waiting {
            return None;
        }
        capture.waiting = waiting;
        Some((
            run,
            if waiting {
                Update::Waiting
            } else {
                Update::Resumed
            },
        ))
    }

    pub fn ended(
        &mut self,
        cancelled: bool,
        status: Option<std::process::ExitStatus>,
        diagnostic: Option<&str>,
        usage: Usage,
    ) -> Vec<(RunId, Update)> {
        let state = if cancelled { State::Cancelled }
            else if status.is_some_and(|status| status.success()) { State::Interrupted }
            else if status.is_some() || diagnostic.is_some() { State::Failed }
            else { State::Interrupted };
        let reason = if cancelled {
            "The agent was stopped before its provider reported a final result".to_owned()
        } else {
            exit_reason(status, diagnostic)
        };
        let runs = self.runs.keys().copied().collect::<Vec<_>>();
        let mut updates = Vec::new();
        for run in runs {
            if self.settle(run, usage.clone()).is_err() {
                continue;
            }
            if let Some(capture) = self.runs.get_mut(&run)
                && let Some(result) = &mut capture.result
            {
                result.diagnostic = Some(reason.clone());
            }
            if let Some(result) = self.runs.remove(&run).and_then(|capture| capture.result) {
                updates.push((run, Update::Finished { state, result }));
            }
        }
        updates
    }

    pub fn start(&mut self, run: RunId, key: String) -> Result<Update, String> {
        if self.active.is_some() || self.runs.contains_key(&run) || self.runs.len() >= 8 {
            return Err("Agent execution result capacity or active-turn ownership exceeded".into());
        }
        self.active = Some(run);
        self.runs.insert(run, Capture::default());
        Ok(Update::Started { provider_key: key })
    }

    pub fn observe(&mut self, run: RunId, event: &Event) {
        let Some(capture) = self
            .runs
            .get_mut(&run)
            .filter(|capture| capture.result.is_none())
        else {
            return;
        };
        match event {
            Event::Item(item) if item.role == Role::Assistant => {
                if let ItemContent::Text {
                    text,
                    thinking: false,
                    ..
                } = &item.content
                {
                    capture.put(&item.id, text, false);
                }
            }
            Event::TextDelta {
                id,
                role: Role::Assistant,
                thinking: false,
                delta,
            } => capture.put(id, delta, true),
            Event::ItemsRemoved { ids } => {
                for id in ids {
                    if let Some(text) = capture.text.remove(id) {
                        capture.bytes -= text.len();
                    }
                }
                capture.order.retain(|id| !ids.contains(id));
            }
            Event::Error { message, .. } => {
                capture.diagnostic =
                    Some(bounded(message, crate::agent_execution::MAX_DIAGNOSTIC_BYTES).to_owned());
            }
            _ => {}
        }
    }

    pub fn settle(&mut self, run: RunId, usage: Usage) -> Result<(), String> {
        let capture = self
            .runs
            .get_mut(&run)
            .ok_or("Unknown agent execution settlement")?;
        if capture.result.is_none() {
            let text = capture
                .order
                .iter()
                .filter_map(|id| capture.text.get(id))
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut result = ResultSummary {
                text,
                truncated: capture.truncated,
                usage,
                diagnostic: capture.diagnostic.take(),
            };
            result.bound();
            capture.result = Some(result);
            capture.text.clear();
            capture.order.clear();
            capture.bytes = 0;
        }
        if self.active == Some(run) {
            self.active = None;
        }
        Ok(())
    }

    pub fn finish(&mut self, run: RunId, outcome: Kind) -> Result<Update, String> {
        let state = match outcome {
            Kind::Succeeded => State::Succeeded,
            Kind::Failed => State::Failed,
            Kind::Cancelled => State::Cancelled,
            Kind::Waiting | Kind::Interrupted | Kind::ReviewRequested | Kind::MessageReceived => {
                return Err("Attention is not a confirmed provider outcome".into());
            }
        };
        let mut capture = self
            .runs
            .remove(&run)
            .ok_or("Unknown agent execution outcome")?;
        let mut result = capture
            .result
            .take()
            .ok_or("Agent outcome arrived before settlement")?;
        if state == State::Succeeded {
            result.diagnostic = None;
        }
        Ok(Update::Finished { state, result })
    }
}

impl Capture {
    fn put(&mut self, id: &str, text: &str, append: bool) {
        if !self.text.contains_key(id) {
            if self.text.len() >= 1024 || id.len() > 1024 {
                self.truncated = true;
                return;
            }
            self.order.push(id.to_owned());
        }
        let entry = self.text.entry(id.to_owned()).or_default();
        if !append {
            self.bytes -= entry.len();
            entry.clear();
        }
        let remaining = MAX_RESULT_BYTES.saturating_sub(self.bytes);
        let part = bounded(text, remaining);
        self.truncated |= part.len() != text.len();
        entry.push_str(part);
        self.bytes += part.len();
    }
}

pub(super) fn exit_reason(
    status: Option<std::process::ExitStatus>,
    diagnostic: Option<&str>,
) -> String {
    use std::os::unix::process::ExitStatusExt;
    let status = match status {
        Some(status) if status.signal().is_some() => {
            format!("Provider exited with signal {}", status.signal().unwrap())
        }
        Some(status) if !status.success() => format!(
            "Provider exited with status {}",
            status.code().unwrap_or_default()
        ),
        Some(_) => "Provider exited with status 0 before reporting this prompt's final result".into(),
        None => "The provider exited before reporting this prompt's final result".into(),
    };
    let reason = match diagnostic.filter(|message| !message.is_empty()) {
        Some(message) if message.starts_with(&status) => message.to_owned(),
        Some(message) => format!("{status}: {message}"),
        None => status,
    };
    bounded(&reason, crate::agent_execution::MAX_DIAGNOSTIC_BYTES).to_owned()
}

fn bounded(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(id: &str, delta: &str) -> Event {
        Event::TextDelta {
            id: id.into(),
            role: Role::Assistant,
            thinking: false,
            delta: delta.into(),
        }
    }

    #[test]
    fn provider_exit_outcomes_preserve_stop_status_and_signal() {
        use std::os::unix::process::ExitStatusExt;
        for (stop, status, expected, reason) in [
            (true, 3 << 8, State::Cancelled, "stopped"),
            (false, 0, State::Interrupted, "before reporting"),
            (false, 3 << 8, State::Failed, "status 3"),
            (false, 9, State::Failed, "signal 9"),
        ] {
            let mut tracker = Tracker::default();
            let run = RunId::new();
            tracker.start(run, "request".into()).unwrap();
            // Many tool calls must never imply provider-reported success.
            for index in 0..300 {
                tracker.observe(run, &Event::Item(crate::chat::Item {
                    id:format!("tool-{index}"),role:Role::Assistant,started_at:None,at:None,
                    content:ItemContent::Tool {name:"Bash".into(),input:"true".into(),output:"done".into(),status:crate::chat::ToolStatus::Succeeded},
                }));
            }
            let updates = tracker.ended(
                stop,
                Some(std::process::ExitStatus::from_raw(status)),
                None,
                Usage::default(),
            );
            let Update::Finished { state, result } = &updates[0].1 else {
                panic!("missing outcome")
            };
            assert_eq!(*state, expected);
            assert!(result.diagnostic.as_ref().unwrap().contains(reason));
        }
    }

    #[test]
    fn deferred_outcome_retains_the_old_result_and_usage_after_a_new_prompt() {
        let mut tracker = Tracker::default();
        let first = RunId::new();
        tracker
            .start(first, "first-provider-request".into())
            .unwrap();
        tracker.observe(first, &text("message", "First answer"));
        tracker
            .settle(
                first,
                Usage {
                    output_tokens: 12,
                    ..Default::default()
                },
            )
            .unwrap();
        let next = RunId::new();
        tracker.start(next, "next-provider-request".into()).unwrap();
        tracker.observe(next, &text("message", "Next answer"));
        let Update::Finished { state, result } = tracker.finish(first, Kind::Cancelled).unwrap()
        else {
            panic!("expected result")
        };
        assert_eq!(state, State::Cancelled);
        assert_eq!(result.text, "First answer");
        assert_eq!(result.usage.output_tokens, 12);
        tracker
            .settle(
                next,
                Usage {
                    output_tokens: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        let Update::Finished { result, .. } = tracker.finish(next, Kind::Succeeded).unwrap() else {
            panic!("expected result")
        };
        assert_eq!(result.text, "Next answer");
        assert_eq!(result.usage.output_tokens, 7);
    }

    #[test]
    fn inbox_attention_cannot_consume_a_settled_provider_result() {
        let mut tracker = Tracker::default();
        let run = RunId::new();
        tracker.start(run, "provider-request".into()).unwrap();
        tracker.observe(run, &text("message", "Completed answer"));
        tracker.settle(run, Usage::default()).unwrap();

        assert!(tracker.finish(run, Kind::MessageReceived).is_err());
        let Update::Finished { state, result } = tracker.finish(run, Kind::Succeeded).unwrap()
        else {
            panic!("expected result")
        };
        assert_eq!(state, State::Succeeded);
        assert_eq!(result.text, "Completed answer");
    }
}
