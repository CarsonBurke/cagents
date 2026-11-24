//! The status bar's phone control: whether a phone is paired and connected,
//! how a phone reaches this computer, and pairing a new one by QR code.
//!
//! Everything it reads or writes is this computer's own: `authorized_keys`,
//! running bridges, network addresses and sshd. It works the same whichever
//! device the window shows. See `cagents::mobile::pairing` for the pairing
//! itself.

mod host;

use super::*;
use cagents::mobile::pairing::{self, Added, AuthorizedKeys, Outcome, Progress};
use std::{
    sync::Arc,
    time::{Instant, SystemTime},
};

/// How often the popover rereads connections and paired phones while open.
const OPEN_PROBE: Duration = Duration::from_secs(2);
/// How often the popover rereads addresses, Tailscale and sshd while open.
const NETWORK_PROBE: Duration = Duration::from_secs(10);
/// How often the closed control rereads whether a phone is connected.
const CLOSED_PROBE: Duration = Duration::from_secs(15);
/// The popover's content width.
const WIDTH: f32 = 312.0;
/// The QR code's side, quiet zone included.
const CODE_SIZE: f32 = 232.0;

fn scroll_id() -> widget::Id {
    widget::Id::new("phone-popover-scroll")
}

#[derive(Debug, Clone)]
pub enum Action {
    Menu(bool),
    Tick,
    Probed(Box<host::Status>),
    Pair,
    Started(Result<Arc<Started>, String>),
    Progressed(Progress),
    Allow,
    Decline,
    Cancel,
    Finished(Result<Option<String>, String>),
    Remove(String),
    ConfirmRemove,
    CancelRemove,
    Copy(String),
}

/// A pairing that has begun: its one-time key is authorized.
#[derive(Debug)]
pub struct Started {
    session: pairing::Pairing,
    code: Option<Arc<crate::ui_controls::QrMatrix>>,
}

struct Pairing {
    started: Arc<Started>,
    progress: Progress,
    /// The one-time key's line is being removed, or has been.
    ending: bool,
}

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    status: Option<host::Status>,
    probing: bool,
    probed_at: Option<Instant>,
    network_at: Option<Instant>,
    pairing: Option<Pairing>,
    /// Starting, allowing, declining or removing.
    busy: bool,
    /// The fingerprint of the phone whose removal awaits confirmation.
    removing: Option<String>,
    notice: Option<Result<String, String>>,
    /// Whether leftover pairings were cleaned up.
    swept: bool,
}

impl State {
    fn connected(&self) -> usize {
        self.status
            .as_ref()
            .map_or(0, |status| status.connections.len())
    }
}

fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) -> Action + Send + 'static,
    failed: impl FnOnce(String) -> T + Send + 'static,
) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(work)
                .await
                .unwrap_or_else(|error| failed(error.to_string()))
        },
        move |result| cosmic::Action::App(Message::Phone(done(result))),
    )
}

impl Cagents {
    pub(super) fn phone_subscription(&self) -> Subscription<Message> {
        let state = &self.phone_ui;
        if !self.settings.show_status_bar && state.pairing.is_none() {
            return Subscription::none();
        }
        let period = if state.open || state.pairing.is_some() {
            Duration::from_secs(1)
        } else {
            CLOSED_PROBE
        };
        iced::time::every(period).map(|_| Message::Phone(Action::Tick))
    }

    fn phone_probe(&mut self, force: bool) -> Task<Message> {
        let state = &mut self.phone_ui;
        let due = |at: Option<Instant>, every: Duration| at.is_none_or(|at| at.elapsed() >= every);
        if state.probing
            || !(force
                || due(
                    state.probed_at,
                    if state.open { OPEN_PROBE } else { CLOSED_PROBE },
                ))
        {
            return Task::none();
        }
        let network = state.open && (force || due(state.network_at, NETWORK_PROBE));
        state.probing = true;
        let sweep = !std::mem::replace(&mut state.swept, true);
        blocking(
            move || {
                if sweep
                    && let (Ok(keys), Ok(runtime)) = (
                        AuthorizedKeys::of_user(),
                        crate::instance::runtime_directory(),
                    )
                    && let Err(error) =
                        pairing::remove_stale_pairings(&keys, &runtime, SystemTime::now())
                {
                    tracing::warn!("couldn't remove leftover phone pairings: {error}");
                }
                host::probe(network)
            },
            |status| Action::Probed(Box::new(status)),
            |error| host::Status {
                phones: Err(error),
                unwritable: None,
                connections: Vec::new(),
                network: None,
            },
        )
    }

    pub(super) fn update_phone(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Menu(open) => {
                let open = open && self.settings.show_status_bar && !self.modal_is_open();
                self.phone_ui.open = open;
                if open {
                    self.device_ui.menu_open = false;
                    self.dismiss_chat_picker();
                    return self.phone_probe(true);
                }
                self.phone_ui.removing = None;
                if self.phone_ui.pairing.is_none() {
                    self.phone_ui.notice = None;
                }
                Task::none()
            }
            Action::Tick => {
                let mut tasks = vec![self.phone_probe(false)];
                if let Some(pairing) = &self.phone_ui.pairing
                    && !pairing.ending
                {
                    let started = pairing.started.clone();
                    tasks.push(blocking(
                        move || started.session.progress(SystemTime::now()),
                        Action::Progressed,
                        |_| Progress::Waiting,
                    ));
                }
                Task::batch(tasks)
            }
            Action::Probed(status) => {
                let state = &mut self.phone_ui;
                state.probing = false;
                state.probed_at = Some(Instant::now());
                let previous = state.status.take();
                let network = status
                    .network
                    .clone()
                    .or_else(|| previous.and_then(|previous| previous.network));
                if status.network.is_some() {
                    state.network_at = state.probed_at;
                }
                state.status = Some(host::Status { network, ..*status });
                Task::none()
            }
            Action::Pair => {
                let Some(network) = self
                    .phone_ui
                    .status
                    .as_ref()
                    .and_then(|status| status.network.clone())
                else {
                    return Task::none();
                };
                if self.phone_ui.busy {
                    return Task::none();
                }
                match &self.phone_ui.pairing {
                    // One that ended on its own makes way for a new code.
                    Some(pairing) if pairing.ending => self.phone_ui.pairing = None,
                    Some(_) => return Task::none(),
                    None => {}
                }
                self.phone_ui.busy = true;
                self.phone_ui.notice = None;
                self.phone_ui.removing = None;
                blocking(
                    move || {
                        let keys = AuthorizedKeys::of_user().map_err(|error| error.to_string())?;
                        let executable =
                            pairing::installed_cli().map_err(|error| error.to_string())?;
                        let runtime = crate::instance::runtime_directory()
                            .map_err(|error| error.to_string())?;
                        let session = pairing::Pairing::start(
                            keys,
                            &executable,
                            &runtime,
                            SystemTime::now(),
                        )?;
                        let code =
                            crate::ui_controls::QrMatrix::encode(&session.code(&network.connect()));
                        Ok(Arc::new(Started { session, code }))
                    },
                    Action::Started,
                    Err,
                )
            }
            Action::Started(result) => {
                self.phone_ui.busy = false;
                match result {
                    Ok(started) => {
                        self.phone_ui.pairing = Some(Pairing {
                            started,
                            progress: Progress::Waiting,
                            ending: false,
                        });
                    }
                    Err(error) => {
                        self.phone_ui.notice =
                            Some(Err(format!("Couldn't start pairing. {error}")));
                    }
                }
                Task::none()
            }
            Action::Progressed(progress) => {
                let Some(pairing) = &mut self.phone_ui.pairing else {
                    return Task::none();
                };
                if pairing.ending {
                    return Task::none();
                }
                let arrived = matches!(progress, Progress::Requested(_))
                    && !matches!(pairing.progress, Progress::Requested(_));
                let over = matches!(progress, Progress::Withdrawn | Progress::Expired);
                pairing.progress = progress;
                let mut tasks = Vec::new();
                if over {
                    // The code can't be used any more: take its key away now,
                    // and leave the outcome on screen.
                    pairing.ending = true;
                    let started = pairing.started.clone();
                    tasks.push(blocking(
                        move || started.session.finish(Outcome::Cancelled),
                        |result| match result {
                            Ok(()) => Action::Tick,
                            Err(error) => Action::Finished(Err(error)),
                        },
                        Err,
                    ));
                }
                if arrived && !self.phone_ui.open {
                    // The person is looking at the phone; bring the question
                    // to them.
                    tasks.push(self.update_phone(Action::Menu(true)));
                }
                Task::batch(tasks)
            }
            Action::Allow => {
                let Some(pairing) = &mut self.phone_ui.pairing else {
                    return Task::none();
                };
                let Progress::Requested(request) = pairing.progress.clone() else {
                    return Task::none();
                };
                if self.phone_ui.busy || pairing.ending {
                    return Task::none();
                }
                self.phone_ui.busy = true;
                pairing.ending = true;
                let started = pairing.started.clone();
                blocking(
                    move || {
                        let command = pairing::installed_cli()
                            .map_err(|error| error.to_string())
                            .and_then(|cli| pairing::bridge_command(&cli));
                        let result =
                            command.and_then(|command| started.session.allow(&request, &command));
                        match result {
                            Ok(Added::Added) => Ok(Some(format!(
                                "{} is paired. It connects on its own.",
                                request.name
                            ))),
                            Ok(Added::AlreadyPaired) => {
                                Ok(Some(format!("{} was already paired.", request.name)))
                            }
                            Err(error) => {
                                let _ = started.session.finish(Outcome::Cancelled);
                                Err(error)
                            }
                        }
                    },
                    Action::Finished,
                    Err,
                )
            }
            Action::Decline | Action::Cancel => {
                let declined = matches!(action, Action::Decline);
                let Some(pairing) = &mut self.phone_ui.pairing else {
                    return Task::none();
                };
                if pairing.ending {
                    // Already ended on its own: just put it away.
                    self.phone_ui.pairing = None;
                    return Task::none();
                }
                if self.phone_ui.busy {
                    return Task::none();
                }
                self.phone_ui.busy = true;
                pairing.ending = true;
                let started = pairing.started.clone();
                let name = match &pairing.progress {
                    Progress::Requested(request) => Some(request.name.clone()),
                    _ => None,
                };
                blocking(
                    move || {
                        let outcome = if declined {
                            Outcome::Declined
                        } else {
                            Outcome::Cancelled
                        };
                        started.session.finish(outcome)?;
                        Ok(name
                            .filter(|_| declined)
                            .map(|name| format!("{name} wasn't allowed.")))
                    },
                    Action::Finished,
                    Err,
                )
            }
            Action::Finished(result) => {
                self.phone_ui.busy = false;
                self.phone_ui.pairing = None;
                self.phone_ui.removing = None;
                self.phone_ui.notice = match result {
                    Ok(message) => message.map(Ok),
                    Err(error) => Some(Err(error)),
                };
                self.phone_probe(true)
            }
            Action::Remove(fingerprint) => {
                self.phone_ui.removing = Some(fingerprint);
                self.phone_ui.notice = None;
                Task::none()
            }
            Action::CancelRemove => {
                self.phone_ui.removing = None;
                Task::none()
            }
            Action::ConfirmRemove => {
                let Some(phone) = self.phone_ui.removing.as_ref().and_then(|fingerprint| {
                    self.phone_ui
                        .status
                        .as_ref()?
                        .phones
                        .as_ref()
                        .ok()?
                        .iter()
                        .find(|phone| phone.key.fingerprint() == *fingerprint)
                        .cloned()
                }) else {
                    self.phone_ui.removing = None;
                    return Task::none();
                };
                if self.phone_ui.busy {
                    return Task::none();
                }
                self.phone_ui.busy = true;
                blocking(
                    move || {
                        AuthorizedKeys::of_user()
                            .map_err(|error| error.to_string())?
                            .remove_phone(&phone.key)?;
                        Ok(Some(format!("{} can no longer connect.", phone.name)))
                    },
                    Action::Finished,
                    Err,
                )
            }
            Action::Copy(text) => iced::clipboard::write(text).map(cosmic::Action::App),
        }
    }

    /// The status bar's phone control, with its popover.
    pub(super) fn phone_status_control(&self) -> Element<'_, Message> {
        let state = &self.phone_ui;
        let open = state.open && !self.modal_is_open();
        let connected = state.connected();
        let (label, tone) = if state.pairing.is_some() {
            (
                "Pairing a phone".to_owned(),
                Some(design::accent_color as fn(&cosmic::Theme) -> iced::Color),
            )
        } else if connected > 0 {
            (
                if connected == 1 {
                    "Phone connected".to_owned()
                } else {
                    format!("{connected} phones connected")
                },
                Some(design::success_color as fn(&cosmic::Theme) -> iced::Color),
            )
        } else {
            ("Phone".to_owned(), None)
        };
        let mut control = crate::ui_controls::icon_button(Icon::Phone, label)
            .compact()
            .selected(open)
            .on_press(Message::Phone(Action::Menu(!open)));
        if let Some(tone) = tone {
            control = control.tone(design::Tone::Themed(tone));
        }
        let max_height = (self.window_height - super::account_ui::STATUS_BAR_HEIGHT - 2.0 * space::XL).max(160.0);
        crate::ui_controls::scrolling_popover(
            control,
            open.then(|| {
                widget::container(crate::ui_controls::popover_scroll(
                    self.phone_popover(),
                    scroll_id(),
                    max_height,
                    space::S,
                ))
                .width(crate::ui_controls::scrolling_width(WIDTH) + 2.0 * space::S)
                .into()
            }),
            widget::popover::Position::Top,
            Message::Phone(Action::Menu(false)),
        )
    }

    fn phone_popover(&self) -> Element<'_, Message> {
        let state = &self.phone_ui;
        let mut rows: Vec<Element<'_, Message>> = vec![design::heading("Phone").into()];
        let Some(status) = &state.status else {
            rows.push(design::caption("Checking…").into());
            return column(rows);
        };
        if let Some(notice) = &state.notice {
            rows.push(match notice {
                Ok(message) => design::body(message.as_str())
                    .class(cosmic::theme::Text::Custom(design::success_text))
                    .into(),
                Err(message) => design::body(message.as_str())
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            });
        }
        match &state.pairing {
            Some(pairing) => rows.push(self.phone_pairing(pairing)),
            None => {
                rows.push(phones_section(state, status));
                rows.push(self.phone_pair_action(status));
            }
        }
        if let Some(network) = &status.network {
            rows.push(design::horizontal_rule());
            rows.push(connect_section(network));
        }
        column(rows)
    }

    fn phone_pair_action<'a>(&'a self, status: &'a host::Status) -> Element<'a, Message> {
        let paired = status
            .phones
            .as_ref()
            .is_ok_and(|phones| !phones.is_empty());
        let label = if paired {
            "Pair another phone"
        } else {
            "Pair a phone"
        };
        let blocker = match &status.network {
            None => Some("Checking this computer…".to_owned()),
            Some(network) if !network.listening => Some(format!(
                "Turn on this computer's SSH server so a phone can connect. Nothing listens on port {}.",
                network.port
            )),
            Some(network) if network.host_key.is_none() => Some(
                "Couldn't read this computer's SSH host key, which a phone checks to be sure it reached this computer."
                    .to_owned(),
            ),
            // The connect section below says which addresses and why.
            Some(network) if network.hosts().is_empty() && network.only_tailscale => {
                Some("Connect Tailscale to pair a phone.".to_owned())
            }
            Some(network) if network.hosts().is_empty() => {
                Some("This computer has no network address a phone can reach.".to_owned())
            }
            Some(_) => status.unwritable.clone(),
        };
        let button = crate::ui_controls::button::primary(label)
            .width(Length::Fill)
            .on_press_maybe(
                (blocker.is_none() && !self.phone_ui.busy).then_some(Message::Phone(Action::Pair)),
            );
        let mut rows: Vec<Element<'_, Message>> = vec![button.into()];
        if let Some(blocker) = blocker {
            rows.push(design::caption(blocker).into());
        }
        widget::column::with_children(rows)
            .spacing(space::XS)
            .into()
    }

    fn phone_pairing<'a>(&'a self, pairing: &'a Pairing) -> Element<'a, Message> {
        let busy = self.phone_ui.busy;
        match &pairing.progress {
            Progress::Requested(request) if !pairing.ending || busy => {
                let actions = widget::row::with_children(vec![
                    crate::ui_controls::button::secondary("Don't allow")
                        .on_press_maybe((!busy).then_some(Message::Phone(Action::Decline)))
                        .into(),
                    crate::ui_controls::button::primary("Allow")
                        .on_press_maybe((!busy).then_some(Message::Phone(Action::Allow)))
                        .into(),
                ])
                .spacing(space::S);
                widget::column::with_children(vec![
                    design::heading(format!("Allow “{}” to connect?", request.name)).into(),
                    design::body(
                        "It will be able to read your conversations, answer approvals and send \
                         messages, as you. You can remove it here at any time.",
                    )
                    .into(),
                    design::caption(format!("Phone key {}", request.key.fingerprint())).into(),
                    widget::container(actions).align_right(Length::Fill).into(),
                ])
                .spacing(space::S)
                .into()
            }
            Progress::Withdrawn | Progress::Expired | Progress::Requested(_) => {
                let message = match pairing.progress {
                    Progress::Withdrawn => "The phone stopped pairing before you answered.",
                    _ => "The code expired.",
                };
                widget::column::with_children(vec![
                    design::body(message).into(),
                    widget::row::with_children(vec![
                        crate::ui_controls::button::secondary("Close")
                            .on_press(Message::Phone(Action::Cancel))
                            .into(),
                        crate::ui_controls::button::primary("Show a new code")
                            .on_press_maybe((!busy).then_some(Message::Phone(Action::Pair)))
                            .into(),
                    ])
                    .spacing(space::S)
                    .into(),
                ])
                .spacing(space::S)
                .into()
            }
            Progress::Waiting => {
                let left = pairing
                    .started
                    .session
                    .expires()
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs();
                let code: Element<'_, Message> = match &pairing.started.code {
                    Some(code) => crate::ui_controls::qr_code(code.clone(), CODE_SIZE),
                    None => design::caption("The code is too long to show.").into(),
                };
                widget::column::with_children(vec![
                    design::body("Scan this code with the cagents app on your phone.").into(),
                    widget::container(code).center_x(Length::Fill).into(),
                    widget::row::with_children(vec![
                        design::caption(format!(
                            "Works once · expires in {}:{:02}",
                            left / 60,
                            left % 60
                        ))
                        .width(Length::Fill)
                        .into(),
                        crate::ui_controls::button::secondary("Cancel")
                            .on_press_maybe((!busy).then_some(Message::Phone(Action::Cancel)))
                            .into(),
                    ])
                    .align_y(Alignment::Center)
                    .into(),
                ])
                .spacing(space::S)
                .into()
            }
        }
    }
}

fn column(rows: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    widget::column::with_children(rows)
        .spacing(space::S)
        .width(WIDTH)
        .into()
}

/// An icon inked with `tone`.
fn toned(icon: Icon, tone: fn(&cosmic::Theme) -> iced::Color) -> Element<'static, Message> {
    widget::container(widget::icon(icon.handle()).size(design::ROW_ICON_SIZE))
        .class(cosmic::theme::Container::custom(move |theme| {
            iced::widget::container::Style {
                icon_color: Some(tone(theme)),
                ..Default::default()
            }
        }))
        .into()
}

fn route_label(route: host::Route) -> &'static str {
    match route {
        host::Route::Tailscale => "over Tailscale",
        host::Route::Lan => "on the local network",
        host::Route::Other => "",
    }
}

fn phones_section<'a>(state: &'a State, status: &'a host::Status) -> Element<'a, Message> {
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    match &status.connections[..] {
        [] => {}
        connections => {
            for connection in connections {
                rows.push(
                    widget::row::with_children(vec![
                        toned(Icon::Check, design::success_color),
                        design::body(
                            format!("Connected {}", route_label(connection.route))
                                .trim_end()
                                .to_owned(),
                        )
                        .width(Length::Fill)
                        .into(),
                        design::caption(connection.from.as_str()).into(),
                    ])
                    .spacing(space::S)
                    .align_y(Alignment::Center)
                    .into(),
                );
            }
        }
    }
    let phones = match &status.phones {
        Ok(phones) => phones,
        Err(error) => {
            rows.push(
                design::body(error.as_str())
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            );
            return widget::column::with_children(rows).spacing(space::S).into();
        }
    };
    if phones.is_empty() {
        rows.push(
            widget::column::with_children(vec![
                design::body("No phone is paired with this computer.").into(),
                design::caption(
                    "Pair one to follow your agents and answer them from the cagents app.",
                )
                .into(),
            ])
            .spacing(space::XXS)
            .into(),
        );
    } else if status.connections.is_empty() {
        rows.push(design::caption("No phone is connected now.").into());
    }
    for phone in phones {
        let fingerprint = phone.key.fingerprint();
        if state.removing.as_deref() == Some(fingerprint.as_str()) {
            rows.push(
                widget::column::with_children(vec![
                    design::body(format!("Remove {}? It will no longer connect.", phone.name))
                        .into(),
                    widget::container(
                        widget::row::with_children(vec![
                            crate::ui_controls::button::secondary("Cancel")
                                .on_press(Message::Phone(Action::CancelRemove))
                                .into(),
                            crate::ui_controls::button::destructive("Remove")
                                .on_press_maybe(
                                    (!state.busy).then_some(Message::Phone(Action::ConfirmRemove)),
                                )
                                .into(),
                        ])
                        .spacing(space::S),
                    )
                    .align_right(Length::Fill)
                    .into(),
                ])
                .spacing(space::S)
                .into(),
            );
            continue;
        }
        let detail = if !phone.restricted {
            Some("Not restricted to cagents: this key can also open a shell.".to_owned())
        } else {
            phone
                .missing_executable
                .as_ref()
                .map(|path| format!("Runs {path}, which no longer exists. Pair it again."))
        };
        let mut text: Vec<Element<'_, Message>> = vec![design::body(phone.name.as_str()).into()];
        match detail {
            Some(detail) => text.push(
                design::caption(detail)
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            ),
            None => text.push(design::caption(short_fingerprint(&fingerprint)).into()),
        }
        rows.push(
            widget::row::with_children(vec![
                toned(Icon::Phone, design::muted_icon_color),
                widget::column::with_children(text)
                    .width(Length::Fill)
                    .into(),
                crate::ui_controls::icon_button(Icon::Delete, format!("Remove {}", phone.name))
                    .on_press_maybe(
                        (!state.busy).then(|| Message::Phone(Action::Remove(fingerprint.clone()))),
                    )
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    widget::column::with_children(rows).spacing(space::S).into()
}

/// The start of a fingerprint, enough to tell two keys apart.
fn short_fingerprint(fingerprint: &str) -> String {
    let mut short: String = fingerprint.chars().take(7 + 12).collect();
    if short.len() < fingerprint.len() {
        short.push('…');
    }
    short
}

fn connect_section(network: &host::Network) -> Element<'_, Message> {
    let mut rows: Vec<Element<'_, Message>> =
        vec![design::section_heading("How a phone connects").into()];
    let fact =
        |label: &'static str, value: Element<'static, Message>| -> Element<'static, Message> {
            widget::row::with_children(vec![
                design::caption(label).width(Length::Fixed(72.0)).into(),
                widget::container(value).width(Length::Fill).into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Start)
            .into()
        };
    let lines = |values: Vec<String>| -> Element<'static, Message> {
        widget::column::with_children(
            values
                .into_iter()
                .map(|value| -> Element<'static, Message> { design::body(value).into() })
                .collect::<Vec<_>>(),
        )
        .into()
    };
    rows.push(fact(
        "Tailscale",
        match &network.tailscale {
            host::Tailscale::Connected { name, addresses } => {
                lines(name.iter().chain(addresses).cloned().collect())
            }
            host::Tailscale::NotConnected => design::body("Not connected")
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
            host::Tailscale::NotInstalled => design::body("Not installed")
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        },
    ));
    let muted = |value: String| -> Element<'static, Message> {
        design::body(value)
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .into()
    };
    rows.push(fact(
        "Network",
        if network.lan.is_empty() && network.blocked.is_empty() {
            muted("No local address".into())
        } else {
            widget::column::with_children(
                network
                    .lan
                    .iter()
                    .map(|address| -> Element<'static, Message> {
                        design::body(address.clone()).into()
                    })
                    .chain(network.blocked.iter().map(|address| muted(address.clone())))
                    .collect::<Vec<_>>(),
            )
            .into()
        },
    ));
    rows.push(fact(
        "SSH",
        if network.listening {
            design::body(format!("{} · port {}", network.user, network.port)).into()
        } else {
            design::body(format!("Not running on port {}", network.port))
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .into()
        },
    ));
    if !network.blocked.is_empty() {
        let addresses = network.blocked.join(", ");
        let note = if network.only_tailscale {
            format!(
                "This computer's firewall allows SSH only over Tailscale, so a phone can't connect to {addresses}."
            )
        } else {
            format!(
                "This computer's firewall blocks SSH on {addresses}, so a phone can't connect there."
            )
        };
        rows.push(
            design::caption(note)
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .into(),
        );
    }
    if !matches!(network.tailscale, host::Tailscale::Connected { .. }) && !network.lan.is_empty() {
        rows.push(
            design::caption("Without Tailscale, a phone connects only on the same network.").into(),
        );
    }
    if let Some(host_key) = &network.host_key {
        rows.push(fact(
            "Host key",
            widget::row::with_children(vec![
                design::caption(host_key.clone()).width(Length::Fill).into(),
                crate::ui_controls::icon_button(Icon::Copy, "Copy host key fingerprint")
                    .compact()
                    .on_press(Message::Phone(Action::Copy(host_key.clone())))
                    .into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .into(),
        ));
    }
    widget::column::with_children(rows).spacing(space::S).into()
}
