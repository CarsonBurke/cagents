//! Grouping a flat transcript into the stretches of work a reader perceives.
//!
//! A provider hands us an ordered list of items and no notion of a turn. The
//! consequence on screen was that one assistant response arrived as three
//! sibling rows, each captioned with the provider's name, with nothing to say
//! they belonged together or which of them was the answer. Grouping is purely
//! a presentation concern - it needs no provider support, only the order the
//! items already arrive in - so it lives here rather than on the wire.
//!
//! A group is one stretch of work, not a whole exchange. Grouping only on the
//! reader's messages meant an hour-long session with a single prompt collapsed
//! into one strip captioned "217 steps": everything the agent said in between
//! was inside a group whose header the reader had to open to find out what had
//! happened since. A group therefore ends wherever the agent hands control
//! back - it says something to the reader, or asks them something - and the
//! work that follows belongs to the next group.

use super::coordination::Coordinated;
use crate::chat::{self, Item, ItemContent, Phase, Role, ToolStatus};
use std::{borrow::Borrow, collections::HashSet};

/// Where an item belongs inside its group.
///
/// The four roles read very differently and must not share a treatment:
/// `Step` is what the agent did, `Thinking` is it reasoning about what to do,
/// `Commentary` is it narrating while it worked, and `Prose` is what the user
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Slot {
    /// A command, tool call, file change, or search.
    Step,
    /// Reasoning streams inline and collapses behind a disclosure when done.
    Thinking,
    /// Progress narration, and the session notices the harness sends
    /// alongside it. Measured over this workstation's Codex history narration
    /// outnumbers answers roughly seven to one, so it has to recede.
    Commentary,
    /// The answer, or a user message.
    Prose,
    /// A message to another agent, or agents started on a prompt: a step,
    /// drawn as a card among the prose for the words it carries.
    Card,
}

/// One transcript item placed within a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placed {
    /// Index into the snapshot's item list. An index rather than an owned id
    /// because the previous view cloned every item's id three to seven times
    /// per frame purely to address it.
    pub(super) index: usize,
    pub(super) slot: Slot,
}

/// One stretch of work: what prompted it, what the agent did, and what it said
/// about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Turn {
    /// The user message that opened the group, in the order its parts
    /// arrived. Empty for work the agent started on its own - a resumed
    /// session's history, a provider greeting, or anything it did after it
    /// last spoke. More than one entry because a single message is not a
    /// single item: a prompt with two attachments reaches the transcript as
    /// three `Role::User` items.
    pub(super) prompt: Vec<usize>,
    pub(super) steps: Vec<Placed>,
    pub(super) prose: Vec<Placed>,
    /// Set while any step in this group is still running, which is what marks
    /// the live group without needing a separate global flag.
    pub(super) running: bool,
    /// How the group's last resolved step went, so a collapsed strip reports
    /// the state the work reached.
    pub(super) outcome: Option<ToolStatus>,
    /// Earliest first observation in the group, when this stretch of work began.
    started_at: Option<u64>,
    /// Latest observation in the group. An earlier item can finish after later
    /// items appeared; for a running group this is the last sign of life.
    ended_at: Option<u64>,
    /// The key the reader's disclosure choice is stored under, and the
    /// collapsed strip's own line. Both are formed here, once per snapshot,
    /// because the view draws them on every frame and they change only when
    /// the transcript does.
    ///
    /// The key is the id of the item that started the group, taken when the
    /// group is created and never revised. A group has no identity of its own
    /// - it is derived from item order on every snapshot - and the reader's
    /// choice to open its steps survives ordinary appended items and status
    /// updates. A tool after narration opens its own group, keyed by that tool,
    /// so adding more prose never moves an existing tool strip's identity.
    pub(super) key: String,
    pub(super) summary: String,
}

impl Turn {
    /// Opens a group whose first item is `index`, taken as its prompt when
    /// `prompt` is set and as its first activity otherwise.
    fn opened<T: Borrow<Item>>(items: &[T], index: usize, prompt: bool) -> Self {
        let item = items.get(index).map(Borrow::borrow);
        Self {
            prompt: if prompt { vec![index] } else { Vec::new() },
            steps: Vec::new(),
            prose: Vec::new(),
            running: false,
            outcome: None,
            started_at: item.and_then(|item| item.started_at.or(item.at)),
            ended_at: item.and_then(|item| item.at),
            key: item.map(|item| item.id.clone()).unwrap_or_default(),
            summary: String::new(),
        }
    }

    /// The glyph and tone a collapsed strip reports for this group.
    ///
    /// A live group reports that it is live, whatever its finished steps did:
    /// a reader deciding whether to wait needs that before they need a
    /// verdict, and the step that failed still says so on its own row.
    ///
    /// `None` when nothing has reported a status at all, which is the opening
    /// seconds of most groups: the first thing a provider sends is reasoning,
    /// and reasoning carries no status. Claiming success there - which a
    /// checkmark does - is a claim about work that has not happened yet.
    pub(super) fn glyph(&self) -> Option<ToolStatus> {
        if self.running {
            return Some(ToolStatus::Running);
        }
        self.outcome
    }

    /// How long this stretch of work has taken, in milliseconds.
    ///
    /// A finished group spans its earliest start to its latest update. A running
    /// one is measured to `now`, so the strip counts up for as long as the
    /// agent is working rather than reporting the age of its most recent
    /// frame. `None` where the store has no stamp for either end, which is how
    /// a transcript written before items carried one still renders.
    pub(super) fn working_for(&self, now: u64) -> Option<u64> {
        let start = self.started_at?;
        let end = if self.running { now } else { self.ended_at? };
        Some(end.saturating_sub(start))
    }

    /// Running timers advance at a readable, whole-second cadence. Finished
    /// durations retain their precision; the cache uses the same cadence so
    /// unrelated redraws cannot invalidate a turn at millisecond frequency.
    pub(super) fn duration_label(&self, now: u64) -> Option<String> {
        self.working_for(now).map(|millis| {
            if self.running {
                match millis / 1_000 {
                    seconds @ 0..60 => format!("{seconds}s"),
                    seconds => format!("{}m {}s", seconds / 60, seconds % 60),
                }
            } else {
                super::format_duration(millis)
            }
        })
    }

    /// Fills in the summary once the group's membership is final.
    fn describe<'c, T: Borrow<Item>>(
        &mut self,
        items: &[T],
        coordinated: &impl Fn(&Item) -> Option<&'c Coordinated>,
    ) {
        // A single step names itself, because "1 step" tells the reader
        // nothing they could not see from the row's absence. Several steps are
        // counted, with a failure called out, because that is the one thing a
        // collapsed strip must not hide.
        self.summary = match self.steps.as_slice() {
            // A group that did no work draws no strip, so it needs no line.
            [] => return,
            [only] => match items.get(only.index).map(Borrow::borrow) {
                Some(item) => {
                    if let Some(coordinated) = coordinated(item) {
                        coordinated.summary()
                    } else {
                        let verb = step_verb(&item.content);
                        let label = step_label(&item.content);
                        if verb.is_empty() {
                            label.to_owned()
                        } else {
                            format!("{verb} {label}")
                        }
                    }
                }
                None => "1 step".to_owned(),
            },
            steps => summarize_steps(steps, items, coordinated)
                .unwrap_or_else(|| format!("{} steps", steps.len())),
        };
        // Still working outranks how the finished steps went, because a
        // reader deciding whether to wait needs that first.
        if self.running {
            self.summary.push_str(" · working");
            return;
        }
        match self.outcome {
            Some(ToolStatus::Failed) => self.summary.push_str(" · failed"),
            Some(ToolStatus::Denied) => self.summary.push_str(" · denied"),
            Some(ToolStatus::Stopped) => self.summary.push_str(" · stopped"),
            Some(ToolStatus::Running) | Some(ToolStatus::Succeeded) | None => {}
        }
    }

    /// Whether nothing has happened in this group since its prompt.
    ///
    /// A user message's remaining parts are what this is for: they arrive as
    /// separate items, and each one would otherwise open a group of its own.
    fn awaiting_activity(&self) -> bool {
        self.steps.is_empty() && self.prose.is_empty()
    }

    /// Extends the group's span to take in an item's stamp.
    ///
    /// The earliest start and latest observation the group actually has. Resumed
    /// history reaches the store unstamped, and a group that mixes it with live work should report
    /// the span it can account for rather than nothing at all.
    fn spans(&mut self, item: &Item) {
        if let Some(start) = item.started_at.or(item.at) {
            self.started_at = Some(
                self.started_at
                    .map_or(start, |previous| previous.min(start)),
            );
        }
        if let Some(end) = item.at {
            self.ended_at = Some(self.ended_at.map_or(end, |previous| previous.max(end)));
        }
    }

    fn observe(&mut self, status: Option<ToolStatus>) {
        match status {
            Some(ToolStatus::Running) => self.running = true,
            // The group's outcome is what its last resolved step reported,
            // not the worst thing that ever happened inside it. A failure a
            // retry recovered from is not how the stretch of work ended, and
            // ranking it highest meant one grep with no matches painted an
            // error icon on the group for the rest of the session.
            Some(status) => self.outcome = Some(status),
            None => {}
        }
    }
}

/// Whether the agent is addressing the reader with this item.
///
/// The answer, not its narration and not its reasoning: those are part of the
/// work rather than a report on it. Their consecutive text chunks stay
/// together; a later tool starts a new chronological group.
fn speaks(item: &Item) -> bool {
    matches!(
        (item.role, &item.content),
        (
            Role::Assistant,
            ItemContent::Text {
                thinking: false,
                phase: Phase::Final,
                ..
            }
        )
    )
}

/// Whether a group is still taking steps, and if not, what closed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    /// Steps, narration and reasoning join the current group.
    Taking,
    /// The agent spoke to the reader. More speech joins the same group,
    /// because one answer reaches the transcript as several text items and a
    /// boundary between them would claim there was work in between; anything
    /// else starts a new group.
    Spoke,
    /// The reader was asked something, or nothing has started yet. Whatever
    /// comes next starts a new group.
    Closed,
}

/// Splits `items` into groups of work, preserving their order.
///
/// A user message opens a group, and so does the first activity after the
/// agent last handed control back. The group runs until the agent says
/// something to the reader or asks them something; tool calls, commands,
/// reasoning and narration join the group that is open. A tool after prose
/// opens another group, so the tool strip never jumps ahead of earlier text.
///
/// The item that closes a group is the last item *in* that group rather than
/// the first item of the next one, because that is the order the view draws:
/// what was asked, what was done, then what came of it. A group therefore
/// reads as one block of work and the sentence it produced.
///
/// `asked_after` holds the ids of items the reader was asked something after.
/// A question never enters the transcript and disappears the moment it is
/// answered, so the boundary it creates has to be remembered by the caller;
/// without that, answering a question would silently merge the work before it
/// with the work after.
///
/// Consecutive user messages with no activity between them share one group.
/// They have to, because a prompt carrying attachments arrives as one item per
/// content part and each part is a `Role::User` item; grouping them by arrival
/// would detach the agent's work from the message that caused it and print an
/// "attachment" bubble as though the reader had sent it alone. Two messages
/// the reader genuinely sent back to back land in one group as well, which is
/// right for the same reason: there is no activity between them to attribute.
///
/// `coordinated` reads what an item did to work with other agents: a message
/// or a launch is drawn as a card among the prose rather than as a step.
pub(super) fn group<'c, T: Borrow<Item>>(
    items: &[T],
    asked_after: &HashSet<String>,
    coordinated: impl Fn(&Item) -> Option<&'c Coordinated>,
) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut state = Open::Closed;
    for (index, item) in items.iter().map(Borrow::borrow).enumerate() {
        if !renders(&item.content) {
            continue;
        }
        let asked = asked_after.contains(item.id.as_str());
        // A message from outside this stretch of work - the reader's own
        // prompt, a steering message, anything the harness delivers on their
        // behalf - is a new instruction, so the work that follows it is
        // attributed to it and not to whatever was running when it landed.
        if item.role == Role::User && !item.content.is_step() {
            match turns.last_mut() {
                Some(turn) if state == Open::Taking && turn.awaiting_activity() => {
                    turn.prompt.push(index);
                    turn.spans(item);
                }
                _ => turns.push(Turn::opened(items, index, true)),
            }
            state = if asked { Open::Closed } else { Open::Taking };
            continue;
        }
        let speech = speaks(item);
        let slot = if coordinated(item).is_some_and(Coordinated::is_card) {
            Slot::Card
        } else {
            slot_of(item)
        };
        // Each rendered group is prompt, contiguous tools, then prose. A tool
        // arriving after any visible prose must start another group; otherwise
        // the view would move it ahead of that earlier reasoning or narration.
        let resumes_tools =
            slot == Slot::Step && turns.last().is_some_and(|turn| !turn.prose.is_empty());
        if state == Open::Closed || (state == Open::Spoke && !speech) || resumes_tools {
            turns.push(Turn::opened(items, index, false));
        }
        // `turns` is non-empty: the branch above pushes when it would be, and
        // the states that skip it only occur once a group exists.
        let turn = turns.last_mut().expect("a group is open");
        let status = item.content.status();
        match coordinated(item) {
            // A card shows how it went itself; the strip summarizes only the
            // steps it collapses, so a refused message beside a lookup does
            // not mark the lookup failed. Still waiting on one keeps the
            // group working.
            Some(_) if slot == Slot::Card => {
                if status == Some(ToolStatus::Running) {
                    turn.running = true;
                }
            }
            Some(step) => turn.observe(status.map(|status| step.reading.status(status))),
            None => turn.observe(status),
        }
        turn.spans(item);
        match slot {
            Slot::Step => turn.steps.push(Placed { index, slot }),
            Slot::Thinking | Slot::Commentary | Slot::Prose | Slot::Card => {
                turn.prose.push(Placed { index, slot })
            }
        }
        state = if asked {
            Open::Closed
        } else if speech {
            Open::Spoke
        } else {
            Open::Taking
        };
    }
    for turn in &mut turns {
        turn.describe(items, &coordinated);
    }
    turns
}

/// Streaming prose changes its body without changing placement or a group's
/// status/timing. Immutable unchanged items take a pointer comparison; changed
/// steps and timestamps conservatively request normal grouping.
pub(super) fn same_structure(
    previous: &[std::sync::Arc<Item>],
    next: &[std::sync::Arc<Item>],
) -> bool {
    previous.len() == next.len() && previous.iter().zip(next).all(|(old, new)| {
        std::sync::Arc::ptr_eq(old, new) || (
            old.id == new.id && old.role == new.role && old.at == new.at && old.started_at == new.started_at
            && (matches!((&old.content, &new.content),
                (ItemContent::Text { thinking: a, phase: ap, .. }, ItemContent::Text { thinking: b, phase: bp, .. })
                if a == b && ap == bp && renders(&old.content) == renders(&new.content))
                || old.content == new.content))
    })
}

/// Whether an item would put anything on screen.
///
/// A provider that announces a session emits a notice with no body, and the
/// surface used to draw it as an empty rectangle at the top of every
/// conversation - two of them, in fact, one captioned and one blank. A text
/// item with nothing in it is also the transient state of a message that is
/// about to stream, so it reappears as soon as its first bytes arrive rather
/// than being dropped for good.
fn renders(content: &ItemContent) -> bool {
    match content {
        ItemContent::Text { text, .. } => !text.trim().is_empty(),
        _ => true,
    }
}

/// Where an item belongs inside its group.
///
/// A session notice is filed with the agent's own narration: it is the
/// harness saying something about the session rather than the agent
/// addressing the reader, so it takes the one recessed line that treatment
/// draws instead of being typeset like an answer. It used to be dropped from
/// the transcript entirely and the newest one pinned above it as a heading,
/// which read as the conversation's subtitle and cost a row on every frame.
pub(super) fn slot_of(item: &Item) -> Slot {
    if item.role == Role::System {
        return Slot::Commentary;
    }
    match &item.content {
        ItemContent::Text { thinking: true, .. } => Slot::Thinking,
        ItemContent::Text {
            phase: Phase::Commentary,
            ..
        } => Slot::Commentary,
        content if content.is_step() => Slot::Step,
        _ => Slot::Prose,
    }
}

/// The one-line summary a collapsed step shows.
///
/// This is the difference between a transcript that says `read · Completed` and
/// one that says `Read src/app.rs`. The provider's own label wins when it has
/// one, because the provider knows what it was doing; the fallbacks exist so an
/// adapter that cannot supply a label still produces something a reader can act
/// on rather than the word "Tool".
pub(super) fn step_label(content: &ItemContent) -> &str {
    match content {
        ItemContent::Text { .. } => "Thinking",
        ItemContent::Command {
            label,
            command,
            action,
            ..
        } => {
            if *action == chat::CommandAction::Run {
                first_line(if label.trim().is_empty() {
                    command
                } else {
                    label
                })
            } else if label.is_empty() {
                first_line(command)
            } else if label == command {
                // Built-in reads/searches carry the exact target in both
                // fields. A filename or pattern may itself start with a verb.
                label
            } else {
                target(*action, label)
            }
        }
        ItemContent::Changes { changes, .. } => match changes.as_slice() {
            [single] => single.path.as_str(),
            _ => "File changes",
        },
        ItemContent::WebSearch { query, .. } => query.as_str(),
        ItemContent::Tool { name, .. } => name.as_str(),
        ItemContent::Image { description, .. } => description.as_str(),
        // As `chat::delivery_heading` has it, less a named sender's summary.
        ItemContent::Delivery {
            from,
            sender,
            summary,
            ..
        } => {
            if !from.is_empty() {
                from.as_str()
            } else if !summary.is_empty() {
                summary.as_str()
            } else {
                sender.get(..8).unwrap_or(sender)
            }
        }
    }
}

/// A command's label without the verb transcripts once stored in it.
///
/// Adapters used to write `Read src/app.rs` and `Search TODO`, which the verb
/// in front of every step turned into "Read Read src/app.rs". They write the
/// target alone now, and a transcript stored before that still holds the old
/// form, so the word is taken off here. Exact command targets are preserved
/// before this fallback; a distinct summary still lacks metadata to distinguish
/// a verb from a target name.
fn target(action: chat::CommandAction, label: &str) -> &str {
    let stored = match action {
        chat::CommandAction::Read => "Read ",
        chat::CommandAction::Search => "Search ",
        chat::CommandAction::List => "List ",
        chat::CommandAction::Run => return label,
    };
    match label.strip_prefix(stored) {
        Some(target) if !target.is_empty() => target,
        _ => label,
    }
}

/// The verb a step is introduced with, kept separate from its target so the two
/// can be typeset differently.
pub(super) fn step_verb(content: &ItemContent) -> &'static str {
    match content {
        ItemContent::Text { .. } => "",
        ItemContent::Command { action, .. } => match action {
            chat::CommandAction::Read => "Read",
            chat::CommandAction::Search => "Searched",
            chat::CommandAction::List => "Listed",
            chat::CommandAction::Run => "Ran",
        },
        ItemContent::Changes { changes, .. } => match changes.as_slice() {
            [single] => match single.kind {
                chat::ChangeKind::Added => "Added",
                chat::ChangeKind::Deleted => "Deleted",
                chat::ChangeKind::Updated => "Edited",
                chat::ChangeKind::Renamed { .. } => "Renamed",
            },
            _ => "Edited",
        },
        ItemContent::WebSearch { .. } => "Searched the web for",
        ItemContent::Tool { .. } => "Called",
        ItemContent::Image { .. } => "Viewed",
        ItemContent::Delivery { from, summary, .. } if from.is_empty() && !summary.is_empty() => "",
        ItemContent::Delivery { kind, .. } => kind.verb(),
    }
}

/// What a group did, told by kind and count the way a reader would tell
/// someone: "Edited 2 files, ran 3 commands, read a file". What changed comes
/// first because it is what a reader scrolling back is looking for; a kind
/// that did not happen is left out. `None` when nothing in the group is a
/// kind this can name, which is when a bare count is the honest summary.
fn summarize_steps<'c, T: Borrow<Item>>(
    steps: &[Placed],
    items: &[T],
    coordinated: &impl Fn(&Item) -> Option<&'c Coordinated>,
) -> Option<String> {
    let mut edited = std::collections::BTreeSet::new();
    let (mut ran, mut read, mut searched, mut listed, mut web, mut called, mut viewed) =
        (0, 0, 0, 0, 0, 0, 0);
    let mut coordinating = 0;
    for placed in steps {
        let item = items.get(placed.index)?.borrow();
        if coordinated(item).is_some_and(|coordinated| coordinated.reading.involves_agents()) {
            coordinating += 1;
            continue;
        }
        match &item.content {
            ItemContent::Changes { changes, .. } => {
                edited.extend(changes.iter().map(|change| change.path.as_str()));
            }
            ItemContent::Command { action, .. } => match action {
                chat::CommandAction::Run => ran += 1,
                chat::CommandAction::Read => read += 1,
                chat::CommandAction::Search => searched += 1,
                chat::CommandAction::List => listed += 1,
            },
            ItemContent::WebSearch { .. } => web += 1,
            ItemContent::Tool { .. } => called += 1,
            ItemContent::Image { .. } => viewed += 1,
            ItemContent::Text { .. } | ItemContent::Delivery { .. } => {}
        }
    }
    let counted = |count: usize, one: &str, many: &str| match count {
        1 => format!("a {one}"),
        count => format!("{count} {many}"),
    };
    let mut parts = Vec::new();
    if !edited.is_empty() {
        parts.push(format!("edited {}", counted(edited.len(), "file", "files")));
    }
    if ran > 0 {
        parts.push(format!("ran {}", counted(ran, "command", "commands")));
    }
    if read > 0 {
        parts.push(format!("read {}", counted(read, "file", "files")));
    }
    if searched > 0 {
        parts.push(format!("ran {}", counted(searched, "search", "searches")));
    }
    if listed > 0 {
        parts.push(format!(
            "listed {}",
            counted(listed, "directory", "directories")
        ));
    }
    if web > 0 {
        parts.push(match web {
            1 => "searched the web".to_owned(),
            count => format!("searched the web {count} times"),
        });
    }
    if called > 0 {
        parts.push(format!("called {}", counted(called, "tool", "tools")));
    }
    if viewed > 0 {
        parts.push(format!("viewed {}", counted(viewed, "image", "images")));
    }
    if coordinating > 0 {
        parts.push(match coordinating {
            1 => "worked with another agent".to_owned(),
            count => format!("worked with other agents {count} times"),
        });
    }
    let mut summary = parts.join(", ");
    let first = summary.get(..1)?.to_uppercase();
    summary.replace_range(..1, &first);
    Some(summary)
}

/// A command's first line, which is what a label should show when a provider
/// offered none. A heredoc or a multi-line pipeline would otherwise push the
/// whole script into a one-line summary.
pub(super) fn first_line(command: &str) -> &str {
    let line = command.trim();
    match line.find('\n') {
        Some(end) => line[..end].trim_end(),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChangeKind, CommandAction, FileChange};

    fn text(id: &str, role: Role, phase: Phase, thinking: bool) -> Item {
        Item {
            id: id.into(),
            role,
            content: ItemContent::Text {
                text: "body".into(),
                thinking,
                phase,
            },
            started_at: None,
            at: None,
        }
    }

    fn command(id: &str, status: ToolStatus) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Command {
                label: String::new(),
                command: "cargo check".into(),
                working_directory: "/tmp".into(),
                action: CommandAction::Run,
                status,
                exit_code: None,
                duration_ms: None,
                output: String::new(),
            },
            started_at: None,
            at: None,
        }
    }

    fn stamped(item: Item, at: u64) -> Item {
        Item {
            started_at: None,
            at: Some(at),
            ..item
        }
    }

    fn slots(turn: &Turn) -> Vec<(usize, Slot)> {
        turn.prose.iter().map(|p| (p.index, p.slot)).collect()
    }

    fn steps(turn: &Turn) -> Vec<usize> {
        turn.steps.iter().map(|p| p.index).collect()
    }

    /// Nothing here coordinates with other agents.
    fn plain(_: &Item) -> Option<&'static Coordinated> {
        None
    }

    /// No question was pending, which is the ordinary case.
    fn unasked() -> HashSet<String> {
        HashSet::new()
    }

    /// A subagent's report prompts the work that follows it the way the
    /// reader's own message does, so that work is attributed to it rather
    /// than to whatever ran before it landed.
    #[test]
    fn prose_appends_reuse_structure_but_placement_and_status_changes_do_not() {
        use std::sync::Arc;
        let initial = vec![Arc::new(text(
            "answer",
            Role::Assistant,
            Phase::Final,
            false,
        ))];
        let mut appended = initial[0].as_ref().clone();
        let ItemContent::Text { text, .. } = &mut appended.content else {
            unreachable!()
        };
        text.push_str(" continued words");
        assert!(same_structure(&initial, &[Arc::new(appended.clone())]));
        appended.at = Some(42);
        assert!(!same_structure(&initial, &[Arc::new(appended.clone())]));
        appended.at = initial[0].at;
        let ItemContent::Text { text, thinking, .. } = &mut appended.content else {
            unreachable!()
        };
        *thinking = true;
        assert!(!same_structure(&initial, &[Arc::new(appended.clone())]));
        let ItemContent::Text { text, thinking, .. } = &mut appended.content else {
            unreachable!()
        };
        *thinking = false;
        text.clear();
        assert!(!same_structure(&initial, &[Arc::new(appended)]));
        let running = vec![Arc::new(command("step", ToolStatus::Running))];
        let resolved = vec![Arc::new(command("step", ToolStatus::Succeeded))];
        assert!(!same_structure(&running, &resolved));
        assert!(same_structure(&running, &running));
    }

    #[test]
    fn a_delivery_opens_the_group_of_the_work_it_prompted() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("a1", Role::Assistant, Phase::Final, false),
            Item {
                id: "report".into(),
                role: Role::User,
                content: ItemContent::Delivery {
                    kind: chat::DeliveryKind::Report,
                    from: "general-purpose".into(),
                    sender: "a1b2c3d4e5".into(),
                    summary: String::new(),
                    body: "Done".into(),
                },
                started_at: None,
                at: None,
            },
            command("c1", ToolStatus::Succeeded),
            text("a2", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].prompt, vec![2]);
        assert_eq!(steps(&turns[1]), vec![3]);
        assert_eq!(slots(&turns[1]), vec![(4, Slot::Prose)]);
    }

    #[test]
    fn a_user_message_opens_a_group_and_the_work_it_caused_joins_it() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Commentary, false),
            text("r1", Role::Assistant, Phase::Final, true),
            text("a2", Role::Assistant, Phase::Final, false),
            text("u2", Role::User, Phase::Final, false),
            command("c2", ToolStatus::Running),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns.len(),
            2,
            "the answer closes the first group and the next message opens one"
        );
        assert_eq!(turns[0].prompt, [0]);
        assert_eq!(steps(&turns[0]), [1], "a command is a step");
        assert_eq!(
            slots(&turns[0]),
            [(2, Slot::Commentary), (3, Slot::Thinking), (4, Slot::Prose)],
            "narration, reasoning and the answer each have their own treatment"
        );
        assert!(!turns[0].running);
        assert_eq!(turns[1].prompt, [5]);
        assert_eq!(steps(&turns[1]), [6]);
        assert!(turns[1].running, "a running step marks its own group live");
    }

    #[test]
    fn agent_speech_closes_a_group_so_later_work_is_not_attributed_to_the_prompt() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Final, false),
            command("c2", ToolStatus::Succeeded),
            command("c3", ToolStatus::Succeeded),
            text("a2", Role::Assistant, Phase::Final, false),
            text("a3", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2, "what the agent said in between splits them");
        assert_eq!(steps(&turns[0]), [1]);
        assert_eq!(slots(&turns[0]), [(2, Slot::Prose)]);
        assert!(
            turns[1].prompt.is_empty(),
            "work the agent started on its own has no prompt to invent"
        );
        assert_eq!(
            steps(&turns[1]),
            [3, 4],
            "the steps after the answer belong to the group that follows it"
        );
        assert_eq!(
            slots(&turns[1]),
            [(5, Slot::Prose), (6, Slot::Prose)],
            "one answer arriving as two items is still one answer"
        );
        assert_eq!(
            turns[1].key, "c2",
            "a group is keyed by the item that started it"
        );
    }

    #[test]
    fn a_question_closes_the_group_so_the_answer_starts_new_work() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            command("c2", ToolStatus::Succeeded),
        ];
        let asked = HashSet::from(["c1".to_owned()]);
        let turns = group(&items, &asked, plain);
        assert_eq!(
            turns.len(),
            2,
            "being asked something ends the stretch of work that asked"
        );
        assert_eq!(steps(&turns[0]), [1]);
        assert_eq!(steps(&turns[1]), [2]);
        assert_eq!(
            group(&items, &unasked(), plain).len(),
            1,
            "with no question pending the same items are one stretch of work"
        );
    }

    #[test]
    fn every_part_of_one_user_message_shares_the_group_its_activity_belongs_to() {
        let items = vec![
            text("u1/input/0", Role::User, Phase::Final, false),
            Item {
                id: "u1/input/1".into(),
                role: Role::User,
                content: ItemContent::Image {
                    path: "/home/reader/diagram.png".into(),
                    description: "User attachment".into(),
                    reference: None,
                },
                started_at: None,
                at: None,
            },
            command("c1", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns.len(),
            1,
            "a prompt and its attachment are one message, not two groups"
        );
        assert_eq!(turns[0].prompt, [0, 1]);
        assert_eq!(
            steps(&turns[0]),
            [2],
            "the work stays attached to the message that caused it"
        );
        assert_eq!(turns[0].prose.len(), 1);
    }

    #[test]
    fn a_group_claims_no_outcome_until_a_step_reports_one() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("r1", Role::Assistant, Phase::Final, true),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].glyph(),
            None,
            "reasoning carries no status, so nothing may claim success yet"
        );
        assert_eq!(
            turns[0].summary, "",
            "a group that did no work has no strip to caption"
        );
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Failed),
            command("c2", ToolStatus::Running),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].glyph(),
            Some(ToolStatus::Running),
            "a live group reports that it is live; the step that failed says so on its own row"
        );
        assert_eq!(
            turns[0].summary, "Ran 2 commands · working",
            "still working outranks how the finished steps went"
        );
    }

    #[test]
    fn the_collapsed_outcome_is_how_the_group_ended() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Failed),
            command("c2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].outcome,
            Some(ToolStatus::Succeeded),
            "a failure a later step recovered from is not how the work ended"
        );
        assert_eq!(turns[0].summary, "Ran 2 commands");
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            command("c2", ToolStatus::Failed),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].outcome,
            Some(ToolStatus::Failed),
            "a group that ended on a failure still reports it"
        );
        assert_eq!(turns[0].summary, "Ran 2 commands · failed");
    }

    #[test]
    fn a_card_reports_its_own_outcome_and_not_the_strip_beside_it() {
        use crate::chat::coordination::{Act, Coordination, Kind, Outbound, OutboundKind, Text};
        let reading = |kind| {
            Coordinated::new(Coordination {
                kind,
                outcome: Default::default(),
                subagents: Vec::new(),
                cli: true,
            })
        };
        let lookup = reading(Kind::Act(Act::Whoami));
        let message = reading(Kind::Message(Outbound {
            kind: OutboundKind::Send,
            to: "db8a852c".into(),
            body: Text::Inline("hello".into()),
            summary: String::new(),
            waits: false,
        }));
        let coordinated = |item: &Item| match item.id.as_str() {
            "whoami" => Some(&lookup),
            "send" => Some(&message),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("whoami", ToolStatus::Succeeded),
            command("send", ToolStatus::Failed),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(steps(&turns[0]), [1]);
        assert_eq!(slots(&turns[0]), [(2, Slot::Card)]);
        assert_eq!(
            turns[0].summary, "Looked up this session",
            "the refused message says so on its card; the lookup went fine"
        );
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("whoami", ToolStatus::Succeeded),
            command("send", ToolStatus::Running),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(
            turns[0].glyph(),
            Some(ToolStatus::Running),
            "waiting on a card's answer is still work in progress"
        );
    }

    #[test]
    fn a_group_is_measured_from_its_first_item_to_its_last() {
        let items = vec![
            stamped(text("u1", Role::User, Phase::Final, false), 1_000),
            stamped(command("c1", ToolStatus::Succeeded), 1_500),
            stamped(text("a1", Role::Assistant, Phase::Final, false), 3_500),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].working_for(9_000),
            Some(2_500),
            "a finished group is measured by its own span, not against now"
        );
        let items = vec![
            stamped(text("u1", Role::User, Phase::Final, false), 1_000),
            stamped(command("c1", ToolStatus::Running), 1_200),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].working_for(9_000),
            Some(8_000),
            "a running group counts up to now"
        );
        let undated = vec![text("u1", Role::User, Phase::Final, false)];
        assert_eq!(
            group(&undated, &unasked(), plain)[0].working_for(9_000),
            None,
            "a transcript stored without stamps reports no duration at all"
        );
        // A resumed session's history reaches the store unstamped, so a group
        // that mixes it with live work reports the part it can account for.
        let mixed = vec![
            text("u1", Role::User, Phase::Final, false),
            stamped(command("c1", ToolStatus::Succeeded), 2_000),
            stamped(text("a1", Role::Assistant, Phase::Final, false), 2_600),
        ];
        assert_eq!(
            group(&mixed, &unasked(), plain)[0].working_for(9_000),
            Some(600)
        );
    }

    #[test]
    fn finished_group_includes_late_updates_to_earlier_items() {
        let mut tool = command("c1", ToolStatus::Succeeded);
        tool.started_at = Some(1_000);
        tool.at = Some(5_000);
        let mut reasoning = text("r1", Role::Assistant, Phase::Final, true);
        reasoning.started_at = Some(2_000);
        reasoning.at = Some(3_000);
        let turns = group(&[tool, reasoning], &unasked(), plain);
        assert_eq!(turns[0].working_for(9_000), Some(4_000));
    }

    #[test]
    fn streamed_tool_duration_keeps_its_first_observation() {
        let mut tool = command("c1", ToolStatus::Running);
        tool.started_at = Some(1_000);
        tool.at = Some(5_950);
        let first = group(&[tool.clone()], &unasked(), plain);
        assert_eq!(first[0].working_for(6_000), Some(5_000));
        assert_eq!(first[0].duration_label(6_000).as_deref(), Some("5s"));
        assert_eq!(
            first[0].duration_label(6_999),
            first[0].duration_label(6_000)
        );
        assert_eq!(first[0].duration_label(7_000).as_deref(), Some("6s"));
        tool.at = Some(6_050);
        let updated = group(&[tool.clone()], &unasked(), plain);
        assert_eq!(updated[0].working_for(6_100), Some(5_100));
        if let ItemContent::Command { status, .. } = &mut tool.content {
            *status = ToolStatus::Succeeded;
        }
        let finished = group(&[tool], &unasked(), plain);
        assert_eq!(finished[0].working_for(90_000), Some(5_050));
        assert_eq!(finished[0].duration_label(90_000).as_deref(), Some("5.0s"));
    }

    #[test]
    fn running_timer_handles_subsecond_and_minute_boundaries() {
        let mut tool = command("c1", ToolStatus::Running);
        tool.started_at = Some(1_000);
        tool.at = Some(1_100);
        let turns = group(&[tool], &unasked(), plain);
        assert_eq!(turns[0].duration_label(999).as_deref(), Some("0s"));
        assert_eq!(turns[0].duration_label(1_999).as_deref(), Some("0s"));
        assert_eq!(turns[0].duration_label(60_999).as_deref(), Some("59s"));
        assert_eq!(turns[0].duration_label(61_000).as_deref(), Some("1m 0s"));
    }

    #[test]
    fn activity_before_any_user_message_forms_a_promptless_group() {
        let items = vec![
            text("a1", Role::Assistant, Phase::Final, false),
            text("u1", Role::User, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert!(
            turns[0].prompt.is_empty(),
            "history must not invent a prompt"
        );
        assert_eq!(slots(&turns[0]), [(0, Slot::Prose)]);
        assert_eq!(
            turns[0].key, "a1",
            "the group is keyed by the item that started it, prompt or not"
        );
        assert_eq!(turns[1].prompt, [1]);
    }

    /// A notice used to be dropped from the transcript and the newest one
    /// pinned above it as a heading. It belongs in the conversation, reading
    /// as the aside it is, between the work that preceded and followed it.
    #[test]
    fn a_notice_is_narration_inside_the_work_it_lands_in() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            text("s1", Role::System, Phase::Final, false),
            command("c2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns.len(),
            2,
            "later tools must not move ahead of the notice"
        );
        assert_eq!(
            slots(&turns[0]),
            [(2, Slot::Commentary)],
            "a notice recedes to the treatment narration gets"
        );
        assert_eq!(
            steps(&turns[0]),
            [1],
            "the notice is not counted as work the agent did"
        );
    }

    #[test]
    fn reasoning_is_inline_prose_rather_than_a_step() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("r1", Role::Assistant, Phase::Final, true),
            command("c1", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert!(steps(&turns[0]).is_empty());
        assert_eq!(steps(&turns[1]), [2]);
        assert_eq!(
            slots(&turns[0]),
            [(1, Slot::Thinking)],
            "it is rendered in the transcript, in its own slot"
        );
        assert_eq!(
            turns[1].summary, "Ran cargo check",
            "and it does not count toward what the strip says was done"
        );
    }

    #[test]
    fn a_notice_with_no_body_is_not_rendered_at_all() {
        // Two `System` headings used to open every conversation, one of them
        // with no body whatsoever, drawn as an empty rectangle.
        let empty = Item {
            id: "s1".into(),
            role: Role::System,
            content: ItemContent::Text {
                text: "   \n".into(),
                thinking: false,
                phase: Phase::Final,
            },
            started_at: None,
            at: None,
        };
        assert_eq!(
            group(std::slice::from_ref(&empty), &unasked(), plain).len(),
            0
        );
        assert_eq!(group::<Item>(&[], &unasked(), plain).len(), 0);
        // A message that has not streamed its first bytes yet is the same
        // shape, and must come back once it has them.
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            empty,
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            slots(&turns[0]),
            [(2, Slot::Prose)],
            "only the item with a body occupies a slot"
        );
    }

    /// The view renders prompt, tools, prose for each group. Every streaming
    /// prefix must reproduce arrival order, including system notices, images,
    /// and empty text that gains its first bytes after later items arrive.
    #[test]
    fn interleaved_transcript_groups_render_in_arrival_order() {
        let mut items = vec![
            text("prompt", Role::User, Phase::Final, false),
            text("reasoning", Role::Assistant, Phase::Final, true),
            command("tool1", ToolStatus::Succeeded),
            command("tool2", ToolStatus::Running),
            text("progress", Role::Assistant, Phase::Commentary, false),
            command("tool3", ToolStatus::Succeeded),
            text("notice", Role::System, Phase::Final, false),
            Item {
                id: "image".into(),
                role: Role::Assistant,
                content: ItemContent::Image {
                    path: "/fixture.png".into(),
                    description: "Preview".into(),
                    reference: None,
                },
                started_at: None,
                at: None,
            },
            command("tool4", ToolStatus::Succeeded),
            text("answer", Role::Assistant, Phase::Final, false),
        ];
        let rendered = |items: &[Item]| {
            group(items, &unasked(), plain)
                .into_iter()
                .flat_map(|turn| {
                    turn.prompt
                        .into_iter()
                        .chain(turn.steps.into_iter().map(|placed| placed.index))
                        .chain(turn.prose.into_iter().map(|placed| placed.index))
                })
                .collect::<Vec<_>>()
        };
        for end in 0..=items.len() {
            assert_eq!(rendered(&items[..end]), (0..end).collect::<Vec<_>>());
        }
        let ItemContent::Text { text, .. } = &mut items[4].content else {
            unreachable!()
        };
        text.clear();
        assert_eq!(
            rendered(&items),
            (0..items.len())
                .filter(|index| *index != 4)
                .collect::<Vec<_>>()
        );
        let ItemContent::Text { text, .. } = &mut items[4].content else {
            unreachable!()
        };
        *text = "Progress before tool3".into();
        assert_eq!(rendered(&items), (0..items.len()).collect::<Vec<_>>());
        let turns = group(&items, &unasked(), plain);
        let tools = turns.iter().find(|turn| steps(turn) == [2, 3]).unwrap();
        assert_eq!(tools.key, "tool1");
        assert!(tools.running);
    }

    #[test]
    fn a_command_without_a_provider_label_falls_back_to_its_first_line() {
        let content = ItemContent::Command {
            label: String::new(),
            command: "git add -A \\\n  && git commit".into(),
            working_directory: String::new(),
            action: CommandAction::Run,
            status: ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(120),
            output: String::new(),
        };
        assert_eq!(step_label(&content), "git add -A \\");
        assert_eq!(step_verb(&content), "Ran");
    }

    #[test]
    fn multiline_shell_labels_do_not_expose_heredoc_payloads() {
        let mut content = command("shell", ToolStatus::Running).content;
        if let ItemContent::Command { label, command, .. } = &mut content {
            *command = "cat > tests.rs <<'EOF'\nprivate payload\nEOF".into();
            *label = command.clone();
        }
        assert_eq!(step_label(&content), "cat > tests.rs <<'EOF'");
    }

    #[test]
    fn exact_targets_that_start_with_verbs_keep_their_bytes() {
        for (action, target, expected) in [
            (
                CommandAction::Read,
                "Read λ notes.md",
                "Read Read λ notes.md",
            ),
            (CommandAction::Search, "Search TODO", "Searched Search TODO"),
            (CommandAction::List, "List *.rs", "Listed List *.rs"),
        ] {
            let content = ItemContent::Command {
                label: target.into(),
                command: target.into(),
                working_directory: "/workspace".into(),
                action,
                status: ToolStatus::Succeeded,
                exit_code: None,
                duration_ms: None,
                output: String::new(),
            };
            assert_eq!(step_label(&content), target);
            let item = Item {
                id: "exact".into(),
                role: Role::Assistant,
                content,
                started_at: None,
                at: None,
            };
            assert_eq!(group(&[item], &unasked(), plain)[0].summary, expected);
        }
    }

    /// A read, a search and a listing say what they did once, whether the
    /// label is the target an adapter writes now or the verb and target a
    /// transcript stored earlier.
    #[test]
    fn a_classified_command_states_its_verb_once() {
        let command = |action, label: &str| ItemContent::Command {
            label: label.into(),
            command: "sed -n 1,20p src/app.rs".into(),
            working_directory: String::new(),
            action,
            status: ToolStatus::Succeeded,
            exit_code: None,
            duration_ms: None,
            output: String::new(),
        };
        for (action, label, row) in [
            (CommandAction::Read, "src/app.rs", "Read src/app.rs"),
            (CommandAction::Read, "Read src/app.rs", "Read src/app.rs"),
            (CommandAction::Search, "TODO in src", "Searched TODO in src"),
            (CommandAction::Search, "Search TODO", "Searched TODO"),
            (CommandAction::List, "**/*.rs", "Listed **/*.rs"),
            (CommandAction::List, "List src", "Listed src"),
            // A shell command's label is the provider's own description.
            (CommandAction::Run, "Read the logs", "Ran Read the logs"),
            // Only the stored verb is taken off, and never the whole label.
            (CommandAction::Read, "Search.md", "Read Search.md"),
            (CommandAction::Read, "Read ", "Read Read "),
        ] {
            let content = command(action, label);
            assert_eq!(
                format!("{} {}", step_verb(&content), step_label(&content)),
                row,
                "{action:?} {label:?}"
            );
            // A group of one step is summarized as that step's row.
            let items = [Item {
                id: "step".into(),
                role: Role::Assistant,
                content,
                started_at: None,
                at: None,
            }];
            assert_eq!(group(&items, &unasked(), plain)[0].summary, row);
        }
    }

    #[test]
    fn a_single_file_change_names_the_path_and_its_kind() {
        let content = ItemContent::Changes {
            changes: vec![FileChange {
                path: "src/app.rs".into(),
                kind: ChangeKind::Renamed {
                    from: "src/old.rs".into(),
                },
                diff: String::new(),
            }],
            status: ToolStatus::Succeeded,
        };
        assert_eq!(step_label(&content), "src/app.rs");
        assert_eq!(step_verb(&content), "Renamed");
    }
}
