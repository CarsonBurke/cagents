use super::*;
use crate::{
    control::{ControlClient, shutdown_outcome_message},
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
    failed: Arc<AtomicBool>,
}

impl Application for DaemonStop {
    type Executor = cosmic::executor::Default;
    type Flags = (ShutdownCapability, Arc<AtomicBool>);
    type Message = StopMessage;
    const APP_ID: &'static str = "dev.cagents.Cagents.StopConfirmation";
    fn init(core: Core, (capability, failed): Self::Flags) -> (Self, Task<StopMessage>) {
        (Self { core, capability, pending: false, settled: false, detail: "Confirming this request terminates the counted hosted terminals. Cancel makes no shutdown request.".into(), failed }, Task::none())
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
                if !self.settled {
                    let _ = writeln!(
                        std::io::stdout(),
                        "Daemon shutdown cancelled; no shutdown was requested."
                    );
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
                self.detail = match result {
                    Ok(outcome) => {
                        self.failed.store(
                            !matches!(outcome, ShutdownOutcome::Accepted { .. }),
                            Ordering::Release,
                        );
                        shutdown_outcome_message(&outcome)
                    }
                    Err(error) => {
                        self.failed.store(true, Ordering::Release);
                        format!("Failed: daemon shutdown was not confirmed: {error}")
                    }
                };
                let _ = writeln!(std::io::stdout(), "{}", self.detail);
                Task::none()
            }
            StopMessage::Refreshed(result) if self.pending => {
                self.pending = false;
                match result {
                    Ok(capability) => {
                        self.capability = capability;
                        self.settled = false;
                        self.detail = "Fresh inventory loaded. Confirm the count below; no shutdown has been requested by this refresh.".into();
                    }
                    Err(error) => {
                        self.settled = true;
                        self.failed.store(true, Ordering::Release);
                        self.detail = format!("Failed to refresh the daemon: {error}");
                    }
                }
                Task::none()
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
                widget::text::title2("Stop daemon?").into(),
                widget::text::title3(format!(
                    "Terminate exactly {} live terminal(s)",
                    self.capability.live_terminals
                ))
                .into(),
                widget::text::body(&self.detail).into(),
                widget::row::with_children(vec![
                    widget::button::standard(if self.settled { "Close" } else { "Cancel" })
                        .on_press_maybe((!self.pending).then_some(StopMessage::Cancel))
                        .into(),
                    widget::button::standard("Refresh count")
                        .on_press_maybe(
                            (!self.pending && self.settled && self.failed.load(Ordering::Acquire))
                                .then_some(StopMessage::Refresh),
                        )
                        .into(),
                    widget::button::destructive("Stop daemon")
                        .on_press_maybe(
                            (!self.pending && !self.settled).then_some(StopMessage::Confirm),
                        )
                        .into(),
                ])
                .spacing(8)
                .into(),
            ])
            .spacing(16),
        )
        .padding(24)
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
}
