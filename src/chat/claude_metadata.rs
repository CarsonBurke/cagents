//! Read-only Claude JSONL metadata, shared by live titles and Resume discovery.
//! Full coverage uses bounded memory; an unfinished or oversized record never
//! supplies a name. The live adapter retains the cursor between result events.
use serde_json::Value;
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_LINE_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(crate) struct Lines {
    pub(crate) offset: u64,
    pending: Vec<u8>,
    oversized: bool,
}

impl Lines {
    /// Read through the caller's original EOF, retaining an incomplete line
    /// for a later append. Cancellation or a truncated snapshot returns `false`.
    pub(crate) fn scan(
        &mut self,
        file: &mut File,
        end: u64,
        cancelled: impl Fn() -> bool,
        mut note: impl FnMut(&[u8]),
    ) -> io::Result<bool> {
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file.take(end.saturating_sub(self.offset)));
        loop {
            if cancelled() {
                return Ok(false);
            }
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(self.offset == end);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |index| index + 1);
            if !self.oversized {
                let required = self.pending.len() + count;
                if required <= MAX_LINE_BYTES {
                    if self.pending.capacity() < required {
                        let target = required.next_power_of_two().min(MAX_LINE_BYTES);
                        self.pending.reserve_exact(target - self.pending.len());
                    }
                    self.pending.extend_from_slice(&available[..count]);
                } else {
                    self.pending = Vec::new();
                    self.oversized = true;
                }
            }
            self.offset += count as u64;
            reader.consume(count);
            if newline.is_some() {
                if !self.oversized {
                    note(&self.pending);
                }
                self.pending.clear();
                self.oversized = false;
            }
        }
    }
}

/// Latest valid name in each provider rank. A later generated title cannot
/// replace a custom name, including when ordinary records move it far from EOF.
#[derive(Default)]
pub(crate) struct Names {
    matched: bool,
    custom: Option<String>,
    generated: Option<String>,
    summary: Option<String>,
}

impl Names {
    pub(crate) fn note(&mut self, line: &[u8], id: &str, cwd: &Path) {
        let contains = |needle: &[u8]| line.windows(needle.len()).any(|part| part == needle);
        let title = [
            b"\"customTitle\"".as_slice(),
            b"\"aiTitle\"",
            b"\"summary\"",
        ]
        .into_iter()
        .any(contains);
        if !title && (self.matched || !contains(b"\"sessionId\"")) {
            return;
        }
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if row["isSidechain"] == true
            || row
                .get("cwd")
                .is_some_and(|value| value.as_str().is_none_or(|path| Path::new(path) != cwd))
            || row
                .get("sessionId")
                .is_some_and(|value| value.as_str() != Some(id))
        {
            return;
        }
        if row["sessionId"].as_str() == Some(id)
            && row["cwd"]
                .as_str()
                .is_some_and(|path| Path::new(path) == cwd)
        {
            self.matched = true;
        }
        let label = |key: &str| {
            row[key]
                .as_str()
                .and_then(crate::workspace_model::channel_title)
        };
        match row["type"].as_str() {
            Some("custom-title") => self.custom = label("customTitle").or(self.custom.take()),
            Some("ai-title") => self.generated = label("aiTitle").or(self.generated.take()),
            Some("summary") => self.summary = label("summary").or(self.summary.take()),
            _ => {}
        }
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.matched
            .then(|| {
                self.custom
                    .as_deref()
                    .or(self.generated.as_deref())
                    .or(self.summary.as_deref())
            })
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::Cell, io::Write};

    #[test]
    fn oversized_ordinary_line_is_skipped_without_losing_later_metadata() {
        let mut file = tempfile::tempfile().unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        let cwd = Path::new("/work");
        writeln!(file, "{}", json!({"type":"user","sessionId":id,"cwd":cwd})).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"assistant","message":{"content":"x".repeat(4*MAX_LINE_BYTES)}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"custom-title","sessionId":id,"customTitle":"After large body"})
        )
        .unwrap();
        let mut lines = Lines::default();
        let mut names = Names::default();
        let end = file.metadata().unwrap().len();
        assert!(
            lines
                .scan(&mut file, end, || false, |line| names.note(line, id, cwd))
                .unwrap()
        );
        assert_eq!(names.title(), Some("After large body"));
        assert!(lines.pending.capacity() <= MAX_LINE_BYTES);
        assert_eq!(lines.offset, end);
    }

    #[test]
    fn cancellation_reports_incomplete_coverage_and_cursor_can_continue() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&vec![b'x'; 4 * MAX_LINE_BYTES]).unwrap();
        file.write_all(b"\n{}\n").unwrap();
        let mut lines = Lines::default();
        let calls = Cell::new(0);
        let mut notes = 0;
        let end = file.metadata().unwrap().len();
        assert!(
            !lines
                .scan(
                    &mut file,
                    end,
                    || {
                        calls.set(calls.get() + 1);
                        calls.get() > 2
                    },
                    |_| notes += 1
                )
                .unwrap()
        );
        assert!(lines.offset < end);
        assert_eq!(notes, 0);
        assert!(
            lines
                .scan(&mut file, end, || false, |_| notes += 1)
                .unwrap()
        );
        assert_eq!(notes, 1);
        assert_eq!(lines.offset, end);
    }

    #[test]
    fn same_file_truncation_cannot_report_complete_snapshot_coverage() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"{}\n").unwrap();
        file.write_all(&vec![b'x'; 4 * MAX_LINE_BYTES]).unwrap();
        file.write_all(b"\n").unwrap();
        let truncate = file.try_clone().unwrap();
        let end = file.metadata().unwrap().len();
        let mut lines = Lines::default();
        let mut truncated = false;
        let complete = lines
            .scan(
                &mut file,
                end,
                || false,
                |_| {
                    if !truncated {
                        truncate.set_len(0).unwrap();
                        truncated = true;
                    }
                },
            )
            .unwrap();
        assert!(truncated);
        assert!(
            !complete,
            "Early EOF after truncation is not the captured EOF"
        );
        assert!(lines.offset < end);
    }
}
