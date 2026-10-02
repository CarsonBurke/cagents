//! Bounded, read-only discovery. No provider process or model request is needed.
//!
//! Discovery takes two passes so that a large history is listed at once. The
//! first only stats each provider's session store, which stays cheap at any
//! size and orders every transcript newest first. The second reads each
//! transcript's identity and title from as little of the file as answers
//! them, newest first, and remembers the answer until the file changes.
use super::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::sync::Mutex;

/// Directory entries walked per session store, which bounds a store that is
/// not one, such as a home directory configured by mistake.
const ENTRY_LIMIT: usize = 262_144;
/// How far into a transcript its identity and first message are looked for.
const HEAD_BYTES: u64 = 256 * 1024;
/// How much of a transcript's end is searched for a title set later.
const TAIL_BYTES: u64 = 64 * 1024;
/// Transcripts remembered across openings of the picker.
const CACHE_LIMIT: usize = 131_072;

/// A transcript found by its file alone, before anything in it is read.
pub struct Candidate {
    /// The index of the session whose account and launch it resumes with.
    pub seed: usize,
    pub path: PathBuf,
    modified_ms: i64,
    len: u64,
}

/// A saved conversation as listed, without the session it would resume as.
/// The session is built only for the one the reader chooses.
#[derive(Debug, Clone)]
pub struct Found {
    pub seed: usize,
    pub path: PathBuf,
    pub id: String,
    pub directory: PathBuf,
    pub title: String,
    /// Whether the transcript names itself, rather than going by its
    /// provider.
    pub titled: bool,
    pub updated_at_unix_ms: i64,
}

/// What a transcript says of itself.
#[derive(Debug, Clone)]
struct Header {
    id: String,
    directory: PathBuf,
    title: Option<String>,
}

/// Transcript headers by path, valid while the file keeps its size and
/// modification time.
#[derive(Default)]
pub struct Cache(Mutex<HashMap<PathBuf, (i64, u64, Option<Header>)>>);

/// Every transcript in the seeds' session stores, newest first.
pub fn candidates(seeds: &[chat::Session]) -> Vec<Candidate> {
    let mut roots = HashSet::new();
    let mut paths = HashSet::new();
    let mut candidates = Vec::new();
    for (index, seed) in seeds.iter().enumerate() {
        let provider = seed.launch.provider;
        let Some(root) = provider.session_root(&seed.launch.recipe) else {
            continue;
        };
        let (root, depth) = match provider {
            Provider::Claude => (root.join("projects"), 2),
            Provider::Codex => (root.join("sessions"), 4),
            Provider::Omp => (root.join("sessions"), 3),
        };
        let root = root.canonicalize().unwrap_or(root);
        if !roots.insert((provider, root.clone())) {
            continue;
        }
        let mut files = Vec::new();
        let mut budget = ENTRY_LIMIT;
        collect(&root, depth, &mut budget, &mut files);
        for (path, metadata) in files {
            if !paths.insert(path.clone()) {
                continue;
            }
            let Some(modified_ms) = modified_ms(&metadata) else {
                continue;
            };
            candidates.push(Candidate {
                seed: index,
                path,
                modified_ms,
                len: metadata.len(),
            });
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified_ms));
    candidates
}

fn collect(
    root: &Path,
    depth: usize,
    budget: &mut usize,
    files: &mut Vec<(PathBuf, std::fs::Metadata)>,
) {
    if depth == 0 || *budget == 0 {
        return;
    }
    let Ok(directory) = std::fs::read_dir(root) else {
        return;
    };
    // Bound directory entries too, including unrelated files and symlinks.
    for entry in directory.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && path.file_name().is_none_or(|name| name != "subagents") {
            collect(&path, depth - 1, budget, files);
        } else if kind.is_file()
            && path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(metadata) = entry.metadata()
        {
            files.push((path, metadata));
        }
    }
}

fn modified_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

impl Cache {
    /// The conversation `candidate` holds, read only when it changed since it
    /// was last read.
    pub fn find(&self, provider: Provider, candidate: &Candidate) -> Option<Found> {
        let cached = self
            .0
            .lock()
            .ok()?
            .get(&candidate.path)
            .filter(|(modified, len, _)| {
                *modified == candidate.modified_ms && *len == candidate.len
            })
            .map(|(_, _, header)| header.clone());
        let header = match cached {
            Some(header) => header,
            None => {
                let header = header(provider, &candidate.path);
                let mut cache = self.0.lock().ok()?;
                if cache.len() >= CACHE_LIMIT {
                    cache.clear();
                }
                cache.insert(
                    candidate.path.clone(),
                    (candidate.modified_ms, candidate.len, header.clone()),
                );
                header
            }
        }?;
        Some(Found {
            seed: candidate.seed,
            path: candidate.path.clone(),
            id: header.id,
            directory: header.directory,
            titled: header.title.is_some(),
            title: header
                .title
                .unwrap_or_else(|| format!("{} conversation", provider.label())),
            updated_at_unix_ms: candidate.modified_ms,
        })
    }
}

/// The conversations `candidates` hold, in their order, read by up to
/// `workers` threads.
pub fn find_all(
    cache: &Cache,
    seeds: &[chat::Session],
    candidates: &[Candidate],
    workers: usize,
) -> Vec<Found> {
    let find = |chunk: &[Candidate]| {
        chunk
            .iter()
            .filter_map(|candidate| cache.find(seeds[candidate.seed].launch.provider, candidate))
            .collect::<Vec<_>>()
    };
    let size = candidates.len().div_ceil(workers.max(1)).max(1);
    if candidates.len() <= size {
        return find(candidates);
    }
    std::thread::scope(|scope| {
        candidates
            .chunks(size)
            .map(|chunk| scope.spawn(move || find(chunk)))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|reader| reader.join().unwrap_or_default())
            .collect()
    })
}

/// The names Codex keeps for its conversations apart from their
/// transcripts, by conversation ID, the latest for each.
pub fn names(seeds: &[chat::Session]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let mut roots = HashSet::new();
    for seed in seeds {
        if seed.launch.provider != Provider::Codex {
            continue;
        }
        let Some(root) = Provider::Codex.session_root(&seed.launch.recipe) else {
            continue;
        };
        let path = root.join("session_index.jsonl");
        if !roots.insert(path.clone()) {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let (Some(id), Some(name)) = (
                value["id"].as_str(),
                value["thread_name"]
                    .as_str()
                    .and_then(crate::account::clean_label),
            ) {
                names.insert(id.to_owned(), name);
            }
        }
    }
    names
}

/// Reads a transcript's identity and title, or `None` for a file that is not
/// a resumable top-level conversation.
fn header(provider: Provider, path: &Path) -> Option<Header> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut identity = None;
    let mut prompt = None;
    let mut titles = Titles::default();
    let mut reader = std::io::BufReader::new((&mut file).take(HEAD_BYTES));
    let mut line = Vec::new();
    // Most records are irrelevant and some are large, so a record is parsed
    // only when its text could hold what is still missing.
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let wants_identity = identity.is_none() && text.contains(identity_hint(provider));
        let wants_prompt = prompt.is_none() && text.contains(prompt_hint(provider));
        let wants_title = holds_title(text);
        if !(wants_identity || wants_prompt || wants_title) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if wants_identity && let Some(declared) = declaration(provider, &value) {
            // The first record to declare a conversation decides what the
            // file holds; a subagent's is not resumed on its own.
            identity = Some(declared?);
        }
        if wants_prompt {
            prompt = prompt_of(provider, &value);
        }
        if wants_title {
            titles.note(&value);
        }
        if identity.is_some() && prompt.is_some() {
            break;
        }
    }
    let (id, directory) = identity?;
    let title = |titles: Titles| titles.named.or(titles.summary).or(prompt);
    // Codex keeps a conversation's name apart from it, in [`names`].
    if provider == Provider::Codex {
        return Some(Header {
            id,
            directory,
            title: title(titles),
        });
    }
    // A title set by renaming the conversation is appended to its end.
    if len > TAIL_BYTES {
        file.seek(SeekFrom::End(-(TAIL_BYTES as i64))).ok()?;
    } else {
        file.rewind().ok()?;
    }
    let mut tail = Vec::new();
    (&mut file).take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let mut lines = tail.split('\n');
    if len > TAIL_BYTES {
        // The first line of a tail is the end of a record cut in two.
        lines.next();
    }
    for line in lines.filter(|line| holds_title(line)) {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            titles.note(&value);
        }
    }
    Some(Header {
        id,
        directory,
        title: title(titles),
    })
}

/// The titles a transcript records, the latest of each kind.
#[derive(Default)]
struct Titles {
    /// Set by the reader or the provider as the conversation's name.
    named: Option<String>,
    /// A provider's generated account of the conversation.
    summary: Option<String>,
}

impl Titles {
    fn note(&mut self, value: &Value) {
        let label = |key: &str| value[key].as_str().and_then(crate::account::clean_label);
        match value["type"].as_str() {
            Some("custom-title") => self.named = label("customTitle").or(self.named.take()),
            Some("title") => self.named = label("title").or(self.named.take()),
            Some("session_info") => self.named = label("name").or(self.named.take()),
            Some("summary") => self.summary = label("summary").or(self.summary.take()),
            _ => {}
        }
    }
}

/// Whether a record's text could be one of the titles [`Titles`] reads.
fn holds_title(line: &str) -> bool {
    [
        "\"custom-title\"",
        "\"type\":\"title\"",
        "\"session_info\"",
        "\"type\":\"summary\"",
    ]
    .into_iter()
    .any(|needle| line.contains(needle))
}

/// Text every record declaring a provider's conversation contains.
fn identity_hint(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "\"sessionId\"",
        Provider::Codex => "\"session_meta\"",
        Provider::Omp => "\"type\":\"session\"",
    }
}

/// Text every record holding a provider's first user message contains.
fn prompt_hint(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "\"type\":\"user\"",
        Provider::Codex => "\"user_message\"",
        Provider::Omp => "\"role\":\"user\"",
    }
}

/// Whether a record declares a conversation, and if it does, the ID and
/// working directory it resumes with, or `None` for one that is not
/// resumable: a subagent's, or one without a valid identity.
fn declaration(provider: Provider, value: &Value) -> Option<Option<(String, PathBuf)>> {
    let header = match (provider, value["type"].as_str()) {
        (Provider::Codex, Some("session_meta")) => &value["payload"],
        (Provider::Omp, Some("session")) => value,
        // Claude records a subagent's messages beside its parent's in older
        // transcripts, so a sidechain record declares nothing either way.
        (Provider::Claude, _)
            if value["cwd"].is_string()
                && value["sessionId"].is_string()
                && value["isSidechain"] != true =>
        {
            value
        }
        _ => return None,
    };
    let subagent = provider == Provider::Codex && header["source"].is_object();
    Some((!subagent).then(|| identity(provider, header)).flatten())
}

fn identity(provider: Provider, header: &Value) -> Option<(String, PathBuf)> {
    let id = header[if provider == Provider::Claude {
        "sessionId"
    } else {
        "id"
    }]
    .as_str()?;
    if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
        return None;
    }
    let directory = PathBuf::from(header["cwd"].as_str()?);
    directory.is_absolute().then(|| (id.to_owned(), directory))
}

/// The text of the first message the user wrote, as a title.
fn prompt_of(provider: Provider, value: &Value) -> Option<String> {
    let message = match provider {
        Provider::Claude
            if value["type"] == "user"
                && value["isMeta"] != true
                && value["isSidechain"] != true =>
        {
            &value["message"]
        }
        Provider::Omp if value["type"] == "message" && value["message"]["role"] == "user" => {
            &value["message"]
        }
        Provider::Codex
            if value["type"] == "event_msg" && value["payload"]["type"] == "user_message" =>
        {
            return value["payload"]["message"]
                .as_str()
                .and_then(crate::account::clean_label);
        }
        _ => return None,
    };
    let content = &message["content"];
    let text = content.as_str().or_else(|| {
        content
            .as_array()?
            .iter()
            .find_map(|part| part["text"].as_str())
    })?;
    if provider == Provider::Claude {
        return claude_prompt(text);
    }
    crate::account::clean_label(text)
}

/// What a Claude user record asked, as a title: a command with what it was
/// asked to do, or the message itself. A bare command, its output, and a
/// shell escape say nothing of what the conversation is about.
fn claude_prompt(text: &str) -> Option<String> {
    let element = |name: &str| {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let end = start + text[start..].find(&format!("</{name}>"))?;
        Some(text[start..end].trim())
    };
    if let Some(command) = element("command-name") {
        let args = element("command-args").unwrap_or_default();
        return (!args.is_empty())
            .then(|| crate::account::clean_label(&format!("{command} {args}")))
            .flatten();
    }
    let trimmed = text.trim_start();
    if ["<local-command-", "<bash-", "<command-"]
        .iter()
        .any(|tag| trimmed.starts_with(tag))
    {
        return None;
    }
    crate::account::clean_label(text)
}

/// The session `found` resumes as: `seed`'s account and launch, with the
/// conversation's own identity and directory.
pub fn session(seed: &chat::Session, found: &Found) -> Option<chat::Session> {
    let provider = seed.launch.provider;
    let mut session = seed.clone();
    session.reset_owned_codex_listener();
    session.id = TerminalId::new();
    session.proof = None;
    session.terminal = None;
    session.launch.recipe.working_directory = found.directory.clone();
    session.launch.session_id = Some(found.id.clone());
    if provider == Provider::Omp {
        let args = &mut session.launch.recipe.arguments;
        let mut index = 0;
        while index < args.len() {
            if args[index] == "--session-dir" {
                args.drain(index..(index + 2).min(args.len()));
            } else if args[index].starts_with("--session-dir=") {
                args.remove(index);
            } else {
                index += 1;
            }
        }
        args.extend([
            "--session-dir".into(),
            found.path.parent()?.to_string_lossy().into_owned(),
        ]);
    }
    // Rebuild resume arguments from the exact discovered identity rather than
    // carry another saved conversation's resume selector into its launch.
    session.launch.recipe = session.launch.terminal_recipe()?;
    Some(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn discover(seeds: &[chat::Session]) -> Vec<(Found, chat::Session)> {
        let cache = Cache::default();
        candidates(seeds)
            .iter()
            .filter_map(|candidate| {
                let seed = &seeds[candidate.seed];
                let found = cache.find(seed.launch.provider, candidate)?;
                let session = session(seed, &found)?;
                Some((found, session))
            })
            .collect()
    }

    #[test]
    fn discovers_all_provider_history_without_changing_account_or_identity() {
        let root = tempfile::tempdir().unwrap();
        let id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        for provider in Provider::ALL {
            let account_home = root.path().join(provider.executable());
            std::fs::create_dir_all(&account_home).unwrap();
            let account = crate::account::Account {
                provider,
                home: crate::account::AccountHome::Linked {
                    key: uuid::Uuid::new_v4(),
                    path: account_home.clone(),
                },
            };
            let mut seed =
                chat::fresh_session(provider, TerminalId::new(), root.path().into(), &account)
                    .unwrap();
            if provider == Provider::Omp {
                seed.launch.recipe.environment.insert(
                    "PI_CODING_AGENT_DIR".into(),
                    account_home.to_string_lossy().into_owned(),
                );
            }
            let base = provider.session_root(&seed.launch.recipe).unwrap();
            let directory = base.join(if provider == Provider::Claude {
                "projects/project"
            } else {
                "sessions/native"
            });
            std::fs::create_dir_all(&directory).unwrap();
            let record = match provider {
                Provider::Claude => {
                    json!({"type":"user","sessionId":id,"cwd":root.path(),"message":{"content":"Restore me"}})
                }
                Provider::Codex => {
                    json!({"type":"session_meta","payload":{"id":id,"cwd":root.path(),"source":"cli"}})
                }
                Provider::Omp => json!({"type":"session","id":id,"cwd":root.path()}),
            };
            std::fs::write(directory.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
            let entries = discover(&[seed.clone()]);
            assert_eq!(entries.len(), 1, "{provider:?}");
            let (found, session) = &entries[0];
            assert_eq!(found.id, id);
            assert_eq!(session.launch.session_id.as_deref(), Some(id));
            assert_eq!(session.launch.recipe.working_directory, root.path());
            assert_eq!(provider.session_root(&session.launch.recipe), Some(base));
            assert!(
                !session
                    .launch
                    .recipe
                    .arguments
                    .contains(&"bypassPermissions".into())
            );
        }
    }

    #[test]
    fn directory_budget_bounds_empty_trees_and_excludes_child_transcripts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("subagents")).unwrap();
        std::fs::write(root.path().join("subagents/child.jsonl"), "{}").unwrap();
        for index in 0..10 {
            std::fs::create_dir(root.path().join(format!("empty-{index}"))).unwrap();
        }
        let mut files = Vec::new();
        let mut budget = 3;
        collect(root.path(), 3, &mut budget, &mut files);
        assert_eq!(budget, 0);
        assert!(files.is_empty());
    }

    /// A title set by renaming outranks a summary, which outranks the first
    /// message, and none of them is looked for in a subagent's records.
    #[test]
    fn a_transcript_titles_itself_by_its_name_then_summary_then_first_message() {
        let directory = tempfile::tempdir().unwrap();
        let id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        let cwd = directory.path().display().to_string();
        let record = |value: Value| format!("{value}\n");
        let path = directory.path().join("transcript.jsonl");
        let mut text = [
            json!({"type":"user","sessionId":id,"cwd":cwd,"isSidechain":true,"message":{"content":"Wrong subagent"}}),
            json!({"type":"user","sessionId":id,"cwd":cwd,"message":{"content":[{"type":"text","text":"Review Rust"}]}}),
        ]
        .into_iter()
        .map(record)
        .collect::<String>();
        std::fs::write(&path, &text).unwrap();
        let title = || header(Provider::Claude, &path).unwrap().title;
        assert_eq!(title().as_deref(), Some("Review Rust"));
        // Records too far into the file to be read in the head are found in
        // its tail.
        text.push_str(&record(
            json!({"type":"assistant","message":{"content":"x".repeat(2 * HEAD_BYTES as usize)}}),
        ));
        text.push_str(&record(
            json!({"type":"summary","summary":"Generated summary"}),
        ));
        std::fs::write(&path, &text).unwrap();
        assert_eq!(title().as_deref(), Some("Generated summary"));
        text.push_str(&record(
            json!({"type":"custom-title","customTitle":"My saved review"}),
        ));
        std::fs::write(&path, &text).unwrap();
        assert_eq!(title().as_deref(), Some("My saved review"));
        assert_eq!(header(Provider::Claude, &path).unwrap().id, id);
    }

    /// A Codex subagent's rollout is its parent's history once over, so it
    /// is never listed, whatever it goes on to record.
    #[test]
    fn a_codex_subagent_rollout_is_not_a_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let meta = |id: &str, source: Value| json!({"type":"session_meta","payload":{"id":id,"cwd":directory.path(),"source":source}});
        let parent = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        let child = "01a0bffc-5aa8-7d30-bd0b-3589308d81a1";
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                meta(child, json!({"subagent":{"thread_spawn":{}}})),
                meta(parent, json!("cli"))
            ),
        )
        .unwrap();
        assert!(header(Provider::Codex, &path).is_none());
        std::fs::write(&path, format!("{}\n", meta(parent, json!("cli")))).unwrap();
        assert_eq!(header(Provider::Codex, &path).unwrap().id, parent);
    }

    #[test]
    fn a_claude_command_titles_a_conversation_only_by_what_it_asks() {
        let command = |name: &str, args: &str| {
            format!(
                "<command-name>{name}</command-name>\n            <command-message>{name}</command-message>\n            <command-args>{args}</command-args>"
            )
        };
        assert_eq!(claude_prompt(&command("/exit", "")), None);
        assert_eq!(
            claude_prompt(&command("/solve-task", "fix resume")).as_deref(),
            Some("/solve-task fix resume")
        );
        assert_eq!(
            claude_prompt("<local-command-stdout>Bye!</local-command-stdout>"),
            None
        );
        assert_eq!(claude_prompt("<bash-input>ls</bash-input>"), None);
        assert_eq!(
            claude_prompt("Review <this> diff").as_deref(),
            Some("Review <this> diff")
        );
    }

    /// Codex names a conversation in its session index, where the latest
    /// name stands.
    #[test]
    fn codex_names_come_from_its_session_index() {
        let home = tempfile::tempdir().unwrap();
        let account = crate::account::Account {
            provider: Provider::Codex,
            home: crate::account::AccountHome::Linked {
                key: uuid::Uuid::new_v4(),
                path: home.path().into(),
            },
        };
        let seed = chat::fresh_session(
            Provider::Codex,
            TerminalId::new(),
            home.path().into(),
            &account,
        )
        .unwrap();
        let root = Provider::Codex.session_root(&seed.launch.recipe).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let id = "01a0743a-ccb3-7990-b840-3a0779cca5dd";
        std::fs::write(
            root.join("session_index.jsonl"),
            [
                json!({"id":id,"thread_name":"First name"}),
                json!({"id":id,"thread_name":"Renamed"}),
            ]
            .map(|record| format!("{record}\n"))
            .concat(),
        )
        .unwrap();
        assert_eq!(names(&[seed]).get(id).map(String::as_str), Some("Renamed"));
    }

    /// The cache answers for a file until it changes.
    #[test]
    fn a_changed_transcript_is_read_again() {
        let directory = tempfile::tempdir().unwrap();
        let id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        let path = directory.path().join(format!("{id}.jsonl"));
        let write = |text: &str| {
            std::fs::write(&path, format!("{}\n", json!({"type":"user","sessionId":id,"cwd":directory.path(),"message":{"content":text}}))).unwrap();
        };
        let candidate = || {
            let metadata = std::fs::metadata(&path).unwrap();
            Candidate {
                seed: 0,
                path: path.clone(),
                modified_ms: modified_ms(&metadata).unwrap(),
                len: metadata.len(),
            }
        };
        let cache = Cache::default();
        write("First");
        assert_eq!(
            cache.find(Provider::Claude, &candidate()).unwrap().title,
            "First"
        );
        write("Second, longer");
        assert_eq!(
            cache.find(Provider::Claude, &candidate()).unwrap().title,
            "Second, longer"
        );
    }
}
