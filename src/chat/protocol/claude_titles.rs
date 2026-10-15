//! Claude's persisted session names, using the same custom-title / summary
//! records exposed by the Agent SDK session-info API. No model request is made.
//! Read only after a confirmed resume or top-level result, on the host worker.
//! Metadata format: https://platform.claude.com/cookbook/claude-agent-sdk-05-building-a-session-browser
//! This is presentation metadata, never execution or filesystem ownership proof.
use crate::{agent::Provider, chat::Launch};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const HEAD_BYTES: u64 = 256 * 1024;
const TAIL_BYTES: u64 = 64 * 1024;

pub(super) struct ClaudeTitles {
    config: PathBuf,
    cwd: PathBuf,
    previous: Option<(String, String)>,
}

impl ClaudeTitles {
    pub(super) fn new(launch: &Launch) -> Option<Self> {
        Some(Self {
            config: Provider::Claude.session_root(&launch.recipe)?,
            cwd: launch.recipe.working_directory.clone(),
            previous: None,
        })
    }

    pub(super) fn update(&mut self, id: &str) -> Option<String> {
        let cwd = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        if !cwd.is_absolute() {
            return None;
        }
        let path = crate::agent::claude_transcript(&self.config, &cwd, id).ok()?;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let title = named_title(&mut file, id, &cwd)?;
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

#[derive(Default)]
struct Names {
    matched: bool,
    custom: Option<String>,
    generated: Option<String>,
    summary: Option<String>,
}
impl Names {
    fn note(&mut self, line: &[u8], id: &str, cwd: &Path) {
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if row["isSidechain"] == true
            || row["cwd"]
                .as_str()
                .is_some_and(|path| Path::new(path) != cwd)
        {
            return;
        }
        if let Some(session) = row["sessionId"].as_str() {
            if session != id {
                return;
            }
            if row["cwd"]
                .as_str()
                .is_some_and(|path| Path::new(path) == cwd)
            {
                self.matched = true;
            }
        }
        let label = |key: &str| {
            row[key]
                .as_str()
                .and_then(crate::workspace_model::channel_title)
        };
        match row["type"].as_str() {
            Some("custom-title") => {
                if let Some(title) = label("customTitle") {
                    self.custom = Some(title);
                }
            }
            Some("ai-title") => {
                if let Some(title) = label("aiTitle") {
                    self.generated = Some(title);
                }
            }
            Some("summary") => {
                if let Some(title) = label("summary") {
                    self.summary = Some(title);
                }
            }
            _ => {}
        }
    }
}

fn named_title(file: &mut File, id: &str, cwd: &Path) -> Option<String> {
    file.rewind().ok()?;
    let len = file.metadata().ok()?.len();
    let mut names = Names::default();
    let mut reader = BufReader::new((&mut *file).take(HEAD_BYTES));
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).ok()? != 0 {
        names.note(&line, id, cwd);
        line.clear();
    }
    drop(reader);
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut reader = BufReader::new(file.take(TAIL_BYTES));
    if len > TAIL_BYTES {
        reader.read_until(b'\n', &mut line).ok()?;
        line.clear();
    }
    while reader.read_until(b'\n', &mut line).ok()? != 0 {
        names.note(&line, id, cwd);
        line.clear();
    }
    names
        .matched
        .then(|| names.custom.or(names.generated).or(names.summary))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

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
        append(&mut file, json!({"type":"ai-title","sessionId":id,"aiTitle":"Generated bundle audit"}));
        assert_eq!(named_title(&mut file, &id, cwd).as_deref(), Some("Generated bundle audit"));
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
}
