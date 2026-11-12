//! The pull requests the inspected conversation worked on or mentioned, and
//! the worktrees it edited, as the daemon infers them from its transcript
//! and keeps current with GitHub.

use super::*;
use crate::work::{
    Check, CheckState, ConversationPullRequest, ConversationPullRequests, ConversationWorktree,
    Relation, TranscriptCoverage,
};
use cosmic::iced::futures::SinkExt;

/// Mentions listed before the rest are folded behind "Show N more".
const MENTIONS_SHOWN: usize = 5;
/// While GitHub cannot be reached, a row checked longer ago than this says
/// when it was.
const STALE_AFTER_MS: u64 = 5 * 60 * 1000;
const CHECKS_POPUP_WIDTH: f32 = 320.0;
const CHECKS_POPUP_HEIGHT: f32 = 360.0;

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
    pub(super) all_mentions: bool,
    /// The pull request whose checks are open in a popup.
    checks_shown: Option<(GitHubRepoKey, u64)>,
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
                // A popup for a pull request no longer listed has nothing
                // left to show.
                if let Some((repository, number)) = &self.checks_shown
                    && !self.rows().iter().any(|row| {
                        row.number == *number && row.repository.same_repository(repository)
                    })
                {
                    self.checks_shown = None;
                }
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
            listed_before: conversation.is_some() && !self.rows().is_empty(),
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

    pub(super) fn checks_popup_open(&self) -> bool {
        self.checks_shown.is_some()
    }

    /// Opens the checks popup of this pull request, or with `None` closes it.
    pub(super) fn show_checks(&mut self, shown: Option<(GitHubRepoKey, u64)>) {
        self.checks_shown = shown;
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

    /// Whether the section lists any pull request, or the previous
    /// conversation did and this one's list has not arrived yet.
    pub(super) fn lists_any(&self, show_mentions: bool) -> bool {
        match &self.list {
            Some(_) => self.listed(show_mentions).next().is_some(),
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

/// The list, or `None` while the section lists no pull request.
pub(super) fn view<'a>(
    state: &'a State,
    show_mentions: bool,
    place: &dyn Fn(&ConversationWorktree) -> Placement,
) -> Option<Element<'a, Action>> {
    let list = state.list.as_deref()?;
    let (worked_on, mentioned): (Vec<_>, Vec<_>) = state
        .listed(show_mentions)
        .partition(|row| row.relation.worked_on());
    if worked_on.is_empty() && mentioned.is_empty() {
        return None;
    }
    let mut children: Vec<Element<'a, Action>> = Vec::new();
    // The section header already names and counts a single group.
    let grouped = !worked_on.is_empty() && !mentioned.is_empty();
    let heading = |label: &'static str, count: usize| -> Element<'a, Action> {
        widget::row::with_children(vec![
            design::caption(label)
                .font(design::heading_font())
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            design::caption(count.to_string()).into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
    };
    if !worked_on.is_empty() {
        if grouped {
            children.push(heading("Worked on", worked_on.len()));
        }
        for row in &worked_on {
            let worktree = state
                .worktree_of(row)
                .map(|worktree| (worktree, place(worktree)));
            children.push(worked_on_row(state, list, row, worktree));
        }
    }
    if !mentioned.is_empty() {
        if grouped || worked_on.is_empty() {
            children.push(heading("Mentioned", mentioned.len()));
        }
        let shown = if state.all_mentions {
            mentioned.len()
        } else {
            MENTIONS_SHOWN
        };
        for row in mentioned.iter().take(shown) {
            children.push(mentioned_row(list, row));
        }
        if mentioned.len() > MENTIONS_SHOWN {
            let (label, name) = if state.all_mentions {
                (
                    "Show fewer".to_owned(),
                    "Show only the first mentioned pull requests",
                )
            } else {
                (
                    format!("Show {} more", mentioned.len() - shown),
                    "Show every mentioned pull request",
                )
            };
            children.push(
                crate::ui_controls::button::secondary(label)
                    .name(name)
                    .on_press(Action::ToggleAllMentions)
                    .into(),
            );
        }
    }
    if let Some(note) = note(list) {
        children.push(note);
    }
    Some(
        widget::column::with_children(children)
            .spacing(space::XS)
            .width(Length::Fill)
            .into(),
    )
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
        widget::column::with_children(lines)
            .spacing(space::XXS)
            .width(Length::Fill)
            .into()
    })
}

/// What the row's pull request is called: its title, after its repository
/// when that is not the conversation's.
fn name(list: &ConversationPullRequests, row: &ConversationPullRequest) -> String {
    let number = if list
        .repository
        .as_ref()
        .is_some_and(|own| own.same_repository(&row.repository))
    {
        format!("#{}", row.number)
    } else {
        format!("{}#{}", repository_label(&row.repository), row.number)
    };
    match &row.pull_request {
        Some(pull_request) => format!("{number} {}", pull_request.title),
        None => number,
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
        reason.push_str(" · by a subagent");
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

fn glyph<'a>(row: &ConversationPullRequest, muted: bool) -> Element<'a, Action> {
    match &row.pull_request {
        Some(pull_request) if !muted => crate::work_ui::pull_request_glyph(pull_request),
        _ => crate::ui_controls::passive_icon(Icon::PullRequest.handle(), design::ROW_ICON_SIZE),
    }
}

fn state_tone(state: CheckState) -> fn(&cosmic::Theme) -> iced::Color {
    match state {
        CheckState::Failed => design::destructive_color,
        CheckState::Pending => design::warning_color,
        CheckState::Passed => design::success_color,
        CheckState::Cancelled | CheckState::Skipped => design::muted_color,
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
    // The state most in need of attention, and how many checks are in it.
    let worst = checks.iter().map(|check| check.state).min()?;
    let count = checks.iter().filter(|check| check.state == worst).count();
    let label = match worst {
        CheckState::Failed => format!("{count} failing"),
        CheckState::Pending => format!("{count} running"),
        CheckState::Cancelled => format!("{count} cancelled"),
        CheckState::Passed => format!("{count} passed"),
        CheckState::Skipped => format!("{count} skipped"),
    };
    Some((worst, label))
}

/// The CI chip of an open pull request: its checks' overall state, opening
/// every check in a popup.
fn checks_chip<'a>(state: &State, row: &ConversationPullRequest) -> Option<Element<'a, Action>> {
    let (overall, label) = checks_summary(&row.checks)?;
    let tone = state_tone(overall);
    let shown = state
        .checks_shown
        .as_ref()
        .is_some_and(|(repository, number)| {
            *number == row.number && repository.same_repository(&row.repository)
        });
    let chip = widget::button::custom(
        widget::row::with_children(vec![
            tinted_icon(state_icon(overall), tone),
            design::caption(label)
                .wrapping(iced::widget::text::Wrapping::None)
                .class(cosmic::theme::Text::Custom(match overall {
                    CheckState::Failed => design::destructive_text,
                    CheckState::Pending => design::warning_text,
                    CheckState::Passed => design::success_text,
                    CheckState::Cancelled | CheckState::Skipped => design::muted_text,
                }))
                .into(),
        ])
        .spacing(space::XXS)
        .align_y(Alignment::Center),
    )
    .name(format!("Show the checks of #{}", row.number))
    .class(crate::ui_controls::control_style(shown, None))
    .padding([1.0, space::XS])
    .on_press(Action::ShowChecks(
        (!shown).then(|| (row.repository.clone(), row.number)),
    ));
    let popup = shown.then(|| checks_popup(row));
    Some(crate::ui_controls::popover(
        crate::interaction::hover(chip),
        popup,
        widget::popover::Position::Bottom,
        Action::ShowChecks(None),
    ))
}

/// Every check on the pull request's head, failing first, each opening its
/// details page.
fn checks_popup<'a>(row: &ConversationPullRequest) -> Element<'a, Action> {
    let mut rows: Vec<Element<'a, Action>> = Vec::with_capacity(row.checks.len());
    for check in &row.checks {
        let mut label = vec![
            design::body(check.name.clone())
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ))
                .into(),
        ];
        if !check.workflow.is_empty() {
            label.push(
                design::caption(check.workflow.clone())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    ))
                    .into(),
            );
        }
        rows.push(crate::interaction::hover(
            widget::button::custom(
                widget::row::with_children(vec![
                    tinted_icon(state_icon(check.state), state_tone(check.state)),
                    widget::column::with_children(label)
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center),
            )
            .name(format!("Open the {} check", check.name))
            .class(crate::ui_controls::control_style(false, None))
            .padding([space::XS, space::S])
            .width(Length::Fill)
            .on_press_maybe(Url::parse(&check.url).ok().map(Action::OpenCheck)),
        ));
    }
    let list = widget::column::with_children(vec![
        widget::container(
            design::caption(format!("Checks on #{}", row.number)).font(design::heading_font()),
        )
        .padding([space::XXS, space::S])
        .into(),
        widget::scrollable(widget::column::with_children(rows).width(Length::Fill))
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .height(Length::Shrink)
            .into(),
    ])
    .spacing(space::XXS);
    widget::container(list)
        .width(Length::Fixed(CHECKS_POPUP_WIDTH))
        .max_height(CHECKS_POPUP_HEIGHT)
        .into()
}

/// Where an edited worktree stands in the window.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Placement {
    /// Open in Projects, so choosing it can go there.
    pub(crate) openable: bool,
    /// What its row says when it is the current worktree.
    pub(crate) current: Option<&'static str>,
}

/// What a row says about an edited worktree: its uncommitted changes, the
/// lines they change, and where it stands.
pub(crate) fn worktree_facts<'a, M: 'a>(
    worktree: &ConversationWorktree,
    placement: Placement,
) -> Vec<Element<'a, M>> {
    let mut facts: Vec<Element<'a, M>> = Vec::with_capacity(5);
    // The daemon's count includes untracked files, which a worktree row in
    // Projects may leave out.
    match worktree.changed_paths {
        Some(0) => facts.push(design::caption("Clean").into()),
        Some(count) => facts.push(
            design::caption(if count == 1 {
                "1 change".to_owned()
            } else {
                format!("{count} changes")
            })
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
    if !placement.openable {
        facts.push(design::caption("Not open in Projects").into());
    }
    if let Some(current) = placement.current {
        facts.push(design::caption(current).into());
    }
    if worktree.by_subagent {
        facts.push(design::caption("by a subagent").into());
    }
    facts
}

/// Facts with a dot between each.
pub(crate) fn separated<'a, M: 'a>(facts: Vec<Element<'a, M>>) -> Vec<Element<'a, M>> {
    facts
        .into_iter()
        .enumerate()
        .flat_map(|(index, fact)| {
            let dot = (index > 0).then(|| design::caption("·").into());
            dot.into_iter().chain(std::iter::once(fact))
        })
        .collect()
}

/// The line of a pull request's row naming the worktree where the
/// conversation edited its branch; choosing it opens the worktree.
fn worktree_line<'a>(worktree: &ConversationWorktree, placement: Placement) -> Element<'a, Action> {
    let branch = worktree.branch.clone().unwrap_or_else(|| {
        worktree
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
    });
    let mut parts = vec![
        crate::ui_controls::passive_icon(
            crate::sidebar::worktree_icon(),
            design::COMPACT_ICON_SIZE,
        ),
        design::caption(branch.clone())
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
    ];
    let facts = worktree_facts(worktree, placement);
    if !facts.is_empty() {
        parts.push(design::caption("·").into());
        parts.extend(separated(facts));
    }
    let line = widget::button::custom(
        widget::row::with_children(parts)
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .wrap(),
    )
    .name(format!("Open worktree {branch}"))
    .class(crate::ui_controls::control_style(false, None))
    .padding([space::XXS, space::XS])
    .on_press_maybe(
        placement
            .openable
            .then(|| Action::OpenWorktree(worktree.path.clone())),
    );
    crate::interaction::tooltip(
        crate::interaction::hover(line),
        worktree.path.display().to_string(),
    )
}

fn worked_on_row<'a>(
    state: &State,
    list: &ConversationPullRequests,
    row: &ConversationPullRequest,
    worktree: Option<(&ConversationWorktree, Placement)>,
) -> Element<'a, Action> {
    let name = name(list, row);
    let title = widget::row::with_children(vec![
        glyph(row, false),
        design::heading(name.clone())
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into(),
    ])
    .spacing(space::S)
    .align_y(Alignment::Center);
    let mut facts = vec![design::caption(reason(row)).into()];
    match &row.pull_request {
        Some(pull_request) => {
            facts.push(crate::work_ui::pull_request_state_badge(pull_request));
            if let Some(diff) = row.diff {
                let counts = crate::repository::line_change_counts(
                    Some(diff.additions),
                    Some(diff.deletions),
                );
                if !counts.is_empty() {
                    facts.push(crate::ui_controls::change_counts(&counts));
                }
            }
            if pull_request.state == crate::work::PullRequestState::Open {
                facts.extend(checks_chip(state, row));
            }
            facts.extend(crate::work_ui::pull_request_review_badge(pull_request));
            if list.forge_error.is_some()
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
        None => facts.push(design::caption("State unknown").into()),
    }
    let indent = |line: Element<'a, Action>, inset: f32| -> Element<'a, Action> {
        widget::container(line)
            .padding(Padding {
                left: f32::from(design::ROW_ICON_SIZE) + space::S - inset,
                ..Padding::ZERO
            })
            .into()
    };
    let mut lines = vec![
        title.into(),
        indent(
            widget::row::with_children(facts)
                .spacing(space::XS)
                .align_y(Alignment::Center)
                .wrap()
                .into(),
            0.0,
        ),
    ];
    if let Some((worktree, placement)) = worktree {
        // Inset by its own padding, so its text lines up with the facts.
        lines.push(indent(worktree_line(worktree, placement), space::XS));
    }
    let lines = widget::column::with_children(lines).spacing(space::XXS);
    crate::interaction::hover(
        widget::button::custom(lines)
            .name(format!("Open {name}"))
            .class(crate::ui_controls::control_style(false, None))
            .padding([space::XS, space::S])
            .width(Length::Fill)
            .on_press_maybe(open(row)),
    )
}

fn mentioned_row<'a>(
    list: &ConversationPullRequests,
    row: &ConversationPullRequest,
) -> Element<'a, Action> {
    let name = name(list, row);
    let mut parts = vec![
        glyph(row, true),
        design::body(name.clone())
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into(),
    ];
    match &row.pull_request {
        None => parts.push(design::caption("State unknown").into()),
        Some(pull_request) if pull_request.state != crate::work::PullRequestState::Open => {
            parts.push(crate::work_ui::pull_request_state_badge(pull_request));
        }
        Some(_) => {}
    }
    if row.by_subagent {
        parts.push(design::caption("by a subagent").into());
    }
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(parts)
                .spacing(space::S)
                .align_y(Alignment::Center),
        )
        .name(format!("Open {name}"))
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
        state.all_mentions = true;
        // The next conversation's section stays while its list is on its way.
        assert!(state.retarget(Some((CardId::new(), TerminalId::new()))));
        assert!(state.rows().is_empty() && !state.all_mentions);
        assert!(state.lists_any(false));
        assert!(state.retarget(None));
        assert!(state.conversation.is_none());
        assert!(!state.lists_any(true));
    }

    #[test]
    fn a_checks_popup_closes_once_its_pull_request_is_no_longer_listed() {
        let mut state = State::default();
        let conversation = TerminalId::new();
        state.retarget(Some((CardId::new(), conversation)));
        state.update(listed(
            conversation,
            1,
            vec![row(1, Relation::Created), row(2, Relation::Pushed)],
            true,
        ));
        state.show_checks(Some((repo("ME"), 2)));
        state.update(listed(
            conversation,
            2,
            vec![row(2, Relation::Pushed)],
            false,
        ));
        assert!(state.checks_shown.is_some());
        state.update(listed(
            conversation,
            3,
            vec![row(1, Relation::Created)],
            false,
        ));
        assert!(state.checks_shown.is_none());
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
            Some((CheckState::Passed, "1 passed".into()))
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
        assert_eq!(reason(&delegated), "Created · by a subagent");
        let list = ConversationPullRequests {
            generated_at_unix_ms: 0,
            repository: Some(repo("me")),
            pull_requests: Vec::new(),
            transcript: TranscriptCoverage::Complete,
            forge_error: None,
            worktrees: Vec::new(),
        };
        assert_eq!(name(&list, &delegated), "#2 Change 2");
        delegated.repository = repo("upstream");
        delegated.pull_request = None;
        assert_eq!(name(&list, &delegated), "upstream/repo#2");
        assert!(
            matches!(open(&delegated), Some(Action::Open { repository, number: 2, url })
            if repository == repo("upstream")
                && url.as_str() == "https://github.com/upstream/repo/pull/2")
        );
        delegated.repository = GitHubRepoKey::new("ghe.example", None, "me", "repo").unwrap();
        assert_eq!(name(&list, &delegated), "ghe.example/me/repo#2");
    }
}
