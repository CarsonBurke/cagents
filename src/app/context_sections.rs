//! The context inspector as vertical, collapsible sections.
//!
//! A channel's conversation is the inspector's subject, so its sections lead
//! with what that conversation produced: the pull requests it worked on, then
//! the worktrees it edited. The active worktree's changes and files follow
//! behind one switch, collapsed until asked for unless nothing else is shown,
//! and a focused editor adds what its language server found. Each section
//! says how much it holds in its header, so a collapsed one still reads at a
//! glance.
//!
//! Expanded sections share the panel's height the way a stack of panes does:
//! the worktree's tree takes what is left, and the short lists take only the
//! rows they have. Nothing loads or polls for a section that is collapsed.

use super::*;

/// One of the context inspector's sections, in the order they stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Section {
    PullRequests,
    Worktrees,
    /// The active worktree's changes or files, whichever [`WorkspaceView`]
    /// is chosen.
    Workspace,
    /// The focused editor's diagnostics.
    Problems,
    /// A focused editor's definition or references results.
    Locations,
}

/// What the workspace section shows of the active worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkspaceView {
    /// Source control: what is uncommitted, what the branch changes, and its
    /// history.
    #[default]
    SourceControl,
    Files,
}

impl WorkspaceView {
    const fn label(self) -> &'static str {
        match self {
            Self::SourceControl => "Source control",
            Self::Files => "Files",
        }
    }

    const fn icon(self) -> Icon {
        match self {
            Self::SourceControl => Icon::Changes,
            Self::Files => Icon::Folder,
        }
    }
}

/// Which sections are expanded, and the workspace section's view. Kept with
/// the window, so the inspector reopens as it was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Sections {
    pull_requests: bool,
    worktrees: bool,
    workspace: bool,
    problems: bool,
    locations: bool,
    pub workspace_view: WorkspaceView,
}

impl Default for Sections {
    /// What the conversation did leads, expanded. The worktree's own changes
    /// and files are a click away rather than a tall tree beneath it.
    fn default() -> Self {
        Self {
            pull_requests: true,
            worktrees: true,
            workspace: false,
            problems: true,
            locations: true,
            workspace_view: WorkspaceView::default(),
        }
    }
}

impl Sections {
    pub fn is_open(self, section: Section) -> bool {
        match section {
            Section::PullRequests => self.pull_requests,
            Section::Worktrees => self.worktrees,
            Section::Workspace => self.workspace,
            Section::Problems => self.problems,
            Section::Locations => self.locations,
        }
    }

    pub fn set_open(&mut self, section: Section, open: bool) {
        *match section {
            Section::PullRequests => &mut self.pull_requests,
            Section::Worktrees => &mut self.worktrees,
            Section::Workspace => &mut self.workspace,
            Section::Problems => &mut self.problems,
            Section::Locations => &mut self.locations,
        } = open;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle(Section),
    /// Choose the workspace section's view, expanding it.
    Show(WorkspaceView),
}

/// How an expanded section's body takes the panel's height.
#[derive(Clone, Copy, PartialEq)]
enum Height {
    /// A share of the height the other sections leave; the body scrolls.
    Share,
    /// Its own content's height, up to this many two-line rows, past which
    /// it scrolls.
    Content { rows: f32 },
}

/// The rows of a short list before it scrolls instead of growing. The half
/// row shows there is more.
const LIST_ROWS_SHOWN: f32 = 5.5;

/// A section header's height: a dense row and its padding.
const SECTION_HEADER_HEIGHT: f32 = design::DENSE_ROW_HEIGHT + 2.0 * space::XXS;

/// The height a short list takes before it scrolls.
fn list_height(rows: f32) -> f32 {
    design::DENSE_ROW_HEIGHT * 2.0 * rows
}

/// The height the first open list may take when no section shares the
/// panel: what is left of `panel` after the `sections` headers, the rules
/// between and below them, and the `others` lists' own heights, and never
/// less than its usual `own` height.
fn lead_list_height(panel: f32, sections: usize, own: f32, others: f32) -> f32 {
    let headers = sections as f32 * (SECTION_HEADER_HEIGHT + design::BORDER_WIDTH);
    (panel - headers - others).max(own)
}

/// A header's count and the text style that says what kind of count it is.
type Count = (String, fn(&cosmic::Theme) -> iced::widget::text::Style);

struct Pane<'a> {
    section: Section,
    title: Cow<'a, str>,
    count: Option<Count>,
    /// Spoken after the title, for what the count and its tone mean.
    description: Option<String>,
    controls: Vec<Element<'a, Message>>,
    height: Height,
    body: Option<Element<'a, Message>>,
}

impl Cagents {
    /// Whether the workspace section is all the inspector has: no
    /// conversation's pull requests or worktrees lead it. It is then shown
    /// rather than left as a lone collapsed header over an empty panel.
    fn workspace_alone(&self) -> bool {
        !self.pull_requests_listed() && self.edited_worktrees().is_empty()
    }

    /// The inspector's height beneath its header, above the status bar.
    fn inspector_body_height(&self) -> f32 {
        let status = if self.settings.show_status_bar {
            super::account_ui::STATUS_BAR_HEIGHT
        } else {
            0.0
        };
        self.window_height - design::HEADER_HEIGHT - design::BORDER_WIDTH - status
    }

    fn workspace_open(&self) -> bool {
        self.workspace_alone() || self.context_sections.is_open(Section::Workspace)
    }

    /// The workspace section's view while it is expanded. A folder that is
    /// not a Git worktree has only its files.
    pub(super) fn workspace_view_shown(&self) -> Option<WorkspaceView> {
        self.workspace_open().then(|| self.workspace_view())
    }

    pub(super) fn workspace_view(&self) -> WorkspaceView {
        if self.active_folder_scope() {
            WorkspaceView::Files
        } else {
            self.context_sections.workspace_view
        }
    }

    pub(super) fn handle_context_section(&mut self, action: Action) -> Task<Message> {
        let before = self.workspace_view_shown();
        match action {
            // Shown because it is alone, it has nothing to collapse into.
            Action::Toggle(Section::Workspace) if self.workspace_alone() => return Task::none(),
            Action::Toggle(section) => {
                let open = !self.context_sections.is_open(section);
                self.context_sections.set_open(section, open);
                if section == Section::PullRequests && !open {
                    self.close_checks_popup();
                }
            }
            Action::Show(view) => {
                if view == WorkspaceView::SourceControl && self.active_folder_scope() {
                    return Task::none();
                }
                self.context_sections.workspace_view = view;
                self.context_sections.set_open(Section::Workspace, true);
            }
        }
        let save = self.save_settings_task(false);
        // Files read untracked paths source control does not, so a newly
        // shown view reads the worktree again.
        let shown = self.workspace_view_shown();
        let reload = match self.sidebar_state.active_worktree().cloned() {
            Some(key) if shown.is_some() && shown != before => self.reload_source_control(&key),
            _ => Task::none(),
        };
        Task::batch([save, reload])
    }

    /// Expands `section` and opens the inspector, for a command that shows
    /// its result there.
    pub(super) fn reveal_context_section(&mut self, section: Section) {
        self.context_sections.set_open(section, true);
        self.context_inspector_open = true;
    }

    /// The inspector's sections beneath its header.
    pub(super) fn context_sections_view(&self, width: f32) -> Element<'_, Message> {
        let mut panes = Vec::with_capacity(5);
        if self.pull_requests_listed() {
            panes.push(self.pull_requests_pane());
        }
        if let Some(pane) = self.worktrees_pane() {
            panes.push(pane);
        }
        panes.push(self.workspace_pane(width));
        panes.extend(self.language_server_panes());
        let shared = panes
            .iter()
            .any(|pane| pane.body.is_some() && pane.height == Height::Share);
        let lists: Vec<f32> = panes
            .iter()
            .filter_map(|pane| match (&pane.body, pane.height) {
                (Some(_), Height::Content { rows }) => Some(list_height(rows)),
                _ => None,
            })
            .collect();
        // With nothing sharing the height, the first open list grows into
        // what the headers and the other lists leave, rather than scrolling
        // within a few rows above an empty panel.
        let mut lead = (!shared).then(|| {
            lead_list_height(
                self.inspector_body_height(),
                panes.len(),
                lists.first().copied().unwrap_or_default(),
                lists.iter().skip(1).sum(),
            )
        });
        let mut children: Vec<Element<'_, Message>> = Vec::with_capacity(panes.len() * 3 + 1);
        for (index, pane) in panes.into_iter().enumerate() {
            if index > 0 {
                children.push(design::horizontal_rule().into());
            }
            let Pane {
                section,
                title,
                count,
                description,
                controls,
                height,
                body,
            } = pane;
            let open = body.is_some();
            children.push(self.section_header(section, title, count, description, controls, open));
            if let Some(body) = body {
                children.push(match height {
                    Height::Share => widget::container(body)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                    Height::Content { rows } => widget::container(body)
                        .width(Length::Fill)
                        .max_height(lead.take().unwrap_or_else(|| list_height(rows)))
                        .into(),
                });
            }
        }
        // Collapsed sections stay stacked at the top rather than spreading
        // over the height none of them claims.
        if !shared {
            children.push(design::horizontal_rule().into());
            children.push(iced::widget::Space::new().height(Length::Fill).into());
        }
        widget::column::with_children(children)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn section_header<'a>(
        &self,
        section: Section,
        title: Cow<'a, str>,
        count: Option<Count>,
        description: Option<String>,
        controls: Vec<Element<'a, Message>>,
        open: bool,
    ) -> Element<'a, Message> {
        // The workspace section shown because it is alone has nothing to
        // collapse into, so it draws no disclosure.
        let collapsible = !(section == Section::Workspace && self.workspace_alone());
        let mut name = if collapsible {
            format!(
                "{title} section, {}",
                if open { "expanded" } else { "collapsed" }
            )
        } else {
            format!("{title} section")
        };
        let mut label: Vec<Element<'a, Message>> = Vec::with_capacity(3);
        if collapsible {
            label.push(crate::ui_controls::passive_icon(
                if open { Icon::PanDown } else { Icon::PanEnd }.handle(),
                design::COMPACT_ICON_SIZE,
            ));
        }
        label.push(
            design::caption(title)
                .font(design::heading_font())
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
        if let Some((count, style)) = count {
            name.push_str(", ");
            name.push_str(description.as_deref().unwrap_or(&count));
            label.push(
                design::caption(count)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .class(cosmic::theme::Text::Custom(style))
                    .into(),
            );
        }
        let toggle = crate::interaction::hover(
            widget::button::custom(
                widget::container(
                    widget::row::with_children(label)
                        .spacing(space::XS)
                        .align_y(Alignment::Center)
                        .width(Length::Fill),
                )
                .center_y(Length::Fill),
            )
            .name(name)
            .class(crate::ui_controls::control_style(false, None))
            .padding([0.0, space::S])
            .height(design::DENSE_ROW_HEIGHT)
            .width(Length::Fill)
            .on_press_maybe(
                (collapsible && !self.modal_is_open())
                    .then_some(Message::ContextSection(Action::Toggle(section))),
            ),
        );
        let mut row = vec![toggle];
        row.extend(controls);
        widget::container(
            widget::row::with_children(row)
                .spacing(space::XXS)
                .align_y(Alignment::Center)
                .width(Length::Fill),
        )
        .padding([space::XXS, space::XS])
        .width(Length::Fill)
        .into()
    }

    fn pull_requests_pane(&self) -> Pane<'_> {
        let open = self.context_sections.is_open(Section::PullRequests);
        let count = self.pull_request_count();
        Pane {
            section: Section::PullRequests,
            title: "Pull requests".into(),
            count: (count > 0).then(|| (count.to_string(), design::muted_text as _)),
            description: None,
            controls: Vec::new(),
            height: Height::Content {
                rows: LIST_ROWS_SHOWN,
            },
            body: open.then(|| self.pull_request_section()),
        }
    }

    fn worktrees_pane(&self) -> Option<Pane<'_>> {
        let edited = self.edited_worktrees();
        if edited.is_empty() {
            return None;
        }
        let open = self.context_sections.is_open(Section::Worktrees);
        Some(Pane {
            section: Section::Worktrees,
            title: "Worktrees".into(),
            count: Some((edited.len().to_string(), design::muted_text as _)),
            description: Some(if edited.len() == 1 {
                "1 edited".to_owned()
            } else {
                format!("{} edited", edited.len())
            }),
            controls: Vec::new(),
            height: Height::Content {
                rows: LIST_ROWS_SHOWN,
            },
            body: open.then(|| self.edited_worktrees_view(edited)),
        })
    }

    /// The project and worktree Projects lists at `path`.
    fn project_worktree(
        &self,
        path: &std::path::Path,
    ) -> Option<(&sidebar::SidebarProject, &sidebar::SidebarWorktree)> {
        self.sidebar_model.projects.iter().find_map(|project| {
            project
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .map(|worktree| (project, worktree))
        })
    }

    /// Where the edited worktree at `path` stands in this window.
    pub(super) fn worktree_placement(&self, path: &std::path::Path) -> pull_request_ui::Placement {
        let current = self
            .sidebar_state
            .active_worktree()
            .is_some_and(|key| key.canonical_path == path);
        pull_request_ui::Placement {
            openable: self.project_worktree(path).is_some(),
            // Point at the workspace section only while it shows the current
            // worktree; collapsed, the row just marks which one is current.
            current: current.then(|| {
                if self.workspace_view_shown().is_some() {
                    "Shown below"
                } else {
                    "Current"
                }
            }),
        }
    }

    /// Opens the worktree Projects lists at `path`.
    pub(super) fn open_worktree_message(&self, path: &std::path::Path) -> Option<Message> {
        let (project, worktree) = self.project_worktree(path)?;
        Some(Message::Sidebar(SidebarAction::OpenWorktree {
            project_id: project.id,
            worktree_id: worktree.id,
            path: worktree.path.clone(),
        }))
    }

    /// The edited worktrees no pull request row shows, each with its branch,
    /// its uncommitted changes and any pull request Projects knows for it.
    /// Opening one goes to it.
    fn edited_worktrees_view<'a>(
        &'a self,
        edited: Vec<&'a crate::work::ConversationWorktree>,
    ) -> Element<'a, Message> {
        let rows = edited.into_iter().map(|edited| {
            let found = self.project_worktree(&edited.path);
            let placement = self.worktree_placement(&edited.path);
            let indicators = found
                .map(|(_, worktree)| sidebar::worktree_indicators(worktree))
                .unwrap_or_default();
            let label = found.map_or_else(
                || {
                    sidebar::worktree_display_name(
                        &edited.path,
                        edited.branch.as_deref(),
                        edited.branch.is_none(),
                    )
                },
                |(_, worktree)| worktree.label().to_owned(),
            );
            let visual = indicators.pull_request.map(sidebar::pull_request_visual);
            let glyph: Element<'a, Message> = match visual {
                Some((icon, color)) => widget::icon(icon)
                    .size(design::ROW_ICON_SIZE)
                    .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |_| {
                        cosmic::iced::widget::svg::Style { color: Some(color) }
                    })))
                    .into(),
                None => crate::ui_controls::passive_icon(
                    sidebar::worktree_icon(),
                    design::ROW_ICON_SIZE,
                ),
            };
            // A pull request the conversation did not work on, such as one
            // merged before it began, is still worth naming.
            let mut facts: Vec<Element<'a, Message>> = Vec::with_capacity(6);
            if let Some(pull_request) = indicators.pull_request {
                facts.push(design::caption(format!("#{}", pull_request.number)).into());
            }
            facts.extend(pull_request_ui::worktree_facts(edited, placement));
            let lines = widget::column::with_children(vec![
                widget::row::with_children(vec![
                    glyph,
                    design::body(label.clone())
                        .wrapping(iced::widget::text::Wrapping::None)
                        .ellipsize(iced::widget::text::Ellipsize::End(
                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
                widget::container(
                    widget::row::with_children(pull_request_ui::separated(facts))
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
            let button = widget::button::custom(lines)
                .name(format!("Open worktree {label}"))
                .class(crate::ui_controls::control_style(
                    placement.current.is_some(),
                    None,
                ))
                .padding([space::XS, space::S])
                .width(Length::Fill)
                .on_press_maybe(
                    self.open_worktree_message(&edited.path)
                        .filter(|_| !self.modal_is_open()),
                );
            crate::interaction::tooltip(
                crate::interaction::hover(button),
                edited.path.display().to_string(),
            )
        });
        widget::scrollable(
            widget::column::with_children(rows.collect::<Vec<_>>())
                .spacing(space::XXS)
                .padding(design::with_vertical_rail(Padding::from([
                    space::XXS,
                    space::S,
                ])))
                .width(Length::Fill),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .width(Length::Fill)
        .into()
    }

    fn workspace_pane(&self, width: f32) -> Pane<'_> {
        let open = self.workspace_open();
        let view = self.workspace_view();
        let git = !self.active_folder_scope();
        let indicators = git.then(|| self.context_rail_indicators());
        // Uncommitted changes are what source control lists; beside Files the
        // same figure would read as a number of files.
        let (count, description) = match indicators {
            Some(sidebar::WorktreeIndicators {
                dirty: Some(dirty),
                dirty_count: Some(count),
                ..
            }) if view == WorkspaceView::SourceControl => (
                Some((count.to_string(), design::warning_text as _)),
                Some(dirty.to_owned()),
            ),
            _ => (None, None),
        };
        // One view is not a choice: a plain folder draws no switch.
        let controls = if git {
            vec![crate::ui_controls::segmented_controls(
                [WorkspaceView::SourceControl, WorkspaceView::Files]
                    .into_iter()
                    .map(|choice| {
                        crate::ui_controls::icon_button(
                            choice.icon(),
                            format!("Show {}", choice.label().to_lowercase()),
                        )
                        .compact()
                        .selected(open && view == choice)
                        .on_press_maybe(
                            (!self.modal_is_open())
                                .then_some(Message::ContextSection(Action::Show(choice))),
                        )
                        .into()
                    })
                    .collect(),
            )]
        } else {
            Vec::new()
        };
        Pane {
            section: Section::Workspace,
            title: view.label().into(),
            count,
            description,
            controls,
            height: Height::Share,
            body: open.then(|| self.workspace_body(view, width)),
        }
    }

    fn workspace_body(&self, view: WorkspaceView, width: f32) -> Element<'_, Message> {
        if crate::device::is_remote() {
            return self.device_workspace_body(view);
        }
        match view {
            WorkspaceView::Files => sidebar::file_panel(
                &self.sidebar_model,
                &self.sidebar_state,
                width,
                self.modal_is_open(),
            ),
            WorkspaceView::SourceControl => sidebar::git_panel(
                &self.sidebar_model,
                &self.sidebar_state,
                width,
                self.modal_is_open(),
            ),
        }
        .map(Message::Sidebar)
    }

    /// A remote device's worktree opens in its own window rather than being
    /// read through this one.
    fn device_workspace_body(&self, view: WorkspaceView) -> Element<'_, Message> {
        let changes = view == WorkspaceView::SourceControl;
        let panel = if changes {
            device_ui::Panel::Changes
        } else {
            device_ui::Panel::Files
        };
        let path = self.active_workspace.as_ref();
        widget::container(
            widget::column::with_children(vec![
                crate::interaction::tooltip(
                    design::caption(
                        path.and_then(|path| path.file_name())
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "Device".into()),
                    )
                    .width(Length::Fill),
                    path.map(|path| path.display().to_string())
                        .unwrap_or_else(|| {
                            crate::device::selected()
                                .map(|device| device.name.clone())
                                .unwrap_or_default()
                        }),
                ),
                crate::ui_controls::button::secondary(if changes {
                    "Open changes"
                } else {
                    "Open files"
                })
                .icon(if changes {
                    Icon::Diff
                } else {
                    Icon::OpenFolder
                })
                .name(if changes {
                    "Open device changes"
                } else {
                    "Open device files"
                })
                .on_press_maybe((!self.modal_is_open()).then_some(Message::Device(
                    device_ui::Action::OpenIn {
                        path: path.cloned(),
                        panel,
                    },
                )))
                .into(),
            ])
            .spacing(space::M),
        )
        .padding(space::L)
        .width(Length::Fill)
        .into()
    }

    /// The focused editor's Problems and its definition or references
    /// results, when it has them.
    fn language_server_panes(&self) -> Vec<Pane<'_>> {
        let Some((card, lsp)) = self.workspace.focused().and_then(|card| {
            matches!(self.cards.get(&card), Some(CardRuntime::Repository(_)))
                .then(|| self.lsp_context(card))
                .flatten()
                .map(|lsp| (card, lsp))
        }) else {
            return Vec::new();
        };
        let dismiss = || -> Element<'_, Message> {
            crate::ui_controls::icon_button(Icon::Close, "Dismiss results")
                .compact()
                .on_press_maybe((!self.modal_is_open()).then_some(Message::ContextInspector(
                    InspectorAction::DismissLspResults(card),
                )))
                .into()
        };
        let mut panes = Vec::with_capacity(2);
        if inspector::has_problems(lsp) {
            let open = self.context_sections.is_open(Section::Problems);
            let count = lsp.diagnostics.len();
            let style: fn(&cosmic::Theme) -> iced::widget::text::Style =
                match inspector::worst_severity(lsp) {
                    Some(crate::lsp::LspSeverity::Error) => design::destructive_text,
                    Some(crate::lsp::LspSeverity::Warning) => design::warning_text,
                    _ => design::muted_text,
                };
            panes.push(Pane {
                section: Section::Problems,
                title: "Problems".into(),
                count: (count > 0).then(|| (count.to_string(), style)),
                description: None,
                // A request's failure notice lingers until it is dismissed.
                controls: lsp.notice.is_some().then(dismiss).into_iter().collect(),
                height: Height::Share,
                body: open
                    .then(|| inspector::problems_view(card, lsp).map(Message::ContextInspector)),
            });
        }
        if let Some((kind, locations)) = &lsp.locations {
            let open = self.context_sections.is_open(Section::Locations);
            panes.push(Pane {
                section: Section::Locations,
                title: inspector::locations_title(*kind).into(),
                count: Some((locations.len().to_string(), design::muted_text as _)),
                description: None,
                controls: vec![dismiss()],
                height: Height::Share,
                body: open.then(|| {
                    inspector::locations_view(card, *kind, locations).map(Message::ContextInspector)
                }),
            });
        }
        panes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_conversation_did_leads_and_the_worktree_tree_waits_to_be_asked_for() {
        let sections = Sections::default();
        for section in [
            Section::PullRequests,
            Section::Worktrees,
            Section::Problems,
            Section::Locations,
        ] {
            assert!(sections.is_open(section), "{section:?} starts collapsed");
        }
        assert!(!sections.is_open(Section::Workspace));
        assert_eq!(sections.workspace_view, WorkspaceView::SourceControl);
    }

    #[test]
    fn a_leading_list_takes_the_height_nothing_else_needs_and_never_less_than_its_rows() {
        let own = list_height(LIST_ROWS_SHOWN);
        let others = list_height(LIST_ROWS_SHOWN);
        // Three headers in an 870 px panel leave room for more than its
        // usual rows, less the other list's height.
        let grown = lead_list_height(870.0, 3, own, others);
        assert_eq!(
            grown,
            870.0 - 3.0 * (SECTION_HEADER_HEIGHT + design::BORDER_WIDTH) - others
        );
        assert!(grown > own);
        // A panel too short for that keeps the usual rows and scrolls.
        assert_eq!(lead_list_height(300.0, 3, own, others), own);
    }

    #[test]
    fn each_section_toggles_alone() {
        for section in [
            Section::PullRequests,
            Section::Worktrees,
            Section::Workspace,
            Section::Problems,
            Section::Locations,
        ] {
            let before = Sections::default();
            let mut sections = before;
            sections.set_open(section, !before.is_open(section));
            assert_ne!(sections.is_open(section), before.is_open(section));
            sections.set_open(section, before.is_open(section));
            assert_eq!(
                sections, before,
                "toggling {section:?} touched another section"
            );
        }
    }

    #[test]
    fn sections_survive_a_restart_and_older_saves_keep_what_they_had() {
        let mut sections = Sections::default();
        sections.set_open(Section::Workspace, true);
        sections.set_open(Section::PullRequests, false);
        sections.workspace_view = WorkspaceView::Files;
        let saved = serde_json::to_string(&sections).unwrap();
        assert_eq!(serde_json::from_str::<Sections>(&saved).unwrap(), sections);

        // A window saved before a section existed keeps the rest of its state.
        let partial: Sections = serde_json::from_str(r#"{"workspace":true}"#).unwrap();
        assert!(partial.is_open(Section::Workspace));
        assert!(partial.is_open(Section::PullRequests));
    }

    #[test]
    fn a_workspace_section_with_nothing_above_it_is_shown_and_stays_shown() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.context_inspector_open = true;
        app.context_sections.set_open(Section::Workspace, false);
        // Left collapsed, it would be one header over an empty panel.
        assert_eq!(
            app.workspace_view_shown(),
            Some(WorkspaceView::SourceControl)
        );
        let _ = app.update(Message::ContextSection(Action::Toggle(Section::Workspace)));
        assert_eq!(
            app.workspace_view_shown(),
            Some(WorkspaceView::SourceControl)
        );
        assert!(!app.context_sections.is_open(Section::Workspace));
        let _ = app.update(Message::ContextSection(Action::Show(WorkspaceView::Files)));
        assert_eq!(app.workspace_view_shown(), Some(WorkspaceView::Files));
    }

    #[test]
    fn revealing_a_result_opens_its_section_and_the_inspector() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.context_sections.set_open(Section::Problems, false);
        app.reveal_context_section(Section::Problems);
        assert!(app.context_inspector_open);
        assert!(app.context_sections.is_open(Section::Problems));
    }
}
