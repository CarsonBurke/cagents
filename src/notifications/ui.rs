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
    presence_wake: Option<crossbeam_channel::Sender<()>>,
    pending_acknowledgements: Arc<Mutex<std::collections::BTreeSet<uuid::Uuid>>>,
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
            presence_wake: None,
            pending_acknowledgements: Default::default(),
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
        if self.presence_wake.is_none() {
            let (wake, receiver) = crossbeam_channel::bounded(1);
            let presence = self.presence.clone();
            let acknowledgements = self.pending_acknowledgements.clone();
            let actions = self.sender.clone();
            let stopped = self.sender.clone();
            std::thread::Builder::new()
                .name("ceditor-focus".into())
                .spawn(move || {
                    run_presence_updates(
                        receiver,
                        &presence,
                        |presence| {
                            let result = (|| {
                                let socket = crate::terminal_ipc::daemon_socket_path()
                                    .map_err(|e| e.to_string())?;
                                let mut client =
                                    ControlClient::connect(&socket).map_err(|e| e.to_string())?;
                                client
                                    .notification_presence(presence)
                                    .map_err(|e| e.to_string())?;
                                flush_acknowledgements(&acknowledgements, |id| {
                                    client
                                        .acknowledge_notification(id)
                                        .map_err(|e| e.to_string())
                                })
                            })();
                            if let Err(error) = &result {
                                let _ = actions.try_send(Action::Failed(error.clone()));
                            }
                            result.is_ok()
                        },
                        || stopped.is_closed(),
                    );
                })
                .map_err(|error| error.to_string())?;
            self.presence_wake = Some(wake);
        }
        let wake = self
            .presence_wake
            .as_ref()
            .expect("created control worker")
            .clone();
        let sender = self.sender.clone();
        let presence = self.presence.clone();
        std::thread::Builder::new()
            .name("ceditor-attention".into())
            .spawn(move || {
                let mut reported = None;
                while !sender.is_closed() {
                    let result = (|| -> Result<(), String> {
                        let socket =
                            crate::terminal::ensure_daemon_ready_for(super::Application::Ceditor)
                                .map_err(|error| error.to_string())?;
                        let mut client =
                            ControlClient::connect(&socket).map_err(|error| error.to_string())?;
                        let snapshot = client
                            .watch_notifications(
                                presence.lock().unwrap_or_else(|p| p.into_inner()).clone(),
                            )
                            .map_err(|error| error.to_string())?;
                        // Registration can race a focus edge on another socket.
                        // Reapply latest presence after this watch is installed.
                        let _ = wake.try_send(());
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

    pub fn focused_target(&self) -> Option<Target> {
        self.presence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .focused_target
            .clone()
    }

    pub fn acknowledge(&self, ids: Vec<uuid::Uuid>) -> cosmic::iced::Task<Action> {
        let changed = {
            let mut pending = self
                .pending_acknowledgements
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let before = pending.len();
            pending.extend(ids);
            before != pending.len()
        };
        if changed && let Some(wake) = &self.presence_wake {
            let _ = wake.try_send(());
        }
        cosmic::iced::Task::none()
    }

    pub fn focus(&self, target: Option<Target>) -> cosmic::iced::Task<Action> {
        {
            let mut presence = self.presence.lock().unwrap_or_else(|p| p.into_inner());
            if presence.focused_target == target {
                return cosmic::iced::Task::none();
            }
            presence.focused_target = target;
        }
        if let Some(wake) = &self.presence_wake {
            let _ = wake.try_send(());
        }
        cosmic::iced::Task::none()
    }
}

fn flush_acknowledgements(
    pending: &Mutex<std::collections::BTreeSet<uuid::Uuid>>,
    mut acknowledge: impl FnMut(uuid::Uuid) -> Result<(), String>,
) -> Result<(), String> {
    let ids = pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .copied()
        .collect::<Vec<_>>();
    for id in ids {
        acknowledge(id)?;
        pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }
    Ok(())
}

fn run_presence_updates(
    receiver: crossbeam_channel::Receiver<()>,
    presence: &Mutex<Presence>,
    mut deliver: impl FnMut(Presence) -> bool,
    stopped: impl Fn() -> bool,
) {
    while receiver.recv().is_ok() {
        loop {
            if stopped() {
                return;
            }
            // Read after the previous RPC completed, never capture a stale
            // presence in an independently scheduled blocking task.
            let latest = presence.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if deliver(latest) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_focus_ack_is_retained_and_retried_without_another_ui_edge() {
        let id = uuid::Uuid::new_v4();
        let pending = Arc::new(Mutex::new(std::collections::BTreeSet::from([id])));
        let (wake, receiver) = crossbeam_channel::bounded(1);
        wake.send(()).unwrap();
        drop(wake); // No later focus edge is available to rescue this retry.
        let mut attempts = Vec::new();
        run_presence_updates(
            receiver,
            &Mutex::new(Presence::default()),
            |_| {
                let result = flush_acknowledgements(&pending, |found| {
                    assert_eq!(found, id);
                    attempts.push(std::time::Instant::now());
                    if attempts.len() == 1 {
                        Err("daemon temporarily unavailable".into())
                    } else {
                        Ok(())
                    }
                });
                if result.is_err() {
                    assert!(pending.lock().unwrap().contains(&id));
                }
                result.is_ok()
            },
            || false,
        );
        assert!(pending.lock().unwrap().is_empty());
        assert_eq!(attempts.len(), 2);
        assert!(attempts[1].duration_since(attempts[0]) >= std::time::Duration::from_secs(2));
    }

    #[test]
    fn delayed_presence_rpc_cannot_overwrite_a_later_focus_edge() {
        let presence = Arc::new(Mutex::new(Presence {
            window: "first".into(),
            ..Default::default()
        }));
        let (wake, receiver) = crossbeam_channel::bounded(1);
        let (entered, entry) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let values = Arc::new(Mutex::new(Vec::new()));
        let recorded = values.clone();
        let current = presence.clone();
        let worker = std::thread::spawn(move || {
            run_presence_updates(
                receiver,
                &current,
                |value| {
                    if value.window == "first" {
                        entered.send(()).unwrap();
                        blocked.recv().unwrap();
                    }
                    recorded.lock().unwrap().push(value.window);
                    true
                },
                || false,
            )
        });
        wake.send(()).unwrap();
        entry
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        presence.lock().unwrap().window = "second".into();
        wake.try_send(()).unwrap();
        presence.lock().unwrap().window = "latest".into();
        assert!(
            wake.try_send(()).is_err(),
            "pending edge coalesces into latest presence"
        );
        release.send(()).unwrap();
        drop(wake);
        worker.join().unwrap();
        assert_eq!(*values.lock().unwrap(), ["first", "latest"]);
    }
}
