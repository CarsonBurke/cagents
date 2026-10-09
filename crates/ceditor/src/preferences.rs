use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const fn default_sidebar_open() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    #[serde(with = "session_paths")]
    pub paths: Vec<PathBuf>,
    pub active: Option<usize>,
    #[serde(default = "default_sidebar_open")]
    pub sidebar_open: bool,
    /// File-tree folders the user opened, keyed by worktree path. Everything
    /// else renders collapsed, so whole project trees stay cheap to load.
    #[serde(default)]
    pub expanded_directories: BTreeMap<String, Vec<String>>,
}

// UTF-8 paths retain the legacy JSON string representation. Unix filenames
// that cannot be represented as text carry their original bytes explicitly.
mod session_paths {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::PathBuf,
    };

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct UnixPath<T> {
        unix_bytes: T,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SavedPath {
        Text(String),
        Unix(UnixPath<Vec<u8>>),
    }

    pub fn serialize<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(paths.len()))?;
        for path in paths {
            let bytes = path.as_os_str().as_bytes();
            if bytes.contains(&0) {
                return Err(serde::ser::Error::custom(
                    "session path contains a NUL byte",
                ));
            }
            if let Some(text) = path.to_str() {
                sequence.serialize_element(text)?;
            } else {
                sequence.serialize_element(&UnixPath { unix_bytes: bytes })?;
            }
        }
        sequence.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<SavedPath>::deserialize(deserializer)?
            .into_iter()
            .map(|path| {
                let bytes = match path {
                    SavedPath::Text(text) => text.into_bytes(),
                    SavedPath::Unix(path) => path.unix_bytes,
                };
                if bytes.contains(&0) {
                    return Err(serde::de::Error::custom("session path contains a NUL byte"));
                }
                Ok(PathBuf::from(OsString::from_vec(bytes)))
            })
            .collect()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            active: None,
            sidebar_open: default_sidebar_open(),
            expanded_directories: BTreeMap::new(),
        }
    }
}

impl Session {
    pub fn startup_paths(&self, explicit: Vec<PathBuf>) -> Vec<(PathBuf, bool)> {
        let restore_focus = explicit.is_empty();
        self.paths
            .iter()
            .enumerate()
            .map(|(index, path)| (path.clone(), restore_focus && self.active == Some(index)))
            .chain(explicit.into_iter().map(|path| (path, true)))
            .collect()
    }
}

pub fn load_session(settings: &Path) -> Result<Session, String> {
    let path = settings.with_file_name("session.json");
    match std::fs::File::open(&path) {
        Ok(file) => serde_json::from_reader(file.take(4 * 1024 * 1024))
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Session::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

pub fn save_session(settings: &Path, session: &Session) -> Result<(), String> {
    let document = serde_json::to_vec(session).map_err(|error| error.to_string())?;
    save_atomic(&settings.with_file_name("session.json"), &document)
}

pub fn load_language_server_assistance(settings: &Path) -> Result<bool, String> {
    let path = settings.with_file_name("language-server-assistance");
    let mut value = String::new();
    match std::fs::File::open(&path) {
        Ok(file) => {
            file.take(16)
                .read_to_string(&mut value)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    match value.trim() {
        "enabled" => Ok(true),
        "disabled" => Ok(false),
        _ => Err(format!(
            "Invalid language-server assistance preference in {}",
            path.display()
        )),
    }
}

pub fn save_language_server_assistance(settings: &Path, enabled: bool) -> Result<(), String> {
    save_atomic(
        &settings.with_file_name("language-server-assistance"),
        if enabled { b"enabled\n" } else { b"disabled\n" },
    )
}

fn save_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let directory = path.parent().ok_or("Settings directory is unavailable")?;
    // The shared settings writer creates and validates this directory first.
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    temporary
        .persist(&path)
        .map_err(|error| error.to_string())?;
    std::fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorShortcuts {
    pub new_tab: String,
    pub open_file: String,
    pub open_folder: String,
    pub save_file: String,
    pub close_tab: String,
    #[serde(alias = "search_files")]
    pub find_in_file: String,
    pub filter_files: String,
    pub quick_open: String,
    pub toggle_sidebar: String,
    pub review_toggle_viewed: String,
    pub review_next_file: String,
    pub review_previous_file: String,
    pub review_next_unviewed: String,
    pub review_next_change: String,
    pub review_previous_change: String,
}

impl Default for EditorShortcuts {
    fn default() -> Self {
        Self {
            new_tab: "Ctrl+T".into(),
            open_file: "Ctrl+O".into(),
            open_folder: "Ctrl+Shift+O".into(),
            save_file: "Ctrl+S".into(),
            close_tab: "Ctrl+W".into(),
            find_in_file: "Ctrl+F".into(),
            filter_files: "Ctrl+Shift+F".into(),
            quick_open: "Ctrl+P".into(),
            toggle_sidebar: "Ctrl+B".into(),
            review_toggle_viewed: "Alt+V".into(),
            review_next_file: "Alt+Down".into(),
            review_previous_file: "Alt+Up".into(),
            review_next_unviewed: "Alt+Shift+Down".into(),
            review_next_change: "Alt+J".into(),
            review_previous_change: "Alt+K".into(),
        }
    }
}

pub fn load_shortcuts(settings: &Path) -> Result<EditorShortcuts, String> {
    let path = settings.with_file_name("shortcuts.toml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            parse_shortcuts(&contents).map_err(|error| format!("{}: {error}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(EditorShortcuts::default())
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Reads a shortcut table, giving actions the table predates their defaults
/// unless an action the table does bind already owns that chord; then the
/// new action stays unbound rather than making the whole table conflict,
/// which would wedge the shortcut settings until the pair was found.
fn parse_shortcuts(contents: &str) -> Result<EditorShortcuts, toml::de::Error> {
    let table: toml::Table = toml::from_str(contents)?;
    let mut shortcuts: EditorShortcuts = toml::from_str(contents)?;
    let mut taken: Vec<String> = [
        &shortcuts.new_tab,
        &shortcuts.open_file,
        &shortcuts.open_folder,
        &shortcuts.save_file,
        &shortcuts.close_tab,
        &shortcuts.find_in_file,
        &shortcuts.filter_files,
        &shortcuts.quick_open,
        &shortcuts.toggle_sidebar,
    ]
    .into_iter()
    .map(|value| canonical_chord(value))
    .collect();
    // Explicit review keys are taken as written and claim their chord
    // before the defaulted ones are resolved.
    let review = [
        ("review_toggle_viewed", &mut shortcuts.review_toggle_viewed),
        ("review_next_file", &mut shortcuts.review_next_file),
        ("review_previous_file", &mut shortcuts.review_previous_file),
        ("review_next_unviewed", &mut shortcuts.review_next_unviewed),
        ("review_next_change", &mut shortcuts.review_next_change),
        (
            "review_previous_change",
            &mut shortcuts.review_previous_change,
        ),
    ];
    let (explicit, defaulted): (Vec<_>, Vec<_>) = review
        .into_iter()
        .partition(|(key, _)| table.contains_key(*key));
    taken.extend(
        explicit
            .iter()
            .map(|(_, value)| canonical_chord(value))
            .filter(|chord| !chord.is_empty()),
    );
    for (_, value) in defaulted {
        let chord = canonical_chord(value);
        if taken.contains(&chord) {
            value.clear();
        } else {
            taken.push(chord);
        }
    }
    Ok(shortcuts)
}

/// A chord in the form two spellings of the same binding share.
fn canonical_chord(value: &str) -> String {
    cagents::settings::parse_shortcut(value).map_or_else(
        |_| value.trim().to_ascii_lowercase(),
        |binding| format!("{binding:?}"),
    )
}

pub fn save_shortcuts(settings: &Path, shortcuts: &EditorShortcuts) -> Result<(), String> {
    let contents = toml::to_string_pretty(shortcuts).map_err(|error| error.to_string())?;
    save_atomic(
        &settings.with_file_name("shortcuts.toml"),
        contents.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn new_review_navigation_defaults_do_not_steal_existing_chords() {
        let shortcuts =
            super::parse_shortcuts("quick_open = \"Alt+J\"\nfilter_files = \"Alt+Shift+Down\"\n")
                .unwrap();
        assert!(shortcuts.review_next_change.is_empty());
        assert!(shortcuts.review_next_unviewed.is_empty());
        assert_eq!(shortcuts.review_previous_change, "Alt+K");
    }
    use super::*;

    /// A table written before the review actions existed may already give
    /// Alt+V (or an arrow chord) to something else; the review action then
    /// stays unbound instead of the whole table failing to validate.
    #[test]
    fn loading_an_older_table_unbinds_a_review_default_another_action_owns() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        std::fs::write(
            settings.with_file_name("shortcuts.toml"),
            "quick_open = \"alt+v\"\nfilter_files = \"Alt+Down\"\n",
        )
        .unwrap();

        let shortcuts = load_shortcuts(&settings).unwrap();

        assert_eq!(shortcuts.quick_open, "alt+v");
        assert_eq!(shortcuts.review_toggle_viewed, "", "Alt+V is quick open's");
        assert_eq!(shortcuts.review_next_file, "", "Alt+Down filters files");
        assert_eq!(
            shortcuts.review_previous_file,
            EditorShortcuts::default().review_previous_file,
            "an unclaimed default is kept"
        );

        // Explicit review keys are taken as written, conflicts and all, and
        // they claim their chord from the defaulted review keys.
        std::fs::write(
            settings.with_file_name("shortcuts.toml"),
            "quick_open = \"Alt+V\"\nreview_toggle_viewed = \"Alt+V\"\nreview_previous_file = \"Alt+Down\"\n",
        )
        .unwrap();
        let shortcuts = load_shortcuts(&settings).unwrap();
        assert_eq!(shortcuts.review_toggle_viewed, "Alt+V");
        assert_eq!(shortcuts.review_previous_file, "Alt+Down");
        assert_eq!(shortcuts.review_next_file, "", "Alt+Down was claimed");
    }

    #[test]
    fn session_round_trip_preserves_order_selection_and_unusual_file_names() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        let session = Session {
            paths: vec!["/project/z\nλ.txt".into(), "/project/a.txt".into()],
            active: Some(0),
            sidebar_open: false,
            expanded_directories: BTreeMap::from([(
                "/project".into(),
                vec!["src".into(), "src/nested".into()],
            )]),
        };
        save_session(&settings, &session).unwrap();
        assert_eq!(load_session(&settings).unwrap(), session);
        let previous_reader = std::fs::File::open(settings.with_file_name("session.json")).unwrap();
        let replacement = Session {
            paths: vec![session.paths[1].clone()],
            active: Some(0),
            sidebar_open: true,
            expanded_directories: BTreeMap::new(),
        };
        save_session(&settings, &replacement).unwrap();
        assert_eq!(load_session(&settings).unwrap(), replacement);
        assert_eq!(
            serde_json::from_reader::<_, Session>(previous_reader).unwrap(),
            session
        );
    }

    #[test]
    fn legacy_session_paths_remain_strings_and_preserve_no_selection() {
        let document = r#"{"paths":["/project/z\nλ.txt","/project/a.txt"],"active":null}"#;
        let session: Session = serde_json::from_str(document).unwrap();
        assert_eq!(session.active, None);
        assert_eq!(
            session.paths,
            vec![PathBuf::from("/project/z\nλ.txt"), "/project/a.txt".into()]
        );
        let encoded = serde_json::to_value(&session).unwrap();
        assert_eq!(
            encoded["paths"],
            serde_json::from_str::<serde_json::Value>(document).unwrap()["paths"]
        );
        assert!(
            session
                .startup_paths(Vec::new())
                .iter()
                .all(|(_, focus)| !focus)
        );
    }

    #[test]
    fn raw_byte_session_paths_save_order_selection_and_atomic_replacement() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        let raw = directory
            .path()
            .join(OsString::from_vec(b"raw-\xff.txt".to_vec()));
        let normal = directory.path().join("normal.txt");
        let mut session = Session {
            paths: vec![normal.clone(), raw.clone()],
            active: Some(1),
            ..Session::default()
        };
        save_session(&settings, &session).unwrap();
        assert_eq!(load_session(&settings).unwrap(), session);
        let mut previous_reader =
            std::fs::File::open(settings.with_file_name("session.json")).unwrap();
        let document: serde_json::Value =
            serde_json::from_reader(previous_reader.try_clone().unwrap()).unwrap();
        assert_eq!(document["paths"][0], normal.to_str().unwrap());
        assert_eq!(
            document["paths"][1]["unix_bytes"],
            serde_json::json!(raw.as_os_str().as_encoded_bytes())
        );
        session.paths.swap(0, 1);
        session.active = None;
        save_session(&settings, &session).unwrap();
        assert_eq!(load_session(&settings).unwrap(), session);
        // A reader of the old inode still sees the complete prior snapshot.
        std::io::Seek::rewind(&mut previous_reader).unwrap();
        let previous: Session = serde_json::from_reader(previous_reader).unwrap();
        assert_eq!(previous.paths, vec![normal, raw]);
        assert_eq!(previous.active, Some(1));
    }

    #[test]
    fn invalid_session_paths_do_not_replace_a_saved_snapshot() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        let original = Session {
            paths: vec!["/project/a.txt".into()],
            active: Some(0),
            ..Session::default()
        };
        save_session(&settings, &original).unwrap();
        let invalid = Session {
            paths: vec![OsString::from_vec(b"bad\0path".to_vec()).into()],
            ..Session::default()
        };
        assert!(save_session(&settings, &invalid).is_err());
        assert_eq!(load_session(&settings).unwrap(), original);
    }

    #[test]
    fn malformed_session_paths_are_rejected() {
        for paths in [
            r#"[{"unix_bytes":[256]}]"#,
            r#"[{"unix_bytes":[0]}]"#,
            r#"["bad\u0000path"]"#,
            r#"[{"unix_bytes":[255],"extra":true}]"#,
        ] {
            assert!(
                serde_json::from_str::<Session>(&format!(r#"{{"paths":{paths},"active":null}}"#))
                    .is_err()
            );
        }
    }

    #[test]
    fn session_without_sidebar_state_restores_an_open_sidebar() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        std::fs::write(
            settings.with_file_name("session.json"),
            r#"{"paths":[],"active":null}"#,
        )
        .unwrap();
        assert!(load_session(&settings).unwrap().sidebar_open);
    }

    #[test]
    fn explicit_files_follow_restored_tabs_and_own_startup_focus() {
        let session = Session {
            paths: vec!["/project/b.txt".into(), "/project/a.txt".into()],
            active: Some(0),
            sidebar_open: true,
            expanded_directories: BTreeMap::new(),
        };
        assert_eq!(
            session.startup_paths(Vec::new()),
            vec![
                ("/project/b.txt".into(), true),
                ("/project/a.txt".into(), false)
            ],
        );
        assert_eq!(
            session.startup_paths(vec!["/requested.txt".into()]),
            vec![
                ("/project/b.txt".into(), false),
                ("/project/a.txt".into(), false),
                ("/requested.txt".into(), true),
            ],
        );
    }
}
