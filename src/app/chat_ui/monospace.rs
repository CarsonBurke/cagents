//! The family the generic monospace font stands for, by name.
//!
//! cosmic-text chooses a face for the generic monospace family one word at a
//! time, and to do it ranks every installed face by how much of that word it
//! covers. On a desktop with a thousand-odd faces that is most of what
//! shaping a code block costs: forty times what the same block costs when its
//! family is named, measured on the transcript's own fenced blocks. The
//! ranking only changes the outcome for a word the family's own face cannot
//! draw, so text that face covers can name the family and skip it.
//!
//! Which text that is depends on the fonts installed, so nothing is assumed:
//! the printable ASCII range is shaped both ways once, and the family is only
//! named for the weights and styles where the two came out glyph for glyph the
//! same. Tabs are compared separately, because a face need not draw one.

use cosmic::iced::advanced::graphics::text::{cosmic_text::fontdb, font_system};
use cosmic::iced::advanced::text::Paragraph as _;
use cosmic::iced::{self, Font, font};
use std::sync::OnceLock;

type Paragraph = <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph;

/// Every printable ASCII character, as the words and runs code is made of.
const PROBE: &str = "the quick brown fox jumps over the lazy dog THE QUICK BROWN FOX JUMPS OVER \
                     THE LAZY DOG 0123456789 !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~ fn main() -> u8 { \
                     a != b && c <= d || e == f; x |> y }";

/// Indentation and alignment by tab, in the blank runs code has them in.
const TABS: &str = "\tfn\t\tmain \t x\t \ty";

/// What comparing the two ways of shaping found.
struct Named {
    /// The family's name for each of [`faces`] where naming it changes
    /// nothing.
    families: [Option<&'static str>; 4],
    /// Whether that also holds for tabs, in every face that is named.
    tabs: bool,
}

fn named() -> &'static Named {
    static NAMED: OnceLock<Named> = OnceLock::new();
    NAMED.get_or_init(resolve)
}

/// `font` with its family named, where `font` is the generic monospace family
/// and naming it shapes ASCII text into the same glyphs; `font` otherwise.
///
/// Only text that [`covers`] may be set in the result.
pub(super) fn concrete(font: Font) -> Font {
    match variant(font).and_then(|variant| named().families[variant]) {
        Some(name) => Font {
            family: font::Family::Name(name),
            ..font
        },
        None => font,
    }
}

/// Whether [`concrete`] draws `text` as the generic family would: all of it
/// is in the range the two were compared over.
pub(super) fn covers(text: &str) -> bool {
    text.bytes()
        .all(|byte| matches!(byte, b' '..=b'~') || (byte == b'\t' && named().tabs))
}

/// Which of the four faces syntax highlighting asks for `font` is, if it is
/// one of them.
fn variant(font: Font) -> Option<usize> {
    let bold = match font.weight {
        font::Weight::Normal => false,
        font::Weight::Bold => true,
        _ => return None,
    };
    let italic = match font.style {
        font::Style::Normal => false,
        font::Style::Italic => true,
        font::Style::Oblique => return None,
    };
    (font.family == font::Family::Monospace && font.stretch == font::Stretch::Normal)
        .then_some(usize::from(bold) | usize::from(italic) << 1)
}

fn faces() -> [Font; 4] {
    [0, 1, 2, 3].map(|variant| Font {
        weight: if variant & 1 == 0 {
            font::Weight::Normal
        } else {
            font::Weight::Bold
        },
        style: if variant & 2 == 0 {
            font::Style::Normal
        } else {
            font::Style::Italic
        },
        ..Font::MONOSPACE
    })
}

fn resolve() -> Named {
    let name = font_system()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .raw()
        .db()
        .family_name(&fontdb::Family::Monospace)
        .to_owned();
    // Once per process: a font's family is a `'static` name.
    let name: &'static str = Box::leak(name.into_boxed_str());
    let by_name = |generic: Font| Font {
        family: font::Family::Name(name),
        ..generic
    };
    let families = faces().map(|generic| {
        let glyphs = glyphs(PROBE, generic);
        // A glyph id of zero is the face's "missing" box, which the generic
        // family would have looked elsewhere for.
        (glyphs.iter().all(|glyph| glyph.1 != 0) && glyphs == self::glyphs(PROBE, by_name(generic)))
            .then_some(name)
    });
    let tabs = faces()
        .into_iter()
        .zip(families)
        .filter(|(_, family)| family.is_some())
        .all(|(generic, _)| glyphs(TABS, generic) == glyphs(TABS, by_name(generic)));
    Named { families, tabs }
}

/// What `text` shapes into in `font`: each glyph's face, id and placement.
fn glyphs(text: &str, font: Font) -> Vec<(fontdb::ID, u16, u32, u32)> {
    let spans = [iced::widget::text::Span::<'_, (), Font>::new(text)];
    let paragraph = Paragraph::with_spans(iced::advanced::Text {
        content: spans.as_slice(),
        bounds: iced::Size::INFINITE,
        size: iced::Pixels(16.0),
        line_height: Default::default(),
        font,
        align_x: Default::default(),
        align_y: iced::alignment::Vertical::Top,
        shaping: iced::widget::text::Shaping::Advanced,
        wrapping: iced::widget::text::Wrapping::None,
        ellipsize: Default::default(),
    });
    paragraph
        .buffer()
        .layout_runs()
        .flat_map(|run| run.glyphs.iter())
        .map(|glyph| {
            (
                glyph.font_id,
                glyph.glyph_id,
                glyph.x.to_bits(),
                glyph.w.to_bits(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever fonts the machine running this has, text set in the named
    /// family is the text the generic family draws.
    #[test]
    fn a_named_family_shapes_the_glyphs_the_generic_one_does() {
        let _renderer = crate::app::tests::headless_renderer();
        let code = "fn fold(input: &[u8]) -> u32 {\n    let mut total = 0u32; // carry\n    \
                    total = (total >> 16) + (total & 0xffff);\n    total != 0 && a <= b\n}\n";
        let tabbed = "func main() {\n\tx := 1\t// one\n\t\treturn x\n}\n";
        assert!(code.lines().all(covers));
        for text in [code, tabbed] {
            for generic in faces() {
                // A line the comparison did not cover keeps the generic family.
                let font = if text.lines().all(covers) {
                    concrete(generic)
                } else {
                    generic
                };
                assert_eq!(glyphs(text, font), glyphs(text, generic), "{generic:?}");
            }
        }
    }

    #[test]
    fn only_the_generic_monospace_faces_and_plain_ascii_are_renamed() {
        let _renderer = crate::app::tests::headless_renderer();
        for font in [
            Font::default(),
            Font::with_name("Noto Sans Mono"),
            Font {
                weight: font::Weight::Light,
                ..Font::MONOSPACE
            },
            Font {
                stretch: font::Stretch::Condensed,
                ..Font::MONOSPACE
            },
        ] {
            assert_eq!(concrete(font), font);
        }
        assert!(covers("let total = 0u32; // ~"));
        for text in ["caf\u{e9}", "a \u{2192} b", "line\n", "bell\u{7}"] {
            assert!(!covers(text), "{text:?}");
        }
    }
}
