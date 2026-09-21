use super::*;
use crate::github::{PullRequest, PullRequestComment, PullRequestInspection};
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
    if let Some(target) = &state.target {
        children.push(
            crate::design::caption(format!(
                "{} · {}",
                target.repository.gh_selector(),
                target.branch
            ))
            .into(),
        );
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
        children.push(design::body("No pull request matches this worktree branch.").into());
    }
    if let Some(pr) = state.pull_request.as_deref() {
        children.push(design::heading(&pr.title).width(Length::Fill).into());
        children.push(
            crate::design::caption(format!(
                "{}{} · {} → {}",
                readable_state(&pr.state.0),
                if pr.is_draft { " · Draft" } else { "" },
                pr.head_ref_name,
                pr.base_ref_name
            ))
            .into(),
        );
        children.push(link("Open pull request on GitHub", &pr.url));
        if state
            .target
            .as_ref()
            .and_then(|target| target.head_oid.as_deref())
            != Some(pr.head_ref_oid.as_str())
        {
            children.push(design::body("Local HEAD differs from the pull request head. Checks below apply to the remote pull request commit.").into());
        }
        children.push(
            crate::design::caption(format!(
                "Review decision: {}",
                readable_state(
                    pr.review_decision
                        .as_ref()
                        .map(|decision| decision.0.as_str())
                        .filter(|value| !value.is_empty())
                        .unwrap_or("No decision reported")
                )
            ))
            .into(),
        );
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
                    children.push(design::body("No checks reported.").into())
                }
                Ok(checks) => {
                    for check in checks {
                        let (symbol, tone): (_, fn(&cosmic::Theme) -> cosmic::iced::Color) =
                            match check.bucket.0.as_str() {
                                "pass" => ("emblem-ok-symbolic", design::success_color),
                                "fail" => ("dialog-error-symbolic", design::destructive_color),
                                "pending" => ("content-loading-symbolic", design::warning_color),
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
                                    widget::icon::from_name(symbol)
                                        .size(design::ROW_ICON_SIZE)
                                        .into(),
                                    content.into(),
                                    widget::icon::from_name("external-link-symbolic")
                                        .size(design::COMPACT_ICON_SIZE)
                                        .into(),
                                ])
                                .spacing(8)
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
                    children.push(design::body(format!("Checks unavailable: {error}")).into())
                }
            }
        }
        children.push(section_header(
            "Discussion",
            Section::Discussion,
            !state.discussion_collapsed,
        ));
        if !state.discussion_collapsed {
            match &inspection.conversation {
                Err(error) => {
                    children.push(design::body(format!("Discussion unavailable: {error}")).into())
                }
                Ok(conversation) => {
                    children.push(
                        widget::text_input("Filter by author, text, or file", &state.filter)
                            .on_input(Action::Filter)
                            .into(),
                    );
                    children.push(
                        widget::button::custom(design::caption(if state.hide_resolved {
                            "Show resolved threads"
                        } else {
                            "Hide resolved threads"
                        }))
                        .class(crate::ui_controls::control_style(state.hide_resolved, None))
                        .height(design::DENSE_ROW_HEIGHT)
                        .on_press(Action::ToggleResolved)
                        .into(),
                    );
                    let filter = state.filter.to_lowercase();
                    let mut shown = 0;
                    for thread in &conversation.threads {
                        if state.hide_resolved && thread.resolved {
                            continue;
                        }
                        if !filter.is_empty()
                            && !thread.path.to_lowercase().contains(&filter)
                            && !thread
                                .comments
                                .iter()
                                .any(|comment| comment_matches(comment, &filter))
                        {
                            continue;
                        }
                        shown += 1;
                        children.push(crate::design::horizontal_rule().into());
                        let location = thread.line.map_or_else(
                            || thread.path.clone(),
                            |line| format!("{}:{line}", thread.path),
                        );
                        children.push(
                            design::body(format!(
                                "{location} · {}{}",
                                if thread.resolved {
                                    "Resolved"
                                } else {
                                    "Unresolved"
                                },
                                if thread.outdated { " · Outdated" } else { "" }
                            ))
                            .into(),
                        );
                        // Preserve the whole matching thread so replies retain context.
                        for (index, item) in thread.comments.iter().enumerate() {
                            children.push(comment(item, index != 0));
                        }
                        if thread.truncated {
                            children.push(crate::design::caption("Showing the first 100 comments in this thread. Open its comment link for the full discussion.").into());
                        }
                    }
                    if !conversation.reviews.is_empty() {
                        children.push(design::heading("Reviews").into());
                    }
                    for review in &conversation.reviews {
                        if comment_matches(&review.comment, &filter) {
                            shown += 1;
                            children.push(crate::design::caption(&review.state.0).into());
                            children.push(comment(&review.comment, false));
                        }
                    }
                    if !conversation.comments.is_empty() {
                        children.push(design::heading("Comments").into());
                    }
                    for item in &conversation.comments {
                        if comment_matches(item, &filter) {
                            shown += 1;
                            children.push(comment(item, false));
                        }
                    }
                    if shown == 0 {
                        children.push(
                            design::body(if filter.is_empty() && !state.hide_resolved {
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
