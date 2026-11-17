//! A Close the daemon refused stays on the pane it was about, saying why. When
//! the conversation's Cagents helper died and left processes of it running,
//! which is what holds the Close, the pane names them and offers to end them
//! and close, so the person never has to find and end them in a shell.
use super::*;
use crate::terminal::ipc::{LostProcess, ProcessStart};

#[derive(Debug, Clone)]
pub(super) struct CloseRefusal {
    /// Why the daemon kept the conversation, in its words.
    reason: String,
    /// What still runs for it, once the daemon has said.
    processes: Vec<LostProcess>,
    /// Ending them, and closing again, is under way.
    ending: bool,
}

impl Cagents {
    /// Shows a refused Close on its pane, and asks the daemon what still runs
    /// for the conversation.
    pub(super) fn refuse_chat_close(
        &mut self,
        card: CardId,
        session: chat::Session,
        reason: String,
    ) -> Task<Message> {
        self.status =
            Some(TransientNotice::error(reason.clone()).with_key(NoticeKey::ChatClose(card)));
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        let target = runtime.target;
        runtime.close_refusal = Some(CloseRefusal {
            reason,
            processes: Vec::new(),
            ending: false,
        });
        let holders =
            cosmic::task::message(Message::ChatUi(Action::AskCloseHolders(target, session)));
        // The person who closed the pane in front of them is answered there;
        // a pane closed from elsewhere doesn't take the keyboard from them.
        if self.workspace.focused() == Some(card) {
            Task::batch([
                QUESTION.focus(close_refusal_dismiss_id(card), close_refusal_end_id(card)),
                holders,
            ])
        } else {
            holders
        }
    }

    pub(super) fn ask_close_holders(
        &mut self,
        target: Target,
        session: chat::Session,
    ) -> Task<Message> {
        if self.chat_return_session(target.card).as_ref() != Some(&session)
            || self
                .chat_runtime_mut(target)
                .is_none_or(|runtime| runtime.close_refusal.is_none())
        {
            return Task::none();
        }
        cosmic::task::future(async move {
            let asked = session.clone();
            let result = tokio::task::spawn_blocking(move || lost_processes(asked, Vec::new()))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CloseHolders(target, session, result))
        })
    }

    /// The refusal on the focused pane, which Escape dismisses.
    pub(in crate::app) fn focused_close_refusal(&self) -> Option<Target> {
        let card = self.workspace.focused()?;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        runtime
            .close_refusal
            .as_ref()
            .filter(|refusal| !refusal.ending)
            .map(|_| runtime.target)
    }

    pub(super) fn show_close_holders(
        &mut self,
        target: Target,
        session: chat::Session,
        result: Result<Vec<LostProcess>, String>,
    ) -> Task<Message> {
        if self.chat_return_session(target.card).as_ref() != Some(&session) {
            return Task::none();
        }
        if let Some(refusal) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.close_refusal.as_mut())
        {
            // Without an answer the reason stands alone, as it did before
            // anything could be ended from here.
            refusal.processes = result.unwrap_or_default();
        }
        Task::none()
    }

    /// Ends what the daemon proves is the conversation's, then closes it again.
    pub(super) fn end_close_holders(&mut self, target: Target) -> Task<Message> {
        let Some(session) = self.chat_return_session(target.card) else {
            return Task::none();
        };
        let Some(refusal) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.close_refusal.as_mut())
            .filter(|refusal| !refusal.ending)
        else {
            return Task::none();
        };
        // Exactly what the person was shown: a process found since is shown
        // before anything ends it.
        let chosen: Vec<ProcessStart> = refusal
            .processes
            .iter()
            .filter(|process| process.proved)
            .map(|process| process.process)
            .collect();
        if chosen.is_empty() {
            return Task::none();
        }
        refusal.ending = true;
        cosmic::task::future(async move {
            let asked = session.clone();
            let result = tokio::task::spawn_blocking(move || lost_processes(asked, chosen))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CloseHoldersEnded(target, session, result))
        })
    }

    pub(super) fn close_holders_ended(
        &mut self,
        target: Target,
        session: chat::Session,
        result: Result<Vec<LostProcess>, String>,
    ) -> Task<Message> {
        if self.chat_return_session(target.card).as_ref() != Some(&session) {
            return Task::none();
        }
        let Some(refusal) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.close_refusal.as_mut())
        else {
            return Task::none();
        };
        refusal.ending = false;
        match result {
            // Close again: it closes once nothing holds the conversation, and
            // otherwise says what still does.
            Ok(_) => self.begin_chat_close(target.card),
            Err(error) => {
                refusal.reason = error;
                Task::none()
            }
        }
    }

    /// A Close tried again replaces what the last one said: it closes, or
    /// says again what still holds it. Ending, which closes again itself,
    /// keeps its progress shown.
    pub(super) fn clear_close_refusal(&mut self, card: CardId) {
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card)
            && runtime
                .close_refusal
                .as_ref()
                .is_some_and(|refusal| !refusal.ending)
        {
            runtime.close_refusal = None;
        }
    }

    pub(super) fn dismiss_close_refusal(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.close_refusal = None;
        self.resolve_notice(NoticeKey::ChatClose(target.card));
        // Dismissed in the pane in front of the person, the keyboard goes
        // back to the conversation's composer rather than nowhere.
        if self.workspace.focused() == Some(target.card) {
            iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                .map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }
}

/// What still runs for `session`, after ending those of `end` that are proved
/// to be its own.
fn lost_processes(
    session: chat::Session,
    end: Vec<ProcessStart>,
) -> Result<Vec<LostProcess>, String> {
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = crate::control::ControlClient::connect(&socket)
        .map_err(|error| format!("Cannot reach the daemon: {error}"))?;
    client
        .lost_chat_processes(session, end)
        .map_err(|error| error.reason())
}

#[cfg(test)]
impl CloseRefusal {
    pub(super) fn reason(&self) -> &str {
        &self.reason
    }
    pub(super) fn processes(&self) -> &[LostProcess] {
        &self.processes
    }
    pub(super) fn ending(&self) -> bool {
        self.ending
    }
    /// The label of the action ending what holds the Close, if it offers one.
    pub(super) fn end_action(&self) -> Option<String> {
        let proved = self
            .processes
            .iter()
            .filter(|process| process.proved)
            .count();
        (proved > 0).then(|| end_label(proved))
    }
}

/// The tallest the panel grows before its detail scrolls.
const CEILING: f32 = 320.0;

/// How many of what still runs the pane lists in full; the rest are named
/// together, so that everything the action ends has been shown.
const LISTED_PROCESSES: usize = 5;

/// The command names of `processes`, each once with how many share it, in the
/// order they first appear: `python3 ×3, node`.
fn named_together<'a>(processes: impl IntoIterator<Item = &'a LostProcess>) -> String {
    let mut names: Vec<(&str, usize)> = Vec::new();
    for process in processes {
        match names.iter_mut().find(|(name, _)| *name == process.name) {
            Some((_, count)) => *count += 1,
            None => names.push((process.name.as_str(), 1)),
        }
    }
    names
        .into_iter()
        .map(|(name, count)| match count {
            1 => name.to_owned(),
            count => format!("{name} \u{d7}{count}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The action that ends `count` processes and closes the conversation.
fn end_label(count: usize) -> String {
    match count {
        1 => "End 1 process and close".to_owned(),
        count => format!("End {count} processes and close"),
    }
}

impl Runtime {
    pub(in crate::app) fn close_refusal_view(&self) -> Option<Element<'_, Message>> {
        Some(refusal_panel(self.close_refusal.as_ref()?, self.target))
    }
}

/// The refusal, what still runs for it, and the answers to it.
fn refusal_panel(refusal: &CloseRefusal, target: Target) -> Element<'_, Message> {
    let action = |action| Message::ChatUi(action);
    let proved = refusal
        .processes
        .iter()
        .filter(|process| process.proved)
        .count();
    let mut rows: Vec<Element<'_, Message>> = vec![design::body(refusal.reason.as_str()).into()];
    if !refusal.processes.is_empty() {
        rows.push(design::caption("Still running for it:").into());
        // What the action ends comes first, then what it leaves running.
        let (ended, left): (Vec<&LostProcess>, Vec<&LostProcess>) =
            refusal.processes.iter().partition(|process| process.proved);
        let listed = ended.len().min(LISTED_PROCESSES);
        let (ended_listed, ended_rest) = ended.split_at(listed);
        let (left_listed, left_rest) = left.split_at(left.len().min(LISTED_PROCESSES - listed));
        for process in ended_listed.iter().chain(left_listed) {
            let mut line = vec![
                design::body(process.name.as_str()).into(),
                design::caption(process.command.as_str()).into(),
            ];
            if !process.proved {
                line.push(
                    design::caption(format!(
                        "Process {}. Cagents can't prove it is this conversation's, so it leaves it running. End it yourself if it is.",
                        process.process.pid
                    ))
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
                );
            }
            rows.push(
                widget::column::with_children(line)
                    .spacing(space::XXS)
                    .into(),
            );
        }
        if !ended_rest.is_empty() {
            rows.push(
                design::caption(format!(
                    "{} more: {}",
                    ended_rest.len(),
                    named_together(ended_rest.iter().copied())
                ))
                .into(),
            );
        }
        if !left_rest.is_empty() {
            rows.push(
                design::caption(format!(
                    "{} more Cagents can't prove are this conversation's, so it leaves them running: {}",
                    left_rest.len(),
                    named_together(left_rest.iter().copied())
                ))
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .into(),
            );
        }
    }
    // The dismissal leads, as in every question, and stays where it is when
    // the action ending what holds the Close arrives beside it.
    let mut buttons: Vec<Element<'_, Message>> = vec![
        crate::ui_controls::button::secondary("Dismiss")
            .id(close_refusal_dismiss_id(target.card))
            .on_press_maybe((!refusal.ending).then(|| action(Action::DismissCloseRefusal(target))))
            .into(),
    ];
    if proved > 0 {
        buttons.push(
            crate::ui_controls::button::destructive(end_label(proved))
                .instead("Ending…", refusal.ending)
                .id(close_refusal_end_id(target.card))
                .on_press_maybe((!refusal.ending).then(|| action(Action::EndCloseHolders(target))))
                .into(),
        );
    }
    // However long the reason and the commands, the answers stay in
    // reach and the conversation above keeps its room: only the detail
    // scrolls.
    let panel = widget::container(crate::app::dialog_ui::scrolling_with_actions(
        widget::column::with_children(rows).spacing(space::S),
        widget::row::with_children(buttons).spacing(space::S),
        iced::Padding::ZERO.bottom(space::S),
        design::raised,
    ))
    .padding(space::M)
    .width(Length::Fill)
    .max_width(super::layout::READING_COLUMN)
    .max_height(CEILING)
    .class(cosmic::theme::Container::custom(|theme| {
        widget::container::Style {
            background: Some(iced::Background::Color(design::raised(theme))),
            border: iced::Border {
                color: design::destructive_color(theme),
                width: design::BORDER_WIDTH,
                radius: design::RADIUS_S.into(),
            },
            ..Default::default()
        }
    }));
    crate::app::dialog_ui::inline_question(
        QUESTION,
        widget::container(panel).center_x(Length::Fill),
    )
}

const QUESTION: crate::app::dialog_ui::Question = crate::app::dialog_ui::Question::EndCloseHolders;

pub(in crate::app) fn close_refusal_dismiss_id(card: CardId) -> widget::Id {
    card_control_id(card, "close-refusal.dismiss")
}

pub(in crate::app) fn close_refusal_end_id(card: CardId) -> widget::Id {
    card_control_id(card, "close-refusal.end")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, name: &str) -> LostProcess {
        LostProcess {
            process: ProcessStart { pid, start: 1 },
            name: name.to_owned(),
            command: name.to_owned(),
            proved: true,
        }
    }

    /// However long the reason and the commands, the panel stops growing
    /// and its answers stay whole inside it; only the detail scrolls.
    #[test]
    fn a_long_refusal_keeps_its_answers_in_reach() {
        use cosmic::iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        let long = "a reason the daemon gave that runs on ".repeat(30);
        let refusal = CloseRefusal {
            reason: long.clone(),
            processes: (1..=7)
                .map(|pid| LostProcess {
                    command: format!("python3 -m {long}"),
                    proved: pid % 2 == 0,
                    ..process(pid, "python3")
                })
                .collect(),
            ending: false,
        };
        let mut panel = refusal_panel(
            &refusal,
            Target {
                card: CardId::new(),
                generation: 1,
            },
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(panel.as_widget());
        let node = panel.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 900.0)),
        );
        #[derive(Default)]
        struct Answers(Vec<(String, iced::Rectangle)>);
        impl Operation for Answers {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                if text == "Dismiss" || text.starts_with("End ") {
                    self.0.push((text.to_owned(), bounds));
                }
            }
        }
        let mut answers = Answers::default();
        panel
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut answers);
        let height = node.size().height;
        assert!(height <= CEILING, "the panel grows to {height}");
        assert_eq!(answers.0.len(), 2, "{:?}", answers.0);
        for (answer, bounds) in answers.0 {
            assert!(
                bounds.y >= 0.0 && bounds.y + bounds.height <= height,
                "{answer} lies outside the panel: {bounds:?} in {height}"
            );
        }
    }

    #[test]
    fn processes_beyond_the_listed_ones_are_still_named() {
        let rest = [
            process(1, "python3"),
            process(2, "node"),
            process(3, "python3"),
            process(4, "python3"),
        ];
        assert_eq!(named_together(&rest), "python3 \u{d7}3, node");
        assert_eq!(named_together(&rest[1..2]), "node");
    }
}
