//! Bringing a device to this computer's cagents from the app: the strip that
//! says which side is older and offers **Update NAME**, which runs
//! `cagents device update NAME --yes` beside the app and reconnects when it
//! finishes. A device window shows it in place of its daemon health banner; a
//! window whose switch to a device failed on a mismatch shows it on its own.
//! See docs/devices.md#updating-a-device.

use super::*;
use cosmic::iced::futures::StreamExt;

#[derive(Default)]
pub(super) struct State {
    /// A device this window failed to open because of its cagents. A device
    /// window's own mismatch comes from its health instead.
    offer: Option<Offer>,
    run: Option<Run>,
}

/// What a device needs before this computer can use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// It has no cagents.
    Install,
    /// It has an older cagents; its daemon speaks this protocol, if it said.
    Update(Option<u16>),
    /// It has a newer cagents, which this computer never replaces.
    Newer(u16),
}

impl Need {
    /// The need a daemon of protocol `actual` implies.
    pub(super) fn of_protocol(actual: u16) -> Self {
        if actual < crate::terminal_ipc::PROTOCOL_VERSION {
            Self::Update(Some(actual))
        } else {
            Self::Newer(actual)
        }
    }

    /// Whether this computer's cagents fixes it.
    fn sendable(&self) -> bool {
        !matches!(self, Self::Newer(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Offer {
    device: String,
    need: Need,
    /// Why this computer cannot send its cagents there, when it cannot.
    blocked: Option<String>,
    /// Whether opening it would have replaced this window, as a switch does.
    replace: bool,
}

struct Run {
    device: String,
    need: Need,
    status: RunStatus,
}

#[derive(Debug)]
pub(super) enum RunStatus {
    /// The update's latest progress line.
    Running(String),
    Failed {
        error: String,
        /// What the device's own update said, such as the work it kept.
        report: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub enum Action {
    /// Opening `device` failed for want of `need`.
    Offer {
        device: String,
        need: Need,
        blocked: Option<String>,
        replace: bool,
    },
    Start(String),
    Progress(String),
    Finished(Result<Success, Failure>),
    Dismiss,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Success {
    /// Whether the device already had a compatible cagents.
    pub(super) unchanged: bool,
    /// Something the person should act on, such as an executable SSH
    /// commands cannot find.
    pub(super) note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub(super) error: String,
    pub(super) report: Option<String>,
}

/// What the strip is about: the device, what it needs, and whether a failed
/// switch raised it.
struct Subject<'a> {
    device: &'a str,
    need: Need,
    blocked: Option<&'a str>,
    offer: bool,
}

impl Cagents {
    fn device_update_subject(&self) -> Option<Subject<'_>> {
        let state = &self.device_update_ui;
        if let Some(offer) = &state.offer {
            return Some(Subject {
                device: &offer.device,
                need: offer.need.clone(),
                blocked: offer.blocked.as_deref(),
                offer: true,
            });
        }
        let device = crate::device::selected()?;
        // A device window's update outlives the health it started from: the
        // daemon is briefly unreachable while it is handed over.
        if let Some(run) = state
            .run
            .as_ref()
            .filter(|run| run.device == device.name && matches!(run.status, RunStatus::Running(_)))
        {
            return Some(Subject {
                device: &device.name,
                need: run.need.clone(),
                blocked: None,
                offer: false,
            });
        }
        match self.daemon_health_ui.health {
            crate::daemon_health::State::Incompatible { actual, .. } => Some(Subject {
                device: &device.name,
                need: Need::of_protocol(actual),
                blocked: None,
                offer: false,
            }),
            _ => None,
        }
    }

    fn device_update_running(&self) -> Option<&str> {
        self.device_update_ui
            .run
            .as_ref()
            .filter(|run| matches!(run.status, RunStatus::Running(_)))
            .map(|run| run.device.as_str())
    }

    /// Whether the strip stands in for the daemon health banner: it explains
    /// this window's own daemon, rather than a device it failed to open.
    pub(super) fn device_update_replaces_health(&self) -> bool {
        self.device_update_subject()
            .is_some_and(|subject| !subject.offer)
    }

    pub(super) fn update_device_update(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Offer {
                device,
                need,
                blocked,
                replace,
            } => {
                let running = self.device_update_running().map(str::to_owned);
                if let Some(running) = running.as_ref().filter(|running| **running != device) {
                    // One update at a time; the strip keeps following it.
                    self.status = Some(TransientNotice::warning(format!(
                        "Couldn't open {device}. Try again once {running} has finished updating."
                    )));
                    return Task::none();
                }
                let state = &mut self.device_update_ui;
                if running.is_none() {
                    state.run = None;
                }
                state.offer = Some(Offer {
                    device,
                    need,
                    blocked,
                    replace,
                });
                Task::none()
            }
            Action::Start(device) => {
                if self.device_update_running().is_some() {
                    return Task::none();
                }
                let Some(subject) = self
                    .device_update_subject()
                    .filter(|subject| subject.device == device)
                else {
                    return Task::none();
                };
                if !subject.need.sendable() || subject.blocked.is_some() {
                    return Task::none();
                }
                let need = subject.need;
                self.device_update_ui.run = Some(Run {
                    device: device.clone(),
                    need,
                    status: RunStatus::Running(format!("Checking {device}…")),
                });
                Task::stream(run_update(device).map(|event| {
                    cosmic::Action::App(Message::DeviceUpdate(match event {
                        Event::Line(line) => Action::Progress(line),
                        Event::Finished(result) => Action::Finished(result),
                    }))
                }))
            }
            Action::Progress(line) => {
                if let Some(Run {
                    status: RunStatus::Running(latest),
                    ..
                }) = &mut self.device_update_ui.run
                {
                    *latest = line;
                }
                Task::none()
            }
            Action::Finished(result) => {
                let state = &mut self.device_update_ui;
                let Some(run) = state
                    .run
                    .take_if(|run| matches!(run.status, RunStatus::Running(_)))
                else {
                    return Task::none();
                };
                match result {
                    Ok(Success { unchanged, note }) => {
                        let summary = if unchanged {
                            format!(
                                "{} already had a cagents this computer can use.",
                                run.device
                            )
                        } else {
                            format!("{} now has cagents {}.", run.device, crate::version())
                        };
                        self.status = Some(match note {
                            Some(note) => TransientNotice::warning(format!("{summary} {note}")),
                            None => TransientNotice::success(summary),
                        });
                        // A device window reconnects on its own once the
                        // daemon answers; a failed open is tried again.
                        let Some(offer) = state.offer.take_if(|offer| offer.device == run.device)
                        else {
                            return Task::none();
                        };
                        let Some(index) = self
                            .device_ui
                            .devices
                            .iter()
                            .position(|name| *name == offer.device)
                        else {
                            return Task::none();
                        };
                        self.update_device(if offer.replace {
                            device_ui::Action::Device(index)
                        } else {
                            device_ui::Action::OpenDeviceWindow(index)
                        })
                    }
                    Err(Failure { error, report }) => {
                        state.run = Some(Run {
                            status: RunStatus::Failed { error, report },
                            ..run
                        });
                        Task::none()
                    }
                }
            }
            Action::Dismiss => {
                if self.device_update_running().is_some() {
                    return Task::none();
                }
                let state = &mut self.device_update_ui;
                state.offer = None;
                state.run = None;
                Task::none()
            }
        }
    }

    /// The strip for a device whose cagents this computer cannot use, or
    /// nothing.
    pub(super) fn device_update_banner(&self) -> Option<Element<'_, Message>> {
        let subject = self.device_update_subject()?;
        let device = subject.device;
        let status = self
            .device_update_ui
            .run
            .as_ref()
            .filter(|run| run.device == device)
            .map(|run| &run.status);
        let enabled = !self.modal_is_open();
        let running = matches!(status, Some(RunStatus::Running(_)));
        Some(strip(
            device,
            &subject.need,
            subject.blocked,
            status,
            (enabled && self.device_update_running().is_none())
                .then(|| Message::DeviceUpdate(Action::Start(device.to_owned()))),
            (subject.offer && !running)
                .then_some(enabled.then_some(Message::DeviceUpdate(Action::Dismiss))),
        ))
    }
}

/// Says which side is older, or that `device` has no cagents, and offers to
/// send this computer's there: `start` begins it, and `dismiss`, when given,
/// adds a close button (enabled when it holds a message). `status` follows
/// the update once it has started. A window's strip and the screen a device
/// window shows when it cannot reopen both show this.
pub(super) fn strip<'a, M: Clone + 'static>(
    device: &str,
    need: &Need,
    blocked: Option<&str>,
    status: Option<&RunStatus>,
    start: Option<M>,
    dismiss: Option<Option<M>>,
) -> Element<'a, M> {
    let ours = crate::terminal_ipc::PROTOCOL_VERSION;
    let installing = *need == Need::Install;
    let sendable = need.sendable() && blocked.is_none();
    let running = matches!(status, Some(RunStatus::Running(_)));
    let (heading, detail): (String, Option<String>) = match (status, need) {
        (Some(RunStatus::Running(line)), _) => (
            if installing { format!("Installing cagents on {device}") } else { format!("Updating {device}") },
            Some(line.clone()),
        ),
        (Some(RunStatus::Failed { error, .. }), _) => (
            if installing { format!("Could not install cagents on {device}") } else { format!("Could not update {device}") },
            Some(error.clone()),
        ),
        (None, Need::Install) => (
            format!("{device} doesn't have cagents yet"),
            Some("Install this computer's version there over SSH to connect.".to_owned()),
        ),
        (None, Need::Update(actual)) => (
            match actual {
                Some(actual) => format!(
                    "{device} runs an older cagents (protocol {actual}) than this computer (protocol {ours})"
                ),
                None => format!("{device} runs an older cagents than this computer"),
            },
            Some(
                "Update it to this computer's version to connect. Its terminals keep running, and conversations there finish their turn first."
                    .to_owned(),
            ),
        ),
        (None, Need::Newer(actual)) => (
            format!(
                "{device} runs a newer cagents (protocol {actual}) than this computer (protocol {ours})"
            ),
            Some("Update cagents on this computer to connect to it.".to_owned()),
        ),
    };
    // What stops this computer sending its cagents replaces the advice to.
    let detail = match (status, blocked) {
        (None, Some(blocked)) if need.sendable() => Some(blocked.to_owned()),
        _ => detail,
    };
    let mut text = vec![design::heading(heading).into()];
    if let Some(detail) = detail {
        text.push(design::caption(detail).into());
    }
    if let Some(RunStatus::Failed {
        report: Some(report),
        ..
    }) = status
    {
        text.push(design::caption(report.clone()).into());
    }
    let mut row = widget::row::with_children(vec![
        widget::column::with_children(text)
            .spacing(space::XXS)
            .width(Length::Fill)
            .into(),
    ])
    .spacing(space::S)
    .align_y(Alignment::Center);
    if sendable {
        let offered = if installing {
            format!("Install on {device}")
        } else {
            format!("Update {device}")
        };
        let busy = if installing {
            "Installing…"
        } else {
            "Updating…"
        };
        let failed = matches!(status, Some(RunStatus::Failed { .. }));
        row = row.push(
            crate::ui_controls::button::primary(offered)
                .instead("Try again", failed)
                .instead(busy, running)
                .name(if installing {
                    format!("Install this computer's cagents on {device}")
                } else {
                    format!("Update {device} to this computer's cagents")
                })
                .on_press_maybe(start.filter(|_| !running)),
        );
    }
    // Nothing to dismiss while the update runs: it cannot be stopped
    // from here.
    if let Some(dismiss) = dismiss {
        row = row.push(crate::interaction::tooltip(
            widget::button::icon(Icon::Close)
                .id(widget::Id::new("device-update.dismiss"))
                .name(format!("Dismiss the offer for {device}"))
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(dismiss),
            "Dismiss",
        ));
    }
    widget::container(row)
        .padding(space::M)
        .width(Length::Fill)
        .class(cosmic::theme::Container::Dialog(false))
        .into()
}

pub(super) enum Event {
    Line(String),
    Finished(Result<Success, Failure>),
}

/// The lines of the device's own update worth showing when it fails: what it
/// kept and why it stopped, without the progress before it.
const REPORT_LINES: usize = 12;

/// Runs the CLI's update, streaming its progress lines, then its result.
pub(super) fn run_update(device: String) -> impl iced::futures::Stream<Item = Event> {
    let (sender, receiver) = async_channel::bounded(8);
    let spawned = std::thread::Builder::new()
        .name("device-update".into())
        .spawn(move || {
            let result = update_device(&device, |line| {
                let _ = sender.send_blocking(Event::Line(line));
            });
            let _ = sender.send_blocking(Event::Finished(result));
        });
    let failed = spawned.err().map(|error| {
        Event::Finished(Err(Failure {
            error: format!("Could not start the update: {error}"),
            report: None,
        }))
    });
    receiver.chain(cosmic::iced::futures::stream::iter(failed))
}

fn update_device(device: &str, mut progress: impl FnMut(String)) -> Result<Success, Failure> {
    use std::io::{BufRead, Read};
    let failure = |error: String| Failure {
        error,
        report: None,
    };
    let executable = crate::companion::Product::Cli
        .beside_current()
        .map_err(|error| failure(error.to_string()))?;
    let mut child = std::process::Command::new(executable)
        .args(["--json", "device", "update", device, "--yes"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| failure(format!("Could not start the update: {error}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    // The result is one JSON document; read it beside the progress so a full
    // pipe can never stall the update.
    let result = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    let stderr = std::io::BufReader::new(child.stderr.take().expect("piped stderr"));
    let (report, failed) = read_progress(stderr.lines().map_while(Result::ok), &mut progress);
    let status = child
        .wait()
        .map_err(|error| failure(format!("The update did not finish: {error}")))?;
    let output = result.join().ok().and_then(Result::ok).unwrap_or_default();
    // A failure's JSON comes on stderr, after the progress.
    let output = match failed {
        Some(failed) if !status.success() => failed,
        _ => output,
    };
    outcome(status.success(), &output, report)
}

/// Splits what the update wrote to stderr into its latest lines, each also
/// passed to `progress`, and the JSON a failure ends with.
fn read_progress(
    lines: impl Iterator<Item = String>,
    mut progress: impl FnMut(String),
) -> (Vec<String>, Option<String>) {
    let mut report = std::collections::VecDeque::with_capacity(REPORT_LINES);
    let mut failed = None;
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('{') {
            failed = Some(line.to_owned());
            continue;
        }
        if report.len() == REPORT_LINES {
            report.pop_front();
        }
        report.push_back(line.to_owned());
        progress(line.to_owned());
    }
    (report.into(), failed)
}

/// Reads the CLI's JSON result. On failure, its message is the error and the
/// device's own lines before it are the report, less the CLI's restatement
/// and its progress.
fn outcome(success: bool, output: &str, report: Vec<String>) -> Result<Success, Failure> {
    let value: Option<serde_json::Value> = serde_json::from_str(output).ok();
    if success && let Some(value) = value.filter(|value| value["ok"].as_bool() == Some(true)) {
        let data = &value["data"];
        return Ok(Success {
            unchanged: data["state"] == "compatible",
            note: note(data),
        });
    }
    let error = error_message(output)
        .unwrap_or_else(|| "The update stopped without saying why.".to_owned());
    let report: Vec<_> = report
        .into_iter()
        .filter(|line| !line.ends_with(error.as_str()) && !line.ends_with('…'))
        .filter(|line| !line.starts_with("Sending cagents "))
        .collect();
    Err(Failure {
        error,
        report: (!report.is_empty()).then(|| report.join("\n")),
    })
}

fn note(data: &serde_json::Value) -> Option<String> {
    data["note"]
        .as_str()
        .filter(|note| !note.is_empty())
        .map(str::to_owned)
}

/// What `device update --check` printed, when it inspected the device: what
/// the device needs, if anything, and what the person should act on.
pub(super) struct Checked {
    pub need: Option<Need>,
    pub note: Option<String>,
}

/// Reads what `device update --check` printed, or `None` when it could not
/// inspect the device.
pub(super) fn checked(output: &str) -> Option<Checked> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    if value["ok"].as_bool() != Some(true) {
        return None;
    }
    let data = &value["data"];
    let protocol = data["protocol"]
        .as_u64()
        .and_then(|protocol| u16::try_from(protocol).ok());
    let need = match data["state"].as_str()? {
        "install" => Some(Need::Install),
        "update" => Some(Need::Update(protocol)),
        "newer" => Some(Need::Newer(protocol?)),
        _ => None,
    };
    Some(Checked {
        need,
        note: note(data),
    })
}

/// The error message in `output`, the JSON a failed command printed.
pub(super) fn error_message(output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    value["error"]["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}

/// Whether `output`, the JSON a device command printed, is a protocol
/// mismatch, and the device's protocol if so.
pub(super) fn mismatch(output: &str) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    (value["error"]["kind"] == "device_incompatible")
        .then(|| value["error"]["details"]["protocol"].as_u64())
        .flatten()
        .and_then(|protocol| u16::try_from(protocol).ok())
}

#[cfg(test)]
mod tests;
