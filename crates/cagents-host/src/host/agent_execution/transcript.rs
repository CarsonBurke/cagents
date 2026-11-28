//! Reads a page of a session's conversation from its provider's own saved
//! history, whether or not the provider is still running.
//!
//! The history is located from the session's launch alone: its provider,
//! conversation ID, working directory and account home. A caller names a
//! session, never a file. Reading confers no control, like listing.

use super::{SharedHost, directory};
use crate::host::file_revision::FileRevision;
use crate::{
    agent::Provider,
    agent_execution::{
        Authority,
        directory::{Member, Relation, Scope},
        transcript::{Builder, Entry, MAX_LIMIT, Page, Query, matches_cursor},
    },
    chat::{
        self, Item, ItemContent, Launch,
        protocol::codex::rollout::{Batch, Rollout},
    },
    model::TerminalId,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::Metadata,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// The shortest handle accepted, as for messages.
const MIN_HANDLE: usize = 4;
/// Provider history pages read for one reply before it stops with a cursor.
const MAX_PAGES: usize = 4_096;

pub(super) fn read(host: &SharedHost, authority: &Authority, query: Query) -> Result<Page, String> {
    if query.limit == 0 || query.limit > MAX_LIMIT {
        return Err(format!("A page holds between 1 and {MAX_LIMIT} messages"));
    }
    let listing = directory::list(
        host,
        authority,
        directory::Wanted::Scope(Scope::All),
        true,
        true,
    )?;
    // A conversation is also named by its provider's own ID, as the
    // provider's files and resume commands show it.
    let mut conversations: HashMap<TerminalId, String> = listing
        .saved
        .iter()
        .filter_map(|(id, launch)| Some((*id, launch.session_id.clone()?)))
        .collect();
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        for (id, record) in &state.chats {
            if let Some(conversation) = record
                .handle
                .confirmed_session_id()
                .or_else(|| record.session.launch.session_id.clone())
            {
                conversations.insert(*id, conversation);
            }
        }
    }
    let mut session = resolve(&listing, &conversations, &query.session)?;
    let source = source(
        host,
        session.session.id,
        true,
        listing.saved.get(&session.session.id),
    )?;
    let recognizer = super::mailbox::recognizer(host, session.session.id);
    let rollouts = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .rollouts
        .clone();
    let (modified, (entries, earlier)) = source.page(&rollouts, &query, |item| {
        recognizer.as_ref().and_then(|seal| seal.recognized(item))
    })?;
    session.active_at = modified;
    Ok(Page {
        session,
        entries,
        earlier,
    })
}

type Entries = (Vec<Entry>, Option<String>);

fn page(
    history: &mut dyn History,
    query: &Query,
    recognize: impl Fn(&Item) -> Option<Item>,
) -> Result<Entries, String> {
    let mut builder = Builder::new(query);
    let mut seen = HashSet::new();
    let mut pages = 0;
    let mut exhausted = false;
    'pages: loop {
        let skipped = query
            .before
            .as_deref()
            .filter(|_| !builder.found_cursor())
            .and_then(|before| history.skip_before(before, &mut seen));
        let earlier = if let Some(earlier) = skipped {
            earlier
        } else {
            let (items, earlier) = if pages == 0 {
                history.newest()?
            } else {
                history.earlier()?
            };
            for item in items.iter().rev() {
                // The newest occurrence of each provider ID wins.
                if !seen.insert(item.id.clone()) {
                    continue;
                }
                let recognized = recognize(item);
                if !builder.push(recognized.as_ref().unwrap_or(item)) {
                    break 'pages;
                }
            }
            earlier
        };
        pages += 1;
        if !earlier {
            exhausted = true;
            break;
        }
        if pages == MAX_PAGES {
            break;
        }
    }
    if !exhausted && !builder.found_cursor() {
        return Err("The cursor is too far back to reach in one read".into());
    }
    builder.finish(exhausted)
}

/// The one session `handle` names, as `cagents send` resolves a recipient:
/// `parent`, or a prefix of a session ID, lineage or delegated agent ID; or
/// a prefix of the provider's ID for its conversation, from `conversations`.
fn resolve(
    listing: &directory::Listing,
    conversations: &HashMap<TerminalId, String>,
    handle: &str,
) -> Result<Member, String> {
    if handle == "parent" {
        // A parent relaunched in its card leaves the exited original listed
        // beside it; the live one is the parent now.
        let parents: Vec<&Member> = listing
            .members()
            .filter(|member| member.session.relation == Relation::Parent)
            .collect();
        let live: Vec<&Member> = parents
            .iter()
            .copied()
            .filter(|member| !member.exited)
            .collect();
        let chosen = if live.is_empty() { parents } else { live };
        let mut parents = chosen.into_iter();
        return match (parents.next(), parents.next()) {
            (Some(parent), None) => Ok(parent.clone()),
            (None, _) if listing.caller.is_none() => {
                Err("Not running inside a cagents session, so there is no parent".into())
            }
            (None, _) => Err("This session has no parent session".into()),
            (Some(_), Some(_)) => Err("The parent lineage has more than one session".into()),
        };
    }
    let handle = handle.to_ascii_lowercase();
    if handle.len() < MIN_HANDLE {
        return Err(format!(
            "Name the session by at least {MIN_HANDLE} characters of its ID"
        ));
    }
    let matches = |id: &dyn std::fmt::Display| id.to_string().starts_with(&handle);
    let found: Vec<&Member> = listing
        .members()
        .filter(|member| member.session.provider.is_some())
        .filter(|Member { session: entry, .. }| {
            matches(&entry.id)
                || matches(&entry.lineage)
                || conversations
                    .get(&entry.id)
                    .is_some_and(|conversation| matches(conversation))
                || entry
                    .delegation
                    .as_ref()
                    .is_some_and(|delegation| matches(&delegation.agent))
        })
        .collect();
    // A relaunched session shares its lineage with the exited one it
    // replaced; a session ID names one exactly, otherwise the live one wins.
    let by_id: Vec<&Member> = found
        .iter()
        .copied()
        .filter(|member| matches(&member.session.id))
        .collect();
    let live: Vec<&Member> = found
        .iter()
        .copied()
        .filter(|member| !member.exited)
        .collect();
    match (found.as_slice(), by_id.as_slice(), live.as_slice()) {
        ([only], _, _) | (_, [only], _) => Ok((*only).clone()),
        (many, _, [only])
            if many
                .iter()
                .all(|member| member.session.lineage == only.session.lineage) =>
        {
            Ok((**only).clone())
        }
        ([], _, _) => Err(format!(
            "No session with a conversation matches {handle}; list them with `cagents ls --channel` or `cagents ls --all`"
        )),
        (many, _, _) => Err(format!(
            "{} sessions match {handle}; give more of the ID",
            many.len()
        )),
    }
}

/// What locates a session's saved provider history.
pub(super) struct Source {
    launch: Launch,
}

/// The history of `id`, located from its launch. A terminal's provider
/// conversation is observed afresh when `observe` is set, which waits on its
/// PTY owner; otherwise its last recorded resume hint is used. A chat this
/// daemon is not hosting is located from `saved`, its pane's saved launch.
pub(super) fn source(
    host: &SharedHost,
    id: TerminalId,
    observe: bool,
    saved: Option<&Launch>,
) -> Result<Source, String> {
    enum Found {
        Chat(Launch, Option<String>),
        Terminal(crate::terminal::TerminalHandle, Option<String>),
    }
    let found = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = state.chats.get(&id) {
            Some(Found::Chat(
                record.session.launch.clone(),
                record.handle.confirmed_session_id(),
            ))
        } else {
            state.terminals.get(&id).map(|record| {
                Found::Terminal(record.handle.clone(), record.session.resume_hint.clone())
            })
        }
    };
    let found = match found {
        Some(found) => found,
        // A pane saved before a restart keeps the launch that located its
        // conversation, though no window has reopened it here.
        None => Found::Chat(
            saved.cloned().ok_or("The session is no longer hosted")?,
            None,
        ),
    };
    let mut launch = match found {
        Found::Chat(mut launch, confirmed) => {
            // The provider may have moved to another conversation, as after
            // a clear; the identity it confirmed is the current one.
            if confirmed.is_some() {
                launch.session_id = confirmed;
            }
            launch
        }
        Found::Terminal(handle, recorded) => {
            let hint = if observe {
                handle.observe_agent().ok().flatten().or(recorded)
            } else {
                recorded
            };
            let recipe = hint
                .as_deref()
                .and_then(crate::model::agent_recovery::recipe_from_hint)
                .ok_or("cagents does not know which conversation this terminal's agent is in")?;
            let provider = Provider::recognize_launch(&recipe)
                .ok_or("cagents does not recognize this terminal's agent")?;
            let session_id = provider.resume_id(&recipe.arguments).map(str::to_owned);
            Launch {
                provider,
                recipe,
                session_id,
                draft_model: None,
            }
        }
    };
    if launch.session_id.is_none() {
        return Err("The conversation has no saved history yet".into());
    }
    // A terminal's recipe can leave HOME to the environment it inherited.
    if !launch.recipe.environment.contains_key("HOME")
        && let Some(home) = std::env::var_os("HOME").and_then(|home| home.into_string().ok())
    {
        launch.recipe.environment.insert("HOME".into(), home);
    }
    Ok(Source { launch })
}

impl Source {
    fn path(&self) -> Result<PathBuf, String> {
        let id = self.launch.session_id.as_deref().unwrap_or_default();
        let root = self
            .launch
            .provider
            .session_root(&self.launch.recipe)
            .ok_or("Cannot locate the provider's history")?;
        match self.launch.provider {
            Provider::Claude => {
                let cwd = self
                    .launch
                    .recipe
                    .working_directory
                    .canonicalize()
                    .unwrap_or_else(|_| self.launch.recipe.working_directory.clone());
                crate::agent::claude_transcript(&root, &cwd, id)
            }
            // An archived thread keeps its rollout outside the dated tree.
            Provider::Codex => crate::agent::codex_rollout(&root, id).or_else(|error| {
                crate::agent::codex_archived_rollout(&root, id).map_err(|_| error)
            }),
            Provider::Omp => Err("Reading an Omp conversation is not supported yet".into()),
            Provider::Antigravity | Provider::Muse => Err(format!(
                "Reading {} conversations is not supported yet",
                self.launch.provider.label()
            )),
        }
    }

    /// When the provider last saved this conversation, in Unix milliseconds.
    pub(super) fn modified(&self) -> Option<u64> {
        modified(&std::fs::metadata(self.path().ok()?).ok()?)
    }

    fn page(
        &self,
        rollouts: &Rollouts,
        query: &Query,
        recognize: impl Fn(&Item) -> Option<Item>,
    ) -> Result<(Option<u64>, Entries), String> {
        if self.launch.provider == Provider::Claude {
            let at = self.modified();
            return Ok((
                at,
                page(&mut Claude::open(&self.launch)?, query, recognize)?,
            ));
        }
        let rollout = Rollout::open(&self.path()?)?;
        let at = modified(rollout.metadata());
        match rollouts.open(rollout) {
            Ok(mut history) => {
                let result = page(&mut *history, query, &recognize);
                if history.still_current() {
                    return Ok((at, result?));
                }
            }
            Err(error) if error.kind() != std::io::ErrorKind::UnexpectedEof => {
                return Err(error.to_string());
            }
            Err(_) => {}
        }
        // A writer changed the descriptor between indexing and replay. Use a
        // single forward read of a freshly opened revision.
        let rollout = Rollout::open(&self.path()?)?;
        let at = modified(rollout.metadata());
        let entries = page(&mut Whole(Arc::new(rollout.read().0)), query, recognize)?;
        Ok((at, entries))
    }
}

fn modified(metadata: &Metadata) -> Option<u64> {
    let since_epoch = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}

/// The recent rollout snapshots requested by the daemon. Lookups still
/// resolve the thread and securely open its file before consulting the cache.
#[derive(Default)]
pub(in crate::host) struct Rollouts(Mutex<VecDeque<Arc<CachedRollout>>>);

struct CachedRollout {
    revision: FileRevision,
    /// Same-revision readers share the index and recent small batches.
    snapshot: Mutex<Option<Snapshot>>,
}

const BATCH_BYTES: u64 = 256 * 1024;

type Items = Arc<Vec<Item>>;
type BatchCache = Mutex<Option<Items>>;

enum Snapshot {
    Whole(Items),
    Indexed(Arc<Index>),
}

impl Snapshot {
    fn history(&self, rollout: Rollout, revision: FileRevision) -> Box<dyn History> {
        match self {
            Self::Whole(items) => Box::new(Whole(items.clone())),
            Self::Indexed(index) => Box::new(Codex {
                remaining: index.batches.len(),
                index: index.clone(),
                rollout,
                revision,
            }),
        }
    }
}

struct Index {
    batches: Vec<Batch>,
    /// Recent payloads have a separate bound from the size of the file.
    recent: [BatchCache; 2],
    /// Earlier batches already visited for a cursor need only their IDs
    /// when a later request searches through them again.
    visited: Mutex<Visited>,
}

#[derive(Default)]
struct Visited {
    bytes: usize,
    ids: HashMap<usize, Box<[String]>>,
}

impl Visited {
    fn remember(&mut self, batch: usize, items: &[Item]) {
        const MAX_BYTES: usize = 1024 * 1024;
        if self.ids.contains_key(&batch) {
            return;
        }
        let bytes = items
            .iter()
            .fold(std::mem::size_of::<usize>(), |bytes, item| {
                bytes + std::mem::size_of::<String>() + item.id.len()
            });
        if bytes <= MAX_BYTES - self.bytes {
            self.ids
                .insert(batch, items.iter().map(|item| item.id.clone()).collect());
            self.bytes += bytes;
        }
    }
}

impl Rollouts {
    fn open(&self, rollout: Rollout) -> std::io::Result<Box<dyn History>> {
        const MAX_ENTRIES: usize = 4;

        let Some(revision) =
            FileRevision::of(rollout.metadata()).filter(|_| rollout.supports_revision_cache())
        else {
            return Ok(Box::new(Whole(Arc::new(rollout.read().0))));
        };
        let entry = {
            let mut entries = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let known = entries.iter().position(|entry| entry.revision == revision);
            let entry = known
                .and_then(|index| entries.remove(index))
                .unwrap_or_else(|| {
                    entries.retain(|entry| !entry.revision.same_file(revision));
                    Arc::new(CachedRollout {
                        revision,
                        snapshot: Mutex::new(None),
                    })
                });
            entries.push_front(entry.clone());
            entries.truncate(MAX_ENTRIES);
            entry
        };
        let mut cached = entry.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(snapshot) = cached.as_ref() {
            return Ok(snapshot.history(rollout, revision));
        }
        if revision.len <= BATCH_BYTES {
            let (items, closing) = rollout.read();
            let items = Arc::new(items);
            if closing.as_ref().and_then(FileRevision::of) == Some(revision) && cacheable(&items) {
                *cached = Some(Snapshot::Whole(items.clone()));
            }
            return Ok(Box::new(Whole(items)));
        }
        let snapshot = Snapshot::Indexed(Arc::new(Index {
            batches: rollout.batches(BATCH_BYTES)?,
            recent: Default::default(),
            visited: Default::default(),
        }));
        let history = snapshot.history(rollout, revision);
        if !history.still_current() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "The Codex rollout changed during a read",
            ));
        }
        *cached = Some(snapshot);
        Ok(history)
    }
}

struct Codex {
    rollout: Rollout,
    revision: FileRevision,
    index: Arc<Index>,
    remaining: usize,
}

impl Codex {
    fn remember(&self, index: usize, items: &[Item]) {
        self.index
            .visited
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remember(index, items);
    }
}

impl History for Codex {
    fn newest(&mut self) -> Result<HistoryPage, String> {
        self.earlier()
    }

    fn earlier(&mut self) -> Result<HistoryPage, String> {
        let Some(index) = self.remaining.checked_sub(1) else {
            return Ok(HistoryPage::default());
        };
        let batch = self.index.batches[index];
        let replay = || {
            self.rollout
                .replay(batch)
                .map(Arc::new)
                .map_err(|err| err.to_string())
        };
        let distance = self.index.batches.len() - self.remaining;
        let items = if let Some(slot) = self.index.recent.get(distance) {
            let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(items) = slot.as_ref() {
                items.clone()
            } else {
                let items = replay()?;
                if self.still_current() {
                    if cacheable(&items) {
                        *slot = Some(items.clone());
                    } else {
                        self.remember(index, &items);
                    }
                }
                items
            }
        } else {
            let items = replay()?;
            if self.still_current() {
                self.remember(index, &items);
            }
            items
        };
        self.remaining = index;
        Ok((items, index != 0))
    }

    fn skip_before(&mut self, before: &str, seen: &mut HashSet<String>) -> Option<bool> {
        let index = self.remaining.checked_sub(1)?;
        let visited = self.index.visited.lock().unwrap_or_else(|p| p.into_inner());
        let ids = visited.ids.get(&index)?;
        if ids.iter().any(|id| matches_cursor(before, id)) {
            return None;
        }
        // Preserve deduplication against items in the skipped newer batch.
        seen.extend(ids.iter().cloned());
        self.remaining = index;
        Some(index != 0)
    }

    fn still_current(&self) -> bool {
        self.rollout
            .current_metadata()
            .ok()
            .as_ref()
            .and_then(FileRevision::of)
            == Some(self.revision)
    }
}

/// A large source turn can project to a small batch after clipping tool
/// output. Charge retained payloads, including vector storage for short rows.
fn cacheable(items: &Vec<Item>) -> bool {
    const MAX_BYTES: usize = 1024 * 1024;
    let bytes = items.iter().fold(
        items.capacity() * std::mem::size_of::<Item>(),
        |total, item| {
            total
                + item.id.capacity()
                + item.content.byte_len()
                + match &item.content {
                    ItemContent::Changes { changes, .. } => {
                        changes.capacity() * std::mem::size_of::<chat::FileChange>()
                    }
                    ItemContent::WebSearch { results, .. } => {
                        results.capacity() * std::mem::size_of::<chat::SearchResult>()
                    }
                    _ => 0,
                }
        },
    );
    bytes <= MAX_BYTES
}

type HistoryPage = (Items, bool);

/// Saved history read newest page first. Each page is oldest first and says
/// whether earlier pages remain.
trait History {
    fn newest(&mut self) -> Result<HistoryPage, String>;
    fn earlier(&mut self) -> Result<HistoryPage, String>;

    fn skip_before(&mut self, _before: &str, _seen: &mut HashSet<String>) -> Option<bool> {
        None
    }

    fn still_current(&self) -> bool {
        true
    }
}

struct Whole(Items);

impl History for Whole {
    fn newest(&mut self) -> Result<HistoryPage, String> {
        Ok((self.0.clone(), false))
    }

    fn earlier(&mut self) -> Result<HistoryPage, String> {
        Ok(HistoryPage::default())
    }
}

/// Claude's read-only history reader: the same one a window uses to show a
/// conversation whose provider is gone. It opens the exact transcript and
/// never starts Claude.
struct Claude(Box<dyn chat::protocol::Adapter>);

impl Claude {
    fn open(launch: &Launch) -> Result<Self, String> {
        let (spec, adapter) = chat::protocol::create_history(launch)?;
        if !matches!(spec.transport, chat::protocol::TransportSpec::History) {
            return Err("Claude history cannot be read without starting Claude".into());
        }
        Ok(Self(adapter))
    }

    fn page(step: chat::protocol::Step) -> Result<HistoryPage, String> {
        if !step.writes.is_empty() {
            return Err("Reading Claude history attempted a provider write".into());
        }
        let mut page = None;
        for event in step.events {
            match event {
                chat::Event::History {
                    items, has_earlier, ..
                } => {
                    page = Some((Arc::new(items), has_earlier));
                }
                chat::Event::HistoryError(error) => return Err(error),
                _ => {}
            }
        }
        Ok(page.unwrap_or_default())
    }
}

impl History for Claude {
    fn newest(&mut self) -> Result<HistoryPage, String> {
        Self::page(self.0.initialize()?)
    }

    fn earlier(&mut self) -> Result<HistoryPage, String> {
        Self::page(self.0.command(chat::Command::LoadEarlier)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_execution::{
        Lineage,
        directory::{Entry, Surface},
        transcript::Content,
    };

    fn codex_source(root: &std::path::Path) -> (Source, PathBuf) {
        let id = "01a1b0ba-e200-7000-8000-000000000002";
        let day = root.join("sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-11-06T12-00-00-{id}.jsonl"));
        let launch = Launch {
            provider: Provider::Codex,
            session_id: Some(id.into()),
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: Vec::new(),
                working_directory: root.into(),
                environment: [
                    ("HOME".into(), root.display().to_string()),
                    ("CODEX_HOME".into(), root.display().to_string()),
                ]
                .into(),
                unset_environment: Default::default(),
            },
            draft_model: None,
        };
        (Source { launch }, path)
    }

    fn saved_message(id: &str, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":"response_item","payload":{
                "type":"message","id":id,"role":"assistant",
                "content":[{"type":"output_text","text":text}]
            }})
        )
    }

    const TURN: &str = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n";

    fn query(before: Option<String>) -> Query {
        Query {
            session: "fixture".into(),
            before,
            limit: 3,
            tools: true,
            full: false,
        }
    }

    #[test]
    fn codex_batched_pages_match_whole_history_and_warm_cursor_lookup() {
        use crate::agent_execution::transcript::cursor;
        use std::io::Write as _;

        let root = tempfile::tempdir().unwrap();
        let (source, path) = codex_source(root.path());
        let mut file = std::fs::File::create(&path).unwrap();
        let payload = "x".repeat(BATCH_BYTES as usize / 3);
        for turn in 0..24 {
            file.write_all(TURN.as_bytes()).unwrap();
            // The newer copy of this ID excludes the old one even when its
            // batch is skipped during a warm lookup.
            let id = if turn == 3 || turn == 12 {
                "duplicate".into()
            } else {
                format!("turn-{turn}")
            };
            file.write_all(saved_message(&id, &format!("{turn} {payload}")).as_bytes())
                .unwrap();
        }
        drop(file);
        let rollouts = Rollouts::default();
        let whole = Arc::new(Rollout::open(&path).unwrap().read().0);
        for before in [
            None,
            Some(cursor("turn-4")),
            Some("turn-4".into()),
            Some("missing".into()),
        ] {
            let query = query(before);
            let expected = page(&mut Whole(whole.clone()), &query, |_| None);
            let actual = || {
                source
                    .page(&rollouts, &query, |_| None)
                    .map(|(_, entries)| entries)
            };
            assert_eq!(actual(), expected);
            assert_eq!(actual(), expected);
        }
    }

    #[test]
    fn codex_batch_index_follows_appends_and_in_place_rewrites() {
        use std::{fs, io::Write as _};

        let root = tempfile::tempdir().unwrap();
        let (source, path) = codex_source(root.path());
        let old = saved_message("old", &"x".repeat(BATCH_BYTES as usize));
        let complete = saved_message("new", "finished");
        let split = complete.len() / 2;
        fs::write(&path, format!("{TURN}{old}{TURN}{}", &complete[..split])).unwrap();
        let rollouts = Rollouts::default();
        let query = query(None);
        let latest = || source.page(&rollouts, &query, |_| None).unwrap().1.0;
        assert_eq!(latest()[0].id, "old");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&complete.as_bytes()[split..])
            .unwrap();
        assert_eq!(latest()[1].id, "new");

        // Growing in place can also rewrite previous records and introduce
        // new turn boundaries. No prefix inferred from length may survive.
        let corrected = saved_message("corrected", &"y".repeat(BATCH_BYTES as usize + 500));
        fs::write(&path, format!("{TURN}{corrected}{TURN}{complete}")).unwrap();
        let entries = latest();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "corrected");
        assert_eq!(entries[1].id, "new");

        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let edited = corrected.replace("corrected", "different");
        assert_eq!(corrected.len(), edited.len());
        fs::write(&path, format!("{TURN}{edited}{TURN}{complete}")).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(latest()[0].id, "different");
    }

    #[test]
    fn codex_archived_rewrite_during_page_resolves_the_session_again() {
        use std::{cell::Cell, fs};

        let root = tempfile::tempdir().unwrap();
        let (source, path) = codex_source(root.path());
        let older = saved_message("older", &"x".repeat(BATCH_BYTES as usize));
        let newer = saved_message("newer", "present at open");
        fs::write(&path, format!("{TURN}{older}{TURN}{newer}")).unwrap();

        let archive = root.path().join("archived_sessions");
        fs::create_dir(&archive).unwrap();
        let archived = archive.join(path.file_name().unwrap());
        let changed = Cell::new(false);
        let rollouts = Rollouts::default();
        let (_, (entries, _)) = source
            .page(&rollouts, &query(None), |_| {
                if !changed.replace(true) {
                    let replacement = saved_message("replacement", "present after rewrite");
                    fs::rename(&path, &archived).unwrap();
                    fs::write(&archived, replacement).unwrap();
                }
                None
            })
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["replacement"]
        );
    }

    #[test]
    fn codex_caches_a_large_turn_when_its_projected_payload_is_small() {
        let root = tempfile::tempdir().unwrap();
        let (_source, path) = codex_source(root.path());
        let records = [
            serde_json::json!({"type":"response_item","payload":{
                "type":"function_call","call_id":"large","name":"exec_command","arguments":"run"
            }}),
            serde_json::json!({"type":"response_item","payload":{
                "type":"function_call_output","call_id":"large","output":"x".repeat(BATCH_BYTES as usize * 2)
            }}),
        ];
        let transcript = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, transcript).unwrap();
        let rollouts = Rollouts::default();
        let read = || {
            rollouts
                .open(Rollout::open(&path).unwrap())
                .unwrap()
                .newest()
                .unwrap()
                .0
        };
        let first = read();
        assert_eq!(first.len(), 1);
        assert!(Arc::ptr_eq(&first, &read()));
    }

    fn open_codex(source: &Source, rollouts: &Rollouts) -> (Option<u64>, Box<dyn History>) {
        let rollout = Rollout::open(&source.path().unwrap()).unwrap();
        (
            modified(rollout.metadata()),
            rollouts.open(rollout).unwrap(),
        )
    }

    fn member(id: &str, lineage: &str, relation: Relation, exited: bool) -> Member {
        let session = Entry {
            id: id.parse().unwrap(),
            surface: Surface::Chat,
            provider: Some(Provider::Claude),
            title: None,
            channel: None,
            view_scope: None,
            channel_repository: None,
            lineage_channel: None,
            state: crate::agent::ActivityState::Idle,
            directory: None,
            worktree: None,
            project: None,
            project_name: None,
            delegation: None,
            relation,
            lineage: Lineage(lineage.parse().unwrap()),
        };
        Member {
            session,
            exited,
            failure: None,
            active_at: None,
            limit: None,
            account: None,
            cache: None,
            context: None,
        }
    }

    const ORIGINAL: &str = "11111111-0000-4000-8000-000000000001";
    const RELAUNCH: &str = "22222222-0000-4000-8000-000000000002";
    const OTHER: &str = "33333333-0000-4000-8000-000000000003";

    fn listing(sessions: Vec<Member>) -> directory::Listing {
        directory::Listing {
            caller: None,
            sessions,
            served: directory::Wanted::Scope(Scope::All),
            saved: Default::default(),
        }
    }

    #[test]
    fn handles_name_one_session_and_a_relaunch_supersedes_its_exited_original() {
        let relaunched = listing(vec![
            member(ORIGINAL, ORIGINAL, Relation::Parent, true),
            member(RELAUNCH, ORIGINAL, Relation::Parent, false),
            member(OTHER, OTHER, Relation::Peer, false),
        ]);
        let id = |handle| {
            resolve(&relaunched, &HashMap::new(), handle)
                .map(|member| member.session.id.to_string())
        };
        assert_eq!(id("parent").unwrap(), RELAUNCH);
        assert_eq!(id("2222").unwrap(), RELAUNCH);
        assert_eq!(id("3333").unwrap(), OTHER);
        // A session ID names that exact session, even an exited one.
        assert_eq!(id("1111").unwrap(), ORIGINAL);
        assert!(id("111").unwrap_err().contains("at least 4"));
        assert!(id("4444").unwrap_err().contains("cagents ls --channel"));

        let exited_only = listing(vec![member(ORIGINAL, ORIGINAL, Relation::Parent, true)]);
        assert_eq!(
            resolve(&exited_only, &HashMap::new(), "parent")
                .unwrap()
                .session
                .id
                .to_string(),
            ORIGINAL,
            "an exited parent is still readable"
        );
        let twins = listing(vec![
            member(ORIGINAL, OTHER, Relation::Peer, false),
            member(RELAUNCH, OTHER, Relation::Peer, false),
        ]);
        assert!(
            resolve(&twins, &HashMap::new(), "3333")
                .unwrap_err()
                .contains("2 sessions match")
        );
    }

    #[test]
    fn codex_running_command_remains_after_an_earlier_command_completes() {
        let home = tempfile::tempdir().unwrap();
        let id = "01a1b0ba-e200-7000-8000-000000000002";
        let day = home.path().join("codex/sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        let records = [
            serde_json::json!({"type":"event_msg","payload":{"type":"task_started"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-first","name":"exec_command","arguments":"{\"cmd\":\"git status --short\"}"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"call-first","command":"git status --short","status":"completed","exit_code":0,"aggregated_output":""}}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-second","name":"exec_command","arguments":"{\"cmd\":\"./migrate-production\"}"}}),
        ];
        std::fs::write(
            day.join(format!("rollout-2026-11-06T12-00-00-{id}.jsonl")),
            records
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let source = Source {
            launch: Launch {
                provider: Provider::Codex,
                session_id: Some(id.into()),
                recipe: crate::model::LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: home.path().into(),
                    environment: [
                        ("HOME".to_owned(), home.path().display().to_string()),
                        ("CODEX_HOME".to_owned(), "codex".to_owned()),
                    ]
                    .into(),
                    unset_environment: Default::default(),
                },
                draft_model: None,
            },
        };
        let rollouts = Rollouts::default();
        for tools in [true, false] {
            let (_, mut history) = open_codex(&source, &rollouts);
            let (items, earlier) = history.newest().unwrap();
            assert!(!earlier);
            let query = Query {
                session: id.into(),
                before: None,
                limit: 10,
                tools,
                full: false,
            };
            let mut builder = Builder::new(&query);
            for item in items.iter().rev() {
                assert!(builder.push(item));
            }
            let (entries, earlier) = builder.finish(true).unwrap();
            assert_eq!(earlier, None);
            if tools {
                assert!(
                    entries.iter().any(|entry| matches!(
                        &entry.content,
                        Content::Tool(tool) if tool.summary.contains("./migrate-production")
                    )),
                    "active command is missing: {entries:#?}"
                );
                assert_eq!(entries.len(), 2, "the completed call is listed only once");
            } else {
                assert!(
                    matches!(
                        entries.as_slice(),
                        [entry] if matches!(
                            &entry.content,
                            Content::Tools(run) if run.calls.iter().map(|call| call.count).sum::<u32>() == 2
                        )
                    ),
                    "both calls must count in the compact transcript: {entries:#?}"
                );
            }
        }
    }

    #[test]
    fn codex_history_is_read_from_the_sessions_own_rollout() {
        let home = tempfile::tempdir().unwrap();
        let id = "01a1b0ba-e200-7000-8000-000000000002";
        let day = home.path().join("codex/sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        let records = [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Ship it"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"c1","command":"git push","status":"completed","exit_code":0,"aggregated_output":""}}}"#,
            r#"{"type":"response_item","payload":{"type":"message","id":"m1","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Pushed."}]}}"#,
        ];
        std::fs::write(
            day.join(format!("rollout-2026-11-06T12-00-00-{id}.jsonl")),
            records.join("\n"),
        )
        .unwrap();
        let source = Source {
            launch: Launch {
                provider: Provider::Codex,
                session_id: Some(id.into()),
                recipe: crate::model::LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: home.path().into(),
                    environment: [
                        ("HOME".to_owned(), home.path().display().to_string()),
                        ("CODEX_HOME".to_owned(), "codex".to_owned()),
                    ]
                    .into(),
                    unset_environment: Default::default(),
                },
                draft_model: None,
            },
        };
        let rollouts = Rollouts::default();
        let (modified, mut history) = open_codex(&source, &rollouts);
        assert!(modified.is_some());
        assert_eq!(modified, source.modified());
        let (items, earlier) = history.newest().unwrap();
        assert!(!earlier);
        let query = Query {
            session: id.into(),
            before: None,
            limit: 10,
            tools: false,
            full: false,
        };
        let mut builder = Builder::new(&query);
        for item in items.iter().rev() {
            assert!(builder.push(item));
        }
        let (entries, earlier) = builder.finish(true).unwrap();
        assert_eq!(earlier, None);
        assert!(matches!(&entries[0].content, Content::User(text) if text.text == "Ship it"));
        assert!(matches!(&entries[1].content, Content::Tools(run) if run.calls[0].name == "Run"));
        assert!(
            matches!(&entries[2].content, Content::Assistant { text, .. } if text.text == "Pushed.")
        );

        let mut moved = source;
        moved.launch.session_id = Some("01a1b0ba-e200-7000-8000-000000000009".into());
        assert!(moved.path().unwrap_err().contains("missing"));
    }

    #[test]
    fn codex_snapshot_reuses_unchanged_bytes_and_follows_file_changes() {
        use std::{fs, io::Write as _};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rollout.jsonl");
        let record = |text| {
            format!(
                "{}\n",
                serde_json::json!({"type":"response_item","payload":{
                    "type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":text}]
                }})
            )
        };
        let rollouts = Rollouts::default();
        let read = || {
            rollouts
                .open(Rollout::open(&path).unwrap())
                .unwrap()
                .newest()
                .unwrap()
                .0
        };
        fs::write(&path, record("first")).unwrap();
        let first = read();
        assert!(Arc::ptr_eq(&first, &read()));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(record("added").as_bytes())
            .unwrap();
        let appended = read();
        assert_eq!(appended.len(), 2);
        assert_eq!(appended[0], first[0]);
        assert!(Arc::ptr_eq(&appended, &read()));

        // The old descriptor has a longer opening length than it can read.
        // A short read cannot populate the old revision's cache entry.
        let opened = Rollout::open(&path).unwrap();
        fs::write(&path, record("short")).unwrap();
        assert!(opened.read().1.is_none());
        let truncated = read();
        assert_eq!(truncated.len(), 1);
        assert_ne!(truncated[0], first[0]);

        let replacement = root.path().join("replacement");
        fs::write(&replacement, record("other")).unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_modified(fs::metadata(&path).unwrap().modified().unwrap())
            .unwrap();
        fs::rename(replacement, &path).unwrap();
        let replaced = read();
        assert_ne!(replaced[0], truncated[0]);
        assert!(Arc::ptr_eq(&replaced, &read()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "opt-in transcript measurement"]
    fn measure_rollout_page() {
        use std::{fs, io::Write as _, path::Path, time::Instant};

        fn metric(path: &str, key: &str) -> u64 {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        }

        let root = std::env::var("CAGENTS_TRANSCRIPT_FIXTURE").unwrap();
        let root = Path::new(&root);
        let dir = tempfile::tempdir().unwrap();
        let (source, path) = codex_source(dir.path());
        fs::copy(root.join("rollout.jsonl"), &path).unwrap();
        crate::instance::install_tracing("transcript benchmark", "off");
        let scenario = std::env::var("CAGENTS_TRANSCRIPT_SCENARIO").unwrap();
        let mut query = query(None);
        query.limit = 20;
        let rollouts = Rollouts::default();
        let read = |query: &Query| source.page(&rollouts, query, |_| None).unwrap().1;
        match scenario.as_str() {
            "append" => {
                read(&query);
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(&fs::read(root.join("append.jsonl")).unwrap())
                    .unwrap();
            }
            "deep" | "warm-deep" => {
                query.before = Some(crate::agent_execution::transcript::cursor("reply-60"));
                if scenario == "warm-deep" {
                    read(&query);
                }
            }
            "latest" => {}
            _ => panic!("unknown transcript benchmark scenario"),
        }
        if crate::diagnostics::local_debug() {
            eprintln!("measuring {scenario}");
        }
        let io_before = metric("/proc/self/io", "rchar:");
        let start = Instant::now();
        let result = read(&query);
        let took = start.elapsed();
        let read_bytes = metric("/proc/self/io", "rchar:") - io_before;
        let rss_kib = metric("/proc/self/status", "VmHWM:");
        use sha2::{Digest as _, Sha256};
        let page_hash = Sha256::digest(serde_json::to_vec(&result).unwrap());
        println!(
            "{scenario}: {took:?}, rchar delta {read_bytes}, peak RSS {rss_kib} KiB, page SHA-256 {page_hash:x}"
        );
    }
}
