//! A read-only phone view of a terminal agent's current screen.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use cagents_mobile_api as api;
use crossbeam_channel::{Receiver, Sender, TryRecvError, after, bounded, never, select_biased};
use unicode_width::UnicodeWidthStr;

use crate::{
    agent::Provider,
    control::ControlClient,
    model::{HostSessionProof, TerminalId},
    terminal::{HostedTerminalHandle, RenderSnapshot, TerminalEvent},
};

use super::{Outbox, PendingScreen, WatchSocket, translate};

const OPEN_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_ROWS: usize = 128;
const MAX_ROW_BYTES: usize = 2048;
const MAX_SCREEN_BYTES: usize = 32 * 1024;

pub(super) struct Watch {
    pub session: TerminalId,
    // Dropping the sender wakes the watch even when the terminal is quiet.
    _stop: Sender<()>,
    lookup: WatchSocket,
    publisher: Publisher,
}

impl Watch {
    pub fn start(
        id: u64,
        session: TerminalId,
        socket: PathBuf,
        proof: HostSessionProof,
        outbox: Outbox,
    ) -> std::io::Result<Self> {
        let (stop, stopped) = bounded(0);
        let lookup = WatchSocket::default();
        let worker_lookup = lookup.clone();
        let publisher = Publisher {
            session,
            state: Arc::new(Mutex::new(Some(State::Opening(id)))),
            pending: PendingScreen::default(),
            outbox,
        };
        let worker = publisher.clone();
        thread::Builder::new()
            .name("mobile-terminal".into())
            .spawn(move || {
                let provider = find_provider(&socket, &proof, session, &worker_lookup);
                worker_lookup.stop();
                let ending = match provider {
                    Ok(provider) => follow(&worker, stopped, provider, socket, proof),
                    Err(error) => Some(error),
                };
                if let Some(ending) = ending {
                    worker.finish(ending);
                }
            })?;
        Ok(Self {
            session,
            _stop: stop,
            lookup,
            publisher,
        })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // Fence any publisher that is still processing a wake, and answer an
        // open that was superseded before its first frame.
        let previous = self.publisher.state.lock().unwrap().take();
        self.lookup.stop();
        if let Some(State::Opening(id)) = previous {
            self.publisher.outbox.error(
                Some(id),
                api::Error {
                    code: api::ErrorCode::Unavailable,
                    message: "The terminal was closed on the phone before it opened.".into(),
                },
            );
        }
    }
}

enum State {
    Opening(u64),
    Open,
}

#[derive(Clone)]
struct Publisher {
    session: TerminalId,
    state: Arc<Mutex<Option<State>>>,
    pending: PendingScreen,
    outbox: Outbox,
}

impl Publisher {
    fn screen(&self, screen: api::TerminalScreen) {
        let mut state = self.state.lock().unwrap();
        match *state {
            None => return,
            Some(State::Opening(id)) => {
                self.outbox.ok(id);
                *state = Some(State::Open);
            }
            Some(State::Open) => {}
        }
        self.outbox.terminal_screen(&self.pending, screen);
    }

    fn finish(&self, error: api::Error) {
        // Order a last event against an unwatch or a watch that replaces us.
        let mut state = self.state.lock().unwrap();
        match state.take() {
            None => {}
            Some(State::Opening(id)) => self.outbox.error(Some(id), error),
            Some(State::Open) => self
                .outbox
                .event(api::Event::TerminalEnded(api::TerminalEnded {
                    session: self.session.to_string(),
                    message: error.message,
                })),
        }
    }
}

fn unavailable(message: impl Into<String>) -> api::Error {
    api::Error {
        code: api::ErrorCode::Unavailable,
        message: message.into(),
    }
}

fn exited(exit_code: u32, signal: Option<&str>) -> api::Error {
    api::Error {
        code: api::ErrorCode::NotWatchable,
        message: if let Some(signal) = signal {
            format!("The terminal exited after receiving {signal}.")
        } else {
            format!("The terminal exited with status {exit_code}.")
        },
    }
}

fn find_provider(
    socket: &Path,
    proof: &HostSessionProof,
    session: TerminalId,
    lookup: &WatchSocket,
) -> Result<Provider, api::Error> {
    let failed = |error| unavailable(format!("Could not list terminal agents: {error}"));
    let mut control =
        ControlClient::connect_observing_socket(socket, |stream| lookup.register(stream))
            .map_err(failed)?;
    if control.proof() != proof {
        return Err(unavailable("The computer's cagents daemon was replaced."));
    }
    control
        .mobile_sessions()
        .map_err(failed)?
        .iter()
        .find(|member| member.session.id == session)
        .and_then(translate::terminal_provider)
        .ok_or_else(|| api::Error {
            code: api::ErrorCode::NotWatchable,
            message: "Only a listed, running terminal agent can be followed.".into(),
        })
}

fn follow(
    publisher: &Publisher,
    stopped: Receiver<()>,
    provider: Provider,
    socket: PathBuf,
    proof: HostSessionProof,
) -> Option<api::Error> {
    if matches!(stopped.try_recv(), Err(TryRecvError::Disconnected)) {
        return None;
    }
    let (wake, updates) = bounded(1);
    let handle =
        match HostedTerminalHandle::observe_at(socket, publisher.session, proof, move |_| {
            let _ = wake.try_send(());
        }) {
            Ok(handle) => handle,
            Err(error) => {
                return Some(unavailable(format!(
                    "Could not follow the terminal: {error}"
                )));
            }
        };
    let mut deadline = after(OPEN_TIMEOUT);
    let mut previous = None;
    let mut last_generation = None;
    loop {
        select_biased! {
            recv(stopped) -> _ => return None,
            recv(deadline) -> _ => return Some(unavailable("The computer did not open the terminal in time.")),
            recv(updates) -> update => {
                if update.is_err() {
                    return Some(unavailable("The connection to the terminal ended."));
                }
            }
        }

        // The renderer also wakes for semantic events. Drain those so the
        // ordinary terminal client can keep receiving grid updates.
        for event in handle.events().try_iter() {
            match event {
                TerminalEvent::Exited {
                    exit_code, signal, ..
                }
                | TerminalEvent::Completed {
                    exit_code, signal, ..
                } => {
                    return Some(exited(exit_code, signal.as_deref()));
                }
                TerminalEvent::AgentActivityChanged(activity)
                    if activity.is_none_or(|activity| activity.provider != provider) =>
                {
                    return Some(unavailable("The terminal is no longer running this agent."));
                }
                _ => {}
            }
        }
        match handle.current_attachment() {
            Ok(Some(_)) => {}
            Ok(None) if previous.is_none() => continue,
            Ok(None) => return Some(unavailable("The terminal connection ended.")),
            Err(message) => return Some(unavailable(message)),
        }

        // The directory's provider can disappear between listing and attach,
        // or a provider launched inside a shell can return to its prompt.
        let frame = handle.snapshot();
        if frame.detached || frame.generation == 0 {
            continue;
        }
        let Some(activity) = handle
            .agent_activity()
            .filter(|activity| activity.provider == provider)
        else {
            return Some(unavailable("The terminal is no longer running this agent."));
        };
        let activity = translate::activity(activity.state);
        let last_output_at = handle
            .activity()
            .and_then(|output| output.last_output_at_unix_ms.try_into().ok());
        if last_generation == Some(frame.generation)
            && previous
                .as_ref()
                .is_some_and(|screen: &api::TerminalScreen| {
                    screen.activity == activity && screen.last_output_at == last_output_at
                })
        {
            continue;
        }
        let screen = screen(publisher.session, &frame, activity, last_output_at);
        if last_generation.replace(frame.generation) != Some(frame.generation) {
            handle.acknowledge_redraw(frame.generation);
        }
        if previous.as_ref() != Some(&screen) {
            publisher.screen(screen.clone());
            previous = Some(screen);
            deadline = never();
        }
    }
}

fn screen(
    session: TerminalId,
    frame: &RenderSnapshot,
    activity: api::Activity,
    last_output_at: Option<u64>,
) -> api::TerminalScreen {
    let mut lines = Vec::with_capacity(frame.lines.len().min(MAX_ROWS));
    let mut remaining = MAX_SCREEN_BYTES;
    let mut clipped = frame.lines.len() > MAX_ROWS;
    for row in frame.lines.iter().rev().take(MAX_ROWS) {
        let mut line = String::new();
        // Work on cell text only: no terminal sequences, hyperlinks or styles
        // cross the bridge. Cap at a character boundary before allocating.
        let limit = remaining.min(MAX_ROW_BYTES);
        let mut covered_until = 0;
        'cells: for (column, cell) in row.cells.iter().enumerate() {
            if cell.text.is_empty() && column < covered_until {
                // The leading cell already accounted for this wide glyph.
                continue;
            }
            let text = if cell.style.invisible || cell.text.is_empty() {
                " "
            } else {
                cell.text.as_str()
            };
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                if line.len() + ch.len_utf8() > limit {
                    clipped = true;
                    break 'cells;
                }
                line.push(ch);
            }
            covered_until = column + text.width().max(1);
        }
        line.truncate(line.trim_end().len());
        remaining -= line.len();
        lines.push(line);
        if remaining == 0 {
            clipped |= lines.len() < frame.lines.len();
            break;
        }
    }
    lines.reverse();
    api::TerminalScreen {
        session: session.to_string(),
        activity,
        last_output_at,
        columns: frame.columns,
        rows: frame.rows,
        lines,
        clipped,
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
#[path = "terminal_tests.rs"]
mod tests;
