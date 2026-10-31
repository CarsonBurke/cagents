use super::{Follow, Snapshot};
use crate::{
    design::{self, space},
    instance,
};
use cosmic::{Core, app::cosmic::Cosmic, iced, widget};
use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::Duration,
};

/// Only public window/device arguments, never an update decision capability.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    new_window: bool,
    slot: Option<String>,
    source: Option<String>,
    device: Option<String>,
    scoped: bool,
    /// In-process custody is never serialized into the public retry context.
    #[serde(skip)]
    recovery: std::sync::Arc<std::sync::Mutex<RecoveryState>>,
}

#[derive(Clone, Debug)]
struct RecoveryTarget {
    slot: String,
    source: Option<String>,
}

#[derive(Clone)]
struct RetainedRecovery {
    monitor: crate::control::UiLaunchMonitor,
    trusted: std::sync::Arc<instance::startup_check::TrustedUiExecutable>,
}

#[derive(Default)]
struct RecoveryState {
    target: Option<RecoveryTarget>,
    retained: Option<RetainedRecovery>,
    opening: bool,
    uncertain: bool,
}

impl std::fmt::Debug for RecoveryState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryState")
            .field("target", &self.target)
            .field("retained", &self.retained.is_some())
            .field("opening", &self.opening)
            .finish()
    }
}

struct RecoveryOpen(std::sync::Arc<std::sync::Mutex<RecoveryState>>);
impl Drop for RecoveryOpen {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.lock() {
            state.opening = false;
        }
    }
}

const STARTUP_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_PENDING: &str =
    "The requested window is still recovering. Keep its recovery controls open and check again.";
const STARTUP_INCOMPLETE: &str = "The requested window has not completed recovery. Resolve its recovery notices, then retry this check.";
const STARTUP_UNCONFIRMED: &str = "The original requested window could not be verified. It was not replaced; keep its recovery controls open and try again.";
impl Request {
    pub fn new(
        new_window: bool,
        slot: Option<String>,
        source: Option<String>,
        device: Option<String>,
        scoped: bool,
    ) -> Result<Self, &'static str> {
        let request = Self {
            new_window,
            slot,
            source,
            device,
            scoped,
            recovery: Default::default(),
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.new_window && self.slot.is_some() {
            return Err("invalid window request");
        }
        for slot in [&self.slot, &self.source].into_iter().flatten() {
            instance::validate_window_slot(slot).map_err(|_| "invalid window request")?;
        }
        if self.device.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 128 || name.chars().any(char::is_control)
        }) {
            return Err("invalid device request");
        }
        Ok(())
    }
    pub fn prepare_context(&self) -> Result<(), &'static str> {
        self.validate()?;
        crate::device::select(self.device.as_deref())
            .map_err(|_| "The original device profile is no longer available.")
    }
    /// Once this launch has acquired a concrete slot, its detached failure
    /// screen must retry that slot rather than allocate another new window.
    pub fn bind_acquired_window(
        &mut self,
        slot: &str,
        source: Option<&str>,
    ) -> Result<(), &'static str> {
        instance::validate_window_slot(slot).map_err(|_| "invalid acquired window")?;
        if let Some(source) = source {
            instance::validate_window_slot(source).map_err(|_| "invalid acquired window source")?;
        }
        self.new_window = false;
        self.slot = Some(slot.to_owned());
        self.source = source.map(str::to_owned);
        // Both values were normalized by the actual acquiring entry point.
        self.scoped = true;
        self.recovery = Default::default();
        Ok(())
    }

    fn recovery_target(&self) -> Result<RecoveryTarget, &'static str> {
        let slot = if self.new_window {
            // Allocate once in the parent, before any child can become a
            // retained recovery window. The public --new-window contract is
            // unchanged; only this retry's internal command is made explicit.
            let name = format!("w-{}", uuid::Uuid::new_v4());
            crate::device::window_slot(Some(&name))
                .map_err(|_| "The requested window could not be identified.")?
                .unwrap_or(name)
        } else {
            self.normalized_slot(self.slot.as_deref())?
                .unwrap_or_else(|| instance::PRIMARY_WINDOW.into())
        };
        let source = self
            .source
            .as_deref()
            .map(|source| self.normalized_slot(Some(source)))
            .transpose()?
            .flatten();
        Ok(RecoveryTarget { slot, source })
    }

    fn for_recovery_target(&self, target: &RecoveryTarget) -> Self {
        Self {
            new_window: false,
            slot: Some(target.slot.clone()),
            source: target.source.clone(),
            device: self.device.clone(),
            scoped: true,
            recovery: Default::default(),
        }
    }

    fn bound_recovery_target(&self) -> Result<RecoveryTarget, &'static str> {
        let mut state = self.recovery.lock().map_err(|_| STARTUP_UNCONFIRMED)?;
        if state.target.is_none() {
            state.target = Some(self.recovery_target()?);
        }
        Ok(state.target.clone().unwrap())
    }
    /// Retry the same public request through ordinary startup and readiness.
    /// Callers keep their recovery screen until this returns successfully.
    pub fn open(&self) -> Result<(), &'static str> {
        self.validate()?;
        let requested = self.device.as_deref().filter(|name| *name != "local");
        let selected = crate::device::selected().map(|device| device.name.as_str());
        if requested != selected {
            return Err("The original device profile is no longer available.");
        }
        reopen(self)
    }
    fn arguments(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(device) = &self.device {
            args.extend(["--device".into(), device.clone()]);
        }
        if self.scoped {
            args.push("--device-window-scoped".into());
        }
        if self.new_window {
            args.push("--new-window".into());
        }
        if let Some(slot) = &self.slot {
            args.extend(["--window".into(), slot.clone()]);
        }
        if let Some(source) = &self.source {
            args.extend(["--from-window".into(), source.clone()]);
        }
        args
    }
    fn normalized_slot(&self, slot: Option<&str>) -> Result<Option<String>, &'static str> {
        if self.scoped && slot.is_some() {
            Ok(slot.map(str::to_owned))
        } else {
            crate::device::window_slot(slot)
                .map_err(|_| "The requested window could not be identified.")
        }
    }
    fn focus(&self) -> Result<bool, &'static str> {
        self.focus_exact_with(instance::activate_window)
    }
    fn focus_exact_with(
        &self,
        activate: impl FnOnce(&str) -> std::io::Result<bool>,
    ) -> Result<bool, &'static str> {
        let slot = self.normalized_slot(self.slot.as_deref())?;
        activate(slot.as_deref().unwrap_or(instance::PRIMARY_WINDOW))
            .map_err(|_| "The existing window did not answer. Progress will keep updating here.")
    }
    fn focus_existing(&self) -> Result<bool, &'static str> {
        let mut preferred = Vec::new();
        for slot in self
            .slot
            .iter()
            .chain(self.source.iter())
            .map(|slot| Some(slot.as_str()))
            .chain([None])
        {
            let slot = self
                .normalized_slot(slot)?
                .unwrap_or_else(|| instance::PRIMARY_WINDOW.into());
            if !preferred.contains(&slot) {
                preferred.push(slot);
            }
        }
        instance::activate_existing_device_window(&preferred)
            .map_err(|_| "The existing window did not answer. Progress will keep updating here.")
    }
}

#[derive(Clone, Debug)]
enum Message {
    Poll,
    Observed(Snapshot),
    Open,
    Opened(Result<(), &'static str>),
    Focus,
    Focused(Result<bool, &'static str>),
    Details,
    Diagnostics,
    Diagnosed(String),
    Close,
}
struct Window {
    core: Core,
    request: Request,
    snapshot: Snapshot,
    follow: Follow,
    polling: bool,
    working: bool,
    opening: bool,
    details: bool,
    notice: Option<String>,
    context_ready: bool,
}
impl Window {
    fn poll(&mut self) -> cosmic::app::Task<Message> {
        if self.polling {
            return cosmic::app::Task::none();
        }
        self.polling = true;
        cosmic::task::future(async {
            let snapshot = tokio::task::spawn_blocking(super::observe)
                .await
                .unwrap_or_else(|_| Snapshot::unavailable());
            Message::Observed(snapshot)
        })
    }
    fn open(&mut self) -> cosmic::app::Task<Message> {
        if self.working || !self.context_ready || !self.snapshot.can_open() {
            return cosmic::app::Task::none();
        }
        self.working = true;
        self.opening = true;
        self.follow.mark_opened();
        let request = self.request.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || request.open())
                .await
                .unwrap_or(Err("cagents could not be opened. You can try again."));
            Message::Opened(result)
        })
    }
}
impl cosmic::Application for Window {
    type Executor = cosmic::executor::Default;
    type Flags = (Request, bool);
    type Message = Message;
    const APP_ID: &'static str = "dev.cagents.Cagents.UpdateStartup";
    fn init(
        core: Core,
        (request, context_ready): (Request, bool),
    ) -> (Self, cosmic::app::Task<Message>) {
        let mut window = Self { core, request, snapshot: Snapshot::unavailable(), follow: Follow::default(), polling: false, working: false, opening: false, details: false, notice: (!context_ready).then(|| "The original device profile is no longer available. Close this screen and choose the device again in cagents.".into()), context_ready };
        let task = window.poll();
        (window, task)
    }
    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }
    fn on_close_requested(&self, _: iced::window::Id) -> Option<Message> {
        Some(Message::Close)
    }
    fn subscription(&self) -> iced::Subscription<Message> {
        iced::time::every(Duration::from_secs(1)).map(|_| Message::Poll)
    }
    fn update(&mut self, message: Message) -> cosmic::app::Task<Message> {
        match message {
            Message::Poll => self.poll(),
            Message::Observed(snapshot) => {
                self.polling = false;
                self.snapshot = snapshot;
                if self
                    .follow
                    .observe_when_ready(&self.snapshot, !self.working)
                {
                    self.open()
                } else {
                    cosmic::app::Task::none()
                }
            }
            Message::Open => self.open(),
            Message::Opened(Ok(())) => iced::exit().map(cosmic::Action::App),
            Message::Opened(Err(error)) => {
                self.working = false;
                self.opening = false;
                self.notice = Some(error.into());
                cosmic::app::Task::none()
            }
            Message::Focus if !self.working && self.context_ready => {
                self.working = true;
                let request = self.request.clone();
                cosmic::task::future(async move {
                    Message::Focused(
                        tokio::task::spawn_blocking(move || request.focus_existing())
                            .await
                            .unwrap_or(Err("The existing window could not be reached.")),
                    )
                })
            }
            Message::Focused(result) => {
                self.working = false;
                self.notice = Some(match result { Ok(true) => "The existing window was activated. This window will keep following the update.", Ok(false) => "No existing window is ready to focus yet. This window will keep checking.", Err(error) => error }.into());
                cosmic::app::Task::none()
            }
            Message::Details => {
                self.details = !self.details;
                cosmic::app::Task::none()
            }
            Message::Diagnostics if !self.working => {
                self.working = true;
                cosmic::task::future(async {
                    let summary = tokio::task::spawn_blocking(|| match crate::diagnostics::inspect() {
                        Ok(report) => diagnostic_summary(&report),
                        Err(_) => "Private startup diagnostics could not be read. No update state was changed.".into(),
                    }).await.unwrap_or_else(|_| "The diagnostics check could not finish.".into());
                    Message::Diagnosed(summary)
                })
            }
            Message::Diagnosed(summary) => {
                self.working = false;
                self.notice = Some(summary.into());
                cosmic::app::Task::none()
            }
            Message::Close => iced::exit().map(cosmic::Action::App),
            Message::Focus | Message::Diagnostics => cosmic::app::Task::none(),
        }
    }
    fn view(&self) -> cosmic::Element<'_, Message> {
        let wrap = iced::widget::text::Wrapping::WordOrGlyph;
        let mut body = vec![
            design::title(if self.opening {
                "Opening your workspace"
            } else {
                self.snapshot.heading()
            })
            .wrapping(wrap)
            .into(),
        ];
        if let Some(version) = self.snapshot.target() {
            body.push(
                design::heading(format!("Target version {version}"))
                    .wrapping(wrap)
                    .into(),
            );
        }
        body.push(
            design::body(if self.opening {
                "Waiting for the requested workspace to finish its startup check. Saved sessions use the normal recovery flow. This screen stays open until the workspace acknowledges startup.".into()
            } else {
                self.snapshot.explanation()
            })
                .wrapping(wrap)
                .into(),
        );
        body.push(design::caption("This window follows the update without changing it. Close only dismisses this screen.").wrapping(wrap).into());
        if let Some(notice) = &self.notice {
            body.push(design::body(notice).wrapping(wrap).into());
        }

        if self.details {
            body.push(
                design::body(self.snapshot.technical_summary())
                    .wrapping(wrap)
                    .into(),
            );
            let mut diagnostics = crate::ui_controls::button::secondary("Check diagnostics");
            if !self.working {
                diagnostics = diagnostics.on_press(Message::Diagnostics);
            }
            body.push(diagnostics.into());
        }
        let mut open = crate::ui_controls::button::primary(if self.working {
            "Please wait…"
        } else {
            "Open cagents"
        });
        if !self.working && self.context_ready && self.snapshot.can_open() {
            open = open.on_press(Message::Open);
        }
        let mut focus = crate::ui_controls::button::secondary("Existing window");
        if !self.working && self.context_ready {
            focus = focus.on_press(Message::Focus);
        }
        let footer = widget::row::with_children(vec![
            open.into(),
            focus.into(),
            crate::ui_controls::button::secondary("Close")
                .on_press(Message::Close)
                .into(),
        ])
        .spacing(space::S);
        widget::container(
            widget::column::with_children(vec![
                widget::row::with_children(vec![
                    design::caption("CAGENTS · UPDATE").into(),
                    crate::ui_controls::button::secondary(if self.details {
                        "Hide technical details"
                    } else {
                        "Technical details"
                    })
                    .on_press(Message::Details)
                    .into(),
                ])
                .spacing(space::S)
                .align_y(iced::Alignment::Center)
                .into(),
                widget::scrollable(
                    widget::column::with_children(body)
                        .spacing(space::L)
                        .width(iced::Length::Fill),
                )
                .height(iced::Length::Fill)
                .into(),
                footer.into(),
            ])
            .spacing(space::L)
            .height(iced::Length::Fill),
        )
        .padding(space::XL)
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
        .into()
    }
}

fn diagnostic_summary(report: &crate::diagnostics::Report) -> String {
    use crate::diagnostics::Lifecycle;
    let mut lines =
        vec!["Retained startup evidence (historical, not a live health check):".to_owned()];
    for product in &report.products {
        let state = if !product.readable {
            "unavailable"
        } else {
            match product
                .last_observed
                .as_ref()
                .map(|record| record.lifecycle)
            {
                None | Some(Lifecycle::Unknown) => "unknown",
                Some(Lifecycle::Started) => "startup recorded",
                Some(Lifecycle::Ready) => "ready recorded",
                Some(Lifecycle::StartupFailed) => "startup failure recorded",
                Some(Lifecycle::Panicked) => "crash recorded",
                Some(Lifecycle::Exited) => "exit recorded",
            }
        };
        lines.push(format!("{}: {state}.", product.product.name()));
    }
    lines.join("\n")
}

fn reopen(request: &Request) -> Result<(), &'static str> {
    // Fresh status is still not admission: normal startup reacquires every
    // original fence. Never start an updater or installer from here; the
    // ordinary UI entry point owns daemon readiness.
    if let Some(reason) = super::observe().opening_refusal() {
        return Err(reason);
    }
    let target = request.bound_recovery_target()?;
    let retained = {
        let mut state = request.recovery.lock().map_err(|_| STARTUP_UNCONFIRMED)?;
        if state.opening || state.uncertain {
            return Err(STARTUP_UNCONFIRMED);
        }
        if state.retained.as_ref().is_some_and(|retained| {
            retained.monitor.status() == crate::control::UiLaunchStatus::Exited
        }) {
            state.retained = None;
        }
        state.opening = true;
        state.retained.clone()
    };
    let _opening = RecoveryOpen(request.recovery.clone());
    if let Some(retained) = retained {
        let disposition = retained
            .monitor
            .with_live_child(|child| {
                if retained.monitor.status() == crate::control::UiLaunchStatus::Ready {
                    return Ok(instance::startup_check::StartupDisposition::Ready);
                }
                instance::startup_check::ExactStartupTarget::connect(
                    &target.slot,
                    request.device.as_deref(),
                    std::slice::from_ref(retained.trusted.as_ref()),
                    STARTUP_CHECK_TIMEOUT,
                )?
                .require_original_child(child)?
                .check(STARTUP_CHECK_TIMEOUT)
            })
            .map_err(|_| STARTUP_UNCONFIRMED)?;
        return finish_recovery_check(disposition);
    }
    let executable = crate::update::superseding_ui()
        .or_else(|| std::env::current_exe().ok())
        .ok_or("The cagents executable could not be found.")?;
    let trusted = std::sync::Arc::new(
        instance::startup_check::TrustedUiExecutable::open(&executable)
            .map_err(|_| STARTUP_UNCONFIRMED)?,
    );
    let selected = request.for_recovery_target(&target);
    if !request.new_window && selected.focus()? {
        let disposition = instance::startup_check::ExactStartupTarget::connect(
            &target.slot,
            request.device.as_deref(),
            std::slice::from_ref(trusted.as_ref()),
            STARTUP_CHECK_TIMEOUT,
        )
        .and_then(|target| target.check(STARTUP_CHECK_TIMEOUT))
        .map_err(|_| STARTUP_UNCONFIRMED)?;
        return finish_recovery_check(disposition);
    }
    // If this task panics after starting a child, no subsequent Retry may
    // assume that the child never existed and launch another one.
    request
        .recovery
        .lock()
        .map_err(|_| STARTUP_UNCONFIRMED)?
        .uncertain = true;
    let launched =
        crate::control::spawn_ui_process_monitored(&mut reopen_command(&executable, &selected));
    let mut state = request.recovery.lock().map_err(|_| STARTUP_UNCONFIRMED)?;
    match launched {
        Ok(crate::control::UiLaunchOutcome::Ready) => {
            state.uncertain = false;
            Ok(())
        }
        Ok(crate::control::UiLaunchOutcome::Retained(monitor)) => {
            let pending = monitor.status() == crate::control::UiLaunchStatus::Pending;
            state.retained = Some(RetainedRecovery { monitor, trusted });
            state.uncertain = false;
            Err(if pending {
                STARTUP_PENDING
            } else {
                STARTUP_INCOMPLETE
            })
        }
        Err(error) => {
            state.uncertain =
                error.to_string() == instance::ApplicationLeaseError::AlreadyRunning.to_string();
            Err(
                "cagents did not confirm startup. Check diagnostics or try again; your requested window is still selected.",
            )
        }
    }
}

fn finish_recovery_check(
    disposition: instance::startup_check::StartupDisposition,
) -> Result<(), &'static str> {
    match disposition {
        instance::startup_check::StartupDisposition::Ready => Ok(()),
        instance::startup_check::StartupDisposition::Pending => Err(STARTUP_PENDING),
        instance::startup_check::StartupDisposition::RecoveryIncomplete => Err(STARTUP_INCOMPLETE),
    }
}
fn wait_for_reopen(command: &mut Command) -> Result<(), &'static str> {
    crate::control::spawn_ui_process(command)
        .map_err(|_| "cagents did not confirm startup. Check diagnostics or try again; your requested window is still selected.")
}
fn reopen_command(executable: &std::path::Path, request: &Request) -> Command {
    let mut command = Command::new(executable);
    command
        .args(request.arguments())
        .arg(super::RECOVERY_ENTRY_ARGUMENT)
        .env_remove(instance::UI_LAUNCH_HANDSHAKE_ENV)
        .env_remove(instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV)
        .env(super::FAILURE_SCREEN_OWNED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
}

pub fn run(request: Request) -> Result<(), Box<dyn std::error::Error>> {
    request.validate()?;
    let context_ready = request.prepare_context().is_ok();
    type Shell = Cosmic<Window>;
    use cosmic::Application;
    cosmic::icon_theme::set_default(cosmic::config::icon_theme());
    let request = std::cell::RefCell::new(Some((request, context_ready)));
    iced::daemon(
        move || {
            let (window, open) = iced::window::open(iced::window::Settings {
                size: iced::Size::new(640.0, 440.0),
                min_size: Some(iced::Size::new(420.0, 340.0)),
                exit_on_close_request: false,
                icon: Some(crate::app::application_window_icon()),
                ..iced::window::Settings::default()
            });
            let mut core = Core::default();
            core.set_main_window_id(Some(window));
            let (shell, task) = Shell::init((
                core,
                request
                    .borrow_mut()
                    .take()
                    .expect("startup screen initialized once"),
            ));
            (shell, iced::Task::batch([open.discard(), task]))
        },
        Shell::update,
        Shell::view,
    )
    .subscription(Shell::subscription)
    .title(|_: &Shell, _| "cagents update".to_owned())
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
mod tests;
