//! The pull requests each watched conversation worked on or mentioned, and
//! the worktrees it edited.
//!
//! A window watching a conversation's inspector subscribes to it. This
//! service then reads the provider's own transcript files incrementally,
//! keeps what they point at, and resolves that against what the Work board
//! already knows: the conversation's project, its worktrees and branches, and
//! the pull requests its forge poll found. Only pull requests the board does
//! not hold are looked up, a few per pass, through the same `gh` budget, and
//! what was read last is kept while GitHub cannot be reached.

use super::{
    super::{
        delivery::{DeliveryQueue, Outbound},
        state::{ClientId, SharedHost},
    },
    Handle, Signal, board, forge, now_unix_ms,
    transcript::{self, Format, Reader},
};
use crate::{
    agent::Provider,
    github::{GitHubError, GitHubService},
    github_identity::{GitHubRepoKey, PullRequestKey},
    model::TerminalId,
    pull_request_signals::{PullRequestKeyOrd, Reference, Relation, RepoKeyOrd},
    terminal_ipc::ServerMessage,
    work::{
        Check, CheckState, ConversationPullRequest, ConversationPullRequests, ConversationWorktree,
        DiffStat, PullRequest, TranscriptCoverage,
    },
};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    os::unix::fs::{FileExt as _, MetadataExt as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::{Duration, Instant},
};

#[cfg(test)]
mod tests;

/// How often a watched conversation's transcript is read again.
const TICK: Duration = Duration::from_secs(2);
const UNWATCHED: Duration = Duration::from_secs(60 * 60);
/// How long what was read of an unwatched conversation is kept, so coming
/// back to it, or showing the inspector again, does not read it all again.
const KEPT_UNWATCHED: Duration = Duration::from_secs(10 * 60);
/// The most of one file read in a pass; a larger backlog continues next pass.
const READ_PER_PASS: u64 = 8 * 1024 * 1024;
/// The newest part of a transcript read when it is first watched.
const FIRST_READ: u64 = 64 * 1024 * 1024;
/// Lines longer than this, such as an attached image or a huge tool output,
/// are skipped. They name no pull request worth listing, so skipping one
/// does not make the list partial.
const MAX_LINE: usize = 4 * 1024 * 1024;
/// Distinct references kept per conversation; the oldest mentions go first.
const REFERENCES: usize = 256;
/// Distinct edited directories kept per conversation; the least recently
/// edited go first. Worktrees are resolved from them when a list is built,
/// so a worktree the board scans later is still found.
const EDITED_DIRECTORIES: usize = 256;
/// Subagent transcripts read per conversation, the most recently written
/// first.
const SUBAGENTS: usize = 256;
/// Forge lookups per pass, across every watched conversation.
const LOOKUPS_PER_PASS: usize = 4;
/// Edited worktrees diffed in one pass; the rest wait for the next.
const MEASURES_PER_PASS: usize = 4;
/// Of the mentions, only the most recent are looked up.
const MENTIONS_LOOKED_UP: usize = 24;
const OPEN_FOR: Duration = Duration::from_secs(60);
/// An open pull request only mentioned shows its state, not its checks or
/// review, so it is asked about less often.
const MENTIONED_OPEN_FOR: Duration = Duration::from_secs(10 * 60);
const FINISHED_FOR: Duration = Duration::from_secs(30 * 60);
const NOT_A_PULL_REQUEST_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const BRANCH_FOUND_FOR: Duration = Duration::from_secs(15 * 60);
const BRANCH_MISSING_FOR: Duration = Duration::from_secs(10 * 60);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Conversations {
    state: Mutex<State>,
    pub(super) signal: Signal,
}

#[derive(Default)]
struct State {
    watched: BTreeMap<TerminalId, Watched>,
    /// What GitHub said about pull requests the board does not list, by key.
    observed: BTreeMap<PullRequestKeyOrd, Observed>,
    /// Which pull request heads a branch, by (base repository, branch).
    branches: BTreeMap<(RepoKeyOrd, String), BranchLookup>,
    forge_error: Option<String>,
    /// The line totals of edited worktrees' uncommitted changes, by path,
    /// with the status they were measured at.
    measured: BTreeMap<PathBuf, Measured>,
    /// Lookups that failed in a row, and when the last did; while GitHub
    /// cannot be reached, lookups back off rather than retry every pass.
    failures: u32,
    failed: Option<Instant>,
}

impl State {
    fn lookups_due(&self) -> bool {
        self.failed
            .is_none_or(|failed| failed.elapsed() >= backoff(self.failures))
    }

    fn succeeded(&mut self) {
        self.forge_error = None;
        self.failures = 0;
        self.failed = None;
    }

    fn failed(&mut self, error: &GitHubError) {
        self.forge_error = Some(error.to_string());
        self.failures = self.failures.saturating_add(1);
        self.failed = Some(Instant::now());
    }
}

/// As the board backs off: 30 s doubling, at most 30 min.
fn backoff(failures: u32) -> Duration {
    (Duration::from_secs(30) * 2_u32.saturating_pow(failures.saturating_sub(1).min(6)))
        .min(FINISHED_FOR)
}

#[derive(Debug, Clone, Default)]
struct Observed {
    pull_request: Option<PullRequest>,
    /// What it changes and its head's checks, as last read with it.
    detail: Detail,
    observed_at_unix_ms: Option<u64>,
    checked: Option<Instant>,
    /// GitHub says the number is an issue, or there is no such pull request.
    missing: bool,
    failures: u32,
}

impl Observed {
    fn due(&self, worked_on: bool) -> bool {
        let Some(checked) = self.checked else {
            return true;
        };
        let wait =
            if self.missing {
                NOT_A_PULL_REQUEST_FOR
            } else if self.failures > 0 {
                backoff(self.failures)
            } else if self.pull_request.as_ref().is_some_and(|pull_request| {
                pull_request.state != crate::work::PullRequestState::Open
            }) {
                FINISHED_FOR
            } else if worked_on {
                OPEN_FOR
            } else {
                MENTIONED_OPEN_FOR
            };
        checked.elapsed() >= wait
    }
}

#[derive(Debug, Clone)]
struct BranchLookup {
    number: Option<u64>,
    checked: Instant,
}

struct Watched {
    watchers: BTreeMap<ClientId, Weak<DeliveryQueue>>,
    source: Source,
    files: BTreeMap<PathBuf, Cursor>,
    references: BTreeMap<Reference, Aggregate>,
    /// Absolute directories files were edited in, with when they last were
    /// and whether only subagents edited there.
    edited: BTreeMap<PathBuf, Edit>,
    sequence: u64,
    coverage: TranscriptCoverage,
    revision: u64,
    sent: Option<ConversationPullRequests>,
    unwatched_since: Option<Instant>,
    /// The provider's conversation ID `source` was located for, so finding
    /// the transcript, a search for Codex, happens once per conversation ID.
    located_id: Option<String>,
    /// Searching for a Codex rollout walks its dated directories, so a
    /// search that found nothing is not repeated before this.
    search_after: Option<Instant>,
    searches_missed: u32,
}

/// Where a pass found the conversation's transcript.
enum Location {
    /// As before, or not looked for this pass.
    Unchanged,
    Found(String, Source),
    /// A search found no transcript for the conversation's ID yet.
    Missed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// Not located yet: the conversation may not have reported its ID.
    Pending,
    Located(Located),
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Located {
    format: Format,
    directory: PathBuf,
    transcript: PathBuf,
    /// Where Claude Code keeps this conversation's subagent transcripts.
    subagents: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct Edit {
    by_subagent: bool,
    seen: u64,
}

#[derive(Debug, Clone, Copy)]
struct Aggregate {
    relation: Relation,
    /// Every signal came from a subagent.
    by_subagent: bool,
    seen: u64,
}

struct Cursor {
    identity: (u64, u64),
    offset: u64,
    partial: Vec<u8>,
    /// Skipping the rest of a line that started before the first read, or
    /// that grew longer than a line may be.
    skipping: bool,
    reader: Reader,
    truncated: bool,
}

impl Watched {
    fn new() -> Self {
        Self {
            watchers: BTreeMap::new(),
            source: Source::Pending,
            files: BTreeMap::new(),
            references: BTreeMap::new(),
            edited: BTreeMap::new(),
            sequence: 0,
            coverage: TranscriptCoverage::Pending,
            revision: 0,
            sent: None,
            unwatched_since: None,
            located_id: None,
            search_after: None,
            searches_missed: 0,
        }
    }

    /// Records what a read found. A command run on "the current branch" is
    /// pinned to the branch checked out now, so a later switch does not move
    /// it to another branch's pull request.
    fn apply(&mut self, scanned: Scanned, facts: Option<&Facts>) {
        for mut found in scanned.found {
            let Some(facts) = facts else {
                self.record(found);
                continue;
            };
            let Reference::CurrentBranch { directory } = &found.signal.reference else {
                self.record(found);
                continue;
            };
            let directory = match directory {
                Some(directory) => normalize(&facts.directory.join(directory)),
                None => facts.directory.clone(),
            };
            if found.signal.relation == Relation::Edited {
                self.edited_in(directory.clone(), found.by_subagent);
            }
            if let Some(branch) = facts
                .project
                .as_ref()
                .and_then(|project| project.worktree(&directory)?.branch.clone())
            {
                found.signal.reference = Reference::Branch {
                    repository: None,
                    name: branch,
                };
            }
            self.record(found);
        }
        self.coverage = scanned.coverage;
    }

    fn edited_in(&mut self, directory: PathBuf, by_subagent: bool) {
        self.sequence += 1;
        let seen = self.sequence;
        let entry = self
            .edited
            .entry(directory)
            .or_insert(Edit { by_subagent, seen });
        entry.by_subagent &= by_subagent;
        entry.seen = seen;
        if self.edited.len() > EDITED_DIRECTORIES {
            let oldest = self
                .edited
                .iter()
                .min_by_key(|(_, edit)| edit.seen)
                .map(|(directory, _)| directory.clone());
            if let Some(oldest) = oldest {
                self.edited.remove(&oldest);
            }
        }
    }

    fn record(&mut self, found: transcript::Found) {
        self.sequence += 1;
        let sequence = self.sequence;
        let entry = self
            .references
            .entry(found.signal.reference)
            .or_insert(Aggregate {
                relation: found.signal.relation,
                by_subagent: found.by_subagent,
                seen: sequence,
            });
        entry.relation = entry.relation.min(found.signal.relation);
        entry.by_subagent &= found.by_subagent;
        entry.seen = sequence;
        if self.references.len() > REFERENCES {
            // The oldest mention goes, then the oldest of anything.
            let oldest = self
                .references
                .iter()
                .min_by_key(|(_, aggregate)| (aggregate.relation.worked_on(), aggregate.seen))
                .map(|(reference, _)| reference.clone());
            if let Some(oldest) = oldest {
                self.references.remove(&oldest);
            }
        }
    }
}

/// What resolving a conversation's references needs from the board, copied
/// out so neither lock is held while reading files or asking GitHub.
struct Facts {
    directory: PathBuf,
    project: Option<ProjectFacts>,
}

#[derive(Clone)]
struct ProjectFacts {
    snapshot: Option<Arc<forge::Snapshot>>,
    local: Option<Arc<super::scan::Local>>,
    forge_error: Option<String>,
    /// When the board last listed the project's pull requests.
    synced_at_unix_ms: Option<u64>,
}

impl ProjectFacts {
    fn repository(&self) -> Option<&GitHubRepoKey> {
        self.snapshot
            .as_deref()
            .map(|snapshot| &snapshot.repository)
    }

    /// Whether one of the project's worktrees has `branch` checked out.
    fn has_branch(&self, branch: &str) -> bool {
        self.local.as_deref().is_some_and(|local| {
            local
                .worktrees
                .iter()
                .any(|worktree| worktree.branch.as_deref() == Some(branch))
        })
    }

    /// The worktree containing `directory`, the deepest when they nest.
    fn worktree(&self, directory: &Path) -> Option<&super::scan::Worktree> {
        self.local
            .as_deref()?
            .worktrees
            .iter()
            .filter(|worktree| directory.starts_with(&worktree.path))
            .max_by_key(|worktree| worktree.path.components().count())
    }

    fn listed(&self, key: &PullRequestKeyOrd) -> Option<&crate::github::PullRequest> {
        let snapshot = self.snapshot.as_deref()?;
        if RepoKeyOrd::from(&snapshot.repository) != key.repository {
            return None;
        }
        snapshot
            .pull_requests
            .iter()
            .chain(snapshot.others.values())
            .find(|pull_request| pull_request.number == key.number)
    }
}

/// A reference turned into a pull request, or what must be asked to do so.
enum Resolved {
    Key(PullRequestKeyOrd),
    /// The pull request heading `branch` is not known yet.
    Branch {
        base: GitHubRepoKey,
        head: GitHubRepoKey,
        branch: String,
    },
    Nothing,
}

impl Handle {
    fn conversations(&self) -> MutexGuard<'_, State> {
        self.0
            .conversations
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Subscribes `client` to one conversation's pull requests and returns
    /// them as of the revision it starts from.
    pub(in crate::host) fn watch_conversation(
        &self,
        host: &SharedHost,
        client: ClientId,
        conversation: TerminalId,
        delivery: Weak<DeliveryQueue>,
    ) -> Result<(u64, ConversationPullRequests), String> {
        if !host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&conversation)
        {
            return Err("No such conversation".into());
        }
        let facts = self.facts(host, conversation);
        let mut state = self.conversations();
        // A connection watches one conversation at a time.
        for watched in state.watched.values_mut() {
            watched.watchers.remove(&client);
        }
        let forge_error = state.forge_error.clone();
        let observed = state.observed.clone();
        let branches = state.branches.clone();
        let measured = state.measured.clone();
        let watched = state
            .watched
            .entry(conversation)
            .or_insert_with(Watched::new);
        watched.watchers.insert(client, delivery);
        watched.unwatched_since = None;
        let mut list = assemble(watched, facts.as_ref(), &observed, &branches, forge_error);
        with_measured(&mut list, &measured);
        let mut comparable = list.clone();
        comparable.generated_at_unix_ms = 0;
        if watched.sent.as_ref() != Some(&comparable) {
            watched.sent = Some(comparable);
            watched.revision += 1;
        }
        let revision = watched.revision;
        drop(state);
        self.0.conversations.signal.notify();
        Ok((revision, list))
    }

    pub(in crate::host) fn unwatch_conversation(&self, client: ClientId) {
        let mut state = self.conversations();
        for watched in state.watched.values_mut() {
            watched.watchers.remove(&client);
        }
    }

    pub(super) fn conversation_loop(&self) {
        while let Some(observed) = self.0.conversations.signal.observe() {
            let Some(host) = self.0.host.upgrade() else {
                break;
            };
            let watched = super::super::service::attempt("Conversation pull requests", || {
                Ok(self.conversation_pass(&host))
            })
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "conversation pull requests are temporarily unavailable");
                true
            });
            drop(host);
            self.0
                .conversations
                .signal
                .wait(observed, if watched { TICK } else { UNWATCHED });
        }
    }

    /// Reads, resolves and publishes every watched conversation once, and
    /// reports whether any is watched.
    fn conversation_pass(&self, host: &SharedHost) -> bool {
        let conversations = {
            let mut state = self.conversations();
            state.watched.retain(|_, watched| {
                watched
                    .watchers
                    .retain(|_, delivery| delivery.strong_count() > 0);
                if watched.watchers.is_empty() {
                    let since = *watched.unwatched_since.get_or_insert_with(Instant::now);
                    since.elapsed() < KEPT_UNWATCHED
                } else {
                    watched.unwatched_since = None;
                    true
                }
            });
            state
                .watched
                .iter()
                .filter(|(_, watched)| !watched.watchers.is_empty())
                .map(|(conversation, _)| *conversation)
                .collect::<Vec<_>>()
        };
        if conversations.is_empty() {
            return false;
        }
        let mut facts = BTreeMap::new();
        for conversation in &conversations {
            let (known, search_due) = self
                .conversations()
                .watched
                .get(conversation)
                .map(|watched| {
                    (
                        watched.located_id.clone(),
                        watched
                            .search_after
                            .is_none_or(|after| Instant::now() >= after),
                    )
                })
                .unwrap_or_default();
            let located = locate(host, *conversation, known.as_deref(), search_due);
            facts.insert(*conversation, self.facts(host, *conversation));
            let (source, mut cursors) = {
                let mut state = self.conversations();
                let Some(watched) = state.watched.get_mut(conversation) else {
                    continue;
                };
                if matches!(located, Location::Missed) {
                    watched.searches_missed = watched.searches_missed.saturating_add(1);
                    watched.search_after =
                        Some(Instant::now() + search_backoff(watched.searches_missed));
                }
                if let Location::Found(id, source) = located {
                    watched.searches_missed = 0;
                    watched.search_after = None;
                    watched.located_id = Some(id);
                    if watched.source != source {
                        // A new conversation ID, after `/clear` or a resume,
                        // is a new transcript.
                        watched.source = source;
                        watched.files.clear();
                        watched.references.clear();
                        watched.edited.clear();
                    }
                }
                (watched.source.clone(), std::mem::take(&mut watched.files))
            };
            // Watching and unwatching take the same lock, so the files are
            // read without it. Only this thread moves the cursors.
            let scanned = scan(&source, &mut cursors);
            let mut state = self.conversations();
            if let Some(watched) = state.watched.get_mut(conversation)
                && watched.source == source
            {
                watched.files = cursors;
                watched.apply(scanned, facts.get(conversation).and_then(Option::as_ref));
            }
        }
        self.look_up(&conversations, &facts);
        self.measure(&conversations, &facts);
        let mut state = self.conversations();
        let observed = state.observed.clone();
        let branches = state.branches.clone();
        let measured = state.measured.clone();
        let forge_error = state.forge_error.clone();
        let proof = Arc::new(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone(),
        );
        for conversation in conversations {
            let Some(watched) = state.watched.get_mut(&conversation) else {
                continue;
            };
            let facts = facts.get(&conversation).and_then(Option::as_ref);
            let mut list = assemble(watched, facts, &observed, &branches, forge_error.clone());
            with_measured(&mut list, &measured);
            let mut comparable = list.clone();
            comparable.generated_at_unix_ms = 0;
            if watched.sent.as_ref() == Some(&comparable) {
                continue;
            }
            watched.sent = Some(comparable);
            watched.revision += 1;
            let revision = watched.revision;
            watched.watchers.retain(|_, delivery| {
                let Some(delivery) = delivery.upgrade() else {
                    return false;
                };
                delivery
                    .push(Outbound::broadcast(
                        &proof,
                        ServerMessage::ConversationPullRequests {
                            conversation,
                            revision,
                            pull_requests: Box::new(list.clone()),
                        },
                    ))
                    .is_ok()
            });
        }
        true
    }

    /// The board's facts for a conversation's directory. Watching a
    /// conversation counts as reading the board, which tracks its project
    /// and keeps its forge poll at the watched cadence.
    fn facts(&self, host: &SharedHost, conversation: TerminalId) -> Option<Facts> {
        let directory = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&conversation)?
            .session
            .launch
            .recipe
            .working_directory
            .clone();
        let directory = std::fs::canonicalize(&directory).unwrap_or(directory);
        let selected = self
            .resolve(host, &crate::work::Scope::Project(directory.clone()))
            .ok();
        let mut shared = self.shared();
        shared.read_at = Some(Instant::now());
        let project = selected.and_then(|selected| {
            let (repository, name) = selected.into_iter().next()?;
            let new = !shared.projects.contains_key(&repository);
            let tracked = shared.track(&repository, &name);
            let facts = ProjectFacts {
                snapshot: tracked.snapshot.clone(),
                local: tracked.local.clone(),
                forge_error: match &tracked.forge {
                    crate::work::Forge::Unavailable { reason } => Some(reason.clone()),
                    crate::work::Forge::Synced { error, .. } => error.clone(),
                    crate::work::Forge::Pending | crate::work::Forge::NotGitHub => None,
                },
                synced_at_unix_ms: match &tracked.forge {
                    crate::work::Forge::Synced { at_unix_ms, .. } => Some(*at_unix_ms),
                    _ => None,
                },
            };
            if new {
                self.0.scanner.notify();
            }
            Some(facts)
        });
        Some(Facts { directory, project })
    }

    /// Measures the uncommitted lines of each worktree a watched
    /// conversation edited whose status changed since it was last measured.
    /// The board's scan, which its file watcher drives, says when one did, so
    /// an unchanged worktree is never diffed again.
    fn measure(&self, conversations: &[TerminalId], facts: &BTreeMap<TerminalId, Option<Facts>>) {
        let due = {
            let mut state = self.conversations();
            let mut wanted = BTreeMap::new();
            for conversation in conversations {
                let (Some(watched), Some(Some(facts))) =
                    (state.watched.get(conversation), facts.get(conversation))
                else {
                    continue;
                };
                let Some(project) = &facts.project else {
                    continue;
                };
                for (worktree, _) in edited(watched, project) {
                    wanted.insert(worktree.path.clone(), Fingerprint::of(worktree));
                }
            }
            state.measured.retain(|path, _| wanted.contains_key(path));
            wanted
                .into_iter()
                .filter(|(path, fingerprint)| {
                    state
                        .measured
                        .get(path)
                        .is_none_or(|measured| measured.fingerprint != *fingerprint)
                })
                .collect::<Vec<_>>()
        };
        let git = crate::repository::RepositoryService::system("git");
        for (path, fingerprint) in due.into_iter().take(MEASURES_PER_PASS) {
            let diff = match &fingerprint.dirty {
                // Until its status is read there is nothing to say.
                None => None,
                Some(dirty) if dirty.paths == 0 => Some(DiffStat::default()),
                Some(_) => git
                    .uncommitted_line_totals(&path, &self.0.cancellation)
                    .ok()
                    .map(|(additions, deletions)| DiffStat {
                        additions,
                        deletions,
                    }),
            };
            self.conversations()
                .measured
                .insert(path, Measured { fingerprint, diff });
        }
    }

    /// Asks GitHub about the few pull requests and branches most worth it.
    fn look_up(&self, conversations: &[TerminalId], facts: &BTreeMap<TerminalId, Option<Facts>>) {
        // In rank order across conversations, so the budget goes to what
        // they worked on before what they mentioned.
        let lookups = {
            let state = self.conversations();
            if !state.lookups_due() {
                return;
            }
            let mut lookups: Vec<(Relation, Lookup)> = Vec::new();
            for conversation in conversations {
                let (Some(watched), Some(Some(facts))) =
                    (state.watched.get(conversation), facts.get(conversation))
                else {
                    continue;
                };
                let mut mentions = 0;
                for (reference, aggregate) in ranked(watched) {
                    let worked_on = aggregate.relation.worked_on();
                    if !worked_on {
                        mentions += 1;
                        if mentions > MENTIONS_LOOKED_UP {
                            break;
                        }
                    }
                    let lookup = match resolve(reference, facts, &state.branches) {
                        Resolved::Key(key) => {
                            let listed = facts
                                .project
                                .as_ref()
                                .is_some_and(|project| project.listed(&key).is_some());
                            if listed
                                || !state
                                    .observed
                                    .get(&key)
                                    .is_none_or(|observed| observed.due(worked_on))
                            {
                                continue;
                            }
                            Lookup::PullRequest(key)
                        }
                        // A name merely checked out may be a path or a branch
                        // never pushed; GitHub is asked about it only while a
                        // worktree has it.
                        Resolved::Branch { branch, .. }
                            if aggregate.relation == Relation::CheckedOut
                                && matches!(reference, Reference::Branch { .. })
                                && !facts
                                    .project
                                    .as_ref()
                                    .is_some_and(|project| project.has_branch(&branch)) =>
                        {
                            continue;
                        }
                        Resolved::Branch { base, head, branch } => Lookup::Branch {
                            base: RepoKeyOrd::from(&base),
                            head: RepoKeyOrd::from(&head),
                            branch,
                        },
                        Resolved::Nothing => continue,
                    };
                    match lookups.iter_mut().find(|(_, known)| *known == lookup) {
                        Some((relation, _)) => *relation = (*relation).min(aggregate.relation),
                        None => lookups.push((aggregate.relation, lookup)),
                    }
                }
            }
            lookups.sort_by_key(|(relation, _)| *relation);
            lookups
        };
        let gh = GitHubService::system("gh");
        let cancellation = &self.0.cancellation;
        for (_, lookup) in lookups.into_iter().take(LOOKUPS_PER_PASS) {
            match lookup {
                Lookup::Branch {
                    base: base_key,
                    head,
                    branch,
                } => {
                    let base = GitHubRepoKey::from(&base_key);
                    let head = GitHubRepoKey::from(&head);
                    let result = gh.resolve_branch(&base, &head, &branch, None, cancellation);
                    let mut state = self.conversations();
                    match result {
                        Ok(found) => {
                            if let Some(found) = &found
                                && let Ok(key) = PullRequestKey::new(base.clone(), found.number)
                            {
                                state.observed.insert((&key).into(), observed_now(found));
                            }
                            state.branches.insert(
                                (base_key, branch),
                                BranchLookup {
                                    number: found.map(|found| found.number),
                                    checked: Instant::now(),
                                },
                            );
                            state.succeeded();
                        }
                        // GitHub answered, about this branch only: it has no
                        // single pull request here. That is not GitHub being
                        // unreachable.
                        Err(error) if answered(&error) => {
                            state.branches.insert(
                                (base_key, branch),
                                BranchLookup {
                                    number: None,
                                    checked: Instant::now(),
                                },
                            );
                        }
                        Err(error) => {
                            state.failed(&error);
                            return;
                        }
                    }
                }
                Lookup::PullRequest(key) => {
                    let pull_request_key = PullRequestKey::from(&key);
                    let result =
                        gh.view_with_timeout(&pull_request_key, cancellation, LOOKUP_TIMEOUT);
                    let mut state = self.conversations();
                    let entry = state.observed.entry(key).or_default();
                    entry.checked = Some(Instant::now());
                    match result {
                        Ok(found) => {
                            *entry = observed_now(&found);
                            state.succeeded();
                        }
                        Err(error) if answered(&error) => {
                            entry.missing = true;
                            entry.failures = 0;
                        }
                        Err(error) => {
                            entry.failures = entry.failures.saturating_add(1);
                            state.failed(&error);
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// A question for GitHub.
#[derive(PartialEq, Eq)]
enum Lookup {
    PullRequest(PullRequestKeyOrd),
    /// Which pull request `head`'s `branch` opens against `base`.
    Branch {
        base: RepoKeyOrd,
        head: RepoKeyOrd,
        branch: String,
    },
}

/// What a pull request's row shows beyond its state: the lines it changes
/// and its head's checks.
#[derive(Debug, Clone, Default)]
struct Detail {
    diff: Option<DiffStat>,
    checks: Vec<Check>,
}

impl Detail {
    fn of(found: &crate::github::PullRequest) -> Self {
        let diff = found
            .additions
            .zip(found.deletions)
            .map(|(additions, deletions)| DiffStat {
                additions,
                deletions,
            });
        let mut checks = found
            .check_summaries()
            .into_iter()
            .filter_map(|check| {
                // The summary's buckets count a skipped check as passing and
                // a cancelled one as failing; the row says which it was.
                let state = match (check.state.0.as_str(), check.bucket.0.as_str()) {
                    ("SKIPPED", _) => CheckState::Skipped,
                    ("CANCELLED", _) => CheckState::Cancelled,
                    (_, "pass") => CheckState::Passed,
                    (_, "fail") => CheckState::Failed,
                    (_, "pending") => CheckState::Pending,
                    _ => return None,
                };
                Some(Check {
                    name: check.name,
                    workflow: check.workflow,
                    state,
                    url: check.link.to_string(),
                })
            })
            .collect::<Vec<_>>();
        checks.sort_by(|a, b| {
            a.state
                .cmp(&b.state)
                .then_with(|| a.workflow.cmp(&b.workflow))
                .then_with(|| a.name.cmp(&b.name))
        });
        Self { diff, checks }
    }
}

fn observed_now(found: &crate::github::PullRequest) -> Observed {
    Observed {
        pull_request: Some(board::convert(found, false)),
        detail: Detail::of(found),
        observed_at_unix_ms: Some(now_unix_ms()),
        checked: Some(Instant::now()),
        missing: false,
        failures: 0,
    }
}

/// GitHub answered that this number names no pull request: an issue, or
/// nothing at all. Neither changes soon.
/// GitHub answered about this one pull request or branch: it is not one,
/// its repository does not exist or is hidden, or several match. Unlike a
/// failure to reach GitHub, this says nothing about any other lookup.
fn answered(error: &GitHubError) -> bool {
    let text = match error {
        GitHubError::Process(failure) => String::from_utf8_lossy(&failure.stderr).into_owned(),
        GitHubError::InvalidResponse { source, .. } => return answered(source),
        GitHubError::Inspection(_) | GitHubError::Validation(_) => return true,
        error => error.to_string(),
    };
    text.contains("Could not resolve to a PullRequest")
        || text.contains("Could not resolve to a Repository")
        || text.contains("no pull requests found")
}

/// A conversation's references, worked-on first, strongest first, then most
/// recent.
fn ranked(watched: &Watched) -> impl Iterator<Item = (&Reference, &Aggregate)> + '_ {
    let mut references = watched.references.iter().collect::<Vec<_>>();
    references.sort_by(|(_, a), (_, b)| a.relation.cmp(&b.relation).then(b.seen.cmp(&a.seen)));
    references.into_iter()
}

fn resolve(
    reference: &Reference,
    facts: &Facts,
    branches: &BTreeMap<(RepoKeyOrd, String), BranchLookup>,
) -> Resolved {
    let project = facts.project.as_ref();
    let repository = project.and_then(ProjectFacts::repository);
    let branch_in = |head: Option<&GitHubRepoKey>, name: &str| -> Resolved {
        let (Some(project), Some(base)) = (project, repository) else {
            return Resolved::Nothing;
        };
        let snapshot = project.snapshot.as_deref();
        // A branch pushed elsewhere than the project's own remotes is no
        // branch of this project.
        if let (Some(head), Some(snapshot)) = (head, snapshot)
            && !snapshot
                .remotes
                .iter()
                .chain(std::iter::once(&snapshot.repository))
                .any(|remote| remote.same_repository(head))
        {
            return Resolved::Nothing;
        }
        if let Some(found) = snapshot.and_then(|snapshot| snapshot.for_branch(name, None)) {
            return Resolved::Key(PullRequestKeyOrd {
                repository: base.into(),
                number: found.number,
            });
        }
        match branches.get(&(base.into(), name.to_owned())) {
            Some(lookup) => {
                let fresh = lookup.checked.elapsed()
                    < if lookup.number.is_some() {
                        BRANCH_FOUND_FOR
                    } else {
                        BRANCH_MISSING_FOR
                    };
                match lookup.number {
                    Some(number) => Resolved::Key(PullRequestKeyOrd {
                        repository: base.into(),
                        number,
                    }),
                    None if fresh => Resolved::Nothing,
                    None => Resolved::Branch {
                        base: base.clone(),
                        head: head.unwrap_or(base).clone(),
                        branch: name.to_owned(),
                    },
                }
            }
            None => Resolved::Branch {
                base: base.clone(),
                head: head.unwrap_or(base).clone(),
                branch: name.to_owned(),
            },
        }
    };
    match reference {
        Reference::PullRequest(key) => Resolved::Key(key.clone()),
        Reference::Number(number) => match repository {
            Some(repository) => Resolved::Key(PullRequestKeyOrd {
                repository: repository.into(),
                number: *number,
            }),
            None => Resolved::Nothing,
        },
        Reference::Branch { repository, name } => {
            let head = repository.as_ref().map(GitHubRepoKey::from);
            branch_in(head.as_ref(), name)
        }
        Reference::CurrentBranch { directory } => {
            let Some(project) = project else {
                return Resolved::Nothing;
            };
            let directory = match directory {
                Some(directory) => normalize(&facts.directory.join(directory)),
                None => facts.directory.clone(),
            };
            match project
                .worktree(&directory)
                .and_then(|worktree| worktree.branch.as_deref())
            {
                Some(branch) => branch_in(None, branch),
                None => Resolved::Nothing,
            }
        }
    }
}

/// `..` and `.` removed lexically: the directory may no longer exist.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            component => normal.push(component),
        }
    }
    normal
}

fn assemble(
    watched: &Watched,
    facts: Option<&Facts>,
    observed: &BTreeMap<PullRequestKeyOrd, Observed>,
    branches: &BTreeMap<(RepoKeyOrd, String), BranchLookup>,
    forge_error: Option<String>,
) -> ConversationPullRequests {
    let project = facts.and_then(|facts| facts.project.as_ref());
    let repository = project.and_then(ProjectFacts::repository).cloned();
    // The branch where the conversation works heads its current pull
    // request, which belongs in the list even before anything touches it.
    let current = facts.and_then(|facts| {
        match resolve(
            &Reference::CurrentBranch { directory: None },
            facts,
            branches,
        ) {
            Resolved::Key(key) => Some(key),
            _ => None,
        }
    });
    let mut rows: BTreeMap<PullRequestKeyOrd, (Aggregate, bool)> = BTreeMap::new();
    if let Some(current) = &current {
        rows.insert(
            current.clone(),
            (
                Aggregate {
                    relation: Relation::CheckedOut,
                    by_subagent: false,
                    seen: 0,
                },
                true,
            ),
        );
    }
    for (reference, aggregate) in &watched.references {
        let Some(facts) = facts else {
            break;
        };
        let Resolved::Key(key) = resolve(reference, facts, branches) else {
            continue;
        };
        match rows.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((*aggregate, false));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let (row, current_branch) = entry.get_mut();
                row.relation = row.relation.min(aggregate.relation);
                // The current branch is the conversation's own, not a
                // subagent's.
                row.by_subagent &= aggregate.by_subagent && !*current_branch;
                row.seen = row.seen.max(aggregate.seen);
            }
        }
    }
    let mut pull_requests = rows
        .into_iter()
        .filter_map(|(key, (aggregate, current_branch))| {
            let listed = project
                .and_then(|project| project.listed(&key))
                .map(|found| (board::convert(found, false), Detail::of(found)));
            let observation = observed.get(&key);
            if listed.is_none() && observation.is_some_and(|observation| observation.missing) {
                return None;
            }
            // A bare number is shown only once GitHub says it names a pull
            // request; a URL or a branch names one already.
            let named_by_number = watched.references.iter().any(|(reference, _)| {
                matches!(reference, Reference::Number(number) if *number == key.number)
            }) && !watched.references.iter().any(|(reference, _)| {
                matches!(reference, Reference::PullRequest(other) if *other == key)
            });
            let (pull_request, detail) = match listed {
                Some((pull_request, detail)) => (Some(pull_request), detail),
                None => observation.map_or_else(
                    || (None, Detail::default()),
                    |observation| (observation.pull_request.clone(), observation.detail.clone()),
                ),
            };
            if pull_request.is_none() && named_by_number && !current_branch {
                return None;
            }
            // Nothing in the conversation touched the branch's pull request,
            // so it is listed only while it is still the branch's: open, or
            // at the checkout's commit. An old merged one is not.
            if current_branch && aggregate.seen == 0 {
                let head = facts
                    .zip(project)
                    .and_then(|(facts, project)| project.worktree(&facts.directory))
                    .and_then(|worktree| worktree.head.as_ref())
                    .map(|head| head.oid.as_str());
                let still_the_branch_s = pull_request.as_ref().is_some_and(|found| {
                    found.state == crate::work::PullRequestState::Open
                        || head.is_some_and(|head| found.head_oid == head)
                });
                if !still_the_branch_s {
                    return None;
                }
            }
            let observed_at_unix_ms = match (&pull_request, observation) {
                (Some(_), Some(observation)) if observation.pull_request.is_some() => {
                    observation.observed_at_unix_ms
                }
                // The board's listing is as old as its last sync, which a
                // later pass does not change.
                (Some(_), _) => project.and_then(|project| project.synced_at_unix_ms),
                (None, _) => None,
            };
            // The key is kept in one case; show the spelling GitHub uses.
            let spelled = GitHubRepoKey::from(&key.repository);
            let canonical = match (&repository, &pull_request) {
                (Some(own), _) if own.same_repository(&spelled) => own.clone(),
                (_, Some(pull_request)) => PullRequestKey::from_url(&pull_request.url)
                    .ok()
                    .map(|found| found.repository)
                    .filter(|found| found.same_repository(&spelled))
                    .unwrap_or(spelled),
                _ => spelled,
            };
            Some((
                aggregate.seen,
                ConversationPullRequest {
                    repository: canonical,
                    number: key.number,
                    relation: aggregate.relation,
                    by_subagent: aggregate.by_subagent,
                    current_branch,
                    pull_request,
                    observed_at_unix_ms,
                    diff: detail.diff,
                    checks: detail.checks,
                },
            ))
        })
        .collect::<Vec<_>>();
    pull_requests.sort_by(|(a_seen, a), (b_seen, b)| {
        a.relation
            .cmp(&b.relation)
            .then(b.current_branch.cmp(&a.current_branch))
            .then(b_seen.cmp(a_seen))
    });
    let forge_error = project
        .and_then(|project| project.forge_error.clone())
        .or(forge_error);
    ConversationPullRequests {
        generated_at_unix_ms: now_unix_ms(),
        repository,
        transcript: watched.coverage.clone(),
        forge_error,
        worktrees: facts
            .zip(project)
            .map_or_else(Vec::new, |(facts, project)| {
                edited_worktrees(watched, project, |branch| {
                    let Resolved::Key(key) = resolve(
                        &Reference::Branch {
                            repository: None,
                            name: branch.to_owned(),
                        },
                        facts,
                        branches,
                    ) else {
                        return None;
                    };
                    pull_requests
                        .iter()
                        .any(|(_, row)| {
                            row.relation != Relation::Mentioned
                                && row.number == key.number
                                && RepoKeyOrd::from(&row.repository) == key.repository
                        })
                        .then_some(key.number)
                })
            }),
        pull_requests: pull_requests.into_iter().map(|(_, row)| row).collect(),
    }
}

/// The project's worktrees holding the directories `watched` edited, most
/// recently edited first. A directory outside every worktree, such as another
/// project's, names none.
///
/// `listed` names the worked-on pull request a branch heads, if any.
fn edited_worktrees(
    watched: &Watched,
    project: &ProjectFacts,
    listed: impl Fn(&str) -> Option<u64>,
) -> Vec<ConversationWorktree> {
    edited(watched, project)
        .into_iter()
        .map(|(worktree, edit)| ConversationWorktree {
            path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            by_subagent: edit.by_subagent,
            changed_paths: worktree.dirty.as_ref().map(|dirty| dirty.paths),
            // Filled in from what was measured, which the scan does not hold.
            diff: None,
            pull_request: worktree.branch.as_deref().and_then(&listed),
        })
        .collect()
}

/// The project's worktrees holding the directories `watched` edited, with
/// how they were edited, most recently edited first.
fn edited<'a>(
    watched: &Watched,
    project: &'a ProjectFacts,
) -> Vec<(&'a super::scan::Worktree, Edit)> {
    let mut worktrees: BTreeMap<&Path, (&super::scan::Worktree, Edit)> = BTreeMap::new();
    for (directory, edit) in &watched.edited {
        let Some(worktree) = project.worktree(directory) else {
            continue;
        };
        worktrees
            .entry(worktree.path.as_path())
            .and_modify(|(_, found)| {
                found.by_subagent &= edit.by_subagent;
                found.seen = found.seen.max(edit.seen);
            })
            .or_insert((worktree, *edit));
    }
    let mut worktrees = worktrees.into_values().collect::<Vec<_>>();
    worktrees.sort_by(|(_, a), (_, b)| b.seen.cmp(&a.seen));
    worktrees
}

/// What a worktree's uncommitted lines were measured against: a commit or
/// any change to its status, which the scan reads again on every edit,
/// measures it again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    head: Option<String>,
    dirty: Option<crate::work::Dirty>,
}

impl Fingerprint {
    fn of(worktree: &super::scan::Worktree) -> Self {
        Self {
            head: worktree.head.as_ref().map(|head| head.oid.clone()),
            dirty: worktree.dirty.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Measured {
    fingerprint: Fingerprint,
    diff: Option<DiffStat>,
}

fn with_measured(list: &mut ConversationPullRequests, measured: &BTreeMap<PathBuf, Measured>) {
    for worktree in &mut list.worktrees {
        worktree.diff = measured
            .get(&worktree.path)
            .and_then(|measured| measured.diff);
    }
}

/// Where a conversation's provider keeps its transcript, once it has
/// reported which conversation it is.
///
/// Returns nothing new while the conversation ID is still `known`.
/// Waits between searches that found nothing: 5 s, doubling to 2 min.
fn search_backoff(missed: u32) -> Duration {
    (Duration::from_secs(5) * 2_u32.saturating_pow(missed.saturating_sub(1).min(5)))
        .min(Duration::from_secs(120))
}

/// Finds the conversation's transcript when its provider ID is new. A
/// Codex search runs only while `search_due`.
fn locate(
    host: &SharedHost,
    conversation: TerminalId,
    known: Option<&str>,
    search_due: bool,
) -> Location {
    let (provider, recipe, session_id) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&conversation) else {
            return Location::Unchanged;
        };
        (
            record.session.launch.provider,
            record.session.launch.recipe.clone(),
            record
                .handle
                .session_id()
                .or_else(|| record.session.launch.session_id.clone()),
        )
    };
    let Some(id) = session_id.filter(|id| known != Some(id.as_str())) else {
        return Location::Unchanged;
    };
    let Some(root) = provider.session_root(&recipe) else {
        return Location::Unchanged;
    };
    let directory = std::fs::canonicalize(&recipe.working_directory)
        .unwrap_or_else(|_| recipe.working_directory.clone());
    match provider {
        Provider::Claude => {
            let Ok(transcript) = crate::agent::claude_transcript(&root, &directory, &id) else {
                return Location::Unchanged;
            };
            Location::Found(
                id,
                Source::Located(Located {
                    format: Format::Claude,
                    directory,
                    subagents: Some(transcript.with_extension("").join("subagents")),
                    transcript,
                }),
            )
        }
        Provider::Codex if !search_due => Location::Unchanged,
        Provider::Codex => match codex_rollout(&root.join("sessions"), &id) {
            Some(transcript) => Location::Found(
                id,
                Source::Located(Located {
                    format: Format::Codex,
                    directory,
                    transcript,
                    subagents: None,
                }),
            ),
            None => Location::Missed,
        },
        Provider::Omp => Location::Found(id, Source::Unsupported),
    }
}

/// Codex names a thread's rollout `rollout-<time>-<thread id>.jsonl` under
/// dated directories; the newest are searched first.
fn codex_rollout(sessions: &Path, id: &str) -> Option<PathBuf> {
    fn newest_first(directory: &Path) -> Vec<PathBuf> {
        let mut entries = std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| b.cmp(a));
        entries
    }
    let suffix = format!("-{id}.jsonl");
    let mut visited = 0;
    for year in newest_first(sessions).into_iter().take(4) {
        for month in newest_first(&year) {
            for day in newest_first(&month) {
                for file in newest_first(&day) {
                    visited += 1;
                    if visited > 50_000 {
                        return None;
                    }
                    if file
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
                    {
                        return Some(file);
                    }
                }
            }
        }
    }
    None
}

/// Reads what each of the conversation's transcript files gained since the
/// last pass.
#[cfg(test)]
fn read(watched: &mut Watched) {
    let scanned = scan(&watched.source, &mut watched.files);
    watched.apply(scanned, None);
}

/// What one read of a conversation's transcripts found.
struct Scanned {
    found: Vec<transcript::Found>,
    coverage: TranscriptCoverage,
}

/// Reads the new part of a conversation's transcripts. It needs only the
/// cursors, so no lock is held while it reads.
fn scan(source: &Source, cursors: &mut BTreeMap<PathBuf, Cursor>) -> Scanned {
    let located = match source {
        Source::Pending => {
            return Scanned {
                found: Vec::new(),
                coverage: TranscriptCoverage::Pending,
            };
        }
        Source::Unsupported => {
            return Scanned {
                found: Vec::new(),
                coverage: TranscriptCoverage::Unsupported,
            };
        }
        Source::Located(located) => located.clone(),
    };
    let mut files = vec![(located.transcript.clone(), false)];
    let mut capped = false;
    if let Some(subagents) = &located.subagents {
        let mut children = std::fs::read_dir(subagents)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("agent-"))
            })
            .collect::<Vec<_>>();
        // The newest subagents are read when there are too many to read.
        capped = children.len() > SUBAGENTS;
        children.sort_by_cached_key(|path| {
            std::cmp::Reverse(
                std::fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
            )
        });
        files.extend(
            children
                .into_iter()
                .take(SUBAGENTS)
                .map(|path| (path, true)),
        );
    }
    let mut complete = true;
    let mut unavailable = None;
    let mut found = Vec::new();
    for (path, subagent) in files {
        match read_file(cursors, &path, located.format, subagent, &mut found) {
            Ok(caught_up) => complete &= caught_up,
            Err(error) if error.kind() == io::ErrorKind::NotFound && subagent => {}
            Err(error) => {
                if !subagent {
                    unavailable = Some(format!("The transcript cannot be read: {error}"));
                }
            }
        }
    }
    let truncated = cursors.values().any(|cursor| cursor.truncated);
    let coverage = match unavailable {
        Some(reason) if cursors.is_empty() => TranscriptCoverage::Unavailable { reason },
        _ if !complete => TranscriptCoverage::Pending,
        _ if truncated || capped => TranscriptCoverage::Partial,
        _ => TranscriptCoverage::Complete,
    };
    Scanned { found, coverage }
}

/// Reads the new part of one file, and reports whether it reached the end.
fn read_file(
    cursors: &mut BTreeMap<PathBuf, Cursor>,
    path: &Path,
    format: Format,
    subagent: bool,
    found: &mut Vec<transcript::Found>,
) -> io::Result<bool> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let identity = (metadata.dev(), metadata.ino());
    let size = metadata.len();
    let cursor = cursors.entry(path.to_owned()).or_insert_with(|| {
        let start = size.saturating_sub(FIRST_READ);
        // Starting mid-file, the first line is whole only when the byte
        // before it ends a line.
        let mut before = [0];
        let mid_line = start > 0
            && (file.read_exact_at(&mut before, start - 1).is_err() || before[0] != b'\n');
        Cursor {
            identity,
            offset: start,
            partial: Vec::new(),
            skipping: mid_line,
            reader: Reader::new(format, subagent),
            truncated: start > 0,
        }
    });
    if cursor.identity != identity || cursor.offset > size {
        // Replaced or rewritten: read it again from its start.
        *cursor = Cursor {
            identity,
            offset: 0,
            partial: Vec::new(),
            skipping: false,
            reader: Reader::new(format, subagent),
            truncated: false,
        };
    }
    let end = size.min(cursor.offset + READ_PER_PASS);
    let mut buffer = vec![0; (end - cursor.offset) as usize];
    file.read_exact_at(&mut buffer, cursor.offset)?;
    cursor.offset = end;
    let mut rest = buffer.as_slice();
    while let Some(newline) = rest.iter().position(|byte| *byte == b'\n') {
        let (line, after) = rest.split_at(newline);
        rest = &after[1..];
        if cursor.skipping || cursor.partial.len() + line.len() > MAX_LINE {
            cursor.skipping = false;
            cursor.partial.clear();
            continue;
        }
        if cursor.partial.is_empty() {
            cursor.reader.line(line, found);
        } else {
            cursor.partial.extend_from_slice(line);
            let whole = std::mem::take(&mut cursor.partial);
            cursor.reader.line(&whole, found);
        }
    }
    if !cursor.skipping {
        cursor.partial.extend_from_slice(rest);
        if cursor.partial.len() > MAX_LINE {
            cursor.partial.clear();
            cursor.skipping = true;
        }
    }
    Ok(end == size)
}
