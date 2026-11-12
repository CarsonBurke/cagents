//! The context inspector as vertical, collapsible sections.
//!
//! A channel's conversation is the inspector's subject, so its sections lead
//! with what that conversation produced: the pull requests it worked on or
//! mentioned, then the worktrees it edited. The active worktree's changes and
//! files follow behind one switch, collapsed until asked for, and a focused
//! editor adds what its language server found. Each section says how much it
//! holds in its header, so a collapsed one still reads at a glance.
//!
//! Expanded sections share the panel's height the way a stack of panes does:
//! a list that scrolls on its own takes a share of what is left, and the
//! short worktree list takes only the rows it has. Nothing loads or polls for
//! a section that is collapsed.

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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Height {
    /// A share of the height the other sections leave; the body scrolls.
    Share,
    /// Its own content's height, up to a bound past which it scrolls.
    Content,
}

/// The rows of the worktree list before it scrolls instead of growing.
const WORKTREE_ROWS_SHOWN: f32 = 5.5;

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
    /// Whether the Pull requests section is expanded and has pull requests to
    /// show, which is when the shown pull request is read and refreshed.
    pub(super) fn pull_request_section_open(&self) -> bool {
        self.context_inspector_open
            && self.context_sections.is_open(Section::PullRequests)
            && self.pull_requests_listed()
    }

    /// The workspace section's view while it is expanded. A folder that is
    /// not a Git worktree has only its files.
    pub(super) fn workspace_view_shown(&self) -> Option<WorkspaceView> {
        self.context_sections
            .is_open(Section::Workspace)
            .then(|| self.workspace_view())
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
            Action::Toggle(section) => {
                let open = !self.context_sections.is_open(section);
                self.context_sections.set_open(section, open);
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
                    Height::Content => widget::container(body)
                        .width(Length::Fill)
                        .max_height(design::DENSE_ROW_HEIGHT * 2.0 * WORKTREE_ROWS_SHOWN)
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
        let mut name = format!(
            "{title} section, {}",
            if open { "expanded" } else { "collapsed" }
        );
        let mut label: Vec<Element<'a, Message>> = vec![
            crate::ui_controls::passive_icon(
                if open { Icon::PanDown } else { Icon::PanEnd }.handle(),
                design::COMPACT_ICON_SIZE,
            ),
            design::caption(title)
                .font(design::heading_font())
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        ];
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
                (!self.modal_is_open()).then_some(Message::ContextSection(Action::Toggle(section))),
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
            controls: if open {
                self.pull_request_controls()
            } else {
                Vec::new()
            },
            height: Height::Share,
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
            height: Height::Content,
            body: open.then(|| self.edited_worktrees_view(edited)),
        })
    }

    /// The worktrees the conversation edited, each with its branch, its
    /// uncommitted changes and its pull request. Opening one goes to it.
    fn edited_worktrees_view<'a>(
        &'a self,
        edited: &'a [crate::work::ConversationWorktree],
    ) -> Element<'a, Message> {
        let active = self.sidebar_state.active_worktree();
        let rows = edited.iter().map(|edited| {
            let found = self.sidebar_model.projects.iter().find_map(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == edited.path)
                    .map(|worktree| (project, worktree))
            });
            let current = active.is_some_and(|key| key.canonical_path == edited.path);
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
            let mut facts: Vec<Element<'a, Message>> = Vec::with_capacity(4);
            if let Some(pull_request) = indicators.pull_request {
                facts.push(design::caption(format!("#{}", pull_request.number)).into());
            }
            match indicators.dirty {
                Some(dirty) => facts.push(
                    design::caption(dirty.to_owned())
                        .class(cosmic::theme::Text::Custom(design::warning_text))
                        .into(),
                ),
                None if found.is_some() => facts.push(design::caption("Clean").into()),
                None => facts.push(design::caption("Not open in Projects").into()),
            }
            if current {
                facts.push(design::caption("Shown below").into());
            }
            if edited.by_subagent {
                facts.push(design::caption("by a subagent").into());
            }
            let separated = facts
                .into_iter()
                .enumerate()
                .flat_map(|(index, fact)| {
                    let dot = (index > 0).then(|| design::caption("·").into());
                    dot.into_iter().chain(std::iter::once(fact))
                })
                .collect::<Vec<_>>();
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
                    widget::row::with_children(separated)
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
            let open = found.map(|(project, worktree)| {
                Message::Sidebar(SidebarAction::OpenWorktree {
                    project_id: project.id,
                    worktree_id: worktree.id,
                    path: worktree.path.clone(),
                })
            });
            let button = widget::button::custom(lines)
                .name(format!("Open worktree {label}"))
                .class(crate::ui_controls::control_style(current, None))
                .padding([space::XS, space::S])
                .width(Length::Fill)
                .on_press_maybe(open.filter(|_| !self.modal_is_open()));
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
        let open = self.context_sections.is_open(Section::Workspace);
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
    fn a_collapsed_section_reads_and_polls_nothing() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.context_inspector_open = true;
        app.context_sections.set_open(Section::Workspace, false);
        assert_eq!(app.workspace_view_shown(), None);
        let _ = app.update(Message::ContextSection(Action::Show(WorkspaceView::Files)));
        assert_eq!(app.workspace_view_shown(), Some(WorkspaceView::Files));
        let _ = app.update(Message::ContextSection(Action::Toggle(Section::Workspace)));
        assert_eq!(app.workspace_view_shown(), None);
        assert!(
            !app.pull_request_section_open(),
            "a section with no pull requests to show reads none"
        );
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
