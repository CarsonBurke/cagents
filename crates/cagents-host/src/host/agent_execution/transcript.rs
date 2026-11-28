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
        transcript::{Builder, MAX_LIMIT, Page, Query},
    },
    chat::{self, Item, ItemContent, Launch, protocol::codex::rollout::Rollout},
    model::TerminalId,
};
use std::{
    collections::{HashMap, VecDeque},
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
    let (modified, mut history) = source.open(&rollouts)?;
    session.active_at = modified;
    let mut builder = Builder::new(&query);
    let mut seen = std::collections::HashSet::new();
    let mut pages = 0;
    let mut exhausted = false;
    'pages: loop {
        let (items, earlier) = if pages == 0 {
            history.newest()?
        } else {
            history.earlier()?
        };
        pages += 1;
        for item in items.iter().rev() {
            // Neighboring provider pages can repeat an item at their seam.
            if !seen.insert(item.id.clone()) {
                continue;
            }
            let recognized = recognizer.as_ref().and_then(|seal| seal.recognized(item));
            if !builder.push(recognized.as_ref().unwrap_or(item)) {
                break 'pages;
            }
        }
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
    let (entries, earlier) = builder.finish(exhausted)?;
    Ok(Page {
        session,
        entries,
        earlier,
    })
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

    fn open(&self, rollouts: &Rollouts) -> Result<(Option<u64>, Box<dyn History>), String> {
        match self.launch.provider {
            Provider::Claude => Ok((self.modified(), Box::new(Claude::open(&self.launch)?))),
            Provider::Codex => {
                let rollout = Rollout::open(&self.path()?)?;
                let at = modified(rollout.metadata());
                Ok((at, Box::new(Whole(rollouts.read(rollout)))))
            }
            Provider::Omp => Err("Reading an Omp conversation is not supported yet".into()),
            Provider::Antigravity | Provider::Muse => Err(format!(
                "Reading {} conversations is not supported yet",
                self.launch.provider.label()
            )),
        }
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
    /// Coalesce simultaneous requests for the same version without blocking
    /// reads of other conversations on this file.
    items: Mutex<Option<Arc<Vec<Item>>>>,
}

impl Rollouts {
    fn read(&self, rollout: Rollout) -> Arc<Vec<Item>> {
        const MAX_ENTRIES: usize = 4;
        const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
        // Short records, such as compactions, still allocate a complete Item
        // and its strings. Bound that expansion separately from source bytes.
        const MAX_ROWS_PER_ENTRY: usize = 16_384;

        let Some(revision) = FileRevision::of(rollout.metadata()).filter(|revision| {
            revision.len <= MAX_SOURCE_BYTES && rollout.supports_revision_cache()
        }) else {
            return Arc::new(rollout.read().0);
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
                        items: Mutex::new(None),
                    })
                });
            entries.push_front(entry.clone());
            while entries.len() > MAX_ENTRIES
                || entries.iter().map(|entry| entry.revision.len).sum::<u64>() > MAX_SOURCE_BYTES
            {
                entries.pop_back();
            }
            entry
        };
        let mut cached = entry.items.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(items) = cached.as_ref() {
            return items.clone();
        }
        let (items, closing) = rollout.read();
        let rows = items.iter().fold(items.capacity(), |total, item| {
            total
                + match &item.content {
                    ItemContent::Changes { changes, .. } => changes.capacity(),
                    ItemContent::WebSearch { results, .. } => results.capacity(),
                    _ => 0,
                }
        });
        let items = Arc::new(items);
        if rows <= MAX_ROWS_PER_ENTRY
            && closing.as_ref().and_then(FileRevision::of) == Some(revision)
        {
            *cached = Some(items.clone());
        }
        items
    }
}

type HistoryPage = (Arc<Vec<Item>>, bool);

/// Saved history read newest page first. Each page is oldest first and says
/// whether earlier pages remain.
trait History {
    fn newest(&mut self) -> Result<HistoryPage, String>;
    fn earlier(&mut self) -> Result<HistoryPage, String>;
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
                chat::Event::History { items, has_earlier } => {
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

/// A history read whole, as Codex rollouts are.
struct Whole(Arc<Vec<Item>>);

impl History for Whole {
    fn newest(&mut self) -> Result<HistoryPage, String> {
        Ok((self.0.clone(), false))
    }

    fn earlier(&mut self) -> Result<HistoryPage, String> {
        Ok(HistoryPage::default())
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
            let (_, mut history) = source.open(&rollouts).unwrap();
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
        let (modified, mut history) = source.open(&rollouts).unwrap();
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
        assert!(moved.open(&rollouts).err().unwrap().contains("missing"));
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
        let read = || rollouts.read(Rollout::open(&path).unwrap());
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
}
