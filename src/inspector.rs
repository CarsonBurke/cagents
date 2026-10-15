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
    design::{self, InspectorLayout, space},
    location::SanitizedLocation,
    lsp::{
        LocationKind, LspCompletionItem, LspCompletionKind, LspCoordinates, LspDiagnostic,
        LspLocation, LspPosition, LspSeverity, LspStatus, LspTarget,
    },
    model::{
        BrowserRestoreFidelity, CardId, CardKind, CardState, ProjectId, Residency,
        TerminalContinuity,
    },
    repository_widget::RepositoryInspectorContext,
    sidebar::{SidebarProject, SidebarWorktree},
    ui_controls::Icon,
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
        let mut children = layout.children();
        let (background_layout, foreground_layout) = (children.next()?, children.next()?);
        let (background_tree, foreground_tree) = tree.children.split_at_mut(1);
        // The background is painted, so whatever it raises has to be painted
        // too: dropping its overlay drew a card's dropdown as a closed button
        // with no menu. It stays inert, because this layer withholds events and
        // operations from the background and an overlay that took them would
        // hand a frozen subtree the clicks aimed at the modal. The foreground's
        // overlay is grouped last so it composites above it.
        let background = self
            .background
            .as_widget_mut()
            .overlay(
                &mut background_tree[0],
                background_layout,
                renderer,
                viewport,
                translation,
            )
            .map(|overlay| overlay::Element::new(Box::new(Inert { overlay })));
        let foreground = self.foreground.as_widget_mut().overlay(
            &mut foreground_tree[0],
            foreground_layout,
            renderer,
            viewport,
            translation,
        );
        match (background, foreground) {
            (Some(background), Some(foreground)) => {
                Some(overlay::Group::with_children(vec![background, foreground]).overlay())
            }
            (background, foreground) => background.or(foreground),
        }
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

/// An overlay that is drawn but never operated.
///
/// [`ModalOverlay`] paints its background while withholding every event and
/// operation from it. This wrapper extends that same split to the overlays the
/// background raises, so a control that was already open keeps its painted
/// shape without stealing input from the modal in front of it.
struct Inert<'a, Message> {
    overlay: overlay::Element<'a, Message, Theme, Renderer>,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for Inert<'_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        self.overlay.as_overlay_mut().layout(renderer, bounds)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
    ) {
        // The obscured tree is drawn with no cursor, exactly as `Widget::draw`
        // does for the background itself.
        self.overlay
            .as_overlay()
            .draw(renderer, theme, style, layout, mouse::Cursor::Unavailable);
    }

    /// Sorts below any overlay the foreground raises. `overlay::Group` orders
    /// its children by this index with an unstable sort, so equal indices would
    /// leave the compositing order of the two layers to chance.
    fn index(&self) -> f32 {
        self.overlay.as_overlay().index() - 1.0
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
    Attention,
    Suspending,
    Suspended,
    Unavailable,
}

impl RuntimeStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Ready => "Ready",
            Self::Restoring => "Restoring",
            Self::Loading => "Loading",
            Self::Attention => "Needs attention",
            Self::Suspending => "Suspending",
            Self::Suspended => "Suspended",
            Self::Unavailable => "Unavailable",
        }
    }

    #[cfg(feature = "integrated-browser")]
    const fn has_live_runtime(self) -> bool {
        matches!(self, Self::Ready | Self::Loading)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InspectorAction {
    Close,
    ToggleDiagnostics(CardId),
    Resize {
        width: f32,
    },
    ResizeFinished,
    NewTerminalAt(PathBuf),
    Retry(CardId),
    ReloadBrowser(CardId),
    OpenBrowser(Url),
    RefreshRepository(CardId),
    BackToRepositoryFiles(CardId),
    SaveRepositoryFile(CardId),
    OverwriteRepositoryFile(CardId),
    ReloadRepositoryFile(CardId),
    DiscardRepositoryEdits(CardId),
    ShowRepositoryEditor(CardId),
    ShowRepositoryDiff(CardId),
    ShowMarkdownEdit(CardId),
    ShowMarkdownPreview(CardId),
    ShowStagedDiff(CardId),
    ShowUnstagedDiff(CardId),
    ShowInlineDiff(CardId),
    ShowSplitDiff(CardId),
    RetryRepositoryDiff(CardId),
    PreviousRepositoryChange(CardId),
    NextRepositoryChange(CardId),
    CreatePullRequest {
        project_id: ProjectId,
        worktree: PathBuf,
    },
    FocusProjectSearch,
    CloseCard(CardId),
    /// Move the caret of `card`'s open document. Self-contained: a language
    /// server replaces a document's diagnostics without being asked, so a row
    /// that carried a position in a list the application holds could resolve
    /// against a list that had already been replaced.
    JumpToLspDiagnostic {
        card: CardId,
        at: LspPosition,
    },
    /// Open a file of `card`'s own worktree and move its caret. `coordinates`
    /// says whether `at` is already in editor-native scalars, or is still in
    /// protocol code units and must be converted once the file is loaded.
    JumpToLspLocation {
        card: CardId,
        path: PathBuf,
        at: LspPosition,
        coordinates: LspCoordinates,
    },
    /// Apply `card`'s completion candidate. Applying a candidate needs the
    /// whole item, which the application holds, so this addresses it by
    /// position; `label` is the fingerprint the application compares against
    /// the item it finds before touching the buffer.
    ApplyLspCompletion {
        card: CardId,
        index: usize,
        label: String,
    },
    RequestLspHover(CardId),
    GoToLspDefinition(CardId),
    FindLspReferences(CardId),
    RestartLanguageServer(CardId),
    DismissLspResults(CardId),
    /// A link inside rendered hover Markdown. The application parses it, as it
    /// already does for a Markdown document's links, because a hover link is
    /// not guaranteed to be an absolute URL.
    OpenLspHoverLink(String),
}

/// One labelled line of an inspector section.
///
/// A fact is inert unless it carries an `action`: the language-server sections
/// need rows the reader can press, and only a footer [`Action`] could carry a
/// message before. Every fact built by [`model_for_card`] leaves `action`
/// unset. Internal facts are kept behind the pane diagnostics disclosure.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// Internal state, shown only in the pane diagnostics disclosure.
    diagnostic: bool,
    pub label: Cow<'static, str>,
    pub value: String,
    /// Rendered as Markdown instead of plain selectable text.
    pub markdown: bool,
    /// Pressing the fact's row emits this message.
    pub action: Option<InspectorAction>,
}

impl Fact {
    fn new(label: impl Into<Cow<'static, str>>, value: impl Into<String>) -> Self {
        Self {
            diagnostic: false,
            label: label.into(),
            value: value.into(),
            markdown: false,
            action: None,
        }
    }

    fn diagnostic(mut self) -> Self {
        self.diagnostic = true;
        self
    }

    fn markdown(mut self) -> Self {
        self.markdown = true;
        self
    }

    fn activated(mut self, action: InspectorAction) -> Self {
        self.action = Some(action);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    pub label: Cow<'static, str>,
    pub icon: Icon,
    pub message: InspectorAction,
    pub primary: bool,
}

impl Action {
    fn new(label: impl Into<Cow<'static, str>>, icon: Icon, message: InspectorAction) -> Self {
        Self {
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
    pub sections: Vec<Section>,
    pub actions: Vec<Action>,
}

/// Known chat payload owners, not process RSS or a complete heap estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatMemory {
    pub source_bytes: usize,
    pub presentation_bytes: usize,
    pub selection_bytes: usize,
    pub prepared_items: usize,
}

pub fn with_chat_memory(mut model: InspectorModel, memory: ChatMemory) -> InspectorModel {
    if let Some(section) = model.sections.iter_mut().find(|section| section.title == "Chat") {
        section.facts.extend([
            Fact::new("Loaded transcript payload", format!("{} bytes", memory.source_bytes)).diagnostic(),
            Fact::new("Prepared presentation payload", format!("{} bytes", memory.presentation_bytes)).diagnostic(),
            Fact::new("Logical selection payload", format!("{} bytes", memory.selection_bytes)).diagnostic(),
            Fact::new("Prepared transcript items", memory.prepared_items.to_string()).diagnostic(),
            Fact::new("Memory accounting", "Known payload only. Shared bodies, parser/font/renderer allocations and images are not a complete heap or RSS estimate.").diagnostic(),
        ]);
    }
    model
}

pub fn model_for_card(
    card: &CardState,
    runtime: RuntimeStatus,
    projects: &[SidebarProject],
    repository_runtime: Option<&RepositoryInspectorContext>,
) -> InspectorModel {
    let kind = match &card.kind {
        CardKind::Terminal(_) => "Terminal",
        CardKind::Chat(_) => "Chat",
        CardKind::Browser(_) => "Browser",
        CardKind::PullRequests(_) => "Pull requests",
        CardKind::Repository(_) => "Repository",
        CardKind::Settings => "Settings",
        CardKind::Accounts => "Accounts",
        CardKind::Automations => "Automations",
    };
    let mut sections = Vec::with_capacity(4);
    let mut actions = Vec::new();

    let repository_context = repository_context(card, projects);
    match &card.kind {
        CardKind::Terminal(session) => {
            sections.push(Section::new(
                "Terminal",
                vec![
                    Fact::new("Command", command_label(&session.launch)),
                    Fact::new(
                        "Working directory",
                        session.launch.working_directory.display().to_string(),
                    ),
                    Fact::new("Continuity", terminal_continuity_label(&session.continuity))
                        .diagnostic(),
                    Fact::new(
                        "Transcript",
                        session.transcript_path.as_ref().map_or_else(
                            || "Not recorded".into(),
                            |path| path.display().to_string(),
                        ),
                    )
                    .diagnostic(),
                    Fact::new("Session ID", session.id.to_string()).diagnostic(),
                ],
            ));
            actions.push(
                Action::new(
                    "New terminal here",
                    Icon::Terminal,
                    InspectorAction::NewTerminalAt(session.launch.working_directory.clone()),
                )
                .primary(),
            );
        }
        CardKind::Chat(session) => {
            sections.push(Section::new(
                "Chat",
                vec![
                    Fact::new("Provider", session.launch.provider.label()),
                    Fact::new(
                        "Conversation ID",
                        session
                            .launch
                            .session_id
                            .as_deref()
                            .and_then(crate::workspace::normalize_card_name)
                            .unwrap_or_else(|| "Not established".into()),
                    )
                    .diagnostic(),
                    Fact::new(
                        "Working directory",
                        crate::workspace::normalize_card_name(
                            &session
                                .launch
                                .recipe
                                .working_directory
                                .display()
                                .to_string(),
                        )
                        .unwrap_or_else(|| "Unavailable".into()),
                    ),
                    Fact::new("Session ID", session.id.to_string()).diagnostic(),
                ],
            ));
            actions.push(Action::new(
                "New terminal here",
                Icon::Terminal,
                InspectorAction::NewTerminalAt(session.launch.recipe.working_directory.clone()),
            ));
        }
        CardKind::Browser(session) => {
            let presented_location = SanitizedLocation::from(&session.current_url);
            sections.push(Section::new(
                "Browser",
                vec![
                    Fact::new("Browser ID", session.id.to_string()).diagnostic(),
                    Fact::new("URL", presented_location.into_inner()),
                    Fact::new(
                        "History",
                        format!(
                            "{} of {}",
                            session.history_index.saturating_add(1),
                            session.history.len()
                        ),
                    ),
                    Fact::new("Restore", browser_fidelity_label(session.fidelity)).diagnostic(),
                ],
            ));
            #[cfg(feature = "integrated-browser")]
            if runtime.has_live_runtime() {
                actions.push(
                    Action::new(
                        "Reload",
                        Icon::Refresh,
                        InspectorAction::ReloadBrowser(card.id),
                    )
                    .primary(),
                );
            }
            #[cfg(not(feature = "integrated-browser"))]
            actions.push(
                Action::new(
                    "Open in system browser",
                    Icon::Browser,
                    InspectorAction::Retry(card.id),
                )
                .primary(),
            );
            actions.push(Action::new(
                "Open URL",
                Icon::Browser,
                InspectorAction::OpenBrowser(session.current_url.clone()),
            ));
        }
        CardKind::Repository(view) => {
            sections.push(Section::new(
                "Repository",
                vec![
                    Fact::new("Worktree", view.worktree.display().to_string()),
                    Fact::new("Git directory", view.repository.display().to_string()).diagnostic(),
                ],
            ));
            actions.push(
                Action::new(
                    "New terminal here",
                    Icon::Terminal,
                    InspectorAction::NewTerminalAt(view.worktree.clone()),
                )
                .primary(),
            );
            actions.push(Action::new(
                "Refresh files",
                Icon::Refresh,
                InspectorAction::RefreshRepository(card.id),
            ));
            if let Some(source) = repository_runtime {
                let mut facts = vec![
                    Fact::new("Surface", source.surface),
                    Fact::new("Files", source.file_list),
                ];
                if let Some(count) = source.file_count {
                    facts.push(Fact::new("File count", count.to_string()));
                }
                if let Some(error) = &source.file_error {
                    facts.push(Fact::new("File-list error", error));
                }
                if let Some(path) = &source.selected_path {
                    facts.push(Fact::new("Selected file", path.display().to_string()));
                    facts.push(Fact::new(
                        "File status",
                        source.selected_status.as_deref().unwrap_or("Clean"),
                    ));
                    facts.push(Fact::new("Document", source.document).diagnostic());
                }
                if let Some(error) = &source.document_error {
                    facts.push(Fact::new("Document error", error));
                }
                if let Some(mode) = source.markdown_mode {
                    facts.push(Fact::new("Markdown", mode));
                }
                if let Some(basis) = source.diff_basis {
                    facts.push(Fact::new("Diff basis", basis).diagnostic());
                }
                if let Some(mode) = source.diff_mode {
                    facts.push(Fact::new("Diff layout", mode).diagnostic());
                }
                if let Some(state) = source.diff_state {
                    facts.push(Fact::new("Diff", state).diagnostic());
                }
                if let Some(error) = &source.diff_error {
                    facts.push(Fact::new("Diff error", error));
                }
                let counts = crate::repository::line_change_counts(
                    source.diff_additions.map(u64::from),
                    source.diff_deletions.map(u64::from),
                );
                if !counts.is_empty() {
                    facts.push(Fact::new("Changes", counts));
                }
                if let Some((current, total)) = source.diff_change {
                    facts.push(Fact::new("Change", format!("{current} of {total}")));
                }
                sections.push(Section::new("Source", facts));
                if source.can_back {
                    actions.push(Action::new(
                        "Back to files",
                        Icon::ChevronLeft,
                        InspectorAction::BackToRepositoryFiles(card.id),
                    ));
                }
                // Capabilities come from the runtime, including committed-tree
                // comparisons that are valid without local changes.
                if source.surface == "Editor" && source.can_show_diff {
                    actions.push(Action::new(
                        "Show diff",
                        Icon::Diff,
                        InspectorAction::ShowRepositoryDiff(card.id),
                    ));
                } else if source.surface == "Diff" && !source.saving {
                    actions.push(Action::new(
                        "Show editor",
                        Icon::Edit,
                        InspectorAction::ShowRepositoryEditor(card.id),
                    ));
                }
                match source.markdown_mode {
                    Some("Edit") if !source.saving => actions.push(Action::new(
                        "Preview Markdown",
                        Icon::Preview,
                        InspectorAction::ShowMarkdownPreview(card.id),
                    )),
                    Some("Preview") if !source.saving => actions.push(Action::new(
                        "Edit Markdown",
                        Icon::Changes,
                        InspectorAction::ShowMarkdownEdit(card.id),
                    )),
                    _ => {}
                }
                if source.surface == "Diff" {
                    // Each action wears the glyph of the toolbar control that
                    // selects the same basis or layout.
                    let opposite_basis = match source.diff_basis {
                        Some("Staged") if source.can_select_unstaged_diff => Some((
                            "Show unstaged diff",
                            Icon::Changes,
                            InspectorAction::ShowUnstagedDiff(card.id),
                        )),
                        Some("Unstaged") if source.can_select_staged_diff => Some((
                            "Show staged diff",
                            Icon::Select,
                            InspectorAction::ShowStagedDiff(card.id),
                        )),
                        _ => None,
                    };
                    if let Some((label, icon, message)) = opposite_basis {
                        actions.push(Action::new(label, icon, message));
                    }
                    if source.can_change_diff_layout {
                        actions.push(if source.diff_mode == Some("Split") {
                            Action::new(
                                "Show inline diff",
                                Icon::List,
                                InspectorAction::ShowInlineDiff(card.id),
                            )
                        } else {
                            Action::new(
                                "Show split diff",
                                Icon::Diff,
                                InspectorAction::ShowSplitDiff(card.id),
                            )
                        });
                    }
                    if source.can_change_diff_layout && source.diff_change.is_some() {
                        actions.push(Action::new(
                            "Previous change",
                            Icon::ChevronUp,
                            InspectorAction::PreviousRepositoryChange(card.id),
                        ));
                        actions.push(Action::new(
                            "Next change",
                            Icon::ChevronDown,
                            InspectorAction::NextRepositoryChange(card.id),
                        ));
                    }
                    if source.can_retry_diff {
                        actions.push(Action::new(
                            "Retry diff",
                            Icon::Refresh,
                            InspectorAction::RetryRepositoryDiff(card.id),
                        ));
                    }
                }
                if source.can_discard {
                    actions.push(Action::new(
                        "Discard unsaved edits",
                        Icon::RevertFile,
                        InspectorAction::DiscardRepositoryEdits(card.id),
                    ));
                }
                if source.can_reload && !source.saving {
                    actions.push(Action::new(
                        "Reload from disk",
                        Icon::Refresh,
                        InspectorAction::ReloadRepositoryFile(card.id),
                    ));
                }
                if source.conflict && !source.saving {
                    actions.push(
                        Action::new(
                            "Overwrite file",
                            Icon::Save,
                            InspectorAction::OverwriteRepositoryFile(card.id),
                        )
                        .primary(),
                    );
                } else if source.can_save {
                    actions.push(
                        Action::new(
                            "Save file",
                            Icon::Save,
                            InspectorAction::SaveRepositoryFile(card.id),
                        )
                        .primary(),
                    );
                }
            }
            if let Some(project) = projects.iter().find(|project| project.id == view.project) {
                actions.push(Action::new(
                    "Create PR",
                    Icon::PullRequest,
                    InspectorAction::CreatePullRequest {
                        project_id: project.id,
                        worktree: view.worktree.clone(),
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
                    "Search projects",
                    Icon::Search,
                    InspectorAction::FocusProjectSearch,
                )
                .primary(),
            );
        }
        // Utility panes show application state, which the pane itself lays
        // out; the inspector has nothing to add beyond its status.
        CardKind::Settings | CardKind::Accounts | CardKind::Automations => {}
    }

    sections.push(Section::new(
        "Status",
        vec![
            Fact::new("Runtime", runtime.label()),
            Fact::new("Lifecycle", residency_label(&card.residency)).diagnostic(),
        ],
    ));

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
            Action::new("Retry", Icon::Refresh, InspectorAction::Retry(card.id)).primary(),
        );
    }
    actions.push(Action::new(
        "Close card",
        Icon::Close,
        InspectorAction::CloseCard(card.id),
    ));

    InspectorModel {
        card_id: card.id,
        title: card.title.clone(),
        kind,
        sections,
        actions,
    }
}

/// The LSP half of a repository card's inspector, owned by the application
/// because results arrive on the manager's event channel rather than from
/// widget state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LspInspectorContext {
    pub status: Option<LspStatus>,
    pub diagnostics: Vec<LspDiagnostic>,
    pub hover: Option<String>,
    pub locations: Option<(LocationKind, Vec<LspLocation>)>,
    pub completions: Vec<LspCompletionItem>,
    pub notice: Option<String>,
}

/// Hover Markdown is arbitrary server text, so its block count and its nesting
/// are bounded for the same reason the result lists are.
const MARKDOWN_BLOCK_LIMIT: usize = 48;
const MARKDOWN_NESTING_LIMIT: usize = 8;

/// A language server may report thousands of diagnostics or references for one
/// file. The inspector is a narrow column, so each list renders a bounded
/// prefix and states the remainder instead of growing the widget tree with the
/// server's answer.
pub const LSP_ROW_LIMIT: usize = 24;

/// One diagnostic, ready to render.
///
/// Deliberately carries no widget type: diagnostics presentation - file
/// ordering, the severity and origin label, the bounded row count - is a
/// product behaviour shared by every shell that embeds an editor, while the
/// widget a shell builds from it is that shell's own affair.
pub struct DiagnosticRow<'a> {
    /// `Error · line 42 · rustc E0308`, already assembled.
    pub label: String,
    pub message: &'a str,
    /// Where activating the row should place the caret.
    pub at: LspPosition,
    pub severity: LspSeverity,
}

/// [`DiagnosticRow`]s in file order, capped, plus the count not shown.
pub struct DiagnosticRows<'a> {
    pub rows: Vec<DiagnosticRow<'a>>,
    pub withheld: usize,
}

/// Orders `diagnostics` by position, labels each one, and keeps at most
/// `limit`. Pass [`LSP_ROW_LIMIT`] unless the surface is genuinely unbounded.
pub fn diagnostic_rows(diagnostics: &[LspDiagnostic], limit: usize) -> DiagnosticRows<'_> {
    let mut ordered: Vec<&LspDiagnostic> = diagnostics.iter().collect();
    // Servers publish in their own order; a reader wants file order.
    ordered.sort_by_key(|diagnostic| diagnostic.range.start);
    let withheld = ordered.len().saturating_sub(limit);
    let rows = ordered
        .into_iter()
        .take(limit)
        .map(|diagnostic| DiagnosticRow {
            label: diagnostic_label(diagnostic),
            message: diagnostic.message.as_str(),
            at: diagnostic.range.start,
            severity: diagnostic.severity,
        })
        .collect();
    DiagnosticRows { rows, withheld }
}

/// The sentence a shell shows in place of the rows it withheld.
pub fn withheld_diagnostics_label(withheld: usize) -> String {
    withheld_label(withheld, "diagnostic", "diagnostics")
}

/// Appends the language-server sections to a repository card's model.
/// Additive so `model_for_card`'s existing four-argument signature, and its
/// single call site in `app.rs`, stay untouched in this batch.
pub fn with_lsp(
    mut model: InspectorModel,
    card: CardId,
    lsp: &LspInspectorContext,
) -> InspectorModel {
    model.sections.extend(language_server_section(
        lsp.status.as_ref(),
        lsp.notice.as_deref(),
    ));
    model
        .sections
        .extend(diagnostics_section(card, &lsp.diagnostics));
    model
        .sections
        .extend(lsp.hover.as_deref().map(hover_section));
    model.sections.extend(
        lsp.locations
            .as_ref()
            .map(|(kind, locations)| locations_section(card, *kind, locations)),
    );
    model
        .sections
        .extend(completions_section(card, &lsp.completions));

    // Closing the card stays the last thing the footer offers.
    let close = model
        .actions
        .iter()
        .position(|action| matches!(action.message, InspectorAction::CloseCard(_)))
        .unwrap_or(model.actions.len());
    model.actions.splice(close..close, lsp_actions(card, lsp));
    model
}

fn language_server_section(status: Option<&LspStatus>, notice: Option<&str>) -> Option<Section> {
    if status.is_none() && notice.is_none() {
        return None;
    }
    let mut facts = Vec::new();
    if let Some(status) = status {
        facts.push(Fact::new("Status", lsp_status_label(status)));
        match status {
            // Not configuring a server for a file type is a statement about the
            // settings, not a failure of anything.
            LspStatus::Disabled => facts.push(Fact::new(
                "Detail",
                "No language server is configured for this file type.",
            )),
            LspStatus::Unavailable { reason } => facts.push(Fact::new("Reason", reason)),
            LspStatus::Failed {
                reason,
                stderr_tail,
            } => {
                facts.push(Fact::new("Reason", reason));
                // The tail is the only evidence of why the session died, so it
                // is presented rather than swallowed.
                facts.push(Fact::new(
                    "Server output",
                    if stderr_tail.is_empty() {
                        "The server wrote nothing to standard error.".to_owned()
                    } else {
                        stderr_tail.join("\n")
                    },
                ));
            }
            LspStatus::Starting | LspStatus::Ready => {}
        }
    }
    if let Some(notice) = notice {
        facts.push(Fact::new("Notice", notice));
    }
    Some(Section::new("Language server", facts))
}

fn diagnostics_section(card: CardId, diagnostics: &[LspDiagnostic]) -> Option<Section> {
    if diagnostics.is_empty() {
        return None;
    }
    let presented = diagnostic_rows(diagnostics, LSP_ROW_LIMIT);
    let mut facts: Vec<Fact> = presented
        .rows
        .into_iter()
        .map(|row| {
            Fact::new(row.label, row.message)
                .activated(InspectorAction::JumpToLspDiagnostic { card, at: row.at })
        })
        .collect();
    if presented.withheld > 0 {
        facts.push(withheld_fact(
            presented.withheld,
            "diagnostic",
            "diagnostics",
        ));
    }
    Some(Section::new("Diagnostics", facts))
}

fn hover_section(hover: &str) -> Section {
    Section::new(
        "Hover",
        vec![if hover.trim().is_empty() {
            Fact::new("Symbol", "The server returned no description.")
        } else {
            Fact::new("Symbol", hover).markdown()
        }],
    )
}

fn locations_section(card: CardId, kind: LocationKind, locations: &[LspLocation]) -> Section {
    let mut facts = Vec::new();
    if locations.is_empty() {
        facts.push(Fact::new("Result", "The server reported no locations."));
    }
    let withheld = locations.len().saturating_sub(LSP_ROW_LIMIT);
    facts.extend(
        locations
            .iter()
            .take(LSP_ROW_LIMIT)
            .map(|location| location_fact(card, location)),
    );
    if withheld > 0 {
        facts.push(withheld_fact(withheld, "location", "locations"));
    }
    Section::new(
        match kind {
            LocationKind::Definition => "Definition",
            LocationKind::References => "References",
        },
        facts,
    )
}

fn completions_section(card: CardId, completions: &[LspCompletionItem]) -> Option<Section> {
    if completions.is_empty() {
        return None;
    }
    let mut ordered: Vec<(usize, &LspCompletionItem)> = completions.iter().enumerate().collect();
    // The preselected candidate is the one the editor would accept by default,
    // so it leads; the rest keep the server's ranking.
    ordered.sort_by_key(|(_, item)| !item.preselect);
    let withheld = ordered.len().saturating_sub(LSP_ROW_LIMIT);
    let mut facts: Vec<Fact> = ordered
        .into_iter()
        .take(LSP_ROW_LIMIT)
        .map(|(index, item)| {
            Fact::new(completion_label(item), completion_value(item)).activated(
                InspectorAction::ApplyLspCompletion {
                    card,
                    index,
                    label: item.label.clone(),
                },
            )
        })
        .collect();
    if withheld > 0 {
        facts.push(withheld_fact(withheld, "candidate", "candidates"));
    }
    Some(Section::new("Completions", facts))
}

fn lsp_actions(card: CardId, lsp: &LspInspectorContext) -> Vec<Action> {
    let mut actions = Vec::new();
    if let Some(status) = &lsp.status {
        if status.can_restart() {
            actions.push(Action::new(
                "Restart language server",
                Icon::Refresh,
                InspectorAction::RestartLanguageServer(card),
            ));
        }
        if status.is_ready() {
            actions.push(Action::new(
                "Describe symbol",
                Icon::Info,
                InspectorAction::RequestLspHover(card),
            ));
            actions.push(Action::new(
                "Go to definition",
                Icon::ChevronRight,
                InspectorAction::GoToLspDefinition(card),
            ));
            actions.push(Action::new(
                "Find references",
                Icon::Search,
                InspectorAction::FindLspReferences(card),
            ));
        }
    }
    if lsp.hover.is_some()
        || lsp.locations.is_some()
        || !lsp.completions.is_empty()
        || lsp.notice.is_some()
    {
        actions.push(Action::new(
            "Dismiss results",
            Icon::Close,
            InspectorAction::DismissLspResults(card),
        ));
    }
    actions
}

fn lsp_status_label(status: &LspStatus) -> &'static str {
    match status {
        LspStatus::Disabled => "Not configured",
        LspStatus::Unavailable { .. } => "Unavailable",
        LspStatus::Starting => "Starting",
        LspStatus::Ready => "Ready",
        LspStatus::Failed { .. } => "Failed",
    }
}

fn severity_label(severity: LspSeverity) -> &'static str {
    match severity {
        LspSeverity::Error => "Error",
        LspSeverity::Warning => "Warning",
        LspSeverity::Information => "Information",
        LspSeverity::Hint => "Hint",
    }
}

fn withheld_fact(withheld: usize, singular: &str, plural: &str) -> Fact {
    Fact::new("Withheld", withheld_label(withheld, singular, plural))
}

fn withheld_label(withheld: usize, singular: &str, plural: &str) -> String {
    if withheld == 1 {
        format!("1 further {singular} is not shown.")
    } else {
        format!("{withheld} further {plural} are not shown.")
    }
}

fn diagnostic_label(diagnostic: &LspDiagnostic) -> String {
    let origin = match (&diagnostic.source, &diagnostic.code) {
        (Some(source), Some(code)) => format!(" · {source} {code}"),
        (Some(text), None) | (None, Some(text)) => format!(" · {text}"),
        (None, None) => String::new(),
    };
    format!(
        "{} · line {}{origin}",
        severity_label(diagnostic.severity),
        diagnostic.range.start.line.saturating_add(1)
    )
}

fn location_fact(card: CardId, location: &LspLocation) -> Fact {
    let line = location.range.start.line.saturating_add(1);
    match (&location.target, location.coordinates) {
        (LspTarget::Workspace { path }, LspCoordinates::Scalar) => Fact::new(
            format!(
                "Open · line {line}, column {}",
                location.range.start.character.saturating_add(1)
            ),
            path.display().to_string(),
        )
        .activated(InspectorAction::JumpToLspLocation {
            card,
            path: path.clone(),
            at: location.range.start,
            coordinates: LspCoordinates::Scalar,
        }),
        // Only `character` is still in protocol code units; a line counts the
        // same either way. The row therefore offers the line it does know and
        // says that the column is settled when the file is loaded, rather than
        // claiming a caret this editor cannot yet place.
        (LspTarget::Workspace { path }, LspCoordinates::Utf16) => Fact::new(
            format!("Open · line {line} · column resolved on open"),
            path.display().to_string(),
        )
        .activated(InspectorAction::JumpToLspLocation {
            card,
            path: path.clone(),
            at: location.range.start,
            coordinates: LspCoordinates::Utf16,
        }),
        (LspTarget::External { path }, _) => Fact::new(
            "Outside this worktree",
            format!(
                "{}: this card opens files from its own worktree only.",
                path.display()
            ),
        ),
        (LspTarget::Uri { uri }, _) => Fact::new(
            "Not a file",
            format!("{uri}: this card cannot open a location that is not a file."),
        ),
    }
}

fn completion_label(item: &LspCompletionItem) -> String {
    let mut label = item
        .kind
        .map_or("Candidate", completion_kind_label)
        .to_owned();
    if item.preselect {
        label.push_str(" · preselected");
    }
    if item.deprecated {
        label.push_str(" · deprecated");
    }
    label
}

fn completion_value(item: &LspCompletionItem) -> String {
    match &item.detail {
        Some(detail) => format!("{}\n{detail}", item.label),
        None => item.label.clone(),
    }
}

fn completion_kind_label(kind: LspCompletionKind) -> &'static str {
    match kind {
        LspCompletionKind::Text => "Text",
        LspCompletionKind::Method => "Method",
        LspCompletionKind::Function => "Function",
        LspCompletionKind::Constructor => "Constructor",
        LspCompletionKind::Field => "Field",
        LspCompletionKind::Variable => "Variable",
        LspCompletionKind::Class => "Class",
        LspCompletionKind::Interface => "Interface",
        LspCompletionKind::Module => "Module",
        LspCompletionKind::Property => "Property",
        LspCompletionKind::Unit => "Unit",
        LspCompletionKind::Value => "Value",
        LspCompletionKind::Enum => "Enum",
        LspCompletionKind::Keyword => "Keyword",
        LspCompletionKind::Snippet => "Snippet",
        LspCompletionKind::Color => "Color",
        LspCompletionKind::File => "File",
        LspCompletionKind::Reference => "Reference",
        LspCompletionKind::Folder => "Folder",
        LspCompletionKind::EnumMember => "Enum member",
        LspCompletionKind::Constant => "Constant",
        LspCompletionKind::Struct => "Struct",
        LspCompletionKind::Event => "Event",
        LspCompletionKind::Operator => "Operator",
        LspCompletionKind::TypeParameter => "Type parameter",
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
        CardKind::Chat(session) => projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(move |worktree| (project, worktree))
            })
            .filter(|(_, worktree)| {
                session
                    .launch
                    .recipe
                    .working_directory
                    .starts_with(&worktree.path)
            })
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
        CardKind::Settings | CardKind::Accounts | CardKind::Automations => None,
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
                    "{total} changes ({} staged, {} unstaged, {} untracked, {} conflicted, {} submodule)",
                    dirty.staged,
                    dirty.unstaged,
                    dirty.untracked,
                    dirty.conflicted,
                    dirty.submodule
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
        let mut children = Vec::with_capacity(self.content.len() + 1);
        children.push(design::heading(self.title).into());
        children.extend(self.content);
        widget::column::with_children(children)
            .spacing(space::S)
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
        let buttons: Vec<Element<'static, InspectorAction>> = self
            .actions
            .into_iter()
            .map(|action| -> Element<'static, InspectorAction> {
                // Deliberately unidentified. A `widget::Id::new(string)` makes
                // the node *named*, and `UserInterface::build` empties every
                // named node's state into a thread-local map, restoring it
                // only for the names the next frame still asks for. Children
                // are then claimed by name in place and never repositioned, so
                // an action that goes away leaves its emptied node behind at
                // its old index for whichever button now occupies that slot to
                // draw against - and drawing a button against stateless state
                // aborts the process. This list's membership changes with
                // every editor and diff transition, nothing focuses these
                // buttons by id, and their accessible names come from their
                // labels, so the identity buys nothing and costs a crash.
                widget::button::standard(action.label)
                    .leading_icon(action.icon)
                    .class(crate::sidebar::sidebar_button_style(false))
                    .width(Length::Fill)
                    .on_press(action.message)
                    .into()
            })
            .collect();
        widget::column::with_children(buttons)
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    }
}

/// One fact's row.
///
/// An actionless fact keeps the caption-over-selectable-text shape the
/// inspector has always drawn. A fact that carries a message becomes a
/// pressable text row, the same control the sidebar uses for its own rows,
/// because the editor offers no gutter or overlay to host a language server's
/// results.
fn fact_element(fact: Fact) -> Element<'static, InspectorAction> {
    match fact.action {
        Some(action) => {
            let name = format!("{}: {}", fact.label, fact.value);
            widget::button::custom(
                widget::column::with_children(vec![
                    design::caption(fact.label).into(),
                    fact_body(fact.value, fact.markdown, false),
                ])
                .width(Length::Fill),
            )
            .class(crate::sidebar::sidebar_button_style(false))
            .name(name)
            .padding([0.0, space::XS])
            .width(Length::Fill)
            .on_press(action)
            .into()
        }
        None => widget::column::with_children(vec![
            design::caption(fact.label).into(),
            fact_body(fact.value, fact.markdown, true),
        ])
        .width(Length::Fill)
        .into(),
    }
}

fn fact_body(value: String, markdown: bool, selectable: bool) -> Element<'static, InspectorAction> {
    if markdown {
        markdown_body(&value)
    } else if selectable {
        widget::selectable_text(value)
            .size(design::TEXT_BODY)
            .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                18.0.into(),
            ))
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into()
    } else {
        // Selectable text inside a button consumes the press that would
        // activate the row.
        design::body(value)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into()
    }
}

/// Renders Markdown into owned widgets.
///
/// `markdown::view` returns an element borrowed from the items it renders, and
/// this module's model is consumed by value, so the crate's own item viewer
/// cannot be reused here. The parsing and all inline styling still come from
/// it: `markdown::Text::spans` yields owned, already-styled spans.
/// The most recent source is parsed once; theme-dependent spans are applied
/// when the cached items are rendered.
fn markdown_body(source: &str) -> Element<'static, InspectorAction> {
    let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_BODY);
    let mut rendered = None;
    crate::runtime_state::with_inspector_markdown(source, &mut |items| {
        rendered = Some(markdown_items(items, &settings, 0));
    });
    rendered.expect("the markdown cache renders once")
}

fn markdown_items(
    items: &[widget::markdown::Item],
    settings: &widget::markdown::Settings,
    depth: usize,
) -> Element<'static, InspectorAction> {
    // A server's Markdown chooses its own block count and nesting, and both
    // become the size and the depth of this widget tree.
    if depth >= MARKDOWN_NESTING_LIMIT {
        return crate::design::caption("Markdown nested deeper than this is not shown.").into();
    }
    let blocks = bounded_indexed(items, "Markdown block", "Markdown blocks", |index, item| {
        markdown_item(item, settings, index, depth)
    });
    widget::column::with_children(blocks)
        .spacing(settings.spacing.0)
        .width(Length::Fill)
        .into()
}

/// A prefix of `items` rendered by `render`, with a stated remainder when the
/// server sent more than the inspector will draw. Every fan-out inside a
/// Markdown block is a count the server chose, so each one is bounded.
fn bounded<T>(
    items: &[T],
    singular: &str,
    plural: &str,
    render: impl Fn(&T) -> Element<'static, InspectorAction>,
) -> Vec<Element<'static, InspectorAction>> {
    bounded_indexed(items, singular, plural, |_, item| render(item))
}

fn bounded_indexed<T>(
    items: &[T],
    singular: &str,
    plural: &str,
    render: impl Fn(usize, &T) -> Element<'static, InspectorAction>,
) -> Vec<Element<'static, InspectorAction>> {
    let withheld = items.len().saturating_sub(MARKDOWN_BLOCK_LIMIT);
    let mut rendered: Vec<Element<'static, InspectorAction>> = items
        .iter()
        .take(MARKDOWN_BLOCK_LIMIT)
        .enumerate()
        .map(|(index, item)| render(index, item))
        .collect();
    if withheld > 0 {
        rendered.push(markdown_withheld(withheld, singular, plural));
    }
    rendered
}

fn markdown_withheld(
    withheld: usize,
    singular: &str,
    plural: &str,
) -> Element<'static, InspectorAction> {
    crate::design::caption(withheld_label(withheld, singular, plural)).into()
}

fn markdown_item(
    item: &widget::markdown::Item,
    settings: &widget::markdown::Settings,
    index: usize,
    depth: usize,
) -> Element<'static, InspectorAction> {
    use widget::markdown::{Bullet, HeadingLevel, Item};

    match item {
        Item::Heading(level, text) => widget::container(markdown_text(
            text,
            settings,
            match level {
                HeadingLevel::H1 => settings.h1_size,
                HeadingLevel::H2 => settings.h2_size,
                HeadingLevel::H3 => settings.h3_size,
                HeadingLevel::H4 => settings.h4_size,
                HeadingLevel::H5 => settings.h5_size,
                HeadingLevel::H6 => settings.h6_size,
            },
            None,
        ))
        .padding(cosmic::iced::padding::top(if index > 0 {
            settings.text_size.0 / 2.0
        } else {
            0.0
        }))
        .into(),
        Item::Paragraph(text) => markdown_text(text, settings, settings.text_size, None),
        Item::CodeBlock { lines, .. } => widget::container(
            widget::column::with_children(bounded(lines, "line", "lines", |line| {
                markdown_text(
                    line,
                    settings,
                    settings.code_size,
                    Some(settings.style.code_block_font),
                )
            }))
            .width(Length::Fill),
        )
        .class(theme::Container::Secondary)
        .padding(settings.code_size.0 / 2.0)
        .width(Length::Fill)
        .into(),
        Item::List { start, bullets } => widget::column::with_children(bounded_indexed(
            bullets,
            "item",
            "items",
            |offset, bullet| {
                let (Bullet::Point { items } | Bullet::Task { items, .. }) = bullet;
                let marker = match (start, bullet) {
                    (Some(first), _) => {
                        format!(
                            "{}.",
                            first.saturating_add(u64::try_from(offset).unwrap_or(0))
                        )
                    }
                    (None, Bullet::Task { done: true, .. }) => "☑".to_owned(),
                    (None, Bullet::Task { done: false, .. }) => "☐".to_owned(),
                    (None, Bullet::Point { .. }) => "•".to_owned(),
                };
                widget::row::with_children(vec![
                    widget::text::body(marker).size(settings.text_size).into(),
                    markdown_items(items, settings, depth + 1),
                ])
                .spacing(settings.spacing.0 / 2.0)
                .into()
            },
        ))
        .spacing(settings.spacing.0 / 2.0)
        .width(Length::Fill)
        .into(),
        // The inspector performs no I/O, so an image is named rather than
        // fetched.
        Item::Image { url, title, alt } => widget::column::with_children(vec![
            markdown_text(alt, settings, settings.text_size, None),
            design::caption(if title.is_empty() {
                format!("Image: {url}")
            } else {
                format!("Image: {title} ({url})")
            })
            .into(),
        ])
        .width(Length::Fill)
        .into(),
        Item::Quote(quote) => widget::row::with_children(vec![
            crate::design::vertical_rule().into(),
            markdown_items(quote, settings, depth + 1),
        ])
        .spacing(settings.spacing.0 / 2.0)
        .into(),
        Item::Rule => crate::design::horizontal_rule().into(),
        Item::Table { columns, rows } => {
            let width = columns.len().min(MARKDOWN_BLOCK_LIMIT);
            let mut table = vec![
                widget::row::with_children(
                    columns
                        .iter()
                        .take(width)
                        .map(|column| markdown_items(&column.header, settings, depth + 1))
                        .collect::<Vec<_>>(),
                )
                .spacing(settings.spacing.0)
                .into(),
            ];
            // A row the parser could not pad holds fewer cells than the
            // header; rendering it as it stands would spread those cells over
            // the whole width and misalign every column beside the gap.
            table.extend(bounded(rows, "row", "rows", |row| {
                widget::row::with_children(
                    (0..width)
                        .map(|column| {
                            row.cells.get(column).map_or_else(
                                || design::body("").width(Length::Fill).into(),
                                |cells| markdown_items(cells, settings, depth + 1),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .spacing(settings.spacing.0)
                .into()
            }));
            widget::column::with_children(table)
                .spacing(settings.spacing.0 / 2.0)
                .width(Length::Fill)
                .into()
        }
    }
}

fn markdown_text(
    text: &widget::markdown::Text,
    settings: &widget::markdown::Settings,
    size: cosmic::iced::Pixels,
    font: Option<cosmic::font::Font>,
) -> Element<'static, InspectorAction> {
    let mut rich = cosmic::iced::widget::rich_text(text.spans(settings.style))
        .size(size)
        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
        .width(Length::Fill)
        .on_link_click(InspectorAction::OpenLspHoverLink);
    if let Some(font) = font {
        rich = rich.font(font);
    }
    rich.into()
}

/// `icon` is the card's tab mark, so the heading names the pane as its tab does.
pub fn view(
    model: InspectorModel,
    icon: widget::icon::Handle,
    width: f32,
    diagnostics_open: bool,
) -> Element<'static, InspectorAction> {
    let heading = widget::row::with_children(vec![
        widget::icon(icon).size(design::ROW_ICON_SIZE).into(),
        widget::column::with_children(vec![
            design::heading(model.title)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
            design::caption(model.kind).into(),
        ])
        .width(Length::Fill)
        .into(),
    ])
    .align_y(Alignment::Center)
    .spacing(space::S);

    // Commands stay within reach, before potentially long source and LSP results.
    let mut sections = vec![SegmentedActions::new(model.actions).view()];
    let mut diagnostics = Vec::new();
    for section in model.sections {
        let (internal, visible): (Vec<_>, Vec<_>) =
            section.facts.into_iter().partition(|fact| fact.diagnostic);
        if !visible.is_empty() {
            sections.push(crate::design::horizontal_rule().into());
            sections.push(
                InspectorSection::new(
                    section.title.clone(),
                    visible.into_iter().map(fact_element).collect(),
                )
                .view(),
            );
        }
        if !internal.is_empty() {
            diagnostics.push(
                InspectorSection::new(
                    section.title,
                    internal.into_iter().map(fact_element).collect(),
                )
                .view(),
            );
        }
    }
    if !diagnostics.is_empty() {
        sections.push(crate::design::horizontal_rule().into());
        sections.push(
            widget::button::text("Pane diagnostics")
                .leading_icon(if diagnostics_open {
                    Icon::PanDown
                } else {
                    Icon::PanEnd
                })
                .on_press(InspectorAction::ToggleDiagnostics(model.card_id))
                .width(Length::Fill)
                .into(),
        );
        if diagnostics_open {
            sections.extend(diagnostics);
        }
    }
    let content = widget::scrollable(
        widget::container(
            widget::column::with_children(sections)
                .spacing(space::L)
                .width(Length::Fill),
        )
        .padding(space::M)
        .width(Length::Fill),
    )
    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
        crate::design::scrollbar(),
    ))
    .class(cosmic::style::iced::Scrollable::Minimal)
    .height(Length::Fill);

    widget::container(
        widget::column::with_children(vec![
            widget::container(heading)
                .padding(space::M)
                .width(Length::Fill)
                .into(),
            crate::design::horizontal_rule().into(),
            content.into(),
        ])
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .class(theme::Container::Background)
    .width(Length::Fixed(width.max(0.0)))
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{LspCompletionEdit, LspPosition, LspRange};
    use crate::model::{
        BrowserId, BrowserRestoreFidelity, BrowserSession, Existence, Exposure, LaunchRecipe,
        ProjectId, RepositoryView, TerminalId, TerminalSession, WorktreeId,
    };
    use crate::splitter::{Edge as SplitterEdge, Splitter};
    use cosmic::iced::{
        advanced::{Layout, layout, mouse, widget::Tree},
        id::Id,
    };
    use std::collections::BTreeMap;

    fn terminal_card() -> CardState {
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
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
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
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

    #[test]
    fn chat_identity_facts_strip_display_controls_without_exposing_launch_environment() {
        let mut card = terminal_card();
        let CardKind::Terminal(mut source) = card.kind else {
            unreachable!()
        };
        source.launch.working_directory = "/repo/\u{202e}worktree".into();
        source
            .launch
            .environment
            .insert("API_KEY".into(), "private-secret".into());
        card.kind = CardKind::Chat(crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: source.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: source.launch.clone(),
                session_id: Some("exact\u{202e}\u{1b}-id".into()),
            },
            proof: None,
            terminal: Some(source),
        });
        let model = model_for_card(&card, RuntimeStatus::Ready, &[], None);
        let facts = &model
            .sections
            .iter()
            .find(|section| section.title == "Chat")
            .unwrap()
            .facts;
        assert_eq!(
            facts
                .iter()
                .find(|fact| fact.label == "Conversation ID")
                .unwrap()
                .value,
            "exact-id"
        );
        assert_eq!(
            facts
                .iter()
                .find(|fact| fact.label == "Working directory")
                .unwrap()
                .value,
            "/repo/worktree"
        );
        assert!(
            facts
                .iter()
                .all(|fact| !fact.value.contains("private-secret"))
        );
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

    fn repository_card() -> CardState {
        CardState {
            id: CardId::new(),
            generation: 1,
            title: "Repository".into(),
            width_fraction: 0.7,
            kind: CardKind::Repository(RepositoryView {
                selected_file: None,
                project: ProjectId::new(),
                repository: PathBuf::from("/repo/.git"),
                worktree: PathBuf::from("/repo"),
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
            None,
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

    #[cfg(feature = "integrated-browser")]
    #[test]
    fn reload_is_available_only_while_the_browser_is_materialized() {
        let card = browser_card("https://example.test");
        for status in [
            RuntimeStatus::Starting,
            RuntimeStatus::Restoring,
            RuntimeStatus::Suspending,
            RuntimeStatus::Suspended,
            RuntimeStatus::Unavailable,
        ] {
            let model = model_for_card(&card, status, &[], None);
            assert!(
                !model
                    .actions
                    .iter()
                    .any(|action| matches!(action.message, InspectorAction::ReloadBrowser(_))),
                "{status:?} exposed Reload"
            );
        }
        for status in [RuntimeStatus::Ready, RuntimeStatus::Loading] {
            let model = model_for_card(&card, status, &[], None);
            assert!(
                model
                    .actions
                    .iter()
                    .any(|action| matches!(action.message, InspectorAction::ReloadBrowser(_))),
                "{status:?} hid Reload"
            );
        }
    }

    #[test]
    fn repository_context_targets_its_worktree() {
        let project_id = ProjectId::new();
        let repository = PathBuf::from("/repo");
        let worktree_path = PathBuf::from("/repo/worktrees/feature");
        let worktree = SidebarWorktree::new(
            WorktreeId::new(),
            worktree_path.clone(),
            Some("feature".into()),
            false,
            false,
            Some(crate::repository::DirtyState {
                staged: 1,
                unstaged: 2,
                untracked: 3,
                conflicted: 4,
                submodule: 5,
            }),
            Some(0),
            Some(0),
        );
        let project = SidebarProject::new(
            project_id,
            crate::repository::RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            vec![worktree],
            Vec::new(),
        );
        let card = CardState {
            id: CardId::new(),
            generation: 1,
            title: "feature".into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Repository(RepositoryView {
                selected_file: None,
                project: project_id,
                repository,
                worktree: worktree_path.clone(),
            }),
        };

        let model = model_for_card(&card, RuntimeStatus::Ready, &[project], None);
        assert!(model.actions.iter().any(|action| matches!(
            &action.message,
            InspectorAction::CreatePullRequest { project_id: action_project, worktree }
                if *action_project == project_id && worktree == &worktree_path
        )));
    }

    #[test]
    fn repository_conflict_exposes_recovery_actions() {
        let card = repository_card();
        let source = source_context();
        let model = model_for_card(&card, RuntimeStatus::Attention, &[], Some(&source));
        assert!(model.actions.iter().any(|action| matches!(
            action.message,
            InspectorAction::OverwriteRepositoryFile(id) if id == card.id
        )));
        assert!(model.actions.iter().any(|action| matches!(
            action.message,
            InspectorAction::ReloadRepositoryFile(id) if id == card.id
        )));
        assert!(model.actions.iter().any(|action| matches!(
            action.message,
            InspectorAction::ShowUnstagedDiff(id) if id == card.id
        )));
        assert!(model.actions.iter().any(|action| matches!(
            action.message,
            InspectorAction::RetryRepositoryDiff(id) if id == card.id
        )));
        assert!(
            !model
                .actions
                .iter()
                .any(|action| matches!(action.message, InspectorAction::Retry(_)))
        );
        assert!(
            !model
                .actions
                .iter()
                .any(|action| matches!(action.message, InspectorAction::SaveRepositoryFile(_)))
        );
    }

    #[test]
    fn terminal_model_exposes_identity_context_and_real_actions() {
        let card = terminal_card();
        let model = model_for_card(&card, RuntimeStatus::Ready, &[], None);
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
        let model = model_for_card(&card, RuntimeStatus::Unavailable, &[], None);
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

    /// A leaf whose overlay answers a click, so a test can tell an overlay
    /// that was dropped from one that was raised but held inert.
    struct Talkative;

    struct TalkativeOverlay;

    impl Widget<&'static str, Theme, Renderer> for Talkative {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }
        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.max())
        }
        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }
        fn overlay<'a>(
            &'a mut self,
            _tree: &'a mut Tree,
            _layout: Layout<'a>,
            _renderer: &Renderer,
            _viewport: &Rectangle,
            _translation: cosmic::iced::Vector,
        ) -> Option<overlay::Element<'a, &'static str, Theme, Renderer>> {
            Some(overlay::Element::new(Box::new(TalkativeOverlay)))
        }
    }

    impl overlay::Overlay<&'static str, Theme, Renderer> for TalkativeOverlay {
        fn layout(&mut self, _renderer: &Renderer, _bounds: Size) -> layout::Node {
            layout::Node::new(Size::new(16.0, 16.0))
        }
        fn draw(
            &self,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
        ) {
        }
        fn update(
            &mut self,
            _event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &Renderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, &'static str>,
        ) {
            shell.publish("background overlay took the event");
        }
    }

    #[test]
    fn modal_overlay_paints_the_background_overlay_without_letting_it_take_input() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut element: Element<'_, &'static str> = ModalOverlay::new(
            Element::new(Talkative),
            widget::container(cosmic::iced::widget::Space::new())
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 300.0)),
        );
        let viewport = Rectangle::new(cosmic::iced::Point::ORIGIN, Size::new(400.0, 300.0));
        let mut raised = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                cosmic::iced::Vector::ZERO,
            )
            .expect("the painted background keeps the overlay it raised");
        let laid_out = raised.as_overlay_mut().layout(&renderer, viewport.size());
        assert_eq!(laid_out.size(), Size::new(16.0, 16.0));

        let mut messages = Vec::new();
        raised.as_overlay_mut().update(
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&laid_out),
            mouse::Cursor::Available(cosmic::iced::Point::new(8.0, 8.0)),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
        );
        assert!(
            messages.is_empty(),
            "the obscured background must not answer input aimed at the modal"
        );
    }

    fn source_context() -> RepositoryInspectorContext {
        RepositoryInspectorContext {
            file_list: "Ready",
            file_count: Some(3),
            file_error: None,
            selected_path: Some(PathBuf::from("README.md")),
            selected_status: Some("MM".into()),
            selected_changed: true,
            has_staged: true,
            has_unstaged: true,
            surface: "Diff",
            document: "Save conflict",
            document_error: Some("changed on disk".into()),
            conflict: true,
            markdown_mode: None,
            diff_basis: Some("Staged"),
            diff_mode: Some("Split"),
            diff_state: Some("Unavailable"),
            diff_error: Some("diff failed".into()),
            diff_additions: Some(3),
            diff_deletions: Some(2),
            diff_change: Some((1, 2)),
            dirty: true,
            saving: false,
            can_save: true,
            can_discard: true,
            can_reload: true,
            can_retry_diff: true,
            can_show_diff: true,
            can_change_diff_layout: true,
            can_select_staged_diff: true,
            can_select_unstaged_diff: true,
            can_back: false,
        }
    }

    fn diagnostic(line: u32, severity: LspSeverity, message: &str) -> LspDiagnostic {
        LspDiagnostic {
            range: LspRange::new(LspPosition::new(line, 0), LspPosition::new(line, 4)),
            severity,
            message: message.to_owned(),
            source: None,
            code: None,
        }
    }

    fn location(target: LspTarget, coordinates: LspCoordinates) -> LspLocation {
        LspLocation {
            target,
            range: LspRange::new(LspPosition::new(7, 2), LspPosition::new(7, 9)),
            coordinates,
        }
    }

    fn completion(label: &str, preselect: bool, deprecated: bool) -> LspCompletionItem {
        LspCompletionItem {
            label: label.to_owned(),
            kind: Some(LspCompletionKind::Function),
            detail: Some(format!("fn {label}()")),
            documentation: None,
            sort_text: None,
            filter_text: None,
            preselect,
            deprecated,
            edit: LspCompletionEdit {
                range: LspRange::default(),
                replace_range: None,
                new_text: label.to_owned(),
                snippet: None,
            },
            additional_edits: Vec::new(),
        }
    }

    fn lsp_model(card: &CardState, lsp: &LspInspectorContext) -> InspectorModel {
        let source = source_context();
        with_lsp(
            model_for_card(card, RuntimeStatus::Ready, &[], Some(&source)),
            card.id,
            lsp,
        )
    }

    fn section<'a>(model: &'a InspectorModel, title: &str) -> &'a Section {
        model
            .sections
            .iter()
            .find(|section| section.title == title)
            .unwrap_or_else(|| panic!("no {title:?} section in {:?}", model.sections))
    }

    /// Two of the card's four diff bases compare against committed trees, so a
    /// file the branch changed and the worktree left alone still has a diff to
    /// read. The inspector must not withhold what the card itself offers.
    #[test]
    fn an_unchanged_file_with_a_committed_comparison_is_still_offered_a_diff() {
        let card = repository_card();
        let source = RepositoryInspectorContext {
            selected_status: Some("  ".into()),
            selected_changed: false,
            has_staged: false,
            has_unstaged: false,
            surface: "Editor",
            diff_basis: Some("Branch"),
            ..source_context()
        };
        let model = model_for_card(&card, RuntimeStatus::Ready, &[], Some(&source));

        assert!(
            model
                .actions
                .iter()
                .any(|action| action.message == InspectorAction::ShowRepositoryDiff(card.id)),
            "an unchanged file was refused the diff its card offers: {:?}",
            model.actions
        );

        // A save in flight is a different refusal and stays in force: the
        // diff would describe text the write has not finished replacing.
        let saving = RepositoryInspectorContext {
            saving: true,
            can_show_diff: false,
            ..source
        };
        let model = model_for_card(&card, RuntimeStatus::Ready, &[], Some(&saving));

        assert!(
            !model
                .actions
                .iter()
                .any(|action| action.message == InspectorAction::ShowRepositoryDiff(card.id)),
            "a diff was offered while the file was being written: {:?}",
            model.actions
        );
    }

    #[test]
    fn a_quiet_language_server_context_adds_nothing() {
        let card = repository_card();
        let source = source_context();
        let plain = model_for_card(&card, RuntimeStatus::Ready, &[], Some(&source));
        let decorated = with_lsp(plain.clone(), card.id, &LspInspectorContext::default());

        assert_eq!(plain, decorated);
    }

    #[test]
    fn every_fact_a_card_builds_for_itself_stays_inert() {
        let source = source_context();
        for model in [
            model_for_card(&terminal_card(), RuntimeStatus::Ready, &[], None),
            model_for_card(
                &browser_card("https://example.test"),
                RuntimeStatus::Ready,
                &[],
                None,
            ),
            model_for_card(
                &repository_card(),
                RuntimeStatus::Attention,
                &[],
                Some(&source),
            ),
        ] {
            for section in &model.sections {
                for fact in &section.facts {
                    assert_eq!(
                        fact.action, None,
                        "{}/{} gained an action",
                        section.title, fact.label
                    );
                    assert!(
                        !fact.markdown,
                        "{}/{} became Markdown",
                        section.title, fact.label
                    );
                }
            }
        }
    }

    #[test]
    fn an_activatable_fact_carries_exactly_its_own_action() {
        let card = repository_card();
        let lsp = LspInspectorContext {
            diagnostics: vec![
                diagnostic(0, LspSeverity::Error, "first"),
                diagnostic(1, LspSeverity::Warning, "second"),
            ],
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let facts = &section(&model, "Diagnostics").facts;

        assert_eq!(
            facts[0].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card: card.id,
                at: LspPosition::new(0, 0)
            })
        );
        assert_eq!(
            facts[1].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card: card.id,
                at: LspPosition::new(1, 0)
            })
        );
    }

    #[test]
    fn a_disabled_server_offers_no_restart_while_a_failed_one_does() {
        let card = repository_card();
        let disabled = lsp_model(
            &card,
            &LspInspectorContext {
                status: Some(LspStatus::Disabled),
                ..LspInspectorContext::default()
            },
        );
        assert!(
            !disabled
                .actions
                .iter()
                .any(|action| action.message == InspectorAction::RestartLanguageServer(card.id)),
            "a file type with no configured server offered Restart"
        );
        let facts = &section(&disabled, "Language server").facts;
        assert_eq!(facts[0].value, "Not configured");
        assert!(
            facts
                .iter()
                .any(|fact| fact.value.contains("No language server is configured")),
            "Disabled must read as a plain statement: {facts:?}"
        );

        let failed = lsp_model(
            &card,
            &LspInspectorContext {
                status: Some(LspStatus::Failed {
                    reason: "exited".into(),
                    stderr_tail: Vec::new(),
                }),
                ..LspInspectorContext::default()
            },
        );
        assert!(
            failed
                .actions
                .iter()
                .any(|action| action.message == InspectorAction::RestartLanguageServer(card.id)),
            "a failed server hid Restart"
        );
    }

    #[test]
    fn a_failed_server_presents_its_stderr_tail_as_evidence() {
        let card = repository_card();
        let model = lsp_model(
            &card,
            &LspInspectorContext {
                status: Some(LspStatus::Failed {
                    reason: "initialize failed".into(),
                    stderr_tail: vec!["thread panicked".into(), "at src/main.rs:1".into()],
                }),
                ..LspInspectorContext::default()
            },
        );
        let facts = &section(&model, "Language server").facts;

        assert!(
            facts.iter().any(|fact| fact.value == "initialize failed"),
            "the failure reason was swallowed: {facts:?}"
        );
        let tail = facts
            .iter()
            .find(|fact| fact.label == "Server output")
            .unwrap_or_else(|| panic!("no server output in {facts:?}"));
        assert!(tail.value.contains("thread panicked"), "{}", tail.value);
        assert!(tail.value.contains("at src/main.rs:1"), "{}", tail.value);
    }

    #[test]
    fn a_ready_server_offers_the_request_actions_and_a_starting_one_does_not() {
        let card = repository_card();
        let ready = lsp_model(
            &card,
            &LspInspectorContext {
                status: Some(LspStatus::Ready),
                ..LspInspectorContext::default()
            },
        );
        for expected in [
            InspectorAction::RequestLspHover(card.id),
            InspectorAction::GoToLspDefinition(card.id),
            InspectorAction::FindLspReferences(card.id),
        ] {
            assert!(
                ready
                    .actions
                    .iter()
                    .any(|action| action.message == expected),
                "a ready server hid {expected:?}"
            );
        }
        assert_eq!(
            ready.actions.last().map(|action| &action.message),
            Some(&InspectorAction::CloseCard(card.id)),
            "closing the card must stay the last footer action"
        );

        let starting = lsp_model(
            &card,
            &LspInspectorContext {
                status: Some(LspStatus::Starting),
                ..LspInspectorContext::default()
            },
        );
        assert!(
            !starting
                .actions
                .iter()
                .any(|action| action.message == InspectorAction::RequestLspHover(card.id)),
            "a server still initialising offered a request"
        );
    }

    #[test]
    fn a_long_diagnostic_list_is_bounded_and_says_what_it_withheld() {
        let card = repository_card();
        let total = LSP_ROW_LIMIT + 5;
        let lsp = LspInspectorContext {
            diagnostics: (0..total)
                .map(|line| {
                    diagnostic(
                        u32::try_from(line).unwrap(),
                        LspSeverity::Error,
                        &format!("problem {line}"),
                    )
                })
                .collect(),
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let facts = &section(&model, "Diagnostics").facts;

        assert_eq!(
            facts.iter().filter(|fact| fact.action.is_some()).count(),
            LSP_ROW_LIMIT,
            "the rendered rows are not bounded"
        );
        let withheld = facts.last().unwrap();
        assert_eq!(withheld.action, None);
        assert_eq!(withheld.value, "5 further diagnostics are not shown.");
    }

    #[test]
    fn diagnostics_read_in_file_order_and_carry_their_own_position() {
        let card = repository_card();
        let lsp = LspInspectorContext {
            diagnostics: vec![
                diagnostic(9, LspSeverity::Warning, "later"),
                diagnostic(2, LspSeverity::Error, "earlier"),
            ],
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let facts = &section(&model, "Diagnostics").facts;

        assert_eq!(facts[0].value, "earlier");
        assert_eq!(
            facts[0].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card: card.id,
                at: LspPosition::new(2, 0)
            }),
            "a row must carry the position it displays, not a place in a list \
             the server can replace before the press arrives"
        );
        assert_eq!(facts[1].value, "later");
        assert_eq!(
            facts[1].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card: card.id,
                at: LspPosition::new(9, 0)
            })
        );
        assert!(
            facts[0].label.starts_with("Error · line 3"),
            "{:?}",
            facts[0]
        );
    }

    #[test]
    fn a_location_outside_the_worktree_states_a_reason_instead_of_a_jump() {
        let card = repository_card();
        let lsp = LspInspectorContext {
            locations: Some((
                LocationKind::Definition,
                vec![location(
                    LspTarget::External {
                        path: PathBuf::from("/usr/lib/std.rs"),
                    },
                    LspCoordinates::Scalar,
                )],
            )),
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let facts = &section(&model, "Definition").facts;

        assert_eq!(facts[0].action, None, "an unopenable file offered a jump");
        assert!(facts[0].value.contains("/usr/lib/std.rs"), "{:?}", facts[0]);
        assert!(facts[0].value.contains("worktree"), "{:?}", facts[0]);
    }

    #[test]
    fn an_unconverted_location_offers_its_line_without_claiming_a_column() {
        let card = repository_card();
        let lsp = LspInspectorContext {
            locations: Some((
                LocationKind::References,
                vec![
                    location(
                        LspTarget::Workspace {
                            path: PathBuf::from("/repo/src/lib.rs"),
                        },
                        LspCoordinates::Utf16,
                    ),
                    location(
                        LspTarget::Workspace {
                            path: PathBuf::from("/repo/src/main.rs"),
                        },
                        LspCoordinates::Scalar,
                    ),
                    location(
                        LspTarget::Uri {
                            uri: "jdt://contents".into(),
                        },
                        LspCoordinates::Scalar,
                    ),
                ],
            )),
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let facts = &section(&model, "References").facts;

        assert_eq!(
            facts[0].action,
            Some(InspectorAction::JumpToLspLocation {
                card: card.id,
                path: PathBuf::from("/repo/src/lib.rs"),
                at: LspPosition::new(7, 2),
                coordinates: LspCoordinates::Utf16
            }),
            "an unconverted location must still open its file, and must say so"
        );
        assert!(
            facts[0].label.contains("column resolved on open"),
            "an unconverted row must say when its column is settled: {:?}",
            facts[0]
        );
        assert!(
            !facts[0].label.contains("column 3"),
            "an unconverted row must not claim a column this editor cannot place: {:?}",
            facts[0]
        );
        assert!(
            facts[0].label.contains("line 8"),
            "a line counts the same in either coordinate space: {:?}",
            facts[0]
        );
        assert_eq!(
            facts[1].action,
            Some(InspectorAction::JumpToLspLocation {
                card: card.id,
                path: PathBuf::from("/repo/src/main.rs"),
                at: LspPosition::new(7, 2),
                coordinates: LspCoordinates::Scalar
            }),
            "a converted workspace location must stay pressable"
        );
        assert!(
            facts[1].label.contains("column 3"),
            "a converted row must name the column it can honour: {:?}",
            facts[1]
        );
        assert_eq!(facts[2].action, None, "a non-file URI offered a jump");
    }

    #[test]
    fn hover_renders_as_markdown_rather_than_flat_text() {
        let card = repository_card();
        let lsp = LspInspectorContext {
            hover: Some("# Signature\n\n`fn open()` **opens**".into()),
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let fact = section(&model, "Hover").facts[0].clone();

        assert!(fact.markdown, "hover was not marked as Markdown: {fact:?}");
        assert_eq!(fact.action, None);

        let rendered = Tree::new(&fact_element(fact.clone()));
        let markdown = Tree::new(&markdown_body(&fact.value));
        assert_eq!(
            tree_shape(&rendered.children[1]),
            tree_shape(&markdown),
            "the hover body did not render through the Markdown renderer"
        );
        assert_eq!(
            markdown.children.len(),
            2,
            "the Markdown was not split into its heading and paragraph"
        );
        let flat: Element<'static, InspectorAction> = widget::selectable_text(fact.value.clone())
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into();
        assert_ne!(
            rendered.children[1].tag,
            Tree::new(&flat).tag,
            "the hover body rendered as flat text"
        );
    }

    #[test]
    fn completions_lead_with_the_preselected_candidate_and_mark_the_deprecated_one() {
        let card = repository_card();
        let lsp = LspInspectorContext {
            completions: vec![
                completion("legacy", false, true),
                completion("preferred", true, false),
            ],
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);
        let facts = &section(&model, "Completions").facts;

        assert!(facts[0].value.starts_with("preferred"), "{:?}", facts[0]);
        assert_eq!(
            facts[0].action,
            Some(InspectorAction::ApplyLspCompletion {
                card: card.id,
                index: 1,
                label: "preferred".to_owned()
            }),
            "the message must address the application's own unsorted list, and \
             fingerprint the item it expects to find there"
        );
        assert!(facts[0].label.contains("preselected"), "{:?}", facts[0]);
        assert!(facts[0].label.contains("Function"), "{:?}", facts[0]);
        assert!(facts[0].value.contains("fn preferred()"), "{:?}", facts[0]);
        assert!(facts[1].label.contains("deprecated"), "{:?}", facts[1]);
    }

    #[test]
    fn location_and_completion_lists_are_bounded_like_diagnostics() {
        let card = repository_card();
        let extra = 3;
        let lsp = LspInspectorContext {
            locations: Some((
                LocationKind::References,
                (0..LSP_ROW_LIMIT + extra)
                    .map(|index| {
                        location(
                            LspTarget::Workspace {
                                path: PathBuf::from(format!("/repo/src/{index}.rs")),
                            },
                            LspCoordinates::Scalar,
                        )
                    })
                    .collect(),
            )),
            completions: (0..LSP_ROW_LIMIT + extra)
                .map(|index| completion(&format!("candidate{index}"), false, false))
                .collect(),
            ..LspInspectorContext::default()
        };
        let model = lsp_model(&card, &lsp);

        for (title, noun) in [("References", "locations"), ("Completions", "candidates")] {
            let facts = &section(&model, title).facts;
            assert_eq!(
                facts.iter().filter(|fact| fact.action.is_some()).count(),
                LSP_ROW_LIMIT,
                "{title} is not bounded"
            );
            assert_eq!(
                facts.last().unwrap().value,
                format!("{extra} further {noun} are not shown.")
            );
        }
    }

    #[test]
    fn a_notice_reads_as_a_plain_statement_and_can_be_dismissed() {
        let card = repository_card();
        let model = lsp_model(
            &card,
            &LspInspectorContext {
                notice: Some("hover request cancelled".into()),
                ..LspInspectorContext::default()
            },
        );
        let facts = &section(&model, "Language server").facts;

        assert_eq!(facts[0].label, "Notice");
        assert_eq!(facts[0].value, "hover request cancelled");
        assert_eq!(facts[0].action, None);
        assert!(
            model
                .actions
                .iter()
                .any(|action| action.message == InspectorAction::DismissLspResults(card.id)),
            "results the reader can see must be dismissable"
        );
    }

    fn tree_depth(tree: &Tree) -> usize {
        1 + tree.children.iter().map(tree_depth).max().unwrap_or(0)
    }

    /// A widget tree folded into its per-node state tags, so a test can pin the
    /// shape a fact renders as rather than only the kind of its root.
    #[derive(Debug, PartialEq)]
    struct TreeShape(cosmic::iced::advanced::widget::tree::Tag, Vec<TreeShape>);

    fn tree_shape(tree: &Tree) -> TreeShape {
        TreeShape(tree.tag, tree.children.iter().map(tree_shape).collect())
    }

    #[test]
    fn a_long_markdown_hover_is_bounded_and_says_what_it_withheld() {
        let extra = 4;
        let source = (0..MARKDOWN_BLOCK_LIMIT + extra)
            .map(|index| format!("paragraph {index}\n\n"))
            .collect::<String>();
        let tree = Tree::new(&markdown_body(&source));

        assert_eq!(
            tree.children.len(),
            MARKDOWN_BLOCK_LIMIT + 1,
            "the rendered blocks are not bounded"
        );
        assert_eq!(
            withheld_label(extra, "Markdown block", "Markdown blocks"),
            "4 further Markdown blocks are not shown."
        );
        assert_eq!(
            withheld_label(1, "Markdown block", "Markdown blocks"),
            "1 further Markdown block is not shown.",
            "the boundary case must still read as English"
        );
    }

    #[test]
    fn deeply_nested_markdown_stops_at_a_stated_depth() {
        let source = format!("{}quoted\n", "> ".repeat(40));
        let bounded = tree_depth(&Tree::new(&markdown_body(&source)));
        let shallow = tree_depth(&Tree::new(&markdown_body("> quoted\n")));

        assert!(
            bounded <= shallow + 3 * MARKDOWN_NESTING_LIMIT,
            "nesting grew the widget tree to depth {bounded}"
        );
    }

    /// A code block, a list and a table are each one Markdown item, so the
    /// per-item block bound says nothing about the rows inside them.
    #[test]
    fn the_rows_inside_one_markdown_block_are_bounded_too() {
        let over = MARKDOWN_BLOCK_LIMIT + 2;

        let code = format!(
            "```\n{}```\n",
            (0..over)
                .map(|line| format!("line {line}\n"))
                .collect::<String>()
        );
        let rendered = Tree::new(&markdown_body(&code));
        assert_eq!(
            rendered.children[0].children.len(),
            MARKDOWN_BLOCK_LIMIT + 1,
            "a code block's lines are not bounded"
        );

        let list = (0..over)
            .map(|item| format!("- item {item}\n"))
            .collect::<String>();
        let rendered = Tree::new(&markdown_body(&list));
        assert_eq!(
            rendered.children[0].children.len(),
            MARKDOWN_BLOCK_LIMIT + 1,
            "a list's items are not bounded"
        );

        let columns = MARKDOWN_BLOCK_LIMIT + 2;
        let row = format!("|{}|\n", vec!["x"; columns].join("|"));
        let table = format!(
            "|{}|\n|{}|\n{}",
            (0..columns)
                .map(|column| format!("h{column}"))
                .collect::<Vec<_>>()
                .join("|"),
            vec!["-"; columns].join("|"),
            row.repeat(over)
        );
        let rendered = Tree::new(&markdown_body(&table));
        let table = &rendered.children[0];
        assert_eq!(
            table.children.len(),
            MARKDOWN_BLOCK_LIMIT + 2,
            "a table's rows are not bounded"
        );
        for row in &table.children[..=MARKDOWN_BLOCK_LIMIT] {
            assert_eq!(
                row.children.len(),
                MARKDOWN_BLOCK_LIMIT,
                "a table's columns are not bounded, or a row is not padded to its header"
            );
        }
    }
}
