use std::{
    cell::RefCell,
    io::{BufWriter, Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use libghostty_vt::{
    Terminal, TerminalOptions, focus,
    key::{self, Encoder as KeyEncoder},
    mouse::{self, Encoder as MouseEncoder},
    terminal::{Mode, ScrollViewport},
};
use portable_pty::{
    ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    model::{LaunchRecipe, TerminalSession},
    persistence::open_private_append_file,
};

use super::snapshot::{RenderSnapshot, VisualResources};

const DEFAULT_COMMAND_CAPACITY: usize = 256;
const DEFAULT_PTY_CAPACITY: usize = 64;
const DEFAULT_TRANSCRIPT_CAPACITY: usize = 64;
const PTY_READ_SIZE: usize = 16 * 1024;
const PTY_BATCH_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub max_scrollback: usize,
    pub initially_visible: bool,
    pub command_capacity: usize,
    pub pty_capacity: usize,
    pub transcript_capacity: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
            cell_width_px: 9,
            cell_height_px: 18,
            max_scrollback: 10_000,
            initially_visible: true,
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            pty_capacity: DEFAULT_PTY_CAPACITY,
            transcript_capacity: DEFAULT_TRANSCRIPT_CAPACITY,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalWake {
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    TitleChanged(String),
    Bell,
    Exited {
        exit_code: u32,
        signal: Option<String>,
    },
    Error {
        operation: &'static str,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
    pub caps_lock: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalKey {
    Unidentified,
    Character(char),
    Backspace,
    Enter,
    Tab,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Function(u8),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInput {
    pub action: KeyAction,
    pub key: TerminalKey,
    pub modifiers: KeyModifiers,
    pub text: Option<String>,
    pub unshifted_codepoint: Option<char>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub modifiers: KeyModifiers,
    pub x: f32,
    pub y: f32,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub any_button_pressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

#[derive(Debug, Error)]
pub enum TerminalStartError {
    #[error("terminal worker failed to start: {0}")]
    Worker(String),
    #[error("terminal worker exited before startup completed")]
    WorkerExited,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum TerminalSendError {
    #[error("terminal command queue is full")]
    Full,
    #[error("terminal worker has closed")]
    Closed,
}

#[derive(Debug)]
enum Command {
    Input(Vec<u8>),
    Key(KeyInput),
    Mouse(MouseInput),
    Focus(bool),
    Resize(TerminalSize),
    Scroll(ScrollCommand),
    Hide,
    Reveal,
    Hibernate,
    Restore { visible: bool },
    Shutdown,
}

#[derive(Clone, Copy, Debug)]
enum ScrollCommand {
    Top,
    Bottom,
    Delta(isize),
}

#[derive(Debug)]
enum PtyMessage {
    Bytes(Vec<u8>),
    Eof,
    Error(String),
}

type ChildExit = Result<ExitStatus, String>;

#[derive(Debug)]
enum WorkerEvent {
    Command(Option<Command>),
    Pty(Option<PtyMessage>),
    ChildExit(Option<ChildExit>),
}

#[derive(Debug)]
enum TranscriptMessage {
    Bytes(Vec<u8>),
    Shutdown,
}

#[derive(Debug)]
enum Effect {
    PtyWrite(Vec<u8>),
    Bell,
    Title(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Visible,
    Hidden,
    Hibernated,
}

type WakeCallback = dyn Fn(TerminalWake) + Send + Sync + 'static;

struct PublishedState {
    snapshot: ArcSwap<RenderSnapshot>,
    published_generation: AtomicU64,
    wake_pending: AtomicBool,
    visible: AtomicBool,
    wake: Arc<WakeCallback>,
}

impl PublishedState {
    fn new(visible: bool, wake: Arc<WakeCallback>) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(RenderSnapshot::empty()),
            published_generation: AtomicU64::new(0),
            wake_pending: AtomicBool::new(false),
            visible: AtomicBool::new(visible),
            wake,
        }
    }

    fn publish(&self, snapshot: Arc<RenderSnapshot>) {
        let generation = snapshot.generation;
        // The immutable snapshot must be observable before the edge is emitted.
        self.snapshot.store(snapshot);
        self.published_generation
            .store(generation, Ordering::Release);
        self.request_wake(generation);
    }

    fn detach(&self, generation: u64) {
        self.snapshot
            .store(Arc::new(RenderSnapshot::detached(generation)));
        self.published_generation
            .store(generation, Ordering::Release);
    }

    fn request_wake(&self, generation: u64) {
        if !self.visible.load(Ordering::Acquire) {
            return;
        }
        if self
            .wake_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if self.visible.load(Ordering::Acquire) {
                (self.wake)(TerminalWake { generation });
            } else {
                self.wake_pending.store(false, Ordering::Release);
            }
        }
    }

    fn suppress_wakes(&self) {
        self.visible.store(false, Ordering::Release);
        self.wake_pending.store(false, Ordering::Release);
    }

    fn acknowledge(&self, observed_generation: u64) {
        self.wake_pending.store(false, Ordering::Release);
        let latest = self.published_generation.load(Ordering::Acquire);
        if latest > observed_generation && self.visible.load(Ordering::Acquire) {
            self.request_wake(latest);
        }
    }
}

struct HandleInner {
    commands: Sender<Command>,
    events: Receiver<TerminalEvent>,
    published: Arc<PublishedState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let on_worker_thread = self
            .worker
            .get_mut()
            .ok()
            .and_then(|worker| worker.as_ref())
            .is_some_and(|worker| worker.thread().id() == thread::current().id());
        if on_worker_thread {
            let _ = self.commands.try_send(Command::Shutdown);
        } else {
            let _ = self.commands.send(Command::Shutdown);
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
pub struct TerminalHandle {
    inner: Arc<HandleInner>,
}

impl std::fmt::Debug for TerminalHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalHandle")
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl TerminalHandle {
    pub fn spawn(
        session: TerminalSession,
        config: TerminalConfig,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, TerminalStartError> {
        let (command_tx, command_rx) = command_channel(config.command_capacity);
        // Semantic events are lossless and independent from visual wakes. They
        // are low-volume metadata; an unbounded queue avoids either dropping a
        // terminal exit/error or deadlocking teardown behind the UI receiver.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(
            config.initially_visible,
            Arc::new(wake),
        ));
        let worker_published = Arc::clone(&published);
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);

        let worker = thread::Builder::new()
            .name(format!("terminal-{:?}", session.id))
            .spawn(move || {
                let result = Worker::start(session, config, command_rx, event_tx, worker_published);
                match result {
                    Ok(worker) => {
                        let _ = startup_tx.send(Ok(()));
                        worker.run();
                    }
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                    }
                }
            })
            .map_err(|error| TerminalStartError::Worker(error.to_string()))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(HandleInner {
                    commands: command_tx,
                    events: event_rx,
                    published,
                    worker: Mutex::new(Some(worker)),
                }),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(TerminalStartError::Worker(error))
            }
            Err(_) => {
                let _ = worker.join();
                Err(TerminalStartError::WorkerExited)
            }
        }
    }

    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        self.inner.published.snapshot.load_full()
    }

    pub fn generation(&self) -> u64 {
        self.inner
            .published
            .published_generation
            .load(Ordering::Acquire)
    }

    /// Acknowledges a delivered redraw edge after observing its snapshot.
    /// Call this even if the card became hidden before the edge was handled;
    /// [`TerminalWidget`](super::TerminalWidget) calls it after drawing.
    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        self.inner.published.acknowledge(observed_generation);
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.inner.events.clone()
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        self.try_command(Command::Input(bytes.into()))
    }

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
        self.try_command(Command::Key(key))
    }

    pub fn mouse(&self, mouse: MouseInput) -> Result<(), TerminalSendError> {
        self.try_command(Command::Mouse(mouse))
    }

    pub fn focus(&self, focused: bool) -> Result<(), TerminalSendError> {
        self.try_command(Command::Focus(focused))
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalSendError> {
        self.try_command(Command::Resize(size))
    }

    pub fn scroll_lines(&self, delta: isize) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Delta(delta)))
    }

    pub fn scroll_to_top(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Top))
    }

    pub fn scroll_to_bottom(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Bottom))
    }

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Hide)
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Reveal)
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Hibernate)
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        self.try_command(Command::Restore { visible })
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        self.send_command(Command::Shutdown)?;
        if let Ok(mut worker) = self.inner.worker.lock()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
        Ok(())
    }

    fn try_command(&self, command: Command) -> Result<(), TerminalSendError> {
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => TerminalSendError::Full,
                TrySendError::Disconnected(_) => TerminalSendError::Closed,
            })
    }

    fn send_command(&self, command: Command) -> Result<(), TerminalSendError> {
        self.inner
            .commands
            .send(command)
            .map_err(|_| TerminalSendError::Closed)
    }
}

fn command_channel(capacity: usize) -> (Sender<Command>, Receiver<Command>) {
    crossbeam_channel::bounded(capacity.max(1))
}

struct Worker {
    terminal: Terminal<'static, 'static>,
    key_encoder: KeyEncoder<'static>,
    mouse_encoder: MouseEncoder<'static>,
    effects: Rc<RefCell<Vec<Effect>>>,
    visual: Option<VisualResources>,
    visibility: Visibility,
    generation: u64,
    current_size: TerminalSize,
    commands: Receiver<Command>,
    pty_rx: Receiver<PtyMessage>,
    events: Sender<TerminalEvent>,
    transcript: Option<TranscriptWriter>,
    published: Arc<PublishedState>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child_killer: Box<dyn ChildKiller + Send + Sync>,
    child_exit_rx: Receiver<ChildExit>,
    child_reaper: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(
        session: TerminalSession,
        config: TerminalConfig,
        commands: Receiver<Command>,
        events: Sender<TerminalEvent>,
        published: Arc<PublishedState>,
    ) -> Result<Self, String> {
        let current_size = TerminalSize {
            columns: config.columns.max(1),
            rows: config.rows.max(1),
            cell_width_px: config.cell_width_px.max(1),
            cell_height_px: config.cell_height_px.max(1),
        };
        let mut terminal = Terminal::new(TerminalOptions {
            cols: current_size.columns,
            rows: current_size.rows,
            max_scrollback: config.max_scrollback,
        })
        .map_err(|error| format!("create Ghostty terminal: {error}"))?;

        let effects = Rc::new(RefCell::new(Vec::new()));
        terminal
            .on_pty_write({
                let effects = Rc::clone(&effects);
                move |_terminal, bytes| {
                    effects.borrow_mut().push(Effect::PtyWrite(bytes.to_vec()));
                }
            })
            .map_err(|error| format!("register PTY response effect: {error}"))?;
        terminal
            .on_bell({
                let effects = Rc::clone(&effects);
                move |_terminal| effects.borrow_mut().push(Effect::Bell)
            })
            .map_err(|error| format!("register bell effect: {error}"))?;
        terminal
            .on_title_changed({
                let effects = Rc::clone(&effects);
                move |terminal| {
                    let title = terminal.title().unwrap_or_default().to_owned();
                    effects.borrow_mut().push(Effect::Title(title));
                }
            })
            .map_err(|error| format!("register title effect: {error}"))?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(pty_size(current_size))
            .map_err(|error| format!("open PTY: {error:#}"))?;
        let command = command_from_recipe(&session.launch);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("spawn {}: {error:#}", session.launch.program))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("clone PTY reader: {error:#}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("take PTY writer: {error:#}"))?;

        let (pty_tx, pty_rx) = crossbeam_channel::bounded(config.pty_capacity.max(1));
        let reader_thread = thread::Builder::new()
            .name(format!("terminal-pty-reader-{:?}", session.id))
            .spawn(move || read_pty(&mut reader, &pty_tx))
            .map_err(|error| format!("start PTY reader: {error}"))?;

        let transcript = session
            .transcript_path
            .map(|path| TranscriptWriter::start(path, config.transcript_capacity.max(1)))
            .transpose()?;
        let visibility = if config.initially_visible {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        let visual = if visibility == Visibility::Visible {
            Some(VisualResources::new().map_err(|error| format!("create render state: {error}"))?)
        } else {
            None
        };
        let key_encoder =
            KeyEncoder::new().map_err(|error| format!("create key encoder: {error}"))?;
        let mouse_encoder =
            MouseEncoder::new().map_err(|error| format!("create mouse encoder: {error}"))?;

        let child_killer = child.clone_killer();
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
        let child_reaper = thread::Builder::new()
            .name(format!("terminal-child-reaper-{:?}", session.id))
            .spawn(move || {
                let result = child.wait().map_err(|error| error.to_string());
                let _ = child_exit_tx.send(result);
            })
            .map_err(|error| format!("start child reaper: {error}"))?;

        Ok(Self {
            terminal,
            key_encoder,
            mouse_encoder,
            effects,
            visual,
            visibility,
            generation: 0,
            current_size,
            commands,
            pty_rx,
            events,
            transcript,
            published,
            master: pair.master,
            writer,
            child_killer,
            child_exit_rx,
            child_reaper: Some(child_reaper),
            reader: Some(reader_thread),
        })
    }

    fn run(mut self) {
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }

        let mut pty_open = true;
        let mut exited = false;
        let mut shutting_down = false;
        while !shutting_down && (pty_open || !exited) {
            match wait_for_worker_event(
                &self.commands,
                &self.pty_rx,
                &self.child_exit_rx,
                pty_open,
                !exited,
            ) {
                WorkerEvent::Command(Some(command)) => {
                    shutting_down = self.handle_command(command);
                }
                WorkerEvent::Command(None) => shutting_down = true,
                WorkerEvent::Pty(message) => {
                    pty_open = self.handle_pty_message(message);
                }
                WorkerEvent::ChildExit(message) => {
                    self.handle_child_exit(
                        &mut exited,
                        message.unwrap_or_else(|| Err("child reaper closed".into())),
                    );
                }
            }
        }

        if shutting_down && !exited {
            match self.child_exit_rx.try_recv() {
                Ok(result) => self.handle_child_exit(&mut exited, result),
                Err(TryRecvError::Disconnected) => {
                    self.handle_child_exit(&mut exited, Err("child reaper closed".into()));
                }
                Err(TryRecvError::Empty) => match self.child_killer.kill() {
                    Ok(()) => {
                        let result = self
                            .child_exit_rx
                            .recv()
                            .unwrap_or_else(|_| Err("child reaper closed".into()));
                        self.handle_child_exit(&mut exited, result);
                    }
                    Err(error) => match self.child_exit_rx.try_recv() {
                        Ok(result) => self.handle_child_exit(&mut exited, result),
                        Err(_) => self.error("terminate child", error.to_string()),
                    },
                },
            }
        }
        if exited && let Some(child_reaper) = self.child_reaper.take() {
            let _ = child_reaper.join();
        }
        if let Some(transcript) = self.transcript.take() {
            transcript.finish();
        }

        // Drop the controlling descriptors before waiting for the blocking
        // reader so shutdown reliably releases its read call.
        drop(self.writer);
        drop(self.master);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // Terminal, render state, and every Ghostty encoder drop here, on the
        // same OS thread on which they were constructed and used.
    }

    fn handle_child_exit(&self, exited: &mut bool, result: ChildExit) {
        if let Some(event) = child_exit_event(exited, result) {
            self.emit(event);
        }
    }

    fn handle_pty_message(&mut self, message: Option<PtyMessage>) -> bool {
        match message {
            Some(PtyMessage::Bytes(mut bytes)) => {
                while bytes.len() < PTY_BATCH_LIMIT {
                    match self.pty_rx.try_recv() {
                        Ok(PtyMessage::Bytes(next)) => bytes.extend_from_slice(&next),
                        Ok(PtyMessage::Eof) => {
                            self.process_output(bytes);
                            return false;
                        }
                        Ok(PtyMessage::Error(error)) => {
                            self.error("read PTY", error);
                            self.process_output(bytes);
                            return false;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            self.process_output(bytes);
                            return false;
                        }
                    }
                }
                self.process_output(bytes);
                true
            }
            Some(PtyMessage::Eof) | None => false,
            Some(PtyMessage::Error(error)) => {
                self.error("read PTY", error);
                false
            }
        }
    }

    fn process_output(&mut self, bytes: Vec<u8>) {
        if let Some(transcript) = &self.transcript
            && transcript.send(bytes.clone()).is_err()
        {
            self.error("write transcript", "transcript writer closed".into());
            self.transcript = None;
        }

        self.terminal.vt_write(&bytes);
        self.drain_effects();
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(false);
        }
    }

    /// Returns true when shutdown was requested.
    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Input(bytes) => self.write_pty(&bytes, "write terminal input"),
            Command::Key(input) => self.encode_key(input),
            Command::Mouse(input) => self.encode_mouse(input),
            Command::Focus(focused) => {
                match self.terminal.mode(Mode::FOCUS_EVENT) {
                    Ok(true) => {}
                    Ok(false) => return false,
                    Err(error) => {
                        self.error("read focus-reporting mode", error.to_string());
                        return false;
                    }
                }
                let mut buffer = [0u8; 8];
                let event = if focused {
                    focus::Event::Gained
                } else {
                    focus::Event::Lost
                };
                match event.encode(&mut buffer) {
                    Ok(written) => self.write_pty(&buffer[..written], "write focus report"),
                    Err(error) => self.error("encode focus report", error.to_string()),
                }
            }
            Command::Resize(size) => self.resize(size),
            Command::Scroll(scroll) => {
                self.terminal.scroll_viewport(match scroll {
                    ScrollCommand::Top => ScrollViewport::Top,
                    ScrollCommand::Bottom => ScrollViewport::Bottom,
                    ScrollCommand::Delta(delta) => ScrollViewport::Delta(delta),
                });
                if self.visibility == Visibility::Visible {
                    self.publish_snapshot(false);
                }
            }
            Command::Hide => self.set_hidden(),
            Command::Reveal => self.reveal(),
            Command::Hibernate => self.hibernate_visuals(),
            Command::Restore { visible } => self.restore_visuals(visible),
            Command::Shutdown => return true,
        }
        false
    }

    fn encode_key(&mut self, input: KeyInput) {
        match encode_key_input(&self.terminal, &mut self.key_encoder, input) {
            Ok(encoded) => self.write_pty(&encoded, "write encoded key"),
            Err(error) => self.error("encode key", error.to_string()),
        }
    }

    fn encode_mouse(&mut self, input: MouseInput) {
        let mut event = match mouse::Event::new() {
            Ok(event) => event,
            Err(error) => {
                self.error("create mouse event", error.to_string());
                return;
            }
        };
        event
            .set_action(map_mouse_action(input.action))
            .set_button(input.button.map(map_mouse_button))
            .set_mods(map_modifiers(input.modifiers))
            .set_position(mouse::Position {
                x: input.x,
                y: input.y,
            });
        self.mouse_encoder
            .set_options_from_terminal(&self.terminal)
            .set_any_button_pressed(input.any_button_pressed)
            .set_size(mouse::EncoderSize {
                screen_width: input.screen_width.max(1),
                screen_height: input.screen_height.max(1),
                cell_width: input.cell_width.max(1),
                cell_height: input.cell_height.max(1),
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            });
        let mut encoded = Vec::with_capacity(32);
        match self.mouse_encoder.encode_to_vec(&event, &mut encoded) {
            Ok(()) => self.write_pty(&encoded, "write encoded mouse event"),
            Err(error) => self.error("encode mouse event", error.to_string()),
        }
    }

    fn resize(&mut self, mut size: TerminalSize) {
        size.columns = size.columns.max(1);
        size.rows = size.rows.max(1);
        size.cell_width_px = size.cell_width_px.max(1);
        size.cell_height_px = size.cell_height_px.max(1);
        if size == self.current_size {
            return;
        }
        if let Err(error) = self.master.resize(pty_size(size)) {
            self.error("resize PTY", format!("{error:#}"));
        }
        if let Err(error) = self.terminal.resize(
            size.columns,
            size.rows,
            size.cell_width_px,
            size.cell_height_px,
        ) {
            self.error("resize terminal", error.to_string());
        }
        self.current_size = size;
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }
    }

    fn set_hidden(&mut self) {
        self.visibility = Visibility::Hidden;
        self.published.suppress_wakes();
    }

    fn reveal(&mut self) {
        if self.visual.is_none() {
            match VisualResources::new() {
                Ok(visual) => self.visual = Some(visual),
                Err(error) => {
                    self.error("restore render state", error.to_string());
                    return;
                }
            }
        }
        self.visibility = Visibility::Visible;
        self.published.visible.store(true, Ordering::Release);
        self.publish_snapshot(true);
    }

    fn hibernate_visuals(&mut self) {
        self.visibility = Visibility::Hibernated;
        self.published.suppress_wakes();
        self.visual = None;
        self.generation = self.generation.saturating_add(1);
        self.published.detach(self.generation);
    }

    fn restore_visuals(&mut self, visible: bool) {
        if self.visual.is_none() {
            match VisualResources::new() {
                Ok(visual) => self.visual = Some(visual),
                Err(error) => {
                    self.error("restore render state", error.to_string());
                    return;
                }
            }
        }
        if visible {
            self.visibility = Visibility::Visible;
            self.published.visible.store(true, Ordering::Release);
            self.publish_snapshot(true);
        } else {
            self.visibility = Visibility::Hidden;
            self.published.suppress_wakes();
        }
    }

    fn publish_snapshot(&mut self, force: bool) {
        if self.visibility != Visibility::Visible {
            return;
        }
        let Some(visual) = self.visual.as_mut() else {
            return;
        };
        let next_generation = self.generation.saturating_add(1);
        match visual.capture(&self.terminal, next_generation, force) {
            Ok(Some(snapshot)) => {
                self.generation = next_generation;
                self.published.publish(snapshot);
            }
            Ok(None) => {}
            Err(error) => self.error("build terminal snapshot", error.to_string()),
        }
    }

    fn drain_effects(&mut self) {
        let effects = std::mem::take(&mut *self.effects.borrow_mut());
        for effect in effects {
            match effect {
                Effect::PtyWrite(bytes) => self.write_pty(&bytes, "write terminal response"),
                Effect::Bell => self.emit(TerminalEvent::Bell),
                Effect::Title(title) => self.emit(TerminalEvent::TitleChanged(title)),
            }
        }
    }

    fn write_pty(&mut self, bytes: &[u8], operation: &'static str) {
        if bytes.is_empty() {
            return;
        }
        if let Err(error) = self
            .writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush())
        {
            self.error(operation, error.to_string());
        }
    }

    fn emit(&self, event: TerminalEvent) {
        let _ = self.events.send(event);
    }

    fn error(&self, operation: &'static str, message: String) {
        self.emit(TerminalEvent::Error { operation, message });
    }
}

fn wait_for_worker_event(
    commands: &Receiver<Command>,
    pty: &Receiver<PtyMessage>,
    child_exit: &Receiver<ChildExit>,
    pty_open: bool,
    child_running: bool,
) -> WorkerEvent {
    match (pty_open, child_running) {
        (true, true) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(pty) -> message => WorkerEvent::Pty(message.ok()),
            recv(child_exit) -> message => WorkerEvent::ChildExit(message.ok()),
        },
        (true, false) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(pty) -> message => WorkerEvent::Pty(message.ok()),
        },
        (false, true) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(child_exit) -> message => WorkerEvent::ChildExit(message.ok()),
        },
        (false, false) => WorkerEvent::Command(commands.recv().ok()),
    }
}

fn child_exit_event(exited: &mut bool, result: ChildExit) -> Option<TerminalEvent> {
    if *exited {
        return None;
    }
    *exited = true;
    Some(match result {
        Ok(status) => TerminalEvent::Exited {
            exit_code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
        },
        Err(message) => TerminalEvent::Error {
            operation: "wait for child",
            message,
        },
    })
}

/// Variables the daemon may have inherited from whatever terminal (or desktop
/// session) launched it. Left in place, a child would believe it is talking to
/// that emulator and emit control sequences libghostty-vt does not implement.
///
/// Some entries are worse than a wrong identity, because they name a live
/// control channel into something of the user's that a card is not inside:
/// `KITTY_LISTEN_ON` fully controls their real kitty instance, `TMUX` and
/// `STY` and `ZELLIJ` reach every pane of their multiplexer session, the
/// `KONSOLE_DBUS_*` triple drives their real Konsole over the session bus, and
/// `NVIM`, `VSCODE_IPC_HOOK_CLI` and `EMACS_SERVER_FILE` each run commands in
/// their editor. A child that inherited any of them could act outside its own
/// card - the exact containment this scrub exists to establish. `LC_TERMINAL`
/// and `LC_TERMINAL_VERSION` match sshd's default `AcceptEnv LC_*`, so they
/// carry the wrong identity onto every host the child then reaches.
///
/// Every emulator here contributes both halves where it has them, identity and
/// channel, because a channel is only reachable once and an identity is only
/// misleading once.
///
/// Grouped by the emulator that sets them, with the names any of them may set
/// first and `COLORTERM` - which we go on to set ourselves - last.
const INHERITED_TERMINAL_MARKERS: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "TERMINAL_EMULATOR",
    "TERMINFO",
    "TERMINFO_DIRS",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_INSTALLATION_DIR",
    "KITTY_LISTEN_ON",
    "KITTY_PUBLIC_KEY",
    "KITTY_SHELL_INTEGRATION",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_BIN_DIR",
    "GHOSTTY_SHELL_FEATURES",
    "GHOSTTY_SHELL_INTEGRATION_NO_SUDO",
    "VTE_VERSION",
    "ALACRITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "ALACRITTY_LOG",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_EXECUTABLE_DIR",
    "WEZTERM_CONFIG_DIR",
    "WEZTERM_CONFIG_FILE",
    "ITERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "KONSOLE_VERSION",
    "KONSOLE_DBUS_SERVICE",
    "KONSOLE_DBUS_SESSION",
    "KONSOLE_DBUS_WINDOW",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "ZED_TERM",
    "VSCODE_INJECTION",
    "VSCODE_IPC_HOOK_CLI",
    "VSCODE_GIT_IPC_HANDLE",
    "VSCODE_GIT_ASKPASS_NODE",
    "VSCODE_GIT_ASKPASS_MAIN",
    "EMACS_SERVER_FILE",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "COLORTERM",
];

/// Drops every inherited marker from a command's environment.
///
/// Separate from [`command_from_recipe`] so a test can plant all of them and
/// watch them go, rather than observing that a host which never set them does
/// not have them.
fn scrub_inherited_markers(command: &mut CommandBuilder) {
    for name in INHERITED_TERMINAL_MARKERS {
        command.env_remove(name);
    }
}

fn command_from_recipe(recipe: &LaunchRecipe) -> CommandBuilder {
    let mut command = CommandBuilder::new(&recipe.program);
    command.args(&recipe.arguments);
    command.cwd(&recipe.working_directory);
    scrub_inherited_markers(&mut command);
    // `xterm-256color` rather than a cagents-specific entry: we ship no
    // terminfo database, and a name the host lacks breaks every curses program.
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "cagents");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    // Last, so an explicit per-session entry still wins over our defaults.
    for (name, value) in &recipe.environment {
        command.env(name, value);
    }
    command
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.columns,
        pixel_width: u16::try_from(u32::from(size.columns).saturating_mul(size.cell_width_px))
            .unwrap_or(u16::MAX),
        pixel_height: u16::try_from(u32::from(size.rows).saturating_mul(size.cell_height_px))
            .unwrap_or(u16::MAX),
    }
}

fn read_pty(reader: &mut dyn Read, sender: &Sender<PtyMessage>) {
    let mut buffer = vec![0u8; PTY_READ_SIZE];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(PtyMessage::Eof);
                return;
            }
            Ok(read) => {
                if sender
                    .send(PtyMessage::Bytes(buffer[..read].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                let _ = sender.send(PtyMessage::Error(error.to_string()));
                return;
            }
        }
    }
}

struct TranscriptWriter {
    sender: Sender<TranscriptMessage>,
    worker: JoinHandle<()>,
}

impl TranscriptWriter {
    fn start(path: PathBuf, capacity: usize) -> Result<Self, String> {
        let file = open_private_append_file(&path)
            .map_err(|error| format!("open transcript {}: {error}", path.display()))?;
        let (sender, receiver) = crossbeam_channel::bounded(capacity);
        let worker = thread::Builder::new()
            .name("terminal-transcript-writer".into())
            .spawn(move || {
                let mut writer = BufWriter::new(file);
                while let Ok(message) = receiver.recv() {
                    match message {
                        TranscriptMessage::Bytes(bytes) => {
                            if writer.write_all(&bytes).is_err() {
                                break;
                            }
                        }
                        TranscriptMessage::Shutdown => break,
                    }
                }
                let _ = writer.flush();
            })
            .map_err(|error| format!("start transcript writer: {error}"))?;
        Ok(Self { sender, worker })
    }

    fn send(&self, bytes: Vec<u8>) -> Result<(), ()> {
        self.sender
            .send(TranscriptMessage::Bytes(bytes))
            .map_err(|_| ())
    }

    fn finish(self) {
        let _ = self.sender.send(TranscriptMessage::Shutdown);
        drop(self.sender);
        let _ = self.worker.join();
    }
}

fn encode_key_input(
    terminal: &Terminal<'_, '_>,
    encoder: &mut KeyEncoder<'_>,
    input: KeyInput,
) -> Result<Vec<u8>, libghostty_vt::Error> {
    // Legacy encodings have no repeat event type, so a repeated event must be
    // encoded as another press. Keep Repeat only when the child asked for it.
    let report_events = terminal
        .kitty_keyboard_flags()?
        .contains(key::KittyKeyFlags::REPORT_EVENTS);
    let mut event = key::Event::new()?;
    event
        .set_action(map_key_action(input.action, report_events))
        .set_key(map_key(input.key))
        .set_mods(map_modifiers(input.modifiers))
        .set_utf8(input.text);
    if let Some(codepoint) = input.unshifted_codepoint {
        event.set_unshifted_codepoint(codepoint);
    }

    encoder.set_options_from_terminal(terminal);
    let mut encoded = Vec::with_capacity(32);
    encoder.encode_to_vec(&event, &mut encoded)?;
    Ok(encoded)
}

fn map_key_action(action: KeyAction, report_events: bool) -> key::Action {
    match action {
        KeyAction::Press => key::Action::Press,
        KeyAction::Release => key::Action::Release,
        KeyAction::Repeat if report_events => key::Action::Repeat,
        KeyAction::Repeat => key::Action::Press,
    }
}

fn map_key(input: TerminalKey) -> key::Key {
    match input {
        TerminalKey::Unidentified => key::Key::Unidentified,
        TerminalKey::Character(character) => match character.to_ascii_lowercase() {
            'a' => key::Key::A,
            'b' => key::Key::B,
            'c' => key::Key::C,
            'd' => key::Key::D,
            'e' => key::Key::E,
            'f' => key::Key::F,
            'g' => key::Key::G,
            'h' => key::Key::H,
            'i' => key::Key::I,
            'j' => key::Key::J,
            'k' => key::Key::K,
            'l' => key::Key::L,
            'm' => key::Key::M,
            'n' => key::Key::N,
            'o' => key::Key::O,
            'p' => key::Key::P,
            'q' => key::Key::Q,
            'r' => key::Key::R,
            's' => key::Key::S,
            't' => key::Key::T,
            'u' => key::Key::U,
            'v' => key::Key::V,
            'w' => key::Key::W,
            'x' => key::Key::X,
            'y' => key::Key::Y,
            'z' => key::Key::Z,
            '0' => key::Key::Digit0,
            '1' => key::Key::Digit1,
            '2' => key::Key::Digit2,
            '3' => key::Key::Digit3,
            '4' => key::Key::Digit4,
            '5' => key::Key::Digit5,
            '6' => key::Key::Digit6,
            '7' => key::Key::Digit7,
            '8' => key::Key::Digit8,
            '9' => key::Key::Digit9,
            ' ' => key::Key::Space,
            '-' => key::Key::Minus,
            '=' => key::Key::Equal,
            ',' => key::Key::Comma,
            '.' => key::Key::Period,
            '/' => key::Key::Slash,
            ';' => key::Key::Semicolon,
            '\'' => key::Key::Quote,
            '`' => key::Key::Backquote,
            '[' => key::Key::BracketLeft,
            ']' => key::Key::BracketRight,
            '\\' => key::Key::Backslash,
            _ => key::Key::Unidentified,
        },
        TerminalKey::Backspace => key::Key::Backspace,
        TerminalKey::Enter => key::Key::Enter,
        TerminalKey::Tab => key::Key::Tab,
        TerminalKey::Escape => key::Key::Escape,
        TerminalKey::Delete => key::Key::Delete,
        TerminalKey::Insert => key::Key::Insert,
        TerminalKey::Home => key::Key::Home,
        TerminalKey::End => key::Key::End,
        TerminalKey::PageUp => key::Key::PageUp,
        TerminalKey::PageDown => key::Key::PageDown,
        TerminalKey::ArrowUp => key::Key::ArrowUp,
        TerminalKey::ArrowDown => key::Key::ArrowDown,
        TerminalKey::ArrowLeft => key::Key::ArrowLeft,
        TerminalKey::ArrowRight => key::Key::ArrowRight,
        TerminalKey::Function(number) => match number {
            1 => key::Key::F1,
            2 => key::Key::F2,
            3 => key::Key::F3,
            4 => key::Key::F4,
            5 => key::Key::F5,
            6 => key::Key::F6,
            7 => key::Key::F7,
            8 => key::Key::F8,
            9 => key::Key::F9,
            10 => key::Key::F10,
            11 => key::Key::F11,
            12 => key::Key::F12,
            13 => key::Key::F13,
            14 => key::Key::F14,
            15 => key::Key::F15,
            16 => key::Key::F16,
            17 => key::Key::F17,
            18 => key::Key::F18,
            19 => key::Key::F19,
            20 => key::Key::F20,
            21 => key::Key::F21,
            22 => key::Key::F22,
            23 => key::Key::F23,
            24 => key::Key::F24,
            25 => key::Key::F25,
            _ => key::Key::Unidentified,
        },
    }
}

fn map_modifiers(input: KeyModifiers) -> key::Mods {
    let mut output = key::Mods::empty();
    output.set(key::Mods::SHIFT, input.shift);
    output.set(key::Mods::ALT, input.alt);
    output.set(key::Mods::CTRL, input.control);
    output.set(key::Mods::SUPER, input.super_key);
    output.set(key::Mods::CAPS_LOCK, input.caps_lock);
    output
}

fn map_mouse_action(action: MouseAction) -> mouse::Action {
    match action {
        MouseAction::Press => mouse::Action::Press,
        MouseAction::Release => mouse::Action::Release,
        MouseAction::Motion => mouse::Action::Motion,
    }
}

fn map_mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Right => mouse::Button::Right,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Back => mouse::Button::Four,
        MouseButton::Forward => mouse::Button::Five,
        MouseButton::Other(number) => match number {
            4 => mouse::Button::Four,
            5 => mouse::Button::Five,
            6 => mouse::Button::Six,
            7 => mouse::Button::Seven,
            8 => mouse::Button::Eight,
            9 => mouse::Button::Nine,
            10 => mouse::Button::Ten,
            11 => mouse::Button::Eleven,
            _ => mouse::Button::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsStr,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tempfile::tempdir;

    use super::*;

    fn test_published(visible: bool, wakes: Arc<AtomicUsize>) -> PublishedState {
        PublishedState::new(
            visible,
            Arc::new(move |_| {
                wakes.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    fn snapshot(generation: u64) -> Arc<RenderSnapshot> {
        Arc::new(RenderSnapshot {
            generation,
            ..RenderSnapshot::empty()
        })
    }

    fn key_input(
        action: KeyAction,
        key: TerminalKey,
        text: Option<&str>,
        unshifted_codepoint: Option<char>,
    ) -> KeyInput {
        KeyInput {
            action,
            key,
            modifiers: KeyModifiers::default(),
            text: text.map(str::to_owned),
            unshifted_codepoint,
        }
    }

    #[test]
    fn legacy_repeats_encode_once_as_ordinary_key_presses() {
        let terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 1_000,
        })
        .unwrap();
        let mut encoder = KeyEncoder::new().unwrap();
        let cases = [
            (TerminalKey::ArrowDown, None, None, &b"\x1b[B"[..]),
            (TerminalKey::Character('x'), Some("x"), Some('x'), &b"x"[..]),
            (TerminalKey::Backspace, None, None, &b"\x7f"[..]),
        ];

        for (key, text, unshifted_codepoint, expected) in cases {
            let press = encode_key_input(
                &terminal,
                &mut encoder,
                key_input(KeyAction::Press, key, text, unshifted_codepoint),
            )
            .unwrap();
            let repeat = encode_key_input(
                &terminal,
                &mut encoder,
                key_input(KeyAction::Repeat, key, text, unshifted_codepoint),
            )
            .unwrap();

            assert_eq!(press, expected, "unexpected press encoding for {key:?}");
            assert_eq!(repeat, expected, "unexpected repeat encoding for {key:?}");
        }
    }

    #[test]
    fn event_reporting_protocol_preserves_repeat_semantics() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 1_000,
        })
        .unwrap();
        terminal.vt_write(b"\x1b[>2u");
        assert!(
            terminal
                .kitty_keyboard_flags()
                .unwrap()
                .contains(key::KittyKeyFlags::REPORT_EVENTS)
        );
        let mut encoder = KeyEncoder::new().unwrap();

        let press = encode_key_input(
            &terminal,
            &mut encoder,
            key_input(KeyAction::Press, TerminalKey::ArrowDown, None, None),
        )
        .unwrap();
        let repeat = encode_key_input(
            &terminal,
            &mut encoder,
            key_input(KeyAction::Repeat, TerminalKey::ArrowDown, None, None),
        )
        .unwrap();

        assert_eq!(press, b"\x1b[1;1:1B");
        assert_eq!(repeat, b"\x1b[1;1:2B");
    }

    #[test]
    fn wake_coalescing_generation_recheck_closes_ack_race() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(true, Arc::clone(&wakes));

        published.publish(snapshot(1));
        published.publish(snapshot(2));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);

        published.acknowledge(1);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        published.acknowledge(2);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn hidden_publication_suppresses_visual_wakes() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(false, Arc::clone(&wakes));

        published.publish(snapshot(1));

        assert_eq!(published.snapshot.load().generation, 1);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn visibility_reveal_emits_latest_snapshot_once() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(false, Arc::clone(&wakes));
        published.publish(snapshot(4));

        published.visible.store(true, Ordering::Release);
        published.request_wake(4);
        published.request_wake(4);

        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hiding_clears_an_unacknowledged_wake_edge_before_reveal() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(true, Arc::clone(&wakes));
        published.publish(snapshot(1));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);

        published.suppress_wakes();
        published.visible.store(true, Ordering::Release);
        published.request_wake(1);

        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn transcript_writer_privatizes_storage_and_preserves_append_behavior() {
        let root = tempdir().unwrap();
        let directory = root.path().join("transcripts");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let path = directory.join("terminal.log");
        fs::write(&path, b"before\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let writer = TranscriptWriter::start(path.clone(), 1).unwrap();
        writer.send(b"after\n".to_vec()).unwrap();
        writer.finish();

        assert_eq!(fs::read(&path).unwrap(), b"before\nafter\n");
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn transcript_writer_creates_private_storage() {
        let root = tempdir().unwrap();
        let directory = root.path().join("new-transcripts");
        let path = directory.join("terminal.log");

        TranscriptWriter::start(path.clone(), 1).unwrap().finish();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn transcript_writer_rejects_file_and_directory_symlinks() {
        let root = tempdir().unwrap();
        let directory = root.path().join("transcripts");
        fs::create_dir(&directory).unwrap();
        let victim = directory.join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        let linked_file = directory.join("linked.log");
        symlink(&victim, &linked_file).unwrap();

        assert!(TranscriptWriter::start(linked_file, 1).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");

        let real_directory = root.path().join("real");
        fs::create_dir(&real_directory).unwrap();
        let linked_directory = root.path().join("linked-directory");
        symlink(&real_directory, &linked_directory).unwrap();
        assert!(TranscriptWriter::start(linked_directory.join("terminal.log"), 1).is_err());
    }

    #[test]
    fn command_and_pty_channels_are_bounded() {
        let (commands, _receiver) = command_channel(1);
        commands.try_send(Command::Hide).unwrap();
        assert!(matches!(
            commands.try_send(Command::Reveal),
            Err(TrySendError::Full(_))
        ));

        let (pty, _receiver) = crossbeam_channel::bounded(1);
        pty.try_send(PtyMessage::Bytes(vec![1])).unwrap();
        assert!(matches!(
            pty.try_send(PtyMessage::Bytes(vec![2])),
            Err(TrySendError::Full(_))
        ));
    }
    #[test]
    fn hidden_idle_wait_is_driven_by_commands_and_child_exit_channels() {
        let (command_tx, command_rx) = command_channel(1);
        let (_pty_tx, pty_rx) = crossbeam_channel::bounded(1);
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);

        command_tx.send(Command::Hide).unwrap();
        assert!(matches!(
            wait_for_worker_event(&command_rx, &pty_rx, &child_exit_rx, false, true),
            WorkerEvent::Command(Some(Command::Hide))
        ));

        child_exit_tx
            .send(Ok(ExitStatus::with_exit_code(7)))
            .unwrap();
        assert!(matches!(
            wait_for_worker_event(&command_rx, &pty_rx, &child_exit_rx, false, true),
            WorkerEvent::ChildExit(Some(Ok(status))) if status.exit_code() == 7
        ));
    }

    #[test]
    fn child_exit_is_published_exactly_once() {
        let deliveries = AtomicUsize::new(0);
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut exited = false;

        for status in [7, 8] {
            if let Some(event) =
                child_exit_event(&mut exited, Ok(ExitStatus::with_exit_code(status)))
            {
                deliveries.fetch_add(1, Ordering::SeqCst);
                event_tx.send(event).unwrap();
            }
        }

        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TerminalEvent::Exited {
                exit_code: 7,
                signal: None
            })
        ));
        assert!(event_rx.try_recv().is_err());
    }

    fn env_recipe(environment: BTreeMap<String, String>) -> LaunchRecipe {
        LaunchRecipe {
            program: "/bin/sh".to_owned(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/"),
            environment,
        }
    }

    #[test]
    fn child_environment_advertises_our_own_terminal_identity() {
        let command = command_from_recipe(&env_recipe(BTreeMap::new()));

        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
        assert_eq!(command.get_env("TERM_PROGRAM"), Some(OsStr::new("cagents")));
        assert_eq!(
            command.get_env("TERM_PROGRAM_VERSION"),
            Some(OsStr::new(env!("CARGO_PKG_VERSION")))
        );
        // The test below proves the scrub removes names; this one proves
        // `command_from_recipe` still calls it. Only markers this host
        // actually set can witness that, so the assertion is conditional
        // rather than absent: on a host inside kitty or tmux it is real
        // coverage of the call site, and on a bare host it costs nothing.
        for name in INHERITED_TERMINAL_MARKERS {
            if matches!(*name, "TERM_PROGRAM" | "TERM_PROGRAM_VERSION" | "COLORTERM") {
                // Set again, deliberately, after the scrub.
                continue;
            }
            if std::env::var_os(name).is_some() {
                assert_eq!(command.get_env(name), None, "{name} reached the child");
            }
        }
    }

    #[test]
    fn every_inherited_marker_is_dropped_from_the_child_environment() {
        // Planted rather than inherited: `CommandBuilder::new` seeds its map
        // from `std::env::vars_os`, so asserting absence on a fresh command
        // would pass on any host that never set these - which is every host
        // for most of the list, and so would pass with the scrub deleted.
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        for name in INHERITED_TERMINAL_MARKERS {
            command.env(name, "planted");
        }

        scrub_inherited_markers(&mut command);

        for name in INHERITED_TERMINAL_MARKERS {
            assert_eq!(command.get_env(name), None, "{name} survived the scrub");
        }
        // Each control channel named as well, so that shortening the list is a
        // test failure and not merely a smaller loop: these reach the user's
        // real kitty instance, multiplexer session, Konsole and editor.
        for channel in [
            "KITTY_LISTEN_ON",
            "TMUX",
            "TMUX_PANE",
            "STY",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "ZELLIJ_PANE_ID",
            "KONSOLE_DBUS_SERVICE",
            "KONSOLE_DBUS_SESSION",
            "NVIM",
            "NVIM_LISTEN_ADDRESS",
            "VSCODE_IPC_HOOK_CLI",
            "EMACS_SERVER_FILE",
        ] {
            assert!(
                INHERITED_TERMINAL_MARKERS.contains(&channel),
                "{channel} is not scrubbed"
            );
        }
    }

    #[test]
    fn explicit_recipe_environment_overrides_terminal_defaults() {
        let command = command_from_recipe(&env_recipe(BTreeMap::from([
            ("TERM".to_owned(), "dumb".to_owned()),
            ("KITTY_WINDOW_ID".to_owned(), "3".to_owned()),
        ])));

        assert_eq!(command.get_env("TERM"), Some(OsStr::new("dumb")));
        assert_eq!(command.get_env("KITTY_WINDOW_ID"), Some(OsStr::new("3")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
    }
}
