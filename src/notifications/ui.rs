//! A lightweight client of the daemon broker for standalone shell windows.
//! Delivery and deduplication remain daemon-owned.
use super::{Destination, Presence, Snapshot, Target, Update};
use crate::{control::ControlClient, instance::UiActivationListener};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub enum Action {
    Snapshot(Snapshot),
    Activated(Target),
    ActivateWindow(Option<String>),
    Failed(String),
}

pub struct Observer {
    destination: Destination,
    presence: Arc<Mutex<Presence>>,
    sender: async_channel::Sender<Action>,
    receiver: async_channel::Receiver<Action>,
    listener: Option<UiActivationListener>,
    started: bool,
}

impl Default for Observer {
    fn default() -> Self {
        let destination = Destination {
            application: super::Application::Ceditor,
            window: uuid::Uuid::new_v4().to_string(),
        };
        let (sender, receiver) = async_channel::bounded(32);
        Self {
            presence: Arc::new(Mutex::new(Presence {
                window: destination.window.clone(),
                application: destination.application,
                ..Default::default()
            })),
            destination,
            sender,
            receiver,
            listener: None,
            started: false,
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.receiver.close();
    }
}

impl Observer {
    pub fn is_bound(&self) -> bool {
        self.listener.is_some()
    }

    pub fn destination(&self) -> Destination {
        self.destination.clone()
    }

    pub fn bind_editor(window: Option<String>) -> Result<Self, String> {
        let mut state = Self::default();
        if let Some(window) = window {
            state.destination.window = window;
            state
                .presence
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .window = state.destination.window.clone();
        }
        state.destination.validate()?;
        let (sender, receiver) = async_channel::bounded(8);
        state.listener = Some(
            UiActivationListener::bind_destination(&state.destination, sender)
                .map_err(|error| error.to_string())?,
        );
        let delivery = state.sender.clone();
        std::thread::Builder::new()
            .name("ceditor-activation".into())
            .spawn(move || {
                while let Ok(activation) = receiver.recv_blocking() {
                    if let Some(update) = activation.update {
                        let _ = update.response.send(
                            "error: standalone Ceditor is updated by its own installer".into(),
                        );
                        continue;
                    }
                    if delivery
                        .send_blocking(Action::ActivateWindow(activation.token))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(state)
    }

    /// Start lazily when a GitHub repository is being scanned, or when an
    /// explicit desktop activation reopened this editor window.
    pub fn start(&mut self) -> Result<(), String> {
        if !self.is_bound() {
            return Err(
                "this editor window has no activation listener; reopen it to enable review alerts"
                    .into(),
            );
        }
        if self.started {
            return Ok(());
        }
        let sender = self.sender.clone();
        let presence = self.presence.clone();
        std::thread::Builder::new()
            .name("ceditor-attention".into())
            .spawn(move || {
                let mut reported = None;
                while !sender.is_closed() {
                    let result = (|| -> Result<(), String> {
                        let socket = crate::terminal::ensure_daemon_ready()
                            .map_err(|error| error.to_string())?;
                        let mut client =
                            ControlClient::connect(&socket).map_err(|error| error.to_string())?;
                        let snapshot = client
                            .watch_notifications(
                                presence.lock().unwrap_or_else(|p| p.into_inner()).clone(),
                            )
                            .map_err(|error| error.to_string())?;
                        sender
                            .send_blocking(Action::Snapshot(snapshot))
                            .map_err(|error| error.to_string())?;
                        while !sender.is_closed() {
                            let action = match client
                                .next_notification()
                                .map_err(|error| error.to_string())?
                            {
                                Update::Changed => Action::Snapshot(
                                    client
                                        .read_notifications()
                                        .map_err(|error| error.to_string())?,
                                ),
                                Update::Activated(target) => Action::Activated(target),
                            };
                            sender
                                .send_blocking(action)
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result
                        && reported.as_ref() != Some(&error)
                    {
                        reported = Some(error.clone());
                        let _ = sender.send_blocking(Action::Failed(error));
                    }
                    if !sender.is_closed() {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        self.started = true;
        Ok(())
    }

    pub fn next(&self) -> cosmic::iced::Task<Action> {
        let receiver = self.receiver.clone();
        cosmic::iced::Task::perform(async move { receiver.recv().await.ok() }, |action| action)
            .and_then(cosmic::iced::Task::done)
    }

    pub fn focus(&self, target: Option<Target>) -> cosmic::iced::Task<Action> {
        let presence = {
            let mut presence = self.presence.lock().unwrap_or_else(|p| p.into_inner());
            if presence.focused_target == target {
                return cosmic::iced::Task::none();
            }
            presence.focused_target = target;
            presence.clone()
        };
        if !self.started {
            return cosmic::iced::Task::none();
        }
        cosmic::iced::Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let socket = crate::terminal_ipc::daemon_socket_path()
                        .map_err(|error| error.to_string())?;
                    ControlClient::connect(&socket)
                        .map_err(|error| error.to_string())?
                        .notification_presence(presence)
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result)
            },
            |result| result.err().map(Action::Failed),
        )
        .and_then(cosmic::iced::Task::done)
    }
}
