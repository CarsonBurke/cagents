use super::*;
use crate::{
    control::{ControlClient, Refusal, RequestError, StopReport, shutdown_outcome_summary},
    terminal::ipc::{ShutdownCapability, ShutdownOutcome},
};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub enum StopMessage {
    Confirm,
    Refresh,
    Cancel,
    Finished(Result<ShutdownOutcome, RequestError>),
    Refreshed(Result<ShutdownCapability, RequestError>),
    Key(keyboard::Event),
}

/// What a stop confirmation does, which the dialog keeps saying whenever
/// Confirm is offered.
const CONSEQUENCE: &str = "Confirming stops these hosted terminal/chat sessions and all their background subagents. Background work will not resume automatically. Cancel makes no shutdown request.";

/// What the dialog says when the daemon refused to count again; the earlier
/// result and its Refresh stay as they were.
fn refresh_refused(refusal: Refusal) -> String {
    format!(
        "{}, so the count was not refreshed. Try again in a moment.",
        refusal.cause()
    )
}

/// What the dialog says after a refused stop: nothing was stopped, the count
/// and its capability still stand, and Confirm still does what it did.
fn refused_detail(refusal: Refusal) -> String {
    format!(
        "{}, so nothing was stopped. Try again in a moment.\n\n{CONSEQUENCE}",
        refusal.cause()
    )
}

pub struct DaemonStop {
    core: Core,
    capability: ShutdownCapability,
    pending: bool,
    settled: bool,
    detail: String,
    /// What the dialog said before the refresh in flight, which it says
    /// again if a busy daemon refuses that refresh.
    before_refresh: Option<String>,
    /// What the tray that opened this dialog is told once it closes.
    report: Option<StopReport>,
    failed: Arc<AtomicBool>,
}

impl Application for DaemonStop {
    type Executor = cosmic::executor::Default;
    type Flags = (ShutdownCapability, Arc<AtomicBool>);
    type Message = StopMessage;
    const APP_ID: &'static str = "dev.cagents.Cagents.StopConfirmation";
    fn init(core: Core, (capability, failed): Self::Flags) -> (Self, Task<StopMessage>) {
        (
            Self {
                core,
                capability,
                pending: false,
                settled: false,
                detail: CONSEQUENCE.into(),
                before_refresh: None,
                report: None,
                failed,
            },
            iced::widget::operation::focus(widget::Id::new("daemon-stop.cancel"))
                .map(cosmic::Action::App),
        )
    }
    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }
    fn on_close_requested(&self, _: window::Id) -> Option<StopMessage> {
        Some(StopMessage::Cancel)
    }
    fn update(&mut self, message: StopMessage) -> Task<StopMessage> {
        match message {
            StopMessage::Key(event) if is_escape_press(&event) => self.update(StopMessage::Cancel),
            StopMessage::Cancel if !self.pending => {
                let report = self.report.take().unwrap_or(StopReport::Cancelled);
                if let Ok(line) = serde_json::to_string(&report) {
                    let _ = writeln!(std::io::stdout(), "{line}");
                }
                iced::exit().map(cosmic::Action::App)
            }
            StopMessage::Confirm if !self.pending && !self.settled => {
                self.pending = true;
                self.detail = "Pending: validating the exact daemon incarnation, inventory and expiring capability…".into();
                let capability = self.capability.clone();
                cosmic::task::future(async move {
                    StopMessage::Finished(
                        tokio::task::spawn_blocking(move || {
                            Ok(client()?.confirm_shutdown(capability)?)
                        })
                        .await
                        .map_err(|error| RequestError::Failed(error.to_string()))
                        .and_then(|result| result),
                    )
                })
            }
            StopMessage::Refresh if !self.pending => {
                self.pending = true;
                self.before_refresh = Some(std::mem::replace(
                    &mut self.detail,
                    "Refreshing daemon inventory; no shutdown is being requested.".into(),
                ));
                cosmic::task::future(async {
                    StopMessage::Refreshed(
                        tokio::task::spawn_blocking(|| {
                            Ok(client()?.shutdown_preflight()?.capability)
                        })
                        .await
                        .map_err(|error| RequestError::Failed(error.to_string()))
                        .and_then(|result| result),
                    )
                })
            }
            StopMessage::Finished(result) if self.pending => {
                self.pending = false;
                let report = match result {
                    Err(RequestError::Refused(refusal)) => {
                        // Refused before anything was stopped: the reviewed
                        // count and its capability still stand, so Confirm
                        // stays available. A capability that expires
                        // meanwhile is answered as Expired.
                        // Focus stays on the safe choice, as when the
                        // dialog opened; Confirm is one press away.
                        self.detail = refused_detail(refusal);
                        return iced::widget::operation::focus(widget::Id::new(
                            "daemon-stop.cancel",
                        ))
                        .map(cosmic::Action::App);
                    }
                    Ok(outcome) => {
                        self.failed.store(
                            !matches!(outcome, ShutdownOutcome::Accepted { .. }),
                            Ordering::Release,
                        );
                        self.detail = shutdown_outcome_summary(&outcome);
                        StopReport::Finished { outcome }
                    }
                    Err(RequestError::Failed(error)) => {
                        self.failed.store(true, Ordering::Release);
                        self.detail = format!("Could not stop the cagents daemon: {error}");
                        StopReport::Failed { error }
                    }
                };
                self.settled = true;
                self.report = Some(report);
                iced::widget::operation::focus(widget::Id::new("daemon-stop.cancel"))
                    .map(cosmic::Action::App)
            }
            StopMessage::Refreshed(result) if self.pending => {
                self.pending = false;
                let before = self.before_refresh.take().unwrap_or_default();
                match result {
                    Ok(capability) => {
                        self.capability = capability;
                        self.settled = false;
                        self.report = None;
                        self.detail = "Fresh inventory loaded. Confirm the count below; no shutdown has been requested by this refresh.".into();
                    }
                    // The refresh issued nothing, so the dialog stays as it
                    // was, Refresh and the earlier result's text included;
                    // the refusal follows that text once.
                    Err(RequestError::Refused(refusal)) => {
                        let refused = format!("\n\n{}", refresh_refused(refusal));
                        let earlier = before.strip_suffix(&refused).unwrap_or(&before);
                        self.detail = format!("{earlier}{refused}");
                    }
                    Err(RequestError::Failed(error)) => {
                        self.settled = true;
                        self.failed.store(true, Ordering::Release);
                        self.detail = format!("Could not refresh the cagents daemon: {error}");
                        self.report = Some(StopReport::Failed { error });
                    }
                }
                iced::widget::operation::focus(widget::Id::new("daemon-stop.cancel"))
                    .map(cosmic::Action::App)
            }
            _ => Task::none(),
        }
    }
    fn subscription(&self) -> Subscription<StopMessage> {
        event::listen_with(|event, _, _| match event {
            iced::Event::Keyboard(event) => Some(StopMessage::Key(event)),
            _ => None,
        })
    }
    fn view(&self) -> Element<'_, StopMessage> {
        // The window is small and a failed refresh or stop reports the
        // daemon's own error, so the explanation scrolls and the actions
        // stay in reach.
        widget::container(super::dialog_ui::scrolling_with_actions(
            widget::column::with_children(vec![
                crate::design::display("Stop daemon?").into(),
                crate::design::title(format!(
                    "Stop {} hosted terminal/chat sessions and {} background subagents",
                    self.capability.live_terminals, self.capability.background_subagents
                ))
                .into(),
                crate::design::body(&self.detail).into(),
            ])
            .spacing(space::L),
            super::dialog_ui::confirmation_actions(
                crate::ui_controls::button::secondary("Cancel")
                    .instead("Close", self.settled)
                    .id(widget::Id::new("daemon-stop.cancel"))
                    .on_press_maybe((!self.pending).then_some(StopMessage::Cancel)),
                widget::row::with_children(vec![
                    crate::ui_controls::button::secondary("Refresh count")
                        .on_press_maybe(
                            (!self.pending && self.settled && self.failed.load(Ordering::Acquire))
                                .then_some(StopMessage::Refresh),
                        )
                        .into(),
                    crate::ui_controls::button::destructive("Stop daemon")
                        .id(widget::Id::new("daemon-stop.confirm"))
                        .on_press_maybe(
                            (!self.pending && !self.settled).then_some(StopMessage::Confirm),
                        )
                        .into(),
                ])
                .spacing(space::S),
            ),
            Padding::ZERO.bottom(space::L),
            crate::design::base,
        ))
        .padding(space::XL)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

fn client() -> Result<ControlClient, RequestError> {
    let socket = crate::terminal::ipc::daemon_socket_path()
        .map_err(|error| RequestError::Failed(error.to_string()))?;
    Ok(ControlClient::connect(&socket)?)
}

pub fn run_daemon_stop_confirmation(
    capability: ShutdownCapability,
) -> Result<(), Box<dyn std::error::Error>> {
    use cosmic::app::cosmic::Cosmic;
    type Shell = Cosmic<DaemonStop>;
    let failed = Arc::new(AtomicBool::new(false));
    let initial = capability;
    let result_flag = failed.clone();
    iced::daemon(
        move || {
            let (window, open) = window::open(window::Settings {
                size: iced::Size::new(640.0, 340.0),
                exit_on_close_request: false,
                ..window::Settings::default()
            });
            let mut core = Core::default();
            core.set_main_window_id(Some(window));
            let (shell, task) = Shell::init((core, (initial.clone(), failed.clone())));
            (shell, iced::Task::batch([open.discard(), task]))
        },
        Shell::update,
        Shell::view,
    )
    .subscription(Shell::subscription)
    .title(|_: &Shell, _| "Stop cagents daemon".to_string())
    .style(Shell::style)
    .theme(Shell::theme)
    .settings(iced::Settings {
        is_daemon: false,
        ..iced::Settings::default()
    })
    .run()?;
    if result_flag.load(Ordering::Acquire) {
        Err("Daemon shutdown was rejected or unconfirmed".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmation(count: u64) -> DaemonStop {
        let capability = ShutdownCapability {
            proof: HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("host"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
            },
            token: "bounded-capability".into(),
            inventory_generation: 4,
            live_terminals: count,
            background_subagents: 0,
            background_identity: String::new(),
            expires_at_unix_ms: i64::MAX,
        };
        DaemonStop::init(
            Core::default(),
            (capability, Arc::new(AtomicBool::new(false))),
        )
        .0
    }

    #[test]
    fn shutdown_confirmation_initial_and_refreshed_focus_is_cancel() {
        let mut app = confirmation(2);
        let (_app, initial) = DaemonStop::init(
            Core::default(),
            (app.capability.clone(), Arc::new(AtomicBool::new(false))),
        );
        // Map only the UI message type to reuse the real focus-operation tree
        // reader; no shutdown or refresh task is polled by this test.
        let initial = initial.map(|_| cosmic::Action::App(Message::DismissStatus));
        let view = app.view().map(|_| Message::DismissStatus);
        assert_eq!(
            super::super::tests::focused_element(view, initial),
            Some(widget::Id::new("daemon-stop.cancel"))
        );
        let unpolled_refresh = app.update(StopMessage::Refresh);
        assert!(app.pending);
        let task = app
            .update(StopMessage::Refreshed(Ok(app.capability.clone())))
            .map(|_| cosmic::Action::App(Message::DismissStatus));
        assert_eq!(
            super::super::tests::focused_element(app.view().map(|_| Message::DismissStatus), task),
            Some(widget::Id::new("daemon-stop.cancel"))
        );
        assert!(!app.pending);
        drop(unpolled_refresh);
    }

    #[test]
    fn counted_confirmation_suppresses_duplicates_and_requires_refresh_after_rejection() {
        for count in [0, 1, 20] {
            let mut app = confirmation(count);
            let _unpolled = app.update(StopMessage::Confirm);
            assert!(app.pending);
            let _duplicate = app.update(StopMessage::Confirm);
            assert_eq!(app.capability.live_terminals, count);
            let _ = app.update(StopMessage::Finished(Ok(
                ShutdownOutcome::InventoryChanged {
                    current_count: count + 1,
                },
            )));
            assert!(app.settled);
            assert!(!app.pending);
            let _rejected = app.update(StopMessage::Confirm);
            assert!(
                !app.pending,
                "stale confirmation cannot send another commit"
            );
            let _ = app.update(StopMessage::Refresh);
            let mut fresh = app.capability.clone();
            fresh.live_terminals = count + 1;
            fresh.inventory_generation += 1;
            let _ = app.update(StopMessage::Refreshed(Ok(fresh)));
            assert!(!app.pending);
            assert!(!app.settled);
            assert_eq!(app.capability.live_terminals, count + 1);
        }
    }

    #[test]
    fn expiry_restart_and_async_failure_are_visible_and_cancel_is_nonmutating() {
        for outcome in [
            Ok(ShutdownOutcome::Expired),
            Ok(ShutdownOutcome::DaemonRestarted),
            Err(RequestError::Failed("connection lost".into())),
        ] {
            let mut app = confirmation(3);
            let _cancel = app.update(StopMessage::Cancel);
            assert!(!app.pending);
            assert!(!app.failed.load(Ordering::Acquire));
            let _unpolled = app.update(StopMessage::Confirm);
            let _ = app.update(StopMessage::Finished(outcome));
            assert!(app.failed.load(Ordering::Acquire));
            assert!(app.settled);
        }
    }

    /// A busy daemon refused the stop before stopping anything, so the
    /// reviewed count and capability still stand: Confirm works again at
    /// once, nothing is reported as failed, and the dialog says why.
    #[test]
    fn a_busy_refusal_leaves_confirm_available() {
        let mut app = confirmation(3);
        let capability = app.capability.clone();
        let _unpolled = app.update(StopMessage::Confirm);
        let _ = app.update(StopMessage::Finished(Err(RequestError::Refused(
            Refusal::Busy,
        ))));
        assert!(!app.pending);
        assert!(!app.settled, "Confirm is still available");
        assert!(!app.failed.load(Ordering::Acquire));
        assert_eq!(app.report, None, "closing now reports a cancellation");
        assert_eq!(app.capability, capability);
        assert_eq!(app.detail, refused_detail(Refusal::Busy));
        // The tray says the same of the same refusal.
        assert!(app.detail.starts_with(crate::control::STOP_REFUSED_BUSY));
        let _unpolled = app.update(StopMessage::Confirm);
        assert!(app.pending, "the same stop is asked again");

        // A busy refresh after a failed stop leaves that failure and its
        // Refresh in place, rather than reporting a second failure.
        let mut app = confirmation(3);
        let _unpolled = app.update(StopMessage::Confirm);
        let _ = app.update(StopMessage::Finished(Ok(ShutdownOutcome::Expired)));
        let report = app.report.clone();
        let failure = app.detail.clone();
        let refused = format!(
            "{failure}\n\nThe cagents daemon was busy finishing another operation, so the count was not refreshed. Try again in a moment."
        );
        for _ in 0..2 {
            let _unpolled = app.update(StopMessage::Refresh);
            let _ = app.update(StopMessage::Refreshed(Err(RequestError::Refused(
                Refusal::Busy,
            ))));
            assert!(!app.pending && app.settled);
            assert_eq!(app.report, report);
            assert_eq!(app.capability, capability);
            // Why Stop daemon is unavailable stays said, and repeated
            // refusals do not stack.
            assert_eq!(app.detail, refused);
        }
        let _unpolled = app.update(StopMessage::Refresh);
        assert!(app.pending, "Refresh can be asked again");
    }

    #[test]
    fn the_tray_is_told_the_typed_outcome_and_the_dialog_shows_plain_copy() {
        let mut app = confirmation(3);
        assert_eq!(app.report, None, "closing untouched reports a cancellation");
        let _unpolled = app.update(StopMessage::Confirm);
        let accepted = ShutdownOutcome::Accepted {
            live_terminals_before: 3,
            termination_requested: true,
        };
        let _ = app.update(StopMessage::Finished(Ok(accepted.clone())));
        assert_eq!(app.detail, "Stopping… 3 hosted sessions will close");
        assert_eq!(app.report, Some(StopReport::Finished { outcome: accepted }));
        let line = serde_json::to_string(app.report.as_ref().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<StopReport>(&line).unwrap(),
            app.report.unwrap()
        );
    }
}
