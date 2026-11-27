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
//! A conversation's transcript is read rather than operated, so it has a
//! size of its own, [`TEXT_TRANSCRIPT`]: answers, the names in its rows, its
//! timeline rows and their states are all set at it, through [`transcript`]
//! and [`transcript_name`]. A row is told from an answer by weight and colour,
//! never by size. Captions keep their role inside it, as everywhere.
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

use super::{GRID_UNIT, RADIUS_XS, accent_color, code_chip, muted_text, space};

/// The size of nearly everything in the shell: rows, controls, labels.
pub const TEXT_BODY: f32 = 13.0;
/// Prose that is read at length - an agent's answers and the draft being
/// written to it - one step above the shell's controls and captions.
pub const TEXT_PROSE: f32 = 14.0;
/// The one size a conversation's transcript reads at: an answer, a message,
/// and every row of its timeline - steps, thoughts, notices, the agents it
/// started and the states beside them. The shell around it, its sidebar, tabs
/// and composer controls, stay at [`TEXT_BODY`].
///
/// Set at prose size so reading text keeps its size and the timeline steps up
/// to match; [`TEXT_BODY`] here would set the whole transcript at the shell's.
pub const TEXT_TRANSCRIPT: f32 = TEXT_PROSE;
/// The line box a transcript row's text sits in. Six pixels of leading is
/// about the composer's 1.4 at prose size, on a whole pixel, so a column of
/// one-line rows keeps whole-pixel pitch.
pub const LINE_TRANSCRIPT: f32 = TEXT_TRANSCRIPT + 6.0;
/// Secondary detail beside or beneath a body-sized label.
pub const TEXT_CAPTION: f32 = 11.0;
/// The line box a caption sits in: libcosmic's caption preset's.
pub const LINE_CAPTION: f32 = 17.0;
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
        .line_height(LineHeight::Absolute(LINE_CAPTION.into()))
        .class(cosmic::theme::Text::Custom(muted_text))
}

/// A line of a conversation's transcript: a row's words or its state.
pub fn transcript<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    body(text)
        .size(TEXT_TRANSCRIPT)
        .line_height(LineHeight::Absolute(LINE_TRANSCRIPT.into()))
}

/// A name in a transcript row, such as the agent it started: emphasised by
/// weight, at the size of the words around it.
pub fn transcript_name<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    transcript(text).font(transcript_name_font())
}

/// The face of [`transcript_name`], for a widget that cannot be built from it.
pub fn transcript_name_font() -> Font {
    weighted(Weight::Semibold)
}

/// Body text at medium weight: a pane's heading, a row's name, a button's label.
pub fn heading<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    body(text).size(TEXT_HEADING).font(heading_font())
}

/// The face of [`heading`], for a widget that cannot be built from it.
pub fn heading_font() -> Font {
    weighted(Weight::Medium)
}

/// A list section's header: description-sized, semibold and muted, so it
/// names the rows beneath it without outweighing them.
pub fn section_heading<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(TEXT_DESCRIPTION)
        .line_height(LineHeight::Absolute(LINE_DESCRIPTION.into()))
        .font(weighted(Weight::Semibold))
        .class(cosmic::theme::Text::Custom(muted_text))
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

/// The size of inline code relative to the prose it sits in.
///
/// A monospace face's letters are wider and denser than proportional ones:
/// with the default faces the two share an x-height, but the monospace
/// lowercase advances 15% further, so code at the prose's size read larger
/// and heavier than the sentence around it. At nine tenths its letters take
/// the prose's room and weight. The line height stays the prose's, so lines
/// with code keep the same pitch as lines without.
pub const INLINE_CODE_RELATIVE_SIZE: f32 = 0.9;

/// How far the chip behind inline code comes into the line box of prose
/// set at `size`.
///
/// The highlight starts as the whole line box, and a chip that tall touched
/// the chip on the next line. The chip drops the half-leading and a little
/// more from the top, which leaves about a pixel above inline code's
/// capitals and brackets. Its descenders reach the bottom of the box, so
/// the bottom stays where it is.
pub fn inline_code_padding(size: f32) -> cosmic::iced::Padding {
    let half_leading = (LineHeight::default().to_absolute(size.into()).0 - size) / 2.0;
    cosmic::iced::Padding {
        top: -(half_leading + size * 0.08),
        ..cosmic::iced::Padding::ZERO
    }
}

/// The room laid out before and after inline code in prose set at `size`,
/// which the chip covers. Being laid out, it keeps the chip clear of the
/// letters and punctuation beside the code.
pub fn inline_code_inline_padding(size: f32) -> f32 {
    size * 0.2
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
    // Code reads in the color of the sentence it is part of; the chip is
    // what sets it apart. Full-strength ink made it brighter than the
    // prose around it.
    style.inline_code_color = None;
    style.inline_code_highlight.background = code_chip(theme).into();
    style.inline_code_highlight.border.radius = RADIUS_XS.into();
    style.inline_code_relative_size = INLINE_CODE_RELATIVE_SIZE;
    style.inline_code_padding = inline_code_padding(size);
    style.inline_code_inline_padding = inline_code_inline_padding(size);
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
