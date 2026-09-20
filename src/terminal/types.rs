use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_COMMAND_CAPACITY: usize = 256;
const DEFAULT_PTY_CAPACITY: usize = 64;
const DEFAULT_TRANSCRIPT_CAPACITY: usize = 64;
/// Raw PTY bytes retained per terminal transcript. Transcripts are
/// write-only failure diagnostics, and full-screen apps repaint constantly,
/// so an unbounded log turns one animated pane into endless disk writes.
const DEFAULT_TRANSCRIPT_MAX_BYTES: u64 = 1 << 20;

use super::theme::TerminalTheme;

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub max_scrollback: usize,
    pub initially_visible: bool,
    pub theme: TerminalTheme,
    pub command_capacity: usize,
    pub pty_capacity: usize,
    pub transcript_capacity: usize,
    /// Cap on appended transcript bytes per terminal, including bytes already
    /// present when the terminal starts. New output past the cap is dropped.
    pub transcript_max_bytes: u64,
    /// Exact native execution environment for a host-owned CLI replacement.
    /// Never serialized with the terminal's durable launch recipe.
    pub execution_environment: Option<super::ipc::ChatExecutionEnvironment>,
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
            theme: TerminalTheme::default(),
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            pty_capacity: DEFAULT_PTY_CAPACITY,
            transcript_capacity: DEFAULT_TRANSCRIPT_CAPACITY,
            transcript_max_bytes: DEFAULT_TRANSCRIPT_MAX_BYTES,
            execution_environment: None,
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
    ResumeHintChanged(Option<String>),
    Bell,
    Exited {
        exit_code: u32,
        signal: Option<String>,
        /// The PTY owner successfully requested this root's observed SIGKILL.
        intentional: bool,
    },
    Error {
        operation: &'static str,
        message: String,
    },
    AgentActivityChanged(Option<crate::agent::Activity>),
    /// The PTY has drained and the final grid is published. Unlike `Exited`,
    /// this confirms that descendants no longer own the terminal.
    Completed {
        exit_code: u32,
        signal: Option<String>,
        intentional: bool,
    },
    ConversationTitleChanged(String),
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
    WheelUp,
    WheelDown,
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
