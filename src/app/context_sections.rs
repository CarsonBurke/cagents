//! The context inspector as vertical, collapsible sections.
//!
//! A channel's conversation is the inspector's subject, so it leads with what
//! that conversation produced: its Work, each pull request it worked on and
//! each worktree it edited. Beneath it, one switch shows the changes or files
//! of the work chosen there, and its header names that work, so the changes
//! shown never belong to a worktree the reader has to guess at. A focused
//! editor adds what its language server found. Each section says how much it
//! holds in its header, so a collapsed one still reads at a glance.
//!
//! Expanded sections share the panel's height the way a stack of panes does:
//! the worktree's tree takes what is left, and the short lists take only the
//! rows they have. Nothing loads or polls for a section that is collapsed.

use super::*;

/// One of the context inspector's sections, in the order they stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Section {
    /// The pull requests and worktrees the inspected conversation worked on.
    Work,
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
    #[serde(alias = "pull_requests")]
    work: bool,
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
            work: true,
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
            Section::Work => self.work,
            Section::Workspace => self.workspace,
            Section::Problems => self.problems,
            Section::Locations => self.locations,
        }
    }

    pub fn set_open(&mut self, section: Section, open: bool) {
        *match section {
            Section::Work => &mut self.work,
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
    /// Its own content's height, up to this many pixels, past which it
    /// scrolls.
    Content { height: f32 },
}

/// The rows of a short list before it scrolls instead of growing. The half
/// row shows there is more.
const LIST_ROWS_SHOWN: f32 = 5.5;

/// A section header's height: a dense row and its padding.
const SECTION_HEADER_HEIGHT: f32 = design::DENSE_ROW_HEIGHT + 2.0 * space::XXS;

/// The height a short list of two-line rows takes before it scrolls.
fn list_height(rows: f32) -> f32 {
    rows * (design::list::TWO_LINE_HEIGHT + space::XXS)
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
    /// What the section is about, after its title: the work whose changes
    /// or files it shows, and its glyph.
    scope: Option<(widget::icon::Handle, String)>,
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
        !self.work_listed()
    }

    /// An estimate of the sections' height until they have been laid out:
    /// the window beneath the inspector's header, above the status bar.
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
        let work = self.work_summary();
        if self.work_listed() {
            panes.push(self.work_pane(&work));
        }
        panes.push(self.workspace_pane(width, work.scope.clone()));
        panes.extend(self.language_server_panes());
        let shared = panes
            .iter()
            .any(|pane| pane.body.is_some() && pane.height == Height::Share);
        let lists: Vec<f32> = panes
            .iter()
            .filter_map(|pane| match (&pane.body, pane.height) {
                (Some(_), Height::Content { height }) => Some(height),
                _ => None,
            })
            .collect();
        // With nothing sharing the height, the first open list grows into
        // what the headers and the other lists leave, rather than scrolling
        // within a few rows above an empty panel.
        let mut lead = (!shared).then(|| {
            lead_list_height(
                self.context_sections_height
                    .unwrap_or_else(|| self.inspector_body_height()),
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
                scope,
                description,
                controls,
                height,
                body,
            } = pane;
            let open = body.is_some();
            children.push(self.section_header(
                section,
                title,
                count,
                scope,
                description,
                controls,
                open,
            ));
            if let Some(body) = body {
                children.push(match height {
                    Height::Share => widget::container(body)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                    Height::Content { height } => widget::container(body)
                        .width(Length::Fill)
                        .max_height(lead.take().unwrap_or(height))
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
        // The banners beneath the workspace come and go, so the height the
        // sections have is measured rather than derived from the window.
        iced::widget::sensor(
            widget::column::with_children(children)
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_resize(|size| Message::ContextSectionsMeasured(size.height))
        .into()
    }

    fn section_header<'a>(
        &self,
        section: Section,
        title: Cow<'a, str>,
        count: Option<Count>,
        scope: Option<(widget::icon::Handle, String)>,
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
        let mut label: Vec<Element<'a, Message>> = Vec::with_capacity(4);
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
        if let Some((icon, scope)) = scope {
            name.push_str(", for ");
            name.push_str(&scope);
            label.push(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(icon, design::COMPACT_ICON_SIZE),
                    design::caption(scope)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .ellipsize(iced::widget::text::Ellipsize::End(
                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .into(),
                ])
                .spacing(space::XXS)
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .into(),
            );
        }
        // The disclosure sits in the list grid's icon column and the title
        // on its label column, so a section's name lines up with the names
        // of the rows beneath it.
        let disclosure: Element<'a, Message> = if collapsible {
            crate::ui_controls::passive_icon(
                if open { Icon::PanDown } else { Icon::PanEnd }.handle(),
                design::COMPACT_ICON_SIZE,
            )
        } else {
            iced::widget::Space::new().into()
        };
        let toggle = crate::interaction::hover(
            widget::button::custom(
                widget::row::with_children(vec![
                    widget::container(disclosure)
                        .center_x(Length::Fixed(design::list::ICON))
                        .center_y(Length::Fill)
                        .into(),
                    widget::container(
                        widget::row::with_children(label)
                            .spacing(space::XS)
                            .align_y(Alignment::Center)
                            .width(Length::Fill),
                    )
                    .center_y(Length::Fill)
                    .into(),
                ])
                .spacing(design::list::GAP)
                .height(Length::Fill),
            )
            .name(name)
            .class(crate::ui_controls::control_style(false, None))
            .padding([0.0, design::list::INSET])
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
        .padding([space::XXS, design::list::GUTTER])
        .width(Length::Fill)
        .into()
    }

    fn work_pane(&self, work: &pull_request_ui::WorkSummary) -> Pane<'_> {
        let open = self.context_sections.is_open(Section::Work);
        let count = work.count;
        Pane {
            section: Section::Work,
            title: "Work".into(),
            count: (count > 0).then(|| (count.to_string(), design::muted_text as _)),
            scope: None,
            description: None,
            controls: Vec::new(),
            // The list's own height bounds it, so the leading list need not
            // leave room for rows not there. Expanding the shown work makes
            // room for itself rather than pushing the rows after it out of
            // view.
            height: Height::Content {
                height: work
                    .height
                    .content
                    .min(list_height(LIST_ROWS_SHOWN) + work.height.expanded)
                    + 2.0 * pull_request_ui::WORK_LIST_PADDING,
            },
            body: open.then(|| self.work_section(work)),
        }
    }

    fn workspace_pane(&self, width: f32, scope: pull_request_ui::WorkScope) -> Pane<'_> {
        let open = self.workspace_open();
        let view = self.workspace_view();
        let git = !self.active_folder_scope();
        // Work this window cannot read has no changes to count; the panels'
        // figures would be the focused worktree's.
        let readable = matches!(
            scope,
            pull_request_ui::WorkScope::Focus | pull_request_ui::WorkScope::Worktree { .. }
        );
        let indicators = (git && readable).then(|| self.context_rail_indicators());
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
        // Open source control names its branch in its own first row, so the
        // header names the work only where nothing beneath does. A remote
        // device's panels show the device, not this work.
        let named_beneath = open && readable && view == WorkspaceView::SourceControl;
        let header = (!named_beneath && !crate::device::is_remote())
            .then(|| scope.header().map(|(icon, label)| (icon, label.to_owned())))
            .flatten();
        Pane {
            section: Section::Workspace,
            title: view.label().into(),
            count,
            scope: header,
            description,
            controls,
            height: Height::Share,
            body: open.then(|| match scope {
                pull_request_ui::WorkScope::Elsewhere {
                    repository,
                    number,
                    url,
                    ..
                } => self.elsewhere_body(repository, number, url),
                pull_request_ui::WorkScope::Unlisted { label } => self.unlisted_body(label),
                _ => self.workspace_body(view, width),
            }),
        }
    }

    /// A pull request with no worktree here has no changes or files to show
    /// in this window, which says so rather than showing another worktree's.
    fn elsewhere_body(
        &self,
        repository: GitHubRepoKey,
        number: u64,
        url: Option<Url>,
    ) -> Element<'_, Message> {
        let mut actions: Vec<Element<'_, Message>> = Vec::with_capacity(1);
        if let Some(url) = url {
            actions.push(
                crate::ui_controls::button::secondary("Open pull request")
                    .icon(Icon::PopOut.handle())
                    .name(format!("Open #{number}"))
                    .on_press_maybe((!self.modal_is_open()).then(|| {
                        Message::PullRequestUi(pull_request_ui::Action::Open {
                            repository,
                            number,
                            url,
                        })
                    }))
                    .into(),
            );
        }
        let mut lines: Vec<Element<'_, Message>> = vec![
            design::heading(format!("#{number} isn't checked out here"))
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
            design::caption("Its changes show once a worktree in Projects is on its branch.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ];
        lines.extend(actions);
        widget::container(
            widget::column::with_children(lines)
                .spacing(space::S)
                .width(Length::Fill),
        )
        .padding(space::L)
        .width(Length::Fill)
        .into()
    }

    /// A worktree Projects does not list: this window reads only those, so
    /// it says so rather than showing another worktree's changes.
    fn unlisted_body(&self, label: String) -> Element<'_, Message> {
        widget::container(
            widget::column::with_children(vec![
                design::heading(format!("{label} isn't in Projects"))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
                design::caption("Its changes show once its project is open in Projects.")
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(space::S)
            .width(Length::Fill),
        )
        .padding(space::L)
        .width(Length::Fill)
        .into()
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
                scope: None,
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
                scope: None,
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
        for section in [Section::Work, Section::Problems, Section::Locations] {
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
            Section::Work,
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
        sections.set_open(Section::Work, false);
        sections.workspace_view = WorkspaceView::Files;
        let saved = serde_json::to_string(&sections).unwrap();
        assert_eq!(serde_json::from_str::<Sections>(&saved).unwrap(), sections);

        // A window saved before a section existed keeps the rest of its state.
        let partial: Sections = serde_json::from_str(r#"{"workspace":true}"#).unwrap();
        assert!(partial.is_open(Section::Workspace));
        assert!(partial.is_open(Section::Work));
        // A window saved when the section was Pull requests keeps its fold.
        let renamed: Sections = serde_json::from_str(r#"{"pull_requests":false}"#).unwrap();
        assert!(!renamed.is_open(Section::Work));
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
