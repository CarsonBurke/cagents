//! Claude's persisted names, refreshed only after a confirmed resume/result.
//! Metadata is presentation only, never execution or filesystem ownership proof.
use crate::chat::{
    Launch,
    claude_metadata::{Lines, Names},
};
use std::{fs::File, os::unix::fs::MetadataExt, path::PathBuf};

pub(super) struct ClaudeTitles {
    source: Source,
    previous: Option<(String, String)>,
    transcript: Option<Transcript>,
}

enum Source {
    Live {
        config: PathBuf,
        cwd: PathBuf,
    },
    /// A clone of the History body's original open file. Never resolve its
    /// recorded cwd or reopen a replacement pathname just to get a title.
    History {
        file: File,
        cwd: PathBuf,
    },
}

struct Transcript {
    id: String,
    path: Option<PathBuf>,
    cwd: PathBuf,
    device: u64,
    inode: u64,
    len: u64,
    changed: (i64, i64, i64, i64),
    lines: Lines,
    names: Names,
}

impl ClaudeTitles {
    pub(super) fn new(launch: &Launch) -> Option<Self> {
        let recipe = &launch.recipe;
        if !recipe.working_directory.is_absolute() {
            return None;
        }
        let value = |key: &str| {
            (!recipe.unset_environment.contains(key))
                .then(|| recipe.environment.get(key))
                .flatten()
                .filter(|value| !value.is_empty())
        };
        let config = match value("CLAUDE_CONFIG_DIR") {
            Some(config) => recipe.working_directory.join(config),
            None => {
                let home = std::path::Path::new(value("HOME")?);
                if !home.is_absolute() {
                    return None;
                }
                home.join(".claude")
            }
        };
        Some(Self {
            source: Source::Live {
                config,
                cwd: recipe.working_directory.clone(),
            },
            previous: None,
            transcript: None,
        })
    }

    pub(super) fn from_history(file: File, cwd: PathBuf) -> Self {
        Self {
            source: Source::History { file, cwd },
            previous: None,
            transcript: None,
        }
    }

    pub(super) fn update(&mut self, id: &str) -> Option<String> {
        let (cwd, path, mut file) = match &self.source {
            Source::Live { config, cwd } => {
                let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
                if !cwd.is_absolute() {
                    return None;
                }
                let path = crate::agent::claude_transcript(config, &cwd, id).ok()?;
                use std::os::unix::fs::OpenOptionsExt;
                let file = File::options()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&path)
                    .ok()?;
                (cwd, Some(path), file)
            }
            Source::History { file, cwd } => (cwd.clone(), None, file.try_clone().ok()?),
        };
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        let changed = (
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        );
        if self.transcript.as_ref().is_none_or(|saved| {
            saved.id != id
                || saved.path != path
                || saved.cwd != cwd
                || saved.device != metadata.dev()
                || saved.inode != metadata.ino()
                || metadata.len() < saved.lines.offset
                || metadata.len() < saved.len
                || (metadata.len() == saved.len && changed != saved.changed)
        }) {
            self.transcript = Some(Transcript {
                id: id.to_owned(),
                path,
                cwd: cwd.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
                len: 0,
                changed,
                lines: Lines::default(),
                names: Names::default(),
            });
        }
        let saved = self.transcript.as_mut()?;
        let names = &mut saved.names;
        let complete = saved
            .lines
            .scan(
                &mut file,
                metadata.len(),
                || false,
                |line| names.note(line, id, &cwd),
            )
            .ok()?;
        saved.len = metadata.len();
        saved.changed = changed;
        // Do not publish an inferior partial first-pass result on an IO error
        // or interrupted coverage. Subsequent results only read new bytes.
        let title = complete.then(|| saved.names.title()).flatten()?.to_owned();
        if self
            .previous
            .as_ref()
            .is_some_and(|(session, previous)| session == id && previous == &title)
        {
            return None;
        }
        self.previous = Some((id.to_owned(), title.clone()));
        Some(title)
    }
}

#[cfg(test)]
fn named_title(file: &mut File, id: &str, cwd: &std::path::Path) -> Option<String> {
    let mut lines = Lines::default();
    let mut names = Names::default();
    let len = file.metadata().ok()?.len();
    lines
        .scan(file, len, || false, |line| names.note(line, id, cwd))
        .ok()?;
    names.title().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;
    use std::{
        io::{Seek, SeekFrom},
        path::Path,
    };

    fn append(file: &mut File, value: Value) {
        writeln!(file, "{value}").unwrap();
        file.flush().unwrap();
        file.rewind().unwrap();
    }

    #[test]
    fn exact_session_prefers_custom_name_then_latest_summary() {
        let mut file = tempfile::tempfile().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let cwd = Path::new("/work");
        append(
            &mut file,
            json!({"sessionId":id,"cwd":cwd,"type":"user","message":{}}),
        );
        file.seek(SeekFrom::End(0)).unwrap();
        append(
            &mut file,
            json!({"type":"summary","summary":"Bundle encryption review"}),
        );
        assert_eq!(
            named_title(&mut file, &id, cwd).as_deref(),
            Some("Bundle encryption review")
        );
        file.seek(SeekFrom::End(0)).unwrap();
        append(
            &mut file,
            json!({"type":"ai-title","sessionId":id,"aiTitle":"Generated bundle audit"}),
        );
        assert_eq!(
            named_title(&mut file, &id, cwd).as_deref(),
            Some("Generated bundle audit")
        );
        file.seek(SeekFrom::End(0)).unwrap();
        append(
            &mut file,
            json!({"type":"custom-title","sessionId":id,"customTitle":"Encryption and throughput"}),
        );
        assert_eq!(
            named_title(&mut file, &id, cwd).as_deref(),
            Some("Encryption and throughput")
        );
    }

    #[test]
    fn configured_titles_use_exact_supported_roots_and_refuse_ambiguous_home() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let home = cwd.join("home");
        let config = cwd.join("config");
        let id = "00000000-0000-4000-8000-000000000001";
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        for (root, title) in [
            (home.join(".claude"), "Home title"),
            (config.clone(), "Configured title"),
        ] {
            let project = root.join("projects").join(&project);
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(
                project.join(format!("{id}.jsonl")),
                format!(
                    "{}\n{}\n",
                    json!({"type":"user","sessionId":id,"cwd":cwd,"message":{}}),
                    json!({"type":"custom-title","sessionId":id,"customTitle":title})
                ),
            )
            .unwrap();
        }
        let mut launch = Launch {
            provider: crate::agent::Provider::Claude,
            session_id: Some(id.into()),
            recipe: crate::model::LaunchRecipe {
                program: "claude".into(),
                arguments: vec![],
                working_directory: cwd,
                environment: [("CLAUDE_CONFIG_DIR".into(), config.to_str().unwrap().into())].into(),
                unset_environment: Default::default(),
            },
        };
        // An explicit config requires no HOME and resolves relative paths in
        // the launch directory, just like the existing exact history reader.
        assert_eq!(
            ClaudeTitles::new(&launch).unwrap().update(id).as_deref(),
            Some("Configured title")
        );
        launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "config".into());
        launch
            .recipe
            .environment
            .insert("HOME".into(), "home".into());
        assert_eq!(
            ClaudeTitles::new(&launch).unwrap().update(id).as_deref(),
            Some("Configured title")
        );
        launch.recipe.environment.remove("CLAUDE_CONFIG_DIR");
        assert!(
            ClaudeTitles::new(&launch).is_none(),
            "relative fallback HOME has no coherent history/title origin"
        );
        launch
            .recipe
            .environment
            .insert("HOME".into(), home.to_str().unwrap().into());
        assert_eq!(
            ClaudeTitles::new(&launch).unwrap().update(id).as_deref(),
            Some("Home title")
        );
        launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), config.to_str().unwrap().into());
        launch
            .recipe
            .unset_environment
            .insert("CLAUDE_CONFIG_DIR".into());
        assert_eq!(
            ClaudeTitles::new(&launch).unwrap().update(id).as_deref(),
            Some("Home title")
        );
        launch.recipe.unset_environment.insert("HOME".into());
        assert!(ClaudeTitles::new(&launch).is_none());
        launch.recipe.unset_environment.remove("CLAUDE_CONFIG_DIR");
        assert_eq!(
            ClaudeTitles::new(&launch).unwrap().update(id).as_deref(),
            Some("Configured title")
        );
        launch.recipe.working_directory = "relative-work".into();
        assert!(ClaudeTitles::new(&launch).is_none());
    }

    #[test]
    fn authoritative_custom_names_can_be_numeric_or_emoji() {
        let id = uuid::Uuid::new_v4().to_string();
        let cwd = Path::new("/work");
        for name in ["1341", "🔐", "🧑‍💻"] {
            let mut file = tempfile::tempfile().unwrap();
            append(
                &mut file,
                json!({"type":"user","sessionId":id,"cwd":cwd,"message":{}}),
            );
            file.seek(SeekFrom::End(0)).unwrap();
            append(
                &mut file,
                json!({"type":"custom-title","sessionId":id,"customTitle":name}),
            );
            assert_eq!(named_title(&mut file, &id, cwd).as_deref(), Some(name));
        }
    }

    #[test]
    fn wrong_session_directory_and_sidechain_cannot_name_parent() {
        let mut file = tempfile::tempfile().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        append(
            &mut file,
            json!({"sessionId":id,"cwd":"/other","type":"user","message":{}}),
        );
        file.seek(SeekFrom::End(0)).unwrap();
        append(
            &mut file,
            json!({"type":"summary","summary":"Foreign work"}),
        );
        assert!(named_title(&mut file, &id, Path::new("/work")).is_none());
        file.rewind().unwrap();
        assert!(
            named_title(
                &mut file,
                &uuid::Uuid::new_v4().to_string(),
                Path::new("/other")
            )
            .is_none()
        );
        file.set_len(0).unwrap();
        file.rewind().unwrap();
        append(
            &mut file,
            json!({"sessionId":id,"cwd":"/work","isSidechain":true,"type":"summary","summary":"Child work"}),
        );
        assert!(named_title(&mut file, &id, Path::new("/work")).is_none());
    }

    fn live_fixture() -> (tempfile::TempDir, Launch, String, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let config = directory.path().join("config");
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let id = "00000000-0000-4000-8000-000000000001".to_owned();
        let parent = config.join("projects").join(project);
        std::fs::create_dir_all(&parent).unwrap();
        let path = parent.join(format!("{id}.jsonl"));
        let launch = Launch {
            provider: crate::agent::Provider::Claude,
            session_id: Some(id.clone()),
            recipe: crate::model::LaunchRecipe {
                program: "claude".into(),
                arguments: vec![],
                working_directory: cwd,
                environment: [("CLAUDE_CONFIG_DIR".into(), config.to_str().unwrap().into())].into(),
                unset_environment: Default::default(),
            },
        };
        (directory, launch, id, path)
    }

    fn history(path: &Path, id: &str, cwd: &Path, name: &str) {
        std::fs::write(path, format!("{}\n{}\n", json!({"type":"user","sessionId":id,"cwd":cwd,"message":{"content":"First prompt"}}), json!({"type":"custom-title","sessionId":id,"customTitle":name}))).unwrap();
    }

    #[test]
    fn middle_custom_name_survives_append_and_fresh_live_resume() {
        let (_directory, launch, id, path) = live_fixture();
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{}", json!({"type":"user","sessionId":id,"cwd":launch.recipe.working_directory,"message":{"content":"First prompt"}})).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"ai-title","sessionId":id,"aiTitle":"Early generated"})
        )
        .unwrap();
        let ordinary = json!({"type":"assistant","sessionId":id,"cwd":launch.recipe.working_directory,"message":{"content":"x".repeat(4096)}});
        for _ in 0..70 {
            writeln!(file, "{ordinary}").unwrap();
        }
        writeln!(
            file,
            "{}",
            json!({"type":"custom-title","sessionId":id,"customTitle":"Middle custom"})
        )
        .unwrap();
        file.flush().unwrap();
        let mut live = ClaudeTitles::new(&launch).unwrap();
        assert_eq!(live.update(&id).as_deref(), Some("Middle custom"));
        for _ in 0..30 {
            writeln!(file, "{ordinary}").unwrap();
        }
        writeln!(
            file,
            "{}",
            json!({"type":"ai-title","sessionId":id,"aiTitle":"Later generated"})
        )
        .unwrap();
        file.flush().unwrap();
        assert_eq!(
            live.update(&id),
            None,
            "Appending ordinary/generated rows must not revert custom"
        );
        assert_eq!(
            ClaudeTitles::new(&launch).unwrap().update(&id).as_deref(),
            Some("Middle custom"),
            "A fresh live resume must find the middle too"
        );
        writeln!(
            file,
            "{}",
            json!({"type":"custom-title","sessionId":id,"customTitle":"Newest custom"})
        )
        .unwrap();
        file.flush().unwrap();
        assert_eq!(live.update(&id).as_deref(), Some("Newest custom"));
        assert_eq!(
            live.transcript.as_ref().unwrap().lines.offset,
            file.metadata().unwrap().len()
        );
    }

    #[test]
    fn replaced_truncated_and_different_session_files_reset_live_cursor() {
        let (_directory, launch, id, path) = live_fixture();
        let cwd = &launch.recipe.working_directory;
        history(&path, &id, cwd, "Original custom");
        let mut live = ClaudeTitles::new(&launch).unwrap();
        assert_eq!(live.update(&id).as_deref(), Some("Original custom"));
        let replacement = path.with_extension("replacement");
        history(&replacement, &id, cwd, "Replaced custom");
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(live.update(&id).as_deref(), Some("Replaced custom"));
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(live.update(&id), None);
        history(&path, &id, cwd, "Reset custom");
        assert_eq!(live.update(&id).as_deref(), Some("Reset custom"));
        history(&path, &id, cwd, "Fresh custom");
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123))
            .unwrap();
        assert_eq!(
            live.update(&id).as_deref(),
            Some("Fresh custom"),
            "Same-size rewrite resets ranked names too"
        );
        let other = "00000000-0000-4000-8000-000000000002";
        let other_path = path.with_file_name(format!("{other}.jsonl"));
        history(&other_path, other, cwd, "Other session");
        assert_eq!(live.update(other).as_deref(), Some("Other session"));
        let mut file = File::options().append(true).open(&other_path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"custom-title","sessionId":id,"customTitle":"Wrong session"})
        )
        .unwrap();
        assert_eq!(live.update(other), None);
        assert_eq!(live.update(&id).as_deref(), Some("Fresh custom"));
    }

    #[test]
    fn incomplete_live_title_is_retained_until_its_line_finishes() {
        let (_directory, launch, id, path) = live_fixture();
        history(
            &path,
            &id,
            &launch.recipe.working_directory,
            "Existing custom",
        );
        let mut live = ClaudeTitles::new(&launch).unwrap();
        assert_eq!(live.update(&id).as_deref(), Some("Existing custom"));
        let record = format!(
            "{}\n",
            json!({"type":"custom-title","sessionId":id,"customTitle":"Completed custom"})
        );
        let cut = record.len() / 2;
        let mut file = File::options().append(true).open(&path).unwrap();
        file.write_all(&record.as_bytes()[..cut]).unwrap();
        file.flush().unwrap();
        assert_eq!(live.update(&id), None);
        file.write_all(&record.as_bytes()[cut..]).unwrap();
        file.flush().unwrap();
        assert_eq!(live.update(&id).as_deref(), Some("Completed custom"));
    }
}
