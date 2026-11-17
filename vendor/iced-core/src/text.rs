//! Draw and interact with text.
pub mod blink;
pub mod editor;
pub mod highlighter;
pub mod paragraph;

pub use blink::Blink;
pub use editor::Editor;
pub use highlighter::Highlighter;
pub use paragraph::Paragraph;

use crate::alignment;
use crate::{
    Background, Border, Color, Padding, Pixels, Point, Rectangle, Size,
};

use std::borrow::Cow;
use std::hash::{Hash, Hasher};

/// A paragraph.
#[derive(Debug, Clone, Copy)]
pub struct Text<Content = String, Font = crate::Font> {
    /// The content of the paragraph.
    pub content: Content,

    /// The bounds of the paragraph.
    pub bounds: Size,

    /// The size of the [`Text`] in logical pixels.
    pub size: Pixels,

    /// The line height of the [`Text`].
    pub line_height: LineHeight,

    /// The font of the [`Text`].
    pub font: Font,

    /// The horizontal alignment of the [`Text`].
    pub align_x: Alignment,

    /// The vertical alignment of the [`Text`].
    pub align_y: alignment::Vertical,

    /// The [`Shaping`] strategy of the [`Text`].
    pub shaping: Shaping,

    /// The [`Wrapping`] strategy of the [`Text`].
    pub wrapping: Wrapping,

    /// The  [`Ellipsize`] strategy of the [`Text`].
    pub ellipsize: Ellipsize,
}

impl<Content, Font> Text<Content, Font>
where
    Font: Copy,
{
    /// Returns a new [`Text`] replacing only the content with the
    /// given value.
    pub fn with_content<T>(&self, content: T) -> Text<T, Font> {
        Text {
            content,
            bounds: self.bounds,
            size: self.size,
            line_height: self.line_height,
            font: self.font,
            align_x: self.align_x,
            align_y: self.align_y,
            shaping: self.shaping,
            wrapping: self.wrapping,
            ellipsize: self.ellipsize,
        }
    }
}

impl<Content, Font> Text<Content, Font>
where
    Content: AsRef<str>,
    Font: Copy,
{
    /// Returns a borrowed version of [`Text`].
    pub fn as_ref(&self) -> Text<&str, Font> {
        self.with_content(self.content.as_ref())
    }
}

/// The alignment of some text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Alignment {
    /// No specific alignment.
    ///
    /// Left-to-right text will be aligned to the left, while
    /// right-to-left text will be aligned to the right.
    #[default]
    Default,
    /// Align text to the left.
    Left,
    /// Center text.
    Center,
    /// Align text to the right.
    Right,
    /// Justify text.
    Justified,
}

impl From<alignment::Horizontal> for Alignment {
    fn from(alignment: alignment::Horizontal) -> Self {
        match alignment {
            alignment::Horizontal::Left => Self::Left,
            alignment::Horizontal::Center => Self::Center,
            alignment::Horizontal::Right => Self::Right,
        }
    }
}

impl From<crate::Alignment> for Alignment {
    fn from(alignment: crate::Alignment) -> Self {
        match alignment {
            crate::Alignment::Start => Self::Left,
            crate::Alignment::Center => Self::Center,
            crate::Alignment::End => Self::Right,
        }
    }
}

impl From<Alignment> for alignment::Horizontal {
    fn from(alignment: Alignment) -> Self {
        match alignment {
            Alignment::Default | Alignment::Left | Alignment::Justified => {
                alignment::Horizontal::Left
            }
            Alignment::Center => alignment::Horizontal::Center,
            Alignment::Right => alignment::Horizontal::Right,
        }
    }
}

/// The shaping strategy of some text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shaping {
    /// Auto-detect the best shaping strategy from the text.
    ///
    /// This strategy will use [`Basic`](Self::Basic) shaping if the
    /// text consists of only ASCII characters; otherwise, it will
    /// use [`Advanced`](Self::Advanced) shaping.
    ///
    /// This is the default, if neither the `basic-shaping` nor `advanced-shaping`
    /// features are enabled.
    Auto,
    /// No shaping and no font fallback.
    ///
    /// This shaping strategy is very cheap, but it will not display complex
    /// scripts properly nor try to find missing glyphs in your system fonts.
    ///
    /// You should use this strategy when you have complete control of the text
    /// and the font you are displaying in your application.
    ///
    /// This will be the default if the `basic-shaping` feature is enabled and
    /// the `advanced-shaping` feature is disabled.
    Basic,
    /// Advanced text shaping and font fallback.
    ///
    /// You will need to enable this flag if the text contains a complex
    /// script, the font used needs it, and/or multiple fonts in your system
    /// may be needed to display all of the glyphs.
    ///
    /// Advanced shaping is expensive! You should only enable it when necessary.
    ///
    /// This will be the default if the `advanced-shaping` feature is enabled.
    Advanced,
}

impl Default for Shaping {
    fn default() -> Self {
        if cfg!(feature = "advanced-shaping") {
            Self::Advanced
        } else if cfg!(feature = "basic-shaping") {
            Self::Basic
        } else {
            Self::Auto
        }
    }
}

/// The wrapping strategy of some text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Wrapping {
    /// No wrapping.
    None,
    /// Wraps at the word level.
    ///
    /// This is the default.
    #[default]
    Word,
    /// Wraps at the glyph level.
    Glyph,
    /// Wraps at the word level, or fallback to glyph level if a word can't fit on a line by itself.
    WordOrGlyph,
}

/// The ellipsizing strategy of some text.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Ellipsize {
    /// No ellipsizing.
    ///
    /// This is the default.
    #[default]
    None,
    /// Ellipsize at the start of the text.
    Start(EllipsizeHeightLimit),
    /// Ellipsize in the middle of the text.
    Middle(EllipsizeHeightLimit),
    /// Ellipsize at the end of the text.
    End(EllipsizeHeightLimit),
}

/// The ellipsizing strategy of some text when it exceeds a certain height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EllipsizeHeightLimit {
    /// The number of lines after which ellipsizing should occur.
    Lines(usize),
    /// The height limit in pixels
    Height(f32),
}

/// The height of a line of text in a paragraph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeight {
    /// A factor of the size of the text.
    Relative(f32),

    /// An absolute height in logical pixels.
    Absolute(Pixels),
}

impl LineHeight {
    /// Returns the [`LineHeight`] in absolute logical pixels.
    pub fn to_absolute(self, text_size: Pixels) -> Pixels {
        match self {
            Self::Relative(factor) => Pixels(factor * text_size.0),
            Self::Absolute(pixels) => pixels,
        }
    }
}

impl Default for LineHeight {
    fn default() -> Self {
        Self::Relative(1.4)
    }
}

impl From<f32> for LineHeight {
    fn from(factor: f32) -> Self {
        Self::Relative(factor)
    }
}

impl From<Pixels> for LineHeight {
    fn from(pixels: Pixels) -> Self {
        Self::Absolute(pixels)
    }
}

impl Hash for LineHeight {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Relative(factor) => {
                state.write_u8(0);
                factor.to_bits().hash(state);
            }
            Self::Absolute(pixels) => {
                state.write_u8(1);
                f32::from(*pixels).to_bits().hash(state);
            }
        }
    }
}

/// The result of hit testing on text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// The point was within the bounds of the returned character index.
    CharOffset(usize, Affinity),
}

impl Hit {
    /// Computes the cursor position of the [`Hit`] .
    pub fn cursor(self) -> usize {
        match self {
            Self::CharOffset(i, _) => i,
        }
    }

    /// Returns the cursor [`Affinity`] of the [`Hit`].
    pub fn affinity(&self) -> Affinity {
        match self {
            Self::CharOffset(_, a) => *a,
        }
    }
}

/// Cursor affinity for BiDi text. At the boundary between runs of different
/// directions, the same byte index can map to different visual positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Affinity {
    /// Associate with the run before the cursor index.
    #[default]
    Before,
    /// Associate with the run after the cursor index.
    After,
}

/// The difference detected in some text.
///
/// You will obtain a [`Difference`] when you [`compare`] a [`Paragraph`] with some
/// [`Text`].
///
/// [`compare`]: Paragraph::compare
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Difference {
    /// No difference.
    ///
    /// The text can be reused as it is!
    None,

    /// A bounds difference.
    ///
    /// This normally means a relayout is necessary, but the shape of the text can
    /// be reused.
    Bounds,

    /// A shape difference.
    ///
    /// The contents, alignment, sizes, fonts, or any other essential attributes
    /// of the shape of the text have changed. A complete reshape and relayout of
    /// the text is necessary.
    Shape,
}

/// A renderer capable of measuring and drawing [`Text`].
pub trait Renderer: crate::Renderer {
    /// The font type used.
    type Font: Copy + PartialEq;

    /// The [`Paragraph`] of this [`Renderer`].
    type Paragraph: Paragraph<Font = Self::Font> + 'static;

    /// The [`Editor`] of this [`Renderer`].
    type Editor: Editor<Font = Self::Font> + 'static;

    /// The Raw of this [`Renderer`].
    type Raw;

    /// The icon font of the backend.
    const ICON_FONT: Self::Font;

    /// The `char` representing a ✔ icon in the [`ICON_FONT`].
    ///
    /// [`ICON_FONT`]: Self::ICON_FONT
    const CHECKMARK_ICON: char;

    /// The `char` representing a ▼ icon in the built-in [`ICON_FONT`].
    ///
    /// [`ICON_FONT`]: Self::ICON_FONT
    const ARROW_DOWN_ICON: char;

    /// The `char` representing a ^ icon in the built-in [`ICON_FONT`].
    ///
    /// [`ICON_FONT`]: Self::ICON_FONT
    const SCROLL_UP_ICON: char;

    /// The `char` representing a v icon in the built-in [`ICON_FONT`].
    ///
    /// [`ICON_FONT`]: Self::ICON_FONT
    const SCROLL_DOWN_ICON: char;

    /// The `char` representing a < icon in the built-in [`ICON_FONT`].
    ///
    /// [`ICON_FONT`]: Self::ICON_FONT
    const SCROLL_LEFT_ICON: char;

    /// The `char` representing a > icon in the built-in [`ICON_FONT`].
    ///
    /// [`ICON_FONT`]: Self::ICON_FONT
    const SCROLL_RIGHT_ICON: char;

    /// The 'char' representing the iced logo in the built-in ['ICON_FONT'].
    ///
    /// ['ICON_FONT']: Self::ICON_FONT
    const ICED_LOGO: char;

    /// Returns the default [`Self::Font`].
    fn default_font(&self) -> Self::Font;

    /// Returns the default size of [`Text`].
    fn default_size(&self) -> Pixels;

    /// Draws the given [`Paragraph`] at the given position and with the given
    /// [`Color`].
    fn fill_paragraph(
        &mut self,
        text: &Self::Paragraph,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    );

    /// Draws the given [`Editor`] at the given position and with the given
    /// [`Color`].
    fn fill_editor(
        &mut self,
        editor: &Self::Editor,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    );

    /// Draws the given Raw
    fn fill_raw(&mut self, raw: Self::Raw);

    /// Draws the given [`Text`] at the given position and with the given
    /// [`Color`].
    fn fill_text(
        &mut self,
        text: Text<String, Self::Font>,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    );
}

/// A span of text.
#[derive(Debug, Clone)]
pub struct Span<'a, Link = (), Font = crate::Font> {
    /// The [`Fragment`] of text.
    pub text: Fragment<'a>,
    /// The size of the [`Span`] in [`Pixels`].
    pub size: Option<Pixels>,
    /// The [`LineHeight`] of the [`Span`].
    pub line_height: Option<LineHeight>,
    /// A factor applied to the [`Span`]'s [`size`], or to the paragraph's
    /// size when it has none.
    ///
    /// Unlike [`size`], it keeps the paragraph's line height when
    /// [`line_height`] is unset, so a span set smaller than the text around
    /// it does not change the height of the lines it falls on.
    ///
    /// [`size`]: Self::size
    /// [`line_height`]: Self::line_height
    pub relative_size: Option<f32>,
    /// The font of the [`Span`].
    pub font: Option<Font>,
    /// The [`Color`] of the [`Span`].
    pub color: Option<Color>,
    /// The link of the [`Span`].
    pub link: Option<Link>,
    /// The [`Highlight`] of the [`Span`].
    pub highlight: Option<Highlight>,
    /// The [`Padding`] of the [`Span`].
    ///
    /// Currently, it only affects the bounds of the [`Highlight`].
    pub padding: Padding,
    /// Horizontal room in logical pixels laid out at the [`Span`]'s start
    /// and end, like CSS inline padding.
    ///
    /// Unlike [`padding`], it moves the text around the [`Span`] and lies
    /// inside its bounds, so the [`Highlight`] covers it.
    ///
    /// [`padding`]: Self::padding
    pub inline_padding: f32,
    /// Whether the [`Span`] should be underlined or not.
    pub underline: bool,
    /// Whether the [`Span`] should be struck through or not.
    pub strikethrough: bool,
}

/// A text highlight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Highlight {
    /// The [`Background`] of the highlight.
    pub background: Background,
    /// The [`Border`] of the highlight.
    pub border: Border,
}

impl<'a, Link, Font> Span<'a, Link, Font> {
    /// Creates a new [`Span`] of text with the given text fragment.
    pub fn new(fragment: impl IntoFragment<'a>) -> Self {
        Self {
            text: fragment.into_fragment(),
            ..Self::default()
        }
    }

    /// Sets the size of the [`Span`].
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.size = Some(size.into());
        self
    }

    /// Sets the [`LineHeight`] of the [`Span`].
    pub fn line_height(mut self, line_height: impl Into<LineHeight>) -> Self {
        self.line_height = Some(line_height.into());
        self
    }

    /// Sets the [`relative_size`] of the [`Span`].
    ///
    /// [`relative_size`]: Self::relative_size
    pub fn relative_size(mut self, factor: f32) -> Self {
        self.relative_size = Some(factor);
        self
    }

    /// The font size and absolute line height this [`Span`] is shaped with
    /// in a paragraph of `size` and `line_height`, or `None` when it is
    /// shaped with the paragraph's own.
    pub fn metrics(
        &self,
        size: Pixels,
        line_height: LineHeight,
    ) -> Option<(Pixels, Pixels)> {
        if self.size.is_none()
            && self.line_height.is_none()
            && self.relative_size.is_none()
        {
            return None;
        }
        let base = self.size.unwrap_or(size);
        let span_size = Pixels(base.0 * self.relative_size.unwrap_or(1.0));
        let line_height = match self.line_height {
            Some(line_height) => line_height.to_absolute(span_size),
            // A relative size scales the glyphs, not the line they sit on.
            None => line_height.to_absolute(base),
        };
        Some((span_size, line_height))
    }

    /// Sets the font of the [`Span`].
    pub fn font(mut self, font: impl Into<Font>) -> Self {
        self.font = Some(font.into());
        self
    }

    /// Sets the font of the [`Span`], if any.
    pub fn font_maybe(mut self, font: Option<impl Into<Font>>) -> Self {
        self.font = font.map(Into::into);
        self
    }

    /// Sets the [`Color`] of the [`Span`].
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Sets the [`Color`] of the [`Span`], if any.
    pub fn color_maybe(mut self, color: Option<impl Into<Color>>) -> Self {
        self.color = color.map(Into::into);
        self
    }

    /// Sets the link of the [`Span`].
    pub fn link(mut self, link: impl Into<Link>) -> Self {
        self.link = Some(link.into());
        self
    }

    /// Sets the link of the [`Span`], if any.
    pub fn link_maybe(mut self, link: Option<impl Into<Link>>) -> Self {
        self.link = link.map(Into::into);
        self
    }

    /// Sets the [`Background`] of the [`Span`].
    pub fn background(self, background: impl Into<Background>) -> Self {
        self.background_maybe(Some(background))
    }

    /// Sets the [`Background`] of the [`Span`], if any.
    pub fn background_maybe(
        mut self,
        background: Option<impl Into<Background>>,
    ) -> Self {
        let Some(background) = background else {
            return self;
        };

        match &mut self.highlight {
            Some(highlight) => {
                highlight.background = background.into();
            }
            None => {
                self.highlight = Some(Highlight {
                    background: background.into(),
                    border: Border::default(),
                });
            }
        }

        self
    }

    /// Sets the [`Border`] of the [`Span`].
    pub fn border(self, border: impl Into<Border>) -> Self {
        self.border_maybe(Some(border))
    }

    /// Sets the [`Border`] of the [`Span`], if any.
    pub fn border_maybe(mut self, border: Option<impl Into<Border>>) -> Self {
        let Some(border) = border else {
            return self;
        };

        match &mut self.highlight {
            Some(highlight) => {
                highlight.border = border.into();
            }
            None => {
                self.highlight = Some(Highlight {
                    border: border.into(),
                    background: Background::Color(Color::TRANSPARENT),
                });
            }
        }

        self
    }

    /// Sets the [`Padding`] of the [`Span`].
    ///
    /// It only affects the [`background`] and [`border`] of the
    /// [`Span`], currently.
    ///
    /// [`background`]: Self::background
    /// [`border`]: Self::border
    pub fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.padding = padding.into();
        self
    }

    /// Sets the [`inline_padding`] of the [`Span`].
    ///
    /// [`inline_padding`]: Self::inline_padding
    pub fn inline_padding(mut self, inline_padding: f32) -> Self {
        self.inline_padding = inline_padding;
        self
    }

    /// Sets whether the [`Span`] should be underlined or not.
    pub fn underline(mut self, underline: bool) -> Self {
        self.underline = underline;
        self
    }

    /// Sets whether the [`Span`] should be struck through or not.
    pub fn strikethrough(mut self, strikethrough: bool) -> Self {
        self.strikethrough = strikethrough;
        self
    }

    /// Turns the [`Span`] into a static one.
    pub fn to_static(self) -> Span<'static, Link, Font> {
        Span {
            text: Cow::Owned(self.text.into_owned()),
            size: self.size,
            line_height: self.line_height,
            relative_size: self.relative_size,
            font: self.font,
            color: self.color,
            link: self.link,
            highlight: self.highlight,
            padding: self.padding,
            inline_padding: self.inline_padding,
            underline: self.underline,
            strikethrough: self.strikethrough,
        }
    }
}

impl<Link, Font> Default for Span<'_, Link, Font> {
    fn default() -> Self {
        Self {
            text: Cow::default(),
            size: None,
            line_height: None,
            relative_size: None,
            font: None,
            color: None,
            link: None,
            highlight: None,
            padding: Padding::default(),
            inline_padding: 0.0,
            underline: false,
            strikethrough: false,
        }
    }
}

impl<'a, Link, Font> From<&'a str> for Span<'a, Link, Font> {
    fn from(value: &'a str) -> Self {
        Span::new(value)
    }
}

impl<Link, Font: PartialEq> PartialEq for Span<'_, Link, Font> {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.size == other.size
            && self.line_height == other.line_height
            && self.relative_size == other.relative_size
            && self.inline_padding == other.inline_padding
            && self.font == other.font
            && self.color == other.color
    }
}

/// A fragment of [`Text`].
///
/// This is just an alias to a string that may be either
/// borrowed or owned.
pub type Fragment<'a> = Cow<'a, str>;

/// A trait for converting a value to some text [`Fragment`].
pub trait IntoFragment<'a> {
    /// Converts the value to some text [`Fragment`].
    fn into_fragment(self) -> Fragment<'a>;
}

impl<'a> IntoFragment<'a> for Fragment<'a> {
    fn into_fragment(self) -> Fragment<'a> {
        self
    }
}

impl<'a> IntoFragment<'a> for &'a Fragment<'_> {
    fn into_fragment(self) -> Fragment<'a> {
        Fragment::Borrowed(self)
    }
}

impl<'a> IntoFragment<'a> for &'a str {
    fn into_fragment(self) -> Fragment<'a> {
        Fragment::Borrowed(self)
    }
}

impl<'a> IntoFragment<'a> for &'a String {
    fn into_fragment(self) -> Fragment<'a> {
        Fragment::Borrowed(self.as_str())
    }
}

impl<'a> IntoFragment<'a> for String {
    fn into_fragment(self) -> Fragment<'a> {
        Fragment::Owned(self)
    }
}

macro_rules! into_fragment {
    ($type:ty) => {
        impl<'a> IntoFragment<'a> for $type {
            fn into_fragment(self) -> Fragment<'a> {
                Fragment::Owned(self.to_string())
            }
        }

        impl<'a> IntoFragment<'a> for &$type {
            fn into_fragment(self) -> Fragment<'a> {
                Fragment::Owned(self.to_string())
            }
        }
    };
}

into_fragment!(char);
into_fragment!(bool);

into_fragment!(u8);
into_fragment!(u16);
into_fragment!(u32);
into_fragment!(u64);
into_fragment!(u128);
into_fragment!(usize);

into_fragment!(i8);
into_fragment!(i16);
into_fragment!(i32);
into_fragment!(i64);
into_fragment!(i128);
into_fragment!(isize);

into_fragment!(f32);
into_fragment!(f64);

#[cfg(test)]
mod span_metrics_tests {
    use super::*;

    #[test]
    fn a_relative_size_scales_the_glyphs_but_keeps_the_paragraph_line() {
        let size = Pixels(14.0);
        let line_height = LineHeight::Relative(1.4);
        let plain: Span<'_> = Span::new("prose");
        assert_eq!(plain.metrics(size, line_height), None);

        let code: Span<'_> = Span::new("code").relative_size(0.9);
        let (glyphs, line) = code.metrics(size, line_height).unwrap();
        assert!((glyphs.0 - 12.6).abs() < 1e-4);
        assert!((line.0 - 19.6).abs() < 1e-4);

        // An absolute size still sets its own line, as upstream.
        let large: Span<'_> = Span::new("large").size(20.0);
        assert_eq!(
            large.metrics(size, line_height),
            Some((Pixels(20.0), Pixels(28.0)))
        );
        // A relative size of an absolute one scales from it, and an
        // explicit line height wins over both.
        let (glyphs, line) = large
            .clone()
            .relative_size(0.5)
            .metrics(size, line_height)
            .unwrap();
        assert_eq!((glyphs, line), (Pixels(10.0), Pixels(28.0)));
        let fixed = code.line_height(LineHeight::Absolute(Pixels(30.0)));
        assert_eq!(fixed.metrics(size, line_height).unwrap().1, Pixels(30.0));

        let x: Span<'_> = Span::new("x");
        assert_ne!(x.clone(), x.relative_size(0.9));
    }
}
