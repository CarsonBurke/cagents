//! What keeps a conversation from settling, told in the terms its reader
//! knows it by: the turn they can see, and the subagents by their tasks.
//!
//! A subagent waiting on background work of its own leaves its conversation
//! looking finished, and that work, such as a dev server, may never end. Every
//! surface that waits for or refuses such a conversation names what holds it
//! and how to let it go, rather than leaving the hold unexplained.
use super::{Snapshot, Subagent, ToolStatus};
use serde::{Deserialize, Serialize};

/// Subagents named in one list before the rest are only counted.
const NAMED: usize = 3;
/// Characters of a subagent's task kept in its name.
const NAME_CHARS: usize = 60;

/// What a conversation is doing that closing it now would end or abandon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkHold {
    /// Its own turn is under way or awaiting the person.
    pub turn: bool,
    /// Its subagents that outlive a turn and have yet to end, in the
    /// provider's order.
    pub subagents: Vec<HeldSubagent>,
}

/// A subagent as a person knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldSubagent {
    /// What its tab calls it: its name, which a provider fills with its task
    /// when it has none, else the first line of its task. Empty when whoever
    /// reported it knew neither, as a retained helper from before names were
    /// reported.
    pub name: String,
    /// It finished its own turn and waits on background work it started.
    pub waiting: bool,
}

impl HeldSubagent {
    pub fn of(agent: &Subagent) -> Self {
        let name = [agent.kind.as_str(), agent.description.as_str()]
            .into_iter()
            .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
            .unwrap_or("Subagent");
        let mut chars = name.chars();
        let mut name: String = chars.by_ref().take(NAME_CHARS).collect();
        if chars.next().is_some() {
            name.push('…');
        }
        Self {
            name,
            waiting: agent.status == ToolStatus::Waiting,
        }
    }
}

impl Snapshot {
    /// See [`WorkHold`]. A live subagent that is neither in the background nor
    /// waiting belongs to the turn and ends with it, so the turn stands for it.
    pub fn work_hold(&self) -> WorkHold {
        WorkHold {
            turn: self.turn_accepted(),
            subagents: self
                .subagents
                .iter()
                .filter(|agent| {
                    agent.status == ToolStatus::Waiting
                        || agent.background && agent.status.is_live()
                })
                .map(HeldSubagent::of)
                .collect(),
        }
    }
}

impl WorkHold {
    /// Every subagent that holds the conversation.
    pub fn subagents(&self) -> Held<'_> {
        self.subagents.iter().collect()
    }

    /// The subagents a person needs told about: all of them while no turn
    /// holds the conversation. Alongside a turn, a running subagent is
    /// ordinary and ends in time, so only those waiting, which may not.
    pub fn beyond_turn(&self) -> Held<'_> {
        self.subagents
            .iter()
            .filter(|agent| !self.turn || agent.waiting)
            .collect()
    }
}

/// Some of a conversation's subagents, in the provider's order, which a
/// message names together.
pub struct Held<'a>(Vec<&'a HeldSubagent>);

impl<'a> FromIterator<&'a HeldSubagent> for Held<'a> {
    fn from_iter<I: IntoIterator<Item = &'a HeldSubagent>>(agents: I) -> Self {
        Self(agents.into_iter().collect())
    }
}

impl Held<'_> {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn split(&self) -> (Vec<&HeldSubagent>, Vec<&HeldSubagent>) {
        self.0.iter().partition(|agent| !agent.waiting)
    }

    /// How many, by state: "1 running background subagent", "2 waiting
    /// background subagents", "1 running and 1 waiting background subagent".
    pub fn count(&self) -> String {
        let (running, waiting) = self.split();
        let noun = if running.len() > 1 || waiting.len() > 1 {
            "background subagents"
        } else {
            "background subagent"
        };
        match (running.len(), waiting.len()) {
            (running, 0) => format!("{running} running {noun}"),
            (0, waiting) => format!("{waiting} waiting {noun}"),
            (running, waiting) => format!("{running} running and {waiting} waiting {noun}"),
        }
    }

    /// The count, followed by the names when any are known.
    pub fn counted(&self) -> String {
        match names(&self.0) {
            Some(names) => format!("{} ({names})", self.count()),
            None => self.count(),
        }
    }

    /// What each is doing, saying of those waiting that their work may never
    /// end: "Background subagent “Run the dev server” is waiting on its own
    /// background work, which may not finish on its own."
    pub fn detail(&self) -> String {
        let (running, waiting) = self.split();
        let subject = |agents: &[&HeldSubagent], lead: bool| {
            let plural = agents.len() > 1;
            match names(agents) {
                Some(names) if lead && plural => format!("Background subagents {names}"),
                Some(names) if lead => format!("Background subagent {names}"),
                Some(names) => names,
                None => format!(
                    "{} background subagent{}",
                    agents.len(),
                    if plural { "s" } else { "" }
                ),
            }
        };
        let doing = |agents: &[&HeldSubagent]| {
            if agents[0].waiting {
                if agents.len() > 1 {
                    "are waiting on their own background work, which may not finish on its own"
                } else {
                    "is waiting on its own background work, which may not finish on its own"
                }
            } else if agents.len() > 1 {
                "are still running"
            } else {
                "is still running"
            }
        };
        match (running.is_empty(), waiting.is_empty()) {
            (true, true) => String::new(),
            (false, false) => format!(
                "{} {}, and {} {}.",
                subject(&running, true),
                doing(&running),
                subject(&waiting, false),
                doing(&waiting)
            ),
            _ => {
                let agents = if running.is_empty() {
                    &waiting
                } else {
                    &running
                };
                format!("{} {}.", subject(agents, true), doing(agents))
            }
        }
    }

    /// What to stop, and where: "it from its tab" or "them from their tabs".
    pub fn where_to_stop(&self) -> &'static str {
        if self.0.len() > 1 {
            "them from their tabs"
        } else {
            "it from its tab"
        }
    }
}

/// A bounded list of the known names: “A”, “B”, “C” and 2 more.
fn names(agents: &[&HeldSubagent]) -> Option<String> {
    let named: Vec<_> = agents
        .iter()
        .filter(|agent| !agent.name.is_empty())
        .take(NAMED)
        .map(|agent| format!("\u{201c}{}\u{201d}", agent.name))
        .collect();
    if named.is_empty() {
        return None;
    }
    let rest = agents.len() - named.len();
    let mut parts = named;
    if rest > 0 {
        parts.push(format!("{rest} more"));
    }
    let last = parts.pop()?;
    Some(if parts.is_empty() {
        last
    } else {
        format!("{} and {last}", parts.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, waiting: bool) -> HeldSubagent {
        HeldSubagent {
            name: name.into(),
            waiting,
        }
    }

    fn held(agents: &[HeldSubagent]) -> Held<'_> {
        agents.iter().collect()
    }

    #[test]
    fn a_waiting_subagent_is_named_by_its_task_and_never_called_running() {
        let agents = [agent("Run the dev server", true)];
        let held = held(&agents);
        assert_eq!(held.count(), "1 waiting background subagent");
        assert_eq!(
            held.counted(),
            "1 waiting background subagent (\u{201c}Run the dev server\u{201d})"
        );
        assert_eq!(
            held.detail(),
            "Background subagent \u{201c}Run the dev server\u{201d} is waiting on its own background work, which may not finish on its own."
        );
        assert_eq!(held.where_to_stop(), "it from its tab");
    }

    #[test]
    fn mixed_states_are_counted_and_described_apart() {
        let agents = [
            agent("Audit the parser", false),
            agent("Watch the tests", true),
            agent("Serve the docs", true),
        ];
        let held = held(&agents);
        assert_eq!(held.count(), "1 running and 2 waiting background subagents");
        assert_eq!(
            held.detail(),
            "Background subagent \u{201c}Audit the parser\u{201d} is still running, and \u{201c}Watch the tests\u{201d} and \u{201c}Serve the docs\u{201d} are waiting on their own background work, which may not finish on its own."
        );
        assert_eq!(held.where_to_stop(), "them from their tabs");
        let agents = [agent("A", false), agent("B", true)];
        assert_eq!(
            self::held(&agents).count(),
            "1 running and 1 waiting background subagent"
        );
    }

    #[test]
    fn long_lists_are_bounded_and_unnamed_agents_are_counted() {
        let agents: Vec<_> = (0..5).map(|n| agent(&format!("Task {n}"), false)).collect();
        assert_eq!(
            held(&agents).detail(),
            "Background subagents \u{201c}Task 0\u{201d}, \u{201c}Task 1\u{201d}, \u{201c}Task 2\u{201d} and 2 more are still running."
        );
        let unnamed = [agent("", false), agent("", false)];
        let held = held(&unnamed);
        assert_eq!(held.counted(), "2 running background subagents");
        assert_eq!(held.detail(), "2 background subagents are still running.");
    }

    #[test]
    fn the_hold_names_a_task_and_leaves_a_turns_running_work_to_the_turn() {
        let mut snapshot = Snapshot::default();
        let subagent = |id: &str, kind: &str, description: &str, status, background| Subagent {
            id: id.into(),
            kind: kind.into(),
            description: description.into(),
            status,
            model: None,
            waiting: false,
            detail: String::new(),
            background,
            items: Vec::new(),
        };
        snapshot.subagents = vec![
            subagent(
                "toolu_1",
                "",
                "\n Start the dev server\nThen wait",
                ToolStatus::Waiting,
                true,
            ),
            subagent(
                "toolu_2",
                " Scout ",
                "Map the parser",
                ToolStatus::Running,
                true,
            ),
            subagent(
                "toolu_3",
                "Explore",
                "Read the parser",
                ToolStatus::Running,
                false,
            ),
            subagent("toolu_4", "Explore", "Done", ToolStatus::Succeeded, true),
        ];
        let hold = snapshot.work_hold();
        assert!(!hold.turn);
        assert_eq!(
            hold.subagents,
            [agent("Start the dev server", true), agent("Scout", false)]
        );
        assert_eq!(
            hold.beyond_turn().count(),
            "1 running and 1 waiting background subagent"
        );
        snapshot.busy = true;
        let hold = snapshot.work_hold();
        assert!(hold.turn);
        assert_eq!(hold.beyond_turn().count(), "1 waiting background subagent");
    }
}
