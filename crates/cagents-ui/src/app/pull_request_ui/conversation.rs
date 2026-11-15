//! The work the inspected conversation did: the pull requests it worked on
//! and the worktrees it edited, as the daemon infers them from its transcript
//! and keeps current with GitHub, and the pull requests it only mentioned.
//!
//! Each piece of work is one row. Choosing a row makes it what the
//! inspector's changes and files show, so they always belong to a named
//! piece of work rather than to whichever card happens to be focused.
//!
//! A chat pane's inspector keeps one [`State`] for the pane's conversation.

use super::*;
use crate::design::list;
use crate::work::{
    Check, ConversationPullRequest, ConversationPullRequests, ConversationWorktree, Relation,
    TranscriptCoverage,
};
use cosmic::iced::futures::SinkExt;

/// A chosen pull request lists this many of its checks, the ones most in
/// need of attention first, before pointing at the rest on GitHub.
const CHECKS_SHOWN: usize = 6;
/// While GitHub cannot be reached, a row checked longer ago than this says
/// when it was.
const STALE_AFTER_MS: u64 = 5 * 60 * 1000;

/// A piece of work the inspector can show the changes and files of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkKey {
    PullRequest {
        repository: GitHubRepoKey,
        number: u64,
    },
    Worktree(PathBuf),
}

/// One row of the Work section.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Work<'a> {
    /// A pull request the conversation worked on, with the worktree where it
    /// edited its branch when there is one.
    PullRequest {
        row: &'a ConversationPullRequest,
        worktree: Option<&'a ConversationWorktree>,
    },
    /// A worktree the conversation edited whose branch has no pull request
    /// it worked on.
    Worktree(&'a ConversationWorktree),
    /// The worktree the conversation runs in, listed when nothing else it
    /// worked on is checked out, so its changes stay a click away.
    Here(&'a Path),
}

impl<'a> Work<'a> {
    pub(crate) fn key(&self) -> WorkKey {
        match self {
            Self::PullRequest { row, .. } => WorkKey::PullRequest {
                repository: row.repository.clone(),
                number: row.number,
            },
            Self::Worktree(worktree) => WorkKey::Worktree(worktree.path.clone()),
            Self::Here(path) => WorkKey::Worktree(path.to_path_buf()),
        }
    }

    fn is(&self, key: &WorkKey) -> bool {
        match (self, key) {
            (Self::PullRequest { row, .. }, WorkKey::PullRequest { repository, number }) => {
                row.number == *number && row.repository.same_repository(repository)
            }
            (Self::Worktree(_) | Self::Here(_), WorkKey::Worktree(path)) => {
                self.worktree() == Some(path.as_path())
            }
            _ => false,
        }
    }

    /// The local worktree its changes and files are read from.
    pub(crate) fn worktree(self) -> Option<&'a Path> {
        match self {
            Self::PullRequest { worktree, .. } => worktree.map(|worktree| worktree.path.as_path()),
            Self::Worktree(worktree) => Some(&worktree.path),
            Self::Here(path) => Some(path),
        }
    }

    /// The pull request it is, when it is one.
    pub(crate) fn pull_request(self) -> Option<&'a ConversationPullRequest> {
        match self {
            Self::PullRequest { row, .. } => Some(row),
            Self::Worktree(_) | Self::Here(_) => None,
        }
    }

    /// What its changes are called in the inspector: the branch they are
    /// on, or the pull request when it has no worktree here.
    pub(crate) fn scope_label(&self) -> String {
        match self {
            Self::PullRequest {
                worktree: Some(worktree),
                ..
            }
            | Self::Worktree(worktree) => worktree_branch(worktree),
            Self::PullRequest { row, .. } => format!("#{}", row.number),
            Self::Here(path) => path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        }
    }
}

fn worktree_branch(worktree: &ConversationWorktree) -> String {
    worktree.branch.clone().unwrap_or_else(|| {
        worktree
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
    })
}

#[derive(Debug, Clone)]
pub enum Watch {
    Listed {
        conversation: TerminalId,
        revision: u64,
        list: Arc<ConversationPullRequests>,
        /// The first list on a connection: its revision starts afresh.
        first: bool,
    },
    Lost {
        conversation: TerminalId,
        error: String,
    },
}

#[derive(Debug, Default)]
pub struct State {
    pub(super) conversation: Option<TerminalId>,
    revision: u64,
    list: Option<Arc<ConversationPullRequests>>,
    /// The previous conversation listed pull requests, so the section stays
    /// while this one's list is on its way rather than flickering away.
    listed_before: bool,
}

/// The work the user chose to show in each conversation they chose any in,
/// so returning to one shows what they left it showing, in whichever pane
/// shows it; until then an inspector shows the work where the conversation
/// runs. Every pane's inspector shares one, so a pane that closes, or a
/// conversation reopened in another pane, loses nothing chosen in it.
#[derive(Debug, Default)]
pub struct Choices(HashMap<TerminalId, Chosen>);

/// Work the user chose, and whether its details are unfolded beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chosen {
    key: WorkKey,
    details: bool,
}

impl State {
    pub(super) fn update(&mut self, watch: Watch) {
        match watch {
            Watch::Listed {
                conversation,
                revision,
                list,
                first,
            } => {
                if self.conversation != Some(conversation) || (!first && revision <= self.revision)
                {
                    return;
                }
                self.revision = revision;
                self.list = Some(list);
            }
            // The last list stays: it is what was true, and the daemon's own
            // health is reported elsewhere.
            Watch::Lost {
                conversation,
                error,
            } => {
                if self.conversation == Some(conversation) {
                    self.listed_before = false;
                }
                tracing::debug!(%conversation, %error, "conversation pull requests unavailable")
            }
        }
    }

    /// The inspected conversation's latest list, with its revision.
    pub(super) fn latest_list(&self) -> Option<(TerminalId, u64, &Arc<ConversationPullRequests>)> {
        Some((self.conversation?, self.revision, self.list.as_ref()?))
    }

    /// Follows the pane's conversation, and reports whether it changed.
    pub(super) fn retarget(&mut self, conversation: Option<TerminalId>) -> bool {
        if self.conversation == conversation {
            return false;
        }
        *self = Self {
            conversation,
            listed_before: conversation.is_some()
                && (!self.rows().is_empty() || !self.worktrees().is_empty()),
            ..Self::default()
        };
        true
    }

    /// The pull requests the conversation only mentioned, as rows of the
    /// Mentioned section, when the settings list them. None is checked out
    /// for this conversation, so none has a worktree.
    pub(crate) fn mentioned(&self, show_mentions: bool) -> Vec<Work<'_>> {
        if !show_mentions {
            return Vec::new();
        }
        self.rows()
            .iter()
            .filter(|row| !row.relation.worked_on())
            .map(|row| Work::PullRequest {
                row,
                worktree: None,
            })
            .collect()
    }

    /// Every piece of work the conversation did, pull requests first. `here`
    /// is the worktree the conversation runs in, listed when none of the
    /// work is in a worktree Projects lists, which `openable` says, so its
    /// changes stay a click away.
    pub(crate) fn work<'a>(
        &'a self,
        here: Option<&'a Path>,
        openable: &dyn Fn(&Path) -> bool,
    ) -> Vec<Work<'a>> {
        let mut work: Vec<Work<'a>> = self
            .rows()
            .iter()
            .filter(|row| row.relation.worked_on())
            .map(|row| Work::PullRequest {
                row,
                worktree: self.worktree_of(row),
            })
            .collect();
        work.extend(self.worktrees_alone().map(Work::Worktree));
        if let Some(here) = here
            && !work.is_empty()
            && !work
                .iter()
                .any(|work| work.worktree().is_some_and(openable))
        {
            work.push(Work::Here(here));
        }
        work
    }

    /// The work the inspector shows: the one chosen while it is listed in
    /// `work` or `mentioned`, else the one of `work` where the conversation
    /// runs, else the first checked out in a worktree Projects lists, which
    /// `openable` says, so the panel has something to show. A mention is
    /// shown only once chosen.
    pub(crate) fn shown<'w>(
        &self,
        choices: &Choices,
        work: &'w [Work<'w>],
        mentioned: &'w [Work<'w>],
        here: Option<&Path>,
        openable: &dyn Fn(&Path) -> bool,
    ) -> Option<Work<'w>> {
        self.chosen(choices)
            .and_then(|chosen| {
                work.iter()
                    .chain(mentioned)
                    .find(|work| work.is(&chosen.key))
            })
            .or_else(|| {
                here.and_then(|here| work.iter().find(|work| work.worktree() == Some(here)))
            })
            .or_else(|| {
                work.iter()
                    .find(|work| work.worktree().is_some_and(openable))
            })
            .or_else(|| work.first())
            .copied()
    }

    fn chosen<'c>(&self, choices: &'c Choices) -> Option<&'c Chosen> {
        choices.0.get(&self.conversation?)
    }

    /// Chooses `key` to show, unfolding its details. Choosing it again while
    /// it is `shown_open` folds or unfolds them instead.
    pub(super) fn choose(&self, choices: &mut Choices, key: WorkKey, shown_open: bool) {
        let Some(conversation) = self.conversation else {
            return;
        };
        match choices.0.get_mut(&conversation) {
            Some(chosen) if chosen.key == key && shown_open => chosen.details = !chosen.details,
            _ => {
                choices
                    .0
                    .insert(conversation, Chosen { key, details: true });
            }
        }
    }

    /// Whether `work` was chosen and its details are unfolded. Work shown
    /// only because the conversation runs there stays folded.
    fn details_open(&self, choices: &Choices, work: Work<'_>) -> bool {
        self.chosen(choices)
            .is_some_and(|chosen| chosen.details && work.is(&chosen.key))
    }

    pub(super) fn rows(&self) -> &[ConversationPullRequest] {
        self.list
            .as_deref()
            .map_or(&[], |list| list.pull_requests.as_slice())
    }

    /// The worktrees the conversation edited, most recently edited first.
    pub(super) fn worktrees(&self) -> &[ConversationWorktree] {
        self.list
            .as_deref()
            .map_or(&[], |list| list.worktrees.as_slice())
    }

    /// The edited worktree whose branch `row` heads, shown on the row.
    fn worktree_of(&self, row: &ConversationPullRequest) -> Option<&ConversationWorktree> {
        let list = self.list.as_deref()?;
        if !row.relation.worked_on()
            || !list
                .repository
                .as_ref()
                .is_some_and(|repository| repository.same_repository(&row.repository))
        {
            return None;
        }
        list.worktrees
            .iter()
            .find(|worktree| worktree.pull_request == Some(row.number))
    }

    /// The edited worktrees no listed row shows: those whose branch heads
    /// no pull request the conversation worked on.
    pub(super) fn worktrees_alone(&self) -> impl Iterator<Item = &ConversationWorktree> + '_ {
        let shown = self
            .rows()
            .iter()
            .filter_map(|row| self.worktree_of(row))
            .map(|worktree| worktree.path.as_path())
            .collect::<Vec<_>>();
        self.worktrees()
            .iter()
            .filter(move |worktree| !shown.contains(&worktree.path.as_path()))
    }

    /// Whether the Work section lists anything, or the previous
    /// conversation did and this one's list has not arrived.
    pub(super) fn lists_work(&self) -> bool {
        match &self.list {
            Some(_) => {
                self.rows().iter().any(|row| row.relation.worked_on())
                    || !self.worktrees().is_empty()
            }
            None => self.listed_before,
        }
    }

    /// Whether the conversation mentioned any pull request it did not work
    /// on.
    pub(super) fn mentions_any(&self) -> bool {
        self.rows().iter().any(|row| !row.relation.worked_on())
    }
}

/// The daemon's lists for one conversation, from a thread that holds the
/// subscription. Dropping the stream hangs the connection up, so the daemon
/// stops reading the conversation for this window.
pub(super) fn subscription(conversation: TerminalId) -> Subscription<Watch> {
    Subscription::run_with(conversation, |conversation| stream(*conversation))
}

fn stream(conversation: TerminalId) -> impl cosmic::iced::futures::Stream<Item = Watch> {
    cosmic::iced::stream::channel(4, async move |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let hangup = Arc::new(Mutex::new(None));
        let held = hangup.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-conversation-pull-requests".into())
            .spawn(move || {
                while !sender.is_closed() {
                    let watched = watch(conversation, &sender, &held);
                    if let Some(hangup) = held.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        hangup.hang_up();
                    }
                    let delay = match watched {
                        Ok(()) => crate::daemon_health::retry_delay(),
                        Err(error) => {
                            // A conversation this daemon does not host will
                            // not appear on it soon.
                            let delay = match &error {
                                crate::control::ControlError::Remote { .. } => {
                                    Duration::from_secs(30)
                                }
                                _ => crate::daemon_health::retry_delay(),
                            };
                            if sender
                                .send_blocking(Watch::Lost {
                                    conversation,
                                    error: error.to_string(),
                                })
                                .is_err()
                            {
                                break;
                            }
                            delay
                        }
                    };
                    std::thread::sleep(delay);
                }
            });
        if let Err(error) = spawned {
            let _ = output
                .send(Watch::Lost {
                    conversation,
                    error: error.to_string(),
                })
                .await;
            return;
        }
        let _hangup = HangUpOnDrop(hangup);
        while let Ok(watch) = receiver.recv().await {
            if output.send(watch).await.is_err() {
                break;
            }
        }
    })
}

fn watch(
    conversation: TerminalId,
    sender: &async_channel::Sender<Watch>,
    hangup: &Mutex<Option<crate::control::Hangup>>,
) -> Result<(), crate::control::ControlError> {
    // Never start a daemon merely to watch it.
    let socket = crate::terminal::ipc::daemon_socket_path()
        .map_err(crate::control::ControlError::Connect)?;
    let mut client = crate::control::ControlClient::connect(&socket)?;
    *hangup.lock().unwrap_or_else(|p| p.into_inner()) = Some(client.hangup()?);
    if sender.is_closed() {
        return Ok(());
    }
    let (mut revision, list) = client.watch_conversation_pull_requests(conversation)?;
    let mut first = true;
    let mut list = list;
    loop {
        if sender
            .send_blocking(Watch::Listed {
                conversation,
                revision,
                list: Arc::new(list),
                first,
            })
            .is_err()
        {
            return Ok(());
        }
        first = false;
        (revision, list) = client.next_conversation_pull_requests(revision)?;
    }
}

struct HangUpOnDrop(Arc<Mutex<Option<crate::control::Hangup>>>);

impl Drop for HangUpOnDrop {
    fn drop(&mut self) {
        if let Some(hangup) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            hangup.hang_up();
        }
    }
}

/// Between the list's rows.
const ROW_SPACING: f32 = space::XXS;
/// Above and below the list's notes.
const NOTE_PADDING: f32 = space::XS;

/// `card`'s list of `work`, the Work section's or the Mentioned section's, with
/// the `shown` row expanded, and marked while `shown_open` says its changes
/// or files are open beneath, then the list's notes when `with_note` asks
/// for them. `openable` says whether Projects lists a worktree, so it can be
/// shown.
///
/// Every row is a [`list_row`](crate::ui_controls::list_row) on the shared
/// list grid, so [`height`] can say exactly how tall the list is.
pub(super) fn view<'a>(
    card: CardId,
    state: &State,
    choices: &Choices,
    work: &[Work<'_>],
    shown: Option<&WorkKey>,
    shown_open: bool,
    with_note: bool,
    openable: &dyn Fn(&Path) -> bool,
) -> Element<'a, Action> {
    let mut children: Vec<Element<'a, Action>> = Vec::with_capacity(work.len() + 2);
    let list = state.list.as_deref();
    for item in work {
        let chosen = shown.is_some_and(|key| item.is(key));
        let details = chosen && state.details_open(choices, *item);
        children.push(work_row(
            card,
            list,
            *item,
            chosen && shown_open,
            details,
            openable,
        ));
        if details && let Some(row) = item.pull_request() {
            children.push(chosen_details(row));
        }
    }
    if with_note && let Some(note) = list.and_then(note) {
        children.push(note);
    }
    widget::column::with_children(children)
        .spacing(ROW_SPACING)
        .width(Length::Fill)
        .into()
}

/// How tall [`view`] draws the list, and how much of that the shown work's
/// expansion adds.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct Height {
    pub content: f32,
    pub expanded: f32,
}

/// The height [`view`] gives the same arguments, from the row heights it
/// draws. Each note line is counted once; a note that wraps scrolls.
pub(super) fn height(
    state: &State,
    choices: &Choices,
    work: &[Work<'_>],
    shown: Option<Work<'_>>,
    with_note: bool,
) -> Height {
    let expanded = shown
        .filter(|shown| work.iter().any(|work| work.is(&shown.key())))
        .filter(|shown| state.details_open(choices, *shown))
        .and_then(|shown| shown.pull_request())
        .map_or(0.0, |row| {
            ROW_SPACING + detail_rows(row) as f32 * design::DENSE_ROW_HEIGHT
        });
    let mut heights: Vec<f32> = vec![list::TWO_LINE_HEIGHT; work.len()];
    let notes = if with_note {
        state.list.as_deref().map_or(0, note_lines)
    } else {
        0
    };
    if notes > 0 {
        heights.push(notes as f32 * list::CAPTION_LINE + 2.0 * NOTE_PADDING);
    }
    let rows: f32 = heights.iter().sum();
    let between = heights.len().saturating_sub(1) as f32 * ROW_SPACING;
    Height {
        content: rows + between + expanded,
        expanded,
    }
}

/// Why the list may be incomplete or out of date, when it may be.
fn note<'a>(list: &ConversationPullRequests) -> Option<Element<'a, Action>> {
    let line = |text: String| {
        design::caption(text)
            .line_height(iced::widget::text::LineHeight::Absolute(
                list::CAPTION_LINE.into(),
            ))
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
    };
    let mut lines = Vec::new();
    if let Some(error) = &list.forge_error {
        lines.push(crate::interaction::tooltip(
            line("GitHub could not be reached; states may be out of date.".to_owned()),
            error.clone(),
        ));
    }
    if let Some(coverage) = coverage_note(&list.transcript) {
        lines.push(line(coverage).into());
    }
    (!lines.is_empty()).then(|| {
        widget::container(widget::column::with_children(lines).width(Length::Fill))
            .padding([NOTE_PADDING, list::INSET])
            .into()
    })
}

/// How many lines [`note`] draws, unwrapped.
fn note_lines(list: &ConversationPullRequests) -> usize {
    usize::from(list.forge_error.is_some()) + usize::from(coverage_note(&list.transcript).is_some())
}

/// What the list says about how much of the conversation it was read from,
/// when not all of it.
fn coverage_note(coverage: &TranscriptCoverage) -> Option<String> {
    match coverage {
        TranscriptCoverage::Partial => {
            Some("Read from the latest part of this long conversation.".to_owned())
        }
        TranscriptCoverage::Unavailable { reason } => Some(reason.clone()),
        TranscriptCoverage::Complete
        | TranscriptCoverage::Pending
        | TranscriptCoverage::Unsupported => None,
    }
}

/// A theme's style for one fact on a row's second line.
type Tone = fn(&cosmic::Theme) -> iced::widget::text::Style;

/// One fact on a row's second line: a few words, each part in its own tone
/// when it has one.
struct Fact(Vec<(String, Option<Tone>)>);

impl Fact {
    fn plain(text: impl Into<String>) -> Self {
        Self(vec![(text.into(), None)])
    }

    fn toned(text: impl Into<String>, tone: Tone) -> Self {
        Self(vec![(text.into(), Some(tone))])
    }

    /// Lines added and removed, each in its tone.
    fn change_counts(additions: u64, deletions: u64) -> Option<Self> {
        let counts = crate::repository::line_change_counts(Some(additions), Some(deletions));
        let parts: Vec<(String, Option<Tone>)> = counts
            .split(' ')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let tone: Tone = if part.starts_with('+') {
                    design::added_text
                } else {
                    design::removed_text
                };
                (part.to_owned(), Some(tone))
            })
            .collect();
        (!parts.is_empty()).then_some(Self(parts))
    }

    fn text(&self) -> String {
        self.0
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A row's facts on its second line with a dot between each, cut short
/// with an ellipsis when the row is too narrow, so the facts that matter
/// most go first.
fn facts_line<'a>(facts: &[Fact]) -> Element<'a, Action> {
    let theme = cosmic::theme::active();
    let mut spans: Vec<iced::widget::text::Span<'static, ()>> = Vec::with_capacity(facts.len() * 3);
    for (index, fact) in facts.iter().enumerate() {
        if index > 0 {
            spans.push(iced::widget::span(" · "));
        }
        for (part, (text, tone)) in fact.0.iter().enumerate() {
            if part > 0 {
                spans.push(iced::widget::span(" "));
            }
            spans.push(
                iced::widget::span(text.clone())
                    .color_maybe(tone.and_then(|tone| tone(&theme).color)),
            );
        }
    }
    iced::widget::rich_text(spans)
        .size(design::TEXT_DESCRIPTION)
        .line_height(iced::widget::text::LineHeight::Absolute(
            list::CAPTION_LINE.into(),
        ))
        .wrapping(iced::widget::text::Wrapping::None)
        .ellipsize(iced::widget::text::Ellipsize::End(
            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
        ))
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .width(Length::Fill)
        .into()
}

/// What a worktree's row says about it: its uncommitted changes and the
/// lines they change.
fn worktree_facts(worktree: &ConversationWorktree, facts: &mut Vec<Fact>) {
    // The daemon's count includes untracked files, which a worktree row in
    // Projects may leave out.
    match worktree.changed_paths {
        Some(0) => facts.push(Fact::plain("No uncommitted changes")),
        Some(count) => facts.push(Fact::toned(
            format!("{count} uncommitted"),
            design::warning_text,
        )),
        None => {}
    }
    if let Some(diff) = worktree.diff {
        facts.extend(Fact::change_counts(diff.additions, diff.deletions));
    }
}

/// What a pull request's row says about it, the facts most in need of
/// attention first: whose it is when not this repository's, how current
/// it is, what its worktree has not committed yet, its size, and why it is
/// listed. Its checks and review are [`pull_request_marks`].
fn pull_request_facts(
    list: Option<&ConversationPullRequests>,
    row: &ConversationPullRequest,
    worktree: Option<&ConversationWorktree>,
) -> Vec<Fact> {
    let mut facts = Vec::with_capacity(6);
    // Another repository's number would crowd the title, so it goes
    // beneath it. Not yet known, the row is named by that number already.
    if !own_repository(list, row) && row.pull_request.is_some() {
        facts.push(Fact::plain(repository_label(&row.repository)));
    }
    if row.pull_request.is_none() {
        facts.push(Fact::plain("State unknown"));
        facts.push(Fact::plain(reason(row)));
        return facts;
    }
    if let Some(list) = list
        && list.forge_error.is_some()
        && let Some(at) = row.observed_at_unix_ms
        && list.generated_at_unix_ms.saturating_sub(at) > STALE_AFTER_MS
    {
        facts.push(Fact::plain(format!(
            "as of {}",
            crate::work_ui::age(list.generated_at_unix_ms, at, true)
        )));
    }
    // Work not pushed yet is not in the pull request's size beside it.
    if let Some(count) = worktree
        .and_then(|worktree| worktree.changed_paths)
        .filter(|count| *count > 0)
    {
        facts.push(Fact::toned(
            format!("{count} uncommitted"),
            design::warning_text,
        ));
    }
    if let Some(diff) = row.diff {
        facts.extend(Fact::change_counts(diff.additions, diff.deletions));
    }
    facts.push(Fact::plain(reason(row)));
    facts
}

/// An open pull request's checks and review as glyphs, as the board marks
/// them, each with what it says in words.
fn pull_request_marks<'a>(row: &ConversationPullRequest) -> Vec<(Element<'a, Action>, String)> {
    let mut marks = Vec::with_capacity(2);
    let Some(pull_request) = row
        .pull_request
        .as_ref()
        .filter(|pull_request| pull_request.state == crate::work::PullRequestState::Open)
    else {
        return marks;
    };
    if let Some(checks) =
        crate::ui_controls::ChecksSummary::of(row.checks.iter().map(|check| check.state))
    {
        marks.push((
            crate::ui_controls::checks_glyph(&checks, design::COMPACT_ICON_SIZE),
            checks.name(),
        ));
    }
    if let Some(review) = pull_request.review {
        marks.push((
            crate::ui_controls::pull_request_review_glyph(review, design::COMPACT_ICON_SIZE),
            crate::ui_controls::review_label(review).to_owned(),
        ));
    }
    marks
}

/// What a row of the Work list draws and says.
struct RowParts<'a> {
    glyph: Element<'a, Action>,
    name: String,
    /// The name read out: a pull request's adds its state and number, which
    /// its glyph and trailing number draw.
    spoken: String,
    trailing: Option<String>,
    marks: Vec<(Element<'a, Action>, String)>,
    facts: Vec<Fact>,
}

impl RowParts<'_> {
    /// The row's accessible name. The facts line may be cut short, so it
    /// says them all. Pressing a shown pull request again folds or unfolds
    /// its details, which nothing on the row draws, so it says which.
    fn described(&self, work: Work<'_>, marked: bool, details: bool) -> String {
        let spoken = &self.spoken;
        let shown = match work.pull_request() {
            Some(row) if marked => {
                let what = if lists_checks(row) {
                    "its checks"
                } else {
                    "its details"
                };
                let verb = if details { "hide" } else { "list" };
                format!("{spoken}, shown below, press to {verb} {what}")
            }
            None if marked => format!("{spoken}, shown below"),
            _ => format!("Show {spoken}"),
        };
        std::iter::once(shown)
            .chain(self.marks.iter().map(|(_, said)| said.clone()))
            .chain(self.facts.iter().map(Fact::text))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn row_parts<'a>(
    list: Option<&ConversationPullRequests>,
    work: Work<'_>,
    openable: &dyn Fn(&Path) -> bool,
) -> RowParts<'a> {
    let mut facts: Vec<Fact> = Vec::with_capacity(6);
    // Work this window cannot show says so before anything else, since it
    // is why choosing it shows no changes.
    if let Some(path) = work.worktree()
        && !openable(path)
    {
        facts.push(Fact::plain("Not in Projects"));
    }
    let mut marks: Vec<(Element<'a, Action>, String)> = Vec::new();
    let mut spoken = None;
    let (glyph, name, trailing): (Element<'a, Action>, String, Option<String>) = match work {
        Work::PullRequest { row, worktree } => {
            marks = pull_request_marks(row);
            let state = row
                .pull_request
                .as_ref()
                .map(crate::sidebar::WorktreePullRequestState::from_work);
            facts.extend(pull_request_facts(list, row, worktree));
            let number = number_label(list, row);
            let name = row
                .pull_request
                .as_ref()
                .map_or_else(|| number.clone(), |pull_request| pull_request.title.clone());
            // Not yet known, a pull request is named by its number already.
            // Another repository's name is one of the facts, so the number
            // read out leaves it off.
            spoken = state
                .map(|state| format!("{name}, {}, #{}", state.label().to_lowercase(), row.number));
            (
                crate::ui_controls::pull_request_state_glyph(state, design::ROW_ICON_SIZE),
                name,
                Some(format!("#{}", row.number)),
            )
        }
        Work::Worktree(worktree) => {
            worktree_facts(worktree, &mut facts);
            facts.push(Fact::plain(if worktree.by_subagent {
                "Edited by a subagent"
            } else {
                "Edited"
            }));
            (
                crate::ui_controls::passive_icon(
                    crate::sidebar::worktree_icon(),
                    design::ROW_ICON_SIZE,
                ),
                worktree_branch(worktree),
                None,
            )
        }
        Work::Here(_) => {
            facts.push(Fact::plain("Where this conversation runs"));
            (
                crate::ui_controls::passive_icon(
                    crate::sidebar::worktree_icon(),
                    design::ROW_ICON_SIZE,
                ),
                work.scope_label(),
                None,
            )
        }
    };
    RowParts {
        glyph,
        spoken: spoken.unwrap_or_else(|| name.clone()),
        name,
        trailing,
        marks,
        facts,
    }
}

fn work_row<'a>(
    card: CardId,
    list: Option<&ConversationPullRequests>,
    work: Work<'_>,
    marked: bool,
    details: bool,
    openable: &dyn Fn(&Path) -> bool,
) -> Element<'a, Action> {
    let parts = row_parts(list, work, openable);
    let described = parts.described(work, marked, details);
    let RowParts {
        glyph,
        name,
        trailing,
        marks,
        facts,
        ..
    } = parts;
    let mut row = crate::ui_controls::list_row(None, name)
        .icon_element(glyph)
        .caption_element(facts_line(&facts))
        .name(described)
        .selected(marked)
        .on_press(Action::Choose {
            card,
            key: work.key(),
        });
    let mut idle: Vec<Element<'a, Action>> = marks.into_iter().map(|(mark, _)| mark).collect();
    if let Some(trailing) = trailing {
        idle.push(
            design::caption(trailing)
                .size(design::TEXT_DESCRIPTION)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
    }
    let idle = (!idle.is_empty()).then(|| -> Element<'a, Action> {
        widget::row::with_children(idle)
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .into()
    });
    // A pull request's marks give way to opening it in the browser while
    // the pointer is on its row or the button has focus; pressing the rest
    // of the row still shows its details here.
    let browser = work
        .pull_request()
        .and_then(|row| Some((row.number, Url::parse(&row.url()).ok()?)));
    let region = crate::ui_controls::hover_reveal::Region::new();
    match (browser, idle) {
        (Some((number, url)), idle) => {
            let open =
                crate::ui_controls::icon_button(Icon::PopOut, format!("Open #{number} in browser"))
                    .on_press(Action::OpenInBrowser(url));
            row = row.trailing(crate::ui_controls::hover_reveal::reveal(
                idle.unwrap_or_else(|| iced::widget::Space::new().into()),
                open,
                &region,
            ));
        }
        (None, Some(idle)) => row = row.trailing(idle),
        (None, None) => {}
    }
    let row: Element<'a, Action> = row.into();
    // A worktree's row names its folder; a pull request's would cover the
    // checks beneath it.
    match work {
        Work::Worktree(worktree) => {
            crate::interaction::tooltip(row, worktree.path.display().to_string())
        }
        Work::Here(path) => crate::interaction::tooltip(row, path.display().to_string()),
        Work::PullRequest { .. } => crate::ui_controls::hover_reveal::region(row, &region),
    }
}

/// How many dense rows a chosen pull request adds beneath its row: its
/// checks, a row for the rest when there are more, and its Open row.
fn detail_rows(row: &ConversationPullRequest) -> usize {
    let checks = if lists_checks(row) {
        row.checks.len().min(CHECKS_SHOWN) + usize::from(row.checks.len() > CHECKS_SHOWN)
    } else {
        0
    };
    checks + 1
}

/// Whether a chosen pull request lists its checks: only an open one's
/// still matter.
fn lists_checks(row: &ConversationPullRequest) -> bool {
    !row.checks.is_empty()
        && row
            .pull_request
            .as_ref()
            .is_some_and(|pull_request| pull_request.state == crate::work::PullRequestState::Open)
}

/// What a chosen pull request adds beneath its row: its checks, the ones
/// most in need of attention first, and a way to open it. Its rows hang
/// from the row's label column, so they read as belonging to it.
fn chosen_details<'a>(row: &ConversationPullRequest) -> Element<'a, Action> {
    let mut rows: Vec<Element<'a, Action>> = Vec::with_capacity(detail_rows(row));
    if lists_checks(row) {
        let mut checks: Vec<&Check> = row.checks.iter().collect();
        checks.sort_by_key(|check| check.state);
        for check in checks.iter().take(CHECKS_SHOWN) {
            rows.push(
                crate::ui_controls::list_row(None, check.name.clone())
                    .icon_element(tinted_icon(crate::ui_controls::check_state_look(
                        check.state,
                    )))
                    .name(format!(
                        "Open the {} check, {}",
                        check.name,
                        crate::ui_controls::check_state_label(check.state)
                    ))
                    .dense()
                    .on_press_maybe(Url::parse(&check.url).ok().map(Action::OpenCheck))
                    .into(),
            );
        }
        if checks.len() > CHECKS_SHOWN {
            let more = checks.len() - CHECKS_SHOWN;
            rows.push(
                crate::ui_controls::list_row(Some(Icon::PopOut), format!("{more} more on GitHub"))
                    .name(format!("Open all {} checks on GitHub", checks.len()))
                    .dense()
                    .on_press_maybe(
                        Url::parse(&format!("{}/checks", row.url()))
                            .ok()
                            .map(Action::OpenCheck),
                    )
                    .into(),
            );
        }
    }
    rows.push(
        crate::ui_controls::list_row(Some(Icon::PopOut), "Open pull request")
            .name(format!("Open #{}", row.number))
            .dense()
            .on_press_maybe(open(row))
            .into(),
    );
    widget::container(widget::column::with_children(rows).width(Length::Fill))
        .padding(Padding {
            left: list::ICON + list::GAP,
            ..Padding::ZERO
        })
        .width(Length::Fill)
        .into()
}

/// Whether the row's pull request is in the conversation's own repository.
fn own_repository(list: Option<&ConversationPullRequests>, row: &ConversationPullRequest) -> bool {
    list.and_then(|list| list.repository.as_ref())
        .is_some_and(|own| own.same_repository(&row.repository))
}

/// The number a row's pull request goes by: `#12`, after its repository
/// when that is not the conversation's.
fn number_label(list: Option<&ConversationPullRequests>, row: &ConversationPullRequest) -> String {
    if own_repository(list, row) {
        format!("#{}", row.number)
    } else {
        format!("{}#{}", repository_label(&row.repository), row.number)
    }
}

/// `owner/repo` on github.com, with the host only when it is another one.
fn repository_label(repository: &GitHubRepoKey) -> String {
    if repository.host == "github.com" && repository.port.is_none() {
        format!("{}/{}", repository.owner, repository.name)
    } else {
        repository.gh_selector()
    }
}

/// What the conversation did to the row's pull request.
fn reason(row: &ConversationPullRequest) -> String {
    let mut reason = if row.current_branch && row.relation == Relation::CheckedOut {
        // The branch the conversation is on, which it did nothing else to.
        "Current branch".to_owned()
    } else {
        let mut reason = row.relation.label().to_owned();
        if row.current_branch {
            reason.push_str(" · current branch");
        }
        reason
    };
    if row.by_subagent {
        reason.push_str(" by a subagent");
    }
    reason
}

/// Opening the row's pull request, where Projects would open it.
fn open(row: &ConversationPullRequest) -> Option<Action> {
    Url::parse(&row.url()).ok().map(|url| Action::Open {
        repository: row.repository.clone(),
        number: row.number,
        url,
    })
}

/// A check's glyph in its tone, for a row that names it itself.
fn tinted_icon<'a>((icon, tone): (Icon, fn(&cosmic::Theme) -> iced::Color)) -> Element<'a, Action> {
    widget::icon(icon.handle())
        .size(design::COMPACT_ICON_SIZE)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            iced::widget::svg::Style {
                color: Some(tone(theme)),
            }
        })))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{CheckState, PullRequest, PullRequestState};

    fn repo(owner: &str) -> GitHubRepoKey {
        GitHubRepoKey::new("github.com", None, owner, "repo").unwrap()
    }

    fn row(number: u64, relation: Relation) -> ConversationPullRequest {
        ConversationPullRequest {
            repository: repo("me"),
            number,
            relation,
            by_subagent: false,
            current_branch: false,
            pull_request: Some(PullRequest {
                number,
                title: format!("Change {number}"),
                url: format!("https://github.com/me/repo/pull/{number}"),
                state: PullRequestState::Open,
                draft: false,
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

    fn listed(
        conversation: TerminalId,
        revision: u64,
        rows: Vec<ConversationPullRequest>,
        first: bool,
    ) -> Watch {
        Watch::Listed {
            conversation,
            revision,
            list: Arc::new(ConversationPullRequests {
                generated_at_unix_ms: 0,
                repository: Some(repo("me")),
                pull_requests: rows,
                transcript: TranscriptCoverage::Complete,
                forge_error: None,
                worktrees: Vec::new(),
            }),
            first,
        }
    }

    fn check(name: &str, state: CheckState) -> Check {
        Check {
            name: name.to_owned(),
            workflow: String::new(),
            state,
            url: format!("https://github.com/me/repo/actions/{name}"),
        }
    }

    #[test]
    fn lists_follow_the_inspected_conversation_and_its_revisions() {
        let mut state = State::default();
        let conversation = TerminalId::new();
        assert!(state.retarget(Some(conversation)));
        // Retargeting to the conversation it already follows keeps it.
        assert!(!state.retarget(Some(conversation)));
        state.update(listed(
            conversation,
            2,
            vec![row(1, Relation::Created)],
            true,
        ));
        assert_eq!(state.rows().len(), 1);
        // An older revision on the same connection is not newer.
        state.update(listed(conversation, 1, Vec::new(), false));
        assert_eq!(state.rows().len(), 1);
        // A reconnection starts its revisions again.
        state.update(listed(
            conversation,
            1,
            vec![row(1, Relation::Created), row(2, Relation::Mentioned)],
            true,
        ));
        assert_eq!(state.work(None, &|_| true).len(), 1);
        assert!(state.mentioned(false).is_empty(), "unless the settings ask");
        assert_eq!(state.mentioned(true).len(), 1);
        assert!(state.lists_work() && state.mentions_any());
        // Another conversation's list is not this one's.
        state.update(listed(TerminalId::new(), 9, Vec::new(), true));
        assert_eq!(state.rows().len(), 2);
        // The next conversation's section stays while its list is on its way.
        assert!(state.retarget(Some(TerminalId::new())));
        assert!(state.rows().is_empty());
        assert!(state.lists_work());
        assert!(state.retarget(None));
        assert!(state.conversation.is_none());
        assert!(!state.lists_work());
    }

    fn worktree(path: &str, pull_request: Option<u64>) -> ConversationWorktree {
        ConversationWorktree {
            path: PathBuf::from(path),
            branch: Some(path.trim_start_matches('/').to_owned()),
            by_subagent: false,
            changed_paths: Some(1),
            diff: None,
            pull_request,
        }
    }

    /// The height the list reports is the height it lays out at, so its
    /// section neither clips a row nor leaves a gap below the last.
    #[test]
    fn the_reported_height_is_the_laid_out_height() {
        use cosmic::iced::advanced::{layout, widget::Tree};

        let mut state = State::default();
        let conversation = TerminalId::new();
        state.retarget(Some(conversation));
        let mut created = row(1, Relation::Created);
        created.checks = (0..CHECKS_SHOWN + 2)
            .map(|index| check(&format!("check {index}"), CheckState::Passed))
            .chain([check("lint", CheckState::Failed)])
            .collect();
        state.update(listed(
            conversation,
            1,
            vec![
                created,
                row(2, Relation::Pushed),
                row(3, Relation::Mentioned),
                row(4, Relation::Mentioned),
            ],
            true,
        ));
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let keys: Vec<WorkKey> = state.work(None, &|_| true).iter().map(Work::key).collect();
        // Shown work, whether it was chosen and so unfolds its checks, and
        // whether the list is the Mentioned section's rather than Work's.
        for (shown, chosen, mentions) in [
            (None, false, false),
            (Some(0), false, false),
            (Some(0), true, false),
            (Some(0), true, true),
            (Some(1), true, true),
        ] {
            let key = shown.map(|index: usize| keys[index].clone());
            let mut choices = Choices::default();
            if chosen && let Some(key) = &key {
                state.choose(&mut choices, key.clone(), false);
            }
            let work = state.work(None, &|_| true);
            let mentioned = state.mentioned(true);
            let rows = if mentions { &mentioned } else { &work };
            let shown = key
                .as_ref()
                .and_then(|key| work.iter().copied().find(|work| work.is(key)));
            let mut element = view(
                CardId::new(),
                &state,
                &choices,
                rows,
                key.as_ref(),
                true,
                true,
                &|_| true,
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(
                    cosmic::iced::Size::ZERO,
                    cosmic::iced::Size::new(280.0, 2000.0),
                ),
            );
            let height = height(&state, &choices, rows, shown, true);
            assert!(
                (node.size().height - height.content).abs() < 0.5,
                "{key:?} chosen {chosen} in mentions {mentions}: laid out {} but reported {height:?}",
                node.size().height,
            );
        }
    }

    /// Returning to a conversation shows the work chosen in it, not the
    /// work chosen in the one visited in between.
    #[test]
    fn each_conversation_keeps_the_work_chosen_in_it() {
        let mut state = State::default();
        let mut choices = Choices::default();
        let (first, second) = (TerminalId::new(), TerminalId::new());
        let rows = || vec![row(1, Relation::Created), row(2, Relation::Pushed)];
        let show = |state: &mut State, conversation| {
            state.retarget(Some(conversation));
            state.update(listed(conversation, 1, rows(), true));
        };
        let shown = |state: &State, choices: &Choices| {
            let work = state.work(None, &|_| true);
            state
                .shown(choices, &work, &[], None, &|_| true)
                .map(|work| work.key())
        };
        let number = |number| WorkKey::PullRequest {
            repository: repo("me"),
            number,
        };
        show(&mut state, first);
        assert_eq!(shown(&state, &choices), Some(number(1)));
        state.choose(&mut choices, number(2), false);
        show(&mut state, second);
        assert_eq!(
            shown(&state, &choices),
            Some(number(1)),
            "a new conversation starts at its default"
        );
        show(&mut state, first);
        assert_eq!(shown(&state, &choices), Some(number(2)));
    }

    /// Work shown because the conversation runs there stays folded; choosing
    /// it unfolds its checks, and choosing it again while its changes are
    /// open folds them.
    #[test]
    fn choosing_shown_work_again_folds_its_details() {
        let mut state = State::default();
        let mut choices = Choices::default();
        let conversation = TerminalId::new();
        state.retarget(Some(conversation));
        state.update(listed(
            conversation,
            1,
            vec![row(1, Relation::Created)],
            true,
        ));
        let key = WorkKey::PullRequest {
            repository: repo("me"),
            number: 1,
        };
        let open = |state: &State, choices: &Choices| {
            let work = state.work(None, &|_| true);
            state.details_open(choices, work[0])
        };
        assert!(!open(&state, &choices), "shown by default, folded");
        state.choose(&mut choices, key.clone(), true);
        assert!(open(&state, &choices));
        state.choose(&mut choices, key.clone(), true);
        assert!(!open(&state, &choices), "chosen again while shown folds");
        // Choosing it while its changes are closed is asking to see it all.
        state.choose(&mut choices, key.clone(), false);
        assert!(open(&state, &choices));
    }

    /// The conversation's own worktree stays listed while none of its work
    /// can be shown here, so its changes are never out of reach.
    #[test]
    fn where_the_chat_runs_is_listed_while_no_work_is_in_projects() {
        let mut state = State::default();
        let conversation = TerminalId::new();
        state.retarget(Some(conversation));
        let Watch::Listed { list, .. } = listed(conversation, 1, Vec::new(), true) else {
            unreachable!()
        };
        let mut list = Arc::unwrap_or_clone(list);
        list.worktrees = vec![worktree("/elsewhere", None)];
        state.update(Watch::Listed {
            conversation,
            revision: 1,
            list: Arc::new(list),
            first: true,
        });
        let here = Path::new("/scratch");
        let listed = |openable: &dyn Fn(&Path) -> bool| {
            state
                .work(Some(here), openable)
                .iter()
                .map(Work::key)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            listed(&|path| path != Path::new("/elsewhere")),
            [
                WorkKey::Worktree(PathBuf::from("/elsewhere")),
                WorkKey::Worktree(PathBuf::from("/scratch")),
            ]
        );
        assert_eq!(
            listed(&|_| true),
            [WorkKey::Worktree(PathBuf::from("/elsewhere"))]
        );
    }

    #[test]
    fn work_lists_pull_requests_then_lone_worktrees_and_shows_where_the_chat_runs() {
        let mut state = State::default();
        let mut choices = Choices::default();
        let conversation = TerminalId::new();
        state.retarget(Some(conversation));
        let Watch::Listed { list, .. } = listed(
            conversation,
            1,
            vec![
                row(1, Relation::Created),
                row(2, Relation::Pushed),
                row(3, Relation::Mentioned),
            ],
            true,
        ) else {
            unreachable!()
        };
        let mut list = Arc::unwrap_or_clone(list);
        list.worktrees = vec![worktree("/feature", Some(2)), worktree("/scratch", None)];
        state.update(Watch::Listed {
            conversation,
            revision: 1,
            list: Arc::new(list),
            first: true,
        });
        let here = Path::new("/scratch");
        let work = state.work(Some(here), &|_| true);
        // Mentions are not work, and #2's worktree is shown on its row.
        let keys: Vec<WorkKey> = work.iter().map(Work::key).collect();
        assert_eq!(
            keys,
            vec![
                WorkKey::PullRequest {
                    repository: repo("me"),
                    number: 1
                },
                WorkKey::PullRequest {
                    repository: repo("me"),
                    number: 2
                },
                WorkKey::Worktree(PathBuf::from("/scratch")),
            ]
        );
        assert_eq!(work[1].worktree(), Some(Path::new("/feature")));
        // Until one is chosen, the work where the chat runs is shown.
        assert_eq!(
            state
                .shown(&choices, &work, &[], Some(here), &|_| true)
                .map(|work| work.key()),
            Some(WorkKey::Worktree(PathBuf::from("/scratch")))
        );
        // Else the first checked out here, not a pull request with nothing
        // local to show.
        assert_eq!(
            state
                .shown(&choices, &work, &[], None, &|_| true)
                .map(|work| work.key()),
            Some(keys[1].clone())
        );
        // A worktree Projects does not list has nothing for the panel to
        // show, so the default passes over it.
        assert_eq!(
            state
                .shown(&choices, &work, &[], None, &|path| path
                    != Path::new("/feature"))
                .map(|work| work.key()),
            Some(keys[2].clone())
        );
        state.choose(&mut choices, keys[0].clone(), false);
        let work = state.work(Some(here), &|_| true);
        assert_eq!(
            state
                .shown(&choices, &work, &[], Some(here), &|_| true)
                .map(|work| work.key()),
            Some(keys[0].clone())
        );
        // A choice no longer listed gives way rather than showing nothing.
        state.choose(
            &mut choices,
            WorkKey::Worktree(PathBuf::from("/gone")),
            false,
        );
        let work = state.work(Some(here), &|_| true);
        assert_eq!(
            state
                .shown(&choices, &work, &[], Some(here), &|_| true)
                .map(|work| work.key()),
            Some(WorkKey::Worktree(PathBuf::from("/scratch")))
        );
    }

    #[test]
    fn where_the_chat_runs_is_listed_only_when_nothing_else_is_checked_out() {
        let mut state = State::default();
        let choices = Choices::default();
        let conversation = TerminalId::new();
        state.retarget(Some(conversation));
        state.update(listed(
            conversation,
            1,
            vec![row(1, Relation::Created)],
            true,
        ));
        let here = Path::new("/main");
        let work = state.work(Some(here), &|_| true);
        assert_eq!(work.len(), 2);
        assert!(matches!(work[1], Work::Here(path) if path == here));
        assert_eq!(work[1].scope_label(), "main");
        assert_eq!(
            state
                .shown(&choices, &work, &[], Some(here), &|_| true)
                .map(|work| work.key()),
            Some(WorkKey::Worktree(PathBuf::from("/main")))
        );
        // A conversation that worked on nothing has no Work at all.
        state.update(listed(
            conversation,
            2,
            vec![row(1, Relation::Mentioned)],
            false,
        ));
        assert!(state.work(Some(here), &|_| true).is_empty());
    }

    /// An open pull request's checks and review are glyphs that say in
    /// words what they mark; a closed one's no longer matter.
    #[test]
    fn an_open_pull_request_marks_its_checks_and_review() {
        let mut open = row(1, Relation::Created);
        open.checks = vec![
            check("lint", CheckState::Failed),
            check("test", CheckState::Passed),
            check("build", CheckState::Passed),
        ];
        open.pull_request.as_mut().unwrap().review = Some(crate::work::Review::ChangesRequested);
        let said: Vec<String> = pull_request_marks(&open)
            .into_iter()
            .map(|(_, said)| said)
            .collect();
        assert_eq!(said, ["Checks: 1 failing, 2 passed", "Changes requested"]);
        let mut merged = open.clone();
        merged.pull_request.as_mut().unwrap().state = PullRequestState::Merged;
        assert!(pull_request_marks(&merged).is_empty());
        open.checks.clear();
        open.pull_request.as_mut().unwrap().review = None;
        assert!(pull_request_marks(&open).is_empty());
    }

    /// A row's accessible name says what its glyphs and number draw, and
    /// what pressing it will do.
    #[test]
    fn a_rows_name_says_its_state_number_marks_and_facts() {
        let list = ConversationPullRequests {
            generated_at_unix_ms: 0,
            repository: Some(repo("me")),
            pull_requests: Vec::new(),
            transcript: TranscriptCoverage::Complete,
            forge_error: None,
            worktrees: Vec::new(),
        };
        let list = Some(&list);
        let mut draft = row(9, Relation::Edited);
        draft.pull_request.as_mut().unwrap().draft = true;
        draft.pull_request.as_mut().unwrap().review = Some(crate::work::Review::Required);
        draft.checks = vec![check("test", CheckState::Pending)];
        let mut checked_out = worktree("/repo/fix-cache", Some(9));
        checked_out.changed_paths = Some(2);
        let work = Work::PullRequest {
            row: &draft,
            worktree: Some(&checked_out),
        };
        let parts = row_parts(list, work, &|_| true);
        assert_eq!(
            parts.described(work, false, false),
            "Show Change 9, draft, #9, Checks: 1 running, Review required, 2 uncommitted, Edited"
        );
        assert_eq!(
            parts.described(work, true, false),
            "Change 9, draft, #9, shown below, press to list its checks, \
             Checks: 1 running, Review required, 2 uncommitted, Edited"
        );
        assert_eq!(
            parts.described(work, true, true),
            "Change 9, draft, #9, shown below, press to hide its checks, \
             Checks: 1 running, Review required, 2 uncommitted, Edited"
        );

        // A merged one lists no checks, so pressing it shows only its link.
        let mut merged = row(3, Relation::Created);
        merged.pull_request.as_mut().unwrap().state = PullRequestState::Merged;
        merged.checks = vec![check("test", CheckState::Passed)];
        let work = Work::PullRequest {
            row: &merged,
            worktree: None,
        };
        assert_eq!(
            row_parts(list, work, &|_| true).described(work, true, true),
            "Change 3, merged, #3, shown below, press to hide its details, Created"
        );

        // Another repository is named once, among the facts.
        let mut upstream = row(4, Relation::Reviewed);
        upstream.repository = repo("upstream");
        let work = Work::PullRequest {
            row: &upstream,
            worktree: None,
        };
        assert_eq!(
            row_parts(list, work, &|_| true).described(work, false, false),
            "Show Change 4, open, #4, upstream/repo, Reviewed"
        );
        upstream.pull_request = None;
        let work = Work::PullRequest {
            row: &upstream,
            worktree: None,
        };
        assert_eq!(
            row_parts(list, work, &|_| true).described(work, false, false),
            "Show upstream/repo#4, State unknown, Reviewed"
        );

        let edited = worktree("/repo/notes", None);
        let work = Work::Worktree(&edited);
        assert_eq!(
            row_parts(list, work, &|_| false).described(work, true, false),
            "repo/notes, shown below, Not in Projects, 1 uncommitted, Edited"
        );
    }

    #[test]
    fn a_row_says_why_it_is_listed_and_opens_its_pull_request() {
        let mut current = row(1, Relation::CheckedOut);
        current.current_branch = true;
        assert_eq!(reason(&current), "Current branch");
        current.relation = Relation::Pushed;
        assert_eq!(reason(&current), "Pushed · current branch");
        let mut delegated = row(2, Relation::Created);
        delegated.by_subagent = true;
        assert_eq!(reason(&delegated), "Created by a subagent");
        let list = ConversationPullRequests {
            generated_at_unix_ms: 0,
            repository: Some(repo("me")),
            pull_requests: Vec::new(),
            transcript: TranscriptCoverage::Complete,
            forge_error: None,
            worktrees: Vec::new(),
        };
        assert_eq!(number_label(Some(&list), &delegated), "#2");
        delegated.repository = repo("upstream");
        delegated.pull_request = None;
        assert_eq!(number_label(Some(&list), &delegated), "upstream/repo#2");
        assert!(
            matches!(open(&delegated), Some(Action::Open { repository, number: 2, url })
            if repository == repo("upstream")
                && url.as_str() == "https://github.com/upstream/repo/pull/2")
        );
        delegated.repository = GitHubRepoKey::new("ghe.example", None, "me", "repo").unwrap();
        assert_eq!(
            number_label(Some(&list), &delegated),
            "ghe.example/me/repo#2"
        );
    }
}
