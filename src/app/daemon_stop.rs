use super::*;
use crate::{
    control::{ControlClient, StopReport, shutdown_outcome_summary},
    terminal::ipc::{ShutdownCapability, ShutdownOutcome},
};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub enum StopMessage {
    Confirm,
    Refresh,
    Cancel,
    Finished(Result<ShutdownOutcome, String>),
    Refreshed(Result<ShutdownCapability, String>),
    Key(keyboard::Event),
}

pub struct DaemonStop {
    core: Core,
    capability: ShutdownCapability,
    pending: bool,
    settled: bool,
    detail: String,
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
        (Self { core, capability, pending: false, settled: false, detail: "Confirming this request terminates the counted hosted terminals. Cancel makes no shutdown request.".into(), report: None, failed }, iced::widget::operation::focus(widget::Id::new("daemon-stop.confirm")).map(cosmic::Action::App))
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
                            client()?
                                .confirm_shutdown(capability)
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result),
                    )
                })
            }
            StopMessage::Refresh if !self.pending => {
                self.pending = true;
                self.detail = "Refreshing daemon inventory; no shutdown is being requested.".into();
                cosmic::task::future(async {
                    StopMessage::Refreshed(
                        tokio::task::spawn_blocking(|| {
                            client()?
                                .shutdown_preflight()
                                .map(|preflight| preflight.capability)
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result),
                    )
                })
            }
            StopMessage::Finished(result) if self.pending => {
                self.pending = false;
                self.settled = true;
                let report = match result {
                    Ok(outcome) => {
                        self.failed.store(
                            !matches!(outcome, ShutdownOutcome::Accepted { .. }),
                            Ordering::Release,
                        );
                        self.detail = shutdown_outcome_summary(&outcome);
                        StopReport::Finished { outcome }
                    }
                    Err(error) => {
                        self.failed.store(true, Ordering::Release);
                        self.detail = format!("Could not stop the daemon: {error}");
                        StopReport::Failed { error }
                    }
                };
                self.report = Some(report);
                iced::widget::operation::focus(widget::Id::new("daemon-stop.cancel"))
                    .map(cosmic::Action::App)
            }
            StopMessage::Refreshed(result) if self.pending => {
                self.pending = false;
                match result {
                    Ok(capability) => {
                        self.capability = capability;
                        self.settled = false;
                        self.report = None;
                        self.detail = "Fresh inventory loaded. Confirm the count below; no shutdown has been requested by this refresh.".into();
                    }
                    Err(error) => {
                        self.settled = true;
                        self.failed.store(true, Ordering::Release);
                        self.detail = format!("Could not refresh the daemon: {error}");
                        self.report = Some(StopReport::Failed { error });
                    }
                }
                iced::widget::operation::focus(widget::Id::new(if self.settled {
                    "daemon-stop.cancel"
                } else {
                    "daemon-stop.confirm"
                }))
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
        widget::container(
            widget::column::with_children(vec![
                crate::design::display("Stop daemon?").into(),
                crate::design::title(format!(
                    "Terminate exactly {} live terminal(s)",
                    self.capability.live_terminals
                ))
                .into(),
                crate::design::body(&self.detail).into(),
                widget::row::with_children(vec![
                    crate::ui_controls::button::secondary(if self.settled {
                        "Close"
                    } else {
                        "Cancel"
                    })
                    .id(widget::Id::new("daemon-stop.cancel"))
                    .on_press_maybe((!self.pending).then_some(StopMessage::Cancel))
                    .into(),
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
                .spacing(space::S)
                .into(),
            ])
            .spacing(space::L),
        )
        .padding(space::XL)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

fn client() -> Result<ControlClient, String> {
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    ControlClient::connect(&socket).map_err(|error| error.to_string())
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
            expires_at_unix_ms: i64::MAX,
        };
        DaemonStop::init(
            Core::default(),
            (capability, Arc::new(AtomicBool::new(false))),
        )
        .0
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
            Err("connection lost".into()),
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
        assert_eq!(app.detail, "Stopping… 3 terminals will close");
        assert_eq!(app.report, Some(StopReport::Finished { outcome: accepted }));
        let line = serde_json::to_string(app.report.as_ref().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<StopReport>(&line).unwrap(),
            app.report.unwrap()
        );
    }
}
