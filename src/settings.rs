use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MAX_TOML_INTEGER: u64 = i64::MAX as u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    pub schema_version: u32,
    pub max_hot_cards: usize,
    pub hibernation_delay_ms: u64,
    pub terminal_font_size: f32,
    pub sidebar_width: f32,
    pub default_column_fraction: f32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            max_hot_cards: 6,
            hibernation_delay_ms: 30_000,
            terminal_font_size: 14.0,
            sidebar_width: 280.0,
            default_column_fraction: 0.72,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacySettings {
    schema_version: u32,
    max_hot_cards: usize,
    hibernation_delay_ms: u64,
    terminal_font_size: f32,
    sidebar_width: f32,
    default_column_fraction: f32,
}

impl Default for LegacySettings {
    fn default() -> Self {
        let defaults = AppSettings::default();
        Self {
            schema_version: defaults.schema_version,
            max_hot_cards: defaults.max_hot_cards,
            hibernation_delay_ms: defaults.hibernation_delay_ms,
            terminal_font_size: defaults.terminal_font_size,
            sidebar_width: defaults.sidebar_width,
            default_column_fraction: defaults.default_column_fraction,
        }
    }
}

impl From<LegacySettings> for AppSettings {
    fn from(legacy: LegacySettings) -> Self {
        Self {
            schema_version: legacy.schema_version,
            max_hot_cards: legacy.max_hot_cards,
            hibernation_delay_ms: legacy.hibernation_delay_ms,
            terminal_font_size: legacy.terminal_font_size,
            sidebar_width: legacy.sidebar_width,
            default_column_fraction: legacy.default_column_fraction,
        }
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
        if !(180.0..=480.0).contains(&self.sidebar_width) {
            return Err(SettingsError::Invalid(
                "sidebar_width must be between 180 and 480",
            ));
        }
        if !(0.25..=1.0).contains(&self.default_column_fraction) {
            return Err(SettingsError::Invalid(
                "default_column_fraction must be between 0.25 and 1.0",
            ));
        }
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
    #[error("settings use unsupported schema version {0}; expected 1")]
    UnsupportedSchema(u32),
    #[error("invalid settings: {0}")]
    Invalid(&'static str),
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
    toml::from_str::<AppSettings>(std::str::from_utf8(&bytes)?)?.validate()
}

fn migrate_legacy(path: &Path) -> Result<AppSettings, SettingsError> {
    let legacy_path = path.with_file_name("settings.json");
    reject_symlink_components(&legacy_path)?;
    match fs::symlink_metadata(&legacy_path) {
        Ok(_) => {
            let bytes = read_private_file(&legacy_path)?;
            let settings = AppSettings::from(serde_json::from_slice::<LegacySettings>(&bytes)?)
                .validate()?;
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
                return Err(unsafe_path_error(ancestor, "symbolic links are not allowed"));
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

        let mut future = AppSettings::default();
        future.schema_version = SETTINGS_SCHEMA_VERSION + 1;
        fs::write(&path, toml::to_string(&future).unwrap()).unwrap();
        assert!(matches!(
            load(&path),
            Err(SettingsError::UnsupportedSchema(_))
        ));

        fs::write(&path, "schema_version = 1\n").unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Toml(_))));

        let mut invalid = AppSettings::default();
        invalid.default_column_fraction = 0.1;
        fs::write(&path, toml::to_string(&invalid).unwrap()).unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Invalid(_))));

        let mut too_large = AppSettings::default();
        too_large.hibernation_delay_ms = MAX_TOML_INTEGER + 1;
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
