//! The pull requests the inspected conversation worked on or mentioned, as
//! the daemon infers them from its transcript. They list above the pull
//! request detail; choosing one shows it there.

use super::*;
use crate::work::{
    ConversationPullRequest, ConversationPullRequests, Relation, TranscriptCoverage,
};
use cosmic::iced::futures::SinkExt;

/// Mentions listed before the rest are folded behind "Show N more".
const MENTIONS_SHOWN: usize = 5;
/// While GitHub cannot be reached, a row checked longer ago than this says
/// when it was.
const STALE_AFTER_MS: u64 = 5 * 60 * 1000;

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
    /// The previous conversation listed pull requests, so the tab stays
    /// while this one's list is on its way rather than flickering away.
    listed_before: bool,
    pub(super) all_mentions: bool,
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
            listed_before: conversation.is_some() && self.lists_any(),
            ..Self::default()
        };
        true
    }

    pub(super) fn rows(&self) -> &[ConversationPullRequest] {
        self.list
            .as_deref()
            .map_or(&[], |list| list.pull_requests.as_slice())
    }

    pub(super) fn lists(&self, key: &PullRequestKey) -> bool {
        self.rows().iter().any(|row| names(row, key))
    }

    /// The pull request to show by default: what the conversation worked on
    /// most, which the daemon ranks first by relation, then by recency. The
    /// one already shown stays while it is listed as worked on and nothing
    /// ranks above it by relation alone, so the detail does not jump between
    /// two branches the conversation pushes to in turn.
    pub(super) fn settled(&self, shown: Option<&PullRequestKey>) -> Option<PullRequestKey> {
        let rows = self.rows();
        let leading = rows.iter().find(|row| row.relation.worked_on())?;
        if let Some(shown) = shown
            && rows
                .iter()
                .find(|row| row.relation.worked_on() && names(row, shown))
                .is_some_and(|row| row.relation <= leading.relation)
        {
            return Some(shown.clone());
        }
        row_key(leading)
    }

    /// Whether the conversation lists pull requests, or listed some before
    /// it changed and its new list has not arrived yet.
    pub(super) fn lists_any(&self) -> bool {
        match &self.list {
            Some(list) => !list.pull_requests.is_empty(),
            None => self.listed_before,
        }
    }
}

/// GitHub names repositories case-insensitively.
pub(super) fn same_pull_request(a: &PullRequestKey, b: &PullRequestKey) -> bool {
    a.number == b.number && a.repository.same_repository(&b.repository)
}

fn names(row: &ConversationPullRequest, key: &PullRequestKey) -> bool {
    row.number == key.number && row.repository.same_repository(&key.repository)
}

pub(super) fn row_key(row: &ConversationPullRequest) -> Option<PullRequestKey> {
    PullRequestKey::new(row.repository.clone(), row.number).ok()
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

/// The list, or `None` while the conversation names no pull request, so the
/// panel stays as it was.
///
/// `selected` is the pull request the detail shows.
pub(super) fn view<'a>(
    state: &'a State,
    title: Option<&'a str>,
    selected: Option<&PullRequestKey>,
) -> Option<Element<'a, Action>> {
    let list = state.list.as_deref()?;
    if list.pull_requests.is_empty() {
        return None;
    }
    let (worked_on, mentioned): (Vec<_>, Vec<_>) = list
        .pull_requests
        .iter()
        .partition(|row| row.relation.worked_on());
    let mut children: Vec<Element<'a, Action>> = Vec::new();
    let heading = |label: &'static str, count: usize| -> Element<'a, Action> {
        let mut parts = vec![
            design::caption(label)
                .font(design::heading_font())
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            design::caption(count.to_string()).into(),
        ];
        if let Some(title) = title.filter(|_| label == "Worked on" || worked_on_empty(list)) {
            parts.push(
                design::caption(format!("· {title}"))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            );
        }
        widget::row::with_children(parts)
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
    };
    if !worked_on.is_empty() {
        children.push(heading("Worked on", worked_on.len()));
        for row in &worked_on {
            children.push(worked_on_row(list, row, selected));
        }
    }
    if !mentioned.is_empty() {
        children.push(heading("Mentioned", mentioned.len()));
        let shown = if state.all_mentions {
            mentioned.len()
        } else {
            MENTIONS_SHOWN
        };
        for row in mentioned.iter().take(shown) {
            children.push(mentioned_row(list, row, selected));
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

fn worked_on_empty(list: &ConversationPullRequests) -> bool {
    !list
        .pull_requests
        .iter()
        .any(|row| row.relation.worked_on())
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
pub(super) fn repository_label(repository: &GitHubRepoKey) -> String {
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

fn choose(row: &ConversationPullRequest) -> Option<Action> {
    row_key(row).map(|key| Action::Choose(Some(key)))
}

fn link_out<'a>(row: &ConversationPullRequest) -> Element<'a, Action> {
    let url = row
        .pull_request
        .as_ref()
        .map(|pull_request| pull_request.url.clone())
        .unwrap_or_else(|| row.url());
    crate::ui_controls::icon_button(Icon::PopOut, "Open on GitHub")
        .compact()
        .on_press_maybe(Url::parse(&url).ok().map(Action::OpenUrl))
        .into()
}

fn glyph<'a>(row: &ConversationPullRequest, muted: bool) -> Element<'a, Action> {
    match &row.pull_request {
        Some(pull_request) if !muted => crate::work_ui::pull_request_glyph(pull_request),
        _ => crate::ui_controls::passive_icon(Icon::PullRequest.handle(), design::ROW_ICON_SIZE),
    }
}

fn worked_on_row<'a>(
    list: &ConversationPullRequests,
    row: &ConversationPullRequest,
    selected: Option<&PullRequestKey>,
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
        link_out(row),
    ])
    .spacing(space::S)
    .align_y(Alignment::Center);
    let mut facts = vec![design::caption(reason(row)).into()];
    match &row.pull_request {
        Some(pull_request) => {
            facts.extend(crate::work_ui::pull_request_badges(pull_request));
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
    let lines = widget::column::with_children(vec![
        title.into(),
        widget::container(
            widget::row::with_children(facts)
                .spacing(space::XS)
                .align_y(Alignment::Center)
                .wrap(),
        )
        .padding(Padding {
            left: f32::from(design::ROW_ICON_SIZE) + space::S,
            ..Padding::ZERO
        })
        .into(),
    ])
    .spacing(space::XXS);
    let current = selected.is_some_and(|key| names(row, key));
    crate::interaction::hover(
        widget::button::custom(lines)
            .name(format!("Show {name}"))
            .class(crate::ui_controls::control_style(current, None))
            .padding([space::XS, space::S])
            .width(Length::Fill)
            .on_press_maybe(choose(row)),
    )
}

fn mentioned_row<'a>(
    list: &ConversationPullRequests,
    row: &ConversationPullRequest,
    selected: Option<&PullRequestKey>,
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
    if row.pull_request.is_none() {
        parts.push(design::caption("State unknown").into());
    } else if let Some(pull_request) = &row.pull_request
        && pull_request.state != crate::work::PullRequestState::Open
    {
        parts.extend(crate::work_ui::pull_request_badges(pull_request));
    }
    if row.by_subagent {
        parts.push(design::caption("by a subagent").into());
    }
    parts.push(link_out(row));
    let current = selected.is_some_and(|key| names(row, key));
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(parts)
                .spacing(space::S)
                .align_y(Alignment::Center),
        )
        .name(format!("Show {name}"))
        .class(crate::ui_controls::control_style(current, None))
        .padding([space::XXS, space::S])
        .width(Length::Fill)
        .on_press_maybe(choose(row)),
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

    #[test]
    fn the_default_stays_put_until_a_stronger_relation_leads() {
        let mut state = State::default();
        let conversation = TerminalId::new();
        assert!(state.retarget(Some((CardId::new(), conversation))));
        let key = |number| PullRequestKey::new(repo("me"), number).unwrap();
        assert_eq!(state.settled(None), None);
        state.update(listed(
            conversation,
            1,
            vec![row(9, Relation::Pushed), row(4, Relation::Mentioned)],
            true,
        ));
        let shown = state.settled(None);
        assert_eq!(shown, Some(key(9)));
        // A push to another branch puts it first, but it is no stronger.
        state.update(listed(
            conversation,
            2,
            vec![row(12, Relation::Pushed), row(9, Relation::Pushed)],
            false,
        ));
        let shown = state.settled(shown.as_ref());
        assert_eq!(shown, Some(key(9)));
        // A pull request the conversation created leads.
        state.update(listed(
            conversation,
            3,
            vec![row(15, Relation::Created), row(9, Relation::Pushed)],
            false,
        ));
        let shown = state.settled(shown.as_ref());
        assert_eq!(shown, Some(key(15)));
        // One no longer worked on gives way.
        state.update(listed(
            conversation,
            4,
            vec![row(12, Relation::Pushed), row(15, Relation::Mentioned)],
            false,
        ));
        assert_eq!(state.settled(shown.as_ref()), Some(key(12)));
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
        assert_eq!(state.rows().len(), 2);
        assert!(state.lists(&PullRequestKey::new(repo("me"), 2).unwrap()));
        // Another conversation's list is not this one's.
        state.update(listed(TerminalId::new(), 9, Vec::new(), true));
        assert_eq!(state.rows().len(), 2);
        state.all_mentions = true;
        assert!(state.retarget(Some((CardId::new(), TerminalId::new()))));
        assert!(state.rows().is_empty() && !state.all_mentions);
        assert!(state.retarget(None));
        assert!(state.conversation.is_none());
    }

    #[test]
    fn a_row_says_why_it_is_listed() {
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
        delegated.repository = GitHubRepoKey::new("ghe.example", None, "me", "repo").unwrap();
        assert_eq!(name(&list, &delegated), "ghe.example/me/repo#2");
        delegated.repository = repo("upstream");
        assert!(matches!(choose(&delegated), Some(Action::Choose(Some(key)))
            if key.number == 2 && key.repository == repo("upstream")));
    }
}
