use super::*;
use crate::github::{
    PullRequest, PullRequestComment, PullRequestConversation, PullRequestInspection,
    PullRequestReviewThread,
};
use crate::repository::HeadState;
use cosmic::widget::markdown;

type ActionTask = iced::Task<Action>;

/// Includes local HEAD and remote authority, not just the branch's display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub worktree: PathBuf,
    pub branch: String,
    pub head_oid: Option<String>,
    pub repository: GitHubRepoKey,
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
    Resolved(Target, u64, Result<Option<Arc<PullRequest>>, String>),
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
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(self, Self::Resolved(..) | Self::Loaded(..))
    }
}

#[derive(Debug, Default)]
pub struct State {
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
    checks_collapsed: bool,
    discussion_collapsed: bool,
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
        if self.target == target {
            return ActionTask::none();
        }
        self.cancellation.cancel();
        self.generation = self.generation.wrapping_add(1);
        self.target = target;
        self.inspection = None;
        self.pull_request = None;
        self.preview = None;
        self.error = None;
        self.link_error = None;
        self.loaded = false;
        self.loading = false;
        self.filter.clear();
        self.description.clear();
        self.refresh()
    }

    fn refresh(&mut self) -> ActionTask {
        let Some(target) = self.target.clone() else {
            return ActionTask::none();
        };
        self.cancellation.cancel();
        self.cancellation = CancellationToken::new();
        let cancellation = self.cancellation.clone();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.link_error = None;
        // Clearing old content avoids presenting a prior head's checks as fresh.
        self.inspection = None;
        ActionTask::perform(
            async move {
                let work = target.clone();
                let result = tokio::task::spawn_blocking(move || {
                    verify_local_head(&work, &cancellation)?;
                    let result = GitHubService::system("gh")
                        .resolve_branch(
                            &work.repository,
                            &work.branch,
                            work.head_oid.as_deref(),
                            &cancellation,
                        )
                        .map_err(|error| error.to_string())?;
                    verify_local_head(&work, &cancellation)?;
                    Ok(result.map(Arc::new))
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
                        GitHubService::<crate::repository::SystemProcessExecutor>::invalidate_checks(&target.repository, pr.number);
                    } else if let Some(pr) = &self.preview {
                        GitHubService::<crate::repository::SystemProcessExecutor>::invalidate_checks(&target.repository, pr.number);
                    }
                }
                return self.refresh();
            }
            Action::Resolved(target, generation, result) => {
                if !self.accepts(&target, generation) {
                    return ActionTask::none();
                }
                let pull_request = match result {
                    Ok(Some(pr)) => pr,
                    Ok(None) => {
                        self.preview = None;
                        self.pull_request = None;
                        self.loading = false;
                        self.loaded = true;
                        return ActionTask::none();
                    }
                    Err(error) => {
                        self.loading = false;
                        self.error = Some(error);
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
                self.pull_request = Some(Arc::clone(&pull_request));
                self.preview = None;
                let cancellation = self.cancellation.clone();
                return ActionTask::perform(
                    async move {
                        let work = target.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            verify_local_head(&work, &cancellation)?;
                            let inspection = GitHubService::system("gh")
                                .inspect_pull_request(
                                    &work.repository,
                                    (*pull_request).clone(),
                                    &cancellation,
                                )
                                .map_err(|error| error.to_string())?;
                            verify_local_head(&work, &cancellation)?;
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
            // Link routing belongs to the application, which owns browser preferences.
            Action::OpenUrl(_) | Action::OpenMarkdownLink(_) => {}
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

fn verify_local_head(target: &Target, cancellation: &CancellationToken) -> Result<(), String> {
    let status = RepositoryService::system("git")
        .status_with_untracked(&target.worktree, false, cancellation)
        .map_err(|error| error.to_string())?;
    let matches = match status.head {
        HeadState::Branch { name, oid } => {
            name == target.branch && target.head_oid.as_deref() == Some(oid.as_str())
        }
        HeadState::Unborn { branch } => {
            branch.as_deref() == Some(target.branch.as_str()) && target.head_oid.is_none()
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err("The worktree branch or HEAD changed. Refresh the repository before inspecting its pull request.".into())
    }
}

fn link<'a>(label: &'a str, url: &Url) -> Element<'a, Action> {
    widget::button::custom(design::caption(label))
        .class(crate::ui_controls::control_style(false, None))
        .padding([4, 8])
        .on_press(Action::OpenUrl(url.clone()))
        .into()
}

fn comment<'a>(comment: &'a PullRequestComment, reply: bool) -> Element<'a, Action> {
    let author = comment
        .author
        .as_ref()
        .map_or("Deleted account", |author| author.login.as_str());
    widget::container(
        widget::column::with_children(vec![
            widget::row::with_children(vec![
                design::heading(author).width(Length::Fill).into(),
                design::caption(comment.created_at.get(..10).unwrap_or(&comment.created_at)).into(),
                crate::ui_controls::compact_icon_control(
                    crate::ui_controls::symbolic_icon("external-link-symbolic"),
                    "Open comment on GitHub",
                    Some(Action::OpenUrl(comment.url.clone())),
                )
                .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(4)
            .into(),
            design::body(&comment.body).width(Length::Fill).into(),
        ])
        .spacing(4)
        .width(Length::Fill),
    )
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
    design::badge(label, design::Tone::Themed(tone))
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
    .spacing(design::GRID_UNIT * 2.0)
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
            .spacing(design::GRID_UNIT)
            .width(Length::Fill),
    )
    .padding(design::GRID_UNIT * 2.0)
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
            widget::icon::from_name(if open {
                "go-down-symbolic"
            } else {
                "go-next-symbolic"
            })
            .size(design::ROW_ICON_SIZE)
            .into(),
            design::heading(label.into()).width(Length::Fill).into(),
        ])
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .class(crate::ui_controls::control_style(false, None))
    .height(design::DENSE_ROW_HEIGHT)
    .padding([0, 4])
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
        widget::icon(visual.as_ref().map_or_else(
            || crate::ui_controls::symbolic_icon("pull-request-symbolic"),
            |(icon, _)| icon.clone(),
        ))
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
                    .map_or_else(|| "Pull request".to_owned(), |pr| pr.title.clone())
            },
            |pr| pr.title.clone(),
        ))
        .width(Length::Fill)
        .into(),
        crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("view-refresh-symbolic"),
            "Refresh pull request",
            false,
            state.target.is_some() && !state.loading,
            Some(Action::Refresh),
        )
        .into(),
    ])
    .spacing(4)
    .align_y(Alignment::Center)
    .height(design::HEADER_CONTENT_HEIGHT);
    let mut children: Vec<Element<'_, Action>> = Vec::new();
    // While a pull request is loaded its metadata grid carries the repository
    // and branch; this line only orients the reader before that arrives.
    if let Some(target) = &state.target {
        if state.pull_request.is_none() {
            children.push(
                crate::design::caption(format!(
                    "{} · {}",
                    target.repository.gh_selector(),
                    target.branch
                ))
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
        children.push(crate::design::caption("Loading pull request…").into());
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
    if state.loaded && !state.loading && state.error.is_none() && state.inspection.is_none() {
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
            .spacing(design::GRID_UNIT)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into(),
            design::heading(&pr.title)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .into(),
            widget::row::with_children(vec![
                widget::icon(sidebar::worktree_icon())
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
                design::caption(format!("{} → {}", pr.head_ref_name, pr.base_ref_name))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(design::GRID_UNIT)
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
            .and_then(|target| target.head_oid.as_deref())
            .filter(|oid| !oid.is_empty());
        if let Some(local_head) = local_head
            && !pr.head_ref_oid.is_empty()
            && local_head != pr.head_ref_oid
        {
            identity.push(
                widget::row::with_children(vec![
                    design::badge("Out of sync", design::Tone::Themed(design::warning_color)),
                    design::caption(
                        "Local HEAD differs from the pull request head; checks describe the remote commit.",
                    )
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
                ])
                .spacing(design::GRID_UNIT)
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
                |target| target.repository.gh_selector(),
            ))
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .width(Length::Fill)
            .into(),
        ));
        // The identity block names the remote head. Where the checked-out
        // branch is not that branch, saying so is the whole point of the row.
        if let Some(target) = &state.target
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
                Some("APPROVED") => {
                    design::badge("Approved", design::Tone::Themed(design::success_color))
                }
                Some("CHANGES_REQUESTED") => design::badge(
                    "Changes requested",
                    design::Tone::Themed(design::destructive_color),
                ),
                Some(other) => design::badge(
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
                design::badge(summary, design::Tone::Themed(tone)),
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
            let settings = markdown::Settings {
                h1_size: design::TEXT_HEADING.into(),
                h2_size: design::TEXT_HEADING.into(),
                h3_size: design::TEXT_HEADING.into(),
                h4_size: design::TEXT_HEADING.into(),
                spacing: 8.into(),
                code_size: design::TEXT_CAPTION.into(),
                ..markdown::Settings::with_text_size(
                    design::TEXT_BODY,
                    if theme.theme_type.is_dark() {
                        iced::Theme::Dark
                    } else {
                        iced::Theme::Light
                    },
                )
            };
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
                                    ("dialog-error-symbolic", design::destructive_color)
                                }
                                bucket if sidebar::is_check_pending(bucket) => {
                                    ("content-loading-symbolic", design::warning_color)
                                }
                                bucket if sidebar::is_check_success(bucket) => {
                                    ("emblem-ok-symbolic", design::success_color)
                                }
                                _ => ("dialog-information-symbolic", design::muted_color),
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
                        .spacing(0)
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
                                        widget::icon::from_name(symbol).size(design::ROW_ICON_SIZE),
                                    )
                                    .width(Length::Fixed(design::ROW_ICON_SIZE.into()))
                                    .align_x(Alignment::Center)
                                    .into(),
                                    content.into(),
                                    widget::container(
                                        widget::icon::from_name("external-link-symbolic")
                                            .size(design::COMPACT_ICON_SIZE),
                                    )
                                    .width(Length::Fixed(design::COMPACT_ICON_SIZE.into()))
                                    .align_x(Alignment::Center)
                                    .into(),
                                ])
                                .spacing(design::GRID_UNIT * 2.0)
                                .align_y(Alignment::Center),
                            )
                            .class(crate::ui_controls::control_style(
                                false,
                                Some(design::Tone::Themed(tone)),
                            ))
                            .padding([4, 8])
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
                            widget::text_input("Filter author, text, or file", &state.filter)
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
                        .spacing(design::GRID_UNIT)
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
                                    head.push(design::badge(
                                        "Outdated",
                                        design::Tone::Themed(design::muted_color),
                                    ));
                                }
                                head.push(if thread.resolved {
                                    design::badge(
                                        "Resolved",
                                        design::Tone::Themed(design::success_color),
                                    )
                                } else {
                                    design::badge(
                                        "Unresolved",
                                        design::Tone::Themed(design::warning_color),
                                    )
                                });
                                head
                            })
                            .spacing(design::GRID_UNIT)
                            .align_y(Alignment::Center)
                            .width(Length::Fill)
                            .into(),
                        ];
                        // Preserve the whole matching thread so replies retain context.
                        for (index, item) in thread.comments.iter().enumerate() {
                            thread_rows.push(comment(item, index != 0));
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
                                design::badge(
                                    readable_state(&review.state.0),
                                    design::Tone::Themed(tone),
                                ),
                                comment(&review.comment, false),
                            ]));
                        }
                    }
                    if !conversation.comments.is_empty() {
                        children.push(design::heading("Comments").into());
                    }
                    for item in &conversation.comments {
                        if comment_matches(item, &filter) {
                            shown += 1;
                            children.push(block(vec![comment(item, false)]));
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
            .spacing(8)
            .padding(design::with_vertical_rail(cosmic::iced::Padding::from([
                4, 8,
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
            .padding([0, 8])
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
        if accepted && let Some(target) = self.pull_request_inspector.target.as_ref() {
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
                        project
                            .pull_requests
                            .retain(|pr| pr.head_ref != target.branch);
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

    pub(super) fn sync_pull_request_inspector(&mut self) -> Task<Message> {
        if !self.context_inspector_open || self.context_tab != ContextTab::PullRequest {
            return self
                .pull_request_inspector
                .synchronize(None, None)
                .map(Message::PullRequestUi)
                .map(cosmic::Action::App);
        }
        let source = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| self.sidebar_state.active_worktree() == Some(&source.key));
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
                Some(_) => "This worktree has no authenticated GitHub remote.",
            };
            if self.pull_request_inspector.target.is_none()
                && self.pull_request_inspector.unavailable.as_deref() == Some(reason)
            {
                return Task::none();
            }
            return self
                .pull_request_inspector
                .synchronize(None, Some(reason.to_owned()))
                .map(Message::PullRequestUi)
                .map(cosmic::Action::App);
        };
        if self
            .pull_request_inspector
            .target
            .as_ref()
            .is_some_and(|target| {
                target.worktree == *worktree
                    && target.branch == branch
                    && target.head_oid.as_deref() == head_oid
                    && target.repository == *repository
            })
        {
            return Task::none();
        }
        let target = Target {
            worktree: worktree.clone(),
            branch: branch.to_owned(),
            head_oid: head_oid.map(str::to_owned),
            repository: repository.clone(),
        };
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
                let mut candidates = project
                    .pull_requests
                    .iter()
                    .filter(|pr| pr.head_ref == branch);
                let candidate = candidates.next()?;
                candidates.next().is_none().then(|| candidate.clone())
            });
        let task = self.pull_request_inspector.synchronize(Some(target), None);
        self.pull_request_inspector.preview = preview;
        task.map(Message::PullRequestUi).map(cosmic::Action::App)
    }

    /// The application supplies the visible/focused 30-second cadence.
    pub(super) fn maybe_refresh_pull_request(&mut self) -> Task<Message> {
        if !self.context_inspector_open
            || self.context_tab != ContextTab::PullRequest
            || self.pull_request_inspector.loading
            || self.pull_request_inspector.target.is_none()
        {
            return Task::none();
        }
        self.pull_request_inspector
            .refresh()
            .map(Message::PullRequestUi)
            .map(cosmic::Action::App)
    }

    pub(super) fn pull_request_panel(&self, width: f32) -> Element<'_, Message> {
        widget::container(view(&self.pull_request_inspector).map(Message::PullRequestUi))
            .width(Length::Fixed(width))
            .height(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            worktree: "/tmp/worktree".into(),
            branch: "feature".into(),
            head_oid: Some("aaa".into()),
            repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
        }
    }

    fn discussion_comment(body: &str) -> crate::github::PullRequestComment {
        crate::github::PullRequestComment {
            author: None,
            body: body.into(),
            created_at: "2024-01-01T00:00:00Z".into(),
            url: "https://github.com/owner/repo/pull/1".parse().unwrap(),
        }
    }

    fn review_thread(path: &str, resolved: bool, body: &str) -> PullRequestReviewThread {
        PullRequestReviewThread {
            path: path.into(),
            line: Some(1),
            outdated: false,
            resolved,
            comments: vec![discussion_comment(body)],
            truncated: false,
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
        let _ = state.update(Action::Resolved(target.clone(), 1, Ok(None)));
        assert!(state.loading);
        assert!(!state.loaded);
        let _ = state.update(Action::Loaded(target.clone(), 1, Err("old request".into())));
        assert!(state.loading);
        assert!(state.error.is_none());
        let mut previous = target.clone();
        previous.head_oid = Some("previous".into());
        let _ = state.update(Action::Loaded(previous, 2, Err("old head".into())));
        assert!(state.loading);
        assert!(state.error.is_none());
        let _ = state.update(Action::Loaded(target, 2, Err("current error".into())));
        assert!(!state.loading);
        assert_eq!(state.error.as_deref(), Some("current error"));
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
        let _ = state.update(Action::Resolved(target, 1, Ok(None)));
        assert!(state.loaded);
        assert!(!state.loading);
        assert!(state.inspection.is_none());
        assert!(state.preview.is_none());
    }
}
