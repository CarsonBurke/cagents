use super::*;
use crate::github::{
    PullRequest, PullRequestComment, PullRequestConversation, PullRequestInspection,
    PullRequestReviewThread,
};
use crate::github_identity::PullRequestKey;
use cosmic::widget::markdown;

mod conversation;

type ActionTask = iced::Task<Action>;

/// The active worktree branch, with a separate local HEAD revision for
/// refresh fencing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTarget {
    pub worktree: PathBuf,
    pub branch: String,
    pub head_oid: Option<String>,
    pub repository: GitHubRepoKey,
}

/// What the inspector shows: the active branch's pull request, or one chosen
/// from those the focused conversation worked on or mentioned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Branch(BranchTarget),
    PullRequest(PullRequestKey),
}

#[derive(Debug, Clone, Copy)]
pub enum Section {
    Description,
    Checks,
    Discussion,
}

#[derive(Debug, Clone)]
pub enum Action {
    Refresh,
    Resolved(Target, u64, Result<Resolution, String>),
    Loaded(
        Target,
        u64,
        Result<Option<Arc<PullRequestInspection>>, String>,
    ),
    Filter(String),
    ToggleResolved,
    ToggleSection(Section),
    OpenUrl(Url),
    OpenMarkdownLink(String),
    /// The focused conversation's pull requests changed.
    Conversation(conversation::Watch),
    /// Show this pull request rather than the active branch's, or with
    /// `None` return to the branch's.
    Choose(Option<PullRequestKey>),
    /// Show every mentioned pull request, or again only the first few.
    ToggleAllMentions,
}

#[derive(Debug, Clone)]
pub struct Resolution {
    target: Target,
    pull_request: Option<Arc<PullRequest>>,
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Resolved(..) | Self::Loaded(..) | Self::Conversation(..)
        )
    }
}

#[derive(Debug, Default)]
pub struct State {
    // The source-control snapshot and the inspector's observed ref are distinct:
    // a ref-only refresh must not relabel old files/history as a fresh snapshot.
    source_target: Option<Target>,
    source_key: Option<ActiveWorktreeKey>,
    target: Option<Target>,
    unavailable: Option<String>,
    generation: u64,
    cancellation: CancellationToken,
    loading: bool,
    loaded: bool,
    inspection: Option<Arc<PullRequestInspection>>,
    pull_request: Option<Arc<PullRequest>>,
    preview: Option<SidebarPullRequest>,
    error: Option<String>,
    link_error: Option<String>,
    filter: String,
    hide_resolved: bool,
    description_open: bool,
    description: Vec<markdown::Item>,
    comment_markdown: HashMap<String, Vec<markdown::Item>>,
    checks_collapsed: bool,
    discussion_collapsed: bool,
    /// The pull request chosen from the conversation's list, shown instead
    /// of the active branch's.
    chosen: Option<PullRequestKey>,
    /// The worktree that was active when it was chosen: selecting another
    /// returns the detail to that worktree's branch.
    chosen_in: Option<ActiveWorktreeKey>,
    /// Shown while nothing is chosen and the active branch has no pull
    /// request of its own: what the conversation worked on most. It stays
    /// put as the conversation works on, unless a stronger relation leads.
    default: Option<PullRequestKey>,
    /// The worktree that was active when `default` was settled.
    default_in: Option<ActiveWorktreeKey>,
    conversation: conversation::State,
}

impl From<BranchTarget> for Target {
    fn from(target: BranchTarget) -> Self {
        Self::Branch(target)
    }
}

impl Target {
    fn same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Branch(a), Self::Branch(b)) => {
                a.worktree == b.worktree && a.branch == b.branch && a.repository == b.repository
            }
            (Self::PullRequest(a), Self::PullRequest(b)) => a == b,
            _ => false,
        }
    }

    pub fn repository(&self) -> &GitHubRepoKey {
        match self {
            Self::Branch(target) => &target.repository,
            Self::PullRequest(key) => &key.repository,
        }
    }

    pub fn branch(&self) -> Option<&BranchTarget> {
        match self {
            Self::Branch(target) => Some(target),
            Self::PullRequest(_) => None,
        }
    }
}

impl State {
    /// Call after selection, repository refresh, or HEAD changes, including while
    /// another inspector tab is visible. Old completions cannot restore old data.
    pub fn synchronize(
        &mut self,
        target: Option<Target>,
        unavailable: Option<String>,
    ) -> ActionTask {
        self.unavailable = unavailable;
        let unchanged_source = self.source_target == target;
        self.source_target = target.clone();
        if self.target == target || (unchanged_source && target.is_some()) {
            return ActionTask::none();
        }
        let same_identity = self
            .target
            .as_ref()
            .zip(target.as_ref())
            .is_some_and(|(old, new)| old.same_identity(new));
        self.cancellation.cancel();
        self.generation = self.generation.wrapping_add(1);
        self.target = target;
        if same_identity {
            self.loading = false;
            if self.target.as_ref().is_some_and(|target| {
                !crate::github::polling::ready(&target.repository().host, false)
            }) {
                return ActionTask::none();
            }
            return self.refresh();
        }
        self.inspection = None;
        self.pull_request = None;
        self.preview = None;
        self.error = None;
        self.link_error = None;
        self.loaded = false;
        self.loading = false;
        self.filter.clear();
        self.description.clear();
        self.comment_markdown.clear();
        self.refresh()
    }

    fn refresh(&mut self) -> ActionTask {
        self.refresh_with(
            RepositoryService::system("git"),
            GitHubService::system("gh"),
        )
    }

    fn refresh_with<
        E: crate::repository::ProcessExecutor + 'static,
        G: crate::repository::ProcessExecutor + 'static,
    >(
        &mut self,
        git: RepositoryService<E>,
        github: GitHubService<G>,
    ) -> ActionTask {
        let Some(target) = self.target.clone() else {
            return ActionTask::none();
        };
        if !crate::github::polling::ready(&target.repository().host, true) {
            self.error = crate::github::polling::rate_limit(&target.repository().host)
                .map(|limit| limit.to_string());
            return ActionTask::none();
        }
        self.cancellation.cancel();
        self.cancellation = CancellationToken::new();
        let cancellation = self.cancellation.clone();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.link_error = None;
        // Keep this target's content visible while the disabled refresh control
        // reports the in-flight refresh. Target changes clear it in synchronize.
        ActionTask::perform(
            async move {
                let work = target.clone();
                let result = tokio::task::spawn_blocking(move || match &work {
                    Target::Branch(branch) => {
                        resolve_current_pull_request(&git, &github, branch, &cancellation)
                    }
                    Target::PullRequest(key) => github
                        .view(key, &cancellation)
                        .map(|pull_request| Resolution {
                            target: work.clone(),
                            pull_request: Some(Arc::new(pull_request)),
                        })
                        .map_err(|error| error.to_string()),
                })
                .await
                .map_err(|error| format!("Pull request loader failed: {error}"))
                .and_then(|result| result);
                Action::Resolved(target, generation, result)
            },
            |action| action,
        )
    }

    pub fn update(&mut self, action: Action) -> ActionTask {
        match action {
            Action::Refresh => {
                if let Some(target) = &self.target {
                    if let Some(pr) = self.pull_request.as_ref() {
                        GitHubService::<crate::repository::SystemProcessExecutor>::invalidate_checks(target.repository(), pr.number);
                    } else if let Some(pr) = &self.preview {
                        GitHubService::<crate::repository::SystemProcessExecutor>::invalidate_checks(target.repository(), pr.number);
                    }
                }
                return self.refresh();
            }
            Action::Resolved(target, generation, result) => {
                if !self.accepts(&target, generation) {
                    return ActionTask::none();
                }
                let resolution = match result {
                    Ok(resolution) if resolution.target.same_identity(&target) => resolution,
                    Ok(_) => {
                        self.loading = false;
                        self.error = Some("The worktree identity changed during refresh.".into());
                        return ActionTask::none();
                    }
                    Err(error) => {
                        self.loading = false;
                        self.error = Some(error);
                        return ActionTask::none();
                    }
                };
                let target = resolution.target;
                self.target = Some(target.clone());
                let pull_request = match resolution.pull_request {
                    Some(pr) => pr,
                    None => {
                        self.preview = None;
                        self.pull_request = None;
                        self.inspection = None;
                        self.comment_markdown.clear();
                        self.description.clear();
                        self.loading = false;
                        self.loaded = true;
                        return ActionTask::none();
                    }
                };
                self.description = markdown::parse(
                    pull_request
                        .body
                        .as_deref()
                        .unwrap_or("No description provided."),
                )
                .collect();
                if self
                    .pull_request
                    .as_ref()
                    .is_some_and(|old| old.number != pull_request.number)
                {
                    self.inspection = None;
                    self.comment_markdown.clear();
                }
                self.pull_request = Some(Arc::clone(&pull_request));
                self.preview = None;
                let cancellation = self.cancellation.clone();
                return ActionTask::perform(
                    async move {
                        let work = target.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            let inspection = GitHubService::system("gh")
                                .inspect_pull_request(
                                    work.repository(),
                                    (*pull_request).clone(),
                                    &cancellation,
                                )
                                .map_err(|error| error.to_string())?;
                            Ok(Some(Arc::new(inspection)))
                        })
                        .await
                        .map_err(|error| format!("Pull request loader failed: {error}"))
                        .and_then(|result| result);
                        Action::Loaded(target, generation, result)
                    },
                    |action| action,
                );
            }
            Action::Loaded(target, generation, result) => {
                if !self.accepts(&target, generation) {
                    return ActionTask::none();
                }
                self.loading = false;
                self.loaded = true;
                match result {
                    Ok(inspection) => {
                        self.comment_markdown.clear();
                        if let Some(inspection) = &inspection
                            && let Ok(conversation) = &inspection.conversation
                        {
                            for comment in conversation
                                .comments
                                .iter()
                                .chain(conversation.reviews.iter().map(|review| &review.comment))
                                .chain(
                                    conversation
                                        .threads
                                        .iter()
                                        .flat_map(|thread| &thread.comments),
                                )
                            {
                                self.comment_markdown.insert(
                                    comment.url.to_string(),
                                    markdown::parse(&comment.body).collect(),
                                );
                            }
                        }
                        self.description = markdown::parse(
                            inspection
                                .as_ref()
                                .and_then(|inspection| inspection.pull_request.body.as_deref())
                                .filter(|body| !body.is_empty())
                                .unwrap_or("No description provided."),
                        )
                        .collect();
                        self.pull_request = inspection
                            .as_ref()
                            .map(|inspection| Arc::new(inspection.pull_request.clone()));
                        self.inspection = inspection;
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            Action::ToggleSection(section) => match section {
                Section::Description => self.description_open = !self.description_open,
                Section::Checks => self.checks_collapsed = !self.checks_collapsed,
                Section::Discussion => self.discussion_collapsed = !self.discussion_collapsed,
            },
            Action::Filter(filter) => self.filter = filter,
            Action::ToggleResolved => self.hide_resolved = !self.hide_resolved,
            Action::Conversation(watch) => self.conversation.update(watch),
            Action::ToggleAllMentions => {
                self.conversation.all_mentions = !self.conversation.all_mentions;
            }
            // Link routing belongs to the application, which owns browser
            // preferences; choosing rebinds through the application too.
            Action::OpenUrl(_) | Action::OpenMarkdownLink(_) | Action::Choose(_) => {}
        }
        ActionTask::none()
    }

    fn accepts(&self, target: &Target, generation: u64) -> bool {
        self.target.as_ref() == Some(target) && self.generation == generation
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn verify_local_head_with<E: crate::repository::ProcessExecutor>(
    git: &RepositoryService<E>,
    target: &BranchTarget,
    cancellation: &CancellationToken,
) -> Result<BranchTarget, String> {
    // Resolve only the ref: scanning tracked files cannot establish a stronger
    // revision fence and becomes very expensive with active agent worktrees.
    let head = git
        .resolve_head(&target.worktree, cancellation)
        .map_err(|error| error.to_string())?;
    let branch = git
        .symbolic_head(&target.worktree, cancellation)
        .map_err(|error| error.to_string())?;
    if branch
        .as_ref()
        .and_then(|reference| reference.as_str().strip_prefix("refs/heads/"))
        == Some(target.branch.as_str())
    {
        let mut current = target.clone();
        current.head_oid = head.map(|oid| oid.as_str().to_owned());
        Ok(current)
    } else {
        Err(
            "The worktree branch changed. Refresh the repository before inspecting its pull request."
                .into(),
        )
    }
}

fn resolve_current_pull_request<
    E: crate::repository::ProcessExecutor,
    G: crate::repository::ProcessExecutor,
>(
    git: &RepositoryService<E>,
    github: &GitHubService<G>,
    target: &BranchTarget,
    cancellation: &CancellationToken,
) -> Result<Resolution, String> {
    let current = verify_local_head_with(git, target, cancellation)?;
    let branch = crate::repository::ValidatedBranchName::parse(&current.branch)
        .map_err(|error| error.to_string())?;
    // Same remote selection as worktree_head_repository, using this refresh's
    // reader so the complete path has exactly one HEAD read and no status scan.
    let head_repository = git
        .branch_remote(&current.worktree, &branch, cancellation)
        .ok()
        .flatten()
        .and_then(|remote| {
            remote
                .push_url
                .as_deref()
                .or(remote.fetch_url.as_deref())
                .and_then(crate::github::remote_repository_key)
        });
    let pull_request = match head_repository {
        Some(head_repository) => github
            .resolve_branch(
                &current.repository,
                &head_repository,
                &current.branch,
                current.head_oid.as_deref(),
                cancellation,
            )
            .map_err(|error| error.to_string())?
            .map(Arc::new),
        None => None,
    };
    Ok(Resolution {
        target: Target::Branch(current),
        pull_request,
    })
}

fn link<'a>(label: &'a str, url: &Url) -> Element<'a, Action> {
    widget::button::custom(design::caption(label))
        .class(crate::ui_controls::control_style(false, None))
        .padding([space::XS, space::S])
        .on_press(Action::OpenUrl(url.clone()))
        .into()
}

fn comment<'a>(
    comment: &'a PullRequestComment,
    reply: bool,
    items: Option<&'a Vec<markdown::Item>>,
) -> Element<'a, Action> {
    let author = comment
        .author
        .as_ref()
        .map_or("Deleted account", |author| author.login.as_str());
    let theme = cosmic::theme::active();
    let body = items.map_or_else(
        || design::body(&comment.body).width(Length::Fill).into(),
        |items| {
            markdown::view(items, design::markdown_settings(&theme, design::TEXT_BODY))
                .map(|uri| Action::OpenMarkdownLink(uri.to_string()))
        },
    );
    let actions = crate::ui_controls::icon_button(Icon::PopOut, "Open comment on GitHub")
        .compact()
        .on_press(Action::OpenUrl(comment.url.clone()))
        .into();
    widget::container(crate::ui_controls::comment_card(
        author,
        &comment.created_at,
        body,
        actions,
    ))
    .padding(cosmic::iced::Padding {
        top: 4.,
        right: 0.,
        bottom: 8.,
        left: if reply { 12. } else { 0. },
    })
    .width(Length::Fill)
    .into()
}

/// The tone and wording a pull request's own state carries.
fn state_badge<'a>(pr: &PullRequest) -> Element<'a, Action> {
    let (label, tone): (String, fn(&cosmic::Theme) -> iced::Color) = match pr.state.0.as_str() {
        "OPEN" if pr.is_draft => ("Draft".to_owned(), design::muted_color),
        "OPEN" => ("Open".to_owned(), design::success_color),
        "MERGED" => ("Merged".to_owned(), design::accent_color),
        "CLOSED" => ("Closed".to_owned(), design::destructive_color),
        // The parser keeps remote states it does not recognise rather than
        // discarding them, so the badge names whatever it was handed.
        "" => ("Unknown".to_owned(), design::muted_color),
        other => (readable_state(other), design::muted_color),
    };
    crate::ui_controls::badge(label, design::Tone::Themed(tone))
}

/// The `owner/name` a pull request URL points at, for the rare frame where the
/// panel holds a pull request but no worktree target to name its repository.
fn repository_from_url(url: &url::Url) -> String {
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [owner, name, ..] => format!("{owner}/{name}"),
        _ => url.to_string(),
    }
}

/// Whether a thread survives the panel's filters, as the list below applies them.
fn thread_shown(thread: &PullRequestReviewThread, filter: &str, hide_resolved: bool) -> bool {
    if hide_resolved && thread.resolved {
        return false;
    }
    filter.is_empty()
        || thread.path.to_lowercase().contains(filter)
        || thread
            .comments
            .iter()
            .any(|comment| comment_matches(comment, filter))
}

/// How many discussion entries the panel is about to show. The header counts
/// what the reader can see, so a filter that hides everything reads as zero
/// rather than contradicting the empty state underneath it.
fn discussion_count(
    conversation: &PullRequestConversation,
    filter: &str,
    hide_resolved: bool,
) -> usize {
    conversation
        .threads
        .iter()
        .filter(|thread| thread_shown(thread, filter, hide_resolved))
        .count()
        + conversation
            .reviews
            .iter()
            .filter(|review| comment_matches(&review.comment, filter))
            .count()
        + conversation
            .comments
            .iter()
            .filter(|item| comment_matches(item, filter))
            .count()
}

/// One label/value pair on the metadata grid.
///
/// The labels share a fixed column so the values line up down the panel;
/// concatenating them into captions left the reader scanning for the colon.
fn meta_row<'a>(label: &'a str, value: Element<'a, Action>) -> Element<'a, Action> {
    widget::row::with_children(vec![
        design::caption(label)
            .width(Length::Fixed(META_LABEL_WIDTH))
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
        value,
    ])
    .spacing(space::S)
    .align_y(Alignment::Start)
    .width(Length::Fill)
    .into()
}

/// The width the metadata labels share. Wide enough for "Repository" at
/// caption size with room to breathe, narrow enough to leave the value column
/// readable at the inspector's minimum width.
const META_LABEL_WIDTH: f32 = 80.0;

/// A block of related rows, set on its own surface so the eye reads it as one
/// thing rather than as more entries in a flat column.
fn block<'a>(children: Vec<Element<'a, Action>>) -> Element<'a, Action> {
    widget::container(
        widget::column::with_children(children)
            .spacing(space::XS)
            .width(Length::Fill),
    )
    .padding(space::S)
    .width(Length::Fill)
    .class(design::surface(
        Some(design::raised),
        [design::RADIUS_S; 4],
        true,
    ))
    .into()
}

fn section_header(
    label: impl Into<String>,
    section: Section,
    open: bool,
) -> Element<'static, Action> {
    widget::button::custom(
        widget::row::with_children(vec![
            Icon::handle(if open {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            })
            .icon()
            .size(design::ROW_ICON_SIZE)
            .into(),
            design::heading(label.into()).width(Length::Fill).into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center),
    )
    .class(crate::ui_controls::control_style(false, None))
    .height(design::DENSE_ROW_HEIGHT)
    .padding([0.0, space::XS])
    .width(Length::Fill)
    .on_press(Action::ToggleSection(section))
    .into()
}

fn readable_state(value: &str) -> String {
    value
        .split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn comment_matches(comment: &PullRequestComment, filter: &str) -> bool {
    filter.is_empty()
        || comment.body.to_lowercase().contains(filter)
        || comment
            .author
            .as_ref()
            .is_some_and(|actor| actor.login.to_lowercase().contains(filter))
}

pub fn view(state: &State) -> Element<'_, Action> {
    let visual = state.preview.as_ref().map(sidebar::pull_request_visual);
    let header = widget::row::with_children(vec![
        widget::icon(
            visual
                .as_ref()
                .map_or_else(|| Icon::PullRequest.handle(), |(icon, _)| icon.clone()),
        )
        .size(design::ROW_ICON_SIZE)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            cosmic::iced::widget::svg::Style {
                color: Some(visual.as_ref().map_or_else(
                    || theme.current_container().component.on.into(),
                    |(_, color)| *color,
                )),
            }
        })))
        .into(),
        design::heading(state.pull_request.as_ref().map_or_else(
            || {
                state
                    .preview
                    .as_ref()
                    .map_or_else(|| "Pull request".to_owned(), |pr| format!("#{}", pr.number))
            },
            |pr| format!("#{} · {}", pr.number, readable_state(&pr.state.0)),
        ))
        .width(Length::Fill)
        .into(),
        widget::container(if state.loading {
            crate::ui_controls::activity_spinner(
                f32::from(design::ROW_ICON_SIZE),
                design::warning_color,
            )
        } else {
            iced::widget::Space::new().into()
        })
        .width(design::ROW_ICON_SIZE)
        .height(design::ROW_ICON_SIZE)
        .into(),
        crate::ui_controls::icon_button(Icon::Refresh, "Refresh pull request")
            .on_press_maybe((state.target.is_some() && !state.loading).then_some(Action::Refresh))
            .into(),
    ])
    .spacing(space::XS)
    .align_y(Alignment::Center)
    .height(design::HEADER_CONTENT_HEIGHT);
    let mut children: Vec<Element<'_, Action>> = Vec::new();
    // While a pull request is loaded its metadata grid carries the repository
    // and branch; this line only orients the reader before that arrives.
    if let Some(target) = &state.target {
        if state.pull_request.is_none() {
            children.push(
                crate::design::caption(match target {
                    Target::Branch(target) => {
                        format!("{} · {}", target.repository.gh_selector(), target.branch)
                    }
                    Target::PullRequest(key) => {
                        format!(
                            "{}#{}",
                            conversation::repository_label(&key.repository),
                            key.number
                        )
                    }
                })
                .into(),
            );
        }
    } else {
        children.push(
            design::body(
                state
                    .unavailable
                    .as_deref()
                    .unwrap_or("Select a worktree to inspect its pull request."),
            )
            .into(),
        );
    }
    // Prose only where the panel would otherwise be empty. Narrating a poll over
    // content the user is already reading tells them nothing they can act on,
    // and the disabled Refresh control already reports that one is in flight.
    if state.loading && state.pull_request.is_none() && state.preview.is_none() {
        children.push(
            widget::container(cosmic::iced::widget::Space::new().height(48))
                .width(Length::Fill)
                .into(),
        );
    }
    if let Some(error) = &state.error {
        children.push(design::body(format!("Unable to load pull request: {error}")).into());
    }
    if let Some(error) = &state.link_error {
        children.push(design::body(format!("Unable to open link: {error}")).into());
    }
    if state.pull_request.is_none()
        && let Some(pr) = &state.preview
    {
        children.push(design::heading(&pr.title).into());
        children.push(link("Open pull request on GitHub", &pr.url));
    }
    if state.loaded && state.error.is_none() && state.inspection.is_none() {
        children.push(design::caption("No pull request matches this worktree branch.").into());
    }
    if let Some(pr) = state.pull_request.as_deref() {
        // The identity block: what this is, what state it is in, and where it
        // is going. Everything else in the panel is detail about it.
        let mut identity: Vec<Element<'_, Action>> = vec![
            widget::row::with_children(vec![
                state_badge(pr),
                design::caption(format!("#{}", pr.number))
                    .width(Length::Fill)
                    .into(),
                link("Open on GitHub", &pr.url),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into(),
            design::heading(&pr.title)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .into(),
            widget::row::with_children(vec![
                crate::ui_controls::passive_icon(
                    sidebar::worktree_icon(),
                    design::COMPACT_ICON_SIZE,
                ),
                design::caption(format!("{} → {}", pr.head_ref_name, pr.base_ref_name))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into(),
        ];
        // Only a real disagreement earns a warning. An absent local or remote
        // OID is not evidence of one, and the local branch is as often ahead
        // of the pushed head as behind it.
        let local_head = state
            .target
            .as_ref()
            .and_then(Target::branch)
            .and_then(|target| target.head_oid.as_deref())
            .filter(|oid| !oid.is_empty());
        if let Some(local_head) = local_head
            && !pr.head_ref_oid.is_empty()
            && local_head != pr.head_ref_oid
        {
            identity.push(
                widget::row::with_children(vec![
                    crate::ui_controls::badge("Out of sync", design::Tone::Themed(design::warning_color)),
                    design::caption(
                        "Local HEAD differs from the pull request head; checks describe the remote commit.",
                    )
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Start)
                .width(Length::Fill)
                .into(),
            );
        }
        children.push(block(identity));
        let mut metadata: Vec<Element<'_, Action>> = Vec::new();
        metadata.push(meta_row(
            "Repository",
            design::body(state.target.as_ref().map_or_else(
                // The panel can outlive its target for a frame; the pull
                // request's own URL still names where it lives.
                || repository_from_url(&pr.url),
                |target| target.repository().gh_selector(),
            ))
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .width(Length::Fill)
            .into(),
        ));
        // The identity block names the remote head. Where the checked-out
        // branch is not that branch, saying so is the whole point of the row.
        if let Some(target) = state.target.as_ref().and_then(Target::branch)
            && target.branch != pr.head_ref_name
        {
            metadata.push(meta_row(
                "Branch",
                design::body(target.branch.clone())
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill)
                    .into(),
            ));
        }
        let decision = pr
            .review_decision
            .as_ref()
            .map(|decision| decision.0.as_str())
            .filter(|value| !value.is_empty());
        metadata.push(meta_row(
            "Review",
            match decision {
                Some("APPROVED") => crate::ui_controls::badge(
                    "Approved",
                    design::Tone::Themed(design::success_color),
                ),
                Some("CHANGES_REQUESTED") => crate::ui_controls::badge(
                    "Changes requested",
                    design::Tone::Themed(design::destructive_color),
                ),
                Some(other) => crate::ui_controls::badge(
                    readable_state(other),
                    design::Tone::Themed(design::muted_color),
                ),
                None => design::caption("No decision reported").into(),
            },
        ));
        if let Some(inspection) = &state.inspection
            && let Ok(checks) = &inspection.checks
            && !checks.is_empty()
        {
            // Classify through the sidebar's own predicates so one pull
            // request cannot read as failing in the list and passing here.
            let count = |predicate: fn(&str) -> bool| {
                checks
                    .iter()
                    .filter(|check| predicate(check.bucket.0.as_str()))
                    .count()
            };
            let failing = count(sidebar::is_check_failure);
            let pending = count(sidebar::is_check_pending);
            let (summary, tone): (String, fn(&cosmic::Theme) -> iced::Color) = if failing > 0 {
                (format!("{failing} failing"), design::destructive_color)
            } else if pending > 0 {
                (format!("{pending} pending"), design::warning_color)
            } else {
                (
                    format!("{} passing", count(sidebar::is_check_success)),
                    design::success_color,
                )
            };
            metadata.push(meta_row(
                "Checks",
                crate::ui_controls::badge(summary, design::Tone::Themed(tone)),
            ));
        }
        children.push(block(metadata));
        children.push(section_header(
            "Description",
            Section::Description,
            state.description_open,
        ));
        if state.description_open {
            let theme = cosmic::theme::active();
            let settings = design::markdown_settings(&theme, design::TEXT_BODY);
            children.push(
                markdown::view(&state.description, settings)
                    .map(|uri| Action::OpenMarkdownLink(uri.to_string())),
            );
        }
    }
    if let Some(inspection) = &state.inspection {
        let checks_label = inspection.checks.as_ref().map_or_else(
            |_| "Checks".to_owned(),
            |checks| format!("Checks ({})", checks.len()),
        );
        children.push(section_header(
            checks_label,
            Section::Checks,
            !state.checks_collapsed,
        ));
        if !state.checks_collapsed {
            match &inspection.checks {
                Ok(checks) if checks.is_empty() => {
                    children.push(design::caption("No checks reported.").into())
                }
                Ok(checks) => {
                    for check in checks {
                        let (symbol, tone): (_, fn(&cosmic::Theme) -> cosmic::iced::Color) =
                            match check.bucket.0.as_str() {
                                bucket if sidebar::is_check_failure(bucket) => {
                                    (Icon::Error, design::destructive_color)
                                }
                                bucket if sidebar::is_check_pending(bucket) => {
                                    (Icon::Loading, design::warning_color)
                                }
                                bucket if sidebar::is_check_success(bucket) => {
                                    (Icon::Check, design::success_color)
                                }
                                _ => (Icon::Info, design::muted_color),
                            };
                        let mut content = widget::column::with_children(vec![
                            design::body(&check.name).width(Length::Fill).into(),
                            design::caption(format!(
                                "{}{}{}",
                                readable_state(&check.state.0),
                                if check.workflow.is_empty() {
                                    ""
                                } else {
                                    " · "
                                },
                                check.workflow
                            ))
                            .width(Length::Fill)
                            .into(),
                        ])
                        .width(Length::Fill);
                        if !check.description.is_empty() {
                            content = content
                                .push(design::caption(&check.description).width(Length::Fill));
                        }
                        children.push(
                            widget::button::custom(
                                widget::row::with_children(vec![
                                    // A fixed column for the status glyph, so
                                    // every check's name starts at the same x
                                    // and the run of outcomes reads down the
                                    // panel as one column.
                                    widget::container(
                                        symbol.handle().icon().size(design::ROW_ICON_SIZE),
                                    )
                                    .width(Length::Fixed(design::ROW_ICON_SIZE.into()))
                                    .align_x(Alignment::Center)
                                    .into(),
                                    content.into(),
                                    widget::container(
                                        Icon::PopOut
                                            .handle()
                                            .icon()
                                            .size(design::COMPACT_ICON_SIZE),
                                    )
                                    .width(Length::Fixed(design::COMPACT_ICON_SIZE.into()))
                                    .align_x(Alignment::Center)
                                    .into(),
                                ])
                                .spacing(space::S)
                                .align_y(Alignment::Center),
                            )
                            .class(crate::ui_controls::control_style(
                                false,
                                Some(design::Tone::Themed(tone)),
                            ))
                            .padding([space::XS, space::S])
                            .width(Length::Fill)
                            .on_press(Action::OpenUrl(check.link.clone()))
                            .into(),
                        );
                    }
                }
                Err(error) => {
                    children.push(design::caption(format!("Checks unavailable: {error}")).into())
                }
            }
        }
        let discussion_label = inspection.conversation.as_ref().map_or_else(
            |_| "Discussion".to_owned(),
            |conversation| {
                format!(
                    "Discussion ({})",
                    discussion_count(
                        conversation,
                        &state.filter.to_lowercase(),
                        state.hide_resolved,
                    )
                )
            },
        );
        children.push(section_header(
            discussion_label,
            Section::Discussion,
            !state.discussion_collapsed,
        ));
        if !state.discussion_collapsed {
            match &inspection.conversation {
                Err(error) => children
                    .push(design::caption(format!("Discussion unavailable: {error}")).into()),
                Ok(conversation) => {
                    // The two controls act on one list, so they sit on one
                    // row rather than stacking as two more full-width entries.
                    children.push(
                        widget::row::with_children(vec![
                            crate::ui_controls::field::search(
                                "Filter author, text, or file",
                                &state.filter,
                            )
                            .on_input(Action::Filter)
                            .width(Length::Fill)
                            .into(),
                            widget::button::custom(design::caption(if state.hide_resolved {
                                "Show resolved"
                            } else {
                                "Hide resolved"
                            }))
                            .class(crate::ui_controls::control_style(state.hide_resolved, None))
                            .height(design::DENSE_ROW_HEIGHT)
                            .on_press(Action::ToggleResolved)
                            .into(),
                        ])
                        .spacing(space::XS)
                        .align_y(Alignment::Center)
                        .width(Length::Fill)
                        .into(),
                    );
                    let filter = state.filter.to_lowercase();
                    let mut shown = 0;
                    for thread in &conversation.threads {
                        if !thread_shown(thread, &filter, state.hide_resolved) {
                            continue;
                        }
                        shown += 1;
                        let location = thread.line.map_or_else(
                            || thread.path.clone(),
                            |line| format!("{}:{line}", thread.path),
                        );
                        // A thread is one conversation about one place in the
                        // diff. Grouping it on its own surface keeps a reply
                        // from reading as the start of the next thread, which
                        // a bare rule between them never made clear.
                        let mut thread_rows: Vec<Element<'_, Action>> = vec![
                            widget::row::with_children({
                                let mut head: Vec<Element<'_, Action>> = vec![
                                    design::body(location)
                                        .wrapping(iced::widget::text::Wrapping::Word)
                                        .width(Length::Fill)
                                        .into(),
                                ];
                                if thread.outdated {
                                    head.push(crate::ui_controls::badge(
                                        "Outdated",
                                        design::Tone::Themed(design::muted_color),
                                    ));
                                }
                                head.push(if thread.resolved {
                                    crate::ui_controls::badge(
                                        "Resolved",
                                        design::Tone::Themed(design::success_color),
                                    )
                                } else {
                                    crate::ui_controls::badge(
                                        "Unresolved",
                                        design::Tone::Themed(design::warning_color),
                                    )
                                });
                                head
                            })
                            .spacing(space::XS)
                            .align_y(Alignment::Center)
                            .width(Length::Fill)
                            .into(),
                        ];
                        // Preserve the whole matching thread so replies retain context.
                        for (index, item) in thread.comments.iter().enumerate() {
                            thread_rows.push(comment(
                                item,
                                index != 0,
                                state.comment_markdown.get(item.url.as_str()),
                            ));
                        }
                        if thread.truncated {
                            thread_rows.push(crate::design::caption("Showing the first 100 comments in this thread. Open its comment link for the full discussion.").into());
                        }
                        children.push(block(thread_rows));
                    }
                    if !conversation.reviews.is_empty() {
                        children.push(design::heading("Reviews").into());
                    }
                    for review in &conversation.reviews {
                        if comment_matches(&review.comment, &filter) {
                            shown += 1;
                            let tone: fn(&cosmic::Theme) -> iced::Color =
                                match review.state.0.as_str() {
                                    "APPROVED" => design::success_color,
                                    "CHANGES_REQUESTED" => design::destructive_color,
                                    _ => design::muted_color,
                                };
                            children.push(block(vec![
                                crate::ui_controls::badge(
                                    readable_state(&review.state.0),
                                    design::Tone::Themed(tone),
                                ),
                                comment(
                                    &review.comment,
                                    false,
                                    state.comment_markdown.get(review.comment.url.as_str()),
                                ),
                            ]));
                        }
                    }
                    if !conversation.comments.is_empty() {
                        children.push(design::heading("Comments").into());
                    }
                    for item in &conversation.comments {
                        if comment_matches(item, &filter) {
                            shown += 1;
                            children.push(block(vec![comment(
                                item,
                                false,
                                state.comment_markdown.get(item.url.as_str()),
                            )]));
                        }
                    }
                    if shown == 0 {
                        children.push(
                            design::caption(if filter.is_empty() && !state.hide_resolved {
                                "No discussion comments reported."
                            } else {
                                "No comments match these filters."
                            })
                            .into(),
                        );
                    }
                    if conversation.truncated {
                        children.push(crate::design::caption("Showing up to 200 review threads, 200 reviews, and 200 issue comments. Open the pull request for the full discussion.").into());
                    }
                }
            }
        }
    }
    let body = widget::scrollable(
        widget::column::with_children(children)
            .spacing(space::S)
            .padding(design::with_vertical_rail(cosmic::iced::Padding::from([
                space::XS,
                space::S,
            ])))
            .width(Length::Fill),
    )
    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
        crate::design::scrollbar(),
    ))
    .height(Length::Fill)
    .width(Length::Fill);
    widget::column::with_children(vec![
        widget::container(header)
            .padding([0.0, space::S])
            .width(Length::Fill)
            .into(),
        crate::design::horizontal_rule().into(),
        body.into(),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

impl Cagents {
    pub(super) fn handle_pull_request_ui(&mut self, action: Action) -> Task<Message> {
        if matches!(&action, Action::Resolved(..) | Action::Loaded(..))
            && !self.pull_request_binding_is_current()
        {
            return Task::none();
        }
        match action {
            Action::Choose(key) => {
                // Choosing the chosen row again returns to the default. The
                // active branch's own pull request is shown as the branch's,
                // which also compares it with the local head.
                let again = |key: &PullRequestKey| {
                    self.pull_request_inspector
                        .chosen
                        .as_ref()
                        .is_some_and(|chosen| conversation::same_pull_request(chosen, key))
                };
                let key = key.filter(|key| {
                    !again(key)
                        && !self
                            .active_branch_pull_request()
                            .is_some_and(|active| conversation::same_pull_request(&active, key))
                });
                self.pull_request_inspector.chosen_in = key
                    .as_ref()
                    .and_then(|_| self.sidebar_state.active_worktree().cloned());
                self.pull_request_inspector.chosen = key;
                return self.sync_pull_request_inspector();
            }
            Action::Conversation(watch) => {
                drop(
                    self.pull_request_inspector
                        .update(Action::Conversation(watch)),
                );
                // A pull request the conversation no longer lists is no
                // longer one to show for it.
                let inspector = &mut self.pull_request_inspector;
                if inspector
                    .chosen
                    .as_ref()
                    .is_some_and(|key| !inspector.conversation.lists(key))
                {
                    inspector.chosen = None;
                }
                return self.sync_pull_request_inspector();
            }
            action => self.apply_pull_request_ui(action),
        }
    }

    fn apply_pull_request_ui(&mut self, action: Action) -> Task<Message> {
        let url = match &action {
            Action::OpenUrl(url) => Some(Ok(url.clone())),
            Action::OpenMarkdownLink(uri) => Some(Url::parse(uri)),
            _ => None,
        };
        if let Some(url) = url {
            return match url {
                Ok(url)
                    if matches!(url.scheme(), "https" | "http")
                        && url.username().is_empty()
                        && url.password().is_none() =>
                {
                    self.pull_request_inspector.link_error = None;
                    self.open_clicked_link(url)
                }
                _ => {
                    self.pull_request_inspector.link_error =
                        Some("Only HTTP and HTTPS links without credentials can be opened.".into());
                    Task::none()
                }
            };
        }
        let accepted = match &action {
            Action::Resolved(target, generation, Ok(_))
            | Action::Loaded(target, generation, Ok(_)) => {
                self.pull_request_inspector.accepts(target, *generation)
            }
            _ => false,
        };
        let task = self.pull_request_inspector.update(action);
        // A chosen pull request is no worktree's: only the branch's updates
        // the sidebar's rows.
        if accepted
            && let Some(Target::Branch(target)) = self.pull_request_inspector.target.as_ref()
        {
            for project in &mut self.sidebar_model.projects {
                if self
                    .github_availability
                    .get(&project.id)
                    .and_then(GitHubAvailability::repository_key)
                    == Some(&target.repository)
                    && let Some(worktree) = project
                        .worktrees
                        .iter_mut()
                        .find(|worktree| worktree.path == target.worktree)
                {
                    let Some(pr) = self.pull_request_inspector.pull_request.as_ref() else {
                        worktree.clear_pull_request();
                        continue;
                    };
                    let checks = self
                        .pull_request_inspector
                        .inspection
                        .as_ref()
                        .and_then(|inspection| inspection.checks.as_ref().ok());
                    let mut row = SidebarPullRequest::from_github(
                        project.id,
                        pr,
                        checks.map_or(&[], Vec::as_slice),
                    );
                    if checks.is_none()
                        && pr.status_check_rollup.is_none()
                        && let Some(previous) = project.pull_requests.iter().find(|row| {
                            row.number == pr.number
                                && row.head_oid.as_deref() == Some(pr.head_ref_oid.as_str())
                        })
                    {
                        row.checks = previous.checks;
                    }
                    worktree.set_pull_request(row.clone());
                    if let Some(previous) = project
                        .pull_requests
                        .iter_mut()
                        .find(|previous| previous.number == row.number)
                    {
                        *previous = row;
                    } else {
                        project.pull_requests.push(row);
                    }
                }
            }
        }
        task.map(Message::PullRequestUi).map(cosmic::Action::App)
    }

    /// The chat card whose conversation the inspector lists pull requests
    /// for: the focused one, else the last one focused while it remains in
    /// the workspace.
    fn inspected_conversation(&self) -> Option<(CardId, TerminalId)> {
        let chat = |card: CardId| match &self.card_state(card)?.kind {
            CardKind::Chat(session) => Some((card, session.id)),
            _ => None,
        };
        self.workspace.focused().and_then(chat).or_else(|| {
            self.pull_request_inspector
                .conversation
                .card
                .filter(|card| self.workspace.card(*card).is_some())
                .and_then(chat)
        })
    }

    /// The pull request the sidebar knows for the active worktree's branch.
    fn active_branch_pull_request(&self) -> Option<PullRequestKey> {
        if self.active_folder_scope() {
            return None;
        }
        let source = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| self.sidebar_state.active_worktree() == Some(&source.key))?;
        let branch = source.branch.as_ref()?.as_str();
        let repository = self
            .github_availability
            .get(&source.key.project_id)?
            .repository_key()?;
        let number = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.id == source.key.project_id)?
            .worktrees
            .iter()
            .find(|row| {
                row.path == source.key.canonical_path && row.branch.as_deref() == Some(branch)
            })?
            .pull_request()?
            .number;
        PullRequestKey::new(repository.clone(), number).ok()
    }

    /// The pull request the detail shows instead of the active branch's:
    /// the one chosen from the list or, while the branch has none of its
    /// own, the one the conversation worked on most, kept until it works on
    /// another more closely or the worktree or conversation changes.
    fn shown_pull_request(&self) -> Option<PullRequestKey> {
        let state = &self.pull_request_inspector;
        state.chosen.clone().or_else(|| state.default.clone())
    }

    /// Whether the inspected conversation names any pull request, which
    /// opens the pull request tab even where no worktree branch would.
    pub(super) fn conversation_lists_pull_requests(&self) -> bool {
        self.pull_request_inspector.conversation.lists_any()
    }

    /// The inspected conversation's pull requests, watched while the
    /// inspector is shown.
    pub(super) fn conversation_pull_request_subscription(&self) -> Subscription<Message> {
        match self.pull_request_inspector.conversation.conversation {
            Some(conversation) if self.context_inspector_visible() => {
                conversation::subscription(conversation)
                    .map(|watch| Message::PullRequestUi(Action::Conversation(watch)))
            }
            _ => Subscription::none(),
        }
    }

    pub(super) fn sync_pull_request_inspector(&mut self) -> Task<Message> {
        let inspected = self.inspected_conversation();
        let active = self.sidebar_state.active_worktree().cloned();
        let branch_has_one = self.active_branch_pull_request().is_some();
        let inspector = &mut self.pull_request_inspector;
        let retargeted = inspector.conversation.retarget(inspected);
        if retargeted || inspector.chosen_in != active {
            inspector.chosen = None;
            inspector.chosen_in = None;
        }
        if retargeted || inspector.default_in != active {
            inspector.default = None;
        }
        inspector.default_in = active;
        inspector.default = if branch_has_one {
            None
        } else {
            inspector.conversation.settled(inspector.default.as_ref())
        };
        if !self.context_inspector_open || self.context_tab != ContextTab::PullRequest {
            self.pull_request_inspector.source_key = None;
            return self
                .pull_request_inspector
                .synchronize(None, None)
                .map(Message::PullRequestUi)
                .map(cosmic::Action::App);
        }
        if let Some(key) = self.shown_pull_request() {
            self.pull_request_inspector.source_key = None;
            let target = Target::PullRequest(key);
            if self.pull_request_inspector.source_target.as_ref() == Some(&target) {
                return Task::none();
            }
            return self
                .pull_request_inspector
                .synchronize(Some(target), None)
                .map(Message::PullRequestUi)
                .map(cosmic::Action::App);
        }
        let source = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| self.sidebar_state.active_worktree() == Some(&source.key));
        let source_key = source.map(|source| &source.key);
        if self.pull_request_inspector.source_key.as_ref() != source_key {
            drop(self.pull_request_inspector.synchronize(None, None));
            self.pull_request_inspector.source_key = source_key.cloned();
        }
        let candidate = source
            .filter(|source| source.data_state == SidebarDataState::Ready)
            .and_then(|source| {
                let branch = source.branch.as_ref()?.as_str();
                let repository = self
                    .github_availability
                    .get(&source.key.project_id)?
                    .repository_key()?;
                Some((
                    &source.key.canonical_path,
                    branch,
                    source.head_oid.as_deref(),
                    repository,
                ))
            });
        let Some((worktree, branch, head_oid, repository)) = candidate else {
            let reason = match source {
                None => "Select a worktree to inspect its pull request.",
                Some(source) if source.data_state == SidebarDataState::Loading => {
                    "Loading the active worktree..."
                }
                Some(source) if source.data_state != SidebarDataState::Ready => {
                    "Refresh the worktree to inspect its pull request."
                }
                Some(source) if source.branch.is_none() => "This worktree has no named branch.",
                Some(_) => "Connect this repository to GitHub to inspect pull requests.",
            };
            let reason = source
                .filter(|source| {
                    source.data_state == SidebarDataState::Ready && source.branch.is_some()
                })
                .and_then(|source| self.github_availability.get(&source.key.project_id))
                .map_or_else(|| reason.to_owned(), GitHubAvailability::guidance);
            if self.pull_request_inspector.target.is_none()
                && self.pull_request_inspector.unavailable.as_deref() == Some(reason.as_str())
            {
                return Task::none();
            }
            return self
                .pull_request_inspector
                .synchronize(None, Some(reason))
                .map(Message::PullRequestUi)
                .map(cosmic::Action::App);
        };
        if let Some(Target::Branch(target)) = self.pull_request_inspector.source_target.as_ref()
            && target.worktree == *worktree
            && target.branch == branch
            && target.head_oid.as_deref() == head_oid
            && target.repository == *repository
        {
            return Task::none();
        }
        let target = Target::Branch(BranchTarget {
            worktree: worktree.clone(),
            branch: branch.to_owned(),
            head_oid: head_oid.map(str::to_owned),
            repository: repository.clone(),
        });
        let preview = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| {
                self.github_availability
                    .get(&project.id)
                    .and_then(GitHubAvailability::repository_key)
                    == Some(repository)
            })
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|row| row.path == *worktree && row.branch.as_deref() == Some(branch))
                    .and_then(|row| row.pull_request().cloned())
            });
        let previous_generation = self.pull_request_inspector.generation;
        let task = self.pull_request_inspector.synchronize(Some(target), None);
        if self.pull_request_inspector.generation != previous_generation {
            self.pull_request_inspector.preview = preview;
        }
        task.map(Message::PullRequestUi).map(cosmic::Action::App)
    }

    /// The application supplies the visible/focused 30-second cadence.
    pub(super) fn maybe_refresh_pull_request(&mut self) -> Task<Message> {
        self.maybe_refresh_pull_request_with(
            RepositoryService::system("git"),
            GitHubService::system("gh"),
        )
    }

    fn pull_request_binding_is_current(&self) -> bool {
        let state = &self.pull_request_inspector;
        if let Some(key) = &self.shown_pull_request() {
            return self.context_inspector_open
                && self.context_tab == ContextTab::PullRequest
                && matches!(&state.source_target, Some(Target::PullRequest(bound)) if bound == key);
        }
        self.context_inspector_open
            && self.context_tab == ContextTab::PullRequest
            && state.source_key.as_ref() == self.sidebar_state.active_worktree()
            && self
                .sidebar_model
                .source_control
                .as_ref()
                .is_some_and(|source| {
                    state.source_key.as_ref() == Some(&source.key)
                        && source.data_state == SidebarDataState::Ready
                        && state
                            .source_target
                            .as_ref()
                            .and_then(Target::branch)
                            .is_some_and(|target| {
                                target.worktree == source.key.canonical_path
                                    && source.branch.as_ref().map(|branch| branch.as_str())
                                        == Some(target.branch.as_str())
                                    && source.head_oid.as_deref() == target.head_oid.as_deref()
                                    && self
                                        .github_availability
                                        .get(&source.key.project_id)
                                        .and_then(GitHubAvailability::repository_key)
                                        == Some(&target.repository)
                            })
                })
    }

    fn maybe_refresh_pull_request_with<
        E: crate::repository::ProcessExecutor + 'static,
        G: crate::repository::ProcessExecutor + 'static,
    >(
        &mut self,
        git: RepositoryService<E>,
        github: GitHubService<G>,
    ) -> Task<Message> {
        if !self.context_inspector_open
            || self.context_tab != ContextTab::PullRequest
            || !self.pull_request_binding_is_current()
            || self.pull_request_inspector.loading
            || self
                .pull_request_inspector
                .target
                .as_ref()
                .is_none_or(|target| {
                    !crate::github::polling::ready(&target.repository().host, false)
                })
        {
            return Task::none();
        }
        self.pull_request_inspector
            .refresh_with(git, github)
            .map(Message::PullRequestUi)
            .map(cosmic::Action::App)
    }

    pub(super) fn pull_request_panel(&self, width: f32) -> Element<'_, Message> {
        let state = &self.pull_request_inspector;
        let selected = match &state.target {
            Some(Target::PullRequest(key)) => Some(key.clone()),
            Some(Target::Branch(target)) => state
                .pull_request
                .as_ref()
                .and_then(|pr| PullRequestKey::new(target.repository.clone(), pr.number).ok()),
            None => None,
        };
        let title = state
            .conversation
            .card
            .and_then(|card| self.card_state(card))
            .map(|card| card.title.as_str())
            .filter(|title| !title.is_empty());
        let list = conversation::view(&state.conversation, title, selected.as_ref());
        // Without a branch or a choice, or on a branch with no pull request,
        // the list is the panel's content.
        let prompt = match &state.target {
            None => Some("Choose a pull request to see its checks and discussion.".to_owned()),
            Some(Target::Branch(target))
                if state.loaded
                    && state.error.is_none()
                    && state.inspection.is_none()
                    && state.pull_request.is_none() =>
            {
                Some(format!(
                    "{} has no pull request. Choose one above to see its checks and discussion.",
                    target.branch
                ))
            }
            Some(_) => None,
        };
        let detail: Element<'_, Action> = match (&list, prompt) {
            (Some(_), Some(prompt)) => widget::container(
                design::caption(prompt)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill),
            )
            .padding([space::S, space::S])
            .width(Length::Fill)
            .into(),
            _ => view(state),
        };
        let content: Element<'_, Action> = match list {
            Some(list) => widget::column::with_children(vec![
                widget::container(
                    widget::scrollable(widget::container(list).padding(
                        design::with_vertical_rail(Padding::from([space::S, space::S])),
                    ))
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .width(Length::Fill),
                )
                // The list keeps room for the detail below it.
                .max_height((self.window_height * 0.4).max(160.0))
                .width(Length::Fill)
                .into(),
                design::horizontal_rule().into(),
                detail,
            ])
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
            None => detail,
        };
        widget::container(content.map(Message::PullRequestUi))
            .width(Length::Fixed(width))
            .height(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput};

    struct RefreshFixture {
        head: std::sync::Mutex<String>,
        calls: std::sync::Mutex<Vec<CommandSpec>>,
        caller: std::thread::ThreadId,
        host: &'static str,
    }

    impl ProcessExecutor for RefreshFixture {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            assert_ne!(std::thread::current().id(), self.caller);
            self.calls.lock().unwrap().push(command.clone());
            let has = |value: &str| command.arguments.iter().any(|argument| argument == value);
            let stdout = if has("rev-parse") {
                format!("{}\n", self.head.lock().unwrap()).into_bytes()
            } else if has("symbolic-ref") {
                b"refs/heads/feature\n".to_vec()
            } else if has("config") {
                format!("remote.origin.url\nhttps://{}/owner/repo.git\0", self.host).into_bytes()
            } else {
                assert!(has("pr") && has("list"), "unexpected fixture request");
                serde_json::to_vec(&vec![self.pull_request()]).unwrap()
            };
            Ok(ProcessOutput {
                success: true,
                exit_code: Some(0),
                signal: None,
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    impl RefreshFixture {
        fn pull_request(&self) -> PullRequest {
            let mut value =
                serde_json::to_value(crate::pull_request_review::tests::pull_request(7)).unwrap();
            value["url"] = format!("https://{}/owner/repo/pull/7", self.host).into();
            value["body"] = "Fresh remote description".into();
            value["headRepository"] = serde_json::json!({"name":"repo"});
            value["headRepositoryOwner"] = serde_json::json!({"login":"owner"});
            serde_json::from_value(value).unwrap()
        }
    }

    fn refresh_app(host: &'static str) -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let worktree = PathBuf::from("/repo/feature");
        let project = ProjectId::new();
        app.context_inspector_open = true;
        app.context_tab = ContextTab::PullRequest;
        app.github_availability.insert(
            project,
            GitHubAvailability::Ready {
                repository_key: GitHubRepoKey::new(host, None, "owner", "repo").unwrap(),
                pull_requests: Vec::new(),
                refreshed_at: 1,
            },
        );
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            crate::repository::RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                worktree.clone(),
                Some("feature".into()),
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        ));
        app.sidebar_state
            .sync_active_worktree(&app.sidebar_model, &SidebarFocus::Repository { worktree });
        let key = app.sidebar_state.active_worktree().unwrap().clone();
        let mut source = SourceControlModel::loading(key);
        source.data_state = SidebarDataState::Ready;
        source.branch = Some(crate::repository::ValidatedBranchName::parse("feature").unwrap());
        source.head_oid = Some("a".repeat(40).into_boxed_str());
        app.sidebar_model.source_control = Some(source);
        drop(app.sync_pull_request_inspector());
        app.pull_request_inspector.loading = false;
        app.pull_request_inspector.loaded = true;
        app.pull_request_inspector.filter = "keep this filter".into();
        app.pull_request_inspector.description_open = true;
        app.pull_request_inspector.checks_collapsed = true;
        app.pull_request_inspector.description = markdown::parse("Loaded description").collect();
        app
    }

    #[test]
    fn automatic_refresh_observes_external_head_without_rebinding_or_rescanning() {
        use cosmic::iced::futures::StreamExt;
        let host = "automatic-inspector-refresh.invalid";
        let mut app = refresh_app(host);
        let requested = app.pull_request_inspector.target.clone().unwrap();
        let source = app.sidebar_model.source_control.clone();
        let key = app.sidebar_state.active_worktree().cloned();
        let fixture = Arc::new(RefreshFixture {
            head: std::sync::Mutex::new("b".repeat(40)),
            calls: Default::default(),
            caller: std::thread::current().id(),
            host,
        });
        let old = Arc::new(PullRequestInspection {
            pull_request: fixture.pull_request(),
            checks: Ok(Vec::new()),
            conversation: Ok(Default::default()),
        });
        app.pull_request_inspector.pull_request = Some(Arc::new(old.pull_request.clone()));
        app.pull_request_inspector.inspection = Some(old.clone());
        let task = app.maybe_refresh_pull_request_with(
            RepositoryService::new("git", fixture.clone()),
            GitHubService::new("gh", fixture.clone()),
        );
        let generation = app.pull_request_inspector.generation;
        assert!(app.pull_request_inspector.loading);
        assert!(Arc::ptr_eq(
            app.pull_request_inspector.inspection.as_ref().unwrap(),
            &old
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut outputs = 0;
                if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                    while let Some(action) = stream.next().await {
                        if let iced::runtime::Action::Output(cosmic::Action::App(
                            Message::PullRequestUi(action),
                        )) = action
                        {
                            assert!(matches!(&action, Action::Resolved(_, _, Ok(_))));
                            outputs += 1;
                            drop(app.handle_pull_request_ui(action));
                        }
                    }
                }
                assert_eq!(outputs, 1);
            });
        let observed = app.pull_request_inspector.target.clone().unwrap();
        assert_eq!(
            observed.branch().unwrap().head_oid.as_deref(),
            Some("b".repeat(40).as_str())
        );
        // A local commit need not be pushed: the unique, proven source branch
        // still owns the same PR at its previous hosted tip.
        let resolved = app.pull_request_inspector.pull_request.as_ref().unwrap();
        assert_eq!(resolved.number, 7);
        assert_eq!(resolved.head_ref_oid, "a".repeat(40));
        assert_eq!(app.sidebar_state.active_worktree(), key.as_ref());
        assert_eq!(
            app.sidebar_model.source_control, source,
            "ref read is not a full file/history snapshot"
        );
        assert_eq!(
            app.pull_request_inspector.source_target.as_ref(),
            Some(&requested)
        );
        drop(app.sync_pull_request_inspector());
        assert_eq!(app.pull_request_inspector.target.as_ref(), Some(&observed));
        assert_eq!(app.pull_request_inspector.generation, generation);
        assert!(app.pull_request_inspector.loaded && app.pull_request_inspector.loading);
        assert_eq!(app.pull_request_inspector.filter, "keep this filter");
        assert!(
            app.pull_request_inspector.description_open
                && app.pull_request_inspector.checks_collapsed
        );
        assert!(Arc::ptr_eq(
            app.pull_request_inspector.inspection.as_ref().unwrap(),
            &old
        ));
        drop(app.handle_pull_request_ui(Action::Loaded(
            requested.clone(),
            generation,
            Err("old HEAD".into()),
        )));
        drop(app.handle_pull_request_ui(Action::Loaded(
            observed.clone(),
            generation.wrapping_sub(1),
            Err("old generation".into()),
        )));
        assert!(app.pull_request_inspector.error.is_none());
        let fresh = Arc::new(PullRequestInspection {
            pull_request: fixture.pull_request(),
            checks: Ok(Vec::new()),
            conversation: Ok(Default::default()),
        });
        drop(app.handle_pull_request_ui(Action::Loaded(
            observed.clone(),
            generation,
            Ok(Some(fresh.clone())),
        )));
        assert!(Arc::ptr_eq(
            app.pull_request_inspector.inspection.as_ref().unwrap(),
            &fresh
        ));
        assert!(!app.pull_request_inspector.loading);
        // A later full snapshot catching up to the ref-only result starts no duplicate work.
        app.sidebar_model.source_control.as_mut().unwrap().head_oid = observed
            .branch()
            .unwrap()
            .head_oid
            .clone()
            .map(String::into_boxed_str);
        drop(app.sync_pull_request_inspector());
        assert_eq!(app.pull_request_inspector.generation, generation);
        assert_eq!(app.pull_request_inspector.target.as_ref(), Some(&observed));
        assert_eq!(
            app.pull_request_inspector.source_target.as_ref(),
            Some(&observed)
        );
        let calls = fixture.calls.lock().unwrap();
        for (name, count) in [
            ("rev-parse", 1),
            ("symbolic-ref", 1),
            ("config", 1),
            ("status", 0),
            ("list", 1),
        ] {
            assert_eq!(
                calls
                    .iter()
                    .filter(|command| command.arguments.iter().any(|arg| arg == name))
                    .count(),
                count,
                "{name}"
            );
        }
        assert_eq!(calls.len(), 4);
        crate::github::polling::succeeded(host);
    }

    #[test]
    fn an_old_refresh_cannot_restore_a_replaced_worktree_binding_at_the_same_path() {
        let mut app = refresh_app("replaced-inspector-binding.invalid");
        let old = app.pull_request_inspector.target.clone().unwrap();
        let generation = app.pull_request_inspector.generation;
        let next_id = crate::model::WorktreeId::new();
        app.sidebar_model.projects[0].worktrees[0].id = next_id;
        app.sidebar_state.sync_active_worktree(
            &app.sidebar_model,
            &SidebarFocus::Repository {
                worktree: old.branch().unwrap().worktree.clone(),
            },
        );
        app.sidebar_model
            .source_control
            .as_mut()
            .unwrap()
            .key
            .worktree_id = next_id;
        drop(app.handle_pull_request_ui(Action::Resolved(
            old.clone(),
            generation,
            Ok(Resolution {
                target: old.clone(),
                pull_request: None,
            }),
        )));
        assert!(
            app.pull_request_inspector.loaded,
            "mismatched active key cannot admit the response"
        );
        drop(app.sync_pull_request_inspector());
        assert_ne!(app.pull_request_inspector.generation, generation);
        assert!(!app.pull_request_inspector.loaded);
        drop(app.handle_pull_request_ui(Action::Loaded(
            old,
            generation,
            Err("old attachment".into()),
        )));
        assert!(app.pull_request_inspector.error.is_none());
    }

    fn branch() -> BranchTarget {
        BranchTarget {
            worktree: "/tmp/worktree".into(),
            branch: "feature".into(),
            head_oid: Some("aaa".into()),
            repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
        }
    }

    fn target() -> Target {
        branch().into()
    }

    fn discussion_comment(body: &str) -> crate::github::PullRequestComment {
        crate::github::PullRequestComment {
            id: None,
            author: None,
            body: body.into(),
            created_at: "2024-01-01T00:00:00Z".into(),
            url: "https://github.com/owner/repo/pull/1".parse().unwrap(),
            viewer_did_author: false,
            viewer_can_update: false,
            viewer_can_delete: false,
            comment_state: None,
        }
    }

    fn review_thread(path: &str, resolved: bool, body: &str) -> PullRequestReviewThread {
        PullRequestReviewThread {
            id: format!("thread-{path}"),
            path: path.into(),
            line: Some(1),
            start_line: None,
            side: crate::github::ReviewDiffSide::Right,
            start_side: None,
            outdated: false,
            resolved,
            collapsed: false,
            viewer_can_resolve: false,
            viewer_can_unresolve: false,
            comments: vec![discussion_comment(body)],
            truncated: false,
        }
    }

    #[test]
    fn a_local_commit_preserves_content_and_filter_while_refreshing() {
        let mut state = State::default();
        state.target = Some(target());
        state.loaded = true;
        state.filter = "keep this filter".into();
        state.description = markdown::parse("loaded description").collect();
        let pr = crate::pull_request_review::tests::pull_request(7);
        let inspection = Arc::new(PullRequestInspection {
            pull_request: pr.clone(),
            checks: Ok(Vec::new()),
            conversation: Ok(Default::default()),
        });
        state.pull_request = Some(Arc::new(pr));
        state.inspection = Some(Arc::clone(&inspection));
        let mut next = branch();
        next.head_oid = Some("bbb".into());
        drop(state.synchronize(Some(next.into()), None));
        assert!(state.loaded);
        assert!(state.loading);
        assert_eq!(state.filter, "keep this filter");
        assert!(!state.description.is_empty());
        assert!(state.pull_request.is_some());
        assert!(Arc::ptr_eq(state.inspection.as_ref().unwrap(), &inspection));
    }

    #[test]
    fn local_head_changes_observe_automatic_host_backoff_without_blank_content() {
        let host = "inspector-commit-backoff.invalid";
        let mut original = branch();
        original.repository = GitHubRepoKey::new(host, None, "owner", "repo").unwrap();
        let mut state = State::default();
        state.target = Some(original.clone().into());
        state.loaded = true;
        state.filter = "keep".into();
        state.description = markdown::parse("Loaded content").collect();
        crate::github::polling::failed(
            host,
            &crate::github::GitHubError::Inspection("offline".into()),
        );
        original.head_oid = Some("b".repeat(40));
        drop(state.synchronize(Some(original.into()), None));
        assert!(!state.loading);
        assert!(state.loaded);
        assert_eq!(state.filter, "keep");
        assert!(!state.description.is_empty());
        crate::github::polling::succeeded(host);
    }

    #[test]
    fn a_server_limited_inspector_explains_its_wait_without_discarding_content() {
        let host = "inspector-server-limit.invalid";
        let mut selected = branch();
        selected.repository = GitHubRepoKey::new(host, None, "owner", "repo").unwrap();
        crate::github::polling::failed(
            host,
            &crate::github::GitHubError::RateLimited {
                limit: crate::github::polling::RateLimit::secondary(
                    crate::github::polling::now() + 120,
                ),
            },
        );
        let mut state = State::default();
        drop(state.synchronize(Some(selected.into()), None));
        assert!(!state.loading);
        assert!(state.error.as_deref().unwrap().contains("Retrying at"));
        state.loaded = true;
        state.filter = "keep".into();
        state.description = markdown::parse("Loaded content").collect();
        let before = state.generation;
        drop(state.update(Action::Refresh));
        assert_eq!(state.generation, before);
        assert!(!state.loading);
        assert!(state.loaded);
        assert_eq!(state.filter, "keep");
        assert!(!state.description.is_empty());
        assert!(state.error.as_deref().unwrap().contains("Retrying at"));
    }

    #[test]
    fn inspector_head_verification_reads_one_oid_and_checks_symbolic_branch_without_status() {
        use crate::repository::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput};
        struct CountingGit {
            calls: std::sync::Mutex<Vec<CommandSpec>>,
            branch: Option<&'static str>,
            unborn: bool,
        }
        impl ProcessExecutor for CountingGit {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.calls.lock().unwrap().push(command.clone());
                let symbolic = command
                    .arguments
                    .iter()
                    .any(|argument| argument == "symbolic-ref");
                let (success, stdout) = if symbolic {
                    (
                        self.branch.is_some(),
                        self.branch.unwrap_or_default().to_owned(),
                    )
                } else {
                    (
                        !self.unborn,
                        if self.unborn {
                            String::new()
                        } else {
                            "a".repeat(40)
                        },
                    )
                };
                Ok(ProcessOutput {
                    success,
                    exit_code: Some(if success { 0 } else { 1 }),
                    signal: None,
                    stdout: format!("{stdout}\n").into_bytes(),
                    stderr: Vec::new(),
                })
            }
        }
        for (symbolic, unborn, expected) in [
            (Some("refs/heads/feature"), false, true),
            (Some("refs/heads/other"), false, false),
            (None, false, false),
            (Some("refs/heads/feature"), true, true),
            (Some("refs/heads/other"), true, false),
        ] {
            let executor = Arc::new(CountingGit {
                calls: Default::default(),
                branch: symbolic,
                unborn,
            });
            let git = RepositoryService::new("git", executor.clone());
            let mut target = branch();
            target.head_oid = (!unborn).then(|| "a".repeat(40));
            assert_eq!(
                verify_local_head_with(&git, &target, &CancellationToken::new()).is_ok(),
                expected
            );
            let calls = executor.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls
                    .iter()
                    .filter(|command| command
                        .arguments
                        .iter()
                        .any(|argument| argument == "rev-parse"))
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|command| command
                        .arguments
                        .iter()
                        .any(|argument| argument == "symbolic-ref"))
                    .count(),
                1
            );
            assert!(calls.iter().all(|command| {
                !command
                    .arguments
                    .iter()
                    .any(|argument| argument == "status")
            }));
        }
    }

    #[test]
    fn the_discussion_header_counts_only_what_the_filters_leave_visible() {
        let conversation = PullRequestConversation {
            threads: vec![
                review_thread("src/app.rs", false, "needs a guard"),
                review_thread("src/design.rs", true, "settled"),
            ],
            comments: vec![discussion_comment("looks good")],
            reviews: vec![crate::github::PullRequestReview {
                comment: discussion_comment("approving"),
                state: crate::github::RemoteValue("APPROVED".into()),
            }],
            truncated: false,
            threads_truncated: false,
        };
        assert_eq!(discussion_count(&conversation, "", false), 4);
        // Hiding resolved threads drops the settled one and nothing else.
        assert_eq!(discussion_count(&conversation, "", true), 3);
        // A filter that matches one path leaves exactly that thread.
        assert_eq!(discussion_count(&conversation, "design.rs", false), 1);
        assert_eq!(discussion_count(&conversation, "nothing here", false), 0);
    }

    #[test]
    fn a_pull_request_url_still_names_its_repository_without_a_target() {
        assert_eq!(
            repository_from_url(&"https://github.com/owner/repo/pull/12".parse().unwrap()),
            "owner/repo"
        );
        assert_eq!(
            repository_from_url(&"https://example.test/".parse().unwrap()),
            "https://example.test/"
        );
    }

    #[test]
    fn obsolete_request_cannot_overwrite_current_head_or_refresh() {
        let target = target();
        let mut state = State::default();
        state.target = Some(target.clone());
        state.generation = 2;
        state.loading = true;
        let _ = state.update(Action::Resolved(
            target.clone(),
            1,
            Ok(Resolution {
                target: target.clone(),
                pull_request: None,
            }),
        ));
        assert!(state.loading);
        assert!(!state.loaded);
        let _ = state.update(Action::Loaded(target.clone(), 1, Err("old request".into())));
        assert!(state.loading);
        assert!(state.error.is_none());
        let mut previous = branch();
        previous.head_oid = Some("previous".into());
        let _ = state.update(Action::Loaded(previous.into(), 2, Err("old head".into())));
        assert!(state.loading);
        assert!(state.error.is_none());
        let _ = state.update(Action::Loaded(target, 2, Err("current error".into())));
        assert!(!state.loading);
        assert_eq!(state.error.as_deref(), Some("current error"));
    }

    #[test]
    fn review_inspector_manual_refresh_preserves_content_during_server_pause() {
        let mut branch = branch();
        branch.repository =
            GitHubRepoKey::new("review-inspector-pause.invalid", None, "o", "r").unwrap();
        let target = Target::from(branch);
        crate::github::polling::failed(
            &target.repository().host,
            &crate::github::GitHubError::RateLimited {
                limit: crate::github::polling::RateLimit::secondary(
                    crate::github::polling::now() + 120,
                ),
            },
        );
        let mut state = State::default();
        state.target = Some(target.clone());
        state.generation = 9;
        state.loaded = true;
        drop(state.update(Action::Refresh));
        assert!(!state.loading);
        assert!(state.loaded);
        assert_eq!(state.generation, 9);
        assert_eq!(state.target.as_ref(), Some(&target));
        assert!(
            state
                .error
                .as_deref()
                .unwrap()
                .contains("GitHub rate limit reached")
        );
    }

    #[test]
    fn review_inspector_automatic_refresh_honors_backoff_and_manual_refresh_bypasses_it() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let mut branch = branch();
        branch.repository =
            GitHubRepoKey::new("review-inspector-backoff.invalid", None, "o", "r").unwrap();
        let target = Target::from(branch);
        crate::github::polling::failed(
            &target.repository().host,
            &crate::github::GitHubError::Inspection("offline".into()),
        );
        app.context_inspector_open = true;
        app.context_tab = ContextTab::PullRequest;
        app.pull_request_inspector = State::default();
        app.pull_request_inspector.target = Some(target);
        app.pull_request_inspector.generation = 5;
        drop(app.maybe_refresh_pull_request());
        assert!(!app.pull_request_inspector.loading);
        assert_eq!(app.pull_request_inspector.generation, 5);
        drop(app.pull_request_inspector.update(Action::Refresh));
        assert!(app.pull_request_inspector.loading);
        assert_eq!(app.pull_request_inspector.generation, 6);
    }

    #[test]
    fn lost_authority_discards_pending_and_loaded_content() {
        let target = target();
        let mut state = State::default();
        state.target = Some(target.clone());
        state.generation = 1;
        state.loading = true;
        let _ = state.synchronize(None, Some("Authentication required".into()));
        let _ = state.update(Action::Loaded(target, 1, Ok(None)));
        assert!(!state.loaded);
        assert!(!state.loading);
        assert!(state.target.is_none());
    }

    #[test]
    fn resolved_absence_finishes_without_launching_auxiliary_requests() {
        let target = target();
        let mut state = State::default();
        state.target = Some(target.clone());
        state.generation = 1;
        state.loading = true;
        let _ = state.update(Action::Resolved(
            target.clone(),
            1,
            Ok(Resolution {
                target,
                pull_request: None,
            }),
        ));
        assert!(state.loaded);
        assert!(!state.loading);
        assert!(state.inspection.is_none());
        assert!(state.preview.is_none());
    }

    /// A window with a chat card, focused, and a terminal card beside it.
    fn conversation_app() -> (Cagents, TerminalId, CardId) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let conversation = TerminalId::new();
        let chat = CardState {
            id: CardId::new(),
            generation: 0,
            title: "Fix the parser".into(),
            width_fraction: 0.5,
            existence: Existence::Open,
            kind: CardKind::Chat(crate::chat::Session {
                native_continuity: Default::default(),
                owned_codex_listener: None,
                id: conversation,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: crate::model::LaunchRecipe {
                        unset_environment: Default::default(),
                        program: "/bin/sh".into(),
                        arguments: Vec::new(),
                        working_directory: PathBuf::from("/tmp"),
                        environment: Default::default(),
                    },
                    session_id: None,
                },
                proof: None,
                terminal: None,
            }),
        };
        let terminal = super::super::tests::terminal_card_state();
        let terminal_id = terminal.id;
        let chat_id = chat.id;
        app.durable = Arc::new(WorkspaceState {
            cards: vec![chat, terminal],
            focused: Some(chat_id),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.index_cards();
        app.context_inspector_open = true;
        app.context_tab = ContextTab::PullRequest;
        (app, conversation, terminal_id)
    }

    fn listed(conversation: TerminalId, revision: u64, numbers: &[u64], first: bool) -> Action {
        let repository = GitHubRepoKey::new("chosen-inspector.invalid", None, "o", "r").unwrap();
        Action::Conversation(conversation::Watch::Listed {
            conversation,
            revision,
            list: Arc::new(crate::work::ConversationPullRequests {
                generated_at_unix_ms: 0,
                repository: Some(repository.clone()),
                pull_requests: numbers
                    .iter()
                    .map(|number| crate::work::ConversationPullRequest {
                        repository: repository.clone(),
                        number: *number,
                        relation: crate::work::Relation::Created,
                        by_subagent: false,
                        current_branch: false,
                        pull_request: None,
                        observed_at_unix_ms: None,
                    })
                    .collect(),
                transcript: crate::work::TranscriptCoverage::Complete,
                forge_error: None,
            }),
            first,
        })
    }

    #[test]
    fn a_chosen_or_default_pull_request_is_kept_while_its_conversation_lists_it() {
        let (mut app, conversation, terminal) = conversation_app();
        drop(app.sync_pull_request_inspector());
        assert_eq!(
            app.pull_request_inspector.conversation.conversation,
            Some(conversation)
        );
        assert!(!app.conversation_lists_pull_requests());
        drop(app.handle_pull_request_ui(listed(conversation, 1, &[4, 5], true)));
        assert!(app.conversation_lists_pull_requests());
        let key = PullRequestKey::new(
            GitHubRepoKey::new("chosen-inspector.invalid", None, "o", "r").unwrap(),
            4,
        )
        .unwrap();
        drop(app.handle_pull_request_ui(Action::Choose(Some(key.clone()))));
        assert_eq!(
            app.pull_request_inspector.target,
            Some(Target::PullRequest(key.clone()))
        );
        assert!(app.pull_request_binding_is_current());
        assert!(app.pull_request_inspector.loading);

        // Focusing another card in the workspace keeps the conversation and
        // the choice.
        drop(app.workspace.focus(terminal));
        drop(app.sync_pull_request_inspector());
        assert_eq!(
            app.pull_request_inspector.conversation.conversation,
            Some(conversation)
        );
        assert_eq!(
            app.pull_request_inspector.target,
            Some(Target::PullRequest(key.clone()))
        );

        // A list that no longer names it drops the choice. This window has
        // no branch, so the pull request the conversation worked on most is
        // shown without being chosen.
        drop(app.handle_pull_request_ui(listed(conversation, 2, &[5], false)));
        assert!(app.pull_request_inspector.chosen.is_none());
        let five = PullRequestKey::new(key.repository.clone(), 5).unwrap();
        assert_eq!(
            app.pull_request_inspector.target,
            Some(Target::PullRequest(five.clone()))
        );
        assert!(app.pull_request_binding_is_current());
        assert!(app.conversation_lists_pull_requests());

        // Choosing the chosen row again returns to that default.
        drop(app.handle_pull_request_ui(Action::Choose(Some(five.clone()))));
        assert_eq!(app.pull_request_inspector.chosen, Some(five.clone()));
        drop(app.handle_pull_request_ui(Action::Choose(Some(five.clone()))));
        assert!(app.pull_request_inspector.chosen.is_none());
        assert_eq!(
            app.pull_request_inspector.target,
            Some(Target::PullRequest(five.clone()))
        );

        // A choice belongs to the worktree it was made in: once another is
        // active, the detail follows that worktree again.
        drop(app.handle_pull_request_ui(Action::Choose(Some(five.clone()))));
        app.pull_request_inspector.chosen_in = Some(crate::sidebar::ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: PathBuf::from("/elsewhere"),
        });
        drop(app.sync_pull_request_inspector());
        assert!(app.pull_request_inspector.chosen.is_none());
        assert!(app.pull_request_inspector.chosen_in.is_none());

        // An older revision does not bring it back.
        drop(app.handle_pull_request_ui(listed(conversation, 1, &[4, 5], false)));
        assert_eq!(app.pull_request_inspector.conversation.rows().len(), 1);

        // Working on another pull request just as closely keeps the one
        // already shown, until the worktree changes.
        drop(app.handle_pull_request_ui(listed(conversation, 3, &[6, 5], false)));
        assert_eq!(
            app.pull_request_inspector.target,
            Some(Target::PullRequest(five.clone()))
        );
        app.pull_request_inspector.default_in = Some(crate::sidebar::ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: PathBuf::from("/elsewhere"),
        });
        drop(app.sync_pull_request_inspector());
        let six = PullRequestKey::new(key.repository.clone(), 6).unwrap();
        assert_eq!(
            app.pull_request_inspector.target,
            Some(Target::PullRequest(six))
        );
    }
}
