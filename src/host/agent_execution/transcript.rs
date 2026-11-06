//! Reads a page of a session's conversation from its provider's own saved
//! history, whether or not the provider is still running.
//!
//! The history is located from the session's launch alone: its provider,
//! conversation ID, working directory and account home. A caller names a
//! session, never a file. Reading confers no control, like listing.

use super::{SharedHost, directory};
use crate::{
    agent::Provider,
    agent_execution::{
        Authority,
        directory::{Entry, Relation, Scope},
        transcript::{Builder, MAX_LIMIT, Page, Query},
    },
    chat::{self, Item, Launch},
    model::TerminalId,
};
use std::path::PathBuf;

/// The shortest handle accepted, as for messages.
const MIN_HANDLE: usize = 4;
/// Provider history pages read for one reply before it stops with a cursor.
const MAX_PAGES: usize = 4_096;

pub(super) fn read(host: &SharedHost, authority: &Authority, query: Query) -> Result<Page, String> {
    if query.limit == 0 || query.limit > MAX_LIMIT {
        return Err(format!("A page holds between 1 and {MAX_LIMIT} messages"));
    }
    let listing = directory::read_including(host, authority, Scope::All, true)?;
    let session = resolve(&listing, &query.session)?;
    let source = source(host, session.id, true)?;
    let recognizer = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox_seal
        .as_ref()
        .map(|seal| seal.recognizer());
    let mut history = source.open()?;
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
        for mut item in items.into_iter().rev() {
            // Neighboring provider pages can repeat an item at their seam.
            if !seen.insert(item.id.clone()) {
                continue;
            }
            if let Some(recognizer) = &recognizer {
                recognizer.recognize(&mut item);
            }
            if !builder.push(&item) {
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
    if pages == MAX_PAGES && !builder.found_cursor() {
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
/// `parent`, or a prefix of a session ID, lineage or delegated agent ID.
fn resolve(
    listing: &crate::agent_execution::directory::Directory,
    handle: &str,
) -> Result<Entry, String> {
    let entries = || listing.caller.iter().chain(&listing.sessions);
    if handle == "parent" {
        let mut parents = entries().filter(|entry| entry.relation == Relation::Parent);
        return match (parents.next(), parents.next()) {
            (Some(parent), None) => Ok(parent.clone()),
            (None, _) => Err("This caller has no live parent session".into()),
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
    let found: Vec<&Entry> = entries()
        .filter(|entry| entry.provider.is_some())
        .filter(|entry| {
            matches(&entry.id)
                || matches(&entry.lineage)
                || entry
                    .delegation
                    .as_ref()
                    .is_some_and(|delegation| matches(&delegation.agent))
        })
        .collect();
    // A relaunched session shares its lineage with the exited one it
    // replaced; a session ID names one exactly, otherwise the live one wins.
    let by_id: Vec<&Entry> = found
        .iter()
        .copied()
        .filter(|entry| matches(&entry.id))
        .collect();
    let live: Vec<&Entry> = found
        .iter()
        .copied()
        .filter(|entry| !entry.exited)
        .collect();
    match (found.as_slice(), by_id.as_slice(), live.as_slice()) {
        ([only], _, _) | (_, [only], _) => Ok((*only).clone()),
        (many, _, [only]) if many.iter().all(|entry| entry.lineage == only.lineage) => {
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
/// PTY owner; otherwise its last recorded resume hint is used.
pub(super) fn source(host: &SharedHost, id: TerminalId, observe: bool) -> Result<Source, String> {
    enum Found {
        Chat(Launch, Option<String>),
        Terminal(crate::terminal::TerminalHandle, Option<String>),
    }
    let found = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = state.chats.get(&id) {
            Found::Chat(
                record.session.launch.clone(),
                record.handle.confirmed_session_id(),
            )
        } else if let Some(record) = state.terminals.get(&id) {
            Found::Terminal(record.handle.clone(), record.session.resume_hint.clone())
        } else {
            return Err("The session is no longer hosted".into());
        }
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
            Provider::Codex => crate::agent::codex_rollout(&root, id),
            Provider::Omp => Err("Reading an Omp conversation is not supported yet".into()),
        }
    }

    /// When the provider last saved this conversation, in Unix milliseconds.
    pub(super) fn modified(&self) -> Option<u64> {
        let modified = std::fs::metadata(self.path().ok()?).ok()?.modified().ok()?;
        u64::try_from(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis(),
        )
        .ok()
    }

    fn open(&self) -> Result<Box<dyn History>, String> {
        match self.launch.provider {
            Provider::Claude => Claude::open(&self.launch).map(|history| Box::new(history) as _),
            Provider::Codex => Ok(Box::new(Whole(Some(chat::protocol::codex::rollout::read(
                &self.path()?,
            )?)))),
            Provider::Omp => Err("Reading an Omp conversation is not supported yet".into()),
        }
    }
}

/// Saved history read newest page first. Each page is oldest first and says
/// whether earlier pages remain.
trait History {
    fn newest(&mut self) -> Result<(Vec<Item>, bool), String>;
    fn earlier(&mut self) -> Result<(Vec<Item>, bool), String>;
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

    fn page(step: chat::protocol::Step) -> Result<(Vec<Item>, bool), String> {
        if !step.writes.is_empty() {
            return Err("Reading Claude history attempted a provider write".into());
        }
        let mut page = None;
        for event in step.events {
            match event {
                chat::Event::History { items, has_earlier } => page = Some((items, has_earlier)),
                chat::Event::HistoryError(error) => return Err(error),
                _ => {}
            }
        }
        Ok(page.unwrap_or_default())
    }
}

impl History for Claude {
    fn newest(&mut self) -> Result<(Vec<Item>, bool), String> {
        Self::page(self.0.initialize()?)
    }

    fn earlier(&mut self) -> Result<(Vec<Item>, bool), String> {
        Self::page(self.0.command(chat::Command::LoadEarlier)?)
    }
}

/// A history read whole, as Codex rollouts are.
struct Whole(Option<Vec<Item>>);

impl History for Whole {
    fn newest(&mut self) -> Result<(Vec<Item>, bool), String> {
        Ok((self.0.take().unwrap_or_default(), false))
    }

    fn earlier(&mut self) -> Result<(Vec<Item>, bool), String> {
        Ok((Vec::new(), false))
    }
}
