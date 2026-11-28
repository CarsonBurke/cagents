//! `/btw`: a quick question asked beside the conversation.
//!
//! The provider answers it from the conversation's context, without tools,
//! while a turn keeps running, and the answer never enters the transcript.
//! So it is shown in a card docked on the composer rather than as a message,
//! and goes when the reader puts it away. Asking again while the card is open
//! carries the questions it already answered, as the CLI does; putting it
//! away forgets them.

use super::*;
use cosmic::iced::widget::markdown as rich;

/// The name the composer answers itself, whatever the provider lists.
pub(super) const COMMAND: &str = "btw";

/// What the slash menu says `/btw` does.
pub(super) const DESCRIPTION: &str = "Ask a quick question that stays out of the conversation";

/// How long a question waits for its answer before the card says none is
/// coming. Claude Code gives up on one after ten minutes; this is the bound
/// for a provider that never says.
const ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(12 * 60);

/// The tallest an answer grows before it scrolls, so a long one leaves the
/// transcript in view.
const ANSWER_HEIGHT: f32 = 280.0;

/// The question a draft reading `/btw [question]` asks, which may be empty,
/// or `None` for any other draft.
pub(super) fn invocation(draft: &str) -> Option<&str> {
    let rest = draft.trim().strip_prefix(completion::COMMAND_SIGIL)?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    rest[..end]
        .eq_ignore_ascii_case(COMMAND)
        .then(|| rest[end..].trim())
}

/// The side questions open on a composer.
pub(super) struct Thread {
    /// Exchanges answered before the current one, oldest first.
    earlier: Vec<chat::SideExchange>,
    current: Exchange,
}

struct Exchange {
    id: String,
    question: String,
    state: State,
}

enum State {
    Asking,
    Answered {
        text: Arc<str>,
        markdown: Vec<rich::Item>,
    },
    Failed(String),
}

impl Thread {
    /// Asks `question` after what `open` already answered, if anything.
    /// `open` stays as it is, so a question that can't be asked leaves the
    /// card showing what it did.
    fn ask(open: Option<&Self>, question: String) -> (Self, chat::SideQuestion) {
        let mut earlier = Vec::new();
        if let Some(open) = open {
            earlier.clone_from(&open.earlier);
            if let State::Answered { text, .. } = &open.current.state {
                earlier.push(chat::SideExchange {
                    question: open.current.question.clone(),
                    answer: text.to_string(),
                });
            }
        }
        // The newest exchanges are what a follow-up is most likely about.
        let excess = earlier
            .len()
            .saturating_sub(chat::SideQuestion::MAX_HISTORY);
        earlier.drain(..excess);
        while earlier
            .iter()
            .map(|exchange| exchange.question.len() + exchange.answer.len())
            .sum::<usize>()
            > chat::SideQuestion::MAX_HISTORY_BYTES
        {
            earlier.remove(0);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let request = chat::SideQuestion {
            id: id.clone(),
            question: question.clone(),
            history: earlier.clone(),
        };
        let thread = Self {
            earlier,
            current: Exchange {
                id,
                question,
                state: State::Asking,
            },
        };
        (thread, request)
    }

    /// Takes `answer` if it is for the question still waiting. Any other
    /// window's question, or one this card has moved on from, is not.
    fn answer(&mut self, answer: chat::SideAnswer) -> bool {
        if answer.id != self.current.id || !matches!(self.current.state, State::Asking) {
            return false;
        }
        self.current.state = match answer.outcome {
            chat::SideOutcome::Answered(text) => State::Answered {
                markdown: rich::parse(&text).collect(),
                text: text.into(),
            },
            chat::SideOutcome::Failed(reason) => State::Failed(reason),
        };
        true
    }

    /// Settles a question still waiting that will get no answer.
    fn fail(&mut self, id: Option<&str>, reason: impl Into<String>) {
        if id.is_none_or(|id| id == self.current.id) && matches!(self.current.state, State::Asking)
        {
            self.current.state = State::Failed(reason.into());
        }
    }

    fn answer_text(&self) -> Option<Arc<str>> {
        match &self.current.state {
            State::Answered { text, .. } => Some(text.clone()),
            _ => None,
        }
    }
}

impl Cagents {
    /// Asks the draft's `/btw` question, beside whatever the conversation is
    /// doing. Nothing is queued: the answer does not wait for a turn.
    pub(super) fn ask_side_question(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let draft = runtime.composer.text();
        let Some(question) = invocation(&draft) else {
            return Task::none();
        };
        let refusal = if runtime.is_draft() {
            Some("Send a first message, then ask a side question.")
        } else if !runtime.snapshot.capabilities.side_questions {
            Some(chat::SideQuestion::unavailable(
                runtime.session.launch.provider,
            ))
        } else if question.is_empty() {
            Some("Write a question after /btw.")
        } else if !runtime.attachments.is_empty() {
            Some("A side question can't carry attachments. Remove them, or send a message.")
        } else if runtime.pastes_pending > 0 {
            Some("Wait for the paste to finish, then ask again.")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            runtime.composer_note = Some(refusal.into());
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            runtime.composer_note =
                Some("This conversation isn't connected. Ask again once it reconnects.".into());
            return Task::none();
        };
        let (thread, request) = Thread::ask(runtime.side_question.as_ref(), question.to_owned());
        if let Err(refusal) = request.validate() {
            runtime.composer_note = Some(refusal.into());
            return Task::none();
        }
        runtime.side_question = Some(thread);
        runtime.replace_draft(text_editor::Content::new());
        runtime.composer_note = None;
        let id = request.id.clone();
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || handle.send(Command::SideQuestion(request)))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            Message::ChatUi(Action::SideQuestionSent(target, id, result))
        })
    }

    /// The provider took the question, or said why not. Taken, its answer
    /// arrives on its own, or the card stops waiting at the deadline.
    pub(super) fn side_question_sent(
        &mut self,
        target: Target,
        id: String,
        result: Result<(), String>,
    ) -> Task<Message> {
        let Some(thread) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.side_question.as_mut())
        else {
            return Task::none();
        };
        match result {
            Err(reason) => {
                thread.fail(Some(&id), reason);
                Task::none()
            }
            Ok(()) => cosmic::task::future(async move {
                tokio::time::sleep(ANSWER_DEADLINE).await;
                Message::ChatUi(Action::SideQuestionExpired(target, id))
            }),
        }
    }

    /// The deadline for question `id` passed; if it is still waiting, the
    /// card says no answer is coming.
    pub(super) fn side_question_expired(&mut self, target: Target, id: &str) {
        let provider = match self.chat_runtime_mut(target) {
            Some(runtime) => runtime.session.launch.provider.label(),
            None => return,
        };
        if let Some(thread) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.side_question.as_mut())
        {
            thread.fail(
                Some(id),
                format!("{provider} didn't answer in time. Try again."),
            );
        }
    }

    pub(super) fn side_answered(&mut self, target: Target, answer: chat::SideAnswer) {
        if let Some(thread) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.side_question.as_mut())
        {
            thread.answer(answer);
        }
    }

    /// Puts the card away, and the keyboard back in the composer.
    pub(super) fn dismiss_side_question(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.side_question.take().is_none() {
            return Task::none();
        }
        iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
            .map(cosmic::Action::App)
    }

    pub(super) fn copy_side_answer(&mut self, target: Target) -> Task<Message> {
        self.chat_runtime_mut(target)
            .and_then(|runtime| runtime.side_question.as_ref())
            .and_then(Thread::answer_text)
            .map_or_else(Task::none, text_copy::write_shared_literal)
    }

    /// The focused pane's side question, which an Escape nothing else took
    /// puts away.
    pub(in crate::app) fn focused_side_question(&self) -> Option<Target> {
        let card = self.workspace.focused()?;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        runtime.side_question.as_ref().map(|_| runtime.target)
    }
}

impl Runtime {
    /// A question that will get no answer now that the connection is gone.
    pub(super) fn side_question_lost(&mut self) {
        if let Some(thread) = &mut self.side_question {
            thread.fail(
                None,
                "The conversation disconnected before the answer arrived. Ask again once it reconnects.",
            );
        }
    }

    /// A question that will get no answer because the provider stopped.
    pub(super) fn side_question_stopped(&mut self) {
        let provider = self.session.launch.provider.label();
        if let Some(thread) = &mut self.side_question {
            thread.fail(None, format!("{provider} stopped before it answered."));
        }
    }

    /// The card docked on the composer: the question, then its answer, the
    /// wait for one, or why there is none.
    pub(super) fn side_question_view(&self) -> Option<Element<'_, Message>> {
        let thread = self.side_question.as_ref()?;
        let target = self.target;
        let provider = self.session.launch.provider.label();
        let mut header: Vec<Element<'_, Message>> = vec![
            design::caption("Side question · not added to the conversation")
                .width(Length::Fill)
                .into(),
        ];
        if thread.answer_text().is_some() {
            header.push(small_icon(
                Icon::Copy,
                "Copy answer",
                Action::CopySideAnswer(target),
            ));
        }
        header.push(small_icon(
            Icon::Close,
            crate::interaction::RichTip::new("Dismiss").shortcut("Escape"),
            Action::DismissSideQuestion(target),
        ));
        let body: Element<'_, Message> = match &thread.current.state {
            State::Asking => widget::row::with_children(vec![
                crate::ui_controls::activity_spinner(
                    f32::from(design::COMPACT_ICON_SIZE),
                    design::accent_color,
                ),
                design::caption(format!("{provider} is answering…")).into(),
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .into(),
            State::Answered { markdown, .. } => widget::container(
                widget::scrollable(
                    rich::view(
                        markdown,
                        design::markdown_settings(
                            &cosmic::theme::active(),
                            design::TEXT_TRANSCRIPT,
                        ),
                    )
                    .map(|uri| action(Action::OpenUrl(uri.to_string()))),
                )
                .width(Length::Fill),
            )
            .max_height(ANSWER_HEIGHT)
            .into(),
            State::Failed(reason) => widget::row::with_children(vec![
                crate::ui_controls::passive_icon(Icon::Warning, design::COMPACT_ICON_SIZE),
                design::caption(reason.as_str())
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .into(),
        };
        Some(
            widget::container(
                widget::column::with_children(vec![
                    widget::row::with_children(header)
                        .spacing(step::GAP_XS)
                        .align_y(Alignment::Center)
                        .into(),
                    design::transcript_name(thread.current.question.as_str())
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                    body,
                ])
                .spacing(step::GAP_S),
            )
            .id(card_control_id(target.card, "chat-side-question"))
            .width(Length::Fill)
            .padding(step::GAP_S)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_M, design::RADIUS_M, 0.0, 0.0],
                false,
            ))
            .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(thread: &mut Thread, text: &str) {
        let id = thread.current.id.clone();
        assert!(thread.answer(chat::SideAnswer::answered(id, text)));
    }

    #[test]
    fn only_btw_at_the_head_of_a_draft_is_a_side_question() {
        assert_eq!(invocation("/btw what changed?"), Some("what changed?"));
        assert_eq!(invocation("  /BTW   spaced  "), Some("spaced"));
        assert_eq!(invocation("/btw"), Some(""));
        assert_eq!(
            invocation("/btw\non the next line"),
            Some("on the next line")
        );
        assert_eq!(invocation("/btwx question"), None);
        assert_eq!(invocation("by the way /btw question"), None);
        assert_eq!(invocation("btw question"), None);
    }

    #[test]
    fn a_follow_up_carries_what_the_card_already_answered() {
        let (mut thread, first) = Thread::ask(None, "first".into());
        assert!(first.history.is_empty());
        answered(&mut thread, "one");
        let (mut thread, second) = Thread::ask(Some(&thread), "second".into());
        assert_eq!(
            second.history,
            vec![chat::SideExchange {
                question: "first".into(),
                answer: "one".into(),
            }]
        );
        // A question with no answer is not context for the next one.
        thread.fail(None, "lost");
        let (_, third) = Thread::ask(Some(&thread), "third".into());
        assert_eq!(third.history.len(), 1);
        assert!(third.validate().is_ok());
        // Asking leaves the open card as it was, for a question that can't
        // be asked.
        let (_, oversized) = Thread::ask(
            Some(&thread),
            "x".repeat(chat::SideQuestion::MAX_QUESTION + 1),
        );
        assert!(oversized.validate().is_err());
        assert!(matches!(thread.current.state, State::Failed(_)));
        assert_eq!(thread.earlier.len(), 1);
    }

    #[test]
    fn a_follow_up_keeps_the_newest_exchanges_within_the_bounds() {
        let mut thread = None;
        for index in 0..chat::SideQuestion::MAX_HISTORY + 3 {
            let (mut next, _) = Thread::ask(thread.as_ref(), format!("question {index}"));
            answered(&mut next, &"a".repeat(64 * 1024));
            thread = Some(next);
        }
        let (_, request) = Thread::ask(thread.as_ref(), "last".into());
        assert!(request.validate().is_ok());
        assert!(request.history.len() <= chat::SideQuestion::MAX_HISTORY);
        let newest = chat::SideQuestion::MAX_HISTORY + 2;
        assert_eq!(
            request
                .history
                .last()
                .map(|exchange| exchange.question.as_str()),
            Some(format!("question {newest}").as_str())
        );
    }

    #[test]
    fn an_answer_is_taken_once_and_only_for_the_question_waiting() {
        let (mut thread, request) = Thread::ask(None, "question".into());
        assert!(!thread.answer(chat::SideAnswer::answered("another window", "theirs")));
        assert!(thread.answer_text().is_none());
        assert!(thread.answer(chat::SideAnswer::answered(request.id.clone(), "**ours**")));
        assert_eq!(thread.answer_text().as_deref(), Some("**ours**"));
        // A late refusal of the same question changes nothing.
        thread.fail(Some(&request.id), "late");
        assert!(!thread.answer(chat::SideAnswer::failed(request.id, "again")));
        assert_eq!(thread.answer_text().as_deref(), Some("**ours**"));
    }

    #[test]
    fn a_refused_question_says_why_in_the_card() {
        let (mut thread, request) = Thread::ask(None, "question".into());
        thread.fail(Some("an older question"), "not this one");
        assert!(matches!(thread.current.state, State::Asking));
        thread.fail(Some(&request.id), "Claude couldn't answer: overloaded");
        assert!(
            matches!(&thread.current.state, State::Failed(reason) if reason.contains("overloaded"))
        );
    }
}
