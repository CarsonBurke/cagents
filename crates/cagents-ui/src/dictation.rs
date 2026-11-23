//! Dictation as the app sees it: the `cagents-dictate` helper beside this
//! executable, which listens and transcribes, and its model downloads.
//!
//! The helper starts when someone first dictates and keeps its model loaded
//! for a while after, so the next dictation starts at once; the app closes it
//! when it has been idle (see [`Client::release`]). Everything here returns
//! immediately: processes are run by tasks on the async runtime, and what
//! they report arrives through [`Client::events`] as [`Update`]s.
//!
//! The input level is the exception. It changes twenty times a second, and
//! only the level meter shows it, so the helper's reader stores it in
//! [`Client::level`] for the meter to read when it draws, rather than wake
//! the whole app for each one.

use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use cagents_dictate::protocol::{
    Command, ErrorKind, Event, InstallError, InstallEvent, PROTOCOL, SessionId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::companion::Product;

/// What the helper processes report to the app.
#[derive(Clone, Debug)]
pub enum Update {
    /// An event from the helper started as `generation`. Levels are not
    /// among them: see [`Client::level`].
    Helper { generation: u64, event: Event },
    /// The helper started as `generation` has gone. `problem` says why when
    /// it was not closed by the app.
    Exited {
        generation: u64,
        problem: Option<String>,
    },
    /// Progress of a model download, ending with `Installed` or `Failed`.
    Install {
        model: &'static str,
        event: InstallEvent,
    },
    /// A model was removed, or why not.
    Removed {
        model: &'static str,
        result: Result<(), String>,
    },
}

/// The input level, from 0 to 1, shared with the level meter.
#[derive(Clone, Debug, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, level: f32) {
        self.0
            .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// How long a released helper may take to finish and exit before it is
/// stopped. Released, it holds no session, so this is only its shutdown.
const RELEASE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

struct Helper {
    generation: u64,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    /// Stops the process at once, rather than letting it finish.
    kill: tokio::sync::oneshot::Sender<()>,
}

/// The app's side of dictation.
pub struct Client {
    sender: async_channel::Sender<Update>,
    receiver: async_channel::Receiver<Update>,
    helper: Option<Helper>,
    generations: Arc<AtomicU64>,
    next_session: SessionId,
    level: Level,
    install: Option<(&'static str, tokio::task::AbortHandle)>,
}

impl Default for Client {
    fn default() -> Self {
        let (sender, receiver) = async_channel::unbounded();
        Self {
            sender,
            receiver,
            helper: None,
            generations: Arc::default(),
            next_session: 1,
            level: Level::default(),
            install: None,
        }
    }
}

impl Client {
    /// The updates the app subscribes to. There is one queue for the life
    /// of the client.
    pub fn events(&self) -> async_channel::Receiver<Update> {
        self.receiver.clone()
    }

    /// The level of the session listening now.
    pub fn level(&self) -> Level {
        self.level.clone()
    }

    /// Whether `generation` is the helper running now.
    pub fn is_current(&self, generation: u64) -> bool {
        self.helper
            .as_ref()
            .is_some_and(|helper| helper.generation == generation)
    }

    /// Starts a session with `model`, starting the helper first if it is not
    /// running. A session already running in it is stopped or cancelled by
    /// the helper, as its protocol says.
    pub fn start(&mut self, model: &str) -> Result<SessionId, String> {
        let session = self.next_session;
        self.next_session += 1;
        self.level.set(0.0);
        self.send(Command::Start {
            session,
            model: model.to_owned(),
        })?;
        Ok(session)
    }

    /// Stops listening; the last text and then `Stopped` follow.
    pub fn stop(&mut self, session: SessionId) {
        if let Some(helper) = &self.helper {
            let _ = helper.commands.send(Command::Stop { session });
        }
    }

    /// Stops listening and drops what was not transcribed yet.
    pub fn cancel(&mut self, session: SessionId) {
        if let Some(helper) = &self.helper {
            let _ = helper.commands.send(Command::Cancel { session });
        }
    }

    /// Lets the helper exit, unloading its model. It finishes what it was
    /// asked first.
    pub fn release(&mut self) {
        self.helper = None;
    }

    /// Stops a helper that no longer answers, without waiting for it to
    /// finish anything. Its exit is not reported: it is no longer current.
    pub fn abandon(&mut self) {
        if let Some(helper) = self.helper.take() {
            let _ = helper.kill.send(());
        }
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        if let Some(helper) = &self.helper
            && helper.commands.send(command.clone()).is_ok()
        {
            return Ok(());
        }
        let helper = self.spawn()?;
        helper
            .commands
            .send(command)
            .map_err(|_| "The dictation helper stopped as it started.".to_owned())?;
        self.helper = Some(helper);
        Ok(())
    }

    fn spawn(&self) -> Result<Helper, String> {
        let mut child = command()?
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Dictation could not start: {error}"))?;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed) + 1;
        let (commands, mut queued) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let sender = self.sender.clone();
        let level = self.level.clone();
        let (kill, killed) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            // Commands go out in order until the app lets the helper go;
            // closing its input then lets it finish and exit.
            let writer = tokio::spawn(async move {
                while let Some(command) = queued.recv().await {
                    let line = format!("{}\n", command.to_line());
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
            // Releasing the helper drops `kill` unsent, and it goes on to
            // finish, as long as it does so in [`RELEASE_PATIENCE`]: one that
            // no longer reads its input would otherwise hold its model until
            // the app exits. Abandoning it stops it at once.
            let read = std::pin::pin!(read_events(generation, stdout, &sender, &level));
            let problem = match cosmic::iced::futures::future::select(read, killed).await {
                cosmic::iced::futures::future::Either::Left((problem, _)) => problem,
                cosmic::iced::futures::future::Either::Right((Ok(()), _)) => {
                    let _ = child.kill().await;
                    None
                }
                cosmic::iced::futures::future::Either::Right((Err(_), read)) => {
                    match tokio::time::timeout(RELEASE_PATIENCE, read).await {
                        Ok(problem) => problem,
                        Err(_) => {
                            let _ = child.kill().await;
                            None
                        }
                    }
                }
            };
            // Its output has ended, or it is one this app cannot read: its
            // input closes too, so that it is not left waiting for commands.
            writer.abort();
            // A helper that does not exit once its input closes is given a
            // moment, then stopped.
            let status =
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            if status.is_err() {
                let _ = child.kill().await;
            }
            let problem = problem.or_else(|| match status {
                Ok(Ok(status)) if status.success() => None,
                Ok(Ok(status)) => Some(format!("Dictation stopped unexpectedly ({status}).")),
                Ok(Err(error)) => Some(format!("Dictation stopped unexpectedly: {error}")),
                Err(_) => None,
            });
            let _ = sender
                .send(Update::Exited {
                    generation,
                    problem,
                })
                .await;
        });
        Ok(Helper {
            generation,
            commands,
            kill,
        })
    }

    /// The model being downloaded, if one is.
    pub fn installing(&self) -> Option<&'static str> {
        self.install.as_ref().map(|(model, _)| *model)
    }

    /// Downloads `model`. One download runs at a time.
    pub fn install(&mut self, model: &'static str) -> Result<(), String> {
        if self.installing().is_some() {
            return Err("A speech model is already downloading.".to_owned());
        }
        let mut child = command()?
            .args(["install", model, "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("The download could not start: {error}"))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let sender = self.sender.clone();
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut ended = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(event) = serde_json::from_str::<InstallEvent>(&line) else {
                    continue;
                };
                ended |= !matches!(event, InstallEvent::Progress { .. });
                let _ = sender.send(Update::Install { model, event }).await;
            }
            let _ = child.wait().await;
            if !ended {
                let event = InstallEvent::Failed {
                    kind: InstallError::Storage,
                    message: "The download stopped unexpectedly.".to_owned(),
                };
                let _ = sender.send(Update::Install { model, event }).await;
            }
        });
        self.install = Some((model, task.abort_handle()));
        Ok(())
    }

    /// The download of `model` has ended.
    pub fn installed(&mut self, model: &str) {
        if self.installing() == Some(model) {
            self.install = None;
        }
    }

    /// Stops the download; what it fetched is kept, and the next download
    /// of the same model resumes from there.
    pub fn cancel_install(&mut self) {
        if let Some((_, task)) = self.install.take() {
            // Dropping the task drops the child, which kills it.
            task.abort();
        }
    }

    /// Deletes `model`'s files.
    pub fn remove(&self, model: &'static str) -> Result<(), String> {
        let mut command = command()?;
        command
            .args(["remove", model])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let result = match command.output().await {
                Ok(output) if output.status.success() => Ok(()),
                Ok(output) => Err(String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .trim_start_matches("cagents-dictate: ")
                    .to_owned()),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(Update::Removed { model, result }).await;
        });
        Ok(())
    }
}

/// Whether this copy of cagents has the helper beside it. Packages that do
/// not ship it yet offer no dictation rather than a control that fails.
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Product::Dictate
            .beside_current()
            .is_ok_and(|program| program.exists())
    })
}

/// The helper beside this executable, never one found elsewhere.
fn command() -> Result<tokio::process::Command, String> {
    let program = Product::Dictate
        .beside_current()
        .map_err(|error| format!("Dictation is not available: {error}"))?;
    if !program.exists() {
        return Err("Dictation is not installed with this copy of cagents.".to_owned());
    }
    let mut command = tokio::process::Command::new(program);
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}

/// Relays the helper's events until it closes its output; why it ended,
/// when that was a problem.
async fn read_events(
    generation: u64,
    stdout: tokio::process::ChildStdout,
    sender: &async_channel::Sender<Update>,
    level: &Level,
) -> Option<String> {
    let mut lines = BufReader::new(stdout).lines();
    let mut greeted = false;
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            Err(error) => return Some(format!("Dictation stopped unexpectedly: {error}")),
        };
        let event = match serde_json::from_str::<Event>(&line) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, line, "unreadable line from the dictation helper");
                continue;
            }
        };
        match event {
            Event::Hello { protocol, .. } if protocol != PROTOCOL => {
                // Another build's helper: stop rather than misread it.
                return Some(
                    "Dictation is from a different version of cagents. Restart cagents to use it."
                        .to_owned(),
                );
            }
            Event::Hello { .. } => greeted = true,
            Event::Level { level: value, .. } => level.set(value),
            event if greeted => {
                if let Event::Error {
                    session: None,
                    kind: ErrorKind::Protocol,
                    message,
                } = &event
                {
                    tracing::warn!(message, "the dictation helper refused a command");
                }
                if sender
                    .send(Update::Helper { generation, event })
                    .await
                    .is_err()
                {
                    return None;
                }
            }
            _ => {}
        }
    }
}
