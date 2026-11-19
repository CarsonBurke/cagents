//! After an update, the offer to resume the conversations whose work in
//! flight it stopped.
//!
//! The update records what each conversation had under way and whether it
//! survived (`UpdateJob::in_flight`). A conversation whose turn had simply
//! finished has nothing recorded, so it is never offered. Resuming one holds
//! a message asking its agent to pick up where it left off, which the reader
//! sees in the conversation like any queued message.
use super::*;
use crate::model::TerminalId;
use crate::update::UpdateJob;
use crate::update::in_flight::{InFlight, Outcome, TaskKind};
use serde::{Deserialize, Serialize};

/// The most conversations the offer lists by name; the rest are counted.
const LISTED: usize = 6;

#[derive(Default)]
pub(super) struct State {
    /// What the windows have done with the latest update's offer, once read.
    record: Option<Record>,
    /// The terminal update job the offer is for, until its record is read.
    job: Option<UpdateJob>,
    /// What is still on offer, in the update's order.
    offers: Vec<Outcome>,
}

/// What became of an update's offer, kept with the other windows' state so
/// it is made once per update, not once per window or start.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    job: String,
    /// Conversations no longer on offer: resumed from it, continued by
    /// hand, or dismissed by the window that showed them.
    #[serde(default)]
    handled: Vec<TerminalId>,
}

#[derive(Clone, Debug)]
pub enum Action {
    Loaded(String, Result<Option<String>, String>),
    Show(TerminalId),
    Resume(TerminalId),
    ResumeAll,
    Dismiss,
}

impl Record {
    /// This window's record, with what `stored` says of the same update.
    fn merged(mut self, stored: Option<Record>) -> Self {
        if let Some(stored) = stored.filter(|stored| stored.job == self.job) {
            for id in stored.handled {
                if !self.handled.contains(&id) {
                    self.handled.push(id);
                }
            }
        }
        self
    }
}

impl State {
    fn rebuild(&mut self) {
        let (Some(job), Some(record)) = (&self.job, &self.record) else {
            self.offers.clear();
            return;
        };
        self.offers = job
            .in_flight
            .iter()
            .filter(|outcome| outcome.lost())
            .filter(|outcome| record.job != job.id || !record.handled.contains(&outcome.work.id))
            .cloned()
            .collect();
    }

    /// Takes `id` off the offer, for this window and the others.
    fn handle(&mut self, id: TerminalId) {
        self.offers.retain(|offered| offered.work.id != id);
        if let Some(record) = self.record_for()
            && !record.handled.contains(&id)
        {
            record.handled.push(id);
        }
    }

    fn record_for(&mut self) -> Option<&mut Record> {
        let job = self.job.as_ref()?;
        let record = self.record.get_or_insert_with(Record::default);
        if record.job != job.id {
            *record = Record {
                job: job.id.clone(),
                ..Record::default()
            };
        }
        Some(record)
    }
}

/// What the row says was stopped, without saying how the app tracks it.
fn summary(work: &InFlight) -> String {
    let counted = |count: usize, one: &str, many: &str| match count {
        1 => format!("1 {one}"),
        count => format!("{count} {many}"),
    };
    let mut parts = Vec::new();
    if work.turn {
        parts.push("a reply in progress".to_owned());
    }
    let subagents = work
        .tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Subagent)
        .count();
    if subagents > 0 {
        parts.push(counted(subagents, "subagent", "subagents"));
    }
    let background = work.tasks.len() - subagents;
    if background > 0 {
        parts.push(counted(background, "background task", "background tasks"));
    }
    // Past the census's limits, tasks and queued messages are only counted.
    if work.unlisted > 0 {
        parts.push(counted(work.unlisted as usize, "other item", "other items"));
    }
    let stopped = match parts.split_last() {
        None => None,
        Some((last, [])) => Some(format!("Stopped {last}")),
        Some((last, rest)) => Some(format!("Stopped {} and {last}", rest.join(", "))),
    };
    let undelivered = match work.queued.len() {
        0 => None,
        1 => Some("1 queued message wasn't delivered".to_owned()),
        count => Some(format!("{count} queued messages weren't delivered")),
    };
    match (stopped, undelivered) {
        (Some(stopped), Some(undelivered)) => format!("{stopped}; {undelivered}"),
        (Some(said), None) | (None, Some(said)) => said,
        (None, None) => "Its work in progress stopped".to_owned(),
    }
}

/// The message that asks the agent to pick its work up again. It names what
/// was stopped so the agent can check on each.
fn continue_prompt(work: &InFlight) -> String {
    let mut lines = vec![
        "cagents was updated while you were working, and the update stopped work that was still under way here:".to_owned(),
    ];
    if work.turn {
        lines.push("- your turn in progress".to_owned());
    }
    for task in &work.tasks {
        let kind = match task.kind {
            TaskKind::Subagent => "subagent",
            TaskKind::Command => "background command",
            TaskKind::Step => "background step",
            TaskKind::Unknown => "background task",
        };
        // The reader sees this message too: a name says more to both than
        // the provider's identifier, which is the fallback.
        lines.push(if task.name.is_empty() {
            format!("- {kind} {}", task.id)
        } else {
            format!("- {kind} \"{}\"", task.name)
        });
    }
    if work.unlisted > 0 {
        lines.push(format!(
            "- {} more background tasks or queued messages",
            work.unlisted
        ));
    }
    if !work.queued.is_empty() {
        lines.push(format!(
            "- {} queued message{} not yet delivered to you",
            work.queued.len(),
            if work.queued.len() == 1 { "" } else { "s" }
        ));
    }
    lines.push(
        "Check what finished and what didn't, then carry on from where you left off.".to_owned(),
    );
    lines.join("\n")
}

/// What the notice says of the conversations that could not take the
/// message to resume.
fn refused_notice(labels: &[String]) -> String {
    match labels {
        [one] => format!("{one} can't take a message right now, so it wasn't resumed."),
        [rest @ .., last] => format!(
            "{} and {last} can't take a message right now, so they weren't resumed.",
            rest.join(", ")
        ),
        [] => String::new(),
    }
}

impl Cagents {
    /// Takes the latest update job into the offer once it has ended.
    pub(super) fn offer_update_resume(&mut self, job: &UpdateJob) -> Task<Message> {
        if !job.phase.is_terminal() || !job.in_flight.iter().any(Outcome::lost) {
            let state = &mut self.update_resume;
            state.job = None;
            state.offers.clear();
            return Task::none();
        }
        let state = &mut self.update_resume;
        if state.job.as_ref().is_some_and(|known| known.id == job.id) {
            return Task::none();
        }
        // Read afresh for each update: another window may have taken up its
        // offer since this one last read the record.
        state.job = Some(job.clone());
        state.offers.clear();
        let Some(store) = self
            .session
            .as_ref()
            .map(|session| session.operation_store())
        else {
            // Without the shared state the offer is made in this window alone.
            state.record = Some(Record::default());
            state.rebuild();
            return Task::none();
        };
        let id = job.id.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                store
                    .load_update_resume()
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::UpdateResume(Action::Loaded(id, result))
        })
    }

    pub(super) fn update_update_resume(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Loaded(job, result) => {
                let state = &mut self.update_resume;
                let record = match result {
                    Ok(value) => value
                        .and_then(|value| {
                            serde_json::from_str::<Record>(&value)
                                .inspect_err(|error| {
                                    tracing::warn!(%error, "unreadable update resume record");
                                })
                                .ok()
                        })
                        .unwrap_or_default(),
                    Err(error) => {
                        tracing::warn!(%error, "could not read the update resume record");
                        Record::default()
                    }
                };
                // An answer for an update since replaced is for nothing shown.
                if state.job.as_ref().is_some_and(|known| known.id == job) {
                    state.record = Some(record);
                    state.rebuild();
                }
                Task::none()
            }
            Action::Show(id) => {
                let Some((card, channel)) = self.session_card(id) else {
                    return Task::none();
                };
                let selected =
                    channel.map_or_else(Task::none, |channel| self.navigate_to_channel(channel));
                Task::batch([selected, cosmic::task::message(Message::FocusCard(card))])
            }
            Action::Resume(id) => self.resume_offered_all([id]),
            Action::ResumeAll => {
                let ids: Vec<_> = self.offered_here().map(|outcome| outcome.work.id).collect();
                self.resume_offered_all(ids)
            }
            Action::Dismiss => {
                // Only what this window showed: another window's conversations
                // stay on offer there.
                let ids: Vec<_> = self.offered_here().map(|outcome| outcome.work.id).collect();
                for id in ids {
                    self.update_resume.handle(id);
                }
                self.save_update_resume()
            }
        }
    }

    /// Resumes each offered conversation in `ids`, taking it off the offer,
    /// and says which could not take the message.
    fn resume_offered_all(&mut self, ids: impl IntoIterator<Item = TerminalId>) -> Task<Message> {
        let mut tasks = Vec::new();
        let mut refused = Vec::new();
        for id in ids {
            let Some(outcome) = self
                .update_resume
                .offers
                .iter()
                .find(|outcome| outcome.work.id == id)
                .cloned()
            else {
                continue;
            };
            let Some((card, _)) = self.session_card(id) else {
                continue;
            };
            match self.resume_lost_work(card, continue_prompt(&outcome.work)) {
                Some(task) => {
                    self.update_resume.handle(id);
                    tasks.push(task);
                }
                // It stays on offer, to resume once it can take a message.
                None => refused.push(self.offer_label(&outcome)),
            }
        }
        if !refused.is_empty() {
            self.status = Some(TransientNotice::warning(refused_notice(&refused)));
        }
        if tasks.is_empty() {
            return Task::none();
        }
        Task::batch(tasks.into_iter().chain([self.save_update_resume()]))
    }

    /// Takes the conversation in `card` off the offer once the reader writes
    /// to it themselves: it is under way again, and a message about stopped
    /// work would arrive after their own.
    pub(super) fn update_resume_continued(&mut self, card: CardId) -> Task<Message> {
        let Some(CardKind::Chat(session)) = self.card_state(card).map(|state| &state.kind) else {
            return Task::none();
        };
        let id = session.id;
        if !self
            .update_resume
            .offers
            .iter()
            .any(|outcome| outcome.work.id == id)
        {
            return Task::none();
        }
        self.update_resume.handle(id);
        self.save_update_resume()
    }

    fn save_update_resume(&self) -> Task<Message> {
        let Some(record) = self.update_resume.record.clone() else {
            return Task::none();
        };
        let Some(store) = self
            .session
            .as_ref()
            .map(|session| session.operation_store())
        else {
            return Task::none();
        };
        iced::Task::future(async move {
            let saved = tokio::task::spawn_blocking(move || {
                // Each window takes the conversations it holds off the offer,
                // so what the others recorded for the same update is kept.
                let mut encoded = Ok(());
                let merged = store.merge_update_resume(|stored| {
                    let stored = stored.and_then(|value| serde_json::from_str(&value).ok());
                    serde_json::to_string(&record.merged(stored))
                        .map_err(|error| encoded = Err(error.to_string()))
                        .ok()
                });
                encoded.and(merged.map_err(|error| error.to_string()))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            if let Err(error) = saved {
                tracing::warn!(%error, "could not save the update resume record");
            }
        })
        .discard()
    }

    /// The offered conversations this window holds a tab for. Another
    /// window's are offered there.
    fn offered_here(&self) -> impl Iterator<Item = &Outcome> {
        self.update_resume
            .offers
            .iter()
            .filter(|outcome| self.session_card(outcome.work.id).is_some())
    }

    fn offer_label(&self, outcome: &Outcome) -> String {
        self.session_card(outcome.work.id)
            .and_then(|(card, _)| self.card_state(card))
            .map(|state| state.title.trim().to_owned())
            .filter(|title| !title.is_empty())
            .or_else(|| {
                let label = outcome.label.trim();
                (!label.is_empty()).then(|| label.to_owned())
            })
            .unwrap_or_else(|| "A conversation".to_owned())
    }

    pub(super) fn update_resume_banner(&self) -> Element<'_, Message> {
        let offered: Vec<_> = self.offered_here().collect();
        if offered.is_empty() {
            return iced::widget::Space::new().height(Length::Shrink).into();
        }
        let enabled = !self.modal_is_open();
        let heading = match offered.len() {
            1 => "The update stopped work in 1 conversation".to_owned(),
            count => format!("The update stopped work in {count} conversations"),
        };
        let mut header = widget::row::with_children(vec![
            widget::column::with_children(vec![
                design::heading(heading).into(),
                design::caption(if offered.len() == 1 {
                    "Resume asks it to check where it got to and carry on."
                } else {
                    "Resume asks each one to check where it got to and carry on."
                })
                .into(),
            ])
            .spacing(space::XXS)
            .width(Length::Fill)
            .into(),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center);
        if offered.len() > 1 {
            header = header.push(
                crate::ui_controls::button::primary("Resume all")
                    .name("Resume every conversation listed")
                    .on_press_maybe(enabled.then_some(Message::UpdateResume(Action::ResumeAll))),
            );
        }
        header = header.push(crate::interaction::tooltip(
            widget::button::icon(Icon::Close)
                .id(widget::Id::new("update-resume.dismiss"))
                .name("Dismiss the offer to resume stopped work")
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(enabled.then_some(Message::UpdateResume(Action::Dismiss))),
            "Dismiss",
        ));
        let mut rows: Vec<Element<'_, Message>> = vec![header.into()];
        for outcome in offered.iter().take(LISTED) {
            let id = outcome.work.id;
            let label = self.offer_label(outcome);
            let resume = if offered.len() == 1 {
                crate::ui_controls::button::primary("Resume")
            } else {
                crate::ui_controls::button::secondary("Resume")
            };
            rows.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::quiet(label.clone())
                        .name(format!("Show {label}"))
                        .on_press_maybe(enabled.then_some(Message::UpdateResume(Action::Show(id))))
                        .into(),
                    design::caption(summary(&outcome.work))
                        .width(Length::Fill)
                        .into(),
                    resume
                        .name(format!("Resume {label}"))
                        .on_press_maybe(
                            enabled.then_some(Message::UpdateResume(Action::Resume(id))),
                        )
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        if offered.len() > LISTED {
            rows.push(
                design::caption(format!(
                    "and {} more; Resume all includes them",
                    offered.len() - LISTED
                ))
                .into(),
            );
        }
        widget::container(widget::column::with_children(rows).spacing(space::XS))
            .padding(space::M)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Dialog(false))
            .into()
    }
}

#[cfg(test)]
mod tests;
