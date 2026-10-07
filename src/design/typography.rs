//! The type scale and the text roles built on it.

use std::borrow::Cow;

use cosmic::{
    Renderer, Theme,
    iced::{
        font::{Font, Weight},
        widget::text::LineHeight,
    },
    widget::text::Text,
};

use super::{GRID_UNIT, RADIUS_XS, accent_color, muted_text, raised, space};

/// The size of nearly everything in the shell: rows, controls, labels.
pub const TEXT_BODY: f32 = 13.0;
/// Prose that is read at length - an agent's answers and the draft being
/// written to it - one step above the shell's controls and captions.
pub const TEXT_PROSE: f32 = 14.0;
/// Secondary detail beside or beneath a body-sized label.
pub const TEXT_CAPTION: f32 = 11.0;
/// A heading is set at body size and told apart by weight, so a pane's
/// header does not outshout the rows under it.
pub const TEXT_HEADING: f32 = TEXT_BODY;
/// The name of a dialog, a form or a section of a page.
pub const TEXT_TITLE: f32 = 16.0;
/// The name of a whole page or an otherwise empty pane. Nothing in the shell
/// is set larger: a workbench's hierarchy comes from weight and position, and
/// a 24px or 29px headline beside 13px rows reads as a different product.
pub const TEXT_DISPLAY: f32 = 20.0;

/// The line box body-sized text sits in.
pub const LINE_BODY: f32 = 18.0;

/// The widest a column of prose is allowed to grow.
///
/// Reading a paragraph that spans a maximised window costs the eye its place
/// on every wrap. Typography practice puts the comfortable measure at roughly
/// 60 to 80 characters, which at our body size is this many logical pixels.
/// The transcript's own chrome still spans the full width; only the text is
/// held back.
pub const READABLE_MEASURE: f32 = 720.0;

type ShellText<'a> = Text<'a, Theme, Renderer>;

fn weighted(weight: Weight) -> Font {
    Font {
        weight,
        ..cosmic::font::default()
    }
}

pub fn body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_BODY)
        .line_height(LineHeight::Absolute(LINE_BODY.into()))
}

pub fn caption<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::caption(text)
        .size(TEXT_CAPTION)
        .class(cosmic::theme::Text::Custom(muted_text))
}

/// Body text at medium weight: a pane's heading, a row's name, a button's label.
pub fn heading<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    body(text).size(TEXT_HEADING).font(weighted(Weight::Medium))
}

/// The name of a dialog, a form or a section of a page.
pub fn title<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_TITLE)
        .line_height(LineHeight::Absolute(22.0.into()))
        .font(weighted(Weight::Bold))
}

/// The name of a whole page or an otherwise empty pane.
pub fn display<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_DISPLAY)
        .line_height(LineHeight::Absolute(26.0.into()))
        .font(weighted(Weight::Bold))
}

/// Markdown shares the shell's semantic palette and a compact heading scale.
pub fn markdown_settings(theme: &Theme, size: f32) -> cosmic::widget::markdown::Settings {
    use cosmic::{iced, widget::markdown};
    let mut style = markdown::Style::from(if theme.theme_type.is_dark() {
        iced::Theme::Dark
    } else {
        iced::Theme::Light
    });
    style.link_color = accent_color(theme);
    style.inline_code_color = theme.cosmic().background(false).on.into();
    style.inline_code_highlight.background = raised(theme).into();
    style.inline_code_highlight.border.radius = RADIUS_XS.into();
    markdown::Settings {
        h1_size: (size + GRID_UNIT).into(),
        h2_size: (size + GRID_UNIT / 2.0).into(),
        h3_size: (size + 1.0).into(),
        h4_size: size.into(),
        h5_size: size.into(),
        h6_size: size.into(),
        code_size: size.into(),
        spacing: space::S.into(),
        ..markdown::Settings::with_text_size(size, style)
    }
}
