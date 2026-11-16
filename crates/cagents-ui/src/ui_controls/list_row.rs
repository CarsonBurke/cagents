//! One row of a navigation or choice list, on the shared list grid.
//!
//! Every row is the same height, insets its icon by the same amount and
//! starts its label on the same column; see [`crate::design::list`]. Each
//! part of the row is centred by its own box rather than placed by padding,
//! so a 16px icon and an 18px line box share one centre line however tall the
//! row is.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{Alignment, Length},
    widget,
};

use super::Icon;
use crate::design::{self, list};

/// A list row under construction. Mount it with [`ListRow::into`].
pub struct ListRow<'a, Message> {
    icon: Option<Element<'a, Message>>,
    label: Element<'a, Message>,
    caption: Option<Element<'a, Message>>,
    trailing: Vec<Element<'a, Message>>,
    name: Option<Cow<'a, str>>,
    selected: bool,
    on_press: Option<Message>,
    height: f32,
}

/// A row with an optional icon in the icon column and a one-line label that
/// is cut short with an ellipsis when the row is too narrow for it.
pub fn list_row<'a, Message: Clone + 'a>(
    icon: Option<Icon>,
    label: impl Into<Cow<'a, str>>,
) -> ListRow<'a, Message> {
    let label = label.into();
    ListRow {
        icon: icon.map(|icon| icon.handle().icon().size(design::ROW_ICON_SIZE).into()),
        name: Some(label.clone()),
        label: row_label(label),
        caption: None,
        trailing: Vec::new(),
        selected: false,
        on_press: None,
        height: design::ROW_HEIGHT,
    }
}

/// The label a [`list_row`] draws: body text on one line in its line box.
pub fn row_label<'a, Message: 'a>(label: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    super::fade_text(label).width(Length::Fill).into()
}

impl<'a, Message: Clone + 'a> ListRow<'a, Message> {
    /// Replaces the icon with other content for the icon column, such as a
    /// glyph tinted by the state it shows. It is centred in the column.
    pub fn icon_element(mut self, icon: impl Into<Element<'a, Message>>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Replaces the label with other one-line content, such as a label with
    /// its search matches marked. It is centred like the plain label.
    pub fn label(mut self, label: impl Into<Element<'a, Message>>) -> Self {
        self.label = label.into();
        self
    }

    /// Adds a second line under the label, on the same text column, in the
    /// caption role. The row grows to [`list::TWO_LINE_HEIGHT`] and stays
    /// centred; a dense row stays dense only while it has one line.
    pub fn caption(mut self, caption: impl Into<Cow<'a, str>> + 'a) -> Self {
        self.caption = Some(
            super::FadeText::caption(caption)
                .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                    list::CAPTION_LINE.into(),
                ))
                .width(Length::Fill)
                .into(),
        );
        self.height = list::TWO_LINE_HEIGHT;
        self
    }

    /// Replaces the second line with other one-line content, such as a row of
    /// state chips. It is set in a [`list::CAPTION_LINE`] box like a caption.
    pub fn caption_element(mut self, caption: impl Into<Element<'a, Message>>) -> Self {
        self.caption = Some(
            widget::container(caption.into())
                .class(design::transparent())
                .height(Length::Fixed(list::CAPTION_LINE))
                .width(Length::Fill)
                .align_y(Alignment::Center)
                .into(),
        );
        self.height = list::TWO_LINE_HEIGHT;
        self
    }

    /// Adds an element after the label, such as a count or a status mark.
    /// Trailing elements end at the row's inset.
    pub fn trailing(mut self, element: impl Into<Element<'a, Message>>) -> Self {
        self.trailing.push(element.into());
        self
    }

    /// The row's accessible name, when it says more than the label does.
    pub fn name(mut self, name: impl Into<Cow<'a, str>>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn on_press(mut self, message: Message) -> Self {
        self.on_press = Some(message);
        self
    }

    /// Presses send `message` when there is one; without one the row is
    /// drawn disabled.
    pub fn on_press_maybe(mut self, message: Option<Message>) -> Self {
        self.on_press = message;
        self
    }

    /// One dense row high, for lists inside popovers and panels.
    pub fn dense(mut self) -> Self {
        if self.caption.is_none() {
            self.height = design::DENSE_ROW_HEIGHT;
        }
        self
    }

    fn content(
        self,
    ) -> (
        Element<'a, Message>,
        Option<Cow<'a, str>>,
        bool,
        Option<Message>,
    ) {
        let centred = |element: Element<'a, Message>, width: Length| -> Element<'a, Message> {
            widget::container(element)
                .class(design::transparent())
                .width(width)
                .height(Length::Fill)
                .align_y(Alignment::Center)
                .into()
        };
        let mut children = Vec::with_capacity(2 + self.trailing.len());
        if let Some(icon) = self.icon {
            children.push(
                widget::container(icon)
                    .class(design::transparent())
                    .center_x(Length::Fixed(list::ICON))
                    .center_y(Length::Fill)
                    .into(),
            );
        }
        let text = match self.caption {
            Some(caption) => widget::column::with_children(vec![self.label, caption])
                .width(Length::Fill)
                .into(),
            None => self.label,
        };
        children.push(centred(text, Length::Fill));
        for element in self.trailing {
            children.push(centred(element, Length::Shrink));
        }
        let row = widget::row::with_children(children)
            .spacing(list::GAP)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .height(Length::Fixed(self.height));
        (row.into(), self.name, self.selected, self.on_press)
    }
}

impl<'a, Message: Clone + 'a> From<ListRow<'a, Message>> for Element<'a, Message> {
    fn from(row: ListRow<'a, Message>) -> Self {
        let height = row.height;
        let (content, name, selected, on_press) = row.content();
        let mut button = widget::button::custom(content)
            .class(crate::sidebar::sidebar_button_style(selected))
            .selected(selected)
            .padding([0.0, list::INSET])
            .height(Length::Fixed(height))
            .width(Length::Fill)
            .on_press_maybe(on_press);
        if let Some(name) = name {
            button = button.name(name);
        }
        crate::interaction::hover(button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{layout, widget::Tree};
    use cosmic::iced::{Rectangle, Size};

    /// Every leaf of a laid-out tree, with absolute bounds.
    fn leaves(node: &layout::Node, origin: cosmic::iced::Point, out: &mut Vec<Rectangle>) {
        let bounds = node.bounds() + cosmic::iced::Vector::new(origin.x, origin.y);
        if node.children().is_empty() {
            out.push(bounds);
        }
        for child in node.children() {
            leaves(child, bounds.position(), out);
        }
    }

    fn layout_row(mut element: Element<'_, ()>, width: f32) -> layout::Node {
        let renderer = super::super::accessibility_probe::headless_renderer();
        let mut tree = Tree::new(&element);
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 400.0)),
        )
    }

    #[test]
    fn icon_and_label_share_the_row_centre_line_and_columns() {
        for dense in [false, true] {
            let mut row = list_row::<()>(Some(Icon::Settings), "Appearance").on_press(());
            if dense {
                row = row.dense();
            }
            let node = layout_row(row.into(), 180.0);
            let height = node.size().height;
            assert_eq!(
                height,
                if dense {
                    design::DENSE_ROW_HEIGHT
                } else {
                    design::ROW_HEIGHT
                }
            );
            let mut parts = Vec::new();
            leaves(&node, cosmic::iced::Point::ORIGIN, &mut parts);
            // The icon is the square leaf; the label is the line box.
            let icon = parts
                .iter()
                .find(|part| part.width == list::ICON && part.height == list::ICON)
                .expect("an icon box");
            let label = parts
                .iter()
                .find(|part| part.height == design::LINE_BODY)
                .expect("a label line box");
            let centre = height / 2.0;
            assert!(
                (icon.center_y() - centre).abs() <= 0.5,
                "{icon:?} in {height}"
            );
            assert!(
                (label.center_y() - centre).abs() <= 0.5,
                "{label:?} in {height}"
            );
            assert_eq!(icon.x, list::INSET);
            assert_eq!(label.x, list::label_offset());
        }
    }

    #[test]
    fn a_two_line_row_centres_its_text_block_on_the_text_column() {
        let row = list_row::<()>(Some(Icon::Settings), "#12 Group the sidebar")
            .caption("feature/sections · 5 changes")
            .trailing(widget::Space::new().width(40).height(16));
        let node = layout_row(row.into(), 240.0);
        assert_eq!(node.size().height, list::TWO_LINE_HEIGHT);
        let mut parts = Vec::new();
        leaves(&node, cosmic::iced::Point::ORIGIN, &mut parts);
        let label = parts
            .iter()
            .find(|part| part.height == design::LINE_BODY)
            .expect("label");
        let caption = parts
            .iter()
            .find(|part| part.height == list::CAPTION_LINE && part.width > 40.0)
            .expect("caption");
        assert_eq!(label.x, caption.x, "both lines start on the text column");
        assert_eq!(label.x, list::label_offset());
        let block = (label.y + caption.y + caption.height) / 2.0;
        assert!((block - list::TWO_LINE_HEIGHT / 2.0).abs() <= 0.5);
        assert!(caption.y >= label.y + label.height);
    }

    #[test]
    fn a_long_label_stays_on_one_line_inside_the_row() {
        let row = list_row::<()>(
            Some(Icon::Settings),
            "A category whose name is much longer than the rail",
        )
        .trailing(widget::Space::new().width(6).height(6));
        let node = layout_row(row.into(), 120.0);
        assert_eq!(node.size(), Size::new(120.0, design::ROW_HEIGHT));
        let mut parts = Vec::new();
        leaves(&node, cosmic::iced::Point::ORIGIN, &mut parts);
        for part in parts {
            assert!(part.x + part.width <= 120.0 - list::INSET + 0.5, "{part:?}");
            assert!(part.y >= 0.0 && part.y + part.height <= design::ROW_HEIGHT);
        }
    }
}
