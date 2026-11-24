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

use super::coordination::{Coordinated, MessageRun};
use super::disclosure::Status;
use super::tool_title::{Title, verb};
use crate::chat::{self, Item, ItemContent, Phase, Role, ToolStatus};
use std::{
    borrow::{Borrow, Cow},
    collections::HashSet,
};

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
    /// Stretches of reasoning with nothing to read that took long enough to
    /// account for, where they fell among the steps. An open strip lists
    /// them so its total does not hide that much of it was thinking.
    pub(super) pauses: Vec<Pause>,
    /// Set while any step in this group is still running, which is what marks
    /// the live group without needing a separate global flag.
    pub(super) running: bool,
    /// Set while the agent, at work on nothing else, thinks with nothing to
    /// show after this group's steps: the group is running on that thought.
    pub(super) thinking: bool,
    /// Whether this group holds the newest step still running anywhere in the
    /// transcript. Older running groups, such as one waiting on a background
    /// command, leave the activity indicator to newer work.
    pub(super) newest_running: bool,
    /// A failure anywhere in the group, or its last resolved state otherwise.
    /// Collapsing the group must not hide a failed step behind later success.
    pub(super) outcome: Option<ToolStatus>,
    /// How many of the group's steps failed, and how many were refused. A
    /// collapsed strip counts them, so one failure among several steps does
    /// not read as if every step had failed, nor a refusal before later
    /// success go unsaid.
    failed: usize,
    denied: usize,
    /// When this stretch of work began, by its earliest first observation,
    /// and its latest observation. An earlier item can finish after later
    /// items appeared; for a running group this is the last sign of life.
    span: Span,
    /// The span before its latest message card, which the group returns to
    /// when that message leaves it for the messages after it.
    before_card: Span,
    /// What the group took in since that card, such as hidden thinking,
    /// which leaves with the message for the run it starts.
    since_card: Span,
    /// The key the reader's disclosure choice is stored under, and the
    /// collapsed strip's own line. Both are formed here, once per snapshot,
    /// because the view draws them on every frame and they change only when
    /// the transcript does.
    ///
    /// The key is the id of the item that started the group, taken when the
    /// group is created and never revised. A group has no identity of its own -
    /// it is derived from item order on every snapshot - and the reader's
    /// choice to open its steps survives ordinary appended items and status
    /// updates. A tool after narration opens its own group, keyed by that tool,
    /// so adding more prose never moves an existing tool strip's identity.
    pub(super) key: String,
    pub(super) summary: String,
    /// Whether the group's steps are messages to other agents and nothing
    /// else: several sent one after another, which fold into one row as
    /// other steps do. One message alone is a card among the prose.
    pub(super) messages: bool,
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
            pauses: Vec::new(),
            running: false,
            thinking: false,
            newest_running: false,
            outcome: None,
            failed: 0,
            denied: 0,
            span: item.map(Span::of).unwrap_or_default(),
            before_card: Span::default(),
            since_card: Span::default(),
            key: item.map(|item| item.id.clone()).unwrap_or_default(),
            summary: String::new(),
            messages: false,
        }
    }

    /// The glyph and tone a collapsed strip reports for this group.
    ///
    /// A live group reports that it is live, whatever its finished steps did:
    /// a reader deciding whether to wait needs that before they need a
    /// verdict, and the step that failed still says so on its own row.
    ///
    /// A failure is never hidden, but the full error mark is kept for a
    /// group in which every step failed; where others did not, the mark is
    /// the partial one, so a strip with one failure among many steps does not
    /// look like one in which nothing worked.
    ///
    /// `None` when nothing has reported a status at all, which is the opening
    /// seconds of most groups: the first thing a provider sends is reasoning,
    /// and reasoning carries no status. Claiming success there - which a
    /// checkmark does - is a claim about work that has not happened yet.
    pub(super) fn glyph(&self) -> Option<Status> {
        if self.running {
            return Some(ToolStatus::Running.into());
        }
        if self.partly_failed() {
            return Some(Status::PartlyFailed);
        }
        // A refusal is not hidden behind later success either.
        if self.denied > 0 && self.outcome == Some(ToolStatus::Succeeded) {
            return Some(ToolStatus::Denied.into());
        }
        self.outcome.map(Status::from)
    }

    /// Whether some of the group's steps failed and others went through.
    /// Refused steps did not, so failures and refusals that cover every
    /// step keep the full error mark.
    fn partly_failed(&self) -> bool {
        self.failed > 0 && self.failed + self.denied < self.steps.len()
    }

    /// How long this stretch of work has taken, in milliseconds.
    ///
    /// A finished group spans its earliest start to its latest update. A running
    /// one is measured to `now`, so the strip counts up for as long as the
    /// agent is working rather than reporting the age of its most recent
    /// frame. `None` where the store has no stamp for either end, which is how
    /// a transcript written before items carried one still renders.
    pub(super) fn working_for(&self, now: u64) -> Option<u64> {
        let start = self.span.start?;
        let end = if self.running { now } else { self.span.end? };
        Some(end.saturating_sub(start))
    }

    /// Timers use the same compact seconds/minutes notation as reasoning and
    /// individual steps. The retained cache shares the running timer cadence.
    pub(super) fn duration_label(&self, now: u64) -> Option<String> {
        self.working_for(now).map(super::disclosure::duration)
    }

    /// Fills in the summary once the group's membership is final.
    fn describe<'c, T: Borrow<Item>>(&mut self, items: &[T], presented: &impl Presented<'c>) {
        // Messages say whom they went to, what they said, and how they
        // stand together, which counts what did not go first.
        if self.messages {
            let sent = self.steps.iter().filter_map(|placed| {
                let item = items.get(placed.index)?.borrow();
                Some((item, presented.coordinated(item)?))
            });
            if let Some(run) = MessageRun::of(sent, &|_| None) {
                self.summary = run.line();
                return;
            }
        }
        // A single step names itself, because "1 step" tells the reader
        // nothing they could not see from the row's absence. Several steps are
        // counted, with a failure called out, because that is the one thing a
        // collapsed strip must not hide.
        self.summary = match self.steps.as_slice() {
            // A group that did no work draws no strip, so it needs no line.
            [] => return,
            [only] => match items.get(only.index).map(Borrow::borrow) {
                Some(item) => headline(if let Some(coordinated) = presented.coordinated(item) {
                    coordinated.summary()
                } else if let ItemContent::Tool {
                    name,
                    input,
                    status,
                    ..
                } = &item.content
                {
                    // A call whose presentation is not formed yet, as
                    // one that just changed, is read here once.
                    match presented.titled(item) {
                        Some(title) => title.line(),
                        None => Title::of(name, input, *status).line(),
                    }
                } else {
                    let (verb, label) = step_title(&item.content, None);
                    if verb.is_empty() {
                        label.into_owned()
                    } else {
                        format!("{verb} {label}")
                    }
                }),
                None => "1 step".to_owned(),
            },
            steps => summarize_steps(steps, items, presented, self.failed + self.denied > 0)
                .unwrap_or_else(|| format!("{} steps", steps.len())),
        };
        self.summary.push_str(&self.state_suffix());
    }

    /// How the group stands, as the end of its summary line.
    pub(super) fn state_suffix(&self) -> Cow<'static, str> {
        // Still working outranks how the finished steps went, because a
        // reader deciding whether to wait needs that first.
        if self.running {
            return Cow::Borrowed(" · working");
        }
        // Several steps say how many of them failed or were refused, after
        // the line that counted them: "Ran 3 commands · 1 failed", and
        // "all failed" where none went otherwise. A lone step's line is
        // about that step, so the bare word says it.
        let steps = self.steps.len();
        if steps > 1 {
            let counted = |count: usize, word: &str| match count {
                0 => None,
                count if count >= steps => Some(format!("all {word}")),
                count => Some(format!("{count} {word}")),
            };
            let counts = [
                counted(self.failed, "failed"),
                counted(self.denied, "denied"),
            ];
            let counts: Vec<String> = counts.into_iter().flatten().collect();
            if !counts.is_empty() {
                return Cow::Owned(format!(" · {}", counts.join(", ")));
            }
        }
        Cow::Borrowed(match self.outcome {
            Some(ToolStatus::Failed) => " · failed",
            Some(ToolStatus::Denied) => " · denied",
            Some(ToolStatus::Stopped) => " · stopped",
            Some(ToolStatus::Ended) => " · ended",
            Some(ToolStatus::Waiting) => " · waiting on background work",
            Some(ToolStatus::Running) | Some(ToolStatus::Succeeded) | None => "",
        })
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
        self.since_card.takes_in(Span::of(item));
        self.span.takes_in(Span::of(item));
    }

    fn observe(&mut self, status: Option<ToolStatus>) {
        match status {
            Some(ToolStatus::Running) => self.running = true,
            Some(ToolStatus::Failed) => {
                self.failed += 1;
                self.outcome = Some(ToolStatus::Failed);
            }
            Some(status) => {
                if status == ToolStatus::Denied {
                    self.denied += 1;
                }
                if self.outcome != Some(ToolStatus::Failed) {
                    self.outcome = Some(status);
                }
            }
            None => {}
        }
    }
}

/// Consecutive reasoning with nothing to read, inside a group's steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Pause {
    /// How many of the group's steps came before it.
    pub(super) after: usize,
    /// The first thought's index into the snapshot's item list, which names
    /// the pause's row.
    pub(super) index: usize,
    /// How long the agent thought, from the first thought's start to the
    /// last one's latest observation.
    pub(super) millis: u64,
}

impl Pause {
    /// Shorter thinking is the ordinary gap between steps; it stays as
    /// hidden as the thoughts themselves.
    pub(super) const SHOWN_FROM_MS: u64 = 2_000;
}

/// A run of hidden thoughts being gathered into a [`Pause`].
#[derive(Debug, Clone, Copy)]
struct Thinking {
    lies: Lies,
    after: usize,
    index: usize,
    span: Span,
}

/// Where a run of hidden thoughts is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lies {
    /// Among the steps of the group at this index.
    Among(usize),
    /// At the head of the strip of the work it led to, once the agent had
    /// handed control back.
    Ahead,
    /// Nowhere: it followed its group's prose, below the strip.
    Unlisted,
}

impl Thinking {
    fn pause(self) -> Option<Pause> {
        let millis = self.span.end?.saturating_sub(self.span.start?);
        (millis >= Pause::SHOWN_FROM_MS).then_some(Pause {
            after: self.after,
            index: self.index,
            millis,
        })
    }
}

/// When some work happened: its earliest start and latest observation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Span {
    start: Option<u64>,
    end: Option<u64>,
}

impl Span {
    fn of(item: &Item) -> Self {
        Self {
            start: item.started_at.or(item.at),
            end: item.at,
        }
    }

    fn takes_in(&mut self, other: Span) {
        self.start = match (self.start, other.start) {
            (Some(own), Some(other)) => Some(own.min(other)),
            (own, other) => own.or(other),
        };
        self.end = self.end.max(other.end);
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
/// `presented` reads what an item did to work with other agents, where a
/// message or a launch is drawn as a card among the prose rather than as a
/// step, and the titles that name a group of one tool call. It also says
/// where a message must not join the prompt before it, as when that prompt
/// failed and its notice, kept outside `items`, is drawn after its group.
pub(super) fn group<'c, T: Borrow<Item>>(
    items: &[T],
    asked_after: &HashSet<String>,
    presented: impl Presented<'c>,
) -> Vec<Turn> {
    let coordinated = |item: &Item| presented.coordinated(item);
    // A notice kept outside `items` is drawn after its prompt's group, so a
    // later message joining that group would put the old notice below it.
    let follows_retained_notice = |turn: &Turn, incoming: &Item| {
        turn.prompt
            .iter()
            .rev()
            .map(|&index| items[index].borrow())
            .find(|prompt| !matches!(prompt.content, ItemContent::Delivery { .. }))
            .is_some_and(|typed| presented.separates(typed, incoming))
    };
    let mut turns: Vec<Turn> = Vec::new();
    let mut state = Open::Closed;
    let mut newest_running = None;
    // Reasoning that draws nothing still took time. While a group is taking
    // work, it counts toward that group; once the agent has handed control
    // back, by speaking or asking, toward the group the work it led to opens.
    let mut unplaced: Option<Span> = None;
    // The hidden thoughts since the last drawn item, and the group whose
    // steps they fall among; a drawn item ends the run.
    let mut thinking: Option<Thinking> = None;
    // The group a hidden thought at the very end was counted toward.
    let mut thinking_in = None;
    for (index, item) in items.iter().map(Borrow::borrow).enumerate() {
        thinking_in = None;
        if !renders(&item.content) {
            if matches!(item.content, ItemContent::Text { thinking: true, .. }) {
                let open = turns.len().checked_sub(1).filter(|_| state == Open::Taking);
                let (lies, after) = match open {
                    Some(at) => {
                        let turn = &mut turns[at];
                        turn.spans(item);
                        // Thinking after the group's prose fell below its
                        // strip, so the strip neither lists it nor counts on
                        // through it: the work it leads to opens below.
                        let lies = if turn.prose.is_empty() {
                            thinking_in = Some(at);
                            Lies::Among(at)
                        } else {
                            Lies::Unlisted
                        };
                        (lies, turn.steps.len())
                    }
                    None => {
                        unplaced.get_or_insert_default().takes_in(Span::of(item));
                        (Lies::Ahead, 0)
                    }
                };
                thinking
                    .get_or_insert(Thinking {
                        lies,
                        after,
                        index,
                        span: Span::default(),
                    })
                    .span
                    .takes_in(Span::of(item));
            }
            // A question asked after it still ends the group, though the
            // item draws nothing, and ends the thinking listed in it: the
            // reader's answer was not.
            if asked_after.contains(item.id.as_str()) {
                state = Open::Closed;
                if let Some(Thinking {
                    lies: Lies::Among(at),
                    ..
                }) = thinking
                {
                    turns[at].pauses.extend(thinking.and_then(Thinking::pause));
                }
                thinking = None;
                // The agent is waiting on the reader now, not thinking.
                thinking_in = None;
            }
            continue;
        }
        let asked = asked_after.contains(item.id.as_str());
        // Thinking among a group's steps lies where it fell, whatever group
        // this item goes on to join.
        let ahead = thinking.take().and_then(|run| match run.lies {
            Lies::Among(at) => {
                turns[at].pauses.extend(run.pause());
                None
            }
            Lies::Ahead => run.pause(),
            Lies::Unlisted => None,
        });
        // A message from outside this stretch of work - the reader's own
        // prompt, a steering message, anything the harness delivers on their
        // behalf - is a new instruction, so the work that follows it is
        // attributed to it and not to whatever was running when it landed.
        if item.role == Role::User && !item.content.is_step() {
            // Thinking before the reader's next message was not work on it.
            unplaced = None;
            match turns.last_mut() {
                Some(turn)
                    if state == Open::Taking
                        && turn.awaiting_activity()
                        && !follows_retained_notice(turn, item) =>
                {
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
        // Messages one after another fold into a group of their own, as the
        // steps between two stretches of prose do. The first was drawn as a
        // card until the second came, so it leaves the prose for that
        // group, which it names; one that opened its group becomes that
        // group. Anything drawn between them keeps them apart, so the
        // timeline stays in the order it happened.
        let message = slot == Slot::Card && coordinated(item).is_some_and(Coordinated::is_message);
        if message
            && state == Open::Taking
            && let Some(turn) = turns.last_mut()
            && turn.prose.last().is_some_and(|placed| {
                placed.slot == Slot::Card
                    && coordinated(items[placed.index].borrow())
                        .is_some_and(Coordinated::is_message)
            })
        {
            let first = turn.prose.pop().expect("the message drawn last");
            let lone = turn.prompt.is_empty() && turn.steps.is_empty() && turn.prose.is_empty();
            if !lone {
                // The group's time ends where it did before the message;
                // the message, and what came after it, start the run's.
                let since = turn.since_card;
                turn.span = turn.before_card;
                turns.push(Turn::opened(items, first.index, false));
                turns.last_mut().expect("the run").span.takes_in(since);
            }
            let run = turns.last_mut().expect("a group is open");
            run.messages = true;
            run.steps.push(first);
            let sent = items[first.index].borrow();
            run.observe(
                sent.content
                    .status()
                    .zip(coordinated(sent))
                    .map(|(status, step)| step.reading.status(status)),
            );
        }
        // A message joins the messages before it; anything else drawn as a
        // step after them is other work, with a group of its own.
        let joins_messages = turns
            .last()
            .is_some_and(|turn| turn.messages && turn.prose.is_empty())
            && state == Open::Taking;
        let leaves_messages = slot == Slot::Step && joins_messages;
        let joins_messages = message && joins_messages;
        // Each rendered group is prompt, contiguous tools, then prose. A tool
        // arriving after any visible prose must start another group; otherwise
        // the view would move it ahead of that earlier reasoning or narration.
        // Pictures are the exception: an image is what a step produced, such
        // as a screenshot it read, so the next step joins the same group and
        // its picture joins the same gallery, beneath the rows that made them.
        let resumes_tools = slot == Slot::Step
            && turns.last().is_some_and(|turn| {
                turn.prose.iter().any(|placed| {
                    !matches!(
                        items[placed.index].borrow().content,
                        ItemContent::Image { .. }
                    )
                })
            });
        if state == Open::Closed
            || (state == Open::Spoke && !speech)
            || resumes_tools
            || leaves_messages
        {
            turns.push(Turn::opened(items, index, false));
        }
        // `turns` is non-empty: the branch above pushes when it would be, and
        // the states that skip it only occur once a group exists.
        let at = turns.len() - 1;
        let turn = turns.last_mut().expect("a group is open");
        // Thinking once the agent had spoken counts toward the work it led
        // to, and so opens that work's strip.
        if slot == Slot::Step
            && turn.steps.is_empty()
            && let Some(pause) = ahead
        {
            turn.pauses.push(pause);
        }
        let status = item.content.status();
        match coordinated(item) {
            // A message among messages counts toward their group. Its row
            // says how it stands in words, as a card does, so it leaves the
            // activity indicator to running steps.
            Some(step) if joins_messages => {
                turn.observe(status.map(|status| step.reading.status(status)));
            }
            // A card shows how it went itself; the strip summarizes only the
            // steps it collapses, so a refused message beside a lookup does
            // not mark the lookup failed. Still waiting on one keeps the
            // group working, which is counted once the cards have settled
            // where they are drawn.
            Some(_) if slot == Slot::Card => {}
            coordinated => {
                let status = match coordinated {
                    Some(step) => status.map(|status| step.reading.status(status)),
                    None => status,
                };
                if slot == Slot::Step && status == Some(ToolStatus::Running) {
                    newest_running = Some(at);
                }
                turn.observe(status);
            }
        }
        let before = turn.span;
        turn.spans(item);
        let thought = unplaced.take();
        if let Some(thought) = thought {
            turn.span.takes_in(thought);
        }
        if slot == Slot::Card && !joins_messages {
            // A message drawn as a card may yet leave for a run of its own,
            // taking its time and the thinking that led to it.
            turn.before_card = before;
            turn.since_card = Span::of(item);
            turn.since_card.takes_in(thought.unwrap_or_default());
        }
        match slot {
            Slot::Step => turn.steps.push(Placed { index, slot }),
            Slot::Card if joins_messages => turn.steps.push(Placed { index, slot }),
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
    // Thinking that is still going on keeps its group's clock running,
    // though it draws nothing and its steps have finished.
    if presented.working()
        && let Some(turn) = thinking_in.and_then(|at| turns.get_mut(at))
    {
        turn.running = true;
        turn.thinking = true;
    }
    // Thinking at the end of a group that is still going on is what the
    // working indicator shows; once it is over, it closes the strip.
    if let Some(run) = thinking
        && let Lies::Among(at) = run.lies
        && !(presented.working() && thinking_in.is_some())
    {
        turns[at].pauses.extend(run.pause());
    }
    for turn in &mut turns {
        // A card still waiting on its answer keeps its group working.
        turn.running |= turn.prose.iter().any(|placed| {
            placed.slot == Slot::Card
                && items[placed.index].borrow().content.status() == Some(ToolStatus::Running)
        });
        turn.describe(items, &presented);
    }
    if let Some(turn) = newest_running.and_then(|at| turns.get_mut(at)) {
        turn.newest_running = true;
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
/// answer with nothing in it is also the transient state of a message that is
/// about to stream. Reasoning with nothing to read, which is most of what
/// Claude sends, is working time rather than a row: grouping counts its
/// time without placing it.
pub(super) fn renders(content: &ItemContent) -> bool {
    match content {
        ItemContent::Text { thinking: true, .. } => !super::reasoning::contentless(content),
        ItemContent::Text { text, .. } => !text.trim().is_empty(),
        _ => true,
    }
}

/// What grouping reads from items' presentations: what an item did to work
/// with other agents, and what a plain tool call's row says it did. Both
/// are formed once per change to an item and kept with its presentation.
///
/// A closure over coordination alone is one, with no titles kept: a group
/// that is a single tool call then reads that call's input to name itself.
pub(super) trait Presented<'c> {
    fn coordinated(&self, item: &Item) -> Option<&'c Coordinated>;
    fn titled(&self, _item: &Item) -> Option<&'c Title> {
        None
    }
    /// Whether `incoming` opens a group of its own rather than joining the
    /// prompt whose latest typed message is `typed`, which has no activity
    /// yet. Parts of one message still share their prompt.
    fn separates(&self, _typed: &Item, _incoming: &Item) -> bool {
        false
    }
    /// Whether the agent is still at work on the newest of these items, so a
    /// thought with nothing to read at the end is thinking still going on.
    fn working(&self) -> bool {
        false
    }
}

impl<'c, F: Fn(&Item) -> Option<&'c Coordinated>> Presented<'c> for F {
    fn coordinated(&self, item: &Item) -> Option<&'c Coordinated> {
        self(item)
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
pub(super) fn step_label(content: &ItemContent) -> Cow<'_, str> {
    Cow::Borrowed(match content {
        ItemContent::Text { .. } => "Thinking",
        ItemContent::Command {
            label,
            command,
            action,
            ..
        } => {
            if *action == chat::CommandAction::Run {
                // What ran is the command. A provider's description of it
                // ("Run tests for module 295") reads as a second verb after
                // "Ran", and it is not what a reader scanning for the command
                // that failed is looking for; the open step shows it.
                first_line(if command.trim().is_empty() {
                    label
                } else {
                    command
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
        ItemContent::Changes { changes, status } => match changes.as_slice() {
            // An edit whose input is still arriving: what it changes, and so
            // whether it adds or edits a file, is not known yet.
            [] if *status == ToolStatus::Running => "Preparing an edit",
            // One that ended without describing its change still edited,
            // or tried to edit, something.
            [] => "a file",
            [single] => single.path.as_str(),
            changes => return Cow::Owned(format!("{} files", changes.len())),
        },
        ItemContent::WebSearch { query, .. } => query.as_str(),
        // Its input is not read here: a row's title is formed once, with
        // its presentation (see `step_title`).
        ItemContent::Tool { name, .. } => return Cow::Owned(Title::unread(name).label),
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
    })
}

/// What a provider said a shell command was for, when it said something the
/// command does not already say. The row names the command, so this is the
/// secondary line its detail and accessible name carry.
pub(super) fn command_description(content: &ItemContent) -> Option<&str> {
    match content {
        ItemContent::Command {
            label,
            command,
            action: chat::CommandAction::Run,
            ..
        } => {
            let label = label.trim();
            (!label.is_empty() && !command.trim().is_empty() && label != command.trim())
                .then_some(label)
        }
        _ => None,
    }
}

/// A description the closed row keeps beside a command whose first line is
/// not the whole of it: a heredoc, a script, or a continued line.
#[cfg(test)]
pub(super) fn script_description(content: &ItemContent) -> Option<&str> {
    let ItemContent::Command { command, .. } = content else {
        return None;
    };
    command_description(content).filter(|_| command.trim().contains('\n'))
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

/// A step's verb and label together. A plain tool call's are its `title`,
/// formed once from its input when the call changed; nothing here reads the
/// input, so a row drawn before its title is formed names its tool alone.
pub(super) fn step_title<'a>(
    content: &'a ItemContent,
    title: Option<&'a Title>,
) -> (&'static str, Cow<'a, str>) {
    match (content, title) {
        (ItemContent::Tool { .. }, Some(title)) => (title.verb, Cow::Borrowed(&title.label)),
        (content, _) => (step_verb(content), step_label(content)),
    }
}

/// Whether a step's label is the path of the one file it acted on, which a
/// row shows by its last components; see [`super::disclosure::Headline`].
pub(super) fn names_path(content: &ItemContent, title: Option<&Title>) -> bool {
    match content {
        ItemContent::Changes { changes, .. } => changes.len() == 1,
        // A read is labelled with the file it read, whether that is also
        // the command (a built-in read), beside a shell command (Codex), or
        // stored as "Read <file>"; one with no label shows its command.
        ItemContent::Command {
            action: chat::CommandAction::Read,
            label,
            ..
        } => !label.is_empty(),
        ItemContent::Tool { .. } => title.is_some_and(|title| title.path),
        _ => false,
    }
}

/// A path as a step row shows it: relative to the working directory when it
/// is inside it, as the agent itself would name it, and otherwise from the
/// home directory or the root, so it still reads as lying outside.
///
/// An absolute path is read as written, without resolving links, but with
/// `.`, `..` and repeated separators taken out, so `/work/../etc/x` is not
/// shown as though it lay in `/work`.
pub(super) fn shown_path<'a>(path: &'a str, working_directory: &std::path::Path) -> Cow<'a, str> {
    if !path.starts_with('/') {
        return Cow::Borrowed(path);
    }
    let path = lexical(std::path::Path::new(path));
    let root = lexical(working_directory);
    // Relative to the root itself, every path would lose the slash that
    // says where it lies.
    let inside = root.parent().is_some() && working_directory.is_absolute();
    match path.strip_prefix(&root) {
        Ok(relative) if inside && !relative.as_os_str().is_empty() => {
            Cow::Owned(relative.to_string_lossy().into_owned())
        }
        _ => Cow::Owned(crate::app::display_path(&path).into_owned()),
    }
}

/// `path` with `.`, `..` and repeated separators taken out, as written.
fn lexical(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut normal = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normal.pop() {
                    normal.push(component);
                }
            }
            component => normal.push(component),
        }
    }
    normal
}

/// The verb a step is introduced with, kept separate from its target so the two
/// can be typeset differently.
pub(super) fn step_verb(content: &ItemContent) -> &'static str {
    match content {
        ItemContent::Text { .. } => "",
        // A step says what it is doing while it runs, and what it tried
        // when it did not happen; its status says why.
        ItemContent::Command { action, status, .. } => match action {
            chat::CommandAction::Read => verb("Reading", "Read", "Tried to read"),
            chat::CommandAction::Search => verb("Searching", "Searched", "Tried to search"),
            chat::CommandAction::List => verb("Listing", "Listed", "Tried to list"),
            // A command that failed or was stopped did run, and its exit
            // code says how it ended; only a refused one never ran.
            chat::CommandAction::Run if *status == ToolStatus::Denied => {
                return "Tried to run";
            }
            chat::CommandAction::Run => verb("Running", "Ran", "Ran"),
        }
        .said(*status),
        ItemContent::Changes { changes, status } => match changes.as_slice() {
            // An edit whose input is still arriving names no verb yet.
            [] if *status == ToolStatus::Running => return "",
            [single] => match single.kind {
                chat::ChangeKind::Added => verb("Adding", "Added", "Tried to add"),
                chat::ChangeKind::Deleted => verb("Deleting", "Deleted", "Tried to delete"),
                chat::ChangeKind::Updated => verb("Editing", "Edited", "Tried to edit"),
                chat::ChangeKind::Renamed { .. } => verb("Renaming", "Renamed", "Tried to rename"),
            },
            _ => verb("Editing", "Edited", "Tried to edit"),
        }
        .said(*status),
        // A search reported before its query, as Codex can, says only that
        // it searched rather than leaving "for" without what followed it.
        ItemContent::WebSearch { query, .. } if query.trim().is_empty() => "Searched the web",
        ItemContent::WebSearch { .. } => "Searched the web for",
        ItemContent::Tool { .. } => "",
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
    presented: &impl Presented<'c>,
    counts_steps: bool,
) -> Option<String> {
    let mut edited = std::collections::BTreeSet::new();
    // Edits are named by the files they changed, which is not how many
    // edits there were: a failed edit retried on the same file is two.
    let mut edits = 0;
    let (mut ran, mut read, mut searched, mut listed, mut web, mut used, mut viewed) =
        (0, 0, 0, 0, 0, 0, 0);
    // An edit whose input is still arriving names no file yet, and a
    // summary that left it out would hide a step that is still working.
    let mut preparing = 0;
    let mut coordinating = 0;
    // A skill is read, and context is added, rather than a tool used.
    let (mut skills, mut context) = (0, 0);
    for placed in steps {
        let item = items.get(placed.index)?.borrow();
        if presented
            .coordinated(item)
            .is_some_and(|coordinated| coordinated.reading.involves_agents())
        {
            coordinating += 1;
            continue;
        }
        match &item.content {
            ItemContent::Changes { changes, status }
                if changes.is_empty() && *status == ToolStatus::Running =>
            {
                preparing += 1
            }
            ItemContent::Changes { changes, .. } => {
                edits += 1;
                edited.extend(changes.iter().map(|change| change.path.as_str()));
            }
            ItemContent::Command { action, .. } => match action {
                chat::CommandAction::Run => ran += 1,
                chat::CommandAction::Read => read += 1,
                chat::CommandAction::Search => searched += 1,
                chat::CommandAction::List => listed += 1,
            },
            ItemContent::WebSearch { .. } => web += 1,
            ItemContent::Tool { name, .. } => match name.as_str() {
                "Skill" => skills += 1,
                "Claude context" | "Hook context" => context += 1,
                _ => used += 1,
            },
            ItemContent::Image { .. } => viewed += 1,
            ItemContent::Text { .. } | ItemContent::Delivery { .. } => {}
        }
    }
    let counted = |count: usize, one: &str, many: &str| match count {
        1 => format!("a {one}"),
        count => format!("{count} {many}"),
    };
    let mut parts = Vec::new();
    // Where the line goes on to count failed or refused steps, it counts
    // edits too, so "1 failed" is one of the edits it names and not one of
    // the files: "Made 2 edits to a file · 1 failed".
    if counts_steps && edits != edited.len() {
        let files = counted(edited.len().max(1), "file", "files");
        let edits = match edits {
            1 => "an edit".to_owned(),
            edits => format!("{edits} edits"),
        };
        parts.push(format!("made {edits} to {files}"));
    } else if !edited.is_empty() {
        parts.push(format!("edited {}", counted(edited.len(), "file", "files")));
    }
    if preparing > 0 {
        parts.push(match preparing {
            1 => "preparing an edit".to_owned(),
            count => format!("preparing {count} edits"),
        });
    }
    if ran > 0 {
        parts.push(format!("ran {}", counted(ran, "command", "commands")));
    }
    match (read, skills) {
        (0, 0) => {}
        (read, 0) => parts.push(format!("read {}", counted(read, "file", "files"))),
        (0, skills) => parts.push(format!("read {}", counted(skills, "skill", "skills"))),
        (read, skills) => parts.push(format!(
            "read {} and {}",
            counted(read, "file", "files"),
            counted(skills, "skill", "skills")
        )),
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
    if used > 0 {
        parts.push(format!("used {}", counted(used, "tool", "tools")));
    }
    if viewed > 0 {
        parts.push(format!("viewed {}", counted(viewed, "image", "images")));
    }
    if context > 0 {
        parts.push(match context {
            1 => "added context".to_owned(),
            count => format!("added context {count} times"),
        });
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
    first_line_of(command)
}

/// The longest summary a group of one step keeps. The strip draws it on one
/// line and cuts it to the pane; this only bounds the text shaped for that,
/// as a one-line script can run to thousands of characters.
const SUMMARY_LIMIT: usize = 240;

/// A single step's summary, on one line and bounded for the strip.
/// The fade handles visual overflow; no ellipsis is injected into the text.
/// Full command text remains available in the open step.
pub(super) fn headline(summary: String) -> String {
    let line = first_line_of(&summary);
    if line.len() == summary.len() && line.chars().count() <= SUMMARY_LIMIT {
        return summary;
    }
    line.chars().take(SUMMARY_LIMIT).collect()
}

fn first_line_of(command: &str) -> &str {
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
        let ItemContent::Text { thinking, .. } = &mut appended.content else {
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
            Some(ToolStatus::Running.into()),
            "a live group reports that it is live; the step that failed says so on its own row"
        );
        assert_eq!(
            turns[0].summary, "Ran 2 commands · working",
            "still working outranks how the finished steps went"
        );
    }

    #[test]
    fn a_collapsed_group_keeps_earlier_failures_visible_after_later_success() {
        for items in [
            vec![
                text("u1", Role::User, Phase::Final, false),
                command("c1", ToolStatus::Failed),
                command("c2", ToolStatus::Succeeded),
            ],
            vec![
                text("u1", Role::User, Phase::Final, false),
                command("c1", ToolStatus::Succeeded),
                command("c2", ToolStatus::Failed),
            ],
        ] {
            let turns = group(&items, &unasked(), plain);
            assert_eq!(
                turns[0].outcome,
                Some(ToolStatus::Failed),
                "later success must not hide a failed command in a collapsed group"
            );
            assert_eq!(turns[0].summary, "Ran 2 commands · 1 failed");
            assert_eq!(
                turns[0].glyph(),
                Some(Status::PartlyFailed),
                "one failure beside a success is marked as partial, not as the whole group failing"
            );
        }
    }

    #[test]
    fn a_collapsed_group_counts_how_many_of_its_steps_failed() {
        let summary_and_glyph = |statuses: &[ToolStatus]| {
            let mut items = vec![text("u1", Role::User, Phase::Final, false)];
            items.extend(
                statuses
                    .iter()
                    .enumerate()
                    .map(|(index, status)| command(&format!("c{index}"), *status)),
            );
            let turns = group(&items, &unasked(), plain);
            (turns[0].summary.clone(), turns[0].glyph())
        };
        use ToolStatus::{Failed, Succeeded};
        assert_eq!(
            summary_and_glyph(&[Failed, Succeeded, Failed]),
            (
                "Ran 3 commands · 2 failed".to_owned(),
                Some(Status::PartlyFailed)
            )
        );
        assert_eq!(
            summary_and_glyph(&[Failed, Failed]),
            (
                "Ran 2 commands · all failed".to_owned(),
                Some(Failed.into())
            ),
            "where every step failed the group keeps the full error mark"
        );
        assert_eq!(
            summary_and_glyph(&[Failed, Failed, Failed]),
            (
                "Ran 3 commands · all failed".to_owned(),
                Some(Failed.into())
            )
        );
        assert_eq!(
            summary_and_glyph(&[Succeeded, Succeeded]),
            ("Ran 2 commands".to_owned(), Some(Succeeded.into())),
            "a group in which everything worked is unchanged"
        );
        assert_eq!(
            summary_and_glyph(&[Failed]),
            ("Ran cargo check · failed".to_owned(), Some(Failed.into())),
            "a lone step keeps its own wording"
        );
    }

    #[test]
    fn failures_are_counted_alike_across_every_kind_of_step() {
        let edit = |id: &str, path: &str, status: ToolStatus| Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Changes {
                changes: vec![FileChange {
                    lines: crate::chat::LineOrigin::File,
                    path: path.into(),
                    kind: ChangeKind::Updated,
                    diff: String::new(),
                }],
                status,
            },
            started_at: None,
            at: None,
        };
        let read = |id: &str, status: ToolStatus| {
            let mut item = command(id, status);
            if let ItemContent::Command { action, label, .. } = &mut item.content {
                *action = CommandAction::Read;
                *label = "src/app.rs".into();
            }
            item
        };
        let tool = |id: &str, status: ToolStatus| Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: "mcp__linear__get_issue".into(),
                input: String::new(),
                output: String::new(),
                status,
            },
            started_at: None,
            at: None,
        };
        let summary_and_glyph = |steps: Vec<Item>| {
            let mut items = vec![text("u1", Role::User, Phase::Final, false)];
            items.extend(steps);
            let turns = group(&items, &unasked(), plain);
            (turns[0].summary.clone(), turns[0].glyph())
        };
        use ToolStatus::{Failed, Succeeded};
        assert_eq!(
            summary_and_glyph(vec![
                edit("e1", "a.rs", Failed),
                edit("e2", "b.rs", Succeeded),
            ]),
            (
                "Edited 2 files · 1 failed".to_owned(),
                Some(Status::PartlyFailed)
            )
        );
        // A failed edit retried on the same file is two edits to one file,
        // so the count of failures lines up with what the line counted.
        assert_eq!(
            summary_and_glyph(vec![
                edit("e1", "a.rs", Failed),
                edit("e2", "a.rs", Succeeded),
            ]),
            (
                "Made 2 edits to a file · 1 failed".to_owned(),
                Some(Status::PartlyFailed)
            )
        );
        assert_eq!(
            summary_and_glyph(vec![
                edit("e1", "a.rs", Succeeded),
                edit("e2", "a.rs", Succeeded),
            ])
            .0,
            "Edited a file",
            "with nothing to count against them, edits are named by their files"
        );
        let mut patch = edit("e1", "a.rs", Failed);
        if let ItemContent::Changes { changes, .. } = &mut patch.content {
            let mut other = changes[0].clone();
            other.path = "b.rs".into();
            changes.push(other);
        }
        assert_eq!(
            summary_and_glyph(vec![patch, command("c1", Succeeded)]).0,
            "Made an edit to 2 files, ran a command · 1 failed"
        );
        assert_eq!(
            summary_and_glyph(vec![read("r1", Failed), read("r2", Failed)]),
            ("Read 2 files · all failed".to_owned(), Some(Failed.into()))
        );
        assert_eq!(
            summary_and_glyph(vec![tool("t1", Succeeded), tool("t2", Failed)]),
            (
                "Used 2 tools · 1 failed".to_owned(),
                Some(Status::PartlyFailed)
            )
        );
        assert_eq!(
            summary_and_glyph(vec![
                edit("e1", "a.rs", Succeeded),
                command("c1", Failed),
                read("r1", Succeeded),
                tool("t1", Failed),
            ]),
            (
                "Edited a file, ran a command, read a file, used a tool · 2 failed".to_owned(),
                Some(Status::PartlyFailed)
            ),
            "mixed kinds of step are counted together, after the line that names them"
        );
        assert_eq!(
            summary_and_glyph(vec![edit("e1", "a.rs", Failed), command("c1", Failed)]),
            (
                "Edited a file, ran a command · all failed".to_owned(),
                Some(Failed.into())
            )
        );
    }

    #[test]
    fn a_collapsed_group_counts_refusals_and_keeps_one_before_later_success() {
        let summary_and_glyph = |statuses: &[ToolStatus]| {
            let mut items = vec![text("u1", Role::User, Phase::Final, false)];
            items.extend(
                statuses
                    .iter()
                    .enumerate()
                    .map(|(index, status)| command(&format!("c{index}"), *status)),
            );
            let turns = group(&items, &unasked(), plain);
            (turns[0].summary.clone(), turns[0].glyph())
        };
        let summary = |statuses: &[ToolStatus]| summary_and_glyph(statuses).0;
        use ToolStatus::{Denied, Failed, Succeeded};
        for statuses in [[Denied, Succeeded], [Succeeded, Denied]] {
            assert_eq!(
                summary_and_glyph(&statuses),
                ("Ran 2 commands · 1 denied".to_owned(), Some(Denied.into())),
                "later success must not hide a refused step, in words or in its mark"
            );
        }
        assert_eq!(summary(&[Denied, Denied]), "Ran 2 commands · all denied");
        assert_eq!(
            summary_and_glyph(&[Failed, Denied, Succeeded]),
            (
                "Ran 3 commands · 1 failed, 1 denied".to_owned(),
                Some(Status::PartlyFailed)
            )
        );
        assert_eq!(
            summary_and_glyph(&[Failed, Denied]),
            (
                "Ran 2 commands · 1 failed, 1 denied".to_owned(),
                Some(Failed.into())
            ),
            "where nothing went through, the group keeps the full error mark"
        );
        assert_eq!(summary(&[Denied]), "Tried to run cargo check · denied");
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
            Some(ToolStatus::Running.into()),
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
        assert_eq!(finished[0].duration_label(90_000).as_deref(), Some("5s"));
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

    /// What the Claude adapter makes of a thinking block. Most arrive as
    /// `{"type":"thinking","thinking":"","signature":"…"}`, and a redacted
    /// one is given a placeholder; Codex can send an empty summary part.
    fn thought(id: &str, body: &str, started_at: u64, at: u64) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: body.into(),
                thinking: true,
                phase: Phase::Commentary,
            },
            started_at: Some(started_at),
            at: Some(at),
        }
    }

    fn timed(mut item: Item, started_at: u64, at: u64) -> Item {
        item.started_at = Some(started_at);
        item.at = Some(at);
        item
    }

    #[test]
    fn reasoning_with_nothing_to_read_neither_draws_nor_splits_its_commands() {
        for body in ["", "  \n", "[Redacted thinking]", "** **"] {
            let items = vec![
                text("prompt", Role::User, Phase::Final, false),
                thought("t1", body, 1_000, 2_000),
                timed(command("c1", ToolStatus::Succeeded), 2_000, 3_000),
                thought("t2", body, 3_000, 13_000),
                timed(command("c2", ToolStatus::Succeeded), 13_000, 14_000),
                thought("t3", body, 14_000, 14_200),
                timed(command("c3", ToolStatus::Succeeded), 14_200, 15_000),
                thought("t4", body, 15_000, 23_000),
                timed(
                    text("answer", Role::Assistant, Phase::Final, false),
                    23_000,
                    24_000,
                ),
            ];
            let turns = group(&items, &unasked(), plain);
            assert_eq!(turns.len(), 1, "{body:?}");
            assert_eq!(steps(&turns[0]), [2, 4, 6], "one group of commands");
            assert_eq!(slots(&turns[0]), [(8, Slot::Prose)], "no thought rows");
            assert_eq!(turns[0].summary, "Ran 3 commands");
            // The first thought began the work and the last one ran until
            // the answer: both still count.
            assert_eq!(turns[0].working_for(u64::MAX), Some(23_000));
        }
    }

    #[test]
    fn reasoning_with_a_summary_keeps_its_row_among_contentless_thoughts() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            thought("empty", "", 1_000, 2_000),
            timed(command("c1", ToolStatus::Succeeded), 2_000, 3_000),
            thought(
                "summarized",
                "**Checking the build**\n\nIt fails.",
                3_000,
                5_000,
            ),
            timed(command("c2", ToolStatus::Succeeded), 5_000, 6_000),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert_eq!(steps(&turns[0]), [2]);
        assert_eq!(slots(&turns[0]), [(3, Slot::Thinking)]);
        assert_eq!(steps(&turns[1]), [4]);
    }

    #[test]
    fn a_question_after_contentless_reasoning_still_ends_its_group() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            thought("t", "", 2_000, 3_000),
            timed(command("c2", ToolStatus::Succeeded), 5_000, 6_000),
        ];
        let asked = HashSet::from(["t".to_owned()]);
        let turns = group(&items, &asked, plain);
        assert_eq!(turns.len(), 2, "the work after the answer is its own group");
        assert_eq!(steps(&turns[0]), [1]);
        assert_eq!(steps(&turns[1]), [3]);
        assert_eq!(turns[0].working_for(u64::MAX), Some(2_000));
    }

    #[test]
    fn contentless_reasoning_after_an_answer_counts_toward_the_work_it_led_to() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(
                text("said", Role::Assistant, Phase::Final, false),
                1_000,
                2_000,
            ),
            thought("t", "", 2_000, 9_000),
            timed(command("c", ToolStatus::Succeeded), 9_000, 10_000),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert_eq!(steps(&turns[1]), [3]);
        assert!(slots(&turns[1]).is_empty());
        assert_eq!(turns[1].working_for(u64::MAX), Some(8_000));
        // While the agent is still thinking, nothing new is drawn: the
        // working indicator says so.
        let thinking = group(&items[..3], &unasked(), plain);
        assert_eq!(thinking.len(), 1);
        assert_eq!(slots(&thinking[0]), [(1, Slot::Prose)]);
        assert!(group(&[thought("alone", "", 0, 1)], &unasked(), plain).is_empty());
    }

    /// Grouping for an agent still at work on the newest item.
    struct Working;

    impl<'c> Presented<'c> for Working {
        fn coordinated(&self, _item: &Item) -> Option<&'c Coordinated> {
            None
        }
        fn working(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_group_keeps_counting_while_the_agent_thinks_after_its_steps() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(command("c2", ToolStatus::Succeeded), 2_000, 3_000),
            thought("t", "", 3_000, 4_000),
        ];
        let turns = group(&items, &unasked(), Working);
        assert!(
            turns[0].running,
            "the hidden thought is work still going on"
        );
        assert_eq!(turns[0].working_for(20_000), Some(19_000));
        assert_eq!(turns[0].summary, "Ran 2 commands · working");
        // Once the agent stops, the group ends where its thinking did.
        let turns = group(&items, &unasked(), plain);
        assert!(!turns[0].running);
        assert_eq!(turns[0].working_for(20_000), Some(3_000));
        // A thought that is not the newest item has finished.
        let mut later = items.clone();
        later.push(timed(command("c3", ToolStatus::Succeeded), 4_000, 5_000));
        assert!(!group(&later, &unasked(), Working)[0].running);
    }

    #[test]
    fn a_lone_step_followed_by_thinking_keeps_its_group_thinking() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            thought("t", "", 2_000, 3_000),
        ];
        let turns = group(&items, &unasked(), Working);
        assert!(turns[0].thinking);
        assert!(
            turns[0].summary.ends_with(" · working"),
            "{}",
            turns[0].summary
        );
        assert!(!group(&items, &unasked(), plain)[0].thinking);
    }

    #[test]
    fn thinking_after_the_agent_spoke_does_not_restart_its_group() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(
                text("said", Role::Assistant, Phase::Final, false),
                2_000,
                3_000,
            ),
            thought("t", "", 3_000, 4_000),
        ];
        let turns = group(&items, &unasked(), Working);
        assert_eq!(turns.len(), 1);
        assert!(!turns[0].running, "the thought belongs to work not begun");
    }

    fn pauses(turn: &Turn) -> Vec<(usize, usize, u64)> {
        turn.pauses
            .iter()
            .map(|pause| (pause.after, pause.index, pause.millis))
            .collect()
    }

    #[test]
    fn an_open_strip_lists_long_hidden_thinking_where_it_fell() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            thought("t0", "", 500, 1_000),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            // A run of two thoughts is one stretch of thinking.
            thought("t1", "", 2_000, 9_000),
            thought("t2", "[Redacted thinking]", 9_000, 19_000),
            timed(command("c2", ToolStatus::Succeeded), 19_000, 20_000),
            // The ordinary pause between steps stays hidden.
            thought("t3", "", 20_000, 21_500),
            timed(command("c3", ToolStatus::Succeeded), 21_500, 22_000),
            thought("t4", "", 22_000, 25_000),
            timed(
                text("answer", Role::Assistant, Phase::Final, false),
                25_000,
                26_000,
            ),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 1);
        assert_eq!(steps(&turns[0]), [2, 5, 7]);
        assert_eq!(pauses(&turns[0]), [(1, 3, 17_000), (3, 8, 3_000)]);
        assert_eq!(
            turns[0].summary, "Ran 3 commands",
            "the header is unchanged"
        );
        assert_eq!(turns[0].working_for(u64::MAX), Some(25_500));
    }

    #[test]
    fn thinking_still_going_on_is_not_listed_until_it_ends() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(command("c2", ToolStatus::Succeeded), 2_000, 3_000),
            thought("t", "", 3_000, 9_000),
        ];
        assert!(pauses(&group(&items, &unasked(), Working)[0]).is_empty());
        assert_eq!(
            pauses(&group(&items, &unasked(), plain)[0]),
            [(2, 3, 6_000)]
        );
    }

    #[test]
    fn thinking_after_an_answer_opens_the_strip_of_the_work_it_led_to() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(
                text("said", Role::Assistant, Phase::Final, false),
                2_000,
                3_000,
            ),
            thought("t", "", 3_000, 9_000),
            timed(command("c2", ToolStatus::Succeeded), 9_000, 10_000),
            timed(command("c3", ToolStatus::Succeeded), 10_000, 11_000),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert!(pauses(&turns[0]).is_empty());
        assert_eq!(pauses(&turns[1]), [(0, 3, 6_000)]);
        assert_eq!(turns[1].working_for(u64::MAX), Some(8_000));
    }

    #[test]
    fn a_question_ends_the_thinking_listed_before_it() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(command("c2", ToolStatus::Succeeded), 2_000, 3_000),
            thought("before", "", 3_000, 6_000),
            // Answered a minute later.
            thought("after", "", 60_000, 65_000),
            timed(command("c3", ToolStatus::Succeeded), 65_000, 66_000),
            timed(command("c4", ToolStatus::Succeeded), 66_000, 67_000),
        ];
        let asked = HashSet::from(["before".to_owned()]);
        let turns = group(&items, &asked, plain);
        assert_eq!(turns.len(), 2);
        assert_eq!(pauses(&turns[0]), [(2, 3, 3_000)]);
        assert_eq!(pauses(&turns[1]), [(0, 4, 5_000)]);
        // Thinking after an answer ends at a question too, whether it would
        // have led the next strip or followed the group's prose.
        for (said, phase) in [("answer", Phase::Final), ("progress", Phase::Commentary)] {
            let mut items = items.clone();
            items.insert(
                3,
                timed(text(said, Role::Assistant, phase, false), 3_000, 3_000),
            );
            let turns = group(&items, &asked, plain);
            let last = turns.last().expect("the work after the question");
            assert_eq!(steps(last), [6, 7], "{said}");
            assert_eq!(pauses(last), [(0, 5, 5_000)], "{said}");
        }
    }

    #[test]
    fn a_group_waiting_on_a_question_after_its_thinking_stops_counting() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(command("c2", ToolStatus::Succeeded), 2_000, 3_000),
            thought("before", "", 3_000, 6_000),
        ];
        let asked = HashSet::from(["before".to_owned()]);
        let turns = group(&items, &asked, Working);
        assert_eq!(turns.len(), 1);
        assert!(!turns[0].running);
        assert!(!turns[0].thinking);
        assert_eq!(pauses(&turns[0]), [(2, 3, 3_000)]);
    }

    #[test]
    fn thinking_after_a_groups_narration_leaves_its_strip_finished() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(
                text("progress", Role::Assistant, Phase::Commentary, false),
                2_000,
                3_000,
            ),
            thought("t", "", 3_000, 9_000),
        ];
        let turns = group(&items, &unasked(), Working);
        assert_eq!(turns.len(), 1);
        assert!(!turns[0].running, "the thought fell below the strip");
        assert!(!turns[0].thinking);
        assert!(turns[0].pauses.is_empty());
    }

    #[test]
    fn thinking_below_a_strip_is_not_listed_in_it() {
        let items = vec![
            text("prompt", Role::User, Phase::Final, false),
            timed(command("c1", ToolStatus::Succeeded), 1_000, 2_000),
            timed(command("c2", ToolStatus::Succeeded), 2_000, 3_000),
            timed(
                text("progress", Role::Assistant, Phase::Commentary, false),
                3_000,
                4_000,
            ),
            thought("t", "", 4_000, 9_000),
            timed(command("c3", ToolStatus::Succeeded), 9_000, 10_000),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|turn| turn.pauses.is_empty()));
    }

    #[test]
    fn a_thought_gaining_its_first_readable_line_changes_structure() {
        use std::sync::Arc;
        let before = vec![Arc::new(thought("t", "", 0, 1))];
        let after = vec![Arc::new(thought("t", "**Planning**", 0, 1))];
        assert!(!same_structure(&before, &after));
        let more = vec![Arc::new(thought("t", "**Planning** the fix", 0, 1))];
        assert!(same_structure(&after, &more));
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
            // A shell command's label is the provider's own description,
            // which would repeat the verb; the row names the command.
            (
                CommandAction::Run,
                "Read the logs",
                "Ran sed -n 1,20p src/app.rs",
            ),
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
            // The strip's one line drops a label's trailing space.
            assert_eq!(group(&items, &unasked(), plain)[0].summary, row.trim_end());
        }
    }

    /// A shell row names what ran; the provider's description of it is kept
    /// for the detail rather than read as a second verb.
    #[test]
    fn a_described_shell_command_is_named_by_its_command() {
        let run = |label: &str, command: &str| ItemContent::Command {
            label: label.into(),
            command: command.into(),
            working_directory: String::new(),
            action: CommandAction::Run,
            status: ToolStatus::Failed,
            exit_code: Some(101),
            duration_ms: None,
            output: String::new(),
        };
        let described = run("Run tests for module 295", "cargo test --lib module_295");
        assert_eq!(
            format!("{} {}", step_verb(&described), step_label(&described)),
            "Ran cargo test --lib module_295"
        );
        assert_eq!(
            command_description(&described),
            Some("Run tests for module 295")
        );
        // Its first line says what a one-line command does; a script's does not.
        assert_eq!(script_description(&described), None);
        let script = run(
            "Count failures by module",
            "python3 - <<'EOF'\nprint(1)\nEOF",
        );
        assert_eq!(step_label(&script), "python3 - <<'EOF'");
        assert_eq!(
            script_description(&script),
            Some("Count failures by module")
        );
        assert_eq!(script_description(&run("", "python3 - <<'EOF'\nEOF")), None);
        // A label that is the command, or no label, adds nothing.
        assert_eq!(command_description(&run("cargo test", "cargo test")), None);
        assert_eq!(command_description(&run("", "cargo test")), None);
        // Without a command the description is the row.
        let bare = run("Run tests", "");
        assert_eq!(step_label(&bare), "Run tests");
        assert_eq!(command_description(&bare), None);
        let items = [Item {
            id: "step".into(),
            role: Role::Assistant,
            content: described,
            started_at: None,
            at: None,
        }];
        assert_eq!(
            group(&items, &unasked(), plain)[0].summary,
            "Ran cargo test --lib module_295 · failed"
        );
    }

    #[test]
    fn a_single_file_change_names_the_path_and_its_kind() {
        let content = ItemContent::Changes {
            changes: vec![FileChange {
                lines: crate::chat::LineOrigin::File,
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

    /// An edit whose input is still arriving has not said whether it adds or
    /// edits a file, so its row names neither rather than guessing one and
    /// changing its mind.
    #[test]
    fn an_edit_not_yet_described_does_not_guess_its_verb() {
        let content = ItemContent::Changes {
            changes: Vec::new(),
            status: ToolStatus::Running,
        };
        assert_eq!(step_verb(&content), "");
        assert_eq!(step_label(&content), "Preparing an edit");
        // One that ended without describing a change was never being prepared.
        for status in [ToolStatus::Succeeded, ToolStatus::Failed] {
            let content = ItemContent::Changes {
                changes: Vec::new(),
                status,
            };
            assert_ne!(step_label(&content), "Preparing an edit");
            assert_eq!(
                format!("{} {}", step_verb(&content), step_label(&content)),
                if status == ToolStatus::Failed {
                    "Tried to edit a file"
                } else {
                    "Edited a file"
                }
            );
        }
    }

    /// A group still preparing an edit says so, rather than summarizing only
    /// the steps that have finished describing themselves.
    #[test]
    fn a_group_counts_an_edit_still_being_prepared() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Failed),
            Item {
                id: "e1".into(),
                role: Role::Assistant,
                content: ItemContent::Changes {
                    changes: Vec::new(),
                    status: ToolStatus::Running,
                },
                started_at: None,
                at: None,
            },
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(
            turns[0].summary,
            "Preparing an edit, ran a command · working"
        );
    }

    /// A skill is told as read and context as added, not as tools used.
    #[test]
    fn a_group_reads_skills_and_adds_context_rather_than_using_tools() {
        let tool = |id: &str, name: &str| Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: name.into(),
                input: String::new(),
                output: String::new(),
                status: ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        };
        let summary = |steps: Vec<Item>| {
            let mut items = vec![text("u1", Role::User, Phase::Final, false)];
            items.extend(steps);
            group(&items, &unasked(), plain)[0].summary.clone()
        };
        assert_eq!(
            summary(vec![
                command("c1", ToolStatus::Succeeded),
                tool("s1", "Skill"),
                tool("x1", "Claude context"),
            ]),
            "Ran a command, read a skill, added context"
        );
        assert_eq!(
            summary(vec![
                tool("s1", "Skill"),
                tool("s2", "Skill"),
                tool("t1", "mcp__docs__search"),
                tool("h1", "Hook context"),
                tool("h2", "Hook context"),
            ]),
            "Read 2 skills, used a tool, added context 2 times"
        );
    }

    /// A group of one step names it on one line, bounded, however long the
    /// script it ran: the strip cuts it to the pane and the open row shows
    /// the whole of it.
    #[test]
    fn a_one_step_summary_is_one_bounded_line() {
        let mut item = command("c1", ToolStatus::Succeeded);
        if let ItemContent::Command { command, .. } = &mut item.content {
            *command = format!("cd /tmp; {}\necho second line", "echo x; ".repeat(100));
        }
        let items = vec![text("u1", Role::User, Phase::Final, false), item];
        let summary = &group(&items, &unasked(), plain)[0].summary;
        assert!(summary.starts_with("Ran cd /tmp; echo x;"), "{summary}");
        assert!(!summary.contains('\n'), "{summary}");
        assert_eq!(summary.chars().count(), SUMMARY_LIMIT, "{summary}");
        assert!(!summary.ends_with('…'));
        // Disclosing the row shows later lines without an injected ellipsis.
        assert_eq!(headline("Ran cat <<EOF\nbody\nEOF".into()), "Ran cat <<EOF");
        assert_eq!(headline("Ran ls".into()), "Ran ls");
    }

    /// A step's verb says how it stands: under way, done, or tried and not
    /// done. A command that failed still ran; a refused one did not.
    #[test]
    fn a_steps_verb_follows_its_status() {
        let ran = |action, status| {
            let mut item = command("c", status);
            if let ItemContent::Command { action: kind, .. } = &mut item.content {
                *kind = action;
            }
            step_verb(&item.content)
        };
        use CommandAction::{Read, Run};
        assert_eq!(ran(Run, ToolStatus::Running), "Running");
        assert_eq!(ran(Run, ToolStatus::Succeeded), "Ran");
        assert_eq!(ran(Run, ToolStatus::Failed), "Ran");
        assert_eq!(ran(Run, ToolStatus::Denied), "Tried to run");
        assert_eq!(ran(Read, ToolStatus::Running), "Reading");
        assert_eq!(ran(Read, ToolStatus::Failed), "Tried to read");
        let edit = |status| ItemContent::Changes {
            changes: vec![FileChange {
                lines: crate::chat::LineOrigin::File,
                path: "a.rs".into(),
                kind: ChangeKind::Updated,
                diff: String::new(),
            }],
            status,
        };
        assert_eq!(step_verb(&edit(ToolStatus::Running)), "Editing");
        assert_eq!(step_verb(&edit(ToolStatus::Succeeded)), "Edited");
        assert_eq!(step_verb(&edit(ToolStatus::Denied)), "Tried to edit");
        // A search reported before its query ends on what it did, not "for".
        let search = |query: &str| ItemContent::WebSearch {
            query: query.into(),
            results: Vec::new(),
        };
        assert_eq!(step_verb(&search(" ")), "Searched the web");
        assert_eq!(step_verb(&search("iced")), "Searched the web for");
    }

    /// Tool rows say what each call did, and the group counts them by what
    /// they did, so neither repeats "Called" on every line. A group of several
    /// files counts them rather than reading "Edited File changes".
    #[test]
    fn tool_rows_say_what_they_did_and_groups_count_them_once() {
        let tool = |id: &str, name: &str, input: &str| Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: name.into(),
                input: input.into(),
                output: String::new(),
                status: ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        };
        let skill = tool("t1", "Skill", r#"{"skill":"writing-copy"}"#);
        let title = Title::of(
            "Skill",
            r#"{"skill":"writing-copy"}"#,
            ToolStatus::Succeeded,
        );
        let (verb, label) = step_title(&skill.content, Some(&title));
        assert_eq!((verb, label.as_ref()), ("Read skill", "writing-copy"));
        // Without its title the row names the tool alone: nothing reads the
        // call's input while drawing it.
        assert_eq!(step_title(&skill.content, None), ("", "Skill".into()));
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            skill,
            tool("t2", "mcp__linear__get_issue", r#"{"id":"ENG-1"}"#),
        ];
        let turns = group(&items, &unasked(), plain);
        assert_eq!(turns[0].summary, "Read a skill, used a tool");
        let only = vec![
            text("u1", Role::User, Phase::Final, false),
            tool("t1", "mcp__linear__get_issue", "{}"),
        ];
        assert_eq!(
            group(&only, &unasked(), plain)[0].summary,
            "Used Linear: get issue"
        );
        let change = |path: &str| chat::FileChange {
            lines: crate::chat::LineOrigin::File,
            path: path.into(),
            kind: chat::ChangeKind::Updated,
            diff: String::new(),
        };
        let edits = ItemContent::Changes {
            changes: vec![change("a.rs"), change("b.rs")],
            status: ToolStatus::Succeeded,
        };
        assert_eq!(
            format!("{} {}", step_verb(&edits), step_label(&edits)),
            "Edited 2 files"
        );
    }

    #[test]
    fn a_step_on_one_file_names_it_from_the_working_directory() {
        let edits = |paths: &[&str]| ItemContent::Changes {
            changes: paths
                .iter()
                .map(|path| FileChange {
                    lines: crate::chat::LineOrigin::File,
                    path: (*path).into(),
                    kind: ChangeKind::Updated,
                    diff: String::new(),
                })
                .collect(),
            status: ToolStatus::Succeeded,
        };
        let read = |label: &str, command: &str| ItemContent::Command {
            label: label.into(),
            command: command.into(),
            working_directory: "/work".into(),
            action: CommandAction::Read,
            status: ToolStatus::Succeeded,
            exit_code: None,
            duration_ms: None,
            output: String::new(),
        };
        assert!(names_path(&edits(&["/work/src/a.rs"]), None));
        assert!(!names_path(&edits(&["a.rs", "b.rs"]), None));
        assert!(!names_path(&edits(&[]), None));
        assert!(names_path(&read("/work/a.rs", "/work/a.rs"), None));
        // Codex labels a read with its file, beside the shell command.
        assert!(names_path(&read("src/a.rs", "sed -n 1,80p src/a.rs"), None));
        assert!(names_path(&read("Read a.rs", "cat a.rs"), None));
        assert!(!names_path(&read("", "cat a.rs"), None));
        let tool = ItemContent::Tool {
            name: "Write".into(),
            input: String::new(),
            output: String::new(),
            status: ToolStatus::Succeeded,
        };
        let written = Title::of(
            "Write",
            r#"{"file_path":"/work/a.rs"}"#,
            ToolStatus::Succeeded,
        );
        assert!(names_path(&tool, Some(&written)));
        let searched = Title::of("Grep", r#"{"pattern":"a.rs"}"#, ToolStatus::Succeeded);
        assert!(!names_path(&tool, Some(&searched)));

        let root = std::path::Path::new("/home/user/project");
        assert_eq!(shown_path("/home/user/project/src/a.rs", root), "src/a.rs");
        assert_eq!(
            shown_path(
                "/home/user/project/src/a.rs",
                std::path::Path::new("/home/user/project/")
            ),
            "src/a.rs"
        );
        // A sibling that shares the directory's name as a prefix is outside it.
        assert_eq!(
            shown_path("/home/user/project-two/a.rs", root),
            "/home/user/project-two/a.rs"
        );
        assert_eq!(shown_path("/home/user/project", root), "/home/user/project");
        assert_eq!(shown_path("/etc/hosts", root), "/etc/hosts");
        assert_eq!(shown_path("src/a.rs", root), "src/a.rs");
        // Outside it, a file under the home directory is named from there.
        if let Some(home) = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned()) {
            let settings = home.join(".claude/settings.json");
            assert_eq!(
                shown_path(settings.to_str().unwrap(), root),
                "~/.claude/settings.json"
            );
        }
        assert_eq!(
            shown_path("/etc/hosts", std::path::Path::new("/")),
            "/etc/hosts"
        );
        // Repeated separators, `.` and `..` are read as written.
        assert_eq!(
            shown_path("/home/user/project//src/./a.rs", root),
            "src/a.rs"
        );
        assert_eq!(
            shown_path("/home/user/project/../etc/x", root),
            "/home/user/etc/x"
        );
        assert_eq!(shown_path("/home/user/project/src/../a.rs", root), "a.rs");
        // A relative path is the agent's own, shown as it gave it.
        assert_eq!(shown_path("../other/a.rs", root), "../other/a.rs");
    }

    /// A message sent with a provider's own tool, as the step's reading
    /// gives it: to `to`, saying `body`, queued for that agent.
    fn sent(to: &str, body: &str) -> Coordinated {
        use crate::chat::coordination::{
            Coordination, Delivery, Kind, Outbound, OutboundKind, Outcome, Text,
        };
        Coordinated::new(Coordination {
            kind: Kind::Message(Outbound {
                kind: OutboundKind::Send,
                to: to.into(),
                body: Text::Inline(body.into()),
                summary: String::new(),
                waits: false,
            }),
            outcome: Outcome {
                delivery: Some(Delivery::Queued),
                ..Outcome::default()
            },
            subagents: Vec::new(),
            cli: false,
        })
    }

    /// Messages one after another fold into a group of their own, which
    /// names the first, as the steps between two stretches of prose do. A
    /// step between two runs keeps them apart, in the order they were sent.
    #[test]
    fn consecutive_messages_fold_into_their_own_group() {
        const FYI: &str = "FYI: main's ui lane is broken.";
        let readings: std::collections::HashMap<&str, Coordinated> = [
            ("m1", sent("fix-switcher", FYI)),
            ("m2", sent("main", FYI)),
            ("m3", sent("perf-ui", FYI)),
            ("m4", sent("fix-switcher", "Rebase over #3646.")),
            ("m5", sent("main", "Merged.")),
        ]
        .into_iter()
        .collect();
        let coordinated = |item: &Item| readings.get(item.id.as_str());
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Succeeded),
            command("m3", ToolStatus::Failed),
            command("c2", ToolStatus::Succeeded),
            command("m4", ToolStatus::Succeeded),
            command("m5", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), coordinated);
        let shape: Vec<_> = turns
            .iter()
            .map(|turn| (turn.key.as_str(), turn.messages, steps(turn), slots(turn)))
            .collect();
        assert_eq!(
            shape,
            [
                ("u1", false, vec![1], vec![]),
                ("m1", true, vec![2, 3, 4], vec![]),
                ("c2", false, vec![5], vec![]),
                ("m4", true, vec![6, 7], vec![(8, Slot::Prose)]),
            ]
        );
        // The same words to each agent are one message said to all of
        // them; what did not go leads how they stand.
        assert_eq!(
            turns[1].summary,
            format!("Messaged 3 agents: {FYI} · 1 not sent · 2 queued")
        );
        assert_eq!(
            turns[1].glyph(),
            Some(Status::PartlyFailed),
            "one message not sent among three is a partial failure"
        );
        // Different words are told apart by whom they went to.
        assert_eq!(
            turns[3].summary,
            "Sent 2 messages to fix-switcher, main · Queued"
        );
    }

    /// One message alone stays the card it is, and so do messages that
    /// something drawn between keeps apart.
    #[test]
    fn a_message_alone_stays_a_card() {
        let (first, second) = (sent("main", "one"), sent("perf-ui", "two"));
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            text("n1", Role::Assistant, Phase::Commentary, false),
            command("m2", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(turns.len(), 1);
        assert!(!turns[0].messages);
        assert_eq!(
            slots(&turns[0]),
            [
                (1, Slot::Card),
                (2, Slot::Commentary),
                (3, Slot::Card),
                (4, Slot::Prose)
            ]
        );
    }

    /// A message that opened its group becomes the group of messages it
    /// starts, keyed by it as it was, and a message still sending keeps that
    /// group working without taking the activity indicator, which its row
    /// states in words.
    #[test]
    fn a_run_of_messages_opened_by_its_first_keeps_its_key() {
        let (first, second) = (sent("main", "same"), sent("perf-ui", "same"));
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("a1", Role::Assistant, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Running),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(turns.len(), 2);
        assert_eq!(slots(&turns[0]), [(1, Slot::Prose)]);
        assert_eq!(
            (turns[1].key.as_str(), turns[1].messages, steps(&turns[1])),
            ("m1", true, vec![2, 3])
        );
        assert!(turns[1].running);
        assert!(!turns[1].newest_running);
        assert_eq!(
            turns[1].summary,
            "Messaged 2 agents: same · 1 sending · 1 queued"
        );
    }

    /// Messages are one said to several only when the whole of each is the
    /// same: a long one that differs past what a row shows is not.
    #[test]
    fn messages_that_differ_past_a_rows_reach_are_not_one() {
        let opening = "Main's ui lane is broken. ".repeat(20);
        let (first, second) = (
            sent("docs-review", &format!("{opening}Rebase first.")),
            sent("perf-audit", &format!("{opening}Wait for review.")),
        );
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(
            turns[1].summary,
            "Sent 2 messages to docs-review, perf-audit · Queued"
        );
    }

    /// The same words sent to one agent again, as a retry is, still say
    /// whom they went to.
    #[test]
    fn a_message_sent_again_names_its_agent() {
        let (first, second) = (sent("main", "Rebase first."), sent("main", "Rebase first."));
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(
            turns[1].summary,
            "Sent 2 messages to main: Rebase first. · Queued"
        );
    }

    /// A send that failed is known by the name it was given and its retry
    /// by the agent it reached; one agent sent the words twice is not two.
    #[test]
    fn a_failed_message_and_its_retry_are_to_one_agent() {
        let pinned = |message: Coordinated, agent: &str| {
            let mut reading = message.reading.clone();
            reading.subagents = vec![agent.to_owned()];
            Coordinated::new(reading)
        };
        let (first, second) = (
            pinned(sent("main", "Rebase first."), "main"),
            pinned(sent("main", "Rebase first."), "a0aac0bb3f21"),
        );
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Failed),
            command("m2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(
            turns[1].summary,
            "Sent 2 messages to main: Rebase first. · 1 not sent · 1 queued"
        );
    }

    /// Agents this conversation does not list, each addressed by its own
    /// ID, are as many agents as messages, whatever the rows call them.
    #[test]
    fn agents_known_only_by_id_are_each_their_own() {
        let readings: std::collections::HashMap<&str, Coordinated> = [
            ("m1", sent("019a3c2e", "Rebase first.")),
            ("m2", sent("019a7f10", "Rebase first.")),
            ("m3", sent("019b0d44", "Wait for review.")),
            ("m4", sent("019b5e91", "Merged.")),
        ]
        .into_iter()
        .map(|(id, message)| {
            let mut reading = message.reading.clone();
            if let crate::chat::coordination::Kind::Message(outbound) = &reading.kind {
                reading.subagents = vec![outbound.to.clone()];
            }
            (id, Coordinated::new(reading))
        })
        .collect();
        let coordinated = |item: &Item| readings.get(item.id.as_str());
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Succeeded),
            command("c1", ToolStatus::Succeeded),
            command("m3", ToolStatus::Succeeded),
            command("m4", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert!(
            turns[1]
                .summary
                .starts_with("Messaged 2 agents: Rebase first."),
            "{}",
            turns[1].summary
        );
        assert!(
            turns[3].summary.starts_with("Sent 2 messages to 2 agents"),
            "{}",
            turns[3].summary
        );
    }

    /// A `cagents` send that failed names only what it was given; its
    /// retry also the session it reached, which is the same agent.
    #[test]
    fn a_failed_cli_message_and_its_retry_are_to_one_agent() {
        use crate::chat::coordination::{Arrival, Receipt};
        let cli = |receipt: Option<Receipt>| {
            let mut reading = sent("95fcd1b9", "Rebase first.").reading.clone();
            reading.cli = true;
            reading.outcome.delivery = None;
            reading.outcome.receipt = receipt;
            Coordinated::new(reading)
        };
        let (first, second) = (
            cli(None),
            cli(Some(Receipt {
                message: "4c1e".into(),
                recipient: "session 95fcd1b9 \"Fix the sidebar\"".into(),
                arrival: Arrival::WhenIdle,
            })),
        );
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Failed),
            command("m2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert!(
            turns[1]
                .summary
                .starts_with("Sent 2 messages to Fix the sidebar (95fcd1b9): Rebase first."),
            "{}",
            turns[1].summary
        );
    }

    /// Sessions that share a title, as new channels do, are told apart by
    /// their IDs.
    #[test]
    fn sessions_sharing_a_title_are_each_their_own() {
        use crate::chat::coordination::{Arrival, Receipt};
        let cli = |session: &str| {
            let mut reading = sent(session, "Rebase.").reading.clone();
            reading.cli = true;
            reading.outcome.delivery = None;
            reading.outcome.receipt = Some(Receipt {
                message: "4c1e".into(),
                recipient: format!("session {session} \"New channel\""),
                arrival: Arrival::WhenIdle,
            });
            Coordinated::new(reading)
        };
        let (first, second) = (cli("aaaa1111"), cli("bbbb2222"));
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Succeeded),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert!(
            turns[1].summary.starts_with("Messaged 2 agents: Rebase."),
            "{}",
            turns[1].summary
        );
    }

    /// The first message of a run leaves the group it was drawn in, and
    /// takes its time with it: the steps before it are measured as they
    /// were.
    #[test]
    fn a_message_that_leaves_for_its_run_takes_its_time() {
        let (first, second) = (sent("docs-review", "same"), sent("perf-audit", "same"));
        let coordinated = |item: &Item| match item.id.as_str() {
            "m1" => Some(&first),
            "m2" => Some(&second),
            _ => None,
        };
        let mut tool = command("c1", ToolStatus::Succeeded);
        tool.started_at = Some(1_000);
        tool.at = Some(2_000);
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            tool,
            stamped(command("m1", ToolStatus::Succeeded), 30_000),
            stamped(command("m2", ToolStatus::Succeeded), 31_000),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].working_for(90_000), Some(1_000));
        assert_eq!(turns[1].working_for(90_000), Some(1_000));
        // Hidden thinking between the messages goes with them too.
        let mut items = items;
        items.insert(3, thought("t", "", 30_000, 40_000));
        items[4].at = Some(41_000);
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].working_for(90_000), Some(1_000));
        assert_eq!(turns[1].working_for(90_000), Some(11_000));
    }

    /// A message to every teammate reaches more agents than the run can
    /// count, and one to several agents reaches each of them.
    #[test]
    fn a_run_counts_the_agents_its_messages_reached() {
        let several = |message: Coordinated, agents: &[&str]| {
            let mut reading = message.reading.clone();
            reading.subagents = agents.iter().map(|agent| (*agent).to_owned()).collect();
            Coordinated::new(reading)
        };
        let readings: std::collections::HashMap<&str, Coordinated> = [
            ("m1", sent("*", "Main is green.")),
            ("m2", sent("docs-review", "Main is green.")),
            (
                "m3",
                several(sent("ui", "Rebase."), &["ui-tests", "packaging"]),
            ),
            ("m4", sent("triage", "Rebase.")),
        ]
        .into_iter()
        .collect();
        let coordinated = |item: &Item| readings.get(item.id.as_str());
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("m1", ToolStatus::Succeeded),
            command("m2", ToolStatus::Succeeded),
            command("c1", ToolStatus::Succeeded),
            command("m3", ToolStatus::Succeeded),
            command("m4", ToolStatus::Failed),
        ];
        let turns = group(&items, &unasked(), coordinated);
        assert_eq!(
            turns[1].summary,
            "Sent 2 messages to every teammate, docs-review: Main is green. · Queued"
        );
        assert!(
            turns[3].summary.starts_with("Messaged 3 agents: Rebase."),
            "{}",
            turns[3].summary
        );
        // The tooltip says how each went.
        let run = MessageRun::of(
            turns[3].steps.iter().map(|placed| {
                let item = &items[placed.index];
                (item, coordinated(item).expect("a message"))
            }),
            &|_| None,
        )
        .expect("a run of messages");
        let tip = run.tip();
        assert!(tip.starts_with("Messaged 3 agents\n\n"), "{tip}");
        assert!(tip.contains("\ntriage: Not sent"), "{tip}");
        // A reason that opens with its state says it once.
        assert!(tip.contains("\nui-tests, packaging: Queued for"), "{tip}");
        assert!(tip.ends_with("\n\nRebase."), "{tip}");
    }
}
