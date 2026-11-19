//! Panes that wait on the daemon's startup recovery after an update.
//!
//! While a new daemon restores the conversations an update carried over, and
//! when that restore fails and is held for recovery, it refuses requests that
//! would touch them (`chat::Recovery::UpdateRecovery`), and the shared
//! workspace that admits saved panes may not connect. Both look the same from
//! a pane, and neither is anything wrong with the pane's own conversation, so
//! the window asks the daemon which it is (`StartupState`, which it answers in
//! either case). Each such pane then says which, and once the daemon reports
//! that its recovery has finished, panes it refused reopen, or let their
//! message be sent, without anyone having to find out that they can.
use super::*;
use crate::terminal::ipc::StartupState;

/// How often a window asks while the daemon restores what the update carried.
const WAITING_EVERY: Duration = Duration::from_secs(2);
/// How often it asks otherwise: a held restore lasts as long as its daemon,
/// and while panes still wait after the daemon said it is ready, or could not
/// answer, a later daemon can start out holding its restore.
const SETTLED_EVERY: Duration = Duration::from_secs(10);

/// What the daemon last said about its update restore, for a pane that
/// waits on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::app) enum UpdateWait {
    /// The daemon has not said, could not be asked, or says it is ready.
    #[default]
    Unconfirmed,
    /// Still restoring what the update carried over.
    Restoring,
    /// The restore failed and the daemon holds it for recovery.
    Held,
}

pub(super) const RESTORING_TITLE: &str = "Reopening after the update";
pub(super) const HELD_TITLE: &str = "Held after the update";
const RESTORING: &str = "cagents is still reopening conversations after the update.";
/// The hold lasts as long as the daemon that holds it, so nothing here says
/// it ends by itself. A pane may learn of the hold after the window's
/// retained notice is gone, so it carries the same steps.
const HELD: &str = "The update couldn't reopen every conversation, so cagents is holding them as they were. Nothing was closed or deleted.";
const HELD_STEPS: &str = "To end the hold, let the held sessions finish or close them, then run \"cagents daemon stop\" and reopen cagents.";

impl UpdateWait {
    fn of(state: &StartupState) -> Self {
        match state {
            StartupState::Ready => Self::Unconfirmed,
            StartupState::Restoring { .. } => Self::Restoring,
            StartupState::Retained { .. } => Self::Held,
        }
    }

    /// Whether the daemon says the refusal still stands, so a retry would
    /// only meet it again.
    pub(super) fn holds(self) -> bool {
        self != Self::Unconfirmed
    }

    /// The caption under a refusal that waits on the update.
    pub(super) fn guidance(self, draft: bool) -> &'static str {
        match (self, draft) {
            (Self::Restoring, false) => {
                "cagents is still reopening conversations after the update. This one reopens here when that finishes, with any unsent message."
            }
            (Self::Restoring, true) => {
                "cagents is still reopening conversations after the update. Your message is kept here, and you can send it when that finishes."
            }
            (Self::Held, false) => {
                "The update couldn't reopen every conversation, so cagents is holding this one as it was. Nothing was closed or deleted, and any unsent message is kept here. New conversations can start meanwhile. To end the hold, let the held sessions finish or close them, then run \"cagents daemon stop\" and reopen cagents."
            }
            (Self::Held, true) => {
                "Your message wasn't sent and is kept here: cagents is holding conversations from the update, and this one can't start during the hold. To end the hold, let the held sessions finish or close them, then run \"cagents daemon stop\" and reopen cagents."
            }
            // The action is on the button beside it, whose label differs
            // between a pane and a pane that has no conversation yet.
            (Self::Unconfirmed, false) => {
                "This conversation tried to open while cagents was still reopening conversations after the update. Nothing in it changed."
            }
            (Self::Unconfirmed, true) => {
                "Your message wasn't sent: cagents was still reopening conversations after the update. It's kept here."
            }
        }
    }

    /// What a saved pane still waiting for its workspace says about it: a
    /// title when the update is the reason, and the sentence under it.
    pub(super) fn admission(self) -> (Option<&'static str>, String) {
        const DRAFT_KEPT: &str = "Your unsent draft is kept here.";
        match self {
            Self::Restoring => (
                Some(RESTORING_TITLE),
                format!("{RESTORING} This pane reopens when that finishes. {DRAFT_KEPT}"),
            ),
            Self::Held => (
                Some(HELD_TITLE),
                format!("{HELD} {HELD_STEPS} {DRAFT_KEPT}"),
            ),
            Self::Unconfirmed => (
                None,
                format!("This pane reopens when cagents reconnects to its workspace. {DRAFT_KEPT}"),
            ),
        }
    }
}

/// The window's question to the daemon: at most one out, and at most one
/// due later.
#[derive(Debug, Default)]
pub(super) struct Watch {
    pub(super) wait: UpdateWait,
    asking: bool,
    scheduled: bool,
}

impl Cagents {
    /// Asks the daemon about its update restore when a pane waits on it and
    /// no question is already out or due.
    pub(in crate::app) fn watch_update_recovery(&mut self) -> Task<Message> {
        // A remote bridge may start its daemon on connection, and this
        // question must never start one, so a remote window never asks; its
        // panes offer to try again instead.
        if self.chat_ui.update_recovery.asking
            || crate::device::is_remote()
            || !self.waits_on_update_recovery()
        {
            return Task::none();
        }
        self.chat_ui.update_recovery.asking = true;
        cosmic::task::future(async move {
            let answer = tokio::task::spawn_blocking(move || {
                let socket = crate::terminal::ipc::daemon_socket_path()
                    .map_err(|error| error.to_string())?;
                crate::control::ControlClient::connect(&socket)
                    .and_then(|mut client| client.startup_state())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|answer| answer);
            action(Action::UpdateRecoveryAnswered(answer))
        })
    }

    /// A start the daemon just refused for its update restore. Whatever it
    /// said before, its recovery stood a moment ago, so no pane offers to
    /// try again before it is asked; until it answers, panes say that it is
    /// still reopening conversations.
    pub(super) fn update_refused(&mut self) -> Task<Message> {
        if !crate::device::is_remote() && !self.chat_ui.update_recovery.wait.holds() {
            self.set_update_wait(UpdateWait::Restoring);
        }
        self.watch_update_recovery()
    }

    fn set_update_wait(&mut self, wait: UpdateWait) {
        self.chat_ui.update_recovery.wait = wait;
        for runtime in self.cards.values_mut() {
            if let CardRuntime::Chat(runtime) = runtime {
                runtime.update_wait = wait;
            }
        }
    }

    /// Whether any pane waits on the daemon's update restore: a saved pane
    /// its workspace has not admitted, or a start the restore refused.
    fn waits_on_update_recovery(&self) -> bool {
        !self.durable.pending_chat_admissions.is_empty() || self.update_refusals_stand()
    }

    /// Whether any pane holds a start the update restore refused.
    fn update_refusals_stand(&self) -> bool {
        self.cards.values().any(|runtime| {
            matches!(runtime, CardRuntime::Chat(runtime)
                if runtime.start_blocked == Some(blocked_recovery::Reopen::UpdateRecovery))
        }) || self
            .chat_ui
            .blocked_starts
            .values()
            .any(|blocked| blocked.reopen == blocked_recovery::Reopen::UpdateRecovery)
    }

    pub(super) fn update_recovery_answered(
        &mut self,
        answer: Result<StartupState, String>,
    ) -> Task<Message> {
        self.chat_ui.update_recovery.asking = false;
        let wait = match &answer {
            Ok(state) => UpdateWait::of(state),
            Err(error) => {
                tracing::debug!(%error, "could not ask the daemon about its update restore");
                UpdateWait::Unconfirmed
            }
        };
        self.set_update_wait(wait);
        // Releasing changes what panes show and start, which the answer
        // itself, scoped to the visible panes, does not project.
        let released = if matches!(answer, Ok(StartupState::Ready)) && self.update_refusals_stand()
        {
            cosmic::task::message(action(Action::UpdateRecoveryReady))
        } else {
            Task::none()
        };
        // A question already due asks soon enough.
        if self.chat_ui.update_recovery.scheduled || !self.waits_on_update_recovery() {
            return released;
        }
        self.chat_ui.update_recovery.scheduled = true;
        let every = if wait == UpdateWait::Restoring {
            WAITING_EVERY
        } else {
            SETTLED_EVERY
        };
        Task::batch([
            released,
            cosmic::task::future(async move {
                tokio::time::sleep(every).await;
                action(Action::UpdateRecoveryDue)
            }),
        ])
    }

    pub(super) fn update_recovery_due(&mut self) -> Task<Message> {
        self.chat_ui.update_recovery.scheduled = false;
        self.watch_update_recovery()
    }

    /// The daemon finished its recovery: what it refused is admitted now. A
    /// first message is the reader's to send again, so it only gets its Send
    /// back; a saved conversation reopens, as it would have at startup.
    pub(super) fn release_update_recovery(&mut self) -> Task<Message> {
        if self.chat_ui.update_recovery.wait.holds() {
            return Task::none();
        }
        let mut releases = Vec::new();
        let cards = self.cards.keys().copied().collect::<Vec<_>>();
        for card in cards {
            if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card)
                && runtime.start_blocked == Some(blocked_recovery::Reopen::UpdateRecovery)
            {
                runtime.start_blocked = None;
                if runtime.is_draft() {
                    runtime.error = None;
                } else {
                    let target = runtime.target;
                    releases.push(self.handle_chat_ui_inner(Action::Reconnect(target)));
                }
            } else if self
                .chat_ui
                .blocked_starts
                .get(&card)
                .is_some_and(|blocked| blocked.reopen == blocked_recovery::Reopen::UpdateRecovery)
            {
                self.chat_ui.blocked_starts.remove(&card);
                releases.push(self.retry_card(card));
            }
        }
        Task::batch(releases)
    }
}
