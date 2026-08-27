use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sidebar::{EXPANDED_WIDTH_MAX, EXPANDED_WIDTH_MIN};

pub const SETTINGS_SCHEMA_VERSION: u32 = 2;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MAX_TOML_INTEGER: u64 = i64::MAX as u64;

const MAX_TERMINAL_SCROLLBACK_LINES: usize = 1_000_000;
const LEGACY_SETTINGS_SCHEMA_VERSION: u32 = 1;
const LEGACY_SIDEBAR_WIDTH_MIN: f32 = 180.0;
const LEGACY_SIDEBAR_WIDTH_MAX: f32 = 480.0;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyboardShortcuts {
    pub new_terminal: String,
    pub close_focused_column: String,
    pub search_projects: String,
    pub previous_column: String,
    pub next_column: String,
}

impl Default for KeyboardShortcuts {
    fn default() -> Self {
        Self {
            new_terminal: "Ctrl+T".to_owned(),
            close_focused_column: "Ctrl+W".to_owned(),
            search_projects: "Ctrl+F".to_owned(),
            previous_column: "Alt+Q".to_owned(),
            next_column: "Alt+W".to_owned(),
        }
    }
}

impl KeyboardShortcuts {
    fn validate(&self) -> Result<(), SettingsError> {
        let bindings = [
            parse_shortcut(&self.new_terminal),
            parse_shortcut(&self.close_focused_column),
            parse_shortcut(&self.search_projects),
            parse_shortcut(&self.previous_column),
            parse_shortcut(&self.next_column),
        ];
        let mut parsed = Vec::with_capacity(bindings.len());
        for binding in bindings {
            parsed.push(binding.map_err(SettingsError::InvalidShortcut)?);
        }
        for (index, binding) in parsed.iter().enumerate() {
            if parsed[..index].contains(binding) {
                return Err(SettingsError::InvalidShortcut(
                    "keyboard shortcuts must not conflict",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShortcutBinding {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub key: ShortcutKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutKey {
    Character(char),
    ArrowLeft,
    ArrowRight,
}

pub(crate) fn parse_shortcut(raw: &str) -> Result<ShortcutBinding, &'static str> {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    pub schema_version: u32,
    pub max_hot_cards: usize,
    pub hibernation_delay_ms: u64,
    pub terminal_font_size: f32,
    /// The terminal font family, or empty for the system monospace default.
    #[serde(default)]
    pub terminal_font_family: String,
    pub sidebar_width: f32,
    pub default_column_fraction: f32,
    #[serde(default = "default_terminal_scrollback_lines")]
    pub terminal_scrollback_lines: usize,
    #[serde(default)]
    pub link_open_destination: LinkOpenDestination,
    #[serde(default)]
    pub link_open_gesture: LinkOpenGesture,
    #[serde(default)]
    pub keyboard_shortcuts: KeyboardShortcuts,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            max_hot_cards: 6,
            hibernation_delay_ms: 30_000,
            terminal_font_size: 14.0,
            terminal_font_family: String::new(),
            sidebar_width: 280.0,
            default_column_fraction: 0.72,
            terminal_scrollback_lines: 10_000,
            link_open_destination: LinkOpenDestination::default(),
            link_open_gesture: LinkOpenGesture::default(),
            keyboard_shortcuts: KeyboardShortcuts::default(),
        }
    }
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

fn normalize_legacy_sidebar_width(width: f32) -> f32 {
    if (LEGACY_SIDEBAR_WIDTH_MIN..=LEGACY_SIDEBAR_WIDTH_MAX).contains(&width) {
        width.clamp(EXPANDED_WIDTH_MIN, EXPANDED_WIDTH_MAX)
    } else {
        width
    }
}

impl AppSettings {
    pub fn validate(self) -> Result<Self, SettingsError> {
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
        self.keyboard_shortcuts.validate()?;
        Ok(self)
    }
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
    #[error("settings use unsupported schema version {0}; expected 2")]
    UnsupportedSchema(u32),
    #[error("invalid settings: {0}")]
    Invalid(&'static str),
    #[error("invalid settings: {0}")]
    InvalidShortcut(&'static str),
    /// Quotes the sidebar's own bounds rather than repeating them, because a
    /// width this accepted used to render clamped to a narrower range.
    #[error("invalid settings: sidebar_width must be between {min} and {max}")]
    SidebarWidth { min: f32, max: f32 },
}

pub fn default_path() -> Result<PathBuf, SettingsError> {
    ProjectDirs::from("dev", "cagents", "cagents")
        .map(|dirs| dirs.config_dir().join("settings.toml"))
        .ok_or(SettingsError::DirectoryUnavailable)
}

pub fn load(path: impl AsRef<Path>) -> Result<AppSettings, SettingsError> {
    let path = path.as_ref();
    reject_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => load_toml(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => migrate_legacy(path),
        Err(error) => Err(error.into()),
    }
}

pub fn save(path: impl AsRef<Path>, settings: &AppSettings) -> Result<(), SettingsError> {
    let path = path.as_ref();
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
        SETTINGS_SCHEMA_VERSION => toml::from_str::<AppSettings>(document)?.validate(),
        version => Err(SettingsError::UnsupportedSchema(version)),
    }
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
            save(path, &settings)?;
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
    fn absent_settings_use_defaults_without_creating_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);

        assert_eq!(load(&path).unwrap(), AppSettings::default());
        assert!(!path.exists());
    }

    #[test]
    fn toml_round_trip_preserves_every_value() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let settings = AppSettings {
            max_hot_cards: 9,
            hibernation_delay_ms: 1_234,
            terminal_font_size: 16.5,
            terminal_font_family: "Fira Code".to_owned(),
            sidebar_width: 360.0,
            default_column_fraction: 0.55,
            ..AppSettings::default()
        };

        save(&path, &settings).unwrap();

        assert_eq!(load(&path).unwrap(), settings);
        let decoded: AppSettings = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(decoded, settings);
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
    fn version_one_toml_is_upgraded_and_legacy_sidebar_widths_are_normalized() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        for (legacy_width, expected_width) in [(180.0, 240.0), (400.0, 360.0)] {
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
            assert_eq!(loaded.keyboard_shortcuts, KeyboardShortcuts::default());
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

        let mut conflicting = AppSettings::default();
        conflicting.keyboard_shortcuts.next_column =
            conflicting.keyboard_shortcuts.previous_column.clone();
        assert!(matches!(
            conflicting.validate(),
            Err(SettingsError::InvalidShortcut(message)) if message.contains("conflict")
        ));
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
    fn legacy_json_is_migrated_only_when_toml_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let path = path_in(&directory);
        let legacy_path = directory.path().join("settings.json");
        let settings = AppSettings {
            terminal_font_size: 18.0,
            ..AppSettings::default()
        };
        fs::write(&legacy_path, br#"{"terminal_font_size":18.0}"#).unwrap();

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
