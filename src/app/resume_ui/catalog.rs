//! Bounded, read-only discovery. No provider process or model request is needed.
//!
//! Discovery takes two passes so that a large history is listed at once. The
//! first only stats each provider's session store, which stays cheap at any
//! size and orders every transcript newest first. The second summarizes each
//! transcript, newest first. A summary is kept with the file version it
//! describes, in memory and on disk between runs, and a transcript that only
//! grew is read from where its last summary stopped, so a history is read in
//! full once and then only as it changes.
use super::scan::{self, Scan, Summary};
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Directory entries walked per session store, which bounds a store that is
/// not one, such as a home directory configured by mistake.
const ENTRY_LIMIT: usize = 262_144;
/// Transcripts remembered across openings of the picker.
const CACHE_LIMIT: usize = 131_072;
/// A saved catalog larger than this is not one this build wrote.
const STORE_LIMIT: u64 = 256 << 20;
/// Changes when what a saved catalog holds changes meaning, so an older one
/// is read again from the transcripts rather than misread.
const STORE_FORMAT: u32 = 1;

/// A transcript found by its file alone, before anything in it is read.
pub struct Candidate {
    /// The index of the session whose account and launch it resumes with.
    pub seed: usize,
    pub path: PathBuf,
    modified_ms: i64,
    version: FileVersion,
}

/// A saved conversation as listed, without the session it would resume as.
/// The session is built only for the one the reader chooses.
#[derive(Debug, Clone)]
pub struct Found {
    pub seed: usize,
    pub path: PathBuf,
    pub summary: Arc<Summary>,
    pub title: String,
    /// The transcript's size, as a measure of how long the conversation ran.
    pub bytes: u64,
    /// Whether the transcript gives a title at all, its first message
    /// included, rather than going by its provider.
    pub titled: bool,
    /// Whether that title is a name or summary the provider or the reader
    /// gave the conversation, rather than its first message.
    pub named: bool,
    pub updated_at_unix_ms: i64,
}

impl Found {
    pub fn id(&self) -> &str {
        &self.summary.id
    }

    pub fn directory(&self) -> &Path {
        &self.summary.directory
    }
}

/// File identity and nanosecond change stamps distinguish replacement and
/// same-size rewrites too; the millisecond UI timestamp is not a cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileVersion {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl FileVersion {
    fn new(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// A transcript's summary and the version of the file it describes.
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    version: FileVersion,
    scan: Arc<Scan>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    format: u32,
    entries: Vec<(PathBuf, Entry)>,
}

/// Transcript summaries by exact file version, retained across picker
/// openings and, through [`Cache::restore`] and [`Cache::save`], across runs.
#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// Whether the saved catalog was read, which happens once per run.
    restored: AtomicBool,
    /// Whether anything changed since the catalog was last saved.
    changed: AtomicBool,
}

/// Every transcript in the seeds' session stores, newest first, or as many
/// as were found before `cancel` was set.
pub fn candidates(seeds: &[chat::Session], cancel: &AtomicBool) -> Vec<Candidate> {
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
        collect(&root, depth, &mut budget, cancel, &mut files);
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
                version: FileVersion::new(&metadata),
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
    cancel: &AtomicBool,
    files: &mut Vec<(PathBuf, std::fs::Metadata)>,
) {
    if depth == 0 || *budget == 0 || cancel.load(Ordering::Relaxed) {
        return;
    }
    let Ok(directory) = std::fs::read_dir(root) else {
        return;
    };
    // Bound directory entries too, including unrelated files and symlinks.
    for entry in directory.flatten() {
        if *budget == 0 || cancel.load(Ordering::Relaxed) {
            break;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && path.file_name().is_none_or(|name| name != "subagents") {
            collect(&path, depth - 1, budget, cancel, files);
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
    /// The conversation `candidate` holds, read only as far as it changed
    /// since it was last read.
    #[cfg(test)]
    pub fn find(&self, provider: Provider, candidate: &Candidate) -> Option<Found> {
        self.find_with_cancel(provider, candidate, &AtomicBool::new(false))
    }

    fn find_with_cancel(
        &self,
        provider: Provider,
        candidate: &Candidate,
        cancel: &AtomicBool,
    ) -> Option<Found> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let previous = self.entries.lock().ok()?.get(&candidate.path).cloned();
        let scan = match previous {
            Some(entry) if entry.version == candidate.version => entry.scan,
            previous => {
                // A cancelled or unreadable read is never remembered.
                let scan = Arc::new(scan::scan(
                    provider,
                    &candidate.path,
                    previous.as_ref().map(|entry| &*entry.scan),
                    cancel,
                )?);
                let mut entries = self.entries.lock().ok()?;
                if entries.len() >= CACHE_LIMIT {
                    entries.clear();
                }
                entries.insert(
                    candidate.path.clone(),
                    Entry {
                        version: candidate.version,
                        scan: scan.clone(),
                    },
                );
                self.changed.store(true, Ordering::Relaxed);
                scan
            }
        };
        let summary = scan.summary()?;
        // A transcript of nothing, such as a session opened and left at
        // once, has nothing to return to.
        if summary.messages() == 0 && summary.name.is_none() {
            return None;
        }
        let title = summary.title();
        Some(Found {
            seed: candidate.seed,
            path: candidate.path.clone(),
            titled: title.is_some(),
            named: summary.name.is_some(),
            title: title.unwrap_or_else(|| format!("{} conversation", provider.label())),
            summary: Arc::new(summary.clone()),
            bytes: candidate.version.len,
            updated_at_unix_ms: candidate.modified_ms,
        })
    }

    /// Reads the catalog saved by an earlier run, once per run. A missing,
    /// damaged or foreign one is ignored: the transcripts are read instead.
    pub fn restore(&self, path: &Path) {
        if self.restored.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(stored) = std::fs::File::open(path)
            .ok()
            .filter(|file| file.metadata().is_ok_and(|meta| meta.len() <= STORE_LIMIT))
            .and_then(|file| {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::BufReader::new(file), &mut bytes).ok()?;
                postcard::from_bytes::<Stored>(&bytes).ok()
            })
            .filter(|stored| stored.format == STORE_FORMAT)
        else {
            return;
        };
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        for (path, entry) in stored.entries.into_iter().take(CACHE_LIMIT) {
            entries.entry(path).or_insert(entry);
        }
    }

    /// Saves the catalog for the next run when it changed, keeping only the
    /// transcripts in `live` when every one was looked at.
    pub fn save(&self, path: &Path, live: Option<&HashSet<&Path>>) -> std::io::Result<()> {
        let stored = {
            let Ok(mut entries) = self.entries.lock() else {
                return Ok(());
            };
            if let Some(live) = live {
                let before = entries.len();
                entries.retain(|path, _| live.contains(path.as_path()));
                if entries.len() != before {
                    self.changed.store(true, Ordering::Relaxed);
                }
            }
            if !self.changed.swap(false, Ordering::Relaxed) {
                return Ok(());
            }
            Stored {
                format: STORE_FORMAT,
                entries: entries
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect(),
            }
        };
        let bytes = postcard::to_allocvec(&stored).map_err(std::io::Error::other)?;
        let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".resume-catalog.")
            .tempfile_in(parent)?;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        temporary.write_all(&bytes)?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

/// The conversations `candidates` hold, in their order, read by up to
/// `workers` threads until `cancel` is set.
pub fn find_all(
    cache: &Cache,
    seeds: &[chat::Session],
    candidates: &[Candidate],
    workers: usize,
    cancel: &AtomicBool,
) -> Vec<Found> {
    let find = |chunk: &[Candidate]| {
        chunk
            .iter()
            .take_while(|_| !cancel.load(Ordering::Relaxed))
            .filter_map(|candidate| {
                cache.find_with_cancel(seeds[candidate.seed].launch.provider, candidate, cancel)
            })
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

/// Codex names by the seed's actual account root and conversation ID. A
/// copied session ID in another account is a different resume choice. Shared
/// roots are read once and share their immutable result across seed recipes.
pub fn names(seeds: &[chat::Session]) -> Vec<Arc<HashMap<String, String>>> {
    let mut roots = HashMap::<PathBuf, Arc<HashMap<String, String>>>::new();
    let empty = Arc::new(HashMap::new());
    seeds
        .iter()
        .map(|seed| {
            if seed.launch.provider != Provider::Codex {
                return empty.clone();
            }
            let Some(root) = Provider::Codex.session_root(&seed.launch.recipe) else {
                return empty.clone();
            };
            let path = root.join("session_index.jsonl");
            roots
                .entry(path.clone())
                .or_insert_with(|| {
                    let mut names = HashMap::new();
                    if let Ok(file) = std::fs::File::open(&path) {
                        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if let (Some(id), Some(name)) = (
                                value["id"].as_str(),
                                value["thread_name"]
                                    .as_str()
                                    .and_then(crate::workspace_model::channel_title),
                            ) {
                                names.insert(id.to_owned(), name);
                            }
                        }
                    }
                    Arc::new(names)
                })
                .clone()
        })
        .collect()
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
    session.launch.recipe.working_directory = found.directory().to_path_buf();
    session.launch.session_id = Some(found.id().to_owned());
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

    fn candidate(seed: usize, path: &Path) -> Candidate {
        let metadata = std::fs::metadata(path).unwrap();
        Candidate {
            seed,
            path: path.to_path_buf(),
            modified_ms: modified_ms(&metadata).unwrap(),
            version: FileVersion::new(&metadata),
        }
    }

    /// What a fresh picker lists for one transcript.
    fn header(provider: Provider, path: &Path) -> Option<Found> {
        Cache::default().find(provider, &candidate(0, path))
    }

    fn discover(seeds: &[chat::Session]) -> Vec<(Found, chat::Session)> {
        let cache = Cache::default();
        candidates(seeds, &AtomicBool::new(false))
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
            let records = match provider {
                Provider::Claude => vec![
                    json!({"type":"user","sessionId":id,"cwd":root.path(),"message":{"content":"Restore me"}}),
                ],
                Provider::Codex => vec![
                    json!({"type":"session_meta","payload":{"id":id,"cwd":root.path(),"source":"cli"}}),
                    json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"Restore me"}]}}}),
                ],
                Provider::Omp => vec![
                    json!({"type":"session","id":id,"cwd":root.path()}),
                    json!({"type":"message","message":{"role":"user","content":"Restore me"}}),
                ],
            };
            std::fs::write(
                directory.join(format!("{id}.jsonl")),
                records
                    .iter()
                    .map(|record| format!("{record}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            let entries = discover(&[seed.clone()]);
            assert_eq!(entries.len(), 1, "{provider:?}");
            let (found, session) = &entries[0];
            assert_eq!(found.id(), id);
            assert_eq!(found.title, "Restore me", "{provider:?}");
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
        collect(
            root.path(),
            3,
            &mut budget,
            &AtomicBool::new(false),
            &mut files,
        );
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
        assert_eq!(title(), "Review Rust");
        // A title is found however far into the file it was written.
        text.push_str(&record(
            json!({"type":"assistant","message":{"content":"x".repeat(1 << 20)}}),
        ));
        text.push_str(&record(
            json!({"type":"summary","summary":"Generated summary"}),
        ));
        std::fs::write(&path, &text).unwrap();
        assert_eq!(title(), "Generated summary");
        text.push_str(&record(
            json!({"type":"custom-title","customTitle":"My saved review"}),
        ));
        std::fs::write(&path, &text).unwrap();
        assert_eq!(title(), "My saved review");
        assert_eq!(header(Provider::Claude, &path).unwrap().id(), id);
    }

    /// A Codex subagent's rollout is its parent's history once over, so it
    /// is never listed, whatever it goes on to record.
    #[test]
    fn a_codex_subagent_rollout_is_not_a_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let meta = |id: &str, source: Value| json!({"type":"session_meta","payload":{"id":id,"cwd":directory.path(),"source":source}});
        let asked =
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Profile it"}});
        let parent = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        let child = "01a0bffc-5aa8-7d30-bd0b-3589308d81a1";
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{asked}\n",
                meta(child, json!({"subagent":{"thread_spawn":{}}})),
                meta(parent, json!("cli"))
            ),
        )
        .unwrap();
        assert!(header(Provider::Codex, &path).is_none());
        std::fs::write(&path, format!("{}\n{asked}\n", meta(parent, json!("cli")))).unwrap();
        assert_eq!(header(Provider::Codex, &path).unwrap().id(), parent);
    }

    /// A transcript with nothing in it has nothing to return to.
    #[test]
    fn an_empty_conversation_is_not_listed_unless_named() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        let meta = json!({"type":"session_meta","payload":{"id":id,"cwd":directory.path(),"source":"cli"}});
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        assert!(header(Provider::Codex, &path).is_none());
        let named = json!({"type":"session_info","name":"Kept for later"});
        std::fs::write(&path, format!("{meta}\n{named}\n")).unwrap();
        assert_eq!(
            header(Provider::Codex, &path).unwrap().title,
            "Kept for later"
        );
    }

    #[test]
    fn claude_resume_uses_custom_then_generated_name_then_content_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let id = "12345678-1234-4234-8234-123456789abc";
        let prompt = json!({"type":"user","sessionId":id,"cwd":"/project","message":{"content":"\"\n> Quoted old context\nReview bundle encryption and performance"}});
        let ai = json!({"type":"ai-title","sessionId":id,"aiTitle":"Generated encryption audit"});
        std::fs::write(&path, format!("{prompt}\n{ai}\n")).unwrap();
        let found = header(Provider::Claude, &path).unwrap();
        assert_eq!(found.title, "Generated encryption audit");
        assert!(found.named);
        let custom =
            json!({"type":"custom-title","sessionId":id,"customTitle":"My bundle project"});
        std::fs::write(&path, format!("{prompt}\n{custom}\n{ai}\n")).unwrap();
        assert_eq!(
            header(Provider::Claude, &path).unwrap().title,
            "My bundle project"
        );
        for title in ["1341", "🔐", "🧑‍💻"] {
            let custom = json!({"type":"custom-title","sessionId":id,"customTitle":title});
            std::fs::write(&path, format!("{prompt}\n{custom}\n")).unwrap();
            let found = header(Provider::Claude, &path).unwrap();
            assert_eq!(found.title, title);
            assert!(found.named);
            assert_eq!(
                Some(found.title),
                crate::workspace_model::channel_title(title)
            );
        }
        std::fs::write(&path, format!("{prompt}\n")).unwrap();
        let found = header(Provider::Claude, &path).unwrap();
        assert_eq!(found.title, "Review bundle encryption and performance");
        assert!(found.titled);
        assert!(!found.named);
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
        assert_eq!(
            names(&[seed.clone()])[0].get(id).map(String::as_str),
            Some("Renamed")
        );
        for title in [
            "A".repeat(70),
            "🔐".repeat(90),
            "\u{202e}Bundle\nreview\u{2066}".into(),
        ] {
            std::fs::write(
                root.join("session_index.jsonl"),
                format!("{}\n", json!({"id":id,"thread_name":title})),
            )
            .unwrap();
            assert_eq!(
                names(&[seed.clone()])[0].get(id).cloned(),
                crate::workspace_model::channel_title(&title)
            );
        }
    }

    #[test]
    fn codex_names_stay_with_the_original_account_and_provider() {
        let directory = tempfile::tempdir().unwrap();
        let mut seeds = Vec::new();
        let id = "01a0743a-ccb3-7990-b840-3a0779cca5dd";
        for (slot, provider) in [Provider::Codex, Provider::Codex, Provider::Claude]
            .into_iter()
            .enumerate()
        {
            let home = directory.path().join(slot.to_string());
            std::fs::create_dir_all(&home).unwrap();
            let account = crate::account::Account {
                provider,
                home: crate::account::AccountHome::Linked {
                    key: uuid::Uuid::new_v4(),
                    path: home,
                },
            };
            let seed = chat::fresh_session(
                provider,
                TerminalId::new(),
                directory.path().into(),
                &account,
            )
            .unwrap();
            if provider == Provider::Codex {
                let root = provider.session_root(&seed.launch.recipe).unwrap();
                std::fs::create_dir_all(&root).unwrap();
                std::fs::write(
                    root.join("session_index.jsonl"),
                    format!(
                        "{}\n",
                        json!({"id":id,"thread_name":format!("Account {slot} title")})
                    ),
                )
                .unwrap();
            }
            seeds.push(seed);
        }
        seeds.push(seeds[0].clone());
        let names = names(&seeds);
        assert_eq!(
            names[0].get(id).map(String::as_str),
            Some("Account 0 title")
        );
        assert_eq!(
            names[1].get(id).map(String::as_str),
            Some("Account 1 title")
        );
        assert!(
            names[2].is_empty(),
            "Claude cannot borrow a same-ID Codex name"
        );
        assert!(
            Arc::ptr_eq(&names[0], &names[3]),
            "one account root is read only once"
        );
    }

    #[test]
    fn provider_resume_names_share_live_label_sanitization_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let id = "12345678-1234-4234-8234-123456789abc";
        let prompt = json!({"type":"user","sessionId":id,"cwd":"/project","message":{"content":"Review encryption performance"}});
        for title in [
            "A".repeat(70),
            "🧑‍💻".repeat(40),
            "\u{202e}Bundle\nreview\u{2066}".into(),
        ] {
            let custom = json!({"type":"custom-title","sessionId":id,"customTitle":title});
            std::fs::write(&path, format!("{prompt}\n{custom}\n")).unwrap();
            let found = header(Provider::Claude, &path).unwrap();
            assert_eq!(
                Some(found.title),
                crate::workspace_model::channel_title(&title)
            );
            assert!(found.named);
        }
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
        let cache = Cache::default();
        write("Review encryption");
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(0, &path))
                .unwrap()
                .title,
            "Review encryption"
        );
        write("Review encryption throughput");
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(0, &path))
                .unwrap()
                .title,
            "Review encryption throughput"
        );
    }

    #[test]
    fn a_growing_claude_transcript_keeps_its_custom_name_wherever_it_was_set() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let id = "00000000-0000-4000-8000-000000000001";
        let cwd = directory.path();
        let mut file = std::fs::File::create(&path).unwrap();
        for record in [
            json!({"type":"user","sessionId":id,"cwd":cwd,"message":{"content":"First fallback"}}),
            json!({"type":"ai-title","sessionId":id,"aiTitle":"Earlier generated"}),
            json!({"type":"assistant","message":{"content":"x".repeat(1 << 20)}}),
            json!({"type":"custom-title","sessionId":id,"customTitle":"Middle custom"}),
        ] {
            writeln!(file, "{record}").unwrap();
        }
        file.flush().unwrap();
        let cache = Cache::default();
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(0, &path))
                .unwrap()
                .title,
            "Middle custom"
        );
        for record in [
            json!({"type":"assistant","message":{"content":"y".repeat(128*1024)}}),
            json!({"type":"ai-title","sessionId":id,"aiTitle":"Later generated"}),
        ] {
            writeln!(file, "{record}").unwrap();
        }
        file.flush().unwrap();
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(0, &path))
                .unwrap()
                .title,
            "Middle custom"
        );
        let fresh = header(Provider::Claude, &path).unwrap();
        assert_eq!(fresh.title, "Middle custom");
        assert!(fresh.named);
        assert_eq!(fresh.id(), id);
        assert_eq!(fresh.directory(), cwd);
        writeln!(
            file,
            "{}",
            json!({"type":"custom-title","sessionId":id,"customTitle":"Latest custom"})
        )
        .unwrap();
        file.flush().unwrap();
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(0, &path))
                .unwrap()
                .title,
            "Latest custom"
        );
    }

    #[test]
    fn claude_picker_scopes_names_and_does_not_cache_cancelled_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let id = "00000000-0000-4000-8000-000000000001";
        let records = [
            json!({"type":"user","sessionId":id,"cwd":directory.path(),"message":{"content":"Original fallback"}}),
            json!({"type":"custom-title","sessionId":"00000000-0000-4000-8000-000000000002","customTitle":"Other session"}),
            json!({"type":"custom-title","sessionId":id,"cwd":"/foreign","customTitle":"Other directory"}),
            json!({"type":"custom-title","sessionId":id,"isSidechain":true,"customTitle":"Sidechain"}),
        ];
        std::fs::write(
            &path,
            records
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let cache = Cache::default();
        assert!(
            cache
                .find_with_cancel(
                    Provider::Claude,
                    &candidate(0, &path),
                    &AtomicBool::new(true)
                )
                .is_none()
        );
        assert!(cache.entries.lock().unwrap().is_empty());
        let found = cache.find(Provider::Claude, &candidate(0, &path)).unwrap();
        assert_eq!(found.title, "Original fallback");
        assert!(!found.named);
        let other = directory.path().join("another-account.jsonl");
        std::fs::write(
            &other,
            format!(
                "{}\n{}\n",
                records[0],
                json!({"type":"custom-title","sessionId":id,"customTitle":"Another account name"})
            ),
        )
        .unwrap();
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(1, &other))
                .unwrap()
                .title,
            "Another account name"
        );
        assert_eq!(
            cache
                .find(Provider::Claude, &candidate(0, &path))
                .unwrap()
                .title,
            "Original fallback"
        );
    }

    #[test]
    fn same_size_replacement_invalidates_claude_picker_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let id = "00000000-0000-4000-8000-000000000001";
        let write = |path: &Path, name: &str| {
            std::fs::write(path,format!("{}\n{}\n",json!({"type":"user","sessionId":id,"cwd":directory.path(),"message":{"content":"Fallback"}}),json!({"type":"custom-title","sessionId":id,"customTitle":name}))).unwrap();
        };
        write(&path, "First custom");
        let before = candidate(0, &path);
        let cache = Cache::default();
        assert_eq!(
            cache.find(Provider::Claude, &before).unwrap().title,
            "First custom"
        );
        let replacement = path.with_extension("replacement");
        write(&replacement, "Other custom");
        std::fs::rename(replacement, &path).unwrap();
        let after = candidate(0, &path);
        assert_eq!(before.version.len, after.version.len);
        assert_ne!(before.version.inode, after.version.inode);
        assert_eq!(
            cache.find(Provider::Claude, &after).unwrap().title,
            "Other custom"
        );
    }

    /// A catalog saved by one run answers the next for every transcript
    /// that did not change, without reading it, and forgets transcripts
    /// that are gone.
    #[test]
    fn a_saved_catalog_answers_the_next_run_for_unchanged_transcripts() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        let kept = directory.path().join("kept.jsonl");
        let gone = directory.path().join("gone.jsonl");
        for path in [&kept, &gone] {
            std::fs::write(path, format!("{}\n", json!({"type":"user","sessionId":id,"cwd":directory.path(),"message":{"content":"Remembered question"}}))).unwrap();
        }
        let store = directory.path().join("cache/resume-catalog");
        let first = Cache::default();
        for path in [&kept, &gone] {
            assert!(first.find(Provider::Claude, &candidate(0, path)).is_some());
        }
        let kept_candidate = candidate(0, &kept);
        std::fs::remove_file(&gone).unwrap();
        first
            .save(&store, Some(&HashSet::from([kept.as_path()])))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Unreadable now, so only the saved catalog can answer.
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o000)).unwrap();
        let next = Cache::default();
        next.restore(&store);
        assert_eq!(next.entries.lock().unwrap().len(), 1);
        let found = next.find(Provider::Claude, &kept_candidate).unwrap();
        assert_eq!(found.title, "Remembered question");
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o600)).unwrap();
        // A damaged catalog is ignored rather than trusted.
        std::fs::write(&store, b"not a catalog").unwrap();
        let damaged = Cache::default();
        damaged.restore(&store);
        assert!(damaged.entries.lock().unwrap().is_empty());
    }
}
