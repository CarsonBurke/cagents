//! The screen a device window shows when it cannot reopen on its device, as
//! after a restart or an update of this computer. No window of this computer
//! is open to explain it, so this one does, as a switch's own window would:
//! it says why, offers **Update NAME** or **Install on NAME** when the
//! device's cagents is the cause, and tries again. It never opens a window on
//! this computer unless asked to.

use super::device_ui::{OpenFailure, diagnose};
use super::device_update_ui::{Event, Failure, Need, RunStatus, Success, run_update, strip};
use super::*;
use cosmic::app::cosmic::Cosmic;
use cosmic::iced::futures::StreamExt;
use std::os::unix::process::CommandExt;

/// What the window that could not open hands this screen.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub device: String,
    /// The device window's slot, scoped to the device.
    pub slot: String,
    /// Why it did not open.
    pub reason: String,
}

impl Request {
    pub fn validate(&self) -> Result<(), String> {
        crate::device::validate_name(&self.device).map_err(|error| error.to_string())?;
        crate::instance::validate_window_slot(&self.slot).map_err(|error| error.to_string())?;
        if !self.slot.starts_with("device-") || self.reason.len() > 4096 {
            return Err("invalid device window request".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum ReconnectMessage {
    Diagnosed(OpenFailure),
    Retry,
    Connected(Result<(), String>),
    StartUpdate,
    UpdateProgress(String),
    UpdateFinished(Result<Success, Failure>),
    OpenHere,
    OpenedHere(Result<(), String>),
    /// Copies a command the reason names.
    CopyCommand(String),
    Close,
}

#[derive(Debug)]
enum Phase {
    /// Finding out why the window did not open.
    Checking,
    /// Opening the window again.
    Connecting,
    Failed(String),
    Needs {
        need: Need,
        blocked: Option<String>,
        status: Option<RunStatus>,
    },
}

struct Reconnect {
    core: Core,
    request: Request,
    phase: Phase,
    /// Why opening this computer's window failed.
    notice: Option<String>,
    /// How often, and when last, the person tried again. A device that
    /// refuses at once would otherwise look as though the button did nothing.
    retries: u32,
    last_retry: Option<std::time::SystemTime>,
}

impl Reconnect {
    fn diagnose(&mut self, reason: String) -> Task<ReconnectMessage> {
        self.phase = Phase::Checking;
        let device = self.request.device.clone();
        cosmic::task::future(async move {
            let failure = tokio::task::spawn_blocking(move || diagnose(&device, Some(&reason)))
                .await
                .unwrap_or_else(|error| OpenFailure::Other(error.to_string()));
            ReconnectMessage::Diagnosed(failure)
        })
    }

    fn connect(&mut self) -> Task<ReconnectMessage> {
        self.phase = Phase::Connecting;
        let Request { device, slot, .. } = self.request.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::instance::launch_device_window(&device, &slot)
                    .map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            ReconnectMessage::Connected(result)
        })
    }

    /// Opening the window or updating the device, which leaving now would
    /// cut short.
    fn committed(&self) -> bool {
        matches!(self.phase, Phase::Connecting) || self.updating()
    }

    /// Updating the device, which closing would cut off partway: the update
    /// stops once it can no longer report its progress here.
    fn updating(&self) -> bool {
        matches!(
            self.phase,
            Phase::Needs {
                status: Some(RunStatus::Running(_)),
                ..
            }
        )
    }

    fn failed_title(&self) -> String {
        let device = &self.request.device;
        if self.retries == 0 {
            format!("Couldn't reconnect to {device}")
        } else {
            format!("Still couldn't reconnect to {device}")
        }
    }

    /// When the last attempt was made, once the person has tried again.
    fn tried_again(&self) -> Option<String> {
        let at = super::daemon_health_ui::local_moment(
            self.last_retry?,
            std::time::SystemTime::now(),
            &jiff::tz::TimeZone::system(),
        );
        Some(match self.retries {
            1 => format!("Tried again at {at}."),
            count => format!("Tried again {count} times, most recently at {at}."),
        })
    }

    fn busy(&self) -> bool {
        self.committed() || matches!(self.phase, Phase::Checking)
    }
}

/// Opens this computer's window in place of the device's, and forgets that
/// it was on the device, so the next start opens it here too.
fn open_here(device: &str, slot: &str) -> Result<(), String> {
    let origin =
        crate::device::placement::origin(device, slot).map_err(|error| error.to_string())?;
    let launcher = crate::companion::Product::Cli
        .beside_current()
        .map_err(|error| error.to_string())?;
    // Naming this computer skips the record, so it can go once the window
    // is on its way.
    std::process::Command::new(launcher)
        .args(["--device", "local", "ui", "--window"])
        .arg(origin.as_deref().unwrap_or(crate::instance::PRIMARY_WINDOW))
        .env_remove(crate::instance::UI_LAUNCH_HANDSHAKE_ENV)
        .env_remove(crate::instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("Could not open cagents on this computer: {error}"))?;
    if let Some(origin) = &origin
        && let Err(error) = crate::device::placement::record(origin, None)
    {
        tracing::warn!(%error, "could not forget which device this window showed");
    }
    Ok(())
}

impl cosmic::Application for Reconnect {
    type Executor = cosmic::executor::Default;
    type Flags = Request;
    type Message = ReconnectMessage;
    const APP_ID: &'static str = "dev.cagents.Cagents.DeviceReconnect";

    fn init(core: Core, request: Request) -> (Self, Task<ReconnectMessage>) {
        let reason = request.reason.clone();
        let mut screen = Self {
            core,
            request,
            phase: Phase::Checking,
            notice: None,
            retries: 0,
            last_retry: None,
        };
        let task = screen.diagnose(reason);
        (screen, task)
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn on_close_requested(&self, _: window::Id) -> Option<ReconnectMessage> {
        Some(ReconnectMessage::Close)
    }

    fn update(&mut self, message: ReconnectMessage) -> Task<ReconnectMessage> {
        match message {
            ReconnectMessage::Diagnosed(failure) => {
                self.phase = match failure {
                    OpenFailure::Needs { need, blocked } => Phase::Needs {
                        need,
                        blocked,
                        status: None,
                    },
                    OpenFailure::Other(error) => Phase::Failed(error),
                };
                Task::none()
            }
            ReconnectMessage::Retry => {
                if self.busy() {
                    return Task::none();
                }
                self.notice = None;
                self.retries += 1;
                self.last_retry = Some(std::time::SystemTime::now());
                self.connect()
            }
            ReconnectMessage::Connected(Ok(())) => iced::exit(),
            ReconnectMessage::Connected(Err(reason)) => self.diagnose(reason),
            ReconnectMessage::StartUpdate => {
                let Phase::Needs {
                    need,
                    blocked: None,
                    status,
                } = &mut self.phase
                else {
                    return Task::none();
                };
                if matches!(need, Need::Newer(_)) || matches!(status, Some(RunStatus::Running(_))) {
                    return Task::none();
                }
                let device = self.request.device.clone();
                *status = Some(RunStatus::Running(format!("Checking {device}…")));
                self.notice = None;
                Task::stream(run_update(device).map(|event| {
                    cosmic::Action::App(match event {
                        Event::Line(line) => ReconnectMessage::UpdateProgress(line),
                        Event::Finished(result) => ReconnectMessage::UpdateFinished(result),
                    })
                }))
            }
            ReconnectMessage::UpdateProgress(line) => {
                if let Phase::Needs {
                    status: Some(RunStatus::Running(latest)),
                    ..
                } = &mut self.phase
                {
                    *latest = line;
                }
                Task::none()
            }
            ReconnectMessage::UpdateFinished(result) => {
                let Phase::Needs { status, .. } = &mut self.phase else {
                    return Task::none();
                };
                match result {
                    // The device can serve this window now. A note, such as
                    // an executable SSH commands miss, stays in view.
                    Ok(Success { note, .. }) => {
                        self.notice = note;
                        self.connect()
                    }
                    Err(Failure { error, report }) => {
                        *status = Some(RunStatus::Failed { error, report });
                        Task::none()
                    }
                }
            }
            ReconnectMessage::OpenHere => {
                if self.committed() {
                    return Task::none();
                }
                let Request { device, slot, .. } = self.request.clone();
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || open_here(&device, &slot))
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                    ReconnectMessage::OpenedHere(result)
                })
            }
            ReconnectMessage::Close if self.updating() => Task::none(),
            ReconnectMessage::CopyCommand(command) => {
                iced::clipboard::write(command).map(cosmic::Action::App)
            }
            ReconnectMessage::OpenedHere(Ok(())) | ReconnectMessage::Close => iced::exit(),
            ReconnectMessage::OpenedHere(Err(error)) => {
                self.notice = Some(error);
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, ReconnectMessage> {
        let device = self.request.device.as_str();
        let committed = self.committed();
        // Only a device that answered is known to keep its work running;
        // nothing runs on one without cagents.
        let mut hosts_work = false;
        let waiting = |title: String| -> Element<'_, ReconnectMessage> {
            widget::row::with_children(vec![
                crate::ui_controls::activity_spinner(20.0, design::accent_color),
                design::title(title).into(),
            ])
            .spacing(space::M)
            .align_y(Alignment::Center)
            .into()
        };
        let mut rows: Vec<Element<'_, ReconnectMessage>> = Vec::new();
        let mut retry = false;
        match &self.phase {
            Phase::Checking => {
                rows.push(waiting(format!("Checking {device}…")));
                rows.push(
                    design::body(format!("This window couldn't reconnect to {device}.")).into(),
                );
            }
            Phase::Connecting => {
                rows.push(waiting(format!("Connecting to {device}…")));
                rows.push(
                    design::body("This window opens as soon as the connection is ready.").into(),
                );
            }
            Phase::Failed(reason) => {
                retry = true;
                rows.push(design::title(self.failed_title()).into());
                rows.push(super::command_text::view(
                    reason,
                    |text| design::body(text).into(),
                    |command| {
                        crate::interaction::tooltip(
                            crate::ui_controls::icon_button(Icon::Copy, "Copy command")
                                .compact()
                                .on_press(ReconnectMessage::CopyCommand(command.to_owned())),
                            "Copy command",
                        )
                    },
                ));
            }
            Phase::Needs {
                need,
                blocked,
                status,
            } => {
                hosts_work = *need != Need::Install;
                // Without an update to offer here, the fix is made on the
                // device or this computer, and then tried again.
                retry = blocked.is_some() || matches!(need, Need::Newer(_));
                rows.push(design::title(self.failed_title()).into());
                rows.push(strip(
                    device,
                    need,
                    blocked.as_deref(),
                    status.as_ref(),
                    Some(ReconnectMessage::StartUpdate),
                    None,
                ));
            }
        }
        if hosts_work {
            rows.push(
                design::caption(format!(
                    "Its terminals and conversations keep running on {device}."
                ))
                .into(),
            );
        }
        if matches!(self.phase, Phase::Failed(_) | Phase::Needs { .. })
            && let Some(tried) = self.tried_again()
        {
            rows.push(design::caption(tried).into());
        }
        if let Some(notice) = &self.notice {
            rows.push(design::caption(notice.clone()).into());
        }
        let mut actions: Vec<Element<'_, ReconnectMessage>> = Vec::new();
        if retry {
            actions.push(
                crate::ui_controls::button::primary("Try again")
                    .on_press(ReconnectMessage::Retry)
                    .into(),
            );
        }
        actions.push(
            crate::ui_controls::button::secondary("Open on this computer")
                .name("Open this window on this computer instead")
                .on_press_maybe((!committed).then_some(ReconnectMessage::OpenHere))
                .into(),
        );
        actions.push(
            crate::ui_controls::button::secondary("Close")
                .on_press_maybe((!self.updating()).then_some(ReconnectMessage::Close))
                .into(),
        );
        rows.push(iced::widget::Space::new().height(Length::Fill).into());
        rows.push(widget::row::with_children(actions).spacing(space::S).into());
        widget::container(widget::column::with_children(rows).spacing(space::L))
            .padding(space::XL)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// Runs the screen until it reopens the window, opens this computer's
/// instead, or is closed.
pub fn run_device_reconnect(request: Request) -> Result<(), Box<dyn std::error::Error>> {
    request.validate()?;
    // One screen per window: another start that failed the same way leaves
    // this one to explain it.
    let Some(_showing) = crate::instance::hold_device_reconnect(&request.slot)? else {
        return Ok(());
    };
    type Shell = Cosmic<Reconnect>;
    cosmic::icon_theme::set_default(cosmic::config::icon_theme());
    let request = std::cell::RefCell::new(Some(request));
    iced::daemon(
        move || {
            let (window, open) = window::open(window::Settings {
                size: iced::Size::new(640.0, 340.0),
                min_size: Some(iced::Size::new(480.0, 300.0)),
                exit_on_close_request: false,
                icon: Some(application_window_icon()),
                // It stands in for the window it reopens, under the same
                // desktop entry.
                #[cfg(target_os = "linux")]
                platform_specific: window::PlatformSpecific {
                    application_id: crate::instance::APPLICATION_ID.to_owned(),
                    ..Default::default()
                },
                ..window::Settings::default()
            });
            let mut core = Core::default();
            core.set_main_window_id(Some(window));
            let (shell, task) = Shell::init((
                core,
                request
                    .borrow_mut()
                    .take()
                    .expect("reconnect screen initialized once"),
            ));
            (shell, iced::Task::batch([open.discard(), task]))
        },
        Shell::update,
        Shell::view,
    )
    .subscription(Shell::subscription)
    .title(|_: &Shell, _| "cagents".to_owned())
    .style(Shell::style)
    .theme(Shell::theme)
    .settings(iced::Settings {
        is_daemon: false,
        ..iced::Settings::default()
    })
    .run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(phase: Phase) -> Reconnect {
        Reconnect {
            core: Core::default(),
            request: Request {
                device: "build-host".into(),
                slot: crate::device::scoped_window_slot("build-host", "primary"),
                reason: "could not connect".into(),
            },
            phase,
            notice: None,
            retries: 0,
            last_retry: None,
        }
    }

    #[test]
    fn a_device_that_cannot_be_reached_offers_another_try() {
        let mut screen = screen(Phase::Checking);
        // Checking an unreachable device takes its SSH timeouts; this
        // computer's window stays a choice meanwhile.
        assert!(!screen.committed());
        let _ = screen.update(ReconnectMessage::Diagnosed(OpenFailure::Other(
            "Could not connect to build-host.".into(),
        )));
        assert!(matches!(&screen.phase, Phase::Failed(reason) if reason.contains("build-host")));
        assert!(!screen.busy());
        assert_eq!(screen.failed_title(), "Couldn't reconnect to build-host");
        assert_eq!(screen.tried_again(), None);
        let _ = screen.update(ReconnectMessage::Retry);
        assert!(matches!(screen.phase, Phase::Connecting));
        assert!(screen.committed());
        // A second press while connecting starts nothing more.
        let _ = screen.update(ReconnectMessage::Retry);
        assert!(matches!(screen.phase, Phase::Connecting));
        let _ = screen.update(ReconnectMessage::Connected(Err("refused".into())));
        assert!(matches!(screen.phase, Phase::Checking));
        // A device that refuses at once still shows the attempt was made.
        let _ = screen.update(ReconnectMessage::Diagnosed(OpenFailure::Other(
            "refused".into(),
        )));
        assert_eq!(
            screen.failed_title(),
            "Still couldn't reconnect to build-host"
        );
        assert!(
            screen
                .tried_again()
                .is_some_and(|tried| tried.starts_with("Tried again at "))
        );
        let _ = screen.update(ReconnectMessage::Retry);
        assert!(
            screen
                .tried_again()
                .is_some_and(|tried| tried.starts_with("Tried again 2 times"))
        );
    }

    #[test]
    fn an_older_device_offers_the_update_and_reconnects_after_it() {
        let mut screen = screen(Phase::Checking);
        let _ = screen.update(ReconnectMessage::Diagnosed(OpenFailure::Needs {
            need: Need::Update(Some(1)),
            blocked: None,
        }));
        let _ = screen.update(ReconnectMessage::StartUpdate);
        assert!(matches!(
            screen.phase,
            Phase::Needs {
                status: Some(RunStatus::Running(_)),
                ..
            }
        ));
        assert!(screen.busy());
        // Closing would cut the update off partway.
        assert!(screen.updating());
        let _ = screen.update(ReconnectMessage::Close);
        assert!(screen.updating());
        let _ = screen.update(ReconnectMessage::UpdateProgress(
            "Sending cagents 40%".into(),
        ));
        assert!(matches!(
            &screen.phase,
            Phase::Needs { status: Some(RunStatus::Running(line)), .. } if line == "Sending cagents 40%"
        ));
        let _ = screen.update(ReconnectMessage::UpdateFinished(Err(Failure {
            error: "SSH dropped".into(),
            report: None,
        })));
        assert!(matches!(
            screen.phase,
            Phase::Needs {
                status: Some(RunStatus::Failed { .. }),
                ..
            }
        ));
        let _ = screen.update(ReconnectMessage::StartUpdate);
        let _ = screen.update(ReconnectMessage::UpdateFinished(Ok(Success {
            unchanged: false,
            note: None,
        })));
        assert!(matches!(screen.phase, Phase::Connecting));
    }

    #[test]
    fn a_newer_device_or_one_this_computer_cannot_update_starts_no_update() {
        for (need, blocked) in [
            (Need::Newer(u16::MAX), None),
            (
                Need::Update(None),
                Some("not installed with install.sh".to_owned()),
            ),
        ] {
            let mut screen = screen(Phase::Needs {
                need,
                blocked,
                status: None,
            });
            let _ = screen.update(ReconnectMessage::StartUpdate);
            assert!(matches!(screen.phase, Phase::Needs { status: None, .. }));
        }
    }

    #[test]
    fn only_a_device_window_request_is_accepted() {
        let mut request = screen(Phase::Checking).request;
        assert!(request.validate().is_ok());
        request.slot = "primary".into();
        assert!(request.validate().is_err());
    }
}
