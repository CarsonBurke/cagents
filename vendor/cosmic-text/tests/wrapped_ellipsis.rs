//! Regressions for cagents #1473 / cosmic-text PR #535.
//!
//! Use bundled fonts and an empty database so host font installation and
//! fallback order cannot change the word boundaries under test.
use cosmic_text::{
    fontdb, Attrs, AttrsList, Ellipsize, EllipsizeHeightLimit, Family, FontSystem, Hinting,
    LayoutLine, ShapeBuffer, ShapeLine, Shaping, Wrap,
};

fn font_system() -> FontSystem {
    let mut db = fontdb::Database::new();
    db.load_font_data(
        include_bytes!("../../libcosmic/res/open-sans/OpenSans-Regular.ttf").to_vec(),
    );
    db.load_font_data(include_bytes!("fonts/NotoSansHebrew.ttf").to_vec());
    FontSystem::new_with_locale_and_db("en-US".into(), db)
}

fn layout(line: &ShapeLine, width: f32, wrap: Wrap, ellipsize: Ellipsize) -> Vec<LayoutLine> {
    let mut lines = Vec::new();
    line.layout_to_buffer(
        &mut ShapeBuffer::default(),
        18.0,
        Some(width),
        wrap,
        ellipsize,
        None,
        &mut lines,
        None,
        Hinting::Disabled,
    );
    lines
}

#[test]
fn last_ellipsized_line_starts_where_ordinary_wrapping_starts() {
    let mut fonts = font_system();
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Open Sans")));
    // Pure RTL runs use the same congruent-direction path as pure LTR.
    // Mixed runs also exercise the reverse traversal of incongruent spans.
    let cases = [
        "Pack my box with five dozen liquor jugs. Sphinx of black quartz, judge my vow.",
        "Pack  my  box  with  five  dozen  liquor  jugs.  Sphinx  of  black  quartz.",
        "שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם",
        "Title שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם שלום עולם ending words",
        "שלום title words with many more words that cross several line breaks and continue שלום עולם",
    ];
    for text in cases {
        let shaped = ShapeLine::new(&mut fonts, text, &attrs, Shaping::Advanced, 8);
        for wrap in [Wrap::Word, Wrap::WordOrGlyph] {
            for width in [110.0, 180.0] {
                let ordinary = layout(&shaped, width, wrap, Ellipsize::None);
                for line_count in [2, 3] {
                    assert!(ordinary.len() > line_count, "fixture must overflow: {text}");
                    let limited = layout(
                        &shaped,
                        width,
                        wrap,
                        Ellipsize::End(EllipsizeHeightLimit::Lines(line_count)),
                    );
                    assert_eq!(limited.len(), line_count);
                    let expected = &ordinary[line_count - 1];
                    let actual = &limited[line_count - 1];
                    let first_source_byte = |line: &LayoutLine| {
                        line.glyphs
                            .iter()
                            .filter(|g| g.start != g.end)
                            .map(|g| g.start)
                            .min()
                            .unwrap()
                    };
                    let expected_start = first_source_byte(expected);
                    assert_eq!(
                        first_source_byte(actual), expected_start,
                        "separator from the preceding line leaked into {wrap:?}, width {width}, line {line_count}: {text}",
                    );
                    // Compare the anchor for single-direction text. In mixed
                    // runs, removing a suffix can legitimately move a retained
                    // glyph when the shortened bidi run is reordered.
                    let expected_glyph = expected
                        .glyphs
                        .iter()
                        .find(|g| g.start == expected_start)
                        .unwrap();
                    let actual_glyph = actual
                        .glyphs
                        .iter()
                        .find(|g| g.start == expected_start && g.start != g.end)
                        .unwrap();
                    if shaped
                        .spans
                        .iter()
                        .all(|span| span.level.is_rtl() == shaped.rtl)
                    {
                        assert!((expected_glyph.x - actual_glyph.x).abs() < 0.01,
                            "first glyph shifted for {wrap:?}, width {width}, line {line_count}: {text}");
                    }
                    assert!(actual
                        .glyphs
                        .iter()
                        .all(|g| g.x.is_finite() && g.w.is_finite()));
                    assert!(
                        actual.glyphs.iter().any(|g| g.start == g.end),
                        "fixture must show an ellipsis"
                    );
                }
            }
        }
    }
}

#[test]
fn authored_leading_spaces_are_preserved() {
    let mut fonts = font_system();
    let text = "   This title starts with intentional spaces and continues for several wrapped lines of text";
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Open Sans")));
    let shaped = ShapeLine::new(&mut fonts, text, &attrs, Shaping::Advanced, 8);
    let ordinary = layout(&shaped, 180.0, Wrap::WordOrGlyph, Ellipsize::None);
    for line_count in [1, 2, 3] {
        let limited = layout(
            &shaped,
            180.0,
            Wrap::WordOrGlyph,
            Ellipsize::End(EllipsizeHeightLimit::Lines(line_count)),
        );
        let first_word = text.find("This").unwrap();
        let glyph = |line: &LayoutLine| {
            line.glyphs
                .iter()
                .find(|g| g.start == first_word)
                .unwrap()
                .x
        };
        assert_eq!(limited[0].glyphs[0].start, 0);
        assert!(glyph(&limited[0]) > 0.0);
        assert_eq!(glyph(&limited[0]), glyph(&ordinary[0]));
    }
}

#[test]
fn an_overflowing_separator_stays_out_of_the_last_ellipsized_line() {
    let mut fonts = font_system();
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Open Sans")));
    // A word wider than the line is retained whole under Word wrapping. Its
    // following separator then overflows alone, rather than with the next
    // word. Repeated blanks exercise the same boundary at ordinary widths.
    for text in [
        "Supercalifragilisticexpialidocious and then more words that wrap across several lines",
        "Pack my box   with five dozen   liquor jugs. Sphinx of black quartz, judge my vow.",
    ] {
        let shaped = ShapeLine::new(&mut fonts, text, &attrs, Shaping::Advanced, 8);
        for wrap in [Wrap::Word, Wrap::WordOrGlyph] {
            for width in [60.0, 88.0, 110.0, 180.0] {
                let ordinary = layout(&shaped, width, wrap, Ellipsize::None);
                for count in [2, 3] {
                    if ordinary.len() <= count {
                        continue;
                    }
                    let limited = layout(
                        &shaped,
                        width,
                        wrap,
                        Ellipsize::End(EllipsizeHeightLimit::Lines(count)),
                    );
                    assert_eq!(limited.len(), count);
                    let start = |line: &LayoutLine| {
                        line.glyphs
                            .iter()
                            .filter(|glyph| glyph.start != glyph.end)
                            .map(|glyph| glyph.start)
                            .min()
                            .unwrap()
                    };
                    assert_eq!(
                        start(&limited[count - 1]),
                        start(&ordinary[count - 1]),
                        "overflowing separator: {wrap:?}, width {width}, count {count}: {text}"
                    );
                }
            }
        }
    }
}

#[test]
fn height_limited_ellipsis_keeps_wrapped_boundaries_in_both_text_directions() {
    let mut fonts = font_system();
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Open Sans")));
    // Height limits take a separate admission path from line-count limits.
    // Include overwide words in both congruent and reversed bidi spans, plus
    // authored indentation that must remain on the first, unelided line.
    for text in [
        "   Supercalifragilisticexpialidocious and then more words that wrap across several lines",
        "   שלוםשלוםשלוםשלוםשלוםשלום עולם ועוד מילים ועוד מילים ועוד מילים ועוד מילים",
        "שלום Supercalifragilisticexpialidocious and then more words שלום עולם שלום עולם",
    ] {
        let shaped = ShapeLine::new(&mut fonts, text, &attrs, Shaping::Advanced, 8);
        for wrap in [Wrap::Word, Wrap::WordOrGlyph] {
            for width in [60.0, 110.0, 180.0] {
                let ordinary = layout(&shaped, width, wrap, Ellipsize::None);
                for count in [2, 3] {
                    if ordinary.len() <= count {
                        continue;
                    }
                    let limited = layout(
                        &shaped,
                        width,
                        wrap,
                        Ellipsize::End(EllipsizeHeightLimit::Height(18.0 * count as f32)),
                    );
                    assert_eq!(limited.len(), count);
                    // Preceding rows have not been elided or re-ordered. Their
                    // original source clusters and positions must be identical.
                    for (actual, expected) in
                        limited[..count - 1].iter().zip(&ordinary[..count - 1])
                    {
                        let geometry = |line: &LayoutLine| {
                            line.glyphs
                                .iter()
                                .map(|glyph| (glyph.start, glyph.end, glyph.x, glyph.w))
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(geometry(actual), geometry(expected));
                    }
                    let source_start = |line: &LayoutLine| {
                        line.glyphs
                            .iter()
                            .filter(|glyph| glyph.start != glyph.end)
                            .map(|glyph| glyph.start)
                            .min()
                            .expect("the fixture retains text before its ellipsis")
                    };
                    assert_eq!(
                        source_start(&limited[count - 1]),
                        source_start(&ordinary[count - 1]),
                        "height-limited boundary: {wrap:?}, width {width}, count {count}: {text}"
                    );
                    assert!(
                        limited[count - 1]
                            .glyphs
                            .iter()
                            .any(|glyph| glyph.start == glyph.end),
                        "height-limited fixture must show an ellipsis"
                    );
                    assert!(limited.iter().flat_map(|line| &line.glyphs).all(|glyph| {
                        glyph.x.is_finite() && glyph.y.is_finite() && glyph.w.is_finite()
                    }));
                }
            }
        }
    }
}

#[test]
fn ellipsizing_inside_a_reversed_word_measures_only_its_remaining_glyphs() {
    let mut fonts = font_system();
    let attrs = AttrsList::new(&Attrs::new().family(Family::Name("Open Sans")));
    let ellipsis = ShapeLine::new(&mut fonts, "\u{2026}", &attrs, Shaping::Advanced, 8);
    let ellipsis_width = layout(&ellipsis, 500.0, Wrap::None, Ellipsize::None)[0]
        .glyphs
        .iter()
        .map(|glyph| glyph.w)
        .sum::<f32>();
    // The first strong character sets the paragraph direction; the long
    // unbroken word is in the opposite direction. The last admitted row starts
    // inside that word rather than at one of its word boundaries.
    for text in [
        "A שלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלוםשלום",
        "א SupercalifragilisticexpialidociousSupercalifragilisticexpialidociousSupercalifragilisticexpialidocious",
    ] {
        let shaped = ShapeLine::new(&mut fonts, text, &attrs, Shaping::Advanced, 8);
        assert!(shaped.spans.iter().any(|span| span.level.is_rtl() != shaped.rtl));
        for wrap in [Wrap::Glyph, Wrap::WordOrGlyph] {
            // The smallest width reserves all but a fraction of a pixel for
            // the ellipsis, so no remaining text glyph can fit. This also
            // exercises the empty partial-word range without an inverted slice.
            for width in [ellipsis_width + 0.01, 60.0, 110.0] {
                let ordinary = layout(&shaped, width, wrap, Ellipsize::None);
                for count in [2, 3] {
                    assert!(ordinary.len() > count);
                    for limit in [
                        EllipsizeHeightLimit::Lines(count),
                        EllipsizeHeightLimit::Height(18.0 * count as f32),
                    ] {
                        let limited = layout(&shaped, width, wrap, Ellipsize::End(limit));
                        assert_eq!(limited.len(), count);
                        let last = &limited[count - 1];
                        assert!(last.glyphs.iter().any(|glyph| glyph.start == glyph.end),
                            "missing ellipsis inside reversed word: {wrap:?}, {width}, {count}: {text}");
                        let actual_width = last.glyphs.iter().map(|glyph| glyph.w).sum::<f32>();
                        assert!(actual_width <= width + 0.01,
                            "remaining word escaped its measured width: {actual_width} > {width}");
                        for glyph in &last.glyphs {
                            assert!(glyph.x.is_finite() && glyph.w.is_finite());
                            assert!(glyph.start <= glyph.end && glyph.end <= text.len());
                            assert!(text.is_char_boundary(glyph.start));
                            assert!(text.is_char_boundary(glyph.end));
                            if glyph.start != glyph.end {
                                assert!(ordinary[count - 1].glyphs.iter().any(|expected| {
                                    expected.start == glyph.start && expected.end == glyph.end
                                }), "a glyph from another wrapped row leaked into the ellipsis row");
                            }
                        }
                    }
                }
            }
        }
    }
}
