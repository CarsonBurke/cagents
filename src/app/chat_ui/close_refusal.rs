//! A Close the daemon refused stays on the pane it was about, saying why. When
//! the conversation's Cagents helper died and left processes of it running,
//! which is what holds the Close, the pane names them and offers to end them
//! and close, so the person never has to find and end them in a shell.
use super::*;
use crate::terminal::ipc::LostProcess;

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
            let result = tokio::task::spawn_blocking(move || lost_processes(asked, false))
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
        refusal.ending = true;
        cosmic::task::future(async move {
            let asked = session.clone();
            let result = tokio::task::spawn_blocking(move || lost_processes(asked, true))
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

    pub(super) fn dismiss_close_refusal(&mut self, target: Target) -> Task<Message> {
        if let Some(runtime) = self.chat_runtime_mut(target) {
            runtime.close_refusal = None;
        }
        self.resolve_notice(NoticeKey::ChatClose(target.card));
        Task::none()
    }
}

/// What still runs for `session`, after ending what is proved to be its own
/// when `end` is set.
fn lost_processes(session: chat::Session, end: bool) -> Result<Vec<LostProcess>, String> {
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
            for process in &refusal.processes {
                let mut line = vec![
                    design::body(process.name.as_str()).into(),
                    design::caption(process.command.as_str()).into(),
                ];
                if !process.proved {
                    line.push(
                        design::caption(
                            "Cagents can't prove this is the conversation's, so it leaves it running. End it yourself if it is.",
                        )
                        .into(),
                    );
                }
                rows.push(
                    widget::column::with_children(line)
                        .spacing(space::XXS)
                        .into(),
                );
            }
        }
        let end_label = end_label(proved);
        let mut buttons: Vec<Element<'_, Message>> = Vec::new();
        if proved > 0 {
            buttons.push(
                crate::ui_controls::button::destructive(if refusal.ending {
                    "Ending\u{2026}".to_owned()
                } else {
                    end_label
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
        Some(widget::column::with_children(rows).spacing(space::S).into())
    }
}
