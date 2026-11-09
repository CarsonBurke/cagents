//! What a provider transcript says about its conversation, read once and then
//! only as it grows.
//!
//! Transcripts are append-only, so a scan keeps its place and its findings:
//! a transcript that grew is read from where the last scan stopped, after
//! checking that the bytes before that point are still the ones it read. Only
//! records whose text could matter are parsed; the rest are skipped by a
//! substring test, which keeps a first read of a large history cheap.
use crate::agent::Provider;
use memchr::memmem;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// A record longer than this is skipped: it is tool output or an attachment,
/// never a name, a prompt worth showing, or a declaration.
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Bytes compared before a scan's offset to confirm the file only grew.
const BOUNDARY_BYTES: u64 = 256;
/// How much of a prompt or reply is kept for the preview.
const EXCERPT_CHARS: usize = 600;
/// How much of everything the user asked is kept for search.
const ASKED_CHARS: usize = 1_500;
const READ_CHUNK: usize = 1 << 20;
/// Transcripts larger than this are dropped from the page cache once read,
/// so indexing a large history does not push out what the desktop is using.
const UNCACHE_BYTES: u64 = 16 << 20;

/// What the picker shows of a conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    /// The directory the conversation ran in, as its provider recorded it.
    pub directory: PathBuf,
    pub branch: Option<String>,
    pub model: Option<String>,
    /// The name the reader or the provider gave it, if any.
    pub name: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    /// Messages the reader wrote, and replies the provider gave.
    pub prompts: u32,
    pub replies: u32,
    /// What the reader asked across the conversation, bounded, for search.
    pub asked: String,
}

impl Summary {
    /// The conversation's title: its name, or what it was first asked. A
    /// prompt too short to read as a task, such as "Restore me", still names
    /// the conversation better than its provider does.
    pub fn title(&self) -> Option<String> {
        self.name.clone().or_else(|| {
            let prompt = self.first_prompt.as_deref()?;
            crate::workspace_model::channel_title_from_prompt(prompt).or_else(|| {
                prompt
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .and_then(crate::workspace_model::channel_title)
            })
        })
    }

    pub fn messages(&self) -> u32 {
        self.prompts + self.replies
    }
}

/// A scan's place in a transcript and what it has found so far.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scan {
    offset: u64,
    /// A hash of the bytes just before `offset`.
    boundary: u64,
    /// `None` until a record declares the conversation, then whether it is
    /// one that can be resumed on its own.
    identity: Option<Option<(String, PathBuf)>>,
    custom: Option<String>,
    generated: Option<String>,
    summary_title: Option<String>,
    /// Directories a Claude conversation's records ran in, the first few,
    /// and whether each came from a record of its own rather than one a
    /// fork copied from the conversation it was forked from.
    directories: Vec<(PathBuf, bool)>,
    found: Summary,
}

impl Scan {
    /// The conversation, once the transcript declared one that resumes.
    pub fn summary(&self) -> Option<&Summary> {
        matches!(self.identity, Some(Some(_))).then_some(&self.found)
    }

    /// Whether the transcript declared something that is not a resumable
    /// conversation, such as a subagent's, so nothing it adds can change that.
    fn settled_negative(&self) -> bool {
        matches!(self.identity, Some(None))
    }
}

/// Brings `previous` up to date with `path`, reading only what was appended
/// since when the file only grew. `None` means the scan was cancelled or the
/// file could not be read; an incomplete scan is never kept.
pub fn scan(
    provider: Provider,
    path: &Path,
    previous: Option<&Scan>,
    cancel: &AtomicBool,
) -> Option<Scan> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut scan = previous
        .filter(|previous| previous.settled_negative() || continues(&mut file, len, previous))
        .cloned()
        .unwrap_or_default();
    if scan.settled_negative() {
        return Some(scan);
    }
    file.seek(SeekFrom::Start(scan.offset)).ok()?;
    let mut reader = Records::new(provider, path, &mut scan);
    let mut buffer = vec![0; READ_CHUNK];
    let mut pending = Vec::new();
    let mut oversized = false;
    let mut position = reader.scan.offset;
    let mut remaining = len.saturating_sub(position);
    while remaining > 0 {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let want = buffer.len().min(remaining as usize);
        let read = file.read(&mut buffer[..want]).ok()?;
        if read == 0 {
            break;
        }
        remaining -= read as u64;
        let mut start = 0;
        for newline in memchr::memchr_iter(b'\n', &buffer[..read]) {
            let piece = &buffer[start..newline];
            if !oversized && pending.len() + piece.len() <= MAX_LINE_BYTES {
                if pending.is_empty() {
                    reader.record(piece);
                } else {
                    pending.extend_from_slice(piece);
                    reader.record(&pending);
                }
            }
            pending.clear();
            oversized = false;
            position += (newline + 1 - start) as u64;
            reader.scan.offset = position;
            start = newline + 1;
            if reader.scan.settled_negative() {
                return Some(scan);
            }
        }
        let rest = &buffer[start..read];
        position += rest.len() as u64;
        if !oversized {
            if pending.len() + rest.len() <= MAX_LINE_BYTES {
                pending.extend_from_slice(rest);
            } else {
                pending = Vec::new();
                oversized = true;
            }
        }
    }
    reader.finish();
    // An unfinished last record is read again, whole, next time.
    scan.boundary = boundary(&mut file, scan.offset).unwrap_or(0);
    if len >= UNCACHE_BYTES {
        uncache(&file);
    }
    Some(scan)
}

/// Whether `file` still holds what `previous` read, and possibly more.
fn continues(file: &mut std::fs::File, len: u64, previous: &Scan) -> bool {
    previous.offset <= len
        && boundary(file, previous.offset).is_some_and(|hash| hash == previous.boundary)
}

fn boundary(file: &mut std::fs::File, offset: u64) -> Option<u64> {
    let start = offset.saturating_sub(BOUNDARY_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes).ok()?;
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    Some(bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    }))
}

fn uncache(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    // SAFETY: advisory only, on a descriptor this function borrows.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

/// Reads records into a scan.
struct Records<'a> {
    provider: Provider,
    /// What Claude names the transcript by: the conversation's ID, in the
    /// folder named for the directory it resumes in.
    claude: Option<(Option<String>, String)>,
    scan: &'a mut Scan,
    /// The newest reply record, parsed only once the scan ends.
    reply: Option<Vec<u8>>,
    finder: Finders,
}

struct Finders {
    session_id: memmem::Finder<'static>,
    user: memmem::Finder<'static>,
    assistant: memmem::Finder<'static>,
    text: memmem::Finder<'static>,
    tool_result: memmem::Finder<'static>,
    titles: [memmem::Finder<'static>; 6],
    codex_meta: memmem::Finder<'static>,
    codex_context: memmem::Finder<'static>,
    codex_user: memmem::Finder<'static>,
    codex_old_user: memmem::Finder<'static>,
    codex_agent: memmem::Finder<'static>,
    codex_old_agent: memmem::Finder<'static>,
    codex_complete: memmem::Finder<'static>,
    omp_session: memmem::Finder<'static>,
    omp_user: memmem::Finder<'static>,
    omp_assistant: memmem::Finder<'static>,
}

impl Finders {
    fn new() -> Self {
        Self {
            session_id: memmem::Finder::new(b"\"sessionId\""),
            user: memmem::Finder::new(b"\"type\":\"user\""),
            assistant: memmem::Finder::new(b"\"type\":\"assistant\""),
            text: memmem::Finder::new(b"\"type\":\"text\""),
            tool_result: memmem::Finder::new(b"\"tool_result\""),
            titles: [
                memmem::Finder::new(b"\"custom-title\""),
                memmem::Finder::new(b"\"ai-title\""),
                memmem::Finder::new(b"\"type\":\"title\""),
                memmem::Finder::new(b"\"session_info\""),
                memmem::Finder::new(b"\"type\":\"summary\""),
                memmem::Finder::new(b"\"thread_name_updated\""),
            ],
            codex_meta: memmem::Finder::new(b"\"session_meta\""),
            codex_context: memmem::Finder::new(b"\"turn_context\""),
            codex_user: memmem::Finder::new(b"\"UserMessage\""),
            codex_old_user: memmem::Finder::new(b"\"user_message\""),
            codex_agent: memmem::Finder::new(b"\"AgentMessage\""),
            codex_old_agent: memmem::Finder::new(b"\"agent_message\""),
            codex_complete: memmem::Finder::new(b"\"task_complete\""),
            omp_session: memmem::Finder::new(b"\"type\":\"session\""),
            omp_user: memmem::Finder::new(b"\"role\":\"user\""),
            omp_assistant: memmem::Finder::new(b"\"role\":\"assistant\""),
        }
    }
}

impl<'a> Records<'a> {
    fn new(provider: Provider, path: &Path, scan: &'a mut Scan) -> Self {
        let name =
            |path: Option<&std::ffi::OsStr>| path.and_then(|name| name.to_str()).map(str::to_owned);
        let claude = (provider == Provider::Claude).then(|| {
            (
                name(path.file_stem()).filter(|id| valid_id(id)),
                name(path.parent().and_then(Path::file_name)).unwrap_or_default(),
            )
        });
        Self {
            provider,
            claude,
            scan,
            reply: None,
            finder: Finders::new(),
        }
    }

    fn record(&mut self, line: &[u8]) {
        let has = |finder: &memmem::Finder<'_>| finder.find(line).is_some();
        let declared = self.scan.identity.is_some();
        let titled = self.finder.titles.iter().any(|finder| has(finder));
        match self.provider {
            Provider::Claude => {
                let user = has(&self.finder.user) && !has(&self.finder.tool_result);
                let reply = has(&self.finder.assistant) && has(&self.finder.text);
                if reply && declared {
                    self.scan.found.replies += 1;
                    let kept = self.reply.get_or_insert_default();
                    kept.clear();
                    kept.extend_from_slice(line);
                }
                if !(user || titled || !declared && has(&self.finder.session_id)) {
                    return;
                }
                let Ok(value) = serde_json::from_slice::<Value>(line) else {
                    return;
                };
                self.declare_claude(&value);
                if titled {
                    self.claude_title(&value);
                }
                if user {
                    self.claude_prompt(&value);
                }
            }
            Provider::Codex => {
                let meta = !declared && has(&self.finder.codex_meta);
                let context = has(&self.finder.codex_context);
                let prompt = has(&self.finder.codex_user) || has(&self.finder.codex_old_user);
                let agent = has(&self.finder.codex_agent) || has(&self.finder.codex_old_agent);
                let complete = has(&self.finder.codex_complete);
                if !(meta || context || prompt || agent || complete || titled) {
                    return;
                }
                let Ok(value) = serde_json::from_slice::<Value>(line) else {
                    return;
                };
                if meta && value["type"] == "session_meta" {
                    let header = &value["payload"];
                    // A subagent's rollout repeats its parent's history.
                    let subagent = header["source"].is_object();
                    self.scan.identity =
                        Some((!subagent).then(|| identity(header, "id")).flatten());
                    self.scan.found.branch = text(&header["git"]["branch"]);
                    return;
                }
                if !declared {
                    return;
                }
                self.codex(&value);
                if titled {
                    self.titles(&value);
                }
            }
            Provider::Omp => {
                let session = !declared && has(&self.finder.omp_session);
                let prompt = has(&self.finder.omp_user);
                let reply = has(&self.finder.omp_assistant);
                if !(session || prompt || reply || titled) {
                    return;
                }
                let Ok(value) = serde_json::from_slice::<Value>(line) else {
                    return;
                };
                if session && value["type"] == "session" {
                    self.scan.identity = Some(identity(&value, "id"));
                    return;
                }
                if !declared || value["type"] != "message" {
                    if declared && titled {
                        self.titles(&value);
                    }
                    return;
                }
                let message = &value["message"];
                match message["role"].as_str() {
                    Some("user") => {
                        if let Some(prompt) = content_text(&message["content"])
                            .and_then(|text| clean_prompt(Provider::Omp, &text))
                        {
                            self.prompt(prompt);
                        }
                    }
                    Some("assistant") => {
                        self.scan.found.replies += 1;
                        let kept = self.reply.get_or_insert_default();
                        kept.clear();
                        kept.extend_from_slice(line);
                        if let Some(model) = text(&message["model"]) {
                            self.scan.found.model = Some(model);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Claude resumes a conversation by the ID its file is named for, from
    /// the directory its folder is named for. A forked conversation's file
    /// opens with records copied from the one it was forked from, under that
    /// one's ID and directory, so neither is taken from its first record
    /// alone.
    fn declare_claude(&mut self, value: &Value) {
        // A title names the conversation; the directory it gives is where
        // the title was set, not one the conversation's own messages ran in.
        if value["isSidechain"] == true || title_record(value) {
            return;
        }
        let (Some(session), Some(cwd)) = (value["sessionId"].as_str(), value["cwd"].as_str())
        else {
            return;
        };
        let cwd = PathBuf::from(cwd);
        if !cwd.is_absolute() {
            return;
        }
        let file_id = self.claude.as_ref().and_then(|(id, _)| id.clone());
        let own = file_id.as_deref().is_none_or(|id| id == session);
        let directories = &mut self.scan.directories;
        if let Some(seen) = directories.iter_mut().find(|(seen, _)| *seen == cwd) {
            seen.1 |= own;
        } else if directories.len() < 8 {
            directories.push((cwd.clone(), own));
        }
        if self.scan.identity.is_none() {
            let id = file_id.unwrap_or_else(|| session.to_owned());
            self.scan.identity = Some(valid_id(&id).then_some((id, cwd)));
        }
    }

    /// Where a Claude conversation resumes: the directory its folder is
    /// named for, else the first one its own records ran in.
    fn claude_directory(&self) -> Option<PathBuf> {
        let folder = self.claude.as_ref().map(|(_, folder)| folder.as_str())?;
        let directories = &self.scan.directories;
        directories
            .iter()
            .find(|(directory, _)| names_folder(directory, folder))
            .or_else(|| directories.iter().find(|(_, own)| *own))
            .or_else(|| directories.first())
            .map(|(directory, _)| directory.clone())
    }

    /// A title record counts only for this conversation: not for another
    /// session's or directory's records, nor a subagent's.
    fn claude_title(&mut self, value: &Value) {
        let Some(Some((id, directory))) = &self.scan.identity else {
            return;
        };
        let directories = &self.scan.directories;
        if value["isSidechain"] == true
            || value
                .get("sessionId")
                .is_some_and(|session| session.as_str() != Some(id))
            || value.get("cwd").is_some_and(|cwd| {
                cwd.as_str().is_none_or(|cwd| {
                    Path::new(cwd) != directory
                        && !directories.iter().any(|(seen, _)| seen == Path::new(cwd))
                })
            })
        {
            return;
        }
        self.titles(value);
    }

    fn titles(&mut self, value: &Value) {
        let label = |key: &str| {
            value[key]
                .as_str()
                .and_then(crate::workspace_model::channel_title)
        };
        let scan = &mut *self.scan;
        match value["type"].as_str() {
            Some("custom-title") => scan.custom = label("customTitle").or(scan.custom.take()),
            Some("ai-title") => scan.generated = label("aiTitle").or(scan.generated.take()),
            Some("title") => scan.custom = label("title").or(scan.custom.take()),
            Some("session_info") => scan.custom = label("name").or(scan.custom.take()),
            Some("summary") => {
                scan.summary_title = label("summary").or(scan.summary_title.take());
            }
            _ => {}
        }
    }

    fn claude_prompt(&mut self, value: &Value) {
        if value["type"] != "user"
            || value["isMeta"] == true
            || value["isSidechain"] == true
            || value["isCompactSummary"] == true
            || self.scan.identity.as_ref().is_none_or(Option::is_none)
        {
            return;
        }
        if let Some(branch) = text(&value["gitBranch"]) {
            self.scan.found.branch = Some(branch);
        }
        if let Some(prompt) = content_text(&value["message"]["content"])
            .and_then(|text| clean_prompt(Provider::Claude, &text))
        {
            self.prompt(prompt);
        }
    }

    fn codex(&mut self, value: &Value) {
        let payload = &value["payload"];
        match (value["type"].as_str(), payload["type"].as_str()) {
            (Some("turn_context"), _) => {
                if let Some(model) = text(&payload["model"]) {
                    self.scan.found.model = Some(model);
                }
            }
            (Some("event_msg"), Some("item_completed")) => {
                let item = &payload["item"];
                match item["type"].as_str() {
                    Some("UserMessage") => {
                        if let Some(prompt) = content_text(&item["content"])
                            .and_then(|text| clean_prompt(Provider::Codex, &text))
                        {
                            self.prompt(prompt);
                        }
                    }
                    Some("AgentMessage") => {
                        self.scan.found.replies += 1;
                        if let Some(reply) = content_text(&item["content"]) {
                            self.scan.found.last_reply = Some(excerpt(&reply));
                        }
                    }
                    _ => {}
                }
            }
            // Earlier Codex versions report messages as events of their own.
            (Some("event_msg"), Some("user_message")) => {
                if let Some(prompt) = payload["message"]
                    .as_str()
                    .and_then(|text| clean_prompt(Provider::Codex, text))
                {
                    self.prompt(prompt);
                }
            }
            (Some("event_msg"), Some("agent_message")) => {
                self.scan.found.replies += 1;
                if let Some(reply) = text(&payload["message"]) {
                    self.scan.found.last_reply = Some(excerpt(&reply));
                }
            }
            (Some("event_msg"), Some("task_complete")) => {
                if let Some(reply) = text(&payload["last_agent_message"]) {
                    self.scan.found.last_reply = Some(excerpt(&reply));
                }
            }
            _ => {}
        }
    }

    fn prompt(&mut self, prompt: String) {
        let found = &mut self.scan.found;
        found.prompts += 1;
        let excerpt = excerpt(&prompt);
        if found.first_prompt.is_none() {
            found.first_prompt = Some(excerpt.clone());
        }
        let room = ASKED_CHARS.saturating_sub(found.asked.chars().count());
        if room > 0 {
            if !found.asked.is_empty() {
                found.asked.push('\n');
            }
            found.asked.extend(prompt.chars().take(room.min(240)));
        }
        found.last_prompt = Some(excerpt);
    }

    fn finish(self) {
        if let Some(directory) = self.claude_directory()
            && let Some(Some((_, declared))) = &mut self.scan.identity
        {
            *declared = directory;
        }
        if let Some(line) = self.reply
            && let Ok(value) = serde_json::from_slice::<Value>(&line)
        {
            let message = &value["message"];
            if let Some(reply) = content_text(&message["content"]) {
                self.scan.found.last_reply = Some(excerpt(&reply));
            }
            if let Some(model) = text(&message["model"]) {
                self.scan.found.model = Some(model);
            }
        }
        let scan = &mut *self.scan;
        if let Some(Some((id, directory))) = &scan.identity {
            scan.found.id.clone_from(id);
            scan.found.directory.clone_from(directory);
        }
        scan.found.name = scan
            .custom
            .clone()
            .or_else(|| scan.generated.clone())
            .or_else(|| scan.summary_title.clone());
    }
}

fn title_record(value: &Value) -> bool {
    matches!(
        value["type"].as_str(),
        Some("custom-title" | "ai-title" | "title" | "session_info" | "summary")
    )
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn valid_id(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()
}

/// Whether Claude keeps the conversations of `directory` in `folder`: its
/// path with everything but ASCII letters and digits replaced, and a long
/// one cut short and told apart by a suffix.
fn names_folder(directory: &Path, folder: &str) -> bool {
    let Some(directory) = directory.to_str() else {
        return false;
    };
    let named: String = directory
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if named.len() <= 200 {
        return named == folder;
    }
    folder
        .strip_prefix(&named[..200])
        .is_some_and(|rest| rest.starts_with('-'))
}

fn identity(header: &Value, key: &str) -> Option<(String, PathBuf)> {
    let id = header[key].as_str()?;
    if !valid_id(id) {
        return None;
    }
    let directory = PathBuf::from(header["cwd"].as_str()?);
    directory.is_absolute().then(|| (id.to_owned(), directory))
}

/// The text parts of a message's content, which is either text or a list of
/// typed parts. Parts that are only provider or tool context are left out.
fn content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let mut joined = String::new();
    for part in content.as_array()?.iter().take(32) {
        if !matches!(
            part["type"].as_str(),
            Some("text" | "Text" | "input_text" | "output_text")
        ) {
            continue;
        }
        let Some(text) = part["text"].as_str() else {
            continue;
        };
        if internal(text) {
            continue;
        }
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str(&text[..text.floor_char_boundary(text.len().min(32 * 1024))]);
    }
    (!joined.trim().is_empty()).then_some(joined)
}

/// Text a provider or a harness put in the user's turn that the user did not
/// write: reminders, injected instructions, hook and command output.
fn internal(text: &str) -> bool {
    let text = text.trim_start();
    const PREFIXES: [&str; 14] = [
        "<system-reminder>",
        "<task-notification>",
        "<local-command-",
        "<bash-",
        "<user-prompt-submit-hook>",
        "<environment_context>",
        "<user_instructions>",
        "<INSTRUCTIONS>",
        "# AGENTS.md instructions",
        "Caveat: The messages below were generated by the user while running local commands",
        "Base directory for this skill:",
        "[Request interrupted by user",
        "This session is being continued from a previous conversation",
        "<turn_aborted>",
    ];
    PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

/// What the user wrote, without the context wrapped around it, or `None`
/// for a message that is only context.
pub fn clean_prompt(provider: Provider, text: &str) -> Option<String> {
    let mut text = text.to_owned();
    // Reminders may be wrapped around or inside what the user wrote.
    while let Some(start) = text.find("<system-reminder>") {
        let end = text[start..]
            .find("</system-reminder>")
            .map_or(text.len(), |end| start + end + "</system-reminder>".len());
        text.replace_range(start..end, "");
    }
    let trimmed = text.trim();
    if trimmed.is_empty() || internal(trimmed) {
        return None;
    }
    if provider == Provider::Claude && trimmed.contains("<command-name>") {
        // A command with what it was asked to do, or nothing for one that
        // asks nothing, such as `/clear`.
        let element = |name: &str| {
            let open = format!("<{name}>");
            let start = trimmed.find(&open)? + open.len();
            let end = start + trimmed[start..].find(&format!("</{name}>"))?;
            Some(trimmed[start..end].trim())
        };
        let command = element("command-name").unwrap_or_default();
        let arguments = element("command-args").unwrap_or_default();
        return (!arguments.is_empty()).then(|| format!("{command} {arguments}"));
    }
    if trimmed.starts_with("<command-") {
        return None;
    }
    Some(trimmed.to_owned())
}

/// A bounded, single-paragraph-friendly excerpt.
fn excerpt(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(EXCERPT_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const ID: &str = "f0e49307-af96-4edd-b23f-60b185fe4fea";

    fn write(path: &Path, records: &[Value]) {
        let mut file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    }

    fn read(provider: Provider, path: &Path, previous: Option<&Scan>) -> Scan {
        scan(provider, path, previous, &AtomicBool::new(false)).unwrap()
    }

    fn claude_user(text: Value) -> Value {
        json!({"type":"user","sessionId":ID,"cwd":"/work/api","gitBranch":"fix-auth","message":{"role":"user","content":text}})
    }

    fn claude_reply(text: &str) -> Value {
        json!({"type":"assistant","sessionId":ID,"cwd":"/work/api","message":{"model":"claude-sonnet-4-5","role":"assistant","content":[{"type":"text","text":text}]}})
    }

    /// The preview names what the user asked and what came back, without the
    /// reminders, skill text, command output and tool results around it.
    #[test]
    fn a_claude_transcript_summarizes_what_the_user_wrote() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("t.jsonl");
        write(
            &path,
            &[
                json!({"type":"queue-operation","sessionId":ID}),
                claude_user(json!(
                    "<system-reminder>\nThe user opened a file.\n</system-reminder>"
                )),
                claude_user(json!(
                    "Caveat: The messages below were generated by the user while running local commands."
                )),
                claude_user(json!(
                    "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"
                )),
                claude_user(
                    json!([{"type":"text","text":"<system-reminder>ctx</system-reminder>"},{"type":"text","text":"Fix the token refresh race"}]),
                ),
                json!({"type":"user","sessionId":ID,"cwd":"/work/api","message":{"content":[{"type":"tool_result","content":"log"}]}}),
                claude_reply("I traced it to the retry path."),
                json!({"type":"user","isSidechain":true,"sessionId":ID,"cwd":"/work/api","message":{"content":"Subagent work"}}),
                claude_user(json!("Base directory for this skill: /skills/x\n\n# Skill")),
                claude_user(json!("Add a regression test too")),
                claude_reply("Done; the test fails before the fix."),
            ],
        );
        let scan = read(Provider::Claude, &path, None);
        let summary = scan.summary().unwrap();
        assert_eq!(summary.id, ID);
        assert_eq!(summary.directory, Path::new("/work/api"));
        assert_eq!(summary.branch.as_deref(), Some("fix-auth"));
        assert_eq!(summary.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(
            summary.first_prompt.as_deref(),
            Some("Fix the token refresh race")
        );
        assert_eq!(
            summary.last_prompt.as_deref(),
            Some("Add a regression test too")
        );
        assert_eq!(
            summary.last_reply.as_deref(),
            Some("Done; the test fails before the fix.")
        );
        assert_eq!((summary.prompts, summary.replies), (2, 2));
        assert_eq!(
            summary.title().as_deref(),
            Some("Fix the token refresh race")
        );
    }

    #[test]
    fn an_oh_my_pi_session_summarizes_its_messages() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("t.jsonl");
        write(
            &path,
            &[
                json!({"type":"session","id":ID,"cwd":"/work/api"}),
                json!({"type":"message","message":{"role":"user","content":"Restore me"}}),
                json!({"type":"message","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"Restored"}]}}),
            ],
        );
        let scan = read(Provider::Omp, &path, None);
        let summary = scan.summary().unwrap();
        assert_eq!(summary.directory, Path::new("/work/api"));
        assert_eq!((summary.prompts, summary.replies), (1, 1));
        assert_eq!(summary.first_prompt.as_deref(), Some("Restore me"));
        assert_eq!(summary.last_reply.as_deref(), Some("Restored"));
        assert_eq!(summary.title().as_deref(), Some("Restore me"));
    }

    /// A grown transcript is read from where the last scan stopped, and a
    /// rewritten one from its start.
    #[test]
    fn a_grown_transcript_is_read_from_where_its_scan_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("t.jsonl");
        write(
            &path,
            &[
                claude_user(json!("First question")),
                claude_reply("First answer"),
            ],
        );
        let first = read(Provider::Claude, &path, None);
        let offset = first.offset;
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len());
        // An unfinished record is not counted until it is whole.
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"type":"user","sessionId":"#)
            .unwrap();
        let partial = read(Provider::Claude, &path, Some(&first));
        assert_eq!(partial.offset, offset);
        assert_eq!(partial.summary().unwrap().prompts, 1);
        std::fs::write(
            &path,
            std::fs::read_to_string(&path).unwrap()[..offset as usize].to_owned()
                + &format!(
                    "{}\n{}\n",
                    claude_user(json!("Second question")),
                    json!({"type":"custom-title","sessionId":ID,"customTitle":"Auth race"})
                ),
        )
        .unwrap();
        let grown = read(Provider::Claude, &path, Some(&partial));
        let summary = grown.summary().unwrap();
        assert_eq!(summary.prompts, 2);
        assert_eq!(summary.first_prompt.as_deref(), Some("First question"));
        assert_eq!(summary.last_prompt.as_deref(), Some("Second question"));
        assert_eq!(summary.last_reply.as_deref(), Some("First answer"));
        assert_eq!(summary.title().as_deref(), Some("Auth race"));
        // A rewrite changes the bytes the scan stopped after.
        std::fs::write(
            &path,
            format!("{}\n", claude_user(json!("Replaced history"))),
        )
        .unwrap();
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&vec![b' '; grown.offset as usize])
            .unwrap();
        let rewritten = read(Provider::Claude, &path, Some(&grown));
        let summary = rewritten.summary().unwrap();
        assert_eq!(summary.prompts, 1);
        assert_eq!(summary.first_prompt.as_deref(), Some("Replaced history"));
        assert_eq!(summary.name, None);
    }

    /// Current Codex rollouts carry the user's words only as completed items;
    /// the injected AGENTS.md and environment context are not prompts.
    #[test]
    fn a_codex_rollout_summarizes_its_user_messages_and_replies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let user = |text: &str| json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}});
        let item = |kind: &str, text: &str| json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":kind,"content":[{"type":"text","text":text}]}}});
        write(
            &path,
            &[
                json!({"type":"session_meta","payload":{"id":ID,"cwd":"/work/api","source":"cli","git":{"branch":"main"}}}),
                json!({"type":"turn_context","payload":{"model":"gpt-5.2-codex"}}),
                user("# AGENTS.md instructions\n\n<INSTRUCTIONS>\nx\n</INSTRUCTIONS>"),
                user("<environment_context>\n<cwd>/work/api</cwd>\n</environment_context>"),
                user("Profile the slow endpoint"),
                item("UserMessage", "Profile the slow endpoint"),
                item("AgentMessage", "Looking at the query plan."),
                json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Batching the query fixes it."}}),
            ],
        );
        let scan = read(Provider::Codex, &path, None);
        let summary = scan.summary().unwrap();
        assert_eq!(
            summary.first_prompt.as_deref(),
            Some("Profile the slow endpoint")
        );
        assert_eq!(summary.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(summary.branch.as_deref(), Some("main"));
        assert_eq!(
            summary.last_reply.as_deref(),
            Some("Batching the query fixes it.")
        );
        assert_eq!((summary.prompts, summary.replies), (1, 1));
        // A subagent's rollout is decided by its first record.
        let child = directory.path().join("child.jsonl");
        write(
            &child,
            &[
                json!({"type":"session_meta","payload":{"id":ID,"cwd":"/work/api","source":{"subagent":{}}}}),
            ],
        );
        let scan = read(Provider::Codex, &child, None);
        assert!(scan.summary().is_none());
        assert!(scan.settled_negative());
    }

    #[test]
    fn names_rank_custom_then_generated_then_summary_and_ignore_other_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("t.jsonl");
        write(
            &path,
            &[
                claude_user(json!("Fallback prompt")),
                json!({"type":"summary","summary":"A summary"}),
                json!({"type":"ai-title","sessionId":ID,"aiTitle":"Generated"}),
                json!({"type":"custom-title","sessionId":"00000000-0000-4000-8000-000000000002","customTitle":"Other session"}),
                json!({"type":"custom-title","sessionId":ID,"cwd":"/elsewhere","customTitle":"Other directory"}),
                json!({"type":"custom-title","sessionId":ID,"isSidechain":true,"customTitle":"Sidechain"}),
            ],
        );
        assert_eq!(
            read(Provider::Claude, &path, None)
                .summary()
                .unwrap()
                .name
                .as_deref(),
            Some("Generated")
        );
        write(
            &path,
            &[
                json!({"type":"custom-title","sessionId":ID,"customTitle":"Mine"}),
                json!({"type":"ai-title","sessionId":ID,"aiTitle":"Later generated"}),
            ],
        );
        assert_eq!(
            read(Provider::Claude, &path, None)
                .summary()
                .unwrap()
                .name
                .as_deref(),
            Some("Mine")
        );
    }

    /// A forked conversation opens with its origin's records, under the
    /// origin's ID and directory; it resumes by its own, which its file and
    /// folder are named for.
    #[test]
    fn a_forked_claude_transcript_resumes_as_itself() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("-work-api");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join(format!("{ID}.jsonl"));
        let origin = "00000000-0000-4000-8000-000000000002";
        write(
            &path,
            &[
                json!({"type":"user","sessionId":origin,"cwd":"/scratch/one","message":{"content":"Origin question"}}),
                json!({"type":"custom-title","sessionId":origin,"customTitle":"Origin name"}),
                json!({"type":"user","sessionId":origin,"cwd":"/work/api","message":{"content":"Moved over"}}),
                json!({"type":"custom-title","sessionId":ID,"customTitle":"Forked name"}),
                json!({"type":"user","sessionId":ID,"cwd":"/work/api","message":{"content":"After the fork"}}),
            ],
        );
        let scan = read(Provider::Claude, &path, None);
        let summary = scan.summary().unwrap();
        assert_eq!(summary.id, ID);
        assert_eq!(summary.directory, Path::new("/work/api"));
        assert_eq!(summary.name.as_deref(), Some("Forked name"));
        assert_eq!(summary.prompts, 3);
        assert!(names_folder(Path::new("/work/api"), "-work-api"));
        let long = format!("/{}", "a".repeat(240));
        let named = format!("-{}", "a".repeat(199));
        assert!(names_folder(Path::new(&long), &format!("{named}-1x2y")));
        assert!(!names_folder(Path::new(&long), &named));
    }

    #[test]
    fn cancellation_keeps_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("t.jsonl");
        write(&path, &[claude_user(json!("Question"))]);
        assert!(scan(Provider::Claude, &path, None, &AtomicBool::new(true)).is_none());
    }

    #[test]
    fn oversized_records_are_skipped_without_losing_later_ones() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("t.jsonl");
        write(
            &path,
            &[
                claude_user(json!("Question")),
                claude_reply(&"x".repeat(3 * MAX_LINE_BYTES)),
                claude_reply("Short answer"),
            ],
        );
        let scan = read(Provider::Claude, &path, None);
        let summary = scan.summary().unwrap();
        assert_eq!(summary.replies, 1);
        assert_eq!(summary.last_reply.as_deref(), Some("Short answer"));
    }

    #[test]
    fn commands_keep_only_what_they_ask() {
        let command = |name: &str, args: &str| {
            format!(
                "<command-name>{name}</command-name>\n<command-message>{name}</command-message>\n<command-args>{args}</command-args>"
            )
        };
        assert_eq!(clean_prompt(Provider::Claude, &command("/clear", "")), None);
        assert_eq!(
            clean_prompt(Provider::Claude, &command("/solve-task", "fix resume")).as_deref(),
            Some("/solve-task fix resume")
        );
        assert_eq!(
            clean_prompt(
                Provider::Claude,
                "<local-command-stdout>Bye!</local-command-stdout>"
            ),
            None
        );
        assert_eq!(
            clean_prompt(
                Provider::Claude,
                "<task-notification>\n<task-id>b1</task-id>\n</task-notification>"
            ),
            None
        );
        assert_eq!(
            clean_prompt(Provider::Claude, "Review <this> diff").as_deref(),
            Some("Review <this> diff")
        );
    }
}
