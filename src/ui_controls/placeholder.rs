//! What a pane shows when it has nothing of its own to show.
//!
//! An empty list, a failed load and a missing selection are the same shape:
//! a glyph, a name for the state, a sentence about it, and at most one way
//! out. Each surface used to assemble that shape by hand, at its own icon
//! size, type size and card treatment; this is the one they share.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{Alignment, Length, widget::text::Wrapping},
    widget,
};

use super::passive_icon;
use crate::design::{self, space};

/// The widest a placeholder's sentence runs before it wraps.
const MEASURE: f32 = 320.0;

pub struct Placeholder<'a, Message> {
    title: Cow<'a, str>,
    icon: Option<widget::icon::Handle>,
    detail: Option<Cow<'a, str>>,
    action: Option<Element<'a, Message>>,
    height: Length,
}

/// A centered statement that a pane is empty, unavailable or has failed.
pub fn placeholder<'a, Message>(title: impl Into<Cow<'a, str>>) -> Placeholder<'a, Message> {
    Placeholder {
        title: title.into(),
        icon: None,
        detail: None,
        action: None,
        height: Length::Fill,
    }
}

impl<'a, Message> Placeholder<'a, Message> {
    /// The glyph that names the kind of state before the title is read.
    pub fn icon(mut self, icon: impl Into<widget::icon::Handle>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// One sentence on why the pane is in this state or what would fill it.
    pub fn detail(mut self, detail: impl Into<Cow<'a, str>>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The single way out of the state: a retry, an "open", a "clear search".
    pub fn action(mut self, action: impl Into<Element<'a, Message>>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// A placeholder inside scrolling content takes the height it needs
    /// instead of centering in its pane.
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }
}

impl<'a, Message: 'a> From<Placeholder<'a, Message>> for Element<'a, Message> {
    fn from(placeholder: Placeholder<'a, Message>) -> Self {
        let Placeholder {
            title,
            icon,
            detail,
            action,
            height,
        } = placeholder;
        let mut content = widget::column::with_capacity(4)
            .spacing(space::S)
            .align_x(Alignment::Center)
            .max_width(MEASURE);
        if let Some(icon) = icon {
            content = content.push(passive_icon(icon, design::STATUS_ICON_SIZE));
        }
        let mut text = widget::column::with_capacity(2)
            .spacing(space::XXS)
            .align_x(Alignment::Center)
            .push(design::heading(title).center());
        if let Some(detail) = detail {
            text = text.push(design::caption(detail).wrapping(Wrapping::Word).center());
        }
        content = content.push(text);
        if let Some(action) = action {
            content = content.push(widget::container(action).padding([space::XS, 0.0, 0.0, 0.0]));
        }
        widget::container(content)
            .padding(space::L)
            .center_x(Length::Fill)
            .center_y(height)
            .into()
    }
}
