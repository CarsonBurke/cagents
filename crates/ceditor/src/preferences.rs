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
    pub paths: Vec<PathBuf>,
    pub active: Option<usize>,
    #[serde(default = "default_sidebar_open")]
    pub sidebar_open: bool,
    /// File-tree folders the user opened, keyed by worktree path. Everything
    /// else renders collapsed, so whole project trees stay cheap to load.
    #[serde(default)]
    pub expanded_directories: BTreeMap<String, Vec<String>>,
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
