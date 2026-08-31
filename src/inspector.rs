//! Contextual right-inspector presentation.
//!
//! The module consumes durable card state and repository snapshots that are
//! already in memory. It deliberately performs no repository, terminal, or
//! browser I/O while constructing the view.

use std::{borrow::Cow, path::PathBuf};

use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Alignment, Event, Length, Rectangle, Size,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget::Tree,
        },
    },
    theme, widget,
};
use url::Url;

use crate::{
    design::{self, InspectorLayout},
    location::SanitizedLocation,
    model::{
        BrowserRestoreFidelity, CardId, CardKind, CardState, ProjectId, Residency,
        TerminalContinuity,
    },
    sidebar::{SidebarProject, SidebarWorktree},
};

pub const RESIZE_HANDLE_NAME: &str = "Resize context inspector";

pub fn close_focus_id() -> widget::Id {
    widget::Id::new("context-inspector.close")
}

/// A modal layer that keeps its background painted while exposing and
/// operating only the foreground subtree. Unlike an ordinary stack or
/// popover, this makes the overlay—not the obscured workbench—the root of the
/// accessibility and focus-operation tree.
pub struct ModalOverlay<'a, Message> {
    background: Element<'a, Message>,
    foreground: Element<'a, Message>,
}

impl<'a, Message> ModalOverlay<'a, Message> {
    pub fn new(
        background: impl Into<Element<'a, Message>>,
        foreground: impl Into<Element<'a, Message>>,
    ) -> Self {
        Self {
            background: background.into(),
            foreground: foreground.into(),
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for ModalOverlay<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.background), Tree::new(&self.foreground)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut [&mut self.background, &mut self.foreground]);
    }

    fn size(&self) -> Size<Length> {
        self.background.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let background =
            self.background
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits);
        let size = limits.resolve(self.size().width, self.size().height, background.size());
        let foreground = self.foreground.as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        layout::Node::with_children(size, vec![background, foreground])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn cosmic::iced::advanced::widget::Operation,
    ) {
        if let Some(foreground) = layout.children().nth(1) {
            self.foreground.as_widget_mut().operate(
                &mut tree.children[1],
                foreground,
                renderer,
                operation,
            );
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Some(foreground) = layout.children().nth(1) {
            self.foreground.as_widget_mut().update(
                &mut tree.children[1],
                event,
                foreground,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        layout
            .children()
            .nth(1)
            .map_or_else(mouse::Interaction::default, |foreground| {
                self.foreground.as_widget().mouse_interaction(
                    &tree.children[1],
                    foreground,
                    cursor,
                    viewport,
                    renderer,
                )
            })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let mut children = layout.children();
        let Some(background) = children.next() else {
            return;
        };
        self.background.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            background,
            mouse::Cursor::Unavailable,
            viewport,
        );
        if let Some(foreground) = children.next() {
            self.foreground.as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                foreground,
                cursor,
                viewport,
            );
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: cosmic::iced::Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let foreground = layout.children().nth(1)?;
        self.foreground.as_widget_mut().overlay(
            &mut tree.children[1],
            foreground,
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        layout
            .children()
            .nth(1)
            .map_or_else(iced_accessibility::A11yTree::default, |foreground| {
                self.foreground
                    .as_widget()
                    .a11y_nodes(foreground, &state.children[1], cursor)
            })
    }
}

impl<'a, Message: 'a> From<ModalOverlay<'a, Message>> for Element<'a, Message> {
    fn from(overlay: ModalOverlay<'a, Message>) -> Self {
        Self::new(overlay)
    }
}

/// The inspector's concrete projection for the current shell state.
pub fn mount_for_width(
    window_width: f32,
    open: bool,
    has_context: bool,
) -> Option<InspectorLayout> {
    (open && has_context).then(|| design::ShellLayout::for_width(window_width).inspector)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Starting,
    Ready,
    Restoring,
    Loading,
    Unavailable,
}

impl RuntimeStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Ready => "Ready",
            Self::Restoring => "Restoring",
            Self::Loading => "Loading",
            Self::Unavailable => "Unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InspectorAction {
    Close,
    Resize {
        width: f32,
    },
    ResizeFinished,
    NewTerminalAt(PathBuf),
    Retry(CardId),
    ReloadBrowser(CardId),
    OpenBrowser(Url),
    RefreshRepositories,
    CreatePullRequest {
        project_id: ProjectId,
        repository: PathBuf,
    },
    FocusProjectSearch,
    CloseCard(CardId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub label: Cow<'static, str>,
    pub value: String,
}

impl Fact {
    fn new(label: impl Into<Cow<'static, str>>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: Cow<'static, str>,
    pub facts: Vec<Fact>,
}

impl Section {
    fn new(title: impl Into<Cow<'static, str>>, facts: Vec<Fact>) -> Self {
        Self {
            title: title.into(),
            facts,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub id: String,
    pub label: Cow<'static, str>,
    pub icon: &'static str,
    pub message: InspectorAction,
    pub primary: bool,
}

impl Action {
    fn new(
        id: impl Into<String>,
        label: impl Into<Cow<'static, str>>,
        icon: &'static str,
        message: InspectorAction,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon,
            message,
            primary: false,
        }
    }

    fn primary(mut self) -> Self {
        self.primary = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InspectorModel {
    pub card_id: CardId,
    pub title: String,
    pub kind: &'static str,
    pub icon: &'static str,
    pub sections: Vec<Section>,
    pub actions: Vec<Action>,
}

pub fn model_for_card(
    card: &CardState,
    runtime: RuntimeStatus,
    projects: &[SidebarProject],
) -> InspectorModel {
    let (kind, icon) = match &card.kind {
        CardKind::Terminal(_) => ("Terminal", "utilities-terminal-symbolic"),
        CardKind::Browser(_) => ("Browser", "internet-web-browser-symbolic"),
        CardKind::PullRequests(_) => ("Pull requests", "vcs-merge-request-symbolic"),
        CardKind::Repository(_) => ("Repository", "folder-symbolic"),
    };
    let mut sections = vec![Section::new(
        "Card",
        vec![
            Fact::new("Surface", kind),
            Fact::new("Card ID", card.id.to_string()),
            Fact::new("Runtime", runtime.label()),
            Fact::new("Residency", residency_label(&card.residency)),
        ],
    )];
    let mut actions = Vec::new();

    let repository_context = repository_context(card, projects);
    match &card.kind {
        CardKind::Terminal(session) => {
            sections.push(Section::new(
                "Terminal",
                vec![
                    Fact::new("Session ID", session.id.to_string()),
                    Fact::new("Command", command_label(&session.launch)),
                    Fact::new(
                        "Working directory",
                        session.launch.working_directory.display().to_string(),
                    ),
                    Fact::new("Continuity", terminal_continuity_label(&session.continuity)),
                    Fact::new(
                        "Transcript",
                        session.transcript_path.as_ref().map_or_else(
                            || "Not recorded".into(),
                            |path| path.display().to_string(),
                        ),
                    ),
                ],
            ));
            actions.push(
                Action::new(
                    format!("{}.terminal", card.id),
                    "New terminal here",
                    "utilities-terminal-symbolic",
                    InspectorAction::NewTerminalAt(session.launch.working_directory.clone()),
                )
                .primary(),
            );
        }
        CardKind::Browser(session) => {
            let presented_location = SanitizedLocation::from(&session.current_url);
            sections.push(Section::new(
                "Browser",
                vec![
                    Fact::new("Browser ID", session.id.to_string()),
                    Fact::new("URL", presented_location.into_inner()),
                    Fact::new(
                        "History",
                        format!(
                            "{} of {}",
                            session.history_index.saturating_add(1),
                            session.history.len()
                        ),
                    ),
                    Fact::new("Restore", browser_fidelity_label(session.fidelity)),
                ],
            ));
            #[cfg(feature = "integrated-browser")]
            actions.push(
                Action::new(
                    format!("{}.reload", card.id),
                    "Reload",
                    "view-refresh-symbolic",
                    InspectorAction::ReloadBrowser(card.id),
                )
                .primary(),
            );
            #[cfg(not(feature = "integrated-browser"))]
            actions.push(
                Action::new(
                    format!("{}.external", card.id),
                    "Open in system browser",
                    "internet-web-browser-symbolic",
                    InspectorAction::Retry(card.id),
                )
                .primary(),
            );
            actions.push(Action::new(
                format!("{}.browser-copy", card.id),
                "Open copy",
                "list-add-symbolic",
                InspectorAction::OpenBrowser(session.current_url.clone()),
            ));
        }
        CardKind::Repository(view) => {
            sections.push(Section::new(
                "Repository",
                vec![
                    Fact::new("Worktree", view.worktree.display().to_string()),
                    Fact::new("Git directory", view.repository.display().to_string()),
                ],
            ));
            actions.push(
                Action::new(
                    format!("{}.terminal", card.id),
                    "New terminal here",
                    "utilities-terminal-symbolic",
                    InspectorAction::NewTerminalAt(view.worktree.clone()),
                )
                .primary(),
            );
            actions.push(Action::new(
                format!("{}.refresh", card.id),
                "Refresh",
                "view-refresh-symbolic",
                InspectorAction::RefreshRepositories,
            ));
            if let Some(project) = projects.iter().find(|project| project.id == view.project) {
                actions.push(Action::new(
                    format!("{}.pull-request", card.id),
                    "Create PR",
                    "vcs-merge-request-symbolic",
                    InspectorAction::CreatePullRequest {
                        project_id: project.id,
                        repository: project.repository.clone(),
                    },
                ));
            }
        }
        CardKind::PullRequests(view) => {
            sections.push(Section::new(
                "Pull requests",
                vec![
                    Fact::new("Repository", view.repository.display().to_string()),
                    Fact::new(
                        "Selection",
                        view.selected_number
                            .map_or_else(|| "None".into(), |number| format!("#{number}")),
                    ),
                ],
            ));
            actions.push(
                Action::new(
                    format!("{}.search", card.id),
                    "Search projects",
                    "system-search-symbolic",
                    InspectorAction::FocusProjectSearch,
                )
                .primary(),
            );
        }
    }

    if let Some((project, worktree)) = repository_context {
        sections.push(repository_section(project, worktree));
    }
    if runtime == RuntimeStatus::Unavailable
        && !actions
            .iter()
            .any(|action| matches!(action.message, InspectorAction::Retry(_)))
    {
        actions.insert(
            0,
            Action::new(
                format!("{}.retry", card.id),
                "Retry",
                "view-refresh-symbolic",
                InspectorAction::Retry(card.id),
            )
            .primary(),
        );
    }
    actions.push(Action::new(
        format!("{}.close", card.id),
        "Close card",
        "window-close-symbolic",
        InspectorAction::CloseCard(card.id),
    ));

    InspectorModel {
        card_id: card.id,
        title: card.title.clone(),
        kind,
        icon,
        sections,
        actions,
    }
}

fn repository_context<'a>(
    card: &CardState,
    projects: &'a [SidebarProject],
) -> Option<(&'a SidebarProject, &'a SidebarWorktree)> {
    match &card.kind {
        CardKind::Repository(view) => projects
            .iter()
            .find(|project| project.id == view.project)
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == view.worktree)
                    .map(|worktree| (project, worktree))
            }),
        CardKind::Terminal(session) => projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(move |worktree| (project, worktree))
            })
            .filter(|(_, worktree)| session.launch.working_directory.starts_with(&worktree.path))
            .max_by_key(|(_, worktree)| worktree.path.components().count()),
        CardKind::Browser(session) => projects.iter().find_map(|project| {
            project
                .pull_requests
                .iter()
                .any(|pull_request| pull_request.url == session.current_url)
                .then(|| {
                    project
                        .worktrees
                        .first()
                        .map(|worktree| (project, worktree))
                })
                .flatten()
        }),
        CardKind::PullRequests(view) => projects
            .iter()
            .find(|project| project.repository == view.repository)
            .and_then(|project| {
                project
                    .worktrees
                    .first()
                    .map(|worktree| (project, worktree))
            }),
    }
}

fn repository_section(project: &SidebarProject, worktree: &SidebarWorktree) -> Section {
    let changes = worktree.dirty.as_ref().map_or_else(
        || "Status unavailable".into(),
        |dirty| {
            let total = dirty.staged
                + dirty.unstaged
                + dirty.untracked
                + dirty.conflicted
                + dirty.submodule;
            if total == 0 {
                "Clean".into()
            } else {
                format!(
                    "{total} changes ({} staged, {} unstaged, {} untracked)",
                    dirty.staged, dirty.unstaged, dirty.untracked
                )
            }
        },
    );
    Section::new(
        "Worktree context",
        vec![
            Fact::new("Project", &project.name),
            Fact::new(
                "Branch",
                worktree.branch.as_deref().unwrap_or("Detached HEAD"),
            ),
            Fact::new("Path", worktree.path.display().to_string()),
            Fact::new("Changes", changes),
            Fact::new(
                "Upstream",
                match (worktree.ahead, worktree.behind) {
                    (Some(ahead), Some(behind)) => format!("{ahead} ahead, {behind} behind"),
                    _ => "Comparison unavailable".into(),
                },
            ),
        ],
    )
}

fn residency_label(residency: &Residency) -> &'static str {
    match residency {
        Residency::Cold => "Cold",
        Residency::Hot => "Active",
        Residency::Hydrating { .. } => "Starting",
        Residency::Evicting { .. } => "Suspending",
        Residency::Hibernated => "Suspended",
        Residency::Failed { .. } => "Failed",
    }
}

fn terminal_continuity_label(continuity: &TerminalContinuity) -> &'static str {
    match continuity {
        TerminalContinuity::Live { .. } => "Live",
        TerminalContinuity::DetachedSameBoot { .. } => "Detached; reconnectable",
        TerminalContinuity::RelaunchAvailable { .. } => "Relaunch available",
        TerminalContinuity::Exited { .. } => "Exited",
        TerminalContinuity::Failed {
            retryable: true, ..
        } => "Failed; retryable",
        TerminalContinuity::Failed {
            retryable: false, ..
        } => "Failed",
    }
}

fn browser_fidelity_label(fidelity: BrowserRestoreFidelity) -> &'static str {
    match fidelity {
        BrowserRestoreFidelity::ExactLive => "Live view",
        BrowserRestoreFidelity::HistoryAndScroll => "History and scroll",
        BrowserRestoreFidelity::UrlOnly => "URL only",
    }
}

fn command_label(recipe: &crate::model::LaunchRecipe) -> String {
    std::iter::once(recipe.program.as_str())
        .chain(recipe.arguments.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reusable titled group used by every inspector section.
pub struct InspectorSection<'a, Message> {
    title: Cow<'a, str>,
    content: Vec<Element<'a, Message>>,
}

impl<'a, Message: 'a> InspectorSection<'a, Message> {
    pub fn new(title: impl Into<Cow<'a, str>>, content: Vec<Element<'a, Message>>) -> Self {
        Self {
            title: title.into(),
            content,
        }
    }

    pub fn view(self) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let mut children = Vec::with_capacity(self.content.len() + 1);
        children.push(widget::text::caption_heading(self.title).into());
        children.extend(self.content);
        widget::column::with_children(children)
            .spacing(spacing.space_xxs)
            .width(Length::Fill)
            .into()
    }
}

/// Compact full-width action group for an inspector footer.
pub struct SegmentedActions {
    actions: Vec<Action>,
}

impl SegmentedActions {
    pub fn new(actions: Vec<Action>) -> Self {
        Self { actions }
    }

    pub fn view(self) -> Element<'static, InspectorAction> {
        let spacing = cosmic::theme::spacing();
        let buttons: Vec<Element<'static, InspectorAction>> = self
            .actions
            .into_iter()
            .map(|action| -> Element<'static, InspectorAction> {
                let mut button = widget::button::standard(action.label)
                    .id(widget::Id::new(format!(
                        "context-inspector.action.{}",
                        action.id
                    )))
                    .leading_icon(widget::icon::from_name(action.icon).size(16))
                    .width(Length::Fill)
                    .on_press(action.message);
                if action.primary {
                    button = button.class(theme::Button::Suggested);
                }
                button.into()
            })
            .collect();
        widget::column::with_children(buttons)
            .spacing(spacing.space_xxxs)
            .width(Length::Fill)
            .into()
    }
}

pub fn view(model: InspectorModel, width: f32) -> Element<'static, InspectorAction> {
    let spacing = cosmic::theme::spacing();
    let heading = widget::row::with_children(vec![
        widget::icon::from_name(model.icon).size(20).into(),
        widget::column::with_children(vec![
            widget::text::heading(model.title).into(),
            widget::text::caption(model.kind).into(),
        ])
        .spacing(0)
        .width(Length::Fill)
        .into(),
        widget::button::icon(widget::icon::from_name("window-close-symbolic"))
            .id(close_focus_id())
            .name("Collapse context inspector")
            .tooltip("Collapse context inspector")
            .on_press(InspectorAction::Close)
            .into(),
    ])
    .align_y(Alignment::Center)
    .spacing(spacing.space_xxs);

    let mut sections = Vec::with_capacity(model.sections.len() * 2);
    for (index, section) in model.sections.into_iter().enumerate() {
        if index > 0 {
            sections.push(widget::divider::horizontal::light().into());
        }
        let facts = section
            .facts
            .into_iter()
            .map(|fact| {
                widget::column::with_children(vec![
                    widget::text::caption(fact.label).into(),
                    widget::selectable_text(fact.value)
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(0)
                .width(Length::Fill)
                .into()
            })
            .collect();
        sections.push(InspectorSection::new(section.title, facts).view());
    }
    let content = widget::scrollable(
        widget::column::with_children(sections)
            .spacing(spacing.space_xs)
            .width(Length::Fill),
    )
    .class(cosmic::style::iced::Scrollable::Minimal)
    .height(Length::Fill);
    let actions = SegmentedActions::new(model.actions).view();

    widget::container(
        widget::column::with_children(vec![
            heading.into(),
            widget::divider::horizontal::default().into(),
            content.into(),
            widget::divider::horizontal::default().into(),
            actions,
        ])
        .spacing(spacing.space_xs)
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .class(theme::Container::Background)
    .padding(spacing.space_s)
    .width(Length::Fixed(width.clamp(
        design::INSPECTOR_MIN_WIDTH,
        design::INSPECTOR_MAX_WIDTH,
    )))
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BrowserId, BrowserRestoreFidelity, BrowserSession, Existence, Exposure, LaunchRecipe,
        TerminalId, TerminalSession,
    };
    use crate::splitter::{Edge as SplitterEdge, Splitter};
    use cosmic::iced::{
        advanced::{Layout, layout, mouse, widget::Tree},
        id::Id,
    };
    use std::collections::BTreeMap;

    fn terminal_card() -> CardState {
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-l".into()],
            working_directory: PathBuf::from("/repo/worktree"),
            environment: BTreeMap::new(),
        };
        CardState {
            id: CardId::new(),
            generation: 1,
            title: "worktree".into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Terminal(TerminalSession {
                id: TerminalId::new(),
                launch: launch.clone(),
                transcript_path: Some(PathBuf::from("/state/transcript")),
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch,
                    last_exit: None,
                },
            }),
        }
    }

    fn browser_card(raw: &str) -> CardState {
        let url = Url::parse(raw).unwrap();
        CardState {
            id: CardId::new(),
            generation: 1,
            title: "Browser".into(),
            width_fraction: 0.7,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: url.clone(),
                history: vec![url],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: BrowserRestoreFidelity::HistoryAndScroll,
            }),
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
        }
    }

    #[test]
    fn browser_fact_uses_the_typed_sanitized_location_projection() {
        let model = model_for_card(
            &browser_card(
                "https://alice:password@EXAMPLE.test:443/private%20path?oauth=secret#token",
            ),
            RuntimeStatus::Ready,
            &[],
        );
        let presented = &model
            .sections
            .iter()
            .find(|section| section.title == "Browser")
            .unwrap()
            .facts
            .iter()
            .find(|fact| fact.label == "URL")
            .unwrap()
            .value;

        assert_eq!(presented, "https://example.test/private%20path");
        for sensitive in ["alice", "password", "oauth", "secret", "token", "?", "#"] {
            assert!(
                !presented.contains(sensitive),
                "inspector fact leaked {sensitive:?}: {presented}"
            );
        }
    }

    #[test]
    fn terminal_model_exposes_identity_context_and_real_actions() {
        let card = terminal_card();
        let model = model_for_card(&card, RuntimeStatus::Ready, &[]);
        assert_eq!(model.card_id, card.id);
        assert_eq!(model.kind, "Terminal");
        assert!(
            model
                .sections
                .iter()
                .any(|section| { section.facts.iter().any(|fact| fact.label == "Session ID") })
        );
        assert!(model.actions.iter().any(|action| matches!(
            &action.message,
            InspectorAction::NewTerminalAt(path) if path == &PathBuf::from("/repo/worktree")
        )));
        assert!(model.actions.iter().any(
            |action| matches!(action.message, InspectorAction::CloseCard(id) if id == card.id)
        ));
    }

    #[test]
    fn unavailable_runtime_adds_retry_without_replacing_context() {
        let card = terminal_card();
        let model = model_for_card(&card, RuntimeStatus::Unavailable, &[]);
        assert!(
            model.actions.iter().any(
                |action| matches!(action.message, InspectorAction::Retry(id) if id == card.id)
            )
        );
        assert!(
            model
                .sections
                .iter()
                .any(|section| section.title == "Terminal")
        );
    }

    #[test]
    fn inspector_collapses_and_switches_between_docked_and_overlay_mounts() {
        assert_eq!(
            mount_for_width(design::WIDE_BREAKPOINT, true, true),
            Some(InspectorLayout::Docked)
        );
        assert_eq!(
            mount_for_width(design::WIDE_BREAKPOINT - 1.0, true, true),
            Some(InspectorLayout::Overlay)
        );
        assert_eq!(mount_for_width(design::WIDE_BREAKPOINT, false, true), None);
        assert_eq!(mount_for_width(design::WIDE_BREAKPOINT, true, false), None);
    }

    #[test]
    fn modal_overlay_exposes_only_its_foreground_accessibility_tree() {
        let splitter = |id, label| -> Element<'static, InspectorAction> {
            Splitter::new(
                Id::new(id),
                label,
                360.0,
                320.0,
                420.0,
                SplitterEdge::Leading,
                false,
                |width| InspectorAction::Resize { width },
                || InspectorAction::ResizeFinished,
            )
            .into()
        };
        let element: Element<'_, InspectorAction> = ModalOverlay::new(
            splitter("background", "Background splitter"),
            splitter("foreground", "Foreground splitter"),
        )
        .into();
        let tree = Tree::new(&element);
        let child = || layout::Node::new(Size::new(4.0, 480.0));
        let node = layout::Node::with_children(Size::new(4.0, 480.0), vec![child(), child()]);
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);

        assert_eq!(accessible.root().len(), 1);
        assert_eq!(
            accessible.root()[0].node().label(),
            Some("Foreground splitter")
        );
    }
}
