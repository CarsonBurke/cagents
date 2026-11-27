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
    /// A device this window failed to open because it speaks another
    /// protocol. A device window's own mismatch comes from its health instead.
    offer: Option<Offer>,
    run: Option<Run>,
    /// Fences progress and results from an update that was abandoned.
    generation: u64,
    task: Option<iced::task::Handle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Offer {
    device: String,
    /// The device's protocol, or `None` when it has no cagents.
    actual: Option<u16>,
    /// Whether opening it would have replaced this window, as a switch does.
    replace: bool,
}

struct Run {
    device: String,
    status: RunStatus,
}

enum RunStatus {
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
    /// Opening `device` failed because it has cagents of protocol
    /// `actual`, or none.
    Offer {
        device: String,
        actual: Option<u16>,
        replace: bool,
    },
    Start(String),
    Progress {
        generation: u64,
        line: String,
    },
    Finished {
        generation: u64,
        result: Result<(), Failure>,
    },
    Dismiss,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    error: String,
    report: Option<String>,
}

/// What the strip is about: the device, its protocol, and whether a failed
/// switch raised it.
struct Subject<'a> {
    device: &'a str,
    actual: Option<u16>,
    offer: bool,
}

impl Cagents {
    fn device_update_subject(&self) -> Option<Subject<'_>> {
        if let Some(offer) = &self.device_update_ui.offer {
            return Some(Subject {
                device: &offer.device,
                actual: offer.actual,
                offer: true,
            });
        }
        let device = crate::device::selected()?;
        match self.daemon_health_ui.health {
            crate::daemon_health::State::Incompatible { actual, .. } => Some(Subject {
                device: &device.name,
                actual: Some(actual),
                offer: false,
            }),
            _ => None,
        }
    }

    pub(super) fn update_device_update(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Offer {
                device,
                actual,
                replace,
            } => {
                let running = self.device_update_ui.run.as_ref().is_some_and(|run| {
                    run.device == device && matches!(run.status, RunStatus::Running(_))
                });
                if !running {
                    self.device_update_ui.run = None;
                }
                self.device_update_ui.offer = Some(Offer {
                    device,
                    actual,
                    replace,
                });
                Task::none()
            }
            Action::Start(device) => {
                if self
                    .device_update_ui
                    .run
                    .as_ref()
                    .is_some_and(|run| matches!(run.status, RunStatus::Running(_)))
                {
                    return Task::none();
                }
                let state = &mut self.device_update_ui;
                state.generation = state.generation.wrapping_add(1);
                let generation = state.generation;
                state.run = Some(Run {
                    device: device.clone(),
                    status: RunStatus::Running(format!("Checking {device}…")),
                });
                let (task, handle) = Task::stream(run_update(device).map(move |event| {
                    cosmic::Action::App(Message::DeviceUpdate(match event {
                        Event::Line(line) => Action::Progress { generation, line },
                        Event::Finished(result) => Action::Finished { generation, result },
                    }))
                }))
                .abortable();
                state.task = Some(handle);
                task
            }
            Action::Progress { generation, line }
                if generation == self.device_update_ui.generation =>
            {
                if let Some(Run {
                    status: RunStatus::Running(latest),
                    ..
                }) = &mut self.device_update_ui.run
                {
                    *latest = line;
                }
                Task::none()
            }
            Action::Finished { generation, result }
                if generation == self.device_update_ui.generation =>
            {
                let state = &mut self.device_update_ui;
                state.task = None;
                let Some(run) = state.run.take() else {
                    return Task::none();
                };
                match result {
                    Ok(()) => {
                        self.status = Some(TransientNotice::success(format!(
                            "{} now has cagents {}.",
                            run.device,
                            crate::version()
                        )));
                        // A device window reconnects on its own once the
                        // daemon answers; a failed switch is tried again.
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
                            device: run.device,
                            status: RunStatus::Failed { error, report },
                        });
                        Task::none()
                    }
                }
            }
            Action::Dismiss => {
                let state = &mut self.device_update_ui;
                if state
                    .run
                    .as_ref()
                    .is_some_and(|run| matches!(run.status, RunStatus::Running(_)))
                {
                    return Task::none();
                }
                state.offer = None;
                state.run = None;
                Task::none()
            }
            Action::Progress { .. } | Action::Finished { .. } => Task::none(),
        }
    }

    /// The strip for a device that speaks another protocol, or nothing.
    pub(super) fn device_update_banner(&self) -> Option<Element<'_, Message>> {
        let subject = self.device_update_subject()?;
        let device = subject.device;
        let ours = crate::terminal_ipc::PROTOCOL_VERSION;
        let run = self
            .device_update_ui
            .run
            .as_ref()
            .filter(|run| run.device == device);
        // Something this computer can send: an older build or none.
        let sendable = subject.actual.is_none_or(|actual| actual < ours);
        let installing = subject.actual.is_none();
        let enabled = !self.modal_is_open();
        let (heading, detail): (String, Option<String>) = match (run.map(|run| &run.status), subject.actual) {
            (Some(RunStatus::Running(line)), _) => (
                if installing { format!("Installing cagents on {device}…") } else { format!("Updating {device}…") },
                Some(line.clone()),
            ),
            (Some(RunStatus::Failed { error, .. }), _) => (
                if installing { format!("Could not install cagents on {device}") } else { format!("Could not update {device}") },
                Some(error.clone()),
            ),
            (None, None) => (
                format!("{device} doesn't have cagents yet"),
                Some("Install this computer's version there over SSH to connect.".to_owned()),
            ),
            (None, Some(actual)) if sendable => (
                format!(
                    "{device} runs an older cagents (protocol {actual}) than this computer (protocol {ours})"
                ),
                Some(
                    "Update it to this computer's version to connect. Its terminals keep running, and conversations there finish their turn first."
                        .to_owned(),
                ),
            ),
            (None, Some(actual)) => (
                format!(
                    "{device} runs a newer cagents (protocol {actual}) than this computer (protocol {ours})"
                ),
                Some("Update cagents on this computer to connect to it.".to_owned()),
            ),
        };
        let mut text = vec![design::heading(heading).into()];
        if let Some(detail) = detail {
            text.push(design::caption(detail).into());
        }
        if let Some(Run {
            status:
                RunStatus::Failed {
                    report: Some(report),
                    ..
                },
            ..
        }) = run
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
        let running = matches!(run.map(|run| &run.status), Some(RunStatus::Running(_)));
        let busy = if installing {
            "Installing…"
        } else {
            "Updating…"
        };
        if sendable {
            let offered = if installing {
                format!("Install on {device}")
            } else {
                format!("Update {device}")
            };
            let failed = matches!(run.map(|run| &run.status), Some(RunStatus::Failed { .. }));
            row = row.push(
                crate::ui_controls::button::primary(offered)
                    .instead("Try again", failed)
                    .instead(busy, running)
                    .name(if installing {
                        format!("Install this computer's cagents on {device}")
                    } else {
                        format!("Update {device} to this computer's cagents")
                    })
                    .on_press_maybe(
                        (enabled && !running)
                            .then(|| Message::DeviceUpdate(Action::Start(device.to_owned()))),
                    ),
            );
        }
        if subject.offer {
            row = row.push(crate::interaction::tooltip(
                widget::button::icon(Icon::Close)
                    .id(widget::Id::new("device-update.dismiss"))
                    .name(format!("Dismiss the offer for {device}"))
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .on_press_maybe(
                        (enabled && !running).then_some(Message::DeviceUpdate(Action::Dismiss)),
                    ),
                "Dismiss",
            ));
        }
        Some(
            widget::container(row)
                .padding(space::M)
                .width(Length::Fill)
                .class(cosmic::theme::Container::Dialog(false))
                .into(),
        )
    }
}

enum Event {
    Line(String),
    Finished(Result<(), Failure>),
}

/// The lines of the device's own update worth showing when it fails: what it
/// kept and why it stopped, without the progress before it.
const REPORT_LINES: usize = 12;

/// Runs the CLI's update, streaming its progress lines, then its result.
fn run_update(device: String) -> impl iced::futures::Stream<Item = Event> {
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

fn update_device(device: &str, mut progress: impl FnMut(String)) -> Result<(), Failure> {
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
    let mut report = std::collections::VecDeque::with_capacity(REPORT_LINES);
    for line in std::io::BufReader::new(child.stderr.take().expect("piped stderr")).lines() {
        let Ok(line) = line else { break };
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            continue;
        }
        if report.len() == REPORT_LINES {
            report.pop_front();
        }
        report.push_back(line.clone());
        progress(line);
    }
    let status = child
        .wait()
        .map_err(|error| failure(format!("The update did not finish: {error}")))?;
    let output = result.join().ok().and_then(Result::ok).unwrap_or_default();
    outcome(status.success(), &output, report.into_iter().collect())
}

/// Reads the CLI's JSON result. On failure, its message is the error and the
/// device's own lines before it are the report, less the CLI's restatement.
fn outcome(success: bool, output: &str, report: Vec<String>) -> Result<(), Failure> {
    let value: Option<serde_json::Value> = serde_json::from_str(output).ok();
    if success && value.as_ref().and_then(|value| value["ok"].as_bool()) == Some(true) {
        return Ok(());
    }
    let error = value
        .as_ref()
        .and_then(|value| value["error"]["message"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "The update stopped without saying why.".to_owned());
    let report: Vec<_> = report
        .into_iter()
        .filter(|line| !line.ends_with(error.as_str()) && !line.ends_with('…'))
        .filter(|line| !line.contains("%"))
        .collect();
    Err(Failure {
        error,
        report: (!report.is_empty()).then(|| report.join("\n")),
    })
}

/// What `device update --check` printed, as the reason a device would not
/// open, when it is one an update fixes or a newer device.
pub(super) fn checked(output: &str) -> Option<device_ui::OpenFailure> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let data = &value["data"];
    let protocol = data["protocol"]
        .as_u64()
        .and_then(|protocol| u16::try_from(protocol).ok());
    match data["state"].as_str()? {
        "install" => Some(device_ui::OpenFailure::Missing),
        "update" | "newer" => protocol.map(device_ui::OpenFailure::Incompatible),
        _ => None,
    }
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
