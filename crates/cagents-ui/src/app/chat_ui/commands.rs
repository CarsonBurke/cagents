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
use super::*;
use std::collections::VecDeque;

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
        }
    }

    /// Marks this command as carrying the held follow-ups `ids`, which
    /// together make `message`.
    pub(super) fn carry_followups(&mut self, ids: Vec<u64>, message: Outgoing) {
        self.followups = ids;
        self.carried = Some(message);
    }

    /// Whether giving `command` now would only ask again for what this
    /// command already asks.
    fn repeated_by(&self, command: &Command) -> bool {
        if let Command::Answer { request_id, .. } = command {
            // A request takes one answer; a second click, whichever choice it
            // was on, comes too late.
            return self.request.as_ref() == Some(request_id);
        }
        self.repeat.as_ref() == Some(command)
    }
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
    /// The reader's newest message the transcript held when this was sent.
    /// Another in its place is this one arriving.
    newest_user: Option<String>,
    /// The messages the provider held for later when this was sent. A new
    /// one is this one arriving there.
    held: Vec<String>,
    /// The turns the provider had started when this was sent.
    turns: u64,
}

impl Outgoing {
    pub(super) fn new(
        text: String,
        attachments: Vec<Attachment>,
        mode: PromptMode,
        snapshot: &Snapshot,
    ) -> Self {
        Self {
            preview: prompt::queue_preview(&text),
            prompt: prompt::LocalPrompt::new(text, attachments),
            mode,
            newest_user: newest_user_message(snapshot).map(str::to_owned),
            held: held_messages(snapshot).map(str::to_owned).collect(),
            turns: snapshot.turn_generation,
        }
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
    /// shows nothing more. A snapshot without its items, as a hidden chat's
    /// is, can only show where the message is held.
    fn settled_by(&self, snapshot: &Snapshot, items: bool) -> bool {
        (items && newest_user_message(snapshot) != self.newest_user.as_deref())
            || held_messages(snapshot).any(|id| !self.held.iter().any(|held| held == id))
            || (snapshot.turn_generation > self.turns && !snapshot.busy)
            || (items && snapshot.has_later)
            || snapshot.exited
    }
}

fn held_messages(snapshot: &Snapshot) -> impl Iterator<Item = &str> {
    snapshot
        .queued_messages
        .iter()
        .map(|message| message.id.as_str())
}

fn newest_user_message(snapshot: &Snapshot) -> Option<&str> {
    snapshot
        .items
        .iter()
        .rev()
        .find(|item| {
            item.role == chat::Role::User
                && matches!(
                    item.content,
                    ItemContent::Text { .. } | ItemContent::Image { .. }
                )
        })
        .map(|item| item.id.as_str())
}

/// What a command the provider accepted changed, shown until the
/// conversation shows it too.
enum Effect {
    Outgoing(Outgoing),
    Choice(Choice),
}

struct Settling {
    effect: Effect,
    /// The generation presented when the provider accepted it. A choice is
    /// in effect by any later one.
    after: u64,
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
    /// Whether nothing is on its way to the provider.
    pub(super) fn is_idle(&self) -> bool {
        self.in_flight.is_none()
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self) -> Option<&PendingCommand> {
        self.in_flight.as_ref()
    }

    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        self.waiting.len()
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

    /// Whether `command` would only repeat one still on its way.
    pub(super) fn repeats(&self, command: &Command) -> bool {
        self.iter().any(|pending| pending.repeated_by(command))
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
    /// nothing is ahead of it.
    pub(super) fn push(
        &mut self,
        pending: PendingCommand,
        command: Command,
    ) -> Option<(u64, Command)> {
        if self.in_flight.is_some() {
            self.waiting.push_back((pending, command));
            return None;
        }
        let id = pending.id;
        self.in_flight = Some(pending);
        Some((id, command))
    }

    /// Takes the command in flight once its answer, for `id`, has come.
    pub(super) fn finish(&mut self, id: u64) -> Option<PendingCommand> {
        if self.in_flight.as_ref()?.id != id {
            return None;
        }
        self.in_flight.take()
    }

    /// Puts the next waiting command in flight, if none is.
    pub(super) fn next(&mut self) -> Option<(u64, Command)> {
        if self.in_flight.is_some() {
            return None;
        }
        let (pending, command) = self.waiting.pop_front()?;
        let id = pending.id;
        self.in_flight = Some(pending);
        Some((id, command))
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
            .filter_map(|pending| pending.outgoing)
            .collect()
    }

    /// Keeps what an accepted command changed shown until `snapshot`, or a
    /// later one, shows it.
    pub(super) fn accept(&mut self, pending: &mut PendingCommand, snapshot: &Snapshot) {
        let after = snapshot.generation;
        if let Some(outgoing) = pending.outgoing.take().or(pending.carried.take()) {
            self.settling.push(Settling {
                effect: Effect::Outgoing(outgoing),
                after,
            });
        }
        if let Some(choice) = pending.choice.clone() {
            self.settling.push(Settling {
                effect: Effect::Choice(choice),
                after,
            });
        }
        self.settle(snapshot, true);
    }

    /// Lets go of what `snapshot` now shows. A choice the provider took is
    /// in effect by its next publication, whatever it reports; a message
    /// waits for its own arrival, as [`Outgoing::settled_by`] describes.
    /// `items` says whether `snapshot` carries its transcript.
    pub(super) fn settle(&mut self, snapshot: &Snapshot, items: bool) -> bool {
        let before = self.settling.len();
        self.settling.retain(|settling| match &settling.effect {
            Effect::Choice(choice) => {
                snapshot.generation <= settling.after && !choice.shown_by(snapshot)
            }
            Effect::Outgoing(outgoing) => !outgoing.settled_by(snapshot, items),
        });
        self.settling.len() != before
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
                    .filter_map(|pending| pending.outgoing.as_mut()),
            )
    }

    /// The messages the provider has not accepted yet, which closing the
    /// chat now would leave unsent.
    pub(super) fn unsent(&self) -> impl Iterator<Item = &Outgoing> {
        self.iter().filter_map(|pending| pending.outgoing.as_ref())
    }

    /// What the reader had said before the first message the provider has
    /// not accepted, which tells that message apart once it arrives.
    pub(super) fn first_unsent_after(&self) -> SentAfter {
        self.iter()
            .find(|pending| pending.outgoing.is_some())
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

    /// Returns messages the provider did not take to the composer, in the
    /// order they were written, ahead of whatever has been written since.
    pub(super) fn restore_outgoing(&mut self, messages: Vec<Outgoing>) {
        if messages.is_empty() {
            return;
        }
        let mut text = String::new();
        let mut attachments: Vec<Attachment> = Vec::new();
        for outgoing in messages {
            let part = outgoing.prompt.text().trim_end();
            if !part.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
            for attachment in &outgoing.prompt.attachments {
                if !attachments.contains(attachment) {
                    attachments.push(attachment.clone());
                }
            }
        }
        self.prepend_to_draft(&text, attachments);
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
        assert!(commands.push(pending, command).is_some());
        let refresh = Command::RefreshPermissions;
        assert!(
            commands
                .push(PendingCommand::new(2, &refresh), refresh.clone())
                .is_none()
        );
        let (pending, command) = message(3, "three");
        assert!(commands.push(pending, command).is_none());
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
        assert_eq!(commands.next().map(|(id, _)| id), Some(2));
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
}
