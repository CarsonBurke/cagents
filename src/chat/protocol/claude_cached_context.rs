//! Read-only provenance recovery for presentation cached by older Claude helpers.
//! The helper remains the owner of execution and bytes. This index only corrects
//! attribution when an exact provider record proves where a displayed slice came
//! from; it never recognizes instructions by their text.
use crate::{
    account::scope::LaunchAccountScope,
    agent::Provider,
    chat::{
        DeliveryKind, Item, ItemContent, Launch, Role, TranscriptPart, presentation::ItemChange,
    },
};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::Arc,
};

const MAX_LINE: usize = 30 * 1024 * 1024;
const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 200_000;
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct CachedContext {
    source: Option<Source>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    len: u64,
    changed: (i64, i64, i64, i64),
}
impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        (metadata.is_file() && metadata.len() <= MAX_FILE).then_some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            changed: (
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        })
    }
    fn same_file(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    offset: u64,
    bytes: usize,
    block: Option<usize>,
    base: usize,
}
struct Carrier {
    base: usize,
    items: Vec<String>,
}
struct Source {
    path: PathBuf,
    scope: LaunchAccountScope,
    cwd: PathBuf,
    session: String,
    stamp: Stamp,
    offset: u64,
    records: usize,
    charged: usize,
    failed: bool,
    entries: HashMap<String, Entry>,
    carriers: HashMap<String, Carrier>,
    message_offsets: HashMap<String, usize>,
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

impl CachedContext {
    /// A changed proof set requires a full publication even if the old helper's
    /// transcript generation did not advance while its JSONL writer caught up.
    pub(crate) fn refresh(
        &mut self,
        launch: &Launch,
        scope: &LaunchAccountScope,
        session: &str,
    ) -> bool {
        self.refresh_with(launch, scope, session, || {})
    }

    fn refresh_with(
        &mut self,
        launch: &Launch,
        scope: &LaunchAccountScope,
        session: &str,
        after_scan: impl FnOnce(),
    ) -> bool {
        let candidate = (|| {
            if launch.provider != Provider::Claude
                || scope.provider != Provider::Claude
                || session.len() != 36
                || uuid::Uuid::parse_str(session).is_err()
                || !launch.recipe.working_directory.is_absolute()
                || scope.revalidate().is_err()
            {
                return None;
            }
            let cwd = launch
                .recipe
                .working_directory
                .canonicalize()
                .unwrap_or_else(|_| launch.recipe.working_directory.clone());
            let path =
                crate::agent::claude_transcript(&scope.canonical_home, &cwd, session).ok()?;
            // A projects symlink must not move this read outside the captured home.
            if !path.starts_with(&scope.canonical_home) {
                return None;
            }
            let file = open(&path)?;
            let stamp = Stamp::of(&file.metadata().ok()?)?;
            scope.revalidate().ok()?;
            Some((cwd, path, file, stamp))
        })();
        let Some((cwd, path, mut file, stamp)) = candidate else {
            return self.clear();
        };
        let reset = self.source.as_ref().is_none_or(|source| {
            source.path != path
                || source.scope != *scope
                || source.cwd != cwd
                || source.session != session
                || !source.stamp.same_file(stamp)
                || stamp.len < source.stamp.len
                || (stamp.len == source.stamp.len && source.stamp != stamp)
                || (source.failed && source.stamp != stamp)
        });
        let mut changed = false;
        if reset {
            changed = self.clear();
            self.source = Some(Source {
                path,
                scope: scope.clone(),
                cwd,
                session: session.to_owned(),
                stamp,
                offset: 0,
                records: 0,
                charged: 0,
                failed: false,
                entries: HashMap::new(),
                carriers: HashMap::new(),
                message_offsets: HashMap::new(),
                #[cfg(test)]
                reads: std::sync::atomic::AtomicUsize::new(0),
            });
        }
        let source = self.source.as_mut().unwrap();
        if source.failed {
            return changed;
        }
        let previously_proven = !source.entries.is_empty();
        let result = source.scan(&mut file, stamp.len);
        after_scan();
        let after = file
            .metadata()
            .ok()
            .and_then(|metadata| Stamp::of(&metadata));
        // The scan is bounded by its original EOF. A provider appending its
        // next record does not invalidate that prefix; each projected slice
        // is independently checked against its current raw bytes below.
        let stable = after.is_some_and(|after| {
            stamp.same_file(after) && (after == stamp || after.len > stamp.len)
        }) && source.scope.revalidate().is_ok();
        source.stamp = after.filter(|_| stable).unwrap_or(stamp);
        match result {
            Some(updated) if stable => changed | updated,
            _ => {
                changed |= previously_proven;
                source.entries.clear();
                source.carriers.clear();
                source.message_offsets.clear();
                source.failed = true;
                changed
            }
        }
    }

    fn clear(&mut self) -> bool {
        self.source
            .take()
            .is_some_and(|source| !source.entries.is_empty())
    }

    /// Copy only immutable read proofs for publication. Scanner cursors and
    /// carrier bookkeeping stay with the worker, and no message bodies are
    /// retained in either copy. Only the original scanner should be refreshed.
    pub(crate) fn projection(&self) -> Self {
        Self {
            source: self.source.as_ref().map(|source| Source {
                path: source.path.clone(),
                scope: source.scope.clone(),
                cwd: source.cwd.clone(),
                session: source.session.clone(),
                stamp: source.stamp,
                offset: 0,
                records: 0,
                charged: 0,
                failed: source.failed,
                entries: source.entries.clone(),
                carriers: HashMap::new(),
                message_offsets: HashMap::new(),
                #[cfg(test)]
                reads: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Classify a parent-transcript item. Callers must not pass child transcript
    /// items, whose provider-local IDs have a different provenance scope.
    pub(crate) fn classify(&mut self, item: &mut Item) -> bool {
        let Some(source) = &self.source else {
            return false;
        };
        let Some((entry, paged)) = source.candidate(item) else {
            return false;
        };
        let Some(frame) = source.read(entry) else {
            return false;
        };
        if !matches_frame(item, &frame, entry, paged) {
            return false;
        }
        project(item);
        true
    }

    /// Decode each consecutive source record once for a parent window, however
    /// many blocks or paged slices it supplies. The single decoded record is
    /// dropped before reading another and never retained between publications.
    pub(crate) fn classify_items(&mut self, items: &mut [Arc<Item>]) -> bool {
        let Some(source) = &self.source else {
            return false;
        };
        let mut decoded: Option<((u64, usize), Option<Value>)> = None;
        let mut changed = false;
        for item in items {
            let Some((entry, paged)) = source.candidate(item) else {
                continue;
            };
            let location = (entry.offset, entry.bytes);
            if decoded.as_ref().is_none_or(|(saved, _)| *saved != location) {
                // Drop the old frame before evaluating the next read.
                drop(decoded.take());
                let frame = source.read(entry);
                decoded = Some((location, frame));
            }
            if let Some((_, Some(frame))) = &decoded
                && matches_frame(item, frame, entry, paged)
            {
                project(Arc::make_mut(item));
                changed = true;
            }
        }
        changed
    }

    /// Append carries no old body. Ask the helper for the bounded full window
    /// before converting an indexed context item to a different content variant.
    pub(crate) fn needs_full(&self, change: &ItemChange) -> bool {
        let ItemChange::Append { item, .. } = change else {
            return false;
        };
        item.role == Role::User
            && matches!(
                item.content,
                ItemContent::Text {
                    thinking: false,
                    ..
                }
            )
            && self.source.as_ref().is_some_and(|source| {
                source.entries.contains_key(&item.id)
                    || TranscriptPart::parse(&item.id)
                        .is_some_and(|part| source.entries.contains_key(part.item))
            })
    }
}

fn open(path: &std::path::Path) -> Option<File> {
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

impl Source {
    fn candidate(&self, item: &Item) -> Option<(Entry, bool)> {
        if item.role != Role::User
            || !matches!(
                item.content,
                ItemContent::Text {
                    thinking: false,
                    ..
                }
            )
        {
            return None;
        }
        // Exact provider IDs win over a coincidental part-looking suffix.
        self.entries
            .get(&item.id)
            .map(|entry| (*entry, false))
            .or_else(|| {
                self.entries
                    .get(TranscriptPart::parse(&item.id)?.item)
                    .map(|entry| (*entry, true))
            })
    }

    fn scan(&mut self, file: &mut File, end: u64) -> Option<bool> {
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut reader = BufReader::new(file.take(end.saturating_sub(self.offset)));
        let mut line = Vec::new();
        let mut changed = false;
        while self.offset < end {
            line.clear();
            let count = Read::by_ref(&mut reader)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut line)
                .ok()?;
            if count > MAX_LINE || count == 0 {
                return None;
            }
            // Revisit an unfinished final record after the provider appends it.
            if !line.ends_with(b"\n") {
                break;
            }
            let offset = self.offset;
            self.offset += count as u64;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            self.records += 1;
            if self.records > MAX_RECORDS {
                return None;
            }
            let frame: Value = serde_json::from_slice(&line).ok()?;
            if !same_session(&frame, &self.session) {
                return None;
            }
            changed |= self.note(&frame, offset, count)?;
        }
        Some(changed)
    }

    fn note(&mut self, frame: &Value, offset: u64, bytes: usize) -> Option<bool> {
        if !matches!(frame["type"].as_str(), Some("user" | "assistant")) {
            return Some(false);
        }
        let Some(carrier) = frame["uuid"].as_str() else {
            return Some(false);
        };
        let message = frame["message"]["id"].as_str().unwrap_or(carrier);
        let base = self
            .carriers
            .get(carrier)
            .map(|saved| saved.base)
            .unwrap_or_else(|| self.message_offsets.get(message).copied().unwrap_or(0));
        let content = &frame["message"]["content"];
        let length = content.as_array().map_or(1, Vec::len);
        let next = base.checked_add(length)?;
        // Charge cumulative input, including replacements, before allocating
        // derived keys. This bounds adversarial UUIDs and huge block arrays.
        self.charge(carrier.len().checked_add(message.len())?.checked_add(128)?)?;
        if self.carriers.len() >= MAX_RECORDS && !self.carriers.contains_key(carrier) {
            return None;
        }
        self.message_offsets.insert(message.to_owned(), next);
        let previous = self.carriers.remove(carrier);
        let mut changed = false;
        if let Some(previous) = previous {
            for key in previous.items {
                changed |= self.entries.remove(&key).is_some();
            }
        }
        let context = is_context(frame);
        let mut items = Vec::new();
        let blocks: Box<dyn Iterator<Item = (Option<usize>, usize)> + '_> =
            if let Some(blocks) = content.as_array() {
                Box::new(blocks.iter().enumerate().filter_map(|(index, block)| {
                    (block["type"].as_str() == Some("text")).then_some((Some(index), base + index))
                }))
            } else if content.is_string() {
                Box::new(std::iter::once((None, 0)))
            } else {
                Box::new(std::iter::empty())
            };
        for (block, index) in blocks {
            self.charge(message.len().checked_add(96)?)?;
            if items.len() >= MAX_RECORDS || self.entries.len() >= MAX_RECORDS {
                return None;
            }
            let key = format!("{message}:{index}");
            if context {
                let entry = Entry {
                    offset,
                    bytes,
                    block,
                    base,
                };
                changed |= self.entries.insert(key.clone(), entry) != Some(entry);
            } else {
                changed |= self.entries.remove(&key).is_some();
            }
            items.push(key);
        }
        self.carriers
            .insert(carrier.to_owned(), Carrier { base, items });
        Some(changed)
    }

    fn charge(&mut self, bytes: usize) -> Option<()> {
        self.charged = self.charged.checked_add(bytes)?;
        (self.charged <= MAX_INDEX_BYTES).then_some(())
    }

    fn read(&self, entry: Entry) -> Option<Value> {
        #[cfg(test)]
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.scope.revalidate().ok()?;
        let mut file = open(&self.path)?;
        let stamp = Stamp::of(&file.metadata().ok()?)?;
        if !stamp.same_file(self.stamp)
            || stamp.len < self.stamp.len
            || (stamp.len == self.stamp.len && stamp != self.stamp)
        {
            return None;
        }
        file.seek(SeekFrom::Start(entry.offset)).ok()?;
        let mut bytes = vec![0; entry.bytes];
        file.read_exact(&mut bytes).ok()?;
        let frame: Value = serde_json::from_slice(&bytes).ok()?;
        self.scope.revalidate().ok()?;
        same_session(&frame, &self.session).then_some(frame)
    }
}

fn matches_frame(item: &Item, frame: &Value, entry: Entry, paged: bool) -> bool {
    let ItemContent::Text { text, .. } = &item.content else {
        return false;
    };
    let part = paged.then(|| TranscriptPart::parse(&item.id)).flatten();
    let key = part.as_ref().map_or(item.id.as_str(), |part| part.item);
    if !is_context(frame) || frame_item_id(frame, entry).as_deref() != Some(key) {
        return false;
    }
    let Some(body) = frame_text(frame, entry.block) else {
        return false;
    };
    if let Some(part) = part {
        part.layout == "text"
            && part.fields[1] == (0, 0)
            && part.fields[0].0 > 0
            && body.get(part.fields[0].0..part.fields[0].1) == Some(text.as_str())
    } else {
        // The first window part retains the original provider ID.
        body.starts_with(text)
    }
}

fn project(item: &mut Item) {
    let ItemContent::Text { text, .. } = &mut item.content else {
        unreachable!();
    };
    let body = std::mem::take(text);
    item.role = Role::System;
    item.content = ItemContent::Delivery {
        kind: DeliveryKind::Notice,
        from: String::new(),
        sender: String::new(),
        summary: "Claude context".into(),
        body,
    };
}

fn same_session(frame: &Value, session: &str) -> bool {
    ["sessionId", "session_id"].iter().all(|field| {
        frame
            .get(field)
            .is_none_or(|value| value.as_str() == Some(session))
    })
}
fn is_context(frame: &Value) -> bool {
    frame["type"].as_str() == Some("user")
        && frame["origin"]["kind"].as_str() != Some("peer")
        && frame["parent_tool_use_id"]
            .as_str()
            .is_none_or(str::is_empty)
        && frame["isSidechain"].as_bool() != Some(true)
        && frame["teamName"].as_str().is_none_or(str::is_empty)
        && [
            "isMeta",
            "isSynthetic",
            "isCompactSummary",
            "isVisibleInTranscriptOnly",
        ]
        .iter()
        .any(|field| frame[*field].as_bool() == Some(true))
}
fn frame_text(frame: &Value, block: Option<usize>) -> Option<&str> {
    let content = &frame["message"]["content"];
    match block {
        None => content.as_str(),
        Some(index) => {
            let block = content.as_array()?.get(index)?;
            (block["type"].as_str() == Some("text"))
                .then(|| block["text"].as_str())
                .flatten()
        }
    }
}
fn frame_item_id(frame: &Value, entry: Entry) -> Option<String> {
    let id = frame["message"]["id"]
        .as_str()
        .or_else(|| frame["uuid"].as_str())?;
    Some(format!(
        "{id}:{}",
        entry.block.map_or(0, |index| entry.base + index)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Phase;
    use serde_json::json;
    use std::io::Write;

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";

    struct Fixture {
        _directory: tempfile::TempDir,
        launch: Launch,
        scope: LaunchAccountScope,
        path: PathBuf,
        index: CachedContext,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let cwd = directory.path().join("work");
            let home = directory.path().join("claude");
            std::fs::create_dir(&cwd).unwrap();
            let project: String = cwd
                .to_str()
                .unwrap()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let project = home.join("projects").join(project);
            std::fs::create_dir_all(&project).unwrap();
            let metadata = std::fs::metadata(&home).unwrap();
            let scope = LaunchAccountScope {
                provider: Provider::Claude,
                canonical_home: home,
                device: metadata.dev(),
                inode: metadata.ino(),
                registered_id: None,
                purge_ids: Vec::new(),
            };
            let path = project.join(format!("{SESSION}.jsonl"));
            File::create(&path).unwrap();
            Self {
                _directory: directory,
                launch: Launch {
                    provider: Provider::Claude,
                    session_id: Some(SESSION.into()),
                    recipe: crate::model::LaunchRecipe {
                        program: "claude".into(),
                        arguments: Vec::new(),
                        working_directory: cwd,
                        // Recovery uses the captured account scope, never these
                        // possibly obsolete launch environment spellings.
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                },
                scope,
                path,
                index: CachedContext::default(),
            }
        }
        fn append(&self, frame: Value) {
            let mut file = File::options().append(true).open(&self.path).unwrap();
            writeln!(file, "{frame}").unwrap();
        }
        fn refresh(&mut self) -> bool {
            self.index.refresh(&self.launch, &self.scope, SESSION)
        }
    }
    fn frame(id: &str, body: &str) -> Value {
        json!({"type":"user","uuid":id,"sessionId":SESSION,"isSynthetic":true,
            "message":{"content":body}})
    }
    fn text(id: &str, body: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::User,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Phase::default(),
            },
            started_at: Some(10),
            at: Some(20),
        }
    }

    #[test]
    fn old_cached_user_context_recovers_attribution_without_changing_bytes_or_identity() {
        let mut fixture = Fixture::new();
        let body = "Base directory for this skill: /example\n\nopaque λ instructions";
        fixture.append(frame("internal", body));
        let original = std::fs::read(&fixture.path).unwrap();
        let mut item = text("internal:0", body);
        assert!(!fixture.index.classify(&mut item));
        assert!(fixture.refresh());
        assert!(fixture.index.classify(&mut item));
        assert_eq!(item.id, "internal:0");
        assert_eq!((item.started_at, item.at), (Some(10), Some(20)));
        assert_eq!(item.role, Role::System);
        assert!(
            matches!(&item.content, ItemContent::Delivery { kind: DeliveryKind::Notice, from, sender, summary, body: bytes }
            if from.is_empty() && sender.is_empty() && summary == "Claude context" && bytes == body)
        );
        assert!(!fixture.index.classify(&mut item));
        assert!(
            !fixture.refresh(),
            "an unchanged index needs no full republish"
        );
        assert_eq!(std::fs::read(&fixture.path).unwrap(), original);
    }

    #[test]
    fn explicit_provenance_preserves_quotes_peers_assistants_children_and_tool_results() {
        let mut fixture = Fixture::new();
        let body = "Identical instruction text is also a valid user quote";
        for (index, marker) in [
            "isMeta",
            "isSynthetic",
            "isCompactSummary",
            "isVisibleInTranscriptOnly",
        ]
        .iter()
        .enumerate()
        {
            for (suffix, flag) in [
                ("true", json!(true)),
                ("false", json!(false)),
                ("null", Value::Null),
                ("string", json!("true")),
            ] {
                let id = format!("{index}-{suffix}");
                let mut row = frame(&id, body);
                row.as_object_mut().unwrap().remove("isSynthetic");
                row[*marker] = flag;
                fixture.append(row);
            }
        }
        for (id, field, value) in [
            ("peer", "origin", json!({"kind":"peer"})),
            ("assistant", "type", json!("assistant")),
            ("child", "parent_tool_use_id", json!("agent-tool")),
            ("sidechain", "isSidechain", json!(true)),
            ("team", "teamName", json!("team")),
        ] {
            let mut row = frame(id, body);
            row[field] = value;
            fixture.append(row);
        }
        fixture.append(json!({"type":"user","uuid":"mixed","sessionId":SESSION,"isSynthetic":true,
            "message":{"content":[{"type":"text","text":body},{"type":"tool_result","tool_use_id":"skill","content":"Launching skill"}]}}));
        assert!(fixture.refresh());
        for index in 0..4 {
            for suffix in ["true", "false", "null", "string"] {
                let mut item = text(&format!("{index}-{suffix}:0"), body);
                assert_eq!(fixture.index.classify(&mut item), suffix == "true");
            }
        }
        for id in ["peer", "assistant", "child", "sidechain", "team", "mixed:1"] {
            assert!(
                !fixture.index.classify(&mut text(&format!("{id}:0"), body)),
                "{id}"
            );
        }
        assert!(fixture.index.classify(&mut text("mixed:0", body)));
        assert!(
            !fixture
                .index
                .classify(&mut text("mixed:1", "Launching skill"))
        );
        assert!(
            !fixture
                .index
                .classify(&mut text("0-true:0", "different cached bytes"))
        );
    }

    #[test]
    fn cumulative_carriers_and_rewritten_flags_do_not_reuse_stale_proofs() {
        let mut fixture = Fixture::new();
        for (id, body) in [("one", "first"), ("two", "second")] {
            let mut row = frame(id, body);
            row["message"] = json!({"id":"message","content":[{"type":"text","text":body}]});
            fixture.append(row);
        }
        assert!(fixture.refresh());
        assert!(fixture.index.classify(&mut text("message:0", "first")));
        assert!(fixture.index.classify(&mut text("message:1", "second")));
        let mut rewritten = frame("one", "first");
        rewritten["isSynthetic"] = json!(false);
        rewritten["message"] = json!({"id":"message","content":[{"type":"text","text":"first"}]});
        fixture.append(rewritten);
        assert!(fixture.refresh());
        assert!(!fixture.index.classify(&mut text("message:0", "first")));
        assert!(fixture.index.classify(&mut text("message:1", "second")));
    }

    #[test]
    fn paged_unicode_slices_and_appends_require_exact_source_layout() {
        let mut fixture = Fixture::new();
        // Greater than the title metadata reader's 256 KiB limit.
        let body = "skill λ instructions\n".repeat(32 * 1024);
        fixture.append(frame("large", &body));
        assert!(fixture.refresh());
        assert!(fixture.index.classify(&mut text("large:0", &body[..20])));
        let range = (21, 40);
        let id = TranscriptPart {
            item: "large:0",
            index: 1,
            layout: "text",
            fields: [range, (0, 0)],
        }
        .id();
        let mut item = text(&id, &body[range.0..range.1]);
        let append = ItemChange::Append {
            item: item.clone(),
            offset: 0,
            suffix: String::new(),
        };
        assert!(fixture.index.needs_full(&append));
        assert!(fixture.index.classify(&mut item));
        assert!(!fixture.index.needs_full(&ItemChange::Replace(item)));
        for (layout, fields, bytes) in [
            ("delivery", [range, (0, 0)], &body[range.0..range.1]),
            ("text", [range, (0, 1)], &body[range.0..range.1]),
            ("text", [(0, 20), (0, 0)], &body[..20]),
            ("text", [range, (0, 0)], "unrelated bytes"),
        ] {
            let id = TranscriptPart {
                item: "large:0",
                index: 1,
                layout,
                fields,
            }
            .id();
            assert!(!fixture.index.classify(&mut text(&id, bytes)));
        }
    }

    #[test]
    fn partial_records_retry_and_unchanged_tail_does_not_reclassify() {
        let mut fixture = Fixture::new();
        let line = serde_json::to_vec(&frame("late", "provider wrote this later")).unwrap();
        let midpoint = line.len() / 2;
        std::fs::write(&fixture.path, &line[..midpoint]).unwrap();
        assert!(!fixture.refresh());
        assert!(
            !fixture
                .index
                .classify(&mut text("late:0", "provider wrote this later"))
        );
        assert!(!fixture.refresh());
        let mut file = File::options().append(true).open(&fixture.path).unwrap();
        file.write_all(&line[midpoint..]).unwrap();
        file.write_all(b"\n").unwrap();
        assert!(fixture.refresh());
        assert!(
            fixture
                .index
                .classify(&mut text("late:0", "provider wrote this later"))
        );
        assert!(!fixture.refresh());
    }

    #[test]
    fn missing_malformed_foreign_and_replaced_sources_leave_user_items_unchanged() {
        let mut fixture = Fixture::new();
        fixture.append(frame("context", "instructions"));
        assert!(fixture.refresh());
        // A pathname replacement invalidates the old record offsets immediately.
        let replacement = fixture.path.with_extension("replacement");
        std::fs::write(&replacement, b"{}\n").unwrap();
        std::fs::rename(&replacement, &fixture.path).unwrap();
        assert!(
            !fixture
                .index
                .classify(&mut text("context:0", "instructions"))
        );
        assert!(fixture.refresh());
        assert!(
            !fixture
                .index
                .classify(&mut text("context:0", "instructions"))
        );
        fixture.append(frame("context", "instructions"));
        assert!(fixture.refresh());
        fixture.append(json!({"sessionId":"00000000-0000-4000-8000-000000000099"}));
        assert!(fixture.refresh());
        assert!(
            !fixture
                .index
                .classify(&mut text("context:0", "instructions"))
        );
        assert!(!fixture.refresh(), "a static invalid file is not rescanned");
        std::fs::write(&fixture.path, b"not JSON\n").unwrap();
        assert!(!fixture.refresh());
        std::fs::remove_file(&fixture.path).unwrap();
        assert!(!fixture.refresh());
    }

    #[test]
    fn captured_home_replacement_never_reads_the_replacement_account() {
        let mut fixture = Fixture::new();
        fixture.append(frame("context", "instructions"));
        assert!(fixture.refresh());
        let home = fixture.scope.canonical_home.clone();
        let removed = home.with_extension("old");
        std::fs::rename(&home, &removed).unwrap();
        std::fs::create_dir(&home).unwrap();
        assert!(
            !fixture
                .index
                .classify(&mut text("context:0", "instructions"))
        );
        assert!(fixture.refresh());
        assert!(
            !fixture
                .index
                .classify(&mut text("context:0", "instructions"))
        );
    }

    #[test]
    fn record_metadata_and_file_limits_fail_open_without_partial_classification() {
        for metadata_limit in [false, true] {
            let mut fixture = Fixture::new();
            fixture.append(frame("known", "instructions"));
            assert!(fixture.refresh());
            let source = fixture.index.source.as_mut().unwrap();
            if metadata_limit {
                source.charged = MAX_INDEX_BYTES;
            } else {
                source.records = MAX_RECORDS;
            }
            fixture.append(frame("tail", "more instructions"));
            assert!(fixture.refresh());
            assert!(!fixture.index.classify(&mut text("known:0", "instructions")));
            assert!(fixture.index.source.as_ref().unwrap().entries.is_empty());
        }
        let mut fixture = Fixture::new();
        fixture.append(frame("known", "instructions"));
        assert!(fixture.refresh());
        File::options()
            .write(true)
            .open(&fixture.path)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(fixture.refresh());
        assert!(!fixture.index.classify(&mut text("known:0", "instructions")));
    }

    #[test]
    fn a_window_of_many_blocks_and_parts_decodes_each_record_once() {
        let mut fixture = Fixture::new();
        let mut bodies = vec!["instructions λ\n".repeat(32 * 1024)];
        bodies.extend((1..128).map(|index| format!("opaque instructions block {index}")));
        fixture.append(json!({"type":"user","uuid":"carrier","sessionId":SESSION,"isSynthetic":true,
            "message":{"id":"batch","content":bodies.iter().map(|body|json!({"type":"text","text":body})).collect::<Vec<_>>()}}));
        fixture.append(frame("next-record", "next instructions"));
        assert!(fixture.refresh());
        let first = &bodies[0];
        let part = TranscriptPart {
            item: "batch:0",
            index: 1,
            layout: "text",
            fields: [(16, 32), (0, 0)],
        }
        .id();
        let mut items = vec![
            Arc::new(text("batch:0", &first[..16])),
            Arc::new(text(&part, &first[16..32])),
        ];
        items.extend(
            bodies
                .iter()
                .enumerate()
                .skip(1)
                .map(|(index, body)| Arc::new(text(&format!("batch:{index}"), body))),
        );
        items.push(Arc::new(text("next-record:0", "next instructions")));
        let retained = items.clone();
        let mut expected = items.clone();
        for item in &mut expected {
            assert!(fixture.index.classify(Arc::make_mut(item)));
        }
        let source = fixture.index.source.as_ref().unwrap();
        source.reads.store(0, std::sync::atomic::Ordering::Relaxed);
        assert!(fixture.index.classify_items(&mut items));
        assert_eq!(
            fixture
                .index
                .source
                .as_ref()
                .unwrap()
                .reads
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "128 blocks and both displayed parts share one decode of their raw record"
        );
        assert_eq!(items, expected);
        assert!(retained.iter().all(|item| item.role == Role::User));
        assert!(
            matches!(&items[1].content, ItemContent::Delivery { body, .. } if body == &first[16..32])
        );
        assert!(items.iter().all(|item| item.role == Role::System));
    }

    #[test]
    fn immutable_projection_omits_scanner_state_and_keeps_exact_read_proofs() {
        let mut fixture = Fixture::new();
        fixture.append(frame("known", "instructions"));
        assert!(fixture.refresh());
        let mut projection = fixture.index.projection();
        let source = projection.source.as_ref().unwrap();
        assert!(source.carriers.is_empty() && source.message_offsets.is_empty());
        assert_eq!((source.offset, source.records, source.charged), (0, 0, 0));
        assert!(projection.classify(&mut text("known:0", "instructions")));
        fixture.append(frame("later", "more instructions"));
        assert!(fixture.refresh());
        assert!(!projection.classify(&mut text("later:0", "more instructions")));
        assert!(projection.classify(&mut text("known:0", "instructions")));
    }

    #[test]
    fn appending_during_a_bounded_scan_preserves_prefix_proofs_and_resumes_at_old_eof() {
        let mut fixture = Fixture::new();
        fixture.append(frame("prefix", "existing instructions"));
        let path = fixture.path.clone();
        assert!(
            fixture
                .index
                .refresh_with(&fixture.launch, &fixture.scope, SESSION, move || {
                    let mut file = File::options().append(true).open(&path).unwrap();
                    writeln!(file, "{}", frame("tail", "concurrent instructions")).unwrap();
                })
        );
        assert!(
            fixture
                .index
                .classify(&mut text("prefix:0", "existing instructions"))
        );
        assert!(
            !fixture
                .index
                .classify(&mut text("tail:0", "concurrent instructions"))
        );
        assert!(fixture.refresh());
        assert!(
            fixture
                .index
                .classify(&mut text("prefix:0", "existing instructions"))
        );
        assert!(
            fixture
                .index
                .classify(&mut text("tail:0", "concurrent instructions"))
        );
        assert!(!fixture.refresh());
    }
}
