use super::*;
use crate::github::{PullRequestComment, PullRequestInspection};
use crate::repository::HeadState;

type ActionTask = iced::Task<Action>;

/// Includes local HEAD and remote authority, not just the branch's display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub worktree: PathBuf,
    pub branch: String,
    pub head_oid: Option<String>,
    pub repository: GitHubRepoKey,
}

#[derive(Debug, Clone)]
pub enum Action {
    Refresh,
    Loaded(
        Target,
        u64,
        Result<Option<Arc<PullRequestInspection>>, String>,
    ),
    Filter(String),
    ToggleResolved,
    OpenUrl(Url),
    UrlFinished(u64, Result<(), String>),
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(self, Self::Loaded(..) | Self::UrlFinished(..))
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
    error: Option<String>,
    link_error: Option<String>,
    filter: String,
    hide_resolved: bool,
}

impl State {
    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

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
        self.error = None;
        self.link_error = None;
        self.loaded = false;
        self.loading = false;
        self.filter.clear();
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
                        .inspect_branch(
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
                Action::Loaded(target, generation, result)
            },
            |action| action,
        )
    }

    pub fn update(&mut self, action: Action) -> ActionTask {
        match action {
            Action::Refresh => return self.refresh(),
            Action::Loaded(target, generation, result) => {
                if !self.accepts(&target, generation) {
                    return ActionTask::none();
                }
                self.loading = false;
                self.loaded = true;
                match result {
                    Ok(inspection) => self.inspection = inspection,
                    Err(error) => self.error = Some(error),
                }
            }
            Action::Filter(filter) => self.filter = filter,
            Action::ToggleResolved => self.hide_resolved = !self.hide_resolved,
            Action::OpenUrl(url) => {
                let generation = self.generation;
                return ActionTask::perform(
                    async move {
                        let result = if matches!(url.scheme(), "https" | "http")
                            && url.username().is_empty()
                            && url.password().is_none()
                        {
                            open_system_url(url).await
                        } else {
                            Err(
                                "Only HTTP and HTTPS links without credentials can be opened."
                                    .into(),
                            )
                        };
                        Action::UrlFinished(generation, result)
                    },
                    |action| action,
                );
            }
            Action::UrlFinished(generation, result) if generation == self.generation => {
                self.link_error = result.err();
            }
            Action::UrlFinished(..) => {}
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
        .status(&target.worktree, cancellation)
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
    widget::button::text(label)
        .on_press(Action::OpenUrl(url.clone()))
        .into()
}

fn comment<'a>(comment: &'a PullRequestComment, reply: bool) -> Element<'a, Action> {
    let author = comment
        .author
        .as_ref()
        .map_or("Deleted account", |author| author.login.as_str());
    widget::column::with_children(vec![
        widget::text::caption(format!(
            "{}{} · {}",
            if reply { "Reply · " } else { "" },
            author,
            comment.created_at
        ))
        .into(),
        widget::text::body(&comment.body).width(Length::Fill).into(),
        link("Open comment", &comment.url),
    ])
    .spacing(4)
    .width(Length::Fill)
    .into()
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
    let mut children: Vec<Element<'_, Action>> = vec![
        widget::row::with_children(vec![
            widget::text::title4("Pull request")
                .width(Length::Fill)
                .into(),
            widget::button::text("Refresh")
                .on_press_maybe(
                    (state.target.is_some() && !state.loading).then_some(Action::Refresh),
                )
                .into(),
        ])
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
    ];
    if let Some(target) = &state.target {
        children.push(
            widget::text::caption(format!(
                "{} · {}",
                target.repository.gh_selector(),
                target.branch
            ))
            .into(),
        );
    } else {
        children.push(
            widget::text::body(
                state
                    .unavailable
                    .as_deref()
                    .unwrap_or("Select a worktree to inspect its pull request."),
            )
            .into(),
        );
    }
    if state.loading {
        children.push(widget::text::body("Loading pull request, checks, and discussion…").into());
    }
    if let Some(error) = &state.error {
        children.push(widget::text::body(format!("Unable to load pull request: {error}")).into());
    }
    if let Some(error) = &state.link_error {
        children.push(widget::text::body(format!("Unable to open link: {error}")).into());
    }
    if state.loaded && !state.loading && state.error.is_none() && state.inspection.is_none() {
        children.push(widget::text::body("No pull request matches this worktree branch.").into());
    }
    if let Some(inspection) = &state.inspection {
        let pr = &inspection.pull_request;
        children.push(widget::text::title4(format!("#{} {}", pr.number, pr.title)).into());
        children.push(
            widget::text::caption(format!(
                "{}{} · {} → {}",
                pr.state.0,
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
            children.push(widget::text::body("Local HEAD differs from the pull request head. Checks below apply to the remote pull request commit.").into());
        }
        children.push(
            widget::text::caption(format!(
                "Review decision: {}",
                pr.review_decision
                    .as_ref()
                    .map(|decision| decision.0.as_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("No decision reported")
            ))
            .into(),
        );
        children.push(
            widget::text::body(
                pr.body
                    .as_deref()
                    .filter(|body| !body.is_empty())
                    .unwrap_or("No description provided."),
            )
            .width(Length::Fill)
            .into(),
        );
        children.push(widget::text::title4("Checks").into());
        match &inspection.checks {
            Ok(checks) if checks.is_empty() => {
                children.push(widget::text::body("No checks reported.").into())
            }
            Ok(checks) => {
                for check in checks {
                    let mut check_rows = vec![
                        widget::text::body(format!("{} · {}", check.name, check.state.0)).into(),
                        widget::text::caption(format!(
                            "{}{}{}",
                            check.bucket.0,
                            if check.workflow.is_empty() {
                                ""
                            } else {
                                " · "
                            },
                            check.workflow
                        ))
                        .into(),
                    ];
                    if !check.description.is_empty() {
                        check_rows.push(
                            widget::text::body(&check.description)
                                .width(Length::Fill)
                                .into(),
                        );
                    }
                    check_rows.push(link("Open check details / output", &check.link));
                    children.push(widget::column::with_children(check_rows).spacing(4).into());
                }
            }
            Err(error) => {
                children.push(widget::text::body(format!("Checks unavailable: {error}")).into())
            }
        }
        children.push(widget::text::title4("Discussion").into());
        match &inspection.conversation {
            Err(error) => {
                children.push(widget::text::body(format!("Discussion unavailable: {error}")).into())
            }
            Ok(conversation) => {
                children.push(
                    widget::text_input("Filter by author, text, or file", &state.filter)
                        .on_input(Action::Filter)
                        .into(),
                );
                children.push(
                    widget::button::text(if state.hide_resolved {
                        "Show resolved threads"
                    } else {
                        "Hide resolved threads"
                    })
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
                    let location = thread.line.map_or_else(
                        || thread.path.clone(),
                        |line| format!("{}:{line}", thread.path),
                    );
                    children.push(
                        widget::text::body(format!(
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
                        children.push(widget::text::caption("Showing the first 100 comments in this thread. Open its comment link for the full discussion.").into());
                    }
                }
                children.push(widget::text::title4("Reviews").into());
                for review in &conversation.reviews {
                    if comment_matches(&review.comment, &filter) {
                        shown += 1;
                        children.push(widget::text::caption(&review.state.0).into());
                        children.push(comment(&review.comment, false));
                    }
                }
                children.push(widget::text::title4("Issue comments").into());
                for item in &conversation.comments {
                    if comment_matches(item, &filter) {
                        shown += 1;
                        children.push(comment(item, false));
                    }
                }
                if shown == 0 {
                    children.push(
                        widget::text::body(if filter.is_empty() && !state.hide_resolved {
                            "No discussion comments reported."
                        } else {
                            "No comments match these filters."
                        })
                        .into(),
                    );
                }
                if conversation.truncated {
                    children.push(widget::text::caption("Showing up to 200 review threads, 200 reviews, and 200 issue comments. Open the pull request for the full discussion.").into());
                }
            }
        }
    }
    widget::scrollable(
        widget::column::with_children(children)
            .spacing(12)
            .padding(12)
            .width(Length::Fill),
    )
    .height(Length::Fill)
    .width(Length::Fill)
    .into()
}

impl Cagents {
    pub(super) fn handle_pull_request_ui(&mut self, action: Action) -> Task<Message> {
        let accepted = matches!(&action, Action::Loaded(target, generation, Ok(_))
            if self.pull_request_inspector.accepts(target, *generation));
        let task = self.pull_request_inspector.update(action);
        if accepted
            && let Some(target) = self.pull_request_inspector.target.as_ref()
            && let Some(inspection) = self.pull_request_inspector.inspection.as_ref()
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
                    worktree.pull_request_state =
                        sidebar::WorktreePullRequestState::from_github(&inspection.pull_request);
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
        self.pull_request_inspector
            .synchronize(Some(target), None)
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
}
