//! Antigravity's subagents, as its parent session reports them.
//!
//! agy 1.5.17 starts one with an `invoke_subagent` tool call, whose input
//! names its `Role` and gives its `Prompt`, and which reports `completed` as
//! soon as the subagent has started. The subagent's own steps then arrive in
//! the parent session under the tool call ID `<child session ID>:<call ID>`,
//! but only those that need approval: its reads, its messages and its report
//! to the parent are not streamed. The turn ends once its subagents have
//! reported, and a stopped turn fails their commands.
//!
//! Nothing says which call started which child session, and subagents
//! started together first ask in the order they get there, not the order
//! they were started. A child is therefore matched to a call only on
//! evidence: the one call of the turn not yet matched, or the one whose
//! prompt names what the child asks to do. A child that cannot be matched
//! is listed under its own session ID, rather than under another subagent's
//! role or among the parent's own steps.
use super::tool_item;
use super::tools::{self, Calls};
use crate::chat::{Event, ToolStatus};
use crate::notifications::Kind;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// The child sessions whose steps are kept, as the parent keeps its calls.
const MAX_CHILDREN: usize = 16;
/// The `invoke_subagent` calls remembered, so a revision of one is not
/// taken for another subagent.
const MAX_INVOCATIONS: usize = 128;
/// A prompt's opening is enough to say what the subagent was asked to do.
const MAX_PROMPT: usize = 16 * 1024;
/// Shorter text, such as `ls`, would name a prompt by chance.
const MIN_EVIDENCE: usize = 8;

/// The title agy gives an `invoke_subagent` call, which a revision of the
/// call may carry without its name.
const INVOKE_TITLE: &str = "Run invoke_subagent";

struct Child {
    /// The subagent it is listed under: the call that started it, or its
    /// own session ID.
    subagent: String,
    calls: Calls,
}

#[derive(Default)]
pub(super) struct Subagents {
    /// Subagents started this turn that no child has been matched to, with
    /// their prompts, in the order they were started.
    unmatched: Vec<(String, String)>,
    /// The subagents of this turn, which end with it.
    live: Vec<String>,
    /// Each with whether it has reported its subagent started.
    invocations: VecDeque<(String, bool)>,
    /// The role each remembered subagent was started under.
    roles: HashMap<String, String>,
    children: HashMap<String, Child>,
    order: VecDeque<String>,
    /// The subagents of children no longer kept, so that one heard from
    /// again stays with the subagent it was matched to.
    evicted: VecDeque<(String, String)>,
}

/// The child session a tool call ID names, where it is a subagent's.
pub(super) fn child_of(id: &str) -> Option<&str> {
    let (child, call) = id.split_once(':')?;
    (!call.is_empty() && uuid::Uuid::try_parse(child).is_ok()).then_some(child)
}

impl Subagents {
    /// Lists the subagent an `invoke_subagent` call starts, the first time
    /// the call is seen, and ends it if the call fails. Replayed, it has
    /// ended.
    pub fn invoked(&mut self, update: &Value, live: bool) -> Option<Event> {
        let id = update["toolCallId"].as_str().filter(|id| !id.is_empty())?;
        let status = update["status"].as_str();
        let failed = status == Some("failed");
        if let Some((_, started)) = self.invocations.iter_mut().find(|(known, _)| known == id) {
            // A call that reported its subagent started cannot take that
            // back: a later failure, delivered out of order, says less.
            if *started || !failed {
                *started |= status == Some("completed");
                return None;
            }
            return self.unstarted(id, ToolStatus::Failed);
        }
        if update["name"].as_str() != Some("invoke_subagent")
            && update["title"].as_str() != Some(INVOKE_TITLE)
        {
            return None;
        }
        if self.invocations.len() == MAX_INVOCATIONS
            && let Some((oldest, _)) = self.invocations.pop_front()
        {
            self.roles.remove(&listed(&oldest));
        }
        self.invocations
            .push_back((id.to_owned(), status == Some("completed")));
        // The step shows `rawInput`, and so does the step that links to it.
        let input = &update["rawInput"];
        let text = |field: &str| input[field].as_str().map(str::trim).unwrap_or_default();
        let prompt = bounded(text("Prompt"));
        let role = bounded(text("Role"));
        let subagent = listed(id);
        self.roles.insert(subagent.clone(), role.clone());
        // Only this turn's children can be matched to it.
        let started = live && !failed;
        if started {
            if self.unmatched.len() == MAX_INVOCATIONS {
                self.unmatched.remove(0);
            }
            self.unmatched.push((subagent.clone(), prompt.clone()));
            self.live.push(subagent.clone());
        }
        Some(Event::Subagent {
            id: subagent,
            kind: role,
            description: prompt,
            status: Some(if failed {
                ToolStatus::Failed
            } else if live {
                ToolStatus::Running
            } else {
                ToolStatus::Ended
            }),
            background: Some(false),
            model: Some(text("Model"))
                .filter(|model| !model.is_empty() && *model != "inherit")
                .map(str::to_owned),
            waiting: Some(false),
            detail: None,
        })
    }

    /// Shows a subagent's step in its own transcript. `None` when `update`
    /// is one of the parent's own.
    pub fn step(&mut self, update: &Value, live: bool) -> Option<Vec<Event>> {
        let id = update["toolCallId"].as_str()?;
        let child = child_of(id)?.to_owned();
        let mut events = Vec::new();
        if !self.children.contains_key(&child) {
            let known = self
                .evicted
                .iter()
                .position(|(evicted, _)| *evicted == child)
                .and_then(|index| self.evicted.remove(index))
                .map(|(_, subagent)| subagent);
            let new = known.is_none();
            let subagent = known.unwrap_or_else(|| self.matched(&child, update));
            if new && subagent == child {
                events.push(Event::Subagent {
                    id: child.clone(),
                    kind: String::new(),
                    description: String::new(),
                    status: Some(if live {
                        ToolStatus::Running
                    } else {
                        ToolStatus::Ended
                    }),
                    background: Some(false),
                    model: None,
                    waiting: Some(false),
                    detail: None,
                });
                if live {
                    self.live.push(child.clone());
                }
            }
            if self.order.len() == MAX_CHILDREN
                && let Some(oldest) = self.order.pop_front()
                && let Some(gone) = self.children.remove(&oldest)
            {
                if self.evicted.len() == MAX_INVOCATIONS {
                    self.evicted.pop_front();
                }
                self.evicted.push_back((oldest, gone.subagent));
            }
            self.order.push_back(child.clone());
            self.children.insert(
                child.clone(),
                Child {
                    subagent,
                    calls: Calls::default(),
                },
            );
        }
        let known = self.children.get_mut(&child)?;
        if let Some((item, content)) = known.calls.merge(update) {
            events.push(Event::SubagentItem {
                id: known.subagent.clone(),
                item: tool_item(item, content),
            });
        }
        Some(events)
    }

    /// The call that started `child`, where the evidence names one, or else
    /// `child` itself.
    fn matched(&mut self, child: &str, update: &Value) -> String {
        let index = match self.unmatched.len() {
            0 => None,
            1 => Some(0),
            _ => {
                let evidence: Vec<_> = tools::evidence(update)
                    .into_iter()
                    .filter(|text| text.len() >= MIN_EVIDENCE)
                    .collect();
                let mut named = self
                    .unmatched
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, prompt))| {
                        evidence.iter().any(|text| prompt.contains(text.as_str()))
                    });
                match (named.next(), named.next()) {
                    (Some((index, _)), None) => Some(index),
                    _ => None,
                }
            }
        };
        index.map_or_else(|| child.to_owned(), |index| self.unmatched.remove(index).0)
    }

    /// The subagent that made the call `id`, if a subagent made it.
    pub fn owner(&self, id: &str) -> Option<&str> {
        let child = self.children.get(child_of(id)?)?;
        Some(&child.subagent)
    }

    /// The subagent's calls that `id` is one of.
    pub fn calls(&self, id: &str) -> Option<&Calls> {
        Some(&self.children.get(child_of(id)?)?.calls)
    }

    /// Notes that the reader refused the subagent's call `id`; `false` when
    /// it is not a subagent's.
    pub fn declined(&mut self, id: &str) -> bool {
        let Some(child) = child_of(id).and_then(|child| self.children.get_mut(child)) else {
            return false;
        };
        child.calls.declined(id);
        true
    }

    /// What the subagent listed as `subagent` is called: the role it was
    /// started under, if it is known.
    pub fn name(&self, subagent: &str) -> &str {
        self.roles
            .get(subagent)
            .map(String::as_str)
            .filter(|role| !role.is_empty())
            .unwrap_or("Subagent")
    }

    /// Ends the turn's subagents with it: finished when it was, stopped
    /// when it was stopped, and otherwise ended, as the turn's own notice
    /// says why. Whatever they still ran stopped with the turn.
    pub fn settle(&mut self, outcome: Kind) -> Vec<Event> {
        let mut events = Vec::new();
        for child in self.children.values_mut() {
            for (item, content) in child.calls.stop_running() {
                events.push(Event::SubagentItem {
                    id: child.subagent.clone(),
                    item: tool_item(item, content),
                });
            }
        }
        let status = match outcome {
            Kind::Succeeded => ToolStatus::Succeeded,
            Kind::Cancelled => ToolStatus::Stopped,
            _ => ToolStatus::Ended,
        };
        for id in std::mem::take(&mut self.live) {
            events.push(Event::Subagent {
                id,
                kind: String::new(),
                description: String::new(),
                status: Some(status),
                background: None,
                model: None,
                waiting: Some(false),
                detail: None,
            });
        }
        self.unmatched.clear();
        events
    }

    /// Ends, as `status`, the subagent the `invoke_subagent` call `id` was
    /// to start, when the call failed or will not run: no child can be its.
    pub fn unstarted(&mut self, id: &str, status: ToolStatus) -> Option<Event> {
        let subagent = listed(id);
        let index = self.live.iter().position(|live| *live == subagent)?;
        self.live.remove(index);
        self.unmatched
            .retain(|(unmatched, _)| *unmatched != subagent);
        Some(Event::Subagent {
            id: subagent,
            kind: String::new(),
            description: String::new(),
            status: Some(status),
            background: None,
            model: None,
            waiting: Some(false),
            detail: None,
        })
    }

    /// Whether a subagent of this turn is still at work.
    pub fn working(&self) -> bool {
        !self.live.is_empty()
    }
}

/// The ID the subagent the call `id` started is listed under: the ID of the
/// call's step, so that the step links to it.
fn listed(id: &str) -> String {
    format!("tool:{id}")
}

/// Whether the subagent `id` waits for the reader's answer.
pub(super) fn waiting(id: String, waiting: bool) -> Event {
    Event::Subagent {
        id,
        kind: String::new(),
        description: String::new(),
        status: None,
        background: None,
        model: None,
        waiting: Some(waiting),
        detail: None,
    }
}

fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_PROMPT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
