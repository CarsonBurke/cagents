//! Pull request links in a transcript, shown as what they name.
//!
//! An agent writes `https://github.com/acme/widgets/pull/20` far more often
//! than it says what that pull request is, and a run of such addresses is
//! noise that wraps mid-word. A link that is exactly a pull request's address
//! reads instead as GitHub writes a reference: the pull request's state
//! glyph, `#20` (or `owner/repo#20` from another repository) and its title,
//! shortened to a glance, which wraps only between its title's words. The address itself stays the
//! link's destination and is what Copy gives back.
//!
//! Labels cost no request of their own. They come from what the window
//! already holds: the pull requests its projects list, which their GitHub
//! polls keep current, and, for the conversation the inspector shows, the
//! daemon's list of the pull requests it names (see
//! [`crate::work::ConversationPullRequests`]). A link to a pull request
//! neither holds reads as its reference alone, without a state glyph. Once
//! the inspector stops watching a conversation nothing keeps its list
//! current, so a pull request only that list names keeps its title but shows
//! no state, rather than one that may since have changed.
//!
//! The labels are an input to a chat's Markdown, like the syntax theme. Each
//! prepared message keeps the pull requests it links to, so a change to the
//! labels prepares again only the messages naming one that now reads
//! otherwise, without reading any message's text.

use crate::github_identity::{GitHubRepoKey, PullRequestKey};
use crate::model::ProjectId;
use crate::sidebar::SidebarProject;
use crate::ui_controls::WorktreePullRequestState;
use crate::work::ConversationPullRequests;
#[cfg(test)]
use crate::work::PullRequestState;
use cosmic::iced;
use std::{collections::HashMap, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

/// The most of a title a link shows, in characters. About a third of a
/// reading column: enough to tell pull requests apart, short enough that a
/// sentence naming three of them still reads as a sentence.
const TITLE_CHARACTERS: usize = 40;

/// Where a link's state glyph is drawn. Two figure spaces are a little over
/// an em in common faces, the glyph's size and a gap, and unlike an em space
/// they never let a line break between the glyph and the number after it.
pub(super) const GLYPH_SPACE: &str = "\u{2007}\u{2007}";

/// Joins a reference to its title's first word, so a label never leaves its
/// glyph and number alone at the end of a line. The title's other words wrap
/// as any link's do, which keeps a long label readable in a table cell or a
/// narrow pane rather than broken inside a word.
const NO_BREAK: char = '\u{a0}';

/// Closes a break a label's own characters would allow, as after the slash in
/// `acme/tools` or a hyphen in a title, without taking any room.
pub(super) const WORD_JOINER: char = '\u{2060}';

/// What a link's leading glyph says about its pull request: its state, as
/// every list shows one, or `None` while GitHub has not said or could not be
/// asked.
pub(super) type Glyph = Option<WorktreePullRequestState>;

/// A link's label: its glyph, and the text after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Label {
    pub(super) glyph: Glyph,
    pub(super) text: String,
    /// The reference and the whole title, which the label may shorten.
    summary: String,
}

impl Label {
    /// What hovering the link says: what it is, in full, then its state in
    /// words, which a small glyph only suggests, and where it goes.
    pub(super) fn tip(&self, destination: &str) -> String {
        let state = self
            .glyph
            .map_or("State unknown", WorktreePullRequestState::label);
        format!("{}\n{state} · {destination}", self.summary)
    }
}

/// How one chat's pull request links read: from the pull requests the
/// window's projects already list, and from the inspector's list for the
/// conversation when it has shown one. Nothing here asks GitHub or the daemon
/// for anything; a link to a pull request neither holds reads as its
/// reference alone, without a state glyph.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Labels {
    /// The chat's own repository, whose pull requests are named by number
    /// alone.
    repository: Option<Repository>,
    /// What each of the window's projects lists, shared by every chat.
    board: Arc<[Arc<Facts>]>,
    /// What the inspector last listed for this conversation, which also
    /// holds pull requests no project lists.
    conversation: Arc<Facts>,
    /// Whether the inspector still watches the conversation. Once it stops,
    /// nothing keeps its list current: a title rarely changes, but a state
    /// does, so a pull request only that list holds keeps its title and
    /// shows its state as unknown.
    watched: bool,
}

/// What a link reads of the pull request it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Known<'a> {
    title: &'a str,
    glyph: Glyph,
}

/// What is known of pull requests, by repository and number.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Facts(HashMap<Id, Fact>);

impl Facts {
    /// What a conversation's list from the daemon says of its pull requests.
    pub(super) fn of_conversation(list: &ConversationPullRequests) -> Self {
        Self(
            list.pull_requests
                .iter()
                .filter_map(|row| {
                    let pull_request = row.pull_request.as_ref()?;
                    Some((
                        Id {
                            repository: Repository::of(&row.repository),
                            number: row.number,
                        },
                        Fact {
                            title: displayable(&pull_request.title),
                            glyph: Some(WorktreePullRequestState::from_work(pull_request)),
                        },
                    ))
                })
                .collect(),
        )
    }

    /// What a project lists, each pull request by its address.
    fn of_project(project: &SidebarProject) -> Self {
        Self(
            project
                .display_pull_requests()
                .iter()
                .filter_map(|row| {
                    let key = PullRequestKey::from_url(row.url.as_str()).ok()?;
                    Some((
                        Id::of(&key),
                        Fact {
                            title: displayable(&row.title),
                            glyph: row.state,
                        },
                    ))
                })
                .collect(),
        )
    }

    fn get(&self, id: &Id) -> Option<&Fact> {
        self.0.get(id)
    }
}

/// The window's index of what its projects list of pull requests, which every
/// chat labels its links from. A project's part is rebuilt only when its list
/// or repository changes, which comparing a revision per project detects, so
/// one project's poll reads none of the others' pull requests.
#[derive(Debug, Default)]
pub(in crate::app) struct Board {
    projects: Vec<Listed>,
    /// Each project's facts in sidebar order, shared by every chat's labels.
    facts: Arc<[Arc<Facts>]>,
    /// Each project's repository, in the same order, as the labels last read.
    repositories: Vec<(ProjectId, Option<Repository>)>,
    generation: u64,
}

/// What one project lists, and what it was built from.
#[derive(Debug)]
struct Listed {
    project: ProjectId,
    revision: u64,
    key: Option<GitHubRepoKey>,
    repository: Option<Repository>,
    facts: Arc<Facts>,
}

impl Board {
    /// Rebuilds the part of the index for each project whose pull requests or
    /// repository changed since it was built, and reports whether anything
    /// the index says did.
    pub(super) fn refresh<'a>(
        &mut self,
        projects: &[SidebarProject],
        repository: impl Fn(ProjectId) -> Option<&'a GitHubRepoKey>,
    ) -> bool {
        let current = |listed: &Listed, project: &SidebarProject| {
            listed.project == project.id
                && listed.revision == project.pull_requests_revision()
                && listed.key.as_ref() == repository(project.id)
        };
        if self.projects.len() == projects.len()
            && self
                .projects
                .iter()
                .zip(projects)
                .all(|(listed, project)| current(listed, project))
        {
            return false;
        }
        let mut previous = std::mem::take(&mut self.projects)
            .into_iter()
            .map(|listed| (listed.project, listed))
            .collect::<HashMap<_, _>>();
        self.projects = projects
            .iter()
            .map(|project| {
                let old = match previous.remove(&project.id) {
                    Some(old) if current(&old, project) => return old,
                    old => old,
                };
                let key = repository(project.id).cloned();
                let facts = Facts::of_project(project);
                Listed {
                    project: project.id,
                    revision: project.pull_requests_revision(),
                    repository: key.as_ref().map(Repository::of),
                    key,
                    facts: match old {
                        Some(old) if *old.facts == facts => old.facts,
                        _ => Arc::new(facts),
                    },
                }
            })
            .collect();
        let reads_alike = self.facts.len() == self.projects.len()
            && self.repositories.len() == self.projects.len()
            && self
                .projects
                .iter()
                .zip(self.facts.iter().zip(&self.repositories))
                .all(|(listed, (facts, (project, repository)))| {
                    Arc::ptr_eq(facts, &listed.facts)
                        && *project == listed.project
                        && *repository == listed.repository
                });
        if reads_alike {
            return false;
        }
        self.facts = self
            .projects
            .iter()
            .map(|listed| listed.facts.clone())
            .collect();
        self.repositories = self
            .projects
            .iter()
            .map(|listed| (listed.project, listed.repository.clone()))
            .collect();
        self.generation += 1;
        true
    }

    /// Counts the rebuilds that changed what the index says.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// The labels for a chat in `project`, with what the inspector listed for
    /// its conversation, whose states hold only while `watched`.
    pub(super) fn labels(
        &self,
        project: Option<ProjectId>,
        inspected: &Inspected,
        watched: bool,
    ) -> Labels {
        Labels {
            repository: project
                .and_then(|project| {
                    self.repositories
                        .iter()
                        .find(|(listed, _)| *listed == project)
                })
                .and_then(|(_, repository)| repository.clone())
                .or_else(|| inspected.repository.clone()),
            board: self.facts.clone(),
            conversation: inspected.facts.clone(),
            watched,
        }
    }
}

/// What the inspector last listed for one chat's conversation, kept in
/// revision order. It stays when the inspector moves to another conversation:
/// it is what was last true, as the inspector keeps a list it lost.
#[derive(Debug, Default)]
pub(super) struct Inspected {
    /// The conversation the list is of.
    conversation: Option<crate::model::TerminalId>,
    revision: u64,
    /// The list taken, so taking it again costs nothing.
    list: Option<Arc<ConversationPullRequests>>,
    repository: Option<Repository>,
    facts: Arc<Facts>,
}

impl Inspected {
    /// The conversation the list taken is of, if any.
    pub(super) fn conversation(&self) -> Option<crate::model::TerminalId> {
        self.conversation
    }

    /// Takes `conversation`'s list unless it is older than the one taken, and
    /// reports whether what it says of the links changed. A first list after
    /// a reconnect starts the daemon's count afresh, and one of another
    /// conversation replaces whatever was taken.
    pub(super) fn accept(
        &mut self,
        conversation: crate::model::TerminalId,
        revision: u64,
        first: bool,
        list: &Arc<ConversationPullRequests>,
    ) -> bool {
        let another = self.conversation != Some(conversation);
        if another {
            *self = Self {
                conversation: Some(conversation),
                ..Self::default()
            };
        } else if (!first && revision <= self.revision)
            || self
                .list
                .as_ref()
                .is_some_and(|taken| Arc::ptr_eq(taken, list))
        {
            return false;
        }
        self.revision = revision;
        self.list = Some(list.clone());
        let repository = list.repository.as_ref().map(Repository::of);
        let facts = Facts::of_conversation(list);
        if !another && self.repository == repository && *self.facts == facts {
            return false;
        }
        self.repository = repository;
        self.facts = Arc::new(facts);
        true
    }
}

/// A repository as GitHub compares one: owner and name ignore case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Repository {
    host: String,
    port: Option<u16>,
    owner: String,
    name: String,
}

impl Repository {
    fn of(key: &GitHubRepoKey) -> Self {
        Self {
            host: key.host.to_ascii_lowercase(),
            port: key.port,
            owner: key.owner.to_lowercase(),
            name: key.name.to_lowercase(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Id {
    repository: Repository,
    number: u64,
}

impl Id {
    fn of(key: &PullRequestKey) -> Self {
        Self {
            repository: Repository::of(&key.repository),
            number: key.number,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Fact {
    title: String,
    glyph: Glyph,
}

impl Labels {
    /// What is known of `id`: what a project lists, which its poll keeps
    /// current, before what the inspector last listed.
    fn fact(&self, id: &Id) -> Option<Known<'_>> {
        if let Some(fact) = self.board.iter().find_map(|facts| facts.get(id)) {
            return Some(Known {
                title: &fact.title,
                glyph: fact.glyph,
            });
        }
        self.conversation.get(id).map(|fact| Known {
            title: &fact.title,
            glyph: fact.glyph.filter(|_| self.watched),
        })
    }

    /// Whether a link to `key` reads the same under `other` as under these
    /// labels, without building either label.
    pub(super) fn reads_alike(&self, other: &Self, key: &PullRequestKey) -> bool {
        let id = Id::of(key);
        if (self.repository.as_ref() == Some(&id.repository))
            != (other.repository.as_ref() == Some(&id.repository))
        {
            return false;
        }
        if self.board.len() != other.board.len() {
            return self.fact(&id) == other.fact(&id);
        }
        // Labels after a poll share every source but the polled project's, and
        // a shared source settles a link it holds with one look. The
        // inspector's list reads alike only while both agree it is watched.
        let shared_conversation = self.watched == other.watched;
        let theirs = other.board.iter().chain([&other.conversation]);
        let sources = self.board.iter().chain([&self.conversation]).zip(theirs);
        for (index, (mine, theirs)) in sources.enumerate() {
            let watched_alike = index < self.board.len() || shared_conversation;
            if watched_alike && Arc::ptr_eq(mine, theirs) {
                if mine.get(&id).is_some() {
                    return true;
                }
            } else if mine.get(&id).is_some() || theirs.get(&id).is_some() {
                return self.fact(&id) == other.fact(&id);
            }
        }
        true
    }

    /// How a link to `key` reads: `#20 Retry stalled daemon handshakes`, or
    /// `acme/tools#20 …` from another repository, or the reference alone
    /// while its title is unknown.
    pub(super) fn label(&self, key: &PullRequestKey) -> Label {
        let id = Id::of(key);
        let mut text = if self.repository.as_ref() == Some(&id.repository) {
            format!("#{}", key.number)
        } else {
            format!(
                "{}/{}#{}",
                key.repository.owner, key.repository.name, key.number
            )
        };
        let fact = self.fact(&id);
        let mut summary = text.clone();
        if let Some(title) = fact.map(|fact| fact.title.split_whitespace().collect::<Vec<_>>())
            && !title.is_empty()
        {
            summary.push(' ');
            summary.push_str(&title.join(" "));
        }
        let mut rest = String::new();
        if let Some(title) = fact
            .map(|fact| shortened(fact.title, TITLE_CHARACTERS))
            .filter(|title| !title.is_empty())
        {
            let (first, others) = title.split_once(' ').unwrap_or((&title, ""));
            text.push(NO_BREAK);
            text.push_str(first);
            if !others.is_empty() {
                rest.push(' ');
                rest.push_str(others);
            }
        }
        Label {
            glyph: fact.and_then(|fact| fact.glyph),
            text: unbroken(&text) + &rest,
            summary,
        }
    }
}

/// `text` with a [`WORD_JOINER`] wherever a line could otherwise break inside
/// it, by the same rules the text layout wraps with, so a label always moves
/// to the next line whole.
fn unbroken(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());
    let mut from = 0;
    for (at, _) in unicode_linebreak::linebreaks(text) {
        if at == text.len() {
            break;
        }
        joined.push_str(&text[from..at]);
        joined.push(WORD_JOINER);
        from = at;
    }
    joined.push_str(&text[from..]);
    joined
}

/// `title` without the characters that would act on the sentence around it
/// rather than show: controls, and bidirectional overrides and isolates that
/// could reorder the agent's words.
fn displayable(title: &str) -> String {
    title
        .chars()
        .filter(|&ch| {
            // Line breaks and tabs still separate words.
            (!ch.is_control() || ch.is_whitespace())
                && !matches!(ch, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// The pull request a link names, when its destination is exactly a pull
/// request's address. A link into one, such as its files or a comment, keeps
/// its address: shortening it to the pull request would hide where it goes.
pub(super) fn pull_request(destination: &str) -> Option<PullRequestKey> {
    // The cheap test keeps every other link off the URL parser.
    if !destination.contains("/pull/") {
        return None;
    }
    PullRequestKey::from_url(destination).ok()
}

/// `text` as Find should read it: each address of a pull request as the
/// label it shows, so a search for a title or `#20` reaches a message whose
/// source only holds the address. `None` when nothing in `text` is relabelled.
pub(super) fn rendition(text: &str, labels: &Labels) -> Option<String> {
    if !text.contains("/pull/") {
        return None;
    }
    let mut read = String::with_capacity(text.len());
    let mut relabelled = false;
    let mut at = 0;
    while let Some(found) = text[at..].find(['`', 'h']).map(|index| at + index) {
        let rest = &text[found..];
        if rest.starts_with('`') {
            // Code, in a span or a fence, shows its addresses as written.
            let fence = &rest[..rest.len() - rest.trim_start_matches('`').len()];
            let end = rest[fence.len()..]
                .find(fence)
                .map_or(found + fence.len(), |close| found + 2 * fence.len() + close);
            read.push_str(&text[at..end]);
            at = end;
            continue;
        }
        if !rest.starts_with("https://") {
            read.push_str(&text[at..=found]);
            at = found + 1;
            continue;
        }
        read.push_str(&text[at..found]);
        // Where an address in prose, an autolink or a link destination ends.
        let end = rest
            .find(|ch: char| {
                ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | '`' | ')' | ']' | '|')
            })
            .unwrap_or(rest.len());
        let address = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', '*']);
        // A link shows its own words, which the source already holds, and so
        // does a reference-style link whose definition this is.
        let before = &text[..found];
        let line = before[before.rfind('\n').map_or(0, |at| at + 1)..].trim();
        let destination = before.ends_with("](")
            || before.ends_with("](<")
            || (line.starts_with('[') && line.ends_with("]:"));
        match pull_request(address).filter(|_| !destination) {
            Some(key) => {
                read.push_str(&labels.label(&key).text);
                relabelled = true;
                at = found + address.len();
            }
            None => {
                read.push_str(&rest[..end]);
                at = found + end;
            }
        }
    }
    read.push_str(&text[at..]);
    relabelled.then_some(read)
}

/// The pull requests a message's text links to, by address, in order and
/// without repeats.
pub(super) fn named(text: &str) -> Vec<PullRequestKey> {
    if !text.contains("/pull/") {
        return Vec::new();
    }
    let mut keys = crate::pull_request_signals::pull_request_urls(text);
    deduplicate(&mut keys);
    keys
}

/// `keys` without repeats of a pull request, in order.
pub(super) fn deduplicate(keys: &mut Vec<PullRequestKey>) {
    if keys.len() < 2 {
        return;
    }
    let mut seen = std::collections::HashSet::new();
    keys.retain(|key| seen.insert(Id::of(key)));
}

/// `title`, whitespace collapsed into single spaces, cut at a word within
/// `limit` characters where one is near, with an ellipsis.
fn shortened(title: &str, limit: usize) -> String {
    let words = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.graphemes(true).count() <= limit {
        return words;
    }
    let cut = words
        .grapheme_indices(true)
        .nth(limit - 1)
        .map_or(words.len(), |(at, _)| at);
    let mut kept = &words[..cut];
    // Cut at the last word boundary unless that loses over half of what fits;
    // a cut that falls between words already is one.
    if !words[cut..].starts_with(' ')
        && let Some(space) = kept.rfind(' ')
        && kept[..space].graphemes(true).count() * 2 >= limit
    {
        kept = &kept[..space];
    }
    let kept = kept.trim_end_matches(|ch: char| {
        matches!(
            ch,
            ' ' | ',' | ';' | ':' | '-' | '–' | '—' | '(' | '/' | '.'
        )
    });
    format!("{kept}…")
}

/// Draws a link's state glyph into the room its [`GLYPH_SPACE`] takes, as
/// [`crate::ui_controls::pull_request_state_glyph`] draws one in a list.
pub(super) fn draw_glyph(
    renderer: &mut cosmic::Renderer,
    state: WorktreePullRequestState,
    room: iced::Rectangle,
    size: f32,
    theme: &cosmic::Theme,
    clip: iced::Rectangle,
) {
    use iced::advanced::svg::Renderer as _;
    let (icon, color) = (state.icon(), state.color(theme));
    let cosmic::widget::icon::Data::Svg(handle) = icon.handle().data else {
        return;
    };
    // The glyph matches the text's cap height and a little more, and leaves
    // the rest of its room as the gap before the number.
    // A room split across lines is narrower than the glyph, which then
    // overhangs it rather than shrinking.
    let side = (size * 0.9).min(room.height).round();
    let bounds = iced::Rectangle::new(
        iced::Point::new(
            room.x.round(),
            (room.y + (room.height - side) / 2.0).round(),
        ),
        iced::Size::new(side, side),
    );
    renderer.draw_svg(
        iced::advanced::svg::Svg::new(handle).color(color),
        bounds,
        clip,
    );
}

#[cfg(test)]
pub(super) mod fixture {
    use super::*;
    use crate::work::{ConversationPullRequest, PullRequest, Relation, TranscriptCoverage};

    pub(in crate::app::chat_ui) fn repository(owner: &str, name: &str) -> GitHubRepoKey {
        GitHubRepoKey::new("github.com", None, owner, name).unwrap()
    }

    pub(in crate::app::chat_ui) fn row(
        owner: &str,
        name: &str,
        number: u64,
        known: Option<(&str, PullRequestState, bool)>,
    ) -> ConversationPullRequest {
        ConversationPullRequest {
            repository: repository(owner, name),
            number,
            relation: Relation::Mentioned,
            by_subagent: false,
            current_branch: false,
            pull_request: known.map(|(title, state, draft)| PullRequest {
                number,
                title: title.into(),
                url: format!("https://github.com/{owner}/{name}/pull/{number}"),
                state,
                draft,
                head_oid: String::new(),
                contains_local_head: false,
                checks: None,
                review: None,
                updated_at: String::new(),
            }),
            observed_at_unix_ms: None,
            diff: None,
            checks: Vec::new(),
        }
    }

    /// The labels a chat would read with only `list`, the inspector's.
    pub(in crate::app::chat_ui) fn of(list: &ConversationPullRequests) -> Labels {
        Labels {
            repository: list.repository.as_ref().map(Repository::of),
            board: Arc::default(),
            conversation: Arc::new(Facts::of_conversation(list)),
            watched: true,
        }
    }

    pub(in crate::app::chat_ui) fn fixture() -> Labels {
        of(&fixture_list())
    }

    /// An inspector's list naming pull requests in several states.
    pub(in crate::app::chat_ui) fn fixture_list() -> ConversationPullRequests {
        ConversationPullRequests {
            generated_at_unix_ms: 0,
            repository: Some(repository("acme", "widgets")),
            pull_requests: vec![
                row(
                    "acme",
                    "widgets",
                    20,
                    Some((
                        "Retry stalled daemon handshakes",
                        PullRequestState::Open,
                        false,
                    )),
                ),
                row(
                    "acme",
                    "widgets",
                    21,
                    Some(("Trim sidebar row padding", PullRequestState::Merged, false)),
                ),
                row(
                    "acme",
                    "widgets",
                    9,
                    Some((
                        "Cache worktree status between scans",
                        PullRequestState::Open,
                        true,
                    )),
                ),
                row(
                    "Acme",
                    "Tools",
                    4,
                    Some(("Old sidebar experiment", PullRequestState::Closed, false)),
                ),
                row(
                    "acme",
                    "widgets",
                    22,
                    Some((
                        "fix(ui): measure the inspector sections rather than estimate their height",
                        PullRequestState::Open,
                        false,
                    )),
                ),
                row("acme", "widgets", 30, None),
            ],
            transcript: TranscriptCoverage::Complete,
            forge_error: None,
            worktrees: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    /// The conversation the tests' lists are of.
    const CONVERSATION: crate::model::TerminalId =
        crate::model::TerminalId::from_uuid(uuid::Uuid::from_u128(1));
    use super::fixture::{fixture, fixture_list, repository, row};
    use super::*;
    use crate::work::TranscriptCoverage;

    fn key(url: &str) -> PullRequestKey {
        pull_request(url).expect("a pull request address")
    }

    #[test]
    fn only_a_pull_requests_own_address_is_recognized() {
        let found = key("https://github.com/acme/widgets/pull/20");
        assert_eq!(found.number, 20);
        assert_eq!(found.repository.owner, "acme");
        assert!(pull_request("https://github.com/acme/widgets/pull/20/").is_some());
        assert!(pull_request("https://ghe.example.com/acme/widgets/pull/3").is_some());
        for other in [
            "https://github.com/acme/widgets/pull/20/files",
            "https://github.com/acme/widgets/pull/20#issuecomment-1",
            "https://github.com/acme/widgets/pull/20?w=1",
            "https://github.com/acme/widgets/issues/20",
            "https://github.com/acme/widgets/pull/new",
            "https://github.com/acme/widgets/pull/0",
            "http://github.com/acme/widgets/pull/20",
            "src/pull/20.rs:3",
        ] {
            assert!(pull_request(other).is_none(), "{other}");
        }
    }

    #[test]
    fn a_label_names_the_number_state_and_shortened_title() {
        let labels = fixture();
        let open = labels.label(&key("https://github.com/acme/widgets/pull/20"));
        assert_eq!(open.text, "#20\u{a0}Retry stalled daemon handshakes");
        assert_eq!(open.glyph, Some(WorktreePullRequestState::Open));
        let draft = labels.label(&key("https://github.com/acme/widgets/pull/9"));
        assert_eq!(draft.glyph, Some(WorktreePullRequestState::Draft));
        // Owner and name compare without case, as GitHub's do.
        let merged = labels.label(&key("https://github.com/ACME/Widgets/pull/21"));
        assert_eq!(merged.text, "#21\u{a0}Trim sidebar row padding");
    }

    #[test]
    fn a_tip_gives_the_whole_title_the_state_in_words_and_the_address() {
        let labels = fixture();
        let address = "https://github.com/acme/widgets/pull/9";
        // A small glyph only suggests a state; the tip says it.
        assert_eq!(
            labels.label(&key(address)).tip(address),
            format!("#9 Cache worktree status between scans\nDraft · {address}")
        );
        let address = "https://github.com/acme/widgets/pull/22";
        let long = labels.label(&key(address));
        assert!(long.text.ends_with('…'));
        assert_eq!(
            long.tip(address),
            format!(
                "#22 fix(ui): measure the inspector sections rather than estimate their height\nOpen · {address}"
            )
        );
    }

    #[test]
    fn another_repositorys_pull_request_is_named_in_full() {
        let labels = fixture();
        let closed = labels.label(&key("https://github.com/acme/tools/pull/4"));
        assert_eq!(
            closed.text,
            "acme/\u{2060}tools#4\u{a0}Old sidebar experiment"
        );
        assert_eq!(closed.glyph, Some(WorktreePullRequestState::Closed));
        // The same owner and name on another host is another repository.
        let elsewhere = labels.label(&key("https://ghe.example.com/acme/widgets/pull/20"));
        assert_eq!(elsewhere.text, "acme/\u{2060}widgets#20");
    }

    #[test]
    fn an_unknown_pull_request_reads_as_its_reference_never_its_address() {
        let labels = fixture();
        let unknown = labels.label(&key("https://github.com/acme/widgets/pull/30"));
        assert_eq!((unknown.glyph, unknown.text.as_str()), (None, "#30"));
        assert_eq!(
            unknown.tip("https://github.com/acme/widgets/pull/30"),
            "#30\nState unknown · https://github.com/acme/widgets/pull/30"
        );
        // Before any list arrives the conversation's repository is unknown.
        assert_eq!(
            Labels::default()
                .label(&key("https://github.com/acme/widgets/pull/30"))
                .text,
            "acme/\u{2060}widgets#30"
        );
    }

    #[test]
    fn a_long_title_is_cut_at_a_word_with_an_ellipsis() {
        assert_eq!(
            shortened(
                "fix(ui): measure the inspector sections rather than estimate their height",
                40
            ),
            "fix(ui): measure the inspector sections…"
        );
        // A single long word is cut where the limit falls.
        let long = "x".repeat(60);
        assert_eq!(shortened(&long, 40), format!("{}…", "x".repeat(39)));
        // Whitespace inside a title never lets the label wrap.
        assert_eq!(shortened("Two\n  lines", 40), "Two lines");
        // Characters, not bytes, are counted.
        let wide = "é".repeat(40);
        assert_eq!(shortened(&wide, 40), wide);
    }

    #[test]
    fn named_pull_requests_are_listed_once() {
        let keys = named(
            "See https://github.com/acme/widgets/pull/20, \
             [again](https://github.com/acme/widgets/pull/20) and https://github.com/acme/tools/pull/4.",
        );
        assert_eq!(
            keys.iter().map(|key| key.number).collect::<Vec<_>>(),
            [20, 4]
        );
        assert!(named("no links here").is_empty());
    }

    #[test]
    fn an_older_inspector_list_keeps_the_labels_applied() {
        let list = |title: &str| ConversationPullRequests {
            generated_at_unix_ms: 0,
            repository: Some(repository("acme", "widgets")),
            pull_requests: vec![row(
                "acme",
                "widgets",
                20,
                Some((title, PullRequestState::Open, false)),
            )],
            transcript: TranscriptCoverage::Complete,
            forge_error: None,
            worktrees: Vec::new(),
        };
        let mut inspected = Inspected::default();
        let board = Board::default();
        let title = |inspected: &Inspected| {
            board
                .labels(None, inspected, true)
                .label(&key("https://github.com/acme/widgets/pull/20"))
                .text
        };
        assert!(inspected.accept(CONVERSATION, 3, true, &Arc::new(list("Third"))));
        assert!(!inspected.accept(CONVERSATION, 2, false, &Arc::new(list("Second"))));
        assert!(
            !inspected.accept(CONVERSATION, 4, false, &Arc::new(list("Third"))),
            "nothing changed"
        );
        assert_eq!(title(&inspected), "#20\u{a0}Third");
        // A reconnected daemon counts revisions afresh.
        assert!(inspected.accept(CONVERSATION, 1, true, &Arc::new(list("Again"))));
        assert_eq!(title(&inspected), "#20\u{a0}Again");
        // Another conversation's list replaces this one's, whatever its count.
        let other = crate::model::TerminalId::from_uuid(uuid::Uuid::from_u128(2));
        assert!(inspected.accept(other, 1, false, &Arc::new(list("Other"))));
        assert_eq!(inspected.conversation(), Some(other));
        assert_eq!(title(&inspected), "#20\u{a0}Other");
    }

    #[test]
    fn a_poll_rebuilds_only_its_project_and_relabels_only_what_it_changed() {
        let project = |name: &str, numbers: &[(u64, WorktreePullRequestState)]| {
            let id = ProjectId::new();
            let mut sidebar = SidebarProject::new(
                id,
                crate::repository::RepositoryId(id.as_uuid()),
                name.into(),
                format!("/w/{name}").into(),
                Vec::new(),
                Vec::new(),
            );
            sidebar.replace_pull_requests(listing(name, id, numbers));
            sidebar
        };
        let mut projects = [
            project("widgets", &[(20, WorktreePullRequestState::Open)]),
            project("tools", &[(5, WorktreePullRequestState::Open)]),
        ];
        let mut board = Board::default();
        let none = |_| None;
        assert!(board.refresh(&projects, none));
        let before = board.labels(None, &Inspected::default(), false);

        // A poll of the first project that saw #21 open and nothing else.
        let first = projects[0].id;
        projects[0].replace_pull_requests(listing(
            "widgets",
            first,
            &[
                (20, WorktreePullRequestState::Open),
                (21, WorktreePullRequestState::Open),
            ],
        ));
        assert!(board.refresh(&projects, none));
        let after = board.labels(None, &Inspected::default(), false);
        assert!(Arc::ptr_eq(&before.board[1], &after.board[1]), "tools kept");
        let widgets = |number| key(&format!("https://github.com/acme/widgets/pull/{number}"));
        assert!(before.reads_alike(&after, &widgets(20)));
        assert!(!before.reads_alike(&after, &widgets(21)));
        assert!(before.reads_alike(&after, &key("https://github.com/acme/tools/pull/5")));
        assert!(before.reads_alike(&after, &widgets(99)), "known to neither");

        // A pull request both list reads as the first project says, so the
        // second's poll changes nothing it shows.
        let second = projects[1].id;
        projects[1].replace_pull_requests(
            listing("tools", second, &[(5, WorktreePullRequestState::Open)])
                .into_iter()
                .chain(listing(
                    "widgets",
                    second,
                    &[(20, WorktreePullRequestState::Merged)],
                ))
                .collect(),
        );
        assert!(board.refresh(&projects, none));
        let shadowed = board.labels(None, &Inspected::default(), false);
        assert!(after.reads_alike(&shadowed, &widgets(20)));
        assert_eq!(
            shadowed.label(&widgets(20)).glyph,
            Some(WorktreePullRequestState::Open)
        );
    }

    /// What a project's poll lists of `acme/name`.
    fn listing(
        name: &str,
        project: ProjectId,
        numbers: &[(u64, WorktreePullRequestState)],
    ) -> Vec<crate::sidebar::SidebarPullRequest> {
        numbers
            .iter()
            .map(|&(number, state)| {
                let mut row = crate::sidebar::SidebarPullRequest::new(
                    project,
                    number,
                    format!("Change {number}"),
                    format!("https://github.com/acme/{name}/pull/{number}")
                        .parse()
                        .unwrap(),
                    format!("change-{number}"),
                    None,
                    false,
                    crate::sidebar::CheckStatus::Unknown,
                );
                row.state = Some(state);
                row
            })
            .collect()
    }

    #[test]
    fn a_project_listing_labels_a_link_before_the_inspector_does() {
        let project = ProjectId::new();
        let url = "https://github.com/acme/widgets/pull/20";
        let mut row = crate::sidebar::SidebarPullRequest::new(
            project,
            20,
            "Retry stalled daemon handshakes".into(),
            url.parse().unwrap(),
            "retry".into(),
            None,
            false,
            crate::sidebar::CheckStatus::Unknown,
        );
        row.state = Some(WorktreePullRequestState::Merged);
        let mut sidebar = SidebarProject::new(
            project,
            crate::repository::RepositoryId(project.as_uuid()),
            "widgets".into(),
            "/w/widgets".into(),
            Vec::new(),
            Vec::new(),
        );
        let mut board = Board::default();
        let acme_widgets = repository("acme", "widgets");
        let widgets = |_| Some(&acme_widgets);
        assert!(board.refresh(std::slice::from_ref(&sidebar), widgets));
        assert!(
            !board.refresh(std::slice::from_ref(&sidebar), widgets),
            "unchanged"
        );
        let generation = board.generation();
        sidebar.replace_pull_requests(vec![row.clone()]);
        assert!(board.refresh(std::slice::from_ref(&sidebar), widgets));
        assert_eq!(board.generation(), generation + 1);
        // The same list again is no change.
        sidebar.replace_pull_requests(vec![row]);
        assert!(!board.refresh(std::slice::from_ref(&sidebar), widgets));

        let mut inspected = Inspected::default();
        inspected.accept(CONVERSATION, 1, true, &Arc::new(fixture_list()));
        let label = board
            .labels(Some(project), &inspected, true)
            .label(&key(url));
        assert_eq!(label.glyph, Some(WorktreePullRequestState::Merged));
        assert_eq!(label.text, "#20\u{a0}Retry stalled daemon handshakes");
        // A pull request only the inspector listed still reads from it.
        let only_listed = board
            .labels(Some(project), &inspected, true)
            .label(&key("https://github.com/acme/widgets/pull/21"));
        assert_eq!(only_listed.glyph, Some(WorktreePullRequestState::Merged));
        // One neither holds is its reference alone.
        let unknown = board
            .labels(Some(project), &Inspected::default(), false)
            .label(&key("https://github.com/acme/widgets/pull/77"));
        assert_eq!((unknown.glyph, unknown.text.as_str()), (None, "#77"));
    }

    #[test]
    fn a_pull_request_only_an_unwatched_list_names_keeps_its_title_without_a_state() {
        let mut inspected = Inspected::default();
        inspected.accept(CONVERSATION, 1, true, &Arc::new(fixture_list()));
        let board = Board::default();
        let url = "https://github.com/acme/widgets/pull/20";
        let watched = board.labels(None, &inspected, true);
        let unwatched = board.labels(None, &inspected, false);
        let label = unwatched.label(&key(url));
        assert_eq!(label.glyph, None);
        assert_eq!(label.text, watched.label(&key(url)).text);
        assert!(
            label.tip(url).contains("State unknown"),
            "{}",
            label.tip(url)
        );
        assert!(!watched.reads_alike(&unwatched, &key(url)));
        assert!(watched.reads_alike(&board.labels(None, &inspected, true), &key(url)));
        // One no source names reads the same either way.
        let unknown = key("https://github.com/acme/widgets/pull/77");
        assert!(watched.reads_alike(&unwatched, &unknown));
    }

    /// The one paragraph `markdown` parses to, with the fixture's labels.
    fn paragraph(markdown: &str) -> super::super::markdown::Text {
        let content =
            super::super::markdown::Content::parse_labelled(markdown, Arc::new(fixture()));
        let [super::super::markdown::Item::Paragraph(text)] = content.items() else {
            panic!("one paragraph: {:?}", content.items().len());
        };
        text.clone()
    }

    fn copied(text: &super::super::markdown::Text, range: std::ops::Range<usize>) -> String {
        let plain = text.plain();
        match text.inlines() {
            Some(inlines) => inlines.copy(&plain, range),
            None => plain[range].to_owned(),
        }
    }

    const OPEN: &str = "https://github.com/acme/widgets/pull/20";

    #[test]
    fn a_bare_address_reads_as_its_label_and_copies_as_itself() {
        let source = format!("Opened {OPEN}, then <https://github.com/acme/tools/pull/4>.");
        let text = paragraph(&source);
        let plain = text.plain();
        assert_eq!(
            &*plain,
            "Opened \u{2007}\u{2007}#20\u{a0}Retry stalled daemon handshakes, \
             then \u{2007}\u{2007}acme/\u{2060}tools#4\u{a0}Old sidebar experiment."
        );
        // Every part of a label opens its pull request.
        let style =
            crate::design::markdown_settings(&cosmic::Theme::dark(), crate::design::TEXT_PROSE)
                .style;
        let spans = text.spans(style);
        let linked = spans
            .iter()
            .filter(|span| span.link.as_deref() == Some(OPEN))
            .map(|span| span.text.as_ref())
            .collect::<String>();
        assert_eq!(
            linked,
            "\u{2007}\u{2007}#20\u{a0}Retry stalled daemon handshakes"
        );
        let inlines = text.inlines().expect("labelled links");
        assert_eq!(inlines.glyphs.len(), 2);
        assert_eq!(inlines.glyphs[0].1, WorktreePullRequestState::Open);
        // The glyph is drawn over its own room.
        assert_eq!(spans[inlines.glyphs[0].0].text.as_ref(), GLYPH_SPACE);
        assert!(inlines.tip(inlines.glyphs[1].0).unwrap().contains("Closed"));
        // Copy gives the source's addresses back, whole even when only part
        // of a label is selected.
        assert_eq!(
            copied(&text, 0..plain.len()),
            format!("Opened {OPEN}, then https://github.com/acme/tools/pull/4.")
        );
        let within = plain.find("stalled").unwrap();
        assert_eq!(copied(&text, within..within + 3), OPEN);
        assert_eq!(copied(&text, 0..6), "Opened");
    }

    #[test]
    fn a_link_with_its_own_words_keeps_them_after_the_glyph() {
        let text = paragraph(&format!("Fixed in [the **retry** change]({OPEN})."));
        let plain = text.plain();
        assert_eq!(&*plain, "Fixed in \u{2007}\u{2007}the retry change.");
        let style =
            crate::design::markdown_settings(&cosmic::Theme::dark(), crate::design::TEXT_PROSE)
                .style;
        // The author's emphasis survives.
        let spans = text.spans(style);
        assert!(spans.iter().any(|span| {
            span.text.as_ref() == "retry"
                && span.link.as_deref() == Some(OPEN)
                && span
                    .font
                    .is_some_and(|font| font.weight == iced::font::Weight::Bold)
        }));
        // The glyph's room copies as nothing; the words as they are.
        assert_eq!(copied(&text, 0..plain.len()), "Fixed in the retry change.");
        let inlines = text.inlines().unwrap();
        assert!(
            inlines
                .tip(inlines.glyphs[0].0)
                .unwrap()
                .starts_with("#20 Retry")
        );
    }

    #[test]
    fn a_link_with_code_in_its_words_has_one_glyph_before_them() {
        for source in [
            format!("[the `retry` fix]({OPEN})"),
            format!("[`retry` fix]({OPEN})"),
        ] {
            let text = paragraph(&source);
            let plain = text.plain();
            assert!(plain.starts_with(GLYPH_SPACE), "{plain:?}");
            assert_eq!(plain.matches(GLYPH_SPACE).count(), 1, "{plain:?}");
            assert_eq!(text.inlines().unwrap().glyphs.len(), 1);
        }
        // Wholly code, it is the author's literal text.
        let text = paragraph(&format!("[`{OPEN}`]({OPEN})"));
        assert!(text.inlines().is_none());
    }

    #[test]
    fn find_reads_a_labels_joining_spaces_as_spaces() {
        let text = paragraph(OPEN);
        assert!(super::super::find::first(&text.plain(), "#20 retry stalled").is_some());
        let text = paragraph("https://github.com/acme/tools/pull/4");
        assert!(super::super::find::first(&text.plain(), "acme/tools#4 old").is_some());
    }

    #[test]
    fn find_reads_a_messages_addresses_as_their_labels() {
        let labels = fixture();
        assert_eq!(
            rendition(
                &format!(
                    "Opened {OPEN}, see <https://github.com/acme/tools/pull/4> and {OPEN}/files."
                ),
                &labels
            )
            .as_deref(),
            Some(
                "Opened #20\u{a0}Retry stalled daemon handshakes, see \
                 <acme/\u{2060}tools#4\u{a0}Old sidebar experiment> and \
                 https://github.com/acme/widgets/pull/20/files."
            )
        );
        assert_eq!(rendition("No pull requests here.", &labels), None);
        // What shows as written is read as written.
        for source in [
            format!("Run `gh pr view {OPEN}`."),
            format!("```\ngh pr view {OPEN}\n```"),
            format!("See [the retry change]({OPEN})."),
            format!("See [the retry change](<{OPEN}>)."),
            format!("See [the retry change][retry].\n\n[retry]: {OPEN}"),
        ] {
            assert_eq!(rendition(&source, &labels), None, "{source}");
        }
        // A lone backtick is not code.
        assert_eq!(
            rendition(&format!("A ` then {OPEN}"), &labels).as_deref(),
            Some("A ` then #20\u{a0}Retry stalled daemon handshakes")
        );
        assert_eq!(
            rendition("https://github.com/acme/widgets/pull/20/files", &labels),
            None
        );
    }

    #[test]
    fn a_title_cannot_reorder_or_break_the_sentence_around_it() {
        assert_eq!(
            displayable("Fix\u{202e}txt.exe\u{2069} the\u{7}\u{200f} parser\nnow"),
            "Fixtxt.exe the parser\nnow"
        );
    }

    #[test]
    fn a_label_breaks_only_between_its_titles_words() {
        let labels = fixture();
        for (url, words) in [
            (
                "https://github.com/acme/tools/pull/4",
                ["sidebar", "experiment"].as_slice(),
            ),
            (
                "https://github.com/acme/widgets/pull/22",
                ["measure", "the", "inspector", "sections…"].as_slice(),
            ),
        ] {
            let label = labels.label(&key(url)).text;
            let breaks = unicode_linebreak::linebreaks(&label)
                .map(|(at, _)| at)
                .collect::<Vec<_>>();
            // Never after the glyph's number, inside `owner/repo` or in the
            // first word; only before each later word, and at the end.
            let expected = words
                .iter()
                .map(|word| label.find(&format!(" {word}")).unwrap() + 1)
                .chain([label.len()])
                .collect::<Vec<_>>();
            assert_eq!(breaks, expected, "{label:?}");
        }
    }

    #[test]
    fn other_links_and_code_stay_as_written() {
        for source in [
            format!("`{OPEN}`"),
            format!("{OPEN}/files"),
            "https://github.com/acme/widgets/issues/20".to_owned(),
            "[docs](https://example.com/pull/20)".to_owned(),
        ] {
            let text = paragraph(&source);
            assert!(text.inlines().is_none(), "{source}");
            assert!(!text.plain().contains(GLYPH_SPACE), "{source}");
        }
        for (source, shown) in [
            (format!("`{OPEN}`"), OPEN.to_owned()),
            (format!("{OPEN}/files"), format!("{OPEN}/files")),
        ] {
            assert_eq!(&*paragraph(&source).plain(), shown);
        }
    }

    #[test]
    fn an_unknown_pull_request_still_reads_as_a_reference_without_a_glyph() {
        let text = paragraph("https://github.com/acme/widgets/pull/30");
        assert_eq!(&*text.plain(), "#30");
        let inlines = text
            .inlines()
            .expect("the reference stands for the address");
        assert!(inlines.glyphs.is_empty(), "{:?}", inlines.glyphs);
        assert_eq!(
            &*inlines.tips[0].1,
            "#30\nState unknown · https://github.com/acme/widgets/pull/30"
        );
    }

    #[test]
    fn a_selection_across_paragraphs_copies_the_addresses() {
        let content = super::super::markdown::Content::parse_labelled(
            &format!("First {OPEN}\n\nThen [that one]({OPEN})"),
            Arc::new(fixture()),
        );
        let mut builder = super::super::selection::Catalog::builder();
        builder.markdown("message", &[0], content.items());
        let catalog = builder.finish();
        let (start, end) = catalog.endpoints().unwrap();
        assert_eq!(
            catalog.copy(start, end).unwrap(),
            format!("First {OPEN}\n\nThen that one")
        );
    }
}
