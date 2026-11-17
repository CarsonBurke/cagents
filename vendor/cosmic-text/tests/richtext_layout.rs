use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

// Test for https://github.com/pop-os/cosmic-text/issues/364
//
// Empty lines at the start/end of a span should use that span's line height,
// not the buffer's default line height.
#[test]
fn empty_lines_use_span_metrics() {
    let mut font_system = FontSystem::new();

    let metrics = Metrics::new(32.0, 44.0);
    let mut buffer = Buffer::new(&mut font_system, metrics);
    let mut buffer = buffer.borrow_with(&mut font_system);

    let attrs = Attrs::new();
    let small_attrs = attrs.clone().metrics(Metrics::relative(8.0, 1.2));

    // The empty lines from \n\n at start and end should use 8.0 * 1.2 = 9.6 line height.
    // All newlines are inside the small_attrs span so the empty lines are clearly within it.
    buffer.set_rich_text(
        [
            ("Before", attrs.clone()),
            ("\n\n\nSmall\n\n", small_attrs),
            ("After", attrs.clone()),
        ],
        &attrs,
        Shaping::Advanced,
        None,
    );
    buffer.set_size(Some(500.0), Some(500.0));

    let line_heights: Vec<f32> = buffer.layout_runs().map(|run| run.line_height).collect();

    // line_heights should be:
    // [0] "Before"   -> 44.0 (buffer default)
    // [1] "" (empty)  -> 9.6  (small span metrics: 8.0 * 1.2)
    // [2] "" (empty)  -> 9.6  (small span metrics: 8.0 * 1.2)
    // [3] "Small"     -> 9.6  (small span metrics from glyphs)
    // [4] "" (empty)  -> 9.6  (small span metrics: 8.0 * 1.2)
    // [5] "After"     -> 44.0 (buffer default)
    assert_eq!(
        line_heights.len(),
        6,
        "expected 6 layout runs, got {}",
        line_heights.len()
    );
    assert!(
        (line_heights[0] - 44.0).abs() < 0.1,
        "line 0 should use buffer default: {}",
        line_heights[0]
    );
    assert!(
        (line_heights[1] - 9.6).abs() < 0.1,
        "line 1 (empty) should use span metrics: {}",
        line_heights[1]
    );
    assert!(
        (line_heights[2] - 9.6).abs() < 0.1,
        "line 2 (empty) should use span metrics: {}",
        line_heights[2]
    );
    assert!(
        (line_heights[4] - 9.6).abs() < 0.1,
        "line 4 (empty) should use span metrics: {}",
        line_heights[4]
    );
    assert!(
        (line_heights[5] - 44.0).abs() < 0.1,
        "line 5 should use buffer default: {}",
        line_heights[5]
    );
}

// A visual line holding only smaller text, such as a line of inline code set
// a size below the prose around it but on the prose's line height, keeps the
// baseline of the lines around it: every line reserves the default font's
// ascent and descent, like CSS's strut (cagents#3002).
#[test]
fn a_line_of_only_smaller_text_keeps_the_paragraph_baseline() {
    use cosmic_text::Family;

    let mut font_system = FontSystem::new();
    let mut buffer = Buffer::new(&mut font_system, Metrics::new(14.0, 19.6));
    let mut buffer = buffer.borrow_with(&mut font_system);
    let attrs = Attrs::new();
    let code = attrs
        .clone()
        .family(Family::Monospace)
        .metrics(Metrics::new(12.6, 19.6));
    buffer.set_rich_text(
        [
            ("Prose before\n", attrs.clone()),
            ("[features].multi_agent_v2", code.clone()),
            ("\nprose with ", attrs.clone()),
            ("code", code),
            (" after", attrs.clone()),
        ],
        &attrs,
        Shaping::Advanced,
        None,
    );
    buffer.set_size(Some(500.0), None);

    let baselines: Vec<f32> = buffer
        .layout_runs()
        .map(|run| run.line_y - run.line_top)
        .collect();
    assert_eq!(baselines.len(), 3);
    for baseline in &baselines {
        assert!(
            (baseline - baselines[0]).abs() < 0.01,
            "every line shares the prose baseline: {baselines:?}"
        );
    }
}

// A line whose spans set a line height too short for the default font is
// sized by those spans, not by the paragraph's default, so it keeps centring
// on its own glyphs rather than reserving a strut that would push them out of
// the line box.
#[test]
fn a_line_sized_shorter_than_the_default_font_keeps_its_own_centre() {
    let mut font_system = FontSystem::new();
    let mut buffer = Buffer::new(&mut font_system, Metrics::new(32.0, 44.0));
    let attrs = Attrs::new();
    let small = attrs.clone().metrics(Metrics::new(10.0, 14.0));
    buffer.set_rich_text(
        [("small text", small)],
        &attrs,
        Shaping::Advanced,
        None,
    );
    buffer.set_size(Some(500.0), None);
    buffer.shape_until_scroll(&mut font_system, false);

    let run = buffer.layout_runs().next().expect("one line");
    assert_eq!(run.line_height, 14.0);
    let glyph = &run.glyphs[0];
    let font = font_system
        .get_font(glyph.font_id, glyph.font_weight)
        .expect("shaped font");
    let metrics = font.metrics();
    let em = f32::from(metrics.units_per_em);
    let (ascent, descent) = (metrics.ascent / em * 10.0, -metrics.descent / em * 10.0);
    let centred = (14.0 - (ascent + descent)) / 2.0 + ascent;
    let baseline = run.line_y - run.line_top;
    assert!(
        (baseline - centred).abs() < 0.01,
        "baseline {baseline} against {centred}"
    );
}

// Inline padding is laid out at a span's start and end, inside its edge
// glyphs' boxes, and moves the text after it (cagents#3002).
#[test]
fn inline_padding_lays_out_room_at_both_edges_of_a_span() {
    let mut font_system = FontSystem::new();
    let mut buffer = Buffer::new(&mut font_system, Metrics::new(20.0, 28.0));
    let attrs = Attrs::new();
    let mut layout = |padded: Attrs<'_>| {
        buffer.set_rich_text(
            [
                ("a ", attrs.clone().metadata(0)),
                ("code", padded.metadata(1)),
                (". b", attrs.clone().metadata(2)),
            ],
            &attrs,
            Shaping::Advanced,
            None,
        );
        buffer.set_size(Some(500.0), None);
        buffer.shape_until_scroll(&mut font_system, false);
        let run = buffer.layout_runs().next().expect("one line");
        run.glyphs
            .iter()
            .map(|glyph| (glyph.metadata, glyph.x, glyph.w, glyph.x_offset))
            .collect::<Vec<_>>()
    };
    let plain = layout(attrs.clone());
    let padded = layout(attrs.clone().inline_padding(0.25));
    let room = 0.25 * 20.0;
    let code = |glyphs: &[(usize, f32, f32, f32)]| {
        glyphs
            .iter()
            .copied()
            .filter(|glyph| glyph.0 == 1)
            .collect::<Vec<_>>()
    };
    let (before, after) = (code(&plain), code(&padded));
    assert_eq!(before.len(), 4);
    // The first letter's box starts where it did and draws past the room.
    assert_eq!(after[0].1, before[0].1);
    assert!((after[0].2 - before[0].2 - room).abs() < 0.01);
    assert!((after[0].3 * 20.0 - room).abs() < 0.01);
    // Middle letters only move along; the last one gains the closing room.
    for (before, after) in before.iter().zip(&after).skip(1) {
        assert!((after.1 - before.1 - room).abs() < 0.01);
        assert_eq!(after.3, before.3);
    }
    assert!((after[3].2 - before[3].2 - room).abs() < 0.01);
    // The text after the span moves by both edges.
    let period = |glyphs: &[(usize, f32, f32, f32)]| {
        glyphs.iter().find(|glyph| glyph.0 == 2).unwrap().1
    };
    assert!((period(&padded) - period(&plain) - 2.0 * room).abs() < 0.01);
}
