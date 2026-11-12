//! The work the inspected conversation did: the pull requests it worked on
//! and the worktrees it edited, as the daemon infers them from its transcript
//! and keeps current with GitHub, and the pull requests it only mentioned.
//!
//! Each piece of work is one row. Choosing a row makes it what the
//! inspector's changes and files show, so they always belong to a named
//! piece of work rather than to whichever card happens to be focused.

use super::*;
use crate::work::{
    Check, CheckState, ConversationPullRequest, ConversationPullRequests, ConversationWorktree,
    Relation, TranscriptCoverage,
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

impl Work<'_> {
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
    pub(crate) fn worktree(&self) -> Option<&Path> {
        match self {
            Self::PullRequest { worktree, .. } => worktree.map(|worktree| worktree.path.as_path()),
            Self::Worktree(worktree) => Some(&worktree.path),
            Self::Here(path) => Some(path),
        }
    }

    /// The pull request it is, when it is one.
    pub(crate) fn pull_request(&self) -> Option<&ConversationPullRequest> {
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
    /// The chat card whose conversation is inspected; it stays inspected
    /// while another card in the workspace is focused.
    pub(super) card: Option<CardId>,
    pub(super) conversation: Option<TerminalId>,
    revision: u64,
    list: Option<Arc<ConversationPullRequests>>,
    /// The previous conversation listed pull requests, so the section stays
    /// while this one's list is on its way rather than flickering away.
    listed_before: bool,
    /// Whether the mentioned pull requests are unfolded.
    pub(super) mentions_open: bool,
    /// The work the user chose to show; until then the inspector shows the
    /// work where the conversation runs.
    chosen: Option<WorkKey>,
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

    /// Follows the inspected conversation, and reports whether it changed.
    pub(super) fn retarget(&mut self, inspected: Option<(CardId, TerminalId)>) -> bool {
        let card = inspected.map(|(card, _)| card);
        let conversation = inspected.map(|(_, conversation)| conversation);
        if self.conversation == conversation {
            self.card = card;
            return false;
        }
        *self = Self {
            card,
            conversation,
            listed_before: conversation.is_some()
                && (!self.rows().is_empty() || !self.worktrees().is_empty()),
            ..Self::default()
        };
        true
    }

    /// The rows the section lists: what the conversation worked on, then,
    /// when asked for, what it only mentioned.
    pub(super) fn listed(
        &self,
        show_mentions: bool,
    ) -> impl Iterator<Item = &ConversationPullRequest> + '_ {
        self.rows()
            .iter()
            .filter(move |row| show_mentions || row.relation.worked_on())
    }

    /// Every piece of work the conversation did, pull requests first. `here`
    /// is the worktree the conversation runs in.
    pub(crate) fn work<'a>(&'a self, here: Option<&'a Path>) -> Vec<Work<'a>> {
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
            && !work.iter().any(|work| work.worktree().is_some())
        {
            work.push(Work::Here(here));
        }
        work
    }

    /// The work the inspector shows: the one chosen while it is listed, else
    /// the one where the conversation runs, else the first checked out here.
    pub(crate) fn shown<'w>(&self, work: &'w [Work<'w>], here: Option<&Path>) -> Option<Work<'w>> {
        self.chosen
            .as_ref()
            .and_then(|key| work.iter().find(|work| work.is(key)))
            .or_else(|| {
                here.and_then(|here| work.iter().find(|work| work.worktree() == Some(here)))
            })
            .or_else(|| work.iter().find(|work| work.worktree().is_some()))
            .or_else(|| work.first())
            .copied()
    }

    pub(super) fn choose(&mut self, key: WorkKey) {
        self.chosen = Some(key);
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

    /// Whether the section lists any work, or mentions when asked for, or
    /// the previous conversation did and this one's list has not arrived.
    pub(super) fn lists_any(&self, show_mentions: bool) -> bool {
        match &self.list {
            Some(_) => self.listed(show_mentions).next().is_some() || !self.worktrees().is_empty(),
            None => self.listed_before,
        }
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

/// The Work section's rows, with the `shown` one marked, then the mentioned
/// pull requests folded beneath them when the settings ask for them.
/// `openable` says whether Projects lists a worktree, so it can be shown.
pub(super) fn view<'a>(
    state: &State,
    work: &[Work<'_>],
    shown: Option<&WorkKey>,
    show_mentions: bool,
    openable: &dyn Fn(&Path) -> bool,
) -> Element<'a, Action> {
    let mut children: Vec<Element<'a, Action>> = Vec::with_capacity(work.len() + 4);
    let list = state.list.as_deref();
    for item in work {
        let chosen = shown.is_some_and(|key| item.is(key));
        children.push(work_row(list, *item, chosen, openable));
    }
    let mentioned: Vec<&ConversationPullRequest> = if show_mentions {
        state
            .rows()
            .iter()
            .filter(|row| !row.relation.worked_on())
            .collect()
    } else {
        Vec::new()
    };
    if !mentioned.is_empty() {
        children.push(mentions_toggle(state.mentions_open, mentioned.len()));
        if state.mentions_open {
            for row in mentioned {
                children.push(mentioned_row(list, row));
            }
        }
    }
    if let Some(note) = list.and_then(note) {
        children.push(note);
    }
    widget::column::with_children(children)
        .spacing(space::XXS)
        .width(Length::Fill)
        .into()
}

/// Why the list may be incomplete or out of date, when it may be.
fn note<'a>(list: &ConversationPullRequests) -> Option<Element<'a, Action>> {
    let mut lines = Vec::new();
    if let Some(error) = &list.forge_error {
        lines.push(crate::interaction::tooltip(
            design::caption("GitHub could not be reached; states may be out of date.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill),
            error.clone(),
        ));
    }
    match &list.transcript {
        TranscriptCoverage::Partial => lines.push(
            design::caption("Read from the latest part of this long conversation.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ),
        TranscriptCoverage::Unavailable { reason } => lines.push(
            design::caption(reason.clone())
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ),
        TranscriptCoverage::Complete
        | TranscriptCoverage::Pending
        | TranscriptCoverage::Unsupported => {}
    }
    (!lines.is_empty()).then(|| {
        widget::container(
            widget::column::with_children(lines)
                .spacing(space::XXS)
                .width(Length::Fill),
        )
        .padding([space::XS, space::S])
        .into()
    })
}

/// The number a row's pull request goes by: `#12`, after its repository
/// when that is not the conversation's.
fn number_label(list: Option<&ConversationPullRequests>, row: &ConversationPullRequest) -> String {
    if list
        .and_then(|list| list.repository.as_ref())
        .is_some_and(|own| own.same_repository(&row.repository))
    {
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

fn state_tone(state: CheckState) -> fn(&cosmic::Theme) -> iced::Color {
    match state {
        CheckState::Failed => design::destructive_color,
        CheckState::Pending => design::warning_color,
        CheckState::Passed => design::success_color,
        CheckState::Cancelled | CheckState::Skipped => design::muted_color,
    }
}

fn state_text(state: CheckState) -> fn(&cosmic::Theme) -> iced::widget::text::Style {
    match state {
        CheckState::Failed => design::destructive_text,
        CheckState::Pending => design::warning_text,
        CheckState::Passed => design::success_text,
        CheckState::Cancelled | CheckState::Skipped => design::muted_text,
    }
}

fn state_icon(state: CheckState) -> Icon {
    match state {
        CheckState::Failed => Icon::Error,
        CheckState::Pending => Icon::Loading,
        CheckState::Passed => Icon::Check,
        CheckState::Cancelled => Icon::Cancel,
        CheckState::Skipped => Icon::Minus,
    }
}

fn tinted_icon<'a>(icon: Icon, tone: fn(&cosmic::Theme) -> iced::Color) -> Element<'a, Action> {
    widget::icon(icon.handle())
        .size(design::COMPACT_ICON_SIZE)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            iced::widget::svg::Style {
                color: Some(tone(theme)),
            }
        })))
        .into()
}

/// The checks' overall state, the worst of any check, and how many share it.
fn checks_summary(checks: &[Check]) -> Option<(CheckState, String)> {
    let worst = checks.iter().map(|check| check.state).min()?;
    let count = checks.iter().filter(|check| check.state == worst).count();
    let label = match worst {
        CheckState::Failed => format!("{count} failing"),
        CheckState::Pending => format!("{count} running"),
        CheckState::Cancelled => format!("{count} cancelled"),
        CheckState::Passed => "Checks passed".to_owned(),
        CheckState::Skipped => format!("{count} skipped"),
    };
    Some((worst, label))
}

/// What a worktree's row says about it: its uncommitted changes and the
/// lines they change.
fn worktree_facts<'a>(worktree: &ConversationWorktree) -> Vec<Element<'a, Action>> {
    let mut facts: Vec<Element<'a, Action>> = Vec::with_capacity(2);
    // The daemon's count includes untracked files, which a worktree row in
    // Projects may leave out.
    match worktree.changed_paths {
        Some(0) => facts.push(design::caption("No uncommitted changes").into()),
        Some(count) => facts.push(
            design::caption(format!("{count} uncommitted"))
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .into(),
        ),
        None => {}
    }
    if let Some(diff) = worktree.diff {
        let counts =
            crate::repository::line_change_counts(Some(diff.additions), Some(diff.deletions));
        if !counts.is_empty() {
            facts.push(crate::ui_controls::change_counts(&counts));
        }
    }
    facts
}

/// Facts with a dot between each.
fn separated<'a>(facts: Vec<Element<'a, Action>>) -> Vec<Element<'a, Action>> {
    facts
        .into_iter()
        .enumerate()
        .flat_map(|(index, fact)| {
            let dot = (index > 0).then(|| design::caption("·").into());
            dot.into_iter().chain(std::iter::once(fact))
        })
        .collect()
}

fn one_line<'a>(text: impl Into<Cow<'a, str>> + 'a) -> iced::widget::Text<'a, cosmic::Theme> {
    design::body(text)
        .wrapping(iced::widget::text::Wrapping::None)
        .ellipsize(iced::widget::text::Ellipsize::End(
            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
        ))
}

/// A row's two lines: its glyph, name and trailing number, then its facts
/// under the name.
fn lines<'a>(
    glyph: Element<'a, Action>,
    name: String,
    trailing: Option<String>,
    facts: Vec<Element<'a, Action>>,
) -> Element<'a, Action> {
    let mut first = vec![glyph, one_line(name).width(Length::Fill).into()];
    if let Some(trailing) = trailing {
        first.push(
            design::caption(trailing)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
    }
    let mut column = vec![
        widget::row::with_children(first)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
    ];
    if !facts.is_empty() {
        column.push(
            widget::container(
                widget::row::with_children(separated(facts))
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .wrap(),
            )
            .padding(Padding {
                left: f32::from(design::ROW_ICON_SIZE) + space::S,
                ..Padding::ZERO
            })
            .into(),
        );
    }
    widget::column::with_children(column)
        .spacing(space::XXS)
        .into()
}

fn work_row<'a>(
    list: Option<&ConversationPullRequests>,
    work: Work<'_>,
    chosen: bool,
    openable: &dyn Fn(&Path) -> bool,
) -> Element<'a, Action> {
    let (glyph, name, trailing, mut facts): (Element<'a, Action>, String, Option<String>, _) =
        match work {
            Work::PullRequest { row, .. } => {
                let state = row
                    .pull_request
                    .as_ref()
                    .map(crate::sidebar::WorktreePullRequestState::from_work);
                let number = number_label(list, row);
                let name = row
                    .pull_request
                    .as_ref()
                    .map_or_else(|| number.clone(), |pull_request| pull_request.title.clone());
                let mut facts: Vec<Element<'a, Action>> = vec![design::caption(reason(row)).into()];
                match (state, &row.pull_request) {
                    (Some(_), Some(pull_request)) => {
                        if pull_request.state == crate::work::PullRequestState::Open
                            && let Some((overall, label)) = checks_summary(&row.checks)
                        {
                            facts.push(
                                design::caption(label)
                                    .class(cosmic::theme::Text::Custom(state_text(overall)))
                                    .into(),
                            );
                        }
                        if let Some(diff) = row.diff {
                            let counts = crate::repository::line_change_counts(
                                Some(diff.additions),
                                Some(diff.deletions),
                            );
                            if !counts.is_empty() {
                                facts.push(crate::ui_controls::change_counts(&counts));
                            }
                        }
                        if let Some(list) = list
                            && list.forge_error.is_some()
                            && let Some(at) = row.observed_at_unix_ms
                            && list.generated_at_unix_ms.saturating_sub(at) > STALE_AFTER_MS
                        {
                            facts.push(
                                design::caption(format!(
                                    "as of {}",
                                    crate::work_ui::age(list.generated_at_unix_ms, at, true)
                                ))
                                .into(),
                            );
                        }
                    }
                    _ => facts.push(design::caption("State unknown").into()),
                }
                (
                    crate::ui_controls::pull_request_state_glyph(state, design::ROW_ICON_SIZE),
                    name,
                    Some(number),
                    facts,
                )
            }
            Work::Worktree(worktree) => {
                let mut facts: Vec<Element<'a, Action>> = vec![
                    design::caption(if worktree.by_subagent {
                        "Edited by a subagent"
                    } else {
                        "Edited"
                    })
                    .into(),
                ];
                facts.extend(worktree_facts(worktree));
                (
                    crate::ui_controls::passive_icon(
                        crate::sidebar::worktree_icon(),
                        design::ROW_ICON_SIZE,
                    ),
                    worktree_branch(worktree),
                    None,
                    facts,
                )
            }
            Work::Here(_) => (
                crate::ui_controls::passive_icon(
                    crate::sidebar::worktree_icon(),
                    design::ROW_ICON_SIZE,
                ),
                work.scope_label(),
                None,
                vec![design::caption("Where this conversation runs").into()],
            ),
        };
    if let Some(path) = work.worktree()
        && !openable(path)
    {
        facts.push(design::caption("Not in Projects").into());
    }
    let mut body = vec![lines(glyph, name.clone(), trailing, facts)];
    if chosen && let Some(row) = work.pull_request() {
        body.extend(chosen_details(row));
    }
    let button = widget::button::custom(
        widget::column::with_children(body)
            .spacing(space::XS)
            .width(Length::Fill),
    )
    .name(if chosen {
        format!("{name}, shown below")
    } else {
        format!("Show {name}")
    })
    .class(crate::ui_controls::control_style(chosen, None))
    .padding([space::XS, space::S])
    .width(Length::Fill)
    .on_press(Action::Choose(work.key()));
    let tooltip = work
        .worktree()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let row = crate::interaction::hover(button);
    if tooltip.is_empty() {
        row
    } else {
        crate::interaction::tooltip(row, tooltip)
    }
}

/// What a chosen pull request adds beneath its row: its checks, the ones
/// most in need of attention first, and a way to open it.
fn chosen_details<'a>(row: &ConversationPullRequest) -> Vec<Element<'a, Action>> {
    let mut details: Vec<Element<'a, Action>> = Vec::new();
    let inset = Padding {
        left: f32::from(design::ROW_ICON_SIZE) + space::S,
        ..Padding::ZERO
    };
    let open_state = row
        .pull_request
        .as_ref()
        .is_some_and(|pull_request| pull_request.state == crate::work::PullRequestState::Open);
    if open_state && !row.checks.is_empty() {
        let mut checks: Vec<&Check> = row.checks.iter().collect();
        checks.sort_by_key(|check| check.state);
        let mut lines: Vec<Element<'a, Action>> = Vec::with_capacity(CHECKS_SHOWN + 1);
        for check in checks.iter().take(CHECKS_SHOWN) {
            lines.push(crate::interaction::hover(
                widget::button::custom(
                    widget::row::with_children(vec![
                        tinted_icon(state_icon(check.state), state_tone(check.state)),
                        design::caption(check.name.clone())
                            .wrapping(iced::widget::text::Wrapping::None)
                            .ellipsize(iced::widget::text::Ellipsize::End(
                                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                            ))
                            .width(Length::Fill)
                            .into(),
                    ])
                    .spacing(space::XS)
                    .align_y(Alignment::Center),
                )
                .name(format!("Open the {} check", check.name))
                .class(crate::ui_controls::control_style(false, None))
                .padding([space::XXS, space::XS])
                .width(Length::Fill)
                .on_press_maybe(Url::parse(&check.url).ok().map(Action::OpenCheck)),
            ));
        }
        if checks.len() > CHECKS_SHOWN {
            lines.push(
                widget::container(design::caption(format!(
                    "{} more on GitHub",
                    checks.len() - CHECKS_SHOWN
                )))
                .padding([space::XXS, space::XS])
                .into(),
            );
        }
        details.push(
            widget::container(widget::column::with_children(lines).width(Length::Fill))
                .padding(Padding {
                    left: inset.left - space::XS,
                    ..Padding::ZERO
                })
                .into(),
        );
    }
    details.push(
        widget::container(
            crate::ui_controls::button::quiet("Open pull request")
                .icon(Icon::PopOut.handle())
                .name(format!("Open #{}", row.number))
                .on_press_maybe(open(row)),
        )
        .padding(Padding {
            left: inset.left - space::S,
            ..Padding::ZERO
        })
        .into(),
    );
    details
}

/// The fold over the mentioned pull requests, which says how many there are.
fn mentions_toggle<'a>(open: bool, count: usize) -> Element<'a, Action> {
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(vec![
                crate::ui_controls::passive_icon(
                    if open { Icon::PanDown } else { Icon::PanEnd }.handle(),
                    design::COMPACT_ICON_SIZE,
                ),
                design::caption("Mentioned")
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                design::caption(count.to_string()).into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .name(format!(
            "{count} mentioned pull requests, {}",
            if open { "expanded" } else { "collapsed" }
        ))
        .class(crate::ui_controls::control_style(false, None))
        .padding([space::XS, space::S])
        .width(Length::Fill)
        .on_press(Action::ToggleMentions),
    )
}

fn mentioned_row<'a>(
    list: Option<&ConversationPullRequests>,
    row: &ConversationPullRequest,
) -> Element<'a, Action> {
    let number = number_label(list, row);
    let name = row
        .pull_request
        .as_ref()
        .map_or_else(|| number.clone(), |pull_request| pull_request.title.clone());
    let state = row
        .pull_request
        .as_ref()
        .map(crate::sidebar::WorktreePullRequestState::from_work);
    let glyph = crate::ui_controls::pull_request_state_glyph(state, design::ROW_ICON_SIZE);
    let described = format!(
        "Open {number} {name}{}",
        state.map_or(String::new(), |state| format!(
            ", {}",
            state.label().to_lowercase()
        ))
    );
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(vec![
                glyph,
                one_line(name)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .width(Length::Fill)
                    .into(),
                design::caption(number)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center),
        )
        .name(described)
        .class(crate::ui_controls::control_style(false, None))
        .padding([space::XXS, space::S])
        .width(Length::Fill)
        .on_press_maybe(open(row)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{PullRequest, PullRequestState};

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
        let card = CardId::new();
        assert!(state.retarget(Some((card, conversation))));
        // Another card focused in the workspace keeps the conversation.
        assert!(!state.retarget(Some((card, conversation))));
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
        assert_eq!(state.listed(false).count(), 1);
        assert_eq!(state.listed(true).count(), 2);
        // Another conversation's list is not this one's.
        state.update(listed(TerminalId::new(), 9, Vec::new(), true));
        assert_eq!(state.rows().len(), 2);
        state.mentions_open = true;
        // The next conversation's section stays while its list is on its way.
        assert!(state.retarget(Some((CardId::new(), TerminalId::new()))));
        assert!(state.rows().is_empty() && !state.mentions_open);
        assert!(state.lists_any(false));
        assert!(state.retarget(None));
        assert!(state.conversation.is_none());
        assert!(!state.lists_any(true));
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

    #[test]
    fn work_lists_pull_requests_then_lone_worktrees_and_shows_where_the_chat_runs() {
        let mut state = State::default();
        let conversation = TerminalId::new();
        state.retarget(Some((CardId::new(), conversation)));
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
        let work = state.work(Some(here));
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
            state.shown(&work, Some(here)).map(|work| work.key()),
            Some(WorkKey::Worktree(PathBuf::from("/scratch")))
        );
        // Else the first checked out here, not a pull request with nothing
        // local to show.
        assert_eq!(
            state.shown(&work, None).map(|work| work.key()),
            Some(keys[1].clone())
        );
        state.choose(keys[0].clone());
        let work = state.work(Some(here));
        assert_eq!(
            state.shown(&work, Some(here)).map(|work| work.key()),
            Some(keys[0].clone())
        );
        // A choice no longer listed gives way rather than showing nothing.
        state.choose(WorkKey::Worktree(PathBuf::from("/gone")));
        let work = state.work(Some(here));
        assert_eq!(
            state.shown(&work, Some(here)).map(|work| work.key()),
            Some(WorkKey::Worktree(PathBuf::from("/scratch")))
        );
        // Another conversation starts with nothing chosen.
        state.choose(keys[0].clone());
        state.retarget(Some((CardId::new(), TerminalId::new())));
        assert!(state.chosen.is_none());
    }

    #[test]
    fn where_the_chat_runs_is_listed_only_when_nothing_else_is_checked_out() {
        let mut state = State::default();
        let conversation = TerminalId::new();
        state.retarget(Some((CardId::new(), conversation)));
        state.update(listed(
            conversation,
            1,
            vec![row(1, Relation::Created)],
            true,
        ));
        let here = Path::new("/main");
        let work = state.work(Some(here));
        assert_eq!(work.len(), 2);
        assert!(matches!(work[1], Work::Here(path) if path == here));
        assert_eq!(work[1].scope_label(), "main");
        assert_eq!(
            state.shown(&work, Some(here)).map(|work| work.key()),
            Some(WorkKey::Worktree(PathBuf::from("/main")))
        );
        // A conversation that worked on nothing has no Work at all.
        state.update(listed(
            conversation,
            2,
            vec![row(1, Relation::Mentioned)],
            false,
        ));
        assert!(state.work(Some(here)).is_empty());
    }

    #[test]
    fn the_checks_chip_reports_the_worst_state_and_how_many_share_it() {
        assert_eq!(checks_summary(&[]), None);
        let checks = [
            check("lint", CheckState::Passed),
            check("test", CheckState::Pending),
            check("build", CheckState::Passed),
        ];
        assert_eq!(
            checks_summary(&checks),
            Some((CheckState::Pending, "1 running".into()))
        );
        let checks = [
            check("lint", CheckState::Failed),
            check("test", CheckState::Failed),
            check("build", CheckState::Pending),
        ];
        assert_eq!(
            checks_summary(&checks),
            Some((CheckState::Failed, "2 failing".into()))
        );
        let checks = [
            check("a", CheckState::Passed),
            check("b", CheckState::Skipped),
        ];
        assert_eq!(
            checks_summary(&checks),
            Some((CheckState::Passed, "Checks passed".into()))
        );
        let checks = [
            check("a", CheckState::Passed),
            check("b", CheckState::Cancelled),
        ];
        assert_eq!(
            checks_summary(&checks),
            Some((CheckState::Cancelled, "1 cancelled".into()))
        );
        let checks = [check("a", CheckState::Skipped)];
        assert_eq!(
            checks_summary(&checks),
            Some((CheckState::Skipped, "1 skipped".into()))
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
