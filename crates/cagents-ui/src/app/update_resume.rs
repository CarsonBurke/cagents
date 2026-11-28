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
use crate::persistence::{Taken, UpdateResumeStore};
use crate::update::UpdateJob;
use crate::update::in_flight::{InFlight, Outcome, ResumePoint, TaskKind};

/// The most conversations the offer lists by name; the rest are counted.
const LISTED: usize = 6;

#[derive(Default)]
pub(super) struct State {
    /// What every window has taken off the offer, kept apart from their own
    /// databases because one conversation can show in several windows.
    /// Without it the offer is made in this window alone.
    store: Option<Arc<UpdateResumeStore>>,
    /// The terminal update job the offer is for.
    job: Option<UpdateJob>,
    /// The conversations no window offers any more for `job`, once read.
    handled: Option<Vec<TerminalId>>,
    /// Those asked to resume here, until the shared record says this window
    /// took them first.
    taking: Vec<TerminalId>,
    /// Saved history checks for this job: false until the read returns.
    history_checks: HashMap<TerminalId, bool>,
    /// What is still on offer, in the update's order.
    offers: Vec<Outcome>,
}

#[derive(Clone, Debug)]
pub enum Action {
    Loaded(String, Result<Vec<TerminalId>, String>),
    HistoryChecked(String, Vec<(TerminalId, bool)>),
    DispatchChecked(super::chat_ui::Target, u64, Vec<(u64, bool)>),
    /// The shared record's answer to taking conversations off the update's
    /// offer, with the ones among them to resume.
    Taken(String, Vec<Resume>, Result<Taken, String>),
    Show(TerminalId),
    Resume(TerminalId),
    ResumeAll,
    Dismiss,
}

/// A conversation to resume once this window has taken it off the offer.
#[derive(Clone, Debug)]
pub struct Resume {
    id: TerminalId,
    prompt: String,
    label: String,
    point: Option<ResumePoint>,
    advanced: bool,
}

pub(super) fn check_resume_dispatch(
    target: super::chat_ui::Target,
    turn: u64,
    launch: crate::chat::Launch,
    checks: Vec<(u64, ResumePoint)>,
) -> Task<Message> {
    let ids = checks.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    cosmic::task::future(async move {
        let checked = tokio::task::spawn_blocking(move || {
            checks
                .into_iter()
                .map(|(id, point)| (id, point.advanced_on_disk(&launch)))
                .collect()
        })
        .await
        .unwrap_or_else(|_| ids.into_iter().map(|id| (id, false)).collect());
        Message::UpdateResume(Action::DispatchChecked(target, turn, checked))
    })
}

impl State {
    /// The offer as every window records it, in the data directory.
    pub(super) fn shared() -> Self {
        Self {
            store: instance::project_dirs().map(|dirs| {
                Arc::new(UpdateResumeStore::at(
                    dirs.data_dir().join("update-resume.sqlite3"),
                ))
            }),
            ..Self::default()
        }
    }

    fn rebuild(&mut self) {
        let (Some(job), Some(handled)) = (&self.job, &self.handled) else {
            self.offers.clear();
            return;
        };
        self.offers = job
            .in_flight
            .iter()
            .filter(|outcome| outcome.lost() && !handled.contains(&outcome.work.id))
            .cloned()
            .collect();
    }

    /// Takes `id` off the offer here; the shared record is told apart.
    fn handle(&mut self, id: TerminalId) {
        self.offers.retain(|offered| offered.work.id != id);
        if let Some(handled) = &mut self.handled
            && !handled.contains(&id)
        {
            handled.push(id);
        }
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
    // The reader sees this message too, so a task is named as the provider
    // named it, never by its identifier; those it left unnamed are counted.
    for kind in [
        TaskKind::Subagent,
        TaskKind::Command,
        TaskKind::Step,
        TaskKind::Unknown,
    ] {
        let (one, many) = match kind {
            TaskKind::Subagent => ("subagent", "subagents"),
            TaskKind::Command => ("background command", "background commands"),
            TaskKind::Step => ("background step", "background steps"),
            TaskKind::Unknown => ("background task", "background tasks"),
        };
        let mut unnamed = 0;
        for task in work.tasks.iter().filter(|task| task.kind == kind) {
            if task.name.trim().is_empty() {
                unnamed += 1;
            } else {
                lines.push(format!("- {one} \"{}\"", task.name.trim()));
            }
        }
        match unnamed {
            0 => {}
            1 => lines.push(format!("- 1 {one}")),
            count => lines.push(format!("- {count} {many}")),
        }
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

/// What the notice says of the conversations another window had already
/// resumed or dismissed when this one asked.
fn elsewhere_notice(labels: &[String]) -> String {
    match labels {
        [one] => format!("{one} was already resumed or dismissed in another window."),
        [rest @ .., last] => format!(
            "{} and {last} were already resumed or dismissed in another window.",
            rest.join(", ")
        ),
        [] => String::new(),
    }
}

/// What the notice says of the conversations not resumed because whether
/// another window had already resumed them could not be read.
fn unchecked_notice(labels: &[String]) -> String {
    let names = match labels {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => return String::new(),
    };
    format!(
        "Couldn't check whether another window had already resumed {names}, so nothing was sent. Try again."
    )
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
        let state = &mut self.update_resume;
        if !job.phase.is_terminal() || !job.in_flight.iter().any(Outcome::lost) {
            *state = State {
                store: state.store.take(),
                ..State::default()
            };
            return Task::none();
        }
        if state.job.as_ref().is_some_and(|known| known.id == job.id) {
            return self.check_update_resume_history();
        }
        *state = State {
            store: state.store.take(),
            job: Some(job.clone()),
            ..State::default()
        };
        let Some(store) = state.store.clone() else {
            state.handled = Some(Vec::new());
            state.rebuild();
            return self.check_update_resume_history();
        };
        let id = job.id.clone();
        cosmic::task::future(async move {
            let read = id.clone();
            let result = tokio::task::spawn_blocking(move || {
                store.handled(&read).map_err(|error| error.to_string())
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
                // An answer for an update since replaced is for nothing shown.
                if state.job.as_ref().is_none_or(|known| known.id != job) {
                    return Task::none();
                }
                let handled = result.unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not read the update resume record");
                    Vec::new()
                });
                // What this window took off while it read stays off.
                let mut merged = state.handled.take().unwrap_or_default();
                merged.retain(|id| !handled.contains(id));
                state.handled = Some(handled.into_iter().chain(merged).collect());
                state.rebuild();
                self.check_update_resume_history()
            }
            Action::HistoryChecked(job, checked) => {
                let state = &mut self.update_resume;
                if state.job.as_ref().is_none_or(|known| known.id != job) {
                    return Task::none();
                }
                let mut continued = Vec::new();
                for (id, advanced) in checked {
                    state.history_checks.insert(id, true);
                    if advanced {
                        state.handle(id);
                        continued.push(id);
                    }
                }
                self.take_update_offer(continued, Vec::new())
            }
            Action::DispatchChecked(target, turn, checked) => {
                self.update_resume_dispatch_checked(target, turn, checked)
            }
            Action::Taken(job, resumes, result) => self.update_resume_taken(job, resumes, result),
            Action::Show(id) => {
                let Some((card, channel)) = self.session_card(id) else {
                    return Task::none();
                };
                let selected =
                    channel.map_or_else(Task::none, |channel| self.navigate_to_channel(channel));
                Task::batch([selected, cosmic::task::message(Message::FocusCard(card))])
            }
            Action::Resume(id) => self.resume_offered([id]),
            Action::ResumeAll => {
                let ids: Vec<_> = self.offered_here().map(|outcome| outcome.work.id).collect();
                self.resume_offered(ids)
            }
            Action::Dismiss => {
                // Only what this window showed: another window's conversations
                // stay on offer there.
                let ids: Vec<_> = self.offered_here().map(|outcome| outcome.work.id).collect();
                for id in &ids {
                    self.update_resume.handle(*id);
                }
                self.take_update_offer(ids, Vec::new())
            }
        }
    }

    /// Check exact provider history away from the window thread. A card may
    /// appear after the job, so its first attachment also asks for this read.
    pub(super) fn check_update_resume_history(&mut self) -> Task<Message> {
        if crate::device::is_remote() {
            return Task::none();
        }
        let state = &self.update_resume;
        let Some(job) = state.job.as_ref().map(|job| job.id.clone()) else {
            return Task::none();
        };
        let checks: Vec<_> = state
            .offers
            .iter()
            .filter(|outcome| !state.history_checks.contains_key(&outcome.work.id))
            .filter_map(|outcome| {
                let id = outcome.work.id;
                let point = outcome.resume_point.as_ref()?;
                let (card, _) = self.session_card(id)?;
                let launch = self.update_resume_launch(card)?;
                point
                    .matches_launch(launch)
                    .then(|| (id, point.clone(), launch.clone()))
            })
            .collect();
        if checks.is_empty() {
            return Task::none();
        }
        self.update_resume
            .history_checks
            .extend(checks.iter().map(|(id, _, _)| (*id, false)));
        let ids = checks.iter().map(|(id, _, _)| *id).collect::<Vec<_>>();
        cosmic::task::future(async move {
            let checked = tokio::task::spawn_blocking(move || {
                checks
                    .into_iter()
                    .map(|(id, point, launch)| (id, point.advanced_on_disk(&launch)))
                    .collect()
            })
            .await
            .unwrap_or_else(|_| ids.into_iter().map(|id| (id, false)).collect());
            Message::UpdateResume(Action::HistoryChecked(job, checked))
        })
    }

    fn update_resume_launch(&self, card: CardId) -> Option<&crate::chat::Launch> {
        match &self.card_state(card)?.kind {
            CardKind::Chat(session) => Some(&session.launch),
            _ => None,
        }
    }

    /// Resumes each offered conversation in `ids` that can take the message,
    /// once this window is the first to take it off the offer, and says
    /// which could not.
    fn resume_offered(&mut self, ids: impl IntoIterator<Item = TerminalId>) -> Task<Message> {
        let mut resumes = Vec::new();
        let mut refused = Vec::new();
        for id in ids {
            let Some(outcome) = self
                .update_resume
                .offers
                .iter()
                .find(|outcome| outcome.work.id == id)
                .filter(|outcome| self.update_resume_eligible(outcome))
            else {
                continue;
            };
            let Some((card, _)) = self.session_card(id) else {
                continue;
            };
            let label = self.offer_label(outcome);
            // It stays on offer, to resume once it can take a message.
            if !self.chat_takes_resume(card) {
                refused.push(label);
                continue;
            }
            resumes.push(Resume {
                id,
                prompt: continue_prompt(&outcome.work),
                label,
                point: outcome.resume_point.clone(),
                advanced: false,
            });
        }
        if !refused.is_empty() {
            self.status = Some(TransientNotice::warning(refused_notice(&refused)));
        }
        if resumes.is_empty() {
            return Task::none();
        }
        let Some(job) = self.update_resume.job.as_ref().map(|job| job.id.clone()) else {
            return Task::none();
        };
        let ids = resumes.iter().map(|resume| resume.id).collect::<Vec<_>>();
        // Off the offer at once; sent once the shared record answers, unless
        // the reader writes to the conversation first.
        self.update_resume.taking.extend(ids.iter().copied());
        if self.update_resume.store.is_none()
            && (crate::device::is_remote() || resumes.iter().all(|resume| resume.point.is_none()))
        {
            let first = ids;
            return self.update_resume_taken(
                job,
                resumes,
                Ok(Taken {
                    first,
                    handled: Vec::new(),
                }),
            );
        }
        self.take_update_offer(ids, resumes)
    }

    /// Takes `ids` off the update's offer in every window, then resumes
    /// those of `resumes` this window took first.
    fn take_update_offer(&self, ids: Vec<TerminalId>, resumes: Vec<Resume>) -> Task<Message> {
        let Some(job) = self.update_resume.job.as_ref().map(|job| job.id.clone()) else {
            return Task::none();
        };
        let store = self.update_resume.store.clone();
        if ids.is_empty() || (store.is_none() && resumes.is_empty()) {
            return Task::none();
        }
        let checks: Vec<_> = if crate::device::is_remote() {
            Vec::new()
        } else {
            resumes
                .iter()
                .enumerate()
                .filter(|(_, resume)| resume.point.is_some())
                .filter_map(|(index, resume)| {
                    let (card, _) = self.session_card(resume.id)?;
                    Some((index, self.update_resume_launch(card)?.clone()))
                })
                .collect()
        };
        cosmic::task::future(async move {
            let taking = job.clone();
            let mut checked = resumes.clone();
            let result = tokio::task::spawn_blocking(move || {
                let result = match store {
                    Some(store) => store.take(&taking, &ids).map_err(|error| error.to_string()),
                    None => Ok(Taken {
                        first: ids,
                        handled: Vec::new(),
                    }),
                };
                for (index, launch) in checks {
                    let resume = &mut checked[index];
                    resume.advanced = resume
                        .point
                        .as_ref()
                        .is_some_and(|point| point.advanced_on_disk(&launch));
                }
                (checked, result)
            })
            .await;
            let (resumes, result) =
                result.unwrap_or_else(|error| (resumes, Err(error.to_string())));
            Message::UpdateResume(Action::Taken(job, resumes, result))
        })
    }

    fn update_resume_taken(
        &mut self,
        job: String,
        resumes: Vec<Resume>,
        result: Result<Taken, String>,
    ) -> Task<Message> {
        let state = &mut self.update_resume;
        if state.job.as_ref().is_none_or(|known| known.id != job) {
            return Task::none();
        }
        // One the reader wrote to while the record was asked is under way
        // again, and no longer being taken.
        let resumes: Vec<_> = resumes
            .into_iter()
            .filter(|resume| state.taking.contains(&resume.id))
            .collect();
        state
            .taking
            .retain(|id| !resumes.iter().any(|resume| resume.id == *id));
        let first = match result {
            Ok(taken) => {
                if let Some(handled) = &mut state.handled {
                    for id in taken.handled {
                        if !handled.contains(&id) {
                            handled.push(id);
                        }
                    }
                }
                taken.first
            }
            // Unable to ask the other windows, it resumes none: another may
            // have sent the same message. They stay on offer to try again.
            Err(error) => {
                tracing::warn!(%error, "could not record the update resume offer");
                state.rebuild();
                let labels: Vec<_> = resumes.into_iter().map(|resume| resume.label).collect();
                if !labels.is_empty() {
                    self.status = Some(TransientNotice::warning(unchecked_notice(&labels)));
                }
                return Task::none();
            }
        };
        state.rebuild();
        let mut tasks = Vec::new();
        let mut elsewhere = Vec::new();
        let mut refused = Vec::new();
        for resume in resumes {
            let card = self.session_card(resume.id).map(|(card, _)| card);
            if resume.advanced
                || card
                    .zip(resume.point.as_ref())
                    .is_some_and(|(card, point)| {
                        self.update_resume_launch(card)
                            .is_none_or(|launch| !point.matches_launch(launch))
                            || self.chat_update_resume_advanced(card, point)
                    })
            {
                self.update_resume.handle(resume.id);
                continue;
            }
            if !first.contains(&resume.id) {
                elsewhere.push(resume.label);
                continue;
            }
            let sent = card.and_then(|card| {
                self.resume_lost_work(card, resume.prompt.clone(), resume.point.clone())
            });
            match sent {
                Some(task) => {
                    self.update_resume.handle(resume.id);
                    tasks.push(task);
                }
                None => refused.push(resume),
            }
        }
        if !refused.is_empty() {
            // Changed while the record was asked: back on offer, here and in
            // the other windows.
            let ids: Vec<_> = refused.iter().map(|resume| resume.id).collect();
            if let Some(handled) = &mut self.update_resume.handled {
                handled.retain(|id| !ids.contains(id));
                self.update_resume.rebuild();
            }
            let labels: Vec<_> = refused.into_iter().map(|resume| resume.label).collect();
            self.status = Some(TransientNotice::warning(refused_notice(&labels)));
            if let Some(store) = self.update_resume.store.clone() {
                tasks.push(
                    iced::Task::future(async move {
                        let restored =
                            tokio::task::spawn_blocking(move || store.restore(&job, &ids))
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                        if let Err(error) = restored {
                            tracing::warn!(%error, "could not put a conversation back on offer");
                        }
                    })
                    .discard(),
                );
            }
        } else if !elsewhere.is_empty() {
            self.status = Some(TransientNotice::warning(elsewhere_notice(&elsewhere)));
        }
        Task::batch(tasks)
    }

    /// Takes the conversation in `card` off the offer once the reader writes
    /// to it themselves: it is under way again, and a message about stopped
    /// work would arrive after their own.
    pub(super) fn update_resume_continued(&mut self, card: CardId) -> Task<Message> {
        let Some(CardKind::Chat(session)) = self.card_state(card).map(|state| &state.kind) else {
            return Task::none();
        };
        let id = session.id;
        // A resume still asking the record is not sent after their message,
        // even once another answer has taken the row off the offer.
        let state = &mut self.update_resume;
        let pending = state.taking.contains(&id);
        state.taking.retain(|taking| *taking != id);
        if !pending && !state.offers.iter().any(|outcome| outcome.work.id == id) {
            return Task::none();
        }
        state.handle(id);
        self.take_update_offer(vec![id], Vec::new())
    }

    /// Keep confirmed advancement even if the pane subsequently reads an
    /// older history page or releases its transcript.
    pub(super) fn observe_update_resume_advance(&mut self, card: CardId) -> Task<Message> {
        if self.update_resume.offers.is_empty() {
            return Task::none();
        }
        let Some(CardKind::Chat(session)) = self.card_state(card).map(|state| &state.kind) else {
            return Task::none();
        };
        let id = session.id;
        let advanced = self.update_resume.offers.iter().any(|outcome| {
            outcome.work.id == id
                && outcome
                    .resume_point
                    .as_ref()
                    .is_some_and(|point| self.chat_update_resume_advanced(card, point))
        });
        if !advanced {
            return Task::none();
        }
        self.update_resume.handle(id);
        self.update_resume.taking.retain(|taking| *taking != id);
        self.take_update_offer(vec![id], Vec::new())
    }

    /// The offered conversations this window holds a tab for. Another
    /// window's are offered there.
    fn offered_here(&self) -> impl Iterator<Item = &Outcome> {
        self.update_resume
            .offers
            .iter()
            .filter(|outcome| self.update_resume_eligible(outcome))
    }

    fn update_resume_eligible(&self, outcome: &Outcome) -> bool {
        let id = outcome.work.id;
        !self.update_resume.taking.contains(&id)
            && self.update_resume.history_checks.get(&id) != Some(&false)
            && self.session_card(id).is_some_and(|(card, _)| {
                outcome.resume_point.as_ref().is_none_or(|point| {
                    self.update_resume_launch(card)
                        .is_some_and(|launch| point.matches_launch(launch))
                        && !self.chat_update_resume_advanced(card, point)
                })
            })
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
