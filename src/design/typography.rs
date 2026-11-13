//! The type scale and the text roles built on it.
//!
//! A surface picks text by role, never by size. The roles, from the top of a
//! page down, and the fixed air between them:
//!
//! | Role              | Builder         | Size / line box | Weight  |
//! |-------------------|-----------------|-----------------|---------|
//! | Page title        | [`display`]     | 20 / 26         | Bold    |
//! | Section title     | [`title`]       | 16 / 22         | Bold    |
//! | Label             | [`heading`]     | 13 / 18         | Medium  |
//! | Value, row, prose | [`body`]        | 13 / 18         | Regular |
//! | Description       | [`description`] | 12 / 16         | Regular, muted |
//! | Caption, metadata | [`caption`]     | 11              | Regular, muted |
//!
//! A label names one thing the reader can change or inspect. Its description
//! explains it in sentences, so it is set one step above captions: an 11px
//! paragraph under every setting was hard to read at length. Captions stay
//! for short metadata beside or under a row: counts, times, states.
//!
//! The vertical rhythm between the roles is in [`rhythm`]. Every step is on
//! the spacing scale, and each one is fixed by the pair of roles it separates
//! rather than chosen by the surface, so two settings, two sections or a
//! label and its description sit the same distance apart wherever they
//! appear.

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
    body(text).size(TEXT_HEADING).font(heading_font())
}

/// The face of [`heading`], for a widget that cannot be built from it.
pub fn heading_font() -> Font {
    weighted(Weight::Medium)
}

/// The name of a dialog, a form or a section of a page.
pub fn title<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_TITLE)
        .line_height(LineHeight::Absolute(LINE_TITLE.into()))
        .font(title_font())
}

/// The line box title-sized text sits in.
pub const LINE_TITLE: f32 = 22.0;

/// The face of [`title`], for a widget that cannot be built from it.
pub fn title_font() -> Font {
    weighted(Weight::Bold)
}

/// The line box display-sized text sits in.
pub const LINE_DISPLAY: f32 = 26.0;

/// The size of a description: sentences explaining a label above them.
pub const TEXT_DESCRIPTION: f32 = 12.0;
/// The line box a description's lines sit in. Generous for its size, because
/// a description is read as a paragraph rather than glanced at.
pub const LINE_DESCRIPTION: f32 = 16.0;

/// Sentences that explain the label or control above them. Wraps at word
/// boundaries and fills the width it is given.
pub fn description<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_DESCRIPTION)
        .line_height(LineHeight::Absolute(LINE_DESCRIPTION.into()))
        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
        .width(cosmic::iced::Length::Fill)
        .class(cosmic::theme::Text::Custom(muted_text))
}

/// The fixed air between the text roles, from the top of a page down.
pub mod rhythm {
    use super::space;

    /// From a page title to the first section beneath it.
    pub const PAGE_TO_SECTION: f32 = space::XL;
    /// Between two sections of a page.
    pub const SECTION_TO_SECTION: f32 = space::XXL;
    /// From a section title to the description that introduces it.
    pub const TITLE_TO_DESCRIPTION: f32 = space::XXS;
    /// From a section's header to its first item.
    pub const HEADER_TO_ITEM: f32 = space::L;
    /// Between two items of a section, such as two settings.
    pub const ITEM_TO_ITEM: f32 = space::L;
    /// From a label to the control it names, and from a control to the
    /// description under it.
    pub const LABEL_TO_CONTROL: f32 = space::XS;
    /// From a label to the description directly under it, when the control
    /// sits beside the label rather than below it.
    pub const LABEL_TO_DESCRIPTION: f32 = space::XXS;
}

/// The name of a whole page or an otherwise empty pane.
pub fn display<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_DISPLAY)
        .line_height(LineHeight::Absolute(LINE_DISPLAY.into()))
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
