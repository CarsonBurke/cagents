//! Explicit, newly appended provider boundaries. Initial history is a baseline,
//! never an announcement; activity becoming idle is not completion evidence.
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    os::unix::fs::{FileExt, MetadataExt},
    time::{Duration, Instant},
};

use crate::notifications::{Evidence, Kind};

const MAX_FILES: usize = 4096;
const MAX_LINE: usize = 256 * 1024;
const MAX_READ: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Format {
    Codex,
    Claude,
}

struct Cursor {
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
    seen: Instant,
    eligible: Option<bool>,
}

#[derive(Default)]
pub(super) struct Tails(BTreeMap<(i32, u64, u64, u64), Cursor>);

impl Tails {
    #[cfg(test)]
    fn read(&mut self, file: &File, format: Format) -> io::Result<Option<Evidence>> {
        self.read_for_process(file, format, (1, 1))
    }

    pub fn read_for_process(
        &mut self,
        file: &File,
        format: Format,
        process: (i32, u64),
    ) -> io::Result<Option<Evidence>> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(None);
        }
        let key = (process.0, process.1, metadata.dev(), metadata.ino());
        let size = metadata.len();
        if !self.0.contains_key(&key) && self.0.len() >= MAX_FILES {
            self.0
                .retain(|_, cursor| cursor.seen.elapsed() < Duration::from_secs(600));
            if self.0.len() >= MAX_FILES {
                return Ok(None);
            }
        }
        if self.0.get(&key).is_none_or(|cursor| cursor.offset > size) {
            // A partial historical line must also be skipped to its newline.
            let mut last = [b'\n'];
            if size > 0 {
                file.read_exact_at(&mut last, size - 1)?;
            }
            let eligible = match format {
                Format::Codex => main_codex_rollout(file)?,
                Format::Claude => Some(true),
            };
            self.0.insert(
                key,
                Cursor {
                    offset: size,
                    partial: Vec::new(),
                    skipping: last[0] != b'\n',
                    seen: Instant::now(),
                    eligible,
                },
            );
            return Ok(None);
        }
        let cursor = self.0.get_mut(&key).expect("cursor was initialized");
        cursor.seen = Instant::now();
        if cursor.eligible.is_none() {
            cursor.eligible = main_codex_rollout(file)?;
        }
        if cursor.eligible != Some(true) {
            cursor.offset = size;
            return Ok(None);
        }
        let end = size.min(cursor.offset.saturating_add(MAX_READ));
        let mut buffer = [0; 64 * 1024];
        let mut evidence = None;
        while cursor.offset < end {
            let length = (end - cursor.offset).min(buffer.len() as u64) as usize;
            file.read_exact_at(&mut buffer[..length], cursor.offset)?;
            for segment in buffer[..length].split_inclusive(|byte| *byte == b'\n') {
                cursor.offset += segment.len() as u64;
                let complete = segment.last() == Some(&b'\n');
                let content = if complete {
                    &segment[..segment.len() - 1]
                } else {
                    segment
                };
                if !cursor.skipping {
                    if cursor.partial.len() + content.len() > MAX_LINE {
                        cursor.partial.clear();
                        cursor.skipping = true;
                    } else {
                        cursor.partial.extend_from_slice(content);
                    }
                }
                if complete {
                    if !cursor.skipping
                        && let Some((kind, identity)) = boundary(&cursor.partial, format)
                    {
                        evidence = Some(Evidence {
                            key: identity.unwrap_or_else(|| {
                                format!("log:{}:{}:{}", key.2, key.3, cursor.offset)
                            }),
                            kind,
                        });
                    }
                    cursor.partial.clear();
                    cursor.skipping = false;
                }
            }
        }
        cursor.partial.shrink_to(4096);
        Ok(evidence)
    }
}

fn main_codex_rollout(file: &File) -> io::Result<Option<bool>> {
    let mut bytes = vec![0; MAX_LINE];
    let read = file.read_at(&mut bytes, 0)?;
    let Some(end) = bytes[..read].iter().position(|byte| *byte == b'\n') else {
        return Ok((read == MAX_LINE).then_some(false));
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes[..end]) else {
        return Ok(Some(false));
    };
    Ok(Some(
        value["type"] == "session_meta"
            && matches!(value["payload"]["source"].as_str(), Some("cli" | "exec")),
    ))
}

fn boundary(line: &[u8], format: Format) -> Option<(Kind, Option<String>)> {
    let markers: &[&[u8]] = match format {
        Format::Codex => &[b"\"task_complete\"", b"\"turn_aborted\""],
        Format::Claude => &[b"\"end_turn\""],
    };
    if !markers
        .iter()
        .any(|marker| line.windows(marker.len()).any(|bytes| bytes == *marker))
    {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    match format {
        Format::Codex if value["type"] == "event_msg" => {
            let kind = match value["payload"]["type"].as_str()? {
                "task_complete" => Kind::Succeeded,
                "turn_aborted" => Kind::Cancelled,
                _ => return None,
            };
            // File identity + offset is stable across observer reconnects and
            // distinguishes repeated provider turn IDs in imported history.
            Some((kind, None))
        }
        Format::Claude if value["type"] == "assistant" && value["isSidechain"] != true => {
            let kind = match value["message"]["stop_reason"].as_str()? {
                "end_turn" => Kind::Succeeded,
                _ => return None,
            };
            let identity = value["uuid"]
                .as_str()
                .filter(|id| id.len() <= 128)
                .map(|id| format!("claude:{id}"));
            Some((kind, identity))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn codex_baselines_history_then_reports_only_complete_new_boundaries_once() {
        let mut file = tempfile::tempfile().unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"source":"cli"}}}}"#
        )
        .unwrap();
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;
        writeln!(file, "{complete}").unwrap();
        let mut tails = Tails::default();
        assert!(tails.read(&file, Format::Codex).unwrap().is_none());
        write!(file, "{complete}").unwrap();
        assert!(tails.read(&file, Format::Codex).unwrap().is_none());
        writeln!(file).unwrap();
        let first = tails.read(&file, Format::Codex).unwrap().unwrap();
        assert_eq!(first.kind, Kind::Succeeded);
        assert!(tails.read(&file, Format::Codex).unwrap().is_none());
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"turn_aborted"}}}}"#
        )
        .unwrap();
        let cancelled = tails.read(&file, Format::Codex).unwrap().unwrap();
        assert_eq!(cancelled.kind, Kind::Cancelled);
        assert_ne!(first.key, cancelled.key);
        // Restart is another census baseline, never replay of old backlog.
        assert!(
            Tails::default()
                .read(&file, Format::Codex)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn subagent_rollouts_and_unknown_sources_never_announce_parent_completion() {
        for source in [
            r#"{"subagent":{"thread_spawn":{}}}"#,
            r#""future-provider""#,
        ] {
            let mut file = tempfile::tempfile().unwrap();
            writeln!(
                file,
                "{{\"type\":\"session_meta\",\"payload\":{{\"source\":{source}}}}}"
            )
            .unwrap();
            let mut tails = Tails::default();
            assert!(tails.read(&file, Format::Codex).unwrap().is_none());
            writeln!(
                file,
                r#"{{"type":"event_msg","payload":{{"type":"task_complete"}}}}"#
            )
            .unwrap();
            assert!(tails.read(&file, Format::Codex).unwrap().is_none());
        }
    }

    #[test]
    fn codex_file_opened_before_metadata_recovers_without_replaying_rewrites() {
        use std::io::{Seek, SeekFrom};
        let mut file = tempfile::tempfile().unwrap();
        let mut tails = Tails::default();
        assert!(tails.read(&file, Format::Codex).unwrap().is_none());
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"source":"cli"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"task_complete"}}}}"#
        )
        .unwrap();
        assert_eq!(
            tails.read(&file, Format::Codex).unwrap().unwrap().kind,
            Kind::Succeeded
        );
        file.set_len(0).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"source":"cli"}}}}"#
        )
        .unwrap();
        assert!(tails.read(&file, Format::Codex).unwrap().is_none());
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"turn_aborted"}}}}"#
        )
        .unwrap();
        assert_eq!(
            tails.read(&file, Format::Codex).unwrap().unwrap().kind,
            Kind::Cancelled
        );
    }

    #[test]
    fn replacement_process_baselines_the_same_inode_before_announcing_new_work() {
        let mut file = tempfile::tempfile().unwrap();
        let mut tails = Tails::default();
        assert!(
            tails
                .read_for_process(&file, Format::Claude, (7, 10))
                .unwrap()
                .is_none()
        );
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"stop_reason":"end_turn"}}}}"#
        )
        .unwrap();
        // This boundary predates the replacement process's initial census.
        assert!(
            tails
                .read_for_process(&file, Format::Claude, (7, 11))
                .unwrap()
                .is_none()
        );
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"stop_reason":"end_turn"}}}}"#
        )
        .unwrap();
        assert!(
            tails
                .read_for_process(&file, Format::Claude, (7, 11))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn claude_ignores_tools_sidechains_and_initial_partial_history() {
        let mut file = tempfile::tempfile().unwrap();
        write!(
            file,
            r#"{{"type":"assistant","message":{{"stop_reason":"end_turn"}}"#
        )
        .unwrap();
        let mut tails = Tails::default();
        assert!(tails.read(&file, Format::Claude).unwrap().is_none());
        writeln!(file, "}}").unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"stop_reason":"end_turn"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"stop_reason":"tool_use"}}}}"#
        )
        .unwrap();
        assert!(tails.read(&file, Format::Claude).unwrap().is_none());
        writeln!(
            file,
            r#"{{"type":"assistant","uuid":"settled","message":{{"stop_reason":"end_turn"}}}}"#
        )
        .unwrap();
        let evidence = tails.read(&file, Format::Claude).unwrap().unwrap();
        assert_eq!(evidence.kind, Kind::Succeeded);
        assert_eq!(evidence.key, "claude:settled");
    }
}
