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

use crate::chat::{self, Item, ItemContent, Phase, Role, ToolStatus};
use std::collections::HashSet;

/// Where an item belongs inside its group.
///
/// The four roles read very differently and must not share a treatment:
/// `Step` is what the agent did, `Thinking` is it reasoning about what to do,
/// `Commentary` is it narrating while it worked, and `Prose` is what the user
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    /// A command, tool call, file change, or search.
    Step,
    /// The agent's own reasoning, which is prose about prose: it is rendered
    /// inline and set back, never filed behind a disclosure row.
    Thinking,
    /// Progress narration. Measured over this workstation's Codex history it
    /// outnumbers answers roughly seven to one, so it has to recede.
    Commentary,
    /// The answer, or a user message, or a system notice.
    Prose,
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
    /// Unix milliseconds of the group's first item, which is when this stretch
    /// of work began.
    started_at: Option<u64>,
    /// Unix milliseconds of the group's last item. For a running group this is
    /// the last sign of life, not the end of the work.
    ended_at: Option<u64>,
    /// The key the reader's disclosure choice is stored under, and the
    /// collapsed strip's own line. Both are formed here, once per snapshot,
    /// because the view draws them on every frame and they change only when
    /// the transcript does.
    ///
    /// The key is the id of the item that started the group, taken when the
    /// group is created and never revised. A group has no identity of its own
    /// - it is derived from item order on every snapshot - and the reader's
    /// choice to open its steps has to survive being rederived. Keying it on
    /// whichever item happens to be first *now* did not: a group opened by a
    /// narration line was rekeyed the moment its first step arrived, and the
    /// choice stored under the old key was dropped as a stale entry.
    pub(super) key: String,
    pub(super) summary: String,
}

impl Turn {
    /// Opens a group whose first item is `index`, taken as its prompt when
    /// `prompt` is set and as its first activity otherwise.
    fn opened(items: &[Item], index: usize, prompt: bool) -> Self {
        let item = items.get(index);
        Self {
            prompt: if prompt { vec![index] } else { Vec::new() },
            steps: Vec::new(),
            prose: Vec::new(),
            running: false,
            outcome: None,
            started_at: item.and_then(|item| item.at),
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
    /// A finished group is measured from its first item to its last. A running
    /// one is measured to `now`, so the strip counts up for as long as the
    /// agent is working rather than reporting the age of its most recent
    /// frame. `None` where the store has no stamp for either end, which is how
    /// a transcript written before items carried one still renders.
    pub(super) fn working_for(&self, now: u64) -> Option<u64> {
        let start = self.started_at?;
        let end = if self.running { now } else { self.ended_at? };
        Some(end.saturating_sub(start))
    }

    /// Fills in the summary once the group's membership is final.
    fn describe(&mut self, items: &[Item]) {
        // A single step names itself, because "1 step" tells the reader
        // nothing they could not see from the row's absence. Several steps are
        // counted, with a failure called out, because that is the one thing a
        // collapsed strip must not hide.
        self.summary = match self.steps.as_slice() {
            // A group that did no work draws no strip, so it needs no line.
            [] => return,
            [only] => match items.get(only.index) {
                Some(item) => {
                    let verb = step_verb(&item.content);
                    let label = step_label(&item.content);
                    if verb.is_empty() {
                        label.to_owned()
                    } else {
                        format!("{verb} {label}")
                    }
                }
                None => "1 step".to_owned(),
            },
            steps => format!("{} steps", steps.len()),
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
    /// The first and last stamp the group actually has, rather than the stamps
    /// of its first and last item: a resumed session's history reaches the
    /// store unstamped, and a group that mixes it with live work should report
    /// the span it can account for rather than nothing at all.
    fn spans(&mut self, at: Option<u64>) {
        self.started_at = self.started_at.or(at);
        self.ended_at = at.or(self.ended_at);
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
/// work rather than a report on it, and a group that broke on either would
/// break several times a second.
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
/// reasoning and narration all join the group that is open.
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
pub(super) fn group(items: &[Item], asked_after: &HashSet<String>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut state = Open::Closed;
    for (index, item) in items.iter().enumerate() {
        // A notice is not part of any exchange: it says something about the
        // session as a whole, so it is pinned above the transcript rather than
        // scrolling away inside whichever group happened to be open when the
        // provider sent it.
        if !renders(&item.content) || item.role == Role::System {
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
                    turn.spans(item.at);
                }
                _ => turns.push(Turn::opened(items, index, true)),
            }
            state = if asked { Open::Closed } else { Open::Taking };
            continue;
        }
        let speech = speaks(item);
        if state == Open::Closed || (state == Open::Spoke && !speech) {
            turns.push(Turn::opened(items, index, false));
        }
        // `turns` is non-empty: the branch above pushes when it would be, and
        // the states that skip it only occur once a group exists.
        let turn = turns.last_mut().expect("a group is open");
        let slot = slot_of(&item.content);
        turn.observe(item.content.status());
        turn.spans(item.at);
        match slot {
            Slot::Step => turn.steps.push(Placed { index, slot }),
            Slot::Thinking | Slot::Commentary | Slot::Prose => {
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
        turn.describe(items);
    }
    turns
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

fn slot_of(content: &ItemContent) -> Slot {
    match content {
        ItemContent::Text { thinking: true, .. } => Slot::Thinking,
        ItemContent::Text {
            phase: Phase::Commentary,
            ..
        } => Slot::Commentary,
        _ if content.is_step() => Slot::Step,
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
        ItemContent::Command { label, command, .. } => {
            if label.is_empty() {
                first_line(command)
            } else {
                label.as_str()
            }
        }
        ItemContent::Changes { changes, .. } => match changes.as_slice() {
            [single] => single.path.as_str(),
            _ => "File changes",
        },
        ItemContent::WebSearch { query, .. } => query.as_str(),
        ItemContent::Tool { name, .. } => name.as_str(),
        ItemContent::Image { description, .. } => description.as_str(),
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
    }
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
            at: None,
        }
    }

    fn stamped(item: Item, at: u64) -> Item {
        Item {
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

    /// No question was pending, which is the ordinary case.
    fn unasked() -> HashSet<String> {
        HashSet::new()
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
        let turns = group(&items, &unasked());
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
        let turns = group(&items, &unasked());
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
        let turns = group(&items, &asked);
        assert_eq!(
            turns.len(),
            2,
            "being asked something ends the stretch of work that asked"
        );
        assert_eq!(steps(&turns[0]), [1]);
        assert_eq!(steps(&turns[1]), [2]);
        assert_eq!(
            group(&items, &unasked()).len(),
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
                },
                at: None,
            },
            command("c1", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked());
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
        let turns = group(&items, &unasked());
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
        let turns = group(&items, &unasked());
        assert_eq!(
            turns[0].glyph(),
            Some(ToolStatus::Running),
            "a live group reports that it is live; the step that failed says so on its own row"
        );
        assert_eq!(
            turns[0].summary, "2 steps · working",
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
        let turns = group(&items, &unasked());
        assert_eq!(
            turns[0].outcome,
            Some(ToolStatus::Succeeded),
            "a failure a later step recovered from is not how the work ended"
        );
        assert_eq!(turns[0].summary, "2 steps");
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            command("c2", ToolStatus::Failed),
        ];
        let turns = group(&items, &unasked());
        assert_eq!(
            turns[0].outcome,
            Some(ToolStatus::Failed),
            "a group that ended on a failure still reports it"
        );
        assert_eq!(turns[0].summary, "2 steps · failed");
    }

    #[test]
    fn a_group_is_measured_from_its_first_item_to_its_last() {
        let items = vec![
            stamped(text("u1", Role::User, Phase::Final, false), 1_000),
            stamped(command("c1", ToolStatus::Succeeded), 1_500),
            stamped(text("a1", Role::Assistant, Phase::Final, false), 3_500),
        ];
        let turns = group(&items, &unasked());
        assert_eq!(
            turns[0].working_for(9_000),
            Some(2_500),
            "a finished group is measured by its own span, not against now"
        );
        let items = vec![
            stamped(text("u1", Role::User, Phase::Final, false), 1_000),
            stamped(command("c1", ToolStatus::Running), 1_200),
        ];
        let turns = group(&items, &unasked());
        assert_eq!(
            turns[0].working_for(9_000),
            Some(8_000),
            "a running group counts up to now"
        );
        let undated = vec![text("u1", Role::User, Phase::Final, false)];
        assert_eq!(
            group(&undated, &unasked())[0].working_for(9_000),
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
        assert_eq!(group(&mixed, &unasked())[0].working_for(9_000), Some(600));
    }

    #[test]
    fn activity_before_any_user_message_forms_a_promptless_group() {
        let items = vec![
            text("a1", Role::Assistant, Phase::Final, false),
            text("u1", Role::User, Phase::Final, false),
        ];
        let turns = group(&items, &unasked());
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

    #[test]
    fn a_notice_is_not_an_exchange() {
        let items = vec![
            text("s1", Role::System, Phase::Final, false),
            text("u1", Role::User, Phase::Final, false),
        ];
        let turns = group(&items, &unasked());
        assert_eq!(
            turns.len(),
            1,
            "a session notice belongs to the session, not to a group of its own"
        );
        assert_eq!(turns[0].prompt, [1]);
    }

    #[test]
    fn reasoning_is_inline_prose_rather_than_a_step() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("r1", Role::Assistant, Phase::Final, true),
            command("c1", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked());
        assert_eq!(
            steps(&turns[0]),
            [2],
            "reasoning is not filed behind a disclosure row"
        );
        assert_eq!(
            slots(&turns[0]),
            [(1, Slot::Thinking)],
            "it is rendered in the transcript, in its own slot"
        );
        assert_eq!(
            turns[0].summary, "Ran cargo check",
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
            at: None,
        };
        assert_eq!(group(std::slice::from_ref(&empty), &unasked()).len(), 0);
        assert_eq!(group(&[], &unasked()).len(), 0);
        // A message that has not streamed its first bytes yet is the same
        // shape, and must come back once it has them.
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            empty,
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked());
        assert_eq!(turns.len(), 1);
        assert_eq!(
            slots(&turns[0]),
            [(2, Slot::Prose)],
            "only the item with a body occupies a slot"
        );
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
