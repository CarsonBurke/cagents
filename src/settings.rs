use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    instance, layout_metrics,
    layout_metrics::{
        NAVIGATION_MAX_WIDTH as EXPANDED_WIDTH_MAX, NAVIGATION_MIN_WIDTH as EXPANDED_WIDTH_MIN,
    },
};

pub const SETTINGS_SCHEMA_VERSION: u32 = 3;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MAX_TOML_INTEGER: u64 = i64::MAX as u64;

pub const MAX_TERMINAL_SCROLLBACK_MIB: usize = 1024;
const MAX_LEGACY_TERMINAL_SCROLLBACK_LINES: usize = 1_000_000;
pub const MAX_TERMINAL_SCROLL_SIZE: usize = 10;
/// The longest terminal font family name, in bytes.
pub const MAX_TERMINAL_FONT_FAMILY_BYTES: usize = 128;
const LEGACY_SETTINGS_SCHEMA_VERSION: u32 = 1;
const PREVIOUS_SETTINGS_SCHEMA_VERSION: u32 = 2;
const LEGACY_SIDEBAR_WIDTH_MIN: f32 = 180.0;
const LEGACY_SIDEBAR_WIDTH_MAX: f32 = 480.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppTheme {
    #[default]
    System,
    Dark,
    Light,
    CatppuccinMocha,
    CatppuccinLatte,
    Dracula,
    Nord,
    TokyoNight,
    GruvboxDark,
    SolarizedLight,
}

impl AppTheme {
    pub const ALL: [Self; 10] = [
        Self::System,
        Self::Dark,
        Self::Light,
        Self::CatppuccinMocha,
        Self::CatppuccinLatte,
        Self::Dracula,
        Self::Nord,
        Self::TokyoNight,
        Self::GruvboxDark,
        Self::SolarizedLight,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::CatppuccinMocha => "catppuccin_mocha",
            Self::CatppuccinLatte => "catppuccin_latte",
            Self::Dracula => "dracula",
            Self::Nord => "nord",
            Self::TokyoNight => "tokyo_night",
            Self::GruvboxDark => "gruvbox_dark",
            Self::SolarizedLight => "solarized_light",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::TokyoNight => "Tokyo Night",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::SolarizedLight => "Solarized Light",
        }
    }
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.key() == key)
    }

    /// App surfaces use their own selection; terminal overrides and syntax
    /// palettes remain independent. COSMIC derives accessible component states.
    #[cfg(feature = "ui")]
    pub fn resolve(self, system: &cosmic::Theme) -> cosmic::Theme {
        use crate::terminal::TerminalTheme;
        use cosmic::cosmic_theme::{
            ThemeBuilder,
            palette::{Srgb, Srgba},
        };
        let palette = match self {
            Self::System => {
                return cosmic::Theme::system(std::sync::Arc::new(system.cosmic().clone()));
            }
            Self::Dark => return cosmic::Theme::dark(),
            Self::Light => return cosmic::Theme::light(),
            Self::CatppuccinMocha => TerminalTheme::CatppuccinMocha,
            Self::CatppuccinLatte => TerminalTheme::CatppuccinLatte,
            Self::Dracula => TerminalTheme::Dracula,
            Self::Nord => TerminalTheme::Nord,
            Self::TokyoNight => TerminalTheme::TokyoNight,
            Self::GruvboxDark => TerminalTheme::GruvboxDark,
            Self::SolarizedLight => TerminalTheme::SolarizedLight,
        }
        .palette();
        let rgb = |color: crate::terminal::ThemeColor| {
            let (r, g, b) = color.rgb();
            Srgb::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
        };
        let surface = |amount: f32| {
            let bg = rgb(palette.background);
            let fg = rgb(palette.foreground);
            Srgba::new(
                bg.red + (fg.red - bg.red) * amount,
                bg.green + (fg.green - bg.green) * amount,
                bg.blue + (fg.blue - bg.blue) * amount,
                1.0,
            )
        };
        let mut builder = if palette.light {
            ThemeBuilder::light()
        } else {
            ThemeBuilder::dark()
        };
        builder.bg_color = Some(surface(0.0));
        builder.primary_container_bg = Some(surface(0.035));
        builder.secondary_container_bg = Some(surface(0.07));
        builder.neutral_tint = Some(rgb(palette.background));
        builder.text_tint = Some(rgb(palette.foreground));
        builder.accent = Some(rgb(palette.ansi[4]));
        builder.success = Some(rgb(palette.ansi[2]));
        builder.warning = Some(rgb(palette.ansi[3]));
        builder.destructive = Some(rgb(palette.ansi[1]));
        cosmic::Theme::custom(std::sync::Arc::new(builder.build()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCursorStyle {
    Application,
    Block,
    #[default]
    Beam,
    Underline,
}

const fn default_ui_scale() -> f32 {
    1.0
}
const fn default_terminal_line_height() -> f32 {
    1.286
}
const fn default_terminal_padding() -> f32 {
    8.0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkOpenDestination {
    #[default]
    IntegratedBrowser,
    SystemDefault,
}

impl LinkOpenDestination {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntegratedBrowser => "integrated_browser",
            Self::SystemDefault => "system_default",
        }
    }
}

/// What choosing a pull request in the sidebar does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestOpenTarget {
    /// Opens the review pane: the changed files, their diffs, and viewed marks.
    #[default]
    Review,
    /// Opens the pull request's page through the link preference.
    Link,
}

impl PullRequestOpenTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Link => "link",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkOpenGesture {
    #[default]
    ControlClick,
    ControlShiftClick,
}

impl LinkOpenGesture {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ControlClick => "control_click",
            Self::ControlShiftClick => "control_shift_click",
        }
    }
}

/// The chords bound to each action.
///
/// A list rather than a single chord because an action can be reachable by
/// more than one gesture: column navigation answers to both a letter chord and
/// the arrow keys, and binding one must not cost the other. Every chord in the
/// table, across all actions, must be distinct, and a chord bound here is also
/// reserved - the workspace consumes it instead of forwarding it to the
/// process inside a card, except where the action's scope leaves it to a
/// focused terminal (see `workspace_widget::ShortcutScope`).
///
/// Workspace defaults include `Ctrl+W` to close a pane and `Alt+R` to cycle
/// its width, including while a terminal has focus. These chords belong to
/// the workspace rather than the program inside it. A default that changes
/// bumps [`SHORTCUT_DEFAULTS_REVISION`] and records the chords it
/// replaces in [`RETIRED_SHORTCUT_DEFAULTS`], so a file still holding them
/// moves to the new ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardShortcuts {
    pub new_terminal: Vec<String>,
    pub new_channel: Vec<String>,
    pub clone_pane: Vec<String>,
    pub close_focused_column: Vec<String>,
    pub search_projects: Vec<String>,
    pub find_in_pane: Vec<String>,
    pub previous_column: Vec<String>,
    pub next_column: Vec<String>,
    pub cycle_pane_width: Vec<String>,
    pub previous_workspace: Vec<String>,
    pub next_workspace: Vec<String>,
    /// Flips the viewed mark on the file a review pane shows.
    pub review_toggle_viewed: Vec<String>,
    pub review_next_file: Vec<String>,
    pub review_previous_file: Vec<String>,
    pub review_next_unviewed: Vec<String>,
    pub review_next_change: Vec<String>,
    pub review_previous_change: Vec<String>,
    pub command_palette: Vec<String>,
    pub new_native_thread: Vec<String>,
    pub chat_permissions: Vec<String>,
    pub chat_model: Vec<String>,
    pub chat_account: Vec<String>,
    pub toggle_inspector: Vec<String>,
    pub open_settings: Vec<String>,
    pub move_pane_left: Vec<String>,
    pub move_pane_right: Vec<String>,
    pub reopen_pane: Vec<String>,
    pub focus_pane_1: Vec<String>,
    pub focus_pane_2: Vec<String>,
    pub focus_pane_3: Vec<String>,
    pub focus_pane_4: Vec<String>,
    pub focus_pane_5: Vec<String>,
    pub focus_pane_6: Vec<String>,
    pub focus_pane_7: Vec<String>,
    pub focus_pane_8: Vec<String>,
    pub focus_pane_9: Vec<String>,
    pub toggle_projects: Vec<String>,
    pub rename_pane: Vec<String>,
}

impl Default for KeyboardShortcuts {
    fn default() -> Self {
        Self {
            new_terminal: vec!["Ctrl+Shift+T".to_owned()],
            new_channel: vec!["Ctrl+Shift+G".to_owned()],
            clone_pane: vec!["Ctrl+Shift+D".to_owned()],
            close_focused_column: vec!["Ctrl+W".to_owned()],
            search_projects: vec!["Ctrl+Shift+F".to_owned()],
            find_in_pane: vec!["Ctrl+F".to_owned()],
            previous_column: vec!["Ctrl+PageUp".to_owned()],
            next_column: vec!["Ctrl+PageDown".to_owned()],
            cycle_pane_width: vec!["Alt+R".to_owned()],
            previous_workspace: vec!["Alt+Shift+Tab".to_owned(), "Ctrl+Alt+PageUp".to_owned()],
            next_workspace: vec!["Alt+Tab".to_owned(), "Ctrl+Alt+PageDown".to_owned()],
            review_toggle_viewed: vec!["Alt+V".to_owned()],
            review_next_file: vec!["Alt+Down".to_owned()],
            review_previous_file: vec!["Alt+Up".to_owned()],
            review_next_unviewed: vec!["Alt+Shift+Down".to_owned()],
            review_next_change: vec!["Alt+J".to_owned(), "F7".to_owned()],
            review_previous_change: vec!["Alt+K".to_owned(), "Shift+F7".to_owned()],
            command_palette: vec!["Ctrl+Shift+P".to_owned()],
            new_native_thread: vec!["Ctrl+T".to_owned(), "Ctrl+Shift+N".to_owned()],
            chat_permissions: vec!["Ctrl+Shift+O".to_owned()],
            chat_model: vec!["Ctrl+Shift+M".to_owned()],
            chat_account: vec!["Ctrl+Shift+A".to_owned()],
            toggle_inspector: vec!["Ctrl+Shift+I".to_owned()],
            open_settings: vec!["Ctrl+Comma".to_owned()],
            move_pane_left: vec!["Alt+Shift+Left".to_owned()],
            move_pane_right: vec!["Alt+Shift+Right".to_owned()],
            reopen_pane: vec!["Ctrl+Shift+Z".to_owned()],
            focus_pane_1: vec!["Alt+1".to_owned()],
            focus_pane_2: vec!["Alt+2".to_owned()],
            focus_pane_3: vec!["Alt+3".to_owned()],
            focus_pane_4: vec!["Alt+4".to_owned()],
            focus_pane_5: vec!["Alt+5".to_owned()],
            focus_pane_6: vec!["Alt+6".to_owned()],
            focus_pane_7: vec!["Alt+7".to_owned()],
            focus_pane_8: vec!["Alt+8".to_owned()],
            focus_pane_9: vec!["Alt+9".to_owned()],
            toggle_projects: vec!["Ctrl+Shift+B".to_owned()],
            rename_pane: vec!["F2".to_owned()],
        }
    }
}

/// Separates chords in the one-line form the settings pane edits.
pub const CHORD_SEPARATOR: &str = ", ";

impl KeyboardShortcuts {
    pub fn action_names() -> &'static [&'static str] {
        &SHORTCUT_ACTIONS
    }

    /// Every action's chord list, in the order the dialog presents them.
    pub fn actions(&self) -> [&Vec<String>; 38] {
        [
            &self.new_terminal,
            &self.new_channel,
            &self.clone_pane,
            &self.close_focused_column,
            &self.search_projects,
            &self.find_in_pane,
            &self.previous_column,
            &self.next_column,
            &self.cycle_pane_width,
            &self.previous_workspace,
            &self.next_workspace,
            &self.review_toggle_viewed,
            &self.review_next_file,
            &self.review_previous_file,
            &self.review_next_unviewed,
            &self.review_next_change,
            &self.review_previous_change,
            &self.command_palette,
            &self.new_native_thread,
            &self.chat_permissions,
            &self.chat_model,
            &self.chat_account,
            &self.toggle_inspector,
            &self.open_settings,
            &self.move_pane_left,
            &self.move_pane_right,
            &self.reopen_pane,
            &self.focus_pane_1,
            &self.focus_pane_2,
            &self.focus_pane_3,
            &self.focus_pane_4,
            &self.focus_pane_5,
            &self.focus_pane_6,
            &self.focus_pane_7,
            &self.focus_pane_8,
            &self.focus_pane_9,
            &self.toggle_projects,
            &self.rename_pane,
        ]
    }

    fn validate(&self) -> Result<(), SettingsError> {
        self.fault().map_or(Ok(()), |fault| Err(fault.error))
    }

    /// The first action this table cannot bind, and why: by index into
    /// `actions()`, and for a conflict, the earlier action whose chord it
    /// repeats.
    pub fn fault(&self) -> Option<ShortcutFault> {
        let mut parsed: Vec<(ShortcutBinding, usize)> = Vec::new();
        for (index, chords) in self.actions().into_iter().enumerate() {
            let fault = |error, conflicts_with| {
                Some(ShortcutFault {
                    action: index,
                    conflicts_with,
                    error: SettingsError::InvalidShortcut(error),
                })
            };
            if chords.is_empty() && !INTRODUCED_SHORTCUT_ACTIONS.contains(&SHORTCUT_ACTIONS[index])
            {
                // Newly introduced actions may remain unbound so migration
                // never steals a user's existing custom chord.
                return fault("every keyboard shortcut needs at least one chord", None);
            }
            for chord in chords {
                let binding = match parse_shortcut(chord) {
                    Ok(binding) => binding,
                    Err(error) => return fault(error, None),
                };
                if let Some(message) = withheld_from_the_card(binding) {
                    return fault(message, None);
                }
                if let Some(&(_, owner)) = parsed.iter().find(|(other, _)| *other == binding) {
                    return fault("keyboard shortcuts must not conflict", Some(owner));
                }
                parsed.push((binding, index));
            }
        }
        None
    }
}

/// The serialized name of each action, in `KeyboardShortcuts::actions` order.
const SHORTCUT_ACTIONS: [&str; 38] = [
    "new_terminal",
    "new_channel",
    "clone_pane",
    "close_focused_column",
    "search_projects",
    "find_in_pane",
    "previous_column",
    "next_column",
    "cycle_pane_width",
    "previous_workspace",
    "next_workspace",
    "review_toggle_viewed",
    "review_next_file",
    "review_previous_file",
    "review_next_unviewed",
    "review_next_change",
    "review_previous_change",
    "command_palette",
    "new_native_thread",
    "chat_permissions",
    "chat_model",
    "chat_account",
    "toggle_inspector",
    "open_settings",
    "move_pane_left",
    "move_pane_right",
    "reopen_pane",
    "focus_pane_1",
    "focus_pane_2",
    "focus_pane_3",
    "focus_pane_4",
    "focus_pane_5",
    "focus_pane_6",
    "focus_pane_7",
    "focus_pane_8",
    "focus_pane_9",
    "toggle_projects",
    "rename_pane",
];

/// An action a shortcut table cannot bind.
#[derive(Debug)]
pub struct ShortcutFault {
    pub action: usize,
    pub conflicts_with: Option<usize>,
    pub error: SettingsError,
}

/// Why a chord must not be bound, or `None` when binding it is safe.
///
/// A bound chord is also a reserved one: the card widget captures it and never
/// forwards it to the process inside the card. That is the point for
/// `Ctrl+Shift+W`, but the control characters that carry a signal or end
/// input are how a user stops a runaway program, and a card that swallows
/// them offers no clue that it is doing so.
fn withheld_from_the_card(binding: ShortcutBinding) -> Option<&'static str> {
    if !binding.control || binding.alt || binding.shift || binding.logo {
        // Only the bare `Ctrl` chord reaches a child as a control character.
        // `Ctrl+Shift+C` is the conventional copy gesture and sends nothing,
        // and adding Alt or Super makes it an escape sequence instead.
        return None;
    }
    match binding.key {
        ShortcutKey::Character('c') => Some("Ctrl+C interrupts the program in a card"),
        ShortcutKey::Character('d') => Some("Ctrl+D ends input to the program in a card"),
        ShortcutKey::Character('z') => Some("Ctrl+Z suspends the program in a card"),
        ShortcutKey::Character('s') | ShortcutKey::Character('q') => {
            Some("Ctrl+S and Ctrl+Q control the flow of output in a card")
        }
        _ => None,
    }
}

/// Renders a chord list as the single line the settings pane edits.
pub fn format_chords(chords: &[String]) -> String {
    chords.join(CHORD_SEPARATOR)
}

/// Reads back the line `format_chords` writes.
///
/// Empty entries are dropped rather than kept as blanks, so a trailing
/// separator left mid-edit does not read as an unparsable chord; a line with no
/// chord at all is rejected by [`KeyboardShortcuts::validate`].
pub fn parse_chords(raw: &str) -> Vec<String> {
    let mut chords = Vec::new();
    let mut start = 0;
    for (index, ch) in raw.char_indices() {
        if ch == ',' && !raw[..index].ends_with('+') {
            let chord = raw[start..index].trim();
            if !chord.is_empty() {
                chords.push(chord.to_owned());
            }
            start = index + 1;
        }
    }
    let chord = raw[start..].trim();
    if !chord.is_empty() {
        chords.push(chord.to_owned());
    }
    chords
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutBinding {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub key: ShortcutKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutKey {
    Character(char),
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Tab,
    PageUp,
    PageDown,
    Function(u8),
}

pub fn parse_shortcut(raw: &str) -> Result<ShortcutBinding, &'static str> {
    let mut control = false;
    let mut alt = false;
    let mut shift = false;
    let mut logo = false;
    let mut key = None;
    for token in raw.trim().split('+').map(str::trim) {
        if token.eq_ignore_ascii_case("ctrl") || token.eq_ignore_ascii_case("control") {
            if std::mem::replace(&mut control, true) {
                return Err("keyboard shortcut repeats a modifier");
            }
        } else if token.eq_ignore_ascii_case("alt") {
            if std::mem::replace(&mut alt, true) {
                return Err("keyboard shortcut repeats a modifier");
            }
        } else if token.eq_ignore_ascii_case("shift") {
            if std::mem::replace(&mut shift, true) {
                return Err("keyboard shortcut repeats a modifier");
            }
        } else if token.eq_ignore_ascii_case("super") || token.eq_ignore_ascii_case("meta") {
            if std::mem::replace(&mut logo, true) {
                return Err("keyboard shortcut repeats a modifier");
            }
        } else {
            if key.is_some() {
                return Err("keyboard shortcut must contain exactly one key");
            }
            key = if token.eq_ignore_ascii_case("left") {
                Some(ShortcutKey::ArrowLeft)
            } else if token.eq_ignore_ascii_case("right") {
                Some(ShortcutKey::ArrowRight)
            } else if token.eq_ignore_ascii_case("up") {
                Some(ShortcutKey::ArrowUp)
            } else if token.eq_ignore_ascii_case("down") {
                Some(ShortcutKey::ArrowDown)
            } else if token.eq_ignore_ascii_case("tab") {
                Some(ShortcutKey::Tab)
            } else if token.eq_ignore_ascii_case("pageup") {
                Some(ShortcutKey::PageUp)
            } else if token.eq_ignore_ascii_case("pagedown") {
                Some(ShortcutKey::PageDown)
            } else if token.eq_ignore_ascii_case("comma") {
                Some(ShortcutKey::Character(','))
            } else if token.eq_ignore_ascii_case("plus") {
                Some(ShortcutKey::Character('+'))
            } else if token
                .to_ascii_lowercase()
                .strip_prefix('f')
                .and_then(|number| number.parse::<u8>().ok())
                .is_some_and(|number| (1..=12).contains(&number))
            {
                Some(ShortcutKey::Function(
                    token[1..].parse().expect("validated function key"),
                ))
            } else {
                let mut characters = token.chars();
                match (characters.next(), characters.next()) {
                    (Some(character), None) if character.is_ascii_graphic() => {
                        Some(ShortcutKey::Character(character.to_ascii_lowercase()))
                    }
                    _ => {
                        return Err(
                            "keyboard shortcut key must be a letter, digit, punctuation, arrow, Tab, PageUp, PageDown, or F1-F12 (use Comma and Plus for separator keys)",
                        );
                    }
                }
            };
        }
    }
    if !(control || alt || logo) && !matches!(key, Some(ShortcutKey::Function(_))) {
        return Err("keyboard shortcut must include Ctrl, Alt, or Super");
    }
    Ok(ShortcutBinding {
        control,
        alt,
        shift,
        logo,
        key: key.ok_or("keyboard shortcut must contain a key")?,
    })
}

/// Approximate the old line preference at 1 KiB per requested line, rounded
/// up to MiB. Its former direct use as bytes retained almost no history.
const fn legacy_scrollback_mib(lines: usize) -> usize {
    if lines > MAX_LEGACY_TERMINAL_SCROLLBACK_LINES {
        return MAX_TERMINAL_SCROLLBACK_MIB + 1;
    }
    lines.div_ceil(1024)
}

const fn default_legacy_terminal_scrollback_lines() -> usize {
    10_000
}

const fn default_terminal_scrollback_mib() -> usize {
    16
}

const fn default_terminal_scroll_size() -> usize {
    2
}

const fn default_terminal_scroll_speed() -> f32 {
    1.0
}

/// Clone-local default shared by the local and device creation paths.
pub const DEFAULT_MANAGED_WORKTREE_DIRECTORY: &str = ".worktrees/";

fn default_managed_worktree_directory() -> String {
    DEFAULT_MANAGED_WORKTREE_DIRECTORY.to_owned()
}

const fn default_focus_follows_mouse() -> bool {
    false
}

const fn default_editor_auto_save() -> bool {
    true
}

const fn default_diff_hide_unchanged() -> bool {
    true
}

const fn default_confirm_terminal_close() -> bool {
    true
}
fn default_browser_search_url() -> String {
    "https://duckduckgo.com/?q={query}".into()
}

const fn default_native_fraction() -> f32 {
    0.5
}

const fn default_chat_enter_sends() -> bool {
    true
}

const fn default_show_status_bar() -> bool {
    true
}

const fn default_show_worktree_changes_icons() -> bool {
    true
}

const fn default_relaunch_agents_on_upgrade() -> bool {
    true
}

const fn default_work_stale_after_days() -> u16 {
    crate::work::DEFAULT_STALE_AFTER_DAYS
}

const fn default_check_for_updates() -> bool {
    true
}

const fn default_shortcut_defaults_revision() -> u32 {
    SHORTCUT_DEFAULTS_REVISION
}

/// Main stays first; imported worktrees without creation evidence sort by path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeSort {
    #[default]
    Newest,
    Oldest,
    Alphabetical,
}

/// Syntax colors for native editors and diffs. `Auto` follows the app theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorSyntaxTheme {
    #[default]
    Auto,
    SolarizedDark,
    Mocha,
    Ocean,
    Eighties,
    // snake_case would split the acronym into `inspired_git_hub`; keep the file
    // spelling identical to `key()` and still read files written that way.
    #[serde(rename = "inspired_github", alias = "inspired_git_hub")]
    InspiredGitHub,
}

impl EditorSyntaxTheme {
    pub const ALL: &'static [Self] = &[
        Self::Auto,
        Self::SolarizedDark,
        Self::Mocha,
        Self::Ocean,
        Self::Eighties,
        Self::InspiredGitHub,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Match app",
            Self::SolarizedDark => "Solarized Dark",
            Self::Mocha => "Mocha",
            Self::Ocean => "Ocean",
            Self::Eighties => "Eighties",
            Self::InspiredGitHub => "Inspired GitHub",
        }
    }

    /// The stable identifier the settings surface round-trips. Deliberately
    /// hand-written rather than derived from the serde rename, so renaming a
    /// variant cannot silently change the value the UI parses.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::SolarizedDark => "solarized_dark",
            Self::Mocha => "mocha",
            Self::Ocean => "ocean",
            Self::Eighties => "eighties",
            Self::InspiredGitHub => "inspired_github",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|theme| theme.key() == key)
    }
}

/// The pane opened when visiting an empty worktree or channel.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultPane {
    #[default]
    NativeChat,
    Terminal,
}

impl DefaultPane {
    pub const ALL: [Self; 2] = [Self::NativeChat, Self::Terminal];

    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeChat => "Native chat",
            Self::Terminal => "Terminal",
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::NativeChat => "native_chat",
            Self::Terminal => "terminal",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|pane| pane.key() == key)
    }
}

/// Settings are forward compatible, because mixed builds share one file and an
/// older one must neither refuse to start nor erase what a newer one wrote. A
/// key this build does not know is ignored, and a value it cannot read - a
/// preset a newer build added - is replaced by its default; both are logged
/// on load and written back on save (see `keep_foreign_settings`). A change
/// that an older build cannot read that way bumps `SETTINGS_SCHEMA_VERSION`,
/// and a build never saves over a newer schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    pub schema_version: u32,
    /// Stable palette entry keys, newest first. Old settings default to empty.
    #[serde(default)]
    pub palette_recents: Vec<String>,
    pub max_hot_cards: usize,
    pub hibernation_delay_ms: u64,
    #[serde(default = "default_focus_follows_mouse")]
    pub focus_follows_mouse: bool,
    #[serde(default)]
    pub app_theme: AppTheme,
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    #[serde(default = "default_terminal_line_height")]
    pub terminal_line_height: f32,
    #[serde(default = "default_terminal_scroll_speed")]
    pub terminal_scroll_speed: f32,
    #[serde(default = "default_terminal_padding")]
    pub terminal_padding: f32,
    #[serde(default = "default_terminal_scroll_size")]
    pub terminal_scroll_size: usize,
    #[serde(default)]
    pub terminal_cursor_style: TerminalCursorStyle,
    #[serde(default)]
    pub terminal_theme: crate::terminal::TerminalTheme,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_terminal_theme: Option<crate::terminal::TerminalPalette>,
    pub terminal_font_size: f32,
    /// The terminal font family, or empty for the system monospace default.
    #[serde(default)]
    pub terminal_font_family: String,
    pub sidebar_width: f32,
    pub inspector_width: f32,
    pub default_column_fraction: f32,
    #[serde(default = "default_terminal_scrollback_mib")]
    pub terminal_scrollback_mib: usize,
    #[serde(default)]
    pub link_open_destination: LinkOpenDestination,
    #[serde(default)]
    pub link_open_gesture: LinkOpenGesture,
    #[serde(default)]
    pub pull_request_open_target: PullRequestOpenTarget,
    /// Absolute, or resolved relative to the repository root.
    #[serde(default = "default_managed_worktree_directory")]
    pub managed_worktree_directory: String,
    /// Automatic cleanup is opt-in and never discards changes or stops work.
    #[serde(default)]
    pub auto_delete_merged_worktrees: bool,
    #[serde(default)]
    pub worktree_sort: WorktreeSort,
    #[serde(default)]
    pub default_worktree_pane: DefaultPane,
    #[serde(default)]
    pub default_channel_pane: DefaultPane,
    #[serde(default = "default_editor_auto_save")]
    pub editor_auto_save: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_font_size: Option<f32>,
    #[serde(default = "default_confirm_terminal_close")]
    pub confirm_multiline_paste: bool,
    #[serde(default = "default_browser_search_url")]
    pub browser_search_url: String,
    #[serde(default)]
    pub editor_wrap_lines: bool,
    #[serde(default = "default_diff_hide_unchanged")]
    pub diff_hide_unchanged: bool,
    #[serde(default)]
    pub diff_ignore_whitespace: bool,
    #[serde(default)]
    pub editor_syntax_theme: EditorSyntaxTheme,
    #[serde(default = "default_confirm_terminal_close")]
    pub confirm_terminal_close: bool,
    /// Whether a bare Enter in the chat composer sends the message.
    ///
    /// Sending is the default because it is what every chat surface a user
    /// arrives from does, and because the alternative made the common action
    /// the harder chord. Turning this off restores Enter as a line break and
    /// moves sending to Ctrl+Enter, which suits composing long prompts.
    #[serde(default = "default_chat_enter_sends")]
    pub chat_enter_sends: bool,
    #[serde(default)]
    pub chat_expand_tool_groups: bool,
    /// Legacy preference retained for settings-file compatibility. Native chat
    /// columns now fit their pane up to the shared readable measure.
    #[serde(default = "default_native_fraction")]
    pub chat_content_fraction: f32,
    #[serde(default = "default_native_fraction")]
    pub default_native_pane_fraction: f32,
    /// Whether the window keeps a status bar along its bottom edge, which
    /// reports how much of each signed-in provider account's usage limits
    /// is left. Hiding it also stops the account probes that feed it while
    /// no Accounts pane is open.
    #[serde(default = "default_show_status_bar")]
    pub show_status_bar: bool,
    /// Preview rows by provider: `window:<provider key>` or `credits`.
    /// An absent provider retains its automatic headline; an empty list
    /// deliberately shows only the provider control.
    #[serde(default)]
    pub usage_preview_rows: HashMap<String, Vec<String>>,
    /// Dirty-worktree badges in the Projects sidebar; hiding them leaves
    /// source-control data and agent activity indicators intact.
    #[serde(default = "default_show_worktree_changes_icons")]
    pub show_worktree_changes_icons: bool,
    /// Whether a daemon upgrade relaunches an agent whose conversation cannot
    /// be resumed exactly - into its provider's resume picker, or a fresh
    /// conversation when it had none yet - rather than refusing to upgrade
    /// until it is closed. `cagents daemon prepare-upgrade` reads it
    /// from this file too; see `crate::upgrade::relaunch_agents_by_default`.
    #[serde(default = "default_relaunch_agents_on_upgrade")]
    pub relaunch_agents_on_upgrade: bool,
    /// Whether windows look for a new release on their own, at startup and
    /// every few hours. A check the user asks for runs either way.
    #[serde(default = "default_check_for_updates")]
    pub check_for_updates: bool,
    /// Days without activity after which a Work board card is stale. The
    /// daemon reads it from this file too; see `crate::work::stale_after_days`.
    #[serde(default = "default_work_stale_after_days")]
    pub work_stale_after_days: u16,
    /// The revision of the built-in shortcut defaults `keyboard_shortcuts`
    /// has been brought up to; see [`SHORTCUT_DEFAULTS_REVISION`]. A file
    /// without it is read as revision 1, and loading records the revision it
    /// was upgraded to, so a chord chosen afterwards is never taken for an
    /// old default again. A newer build's revision is kept as it is.
    #[serde(default = "default_shortcut_defaults_revision")]
    pub shortcut_defaults_revision: u32,
    /// Seconds to keep a language server warm after its last document closes.
    #[serde(default = "default_lsp_idle_timeout")]
    pub lsp_idle_timeout_seconds: u64,
    /// Maximum admitted servers; active documents are never evicted.
    #[serde(default = "default_lsp_max_sessions")]
    pub lsp_max_sessions: usize,
    #[serde(default)]
    pub editor_replace_preserve_case: bool,
    /// External language servers, in declaration order. The first server whose
    /// language claims a file extension handles that file. An empty list
    /// disables LSP entirely.
    ///
    /// Declared after every scalar because it serialises as an array of
    /// tables, and TOML admits no scalar key after the first table. Declared
    /// before `keyboard_shortcuts` because an empty list serialises as the
    /// value `language_servers = []`, which after a `[keyboard_shortcuts]`
    /// header would read as a key inside that table.
    #[serde(default = "default_language_servers")]
    pub language_servers: Vec<LanguageServerSettings>,
    #[serde(default)]
    pub editor_find_options: EditorFindOptions,
    #[serde(default, deserialize_with = "deserialize_keyboard_shortcuts")]
    pub keyboard_shortcuts: KeyboardShortcuts,
    #[serde(default)]
    pub notifications: crate::notifications::Preferences,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            notifications: crate::notifications::Preferences::default(),
            schema_version: SETTINGS_SCHEMA_VERSION,
            palette_recents: Vec::new(),
            max_hot_cards: 6,
            hibernation_delay_ms: 30_000,
            focus_follows_mouse: default_focus_follows_mouse(),
            app_theme: AppTheme::default(),
            terminal_scroll_speed: default_terminal_scroll_speed(),
            ui_scale: default_ui_scale(),
            terminal_line_height: default_terminal_line_height(),
            terminal_padding: default_terminal_padding(),
            terminal_scroll_size: default_terminal_scroll_size(),
            terminal_cursor_style: TerminalCursorStyle::default(),
            terminal_theme: crate::terminal::TerminalTheme::default(),
            custom_terminal_theme: None,
            terminal_font_size: 14.0,
            terminal_font_family: String::new(),
            sidebar_width: layout_metrics::NAVIGATION_DEFAULT_WIDTH,
            inspector_width: layout_metrics::INSPECTOR_DEFAULT_WIDTH,
            default_column_fraction: 0.5,
            terminal_scrollback_mib: default_terminal_scrollback_mib(),
            link_open_destination: LinkOpenDestination::default(),
            link_open_gesture: LinkOpenGesture::default(),
            pull_request_open_target: PullRequestOpenTarget::default(),
            managed_worktree_directory: default_managed_worktree_directory(),
            auto_delete_merged_worktrees: false,
            worktree_sort: WorktreeSort::default(),
            default_worktree_pane: DefaultPane::default(),
            default_channel_pane: DefaultPane::default(),
            editor_auto_save: default_editor_auto_save(),
            editor_font_family: None,
            editor_font_size: None,
            confirm_multiline_paste: true,
            browser_search_url: default_browser_search_url(),
            editor_wrap_lines: false,
            diff_hide_unchanged: default_diff_hide_unchanged(),
            diff_ignore_whitespace: false,
            editor_syntax_theme: EditorSyntaxTheme::default(),
            confirm_terminal_close: default_confirm_terminal_close(),
            chat_enter_sends: default_chat_enter_sends(),
            chat_expand_tool_groups: false,
            chat_content_fraction: default_native_fraction(),
            default_native_pane_fraction: default_native_fraction(),
            show_status_bar: default_show_status_bar(),
            usage_preview_rows: HashMap::new(),
            show_worktree_changes_icons: default_show_worktree_changes_icons(),
            relaunch_agents_on_upgrade: default_relaunch_agents_on_upgrade(),
            check_for_updates: default_check_for_updates(),
            work_stale_after_days: default_work_stale_after_days(),
            shortcut_defaults_revision: SHORTCUT_DEFAULTS_REVISION,
            lsp_idle_timeout_seconds: default_lsp_idle_timeout(),
            lsp_max_sessions: default_lsp_max_sessions(),
            editor_find_options: EditorFindOptions::default(),
            editor_replace_preserve_case: false,
            language_servers: default_language_servers(),
            keyboard_shortcuts: KeyboardShortcuts::default(),
        }
    }
}

fn default_lsp_idle_timeout() -> u64 {
    600
}
fn default_lsp_max_sessions() -> usize {
    8
}

/// Persisted matching preferences shared by editor find surfaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorFindOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub whole_word: bool,
}

/// One language a server handles, and the file extensions that select it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerLanguage {
    /// LSP `languageId` sent in `textDocument/didOpen`.
    pub language_id: String,
    /// Lowercase file extensions without a leading dot.
    pub extensions: Vec<String>,
}

/// One external language server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerSettings {
    /// Stable unique identifier, also used as the session key prefix.
    pub id: String,
    pub languages: Vec<LanguageServerLanguage>,
    /// argv; never a shell command line. `command[0]` is the executable.
    pub command: Vec<String>,
    /// Relative marker names that identify a project root.
    #[serde(default)]
    pub root_markers: Vec<String>,
    /// Sent in initialize before the server begins loading its workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<serde_json::Value>,
}

/// The built-in language-server list. A `language_servers` key in the settings
/// file replaces this list wholesale rather than merging with it.
pub fn default_language_servers() -> Vec<LanguageServerSettings> {
    fn server(
        id: &str,
        command: &[&str],
        languages: &[(&str, &[&str])],
        root_markers: &[&str],
    ) -> LanguageServerSettings {
        LanguageServerSettings {
            initialization_options: None,
            id: id.to_owned(),
            languages: languages
                .iter()
                .map(|(language_id, extensions)| LanguageServerLanguage {
                    language_id: (*language_id).to_owned(),
                    extensions: extensions.iter().map(|value| (*value).to_owned()).collect(),
                })
                .collect(),
            command: command.iter().map(|value| (*value).to_owned()).collect(),
            root_markers: root_markers
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    vec![
        server(
            "rust-analyzer",
            &["rust-analyzer"],
            &[("rust", &["rs"])],
            &["Cargo.toml", "rust-project.json", ".git"],
        ),
        server(
            "pyright",
            &["pyright-langserver", "--stdio"],
            &[("python", &["py"])],
            &[
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                ".git",
            ],
        ),
        server(
            "typescript-language-server",
            &["typescript-language-server", "--stdio"],
            &[
                ("javascript", &["js", "mjs", "cjs"]),
                ("javascriptreact", &["jsx"]),
                ("typescript", &["ts", "mts", "cts"]),
                ("typescriptreact", &["tsx"]),
            ],
            &["tsconfig.json", "jsconfig.json", "package.json", ".git"],
        ),
        server(
            "gopls",
            &["gopls"],
            &[("go", &["go"])],
            &["go.work", "go.mod", ".git"],
        ),
        server(
            "clangd",
            &["clangd"],
            &[
                ("c", &["c", "h"]),
                ("cpp", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"]),
            ],
            &["compile_commands.json", "compile_flags.txt", ".git"],
        ),
        server(
            "lua-language-server",
            &["lua-language-server"],
            &[("lua", &["lua"])],
            &[".luarc.json", ".luarc.jsonc", ".git"],
        ),
        server(
            "bash-language-server",
            &["bash-language-server", "start"],
            &[("shellscript", &["sh", "bash"])],
            &[".git"],
        ),
    ]
}

#[derive(Debug, Deserialize)]
struct SettingsHeader {
    schema_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1TomlSettings {
    schema_version: u32,
    max_hot_cards: usize,
    hibernation_delay_ms: u64,
    terminal_font_size: f32,
    #[serde(default)]
    terminal_font_family: String,
    sidebar_width: f32,
    default_column_fraction: f32,
}

/// The complete version-2 shape. Keeping the previous schema explicit makes
/// the inspector-width addition a real versioned migration instead of
/// silently changing the meaning of an existing schema number.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct V2TomlSettings {
    schema_version: u32,
    max_hot_cards: usize,
    hibernation_delay_ms: u64,
    terminal_font_size: f32,
    #[serde(default)]
    terminal_font_family: String,
    sidebar_width: f32,
    default_column_fraction: f32,
    #[serde(default = "default_legacy_terminal_scrollback_lines")]
    terminal_scrollback_lines: usize,
    #[serde(default)]
    link_open_destination: LinkOpenDestination,
    #[serde(default)]
    link_open_gesture: LinkOpenGesture,
    #[serde(default, deserialize_with = "deserialize_v2_keyboard_shortcuts")]
    keyboard_shortcuts: KeyboardShortcuts,
}

/// The revision of the built-in shortcut defaults this build writes.
///
/// A file records the revision its shortcut table was last upgraded to as
/// `shortcut_defaults_revision`; one without it predates the first change of
/// defaults and is at revision 1. Loading a file at an older revision moves
/// every action still bound to that revision's default onto this build's.
pub const SHORTCUT_DEFAULTS_REVISION: u32 = 7;

/// The first revision, and the one a file without a recorded revision is at.
const FIRST_SHORTCUT_DEFAULTS_REVISION: u32 = 1;

/// A built-in default that a later revision replaced.
struct RetiredShortcutDefault {
    /// The serialized action name.
    action: &'static str,
    /// The first revision whose default is no longer `chords`.
    retired_in: u32,
    chords: &'static [&'static str],
}

/// Every default a revision replaced, oldest revision first.
///
/// Revision 2 moved the workspace chords off keys a shell already uses:
/// `Ctrl+W` deleted a word in readline and fish but closed the pane, and
/// `Ctrl+T` and the `Alt` letters and arrows are readline, fish, vim, and
/// agent CLI keys. `search_projects` kept `Ctrl+F`; a focused terminal now
/// receives it instead.
/// Revision 3 adds the conventional new-tab chord to native threads.
/// Revision 4 restores `Ctrl+W` to close a pane and `Alt+R` to cycle its width.
/// Revision 5 separates project search from find in the focused pane.
/// Revision 7 adds F7 aliases to hunk navigation without replacing custom chords.
const RETIRED_SHORTCUT_DEFAULTS: &[RetiredShortcutDefault] = &[
    RetiredShortcutDefault {
        action: "new_terminal",
        retired_in: 2,
        chords: &["Ctrl+T"],
    },
    RetiredShortcutDefault {
        action: "new_channel",
        retired_in: 2,
        chords: &["Alt+G"],
    },
    RetiredShortcutDefault {
        action: "clone_pane",
        retired_in: 2,
        chords: &["Alt+E"],
    },
    RetiredShortcutDefault {
        action: "close_focused_column",
        retired_in: 2,
        chords: &["Ctrl+W"],
    },
    RetiredShortcutDefault {
        action: "previous_column",
        retired_in: 2,
        chords: &["Alt+Q", "Alt+Left"],
    },
    RetiredShortcutDefault {
        action: "next_column",
        retired_in: 2,
        chords: &["Alt+W", "Alt+Right"],
    },
    RetiredShortcutDefault {
        action: "cycle_pane_width",
        retired_in: 2,
        chords: &["Alt+R"],
    },
    RetiredShortcutDefault {
        action: "new_native_thread",
        retired_in: 3,
        chords: &["Ctrl+Shift+N"],
    },
    RetiredShortcutDefault {
        action: "close_focused_column",
        retired_in: 4,
        chords: &["Ctrl+Shift+W"],
    },
    RetiredShortcutDefault {
        action: "cycle_pane_width",
        retired_in: 4,
        chords: &["Ctrl+Shift+R"],
    },
    RetiredShortcutDefault {
        action: "search_projects",
        retired_in: 5,
        chords: &["Ctrl+F"],
    },
    RetiredShortcutDefault {
        action: "review_next_change",
        retired_in: 7,
        chords: &["Alt+J"],
    },
    RetiredShortcutDefault {
        action: "review_previous_change",
        retired_in: 7,
        chords: &["Alt+K"],
    },
];

/// Column navigation's defaults while each action took a single chord, before
/// the arrow chords joined them. Only a version-2 file can hold these.
const SINGLE_CHORD_DEFAULTS: [(&str, &[&str]); 2] =
    [("previous_column", &["Alt+Q"]), ("next_column", &["Alt+W"])];

/// The default `action` had at `revision`, when a later revision replaced it.
fn retired_default(action: &str, revision: u32) -> Option<&'static [&'static str]> {
    RETIRED_SHORTCUT_DEFAULTS
        .iter()
        .find(|retired| retired.action == action && retired.retired_in > revision)
        .map(|retired| retired.chords)
}

/// This build's default chords for the serialized `action`.
fn default_chords(action: &str) -> Vec<String> {
    let defaults = KeyboardShortcuts::default();
    SHORTCUT_ACTIONS
        .iter()
        .position(|name| *name == action)
        .map(|index| defaults.actions()[index].clone())
        .unwrap_or_default()
}

/// The chords of a shortcut table entry, parsed; `None` when the entry is not
/// a list of chords this build reads.
fn parsed_chords(value: &toml::Value) -> Option<Vec<ShortcutBinding>> {
    value
        .as_array()?
        .iter()
        .map(|chord| parse_shortcut(chord.as_str()?).ok())
        .collect()
}

/// Every chord the table binds, other than those of the `excluded` actions.
fn bound_chords(shortcuts: &toml::Table, excluded: &[&str]) -> Vec<ShortcutBinding> {
    shortcuts
        .iter()
        .filter(|(action, _)| !excluded.contains(&action.as_str()))
        .filter_map(|(_, chords)| chords.as_array())
        .flatten()
        .filter_map(toml::Value::as_str)
        .filter_map(|chord| parse_shortcut(chord).ok())
        .collect()
}

/// Of `chords`, those that are not in `bound`.
fn free_defaults(chords: Vec<String>, bound: &[ShortcutBinding]) -> Vec<String> {
    chords
        .into_iter()
        .filter(|chord| parse_shortcut(chord).is_ok_and(|binding| !bound.contains(&binding)))
        .collect()
}

/// Moves every action a table at `revision` still binds to that revision's
/// default onto this build's default.
///
/// Only an action whose chords are exactly the old default, or one of the
/// `also_retired` chord sets the caller knows its table held by default,
/// moves, compared as parsed chords in any order; one a user changed,
/// extended, or emptied stays as it is. A new chord another action already
/// binds is not taken from it; an action whose new chords are all taken keeps
/// what it had, which may in turn keep another action from a chord it would
/// have moved to.
///
/// An action the table lacks was on its default too, and the user chose the
/// table's chords around it, so it takes part as if it held its old default.
/// One introduced after the table was written never had one; it is left to
/// `preserve_existing_shortcut`.
fn adopt_shortcut_defaults(
    shortcuts: &mut toml::Table,
    revision: u32,
    also_retired: &[(&str, &[&str])],
) {
    let parse = |chords: &[&str]| -> Vec<ShortcutBinding> {
        chords
            .iter()
            .map(|chord| parse_shortcut(chord).expect("retired defaults parse"))
            .collect()
    };
    let same = |left: &[ShortcutBinding], right: &[ShortcutBinding]| {
        left.len() == right.len()
            && left.iter().all(|chord| right.contains(chord))
            && right.iter().all(|chord| left.contains(chord))
    };
    let mut implied = Vec::new();
    for action in SHORTCUT_ACTIONS {
        if !shortcuts.contains_key(action)
            && !INTRODUCED_SHORTCUT_ACTIONS.contains(&action)
            && let Some(retired) = retired_default(action, revision)
        {
            shortcuts.insert(action.into(), retired.to_vec().into());
            implied.push(action);
        }
    }
    let mut moving: Vec<&str> = SHORTCUT_ACTIONS
        .into_iter()
        .filter(|action| {
            let Some(chords) = shortcuts.get(*action).and_then(parsed_chords) else {
                return false;
            };
            retired_default(action, revision)
                .into_iter()
                .chain(
                    also_retired
                        .iter()
                        .filter(|(name, _)| name == action)
                        .map(|(_, retired)| *retired),
                )
                .any(|retired| same(&chords, &parse(retired)))
        })
        .collect();
    let adopted = loop {
        let bound = bound_chords(shortcuts, &moving);
        let (adopted, stuck): (Vec<_>, Vec<_>) = moving
            .iter()
            .map(|action| (*action, free_defaults(default_chords(action), &bound)))
            .partition(|(_, chords)| !chords.is_empty());
        if stuck.is_empty() {
            break adopted;
        }
        moving.retain(|action| !stuck.iter().any(|(stuck, _)| stuck == action));
    };
    for (action, chords) in adopted {
        if implied.contains(&action) && chords == default_chords(action) {
            // Absent, it reads as that default anyway; the file stays as
            // sparse as it was.
            shortcuts.remove(action);
        } else {
            shortcuts.insert(action.into(), chords.into());
        }
    }
}

/// Gives an action a table lacks, one introduced after the table was written,
/// the default chords no other action in it binds. One whose defaults are all
/// taken stays unbound rather than steal a user's chord or invalidate the
/// table; an empty list says so, and is written, so that it still holds once
/// the chord is free.
fn preserve_existing_shortcut(shortcuts: &mut toml::Table, action: &str) {
    if shortcuts.contains_key(action) {
        return;
    }
    let defaults = default_chords(action);
    let free = free_defaults(defaults.clone(), &bound_chords(shortcuts, &[]));
    if free != defaults {
        shortcuts.insert(action.into(), free.into());
    }
}

/// The actions added after the shortcut table was introduced, each of which
/// an older table may lack.
const INTRODUCED_SHORTCUT_ACTIONS: [&str; 33] = [
    "find_in_pane",
    "toggle_projects",
    "rename_pane",
    "command_palette",
    "new_native_thread",
    "chat_permissions",
    "chat_model",
    "chat_account",
    "toggle_inspector",
    "open_settings",
    "move_pane_left",
    "move_pane_right",
    "reopen_pane",
    "focus_pane_1",
    "focus_pane_2",
    "focus_pane_3",
    "focus_pane_4",
    "focus_pane_5",
    "focus_pane_6",
    "focus_pane_7",
    "focus_pane_8",
    "focus_pane_9",
    "cycle_pane_width",
    "new_channel",
    "clone_pane",
    "previous_workspace",
    "next_workspace",
    "review_toggle_viewed",
    "review_next_file",
    "review_previous_file",
    "review_next_unviewed",
    "review_next_change",
    "review_previous_change",
];

fn preserve_introduced_shortcuts(shortcuts: &mut toml::Table) {
    for action in INTRODUCED_SHORTCUT_ACTIONS {
        preserve_existing_shortcut(shortcuts, action);
    }
}

/// Accepts the pre-rename spelling of the channel-creation shortcut.
///
/// The action was stored as `new_thread` before the project-level container
/// became a channel, so an existing table would otherwise lose its binding
/// rather than migrate. The legacy key is moved rather than copied because
/// leaving it behind would let two spellings of one action disagree; a table
/// already carrying the new spelling keeps it and discards the stale one.
fn adopt_legacy_channel_shortcut(shortcuts: &mut toml::Table) {
    if let Some(legacy) = shortcuts.remove("new_thread")
        && !shortcuts.contains_key("new_channel")
    {
        shortcuts.insert("new_channel".into(), legacy);
    }
}

/// Decodes a current-schema shortcut table, which `normalize_current` has
/// already brought up to this build's defaults revision.
fn deserialize_keyboard_shortcuts<'de, D>(deserializer: D) -> Result<KeyboardShortcuts, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut shortcuts = toml::Table::deserialize(deserializer)?;
    adopt_legacy_channel_shortcut(&mut shortcuts);
    preserve_introduced_shortcuts(&mut shortcuts);
    toml::Value::Table(shortcuts)
        .try_into()
        .map_err(serde::de::Error::custom)
}

fn deserialize_v2_keyboard_shortcuts<'de, D>(deserializer: D) -> Result<KeyboardShortcuts, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut shortcuts = toml::Table::deserialize(deserializer)?;
    adopt_legacy_channel_shortcut(&mut shortcuts);
    let scalar: Vec<String> = shortcuts
        .iter()
        .filter(|(_, binding)| binding.is_str())
        .map(|(action, _)| action.clone())
        .collect();
    for (_, binding) in &mut shortcuts {
        if let toml::Value::String(chord) = binding {
            *binding = toml::Value::Array(vec![toml::Value::String(std::mem::take(chord))]);
        }
    }
    // A version-2 file predates every change of defaults. One that spells a
    // shortcut as a single string predates chord lists as well, when column
    // navigation had no arrow chord beside its letter; a list with the letter
    // alone was chosen, since lists shipped with both.
    adopt_shortcut_defaults(
        &mut shortcuts,
        FIRST_SHORTCUT_DEFAULTS_REVISION,
        &SINGLE_CHORD_DEFAULTS
            .into_iter()
            .filter(|(action, _)| scalar.iter().any(|name| name == action))
            .collect::<Vec<_>>(),
    );
    preserve_introduced_shortcuts(&mut shortcuts);
    toml::Value::Table(shortcuts)
        .try_into()
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacySettings {
    schema_version: u32,
    max_hot_cards: usize,
    hibernation_delay_ms: u64,
    terminal_font_size: f32,
    terminal_font_family: String,
    sidebar_width: f32,
    default_column_fraction: f32,
}

impl Default for LegacySettings {
    fn default() -> Self {
        let defaults = AppSettings::default();
        Self {
            schema_version: LEGACY_SETTINGS_SCHEMA_VERSION,
            max_hot_cards: defaults.max_hot_cards,
            hibernation_delay_ms: defaults.hibernation_delay_ms,
            terminal_font_size: defaults.terminal_font_size,
            terminal_font_family: defaults.terminal_font_family,
            sidebar_width: defaults.sidebar_width,
            default_column_fraction: defaults.default_column_fraction,
        }
    }
}

impl From<LegacySettings> for AppSettings {
    fn from(legacy: LegacySettings) -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            max_hot_cards: legacy.max_hot_cards,
            hibernation_delay_ms: legacy.hibernation_delay_ms,
            terminal_font_size: legacy.terminal_font_size,
            terminal_font_family: legacy.terminal_font_family,
            sidebar_width: normalize_legacy_sidebar_width(legacy.sidebar_width),
            default_column_fraction: legacy.default_column_fraction,
            ..Self::default()
        }
    }
}

impl From<V1TomlSettings> for AppSettings {
    fn from(legacy: V1TomlSettings) -> Self {
        debug_assert_eq!(legacy.schema_version, LEGACY_SETTINGS_SCHEMA_VERSION);
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            max_hot_cards: legacy.max_hot_cards,
            hibernation_delay_ms: legacy.hibernation_delay_ms,
            terminal_font_size: legacy.terminal_font_size,
            terminal_font_family: legacy.terminal_font_family,
            sidebar_width: normalize_legacy_sidebar_width(legacy.sidebar_width),
            default_column_fraction: legacy.default_column_fraction,
            ..Self::default()
        }
    }
}

impl From<V2TomlSettings> for AppSettings {
    fn from(previous: V2TomlSettings) -> Self {
        debug_assert_eq!(previous.schema_version, PREVIOUS_SETTINGS_SCHEMA_VERSION);
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            max_hot_cards: previous.max_hot_cards,
            hibernation_delay_ms: previous.hibernation_delay_ms,
            terminal_font_size: previous.terminal_font_size,
            terminal_font_family: previous.terminal_font_family,
            sidebar_width: previous.sidebar_width,
            inspector_width: layout_metrics::INSPECTOR_DEFAULT_WIDTH,
            default_column_fraction: previous.default_column_fraction,
            terminal_scrollback_mib: legacy_scrollback_mib(previous.terminal_scrollback_lines),
            link_open_destination: previous.link_open_destination,
            link_open_gesture: previous.link_open_gesture,
            // A version-2 file predates language servers, so it can hold no
            // preference to carry: migration adopts the built-in list, which
            // is what a file that omits the key gets as well.
            language_servers: default_language_servers(),
            keyboard_shortcuts: previous.keyboard_shortcuts,
            ..Self::default()
        }
    }
}

fn normalize_legacy_sidebar_width(width: f32) -> f32 {
    if (LEGACY_SIDEBAR_WIDTH_MIN..=LEGACY_SIDEBAR_WIDTH_MAX).contains(&width) {
        width.clamp(EXPANDED_WIDTH_MIN, EXPANDED_WIDTH_MAX)
    } else {
        width
    }
}

impl AppSettings {
    pub fn editor_font_size(&self) -> f32 {
        self.editor_font_size.unwrap_or(self.terminal_font_size)
    }

    pub fn editor_font_family(&self) -> &str {
        self.editor_font_family
            .as_deref()
            .unwrap_or(&self.terminal_font_family)
    }

    /// A modified click explicitly selects a browser; ordinary links use the preference.
    pub fn link_destination(&self, gesture: Option<LinkOpenGesture>) -> LinkOpenDestination {
        match gesture {
            None => self.link_open_destination,
            Some(gesture) if gesture == self.link_open_gesture => {
                LinkOpenDestination::SystemDefault
            }
            Some(_) => LinkOpenDestination::IntegratedBrowser,
        }
    }
    pub fn validate(mut self) -> Result<Self, SettingsError> {
        let mut seen = std::collections::HashSet::new();
        self.palette_recents
            .retain(|key| !key.is_empty() && key.len() <= 128 && seen.insert(key.clone()));
        self.palette_recents.truncate(24);
        let search_url = url::Url::parse(&self.browser_search_url.replace("{query}", "test"));
        if !self.browser_search_url.contains("{query}")
            || !search_url
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        {
            return Err(SettingsError::Invalid(
                "browser_search_url must be an HTTP(S) URL containing {query}",
            ));
        }
        if let crate::terminal::TerminalTheme::Custom(palette) = self.terminal_theme {
            // Keep custom colors when selecting another preset, and refresh
            // the active palette after the configuration file is edited.
            let palette = *self.custom_terminal_theme.get_or_insert(palette);
            self.terminal_theme = crate::terminal::TerminalTheme::Custom(palette);
        }
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(SettingsError::UnsupportedSchema(self.schema_version));
        }
        if self.max_hot_cards == 0
            || u64::try_from(self.max_hot_cards).map_or(true, |value| value > MAX_TOML_INTEGER)
        {
            return Err(SettingsError::Invalid(
                "max_hot_cards must be between 1 and 9223372036854775807",
            ));
        }
        if self.hibernation_delay_ms > MAX_TOML_INTEGER {
            return Err(SettingsError::Invalid(
                "hibernation_delay_ms must not exceed 9223372036854775807",
            ));
        }
        if !(1..=crate::work::MAX_STALE_AFTER_DAYS).contains(&self.work_stale_after_days) {
            return Err(SettingsError::Invalid(
                "work_stale_after_days must be between 1 and 365",
            ));
        }
        if !(0.8..=1.5).contains(&self.ui_scale) {
            return Err(SettingsError::Invalid(
                "ui_scale must be between 0.8 and 1.5",
            ));
        }
        if !(1.0..=2.0).contains(&self.terminal_line_height) {
            return Err(SettingsError::Invalid(
                "terminal_line_height must be between 1.0 and 2.0",
            ));
        }
        if !(0.0..=24.0).contains(&self.terminal_padding) {
            return Err(SettingsError::Invalid(
                "terminal_padding must be between 0 and 24",
            ));
        }
        if !(1..=MAX_TERMINAL_SCROLL_SIZE).contains(&self.terminal_scroll_size) {
            return Err(SettingsError::Invalid(
                "terminal_scroll_size must be between 1 and 10",
            ));
        }
        if !(0.25..=4.0).contains(&self.terminal_scroll_speed) {
            return Err(SettingsError::Invalid(
                "terminal_scroll_speed must be between 0.25 and 4",
            ));
        }
        if !(8.0..=40.0).contains(&self.terminal_font_size) {
            return Err(SettingsError::Invalid(
                "terminal_font_size must be between 8 and 40",
            ));
        }
        if self
            .editor_font_size
            .is_some_and(|size| !(8.0..=40.0).contains(&size))
        {
            return Err(SettingsError::Invalid(
                "editor_font_size must be between 8 and 40",
            ));
        }
        if self.terminal_font_family.len() > MAX_TERMINAL_FONT_FAMILY_BYTES {
            return Err(SettingsError::Invalid(
                "terminal_font_family must not exceed 128 bytes",
            ));
        }
        if self
            .editor_font_family
            .as_ref()
            .is_some_and(|family| family.len() > 128)
        {
            return Err(SettingsError::Invalid(
                "editor_font_family must not exceed 128 bytes",
            ));
        }
        if !self.terminal_font_family.is_empty() && self.terminal_font_family.trim().is_empty() {
            return Err(SettingsError::Invalid(
                "terminal_font_family must be a family name or empty",
            ));
        }
        if !(EXPANDED_WIDTH_MIN..=EXPANDED_WIDTH_MAX).contains(&self.sidebar_width) {
            return Err(SettingsError::SidebarWidth {
                min: EXPANDED_WIDTH_MIN,
                max: EXPANDED_WIDTH_MAX,
            });
        }
        if !(layout_metrics::INSPECTOR_MIN_WIDTH..=layout_metrics::INSPECTOR_MAX_WIDTH)
            .contains(&self.inspector_width)
        {
            return Err(SettingsError::Invalid(
                "inspector_width must be between 320 and 480",
            ));
        }
        if !(0.25..=1.0).contains(&self.chat_content_fraction)
            || !(0.25..=1.0).contains(&self.default_native_pane_fraction)
        {
            return Err(SettingsError::Invalid(
                "Native pane and chat content widths must be between 0.25 and 1.0".into(),
            ));
        }
        if !(0.25..=1.0).contains(&self.default_column_fraction) {
            return Err(SettingsError::Invalid(
                "default_column_fraction must be between 0.25 and 1.0",
            ));
        }
        if self.terminal_scrollback_mib > MAX_TERMINAL_SCROLLBACK_MIB {
            return Err(SettingsError::Invalid(
                "terminal_scrollback_mib must be between 0 and 1024",
            ));
        }
        if self.managed_worktree_directory.trim().is_empty()
            || self
                .managed_worktree_directory
                .chars()
                .any(char::is_control)
        {
            return Err(SettingsError::Invalid(
                "managed_worktree_directory must be a nonempty path without control characters",
            ));
        }
        if !(1..=64).contains(&self.lsp_max_sessions) {
            return Err(SettingsError::Invalid(
                "lsp_max_sessions must be between 1 and 64",
            ));
        }
        if !(1..=604800).contains(&self.lsp_idle_timeout_seconds) {
            return Err(SettingsError::Invalid(
                "lsp_idle_timeout_seconds must be between 1 and 604800",
            ));
        }
        self.keyboard_shortcuts.validate()?;
        validate_language_servers(&self.language_servers)?;
        Ok(self)
    }
}

/// Validates the flattened language-server mapping. An empty list is valid and
/// disables LSP. Nothing here touches the filesystem, resolves an executable,
/// or invokes a shell.
fn validate_language_servers(servers: &[LanguageServerSettings]) -> Result<(), SettingsError> {
    let mut identifiers: HashSet<&str> = HashSet::new();
    let mut extension_owners: HashMap<&str, &str> = HashMap::new();

    for server in servers {
        if server.initialization_options.as_ref().is_some_and(|value| {
            serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 1024 * 1024)
        }) {
            return Err(SettingsError::LanguageServer(format!(
                "language server {} initialization_options exceed 1 MiB",
                server.id
            )));
        }
        let id = server.id.as_str();
        if id.is_empty() {
            return Err(SettingsError::LanguageServer(
                "language_servers entry has an empty id".to_owned(),
            ));
        }
        if !identifiers.insert(id) {
            return Err(SettingsError::LanguageServer(format!(
                "language server id {id} is declared more than once"
            )));
        }
        if server.command.is_empty() {
            return Err(SettingsError::LanguageServer(format!(
                "language server {id} must declare a non-empty command"
            )));
        }
        if server.command[0].is_empty() {
            return Err(SettingsError::LanguageServer(format!(
                "language server {id} must name an executable in command[0]"
            )));
        }
        if server.languages.is_empty() {
            return Err(SettingsError::LanguageServer(format!(
                "language server {id} must declare at least one language"
            )));
        }
        for marker in &server.root_markers {
            if !is_relative_marker(marker) {
                return Err(SettingsError::LanguageServer(format!(
                    "language server {id} root marker {marker:?} must be a relative path without empty, . or .. components"
                )));
            }
        }
        for language in &server.languages {
            let language_id = language.language_id.as_str();
            if language_id.is_empty() {
                return Err(SettingsError::LanguageServer(format!(
                    "language server {id} has a language with an empty language_id"
                )));
            }
            if language.extensions.is_empty() {
                return Err(SettingsError::LanguageServer(format!(
                    "language server {id} language {language_id} must declare at least one extension"
                )));
            }
            for extension in &language.extensions {
                if extension.is_empty() {
                    return Err(SettingsError::LanguageServer(format!(
                        "language server {id} language {language_id} has an empty extension"
                    )));
                }
                if extension.starts_with('.') {
                    return Err(SettingsError::LanguageServer(format!(
                        "language server {id} extension {extension} must not start with a dot"
                    )));
                }
                // `is_uppercase` is the Unicode `Uppercase` property, which
                // excludes titlecase characters such as U+01C5. Comparing the
                // scalar-wise lowercase mapping is the exact test and, unlike
                // `str::to_lowercase`, allocates nothing.
                if !extension
                    .chars()
                    .flat_map(char::to_lowercase)
                    .eq(extension.chars())
                {
                    return Err(SettingsError::LanguageServer(format!(
                        "language server {id} extension {extension} must be lowercase"
                    )));
                }
                // Repeating an extension under one server is redundant but
                // unambiguous: the first declaration wins. Two servers claiming
                // it is genuinely ambiguous and is rejected.
                if let Some(owner) = extension_owners.insert(extension.as_str(), id)
                    && owner != id
                {
                    return Err(SettingsError::LanguageServer(format!(
                        "extension {extension} is claimed by both {owner} and {id}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// A root marker names a relative path below the project root. Splitting on the
/// separator rejects an empty marker, an absolute path, a trailing or doubled
/// separator, and any `.` or `..` segment, so discovery can never be pointed
/// out of the worktree.
fn is_relative_marker(marker: &str) -> bool {
    marker
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("cagents configuration directory is unavailable")]
    DirectoryUnavailable,
    #[error("failed to access settings: {0}")]
    Io(#[from] io::Error),
    #[error("failed to decode TOML settings: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("TOML settings are not valid UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("failed to encode TOML settings: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("failed to decode legacy JSON settings: {0}")]
    LegacyJson(#[from] serde_json::Error),
    #[error("settings use unsupported schema version {0}; expected 3")]
    UnsupportedSchema(u32),
    #[error("invalid settings: {0}")]
    Invalid(&'static str),
    #[error("invalid settings: {0}")]
    InvalidShortcut(&'static str),
    /// Quotes the sidebar's own bounds rather than repeating them, because a
    /// width this accepted used to render clamped to a narrower range.
    #[error("invalid settings: sidebar_width must be between {min} and {max}")]
    SidebarWidth { min: f32, max: f32 },
    #[error("invalid settings: {0}")]
    LanguageServer(String),
}

pub fn default_path() -> Result<PathBuf, SettingsError> {
    instance::settings_path().ok_or(SettingsError::DirectoryUnavailable)
}

fn settings_lock(path: &Path) -> Result<File, SettingsError> {
    use std::os::fd::AsRawFd;
    let parent = settings_parent(path)?;
    prepare_private_directory(parent)?;
    let lock_path = path.with_extension("toml.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)?;
    validate_owned_regular_file(&lock_path, &lock.metadata()?)?;
    // SAFETY: the returned file owns the locked descriptor until dropped.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(lock)
}

/// Applies only fields this window changed since its last successful save.
/// Nested shortcut tables merge by action; list-valued preferences are atomic.
pub fn save_merged(
    path: impl AsRef<Path>,
    base: &AppSettings,
    desired: &AppSettings,
) -> Result<(), SettingsError> {
    let path = path.as_ref();
    let _lock = settings_lock(path)?;
    let current = load_unlocked(path)?.settings;
    let mut current = toml::Value::try_from(current)?;
    merge_changed_fields(
        &toml::Value::try_from(base)?,
        &toml::Value::try_from(desired)?,
        &mut current,
    );
    save_unlocked(path, &current.try_into::<AppSettings>()?.validate()?)
}

fn merge_changed_fields(base: &toml::Value, desired: &toml::Value, current: &mut toml::Value) {
    if base == desired {
        return;
    }
    if let (Some(base), Some(desired), Some(current)) =
        (base.as_table(), desired.as_table(), current.as_table_mut())
    {
        for (key, value) in desired {
            match (base.get(key), current.get_mut(key)) {
                (Some(previous), Some(current)) => merge_changed_fields(previous, value, current),
                _ => {
                    current.insert(key.clone(), value.clone());
                }
            }
        }
        for key in base.keys().filter(|key| !desired.contains_key(*key)) {
            current.remove(key);
        }
    } else {
        *current = desired.clone();
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<AppSettings, SettingsError> {
    load_detailed(path).map(|loaded| loaded.settings)
}

/// Loads settings along with what the file holds that this build set aside.
pub fn load_detailed(path: impl AsRef<Path>) -> Result<Loaded, SettingsError> {
    let path = path.as_ref();
    let _lock = settings_lock(path)?;
    let loaded = load_unlocked(path)?;
    for setting in &loaded.foreign {
        setting.warn();
    }
    Ok(loaded)
}

fn load_unlocked(path: &Path) -> Result<Loaded, SettingsError> {
    reject_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => load_toml(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            migrate_legacy(path).map(Loaded::complete)
        }
        Err(error) => Err(error.into()),
    }
}

pub fn save(path: impl AsRef<Path>, settings: &AppSettings) -> Result<(), SettingsError> {
    let path = path.as_ref();
    let _lock = settings_lock(path)?;
    save_unlocked(path, settings)
}

fn save_unlocked(path: &Path, settings: &AppSettings) -> Result<(), SettingsError> {
    let settings = settings.clone().validate()?;
    let parent = settings_parent(path)?;
    prepare_private_directory(parent)?;
    let directory = open_private_directory(parent)?;
    reject_unsafe_destination(path)?;

    let mut document = written(&settings)?;
    // Whatever a newer build wrote, and this one cannot represent, survives.
    if let Some(current) = current_document(path)? {
        keep_foreign_settings(&current, &mut document);
    }
    let mut document = toml::to_string_pretty(&document)?;
    if !document.ends_with('\n') {
        document.push('\n');
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(".settings.toml.")
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    validate_owned_regular_file(path, &temporary.as_file().metadata()?)?;
    temporary.as_file_mut().write_all(document.as_bytes())?;
    temporary.as_file_mut().sync_all()?;

    // Check again immediately before the replacement. A pre-existing symbolic
    // link is never accepted as a settings destination.
    reject_unsafe_destination(path)?;
    temporary
        .persist(path)
        .map_err(|error| SettingsError::Io(error.error))?;
    directory.sync_all()?;
    Ok(())
}

fn load_toml(path: &Path) -> Result<Loaded, SettingsError> {
    let bytes = read_private_file(path)?;
    let document = std::str::from_utf8(&bytes)?;
    let settings = match toml::from_str::<SettingsHeader>(document)?.schema_version {
        LEGACY_SETTINGS_SCHEMA_VERSION => {
            AppSettings::from(toml::from_str::<V1TomlSettings>(document)?)
        }
        PREVIOUS_SETTINGS_SCHEMA_VERSION => {
            AppSettings::from(toml::from_str::<V2TomlSettings>(document)?)
        }
        SETTINGS_SCHEMA_VERSION => return load_current_toml(document),
        version => return Err(SettingsError::UnsupportedSchema(version)),
    };
    Ok(Loaded::complete(settings.validate()?))
}

/// Brings a current-schema document up to what this build reads: moves the
/// pre-rename channel shortcut, upgrades a shortcut table written against
/// older defaults (see [`adopt_shortcut_defaults`]), and renames the
/// short-lived scroll-speed key from the first implementation of that
/// preference. Its value represented lines per wheel notch, so it is the same
/// setting; the old default of one is promoted to the new default of two.
fn normalize_current(table: &mut toml::Table) {
    if let Some(legacy) = table.remove("terminal_scrollback_lines") {
        // An explicit current value wins, even when it is unreadable. Retain
        // invalid legacy input under the current key so normal per-setting
        // diagnostics/preservation apply rather than silently dropping it.
        table
            .entry("terminal_scrollback_mib")
            .or_insert_with(|| match legacy {
                toml::Value::Integer(lines)
                    if (0..=MAX_LEGACY_TERMINAL_SCROLLBACK_LINES as i64).contains(&lines) =>
                {
                    usize::try_from(lines).map_or(toml::Value::Integer(lines), |lines| {
                        toml::Value::Integer(legacy_scrollback_mib(lines) as i64)
                    })
                }
                value => value,
            });
    }
    // Decoding moves it too; moving it first lets a setting set aside or
    // kept be found under the name this build writes.
    if let Some(toml::Value::Table(shortcuts)) = table.get_mut("keyboard_shortcuts") {
        adopt_legacy_channel_shortcut(shortcuts);
    }
    // A revision that is not a number this build reads is set aside by
    // decoding, like any unreadable value, and the table left as it is.
    let revision = match table.get("shortcut_defaults_revision") {
        None => Some(FIRST_SHORTCUT_DEFAULTS_REVISION),
        Some(value) => value
            .as_integer()
            .and_then(|revision| u32::try_from(revision).ok()),
    };
    if let Some(revision) = revision.filter(|revision| *revision < SHORTCUT_DEFAULTS_REVISION) {
        if let Some(toml::Value::Table(shortcuts)) = table.get_mut("keyboard_shortcuts") {
            adopt_shortcut_defaults(shortcuts, revision, &[]);
        }
        table.insert(
            "shortcut_defaults_revision".to_owned(),
            i64::from(SHORTCUT_DEFAULTS_REVISION).into(),
        );
    }
    if !table.contains_key("terminal_scroll_size")
        && matches!(
            table.get("terminal_scroll_speed"),
            Some(toml::Value::Integer(_))
        )
        && let Some(value) = table.remove("terminal_scroll_speed")
    {
        let value = match value {
            toml::Value::Integer(1) => toml::Value::Integer(2),
            value => value,
        };
        table.insert("terminal_scroll_size".to_owned(), value);
    }
}

fn load_current_toml(document: &str) -> Result<Loaded, SettingsError> {
    let mut table = toml::from_str::<toml::Table>(document)?;
    normalize_current(&mut table);
    let decoded = decode_current(table)?;
    Ok(Loaded {
        settings: decoded.settings,
        foreign: decoded.foreign,
    })
}

/// Settings as this build writes them: the serializer's own text, read back.
/// A `Value` built directly widens each `f32` to `f64`, so `1.286` would be
/// written, and compared, as `1.2860000133514404`.
fn written(settings: &AppSettings) -> Result<toml::Value, SettingsError> {
    Ok(toml::Value::Table(toml::from_str(&toml::to_string(
        settings,
    )?)?))
}

/// Settings as loaded, with what the file holds that this build left alone.
pub struct Loaded {
    pub settings: AppSettings,
    foreign: Vec<Foreign>,
}

impl Loaded {
    fn complete(settings: AppSettings) -> Self {
        Self {
            settings,
            foreign: Vec::new(),
        }
    }

    /// One line per value in the file that this build could not use, for the
    /// reader. A key this build does not know is expected of a newer build's
    /// file and only logged; a value it knows but cannot use changes what the
    /// app does, so it is said.
    pub fn set_aside(&self) -> impl Iterator<Item = String> + '_ {
        self.foreign
            .iter()
            .filter_map(|setting| match &setting.reason {
                ForeignReason::UnknownKey => None,
                ForeignReason::UnreadableValue(error) => Some(format!(
                    "settings.toml: “{}” was not used ({error}), so its default is in effect. The file keeps the value until the setting is changed here.",
                    dotted(&setting.path)
                )),
            })
    }
}

/// A setting in the file this build could not use, which it leaves as it
/// found it.
#[derive(Clone, Debug, PartialEq)]
struct Foreign {
    path: Vec<Segment>,
    reason: ForeignReason,
}

#[derive(Clone, Debug, PartialEq)]
enum ForeignReason {
    /// A key the settings model has no field for.
    UnknownKey,
    /// A value this build cannot read, such as a preset a newer build added,
    /// or one it reads but does not accept, such as a wider range. The build
    /// uses its own default in its place.
    UnreadableValue(String),
}

impl Foreign {
    fn warn(&self) {
        let setting = dotted(&self.path);
        match &self.reason {
            ForeignReason::UnknownKey => tracing::warn!(
                %setting,
                "settings.toml has a setting this build does not know; it is ignored and kept on save"
            ),
            ForeignReason::UnreadableValue(error) => tracing::warn!(
                %setting,
                %error,
                "settings.toml has a value this build cannot use; the default is used and the value is kept on save"
            ),
        }
    }
}

/// One step of a path into the settings document.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(usize),
}

fn dotted(path: &[Segment]) -> String {
    let mut dotted = String::new();
    for segment in path {
        match segment {
            Segment::Key(key) => {
                if !dotted.is_empty() {
                    dotted.push('.');
                }
                dotted.push_str(key);
            }
            Segment::Index(index) => {
                let _ = write!(dotted, "[{index}]");
            }
        }
    }
    dotted
}

/// A current-schema document as this build understood it.
struct Decoded {
    /// Validated.
    settings: AppSettings,
    foreign: Vec<Foreign>,
    /// Settings the file lacks, each the path to a setting this build filled
    /// in: a top-level setting, or one action of the shortcut table.
    absent: Vec<Vec<Segment>>,
}

/// Decodes and validates a current-schema document, setting aside every key
/// the settings model has no field for and every value it cannot use.
///
/// A setting the file lacks, such as one a newer build dropped, takes its
/// default. A value that does not decode is replaced by its default where the
/// error points, one at a time, and decoding retried; within a list, where a
/// default entry at the same index would describe another entry, an optional
/// key is dropped instead, and otherwise the whole list falls back. A value
/// that decodes but fails validation, such as a range a newer build widened
/// or a chord naming a key it added, falls back the same way; each shortcut
/// action is a setting of its own. Anything else still fails the document.
fn decode_current(table: toml::Table) -> Result<Decoded, SettingsError> {
    let defaults = written(&AppSettings::default())?;
    let mut read = toml::Value::Table(table);
    let mut foreign: Vec<Foreign> = Vec::new();
    let mut absent = Vec::new();
    if let (Some(read), Some(defaults)) = (read.as_table_mut(), defaults.as_table()) {
        for (key, default) in defaults {
            if !read.contains_key(key) {
                absent.push(vec![Segment::Key(key.clone())]);
                read.insert(key.clone(), default.clone());
            }
        }
    }
    // The shortcut table is decoded whole by its own migration, so an action
    // that is not a list of chords is set aside here, before it can cost the
    // table, and an action the file lacks is noted.
    if let Some(toml::Value::Table(shortcuts)) = read.get_mut("keyboard_shortcuts") {
        let defaults = &defaults["keyboard_shortcuts"];
        for action in SHORTCUT_ACTIONS {
            let path = vec![
                Segment::Key("keyboard_shortcuts".into()),
                Segment::Key(action.into()),
            ];
            match shortcuts.get_mut(action) {
                None => absent.push(path),
                Some(value)
                    if !value
                        .as_array()
                        .is_some_and(|chords| chords.iter().all(toml::Value::is_str)) =>
                {
                    *value = defaults[action].clone();
                    foreign.push(Foreign {
                        path,
                        reason: ForeignReason::UnreadableValue("expected a list of chords".into()),
                    });
                }
                Some(_) => {}
            }
        }
    }
    for path in &absent {
        tracing::debug!(setting = %dotted(path), "settings.toml lacks a setting; this build fills it in and writes it only if that is not the plain default");
    }
    let settings = loop {
        let mut unknown = Vec::new();
        let decoded: Result<AppSettings, _> = serde_path_to_error::deserialize(
            serde_ignored::Deserializer::new(read.clone(), &mut |path: serde_ignored::Path<'_>| {
                unknown.extend(ignored_path(&path, &read));
            }),
        );
        // Where a value failed to decode: one that fails around what was just
        // set aside inside it fails because of that, not for its own reason.
        let failed_at = decoded
            .as_ref()
            .err()
            .and_then(|error| error_path(error.path()));
        let (path, reason, error) = match decoded {
            Ok(settings) => {
                let fault = settings.keyboard_shortcuts.fault();
                match (settings.validate(), fault) {
                    (Ok(settings), _) => {
                        foreign.extend(unknown.into_iter().map(|path| Foreign {
                            path,
                            reason: ForeignReason::UnknownKey,
                        }));
                        break settings;
                    }
                    (Err(_), Some(fault)) => (
                        blamed_action(&read, &defaults, &fault),
                        fault.error.to_string(),
                        fault.error,
                    ),
                    (Err(error), None) => (
                        invalid_setting(&read, &defaults, &error),
                        error.to_string(),
                        error,
                    ),
                }
            }
            Err(error) => (
                error_path(error.path()),
                error.inner().to_string(),
                error.into_inner().into(),
            ),
        };
        let Some(path) = path.and_then(|path| set_aside_value(&mut read, &defaults, &path)) else {
            return Err(error);
        };
        // A setting that falls back whole takes with it what was set aside
        // inside it; when setting that aside is why it fell back, its reason
        // is the reason.
        let inside = |setting: &Foreign| setting.path.starts_with(&path);
        let reason = foreign
            .last()
            .filter(|last| {
                inside(last)
                    && failed_at
                        .as_ref()
                        .is_some_and(|failed_at| last.path.starts_with(failed_at))
                    && matches!(last.reason, ForeignReason::UnreadableValue(_))
            })
            .map_or(ForeignReason::UnreadableValue(reason), |last| {
                last.reason.clone()
            });
        foreign.retain(|setting| !inside(setting));
        foreign.push(Foreign { path, reason });
    };

    if let Some(toml::Value::Table(shortcuts)) = read.get("keyboard_shortcuts") {
        foreign.extend(
            shortcuts
                .keys()
                .filter(|key| !SHORTCUT_ACTIONS.contains(&key.as_str()))
                .map(|key| Foreign {
                    path: vec![
                        Segment::Key("keyboard_shortcuts".into()),
                        Segment::Key(key.clone()),
                    ],
                    reason: ForeignReason::UnknownKey,
                }),
        );
    }
    Ok(Decoded {
        settings,
        foreign,
        absent,
    })
}

/// The action a shortcut `fault` is set aside from. Of two actions that share
/// a chord, the one the file set is blamed rather than one this build filled
/// in or fell back to: that one holds its default, which is no way forward.
fn blamed_action(
    read: &toml::Value,
    defaults: &toml::Value,
    fault: &ShortcutFault,
) -> Option<Vec<Segment>> {
    std::iter::once(fault.action)
        .chain(fault.conflicts_with)
        .map(|action| {
            vec![
                Segment::Key("keyboard_shortcuts".into()),
                Segment::Key(SHORTCUT_ACTIONS[action].into()),
            ]
        })
        .find(|path| get(read, path).is_some_and(|value| get(defaults, path) != Some(value)))
}

/// Where serde stopped, as a path into the document. `None` when the error is
/// not about any one value.
fn error_path(path: &serde_path_to_error::Path) -> Option<Vec<Segment>> {
    use serde_path_to_error::Segment as Step;
    let path = path
        .iter()
        .map(|step| match step {
            Step::Map { key } => Some(Segment::Key(key.clone())),
            Step::Seq { index } => Some(Segment::Index(*index)),
            // An externally tagged variant is a key of its own table.
            Step::Enum { variant } => Some(Segment::Key(variant.clone())),
            Step::Unknown => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!path.is_empty()).then_some(path)
}

/// The one top-level setting that fails validation with `error`: the first
/// whose default in its place clears the error or turns it into another.
/// Only a setting that differs from its default can be the cause, and each
/// setting fails validation with a message of its own. The shortcut table
/// says itself which action fails; see `blamed_action`.
fn invalid_setting(
    read: &toml::Value,
    defaults: &toml::Value,
    error: &SettingsError,
) -> Option<Vec<Segment>> {
    let failure = error.to_string();
    read.as_table()?
        .keys()
        .filter(|key| !matches!(key.as_str(), "schema_version" | "keyboard_shortcuts"))
        .map(|key| vec![Segment::Key(key.clone())])
        .filter(|path| get(defaults, path).is_some_and(|default| get(read, path) != Some(default)))
        .find(|path| {
            let mut trial = read.clone();
            if let (Some(value), Some(default)) = (get_mut(&mut trial, path), get(defaults, path)) {
                *value = default.clone();
            }
            trial
                .try_into::<AppSettings>()
                .map_err(SettingsError::from)
                .and_then(AppSettings::validate)
                .map_or_else(|error| error.to_string() != failure, |_| true)
        })
}

/// Takes the value at `path` out of `read` and returns the path of what was
/// set aside. Outside any list, the value's default takes its place; inside
/// one, a default entry at the same index would describe another entry, so an
/// optional key is dropped instead; failing both, the top-level setting that
/// holds the value falls back whole.
fn set_aside_value(
    read: &mut toml::Value,
    defaults: &toml::Value,
    path: &[Segment],
) -> Option<Vec<Segment>> {
    let in_list = path
        .iter()
        .any(|segment| matches!(segment, Segment::Index(_)));
    if !in_list && let Some(default) = get(defaults, path) {
        let value = get_mut(read, path)?;
        // A default that does not read either is no way forward.
        if value == default {
            return None;
        }
        *value = default.clone();
        return Some(path.to_vec());
    }
    if let Some((Segment::Key(key), parent)) = path.split_last()
        && get_mut(read, parent)
            .and_then(toml::Value::as_table_mut)
            .is_some_and(|table| table.remove(key).is_some())
    {
        return Some(path.to_vec());
    }
    let setting = path.get(..1).filter(|setting| setting.len() < path.len())?;
    set_aside_value(read, defaults, setting)
}

/// Where serde ignored a key, as a path into `read`, the document it was
/// decoding. An externally tagged variant's name is the one key of its table.
fn ignored_path(path: &serde_ignored::Path<'_>, read: &toml::Value) -> Option<Vec<Segment>> {
    use serde_ignored::Path;
    match path {
        Path::Root => Some(Vec::new()),
        Path::Map { parent, key } => {
            let mut path = ignored_path(parent, read)?;
            path.push(Segment::Key(key.clone()));
            Some(path)
        }
        Path::Seq { parent, index } => {
            let mut path = ignored_path(parent, read)?;
            path.push(Segment::Index(*index));
            Some(path)
        }
        Path::Some { parent } | Path::NewtypeStruct { parent } => ignored_path(parent, read),
        Path::NewtypeVariant { parent } => {
            let mut path = ignored_path(parent, read)?;
            let variant = get(read, &path)?
                .as_table()
                .filter(|table| table.len() == 1)?
                .keys()
                .next()?
                .clone();
            path.push(Segment::Key(variant));
            Some(path)
        }
    }
}

/// The settings document on disk, when it is one this build writes.
fn current_document(path: &Path) -> Result<Option<toml::Table>, SettingsError> {
    let bytes = match read_private_file(path) {
        Ok(bytes) => bytes,
        Err(SettingsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        // A file this user may not read, or that does not parse, holds
        // nothing this build could keep, and saving over it is how a user
        // recovers from it. The save itself still refuses an unsafe
        // destination, and any other failure to read, which may pass, still
        // fails the save rather than risk writing over a newer schema.
        Err(SettingsError::Io(error)) if error.raw_os_error() == Some(libc::EACCES) => {
            tracing::warn!(%error, "replacing a settings.toml that could not be read");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let parsed = std::str::from_utf8(&bytes)
        .map_err(SettingsError::from)
        .and_then(|text| Ok(toml::from_str::<toml::Table>(text)?));
    let mut table = match parsed {
        Ok(table) => table,
        Err(error) => {
            tracing::warn!(%error, "replacing a settings.toml that could not be read");
            return Ok(None);
        }
    };
    let version = table
        .get("schema_version")
        .and_then(toml::Value::as_integer);
    if version.is_some_and(|version| version > i64::from(SETTINGS_SCHEMA_VERSION)) {
        // Writing this build's schema over a newer one would erase it.
        return Err(SettingsError::UnsupportedSchema(
            version
                .and_then(|version| u32::try_from(version).ok())
                .unwrap_or(u32::MAX),
        ));
    }
    if version != Some(i64::from(SETTINGS_SCHEMA_VERSION)) {
        return Ok(None);
    }
    normalize_current(&mut table);
    Ok(Some(table))
}

/// Copies every foreign setting in `current` into `document`, the settings
/// this build is about to write, and leaves out what this build only filled
/// in.
///
/// A setting this build replaced with its default keeps the file's value only
/// while this build still holds that default: a user who then chose a value
/// here meant it. A key inside a list entry follows the entry by its `id`, so
/// it survives entries being removed or reordered around it; an entry without
/// one keeps its keys only while it is unchanged at the same position. A
/// setting the file lacks stays out of it while this build still holds the
/// plain default it filled in.
fn keep_foreign_settings(current: &toml::Table, document: &mut toml::Value) {
    // What this build made of the file, as it would write it: `document`
    // differs from it exactly where a setting has changed since.
    let (baseline, decoded) = match decode_current(current.clone())
        .and_then(|decoded| Ok((written(&decoded.settings)?, decoded)))
    {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!(%error, "settings.toml could not be read back; settings this build does not know are not kept");
            return;
        }
    };
    let current = toml::Value::Table(current.clone());
    let kept: Vec<(Vec<Segment>, toml::Value)> = decoded
        .foreign
        .iter()
        .filter_map(|setting| {
            let value = get(&current, &setting.path)?.clone();
            let target = locate(&baseline, document, &setting.path)?;
            (matches!(target.last(), Some(Segment::Key(_)))
                && get(document, &target) == get(&baseline, &setting.path))
            .then_some((target, value))
        })
        .collect();
    // Only a plain default stays out: a value a migration filled in, such as
    // an action left unbound because the file gave its chord to another, is
    // written so that it still holds once that chord is free.
    let defaults = written(&AppSettings::default()).ok();
    for path in &decoded.absent {
        if get(document, path) == get(&baseline, path)
            && defaults
                .as_ref()
                .is_some_and(|defaults| get(defaults, path) == get(&baseline, path))
            && let Some((Segment::Key(key), parent)) = path.split_last()
            && let Some(toml::Value::Table(table)) = get_mut(document, parent)
        {
            table.remove(key);
        }
    }
    // Chords the file will bind apart from what is kept. A newer build rejects
    // a file in which two actions share one, so a kept action gives up any
    // chord this build has bound since; kept actions never cost each other,
    // since the file already held them together. A kept action left with no
    // chord is not kept, so the file binds this build's chords for it instead,
    // which other kept actions must then give up in turn.
    let shortcuts = Segment::Key("keyboard_shortcuts".into());
    let is_action =
        |target: &[Segment]| matches!(target, [table, Segment::Key(_)] if *table == shortcuts);
    let mut unkept: Vec<&[Segment]> = Vec::new();
    let bound = loop {
        let bound: Vec<ShortcutBinding> = document
            .get("keyboard_shortcuts")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
            .filter(|(action, _)| {
                !kept.iter().any(|(target, _)| {
                    !unkept.contains(&target.as_slice())
                        && *target == [shortcuts.clone(), Segment::Key((*action).clone())]
                })
            })
            .filter_map(|(_, chords)| chords.as_array())
            .flatten()
            .filter_map(toml::Value::as_str)
            .filter_map(|chord| parse_shortcut(chord).ok())
            .collect();
        let emptied: Vec<&[Segment]> = kept
            .iter()
            .filter(|(target, value)| {
                is_action(target)
                    && !unkept.contains(&target.as_slice())
                    && value
                        .as_array()
                        .is_some_and(|chords| !chords.is_empty() && free(chords, &bound).is_empty())
            })
            .map(|(target, _)| target.as_slice())
            .collect();
        if emptied.is_empty() {
            break bound;
        }
        unkept.extend(emptied);
    };
    for (target, mut value) in kept.iter().cloned() {
        // An action left with no chord is left to the newer build to bind
        // again, which it may insist on.
        if unkept.contains(&target.as_slice()) {
            continue;
        }
        let Some((Segment::Key(key), parent)) = target.split_last() else {
            continue;
        };
        if is_action(&target)
            && let toml::Value::Array(chords) = &mut value
        {
            *chords = free(chords, &bound);
        }
        if let Some(toml::Value::Table(table)) = get_mut(document, parent) {
            table.insert(key.clone(), value);
        }
    }
}

/// The chords of `chords` that nothing in `bound` holds; one this build cannot
/// read is kept, since only a newer build can tell what it binds.
fn free(chords: &[toml::Value], bound: &[ShortcutBinding]) -> Vec<toml::Value> {
    chords
        .iter()
        .filter(|chord| {
            chord
                .as_str()
                .and_then(|chord| parse_shortcut(chord).ok())
                .is_none_or(|binding| !bound.contains(&binding))
        })
        .cloned()
        .collect()
}

/// `path`, a path into `baseline`, as the path to the same setting in
/// `document`.
fn locate(
    baseline: &toml::Value,
    document: &toml::Value,
    path: &[Segment],
) -> Option<Vec<Segment>> {
    let (last, parent) = path.split_last()?;
    let mut located = Vec::with_capacity(path.len());
    let (mut read, mut document) = (baseline, document);
    for segment in parent {
        let (next_read, next_document, segment) = match segment {
            Segment::Key(key) => (
                read.get(key.as_str())?,
                document.get(key.as_str())?,
                segment.clone(),
            ),
            Segment::Index(index) => {
                let entry = read.get(*index)?;
                let entries = document.as_array()?;
                let position = match entry.get("id") {
                    Some(id) => entries
                        .iter()
                        .position(|other| other.get("id") == Some(id))?,
                    None => (entries.get(*index)? == entry).then_some(*index)?,
                };
                (entry, &entries[position], Segment::Index(position))
            }
        };
        read = next_read;
        document = next_document;
        located.push(segment);
    }
    located.push(last.clone());
    Some(located)
}

fn get<'a>(value: &'a toml::Value, path: &[Segment]) -> Option<&'a toml::Value> {
    path.iter().try_fold(value, |value, segment| match segment {
        Segment::Key(key) => value.get(key.as_str()),
        Segment::Index(index) => value.get(*index),
    })
}

fn get_mut<'a>(value: &'a mut toml::Value, path: &[Segment]) -> Option<&'a mut toml::Value> {
    path.iter().try_fold(value, |value, segment| match segment {
        Segment::Key(key) => value.get_mut(key.as_str()),
        Segment::Index(index) => value.get_mut(*index),
    })
}

fn migrate_legacy(path: &Path) -> Result<AppSettings, SettingsError> {
    let legacy_path = path.with_file_name("settings.json");
    reject_symlink_components(&legacy_path)?;
    match fs::symlink_metadata(&legacy_path) {
        Ok(_) => {
            let bytes = read_private_file(&legacy_path)?;
            let legacy = serde_json::from_slice::<LegacySettings>(&bytes)?;
            if legacy.schema_version != LEGACY_SETTINGS_SCHEMA_VERSION {
                return Err(SettingsError::UnsupportedSchema(legacy.schema_version));
            }
            let settings = AppSettings::from(legacy).validate()?;
            save_unlocked(path, &settings)?;
            let _ = fs::remove_file(legacy_path);
            Ok(settings)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AppSettings::default()),
        Err(error) => Err(error.into()),
    }
}

fn settings_parent(path: &Path) -> Result<&Path, SettingsError> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(SettingsError::DirectoryUnavailable)
}

fn read_private_file(path: &Path) -> Result<Vec<u8>, SettingsError> {
    let parent = settings_parent(path)?;
    prepare_private_directory(parent)?;
    reject_symlink_components(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    validate_owned_regular_file(path, &file.metadata()?)?;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn prepare_private_directory(path: &Path) -> io::Result<()> {
    reject_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(PRIVATE_DIRECTORY_MODE);
            builder.create(path)?;
        }
        Err(error) => return Err(error),
    }
    reject_symlink_components(path)?;
    validate_owned_directory(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
    validate_owned_directory(path)
}

fn open_private_directory(path: &Path) -> io::Result<File> {
    reject_symlink_components(path)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(unsafe_path_error(path, "expected a real directory"));
    }
    validate_owner(path, &metadata)?;
    Ok(directory)
}

fn reject_symlink_components(path: &Path) -> io::Result<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(unsafe_path_error(
                    ancestor,
                    "symbolic links are not allowed",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn reject_unsafe_destination(path: &Path) -> io::Result<()> {
    reject_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(unsafe_path_error(path, "symbolic links are not allowed"))
        }
        Ok(metadata) => validate_owned_regular_file(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_owned_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(unsafe_path_error(path, "expected a real directory"));
    }
    validate_owner(path, &metadata)
}

fn validate_owned_regular_file(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(unsafe_path_error(path, "expected a regular file"));
    }
    validate_owner(path, metadata)
}

fn validate_owner(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let expected_uid = effective_uid();
    if metadata.uid() != expected_uid {
        return Err(unsafe_path_error(
            path,
            format!(
                "path is owned by uid {}, expected {expected_uid}",
                metadata.uid()
            ),
        ));
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not modify memory.
    unsafe { libc::geteuid() }
}

fn unsafe_path_error(path: &Path, message: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe settings path {}: {message}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn language_server_initialization_options_round_trip_and_are_bounded() {
        let mut settings = AppSettings::default();
        let options = serde_json::json!({
            "checkOnSave": false,
            "cargo": {"extraArgs": ["--locked", "--offline"]}
        });
        settings.language_servers[0].initialization_options = Some(options.clone());
        let text = toml::to_string(&settings).unwrap();
        let decoded: AppSettings = toml::from_str(&text).unwrap();
        assert_eq!(
            decoded.language_servers[0].initialization_options,
            Some(options)
        );
        assert!(decoded.validate().is_ok());
        settings.language_servers[0].initialization_options =
            Some(serde_json::json!({"oversized": "x".repeat(1024 * 1024)}));
        assert!(settings.validate().is_err());
    }

    #[test]
    fn lsp_lifecycle_settings_reject_unbounded_or_zero_values() {
        for cap in [0, 65, usize::MAX] {
            let settings = AppSettings {
                lsp_max_sessions: cap,
                ..AppSettings::default()
            };
            assert!(settings.validate().is_err());
        }
        for timeout in [0, 604801, u64::MAX] {
            let settings = AppSettings {
                lsp_idle_timeout_seconds: timeout,
                ..AppSettings::default()
            };
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn editor_find_options_round_trip_with_disabled_language_servers() {
        let settings = AppSettings {
            language_servers: Vec::new(),
            editor_find_options: EditorFindOptions {
                case_sensitive: true,
                regex: true,
                whole_word: true,
            },
            editor_replace_preserve_case: true,
            ..AppSettings::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        let restored: AppSettings = toml::from_str(&serialized).unwrap();
        assert!(restored.language_servers.is_empty());
        assert_eq!(restored.editor_find_options, settings.editor_find_options);
        assert!(restored.editor_replace_preserve_case);
    }

    #[test]
    fn usage_preview_selection_round_trips_without_changing_legacy_defaults() {
        let mut previous_schema = toml::Value::try_from(AppSettings::default()).unwrap();
        previous_schema
            .as_table_mut()
            .unwrap()
            .remove("usage_preview_rows");
        let legacy: AppSettings = previous_schema.try_into().unwrap();
        assert!(legacy.usage_preview_rows.is_empty());
        let mut settings = AppSettings::default();
        settings.usage_preview_rows.insert(
            "claude".into(),
            vec![
                "window:five_hour".into(),
                "window:seven_day".into(),
                "window:seven_day_fable".into(),
            ],
        );
        settings
            .usage_preview_rows
            .insert("codex".into(), Vec::new());
        let decoded: AppSettings = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
        assert_eq!(decoded.usage_preview_rows, settings.usage_preview_rows);
    }

    #[test]
    fn automatic_worktree_cleanup_is_opt_in_for_old_and_new_settings() {
        let defaults = super::AppSettings::default();
        assert!(!defaults.auto_delete_merged_worktrees);
        let mut table = toml::Value::try_from(defaults).unwrap();
        table
            .as_table_mut()
            .unwrap()
            .remove("auto_delete_merged_worktrees");
        let legacy: super::AppSettings = table.try_into().unwrap();
        assert!(!legacy.auto_delete_merged_worktrees);
        let mut enabled = legacy;
        enabled.auto_delete_merged_worktrees = true;
        let saved = toml::to_string(&enabled).unwrap();
        let restored: super::AppSettings = toml::from_str(&saved).unwrap();
        assert!(restored.auto_delete_merged_worktrees);
    }

    #[test]
    fn editor_font_inherits_terminal_until_overridden() {
        let mut settings = super::AppSettings::default();
        settings.terminal_font_family = "Test Mono".into();
        settings.terminal_font_size = 19.0;
        assert_eq!(settings.editor_font_family(), "Test Mono");
        assert_eq!(settings.editor_font_size(), 19.0);
        settings.editor_font_family = Some("Editor Mono".into());
        settings.editor_font_size = Some(16.0);
        assert_eq!(settings.editor_font_family(), "Editor Mono");
        assert_eq!(settings.editor_font_size(), 16.0);
        assert!(settings.confirm_multiline_paste);
        assert_eq!(
            settings.terminal_theme,
            crate::terminal::TerminalTheme::Auto
        );
        assert!(settings.validate().is_ok());
    }
    use super::*;

    #[test]
    fn native_chat_preferences_default_and_round_trip() {
        let mut table = toml::Value::try_from(AppSettings::default())
            .unwrap()
            .as_table()
            .unwrap()
            .clone();
        for key in [
            "chat_expand_tool_groups",
            "chat_content_fraction",
            "default_native_pane_fraction",
        ] {
            table.remove(key);
        }
        let legacy: AppSettings = toml::Value::Table(table).try_into().unwrap();
        assert!(!legacy.chat_expand_tool_groups);
        assert_eq!(legacy.chat_content_fraction, 0.5);
        assert_eq!(legacy.default_native_pane_fraction, 0.5);
        let customized = AppSettings {
            chat_expand_tool_groups: true,
            chat_content_fraction: 0.75,
            default_native_pane_fraction: 0.6,
            ..legacy
        };
        let decoded: AppSettings = toml::from_str(&toml::to_string(&customized).unwrap()).unwrap();
        assert_eq!(decoded, customized);
        for invalid in [f32::NAN, 0.0, 1.1] {
            assert!(
                AppSettings {
                    chat_content_fraction: invalid,
                    ..customized.clone()
                }
                .validate()
                .is_err()
            );
            assert!(
                AppSettings {
                    default_native_pane_fraction: invalid,
                    ..customized.clone()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn notification_preferences_default_for_existing_files_and_roundtrip() {
        let mut settings = AppSettings::default();
        let mut old: toml::Table = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
        old.remove("notifications");
        let restored: AppSettings = toml::from_str(&toml::to_string(&old).unwrap()).unwrap();
        assert_eq!(
            restored.notifications,
            crate::notifications::Preferences::default()
        );
        settings.notifications.enabled = false;
        settings.notifications.while_focused = true;
        let restored: AppSettings = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.notifications, settings.notifications);
    }
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn path_in(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().join("settings.toml")
    }

    #[test]
    fn default_panes_are_independent_and_roundtrip_with_stable_keys() {
        for worktree in DefaultPane::ALL {
            for channel in DefaultPane::ALL {
                let settings = AppSettings {
                    default_worktree_pane: worktree,
                    default_channel_pane: channel,
                    ..AppSettings::default()
                };
                let written = toml::to_string(&settings).unwrap();
                assert!(
                    written.contains(&format!("default_worktree_pane = \"{}\"", worktree.key()))
                );
                assert!(written.contains(&format!("default_channel_pane = \"{}\"", channel.key())));
                assert_eq!(toml::from_str::<AppSettings>(&written).unwrap(), settings);
                assert_eq!(DefaultPane::from_key(worktree.key()), Some(worktree));
            }
        }
    }

    #[test]
    fn settings_without_default_panes_open_native_chat_in_both_contexts() {
        let mut previous = toml::Value::try_from(AppSettings::default()).unwrap();
        let table = previous.as_table_mut().unwrap();
        table.remove("default_worktree_pane");
        table.remove("default_channel_pane");
        let restored: AppSettings = previous.try_into().unwrap();
        assert_eq!(restored.default_worktree_pane, DefaultPane::NativeChat);
        assert_eq!(restored.default_channel_pane, DefaultPane::NativeChat);
    }

    #[test]
    fn app_themes_roundtrip_without_changing_editor_or_terminal_palettes() {
        for app_theme in AppTheme::ALL {
            let settings = AppSettings {
                app_theme,
                ..AppSettings::default()
            };
            let restored: AppSettings =
                toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
            assert_eq!(restored, settings);
            assert_eq!(AppTheme::from_key(app_theme.key()), Some(app_theme));
        }
    }

    #[test]
    fn editor_syntax_themes_are_written_with_their_key_and_read_either_spelling() {
        for &theme in EditorSyntaxTheme::ALL {
            let settings = AppSettings {
                editor_syntax_theme: theme,
                ..AppSettings::default()
            };
            let written = toml::to_string(&settings).unwrap();
            assert!(
                written.contains(&format!("editor_syntax_theme = \"{}\"", theme.key())),
                "{written}"
            );
            let restored: AppSettings = toml::from_str(&written).unwrap();
            assert_eq!(restored.editor_syntax_theme, theme);
        }
        let earlier = toml::to_string(&AppSettings::default()).unwrap().replace(
            "editor_syntax_theme = \"auto\"",
            "editor_syntax_theme = \"inspired_git_hub\"",
        );
        let restored: AppSettings = toml::from_str(&earlier).unwrap();
        assert_eq!(
            restored.editor_syntax_theme,
            EditorSyntaxTheme::InspiredGitHub
        );
    }

    #[test]
    fn palette_recents_survive_save_and_old_settings_default_to_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let mut settings = AppSettings::default();
        settings.palette_recents = vec!["command:open_settings".into(), "pane:original".into()];
        save(&path, &settings).unwrap();
        assert_eq!(
            load(&path).unwrap().palette_recents,
            settings.palette_recents
        );
        let mut legacy = toml::Value::try_from(&settings).unwrap();
        legacy.as_table_mut().unwrap().remove("palette_recents");
        let restored: AppSettings = legacy.try_into().unwrap();
        assert!(restored.palette_recents.is_empty());
        settings.palette_recents = vec![
            String::new(),
            "command:open_settings".into(),
            "command:open_settings".into(),
            "x".repeat(129),
        ];
        settings
            .palette_recents
            .extend((0..30).map(|i| format!("pane:{i}")));
        let validated = settings.validate().unwrap();
        assert_eq!(validated.palette_recents.len(), 24);
        assert_eq!(validated.palette_recents[0], "command:open_settings");
        assert_eq!(validated.palette_recents[1], "pane:0");
    }

    #[test]
    fn browser_preference_and_both_gestures_are_independent_and_persist() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for destination in [
            LinkOpenDestination::SystemDefault,
            LinkOpenDestination::IntegratedBrowser,
        ] {
            for system_gesture in [
                LinkOpenGesture::ControlClick,
                LinkOpenGesture::ControlShiftClick,
            ] {
                let mut settings = AppSettings::default();
                settings.link_open_destination = destination;
                settings.link_open_gesture = system_gesture;
                save(&path, &settings).unwrap();
                let settings = load(&path).unwrap();
                assert_eq!(settings.link_destination(None), destination);
                assert_eq!(
                    settings.link_destination(Some(system_gesture)),
                    LinkOpenDestination::SystemDefault
                );
                let other = match system_gesture {
                    LinkOpenGesture::ControlClick => LinkOpenGesture::ControlShiftClick,
                    LinkOpenGesture::ControlShiftClick => LinkOpenGesture::ControlClick,
                };
                assert_eq!(
                    settings.link_destination(Some(other)),
                    LinkOpenDestination::IntegratedBrowser
                );
            }
        }
    }

    #[test]
    fn custom_terminal_palette_survives_preset_switches_and_file_edits() {
        use crate::terminal::{TerminalTheme, ThemeColor};
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let palette = TerminalTheme::Dracula.palette();
        let mut settings = AppSettings {
            terminal_theme: TerminalTheme::Custom(palette),
            ..AppSettings::default()
        }
        .validate()
        .unwrap();
        save(&path, &settings).unwrap();
        settings = load(&path).unwrap();
        assert_eq!(settings.custom_terminal_theme, Some(palette));
        settings.terminal_theme = TerminalTheme::Nord;
        save(&path, &settings).unwrap();
        settings = load(&path).unwrap();
        assert_eq!(settings.custom_terminal_theme, Some(palette));
        settings.terminal_theme = TerminalTheme::Custom(settings.custom_terminal_theme.unwrap());
        save(&path, &settings).unwrap();
        let mut document: toml::Value =
            toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        document["custom_terminal_theme"]["background"] = toml::Value::String("#112233".into());
        fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
        let reloaded = load(&path).unwrap();
        let mut edited = palette;
        edited.background = ThemeColor(0x112233);
        assert_eq!(reloaded.terminal_theme, TerminalTheme::Custom(edited));
        assert_eq!(reloaded.custom_terminal_theme, Some(edited));
    }

    #[test]
    fn concurrent_windows_merge_unrelated_preferences_and_stale_close_is_inert() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let base = AppSettings::default();
        save(&path, &base).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            for (font, theme) in [(Some(19.0), None), (None, Some(AppTheme::Light))] {
                let base = &base;
                let path = &path;
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let mut edited = base.clone();
                    if let Some(font) = font {
                        edited.terminal_font_size = font;
                    }
                    if let Some(theme) = theme {
                        edited.app_theme = theme;
                    }
                    barrier.wait();
                    save_merged(path, base, &edited).unwrap();
                });
            }
        });
        save_merged(&path, &base, &base).unwrap();
        let restored = load(&path).unwrap();
        assert_eq!(restored.terminal_font_size, 19.0);
        assert_eq!(restored.app_theme, AppTheme::Light);
    }

    #[test]
    fn absent_settings_use_defaults_without_creating_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);

        assert_eq!(load(&path).unwrap(), AppSettings::default());
        assert_eq!(load(&path).unwrap().default_column_fraction, 0.5);
        assert!(!path.exists());
    }

    #[test]
    fn toml_round_trip_preserves_every_value() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let settings = AppSettings {
            max_hot_cards: 9,
            hibernation_delay_ms: 1_234,
            app_theme: AppTheme::Light,
            ui_scale: 1.2,
            terminal_line_height: 1.5,
            terminal_padding: 12.0,
            terminal_scroll_size: 4,
            terminal_scroll_speed: 1.75,
            terminal_cursor_style: TerminalCursorStyle::Beam,
            terminal_theme: crate::terminal::TerminalTheme::Light,
            terminal_font_size: 16.5,
            terminal_font_family: "Fira Code".to_owned(),
            sidebar_width: 360.0,
            inspector_width: 404.0,
            default_column_fraction: 0.55,
            managed_worktree_directory: "../managed trees/".to_owned(),
            worktree_sort: WorktreeSort::Oldest,
            focus_follows_mouse: true,
            confirm_terminal_close: false,
            relaunch_agents_on_upgrade: false,
            ..AppSettings::default()
        };

        save(&path, &settings).unwrap();

        assert_eq!(load(&path).unwrap(), settings);
        let decoded: AppSettings = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(decoded, settings);
    }
    #[test]
    fn settings_without_interaction_preferences_keep_defaults_and_custom_width() {
        let mut old = toml::Value::try_from(AppSettings {
            default_column_fraction: 0.65,
            ..AppSettings::default()
        })
        .unwrap();
        let table = old.as_table_mut().unwrap();
        table.remove("confirm_terminal_close");
        table.remove("focus_follows_mouse");
        table.remove("relaunch_agents_on_upgrade");
        let decoded: AppSettings = old.try_into().unwrap();
        assert!(decoded.confirm_terminal_close);
        assert!(!decoded.focus_follows_mouse);
        assert!(decoded.relaunch_agents_on_upgrade);
        assert_eq!(decoded.default_column_fraction, 0.65);
    }

    #[test]
    fn managed_directory_remains_backward_compatible_and_rejects_unusable_paths() {
        let defaults = AppSettings::default();
        let mut old = toml::Value::try_from(&defaults).unwrap();
        old.as_table_mut()
            .unwrap()
            .remove("managed_worktree_directory");
        let decoded: AppSettings = old.try_into().unwrap();
        assert_eq!(decoded.managed_worktree_directory, ".worktrees/");
        for path in ["", "  ", "trees\nelsewhere", "trees\0elsewhere"] {
            assert!(
                AppSettings {
                    managed_worktree_directory: path.into(),
                    ..defaults.clone()
                }
                .validate()
                .is_err()
            );
        }
        let absolute = AppSettings {
            managed_worktree_directory: "/srv/managed trees/".into(),
            ..defaults
        }
        .validate()
        .unwrap();
        let encoded = toml::to_string(&absolute).unwrap();
        assert_eq!(toml::from_str::<AppSettings>(&encoded).unwrap(), absolute);
    }

    #[test]
    fn old_settings_keep_lines_unwrapped_and_wrap_choice_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let defaults = AppSettings::default();
        save(&path, &defaults).unwrap();
        let mut old = toml::Value::try_from(&defaults).unwrap();
        old.as_table_mut().unwrap().remove("editor_wrap_lines");
        old.as_table_mut().unwrap().remove("diff_hide_unchanged");
        fs::write(&path, toml::to_string(&old).unwrap()).unwrap();
        let mut settings = load(&path).unwrap();
        assert!(!settings.editor_wrap_lines);
        assert!(settings.diff_hide_unchanged);
        settings.editor_wrap_lines = true;
        settings.diff_hide_unchanged = false;
        save(&path, &settings).unwrap();
        assert!(load(&path).unwrap().editor_wrap_lines);
        assert!(!load(&path).unwrap().diff_hide_unchanged);
    }

    #[test]
    fn a_file_written_before_the_font_family_key_existed_loads_with_the_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let document = toml::to_string(&AppSettings::default()).unwrap();
        let without_family: String = document
            .lines()
            .filter(|line| !line.starts_with("terminal_font_family"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(!without_family.contains("terminal_font_family"));
        fs::write(&path, without_family).unwrap();

        assert_eq!(load(&path).unwrap(), AppSettings::default());
    }

    #[test]
    fn worktree_changes_icons_default_on_and_persist_when_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let defaults = AppSettings::default();
        let document = toml::to_string(&defaults).unwrap();
        let legacy = document
            .lines()
            .filter(|line| !line.starts_with("show_worktree_changes_icons"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, legacy).unwrap();
        assert!(load(&path).unwrap().show_worktree_changes_icons);
        let disabled = AppSettings {
            show_worktree_changes_icons: false,
            ..defaults
        };
        fs::write(&path, toml::to_string(&disabled).unwrap()).unwrap();
        assert!(!load(&path).unwrap().show_worktree_changes_icons);
    }

    #[test]
    fn existing_settings_keep_preferences_when_appearance_keys_are_absent() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let settings = AppSettings {
            terminal_font_size: 19.0,
            sidebar_width: 350.0,
            ..AppSettings::default()
        };
        let document = toml::to_string(&settings).unwrap();
        let document = document
            .lines()
            .filter(|line| {
                ![
                    "app_theme",
                    "ui_scale",
                    "terminal_line_height",
                    "terminal_padding",
                    "terminal_cursor_style",
                    "terminal_theme",
                ]
                .iter()
                .any(|key| line.starts_with(key))
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, document).unwrap();
        assert_eq!(load(&path).unwrap(), settings);
    }

    #[test]
    fn appearance_rejects_nonfinite_and_out_of_range_geometry() {
        for invalid in [f32::NAN, f32::INFINITY, -1.0, 100.0] {
            assert!(
                AppSettings {
                    ui_scale: invalid,
                    ..AppSettings::default()
                }
                .validate()
                .is_err()
            );
            assert!(
                AppSettings {
                    terminal_line_height: invalid,
                    ..AppSettings::default()
                }
                .validate()
                .is_err()
            );
            assert!(
                AppSettings {
                    terminal_padding: invalid,
                    ..AppSettings::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn version_one_toml_is_upgraded_and_legacy_sidebar_widths_are_normalized() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        // Derived from the constants, not restated: the legacy window strictly
        // contains the rendered range, so its extremes must land on the
        // rendered extremes however either bound is later moved.
        for (legacy_width, expected_width) in [
            (LEGACY_SIDEBAR_WIDTH_MIN, EXPANDED_WIDTH_MIN),
            (LEGACY_SIDEBAR_WIDTH_MAX, EXPANDED_WIDTH_MAX),
        ] {
            fs::write(
                &path,
                format!(
                    "schema_version = 1\nmax_hot_cards = 6\nhibernation_delay_ms = 30000\nterminal_font_size = 14.0\nsidebar_width = {legacy_width}\ndefault_column_fraction = 0.72\n"
                ),
            )
            .unwrap();

            let loaded = load(&path).unwrap();
            assert_eq!(loaded.schema_version, SETTINGS_SCHEMA_VERSION);
            assert_eq!(loaded.sidebar_width, expected_width);
            assert_eq!(loaded.default_column_fraction, 0.72);
            assert!(!loaded.focus_follows_mouse);
            assert_eq!(loaded.keyboard_shortcuts, KeyboardShortcuts::default());
        }
    }

    /// Derives the version-2 document from today's shape rather than pasting a
    /// literal, so a key added to version 3 later cannot pass this test by
    /// simply not appearing in it: the writer has to strip the key here, which
    /// is where the migration decision gets stated.
    #[test]
    fn version_two_migration_preserves_custom_appearance() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        fs::write(
            &path,
            r#"schema_version = 2
max_hot_cards = 6
hibernation_delay_ms = 30000
terminal_font_size = 15.0
sidebar_width = 376.0
default_column_fraction = 0.72
[keyboard_shortcuts]
new_terminal = ["Ctrl+T"]
close_focused_column = ["Ctrl+W"]
search_projects = ["Ctrl+F"]
previous_column = ["Alt+Q", "Alt+Left"]
next_column = ["Alt+W", "Alt+Right"]
cycle_pane_width = ["Alt+R"]
"#,
        )
        .unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(migrated.schema_version, SETTINGS_SCHEMA_VERSION);
        assert_eq!(migrated.sidebar_width, 376.0);
        assert_eq!(migrated.terminal_font_size, 15.0);
        assert_eq!(migrated.default_column_fraction, 0.72);
        assert!(!migrated.focus_follows_mouse);
        // Every shortcut the file held was a default then, so each takes
        // today's default.
        assert_eq!(migrated.keyboard_shortcuts, KeyboardShortcuts::default());
        assert_eq!(
            migrated.shortcut_defaults_revision,
            SHORTCUT_DEFAULTS_REVISION
        );
    }

    #[test]
    fn version_two_scalar_shortcuts_migrate_without_losing_custom_bindings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        fs::write(
            &path,
            r#"schema_version = 2
max_hot_cards = 6
hibernation_delay_ms = 30000
terminal_font_size = 14.0
sidebar_width = 280.0
default_column_fraction = 0.72
[keyboard_shortcuts]
new_terminal = "Ctrl+Shift+T"
previous_column = "Alt+Q"
next_column = ["Alt+W", "Alt+Right"]
"#,
        )
        .unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(migrated.keyboard_shortcuts.new_terminal, ["Ctrl+Shift+T"]);
        assert_eq!(migrated.keyboard_shortcuts.new_channel, ["Ctrl+Shift+G"]);
        assert_eq!(migrated.keyboard_shortcuts.clone_pane, ["Ctrl+Shift+D"]);
        // A single-string Alt+Q was the default then, and the list was the
        // default later; both move.
        assert_eq!(migrated.keyboard_shortcuts.previous_column, ["Ctrl+PageUp"]);
        assert_eq!(migrated.keyboard_shortcuts.next_column, ["Ctrl+PageDown"]);
        assert!(migrated.editor_auto_save);
        assert_eq!(migrated.keyboard_shortcuts.cycle_pane_width, ["Alt+R"]);
        save(&path, &migrated).unwrap();
        assert_eq!(load(&path).unwrap(), migrated);
    }

    #[test]
    fn separating_project_search_migrates_defaults_and_preserves_custom_chords() {
        for (search, occupied, expected_search, expected_find) in [
            (
                vec!["Ctrl+F"],
                "Alt+S",
                vec!["Ctrl+Shift+F"],
                vec!["Ctrl+F"],
            ),
            (vec!["Alt+F"], "Ctrl+F", vec!["Alt+F"], vec![]),
            (
                vec!["Ctrl+F", "Alt+S"],
                "Alt+T",
                vec!["Ctrl+F", "Alt+S"],
                vec![],
            ),
            (vec!["Ctrl+F"], "Ctrl+Shift+F", vec!["Ctrl+F"], vec![]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = path_in(&directory);
            let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
            document.insert("shortcut_defaults_revision".into(), 4.into());
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.remove("find_in_pane");
            shortcuts.insert("search_projects".into(), search.into());
            shortcuts.insert("new_terminal".into(), vec![occupied].into());
            fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
            let loaded = load(&path).unwrap();
            assert_eq!(loaded.keyboard_shortcuts.search_projects, expected_search);
            assert_eq!(loaded.keyboard_shortcuts.find_in_pane, expected_find);
            assert_eq!(loaded.keyboard_shortcuts.new_terminal, [occupied]);
            save(&path, &loaded).unwrap();
            assert_eq!(load(&path).unwrap(), loaded);
        }
    }

    #[test]
    fn pane_find_can_be_explicitly_unbound_and_stays_unbound_on_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let mut settings = AppSettings::default();
        settings.keyboard_shortcuts.find_in_pane.clear();
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).unwrap(), settings);
    }

    #[test]
    fn introducing_pane_cycling_preserves_an_existing_binding_of_its_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for (version, binding) in [(2, "\"Alt+R\""), (3, "[\"Alt+R\"]")] {
            let inspector = if version == 3 {
                "inspector_width = 380.0\n"
            } else {
                ""
            };
            fs::write(&path, format!("schema_version = {version}\nmax_hot_cards = 6\nhibernation_delay_ms = 30000\nterminal_font_size = 14.0\nsidebar_width = 280.0\ndefault_column_fraction = 0.72\n{inspector}[keyboard_shortcuts]\nnew_terminal = {binding}\n")).unwrap();
            let migrated = load(&path).unwrap();
            assert_eq!(migrated.keyboard_shortcuts.new_terminal, ["Alt+R"]);
            assert!(migrated.keyboard_shortcuts.cycle_pane_width.is_empty());
            save(&path, &migrated).unwrap();
            assert_eq!(load(&path).unwrap(), migrated);
        }
    }

    /// A current-schema document as a build before shortcut defaults
    /// revision 2 wrote it: every action on its default of the time, and no
    /// recorded revision.
    fn revision_one_document(path: &Path, extend: impl FnOnce(&mut toml::Table)) {
        let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
        document.remove("shortcut_defaults_revision");
        let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
        shortcuts.remove("find_in_pane");
        for action in SHORTCUT_ACTIONS {
            if let Some(chords) = retired_default(action, FIRST_SHORTCUT_DEFAULTS_REVISION) {
                shortcuts.insert(action.into(), chords.to_vec().into());
            }
        }
        extend(shortcuts);
        fs::write(path, toml::to_string_pretty(&document).unwrap()).unwrap();
    }

    #[test]
    fn shortcuts_on_retired_defaults_move_to_the_current_ones() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        revision_one_document(&path, |_| {});
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, AppSettings::default());

        // Saving anything records the upgrade in the file.
        let desired = AppSettings {
            max_hot_cards: loaded.max_hot_cards + 1,
            ..loaded.clone()
        };
        save_merged(&path, &loaded, &desired).unwrap();
        let file = saved(&path);
        assert_eq!(
            file["shortcut_defaults_revision"],
            i64::from(SHORTCUT_DEFAULTS_REVISION).into()
        );
        assert_eq!(
            file["keyboard_shortcuts"]["close_focused_column"],
            vec!["Ctrl+W"].into()
        );
        assert_eq!(load(&path).unwrap(), desired);
    }

    #[test]
    fn new_tab_default_upgrades_revision_two_without_overriding_user_bindings() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for (previous, terminal, expected) in [
            (
                vec!["Ctrl+Shift+N"],
                "Ctrl+Shift+T",
                vec!["Ctrl+T", "Ctrl+Shift+N"],
            ),
            (vec!["Ctrl+Alt+N"], "Ctrl+Shift+T", vec!["Ctrl+Alt+N"]),
            (vec![], "Ctrl+Shift+T", vec![]),
            (vec!["Ctrl+Shift+N"], "Ctrl+T", vec!["Ctrl+Shift+N"]),
        ] {
            let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
            document.insert("shortcut_defaults_revision".into(), 2.into());
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.insert("new_native_thread".into(), previous.into());
            shortcuts.insert("new_terminal".into(), vec![terminal].into());
            fs::write(&path, toml::to_string_pretty(&document).unwrap()).unwrap();
            let loaded = load(&path).unwrap();
            assert_eq!(loaded.keyboard_shortcuts.new_native_thread, expected);
            assert_eq!(loaded.keyboard_shortcuts.new_terminal, [terminal]);
            assert_eq!(
                loaded.shortcut_defaults_revision,
                SHORTCUT_DEFAULTS_REVISION
            );
            save(&path, &loaded).unwrap();
            assert_eq!(load(&path).unwrap(), loaded);
        }
    }

    #[test]
    fn pane_shortcuts_restore_revision_two_and_three_defaults_without_losing_choices() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for revision in [1, 2, 3, 4] {
            for (close, width) in [
                (vec!["Ctrl+Shift+W"], vec!["Ctrl+Shift+R"]),
                (vec!["Ctrl+Alt+W"], vec!["Alt+Shift+R"]),
                (
                    vec!["Ctrl+W", "Ctrl+Shift+W"],
                    vec!["Alt+R", "Ctrl+Shift+R"],
                ),
                (vec!["Ctrl+Shift+W"], vec![]),
            ] {
                let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
                document.insert("shortcut_defaults_revision".into(), revision.into());
                let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
                shortcuts.insert("close_focused_column".into(), close.clone().into());
                shortcuts.insert("cycle_pane_width".into(), width.clone().into());
                fs::write(&path, toml::to_string_pretty(&document).unwrap()).unwrap();
                let loaded = load(&path).unwrap();
                let expected_close = if matches!(revision, 2 | 3) && close == ["Ctrl+Shift+W"] {
                    vec!["Ctrl+W"]
                } else {
                    close
                };
                let expected_width = if matches!(revision, 2 | 3) && width == ["Ctrl+Shift+R"] {
                    vec!["Alt+R"]
                } else {
                    width
                };
                assert_eq!(
                    loaded.keyboard_shortcuts.close_focused_column, expected_close,
                    "revision {revision}"
                );
                assert_eq!(
                    loaded.keyboard_shortcuts.cycle_pane_width, expected_width,
                    "revision {revision}"
                );
                assert_eq!(
                    loaded.shortcut_defaults_revision,
                    SHORTCUT_DEFAULTS_REVISION
                );
                save(&path, &loaded).unwrap();
                assert_eq!(load(&path).unwrap(), loaded);
            }
        }
    }

    #[test]
    fn introduced_chat_shortcuts_preserve_existing_bindings_and_explicit_unbinding() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
        let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
        shortcuts.remove("chat_account");
        shortcuts.remove("chat_model");
        shortcuts.insert("chat_permissions".into(), Vec::<String>::new().into());
        shortcuts.insert("new_terminal".into(), vec!["Ctrl+Shift+A"].into());
        fs::write(&path, toml::to_string_pretty(&document).unwrap()).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.keyboard_shortcuts.new_terminal, ["Ctrl+Shift+A"]);
        assert!(loaded.keyboard_shortcuts.chat_account.is_empty());
        assert_eq!(loaded.keyboard_shortcuts.chat_model, ["Ctrl+Shift+M"]);
        assert!(loaded.keyboard_shortcuts.chat_permissions.is_empty());
        save(&path, &loaded).unwrap();
        assert_eq!(load(&path).unwrap(), loaded);
    }

    #[test]
    fn restoring_pane_defaults_does_not_steal_another_actions_chords() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for revision in [2, 3] {
            let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
            document.insert("shortcut_defaults_revision".into(), revision.into());
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.insert("close_focused_column".into(), vec!["Ctrl+Shift+W"].into());
            shortcuts.insert("cycle_pane_width".into(), vec!["Ctrl+Shift+R"].into());
            shortcuts.insert("new_channel".into(), vec!["Ctrl+W"].into());
            shortcuts.insert("clone_pane".into(), vec!["Alt+R"].into());
            fs::write(&path, toml::to_string_pretty(&document).unwrap()).unwrap();
            let loaded = load_detailed(&path).unwrap();
            assert_eq!(loaded.set_aside().count(), 0, "revision {revision}");
            let shortcuts = &loaded.settings.keyboard_shortcuts;
            assert_eq!(shortcuts.close_focused_column, ["Ctrl+Shift+W"]);
            assert_eq!(shortcuts.cycle_pane_width, ["Ctrl+Shift+R"]);
            assert_eq!(shortcuts.new_channel, ["Ctrl+W"]);
            assert_eq!(shortcuts.clone_pane, ["Alt+R"]);
            save(&path, &loaded.settings).unwrap();
            assert_eq!(load(&path).unwrap(), loaded.settings);
        }
    }

    #[test]
    fn a_shortcut_the_user_chose_survives_the_move_to_new_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        revision_one_document(&path, |shortcuts| {
            shortcuts.insert("new_terminal".into(), vec!["Ctrl+Alt+T"].into());
            // A subset of the old default is a choice, not the default.
            shortcuts.insert("next_column".into(), vec!["Alt+W"].into());
            shortcuts.insert("cycle_pane_width".into(), Vec::<String>::new().into());
            // The old default in another order or spelling is still it.
            shortcuts.insert("previous_column".into(), vec!["alt+left", "Alt+q"].into());
        });
        let loaded = load(&path).unwrap();
        let shortcuts = &loaded.keyboard_shortcuts;
        assert_eq!(shortcuts.new_terminal, ["Ctrl+Alt+T"]);
        assert_eq!(shortcuts.next_column, ["Alt+W"]);
        assert!(shortcuts.cycle_pane_width.is_empty());
        assert_eq!(shortcuts.previous_column, ["Ctrl+PageUp"]);
        assert_eq!(shortcuts.close_focused_column, ["Ctrl+W"]);

        // Once upgraded, an old default chosen again is a choice too.
        let mut desired = loaded.clone();
        desired.keyboard_shortcuts.close_focused_column = vec!["Ctrl+Shift+W".into()];
        save_merged(&path, &loaded, &desired).unwrap();
        assert_eq!(load(&path).unwrap(), desired);
    }

    #[test]
    fn a_new_default_another_action_already_binds_is_not_taken() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        revision_one_document(&path, |shortcuts| {
            shortcuts.insert("review_toggle_viewed".into(), vec!["Ctrl+PageDown"].into());
        });
        let loaded = load(&path).unwrap();
        let shortcuts = &loaded.keyboard_shortcuts;
        assert_eq!(shortcuts.clone_pane, ["Ctrl+Shift+D"]);
        assert_eq!(shortcuts.close_focused_column, ["Ctrl+W"]);
        assert_eq!(shortcuts.review_toggle_viewed, ["Ctrl+PageDown"]);
        assert_eq!(shortcuts.next_column, ["Alt+W", "Alt+Right"]);
        // The rest move.
        assert_eq!(shortcuts.new_terminal, ["Ctrl+Shift+T"]);
        assert_eq!(shortcuts.previous_column, ["Ctrl+PageUp"]);
        save(&path, &loaded).unwrap();
        assert_eq!(load(&path).unwrap(), loaded);
    }

    #[test]
    fn a_version_two_file_on_old_defaults_moves_to_the_current_ones() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        fs::write(
            &path,
            r#"schema_version = 2
max_hot_cards = 6
hibernation_delay_ms = 30000
terminal_font_size = 14.0
sidebar_width = 280.0
default_column_fraction = 0.5
[keyboard_shortcuts]
new_terminal = "Ctrl+T"
close_focused_column = "Ctrl+W"
search_projects = "Ctrl+F"
previous_column = "Alt+Q"
next_column = ["Alt+W"]
new_thread = "Alt+Shift+G"
"#,
        )
        .unwrap();
        let shortcuts = load(&path).unwrap().keyboard_shortcuts;
        assert_eq!(shortcuts.new_terminal, ["Ctrl+Shift+T"]);
        assert_eq!(shortcuts.close_focused_column, ["Ctrl+W"]);
        assert_eq!(shortcuts.search_projects, ["Ctrl+Shift+F"]);
        // A single string predates chord lists, when Alt+Q alone was the
        // default; a list holding it alone was a choice, since lists shipped
        // with the arrow chord beside it.
        assert_eq!(shortcuts.previous_column, ["Ctrl+PageUp"]);
        assert_eq!(shortcuts.next_column, ["Alt+W"]);
        assert_eq!(shortcuts.new_channel, ["Alt+Shift+G"]);
        assert_eq!(shortcuts.clone_pane, ["Ctrl+Shift+D"]);
    }

    /// A table that leaves an action out had it on its old default, and a
    /// chord chosen around that default must not collide with the new one.
    #[test]
    fn an_action_an_old_table_leaves_out_takes_part_in_the_move() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for (version, chord) in [(2, "\"Ctrl+Shift+W\""), (3, "[\"Ctrl+Shift+W\"]")] {
            let inspector = if version == 3 {
                "inspector_width = 380.0\n"
            } else {
                ""
            };
            fs::write(&path, format!("schema_version = {version}\nmax_hot_cards = 6\nhibernation_delay_ms = 30000\nterminal_font_size = 14.0\nsidebar_width = 280.0\ndefault_column_fraction = 0.5\n{inspector}[keyboard_shortcuts]\nnew_channel = {chord}\n")).unwrap();
            let loaded = load_detailed(&path).unwrap();
            assert_eq!(loaded.set_aside().count(), 0, "version {version}");
            let shortcuts = &loaded.settings.keyboard_shortcuts;
            assert_eq!(shortcuts.new_channel, ["Ctrl+Shift+W"]);
            assert_eq!(shortcuts.close_focused_column, ["Ctrl+W"]);
            assert_eq!(shortcuts.new_terminal, ["Ctrl+Shift+T"]);
            save(&path, &loaded.settings).unwrap();
            assert_eq!(load(&path).unwrap(), loaded.settings);
        }

        // An action that moves cleanly stays out of a sparse table.
        fs::write(
            &path,
            "schema_version = 3\nmax_hot_cards = 6\nhibernation_delay_ms = 30000\nterminal_font_size = 14.0\nsidebar_width = 280.0\ninspector_width = 380.0\ndefault_column_fraction = 0.5\n[keyboard_shortcuts]\nsearch_projects = [\"Alt+S\"]\n",
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.keyboard_shortcuts,
            KeyboardShortcuts {
                search_projects: vec!["Alt+S".into()],
                ..KeyboardShortcuts::default()
            }
        );
    }

    #[test]
    fn a_newer_shortcut_defaults_revision_is_left_as_it_is() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        revision_one_document(&path, |_| {});
        let mut document = saved(&path);
        document.insert(
            "shortcut_defaults_revision".into(),
            i64::from(SHORTCUT_DEFAULTS_REVISION + 1).into(),
        );
        fs::write(&path, toml::to_string_pretty(&document).unwrap()).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.shortcut_defaults_revision,
            SHORTCUT_DEFAULTS_REVISION + 1
        );
        assert_eq!(loaded.keyboard_shortcuts.close_focused_column, ["Ctrl+W"]);
        save(&path, &loaded).unwrap();
        assert_eq!(
            saved(&path)["shortcut_defaults_revision"],
            i64::from(SHORTCUT_DEFAULTS_REVISION + 1).into()
        );
    }

    #[test]
    fn every_retired_default_names_an_action_and_parses() {
        for retired in RETIRED_SHORTCUT_DEFAULTS {
            assert!(
                SHORTCUT_ACTIONS.contains(&retired.action),
                "{}",
                retired.action
            );
            assert!(retired.retired_in <= SHORTCUT_DEFAULTS_REVISION);
            let successor = retired_default(retired.action, retired.retired_in)
                .map(|chords| chords.iter().map(|chord| (*chord).to_owned()).collect())
                .unwrap_or_else(|| default_chords(retired.action));
            assert_ne!(
                successor, retired.chords,
                "{} did not change at revision {}",
                retired.action, retired.retired_in
            );
            for chord in retired.chords {
                parse_shortcut(chord).unwrap();
            }
        }
    }

    #[test]
    fn new_channel_migration_preserves_a_custom_chord_but_rejects_explicit_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut table = toml::Value::try_from(AppSettings::default()).unwrap();
        let shortcuts = table["keyboard_shortcuts"].as_table_mut().unwrap();
        shortcuts.remove("new_channel");
        shortcuts.insert(
            "new_terminal".into(),
            toml::Value::Array(vec![toml::Value::String("Ctrl+Shift+g".into())]),
        );
        fs::write(&path, toml::to_string(&table).unwrap()).unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(migrated.keyboard_shortcuts.new_terminal, ["Ctrl+Shift+g"]);
        assert!(migrated.keyboard_shortcuts.new_channel.is_empty());
        save(&path, &migrated).unwrap();
        assert_eq!(load(&path).unwrap(), migrated);

        let mut conflicting = migrated;
        conflicting.keyboard_shortcuts.new_channel = vec!["Ctrl+Shift+G".into()];
        assert!(matches!(
            conflicting.validate(),
            Err(SettingsError::InvalidShortcut(message)) if message.contains("conflict")
        ));
    }

    #[test]
    fn clone_pane_migration_preserves_owned_chords_and_existing_channel_bindings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for (version, terminal, channel) in [
            (2, "\"Ctrl+Shift+d\"", "\"Ctrl+Shift+T\""),
            (3, "[\"Ctrl+Shift+d\"]", "[\"Ctrl+Shift+T\"]"),
        ] {
            let inspector = if version == 3 {
                "inspector_width = 380.0\n"
            } else {
                ""
            };
            // The fixture spells the action `new_thread` on purpose: that is
            // how every settings file written before the rename names it, at
            // the current schema version as well as at version 2.
            fs::write(&path, format!("schema_version = {version}\nmax_hot_cards = 6\nhibernation_delay_ms = 30000\nterminal_font_size = 14.0\nsidebar_width = 280.0\ndefault_column_fraction = 0.72\n{inspector}[keyboard_shortcuts]\nnew_terminal = {terminal}\nnew_thread = {channel}\n")).unwrap();
            let migrated = load(&path).unwrap();
            assert_eq!(migrated.keyboard_shortcuts.new_terminal, ["Ctrl+Shift+d"]);
            assert_eq!(migrated.keyboard_shortcuts.new_channel, ["Ctrl+Shift+T"]);
            assert!(migrated.keyboard_shortcuts.clone_pane.is_empty());
            save(&path, &migrated).unwrap();
            assert_eq!(load(&path).unwrap(), migrated);

            let mut conflicting = migrated;
            conflicting.keyboard_shortcuts.clone_pane = vec!["Ctrl+Shift+D".into()];
            assert!(matches!(
                conflicting.validate(),
                Err(SettingsError::InvalidShortcut(message)) if message.contains("conflict")
            ));
        }
    }

    #[test]
    fn explicit_creation_shortcuts_survive_loading_even_when_defaults_are_free() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for (clone_pane, new_channel) in [
            (vec!["Alt+Shift+E".into()], vec!["Ctrl+Shift+N".into()]),
            (Vec::new(), Vec::new()),
        ] {
            let mut settings = AppSettings::default();
            settings.keyboard_shortcuts.clone_pane = clone_pane;
            settings.keyboard_shortcuts.new_channel = new_channel;
            settings.keyboard_shortcuts.new_native_thread.clear();
            save(&path, &settings).unwrap();
            assert_eq!(load(&path).unwrap(), settings);
        }
    }

    #[test]
    fn rejects_a_font_family_that_names_nothing() {
        let whitespace_only = AppSettings {
            terminal_font_family: "   ".to_owned(),
            ..AppSettings::default()
        };
        assert!(matches!(
            whitespace_only.validate(),
            Err(SettingsError::Invalid(_))
        ));

        let too_long = AppSettings {
            terminal_font_family: "F".repeat(129),
            ..AppSettings::default()
        };
        assert!(matches!(
            too_long.validate(),
            Err(SettingsError::Invalid(_))
        ));

        let named = AppSettings {
            terminal_font_family: "Fira Code".to_owned(),
            ..AppSettings::default()
        };
        assert!(named.validate().is_ok());
    }

    #[test]
    fn keyboard_shortcuts_are_validated_as_unique_modified_chords() {
        for (chord, key) in [
            ("Ctrl+F1", ShortcutKey::Function(1)),
            ("Alt+F12", ShortcutKey::Function(12)),
            ("Ctrl+Comma", ShortcutKey::Character(',')),
            ("Ctrl+=", ShortcutKey::Character('=')),
            ("Ctrl+-", ShortcutKey::Character('-')),
            ("Ctrl+Plus", ShortcutKey::Character('+')),
        ] {
            assert_eq!(parse_shortcut(chord).unwrap().key, key);
        }
        assert!(parse_shortcut("Ctrl+F13").is_err());
        assert_eq!(
            parse_shortcut("Ctrl+Shift+N").unwrap(),
            ShortcutBinding {
                control: true,
                alt: false,
                shift: true,
                logo: false,
                key: ShortcutKey::Character('n'),
            }
        );
        assert!(parse_shortcut("T").is_err());
        assert_eq!(
            parse_shortcut("Alt+Shift+Tab").unwrap(),
            ShortcutBinding {
                control: false,
                alt: true,
                shift: true,
                logo: false,
                key: ShortcutKey::Tab,
            }
        );
        // Tab alone still belongs to focus traversal inside a card.
        assert!(parse_shortcut("Tab").is_err());
        assert!(parse_shortcut("Ctrl+Alt+Left").is_ok());
        assert!(parse_shortcut("Ctrl+NotAKey").is_err());
        assert_eq!(
            parse_shortcut("ctrl+pageup").unwrap(),
            ShortcutBinding {
                control: true,
                alt: false,
                shift: false,
                logo: false,
                key: ShortcutKey::PageUp,
            }
        );
        assert_eq!(
            parse_shortcut("Ctrl+Shift+PageDown").unwrap().key,
            ShortcutKey::PageDown
        );
        // Shift+PageUp scrolls a terminal's own scrollback.
        assert!(parse_shortcut("Shift+PageUp").is_err());

        // Conflicts are detected across actions and across every chord in an
        // action, not just the first.
        let mut conflicting = AppSettings::default();
        conflicting.keyboard_shortcuts.next_column =
            conflicting.keyboard_shortcuts.previous_column.clone();
        assert!(matches!(
            conflicting.validate(),
            Err(SettingsError::InvalidShortcut(message)) if message.contains("conflict")
        ));

        let mut alternate_conflict = AppSettings::default();
        alternate_conflict
            .keyboard_shortcuts
            .new_terminal
            .push("Ctrl+PageUp".to_owned());
        assert!(matches!(
            alternate_conflict.validate(),
            Err(SettingsError::InvalidShortcut(message)) if message.contains("conflict")
        ));

        // An action bound to nothing would be silently unreachable.
        let mut unbound = AppSettings::default();
        unbound.keyboard_shortcuts.search_projects.clear();
        assert!(matches!(
            unbound.validate(),
            Err(SettingsError::InvalidShortcut(_))
        ));

        assert!(AppSettings::default().validate().is_ok());
    }

    #[test]
    fn new_review_navigation_migration_preserves_existing_bindings() {
        let mut value = toml::Value::try_from(AppSettings::default()).unwrap();
        let shortcuts = value
            .get_mut("keyboard_shortcuts")
            .unwrap()
            .as_table_mut()
            .unwrap();
        for key in [
            "review_next_unviewed",
            "review_next_change",
            "review_previous_change",
        ] {
            shortcuts.remove(key);
        }
        shortcuts.insert("new_terminal".into(), vec!["Alt+J"].into());
        shortcuts.insert("search_projects".into(), vec!["Alt+Shift+Down"].into());
        let migrated: AppSettings = value.try_into().unwrap();
        assert_eq!(migrated.keyboard_shortcuts.review_next_change, ["F7"]);
        assert!(migrated.keyboard_shortcuts.review_next_unviewed.is_empty());
        assert_eq!(
            migrated.keyboard_shortcuts.review_previous_change,
            ["Alt+K", "Shift+F7"]
        );
        migrated.validate().unwrap();
    }

    #[test]
    fn diff_hunk_alias_migration_preserves_custom_bindings_and_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let migrate = |settings: &AppSettings| {
            fs::write(&path, toml::to_string(settings).unwrap()).unwrap();
            load(&path).unwrap()
        };
        let mut previous = AppSettings::default();
        previous.shortcut_defaults_revision = 6;
        previous.keyboard_shortcuts.review_next_change = vec!["Alt+J".into()];
        previous.keyboard_shortcuts.review_previous_change = vec!["Alt+K".into()];
        let migrated = migrate(&previous);
        assert_eq!(
            migrated.keyboard_shortcuts.review_next_change,
            ["Alt+J", "F7"]
        );
        assert_eq!(
            migrated.keyboard_shortcuts.review_previous_change,
            ["Alt+K", "Shift+F7"]
        );
        previous.keyboard_shortcuts.review_next_change = vec!["Alt+L".into()];
        previous.keyboard_shortcuts.new_terminal = vec!["Shift+F7".into()];
        let migrated = migrate(&previous);
        assert_eq!(migrated.keyboard_shortcuts.review_next_change, ["Alt+L"]);
        assert_eq!(
            migrated.keyboard_shortcuts.review_previous_change,
            ["Alt+K"]
        );
        migrated.validate().unwrap();
    }

    #[test]
    fn a_chord_a_card_needs_for_safety_cannot_be_bound() {
        // A bound chord is reserved, so the card never sees it: binding one of
        // these would leave a runaway program with no way to be stopped.
        for chord in ["Ctrl+C", "Ctrl+D", "Ctrl+Z", "Ctrl+S", "Ctrl+Q"] {
            let mut settings = AppSettings::default();
            settings.keyboard_shortcuts.new_terminal = vec![chord.to_owned()];
            assert!(
                matches!(
                    settings.validate(),
                    Err(SettingsError::InvalidShortcut(message)) if message.contains("card")
                ),
                "{chord} was accepted"
            );
        }

        // The gesture only reaches the child as a control character when Ctrl
        // is alone: the shifted and Alt forms send something else, or nothing,
        // and stay bindable.
        for chord in ["Ctrl+Shift+C", "Ctrl+Alt+D", "Super+Z"] {
            let mut settings = AppSettings::default();
            settings.keyboard_shortcuts.new_terminal = vec![chord.to_owned()];
            assert!(settings.validate().is_ok(), "{chord} was refused");
        }
    }

    #[test]
    fn a_chord_line_round_trips_through_the_dialog_representation() {
        let chords = vec!["Alt+Q".to_owned(), "Alt+Left".to_owned()];
        assert_eq!(format_chords(&chords), "Alt+Q, Alt+Left");
        assert_eq!(parse_chords(&format_chords(&chords)), chords);

        // Whitespace and the separator left behind mid-edit are not chords.
        assert_eq!(parse_chords(" Ctrl+T ,, "), vec!["Ctrl+T".to_owned()]);
        assert!(parse_chords("  ").is_empty());
    }

    #[test]
    fn legacy_scrollback_lines_migrate_and_save_only_the_explicit_byte_unit() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for (lines, mib) in [(0, 0), (1, 1), (10_000, 10), (1_000_000, 977)] {
            fs::write(&path, format!(
                "schema_version = 3\nterminal_scrollback_lines = {lines}\nfuture_history = 'keep'\n"
            )).unwrap();
            let loaded = load_detailed(&path).unwrap();
            assert_eq!(loaded.settings.terminal_scrollback_mib, mib);
            assert_eq!(loaded.set_aside().count(), 0);
            save(&path, &loaded.settings).unwrap();
            let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert!(!saved.contains_key("terminal_scrollback_lines"));
            assert_eq!(
                saved["terminal_scrollback_mib"].as_integer(),
                Some(mib as i64)
            );
            assert_eq!(saved["future_history"].as_str(), Some("keep"));
            assert_eq!(load(&path).unwrap().terminal_scrollback_mib, mib);
        }
        assert_eq!(AppSettings::default().terminal_scrollback_mib, 16);
        assert_eq!(
            load_current_toml("schema_version = 3")
                .unwrap()
                .settings
                .terminal_scrollback_mib,
            16
        );
    }

    #[test]
    fn explicit_scrollback_mib_wins_and_invalid_legacy_input_is_not_silent() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for legacy in ["10000", "-1", "1000001", "'unreadable'"] {
            fs::write(&path, format!(
                "schema_version = 3\nterminal_scrollback_lines = {legacy}\nterminal_scrollback_mib = 32\n"
            )).unwrap();
            let loaded = load_detailed(&path).unwrap();
            assert_eq!(loaded.settings.terminal_scrollback_mib, 32);
            assert_eq!(loaded.set_aside().count(), 0);
            save(&path, &loaded.settings).unwrap();
            let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert!(!saved.contains_key("terminal_scrollback_lines"));
            assert_eq!(saved["terminal_scrollback_mib"].as_integer(), Some(32));
        }
        for invalid in ["-1", "1000001", "9223372036854775807", "'unreadable'"] {
            fs::write(
                &path,
                format!("schema_version = 3\nterminal_scrollback_lines = {invalid}\n"),
            )
            .unwrap();
            let loaded = load_detailed(&path).unwrap();
            assert_eq!(loaded.settings.terminal_scrollback_mib, 16);
            assert_eq!(loaded.set_aside().count(), 1, "{invalid}");
            save(&path, &loaded.settings).unwrap();
            let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            let original: toml::Table = toml::from_str(&format!("value = {invalid}")).unwrap();
            assert_eq!(saved["terminal_scrollback_mib"], original["value"]);
            assert!(!saved.contains_key("terminal_scrollback_lines"));
        }
        let invalid_current = load_current_toml(
            "schema_version = 3\nterminal_scrollback_lines = 10000\nterminal_scrollback_mib = -1\n",
        )
        .unwrap();
        assert_eq!(invalid_current.settings.terminal_scrollback_mib, 16);
        assert_eq!(invalid_current.set_aside().count(), 1);
    }

    #[test]
    fn version_two_scrollback_migration_keeps_legacy_bounds_and_zero() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for (lines, expected) in [
            (0, Some(0)),
            (10_000, Some(10)),
            (1_000_000, Some(977)),
            (1_000_001, None),
        ] {
            fs::write(&path, format!(
                "schema_version = 2\nmax_hot_cards = 8\nhibernation_delay_ms = 30000\nterminal_font_size = 14.0\nsidebar_width = 260.0\ndefault_column_fraction = 0.5\nterminal_scrollback_lines = {lines}\n"
            )).unwrap();
            match expected {
                Some(mib) => assert_eq!(load(&path).unwrap().terminal_scrollback_mib, mib),
                None => assert!(matches!(load(&path), Err(SettingsError::Invalid(_)))),
            }
        }
    }

    #[test]
    fn terminal_scrollback_mib_is_bounded() {
        let at_limit = AppSettings {
            terminal_scrollback_mib: MAX_TERMINAL_SCROLLBACK_MIB,
            ..AppSettings::default()
        };
        assert!(at_limit.validate().is_ok());
        let above_limit = AppSettings {
            terminal_scrollback_mib: MAX_TERMINAL_SCROLLBACK_MIB + 1,
            ..AppSettings::default()
        };
        assert!(matches!(
            above_limit.validate(),
            Err(SettingsError::Invalid(_))
        ));
    }

    #[test]
    fn terminal_scroll_size_defaults_to_two_and_is_bounded() {
        assert_eq!(AppSettings::default().terminal_scroll_size, 2);
        assert!(
            AppSettings {
                terminal_scroll_size: MAX_TERMINAL_SCROLL_SIZE,
                ..AppSettings::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            AppSettings {
                terminal_scroll_size: 0,
                ..AppSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            AppSettings {
                terminal_scroll_size: MAX_TERMINAL_SCROLL_SIZE + 1,
                ..AppSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn terminal_scroll_speed_defaults_to_one_and_is_bounded() {
        assert_eq!(AppSettings::default().terminal_scroll_speed, 1.0);
        for speed in [0.25, 4.0] {
            assert!(
                AppSettings {
                    terminal_scroll_speed: speed,
                    ..AppSettings::default()
                }
                .validate()
                .is_ok()
            );
        }
        for speed in [0.0, 4.01] {
            assert!(
                AppSettings {
                    terminal_scroll_speed: speed,
                    ..AppSettings::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn old_scroll_speed_setting_migrates_to_scroll_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let document = toml::to_string(&AppSettings::default()).unwrap();
        let old_base = document
            .replace("terminal_scroll_speed = 1.0\n", "")
            .replace("terminal_scroll_size = 2\n", "");
        let old_default = old_base.replace(
            "terminal_padding",
            "terminal_scroll_speed = 1\nterminal_padding",
        );
        fs::write(&path, old_default).unwrap();
        assert_eq!(load(&path).unwrap().terminal_scroll_size, 2);

        let old_custom = old_base.replace(
            "terminal_padding",
            "terminal_scroll_speed = 4\nterminal_padding",
        );
        fs::write(&path, old_custom).unwrap();
        assert_eq!(load(&path).unwrap().terminal_scroll_size, 4);

        let current =
            document.replace("terminal_scroll_speed = 1.0", "terminal_scroll_speed = 2.5");
        fs::write(&path, &current).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.terminal_scroll_size, 2);
        assert_eq!(loaded.terminal_scroll_speed, 2.5);
        fs::write(&path, current.replace("terminal_scroll_size = 2\n", "")).unwrap();
        assert_eq!(load(&path).unwrap().terminal_scroll_speed, 2.5);
    }

    #[cfg(feature = "ui")]
    #[test]
    fn the_accepted_sidebar_width_is_exactly_the_width_the_pane_renders() {
        use crate::sidebar::{SidebarMode, effective_width};

        let with_width = |sidebar_width| AppSettings {
            sidebar_width,
            ..AppSettings::default()
        };

        // A width outside the pane's bounds used to validate and then render
        // clamped, so the file said 200 while the sidebar showed 240.
        for width in [
            EXPANDED_WIDTH_MIN - 40.0,
            EXPANDED_WIDTH_MIN - 0.5,
            EXPANDED_WIDTH_MAX + 0.5,
            f32::NAN,
        ] {
            assert!(
                matches!(
                    with_width(width).validate(),
                    Err(SettingsError::SidebarWidth { .. })
                ),
                "a width of {width} cannot be persisted: the pane will not render it"
            );
        }

        for width in [
            EXPANDED_WIDTH_MIN,
            AppSettings::default().sidebar_width,
            EXPANDED_WIDTH_MAX,
        ] {
            let accepted = with_width(width)
                .validate()
                .expect("a width the pane renders must validate");
            assert_eq!(
                effective_width(SidebarMode::Expanded, accepted.sidebar_width),
                width
            );
        }

        let message = SettingsError::SidebarWidth {
            min: EXPANDED_WIDTH_MIN,
            max: EXPANDED_WIDTH_MAX,
        }
        .to_string();
        assert!(
            message.contains(&EXPANDED_WIDTH_MIN.to_string())
                && message.contains(&EXPANDED_WIDTH_MAX.to_string()),
            "the rejection must quote the bounds it enforces, not a copy of them: {message}"
        );
    }

    #[test]
    fn inspector_width_round_trips_only_within_rendered_bounds() {
        for width in [
            layout_metrics::INSPECTOR_MIN_WIDTH,
            layout_metrics::INSPECTOR_DEFAULT_WIDTH,
            layout_metrics::INSPECTOR_MAX_WIDTH,
        ] {
            let settings = AppSettings {
                inspector_width: width,
                ..AppSettings::default()
            }
            .validate()
            .unwrap();
            let directory = tempfile::tempdir().unwrap();
            let path = path_in(&directory);
            save(&path, &settings).unwrap();
            assert_eq!(load(&path).unwrap().inspector_width, width);
        }
        for width in [
            layout_metrics::INSPECTOR_MIN_WIDTH - 1.0,
            layout_metrics::INSPECTOR_MAX_WIDTH + 1.0,
            f32::NAN,
        ] {
            assert!(
                AppSettings {
                    inspector_width: width,
                    ..AppSettings::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn legacy_json_is_migrated_only_when_toml_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let legacy_path = directory.path().join("settings.json");
        let settings = AppSettings {
            terminal_font_size: 18.0,
            default_column_fraction: 0.65,
            ..AppSettings::default()
        };
        fs::write(
            &legacy_path,
            br#"{"terminal_font_size":18.0,"default_column_fraction":0.65}"#,
        )
        .unwrap();

        assert_eq!(load(&path).unwrap(), settings);
        assert!(path.is_file());
        assert!(!legacy_path.exists());
        assert_eq!(load(&path).unwrap(), settings);
    }

    #[test]
    fn existing_toml_wins_and_leaves_legacy_json_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let legacy_path = directory.path().join("settings.json");
        let canonical = AppSettings {
            terminal_font_size: 19.0,
            ..AppSettings::default()
        };
        let legacy = AppSettings {
            terminal_font_size: 11.0,
            ..AppSettings::default()
        };
        fs::write(&path, toml::to_string(&canonical).unwrap()).unwrap();
        fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert_eq!(load(&path).unwrap(), canonical);
        assert!(legacy_path.exists());
    }

    #[test]
    fn malformed_canonical_toml_never_falls_back_to_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let legacy_path = directory.path().join("settings.json");
        fs::write(&path, b"not = [valid").unwrap();
        fs::write(
            &legacy_path,
            serde_json::to_vec_pretty(&AppSettings::default()).unwrap(),
        )
        .unwrap();

        assert!(matches!(load(&path), Err(SettingsError::Toml(_))));
        assert!(legacy_path.exists());
    }

    /// A settings file as a newer build writes it: this build's defaults
    /// plus whatever `extend` adds.
    fn newer_build_document(path: &Path, extend: impl FnOnce(&mut toml::Table)) {
        let mut document = toml::Table::try_from(AppSettings::default()).unwrap();
        extend(&mut document);
        fs::write(path, toml::to_string_pretty(&document).unwrap()).unwrap();
    }

    fn saved(path: &Path) -> toml::Table {
        toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn server_mut(document: &mut toml::Table, index: usize) -> &mut toml::Table {
        document["language_servers"].as_array_mut().unwrap()[index]
            .as_table_mut()
            .unwrap()
    }

    #[test]
    fn a_newer_builds_keys_load_and_survive_an_older_builds_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document.insert("future_preference".into(), true.into());
            let mut section = toml::Table::new();
            section.insert("depth".into(), 2.into());
            document.insert("future_section".into(), section.into());
            document["keyboard_shortcuts"]
                .as_table_mut()
                .unwrap()
                .insert("future_action".into(), vec!["Alt+F12"].into());
            server_mut(document, 0).insert("future_option".into(), 3.into());
            server_mut(document, 1)["languages"].as_array_mut().unwrap()[0]
                .as_table_mut()
                .unwrap()
                .insert("future_flag".into(), true.into());
        });

        let loaded = load(&path).unwrap();
        assert_eq!(loaded, AppSettings::default());

        // This build changes one of its own settings; everything it did not
        // understand is written back where it was.
        let desired = AppSettings {
            max_hot_cards: loaded.max_hot_cards + 1,
            ..loaded.clone()
        };
        save_merged(&path, &loaded, &desired).unwrap();
        let file = saved(&path);
        assert_eq!(file["future_preference"], true.into());
        assert_eq!(file["future_section"]["depth"], 2.into());
        assert_eq!(
            file["keyboard_shortcuts"]["future_action"],
            vec!["Alt+F12"].into()
        );
        assert_eq!(file["language_servers"][0]["future_option"], 3.into());
        assert_eq!(
            file["language_servers"][1]["languages"][0]["future_flag"],
            true.into()
        );
        assert_eq!(load(&path).unwrap(), desired);
        let text = fs::read_to_string(&path).unwrap();
        // The file still reads in this build's own order, and each `f32` at
        // its shortest form.
        assert!(text.starts_with("schema_version = "));
        assert!(text.contains(&format!(
            "\nterminal_line_height = {}\n",
            AppSettings::default().terminal_line_height
        )));

        // A list entry's keys follow the entry, not its position.
        let first = desired.language_servers[0].id.clone();
        let mut removed = desired.clone();
        removed.language_servers.remove(1);
        save(&path, &removed).unwrap();
        removed.language_servers.remove(0);
        save(&path, &removed).unwrap();
        let file = saved(&path);
        assert!(
            file["language_servers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|server| server.get("future_option").is_none()
                    && server["id"].as_str() != Some(first.as_str()))
        );
        assert_eq!(file["future_preference"], true.into());
    }

    #[test]
    fn a_list_entrys_unknown_key_survives_an_earlier_entry_being_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            server_mut(document, 1).insert("future_option".into(), 3.into());
        });
        let mut settings = load(&path).unwrap();
        let kept = settings.language_servers[1].id.clone();
        settings.language_servers.remove(0);
        save(&path, &settings).unwrap();
        let file = saved(&path);
        assert_eq!(file["language_servers"][0]["id"], kept.as_str().into());
        assert_eq!(file["language_servers"][0]["future_option"], 3.into());
    }

    #[test]
    fn a_value_this_build_cannot_read_falls_back_and_is_kept_until_changed() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document.insert("terminal_theme".into(), "future_preset".into());
            document.insert("app_theme".into(), "future_scheme".into());
            let mut palette =
                toml::Table::try_from(crate::terminal::TerminalTheme::Dark.palette()).unwrap();
            palette.insert("selection".into(), "#445566".into());
            document.insert("custom_terminal_theme".into(), palette.into());
        });

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.terminal_theme, AppSettings::default().terminal_theme);
        assert_eq!(loaded.app_theme, AppSettings::default().app_theme);

        // Saved untouched, the newer build's values are still there.
        let desired = AppSettings {
            max_hot_cards: loaded.max_hot_cards + 1,
            ..loaded.clone()
        };
        save_merged(&path, &loaded, &desired).unwrap();
        let file = saved(&path);
        assert_eq!(file["terminal_theme"], "future_preset".into());
        assert_eq!(file["app_theme"], "future_scheme".into());
        assert_eq!(file["custom_terminal_theme"]["selection"], "#445566".into());
        assert_eq!(load(&path).unwrap(), desired);

        // A value chosen in this build replaces the one it could not read.
        let chosen = AppSettings {
            terminal_theme: crate::terminal::TerminalTheme::Nord,
            ..desired.clone()
        };
        save_merged(&path, &desired, &chosen).unwrap();
        let file = saved(&path);
        assert_eq!(file["terminal_theme"], "nord".into());
        assert_eq!(file["app_theme"], "future_scheme".into());
    }

    #[test]
    fn a_kept_shortcut_gives_up_a_chord_this_build_has_since_bound() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document["keyboard_shortcuts"]
                .as_table_mut()
                .unwrap()
                .insert("future_action".into(), vec!["Alt+M", "Alt+F12"].into());
        });
        let loaded = load(&path).unwrap();
        let mut desired = loaded.clone();
        desired.keyboard_shortcuts.new_terminal = vec!["alt+m".into()];
        save_merged(&path, &loaded, &desired).unwrap();
        assert_eq!(
            saved(&path)["keyboard_shortcuts"]["future_action"],
            vec!["Alt+F12"].into()
        );
    }

    #[test]
    fn a_newer_schema_is_never_saved_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let newer = AppSettings {
            schema_version: SETTINGS_SCHEMA_VERSION + 1,
            ..AppSettings::default()
        };
        let text = toml::to_string(&newer).unwrap();
        fs::write(&path, &text).unwrap();
        assert!(matches!(
            save(&path, &AppSettings::default()),
            Err(SettingsError::UnsupportedSchema(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    /// A newer build may drop a setting; this one uses its default and
    /// writes it only once it is changed here.
    #[test]
    fn a_setting_missing_from_the_file_takes_its_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document.remove("sidebar_width");
        });
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, AppSettings::default());
        // It stays out of the file until it is changed here, so a newer
        // build that dropped it never reads it back.
        let desired = AppSettings {
            max_hot_cards: loaded.max_hot_cards + 1,
            ..loaded.clone()
        };
        save_merged(&path, &loaded, &desired).unwrap();
        assert!(!saved(&path).contains_key("sidebar_width"));
        assert_eq!(load(&path).unwrap(), desired);
        let chosen = AppSettings {
            sidebar_width: desired.sidebar_width + 10.0,
            ..desired.clone()
        };
        save_merged(&path, &desired, &chosen).unwrap();
        assert!(saved(&path).contains_key("sidebar_width"));
        assert_eq!(load(&path).unwrap(), chosen);
    }

    #[test]
    fn shortcut_action_names_are_the_serialized_ones() {
        let table = toml::Table::try_from(KeyboardShortcuts::default()).unwrap();
        assert!(table.keys().eq(SHORTCUT_ACTIONS.iter()));
    }

    /// Each shortcut action this build cannot use falls back alone, however
    /// many fail the same way, and the file keeps what it held.
    #[test]
    fn every_unusable_shortcut_falls_back_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let mut chords = toml::Table::new();
        chords.insert("chords".into(), vec!["Alt+Y"].into());
        newer_build_document(&path, |document| {
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.insert("new_terminal".into(), vec!["Ctrl+F13"].into());
            shortcuts.insert("search_projects".into(), vec!["Alt+F13"].into());
            // A shape this build does not know costs only its own action.
            shortcuts.insert("clone_pane".into(), chords.clone().into());
            shortcuts.insert("cycle_pane_width".into(), vec!["Alt+X"].into());
        });
        let original = saved(&path)["keyboard_shortcuts"].clone();
        let loaded = load_detailed(&path).unwrap();
        let defaults = KeyboardShortcuts::default();
        let shortcuts = &loaded.settings.keyboard_shortcuts;
        assert_eq!(shortcuts.new_terminal, defaults.new_terminal);
        assert_eq!(shortcuts.search_projects, defaults.search_projects);
        assert_eq!(shortcuts.clone_pane, defaults.clone_pane);
        assert_eq!(shortcuts.cycle_pane_width, ["Alt+X"], "the rest is read");
        assert_eq!(loaded.set_aside().count(), 3);

        // Another shortcut changed here keeps them all.
        let mut desired = loaded.settings.clone();
        desired.keyboard_shortcuts.previous_workspace = vec!["Alt+Shift+P".into()];
        save_merged(&path, &loaded.settings, &desired).unwrap();
        let file = saved(&path);
        for action in ["new_terminal", "search_projects", "clone_pane"] {
            assert_eq!(
                file["keyboard_shortcuts"][action], original[action],
                "{action}"
            );
        }
        assert_eq!(load(&path).unwrap(), desired);
    }

    /// A fallback that takes a chord the file gave another action blames that
    /// action in this build, and the file keeps both as they were.
    #[test]
    fn a_fallback_conflict_costs_the_file_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.insert("new_terminal".into(), vec!["Ctrl+F13"].into());
            shortcuts.insert("search_projects".into(), vec!["Ctrl+Shift+T"].into());
        });
        let original = saved(&path)["keyboard_shortcuts"].clone();
        let loaded = load(&path).unwrap();
        let desired = AppSettings {
            max_hot_cards: loaded.max_hot_cards + 1,
            ..loaded.clone()
        };
        save_merged(&path, &loaded, &desired).unwrap();
        assert_eq!(saved(&path)["keyboard_shortcuts"], original);
        assert_eq!(load(&path).unwrap(), desired);
    }

    /// A newer build that dropped an action may bind its chord elsewhere. This
    /// build binds the action it still has, and the file keeps the newer
    /// build's binding and never gains the dropped action.
    #[test]
    fn a_dropped_actions_chord_stays_where_the_newer_build_put_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.remove("search_projects");
            shortcuts.insert("new_terminal".into(), vec!["Ctrl+Shift+F"].into());
        });
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.keyboard_shortcuts.search_projects, ["Ctrl+Shift+F"]);
        for _ in 0..2 {
            let current = load(&path).unwrap();
            let desired = AppSettings {
                max_hot_cards: current.max_hot_cards + 1,
                ..current.clone()
            };
            save_merged(&path, &current, &desired).unwrap();
            let file = saved(&path);
            assert_eq!(
                file["keyboard_shortcuts"]["new_terminal"],
                vec!["Ctrl+Shift+F"].into()
            );
            assert!(file["keyboard_shortcuts"].get("search_projects").is_none());
        }
    }

    /// An action the file lacks whose default chord the file gave another is
    /// left unbound, and stays so once that chord is freed: the migration's
    /// choice is written, not the default it withheld.
    #[test]
    fn a_missing_action_left_unbound_stays_unbound() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.remove("review_toggle_viewed");
            shortcuts.insert("new_terminal".into(), vec!["Alt+V"].into());
        });
        let loaded = load(&path).unwrap();
        assert!(loaded.keyboard_shortcuts.review_toggle_viewed.is_empty());
        let mut desired = loaded.clone();
        desired.keyboard_shortcuts.new_terminal = vec!["Alt+N".into()];
        save_merged(&path, &loaded, &desired).unwrap();
        assert_eq!(load(&path).unwrap(), desired);
    }

    /// A kept action that gives up its last chord is not kept, so the chords
    /// this build writes for it in turn cost the other kept actions; the file
    /// never binds a chord twice.
    #[test]
    fn an_unkept_action_never_leaves_a_chord_bound_twice() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.remove("search_projects");
            shortcuts.insert("new_terminal".into(), vec!["Ctrl+Shift+F"].into());
            shortcuts.insert("future_action".into(), vec!["Ctrl+Shift+T"].into());
        });
        let loaded = load(&path).unwrap();
        let mut desired = loaded.clone();
        desired.keyboard_shortcuts.search_projects = vec!["Ctrl+Shift+F".into(), "Alt+S".into()];
        save_merged(&path, &loaded, &desired).unwrap();
        let file = saved(&path);
        let bindings: Vec<ShortcutBinding> = file["keyboard_shortcuts"]
            .as_table()
            .unwrap()
            .values()
            .flat_map(|chords| chords.as_array().unwrap())
            .map(|chord| parse_shortcut(chord.as_str().unwrap()).unwrap())
            .collect();
        for (index, binding) in bindings.iter().enumerate() {
            assert!(!bindings[..index].contains(binding), "{file:#?}");
        }
        assert_eq!(load(&path).unwrap(), desired);
    }

    /// The channel shortcut's old name is read, and set aside, under the name
    /// this build writes.
    #[test]
    fn a_legacy_shortcut_name_is_set_aside_under_its_current_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.remove("new_channel");
            shortcuts.insert("new_thread".into(), vec!["Alt+F13"].into());
        });
        let loaded = load(&path).unwrap();
        save(&path, &loaded).unwrap();
        let file = saved(&path);
        assert_eq!(
            file["keyboard_shortcuts"]["new_channel"],
            vec!["Alt+F13"].into()
        );
        assert!(file["keyboard_shortcuts"].get("new_thread").is_none());
    }

    /// A value this build cannot read inside a list entry is dropped from the
    /// entry where it may be, and otherwise costs the whole list; either way
    /// the file keeps it while this build leaves the list as it read it.
    #[test]
    fn an_unreadable_value_in_a_list_entry_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let mut future = toml::Table::new();
        future.insert("future".into(), 1.into());
        newer_build_document(&path, |document| {
            server_mut(document, 0).insert("root_markers".into(), future.clone().into());
        });
        let loaded = load_detailed(&path).unwrap();
        assert!(loaded.settings.language_servers[0].root_markers.is_empty());
        let notes = loaded.set_aside().collect::<Vec<_>>();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("“language_servers[0].root_markers”"),
            "{notes:?}"
        );
        save(&path, &loaded.settings).unwrap();
        assert_eq!(
            saved(&path)["language_servers"][0]["root_markers"],
            future.clone().into()
        );

        // A required key cannot be dropped, so the list falls back whole.
        newer_build_document(&path, |document| {
            server_mut(document, 1).insert("command".into(), future.clone().into());
        });
        let original = saved(&path)["language_servers"].clone();
        let loaded = load_detailed(&path).unwrap();
        assert_eq!(loaded.settings, AppSettings::default());
        let notes = loaded.set_aside().collect::<Vec<_>>();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("“language_servers”"), "{notes:?}");
        assert!(
            !notes[0].contains("missing field"),
            "the reason is the value, not the fallback: {notes:?}"
        );
        let desired = AppSettings {
            max_hot_cards: loaded.settings.max_hot_cards + 1,
            ..loaded.settings.clone()
        };
        save_merged(&path, &loaded.settings, &desired).unwrap();
        assert_eq!(saved(&path)["language_servers"], original);
        assert_eq!(load(&path).unwrap(), desired);

        // A list that falls back for a fault of another entry reports that
        // fault, not what was set aside before it.
        newer_build_document(&path, |document| {
            server_mut(document, 0).insert("root_markers".into(), future.clone().into());
            server_mut(document, 1).remove("command");
        });
        let notes = load_detailed(&path)
            .unwrap()
            .set_aside()
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("missing field"), "{notes:?}");
    }

    /// Each `f32` is compared at the form it is written in, so a fallback for
    /// one is still kept.
    #[test]
    fn an_unreadable_float_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document.insert("terminal_line_height".into(), "tall".into());
        });
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.terminal_line_height,
            AppSettings::default().terminal_line_height
        );
        save(&path, &loaded).unwrap();
        assert_eq!(saved(&path)["terminal_line_height"], "tall".into());
    }

    /// A value that reads but that this build does not accept, such as a
    /// range a newer build widened or a key name it added, costs that one
    /// setting rather than the file, and says so.
    #[test]
    fn a_value_this_build_does_not_accept_falls_back_alone_and_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document.insert("ui_scale".into(), 2.5.into());
            document.insert("terminal_font_size".into(), 18.into());
            let shortcuts = document["keyboard_shortcuts"].as_table_mut().unwrap();
            shortcuts.insert("new_terminal".into(), vec!["Ctrl+FutureKey"].into());
            shortcuts.insert("search_projects".into(), vec!["Alt+S"].into());
        });
        let loaded = load_detailed(&path).unwrap();
        let settings = &loaded.settings;
        let defaults = AppSettings::default();
        assert_eq!(settings.ui_scale, defaults.ui_scale);
        assert_eq!(settings.terminal_font_size, 18.0, "the rest is read");
        assert_eq!(
            settings.keyboard_shortcuts.new_terminal,
            defaults.keyboard_shortcuts.new_terminal
        );
        assert_eq!(settings.keyboard_shortcuts.search_projects, ["Alt+S"]);
        let notes = loaded.set_aside().collect::<Vec<_>>();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes.iter().any(
                |note| note.contains("“ui_scale”") && note.contains("ui_scale must be between")
            )
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("“keyboard_shortcuts.new_terminal”"))
        );

        let desired = AppSettings {
            max_hot_cards: settings.max_hot_cards + 1,
            ..settings.clone()
        };
        save_merged(&path, settings, &desired).unwrap();
        let file = saved(&path);
        assert_eq!(file["ui_scale"], 2.5.into());
        assert_eq!(
            file["keyboard_shortcuts"]["new_terminal"],
            vec!["Ctrl+FutureKey"].into()
        );
        assert_eq!(load(&path).unwrap(), desired);

        // Chosen here, it is this build's value that is saved.
        let chosen = AppSettings {
            ui_scale: 1.25,
            ..desired.clone()
        };
        save_merged(&path, &desired, &chosen).unwrap();
        assert_eq!(saved(&path)["ui_scale"], 1.25.into());
        assert_eq!(load(&path).unwrap(), chosen);
    }

    /// A newer build's action that loses every chord to this build is left
    /// out rather than written unbound.
    #[test]
    fn a_kept_shortcut_left_with_no_chord_is_left_out() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        newer_build_document(&path, |document| {
            document["keyboard_shortcuts"]
                .as_table_mut()
                .unwrap()
                .insert("future_action".into(), vec!["Alt+M"].into());
        });
        let loaded = load(&path).unwrap();
        let mut desired = loaded.clone();
        desired.keyboard_shortcuts.new_terminal = vec!["alt+m".into()];
        save_merged(&path, &loaded, &desired).unwrap();
        assert!(
            saved(&path)["keyboard_shortcuts"]
                .get("future_action")
                .is_none()
        );
    }

    /// A key a newer build added to the custom palette survives both where
    /// the palette is chosen and where it is kept.
    #[test]
    fn a_newer_palette_color_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let mut palette =
            toml::Table::try_from(crate::terminal::TerminalTheme::Dark.palette()).unwrap();
        palette.insert("future_color".into(), "#112233".into());
        newer_build_document(&path, |document| {
            let mut custom = toml::Table::new();
            custom.insert("custom".into(), palette.clone().into());
            document.insert("terminal_theme".into(), custom.into());
            document.insert("custom_terminal_theme".into(), palette.clone().into());
        });
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.terminal_theme,
            crate::terminal::TerminalTheme::Custom(crate::terminal::TerminalTheme::Dark.palette())
        );
        save(&path, &loaded).unwrap();
        let file = saved(&path);
        assert_eq!(
            file["terminal_theme"]["custom"]["future_color"],
            "#112233".into()
        );
        assert_eq!(
            file["custom_terminal_theme"]["future_color"],
            "#112233".into()
        );
        assert_eq!(load(&path).unwrap(), loaded);
    }

    /// Saving over a file that cannot be read is how a user recovers it.
    #[test]
    fn a_file_that_cannot_be_read_is_saved_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        fs::write(&path, "schema_version = 3\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&path).is_ok() {
            // Running with the privilege to read it anyway.
            return;
        }
        save(&path, &AppSettings::default()).unwrap();
        assert_eq!(load(&path).unwrap(), AppSettings::default());
    }

    #[test]
    fn rejects_future_missing_and_unusable_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);

        let future = AppSettings {
            schema_version: SETTINGS_SCHEMA_VERSION + 1,
            ..AppSettings::default()
        };
        fs::write(&path, toml::to_string(&future).unwrap()).unwrap();
        assert!(matches!(
            load(&path),
            Err(SettingsError::UnsupportedSchema(_))
        ));

        fs::write(&path, "schema_version = 1\n").unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Toml(_))));

        // An out-of-range value costs only itself.
        let invalid = AppSettings {
            default_column_fraction: 0.1,
            ..AppSettings::default()
        };
        fs::write(&path, toml::to_string(&invalid).unwrap()).unwrap();
        assert_eq!(load(&path).unwrap(), AppSettings::default());

        let too_large = AppSettings {
            hibernation_delay_ms: MAX_TOML_INTEGER + 1,
            ..AppSettings::default()
        };
        assert!(matches!(
            too_large.validate(),
            Err(SettingsError::Invalid(_))
        ));
    }

    #[test]
    fn save_enforces_private_directory_and_file_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

        save(&path, &AppSettings::default()).unwrap();

        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            PRIVATE_DIRECTORY_MODE
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
    }

    #[test]
    fn save_rejects_symlink_destination_without_touching_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let target = directory.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, &path).unwrap();

        assert!(matches!(
            save(&path, &AppSettings::default()),
            Err(SettingsError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(fs::read(target).unwrap(), b"untouched");
    }

    #[test]
    fn save_rejects_symlinked_configuration_directory() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let linked = root.path().join("linked");
        fs::create_dir(&target).unwrap();
        symlink(&target, &linked).unwrap();

        assert!(matches!(
            save(linked.join("settings.toml"), &AppSettings::default()),
            Err(SettingsError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert!(!target.join("settings.toml").exists());
    }
}
