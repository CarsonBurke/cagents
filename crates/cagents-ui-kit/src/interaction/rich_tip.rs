//! Tooltips with structure: what the control does, the shortcut that does it
//! from the keyboard, and the figures behind it.
//!
//! A tooltip built as one string gives every line the same weight: a shortcut
//! appended with a dash reads as part of the name, and details laid out with
//! blank lines and leading spaces read as one block. [`RichTip`] keeps those
//! parts apart, so the surface can show the shortcut as key caps beside the
//! title and each detail as a label with its value.
//!
//! The tip surface fades in by fading the ink it hands its content, so no text
//! here names a color of its own: secondary text takes a share of the ink it
//! is given, and a toned value takes its tone at the ink's strength.

use std::borrow::Cow;

use cosmic::iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Alignment, Color, Length, Point, Rectangle, Size, mouse};
use cosmic::{Element, Renderer, Theme, widget};

use super::keycap::{KeyChord, key_caps, scaled};
use crate::design::{self, space};

/// What a tooltip says: a label, or a [`RichTip`].
#[derive(Debug, Clone, PartialEq)]
pub enum TipBody<'a> {
    /// One paragraph, wrapped.
    Text(Cow<'a, str>),
    Rich(RichTip<'a>),
}

impl TipBody<'_> {
    /// What the tooltip names: its whole label, or a rich tip's title. This
    /// is what a control that shows the tip is called.
    pub fn title(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Rich(tip) => &tip.title,
        }
    }

    /// The tooltip as plain text, one part to a line.
    pub fn plain(&self) -> String {
        match self {
            Self::Text(text) => text.to_string(),
            Self::Rich(tip) => tip.plain(),
        }
    }

    /// What the tooltip says beyond its title, for a control whose
    /// accessible name is the title: its shortcut, rows and notes, one to a
    /// line. `None` when the title is all it says.
    pub fn description(&self) -> Option<String> {
        let Self::Rich(tip) = self else {
            return None;
        };
        let lines: Vec<String> = tip
            .shortcut
            .iter()
            .map(|chord| format!("Shortcut: {}", chord.label()))
            .chain(tip.rows.iter().map(TipRow::plain))
            .chain(tip.notes.iter().map(|(text, _)| text.to_string()))
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

impl<'a> From<&'a str> for TipBody<'a> {
    fn from(text: &'a str) -> Self {
        Self::Text(Cow::Borrowed(text))
    }
}

impl<'a> From<&'a String> for TipBody<'a> {
    fn from(text: &'a String) -> Self {
        Self::Text(Cow::Borrowed(text))
    }
}

impl From<String> for TipBody<'_> {
    fn from(text: String) -> Self {
        Self::Text(Cow::Owned(text))
    }
}

impl<'a> From<Cow<'a, str>> for TipBody<'a> {
    fn from(text: Cow<'a, str>) -> Self {
        Self::Text(text)
    }
}

impl<'a> From<RichTip<'a>> for TipBody<'a> {
    fn from(tip: RichTip<'a>) -> Self {
        Self::Rich(tip)
    }
}

/// A tooltip's title, the shortcut that runs its control, and detail rows
/// and notes beneath them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RichTip<'a> {
    title: Cow<'a, str>,
    shortcut: Option<KeyChord<'a>>,
    rows: Vec<TipRow<'a>>,
    notes: Vec<(Cow<'a, str>, TipTone)>,
}

impl<'a> RichTip<'a> {
    pub fn new(title: impl Into<Cow<'a, str>>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// The chord that runs the control, such as `Ctrl+M`, shown as key caps
    /// after the title.
    pub fn shortcut(mut self, chord: impl Into<KeyChord<'a>>) -> Self {
        self.shortcut = Some(chord.into());
        self
    }

    /// [`Self::shortcut`] when there is one.
    pub fn shortcut_maybe(mut self, chord: Option<impl Into<KeyChord<'a>>>) -> Self {
        self.shortcut = chord.map(Into::into);
        self
    }

    /// A detail row. Rows share one column of labels, so their values line up.
    pub fn row(mut self, row: TipRow<'a>) -> Self {
        self.rows.push(row);
        self
    }

    pub fn rows(mut self, rows: impl IntoIterator<Item = TipRow<'a>>) -> Self {
        self.rows.extend(rows);
        self
    }

    /// A sentence beneath the rows, in secondary text.
    pub fn note(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.notes.push((text.into(), TipTone::Secondary));
        self
    }

    /// A sentence beneath the rows about something that needs the user,
    /// such as a limit that stops the account.
    pub fn warning(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.notes.push((text.into(), TipTone::Warning));
        self
    }

    /// A sentence beneath the rows that reports a failure.
    pub fn error(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.notes.push((text.into(), TipTone::Error));
        self
    }

    /// A sentence beneath the rows in `tone`.
    pub fn note_in(mut self, text: impl Into<Cow<'a, str>>, tone: TipTone) -> Self {
        self.notes.push((text.into(), tone));
        self
    }

    /// `other`'s rows and notes after this tip's own, for one tooltip that
    /// speaks for several parts under this tip's title. A row or note this
    /// tip already has is not repeated.
    pub fn append(mut self, other: RichTip<'a>) -> Self {
        for row in other.rows {
            if !self.rows.contains(&row) {
                self.rows.push(row);
            }
        }
        for note in other.notes {
            if !self.notes.contains(&note) {
                self.notes.push(note);
            }
        }
        self
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn shortcut_chord(&self) -> Option<&KeyChord<'a>> {
        self.shortcut.as_ref()
    }

    pub fn detail_rows(&self) -> &[TipRow<'a>] {
        &self.rows
    }

    pub fn notes(&self) -> impl Iterator<Item = &str> {
        self.notes.iter().map(|(text, _)| text.as_ref())
    }

    /// The tip as plain text, one part to a line: `Title (Ctrl+M)`, then
    /// `Label: value, detail` for each row, then each note.
    pub fn plain(&self) -> String {
        let mut lines = vec![match &self.shortcut {
            Some(chord) => format!("{} ({})", self.title, chord.label()),
            None => self.title.to_string(),
        }];
        lines.extend(self.rows.iter().map(TipRow::plain));
        lines.extend(self.notes.iter().map(|(text, _)| text.to_string()));
        lines.join("\n")
    }
}

/// One detail of a [`RichTip`]: a label, its value, and an optional
/// qualifier after the value, such as when a figure resets.
#[derive(Debug, Clone, PartialEq)]
pub struct TipRow<'a> {
    label: Cow<'a, str>,
    value: TipValue<'a>,
    detail: Option<Cow<'a, str>>,
    tone: TipTone,
}

#[derive(Debug, Clone, PartialEq)]
enum TipValue<'a> {
    Text(Cow<'a, str>),
    /// A chord, drawn as key caps.
    Shortcut(KeyChord<'a>),
}

/// How strongly a tip's text reads against the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipTone {
    /// The surface's own ink.
    Primary,
    /// Recedes behind the primary text: labels, qualifiers and notes.
    Secondary,
    /// Something that needs the reader, such as an allowance run out.
    Warning,
    /// A failure, such as a check that could not run.
    Error,
}

impl<'a> TipRow<'a> {
    pub fn new(label: impl Into<Cow<'a, str>>, value: impl Into<Cow<'a, str>>) -> Self {
        Self {
            label: label.into(),
            value: TipValue::Text(value.into()),
            detail: None,
            tone: TipTone::Primary,
        }
    }

    /// A row whose value is the chord that runs `label`, such as one of a
    /// control's several commands.
    pub fn shortcut(label: impl Into<Cow<'a, str>>, chord: impl Into<KeyChord<'a>>) -> Self {
        Self {
            label: label.into(),
            value: TipValue::Shortcut(chord.into()),
            detail: None,
            tone: TipTone::Primary,
        }
    }

    pub fn detail(mut self, detail: impl Into<Cow<'a, str>>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn detail_maybe(mut self, detail: Option<impl Into<Cow<'a, str>>>) -> Self {
        self.detail = detail.map(Into::into);
        self
    }

    /// Draws the value as a problem, such as an allowance that has run out.
    pub fn warning(mut self) -> Self {
        self.tone = TipTone::Warning;
        self
    }

    /// Draws the value as secondary text, such as a figure that is no longer
    /// current.
    pub fn secondary(mut self) -> Self {
        self.tone = TipTone::Secondary;
        self
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The value as text: a chord as one line, as a menu prints it.
    pub fn value(&self) -> Cow<'_, str> {
        match &self.value {
            TipValue::Text(text) => Cow::Borrowed(text),
            TipValue::Shortcut(chord) => chord.label(),
        }
    }

    pub fn detail_text(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn tone(&self) -> TipTone {
        self.tone
    }

    fn plain(&self) -> String {
        let mut line = if self.label.is_empty() {
            self.value().into_owned()
        } else {
            format!("{}: {}", self.label, self.value())
        };
        if let Some(detail) = &self.detail {
            line.push_str(", ");
            line.push_str(detail);
        }
        line
    }
}

/// A find bar's controls, the same in every pane: their keys act from the
/// search field.
pub mod find {
    use super::RichTip;

    pub fn previous() -> RichTip<'static> {
        RichTip::new("Previous match").shortcut("Shift+Enter")
    }

    pub fn next() -> RichTip<'static> {
        RichTip::new("Next match").shortcut("Enter")
    }

    pub fn close() -> RichTip<'static> {
        RichTip::new("Close find").shortcut("Escape")
    }
}

/// Secondary text's share of the surface's ink: close to the app's faint
/// text, which a label beside a value needs to recede behind it.
const SECONDARY_INK: f32 = 0.64;
/// Gap between a label column and its values, and between a title and its
/// shortcut.
const COLUMN_GAP: f32 = space::M;
/// Gap between detail rows. Body lines already carry their own leading.
const ROW_GAP: f32 = space::XXS;
/// A detailed tip's widest extent: a plain tip's, plus room for a column of
/// labels beside the values.
const DETAILED_MAX_WIDTH: f32 = super::TIP_MAX_WIDTH + 80.0;

/// The surface of a rich tip.
pub(super) fn surface<'a, Message: 'a>(tip: RichTip<'a>) -> Element<'a, Message> {
    let RichTip {
        title,
        shortcut,
        rows,
        notes,
    } = tip;
    let detailed = !rows.is_empty() || !notes.is_empty();
    // A tip with details names its control in the heading weight, so the
    // title stands apart from the figures under it.
    let title: Element<'a, Message> = if detailed {
        design::heading(title).into()
    } else {
        design::body(title).into()
    };
    let head: Element<'a, Message> = match shortcut {
        Some(chord) => widget::row::with_children(vec![title, key_caps(&chord)])
            .spacing(COLUMN_GAP)
            .align_y(Alignment::Center)
            .into(),
        None => title,
    };
    let mut sections = vec![head];
    if !rows.is_empty() {
        sections.push(table(rows));
    }
    sections.extend(
        notes
            .into_iter()
            .map(|(text, tone)| ink(design::body(text), tone)),
    );
    // A title alone sits as a plain tip does; details take a little more
    // room around them.
    let (padding, max_width) = if detailed {
        (
            [space::XS + space::XXS, space::S + space::XXS],
            DETAILED_MAX_WIDTH,
        )
    } else {
        ([space::XS, space::S], super::TIP_MAX_WIDTH)
    };
    widget::container(
        widget::column::with_children(sections).spacing(if detailed { space::S } else { 0.0 }),
    )
    .class(design::transparent())
    .padding(padding)
    .max_width(max_width)
    .into()
}

fn ink<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    tone: TipTone,
) -> Element<'a, Message> {
    if tone == TipTone::Primary {
        return content.into();
    }
    Element::new(Ink {
        content: content.into(),
        tone,
    })
}

fn table<'a, Message: 'a>(rows: Vec<TipRow<'a>>) -> Element<'a, Message> {
    let mut cells = Vec::with_capacity(rows.len() * COLUMNS);
    for row in rows {
        cells.push(ink(design::body(row.label), TipTone::Secondary));
        cells.push(match row.value {
            TipValue::Text(text) => ink(design::body(text), row.tone),
            TipValue::Shortcut(chord) => key_caps(&chord),
        });
        cells.push(match row.detail {
            Some(detail) => ink(design::body(detail), TipTone::Secondary),
            None => cosmic::iced::widget::Space::new().into(),
        });
    }
    Element::new(Table { cells })
}

/// Draws its content in a share of the ink it is given, or in a tone at that
/// ink's strength. The colors follow the given ink, so they fade in with the
/// tip.
struct Ink<'a, Message> {
    content: Element<'a, Message>,
    tone: TipTone,
}

impl<Message> Widget<Message, Theme, Renderer> for Ink<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        let given = style.text_color;
        let mut style = *style;
        style.text_color = match self.tone {
            TipTone::Primary => given,
            TipTone::Secondary => scaled(given, SECONDARY_INK),
            TipTone::Warning | TipTone::Error => {
                // The given ink is the surface's own at the tip's opacity.
                let full: Color = theme.current_container().component.on.into();
                let strength = if full.a > 0.0 {
                    (given.a / full.a).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let tone = if self.tone == TipTone::Error {
                    design::destructive_color(theme)
                } else {
                    design::warning_color(theme)
                };
                scaled(tone, strength)
            }
        };
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            &style,
            layout,
            cursor,
            viewport,
        );
    }
}

/// The columns of a detail row: its label, its value and its qualifier.
const COLUMNS: usize = 3;

/// Detail rows as columns: labels as wide as the widest of them, values
/// likewise, and qualifiers in what is left, so every value and every
/// qualifier starts at one edge.
struct Table<'a, Message> {
    /// Each row's label, value and qualifier, in turn.
    cells: Vec<Element<'a, Message>>,
}

impl<Message> Widget<Message, Theme, Renderer> for Table<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.cells.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.cells);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let max = limits.max();
        let loose = layout::Limits::new(Size::ZERO, max);
        // Labels and values keep their natural widths; a qualifier wraps in
        // what they leave.
        let mut nodes: Vec<layout::Node> = Vec::with_capacity(self.cells.len());
        let mut widths = [0.0_f32; COLUMNS];
        for (index, (cell, tree)) in self.cells.iter_mut().zip(&mut tree.children).enumerate() {
            let column = index % COLUMNS;
            if column == COLUMNS - 1 {
                nodes.push(layout::Node::default());
                continue;
            }
            let node = cell.as_widget_mut().layout(tree, renderer, &loose);
            widths[column] = widths[column].max(node.size().width);
            nodes.push(node);
        }
        // An empty column takes no gap.
        let mut starts = [0.0_f32; COLUMNS];
        for column in 1..COLUMNS {
            let previous = widths[column - 1];
            starts[column] = starts[column - 1]
                + if previous > 0.0 {
                    previous + COLUMN_GAP
                } else {
                    0.0
                };
        }
        let detail_limits = layout::Limits::new(
            Size::ZERO,
            Size::new((max.width - starts[COLUMNS - 1]).max(0.0), max.height),
        );
        let mut width: f32 = 0.0;
        let mut y = 0.0;
        for (row, (cells, trees)) in self
            .cells
            .chunks_mut(COLUMNS)
            .zip(tree.children.chunks_mut(COLUMNS))
            .enumerate()
        {
            let first = row * COLUMNS;
            nodes[first + COLUMNS - 1] = cells[COLUMNS - 1].as_widget_mut().layout(
                &mut trees[COLUMNS - 1],
                renderer,
                &detail_limits,
            );
            let height = nodes[first..first + COLUMNS]
                .iter()
                .map(|node| node.size().height)
                .fold(0.0, f32::max);
            // The row's first line: a label or value, which do not wrap.
            let line = nodes[first..first + COLUMNS - 1]
                .iter()
                .map(|node| node.size().height)
                .fold(0.0, f32::max);
            for (column, node) in nodes[first..first + COLUMNS].iter_mut().enumerate() {
                // Text starts at the top, so a wrapped qualifier hangs below
                // its value; anything shorter than a line, such as key caps,
                // sits level with it.
                let top = y + ((line - node.size().height) / 2.0).max(0.0);
                node.move_to_mut(Point::new(starts[column], top));
                if node.size().width > 0.0 {
                    width = width.max(starts[column] + node.size().width);
                }
            }
            y += height + ROW_GAP;
        }
        let height = (y - ROW_GAP).max(0.0);
        layout::Node::with_children(
            limits.resolve(Length::Shrink, Length::Shrink, Size::new(width, height)),
            nodes,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((cell, tree), cell_layout) in self
                .cells
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                cell.as_widget_mut()
                    .operate(tree, cell_layout, renderer, operation);
            }
        });
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
        for ((cell, tree), cell_layout) in
            self.cells.iter().zip(&tree.children).zip(layout.children())
        {
            cell.as_widget().draw(
                tree,
                renderer,
                theme,
                style,
                cell_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rich_tip_reads_as_plain_text_one_part_to_a_line() {
        let tip = RichTip::new("Choose an account")
            .shortcut("Ctrl+U")
            .row(TipRow::new("Weekly", "4% left").detail("resets in 3d"))
            .row(TipRow::shortcut("Terminal", "Ctrl+Shift+T"))
            .note("Pinned to this conversation.");
        assert_eq!(
            tip.plain(),
            "Choose an account (Ctrl+U)\nWeekly: 4% left, resets in 3d\nTerminal: Ctrl+Shift+T\nPinned to this conversation."
        );
        assert_eq!(TipBody::from(tip).title(), "Choose an account");
    }

    /// A control named by its tip's title still announces the rest of it.
    #[test]
    fn a_rich_tip_describes_what_its_title_leaves_out() {
        let tip = TipBody::from(
            RichTip::new("Toggle inspector")
                .shortcut("Alt+I")
                .error("Last check failed"),
        );
        assert_eq!(
            tip.description().as_deref(),
            Some("Shortcut: Alt+I\nLast check failed")
        );
        assert_eq!(TipBody::from(RichTip::new("Settings")).description(), None);
        assert_eq!(TipBody::from("Settings").description(), None);
    }

    /// Labels, values and qualifiers each start at one edge, whatever their
    /// lengths; a row without a qualifier leaves its column empty; a key-cap
    /// value sits level with its label; and the table hands an operation to
    /// every cell, so a screen reader or a test reaches the words in it.
    #[test]
    fn a_tip_table_lines_up_its_columns_and_reaches_every_cell() {
        let renderer = crate::workspace_widget::forwarding_test::renderer();
        let mut element: Element<'_, ()> = table(vec![
            TipRow::new("Weekly", "4% left").detail("resets in 3d"),
            TipRow::new("Session window", "100% left").detail("resets in 5h"),
            TipRow::new("Fable", "0% left").warning(),
            TipRow::shortcut("Terminal", "Ctrl+Shift+T"),
        ]);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(DETAILED_MAX_WIDTH, 400.0)),
        );
        let cells: Vec<Rectangle> = node.children().iter().map(|cell| cell.bounds()).collect();
        assert_eq!(cells.len(), 4 * COLUMNS);
        let column = |index: usize| -> Vec<Rectangle> {
            cells.iter().skip(index).step_by(COLUMNS).copied().collect()
        };
        let (labels, values, details) = (column(0), column(1), column(2));
        assert!(labels.iter().all(|label| label.x == 0.0));
        let widest_label = labels.iter().map(|label| label.width).fold(0.0, f32::max);
        assert!(
            values
                .iter()
                .all(|value| value.x == widest_label + COLUMN_GAP),
            "{values:?}"
        );
        let widest_value = values.iter().map(|value| value.width).fold(0.0, f32::max);
        assert_eq!(details[0].x, details[1].x);
        assert_eq!(details[0].x, values[0].x + widest_value + COLUMN_GAP);
        assert_eq!(details[2].width, 0.0, "no qualifier, no text");
        // Rows stack in order without overlapping.
        for pair in labels.windows(2) {
            assert!(pair[1].y >= pair[0].y + pair[0].height, "{labels:?}");
        }
        // The key caps are centered on their label's line.
        let caps = values[3];
        let label = labels[3];
        assert!(
            ((caps.y + caps.height / 2.0) - (label.y + label.height / 2.0)).abs() < 1.0,
            "{caps:?} beside {label:?}"
        );
        assert_eq!(
            node.bounds().width,
            details[0].x + details[0].width.max(details[1].width)
        );

        #[derive(Default)]
        struct Texts(Vec<String>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
                self.0.push(text.to_owned());
            }
        }
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        for text in [
            "Weekly",
            "4% left",
            "resets in 3d",
            "Fable",
            "0% left",
            "Terminal",
            "T",
        ] {
            assert!(
                texts.0.iter().any(|seen| seen == text),
                "{text}: {:?}",
                texts.0
            );
        }
    }

    /// Previews of one account share its notes; the tip says each once.
    #[test]
    fn appending_a_tip_repeats_none_of_its_rows_or_notes() {
        let part = || {
            RichTip::new("Claude")
                .row(TipRow::new("Fable", "0% left"))
                .warning("Limited until it resets.")
        };
        let tip = part()
            .append(part().row(TipRow::new("Weekly", "4% left")))
            .append(part());
        assert_eq!(
            tip.plain(),
            "Claude\nFable: 0% left\nWeekly: 4% left\nLimited until it resets."
        );
    }
}
