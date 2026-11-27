//! Conversations that lost their connection rejoin it without being asked.
//!
//! A daemon that restarts, or a connection that drops, leaves every open
//! conversation of this window disconnected. Each pane has a Reconnect
//! button, but a window can hold dozens of conversations, most of them out of
//! sight, and none of them hears from its agent again until someone finds
//! that button. So the window presses it for them: the conversations on
//! screen first, a few at a time, each waiting longer after a failure, and
//! only so many times. One that still cannot attach keeps its button and
//! the daemon's reason, and the window says once how the round went.
//!
//! An automatic attempt only ever puts a pane back on the conversation it
//! was showing ([`chat::Intent::Restore`]), as its Reconnect would. A
//! provider that went down with its daemon is started again on that
//! conversation, since nothing else can show it, and no message is sent to
//! it again. What still waited to be sent is treated as after Reconnect: a
//! message the provider had queued returns to the composer, and one the
//! pane held for a conversation not yet ready goes once it is. The pane of
//! an agent the daemon manages is the exception: only the daemon starts that
//! agent's provider, so its pane rejoins what the daemon still hosts or
//! keeps its button. An attempt never restarts a provider the
//! window saw stop, repeats a launch the reader asked for, gives a pane up
//! for a draft, or answers a refusal that needs a decision; those stay with
//! the pane's own controls. Like the window's own readers, it starts the
//! daemon where none runs, except in a window that may only attach to what
//! exists.
use super::*;

/// How long an attempt waits: the first after the connection is lost, each
/// later one after the attempt before it failed. A daemon that was killed is
/// usually back within a second or two, and one that takes longer is asked
/// less and less often.
const DELAYS: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];
/// How many conversations attach at once. A restarted daemon may start a
/// provider for each, and a window can hold many more than it shows.
const CONCURRENT: usize = 4;
/// How long a restored connection has to hold before losing it again starts
/// the delays over. A connection that drops sooner continues where they
/// stood, so one that cannot stay up stops being retried.
const SETTLED_AFTER: Duration = Duration::from_secs(60);
/// How long a conversation waits to be looked at again while something of
/// the reader's is in the way, as an open file chooser is. Nothing reports
/// when that ends, and no attempt is spent.
const RECHECK: Duration = Duration::from_secs(2);
/// How long a conversation that is due stands behind what is already under
/// way before it is looked at again unprompted. An attempt's answer, or that
/// of the start it stands behind, looks sooner; this is for the one that
/// never answers, as when its pane closes first.
const STANDBY: Duration = Duration::from_secs(10);

/// Where one disconnected conversation stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Waiting for its next attempt, due at this instant.
    Waiting(Instant),
    /// The attempt this start is making.
    Attaching(Target),
    /// Back since this instant.
    Held(Instant),
    /// Left to its pane's Reconnect. `again`: nothing said another attempt
    /// could not work, so a daemon that comes back is worth asking again.
    GaveUp { again: bool },
}

#[derive(Debug)]
struct Lost {
    /// Attempts made since the connection last held.
    attempts: usize,
    step: Step,
}

#[derive(Debug, Default)]
pub(super) struct Reattach {
    chats: HashMap<CardId, Lost>,
    /// The instant the timer that is out fires, so a later one is not armed
    /// and an earlier one that still fires is not taken for it.
    wake: Option<Instant>,
    /// Whether the daemon could not be reached since it last answered.
    away: bool,
    /// Conversations that came back since the window last said how it went.
    reconnected: HashSet<CardId>,
    /// The conversations the window last said could not come back.
    reported_failed: HashSet<CardId>,
}

impl Reattach {
    /// Whether `card`'s conversation lost a connection this still follows.
    pub(super) fn tracks(&self, card: CardId) -> bool {
        self.chats.contains_key(&card)
    }

    /// Whether `target` is an attempt this made, rather than the reader.
    pub(super) fn attaching(&self, target: Target) -> bool {
        self.chats
            .get(&target.card)
            .is_some_and(|lost| lost.step == Step::Attaching(target))
    }

    /// Whether `card`'s conversation is on its way back by itself.
    fn pending(&self, card: CardId) -> bool {
        self.chats
            .get(&card)
            .is_some_and(|lost| matches!(lost.step, Step::Waiting(_) | Step::Attaching(_)))
    }

    /// Whether `card`'s conversation waits for an attempt still to come.
    pub(super) fn waits(&self, card: CardId) -> bool {
        self.chats
            .get(&card)
            .is_some_and(|lost| matches!(lost.step, Step::Waiting(_)))
    }

    #[cfg(test)]
    fn attempts(&self, card: CardId) -> Option<usize> {
        self.chats.get(&card).map(|lost| lost.attempts)
    }

    #[cfg(test)]
    fn due(&self, card: CardId) -> Option<Instant> {
        match self.chats.get(&card)?.step {
            Step::Waiting(due) => Some(due),
            _ => None,
        }
    }

    #[cfg(test)]
    fn gave_up(&self, card: CardId) -> bool {
        self.chats
            .get(&card)
            .is_some_and(|lost| matches!(lost.step, Step::GaveUp { .. }))
    }
}

/// Whether a conversation may be put back by itself.
enum Readiness {
    /// Attach this saved conversation now.
    Now(Box<chat::Session>),
    /// Something of the reader's is in the way for the moment.
    Later,
    /// Another start already has its card, and its answer looks again.
    Awaited,
    /// The pane's own controls are the way back, or there is nothing to
    /// attach to, for the reason the log gives.
    Never(&'static str),
}

impl Cagents {
    /// A conversation the window was connected to lost that connection.
    pub(super) fn chat_connection_lost(&mut self, card: CardId, now: Instant) -> Task<Message> {
        if let Readiness::Never(why) = self.reattach_readiness(card) {
            tracing::debug!(%card, why, "a conversation that lost its connection is left to its pane");
            return Task::none();
        }
        let lost = self.chat_ui.reattach.chats.entry(card).or_insert(Lost {
            attempts: 0,
            step: Step::Held(now),
        });
        match lost.step {
            // The attempt that is out, or the one to come, answers this loss.
            Step::Waiting(_) | Step::Attaching(_) | Step::GaveUp { .. } => {}
            Step::Held(since) => {
                if now.saturating_duration_since(since) >= SETTLED_AFTER {
                    lost.attempts = 0;
                }
                lost.step = match DELAYS.get(lost.attempts) {
                    Some(delay) => Step::Waiting(now + *delay),
                    None => Step::GaveUp { again: true },
                };
                tracing::debug!(%card, attempts = lost.attempts, step = ?lost.step, "a conversation lost its connection");
            }
        }
        // Whatever was due while its timer's firing is still on its way is
        // made here, and the timer armed for what is left.
        self.pump_reattach(now)
    }

    /// The daemon's health changed. One that answers after it could not be
    /// reached is asked for the conversations that lost it. One that only
    /// answered late kept its connections, and changes nothing here.
    pub(in crate::app) fn chat_daemon_health_changed(&mut self, now: Instant) -> Task<Message> {
        use crate::daemon_health::State;
        match self.daemon_health_ui.health {
            State::Unreachable { .. } | State::Incompatible { .. } => {
                self.chat_ui.reattach.away = true;
                Task::none()
            }
            State::Responsive if std::mem::take(&mut self.chat_ui.reattach.away) => {
                self.daemon_reachable_again(now)
            }
            State::Responsive | State::Unresponsive { .. } => Task::none(),
        }
    }

    /// The daemon answers again after it could not be reached. Whatever a
    /// conversation gave up on while it was away is worth asking of the one
    /// that is back, with a new set of attempts, and nothing still has a
    /// reason to wait out a long delay.
    fn daemon_reachable_again(&mut self, now: Instant) -> Task<Message> {
        let soon = now + DELAYS[0];
        let cards = self
            .cards
            .iter()
            .filter_map(|(card, runtime)| matches!(runtime, CardRuntime::Chat(_)).then_some(*card))
            .collect::<Vec<_>>();
        for card in cards {
            if matches!(self.reattach_readiness(card), Readiness::Never(_)) {
                continue;
            }
            let lost = self.chat_ui.reattach.chats.entry(card).or_insert(Lost {
                attempts: 0,
                step: Step::GaveUp { again: true },
            });
            match lost.step {
                Step::Waiting(due) => {
                    lost.attempts = 0;
                    lost.step = Step::Waiting(due.min(soon));
                }
                Step::GaveUp { again: true } => {
                    lost.attempts = 0;
                    lost.step = Step::Waiting(soon);
                }
                // An attempt that set out before the daemon was back
                // counts for nothing against it.
                Step::Attaching(_) => lost.attempts = 0,
                // The daemon already said why not.
                Step::GaveUp { again: false } | Step::Held(_) => {}
            }
        }
        self.pump_reattach(now)
    }

    /// The timer armed for `due` fired. One that an earlier timer replaced
    /// still fires, and has nothing left to do.
    pub(super) fn reattach_due(&mut self, due: Instant, now: Instant) -> Task<Message> {
        if self.chat_ui.reattach.wake != Some(due) {
            return Task::none();
        }
        self.chat_ui.reattach.wake = None;
        self.pump_reattach(now)
    }

    /// The reader does not wait for `card`'s next attempt: it is made now.
    pub(super) fn reattach_now(&mut self, card: CardId, now: Instant) -> Task<Message> {
        if let Some(lost) = self.chat_ui.reattach.chats.get_mut(&card)
            && let Step::Waiting(due) = lost.step
        {
            lost.step = Step::Waiting(due.min(now));
        }
        self.pump_reattach(now)
    }

    /// `card` is gone, and with it whatever was owed to its conversation.
    pub(super) fn reattach_forget(&mut self, card: CardId) {
        if self.chat_ui.reattach.chats.remove(&card).is_some() {
            self.report_reattach();
        }
    }

    /// A start on a conversation this follows was answered: by a connection,
    /// by a refusal, or by nothing its card can use. `refused` is how the
    /// start failed, when it did.
    ///
    /// `current`: the start was still its card's when the answer arrived,
    /// rather than one another start had since taken the card from.
    pub(super) fn reattach_answered(
        &mut self,
        target: Target,
        refused: Option<chat::Recovery>,
        current: bool,
        now: Instant,
    ) -> Task<Message> {
        let card = target.card;
        // A start that waits on a receipt of its own answers again later, and
        // so does the one an attempt was handed on to, as when its card's
        // agent names the conversation the card did not. A start that took
        // the card from an attempt is not the attempt's: its answer is read
        // as the reader's.
        if let Some(Phase::Starting(attempt)) = self.chat_ui.phases.get(&card) {
            if current
                && let Some(lost) = self.chat_ui.reattach.chats.get_mut(&card)
                && lost.step == Step::Attaching(target)
            {
                lost.step = Step::Attaching(attempt.target);
            }
            return Task::none();
        }
        let connected = matches!(self.cards.get(&card),
            Some(CardRuntime::Chat(runtime)) if runtime.connected());
        let reattach = &mut self.chat_ui.reattach;
        let Some(lost) = reattach.chats.get_mut(&card) else {
            return Task::none();
        };
        let automatic = lost.step == Step::Attaching(target);
        if connected {
            match lost.step {
                // Back while the window was on it, whoever's start got
                // there: its own, the daemon handing the pane its
                // conversation's new host, or the reader not waiting.
                Step::Waiting(_) | Step::Attaching(_) => {
                    lost.step = Step::Held(now);
                    reattach.reconnected.insert(card);
                    tracing::debug!(%card, automatic, "a conversation reconnected");
                }
                // The reader's Reconnect did what the window gave up on.
                Step::GaveUp { .. } => {
                    reattach.chats.remove(&card);
                }
                Step::Held(_) => {}
            }
        } else if automatic {
            // A connection that closed as it was installed reports no
            // refusal, and is as worth another attempt as one that says so.
            let again = refused.is_none_or(|recovery| {
                matches!(
                    recovery,
                    chat::Recovery::Retry
                        | chat::Recovery::Busy
                        | chat::Recovery::Unresponsive
                        | chat::Recovery::Unreached
                )
            });
            tracing::debug!(%card, ?refused, attempts = lost.attempts, "an automatic reconnect failed");
            if refused == Some(chat::Recovery::UpdateRecovery) {
                // The window asks the daemon when its update restore ends,
                // and reopens this pane then (see `update_recovery`).
                reattach.chats.remove(&card);
            } else if let Some(delay) = DELAYS.get(lost.attempts).filter(|_| again) {
                lost.step = Step::Waiting(now + *delay);
            } else {
                lost.step = Step::GaveUp { again };
            }
        }
        self.pump_reattach(now)
    }

    /// Starts the attempts that are due, the conversations on screen first,
    /// says how it went once none is left under way, and arms the timer for
    /// the next.
    fn pump_reattach(&mut self, now: Instant) -> Task<Message> {
        // An attempt another start took the card from has no answer coming.
        for (card, lost) in &mut self.chat_ui.reattach.chats {
            if let Step::Attaching(target) = lost.step
                && !matches!(self.chat_ui.phases.get(card),
                    Some(Phase::Starting(attempt)) if attempt.target == target)
            {
                lost.step = Step::Waiting(now);
            }
        }
        let mut attaching = self
            .chat_ui
            .reattach
            .chats
            .values()
            .filter(|lost| matches!(lost.step, Step::Attaching(_)))
            .count();
        let mut due = self
            .chat_ui
            .reattach
            .chats
            .iter()
            .filter_map(|(card, lost)| match lost.step {
                Step::Waiting(due) if due <= now => Some((
                    self.workspace.visibility(*card) != Some(VisibilityClass::Visible),
                    due,
                    *card,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        due.sort_unstable_by_key(|(hidden, due, _)| (*hidden, *due));
        // A window that speaks another protocol than the daemon cannot
        // attach to it, however often it asks. The daemon's banner says what
        // to do, and the daemon that answers after it is asked again.
        let incompatible = matches!(
            self.daemon_health_ui.health,
            crate::daemon_health::State::Incompatible { .. }
        );
        let mut tasks = Vec::new();
        for (_, _, card) in due {
            let session = match self.reattach_readiness(card) {
                Readiness::Now(session) if !incompatible => session,
                Readiness::Later if !incompatible => {
                    if let Some(lost) = self.chat_ui.reattach.chats.get_mut(&card) {
                        lost.step = Step::Waiting(now + RECHECK);
                    }
                    continue;
                }
                // Still due: that start's answer comes here.
                Readiness::Awaited if !incompatible => continue,
                Readiness::Never(why) => {
                    tracing::debug!(%card, why, "a conversation is left to its pane");
                    self.chat_ui.reattach.chats.remove(&card);
                    continue;
                }
                Readiness::Now(_) | Readiness::Later | Readiness::Awaited => {
                    self.chat_ui.reattach.chats.remove(&card);
                    continue;
                }
            };
            if attaching >= CONCURRENT {
                continue;
            }
            let intent = if self.chat_runs_as_open_agent(card, &session) {
                // An agent the daemon manages runs where its registry says.
                // A provider the window started beside that would not be the
                // agent's, and its pane would stop following the agent. So
                // this pane only rejoins what the daemon still hosts.
                chat::Intent::RestoreExisting
            } else {
                // A window that may only attach to what exists keeps to that.
                chat::client::restore_intent(
                    chat::Intent::Restore,
                    crate::instance::window_restore_policy(),
                )
            };
            let task = self.start_chat(card, *session, intent, None, false);
            let started = self.chat_ui.attempt(card).map(|attempt| attempt.target);
            let Some(lost) = self.chat_ui.reattach.chats.get_mut(&card) else {
                continue;
            };
            match started {
                Some(target) => {
                    lost.attempts += 1;
                    lost.step = Step::Attaching(target);
                    attaching += 1;
                    tracing::debug!(%card, attempt = lost.attempts, "reconnecting a conversation by itself");
                    tasks.push(task);
                }
                // Refused before it began, for a reason its pane now gives.
                None => lost.step = Step::GaveUp { again: false },
            }
        }
        // Whatever was closed since has nothing left to attach, one that got
        // back some other way has nothing left to retry, and a connection
        // that has held is as good as one that was never lost.
        let cards = &self.cards;
        self.chat_ui.reattach.chats.retain(|card, lost| {
            let Some(CardRuntime::Chat(runtime)) = cards.get(card) else {
                return false;
            };
            match lost.step {
                Step::Waiting(_) | Step::Attaching(_) => true,
                Step::GaveUp { .. } => !runtime.connected(),
                Step::Held(since) => now.saturating_duration_since(since) < SETTLED_AFTER,
            }
        });
        self.show_reattaching();
        self.report_reattach();
        tasks.push(self.arm_reattach(now));
        Task::batch(tasks)
    }

    /// Whether `card`'s conversation may be put back without being asked.
    fn reattach_readiness(&self, card: CardId) -> Readiness {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Readiness::Never("it shows no conversation");
        };
        let open = self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open());
        let never = [
            // A remote device's bridge may start its daemon on connection,
            // which nothing may do unasked.
            (crate::device::is_remote(), "another device hosts it"),
            (!runtime.disconnected, "it is connected"),
            (runtime.is_draft(), "it has not started"),
            (
                runtime.closed_elsewhere || self.chat_closed_elsewhere(card),
                "it was closed elsewhere",
            ),
            // Its provider had stopped: there is nothing to rejoin, and
            // starting it again is the reader's to ask for. The connection
            // may have reported that after the pane last read it.
            (runtime.latest_snapshot().exited, "its provider had stopped"),
            (runtime.resume_failed, "its resume failed"),
            (
                runtime.start_blocked.is_some() || self.chat_retry_blocked(card),
                "its start was refused",
            ),
            (runtime.capacity_refused(), "the daemon is full"),
            (
                runtime.auth_error().is_some(),
                "its account needs signing in",
            ),
            (
                self.chat_ui.fresh_offers.contains(&card),
                "it offers a new conversation",
            ),
            (
                self.chat_ui.removed_accounts.contains(&card),
                "its account was removed",
            ),
            // A launch the reader asked for is theirs to repeat.
            (
                self.chat_ui.explicit_starts.contains_key(&card),
                "the reader started it",
            ),
            (
                self.shared_views.quarantined.contains(&card),
                "its saved session is unconfirmed",
            ),
            (
                self.deleting_card_reason(card).is_some() || !open,
                "it is closing",
            ),
        ];
        if let Some((_, why)) = never.into_iter().find(|(holds, _)| *holds) {
            return Readiness::Never(why);
        }
        // A start of the reader's, the daemon handing the pane its
        // conversation's new host, or a reopen waiting for its conversation
        // to stop: each attaches the card itself.
        if self.chat_start_pending(card)
            || self.chat_awaiting_admission(card)
            || self.holds_reopen(card)
        {
            return Readiness::Awaited;
        }
        // What the pane's own Reconnect waits for, and an update about to
        // replace the daemon.
        if self.update_prepared
            || !runtime.commands.is_idle()
            || runtime.attachment_chooser_pending.is_some()
            || runtime.pastes_pending > 0
        {
            return Readiness::Later;
        }
        Readiness::Now(Box::new(runtime.session.clone()))
    }

    /// Tells each pane whether its conversation is on its way back.
    fn show_reattaching(&mut self) {
        let reattach = &self.chat_ui.reattach;
        for (card, runtime) in &mut self.cards {
            if let CardRuntime::Chat(runtime) = runtime {
                runtime.reattaching = reattach.pending(*card);
            }
        }
    }

    /// Arms the timer for the earliest attempt still to come, unless one
    /// that fires no later is already out.
    fn arm_reattach(&mut self, now: Instant) -> Task<Message> {
        let reattach = &mut self.chat_ui.reattach;
        let Some(due) = reattach
            .chats
            .values()
            .filter_map(|lost| match lost.step {
                Step::Waiting(due) if due > now => Some(due),
                // One already due stands behind an attempt or another start,
                // whose answer looks again. Should none come, this does.
                Step::Waiting(_) => Some(now + STANDBY),
                _ => None,
            })
            .min()
        else {
            return Task::none();
        };
        if reattach.wake.is_some_and(|wake| wake > now && wake <= due) {
            return Task::none();
        }
        reattach.wake = Some(due);
        cosmic::task::future(async move {
            tokio::time::sleep_until(due.into()).await;
            action(Action::ReattachDue(due))
        })
    }

    /// Says how the attempts went, once, when none is left under way: how
    /// many conversations came back, and how many still wait for the reader.
    /// The notice that stands follows that second number down as the reader
    /// reconnects them.
    fn report_reattach(&mut self) {
        let reattach = &mut self.chat_ui.reattach;
        if reattach
            .chats
            .values()
            .any(|lost| matches!(lost.step, Step::Waiting(_) | Step::Attaching(_)))
        {
            return;
        }
        let failed = reattach
            .chats
            .iter()
            .filter_map(|(card, lost)| matches!(lost.step, Step::GaveUp { .. }).then_some(*card))
            .collect::<HashSet<_>>();
        // One that came back and was lost for good since counts once, as
        // what it ended as.
        let reconnected = std::mem::take(&mut reattach.reconnected)
            .difference(&failed)
            .count();
        if reconnected == 0 && failed == reattach.reported_failed {
            return;
        }
        // The reader reconnecting the rest takes the count down with them,
        // and a notice they dismissed is not raised again for that. One more
        // conversation left to them is news, whatever the count.
        let fewer = reconnected == 0 && failed.is_subset(&reattach.reported_failed);
        reattach.reported_failed = failed;
        let failed = reattach.reported_failed.len();
        if fewer
            && !self
                .notices
                .entries
                .iter()
                .any(|entry| entry.notice.key == Some(NoticeKey::ChatReattach))
        {
            return;
        }
        let chats = |count: usize| if count == 1 { "chat" } else { "chats" };
        let notice = match (reconnected, failed) {
            (0, 0) => return self.resolve_notice(NoticeKey::ChatReattach),
            (reconnected, 0) => TransientNotice::success(format!(
                "Reconnected {reconnected} {}.",
                chats(reconnected)
            )),
            (0, failed) => TransientNotice::warning(format!(
                "{failed} {} couldn't reconnect. {FAILED_NEXT}",
                chats(failed)
            )),
            (reconnected, failed) => TransientNotice::warning(format!(
                "Reconnected {reconnected} {}; {failed} couldn't reconnect. {FAILED_NEXT}",
                chats(reconnected)
            )),
        };
        self.report_notice(notice.with_key(NoticeKey::ChatReattach));
    }
}

/// Where the reader finds what the attempts left for them.
const FAILED_NEXT: &str = "Each one says why in its pane.";

#[cfg(test)]
mod tests {
    use super::super::tests::{answer_start, chat_runtime, fixture_handle, live_chat};
    use super::*;
    use crate::daemon_health::State as Health;

    fn test_app() -> Cagents {
        crate::app::tests::context_inspector_test_app(false)
    }

    fn away() -> Health {
        Health::Unreachable {
            since: std::time::SystemTime::now(),
        }
    }

    /// The connection of `card`'s conversation drops at `now`.
    fn lose(app: &mut Cagents, card: CardId, now: Instant) {
        let target = chat_runtime(app, card).target;
        chat_runtime(app, card)
            .handle()
            .expect("a connected conversation")
            .disconnect_for_test();
        let runtime = app.chat_runtime_mut(target).expect("chat runtime");
        runtime.disconnected = true;
        runtime.error = Some("fixture: the daemon went away".into());
        drop(app.chat_connection_lost(card, now));
    }

    fn attempt(app: &Cagents, card: CardId) -> Option<Target> {
        app.chat_ui.attempt(card).map(|attempt| attempt.target)
    }

    fn refuse(app: &mut Cagents, card: CardId, recovery: chat::Recovery) {
        let target = attempt(app, card).expect("an attempt is out");
        answer_start(
            app,
            target,
            Err(chat::StartFailure {
                message: "fixture: the daemon refused".into(),
                recovery,
            }),
        );
    }

    fn reconnect(app: &mut Cagents, card: CardId) {
        let target = attempt(app, card).expect("an attempt is out");
        let handle = fixture_handle(app, card, Some("thread"));
        answer_start(app, target, Ok(handle));
    }

    /// Makes the attempt `card` waits for, and returns when it was due.
    fn make_attempt(app: &mut Cagents, card: CardId) -> Instant {
        let due = app.chat_ui.reattach.due(card).expect("an attempt to come");
        drop(app.pump_reattach(due));
        assert!(attempt(app, card).is_some(), "the attempt started when due");
        due
    }

    /// Refuses the attempt that is out, and checks the next waits `delay`.
    fn refuse_then_wait(app: &mut Cagents, card: CardId, delay: Duration) {
        let before = Instant::now();
        refuse(app, card, chat::Recovery::Retry);
        let after = Instant::now();
        let due = app.chat_ui.reattach.due(card).expect("another attempt");
        assert!(
            before + delay <= due && due <= after + delay,
            "the next attempt waits {delay:?}"
        );
    }

    fn notices(app: &Cagents) -> Vec<(NoticeSeverity, String)> {
        app.notices
            .entries
            .iter()
            .filter(|entry| entry.notice.key == Some(NoticeKey::ChatReattach))
            .map(|entry| (entry.notice.severity, entry.notice.message.clone()))
            .collect()
    }

    /// The acceptance case: a daemon that goes away and comes back puts
    /// every disconnected conversation back without a click, and says so
    /// once.
    #[test]
    fn disconnected_chats_reconnect_by_themselves_once_the_daemon_is_back() {
        let mut app = test_app();
        let first = live_chat(&mut app, Some("thread"));
        let second = live_chat(&mut app, Some("thread"));
        let lost = Instant::now() - DELAYS[0];
        drop(app.update(Message::DaemonHealth(away())));
        lose(&mut app, first, lost);
        lose(&mut app, second, lost);
        for card in [first, second] {
            let runtime = chat_runtime(&app, card);
            assert!(runtime.disconnected && runtime.reattaching);
            assert_eq!(app.chat_ui.reattach.due(card), Some(lost + DELAYS[0]));
            assert!(attempt(&app, card).is_none(), "nothing starts unasked");
        }

        drop(app.update(Message::DaemonHealth(Health::Responsive)));
        for card in [first, second] {
            assert_eq!(
                app.chat_ui.attempt(card).map(|attempt| attempt.intent),
                Some(chat::Intent::Restore),
                "an automatic attempt only puts the pane back"
            );
        }
        reconnect(&mut app, first);
        assert!(notices(&app).is_empty(), "the other is still under way");
        reconnect(&mut app, second);
        for card in [first, second] {
            let runtime = chat_runtime(&app, card);
            assert!(!runtime.disconnected && !runtime.reattaching);
            assert!(runtime.error.is_none());
        }
        assert_eq!(
            notices(&app),
            [(NoticeSeverity::Success, "Reconnected 2 chats.".to_owned())]
        );
    }

    /// Each failure waits longer, and the attempts end: the pane then keeps
    /// its Reconnect button and the daemon's reason.
    #[test]
    fn a_chat_that_cannot_reconnect_backs_off_then_keeps_its_manual_control() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        let lost = Instant::now();
        lose(&mut app, card, lost);
        assert_eq!(app.chat_ui.reattach.due(card), Some(lost + DELAYS[0]));
        drop(app.pump_reattach(lost + DELAYS[0] - Duration::from_millis(1)));
        assert!(
            attempt(&app, card).is_none(),
            "an attempt waits its delay out"
        );
        for (made, delay) in DELAYS.iter().enumerate().skip(1) {
            make_attempt(&mut app, card);
            assert_eq!(app.chat_ui.reattach.attempts(card), Some(made));
            assert!(chat_runtime(&app, card).reattaching);
            refuse_then_wait(&mut app, card, *delay);
            assert!(notices(&app).is_empty(), "nothing is said while it tries");
        }
        make_attempt(&mut app, card);
        refuse(&mut app, card, chat::Recovery::Retry);
        assert!(app.chat_ui.reattach.gave_up(card));
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(DELAYS.len()));
        drop(app.pump_reattach(Instant::now() + Duration::from_secs(3600)));
        assert!(attempt(&app, card).is_none(), "the attempts are bounded");
        let runtime = chat_runtime(&app, card);
        assert!(runtime.disconnected && !runtime.reattaching);
        assert_eq!(
            runtime.error.as_deref(),
            Some("fixture: the daemon refused")
        );
        assert_eq!(runtime.conversation_status_base(false).1, "Disconnected");
        assert_eq!(
            runtime.stopped(false, false).primary,
            Some(("Reconnect", stopped::Press::Reconnect, true))
        );
        assert_eq!(
            notices(&app),
            [(
                NoticeSeverity::Warning,
                format!("1 chat couldn't reconnect. {FAILED_NEXT}")
            )]
        );

        // The reader's own Reconnect withdraws the report once it works.
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::Reconnect(target))));
        reconnect(&mut app, card);
        assert!(!chat_runtime(&app, card).disconnected);
        assert!(notices(&app).is_empty());
    }

    /// A refusal that trying again cannot answer ends the attempts at once,
    /// also for the daemon that comes back: it already said no.
    #[test]
    fn a_refusal_that_retrying_cannot_answer_is_not_retried() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        make_attempt(&mut app, card);
        refuse(&mut app, card, chat::Recovery::Blocked);
        assert!(!chat_runtime(&app, card).reattaching);
        drop(app.update(Message::DaemonHealth(away())));
        drop(app.update(Message::DaemonHealth(Health::Responsive)));
        drop(app.pump_reattach(Instant::now() + Duration::from_secs(3600)));
        assert!(attempt(&app, card).is_none());
        assert!(!chat_runtime(&app, card).reattaching);
    }

    /// A daemon that comes back is asked again for what gave up while it
    /// was away, and at once rather than after a long delay.
    #[test]
    fn a_daemon_that_comes_back_gets_a_new_set_of_attempts() {
        let mut app = test_app();
        let exhausted = live_chat(&mut app, Some("thread"));
        let waiting = live_chat(&mut app, Some("thread"));
        lose(&mut app, exhausted, Instant::now());
        for _ in DELAYS {
            make_attempt(&mut app, exhausted);
            refuse(&mut app, exhausted, chat::Recovery::Unreached);
        }
        assert!(app.chat_ui.reattach.gave_up(exhausted));
        lose(&mut app, waiting, Instant::now());
        for _ in 1..DELAYS.len() {
            make_attempt(&mut app, waiting);
            refuse(&mut app, waiting, chat::Recovery::Unreached);
        }
        let long = app.chat_ui.reattach.due(waiting).expect("a last attempt");

        let back = Instant::now();
        assert!(long > back + DELAYS[0]);
        drop(app.daemon_reachable_again(back));
        for card in [waiting, exhausted] {
            assert_eq!(app.chat_ui.reattach.due(card), Some(back + DELAYS[0]));
            assert_eq!(app.chat_ui.reattach.attempts(card), Some(0));
            assert!(chat_runtime(&app, card).reattaching);
        }
        drop(app.pump_reattach(back + DELAYS[0]));
        reconnect(&mut app, waiting);
        reconnect(&mut app, exhausted);
        assert!(!chat_runtime(&app, exhausted).disconnected);
        assert_eq!(
            notices(&app),
            [(NoticeSeverity::Success, "Reconnected 2 chats.".to_owned())]
        );
    }

    /// A window can hold far more conversations than a daemon should be
    /// asked to attach at once, and the ones on screen go first.
    #[test]
    fn attempts_are_bounded_in_number_and_start_with_the_visible_chats() {
        let mut app = test_app();
        let cards = (0..CONCURRENT + 4)
            .map(|_| live_chat(&mut app, Some("thread")))
            .collect::<Vec<_>>();
        // A window too narrow for all of them, looking at the last ones.
        let narrow = app.workspace.resize_viewport(900.0);
        drop(app.apply_viewport(narrow, false, DurableChange::None));
        let end = app.workspace.native_scroll(f32::MAX, 900.0);
        drop(app.apply_viewport(end, false, DurableChange::None));
        let visible = cards
            .iter()
            .copied()
            .filter(|card| app.workspace.visibility(*card) == Some(VisibilityClass::Visible))
            .collect::<Vec<_>>();
        assert!(
            !visible.is_empty() && visible.len() <= CONCURRENT,
            "the window shows some of its chats: {} of {}",
            visible.len(),
            cards.len()
        );
        let lost = Instant::now() - DELAYS[0];
        for card in &cards {
            lose(&mut app, *card, lost);
        }
        drop(app.pump_reattach(lost + DELAYS[0]));
        let out = |app: &Cagents| {
            cards
                .iter()
                .copied()
                .filter(|card| attempt(app, *card).is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(out(&app).len(), CONCURRENT);
        for card in &visible {
            assert!(attempt(&app, *card).is_some(), "a visible chat goes first");
        }
        // Each answer makes room for one more, until all are back.
        let first = out(&app)[0];
        reconnect(&mut app, first);
        assert_eq!(out(&app).len(), CONCURRENT);
        while let Some(card) = out(&app).first().copied() {
            reconnect(&mut app, card);
        }
        assert!(
            cards
                .iter()
                .all(|card| !chat_runtime(&app, *card).disconnected)
        );
        assert_eq!(
            notices(&app),
            [(
                NoticeSeverity::Success,
                format!("Reconnected {} chats.", cards.len())
            )]
        );
    }

    /// A connection that drops again straight away continues the delays
    /// rather than starting them over, so one that cannot stay up stops.
    #[test]
    fn a_connection_that_does_not_hold_keeps_counting_its_attempts() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        for made in 1..=DELAYS.len() {
            make_attempt(&mut app, card);
            reconnect(&mut app, card);
            assert!(!chat_runtime(&app, card).disconnected);
            assert_eq!(app.chat_ui.reattach.attempts(card), Some(made));
            lose(&mut app, card, Instant::now());
        }
        assert!(app.chat_ui.reattach.gave_up(card));
        assert!(!chat_runtime(&app, card).reattaching);

        // One that held is treated as new when it is next lost.
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        make_attempt(&mut app, card);
        reconnect(&mut app, card);
        let later = Instant::now() + SETTLED_AFTER;
        lose(&mut app, card, later);
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(0));
        assert_eq!(app.chat_ui.reattach.due(card), Some(later + DELAYS[0]));
    }

    /// What only the reader may decide is left to the pane's own controls.
    #[test]
    fn a_chat_whose_way_back_is_the_readers_choice_is_left_alone() {
        let mut app = test_app();
        // Its provider had stopped: attaching would start it again.
        let stopped = live_chat(&mut app, Some("thread"));
        let handle = Arc::clone(chat_runtime(&app, stopped).handle().unwrap());
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.exited = true;
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        let target = chat_runtime(&app, stopped).target;
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(chat_runtime(&app, stopped).snapshot.exited);
        // The daemon refused it for a reason a retry does not answer.
        let blocked = live_chat(&mut app, Some("thread"));
        let target = chat_runtime(&app, blocked).target;
        app.chat_runtime_mut(target).unwrap().start_blocked =
            Some(blocked_recovery::Reopen::AfterRestart);
        for card in [stopped, blocked] {
            lose(&mut app, card, Instant::now());
            assert!(!chat_runtime(&app, card).reattaching);
        }
        drop(app.update(Message::DaemonHealth(away())));
        drop(app.update(Message::DaemonHealth(Health::Responsive)));
        drop(app.pump_reattach(Instant::now() + Duration::from_secs(3600)));
        for card in [stopped, blocked] {
            assert!(attempt(&app, card).is_none());
        }
    }

    /// A conversation that never recorded an identity cannot be resumed by a
    /// new daemon. The reader's own Reconnect then opens a draft in its
    /// place; an attempt nobody asked for must not take the pane away.
    #[test]
    fn an_automatic_attempt_never_turns_a_chat_into_a_draft() {
        let mut app = test_app();
        let card = live_chat(&mut app, None);
        lose(&mut app, card, Instant::now());
        make_attempt(&mut app, card);
        refuse(&mut app, card, chat::Recovery::Fresh);
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.is_draft(), "the pane keeps its conversation");
        assert!(runtime.disconnected && !runtime.reattaching);
        assert!(app.chat_ui.reattach.gave_up(card));
    }

    /// While something of the reader's is in the way the attempt waits,
    /// without spending one.
    #[test]
    fn an_attempt_waits_for_the_reader_to_finish_attaching_files() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target).unwrap().pastes_pending = 1;
        let due = app.chat_ui.reattach.due(card).unwrap();
        drop(app.pump_reattach(due));
        assert!(attempt(&app, card).is_none());
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(0));
        assert_eq!(app.chat_ui.reattach.due(card), Some(due + RECHECK));
        assert!(chat_runtime(&app, card).reattaching);
        app.chat_runtime_mut(target).unwrap().pastes_pending = 0;
        make_attempt(&mut app, card);
    }

    /// A window the daemon no longer speaks the protocol of cannot attach
    /// however often it asks, so it does not ask, and its panes do not say
    /// it does: they keep their button, under the banner that says what the
    /// daemon needs.
    #[test]
    fn a_window_incompatible_with_the_daemon_leaves_its_chats_to_the_banner() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        drop(app.update(Message::DaemonHealth(Health::Incompatible {
            since: std::time::SystemTime::now(),
            actual: 1,
        })));
        lose(&mut app, card, Instant::now());
        let due = app.chat_ui.reattach.due(card).unwrap();
        drop(app.pump_reattach(due));
        assert!(attempt(&app, card).is_none());
        assert!(!chat_runtime(&app, card).reattaching);
        assert!(notices(&app).is_empty());
        // The daemon that answers after it is asked.
        drop(app.update(Message::DaemonHealth(Health::Responsive)));
        assert!(chat_runtime(&app, card).reattaching);
        make_attempt(&mut app, card);
    }

    /// The pane says its conversation is coming back, and still lets the
    /// reader ask at once.
    #[test]
    fn a_reconnecting_pane_says_so_and_keeps_its_button() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.conversation_status_base(false).1, "Reconnecting");
        let stopped = runtime.stopped(false, false);
        assert!(
            stopped.guidance.contains("Reconnecting"),
            "{}",
            stopped.guidance
        );
        assert_eq!(
            stopped.primary,
            Some(("Reconnect now", stopped::Press::Reconnect, true))
        );
        make_attempt(&mut app, card);
        assert_eq!(
            chat_runtime(&app, card).conversation_status_base(true).1,
            "Reconnecting"
        );

        // The reader not waiting is counted like any other way back.
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::Reconnect(target))));
        reconnect(&mut app, card);
        assert!(!chat_runtime(&app, card).disconnected);
        drop(app.pump_reattach(Instant::now() + Duration::from_secs(3600)));
        assert!(attempt(&app, card).is_none(), "nothing is left to retry");
        assert_eq!(
            notices(&app),
            [(NoticeSeverity::Success, "Reconnected 1 chat.".to_owned())]
        );
    }

    /// The wake that reports a lost connection is what starts all this.
    #[test]
    fn a_disconnect_wake_schedules_the_first_attempt() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        let target = chat_runtime(&app, card).target;
        let before = Instant::now();
        drop(app.update(Message::ChatUi(Action::Wake(
            target,
            Wake::Disconnected("fixture: the daemon went away".into()),
        ))));
        let due = app.chat_ui.reattach.due(card).expect("an attempt to come");
        assert!(before + DELAYS[0] <= due && due <= Instant::now() + DELAYS[0]);
        assert!(chat_runtime(&app, card).reattaching);
        make_attempt(&mut app, card);
        reconnect(&mut app, card);
        assert!(!chat_runtime(&app, card).disconnected);

        // The connection that replaced it reports its own loss the same way.
        let target = chat_runtime(&app, card).target;
        let before = Instant::now();
        drop(app.update(Message::ChatUi(Action::Wake(
            target,
            Wake::Disconnected("fixture: the daemon went away again".into()),
        ))));
        let due = app.chat_ui.reattach.due(card).expect("another attempt");
        assert!(before + DELAYS[1] <= due && due <= Instant::now() + DELAYS[1]);
        assert!(chat_runtime(&app, card).reattaching);
    }

    /// The timer is the only thing that makes a waiting attempt, so a dialog
    /// may not swallow it, and one an earlier timer replaced does nothing.
    #[test]
    fn the_timer_fires_under_a_dialog_and_only_the_one_that_is_out_counts() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now() - DELAYS[0]);
        let out = app.chat_ui.reattach.wake.expect("a timer is out");
        assert!(Action::ReattachDue(out).allowed_while_modal());
        let replaced = out - Duration::from_millis(1);
        drop(app.update(Message::ChatUi(Action::ReattachDue(replaced))));
        assert!(attempt(&app, card).is_none());
        assert_eq!(app.chat_ui.reattach.wake, Some(out));
        drop(app.update(Message::ChatUi(Action::ReattachDue(out))));
        assert!(attempt(&app, card).is_some());
    }

    /// A daemon that answered late lost no connection, so it gives nothing a
    /// new set of attempts: the bound holds however often it stalls.
    #[test]
    fn a_daemon_that_only_answered_late_restarts_nothing() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        for _ in DELAYS {
            make_attempt(&mut app, card);
            refuse(&mut app, card, chat::Recovery::Retry);
        }
        assert!(app.chat_ui.reattach.gave_up(card));
        drop(app.update(Message::DaemonHealth(Health::Unresponsive {
            since: std::time::SystemTime::now(),
            timeout: Duration::from_secs(5),
        })));
        drop(app.update(Message::DaemonHealth(Health::Responsive)));
        assert!(app.chat_ui.reattach.gave_up(card));
        assert!(!chat_runtime(&app, card).reattaching);
    }

    /// An attempt that set out while the daemon was away fails for that
    /// alone, and is made again as soon as the others.
    #[test]
    fn an_attempt_the_daemon_came_back_during_starts_over() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        drop(app.update(Message::DaemonHealth(away())));
        lose(&mut app, card, Instant::now());
        for _ in 1..DELAYS.len() {
            make_attempt(&mut app, card);
            refuse(&mut app, card, chat::Recovery::Unreached);
        }
        make_attempt(&mut app, card);
        drop(app.update(Message::DaemonHealth(Health::Responsive)));
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(0));
        refuse_then_wait(&mut app, card, DELAYS[0]);
    }

    /// A start of the reader's has the card when the attempt comes due. The
    /// attempt does not start beside it: that start's answer is what looks
    /// again.
    #[test]
    fn an_attempt_stands_back_for_a_start_of_the_readers() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now() - DELAYS[0]);
        let due = app.chat_ui.reattach.due(card).unwrap();
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::Reconnect(target))));
        let readers = attempt(&app, card).expect("the reader's start");
        drop(app.pump_reattach(Instant::now()));
        assert_eq!(attempt(&app, card), Some(readers));
        assert_eq!(app.chat_ui.reattach.due(card), Some(due));
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(0));
        // It failed for a reason another attempt may answer.
        refuse(&mut app, card, chat::Recovery::Retry);
        let own = attempt(&app, card).expect("the window's attempt");
        assert_ne!(own, readers);
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(1));
    }

    /// A managed agent's provider is the daemon's to start. A provider the
    /// window started for its pane would run beside the agent, unknown to
    /// its registry, and the pane would stop following the agent.
    #[test]
    fn a_managed_agents_pane_only_rejoins_what_the_daemon_hosts() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        let source = chat_runtime(&app, card).session.clone();
        app.workspace_model
            .agents
            .push(crate::app::channel_ui::close_tests::agent(&source));
        lose(&mut app, card, Instant::now());
        make_attempt(&mut app, card);
        assert_eq!(
            app.chat_ui.attempt(card).map(|attempt| attempt.intent),
            Some(chat::Intent::RestoreExisting)
        );
        // Nothing hosts it any more, which an attachment that starts
        // nothing hears as a refusal to try again: every attempt stays one,
        // and the pane keeps its conversation and its button.
        for _ in 1..DELAYS.len() {
            refuse(&mut app, card, chat::Recovery::Retry);
            make_attempt(&mut app, card);
            assert_eq!(
                app.chat_ui.attempt(card).map(|attempt| attempt.intent),
                Some(chat::Intent::RestoreExisting)
            );
        }
        refuse(&mut app, card, chat::Recovery::Retry);
        assert!(app.chat_ui.reattach.gave_up(card));
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.is_draft());
        assert!(runtime.disconnected && !runtime.reattaching);
    }

    /// Reconnect now on an agent's pane makes the attempt the window was
    /// waiting to make, not the reader's own kind, which starts a provider.
    #[test]
    fn reconnect_now_on_a_managed_agents_pane_only_brings_its_attempt_forward() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        let source = chat_runtime(&app, card).session.clone();
        app.workspace_model
            .agents
            .push(crate::app::channel_ui::close_tests::agent(&source));
        lose(&mut app, card, Instant::now());
        assert_eq!(
            chat_runtime(&app, card).stopped(false, false).primary,
            Some(("Reconnect now", stopped::Press::Reconnect, true))
        );
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::Reconnect(target))));
        assert_eq!(
            app.chat_ui.attempt(card).map(|attempt| attempt.intent),
            Some(chat::Intent::RestoreExisting)
        );
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(1));
    }

    /// A daemon still restoring after an update refuses the attempt, and the
    /// window reopens what it refused once the restore ends. An agent's pane
    /// is still only put back on what the daemon hosts then.
    #[test]
    fn a_managed_agents_pane_starts_nothing_once_an_update_restore_ends() {
        use crate::terminal::ipc::StartupState;
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        let source = chat_runtime(&app, card).session.clone();
        app.workspace_model
            .agents
            .push(crate::app::channel_ui::close_tests::agent(&source));
        lose(&mut app, card, Instant::now());
        make_attempt(&mut app, card);
        let target = attempt(&app, card).unwrap();
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure {
                message: format!("{}: fixture", chat::STARTUP_RECOVERY_REFUSAL),
                recovery: chat::Recovery::UpdateRecovery,
            }),
        );
        assert!(!app.chat_ui.reattach.tracks(card), "the restore's to end");
        drop(app.update_recovery_answered(Ok(StartupState::Restoring { awaiting: 1 })));
        drop(app.update_recovery_answered(Ok(StartupState::Ready)));
        drop(app.update(Message::ChatUi(Action::UpdateRecoveryReady)));
        assert!(attempt(&app, card).is_none(), "nothing starts at once");
        // A later release finds nothing of this pane's left to retry.
        drop(app.update(Message::ChatUi(Action::UpdateRecoveryReady)));
        assert!(attempt(&app, card).is_none());
        make_attempt(&mut app, card);
        assert_eq!(
            app.chat_ui.attempt(card).map(|attempt| attempt.intent),
            Some(chat::Intent::RestoreExisting)
        );
    }

    /// What an attempt stands behind may never answer, as a start whose
    /// pane's close took its place. The conversation is looked at again
    /// without it.
    #[test]
    fn an_attempt_behind_a_start_that_never_answers_is_made_anyway() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now() - DELAYS[0]);
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::Reconnect(target))));
        let readers = attempt(&app, card).expect("the reader's start");
        let now = Instant::now();
        drop(app.pump_reattach(now));
        assert_eq!(app.chat_ui.reattach.wake, Some(now + STANDBY));
        app.chat_ui.phases.remove(&card);
        drop(app.update(Message::ChatUi(Action::ReattachDue(now + STANDBY))));
        let own = attempt(&app, card).expect("the window's attempt");
        assert_ne!(own, readers);
    }

    /// A start that takes the card from an attempt is the reader's: the
    /// attempt's late answer does not make it the window's, to be retried or
    /// kept from what the reader asked of it.
    #[test]
    fn a_start_that_took_the_card_from_an_attempt_is_not_the_windows() {
        let mut app = test_app();
        let card = live_chat(&mut app, Some("thread"));
        lose(&mut app, card, Instant::now());
        make_attempt(&mut app, card);
        let own = attempt(&app, card).unwrap();
        let session = chat_runtime(&app, card).session.clone();
        drop(app.start_chat(card, session, chat::Intent::Restore, None, false));
        let readers = attempt(&app, card).expect("the start that took the card");
        assert_ne!(readers, own);
        answer_start(
            &mut app,
            own,
            Err(chat::StartFailure {
                message: "fixture: the daemon refused".into(),
                recovery: chat::Recovery::Retry,
            }),
        );
        assert_eq!(attempt(&app, card), Some(readers));
        assert!(!app.chat_ui.reattach.attaching(readers));
        assert_eq!(app.chat_ui.reattach.attempts(card), Some(1));
    }

    /// The notice counts conversations, not connections: one that came back
    /// twice is one, and one that came back and was then lost for good is
    /// only one that could not.
    #[test]
    fn a_conversation_is_counted_once_however_often_it_came_back() {
        let mut app = test_app();
        let flapping = live_chat(&mut app, Some("thread"));
        let slow = live_chat(&mut app, Some("thread"));
        let lost_for_good = live_chat(&mut app, Some("thread"));
        for card in [flapping, slow, lost_for_good] {
            lose(&mut app, card, Instant::now());
        }
        for card in [flapping, lost_for_good] {
            make_attempt(&mut app, card);
            reconnect(&mut app, card);
        }
        lose(&mut app, flapping, Instant::now());
        make_attempt(&mut app, flapping);
        reconnect(&mut app, flapping);
        lose(&mut app, lost_for_good, Instant::now());
        make_attempt(&mut app, lost_for_good);
        refuse(&mut app, lost_for_good, chat::Recovery::Blocked);
        // The third set out with the others, and answers last.
        assert!(notices(&app).is_empty(), "one is still under way");
        reconnect(&mut app, slow);
        assert_eq!(
            notices(&app),
            [(
                NoticeSeverity::Warning,
                format!("Reconnected 2 chats; 1 couldn't reconnect. {FAILED_NEXT}")
            )]
        );
    }

    /// The reader who dismissed the notice and reconnects the rest by hand
    /// is not told again each time the number goes down. Another
    /// conversation left to them is said, though the number is the same.
    #[test]
    fn a_dismissed_notice_stays_dismissed_as_the_reader_reconnects_the_rest() {
        fn give_up(app: &mut Cagents, card: CardId) {
            lose(app, card, Instant::now());
            for _ in DELAYS {
                make_attempt(app, card);
                refuse(app, card, chat::Recovery::Unreached);
            }
            assert!(app.chat_ui.reattach.gave_up(card));
        }
        let mut app = test_app();
        let first = live_chat(&mut app, Some("thread"));
        let second = live_chat(&mut app, Some("thread"));
        let third = live_chat(&mut app, Some("thread"));
        give_up(&mut app, first);
        give_up(&mut app, second);
        assert_eq!(notices(&app).len(), 1);
        app.notices.clear();
        let target = chat_runtime(&app, first).target;
        drop(app.update(Message::ChatUi(Action::Reconnect(target))));
        reconnect(&mut app, first);
        assert!(!chat_runtime(&app, first).disconnected);
        assert!(notices(&app).is_empty());

        give_up(&mut app, third);
        assert_eq!(
            notices(&app),
            [(
                NoticeSeverity::Warning,
                format!("2 chats couldn't reconnect. {FAILED_NEXT}")
            )]
        );
    }

    /// A pane closed while it waits for the reader is no longer one of the
    /// conversations the notice counts.
    #[test]
    fn a_closed_pane_leaves_the_count() {
        let mut app = test_app();
        let kept = live_chat(&mut app, Some("thread"));
        let closed = live_chat(&mut app, Some("thread"));
        for card in [kept, closed] {
            lose(&mut app, card, Instant::now());
            make_attempt(&mut app, card);
            refuse(&mut app, card, chat::Recovery::Blocked);
        }
        assert_eq!(
            notices(&app),
            [(
                NoticeSeverity::Warning,
                format!("2 chats couldn't reconnect. {FAILED_NEXT}")
            )]
        );
        app.chat_forget(closed);
        assert_eq!(
            notices(&app),
            [(
                NoticeSeverity::Warning,
                format!("1 chat couldn't reconnect. {FAILED_NEXT}")
            )]
        );
        app.chat_forget(kept);
        assert!(notices(&app).is_empty());
    }
}
