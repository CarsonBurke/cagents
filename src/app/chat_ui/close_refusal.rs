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
        cosmic::task::future(async move {
            let asked = session.clone();
            let result = tokio::task::spawn_blocking(move || lost_processes(asked, Vec::new()))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CloseHolders(target, session, result))
        })
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
        if let Some(runtime) = self.chat_runtime_mut(target) {
            runtime.close_refusal = None;
        }
        self.resolve_notice(NoticeKey::ChatClose(target.card));
        Task::none()
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

/// How many of what still runs the pane lists; the rest are counted.
const LISTED_PROCESSES: usize = 5;

/// The action that ends `count` processes and closes the conversation.
fn end_label(count: usize) -> String {
    match count {
        1 => "End 1 process and close".to_owned(),
        count => format!("End {count} processes and close"),
    }
}

impl Runtime {
    pub(super) fn close_refusal_view(&self) -> Option<Element<'_, Message>> {
        let refusal = self.close_refusal.as_ref()?;
        let target = self.target;
        let action = |action| Message::ChatUi(action);
        let proved = refusal
            .processes
            .iter()
            .filter(|process| process.proved)
            .count();
        let mut rows: Vec<Element<'_, Message>> =
            vec![design::body(refusal.reason.as_str()).into()];
        if !refusal.processes.is_empty() {
            rows.push(design::caption("Still running for it:").into());
            for process in refusal.processes.iter().take(LISTED_PROCESSES) {
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
            let more = refusal.processes.len().saturating_sub(LISTED_PROCESSES);
            if more > 0 {
                rows.push(design::caption(format!("and {more} more")).into());
            }
        }
        let mut buttons: Vec<Element<'_, Message>> = Vec::new();
        if proved > 0 {
            buttons.push(
                crate::ui_controls::button::destructive(if refusal.ending {
                    "Ending\u{2026}".to_owned()
                } else {
                    end_label(proved)
                })
                .on_press_maybe((!refusal.ending).then(|| action(Action::EndCloseHolders(target))))
                .into(),
            );
        }
        buttons.push(
            crate::ui_controls::button::secondary("Dismiss")
                .on_press_maybe(
                    (!refusal.ending).then(|| action(Action::DismissCloseRefusal(target))),
                )
                .into(),
        );
        rows.push(widget::row::with_children(buttons).spacing(space::S).into());
        let panel = widget::container(widget::column::with_children(rows).spacing(space::S))
            .padding(space::M)
            .width(Length::Fill)
            .max_width(super::layout::READING_COLUMN)
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
        Some(widget::container(panel).center_x(Length::Fill).into())
    }
}
