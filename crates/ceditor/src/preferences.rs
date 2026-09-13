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

#[cfg(test)]
mod tests {
    use super::*;

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
