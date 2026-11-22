//! The Worktrees view of the Work pane: every checkout of every open project,
//! the main checkout first, with what is true of each and who works in it,
//! and the actions that open, create and delete them.
//!
//! The rows are the window's own project model, the one the sidebar reads, and
//! every action is the sidebar's, so deleting a worktree here passes the same
//! checks and dialog as anywhere else. Which panes work in each checkout is
//! derived by the application during its projection, and only while this view
//! is on screen; [`State::refresh`] keeps the list of rows between projects
//! changing, so a frame builds only the rows it shows.
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use cosmic::{
    Element,
    iced::{self, Alignment, Length, Padding},
    widget,
};

use crate::{
    agent::Provider,
    design::{self, Tone, space},
    model::{CardId, ProjectId, ProjectKind, WorktreeId},
    sidebar::{
        ProjectCommand, RowDeletion, SidebarAction, SidebarEditor, SidebarProject, SidebarWorktree,
        WorktreeOperationPhase, WorktreeOperationRow, WorktreePullRequestState,
        is_primary_worktree,
    },
    ui_controls::{Icon, badge, icon_button, placeholder, popover, popover_item},
    work_ui::Layout,
};

const MENU_WIDTH: f32 = 280.0;
/// A worktree row's height before it has been measured: name and facts, then
/// its path and the panes working in it.
const ROW_ESTIMATE: f32 = 58.0;

/// What an agent or terminal working in a checkout is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Waiting,
    Idle,
    /// An agent launched in a terminal that has yet to report.
    Starting,
    /// A shell running no agent.
    Shell,
}

impl AgentState {
    fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting for you",
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Shell => "terminal",
        }
    }
}

/// An open chat or terminal running in a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub card: CardId,
    pub title: String,
    pub provider: Option<Provider>,
    pub state: AgentState,
    /// The channel holding the pane, by name.
    pub channel: Option<String>,
}

/// The open panes working in each checkout, by project and worktree.
pub type Agents = HashMap<(ProjectId, WorktreeId), Vec<Agent>>;

/// Whose actions menu is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Menu {
    Project(ProjectId),
    Worktree(ProjectId, WorktreeId),
}

#[derive(Debug, Clone)]
pub enum Action {
    Query(String),
    Menu(Option<Menu>),
    /// Shows one of the board's layouts in the pane instead.
    Layout(Layout),
    /// One of the sidebar's own project and worktree actions.
    Sidebar(SidebarAction),
    /// Goes to an open pane, in whichever channel holds it.
    FocusPane(CardId),
}

/// What only the application can do for this view.
#[derive(Debug, Clone)]
pub enum Effect {
    Layout(Layout),
    Sidebar(SidebarAction),
    FocusPane(CardId),
}

/// One line of the list, by position in the project model and by identity,
/// so a line whose project or worktree moved since is skipped rather than
/// drawn as another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Project {
        project: usize,
        id: ProjectId,
    },
    Operation {
        project: usize,
        id: ProjectId,
        operation: usize,
    },
    Worktree {
        project: usize,
        id: ProjectId,
        worktree: usize,
        worktree_id: WorktreeId,
    },
    /// A project with no checkout to list: a folder, or one still loading.
    Empty {
        project: usize,
        id: ProjectId,
    },
}

impl Entry {
    fn key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match self {
            Self::Project { id, .. } => (0u8, id).hash(&mut hasher),
            Self::Operation { id, operation, .. } => (1u8, id, operation).hash(&mut hasher),
            Self::Worktree {
                id, worktree_id, ..
            } => (2u8, id, worktree_id).hash(&mut hasher),
            Self::Empty { id, .. } => (3u8, id).hash(&mut hasher),
        }
        hasher.finish()
    }
}

#[derive(Debug, Default)]
pub struct State {
    query: String,
    menu: Option<Menu>,
    entries: Vec<Entry>,
    /// The projects revision and query the entries were listed from.
    listed: Option<(u64, String)>,
    agents: Agents,
}

impl State {
    /// Forgets what the view derived when the pane closes; the query stays.
    pub fn reset(&mut self) {
        self.menu = None;
        self.entries.clear();
        self.listed = None;
        self.agents.clear();
    }

    /// Lists the rows again if the projects or the query changed since.
    pub fn refresh(&mut self, revision: u64, projects: &[SidebarProject]) {
        if self
            .listed
            .as_ref()
            .is_some_and(|(listed, query)| *listed == revision && *query == self.query)
        {
            return;
        }
        self.entries = list(projects, &self.query);
        self.listed = Some((revision, self.query.clone()));
        if self.menu.is_some_and(|menu| !self.lists(menu)) {
            self.menu = None;
        }
    }

    /// Replaces who works in each checkout, when it changed.
    pub fn set_agents(&mut self, agents: Agents) {
        if self.agents != agents {
            self.agents = agents;
        }
    }

    #[cfg(test)]
    pub(crate) fn agents(&self) -> &Agents {
        &self.agents
    }

    fn lists(&self, menu: Menu) -> bool {
        self.entries.iter().any(|entry| match (entry, menu) {
            (Entry::Project { id, .. }, Menu::Project(project)) => *id == project,
            (
                Entry::Worktree {
                    id, worktree_id, ..
                },
                Menu::Worktree(project, worktree),
            ) => *id == project && *worktree_id == worktree,
            _ => false,
        })
    }

    pub fn update(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::Query(query) => {
                self.query = query;
                self.menu = None;
                // Listed again by the next projection, which follows this
                // update.
                self.listed = None;
                None
            }
            Action::Menu(menu) => {
                self.menu = menu;
                None
            }
            Action::Layout(layout) => Some(Effect::Layout(layout)),
            Action::Sidebar(action) => {
                self.menu = None;
                Some(Effect::Sidebar(action))
            }
            Action::FocusPane(card) => Some(Effect::FocusPane(card)),
        }
    }

    pub fn view<'a>(
        &'a self,
        projects: &'a [SidebarProject],
        editors: &'a [SidebarEditor],
        modal_open: bool,
    ) -> Element<'a, Action> {
        let view = View {
            state: self,
            projects,
            editors,
            modal_open,
        };
        widget::column::with_children(vec![view.header(), design::horizontal_rule(), view.body()])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// Every project's lines in sidebar order: its heading, the creations under
/// way, then its checkouts, the main one first. A query keeps the checkouts it
/// finds, and every checkout of a project whose name it finds.
fn list(projects: &[SidebarProject], query: &str) -> Vec<Entry> {
    let folded = crate::sidebar::fold_query(query);
    let mut entries = Vec::new();
    for (project, entity) in projects.iter().enumerate() {
        let id = entity.id;
        let whole = folded.is_empty() || entity.matches(&folded);
        let mut worktrees = entity
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, worktree)| whole || worktree.matches(&folded))
            .collect::<Vec<_>>();
        if !whole && worktrees.is_empty() {
            continue;
        }
        worktrees.sort_by_key(|(_, worktree)| {
            (
                !is_primary_worktree(&entity.repository, &worktree.path),
                std::cmp::Reverse(worktree.created_at_unix_ms),
            )
        });
        entries.push(Entry::Project { project, id });
        if whole {
            entries.extend(
                (0..entity.operations.len()).map(|operation| Entry::Operation {
                    project,
                    id,
                    operation,
                }),
            );
        }
        if worktrees.is_empty() && entity.operations.is_empty() {
            entries.push(Entry::Empty { project, id });
        }
        entries.extend(
            worktrees
                .into_iter()
                .map(|(worktree, row)| Entry::Worktree {
                    project,
                    id,
                    worktree,
                    worktree_id: row.id,
                }),
        );
    }
    entries
}

#[derive(Clone, Copy)]
struct View<'a> {
    state: &'a State,
    projects: &'a [SidebarProject],
    editors: &'a [SidebarEditor],
    modal_open: bool,
}

impl<'a> View<'a> {
    fn project(&self, index: usize, id: ProjectId) -> Option<&'a SidebarProject> {
        self.projects.get(index).filter(|project| project.id == id)
    }

    /// `action`, unless a dialog holds the window.
    fn gated(&self, action: Action) -> Option<Action> {
        (!self.modal_open).then_some(action)
    }

    fn sidebar(&self, action: SidebarAction) -> Option<Action> {
        self.gated(Action::Sidebar(action))
    }

    fn header(&self) -> Element<'a, Action> {
        let mut search = crate::ui_controls::field::search(
            "Filter by project, branch or pull request",
            &self.state.query,
        )
        .on_input(Action::Query)
        .width(Length::Fill);
        if !self.state.query.is_empty() {
            search = search.trailing_icon(crate::ui_controls::field::clear(Action::Query(
                String::new(),
            )));
        }
        // The filter has a line of its own: beside the title and controls it
        // was the one element left to shrink, so a pane beside a chat
        // squeezed it down to its icon.
        let toolbar = widget::row::with_children(vec![
            design::title("Worktrees").into(),
            widget::Space::new().width(Length::Fill).into(),
            crate::work_ui::layout_switch(Layout::Worktrees).map(|action| match action {
                crate::work_ui::Action::Layout(layout) => Action::Layout(layout),
                // The switch sends nothing else.
                _ => Action::Layout(Layout::Worktrees),
            }),
            widget::button::standard("Open project…")
                .leading_icon(Icon::OpenFolder.handle())
                .on_press_maybe(self.sidebar(SidebarAction::OpenRepositoryPicker))
                .into(),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center);
        widget::container(
            widget::column::with_children(vec![toolbar.into(), search.into()]).spacing(space::S),
        )
        .padding(Padding::from([space::S, space::L]))
        .width(Length::Fill)
        .into()
    }

    fn body(&self) -> Element<'a, Action> {
        if self.projects.is_empty() {
            return placeholder("No projects are open")
                .icon(Icon::Worktree.handle())
                .detail("Open a Git project and its checkouts appear here.")
                .into();
        }
        let entries = &self.state.entries;
        if entries.is_empty() {
            return placeholder("No worktrees match")
                .icon(Icon::Search.handle())
                .detail(format!(
                    "No project, branch or pull request mentions “{}”.",
                    self.state.query.trim()
                ))
                .into();
        }
        let keys = entries.iter().map(Entry::key).collect();
        let open = self.state.menu.and_then(|menu| {
            entries
                .iter()
                .find(|entry| match (entry, menu) {
                    (Entry::Project { id, .. }, Menu::Project(project)) => *id == project,
                    (
                        Entry::Worktree {
                            id, worktree_id, ..
                        },
                        Menu::Worktree(project, worktree),
                    ) => *id == project && *worktree_id == worktree,
                    _ => false,
                })
                .map(Entry::key)
        });
        let view = *self;
        crate::interaction::overflow_fade_y(
            crate::ui_controls::keep_in_place(
                widget::scrollable(
                    widget::container(
                        crate::ui_controls::culled_column(keys, move |index| {
                            view.entry(entries[index], index == 0)
                        })
                        .spacing(space::XS)
                        .estimate(ROW_ESTIMATE)
                        .keep_built(open),
                    )
                    .padding(design::with_vertical_rail(Padding::from([
                        space::S,
                        space::L,
                    ]))),
                )
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .width(Length::Fill)
                .height(Length::Fill),
            ),
            design::base,
        )
    }

    fn entry(&self, entry: Entry, first: bool) -> Element<'a, Action> {
        let skipped = || widget::Space::new().height(0).into();
        match entry {
            Entry::Project { project, id } => self
                .project(project, id)
                .map_or_else(skipped, |project| self.project_heading(project, first)),
            Entry::Operation {
                project,
                id,
                operation,
            } => self
                .project(project, id)
                .and_then(|project| project.operations.get(operation))
                .map_or_else(skipped, |operation| self.operation(operation)),
            Entry::Worktree {
                project,
                id,
                worktree,
                worktree_id,
            } => self
                .project(project, id)
                .and_then(|project| {
                    let worktree = project
                        .worktrees
                        .get(worktree)
                        .filter(|row| row.id == worktree_id)?;
                    Some(self.worktree(project, worktree))
                })
                .unwrap_or_else(skipped),
            Entry::Empty { project, id } => self.project(project, id).map_or_else(skipped, |p| {
                design::caption(match (&p.unreachable, p.kind) {
                    (Some(reason), _) => format!("This project could not be read: {reason}"),
                    (None, ProjectKind::Folder) => {
                        "A folder, not a Git repository: it has no worktrees.".to_owned()
                    }
                    (None, ProjectKind::Git) if p.reloading => "Reading checkouts…".to_owned(),
                    (None, ProjectKind::Git) => "No checkouts listed yet.".to_owned(),
                })
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .width(Length::Fill)
                .into()
            }),
        }
    }

    /// The project's name and path, how many worktrees it has and how many
    /// agents work in its checkouts, and its own actions.
    fn project_heading(&self, project: &'a SidebarProject, first: bool) -> Element<'a, Action> {
        let icon: Element<'a, Action> = match &project.avatar {
            Some(handle) => iced::widget::image(handle.clone())
                .width(20)
                .height(20)
                .into(),
            None => widget::icon(Icon::Folder.handle())
                .size(design::ROW_ICON_SIZE)
                .into(),
        };
        let worktrees = project
            .worktrees
            .iter()
            .filter(|worktree| !is_primary_worktree(&project.repository, &worktree.path))
            .count();
        let (mut working, mut waiting) = (0, 0);
        for worktree in &project.worktrees {
            for agent in self
                .state
                .agents
                .get(&(project.id, worktree.id))
                .into_iter()
                .flatten()
            {
                match agent.state {
                    AgentState::Working => working += 1,
                    AgentState::Waiting => waiting += 1,
                    _ => {}
                }
            }
        }
        let mut title = vec![
            icon,
            design::heading(project.name.as_str())
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            crate::ui_controls::fade_text(crate::app::display_path(&project.repository))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .width(Length::Fill)
                .into(),
        ];
        if project.kind == ProjectKind::Git {
            title.push(
                design::caption(match worktrees {
                    1 => "1 worktree".to_owned(),
                    count => format!("{count} worktrees"),
                })
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            );
        }
        if working > 0 {
            title.push(crate::interaction::tooltip(
                widget::row::with_children(vec![
                    crate::ui_controls::activity_spinner(12.0, design::accent_color),
                    design::caption(working.to_string()).into(),
                ])
                .spacing(space::XXS)
                .align_y(Alignment::Center),
                format!("{working} working in this project's checkouts"),
            ));
        }
        if waiting > 0 {
            title.push(crate::interaction::tooltip(
                badge(
                    format!("{waiting} waiting"),
                    Tone::Themed(design::warning_color),
                ),
                "Agents waiting for you in this project's checkouts",
            ));
        }
        if project.kind == ProjectKind::Git {
            title.push(
                icon_button(Icon::WorktreeAdd, "New worktree…")
                    .on_press_maybe(self.sidebar(SidebarAction::NewWorktree {
                        project_id: project.id,
                        repository: project.repository.clone(),
                    }))
                    .into(),
            );
        }
        title.push(self.project_menu(project));
        widget::container(
            widget::row::with_children(title)
                .spacing(space::S)
                .align_y(Alignment::Center),
        )
        .padding(Padding {
            top: if first { 0.0 } else { space::M },
            bottom: space::XXS,
            ..Padding::ZERO
        })
        .width(Length::Fill)
        .into()
    }

    fn project_menu(&self, project: &'a SidebarProject) -> Element<'a, Action> {
        let menu = Menu::Project(project.id);
        let open = self.state.menu == Some(menu);
        let anchor = icon_button(Icon::More, format!("{} actions", project.name))
            .compact()
            .selected(open)
            .on_press_maybe(self.gated(Action::Menu((!open).then_some(menu))));
        let content = (open && !self.modal_open).then(|| {
            let command = |command| {
                Action::Sidebar(SidebarAction::ProjectCommand {
                    project_id: project.id,
                    worktree_id: None,
                    command,
                })
            };
            let mut items: Vec<Element<'a, Action>> = vec![
                popover_item(
                    Icon::OpenFolder.handle(),
                    "Open folder",
                    "In the file manager",
                    command(ProjectCommand::Reveal),
                )
                .into(),
                popover_item(
                    Icon::CopyPath.handle(),
                    "Copy path",
                    crate::app::display_path(&project.repository),
                    command(ProjectCommand::CopyPath),
                )
                .into(),
            ];
            items.extend(self.editor_items(command));
            if project.kind == ProjectKind::Git && project.github_ready {
                items.push(
                    popover_item(
                        Icon::Browser.handle(),
                        "Open repository on GitHub",
                        "In the browser",
                        command(ProjectCommand::RepositoryUrl),
                    )
                    .into(),
                );
            }
            items.push(design::horizontal_rule());
            items.push(
                popover_item(
                    Icon::Delete.handle(),
                    "Remove project…",
                    "Asks first; its files stay on disk",
                    command(ProjectCommand::Remove),
                )
                .into(),
            );
            widget::column::with_children(items)
                .width(MENU_WIDTH)
                .into()
        });
        popover(
            anchor,
            content,
            widget::popover::Position::Bottom,
            Action::Menu(None),
        )
    }

    fn editor_items(
        &self,
        command: impl Fn(ProjectCommand) -> Action,
    ) -> impl Iterator<Item = Element<'a, Action>> {
        self.editors.iter().map(move |editor| {
            popover_item(
                editor_icon(editor),
                editor.name.as_str(),
                "Open in this editor",
                command(ProjectCommand::OpenEditor(editor.id.clone())),
            )
            .into()
        })
    }

    /// A worktree being created, or whose creation stopped.
    fn operation(&self, operation: &'a WorktreeOperationRow) -> Element<'a, Action> {
        let failed = operation.phase == WorktreeOperationPhase::Failed;
        let glyph: Element<'a, Action> = if failed {
            widget::icon(Icon::Warning.handle())
                .size(design::ROW_ICON_SIZE)
                .into()
        } else {
            widget::container(crate::ui_controls::activity_spinner(
                12.0,
                design::muted_color,
            ))
            .center(Length::Fixed(f32::from(design::ROW_ICON_SIZE)))
            .into()
        };
        let mut row = vec![
            glyph,
            widget::column::with_children(vec![
                design::body(operation.label.as_str())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                design::caption(operation.detail.as_str())
                    .class(cosmic::theme::Text::Custom(if failed {
                        design::warning_text
                    } else {
                        design::muted_text
                    }))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(space::XXS)
            .width(Length::Fill)
            .into(),
        ];
        let id = operation.id;
        if failed && operation.reopenable {
            row.push(
                widget::button::standard("Edit…")
                    .on_press_maybe(self.sidebar(SidebarAction::EditWorktreeOperation(id)))
                    .into(),
            );
            row.push(
                widget::button::standard("Retry")
                    .on_press_maybe(self.sidebar(SidebarAction::RetryWorktreeOperation(id)))
                    .into(),
            );
        }
        if operation.cancellable {
            row.push(
                widget::button::standard("Cancel")
                    .on_press_maybe(self.sidebar(SidebarAction::CancelWorktreeOperation(id)))
                    .into(),
            );
        } else if failed {
            row.push(
                icon_button(Icon::Close, "Dismiss")
                    .on_press_maybe(self.sidebar(SidebarAction::DismissWorktreeOperation(id)))
                    .into(),
            );
        }
        widget::container(
            widget::row::with_children(row)
                .spacing(space::S)
                .align_y(Alignment::Center),
        )
        .padding([space::XS, space::S])
        .width(Length::Fill)
        .into()
    }

    /// A checkout: its name and the facts about it, then its path and the
    /// panes working in it. Pressing it opens it.
    fn worktree(
        &self,
        project: &'a SidebarProject,
        worktree: &'a SidebarWorktree,
    ) -> Element<'a, Action> {
        let primary = is_primary_worktree(&project.repository, &worktree.path);
        let deletion = worktree.deletion.as_ref();
        let deleting = deletion == Some(&RowDeletion::Deleting);
        let indicators = crate::sidebar::worktree_indicators(worktree);
        let glyph: Element<'a, Action> = if deleting {
            widget::container(crate::ui_controls::activity_spinner(
                12.0,
                design::muted_color,
            ))
            .center(Length::Fixed(f32::from(design::ROW_ICON_SIZE)))
            .into()
        } else if let Some(pr) = indicators.pull_request {
            crate::ui_controls::pull_request_state_glyph(pr.state, design::ROW_ICON_SIZE)
        } else {
            widget::icon(
                if primary {
                    Icon::Branch
                } else {
                    Icon::Worktree
                }
                .handle(),
            )
            .size(design::ROW_ICON_SIZE)
            .into()
        };
        let mut title = vec![
            glyph,
            design::heading(worktree.label())
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        ];
        if primary {
            title.push(mark(Icon::Home, "Main checkout"));
        }
        if worktree.detached {
            title.push(mark(Icon::Info, "Detached HEAD"));
        }
        if worktree.locked {
            title.push(mark(Icon::Lock, "Locked worktree"));
        }
        title.push(match indicators.pull_request {
            Some(pr) => crate::ui_controls::fade_text(format!("#{} {}", pr.number, pr.title))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .width(Length::Fill)
                .into(),
            None => widget::Space::new().width(Length::Fill).into(),
        });
        title.extend(self.facts(worktree, &indicators));
        title.push(self.worktree_menu(project, worktree, primary));

        let mut second = vec![
            crate::ui_controls::fade_text(crate::app::display_path(&worktree.path))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .width(Length::Fill)
                .into(),
        ];
        match deletion {
            Some(RowDeletion::Deleting) => second.push(
                design::caption("Deleting…")
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            ),
            Some(RowDeletion::Failed(reason)) => {
                second.push(crate::interaction::tooltip(
                    badge("Delete failed", Tone::Themed(design::destructive_color)),
                    reason.to_string(),
                ));
                second.push(
                    widget::button::standard("Retry delete")
                        .on_press_maybe(self.sidebar(SidebarAction::RetryWorktreeDeletion {
                            project_id: project.id,
                            worktree_id: worktree.id,
                        }))
                        .into(),
                );
            }
            None => {}
        }
        let mut details = vec![
            widget::row::with_children(second)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
        ];
        // The panes get a line of their own, so a busy checkout's path keeps
        // its width instead of shrinking to its first characters.
        let agents = self
            .state
            .agents
            .get(&(project.id, worktree.id))
            .map_or(&[][..], Vec::as_slice);
        if !agents.is_empty() {
            details.push(
                widget::row::with_children(agents.iter().map(|agent| self.agent(agent)))
                    .spacing(space::XXS)
                    .align_y(Alignment::Center)
                    .wrap()
                    .into(),
            );
        }
        let lines = widget::column::with_children(vec![
            widget::row::with_children(title)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            widget::container(widget::column::with_children(details).spacing(space::XXS))
                .padding(Padding {
                    left: f32::from(design::ROW_ICON_SIZE) + space::S,
                    ..Padding::ZERO
                })
                .into(),
        ])
        .spacing(space::XS);
        let menu = Menu::Worktree(project.id, worktree.id);
        let selected = self.state.menu == Some(menu);
        // A worktree on its way out is not somewhere to go: opening it would
        // start sessions in a directory Git is about to remove.
        let open = (!deleting).then(|| SidebarAction::OpenWorktree {
            project_id: project.id,
            worktree_id: worktree.id,
            path: worktree.path.clone(),
        });
        let button = widget::button::custom(lines)
            .name(format!("Open {}", worktree.label()))
            .class(crate::work_ui::card_style(selected))
            .padding([space::XS + space::XXS, space::S])
            .width(Length::Fill)
            .on_press_maybe(open.and_then(|open| self.sidebar(open)));
        let row = widget::mouse_area(crate::interaction::hover(button));
        if self.modal_open {
            row.into()
        } else {
            row.on_right_press(Action::Menu(Some(menu))).into()
        }
    }

    /// What is true of the checkout now: its changes, conflicts, how far it
    /// is from its upstream, and its pull request's checks and review.
    fn facts(
        &self,
        worktree: &'a SidebarWorktree,
        indicators: &crate::sidebar::WorktreeIndicators<'a>,
    ) -> Vec<Element<'a, Action>> {
        let mut facts = Vec::new();
        if let Some(dirty) = &worktree.dirty
            && dirty.conflicted > 0
        {
            facts.push(badge(
                format!("{} conflicted", dirty.conflicted),
                Tone::Themed(design::destructive_color),
            ));
        }
        if let Some(label) = indicators.dirty {
            facts.push(crate::interaction::tooltip(
                badge(label, Tone::Themed(design::warning_color)),
                "Uncommitted changes in the checkout",
            ));
        }
        if let (Some(ahead), Some(behind)) = (worktree.ahead, worktree.behind)
            && (ahead > 0 || behind > 0)
        {
            let counts = match (ahead, behind) {
                (ahead, 0) => format!("↑{ahead}"),
                (0, behind) => format!("↓{behind}"),
                (ahead, behind) => format!("↑{ahead} ↓{behind}"),
            };
            facts.push(crate::interaction::tooltip(
                design::caption(counts)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .wrapping(iced::widget::text::Wrapping::None),
                format!("{ahead} ahead of and {behind} behind its upstream"),
            ));
        }
        if let Some(pr) = indicators.pull_request {
            if pr.review_requested() {
                facts.push(badge(
                    if pr.review_requested_again {
                        "Review again"
                    } else {
                        "Review"
                    },
                    Tone::Themed(design::accent_color),
                ));
            }
            if let Some(checks) = pr.active_checks() {
                facts.push(crate::sidebar::check_indicator(checks));
            }
            if let Some(state) = pr.state {
                facts.push(badge(state.label(), Tone::Themed(state.tone())));
            }
        }
        facts
    }

    /// A pane working in the checkout: its mark and title, and the channel
    /// it is in; pressing it goes there.
    fn agent(&self, agent: &'a Agent) -> Element<'a, Action> {
        let glyph: Element<'a, Action> = match (agent.state, agent.provider) {
            (AgentState::Working, _) => {
                crate::ui_controls::activity_spinner(12.0, design::accent_color)
            }
            (AgentState::Waiting, _) => widget::icon(Icon::AgentWaiting.handle())
                .size(design::COMPACT_ICON_SIZE)
                .into(),
            (_, Some(provider)) => widget::icon(crate::ui_controls::provider_icon(provider))
                .size(design::COMPACT_ICON_SIZE)
                .into(),
            (_, None) => widget::icon(Icon::Terminal.handle())
                .size(design::COMPACT_ICON_SIZE)
                .into(),
        };
        let place = agent.channel.as_deref().map_or_else(
            || "in no channel".to_owned(),
            |channel| format!("in #{channel}"),
        );
        let button = widget::button::custom(
            widget::row::with_children(vec![
                glyph,
                design::caption(agent.title.as_str())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ])
            .spacing(space::XXS)
            .align_y(Alignment::Center),
        )
        .name(format!(
            "Go to {}, {}, {place}",
            agent.title,
            agent.state.label()
        ))
        .class(crate::ui_controls::control_style(false, None))
        .padding([space::XXS, space::XS])
        .on_press_maybe(self.gated(Action::FocusPane(agent.card)));
        crate::interaction::tooltip(
            button,
            format!("{}, {}, {place}", agent.title, agent.state.label()),
        )
    }

    fn worktree_menu(
        &self,
        project: &'a SidebarProject,
        worktree: &'a SidebarWorktree,
        primary: bool,
    ) -> Element<'a, Action> {
        let menu = Menu::Worktree(project.id, worktree.id);
        let open = self.state.menu == Some(menu);
        let deleting = worktree.deletion == Some(RowDeletion::Deleting);
        let anchor = icon_button(Icon::More, format!("{} actions", worktree.label()))
            .compact()
            .selected(open)
            .on_press_maybe(
                self.gated(Action::Menu((!open).then_some(menu)))
                    .filter(|_| !deleting),
            );
        let content = (open && !self.modal_open && !deleting).then(|| {
            let git = project.kind == ProjectKind::Git;
            let branch = worktree.branch.as_deref().filter(|_| !worktree.detached);
            let command = |command| {
                Action::Sidebar(SidebarAction::ProjectCommand {
                    project_id: project.id,
                    worktree_id: Some(worktree.id),
                    command,
                })
            };
            let mut items: Vec<Element<'a, Action>> = vec![
                popover_item(
                    Icon::Open.handle(),
                    "Open",
                    "Show its panes",
                    Action::Sidebar(SidebarAction::OpenWorktree {
                        project_id: project.id,
                        worktree_id: worktree.id,
                        path: worktree.path.clone(),
                    }),
                )
                .into(),
                popover_item(
                    Icon::Chat.handle(),
                    "New thread here",
                    "A chat working in this checkout",
                    command(ProjectCommand::NewChannel),
                )
                .into(),
                popover_item(
                    Icon::Terminal.handle(),
                    "New terminal here",
                    "A shell in this checkout",
                    command(ProjectCommand::NewTerminal),
                )
                .into(),
                design::horizontal_rule(),
            ];
            if git {
                let detached = "Detached HEAD has no branch";
                items.push(
                    popover_item(
                        Icon::Branch.handle(),
                        "Copy branch",
                        branch.unwrap_or(detached),
                        command(ProjectCommand::CopyBranch),
                    )
                    .on_press_maybe(branch.map(|_| command(ProjectCommand::CopyBranch)))
                    .into(),
                );
            }
            items.push(
                popover_item(
                    Icon::CopyPath.handle(),
                    "Copy path",
                    crate::app::display_path(&worktree.path),
                    command(ProjectCommand::CopyPath),
                )
                .into(),
            );
            if git {
                items.push(
                    popover_item(
                        Icon::WorktreeAdd.handle(),
                        "New worktree from this branch…",
                        branch.map_or("Detached HEAD has no branch".into(), |branch| {
                            format!("Starting at {branch}")
                        }),
                        command(ProjectCommand::NewWorktreeFromBranch),
                    )
                    .on_press_maybe(branch.map(|_| command(ProjectCommand::NewWorktreeFromBranch)))
                    .into(),
                );
                if project.github_ready {
                    let pr = worktree.pull_request();
                    items.push(
                        popover_item(
                            Icon::PullRequest.handle(),
                            if pr.is_some() {
                                "Open pull request"
                            } else {
                                "Create pull request…"
                            },
                            pr.map_or_else(
                                || "On GitHub".to_owned(),
                                |pr| format!("#{}", pr.number),
                            ),
                            command(ProjectCommand::PullRequest),
                        )
                        .on_press_maybe(branch.map(|_| command(ProjectCommand::PullRequest)))
                        .into(),
                    );
                }
            }
            items.push(
                popover_item(
                    Icon::OpenFolder.handle(),
                    "Open folder",
                    "In the file manager",
                    command(ProjectCommand::Reveal),
                )
                .into(),
            );
            items.extend(self.editor_items(command));
            if git {
                items.push(design::horizontal_rule());
                let refusal = if primary {
                    Some("The main checkout is the repository itself")
                } else if worktree.locked {
                    Some("Unlock it with git worktree unlock first")
                } else {
                    None
                };
                let merged = worktree
                    .pull_request()
                    .is_some_and(|pr| pr.state == Some(WorktreePullRequestState::Merged));
                items.push(
                    popover_item(
                        Icon::Delete.handle(),
                        "Delete worktree…",
                        refusal.unwrap_or(if merged {
                            "Its pull request is merged"
                        } else {
                            "Asks first, and checks for unsaved work"
                        }),
                        command(ProjectCommand::Delete),
                    )
                    .on_press_maybe(refusal.is_none().then(|| command(ProjectCommand::Delete)))
                    .into(),
                );
            }
            widget::column::with_children(items)
                .width(MENU_WIDTH)
                .into()
        });
        popover(
            anchor,
            content,
            widget::popover::Position::Bottom,
            Action::Menu(None),
        )
    }
}

/// A durable fact about a checkout, as a small muted glyph with its name.
fn mark<'a>(icon: Icon, name: &'static str) -> Element<'a, Action> {
    crate::interaction::tooltip(
        widget::icon(icon.handle())
            .size(design::COMPACT_ICON_SIZE)
            .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                iced::widget::svg::Style {
                    color: Some(design::muted_icon_color(theme)),
                }
            }))),
        name,
    )
}

fn editor_icon(editor: &SidebarEditor) -> widget::icon::Handle {
    if std::path::Path::new(&editor.icon).is_absolute() {
        widget::icon::from_path(std::path::PathBuf::from(&editor.icon))
    } else {
        widget::icon::from_name(editor.icon.as_str()).handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::RepositoryId;

    fn worktree(path: &str, branch: &str, created: i64) -> SidebarWorktree {
        let mut row =
            SidebarWorktree::from_durable(WorktreeId::new(), path.into(), Some(branch.to_owned()));
        row.created_at_unix_ms = Some(created);
        row
    }

    fn project(name: &str, worktrees: Vec<SidebarWorktree>) -> SidebarProject {
        SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            name.into(),
            format!("/src/{name}").into(),
            worktrees,
            vec![],
        )
    }

    fn worktree_labels(state: &State, projects: &[SidebarProject]) -> Vec<String> {
        state
            .entries
            .iter()
            .map(|entry| match *entry {
                Entry::Project { project, .. } => format!("# {}", projects[project].name),
                Entry::Worktree {
                    project, worktree, ..
                } => projects[project].worktrees[worktree].label().to_owned(),
                Entry::Operation { .. } => "operation".to_owned(),
                Entry::Empty { .. } => "empty".to_owned(),
            })
            .collect()
    }

    #[test]
    fn the_main_checkout_leads_and_a_query_keeps_what_it_finds() {
        let projects = vec![
            project(
                "cagents",
                vec![
                    worktree("/src/cagents-old", "old-fix", 1),
                    worktree("/src/cagents", "main", 0),
                    worktree("/src/cagents-new", "new-feature", 2),
                ],
            ),
            project("notes", vec![worktree("/src/notes", "trunk", 0)]),
        ];
        let mut state = State::default();
        state.refresh(1, &projects);
        assert_eq!(
            worktree_labels(&state, &projects),
            [
                "# cagents",
                "main",
                "new-feature",
                "old-fix",
                "# notes",
                "trunk"
            ],
            "each project's main checkout first, then its newest worktrees"
        );

        drop(state.update(Action::Query("new-feat".into())));
        state.refresh(1, &projects);
        assert_eq!(
            worktree_labels(&state, &projects),
            ["# cagents", "new-feature"],
            "a branch query keeps its project heading and the rows it finds"
        );

        drop(state.update(Action::Query("notes".into())));
        state.refresh(1, &projects);
        assert_eq!(
            worktree_labels(&state, &projects),
            ["# notes", "trunk"],
            "a project's name keeps every checkout of it"
        );
    }

    #[test]
    fn an_unchanged_model_is_not_listed_again_and_a_closed_row_closes_its_menu() {
        let mut projects = vec![project(
            "cagents",
            vec![
                worktree("/src/cagents", "main", 0),
                worktree("/src/cagents-fix", "fix", 1),
            ],
        )];
        let fix = projects[0].worktrees[1].id;
        let id = projects[0].id;
        let mut state = State::default();
        state.refresh(1, &projects);
        drop(state.update(Action::Menu(Some(Menu::Worktree(id, fix)))));

        // The same revision means the same projects, so nothing is listed.
        projects[0].worktrees.pop();
        state.refresh(1, &projects);
        assert_eq!(state.entries.len(), 3);
        assert_eq!(state.menu, Some(Menu::Worktree(id, fix)));

        state.refresh(2, &projects);
        assert_eq!(worktree_labels(&state, &projects), ["# cagents", "main"]);
        assert_eq!(state.menu, None, "the deleted worktree's menu closed");
    }
}
