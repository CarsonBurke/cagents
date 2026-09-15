//! Grouping a flat transcript into the turns a reader perceives.
//!
//! A provider hands us an ordered list of items and no notion of a turn. The
//! consequence on screen was that one assistant response arrived as three
//! sibling rows, each captioned with the provider's name, with nothing to say
//! they belonged together or which of them was the answer. Grouping is purely
//! a presentation concern - it needs no provider support, only the order the
//! items already arrive in - so it lives here rather than on the wire.

use crate::chat::{self, Item, ItemContent, Phase, Role, ToolStatus};

/// Where an item belongs inside its turn.
///
/// The three roles read very differently and must not share a treatment:
/// `Step` is what the agent did, `Commentary` is it narrating while it worked,
/// and `Answer` is what the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    /// A command, tool call, file change, search, or reasoning block.
    Step,
    /// Progress narration. Measured over this workstation's Codex history it
    /// outnumbers answers roughly seven to one, so it has to recede.
    Commentary,
    /// The answer, or a user message, or a system notice.
    Prose,
}

/// One transcript item placed within a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placed {
    /// Index into the snapshot's item list. An index rather than an owned id
    /// because the previous view cloned every item's id three to seven times
    /// per frame purely to address it.
    pub(super) index: usize,
    pub(super) slot: Slot,
}

/// A user message and the activity it caused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Turn {
    /// The user message that opened the turn, in the order its parts arrived.
    /// Empty for activity that arrived before any user message - a resumed
    /// session's history, or a provider greeting. More than one entry because
    /// a single message is not a single item: a prompt with two attachments
    /// reaches the transcript as three `Role::User` items.
    pub(super) prompt: Vec<usize>,
    pub(super) steps: Vec<Placed>,
    pub(super) prose: Vec<Placed>,
    /// Set while any step in this turn is still running, which is what marks
    /// the live turn without needing a separate global flag.
    pub(super) running: bool,
    /// Worst status any step reached, so a collapsed strip can show that
    /// something failed without being expanded.
    pub(super) outcome: Option<ToolStatus>,
    /// The key the reader's disclosure choice is stored under, and the
    /// collapsed strip's own line. Both are formed here, once per snapshot,
    /// because the view draws them on every frame and they change only when
    /// the transcript does.
    ///
    /// The key is the first item's id, because a turn has no identity of its
    /// own: it is derived from item order on every snapshot, and the reader's
    /// choice to open its steps has to survive that. It does not survive the
    /// store's paging window dropping the turn's prompt, and should not: what
    /// is left is leftover activity with no prompt, which is a different
    /// thing from the exchange the reader made a choice about.
    pub(super) key: String,
    pub(super) summary: String,
}

impl Turn {
    fn opened(prompt: Option<usize>) -> Self {
        Self {
            prompt: prompt.into_iter().collect(),
            steps: Vec::new(),
            prose: Vec::new(),
            running: false,
            outcome: None,
            key: String::new(),
            summary: String::new(),
        }
    }

    /// The glyph and tone a collapsed strip reports for this turn.
    ///
    /// `None` when no step has reported a status at all, which is the opening
    /// seconds of most turns: the first thing a provider sends is reasoning,
    /// and reasoning carries no status. Claiming success there - which a
    /// checkmark does - is a claim about work that has not happened yet.
    pub(super) fn glyph(&self) -> Option<ToolStatus> {
        self.outcome
            .or_else(|| self.running.then_some(ToolStatus::Running))
    }

    /// Fills in the key and summary once the turn's membership is final.
    fn describe(&mut self, items: &[Item]) {
        self.key = self
            .prompt
            .first()
            .copied()
            .or_else(|| self.steps.first().map(|placed| placed.index))
            .or_else(|| self.prose.first().map(|placed| placed.index))
            .and_then(|index| items.get(index))
            .map(|item| item.id.to_string())
            .unwrap_or_default();
        // A single step names itself, because "1 step" tells the reader
        // nothing they could not see from the row's absence. Several steps are
        // counted, with a failure called out, because that is the one thing a
        // collapsed strip must not hide.
        self.summary = match self.steps.as_slice() {
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
        // reader deciding whether to wait needs that first. The glyph still
        // carries the failure, so nothing is hidden by saying "working".
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

    /// Whether the turn carries nothing a reader would see.
    ///
    /// An empty turn is not hypothetical: the surface used to render two
    /// `System` headings at the top of every conversation, one of which had no
    /// body at all.
    pub(super) fn is_empty(&self) -> bool {
        self.prompt.is_empty() && self.steps.is_empty() && self.prose.is_empty()
    }

    /// Whether nothing has happened in this turn since its prompt.
    ///
    /// A user message's remaining parts are what this is for: they arrive as
    /// separate items, and each one would otherwise open a turn of its own.
    fn awaiting_activity(&self) -> bool {
        self.steps.is_empty() && self.prose.is_empty()
    }

    fn observe(&mut self, status: Option<ToolStatus>) {
        let Some(status) = status else {
            return;
        };
        if status == ToolStatus::Running {
            self.running = true;
        }
        // Severity order, so one failure in a long turn is what the collapsed
        // strip reports rather than the last step's incidental success.
        let rank = |status: ToolStatus| match status {
            ToolStatus::Succeeded => 0,
            ToolStatus::Running => 1,
            ToolStatus::Denied => 2,
            ToolStatus::Failed => 3,
        };
        if self
            .outcome
            .is_none_or(|current| rank(status) > rank(current))
        {
            self.outcome = Some(status);
        }
    }
}

/// Splits `items` into turns, preserving their order.
///
/// A user message opens a turn. Everything after it belongs to that turn until
/// the next user message. Items that arrive before any user message form a
/// leading turn with no prompt, which is how a resumed conversation's history
/// and a provider's opening notice both render without inventing a prompt for
/// them.
///
/// Consecutive user messages with no activity between them share one turn.
/// They have to, because a prompt carrying attachments arrives as one item per
/// content part and each part is a `Role::User` item; grouping them by arrival
/// would detach the agent's work from the message that caused it and print an
/// "attachment" bubble as though the reader had sent it alone. Two messages
/// the reader genuinely sent back to back land in one turn as well, which is
/// right for the same reason: there is no activity between them to attribute.
pub(super) fn group(items: &[Item]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !renders(&item.content) {
            continue;
        }
        let opens = item.role == Role::User && !item.content.is_step();
        if opens {
            match turns.last_mut() {
                Some(turn) if turn.awaiting_activity() => turn.prompt.push(index),
                _ => turns.push(Turn::opened(Some(index))),
            }
            continue;
        }
        if turns.is_empty() {
            turns.push(Turn::opened(None));
        }
        // `turns` is non-empty: the branch above pushes when it would not be.
        let turn = turns.last_mut().expect("a turn is open");
        let slot = slot_of(&item.content);
        turn.observe(item.content.status());
        match slot {
            Slot::Step => turn.steps.push(Placed { index, slot }),
            Slot::Commentary | Slot::Prose => turn.prose.push(Placed { index, slot }),
        }
    }
    turns.retain(|turn| !turn.is_empty());
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
    if content.is_step() {
        return Slot::Step;
    }
    match content {
        ItemContent::Text {
            phase: Phase::Commentary,
            ..
        } => Slot::Commentary,
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
        }
    }

    #[test]
    fn a_user_message_opens_a_turn_and_following_activity_joins_it() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Commentary, false),
            text("a2", Role::Assistant, Phase::Final, false),
            text("u2", Role::User, Phase::Final, false),
            command("c2", ToolStatus::Running),
        ];
        let turns = group(&items);
        assert_eq!(turns.len(), 2, "each user message opens exactly one turn");
        assert_eq!(turns[0].prompt, [0]);
        assert_eq!(
            turns[0].steps.iter().map(|p| p.index).collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(
            turns[0]
                .prose
                .iter()
                .map(|p| (p.index, p.slot))
                .collect::<Vec<_>>(),
            [(2, Slot::Commentary), (3, Slot::Prose)],
            "commentary and the answer occupy different slots"
        );
        assert!(!turns[0].running);
        assert_eq!(turns[1].prompt, [4]);
        assert!(turns[1].running, "a running step marks its own turn live");
    }

    #[test]
    fn every_part_of_one_user_message_shares_the_turn_its_activity_belongs_to() {
        let items = vec![
            text("u1/input/0", Role::User, Phase::Final, false),
            Item {
                id: "u1/input/1".into(),
                role: Role::User,
                content: ItemContent::Image {
                    path: "/home/reader/diagram.png".into(),
                    description: "User attachment".into(),
                },
            },
            command("c1", ToolStatus::Succeeded),
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items);
        assert_eq!(
            turns.len(),
            1,
            "a prompt and its attachment are one message, not two turns"
        );
        assert_eq!(turns[0].prompt, [0, 1]);
        assert_eq!(
            turns[0].steps.iter().map(|p| p.index).collect::<Vec<_>>(),
            [2],
            "the work stays attached to the message that caused it"
        );
        assert_eq!(turns[0].prose.len(), 1);
    }

    #[test]
    fn a_turn_claims_no_outcome_until_a_step_reports_one() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("r1", Role::Assistant, Phase::Final, true),
        ];
        let turns = group(&items);
        assert_eq!(
            turns[0].glyph(),
            None,
            "reasoning carries no status, so nothing may claim success yet"
        );
        assert_eq!(turns[0].summary, "Thinking");
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Failed),
            command("c2", ToolStatus::Running),
        ];
        let turns = group(&items);
        assert_eq!(
            turns[0].glyph(),
            Some(ToolStatus::Failed),
            "the failure stays visible in the glyph"
        );
        assert_eq!(
            turns[0].summary, "2 steps · working",
            "still working outranks how the finished steps went"
        );
    }

    #[test]
    fn activity_before_any_user_message_forms_a_promptless_turn() {
        let items = vec![
            text("s1", Role::System, Phase::Final, false),
            text("u1", Role::User, Phase::Final, false),
        ];
        let turns = group(&items);
        assert_eq!(turns.len(), 2);
        assert!(
            turns[0].prompt.is_empty(),
            "history must not invent a prompt"
        );
        assert_eq!(turns[0].prose.len(), 1);
        assert_eq!(turns[1].prompt, [1]);
    }

    #[test]
    fn reasoning_is_a_step_even_though_it_is_text() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            text("r1", Role::Assistant, Phase::Final, true),
        ];
        let turns = group(&items);
        assert_eq!(turns[0].steps.len(), 1, "thinking collapses with the steps");
        assert!(turns[0].prose.is_empty());
    }

    #[test]
    fn a_failure_outranks_later_successes_in_the_collapsed_outcome() {
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            command("c1", ToolStatus::Failed),
            command("c2", ToolStatus::Succeeded),
        ];
        let turns = group(&items);
        assert_eq!(
            turns[0].outcome,
            Some(ToolStatus::Failed),
            "a collapsed turn must still report that something failed"
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
        };
        assert_eq!(group(std::slice::from_ref(&empty)).len(), 0);
        assert_eq!(group(&[]).len(), 0);
        // A message that has not streamed its first bytes yet is the same
        // shape, and must come back once it has them.
        let items = vec![
            text("u1", Role::User, Phase::Final, false),
            empty,
            text("a1", Role::Assistant, Phase::Final, false),
        ];
        let turns = group(&items);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].prose.iter().map(|p| p.index).collect::<Vec<_>>(),
            [2],
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
