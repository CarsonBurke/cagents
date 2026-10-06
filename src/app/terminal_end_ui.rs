//! The ended terminal: its report, its actions, and the final screen it keeps.
//!
//! A terminal whose process has ended keeps showing what the process left on
//! screen, read-only, with a compact footer that says how it ended and offers
//! what can be done next. A card restored after the window restarted asks the
//! daemon for that final screen again; only when the daemon no longer holds
//! it does the card fall back to saying so.

use super::*;
use crate::{control::ControlClient, terminal::HostedTerminalHandle};

/// How long a restored card waits for the daemon to hand back a final screen.
const RETAINED_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a card whose final screen could not be loaded waits before it
/// asks again. A host that was briefly unreachable at startup must not leave
/// the card saying so for good, but the next visibility pass must not hammer
/// a host that stays unreachable either.
const RETAINED_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// How long after a process ends Enter on its final screen is still taken as
/// typing meant for the process. A command's Enter can arrive a moment after
/// the process exits -- the reader typed ahead, or pressed it twice -- and
/// must not launch a new one.
const ENTER_RELAUNCH_GRACE_MS: i64 = 750;

/// Where a restored ended card stands in getting its final screen back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetainedOutput {
    Attaching,
    /// The daemon no longer holds the screen.
    Discarded,
    /// The daemon could not be asked, or could not hand the screen back. The
    /// card asks again once `retry_at` has passed and it is in view.
    Unreachable {
        retry_at: Instant,
    },
    /// A sign-in's screen is never attached: it can hold a pasted
    /// authorization code.
    Withheld,
}

/// The relaunch an ended terminal's card offers as its primary action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalRelaunch {
    /// A managed agent's conversation lives on in the native chat view.
    OpenConversation,
    /// The agent's exact conversation can be resumed in a new terminal.
    Resume,
    /// Only the command itself can run again, from the start.
    Restart,
}

impl TerminalRelaunch {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::OpenConversation => "Open conversation",
            Self::Resume => "Resume session",
            Self::Restart => "Restart",
        }
    }

    /// What pressing the action does, in the reader's terms.
    pub(super) fn explanation(self, session: &TerminalSession) -> String {
        let name = terminal_process_name(session);
        match self {
            Self::OpenConversation => "Continue this conversation in the chat view".into(),
            Self::Resume => format!("Continue this {name} conversation in a new terminal"),
            Self::Restart => format!(
                "Run {name} again in {}",
                display_path(&session.launch.working_directory)
            ),
        }
    }
}

/// The name a reader knows a terminal's process by: the agent it runs, or the
/// program's own file name.
pub(super) fn terminal_process_name(session: &TerminalSession) -> Cow<'_, str> {
    let program = &session.launch.program;
    if let Some(provider) = crate::agent::Provider::from_program(program) {
        return Cow::Borrowed(provider.label());
    }
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy())
        .filter(|name| !name.is_empty())
        .unwrap_or(Cow::Borrowed("Process"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalOutcome {
    /// Status zero, no signal.
    Clean,
    /// A nonzero status.
    Failed,
    /// Ended by a signal.
    Stopped,
    /// The host recorded no status at all.
    Unknown,
}

impl TerminalOutcome {
    fn tone(self) -> design::Tone {
        design::Tone::Themed(match self {
            Self::Clean => design::success_color,
            Self::Failed => design::warning_color,
            Self::Stopped => design::destructive_color,
            Self::Unknown => design::muted_color,
        })
    }
}

/// How an ended terminal finished, as its card reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalEndReport {
    /// A sentence naming the process and its outcome: `fish exited`,
    /// `cargo exited with code 101`, `Claude was stopped`.
    pub headline: String,
    /// The short status its badge carries: `exit 0`, `exit 101`, `Killed`.
    pub status: String,
    pub outcome: TerminalOutcome,
    pub at_unix_ms: Option<i64>,
}

pub(super) fn terminal_end_report(session: &TerminalSession) -> TerminalEndReport {
    let name = terminal_process_name(session);
    let last_exit = match &session.continuity {
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(last_exit),
            ..
        }
        | TerminalContinuity::Exited { last_exit } => Some(last_exit),
        _ => None,
    };
    let (outcome, headline, status) =
        match last_exit.map(|exit| (exit.code, exit.signal.as_deref())) {
            // The host names a signal the way a shell does -- "Killed",
            // "Terminated", "Hangup" -- and a signalled end has no status of
            // its own worth showing beside that name.
            Some((_, Some(signal))) => (
                TerminalOutcome::Stopped,
                format!("{name} was stopped"),
                signal.to_owned(),
            ),
            Some((Some(0), None)) => (
                TerminalOutcome::Clean,
                format!("{name} exited"),
                "exit 0".to_owned(),
            ),
            Some((Some(code), None)) => (
                TerminalOutcome::Failed,
                format!("{name} exited with code {code}"),
                format!("exit {code}"),
            ),
            Some((None, None)) | None => (
                TerminalOutcome::Unknown,
                format!("{name} ended"),
                "ended".to_owned(),
            ),
        };
    TerminalEndReport {
        headline,
        status,
        outcome,
        at_unix_ms: last_exit.map(|exit| exit.occurred_at_unix_ms),
    }
}

/// Attaches to the final screen the daemon kept for an ended terminal, or
/// `None` when it kept none. Attaching grants nothing: the daemon refuses
/// input, resizing and scrolling for a terminal whose process has ended.
fn attach_retained(
    card: CardId,
    session: &TerminalSession,
    sender: async_channel::Sender<TerminalWakeup>,
) -> Result<Option<TerminalSurface>, String> {
    let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
    let (proof, terminals) = {
        let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
        let terminals = client.list().map_err(|error| error.to_string())?;
        (client.proof().clone(), terminals)
    };
    let retained = terminals.iter().any(|terminal| {
        terminal.session.id == session.id
            && terminal.session.launch == session.launch
            && !terminal.screen_lost_after_crash
            && matches!(
                terminal.session.continuity,
                TerminalContinuity::RelaunchAvailable { .. }
            )
    });
    if !retained {
        return Ok(None);
    }
    let handle = HostedTerminalHandle::connect_at(socket, session.id, Some(proof), move |wake| {
        let _ = sender.try_send(TerminalWakeup {
            card,
            generation: wake.generation,
        });
    })
    .map_err(|error| error.to_string())?;
    handle.wait_until_attached(RETAINED_ATTACH_TIMEOUT)?;
    Ok(Some(TerminalSurface::from(handle)))
}

fn enter_relaunches(ended_at_unix_ms: i64, now_unix_ms: i64) -> bool {
    now_unix_ms.saturating_sub(ended_at_unix_ms) >= ENTER_RELAUNCH_GRACE_MS
}

impl Cagents {
    /// The primary action an ended terminal's card offers.
    pub(super) fn terminal_relaunch(
        &self,
        card: CardId,
        session: &TerminalSession,
    ) -> TerminalRelaunch {
        if self.completed_terminal_has_native_owner(card) {
            TerminalRelaunch::OpenConversation
        } else if session.agent_resume_recipe().is_some() {
            TerminalRelaunch::Resume
        } else {
            TerminalRelaunch::Restart
        }
    }

    /// Relaunches an ended terminal from Enter on its final screen, unless the
    /// key followed the end too closely to have been meant for it.
    pub(super) fn relaunch_from_ended_screen(&mut self, card: CardId) -> Task<Message> {
        let settled = ended_terminal(self.card_state(card)).is_some_and(|session| {
            terminal_end_report(session)
                .at_unix_ms
                .is_none_or(|at| enter_relaunches(at, unix_millis()))
        });
        if settled {
            self.retry_card(card)
        } else {
            Task::none()
        }
    }

    /// Asks the daemon for the final screens of ended cards that are coming
    /// into view. A screen the daemon no longer holds does not come back, so
    /// that answer is final; a daemon that could not be reached is asked
    /// again after a pause.
    pub(super) fn attach_retained_outputs(
        &mut self,
        cards: impl IntoIterator<Item = CardId>,
    ) -> Task<Message> {
        let now = Instant::now();
        let mut tasks = Vec::new();
        for card in cards {
            if !matches!(self.cards.get(&card), Some(CardRuntime::Completed))
                || self.terminal_close_pending(card)
                || self.closed_terminal_cards.contains(&card)
            {
                continue;
            }
            let Some(session) = ended_terminal(self.card_state(card)).cloned() else {
                continue;
            };
            // A sign-in's screen can hold a pasted authorization code, which
            // is why it keeps no saved output either; it stays gone.
            if account_ui::is_sign_in(&session.launch) {
                self.retained_outputs
                    .entry(session.id)
                    .or_insert(RetainedOutput::Withheld);
                continue;
            }
            match self.retained_outputs.get(&session.id) {
                None => {}
                Some(RetainedOutput::Unreachable { retry_at }) if *retry_at <= now => {}
                Some(_) => continue,
            }
            self.retained_outputs
                .insert(session.id, RetainedOutput::Attaching);
            let sender = self.terminal_wake_tx.clone();
            tasks.push(cosmic::task::future(async move {
                let terminal = session.id;
                let result =
                    tokio::task::spawn_blocking(move || attach_retained(card, &session, sender))
                        .await
                        .map_err(|error| format!("terminal worker join failed: {error}"))
                        .and_then(|result| result);
                Message::RetainedTerminalAttached {
                    card,
                    terminal,
                    result,
                }
            }));
        }
        Task::batch(tasks)
    }

    pub(super) fn retained_terminal_attached(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        result: Result<Option<TerminalSurface>, String>,
    ) -> Task<Message> {
        // The card may have been closed, restarted or already given a surface
        // while the daemon answered; then this screen belongs to nobody.
        let current = matches!(self.cards.get(&card), Some(CardRuntime::Completed))
            && !self.terminal_close_pending(card)
            && !self.closed_terminal_cards.contains(&card)
            && ended_terminal(self.card_state(card)).is_some_and(|session| session.id == terminal);
        let outcome = match result {
            Ok(Some(surface)) if current => {
                self.retained_outputs.remove(&terminal);
                self.cards.insert(card, CardRuntime::Terminal(surface));
                let mut tasks = vec![self.drain_terminal(card, 0)];
                // Keyboard focus sat on the relaunch button while the card had
                // no screen. Leaving it there would let one Enter reach both
                // the button and the screen.
                if self.workspace.focused() == Some(card) && !self.workspace_inputs_blocked() {
                    tasks.push(
                        iced::widget::operation::focus(self.card_keyboard_focus(card))
                            .map(cosmic::Action::App),
                    );
                }
                return Task::batch(tasks);
            }
            Ok(Some(surface)) => {
                self.retained_outputs.remove(&terminal);
                self.retire_terminal_surface(surface, false);
                return Task::none();
            }
            Ok(None) => RetainedOutput::Discarded,
            Err(error) => {
                tracing::debug!(%terminal, %error, "final terminal screen could not be attached");
                RetainedOutput::Unreachable {
                    retry_at: Instant::now() + RETAINED_RETRY_INTERVAL,
                }
            }
        };
        self.retained_outputs.insert(terminal, outcome);
        // Without a screen to type into, the keyboard belongs on the
        // relaunch, so Enter still does what the footer says it does.
        if current && self.workspace.focused() == Some(card) && !self.workspace_inputs_blocked() {
            return iced::widget::operation::focus(self.card_keyboard_focus(card))
                .map(cosmic::Action::App);
        }
        Task::none()
    }

    /// The footer an ended terminal shows under its final screen: how the
    /// process ended, when, and what can be done next.
    pub(super) fn ended_terminal_footer<'a>(
        &self,
        card: CardId,
        session: &TerminalSession,
    ) -> Element<'a, Message> {
        let report = terminal_end_report(session);
        let relaunch = self.terminal_relaunch(card, session);
        let spacing = cosmic::theme::spacing();

        let mut summary: Vec<Element<'a, Message>> = vec![
            design::badge(report.status, report.outcome.tone()),
            design::heading(report.headline)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .into(),
        ];
        if let Some(at) = report.at_unix_ms.filter(|at| *at > 0) {
            let now = unix_millis() / 1000;
            summary.push(
                design::caption(crate::account::format_local_moment(at / 1000, now))
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .into(),
            );
        }
        let summary = widget::row::with_children(summary)
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);

        let mut actions: Vec<Element<'a, Message>> = Vec::with_capacity(3);
        if session.transcript_path.is_some() {
            actions.push(
                crate::ui_controls::icon_control(
                    Icon::OpenFile,
                    "Open saved output",
                    false,
                    true,
                    Some(Message::OpenTerminalOutput(card)),
                )
                .id(card_control_id(card, "ended.output"))
                .into(),
            );
        }
        actions.push(
            widget::button::standard("Close")
                .id(card_control_id(card, "ended.close"))
                .name(format!("Close {}", terminal_process_name(session)))
                .on_press(Message::CloseCard(card))
                .into(),
        );
        // The final screen keeps keyboard focus, and Enter there presses
        // this; the tooltip is where that is said without crowding the row.
        actions.push(crate::interaction::tooltip(
            widget::button::suggested(relaunch.label())
                .id(card_control_id(card, "ended.relaunch"))
                .on_press(Message::RetryCard(card)),
            format!("{} (Enter)", relaunch.explanation(session)),
        ));

        widget::container(
            widget::row::with_children(vec![
                widget::container(summary)
                    .width(Length::Fill)
                    .clip(true)
                    .into(),
                widget::row::with_children(actions)
                    .spacing(spacing.space_xxs)
                    .align_y(Alignment::Center)
                    .into(),
            ])
            .spacing(spacing.space_s)
            .align_y(Alignment::Center),
        )
        .padding([spacing.space_xxs, spacing.space_s])
        .width(Length::Fill)
        // A narrow card gives the summary up first, then clips the actions
        // rather than spilling them over its neighbour.
        .clip(true)
        .class(cosmic::theme::Container::custom(|theme| {
            widget::container::Style {
                background: Some(iced::Background::Color(design::raised(theme))),
                border: iced::Border {
                    color: design::hairline(theme),
                    width: design::BORDER_WIDTH,
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into()
    }

    /// What an ended card shows where its final screen would be, while the
    /// daemon is asked for it and once the daemon has said it kept none.
    pub(super) fn ended_terminal_placeholder<'a>(
        &self,
        card: CardId,
        session: Option<&TerminalSession>,
    ) -> Element<'a, Message> {
        let outcome = session.and_then(|session| self.retained_outputs.get(&session.id));
        let detail = match outcome {
            Some(RetainedOutput::Discarded) => {
                "The terminal host no longer holds its final screen."
            }
            Some(RetainedOutput::Withheld) => {
                "Sign-in output is not kept, so codes are not left on screen."
            }
            Some(RetainedOutput::Unreachable { .. }) => {
                "Its final screen could not be loaded from the terminal host."
            }
            // Not asked for yet, or on its way: usually answered within a
            // frame or two, so a message that flashes up and away is noise.
            Some(RetainedOutput::Attaching) | None => {
                return iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into();
            }
        };
        let saved = session.is_some_and(|session| session.transcript_path.is_some());
        let spacing = cosmic::theme::spacing();
        let mut children: Vec<Element<'a, Message>> = vec![
            crate::ui_controls::passive_icon(Icon::Terminal, design::STATUS_ICON_SIZE),
            design::heading("Output not available").into(),
            design::caption(detail)
                .align_x(Alignment::Center)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
        ];
        if saved {
            children.push(
                widget::button::text("Open saved output")
                    .on_press_maybe(session.map(|_| Message::OpenTerminalOutput(card)))
                    .into(),
            );
        }
        widget::container(
            widget::column::with_children(children)
                .align_x(Alignment::Center)
                .spacing(spacing.space_xxs)
                .max_width(STATUS_SURFACE_MAX_WIDTH),
        )
        .center(Length::Fill)
        .padding(spacing.space_m)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(program: &str, code: Option<i32>, signal: Option<&str>) -> TerminalSession {
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: program.into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/work"),
            environment: Default::default(),
        };
        let mut session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        transition_terminal_to_relaunchable(
            &mut session,
            TerminalExit {
                code,
                signal: signal.map(str::to_owned),
                occurred_at_unix_ms: 42_000,
            },
        );
        session
    }

    #[test]
    fn a_report_names_the_process_and_how_it_ended() {
        for (program, code, signal, outcome, headline, status) in [
            (
                "/usr/bin/fish",
                Some(0),
                None,
                TerminalOutcome::Clean,
                "fish exited",
                "exit 0",
            ),
            (
                "cargo",
                Some(101),
                None,
                TerminalOutcome::Failed,
                "cargo exited with code 101",
                "exit 101",
            ),
            (
                "/bin/sh",
                None,
                Some("Killed"),
                TerminalOutcome::Stopped,
                "sh was stopped",
                "Killed",
            ),
            (
                "claude",
                None,
                None,
                TerminalOutcome::Unknown,
                "Claude Code ended",
                "ended",
            ),
        ] {
            let report = terminal_end_report(&ended(program, code, signal));
            assert_eq!(report.outcome, outcome, "{program}");
            assert_eq!(report.headline, headline);
            assert_eq!(report.status, status);
            assert_eq!(report.at_unix_ms, Some(42_000));
        }
    }

    #[test]
    fn a_signal_outranks_the_status_it_leaves_behind() {
        // A shell reports a signalled child as 128+n; the card must not.
        let report = terminal_end_report(&ended("bash", Some(137), Some("Killed")));
        assert_eq!(report.outcome, TerminalOutcome::Stopped);
        assert_eq!(report.status, "Killed");
    }

    /// A restored ended card: its process ended before the window started.
    fn restored_ended_card() -> (Cagents, CardId, TerminalId) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let state = app.card_state_mut(card).unwrap();
        state.residency = Residency::Hibernated;
        let CardKind::Terminal(session) = &mut state.kind else {
            panic!("terminal fixture");
        };
        transition_terminal_to_relaunchable(session, terminal_exit(1, None));
        let terminal = session.id;
        app.cards.insert(card, CardRuntime::Completed);
        (app, card, terminal)
    }

    #[test]
    fn a_restored_ended_card_asks_for_its_final_screen_once() {
        let (mut app, card, terminal) = restored_ended_card();
        // Dropping the task never connects to a daemon.
        drop(app.attach_retained_outputs([card]));
        assert_eq!(
            app.retained_outputs.get(&terminal),
            Some(&RetainedOutput::Attaching)
        );

        let _ = app.update(Message::RetainedTerminalAttached {
            card,
            terminal,
            result: Ok(None),
        });
        assert_eq!(
            app.retained_outputs.get(&terminal),
            Some(&RetainedOutput::Discarded)
        );
        // The card keeps its place and its end; nothing asks again.
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
        assert!(ended_terminal(app.card_state(card)).is_some());
        drop(app.attach_retained_outputs([card]));
        assert_eq!(
            app.retained_outputs.get(&terminal),
            Some(&RetainedOutput::Discarded)
        );
    }

    #[test]
    fn a_failed_attach_leaves_the_card_ended_and_quiet() {
        let (mut app, card, terminal) = restored_ended_card();
        drop(app.attach_retained_outputs([card]));
        app.status = None;
        let _ = app.update(Message::RetainedTerminalAttached {
            card,
            terminal,
            result: Err("daemon unreachable".into()),
        });
        assert!(matches!(
            app.retained_outputs.get(&terminal),
            Some(RetainedOutput::Unreachable { .. })
        ));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
        // The end is already reported in the card; a missing old screen is
        // not an error the reader has to acknowledge.
        assert!(app.status.is_none());
    }

    #[test]
    fn an_unreachable_host_is_asked_again_after_a_pause() {
        let (mut app, card, terminal) = restored_ended_card();
        drop(app.attach_retained_outputs([card]));
        let _ = app.update(Message::RetainedTerminalAttached {
            card,
            terminal,
            result: Err("daemon unreachable".into()),
        });
        // The next visibility pass, moments later, does not ask again.
        drop(app.attach_retained_outputs([card]));
        assert!(matches!(
            app.retained_outputs.get(&terminal),
            Some(RetainedOutput::Unreachable { .. })
        ));

        // Once the pause has passed, it does.
        app.retained_outputs.insert(
            terminal,
            RetainedOutput::Unreachable {
                retry_at: Instant::now(),
            },
        );
        drop(app.attach_retained_outputs([card]));
        assert_eq!(
            app.retained_outputs.get(&terminal),
            Some(&RetainedOutput::Attaching)
        );
    }

    #[test]
    fn an_ended_sign_in_withholds_its_final_screen() {
        let (mut app, card, terminal) = restored_ended_card();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!("terminal fixture");
        };
        session.launch.program = "claude".into();
        session.launch.arguments = vec!["auth".into(), "login".into()];
        drop(app.attach_retained_outputs([card]));
        assert_eq!(
            app.retained_outputs.get(&terminal),
            Some(&RetainedOutput::Withheld)
        );
    }

    #[test]
    fn a_live_card_is_never_asked_for_a_final_screen() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.cards.insert(card, CardRuntime::Loading);
        drop(app.attach_retained_outputs([card]));
        assert!(app.retained_outputs.is_empty());
    }

    #[test]
    fn a_second_relaunch_waits_for_the_first() {
        let (mut app, card, terminal) = restored_ended_card();
        // Dropping the task never launches anything.
        drop(app.retry_card(card));
        let CardKind::Terminal(first) = &app.card_state(card).unwrap().kind else {
            panic!("terminal fixture");
        };
        let first = first.id;
        assert_ne!(first, terminal);
        assert!(app.terminal_start_in_flight(card));

        // The same Enter reaching both the final screen and a focused
        // Restart button must not start, and so tear down, a second one.
        drop(app.retry_card(card));
        let CardKind::Terminal(current) = &app.card_state(card).unwrap().kind else {
            panic!("terminal fixture");
        };
        assert_eq!(current.id, first);
    }

    #[test]
    fn enter_typed_ahead_of_the_end_does_not_relaunch() {
        assert!(!enter_relaunches(10_000, 10_000));
        assert!(!enter_relaunches(
            10_000,
            10_000 + ENTER_RELAUNCH_GRACE_MS - 1
        ));
        assert!(enter_relaunches(10_000, 10_000 + ENTER_RELAUNCH_GRACE_MS));
    }

    #[test]
    fn a_plain_command_restarts_where_it_ran() {
        let session = ended("/usr/bin/fish", Some(0), None);
        assert_eq!(
            TerminalRelaunch::Restart.explanation(&session),
            "Run fish again in /work"
        );
        assert_eq!(TerminalRelaunch::Resume.label(), "Resume session");
    }
}
