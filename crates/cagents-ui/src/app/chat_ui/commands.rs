//! What a chat has asked of its provider and not yet heard back about.
//!
//! The provider takes this window's commands one at a time, in the order they
//! were given: a model chosen and then a message sent must reach it in that
//! order, and two requests on their way at once could overtake each other.
//! So one command is in flight and the rest wait behind it here.
//!
//! None of that holds the reader up. What a command changes is shown the
//! moment it is given: a message leaves the composer and appears where it is
//! going, a model or permission mode reads as chosen. The provider's answer
//! then settles it. One it accepts stays shown until the conversation itself
//! shows the change; one it refuses is undone, and a message it refused goes
//! back to the composer with the reason, so nothing written is lost.
//!
//! A message sent starts a turn, but only once the provider says so. What
//! acts on that turn - steering it, queueing behind it, stopping it - waits
//! here until it has begun, since a provider asked about a turn it has not
//! started yet refuses. Should the turn end or never begin meanwhile, a
//! message waiting for it is sent as a message of its own.
use super::*;
use std::collections::VecDeque;

/// How long a model or permission mode the provider took reads as chosen
/// while the conversation still reports the one before it. A provider
/// confirms a choice in a later report than its answer to the command; one
/// that never does has not made the change.
const CHOICE_PATIENCE: Duration = Duration::from_secs(5);

/// How long a message the provider took may go without its turn beginning
/// before what the conversation shows is taken as final: a conversation
/// idle with an error refused it, one idle without has nothing more to
/// show of it. Until then a report of the conversation working may still
/// be on its way.
pub(super) const TURN_PATIENCE: Duration = Duration::from_secs(5);

/// One command to the provider, with what this window shows of it until the
/// provider answers.
pub(super) struct PendingCommand {
    /// The held follow-ups this command carries, by ID. Any written while
    /// it is on its way are not among them, and wait for the next.
    pub(super) followups: Vec<u64>,
    /// The provider's queued message this command asks back, which stops
    /// being waited for if the request never reaches the provider.
    pub(super) take_back: Option<String>,
    pub(super) id: u64,
    pub(super) request: Option<String>,
    /// The first user prompt, used to name a new channel once locally queued.
    pub(super) first_prompt: Option<String>,
    /// The model or permission mode this command chooses.
    pub(super) choice: Option<Choice>,
    /// The message this command sends, kept for the composer's history once
    /// the provider has accepted it. A rejected send is not history: it goes
    /// back to the composer.
    pub(super) sent: Option<String>,
    /// What the reader had said last as `sent` went out, which tells it apart
    /// from an earlier message reading the same.
    pub(super) sent_after: SentAfter,
    pub(super) sent_attachments: Vec<Attachment>,
    /// Which end of the history this command is extending, where it is a
    /// page request at all. The transcript says so at the edge the rows will
    /// appear at, so it has to know which edge is waiting.
    pub(super) page: Option<view::Page>,
    /// The step of a queued first message this command takes, if it is one.
    pub(super) queued: Option<QueuedStep>,
    /// The reader's message this command carries out of the composer.
    pub(super) outgoing: Option<Outgoing>,
    /// The held follow-ups this command carries, as the one message they
    /// make. Their own rows show them on their way; accepted, they show as
    /// this message until the conversation does.
    carried: Option<Outgoing>,
    /// Any other command, kept so that the same request given again while
    /// this one is still on its way is not sent twice.
    repeat: Option<Command>,
    /// Whether this command waited for a turn a message on its way was to
    /// start, so that it was given for that turn.
    held: bool,
    /// Whether this is a stop given while the conversation waited for that
    /// turn to begin, which goes at once: the provider holds it until the
    /// turn has begun, and it is how the reader gets out of one slow to.
    at_once: bool,
}

impl PendingCommand {
    pub(super) fn new(id: u64, command: &Command) -> Self {
        let text = match command {
            Command::Prompt { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        };
        Self {
            followups: Vec::new(),
            take_back: None,
            id,
            request: match command {
                Command::Answer { request_id, .. } => Some(request_id.clone()),
                _ => None,
            },
            first_prompt: text.clone(),
            choice: Choice::of(command),
            sent: match command {
                Command::Slash { command } => Some(command.clone()),
                _ => text,
            },
            sent_after: SentAfter::default(),
            sent_attachments: match command {
                Command::Prompt { attachments, .. } => attachments.clone(),
                _ => Vec::new(),
            },
            page: match command {
                Command::LoadEarlier => Some(view::Page::Earlier),
                Command::Refresh => Some(view::Page::Latest),
                Command::LoadLater => Some(view::Page::Newer),
                _ => None,
            },
            queued: None,
            outgoing: None,
            carried: None,
            repeat: (!matches!(
                command,
                Command::Prompt { .. } | Command::Slash { .. } | Command::Compact
            ))
            .then(|| command.clone()),
            held: false,
            at_once: false,
        }
    }

    /// Whether this command moves the transcript window to an item.
    pub(super) fn seeks(&self) -> bool {
        matches!(self.repeat, Some(Command::Seek { .. }))
    }

    /// Marks this command as carrying the held follow-ups `ids`, which
    /// together make `message`.
    pub(super) fn carry_followups(&mut self, ids: Vec<u64>, message: Outgoing) {
        self.followups = ids;
        self.carried = Some(message);
    }

    /// Takes the reader's message this command carried, unless the
    /// conversation already shows it: one it has is not the reader's to
    /// write again.
    pub(super) fn take_unsent(&mut self) -> Option<Outgoing> {
        self.outgoing.take().filter(|outgoing| !outgoing.arrived)
    }

    /// The same kind of request as `command`, for telling whether `command`
    /// repeats this one.
    fn same_kind(&self, command: &Command) -> bool {
        self.repeat
            .as_ref()
            .is_some_and(|repeat| std::mem::discriminant(repeat) == std::mem::discriminant(command))
    }
}

/// Whether `command` acts on the turn under way, which the provider has to
/// have begun before it can take it.
fn acts_on_turn(command: &Command) -> bool {
    matches!(
        command,
        Command::Prompt {
            mode: PromptMode::Steer | PromptMode::Queue,
            ..
        } | Command::Interrupt
    )
}

/// A setting a command chooses, shown as chosen while the provider takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    Model {
        model: String,
        reasoning: Option<String>,
    },
    PermissionMode(String),
}

impl Choice {
    fn of(command: &Command) -> Option<Self> {
        match command {
            Command::SetModel { model, reasoning } => Some(Self::Model {
                model: model.clone(),
                reasoning: reasoning.clone(),
            }),
            Command::SetPermissionMode { mode } => Some(Self::PermissionMode(mode.clone())),
            _ => None,
        }
    }

    /// Whether `snapshot` already reports this choice.
    fn shown_by(&self, snapshot: &Snapshot) -> bool {
        match self {
            Self::Model { model, reasoning } => {
                snapshot.selected_model.as_ref() == Some(model) && &snapshot.reasoning == reasoning
            }
            Self::PermissionMode(mode) => snapshot.permission_mode.as_ref() == Some(mode),
        }
    }
}

/// The model and reasoning effort a conversation is set to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChosenModel<'a> {
    pub(super) id: Option<&'a str>,
    pub(super) reasoning: Option<&'a str>,
}

impl<'a> ChosenModel<'a> {
    /// What `snapshot` reports the conversation set to.
    pub(super) fn reported(snapshot: &'a Snapshot) -> Self {
        Self {
            id: snapshot.selected_model.as_deref(),
            reasoning: snapshot.reasoning.as_deref(),
        }
    }

    /// The listed model chosen, or the listed default where none has been.
    /// A stale explicit choice does not select another model.
    pub(super) fn model(self, snapshot: &'a Snapshot) -> Option<&'a chat::Model> {
        let id = self.id.unwrap_or("default");
        snapshot.models.iter().find(|model| model.id == id)
    }
}

/// A message of the reader's on its way to the provider.
pub(super) struct Outgoing {
    /// What the composer held, which returns there if the provider does not
    /// take it.
    pub(super) prompt: prompt::LocalPrompt,
    /// The glance a queue row shows of it, made once.
    pub(super) preview: String,
    pub(super) mode: PromptMode,
    /// The reader's newest message the transcript held when this went to
    /// the provider. Another in its place is this one arriving.
    newest_user: Option<String>,
    /// The messages the provider held for later when this went. A new one
    /// is this one arriving there.
    held: Vec<String>,
    /// The turns the provider had started when this went.
    turns: u64,
    /// The error the conversation reported when this went. A new one, with
    /// no turn begun, is the turn this was to start failing.
    error: Option<String>,
    /// The publication this went after.
    generation: u64,
    /// Whether a publication since showed the conversation working.
    busy_seen: bool,
    /// Whether the conversation shows it already, though the provider has
    /// yet to answer for it.
    arrived: bool,
    /// Its place among what the reader wrote, which orders it with the held
    /// follow-ups when both come back to the composer.
    pub(super) written: u64,
    /// Whether the reader stopped the turn it was to start before that turn
    /// began, so that a turn which never comes gives it back rather than
    /// letting it go.
    stopped: bool,
}

impl Outgoing {
    pub(super) fn new(
        text: String,
        attachments: Vec<Attachment>,
        mode: PromptMode,
        snapshot: &Snapshot,
    ) -> Self {
        let mut outgoing = Self {
            preview: prompt::queue_preview(&text),
            prompt: prompt::LocalPrompt::new(text, attachments),
            mode,
            newest_user: None,
            held: Vec::new(),
            turns: 0,
            error: None,
            generation: 0,
            busy_seen: false,
            arrived: false,
            written: 0,
            stopped: false,
        };
        outgoing.leaves_after(snapshot);
        outgoing
    }

    /// Notes what `snapshot` holds as this message goes to the provider, so
    /// that what changes afterwards can be told apart as its doing.
    fn leaves_after(&mut self, snapshot: &Snapshot) {
        self.newest_user = newest_user_message(snapshot).map(|(id, _)| id.to_owned());
        self.held = held_messages(snapshot).map(str::to_owned).collect();
        self.turns = snapshot.turn_generation;
        self.error = snapshot.error.clone();
        self.generation = snapshot.generation;
        self.busy_seen = false;
    }

    /// Whether the transcript shows this message as its own: a message sent
    /// now, rather than one the provider holds for later.
    pub(super) fn in_transcript(&self) -> bool {
        self.mode == PromptMode::Send
    }

    /// Whether `snapshot` has nothing left to wait for: it shows the message, in the transcript
    /// or among those held for later, or it never will. A turn started and
    /// finished since has had this message's place in it; a reader looking
    /// at earlier history sees it arrive out of view; an exited conversation
    /// shows nothing more. A message steered into a turn has had its place
    /// once that turn is over. A snapshot without its items, as a hidden
    /// chat's is, can only show where the message is held.
    fn settled_by(&self, snapshot: &Snapshot, items: bool) -> bool {
        (items && newest_user_message(snapshot).map(|(id, _)| id) != self.newest_user.as_deref())
            || held_messages(snapshot).any(|id| !self.held.iter().any(|held| held == id))
            || (snapshot.turn_generation > self.turns && !snapshot.busy)
            || (self.mode == PromptMode::Steer
                && snapshot.generation > self.generation
                && !snapshot.busy)
            || (items && snapshot.has_later && snapshot.turn_generation > self.turns)
            || snapshot.exited
    }

    /// Whether `snapshot`, `waited` out [`TURN_PATIENCE`] since the provider
    /// took this message, shows a conversation that is not starting its
    /// turn and reports nothing wrong: there is nothing more to wait for.
    fn quiet_in(&self, snapshot: &Snapshot, waited: bool) -> bool {
        waited && self.never_began(snapshot) && snapshot.error.is_none()
    }

    /// Whether `snapshot` shows the conversation idle since this message
    /// went, with its turn not begun.
    fn never_began(&self, snapshot: &Snapshot) -> bool {
        self.in_transcript()
            && snapshot.generation > self.generation
            && !snapshot.busy
            && !snapshot.exited
            && snapshot.turn_generation == self.turns
    }

    /// Whether `snapshot` shows the turn this message was to start failing
    /// before it began: the provider took the message, then stopped working
    /// with an error of its own and no turn begun. Nothing of it reached the
    /// conversation, or it would have settled already. The error tells
    /// only once it differs from the one shown when this went, or the
    /// conversation was seen working first, or [`TURN_PATIENCE`] is `waited`
    /// out: the same error again is this message failing the same way.
    fn failed_in(&self, snapshot: &Snapshot, waited: bool) -> bool {
        self.never_began(snapshot)
            && snapshot.error.is_some()
            && (snapshot.error != self.error || self.busy_seen || waited)
    }

    /// Whether `snapshot` shows this very message, where it is going.
    fn shown_in(&self, snapshot: &Snapshot, items: bool) -> bool {
        let text = self.prompt.text().trim();
        if text.is_empty() {
            return false;
        }
        if self.in_transcript() {
            items
                && newest_user_message(snapshot).is_some_and(|(id, shown)| {
                    Some(id) != self.newest_user.as_deref() && shown.trim() == text
                })
        } else {
            snapshot
                .queued_messages
                .iter()
                .any(|message| !self.held.contains(&message.id) && message.text.trim() == text)
        }
    }
}

fn held_messages(snapshot: &Snapshot) -> impl Iterator<Item = &str> {
    snapshot
        .queued_messages
        .iter()
        .map(|message| message.id.as_str())
}

/// The reader's newest message in `snapshot`'s transcript, by ID, with its
/// text where it has any.
fn newest_user_message(snapshot: &Snapshot) -> Option<(&str, &str)> {
    snapshot.items.iter().rev().find_map(|item| {
        if item.role != chat::Role::User {
            return None;
        }
        match &item.content {
            ItemContent::Text { text, .. } => Some((item.id.as_str(), text.as_str())),
            ItemContent::Image { .. } => Some((item.id.as_str(), "")),
            _ => None,
        }
    })
}

/// What a command the provider accepted changed, shown until the
/// conversation shows it too.
enum Effect {
    Outgoing(Outgoing),
    Choice(Choice),
}

struct Settling {
    effect: Effect,
    /// The generation presented when the provider accepted it.
    after: u64,
    /// The error the conversation reported then. A new one since is how a
    /// provider says it did not make a change it took the command for.
    error: Option<String>,
    accepted: Instant,
}

impl Settling {
    /// Whether a choice the provider took no longer needs showing ahead of
    /// what `snapshot` reports: it reports the choice, or reports since that
    /// it could not make it, or has had its time to.
    fn choice_settled_by(&self, choice: &Choice, snapshot: &Snapshot) -> bool {
        choice.shown_by(snapshot)
            || snapshot.exited
            || (snapshot.generation > self.after
                && (snapshot.error != self.error || self.accepted.elapsed() >= CHOICE_PATIENCE))
    }
}

/// What one publication settled of a chat's commands.
#[derive(Default)]
pub(super) struct Settled {
    /// Whether anything shown of them changed.
    pub(super) changed: bool,
    /// Messages the provider took whose turn failed before it began, in the
    /// order they were sent.
    pub(super) failed: Vec<Outgoing>,
}

/// A chat's commands: the one in flight, those waiting behind it, and those
/// accepted but not yet shown by the conversation.
#[derive(Default)]
pub(super) struct Commands {
    in_flight: Option<PendingCommand>,
    waiting: VecDeque<(PendingCommand, Command)>,
    settling: Vec<Settling>,
}

impl Commands {
    /// Whether nothing is on its way to the provider, nor waiting to go.
    pub(super) fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.waiting.is_empty()
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self) -> Option<&PendingCommand> {
        self.in_flight.as_ref()
    }

    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Takes `by` off the time since each accepted command was accepted.
    #[cfg(test)]
    pub(super) fn age(&mut self, by: Duration) {
        for settling in &mut self.settling {
            settling.accepted = settling.accepted.checked_sub(by).unwrap();
        }
    }

    /// Every command not yet answered, in the order it goes.
    pub(super) fn iter(&self) -> impl Iterator<Item = &PendingCommand> {
        self.in_flight
            .iter()
            .chain(self.waiting.iter().map(|(pending, _)| pending))
    }

    /// The command given last.
    pub(super) fn newest_mut(&mut self) -> Option<&mut PendingCommand> {
        match self.waiting.back_mut() {
            Some((pending, _)) => Some(pending),
            None => self.in_flight.as_mut(),
        }
    }

    /// Whether `command` would only repeat one still on its way: the newest
    /// of its kind asks the same. A model chosen, another, then the first
    /// again is a change of mind, and the last of them goes.
    pub(super) fn repeats(&self, command: &Command) -> bool {
        if let Command::Answer { request_id, .. } = command {
            // A request takes one answer; a second click, whichever choice it
            // was on, comes too late.
            return self
                .iter()
                .any(|pending| pending.request.as_ref() == Some(request_id));
        }
        self.iter()
            .filter(|pending| pending.same_kind(command))
            .last()
            .is_some_and(|pending| pending.repeat.as_ref() == Some(command))
    }

    /// Whether a command is carrying held follow-ups to the provider.
    pub(super) fn carrying_followups(&self) -> bool {
        self.iter().any(|pending| !pending.followups.is_empty())
    }

    /// Whether a command about this conversation's permissions is still on
    /// its way, which the permissions panel says it is waiting on.
    pub(super) fn awaiting_permissions(&self) -> bool {
        self.iter().any(|pending| {
            matches!(pending.choice, Some(Choice::PermissionMode(_)))
                || matches!(
                    pending.repeat,
                    Some(
                        Command::RefreshPermissions
                            | Command::UpdatePermissionSettings { .. }
                            | Command::DismissPermissionDenial { .. }
                    )
                )
        })
    }

    /// Adds a command to the end of the line. Returns it to be sent now when
    /// nothing is ahead of it and it can go, as [`Self::next`] says.
    pub(super) fn push(
        &mut self,
        mut pending: PendingCommand,
        command: Command,
        snapshot: &Snapshot,
    ) -> Option<(u64, Command)> {
        pending.at_once =
            command == Command::Interrupt && snapshot.busy && self.turn_coming(snapshot);
        self.waiting.push_back((pending, command));
        self.next(snapshot)
    }

    /// Takes the command in flight once its answer, for `id`, has come.
    pub(super) fn finish(&mut self, id: u64) -> Option<PendingCommand> {
        if self.in_flight.as_ref()?.id != id {
            return None;
        }
        self.in_flight.take()
    }

    /// Whether a message the provider took has yet to start the turn it
    /// asks for.
    pub(super) fn turn_coming(&self, snapshot: &Snapshot) -> bool {
        !snapshot.exited
            && self.settling.iter().any(|settling| {
                matches!(&settling.effect, Effect::Outgoing(outgoing)
                    if outgoing.in_transcript() && snapshot.turn_generation == outgoing.turns)
            })
    }

    /// Puts the next waiting command in flight, if none is, as `snapshot`
    /// allows. One that acts on the turn waits while a message sent ahead
    /// of it has yet to start that turn. A message for a turn that is over
    /// by the time it goes, or never came, goes as a message of its own, and
    /// a request that waited to stop it is dropped: there is nothing to stop.
    pub(super) fn next(&mut self, snapshot: &Snapshot) -> Option<(u64, Command)> {
        if self.in_flight.is_some() {
            return None;
        }
        let coming = self.turn_coming(snapshot);
        loop {
            let (pending, command) = self.waiting.front_mut()?;
            if acts_on_turn(command) && !pending.at_once {
                if coming {
                    pending.held = true;
                    return None;
                }
                if !snapshot.busy {
                    match command {
                        Command::Prompt { mode, .. } => {
                            *mode = PromptMode::Send;
                            if let Some(outgoing) = &mut pending.outgoing {
                                outgoing.mode = PromptMode::Send;
                            }
                        }
                        _ if pending.held => {
                            self.waiting.pop_front();
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            let (mut pending, command) = self.waiting.pop_front()?;
            if let Some(outgoing) = &mut pending.outgoing {
                outgoing.leaves_after(snapshot);
            }
            let id = pending.id;
            self.in_flight = Some(pending);
            return Some((id, command));
        }
    }

    /// Marks the messages the provider took whose turn has yet to begin as
    /// stopped by the reader, as [`Outgoing::stopped`] says.
    fn stop_before_turn(&mut self, snapshot: &Snapshot) {
        for settling in &mut self.settling {
            if let Effect::Outgoing(outgoing) = &mut settling.effect
                && outgoing.in_transcript()
                && snapshot.turn_generation == outgoing.turns
            {
                outgoing.stopped = true;
            }
        }
    }

    /// Withdraws the waiting messages, which must not reach the provider
    /// ahead of one it refused. Other waiting commands still go.
    pub(super) fn withdraw_messages(&mut self) -> Vec<Outgoing> {
        let mut withdrawn = Vec::new();
        self.waiting
            .retain_mut(|(pending, _)| match pending.outgoing.take() {
                Some(outgoing) => {
                    withdrawn.push(outgoing);
                    false
                }
                None => true,
            });
        withdrawn
    }

    /// Forgets every command, as when the conversation they were for is
    /// gone. Returns the messages that had not been accepted, in order.
    pub(super) fn abandon(&mut self) -> Vec<Outgoing> {
        self.settling.clear();
        self.in_flight
            .take()
            .into_iter()
            .chain(
                std::mem::take(&mut self.waiting)
                    .into_iter()
                    .map(|(pending, _)| pending),
            )
            .filter_map(|mut pending| pending.take_unsent())
            .collect()
    }

    /// Keeps what an accepted command changed shown until `snapshot`, or a
    /// later one, shows it. A message the conversation already shows needs
    /// nothing more.
    pub(super) fn accept(&mut self, pending: &mut PendingCommand, snapshot: &Snapshot) {
        let settling = |effect| Settling {
            effect,
            after: snapshot.generation,
            error: snapshot.error.clone(),
            accepted: Instant::now(),
        };
        let message = match pending.outgoing.take() {
            Some(outgoing) => Some(outgoing).filter(|outgoing| !outgoing.arrived),
            None => pending.carried.take(),
        };
        if let Some(outgoing) = message {
            self.settling.push(settling(Effect::Outgoing(outgoing)));
        }
        if let Some(choice) = pending.choice.clone() {
            self.settling.push(settling(Effect::Choice(choice)));
        }
    }

    /// Lets go of what `snapshot` now shows. A choice the provider took
    /// waits for the conversation to report it, as
    /// [`Settling::choice_settled_by`] says; a message for its own arrival,
    /// as [`Outgoing::settled_by`] does, unless its turn fails first. The
    /// message in flight stops being drawn once the conversation shows it.
    /// `items` says whether `snapshot` carries its transcript.
    pub(super) fn settle(&mut self, snapshot: &Snapshot, items: bool) -> Settled {
        let mut settled = Settled::default();
        let before = self.settling.len();
        for settling in std::mem::take(&mut self.settling) {
            match settling.effect {
                Effect::Choice(ref choice) => {
                    if !settling.choice_settled_by(choice, snapshot) {
                        self.settling.push(settling);
                    }
                }
                Effect::Outgoing(mut outgoing) => {
                    let waited = settling.accepted.elapsed() >= TURN_PATIENCE;
                    let quiet = outgoing.quiet_in(snapshot, waited);
                    if outgoing.settled_by(snapshot, items) || (quiet && !outgoing.stopped) {
                    } else if outgoing.failed_in(snapshot, waited) || quiet {
                        settled.failed.push(outgoing);
                    } else {
                        outgoing.busy_seen |= snapshot.busy;
                        self.settling.push(Settling {
                            effect: Effect::Outgoing(outgoing),
                            ..settling
                        });
                    }
                }
            }
        }
        settled.changed = self.settling.len() != before;
        if let Some(outgoing) = self
            .in_flight
            .as_mut()
            .and_then(|pending| pending.outgoing.as_mut())
            && !outgoing.arrived
            && outgoing.shown_in(snapshot, items)
        {
            outgoing.arrived = true;
            settled.changed = true;
        }
        settled
    }

    /// The reader's messages this window still shows as on their way, in
    /// the order they were sent.
    pub(super) fn outgoing(&self) -> impl Iterator<Item = &Outgoing> {
        self.settling
            .iter()
            .filter_map(|settling| match &settling.effect {
                Effect::Outgoing(outgoing) => Some(outgoing),
                Effect::Choice(_) => None,
            })
            .chain(self.unsent())
    }

    pub(super) fn outgoing_mut(&mut self) -> impl Iterator<Item = &mut Outgoing> {
        self.settling
            .iter_mut()
            .filter_map(|settling| match &mut settling.effect {
                Effect::Outgoing(outgoing) => Some(outgoing),
                Effect::Choice(_) => None,
            })
            .chain(
                self.in_flight
                    .iter_mut()
                    .chain(self.waiting.iter_mut().map(|(pending, _)| pending))
                    .filter_map(|pending| pending.outgoing.as_mut())
                    .filter(|outgoing| !outgoing.arrived),
            )
    }

    /// The messages the provider has not accepted yet, nor the conversation
    /// shown, which closing the chat now would leave unsent.
    pub(super) fn unsent(&self) -> impl Iterator<Item = &Outgoing> {
        self.iter()
            .filter_map(|pending| pending.outgoing.as_ref())
            .filter(|outgoing| !outgoing.arrived)
    }

    /// What the reader had said before the first message the provider has
    /// not accepted, which tells that message apart once it arrives.
    pub(super) fn first_unsent_after(&self) -> SentAfter {
        self.iter()
            .find(|pending| {
                pending
                    .outgoing
                    .as_ref()
                    .is_some_and(|outgoing| !outgoing.arrived)
            })
            .map(|pending| pending.sent_after.clone())
            .unwrap_or_default()
    }

    /// The choice of a model, or else of a permission mode, given last,
    /// accepted or not.
    fn chosen(&self, model: bool) -> Option<&Choice> {
        self.waiting
            .iter()
            .rev()
            .map(|(pending, _)| pending)
            .chain(self.in_flight.iter())
            .filter_map(|pending| pending.choice.as_ref())
            .chain(
                self.settling
                    .iter()
                    .rev()
                    .filter_map(|settling| match &settling.effect {
                        Effect::Choice(choice) => Some(choice),
                        Effect::Outgoing(_) => None,
                    }),
            )
            .find(|choice| matches!(choice, Choice::Model { .. }) == model)
    }

    pub(super) fn chosen_model(&self) -> Option<(&str, Option<&str>)> {
        match self.chosen(true)? {
            Choice::Model { model, reasoning } => Some((model.as_str(), reasoning.as_deref())),
            Choice::PermissionMode(_) => None,
        }
    }

    pub(super) fn chosen_permission_mode(&self) -> Option<&str> {
        match self.chosen(false)? {
            Choice::PermissionMode(mode) => Some(mode.as_str()),
            Choice::Model { .. } => None,
        }
    }
}

impl Runtime {
    /// Whether the conversation is running a turn, or a message on its way
    /// is about to start one. Either way a message written now does not
    /// start a turn of its own: it steers or waits for the next.
    pub(super) fn turn_under_way(&self) -> bool {
        self.snapshot.busy
            || self.commands.outgoing().next().is_some()
            || self.commands.carrying_followups()
    }

    /// Whether the conversation runs a turn that a message could steer or
    /// Stop could end: working, and not only waiting for the turn a message
    /// on its way is to start, as a conversation reads once it takes one.
    pub(super) fn turn_running(&self) -> bool {
        self.snapshot.busy && !self.commands.turn_coming(&self.snapshot)
    }

    /// Takes back into the composer what was lined up behind a turn the
    /// provider has yet to begin, as Stop does while the conversation waits
    /// for it: the messages not yet sent, and the held follow-ups, in the
    /// order written. A message the provider has, or is being given, stays:
    /// the turn it begins shows it, stopped. Whether Stop came before the
    /// turn.
    pub(super) fn withdraw_before_turn(&mut self) -> bool {
        if !self.snapshot.busy || !self.commands.turn_coming(&self.snapshot) {
            return false;
        }
        let snapshot = self.snapshot.clone();
        self.commands.stop_before_turn(&snapshot);
        let messages = self.commands.withdraw_messages();
        self.take_back_lined_up(messages);
        self.prepare_local_prompt();
        true
    }

    /// Lets go of what `snapshot` shows of this chat's commands, as
    /// [`Commands::settle`] does. A message whose turn failed before it began
    /// comes back to the composer, with the reason the conversation gives
    /// and everything written after it, which cannot be sent ahead of it.
    /// Whether anything shown changed.
    pub(super) fn settle_commands(&mut self, snapshot: &Snapshot, items: bool) -> bool {
        let settled = self.commands.settle(snapshot, items);
        if settled.failed.is_empty() {
            return settled.changed;
        }
        let mut messages = settled.failed;
        messages.extend(self.commands.withdraw_messages());
        self.take_back_lined_up(messages);
        true
    }

    /// Returns `messages` the provider did not take to the composer with the
    /// held follow-ups, all in the order they were written, ahead of
    /// whatever has been written since. Neither goes on without the other:
    /// sent apart, they would arrive out of order.
    pub(super) fn take_back_lined_up(&mut self, messages: Vec<Outgoing>) {
        let followups = std::mem::take(&mut self.followups);
        let returned = !followups.is_empty();
        let mut parts: Vec<_> = followups
            .into_iter()
            .map(|followup| (followup.id, followup.text, followup.attachments))
            .chain(messages.into_iter().map(|outgoing| {
                let text = outgoing.prompt.text().to_owned();
                (outgoing.written, text, outgoing.prompt.attachments)
            }))
            .collect();
        parts.sort_by_key(|(written, ..)| *written);
        self.prepend_parts(
            parts
                .into_iter()
                .map(|(_, text, attachments)| (text, attachments)),
        );
        if returned {
            let _ = self.present();
        }
    }

    /// Returns messages the provider did not take to the composer, in the
    /// order they were written, ahead of whatever has been written since.
    pub(super) fn restore_outgoing(&mut self, messages: Vec<Outgoing>) {
        self.prepend_parts(messages.into_iter().map(|outgoing| {
            let text = outgoing.prompt.text().to_owned();
            (text, outgoing.prompt.attachments)
        }));
    }

    /// Puts `parts`, each a message's text and files, in the composer in
    /// order, ahead of whatever is written there, each file once.
    fn prepend_parts(&mut self, parts: impl IntoIterator<Item = (String, Vec<Attachment>)>) {
        let mut text = String::new();
        let mut attachments: Vec<Attachment> = Vec::new();
        let mut any = false;
        for (part, files) in parts {
            any = true;
            let part = part.trim_end();
            if !part.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
            for attachment in files {
                if !attachments.contains(&attachment) {
                    attachments.push(attachment);
                }
            }
        }
        if any {
            self.prepend_to_draft(&text, attachments);
        }
    }

    /// The model and effort the reader last chose for this conversation,
    /// whether or not the provider has confirmed them yet.
    pub(super) fn chosen_model(&self) -> ChosenModel<'_> {
        match self.commands.chosen_model() {
            Some((id, reasoning)) => ChosenModel {
                id: Some(id),
                reasoning,
            },
            None => ChosenModel::reported(&self.snapshot),
        }
    }

    /// The permission mode the reader last chose, as [`Self::chosen_model`].
    pub(super) fn chosen_permission_mode(&self) -> Option<&str> {
        self.commands
            .chosen_permission_mode()
            .or(self.snapshot.permission_mode.as_deref())
    }

    /// The reader's own message this window draws in the transcript ahead of
    /// the conversation: a first message while its conversation starts, or
    /// one sent and not yet shown. Only one is prepared at a time.
    pub(super) fn local_prompt(&self) -> Option<&prompt::LocalPrompt> {
        self.queued
            .as_ref()
            .map(|queued| &queued.prompt)
            .or_else(|| {
                self.commands
                    .outgoing()
                    .find(|outgoing| outgoing.in_transcript())
                    .map(|outgoing| &outgoing.prompt)
            })
    }

    pub(super) fn local_prompt_mut(&mut self) -> Option<&mut prompt::LocalPrompt> {
        match &mut self.queued {
            Some(queued) => Some(&mut queued.prompt),
            None => self
                .commands
                .outgoing_mut()
                .find(|outgoing| outgoing.in_transcript())
                .map(|outgoing| &mut outgoing.prompt),
        }
    }

    /// Lets go of how the reader's local messages are drawn, which a hidden
    /// chat does not need.
    pub(super) fn forget_local_presentations(&mut self) {
        if let Some(queued) = &mut self.queued {
            queued.prompt.presentation = prompt::LocalPresentation::default();
        }
        for outgoing in self.commands.outgoing_mut() {
            outgoing.prompt.presentation = prompt::LocalPresentation::default();
        }
    }

    /// The message of the reader's on its way whose text is `source`, for
    /// copying it.
    pub(super) fn local_prompt_by_source(
        &self,
        source: &std::sync::Weak<prompt::LocalSource>,
    ) -> Option<&prompt::LocalPrompt> {
        self.queued
            .as_ref()
            .map(|queued| &queued.prompt)
            .into_iter()
            .chain(self.commands.outgoing().map(|outgoing| &outgoing.prompt))
            .find(|prompt| source.as_ptr() == Arc::as_ptr(&prompt.source))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        KNOWN_THREAD, chat_runtime, composer_key_messages, live_chat, permission_key, settle,
        visible_text,
    };
    use super::*;

    /// A shown conversation, given room to lay out, sending `text`.
    fn sending(app: &mut Cagents, card: CardId, text: &str) -> Target {
        let _ = app.workspace.resize_viewport(1200.0);
        let target = chat_runtime(app, card).target;
        app.chat_runtime_mut(target).unwrap().show(true);
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(text));
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        target
    }

    fn user_text(id: &str, text: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::User,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        })
    }

    /// The message leaves the composer and shows in the transcript the
    /// moment it is sent, while the provider has yet to answer, and the
    /// composer stays open for the next one.
    #[test]
    fn a_sent_message_shows_before_the_provider_answers() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = sending(&mut app, card, "Summarise the failing tests");
        let runtime = chat_runtime(&app, card);
        assert!(
            runtime.commands.in_flight().is_some(),
            "nothing answered yet"
        );
        assert!(runtime.composer.text().trim().is_empty());
        assert!(runtime.composer_available(false));
        assert!(runtime.turn_under_way());
        let text = visible_text(&app, card);
        assert!(
            text.iter()
                .any(|line| line.contains("Summarise the failing tests")),
            "{text:?}"
        );
        assert!(text.iter().any(|line| line == "Sending…"), "{text:?}");

        // Accepted, it stays shown until the conversation shows it itself,
        // so it never blinks out between the two.
        settle(&mut app, card, Ok(()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.commands.is_idle());
        assert_eq!(runtime.commands.outgoing().count(), 1);
        let mut published = (*runtime.snapshot).clone();
        published.generation += 1;
        published
            .items
            .push(user_text("sent", "Summarise the failing tests"));
        show_chat_snapshot_for_test(&mut app, card, published);
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert!(runtime.local_prompt().is_none());
    }

    /// A message the provider refuses goes back to the composer, ahead of
    /// whatever was written since, with the reason it was not sent.
    #[test]
    fn a_refused_message_returns_to_the_composer_with_the_reason() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = sending(&mut app, card, "First thought");
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Second thought"));
        settle(&mut app, card, Err("Codex is not answering".into()));
        let runtime = chat_runtime(&app, card);
        assert_eq!(
            runtime.composer.text().trim_end(),
            "First thought\nSecond thought"
        );
        assert_eq!(runtime.error.as_deref(), Some("Codex is not answering"));
        assert!(runtime.commands.is_idle());
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert!(!runtime.turn_under_way());
        let text = visible_text(&app, card);
        assert!(!text.iter().any(|line| line == "Sending…"), "{text:?}");
    }

    /// Commands given while one is on its way wait behind it in order and
    /// go one at a time; a repeat of one still waiting is not sent twice.
    #[test]
    fn commands_wait_their_turn_in_order() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        let mut snapshot = (*runtime.snapshot).clone();
        snapshot.generation += 1;
        snapshot.models = vec![
            chat::Model {
                id: "gpt-a".into(),
                label: "gpt-a".into(),
                reasoning: vec!["low".into(), "high".into()],
            },
            chat::Model {
                id: "gpt-b".into(),
                label: "gpt-b".into(),
                reasoning: vec!["low".into(), "high".into()],
            },
        ];
        snapshot.selected_model = Some("gpt-a".into());
        snapshot.reasoning = Some("low".into());
        show_chat_snapshot_for_test(&mut app, card, snapshot);
        sending(&mut app, card, "Start here");
        drop(app.update(Message::ChatUi(Action::Model(target, "gpt-b".into()))));
        drop(app.update(Message::ChatUi(Action::Model(target, "gpt-b".into()))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.commands.waiting(), 1, "the repeat is dropped");
        // The choice reads as made at once, keeping the effort it had.
        assert_eq!(
            runtime.chosen_model(),
            ChosenModel {
                id: Some("gpt-b"),
                reasoning: Some("low"),
            }
        );
        let first = runtime.commands.in_flight().unwrap().id;
        settle(&mut app, card, Ok(()));
        let runtime = chat_runtime(&app, card);
        let second = runtime.commands.in_flight().unwrap();
        assert_ne!(second.id, first);
        assert!(matches!(second.choice, Some(Choice::Model { .. })));
        assert_eq!(runtime.commands.waiting(), 0);

        // Refused, the choice is undone: the conversation's own model reads
        // as chosen again.
        settle(&mut app, card, Err("Model is not advertised".into()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.commands.is_idle());
        assert_eq!(runtime.chosen_model().id, Some("gpt-a"));
    }

    /// A message refused withdraws those waiting behind it, which would
    /// otherwise reach the provider out of order; they come back together.
    #[test]
    fn a_refused_message_brings_back_those_behind_it() {
        let mut commands = Commands::default();
        let snapshot = Snapshot::default();
        let message = |id, text: &str| {
            let command = Command::Prompt {
                text: text.into(),
                attachments: Vec::new(),
                mode: PromptMode::Queue,
            };
            let mut pending = PendingCommand::new(id, &command);
            pending.outgoing = Some(Outgoing::new(
                text.into(),
                Vec::new(),
                PromptMode::Queue,
                &snapshot,
            ));
            (pending, command)
        };
        let (pending, command) = message(1, "one");
        assert!(commands.push(pending, command, &snapshot).is_some());
        let refresh = Command::RefreshPermissions;
        assert!(
            commands
                .push(PendingCommand::new(2, &refresh), refresh.clone(), &snapshot)
                .is_none()
        );
        let (pending, command) = message(3, "three");
        assert!(commands.push(pending, command, &snapshot).is_none());
        assert!(commands.repeats(&refresh));
        let refused = commands.finish(1).unwrap();
        let withdrawn = commands.withdraw_messages();
        assert_eq!(refused.outgoing.unwrap().prompt.text(), "one");
        assert_eq!(
            withdrawn
                .iter()
                .map(|outgoing| outgoing.prompt.text())
                .collect::<Vec<_>>(),
            ["three"]
        );
        assert_eq!(commands.next(&snapshot).map(|(id, _)| id), Some(2));
    }

    /// Enter, where it sends, still sends while a command is on its way: a
    /// message waits behind it rather than breaking the draft's line.
    #[test]
    fn enter_sends_rather_than_breaking_the_line_while_a_command_is_on_its_way() {
        use keyboard::key::Named;
        let renderer = crate::app::tests::headless_renderer();
        for (enter_sends, modifiers) in [
            (true, keyboard::Modifiers::empty()),
            (false, keyboard::Modifiers::CTRL),
        ] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.settings.chat_enter_sends = enter_sends;
            let card = live_chat(&mut app, Some(KNOWN_THREAD));
            let target = sending(&mut app, card, "Look at the logs");
            app.chat_runtime_mut(target)
                .unwrap()
                .replace_draft(text_editor::Content::with_text("Then fix them"));
            assert!(!chat_runtime(&app, card).commands.is_idle());
            let messages = composer_key_messages(
                &app,
                card,
                760.0,
                permission_key(Named::Enter, modifiers),
                &renderer,
            );
            assert!(
                matches!(
                    messages.as_slice(),
                    [Message::ChatUi(Action::Submit(
                        _,
                        PromptMode::Steer | PromptMode::Queue
                    ))]
                ),
                "enter_sends {enter_sends}: {messages:?}"
            );
            for message in messages {
                drop(app.update(message));
            }
            let runtime = chat_runtime(&app, card);
            assert!(
                !runtime.composer.text().contains("Then fix them\n"),
                "{:?}",
                runtime.composer.text()
            );
        }
    }

    /// Publishes the conversation's next report, as `change` has it, and
    /// lets the window hear of it.
    fn publish(app: &mut Cagents, card: CardId, change: impl FnOnce(&mut Snapshot)) {
        let runtime = chat_runtime(app, card);
        let target = runtime.target;
        let mut snapshot = (*runtime.snapshot).clone();
        snapshot.generation += 1;
        change(&mut snapshot);
        show_chat_snapshot_for_test(app, card, snapshot);
        drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
            preparation::drain_fixture(runtime);
        }
    }

    /// The provider takes a message, then fails to start its turn: Codex
    /// refusing `turn/start` goes idle with an error and nothing more. The
    /// message comes back to the composer ahead of what was written since,
    /// rather than reading as sending for good with the composer steering a
    /// turn that never began.
    #[test]
    fn a_message_whose_turn_fails_to_begin_returns_to_the_composer() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = sending(&mut app, card, "Fix the build");
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("and the tests"));
        settle(&mut app, card, Ok(()));
        publish(&mut app, card, |snapshot| {
            snapshot.busy = true;
            snapshot.detail = "Waiting for provider turn".into();
        });
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.commands.outgoing().count(), 1, "still on its way");
        assert!(runtime.composer.text().trim_end() == "and the tests");
        publish(&mut app, card, |snapshot| {
            snapshot.busy = false;
            snapshot.detail = "Turn failed".into();
            snapshot.error = Some("Codex RPC error: the thread is closed".into());
        });
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert!(!runtime.turn_under_way());
        assert_eq!(
            runtime.composer.text().trim_end(),
            "Fix the build\nand the tests"
        );
        assert!(
            runtime
                .reported_errors()
                .any(|error| error.contains("the thread is closed"))
        );
        let text = visible_text(&app, card);
        assert!(!text.iter().any(|line| line == "Sending…"), "{text:?}");

        // A provider that only goes idle, having started the turn, has
        // taken the message.
        let target = sending(&mut app, card, "Fix the build");
        settle(&mut app, card, Ok(()));
        publish(&mut app, card, |snapshot| {
            snapshot.busy = true;
            snapshot.turn_generation += 1;
        });
        publish(&mut app, card, |snapshot| {
            snapshot.busy = false;
            snapshot.error = Some("Codex RPC error: rate limited".into());
        });
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert!(
            runtime.composer.text().trim_end().is_empty(),
            "{:?}",
            runtime.composer.text()
        );
    }

    /// Sent again after failing, a message can fail the same way, with the
    /// error it failed with before and no report of the conversation working
    /// in between. Once its turn has had time to begin, that error is its
    /// own. A conversation idle with no error and no turn has nothing more
    /// to show, and what waited for the turn goes on.
    #[test]
    fn a_message_whose_turn_never_comes_is_not_waited_for_for_good() {
        const CLOSED: &str = "Codex RPC error: the thread is closed";
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        publish(&mut app, card, |snapshot| {
            snapshot.capabilities.steer = true;
            snapshot.error = Some(CLOSED.into());
        });
        let target = sending(&mut app, card, "Fix the build");
        settle(&mut app, card, Ok(()));
        publish(&mut app, card, |snapshot| snapshot.busy = false);
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.commands.outgoing().count(), 1, "a report may lag");
        assert!(!runtime.turn_running(), "no turn to steer or stop yet");
        app.chat_runtime_mut(target)
            .unwrap()
            .commands
            .age(TURN_PATIENCE);
        drop(app.update(Message::ChatUi(Action::CommandsDue(target))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert_eq!(runtime.composer.text().trim_end(), "Fix the build");

        publish(&mut app, card, |snapshot| snapshot.error = None);
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(""));
        sending(&mut app, card, "Fix the build");
        settle(&mut app, card, Ok(()));
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("and the tests"));
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Steer))));
        publish(&mut app, card, |snapshot| snapshot.busy = false);
        assert_eq!(chat_runtime(&app, card).commands.waiting(), 1, "held");
        app.chat_runtime_mut(target)
            .unwrap()
            .commands
            .age(TURN_PATIENCE);
        drop(app.update(Message::ChatUi(Action::CommandsDue(target))));
        let runtime = chat_runtime(&app, card);
        let in_flight = runtime.commands.in_flight().expect("released");
        let outgoing = in_flight.outgoing.as_ref().unwrap();
        assert_eq!(outgoing.prompt.text(), "and the tests");
        assert_eq!(outgoing.mode, PromptMode::Send);
        assert!(runtime.composer.text().trim().is_empty());
    }

    /// A message written while the one before it is still on its way acts
    /// on the turn that one starts. It waits until the provider shows the
    /// turn has begun, since Codex refuses to steer a turn it has not
    /// started; once it has, the message steers it. Should the turn come and
    /// go first, the message is sent as one of its own.
    #[test]
    fn a_message_written_while_one_is_sending_waits_for_its_turn() {
        for turn_over in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some(KNOWN_THREAD));
            let target = chat_runtime(&app, card).target;
            publish(&mut app, card, |snapshot| {
                snapshot.capabilities.steer = true;
            });
            sending(&mut app, card, "Look at the logs");
            app.chat_runtime_mut(target)
                .unwrap()
                .replace_draft(text_editor::Content::with_text("Then fix them"));
            drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Steer))));
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.commands.waiting(), 1);
            assert!(runtime.composer.text().trim().is_empty());

            settle(&mut app, card, Ok(()));
            // Accepted, the first has yet to start its turn: the daemon
            // reads as working, but the provider has no turn to steer.
            publish(&mut app, card, |snapshot| snapshot.busy = true);
            let runtime = chat_runtime(&app, card);
            assert!(runtime.commands.in_flight().is_none(), "held for the turn");
            assert_eq!(runtime.commands.waiting(), 1);
            assert!(!runtime.commands.is_idle());

            publish(&mut app, card, |snapshot| {
                snapshot.turn_generation += 1;
                snapshot.busy = !turn_over;
                snapshot.items.push(user_text("u-logs", "Look at the logs"));
            });
            let runtime = chat_runtime(&app, card);
            let in_flight = runtime.commands.in_flight().expect("released");
            let outgoing = in_flight.outgoing.as_ref().unwrap();
            assert_eq!(outgoing.prompt.text(), "Then fix them");
            assert_eq!(
                outgoing.mode,
                if turn_over {
                    PromptMode::Send
                } else {
                    PromptMode::Steer
                },
                "turn over {turn_over}"
            );
            assert_eq!(runtime.commands.waiting(), 0);
            if turn_over {
                continue;
            }
            // Steered in, it has had its place once that turn is over, even
            // from a provider that never shows it.
            settle(&mut app, card, Ok(()));
            assert_eq!(chat_runtime(&app, card).commands.outgoing().count(), 1);
            publish(&mut app, card, |snapshot| snapshot.busy = false);
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.commands.outgoing().count(), 0);
            assert!(runtime.commands.is_idle());
        }
    }

    /// A conversation working while the provider has a message whose turn
    /// has yet to begin, with `lined_up` written behind it in order: a
    /// message to steer that turn, which waits for it, and one to queue,
    /// held here since the provider keeps no queue.
    fn waiting_for_its_turn(lined_up: [(&str, PromptMode); 2]) -> (Cagents, CardId, Target) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        publish(&mut app, card, |snapshot| {
            snapshot.capabilities.steer = true;
            snapshot.capabilities.queue = false;
        });
        sending(&mut app, card, "Look at the logs");
        settle(&mut app, card, Ok(()));
        publish(&mut app, card, |snapshot| snapshot.busy = true);
        for (text, mode) in lined_up {
            app.chat_runtime_mut(target)
                .unwrap()
                .replace_draft(text_editor::Content::with_text(text));
            drop(app.update(Message::ChatUi(Action::Submit(target, mode))));
        }
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.turn_running(), "no turn to steer yet");
        assert_eq!(runtime.commands.waiting(), 1);
        assert_eq!(runtime.followups.len(), 1, "held for the next turn");
        (app, card, target)
    }

    /// Stop is offered whenever the conversation works, also while it waits
    /// for a message's turn to begin, which can be slow or never come.
    /// Pressed then, it goes to the provider at once, which holds it until
    /// the turn has begun, and takes back into the composer what was lined
    /// up behind that turn, in the order written: a message not yet sent,
    /// and the held follow-up, which would otherwise go as a turn of its
    /// own. The message the provider has stays where it is: the turn it
    /// begins shows it, stopped, so it is neither written twice nor sent
    /// again by accident. Nothing reads as refused.
    #[test]
    fn stop_before_the_turn_begins_takes_back_what_was_lined_up_behind_it() {
        let steer = ("Then fix them", PromptMode::Steer);
        let queue = ("And add a test", PromptMode::Queue);
        for (lined_up, returned) in [
            ([steer, queue], "Then fix them\nAnd add a test"),
            ([queue, steer], "And add a test\nThen fix them"),
        ] {
            let (mut app, card, target) = waiting_for_its_turn(lined_up);
            drop(app.update(Message::ChatUi(Action::Interrupt(target))));
            let runtime = chat_runtime(&app, card);
            let stop = runtime.commands.in_flight().expect("the provider is told");
            assert_eq!(stop.repeat, Some(Command::Interrupt));
            assert_eq!(runtime.commands.waiting(), 0);
            let shown: Vec<_> = runtime
                .commands
                .outgoing()
                .map(|outgoing| outgoing.prompt.text().to_owned())
                .collect();
            assert_eq!(shown, ["Look at the logs"], "the provider has it");
            assert_eq!(runtime.composer.text().trim_end(), returned);
            assert!(runtime.followups.is_empty());
            assert!(runtime.error.is_none());

            // The turn begins and is stopped, showing the message; nothing
            // lined up behind it goes in its place.
            settle(&mut app, card, Ok(()));
            publish(&mut app, card, |snapshot| {
                snapshot.turn_generation += 1;
                snapshot.items.push(user_text("u-logs", "Look at the logs"));
            });
            publish(&mut app, card, |snapshot| snapshot.busy = false);
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.commands.outgoing().count(), 0);
            assert!(runtime.commands.is_idle());
            assert!(runtime.followups.is_empty());
            assert_eq!(runtime.composer.text().trim_end(), returned);
        }
    }

    /// A refused steer and a follow-up held for the next turn come back
    /// together in the order they were written, whichever was first.
    #[test]
    fn a_refused_steer_comes_back_with_the_held_follow_up_in_written_order() {
        let steer = ("Then fix them", PromptMode::Steer);
        let queue = ("And add a test", PromptMode::Queue);
        for (lined_up, returned) in [
            ([steer, queue], "Then fix them\nAnd add a test"),
            ([queue, steer], "And add a test\nThen fix them"),
        ] {
            let (mut app, card, _) = waiting_for_its_turn(lined_up);
            publish(&mut app, card, |snapshot| {
                snapshot.turn_generation += 1;
                snapshot.items.push(user_text("u-logs", "Look at the logs"));
            });
            assert!(
                chat_runtime(&app, card).commands.in_flight().is_some(),
                "the steer goes once its turn has begun"
            );
            settle(&mut app, card, Err("Codex is not answering".into()));
            let runtime = chat_runtime(&app, card);
            assert!(runtime.followups.is_empty());
            assert!(runtime.commands.is_idle(), "nothing sent in its place");
            assert_eq!(runtime.composer.text().trim_end(), returned);
        }
    }

    /// A turn stopped before it began may never begin. The message the
    /// provider had then shows nowhere, so once its turn has had its time,
    /// it comes back to the composer ahead of what was written after it.
    #[test]
    fn a_message_whose_stopped_turn_never_begins_comes_back() {
        let (mut app, card, target) = waiting_for_its_turn([
            ("Then fix them", PromptMode::Steer),
            ("And add a test", PromptMode::Queue),
        ]);
        drop(app.update(Message::ChatUi(Action::Interrupt(target))));
        settle(&mut app, card, Ok(()));
        publish(&mut app, card, |snapshot| snapshot.busy = false);
        assert_eq!(
            chat_runtime(&app, card).commands.outgoing().count(),
            1,
            "a report may lag"
        );
        app.chat_runtime_mut(target)
            .unwrap()
            .commands
            .age(TURN_PATIENCE);
        drop(app.update(Message::ChatUi(Action::CommandsDue(target))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert!(runtime.commands.is_idle());
        assert_eq!(
            runtime.composer.text().trim_end(),
            "Look at the logs\nThen fix them\nAnd add a test"
        );
    }

    /// A message written to steer the turn the one before it starts goes as
    /// a message of its own when that turn is over before it goes, as one
    /// can be by the time the provider answers for the first.
    #[test]
    fn a_message_for_a_turn_already_over_goes_as_its_own() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        publish(&mut app, card, |snapshot| {
            snapshot.capabilities.steer = true
        });
        sending(&mut app, card, "Look at the logs");
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Then fix them"));
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Steer))));
        publish(&mut app, card, |snapshot| {
            snapshot.turn_generation += 1;
            snapshot.items.push(user_text("u-logs", "Look at the logs"));
        });
        settle(&mut app, card, Ok(()));
        let runtime = chat_runtime(&app, card);
        let in_flight = runtime.commands.in_flight().expect("sent");
        let outgoing = in_flight.outgoing.as_ref().unwrap();
        assert_eq!(outgoing.prompt.text(), "Then fix them");
        assert_eq!(outgoing.mode, PromptMode::Send);
    }

    /// Changing one's mind about a model while the first change is on its
    /// way sends each change; only a repeat of the latest is dropped.
    #[test]
    fn a_choice_changed_back_is_not_taken_for_a_repeat() {
        let mut commands = Commands::default();
        let snapshot = Snapshot::default();
        let model = |model: &str| Command::SetModel {
            model: model.into(),
            reasoning: None,
        };
        let mut id = 0;
        let mut give = |commands: &mut Commands, command: Command| {
            if commands.repeats(&command) {
                return false;
            }
            id += 1;
            let _ = commands.push(PendingCommand::new(id, &command), command, &snapshot);
            true
        };
        assert!(give(&mut commands, model("a")));
        assert!(give(&mut commands, model("b")));
        assert!(give(&mut commands, model("a")), "a change of mind");
        assert!(!give(&mut commands, model("a")), "a repeat");
        assert!(give(&mut commands, Command::RefreshPermissions));
        assert!(!give(&mut commands, Command::RefreshPermissions));
        assert_eq!(commands.iter().count(), 4);
        assert_eq!(commands.chosen_model(), Some(("a", None)));
    }

    /// A model the provider took reads as chosen until the conversation
    /// reports it, rather than flicking back to the old one for the reports
    /// in between; a provider that says it could not make the change undoes
    /// it.
    #[test]
    fn an_accepted_choice_holds_until_the_conversation_reports_it() {
        let mut commands = Commands::default();
        let mut snapshot = Snapshot {
            selected_model: Some("old".into()),
            ..Snapshot::default()
        };
        let choose = |commands: &mut Commands, snapshot: &Snapshot, model: &str| {
            let command = Command::SetModel {
                model: model.into(),
                reasoning: None,
            };
            let id = commands.iter().count() as u64 + 1;
            let (id, _) = commands
                .push(PendingCommand::new(id, &command), command, snapshot)
                .unwrap();
            let mut pending = commands.finish(id).unwrap();
            commands.accept(&mut pending, snapshot);
        };
        choose(&mut commands, &snapshot, "new");
        for _ in 0..3 {
            snapshot.generation += 1;
            let _ = commands.settle(&snapshot, true);
            assert_eq!(commands.chosen_model(), Some(("new", None)));
        }
        snapshot.generation += 1;
        snapshot.selected_model = Some("new".into());
        let _ = commands.settle(&snapshot, true);
        assert_eq!(commands.chosen_model(), None, "the report says it now");

        choose(&mut commands, &snapshot, "other");
        snapshot.generation += 1;
        snapshot.error = Some("Claude set_model: unavailable".into());
        let _ = commands.settle(&snapshot, true);
        assert_eq!(commands.chosen_model(), None, "the provider refused it");
    }

    /// A message refused while follow-ups wait behind it brings them back
    /// too, after it, rather than sending them on without it.
    #[test]
    fn a_refused_message_brings_back_the_follow_ups_held_behind_it() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        publish(&mut app, card, |snapshot| {
            snapshot.capabilities.steer = false;
            snapshot.capabilities.queue = false;
        });
        sending(&mut app, card, "First thought");
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Second thought"));
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Queue))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.followups.len(), 1, "held for the next turn");
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Third"));
        settle(&mut app, card, Err("Codex is not answering".into()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.followups.is_empty());
        assert!(runtime.commands.is_idle(), "nothing sent in its place");
        assert_eq!(
            runtime.composer.text().trim_end(),
            "First thought\nSecond thought\nThird"
        );
    }

    /// Where the conversation shows a message before the provider's answer
    /// for it has come, as Claude's can, the message is drawn once: the
    /// conversation's own row replaces the one on its way.
    #[test]
    fn a_message_shown_before_its_answer_is_drawn_once() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = sending(&mut app, card, "Summarise the failing tests");
        publish(&mut app, card, |snapshot| {
            snapshot.busy = true;
            snapshot.turn_generation += 1;
            snapshot
                .items
                .push(user_text("sent", "Summarise the failing tests"));
        });
        let runtime = chat_runtime(&app, card);
        assert!(runtime.commands.in_flight().is_some(), "not answered yet");
        assert_eq!(runtime.commands.outgoing().count(), 0);
        assert!(
            runtime.local_prompt().is_none(),
            "the conversation's row is the one drawn"
        );
        let text = visible_text(&app, card);
        assert!(!text.iter().any(|line| line == "Sending…"), "{text:?}");
        settle(&mut app, card, Ok(()));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.commands.is_idle());
        assert_eq!(runtime.commands.outgoing().count(), 0);
    }
}
