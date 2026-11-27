//! The status bar's reading of how loaded this window's device is: its
//! processors, memory and home disk, each as a ring and a percentage.
//!
//! The daemon on the device sends a reading only when a figure shown here
//! moves, so a steady device redraws nothing.
use super::*;
use crate::device_stats::{CRITICAL_PERCENT, DeviceStats, WARNING_PERCENT};
use cosmic::iced::futures::SinkExt;

/// What the window knows of its device's load.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum DeviceLoad {
    /// No reading has arrived on the current connection yet.
    #[default]
    Waiting,
    Live(DeviceStats),
    /// The device is not reporting; the reason is shown on hover.
    Unavailable(String),
}

/// One figure as the bar draws it.
struct Gauge {
    label: &'static str,
    percent: Option<u8>,
}

/// How much a figure asks for attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Unknown,
    Normal,
    Warning,
    Critical,
}

impl Gauge {
    fn level(&self) -> Level {
        match self.percent {
            None => Level::Unknown,
            Some(percent) if percent >= CRITICAL_PERCENT => Level::Critical,
            Some(percent) if percent >= WARNING_PERCENT => Level::Warning,
            Some(_) => Level::Normal,
        }
    }

    /// The ring's tone: the default accent unless the figure is high.
    fn tone(&self) -> Option<design::Tone> {
        match self.level() {
            Level::Critical => Some(design::Tone::Themed(design::destructive_color)),
            Level::Warning => Some(design::Tone::Themed(design::warning_color)),
            Level::Normal | Level::Unknown => None,
        }
    }

    fn text_class(&self) -> cosmic::theme::Text {
        match self.level() {
            Level::Critical => cosmic::theme::Text::Custom(design::destructive_text),
            Level::Warning => cosmic::theme::Text::Custom(design::warning_text),
            Level::Normal => cosmic::theme::Text::Default,
            Level::Unknown => cosmic::theme::Text::Custom(design::muted_text),
        }
    }
}

fn gauges(load: &DeviceLoad) -> [Gauge; 3] {
    let stats = match load {
        DeviceLoad::Live(stats) => Some(stats),
        _ => None,
    };
    [
        Gauge {
            label: "CPU",
            percent: stats.and_then(|stats| stats.cpu).map(|cpu| cpu.percent),
        },
        Gauge {
            label: "RAM",
            percent: stats
                .and_then(|stats| stats.memory)
                .and_then(|memory| memory.percent()),
        },
        Gauge {
            label: "Disk",
            percent: stats
                .and_then(|stats| stats.disk)
                .and_then(|disk| disk.percent()),
        },
    ]
}

/// A size as people read one: gigabytes to a tenth below a hundred, whole
/// gigabytes below a thousand, and terabytes to a hundredth above.
fn size(bytes: u64) -> String {
    let gigabytes = bytes as f64 / 1e9;
    if gigabytes < 100.0 {
        format!("{gigabytes:.1} GB")
    } else if gigabytes < 999.5 {
        format!("{gigabytes:.0} GB")
    } else {
        format!("{:.2} TB", gigabytes / 1e3)
    }
}

/// The hover text: what each figure measures, in full.
pub(super) fn detail(load: &DeviceLoad, device: &str) -> String {
    let stats = match load {
        DeviceLoad::Waiting => return format!("Reading {device}'s load…"),
        DeviceLoad::Unavailable(reason) => return reason.clone(),
        DeviceLoad::Live(stats) => stats,
    };
    let mut lines = Vec::with_capacity(4);
    lines.push(match stats.cpu {
        Some(cpu) => format!(
            "CPU {}% · {} core{} · load {:.1}",
            cpu.percent,
            cpu.cores,
            if cpu.cores == 1 { "" } else { "s" },
            f64::from(cpu.load_centi) / 100.0
        ),
        None => "CPU: measuring…".to_owned(),
    });
    lines.push(match stats.memory {
        Some(memory) => {
            let mut line = format!("Memory {} of {}", size(memory.used), size(memory.total));
            if memory.swap_total > 0 {
                line.push_str(&format!(" · swap {}", size(memory.swap_used)));
            }
            line
        }
        None => "Memory: unavailable".to_owned(),
    });
    lines.push(match stats.disk {
        Some(disk) => format!("Home disk {} of {}", size(disk.used), size(disk.total)),
        None => "Home disk: unavailable".to_owned(),
    });
    lines.push(match device {
        "This device" => "On this computer".to_owned(),
        device => format!("On {device}"),
    });
    lines.join("\n")
}

impl Cagents {
    /// One connection per window watches its device while the status bar
    /// shows. Hiding the bar drops the subscription, which hangs up, and the
    /// daemon stops reading the device once no window watches.
    pub(super) fn device_load_subscription(&self) -> Subscription<Message> {
        if !self.settings.show_status_bar {
            return Subscription::none();
        }
        Subscription::run(watch_load).map(Message::DeviceLoad)
    }

    /// The device's load for the status bar: read-only, beside the device
    /// chooser it describes. In a compact window the labels drop and the
    /// rings keep their order.
    pub(super) fn device_load_status(&self, compact: bool) -> Element<'_, Message> {
        let selected = crate::device::selected()
            .map(|device| device.name.as_str())
            .unwrap_or("local");
        let load = &self.device_ui.load;
        let items: Vec<Element<'_, Message>> = gauges(load)
            .into_iter()
            .map(|gauge| {
                let ring = match gauge.percent {
                    Some(percent) => crate::ui_controls::budget_radial_toned(
                        f32::from(percent) / 100.0,
                        gauge.tone(),
                    ),
                    // An empty ring: there is a figure here, just none known.
                    None => crate::ui_controls::budget_radial_toned(0.0, None),
                };
                let value = gauge
                    .percent
                    .map_or_else(|| "—".to_owned(), |percent| format!("{percent}%"));
                let mut parts = vec![ring];
                if !compact {
                    parts.push(
                        design::caption(gauge.label)
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .into(),
                    );
                }
                parts.push(
                    design::caption(value)
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .class(gauge.text_class())
                        .into(),
                );
                widget::row::with_children(parts)
                    .spacing(space::XXS * 2.0)
                    .align_y(Alignment::Center)
                    .into()
            })
            .collect();
        let detail = detail(load, super::device_ui::device_label(selected));
        crate::interaction::tooltip_above(
            crate::ui_controls::labelled(
                widget::container(
                    widget::row::with_children(items)
                        .spacing(space::S + space::XXS)
                        .align_y(Alignment::Center),
                )
                .padding(Padding::from([0.0, space::S])),
                detail.clone(),
            ),
            detail,
        )
    }
}

/// The device's readings, from a thread that holds the subscription's
/// connection and reconnects after the daemon or the device goes away.
fn watch_load() -> impl cosmic::iced::futures::Stream<Item = DeviceLoad> {
    cosmic::iced::stream::channel(2, async move |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let link = Arc::new(Mutex::new(Link::default()));
        let held = link.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-device-load".into())
            .spawn(move || {
                while !lock(&held).closed {
                    let watched = watch(&sender, &held);
                    // A connection given up on must not stay open through the
                    // retry delay, where the daemon would keep sampling for it.
                    {
                        let mut link = lock(&held);
                        if let Some(hangup) = link.hangup.take() {
                            hangup.hang_up();
                        }
                        if link.closed {
                            break;
                        }
                    }
                    if let Err(error) = watched {
                        tracing::debug!(%error, "the device's load is not reporting");
                        let device = crate::device::selected()
                            .map_or("This device", |device| device.name.as_str());
                        if sender
                            .send_blocking(DeviceLoad::Unavailable(format!(
                                "{device} isn't reporting its load. Retrying…"
                            )))
                            .is_err()
                        {
                            break;
                        }
                    }
                    std::thread::sleep(crate::daemon_health::retry_delay());
                }
            });
        if let Err(error) = spawned {
            let _ = output
                .send(DeviceLoad::Unavailable(format!(
                    "The device's load could not be watched: {error}"
                )))
                .await;
            return;
        }
        let _closed = CloseOnDrop(link);
        while let Ok(load) = receiver.recv().await {
            if output.send(load).await.is_err() {
                break;
            }
        }
    })
}

/// What the subscription and its thread share. Closing and publishing the
/// connection's hangup happen under one lock, so a subscription dropped while
/// the thread connects can never leave it blocked on a connection nobody
/// will end.
#[derive(Default)]
struct Link {
    closed: bool,
    hangup: Option<crate::control::Hangup>,
}

fn lock(link: &Mutex<Link>) -> std::sync::MutexGuard<'_, Link> {
    link.lock().unwrap_or_else(|p| p.into_inner())
}

fn watch(sender: &async_channel::Sender<DeviceLoad>, link: &Mutex<Link>) -> Result<(), String> {
    // Never start a daemon merely to watch it; the window starts one when it
    // needs one, and this reconnects then.
    let socket =
        crate::terminal::existing_daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    {
        let mut link = lock(link);
        if link.closed {
            return Ok(());
        }
        link.hangup = Some(client.hangup().map_err(|error| error.to_string())?);
    }
    let (mut revision, stats) = client
        .watch_device_stats()
        .map_err(|error| error.to_string())?;
    let closed = |_| "the window closed".to_owned();
    sender
        .send_blocking(stats.map_or(DeviceLoad::Waiting, DeviceLoad::Live))
        .map_err(closed)?;
    loop {
        let (next, stats) = client
            .next_device_stats(revision)
            .map_err(|error| error.to_string())?;
        revision = next;
        sender
            .send_blocking(DeviceLoad::Live(stats))
            .map_err(closed)?;
    }
}

/// Ends the watch when the subscription goes: the window closed or hid its
/// status bar.
struct CloseOnDrop(Arc<Mutex<Link>>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let mut link = lock(&self.0);
        link.closed = true;
        if let Some(hangup) = link.hangup.take() {
            hangup.hang_up();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_stats::{Cpu, Disk, Memory};

    fn reading(cpu: u8, memory: u64, disk: u64) -> DeviceLoad {
        DeviceLoad::Live(DeviceStats {
            cpu: Some(Cpu {
                percent: cpu,
                cores: 16,
                load_centi: 190,
            }),
            memory: Some(Memory {
                used: memory,
                total: 100,
                swap_used: 0,
                swap_total: 0,
            }),
            disk: Some(Disk {
                used: disk,
                total: 100,
            }),
        })
    }

    #[test]
    fn pressure_turns_amber_then_red() {
        let levels = gauges(&reading(79, 80, 90)).map(|gauge| gauge.level());
        assert_eq!(levels, [Level::Normal, Level::Warning, Level::Critical]);
    }

    #[test]
    fn an_unknown_device_shows_no_figures() {
        for load in [DeviceLoad::Waiting, DeviceLoad::Unavailable("x".into())] {
            assert!(
                gauges(&load)
                    .iter()
                    .all(|gauge| gauge.level() == Level::Unknown)
            );
        }
        assert_eq!(
            detail(
                &DeviceLoad::Unavailable("fw16 isn't reporting".into()),
                "fw16"
            ),
            "fw16 isn't reporting"
        );
    }

    #[test]
    fn the_detail_names_every_figure() {
        let DeviceLoad::Live(mut stats) = reading(12, 58, 47) else {
            unreachable!()
        };
        stats.memory = Some(Memory {
            used: 9_300_000_000,
            total: 16_000_000_000,
            swap_used: 400_000_000,
            swap_total: 8_000_000_000,
        });
        stats.disk = Some(Disk {
            used: 238_000_000_000,
            total: 1_982_200_000_000,
        });
        assert_eq!(
            detail(&DeviceLoad::Live(stats), "This device"),
            "CPU 12% · 16 cores · load 1.9\nMemory 9.3 GB of 16.0 GB · swap 0.4 GB\n\
             Home disk 238 GB of 1.98 TB\nOn this computer"
        );
    }
}
