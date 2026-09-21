use std::{
    collections::{HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    design, instance,
    sidebar::{EXPANDED_WIDTH_MAX, EXPANDED_WIDTH_MIN},
};

pub const SETTINGS_SCHEMA_VERSION: u32 = 3;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MAX_TOML_INTEGER: u64 = i64::MAX as u64;

const MAX_TERMINAL_SCROLLBACK_LINES: usize = 1_000_000;
const MAX_TERMINAL_SCROLL_SIZE: usize = 10;
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
/// process inside a card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyboardShortcuts {
    pub new_terminal: Vec<String>,
    pub new_channel: Vec<String>,
    pub clone_pane: Vec<String>,
    pub close_focused_column: Vec<String>,
    pub search_projects: Vec<String>,
    pub previous_column: Vec<String>,
    pub next_column: Vec<String>,
    pub cycle_pane_width: Vec<String>,
}

impl Default for KeyboardShortcuts {
    fn default() -> Self {
        Self {
            new_terminal: vec!["Ctrl+T".to_owned()],
            new_channel: vec!["Alt+G".to_owned()],
            clone_pane: vec!["Alt+E".to_owned()],
            close_focused_column: vec!["Ctrl+W".to_owned()],
            search_projects: vec!["Ctrl+F".to_owned()],
            previous_column: vec!["Alt+Q".to_owned(), "Alt+Left".to_owned()],
            next_column: vec!["Alt+W".to_owned(), "Alt+Right".to_owned()],
            cycle_pane_width: vec!["Alt+R".to_owned()],
        }
    }
}

/// Separates chords in the one-line form the settings dialog edits.
pub const CHORD_SEPARATOR: &str = ", ";

impl KeyboardShortcuts {
    /// Every action's chord list, in the order the dialog presents them.
    fn actions(&self) -> [&Vec<String>; 8] {
        [
            &self.new_terminal,
            &self.new_channel,
            &self.clone_pane,
            &self.close_focused_column,
            &self.search_projects,
            &self.previous_column,
            &self.next_column,
            &self.cycle_pane_width,
        ]
    }

    fn validate(&self) -> Result<(), SettingsError> {
        let mut parsed = Vec::new();
        for (index, chords) in self.actions().into_iter().enumerate() {
            if chords.is_empty() && !matches!(index, 1 | 2 | 7) {
                // Newly introduced actions may remain unbound so migration
                // never steals a user's existing custom chord.
                return Err(SettingsError::InvalidShortcut(
                    "every keyboard shortcut needs at least one chord",
                ));
            }
            for chord in chords {
                let binding = parse_shortcut(chord).map_err(SettingsError::InvalidShortcut)?;
                if let Some(message) = withheld_from_the_card(binding) {
                    return Err(SettingsError::InvalidShortcut(message));
                }
                if parsed.contains(&binding) {
                    return Err(SettingsError::InvalidShortcut(
                        "keyboard shortcuts must not conflict",
                    ));
                }
                parsed.push(binding);
            }
        }
        Ok(())
    }
}

/// Why a chord must not be bound, or `None` when binding it is safe.
///
/// A bound chord is also a reserved one: the card widget captures it and never
/// forwards it to the process inside the card. That is the point for `Ctrl+W`,
/// but the control characters that carry a signal or end input are how a user
/// stops a runaway program, and a card that swallows them offers no clue that
/// it is doing so.
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

/// Renders a chord list as the single line the settings dialog edits.
pub fn format_chords(chords: &[String]) -> String {
    chords.join(CHORD_SEPARATOR)
}

/// Reads back the line `format_chords` writes.
///
/// Empty entries are dropped rather than kept as blanks, so a trailing
/// separator left mid-edit does not read as an unparsable chord; a line with no
/// chord at all is rejected by [`KeyboardShortcuts::validate`].
pub fn parse_chords(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|chord| !chord.is_empty())
        .map(str::to_owned)
        .collect()
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
            } else {
                let mut characters = token.chars();
                match (characters.next(), characters.next()) {
                    (Some(character), None) if character.is_ascii_alphanumeric() => {
                        Some(ShortcutKey::Character(character.to_ascii_lowercase()))
                    }
                    _ => return Err("keyboard shortcut key must be A-Z, 0-9, Left, or Right"),
                }
            };
        }
    }
    if !(control || alt || logo) {
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

const fn default_terminal_scrollback_lines() -> usize {
    10_000
}

const fn default_terminal_scroll_size() -> usize {
    2
}

const fn default_terminal_scroll_speed() -> f32 {
    1.0
}

fn default_managed_worktree_directory() -> String {
    "worktrees/".to_owned()
}

const fn default_focus_follows_mouse() -> bool {
    true
}

const fn default_editor_auto_save() -> bool {
    true
}

const fn default_confirm_terminal_close() -> bool {
    true
}

const fn default_chat_enter_sends() -> bool {
    true
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    pub schema_version: u32,
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
    #[serde(default = "default_terminal_scrollback_lines")]
    pub terminal_scrollback_lines: usize,
    #[serde(default)]
    pub link_open_destination: LinkOpenDestination,
    #[serde(default)]
    pub link_open_gesture: LinkOpenGesture,
    /// Absolute, or resolved relative to the repository root.
    #[serde(default = "default_managed_worktree_directory")]
    pub managed_worktree_directory: String,
    #[serde(default)]
    pub worktree_sort: WorktreeSort,
    #[serde(default = "default_editor_auto_save")]
    pub editor_auto_save: bool,
    #[serde(default)]
    pub editor_wrap_lines: bool,
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
    #[serde(default, deserialize_with = "deserialize_keyboard_shortcuts")]
    pub keyboard_shortcuts: KeyboardShortcuts,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
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
            sidebar_width: design::NAVIGATION_DEFAULT_WIDTH,
            inspector_width: design::INSPECTOR_DEFAULT_WIDTH,
            default_column_fraction: 0.5,
            terminal_scrollback_lines: 10_000,
            link_open_destination: LinkOpenDestination::default(),
            link_open_gesture: LinkOpenGesture::default(),
            managed_worktree_directory: default_managed_worktree_directory(),
            worktree_sort: WorktreeSort::default(),
            editor_auto_save: default_editor_auto_save(),
            editor_wrap_lines: false,
            editor_syntax_theme: EditorSyntaxTheme::default(),
            confirm_terminal_close: default_confirm_terminal_close(),
            chat_enter_sends: default_chat_enter_sends(),
            language_servers: default_language_servers(),
            keyboard_shortcuts: KeyboardShortcuts::default(),
        }
    }
}

/// One language a server handles, and the file extensions that select it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageServerLanguage {
    /// LSP `languageId` sent in `textDocument/didOpen`.
    pub language_id: String,
    /// Lowercase file extensions without a leading dot.
    pub extensions: Vec<String>,
}

/// One external language server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageServerSettings {
    /// Stable unique identifier, also used as the session key prefix.
    pub id: String,
    pub languages: Vec<LanguageServerLanguage>,
    /// argv; never a shell command line. `command[0]` is the executable.
    pub command: Vec<String>,
    /// Relative marker names that identify a project root.
    #[serde(default)]
    pub root_markers: Vec<String>,
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
    #[serde(default = "default_terminal_scrollback_lines")]
    terminal_scrollback_lines: usize,
    #[serde(default)]
    link_open_destination: LinkOpenDestination,
    #[serde(default)]
    link_open_gesture: LinkOpenGesture,
    #[serde(default, deserialize_with = "deserialize_v2_keyboard_shortcuts")]
    keyboard_shortcuts: KeyboardShortcuts,
}

fn preserve_existing_shortcut(shortcuts: &mut toml::Table, action: &str, default: &str) {
    if shortcuts.contains_key(action) {
        return;
    }
    let reserved = parse_shortcut(default).expect("built-in shortcut");
    let occupied = shortcuts
        .values()
        .filter_map(toml::Value::as_array)
        .flatten()
        .filter_map(toml::Value::as_str)
        .any(|chord| parse_shortcut(chord).is_ok_and(|binding| binding == reserved));
    if occupied {
        // A new default must not invalidate or steal an existing custom chord.
        // Leave only this newly introduced action unbound until configured.
        shortcuts.insert(action.into(), toml::Value::Array(Vec::new()));
    }
}

/// Accepts the pre-rename spelling of the channel-creation shortcut.
///
/// The action was stored as `new_thread` before the project-level container
/// became a channel, and [`KeyboardShortcuts`] denies unknown fields, so an
/// existing table would otherwise fail to load rather than migrate. The legacy
/// key is moved rather than copied because leaving it behind would both be
/// rejected as unknown and let two spellings of one action disagree; a table
/// already carrying the new spelling keeps it and discards the stale one.
fn adopt_legacy_channel_shortcut(shortcuts: &mut toml::Table) {
    if let Some(legacy) = shortcuts.remove("new_thread")
        && !shortcuts.contains_key("new_channel")
    {
        shortcuts.insert("new_channel".into(), legacy);
    }
}

fn deserialize_keyboard_shortcuts<'de, D>(deserializer: D) -> Result<KeyboardShortcuts, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut shortcuts = toml::Table::deserialize(deserializer)?;
    adopt_legacy_channel_shortcut(&mut shortcuts);
    preserve_existing_shortcut(&mut shortcuts, "cycle_pane_width", "Alt+R");
    preserve_existing_shortcut(&mut shortcuts, "new_channel", "Alt+G");
    preserve_existing_shortcut(&mut shortcuts, "clone_pane", "Alt+E");
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
    for (_, binding) in &mut shortcuts {
        if let toml::Value::String(chord) = binding {
            *binding = toml::Value::Array(vec![toml::Value::String(std::mem::take(chord))]);
        }
    }
    preserve_existing_shortcut(&mut shortcuts, "cycle_pane_width", "Alt+R");
    preserve_existing_shortcut(&mut shortcuts, "new_channel", "Alt+G");
    preserve_existing_shortcut(&mut shortcuts, "clone_pane", "Alt+E");
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
            inspector_width: design::INSPECTOR_DEFAULT_WIDTH,
            default_column_fraction: previous.default_column_fraction,
            terminal_scrollback_lines: previous.terminal_scrollback_lines,
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
        if self.terminal_font_family.len() > 128 {
            return Err(SettingsError::Invalid(
                "terminal_font_family must not exceed 128 bytes",
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
        if !(design::INSPECTOR_MIN_WIDTH..=design::INSPECTOR_MAX_WIDTH)
            .contains(&self.inspector_width)
        {
            return Err(SettingsError::Invalid(
                "inspector_width must be between 320 and 480",
            ));
        }
        if !(0.25..=1.0).contains(&self.default_column_fraction) {
            return Err(SettingsError::Invalid(
                "default_column_fraction must be between 0.25 and 1.0",
            ));
        }
        if self.terminal_scrollback_lines > MAX_TERMINAL_SCROLLBACK_LINES {
            return Err(SettingsError::Invalid(
                "terminal_scrollback_lines must be between 0 and 1000000",
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
    instance::project_dirs()
        .map(|dirs| dirs.config_dir().join("settings.toml"))
        .ok_or(SettingsError::DirectoryUnavailable)
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
    let current = load_unlocked(path)?;
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
    let path = path.as_ref();
    let _lock = settings_lock(path)?;
    load_unlocked(path)
}

fn load_unlocked(path: &Path) -> Result<AppSettings, SettingsError> {
    reject_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => load_toml(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => migrate_legacy(path),
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

    let mut document = toml::to_string_pretty(&settings)?;
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

fn load_toml(path: &Path) -> Result<AppSettings, SettingsError> {
    let bytes = read_private_file(path)?;
    let document = std::str::from_utf8(&bytes)?;
    match toml::from_str::<SettingsHeader>(document)?.schema_version {
        LEGACY_SETTINGS_SCHEMA_VERSION => {
            AppSettings::from(toml::from_str::<V1TomlSettings>(document)?).validate()
        }
        PREVIOUS_SETTINGS_SCHEMA_VERSION => {
            AppSettings::from(toml::from_str::<V2TomlSettings>(document)?).validate()
        }
        SETTINGS_SCHEMA_VERSION => load_current_toml(document),
        version => Err(SettingsError::UnsupportedSchema(version)),
    }
}

/// Renames the short-lived scroll-speed key from the first implementation of
/// the preference. Its value represented lines per wheel notch, so it is the
/// same setting; the old default of one is promoted to the new default of two.
fn load_current_toml(document: &str) -> Result<AppSettings, SettingsError> {
    let mut table = toml::from_str::<toml::Table>(document)?;
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
    toml::Value::Table(table)
        .try_into::<AppSettings>()
        .map_err(SettingsError::Toml)?
        .validate()
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
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn path_in(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().join("settings.toml")
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
            focus_follows_mouse: false,
            confirm_terminal_close: false,
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
        let decoded: AppSettings = old.try_into().unwrap();
        assert!(decoded.confirm_terminal_close);
        assert!(decoded.focus_follows_mouse);
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
        assert_eq!(decoded.managed_worktree_directory, "worktrees/");
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
        fs::write(&path, toml::to_string(&old).unwrap()).unwrap();
        let mut settings = load(&path).unwrap();
        assert!(!settings.editor_wrap_lines);
        settings.editor_wrap_lines = true;
        save(&path, &settings).unwrap();
        assert!(load(&path).unwrap().editor_wrap_lines);
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
            assert!(loaded.focus_follows_mouse);
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
        assert!(migrated.focus_follows_mouse);
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
        assert_eq!(migrated.keyboard_shortcuts.new_channel, ["Alt+G"]);
        assert_eq!(migrated.keyboard_shortcuts.clone_pane, ["Alt+E"]);
        assert_eq!(migrated.keyboard_shortcuts.previous_column, ["Alt+Q"]);
        assert_eq!(
            migrated.keyboard_shortcuts.next_column,
            ["Alt+W", "Alt+Right"]
        );
        assert!(migrated.editor_auto_save);
        assert_eq!(migrated.keyboard_shortcuts.cycle_pane_width, ["Alt+R"]);
        save(&path, &migrated).unwrap();
        assert_eq!(load(&path).unwrap(), migrated);
    }

    #[test]
    fn introducing_pane_cycling_preserves_an_existing_alt_r_binding() {
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

    #[test]
    fn new_channel_migration_preserves_a_custom_chord_but_rejects_explicit_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut table = toml::Value::try_from(AppSettings::default()).unwrap();
        let shortcuts = table["keyboard_shortcuts"].as_table_mut().unwrap();
        shortcuts.remove("new_channel");
        shortcuts.insert(
            "new_terminal".into(),
            toml::Value::Array(vec![toml::Value::String("Alt+g".into())]),
        );
        fs::write(&path, toml::to_string(&table).unwrap()).unwrap();
        let migrated = load(&path).unwrap();
        assert_eq!(migrated.keyboard_shortcuts.new_terminal, ["Alt+g"]);
        assert!(migrated.keyboard_shortcuts.new_channel.is_empty());
        save(&path, &migrated).unwrap();
        assert_eq!(load(&path).unwrap(), migrated);

        let mut conflicting = migrated;
        conflicting.keyboard_shortcuts.new_channel = vec!["Alt+G".into()];
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
            (2, "\"Alt+e\"", "\"Ctrl+Shift+T\""),
            (3, "[\"Alt+e\"]", "[\"Ctrl+Shift+T\"]"),
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
            assert_eq!(migrated.keyboard_shortcuts.new_terminal, ["Alt+e"]);
            assert_eq!(migrated.keyboard_shortcuts.new_channel, ["Ctrl+Shift+T"]);
            assert!(migrated.keyboard_shortcuts.clone_pane.is_empty());
            save(&path, &migrated).unwrap();
            assert_eq!(load(&path).unwrap(), migrated);

            let mut conflicting = migrated;
            conflicting.keyboard_shortcuts.clone_pane = vec!["Alt+E".into()];
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
            (vec!["Alt+Shift+E".into()], vec!["Ctrl+Shift+T".into()]),
            (Vec::new(), Vec::new()),
        ] {
            let mut settings = AppSettings::default();
            settings.keyboard_shortcuts.clone_pane = clone_pane;
            settings.keyboard_shortcuts.new_channel = new_channel;
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
        assert!(parse_shortcut("Ctrl+Alt+Left").is_ok());
        assert!(parse_shortcut("Ctrl+NotAKey").is_err());

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
            .push("Alt+Left".to_owned());
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

        // Column navigation answers to a letter chord and an arrow, so binding
        // one never costs the other.
        let defaults = KeyboardShortcuts::default();
        assert!(defaults.previous_column.iter().any(|c| c == "Alt+Left"));
        assert!(defaults.next_column.iter().any(|c| c == "Alt+Right"));
        assert!(AppSettings::default().validate().is_ok());
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
    fn terminal_scrollback_is_bounded() {
        let at_limit = AppSettings {
            terminal_scrollback_lines: MAX_TERMINAL_SCROLLBACK_LINES,
            ..AppSettings::default()
        };
        assert!(at_limit.validate().is_ok());
        let above_limit = AppSettings {
            terminal_scrollback_lines: MAX_TERMINAL_SCROLLBACK_LINES + 1,
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
            design::INSPECTOR_MIN_WIDTH,
            design::INSPECTOR_DEFAULT_WIDTH,
            design::INSPECTOR_MAX_WIDTH,
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
            design::INSPECTOR_MIN_WIDTH - 1.0,
            design::INSPECTOR_MAX_WIDTH + 1.0,
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

    #[test]
    fn rejects_unknown_future_missing_and_out_of_range_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let valid = toml::to_string(&AppSettings::default()).unwrap();

        fs::write(&path, format!("{valid}unknown = true\n")).unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Toml(_))));

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

        let invalid = AppSettings {
            default_column_fraction: 0.1,
            ..AppSettings::default()
        };
        fs::write(&path, toml::to_string(&invalid).unwrap()).unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Invalid(_))));

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
