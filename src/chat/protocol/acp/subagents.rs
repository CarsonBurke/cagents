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
//! is provisionally listed under its own session ID. If matching another
//! child leaves one possible pair, the entries merge when the turn ends.
use super::tool_item;
use super::tools::{self, Calls};
use crate::chat::{Event, ToolStatus};
use crate::notifications::Kind;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// The child sessions whose steps are kept, as the parent keeps its calls.
pub(super) const MAX_CHILDREN: usize = 16;
/// The `invoke_subagent` calls remembered, so a revision of one is not
/// taken for another subagent.
pub(super) const MAX_INVOCATIONS: usize = 128;
/// A prompt's opening is enough to say what the subagent was asked to do.
const MAX_PROMPT: usize = 16 * 1024;
/// Shorter text, such as `ls`, would name a prompt by chance.
const MIN_EVIDENCE: usize = 8;

/// The title agy gives an `invoke_subagent` call, which a revision of the
/// call may carry without its name.
const INVOKE_TITLE: &str = "Run invoke_subagent";

/// A subagent as its `invoke_subagent` call lists it.
struct Listing {
    subagent: String,
    role: String,
    prompt: String,
    model: Option<String>,
}

impl Listing {
    fn event(&self, status: ToolStatus) -> Event {
        Event::Subagent {
            id: self.subagent.clone(),
            kind: self.role.clone(),
            description: self.prompt.clone(),
            status: Some(status),
            background: Some(false),
            model: self.model.clone(),
            waiting: Some(false),
            detail: None,
        }
    }
}

struct Child {
    /// The subagent it is listed under: the call that started it, or its
    /// own session ID.
    subagent: String,
    calls: Calls,
}

#[derive(Default)]
enum UnmatchedChildren {
    #[default]
    None,
    One {
        id: String,
        candidates: Vec<String>,
    },
    /// More than one child to place, or a call forgotten at the retention limit.
    Ambiguous,
}

impl UnmatchedChildren {
    fn could_belong_to(&self, invocation: &str) -> bool {
        match self {
            Self::None => false,
            Self::One { candidates, .. } => candidates.iter().any(|id| id == invocation),
            Self::Ambiguous => true,
        }
    }
}

#[derive(Default)]
pub(super) struct Subagents {
    /// Subagents started this turn that no child has been matched to, with
    /// their prompts, in the order they were started.
    unmatched: Vec<(String, String)>,
    unmatched_children: UnmatchedChildren,
    /// The subagents of this turn, which end with it.
    live: Vec<String>,
    /// This turn's calls still pending, such as one awaiting the reader's
    /// approval, whose subagents have not started.
    held: Vec<Listing>,
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
    /// Lists the subagent an `invoke_subagent` call starts, once the call is
    /// under way, and ends it if the call fails. A call still pending, as
    /// while it awaits approval, lists none yet. Replayed, it has ended.
    pub fn invoked(&mut self, update: &Value, live: bool) -> Option<Event> {
        let id = update["toolCallId"].as_str().filter(|id| !id.is_empty())?;
        let status = update["status"].as_str();
        let failed = status == Some("failed");
        let begun = matches!(status, Some("in_progress" | "completed"));
        if let Some((_, started)) = self.invocations.iter_mut().find(|(known, _)| known == id) {
            // A call that reported its subagent started cannot take that
            // back: a later failure, delivered out of order, says less.
            if *started {
                return None;
            }
            *started = status == Some("completed");
            return if failed {
                self.unstarted(id, ToolStatus::Failed)
            } else if begun {
                self.begin_held(id)
            } else {
                None
            };
        }
        if update["name"].as_str() != Some("invoke_subagent")
            && update["title"].as_str() != Some(INVOKE_TITLE)
        {
            return None;
        }
        if self.invocations.len() == MAX_INVOCATIONS
            && let Some((oldest, _)) = self.invocations.pop_front()
        {
            let oldest = listed(&oldest);
            self.roles.remove(&oldest);
            self.held.retain(|held| held.subagent != oldest);
        }
        self.invocations
            .push_back((id.to_owned(), status == Some("completed")));
        // The step shows `rawInput`, and so does the step that links to it.
        let input = &update["rawInput"];
        let text = |field: &str| input[field].as_str().map(str::trim).unwrap_or_default();
        let listing = Listing {
            subagent: listed(id),
            role: bounded(text("Role")),
            prompt: bounded(text("Prompt")),
            model: Some(text("Model"))
                .filter(|model| !model.is_empty() && *model != "inherit")
                .map(str::to_owned),
        };
        self.roles
            .insert(listing.subagent.clone(), listing.role.clone());
        if failed {
            Some(listing.event(ToolStatus::Failed))
        } else if !live {
            Some(listing.event(ToolStatus::Ended))
        } else if begun {
            Some(self.begin(listing))
        } else {
            // Still pending, the call may yet be refused: its subagent is
            // listed once it starts.
            if self.held.len() == MAX_INVOCATIONS {
                self.held.remove(0);
            }
            self.held.push(listing);
            None
        }
    }

    /// Lists a subagent of this turn as started, where children may be
    /// matched to it.
    fn begin(&mut self, listing: Listing) -> Event {
        if self.unmatched.len() == MAX_INVOCATIONS {
            self.unmatched.remove(0);
            // Retention dropping a call is no evidence of who started a child.
            self.unmatched_children = UnmatchedChildren::Ambiguous;
        }
        self.unmatched
            .push((listing.subagent.clone(), listing.prompt.clone()));
        self.keep_live(listing.subagent.clone());
        listing.event(ToolStatus::Running)
    }

    /// Starts the subagent of the pending call `id`, if it is held.
    fn begin_held(&mut self, id: &str) -> Option<Event> {
        let subagent = listed(id);
        let index = self
            .held
            .iter()
            .position(|held| held.subagent == subagent)?;
        let listing = self.held.remove(index);
        Some(self.begin(listing))
    }

    fn keep_live(&mut self, subagent: String) {
        if self.live.len() == MAX_INVOCATIONS {
            self.live.remove(0);
        }
        self.live.push(subagent);
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
            if new && live {
                // A new child shows that the calls still pending started.
                for listing in std::mem::take(&mut self.held) {
                    events.push(self.begin(listing));
                }
            }
            let subagent = known.unwrap_or_else(|| {
                if live {
                    self.matched(update).unwrap_or_else(|| child.clone())
                } else {
                    child.clone()
                }
            });
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
                    self.keep_live(child.clone());
                    self.unmatched_children = match self.unmatched_children {
                        UnmatchedChildren::None => UnmatchedChildren::One {
                            id: child.clone(),
                            candidates: self.unmatched.iter().map(|(id, _)| id.clone()).collect(),
                        },
                        _ => UnmatchedChildren::Ambiguous,
                    };
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

    /// The call that started this child, when the evidence names one.
    fn matched(&mut self, update: &Value) -> Option<String> {
        let index = match self.unmatched.as_slice() {
            [] => None,
            [(id, _)] if !self.unmatched_children.could_belong_to(id) => Some(0),
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
        index.map(|index| self.unmatched.remove(index).0)
    }

    /// A single child and call left at the end of this turn can be paired
    /// if the call was a candidate when the child first appeared. Waiting
    /// until settlement keeps more calls from invalidating that inference.
    fn reconcile(&mut self) -> Option<Event> {
        let UnmatchedChildren::One { id, candidates } =
            std::mem::take(&mut self.unmatched_children)
        else {
            return None;
        };
        let [(target, _)] = self.unmatched.as_slice() else {
            return None;
        };
        if !candidates.contains(target) {
            return None;
        }
        if let Some(child) = self.children.get_mut(&id) {
            child.subagent.clone_from(target);
        } else if let Some((_, subagent)) = self.evicted.iter_mut().find(|(child, _)| *child == id)
        {
            subagent.clone_from(target);
        } else {
            return None;
        }
        self.live.retain(|live| *live != id);
        Some(Event::SubagentMerged {
            from: id,
            into: target.clone(),
        })
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
        events.extend(self.reconcile());
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
        self.held.clear();
        events
    }

    /// Ends, as `status`, the subagent the `invoke_subagent` call `id` was
    /// to start, when the call failed or will not run: no child can be its.
    pub fn unstarted(&mut self, id: &str, status: ToolStatus) -> Option<Event> {
        let subagent = listed(id);
        if let Some(index) = self.held.iter().position(|held| held.subagent == subagent) {
            return Some(self.held.remove(index).event(status));
        }
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

    /// Whether `subagent` is one of this turn's, which can wait on the reader.
    pub fn is_live(&self, subagent: &str) -> bool {
        self.live.iter().any(|live| live == subagent)
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
