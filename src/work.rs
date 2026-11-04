//! The Work board: one card per worktree or branch the user is working on,
//! newest activity first, in columns derived from local Git state, sessions
//! and the forge. The daemon owns the facts; the CLI and every window read the
//! same board, so a card cannot sit in different columns in different places.
//!
//! Everything here is plain data shared by all three executables. Deriving a
//! column is a pure function of a card and the clock, so it is decided the same
//! way wherever it is evaluated and can be tested without Git or a daemon.

use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    agent::{ActivityState, Provider},
    model::TerminalId,
};

/// How recent a local change must be for its card to count as active.
pub const ACTIVE_WINDOW_MS: u64 = 3 * 60 * 60 * 1000;
pub const DEFAULT_STALE_AFTER_DAYS: u16 = 7;
pub const MAX_STALE_AFTER_DAYS: u16 = 365;
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Board columns, in the order they are presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    /// A session is working or waiting there, or something changed recently.
    Active,
    /// Local commits or changes without a pull request ready for review.
    InProgress,
    /// An open pull request that is not a draft.
    InReview,
    /// Merged, with nothing local beyond what merged: a cleanup candidate.
    Landed,
    /// Nothing happened for longer than the configured number of days.
    Stale,
}

impl Column {
    pub const ALL: [Self; 5] = [
        Self::Active,
        Self::InProgress,
        Self::InReview,
        Self::Landed,
        Self::Stale,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::InProgress => "In progress",
            Self::InReview => "In review",
            Self::Landed => "Landed",
            Self::Stale => "Stale",
        }
    }

    /// The stable identifier used on the command line and in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Landed => "landed",
            Self::Stale => "stale",
        }
    }
}

impl fmt::Display for Column {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Column {
    type Err = String;

    /// Accepts the identifier with either separator, so `in-review` and
    /// `in_review` both name the same column.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .into_iter()
            .find(|column| column.as_str() == normalized)
            .ok_or_else(|| {
                format!(
                    "unknown column `{value}`; expected one of {}",
                    Self::ALL.map(Self::as_str).join(", ")
                )
            })
    }
}

/// Which projects a request covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    All,
    /// An open project by its repository key, its name, or a path inside it.
    Project(PathBuf),
}

/// What identifies a card within its project. A worktree's card is its
/// branch's card, so a pin follows the branch when the worktree is recreated;
/// only a detached worktree is identified by its path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKey {
    /// A short branch name, such as `fix/login`.
    Branch(String),
    /// A worktree by its path: a detached head, or a second checkout of a
    /// branch another card already shows.
    Worktree(PathBuf),
}

impl ItemKey {
    /// The durable text form: `branch:NAME` or `worktree:PATH`.
    pub fn storage_key(&self) -> String {
        match self {
            Self::Branch(name) => format!("branch:{name}"),
            Self::Worktree(path) => format!("worktree:{}", path.display()),
        }
    }

    pub fn from_storage_key(value: &str) -> Option<Self> {
        if let Some(name) = value.strip_prefix("branch:") {
            return (!name.is_empty()).then(|| Self::Branch(name.to_owned()));
        }
        value
            .strip_prefix("worktree:")
            .filter(|path| path.starts_with('/'))
            .map(|path| Self::Worktree(PathBuf::from(path)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The board for `scope`. With `fresh`, the daemon first rescans the
    /// covered projects and polls the forge, within a bounded wait.
    Board { scope: Scope, fresh: bool },
    /// Pins the card named by `item` (a branch name or a worktree path) to a
    /// column, overriding the derived one, or removes its pin with `None`.
    Pin {
        scope: Scope,
        item: String,
        column: Option<Column>,
    },
    /// What removing the worktree of the card named by `item` would lose,
    /// without removing it. With `fetch`, the project's remotes are fetched
    /// first, so work that landed upstream counts as landed.
    RemovalPlan {
        scope: Scope,
        item: String,
        fetch: bool,
    },
    /// Removes a card's worktree, provided its content is still what the
    /// plan with the same token judged.
    Remove(Box<Removal>),
    /// The snapshots earlier removals kept, newest first.
    Snapshots { scope: Scope },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Board(Board),
    Pinned(Box<Item>),
    /// The answer to a subscription: the board as of `revision`.
    Watching {
        revision: u64,
        board: Box<Board>,
    },
    RemovalPlan(Box<RemovalPlan>),
    Removed(Box<Removed>),
    Snapshots(Vec<SavedSnapshot>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    pub scope: Scope,
    pub item: String,
    /// The [`crate::worktree_content::Inspection::token`] of the plan the
    /// user accepted.
    pub token: String,
    pub keep: Keep,
    /// Also delete the branch; allowed only when its work landed.
    pub delete_branch: bool,
}

/// What happens to work only the worktree holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// Nothing needs keeping: refused when the plan found unsaved work.
    Nothing,
    /// Store the uncommitted work as a snapshot first.
    Snapshot,
    /// Delete it, as the user explicitly chose.
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalPlan {
    pub project: PathBuf,
    pub project_name: String,
    /// The card's text form, as the command line names it.
    pub item: String,
    pub inspection: crate::worktree_content::Inspection,
    /// The merged pull request whose head counted as landed.
    pub merged_pull_request: Option<u64>,
}

impl RemovalPlan {
    /// What removal keeps and loses, in a sentence.
    pub fn summary(&self) -> String {
        use crate::worktree_content::Verdict;
        match &self.inspection.verdict {
            Verdict::Landed => format!(
                "Everything landed in {}: removing the worktree loses nothing, and its branch may go too.",
                self.inspection
                    .landed_in
                    .as_deref()
                    .unwrap_or("the default branch")
            ),
            Verdict::CommitsKept => {
                "Nothing uncommitted would be lost, and the branch keeps its commits.".into()
            }
            Verdict::Unsaved {
                uncommitted,
                unreachable,
                beyond_snapshot,
            } => {
                let mut losses = Vec::new();
                if *uncommitted {
                    losses.push("changes that are in no landing target");
                }
                if *unreachable {
                    losses.push("commits no branch or tag holds");
                }
                let mut summary = format!("Removal would lose {}.", losses.join(" and "));
                if *beyond_snapshot {
                    summary.push_str(" A snapshot cannot keep all of it.");
                }
                summary
            }
            Verdict::Blocked(blocker) => format!("Removal is refused: {blocker}."),
        }
    }

    /// Whether removing with `keep` and `delete_branch` is allowed, and why
    /// not. The daemon checks again against the content it removes.
    pub fn permits(&self, keep: Keep, delete_branch: bool) -> Result<(), String> {
        use crate::worktree_content::Verdict;
        match &self.inspection.verdict {
            Verdict::Blocked(blocker) => return Err(format!("Removal is refused: {blocker}")),
            Verdict::Unsaved {
                beyond_snapshot: true,
                ..
            } if keep != Keep::Discard => {
                return Err(
                    "Removal would lose work a snapshot cannot keep, such as filtered files or nested repositories; only an explicit discard removes it"
                        .into(),
                );
            }
            Verdict::Unsaved { .. } if keep == Keep::Nothing => {
                return Err("Removal would lose unsaved work; snapshot or discard it".into());
            }
            Verdict::Landed | Verdict::CommitsKept | Verdict::Unsaved { .. } => {}
        }
        if delete_branch {
            if self.inspection.branch.is_none() {
                return Err("The worktree is detached, so it has no branch to delete".into());
            }
            if self.inspection.verdict != Verdict::Landed {
                return Err("Only a branch whose work landed is deleted with its worktree".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub project: PathBuf,
    pub path: PathBuf,
    pub snapshot: Option<crate::worktree_content::Snapshot>,
    /// The branch deleted with the worktree.
    pub deleted_branch: Option<String>,
    /// Why the branch was kept although its deletion was asked for. The
    /// worktree is removed regardless.
    pub branch_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSnapshot {
    pub project: PathBuf,
    pub project_name: String,
    pub refname: String,
    pub oid: String,
    pub created_at_unix_ms: Option<u64>,
    /// Such as `WIP on fix/login: 0123456789ab`.
    pub subject: String,
    /// The worktree it was taken from.
    pub worktree: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub generated_at_unix_ms: u64,
    pub stale_after_days: u16,
    pub projects: Vec<Project>,
    /// Newest activity first; see [`sort`].
    pub items: Vec<Item>,
    /// Why Linear could not be asked about some cards' tickets, which those
    /// cards then lack.
    pub linear_unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The project's key in the shared workspace model (its Git common dir).
    pub repository: PathBuf,
    pub name: String,
    /// The primary checkout, when the repository has one.
    pub root: Option<PathBuf>,
    /// The default branch that ahead/behind counts compare against, such as
    /// `origin/main`.
    pub base: Option<String>,
    pub scanned_at_unix_ms: Option<u64>,
    pub scan_error: Option<String>,
    pub forge: Forge,
}

/// What the board knows from the project's forge. Externally tagged, like
/// every enum on the postcard wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Forge {
    /// Not polled since the daemon started and nothing was retained.
    Pending,
    /// The project has no GitHub remote.
    NotGitHub,
    /// GitHub cannot be read, for example because `gh` is not signed in.
    Unavailable { reason: String },
    /// Pull requests as of `at_unix_ms`. A failed later poll keeps them and
    /// reports `error`, so a flaky network does not empty the board.
    Synced {
        at_unix_ms: u64,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub project: PathBuf,
    pub project_name: String,
    pub key: ItemKey,
    pub branch: Option<String>,
    pub worktree: Option<Worktree>,
    pub head: Option<Head>,
    /// Commits ahead of and behind the project's default branch.
    pub comparison: Option<Comparison>,
    pub dirty: Option<Dirty>,
    pub pull_request: Option<PullRequest>,
    pub linear: Option<LinearTicket>,
    /// Live sessions whose working directory is inside this worktree.
    pub sessions: Vec<Session>,
    pub last_activity: Option<Activity>,
    /// Why this card's worktree status could not be read; its other facts
    /// still stand.
    pub error: Option<String>,
    /// The column shown: the pin when there is one, otherwise `derived`.
    pub column: Column,
    pub derived: Column,
    pub pinned: bool,
}

impl Item {
    /// The text that names this card on the command line.
    pub fn label(&self) -> String {
        match &self.key {
            ItemKey::Branch(name) => name.clone(),
            ItemKey::Worktree(path) => path.display().to_string(),
        }
    }

    pub fn has_live_agent(&self) -> bool {
        self.sessions.iter().any(|session| {
            matches!(
                session.state,
                ActivityState::Working | ActivityState::Waiting
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub primary: bool,
    pub locked: bool,
    /// When the worktree directory was created, where the filesystem records it.
    pub created_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub oid: String,
    pub committed_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    pub base: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dirty {
    /// Changed paths; an untracked directory counts once.
    pub paths: u32,
    pub conflicted: u32,
    pub newest_modified_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    pub draft: bool,
    pub head_oid: String,
    /// Whether the pull request's head is the local head or descends from it,
    /// so that nothing local is missing from what the forge has.
    pub contains_local_head: bool,
    pub checks: Option<Checks>,
    pub review: Option<Review>,
    pub updated_at: String,
}

impl PullRequest {
    /// Open and not a draft: what the forge asks reviewers to look at.
    pub fn ready(&self) -> bool {
        self.state == PullRequestState::Open && !self.draft
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
}

impl Checks {
    pub fn outcome(self) -> Option<CheckOutcome> {
        if self.failed > 0 {
            Some(CheckOutcome::Failing)
        } else if self.pending > 0 {
            Some(CheckOutcome::Pending)
        } else if self.passed > 0 {
            Some(CheckOutcome::Passing)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passing,
    Pending,
    Failing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Review {
    Approved,
    ChangesRequested,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearTicket {
    /// Such as `TEAM-123`. An explicit link stores only Linear's issue id, so
    /// its identifier is absent until the daemon has resolved it once.
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub source: TicketSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketSource {
    /// The worktree's explicit `cagents linear link`.
    Link,
    /// Read from a Linear-style branch name such as `user/team-123-title`
    /// and confirmed by Linear. Display only: it never authorizes acting on
    /// the ticket.
    BranchName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: TerminalId,
    pub provider: Option<Provider>,
    pub title: Option<String>,
    pub state: ActivityState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub at_unix_ms: u64,
    pub source: ActivitySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    /// The newest commit of the branch's own work.
    Commit,
    /// The newest modification among uncommitted changes.
    Files,
    /// A session in the worktree was last seen working or waiting.
    Session,
    /// The worktree was created and nothing has happened since.
    Created,
    /// Only the forge knows the branch; its pull request was last updated.
    PullRequest,
}

/// The local facts behind [`Item::last_activity`]: the latest of the HEAD
/// commit, the newest dirty file and the last session activity. The HEAD
/// commit counts only when the branch has commits of its own; on a branch that
/// is still at its base it is someone else's commit. Session activity from
/// before the worktree was created belongs to an earlier checkout at the same
/// path. A worktree's creation stands in until anything else happens.
pub fn last_activity(
    head: Option<&Head>,
    comparison: Option<&Comparison>,
    dirty: Option<&Dirty>,
    session_at_unix_ms: Option<u64>,
    created_at_unix_ms: Option<u64>,
) -> Option<Activity> {
    let own_commit = comparison.is_none_or(|comparison| comparison.ahead > 0);
    [
        head.filter(|_| own_commit)
            .and_then(|head| head.committed_at_unix_ms)
            .map(|at| (at, ActivitySource::Commit)),
        dirty
            .and_then(|dirty| dirty.newest_modified_at_unix_ms)
            .map(|at| (at, ActivitySource::Files)),
        session_at_unix_ms
            .filter(|at| created_at_unix_ms.is_none_or(|created| *at >= created))
            .map(|at| (at, ActivitySource::Session)),
    ]
    .into_iter()
    .flatten()
    .max_by_key(|(at, _)| *at)
    .or_else(|| created_at_unix_ms.map(|at| (at, ActivitySource::Created)))
    .map(|(at_unix_ms, source)| Activity { at_unix_ms, source })
}

/// The column a card belongs in by its facts alone. In order:
///
/// 1. **Active** while a live session there is working or waiting.
/// 2. **Landed** when the pull request merged and nothing local remains beyond
///    it: a worktree's status read clean, and its head is the merged head or
///    an ancestor of it.
/// 3. **Active** when the last local change is within [`ACTIVE_WINDOW_MS`].
/// 4. **Stale** when the last activity is older than `stale_after_days`.
/// 5. **In review** with an open, non-draft pull request.
/// 6. **In progress** otherwise: commits, changes, drafts and closed drafts.
pub fn derive_column(item: &Item, now_unix_ms: u64, stale_after_days: u16) -> Column {
    if item.has_live_agent() {
        return Column::Active;
    }
    // A worktree whose status could not be read is not known to be clean; a
    // branch without one has nothing uncommitted.
    let clean = match &item.dirty {
        Some(dirty) => dirty.paths == 0,
        None => item.worktree.is_none(),
    };
    if clean
        && item.pull_request.as_ref().is_some_and(|pull_request| {
            pull_request.state == PullRequestState::Merged && pull_request.contains_local_head
        })
    {
        return Column::Landed;
    }
    let age = item
        .last_activity
        .map(|activity| now_unix_ms.saturating_sub(activity.at_unix_ms));
    if age.is_some_and(|age| age <= ACTIVE_WINDOW_MS)
        && item
            .last_activity
            .is_some_and(|activity| activity.source != ActivitySource::PullRequest)
    {
        return Column::Active;
    }
    if age.is_some_and(|age| age > u64::from(stale_after_days.max(1)) * DAY_MS) {
        return Column::Stale;
    }
    if item.pull_request.as_ref().is_some_and(PullRequest::ready) {
        return Column::InReview;
    }
    Column::InProgress
}

/// Sets `derived` from the facts and `column` from the pin, if any.
pub fn place(item: &mut Item, pin: Option<Column>, now_unix_ms: u64, stale_after_days: u16) {
    item.derived = derive_column(item, now_unix_ms, stale_after_days);
    item.pinned = pin.is_some();
    item.column = pin.unwrap_or(item.derived);
}

/// Newest activity first; cards without any known activity last. Ties keep a
/// stable order by project and label so a board does not shuffle on refresh.
pub fn sort(items: &mut [Item]) {
    items.sort_by(|left, right| {
        let at = |item: &Item| item.last_activity.map(|activity| activity.at_unix_ms);
        at(right)
            .cmp(&at(left))
            .then_with(|| left.project_name.cmp(&right.project_name))
            .then_with(|| left.key.cmp(&right.key))
    });
}

/// The Linear identifier a Linear-style branch name carries, such as
/// `TEAM-123` from `fix/team-123-display`. Only the start of the last
/// path segment is read, which is where Linear's suggested names put it, so a
/// trailing date (`browser-20261019`) or a version (`v2-release`) is not one.
pub fn ticket_from_branch(branch: &str) -> Option<String> {
    let segment = branch.rsplit('/').next()?;
    let (team, rest) = segment.split_once('-')?;
    let number = rest.split('-').next()?;
    let team_valid = (2..=7).contains(&team.len())
        && team.starts_with(|c: char| c.is_ascii_alphabetic())
        && team.chars().all(|c| c.is_ascii_alphanumeric());
    let number_valid = (1..=6).contains(&number.len())
        && number.chars().all(|c| c.is_ascii_digit())
        && !number.starts_with('0');
    (team_valid && number_valid).then(|| format!("{}-{number}", team.to_ascii_uppercase()))
}

/// The `work_stale_after_days` preference from the settings file at `path`.
/// Only that key is read, so the daemon needs none of the UI's settings model.
/// A missing file or key is the default; a file that exists but cannot be read
/// is an error, so a broken file is reported rather than silently ignored.
pub fn stale_after_days(path: &std::path::Path) -> Result<u16, String> {
    #[derive(Deserialize)]
    struct Preference {
        #[serde(default = "default_stale_after_days")]
        work_stale_after_days: u16,
    }
    fn default_stale_after_days() -> u16 {
        DEFAULT_STALE_AFTER_DAYS
    }
    let document = match std::fs::read_to_string(path) {
        Ok(document) => document,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DEFAULT_STALE_AFTER_DAYS);
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let days = toml::from_str::<Preference>(&document)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .work_stale_after_days;
    Ok(days.clamp(1, MAX_STALE_AFTER_DAYS))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 60 * 60 * 1000;

    fn item() -> Item {
        Item {
            project: "/repo/.git".into(),
            project_name: "repo".into(),
            key: ItemKey::Branch("feature".into()),
            branch: Some("feature".into()),
            worktree: None,
            head: None,
            comparison: None,
            dirty: None,
            pull_request: None,
            linear: None,
            sessions: Vec::new(),
            last_activity: None,
            error: None,
            column: Column::InProgress,
            derived: Column::InProgress,
            pinned: false,
        }
    }

    fn activity(age: u64, source: ActivitySource) -> Option<Activity> {
        Some(Activity {
            at_unix_ms: NOW - age,
            source,
        })
    }

    fn pull_request(state: PullRequestState, draft: bool) -> PullRequest {
        PullRequest {
            number: 7,
            title: "Feature".into(),
            url: "https://github.com/o/r/pull/7".into(),
            state,
            draft,
            head_oid: "abc".into(),
            contains_local_head: true,
            checks: None,
            review: None,
            updated_at: "2026-10-21T00:00:00Z".into(),
        }
    }

    fn session(state: ActivityState) -> Session {
        Session {
            id: TerminalId::new(),
            provider: Some(Provider::Claude),
            title: None,
            state,
        }
    }

    #[test]
    fn a_working_or_waiting_session_makes_a_card_active_over_everything_else() {
        for state in [ActivityState::Working, ActivityState::Waiting] {
            let mut card = item();
            card.pull_request = Some(pull_request(PullRequestState::Merged, false));
            card.last_activity = activity(40 * 24 * HOUR, ActivitySource::Commit);
            card.sessions = vec![session(ActivityState::Idle), session(state)];
            assert_eq!(derive_column(&card, NOW, 7), Column::Active);
        }
        let mut idle = item();
        idle.sessions = vec![session(ActivityState::Idle)];
        idle.last_activity = activity(10 * 24 * HOUR, ActivitySource::Session);
        assert_eq!(derive_column(&idle, NOW, 7), Column::Stale);
    }

    #[test]
    fn a_recent_local_change_is_active_but_a_just_merged_branch_is_landed() {
        let mut card = item();
        card.last_activity = activity(2 * HOUR, ActivitySource::Files);
        assert_eq!(derive_column(&card, NOW, 7), Column::Active);
        card.last_activity = activity(4 * HOUR, ActivitySource::Files);
        assert_eq!(derive_column(&card, NOW, 7), Column::InProgress);

        let mut merged = item();
        merged.last_activity = activity(HOUR, ActivitySource::Commit);
        merged.pull_request = Some(pull_request(PullRequestState::Merged, false));
        assert_eq!(derive_column(&merged, NOW, 7), Column::Landed);
        // Anything local beyond the merge keeps it out of Landed.
        merged.dirty = Some(Dirty {
            paths: 1,
            conflicted: 0,
            newest_modified_at_unix_ms: Some(NOW - HOUR),
        });
        assert_eq!(derive_column(&merged, NOW, 7), Column::Active);
        merged.dirty = None;
        merged.pull_request.as_mut().unwrap().contains_local_head = false;
        assert_eq!(derive_column(&merged, NOW, 7), Column::Active);

        // A worktree must have read clean, not merely failed to read.
        merged.pull_request.as_mut().unwrap().contains_local_head = true;
        merged.worktree = Some(Worktree {
            path: "/repo/.worktrees/feature".into(),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        assert_eq!(derive_column(&merged, NOW, 7), Column::Active);
        merged.dirty = Some(Dirty::default());
        assert_eq!(derive_column(&merged, NOW, 7), Column::Landed);
    }

    #[test]
    fn forge_only_activity_never_makes_a_card_active() {
        let mut card = item();
        card.pull_request = Some(pull_request(PullRequestState::Open, false));
        card.last_activity = activity(HOUR, ActivitySource::PullRequest);
        assert_eq!(derive_column(&card, NOW, 7), Column::InReview);
    }

    #[test]
    fn stale_follows_the_configured_days_and_ranks_above_review() {
        let mut card = item();
        card.pull_request = Some(pull_request(PullRequestState::Open, false));
        card.last_activity = activity(3 * 24 * HOUR, ActivitySource::Commit);
        assert_eq!(derive_column(&card, NOW, 7), Column::InReview);
        assert_eq!(derive_column(&card, NOW, 2), Column::Stale);
        // Zero is treated as one day rather than making every card stale.
        card.last_activity = activity(12 * HOUR, ActivitySource::Commit);
        assert_eq!(derive_column(&card, NOW, 0), Column::InReview);
    }

    #[test]
    fn drafts_closed_drafts_and_plain_branches_are_in_progress() {
        for pull_request in [
            Some(pull_request(PullRequestState::Open, true)),
            Some(pull_request(PullRequestState::Closed, true)),
            Some(pull_request(PullRequestState::Closed, false)),
            None,
        ] {
            let mut card = item();
            card.pull_request = pull_request;
            card.last_activity = activity(24 * HOUR, ActivitySource::Commit);
            assert_eq!(derive_column(&card, NOW, 7), Column::InProgress);
        }
    }

    #[test]
    fn a_pin_overrides_the_derived_column_and_is_reported() {
        let mut card = item();
        card.last_activity = activity(HOUR, ActivitySource::Commit);
        place(&mut card, Some(Column::Stale), NOW, 7);
        assert_eq!(
            (card.column, card.derived, card.pinned),
            (Column::Stale, Column::Active, true)
        );
        place(&mut card, None, NOW, 7);
        assert_eq!(
            (card.column, card.derived, card.pinned),
            (Column::Active, Column::Active, false)
        );
    }

    #[test]
    fn last_activity_ignores_a_base_commit_and_falls_back_to_creation() {
        let head = Head {
            oid: "abc".into(),
            committed_at_unix_ms: Some(NOW - HOUR),
        };
        let at_base = Comparison {
            base: "origin/main".into(),
            ahead: 0,
            behind: 3,
        };
        assert_eq!(
            last_activity(
                Some(&head),
                Some(&at_base),
                None,
                None,
                Some(NOW - 2 * HOUR)
            ),
            activity(2 * HOUR, ActivitySource::Created)
        );
        let ahead = Comparison {
            ahead: 2,
            ..at_base.clone()
        };
        assert_eq!(
            last_activity(Some(&head), Some(&ahead), None, None, Some(NOW - 2 * HOUR)),
            activity(HOUR, ActivitySource::Commit)
        );
        let dirty = Dirty {
            paths: 2,
            conflicted: 0,
            newest_modified_at_unix_ms: Some(NOW - 60_000),
        };
        assert_eq!(
            last_activity(
                Some(&head),
                Some(&ahead),
                Some(&dirty),
                Some(NOW - 120_000),
                None
            ),
            activity(60_000, ActivitySource::Files)
        );
        assert_eq!(
            last_activity(
                Some(&head),
                Some(&ahead),
                Some(&dirty),
                Some(NOW - 1_000),
                None
            ),
            activity(1_000, ActivitySource::Session)
        );
        assert_eq!(last_activity(None, None, None, None, None), None);
        assert_eq!(
            last_activity(None, None, None, Some(NOW - 2 * HOUR), Some(NOW - HOUR)),
            Some(Activity {
                at_unix_ms: NOW - HOUR,
                source: ActivitySource::Created
            }),
            "an earlier checkout's agent at the same path"
        );
    }

    #[test]
    fn boards_sort_newest_first_with_unknown_activity_last_and_stable_ties() {
        let mut items = [
            (None, "a"),
            (Some(5 * HOUR), "b"),
            (Some(HOUR), "d"),
            (Some(HOUR), "c"),
        ]
        .map(|(age, name)| {
            let mut card = item();
            card.key = ItemKey::Branch(name.into());
            card.last_activity = age.and_then(|age| activity(age, ActivitySource::Commit));
            card
        });
        sort(&mut items);
        assert_eq!(
            items.iter().map(Item::label).collect::<Vec<_>>(),
            ["c", "d", "b", "a"]
        );
    }

    #[test]
    fn ticket_identifiers_are_read_only_from_linear_style_branch_names() {
        for (branch, expected) in [
            ("fix/team-123-display", Some("TEAM-123")),
            ("developer/team-456-search", Some("TEAM-456")),
            ("eng-12", Some("ENG-12")),
            ("fix/browser-refresh-20261019", None),
            ("audit/v2-release-20261020", None),
            ("feat/activity-comments", None),
            ("main", None),
            ("x-1-short-team", None),
            ("team-0123-leading-zero", None),
            ("team-1234567-too-long", None),
        ] {
            assert_eq!(ticket_from_branch(branch).as_deref(), expected, "{branch}");
        }
    }

    #[test]
    fn columns_parse_from_either_separator_and_round_trip() {
        for column in Column::ALL {
            assert_eq!(column.as_str().parse::<Column>(), Ok(column));
            assert_eq!(
                column.as_str().replace('_', "-").parse::<Column>(),
                Ok(column)
            );
        }
        assert!("done".parse::<Column>().is_err());
    }

    #[test]
    fn item_keys_round_trip_through_storage() {
        for key in [
            ItemKey::Branch("fix/a:b".into()),
            ItemKey::Worktree("/repo/.worktrees/x".into()),
        ] {
            assert_eq!(ItemKey::from_storage_key(&key.storage_key()), Some(key));
        }
        assert_eq!(ItemKey::from_storage_key("branch:"), None);
        assert_eq!(ItemKey::from_storage_key("worktree:relative"), None);
        assert_eq!(ItemKey::from_storage_key("other"), None);
    }

    #[test]
    fn stale_days_come_from_settings_with_a_default_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        assert_eq!(stale_after_days(&path), Ok(DEFAULT_STALE_AFTER_DAYS));
        std::fs::write(&path, "theme = \"dark\"\n").unwrap();
        assert_eq!(stale_after_days(&path), Ok(DEFAULT_STALE_AFTER_DAYS));
        std::fs::write(&path, "work_stale_after_days = 14\n").unwrap();
        assert_eq!(stale_after_days(&path), Ok(14));
        std::fs::write(&path, "work_stale_after_days = 0\n").unwrap();
        assert_eq!(stale_after_days(&path), Ok(1));
        std::fs::write(&path, "work_stale_after_days = \"soon\"\n").unwrap();
        assert!(stale_after_days(&path).is_err());
    }
}
