//! Contextual right-inspector presentation.
//!
//! The module consumes durable card state and repository snapshots that are
//! already in memory. It deliberately performs no repository, terminal, or
//! browser I/O while constructing the view.

use std::{borrow::Cow, path::PathBuf};

use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Event, Length, Rectangle, Size,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget::Tree,
        },
    },
    widget,
};

use crate::{
    design::{self, InspectorLayout, space},
    lsp::{
        LocationKind, LspCoordinates, LspDiagnostic, LspLocation, LspPosition, LspSeverity,
        LspStatus, LspTarget,
    },
    model::CardId,
    repository_widget::CompletionTarget,
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

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        if let Some(foreground) = layout.children().nth(1) {
            self.foreground.as_widget().drag_destinations(
                &tree.children[1],
                foreground.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
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
                foreground.with_virtual_offset(layout.virtual_offset()),
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
                foreground.with_virtual_offset(layout.virtual_offset()),
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
                    foreground.with_virtual_offset(layout.virtual_offset()),
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
            background.with_virtual_offset(layout.virtual_offset()),
            mouse::Cursor::Unavailable,
            viewport,
        );
        if let Some(foreground) = children.next() {
            self.foreground.as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                foreground.with_virtual_offset(layout.virtual_offset()),
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
                background_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                viewport,
                translation,
            )
            .map(|overlay| overlay::Element::new(Box::new(Inert { overlay })));
        let foreground = self.foreground.as_widget_mut().overlay(
            &mut foreground_tree[0],
            foreground_layout.with_virtual_offset(layout.virtual_offset()),
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
                self.foreground.as_widget().a11y_nodes(
                    foreground.with_virtual_offset(layout.virtual_offset()),
                    &state.children[1],
                    cursor,
                )
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

#[derive(Debug, Clone, PartialEq)]
pub enum InspectorAction {
    Close,
    Resize {
        width: f32,
    },
    ResizeFinished,
    /// Copy the complete original protocol diagnostic on explicit request.
    CopyLspDiagnostic(LspDiagnostic),
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
    RestartLanguageServer(CardId),
    DismissLspResults(CardId),
}

/// One labelled line of a language-server section.
///
/// A fact is inert unless it carries an `action`, which makes its row
/// pressable: a diagnostic jumps to its position, a location opens its file.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub label: Cow<'static, str>,
    pub value: String,
    /// Pressing the fact's row emits this message.
    pub action: Option<InspectorAction>,
    /// Shared full payload; serialization happens only on explicit copy.
    pub copy_diagnostic: Option<LspDiagnostic>,
}

impl Fact {
    fn new(label: impl Into<Cow<'static, str>>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            action: None,
            copy_diagnostic: None,
        }
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
}

impl Action {
    fn new(label: impl Into<Cow<'static, str>>, icon: Icon, message: InspectorAction) -> Self {
        Self {
            label: label.into(),
            icon,
            message,
        }
    }
}

/// The LSP half of a repository card's inspector, owned by the application
/// because results arrive on the manager's event channel rather than from
/// widget state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LspInspectorContext {
    pub status: Option<LspStatus>,
    pub diagnostics: Vec<LspDiagnostic>,
    pub locations: Option<(LocationKind, Vec<LspLocation>)>,
    pub completions: Vec<CompletionTarget>,
    pub notice: Option<String>,
}

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
    pub message: Cow<'a, str>,
    pub diagnostic: &'a LspDiagnostic,
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
            message: diagnostic.message_preview(),
            diagnostic,
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

/// Whether the focused editor has anything for the Problems section: a
/// diagnostic, a notice, or a language server that could not start or died.
/// A healthy server with nothing to report, like a file type with none
/// configured, draws no section saying so.
pub fn has_problems(lsp: &LspInspectorContext) -> bool {
    !lsp.diagnostics.is_empty()
        || lsp.notice.is_some()
        || lsp.status.as_ref().is_some_and(|status| {
            matches!(
                status,
                LspStatus::Unavailable { .. } | LspStatus::Failed { .. }
            )
        })
}

/// The Problems section's tone: the worst severity reported.
pub fn worst_severity(lsp: &LspInspectorContext) -> Option<LspSeverity> {
    lsp.diagnostics
        .iter()
        .map(|diagnostic| diagnostic.severity)
        .min_by_key(|severity| match severity {
            LspSeverity::Error => 0,
            LspSeverity::Warning => 1,
            LspSeverity::Information => 2,
            LspSeverity::Hint => 3,
        })
}

/// The focused editor's diagnostics in file order, after why its language
/// server is not serving it, when it is not, and how to restart it.
pub fn problems_view(card: CardId, lsp: &LspInspectorContext) -> Element<'static, InspectorAction> {
    let mut rows: Vec<Element<'static, InspectorAction>> = Vec::new();
    let healthy = lsp
        .status
        .as_ref()
        .is_none_or(|status| matches!(status, LspStatus::Starting | LspStatus::Ready));
    if !healthy || lsp.notice.is_some() {
        rows.extend(
            language_server_section(lsp.status.as_ref(), lsp.notice.as_deref())
                .into_iter()
                .flat_map(|section| section.facts)
                .map(fact_element),
        );
    }
    if let Some(restart) = restart_action(card, lsp) {
        rows.push(
            SegmentedActions::new(vec![Action::new(
                "Restart language server",
                Icon::Refresh,
                restart,
            )])
            .view(),
        );
    }
    match diagnostics_section(card, &lsp.diagnostics) {
        Some(section) => rows.extend(section.facts.into_iter().map(fact_element)),
        None if healthy => rows.push(
            design::caption(match lsp.status {
                Some(LspStatus::Starting) => "The language server is starting.",
                _ => "No problems reported in this file.",
            })
            .into(),
        ),
        None => {}
    }
    section_scroll(rows)
}

/// The recovery the Problems section offers when the focused editor's
/// language server has stopped serving it.
pub fn restart_action(card: CardId, lsp: &LspInspectorContext) -> Option<InspectorAction> {
    lsp.status
        .as_ref()
        .is_some_and(LspStatus::can_restart)
        .then_some(InspectorAction::RestartLanguageServer(card))
}

/// The References or Definition section's title for `kind`.
pub fn locations_title(kind: LocationKind) -> &'static str {
    match kind {
        LocationKind::Definition => "Definition",
        LocationKind::References => "References",
    }
}

/// The locations a definition or references request found, each opening its
/// place in the file.
pub fn locations_view(
    card: CardId,
    kind: LocationKind,
    locations: &[LspLocation],
) -> Element<'static, InspectorAction> {
    section_scroll(
        locations_section(card, kind, locations)
            .facts
            .into_iter()
            .map(fact_element)
            .collect(),
    )
}

fn section_scroll(
    rows: Vec<Element<'static, InspectorAction>>,
) -> Element<'static, InspectorAction> {
    widget::scrollable(
        widget::container(
            widget::column::with_children(rows)
                .spacing(space::S)
                .width(Length::Fill),
        )
        .padding(crate::design::with_vertical_rail(
            cosmic::iced::Padding::from([space::XS, space::M]),
        ))
        .width(Length::Fill),
    )
    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
        crate::design::scrollbar(),
    ))
    .class(cosmic::style::iced::Scrollable::Minimal)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
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
            let mut fact = Fact::new(row.label, row.message.into_owned())
                .activated(InspectorAction::JumpToLspDiagnostic { card, at: row.at });
            fact.copy_diagnostic = Some(row.diagnostic.clone());
            fact
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
    Section::new(locations_title(kind), facts)
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
    let origin = diagnostic.origin_preview();
    let origin = if origin.is_empty() {
        String::new()
    } else {
        format!(" · {origin}")
    };
    crate::lsp::diagnostic_preview(&format!(
        "{} · line {}{origin}",
        severity_label(diagnostic.severity),
        diagnostic.range.start.line.saturating_add(1)
    ))
    .into_owned()
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
    let copy = fact.copy_diagnostic;
    let body = match fact.action {
        Some(action) => {
            let name = format!("{}: {}", fact.label, fact.value);
            let name = if copy.is_some() {
                crate::lsp::diagnostic_preview(&name).into_owned()
            } else {
                name
            };
            widget::button::custom(
                widget::column::with_children(vec![
                    design::caption(fact.label).into(),
                    fact_body(fact.value, false),
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
            fact_body(fact.value, true),
        ])
        .width(Length::Fill)
        .into(),
    };
    if let Some(diagnostic) = copy {
        widget::column::with_children(vec![
            body,
            widget::button::text("Copy full diagnostic")
                .on_press(InspectorAction::CopyLspDiagnostic(diagnostic))
                .into(),
        ])
        .width(Length::Fill)
        .into()
    } else {
        body
    }
}

fn fact_body(value: String, selectable: bool) -> Element<'static, InspectorAction> {
    if selectable {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{LspPosition, LspRange};
    use crate::splitter::{Edge as SplitterEdge, Splitter};
    use cosmic::iced::{
        advanced::{Layout, layout, mouse, widget::Tree},
        id::Id,
    };

    #[test]
    fn modal_keeps_only_foreground_native_bounds_and_action_anchors_in_scroll_coordinates() {
        use crate::workspace_widget::forwarding_test::{native_button, native_controls, renderer};
        use cosmic::iced::{Point, Vector};
        use iced_accessibility::accesskit::Rect;

        let renderer = renderer();
        let size = Size::new(100.0, 60.0);
        let mut widget = Element::new(ModalOverlay::new(
            native_button("Background", size),
            native_button("Foreground", size),
        ));
        let mut tree = Tree::new(&widget);
        let node = widget
            .as_widget_mut()
            .layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size))
            .move_to(Point::new(20.0, 30.0));
        let offset = Vector::new(4.0, -20.0);
        assert_eq!(
            native_controls(&mut widget, &mut tree, &node, offset, &renderer),
            vec![(
                "Foreground".to_owned(),
                Rect::new(16.0, 50.0, 116.0, 110.0),
                vec![(
                    "Foreground",
                    offset,
                    Rectangle::new(Point::new(20.0, 30.0), size)
                )],
            )]
        );
    }

    #[test]
    fn modal_forwards_drop_targets_only_from_the_foreground() {
        use crate::workspace_widget::forwarding_test::{leaf, renderer};
        use cosmic::iced::{Point, Vector, advanced::clipboard::DndDestinationRectangles};
        let renderer = renderer();
        let target = Rectangle::new(Point::ORIGIN, Size::new(80.0, 40.0));
        let (background, background_visits) = leaf::<()>(Size::new(100.0, 60.0), vec![(1, target)]);
        let (foreground, foreground_visits) =
            leaf::<()>(Size::new(100.0, 60.0), vec![(2, target), (3, target)]);
        let mut widget = Element::new(ModalOverlay::new(background, foreground));
        let mut tree = Tree::new(&widget);
        let node = widget
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(100.0, 60.0)),
            )
            .move_to(Point::new(20.0, 30.0));
        let offset = Vector::new(4.0, -20.0);
        let mut destinations = DndDestinationRectangles::new();
        widget.as_widget().drag_destinations(
            &tree,
            Layout::new(&node).with_virtual_offset(offset),
            &renderer,
            &mut destinations,
        );
        let actual: Vec<_> = destinations
            .into_native_rectangles()
            .into_iter()
            .map(|destination| {
                (
                    destination.id,
                    destination.rectangle.x,
                    destination.rectangle.y,
                )
            })
            .collect();
        assert_eq!(actual, vec![(3, 20.0, 30.0), (2, 20.0, 30.0)]);
        assert!(background_visits.offsets("drag").is_empty());
        assert_eq!(foreground_visits.offsets("drag"), vec![offset]);
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

    fn diagnostic(line: u32, severity: LspSeverity, message: &str) -> LspDiagnostic {
        LspDiagnostic::new(
            LspRange::new(LspPosition::new(line, 0), LspPosition::new(line, 4)),
            severity,
            message.to_owned(),
            None,
            None,
        )
    }

    fn location(target: LspTarget, coordinates: LspCoordinates) -> LspLocation {
        LspLocation {
            target,
            range: LspRange::new(LspPosition::new(7, 2), LspPosition::new(7, 9)),
            coordinates,
        }
    }

    /// The section a Problems or Locations pane draws `title` from.
    fn lsp_section(card: CardId, lsp: &LspInspectorContext, title: &str) -> Section {
        language_server_section(lsp.status.as_ref(), lsp.notice.as_deref())
            .into_iter()
            .chain(diagnostics_section(card, &lsp.diagnostics))
            .chain(
                lsp.locations
                    .as_ref()
                    .map(|(kind, locations)| locations_section(card, *kind, locations)),
            )
            .find(|section| section.title == title)
            .unwrap_or_else(|| panic!("no {title:?} section for {lsp:?}"))
    }

    #[test]
    fn a_quiet_or_healthy_language_server_has_no_problems_section() {
        assert!(!has_problems(&LspInspectorContext::default()));
        assert!(!has_problems(&LspInspectorContext {
            status: Some(LspStatus::Disabled),
            ..LspInspectorContext::default()
        }));
        for status in [LspStatus::Ready, LspStatus::Starting] {
            assert!(!has_problems(&LspInspectorContext {
                status: Some(status),
                ..LspInspectorContext::default()
            }));
        }
        assert!(has_problems(&LspInspectorContext {
            status: Some(LspStatus::Failed {
                reason: "exited".into(),
                stderr_tail: Vec::new(),
            }),
            ..LspInspectorContext::default()
        }));
    }

    #[test]
    fn the_worst_reported_severity_tones_the_problems_count() {
        let lsp = LspInspectorContext {
            diagnostics: vec![
                diagnostic(0, LspSeverity::Hint, "hint"),
                diagnostic(1, LspSeverity::Warning, "warning"),
                diagnostic(2, LspSeverity::Information, "info"),
            ],
            ..LspInspectorContext::default()
        };
        assert_eq!(worst_severity(&lsp), Some(LspSeverity::Warning));
        assert_eq!(worst_severity(&LspInspectorContext::default()), None);
    }

    #[test]
    fn diagnostic_inspector_bounds_previews_and_retains_shared_full_copy() {
        let card = CardId::new();
        let message = "😀".repeat(crate::lsp::DIAGNOSTIC_PREVIEW_BYTES) + " FULL MESSAGE END";
        let item = LspDiagnostic::new(
            LspRange::default(),
            LspSeverity::Error,
            message.clone(),
            Some("λ".repeat(crate::lsp::DIAGNOSTIC_PREVIEW_BYTES)),
            Some("κ".repeat(crate::lsp::DIAGNOSTIC_PREVIEW_BYTES)),
        );
        let lsp = LspInspectorContext {
            diagnostics: vec![item.clone()],
            ..Default::default()
        };
        let section = lsp_section(card, &lsp, "Diagnostics");
        let fact = &section.facts[0];
        assert!(fact.value.len() <= crate::lsp::DIAGNOSTIC_PREVIEW_BYTES);
        assert!(fact.label.len() <= crate::lsp::DIAGNOSTIC_PREVIEW_BYTES);
        assert!(fact.value.contains("preview truncated"));
        assert!(!fact.value.contains("FULL MESSAGE END"));
        let copy = fact
            .copy_diagnostic
            .as_ref()
            .expect("explicit full-copy control");
        assert!(std::sync::Arc::ptr_eq(&copy.protocol, &item.protocol));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&copy.copy_text()).unwrap()["message"],
            message
        );
        let _view = fact_element(fact.clone());
    }

    #[test]
    fn an_activatable_fact_carries_exactly_its_own_action() {
        let card = CardId::new();
        let lsp = LspInspectorContext {
            diagnostics: vec![
                diagnostic(0, LspSeverity::Error, "first"),
                diagnostic(1, LspSeverity::Warning, "second"),
            ],
            ..LspInspectorContext::default()
        };
        let section = lsp_section(card, &lsp, "Diagnostics");
        let facts = &section.facts;

        assert_eq!(
            facts[0].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card,
                at: LspPosition::new(0, 0)
            })
        );
        assert_eq!(
            facts[1].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card,
                at: LspPosition::new(1, 0)
            })
        );
    }

    #[test]
    fn a_disabled_server_offers_no_restart_while_a_failed_one_does() {
        let card = CardId::new();
        let disabled = LspInspectorContext {
            status: Some(LspStatus::Disabled),
            ..LspInspectorContext::default()
        };
        assert_eq!(
            restart_action(card, &disabled),
            None,
            "a file type with no configured server offered Restart"
        );
        let section = lsp_section(card, &disabled, "Language server");
        let facts = &section.facts;
        assert_eq!(facts[0].value, "Not configured");
        assert!(
            facts
                .iter()
                .any(|fact| fact.value.contains("No language server is configured")),
            "Disabled must read as a plain statement: {facts:?}"
        );

        let failed = LspInspectorContext {
            status: Some(LspStatus::Failed {
                reason: "exited".into(),
                stderr_tail: Vec::new(),
            }),
            ..LspInspectorContext::default()
        };
        assert_eq!(
            restart_action(card, &failed),
            Some(InspectorAction::RestartLanguageServer(card)),
            "a failed server hid Restart"
        );
    }

    #[test]
    fn a_failed_server_presents_its_stderr_tail_as_evidence() {
        let lsp = LspInspectorContext {
            status: Some(LspStatus::Failed {
                reason: "initialize failed".into(),
                stderr_tail: vec!["thread panicked".into(), "at src/main.rs:1".into()],
            }),
            ..LspInspectorContext::default()
        };
        let section = lsp_section(CardId::new(), &lsp, "Language server");
        let facts = &section.facts;

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
    fn a_long_diagnostic_list_is_bounded_and_says_what_it_withheld() {
        let card = CardId::new();
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
        let section = lsp_section(card, &lsp, "Diagnostics");
        let facts = &section.facts;

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
        let card = CardId::new();
        let lsp = LspInspectorContext {
            diagnostics: vec![
                diagnostic(9, LspSeverity::Warning, "later"),
                diagnostic(2, LspSeverity::Error, "earlier"),
            ],
            ..LspInspectorContext::default()
        };
        let section = lsp_section(card, &lsp, "Diagnostics");
        let facts = &section.facts;

        assert_eq!(facts[0].value, "earlier");
        assert_eq!(
            facts[0].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card,
                at: LspPosition::new(2, 0)
            }),
            "a row must carry the position it displays, not a place in a list \
             the server can replace before the press arrives"
        );
        assert_eq!(facts[1].value, "later");
        assert_eq!(
            facts[1].action,
            Some(InspectorAction::JumpToLspDiagnostic {
                card,
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
        let card = CardId::new();
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
        let section = lsp_section(card, &lsp, "Definition");
        let facts = &section.facts;

        assert_eq!(facts[0].action, None, "an unopenable file offered a jump");
        assert!(facts[0].value.contains("/usr/lib/std.rs"), "{:?}", facts[0]);
        assert!(facts[0].value.contains("worktree"), "{:?}", facts[0]);
    }

    #[test]
    fn an_unconverted_location_offers_its_line_without_claiming_a_column() {
        let card = CardId::new();
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
        let section = lsp_section(card, &lsp, "References");
        let facts = &section.facts;

        assert_eq!(
            facts[0].action,
            Some(InspectorAction::JumpToLspLocation {
                card,
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
                card,
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
    fn a_location_list_is_bounded_like_diagnostics() {
        let card = CardId::new();
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
            ..LspInspectorContext::default()
        };
        let section = lsp_section(card, &lsp, "References");
        let facts = &section.facts;
        assert_eq!(
            facts.iter().filter(|fact| fact.action.is_some()).count(),
            LSP_ROW_LIMIT,
            "references are not bounded"
        );
        assert_eq!(
            facts.last().unwrap().value,
            format!("{extra} further locations are not shown.")
        );
    }

    #[test]
    fn a_notice_reads_as_a_plain_statement() {
        let lsp = LspInspectorContext {
            notice: Some("hover request cancelled".into()),
            ..LspInspectorContext::default()
        };
        assert!(
            has_problems(&lsp),
            "a notice must reach the Problems section"
        );
        let section = lsp_section(CardId::new(), &lsp, "Language server");
        let facts = &section.facts;

        assert_eq!(facts[0].label, "Notice");
        assert_eq!(facts[0].value, "hover request cancelled");
        assert_eq!(facts[0].action, None);
    }
}
