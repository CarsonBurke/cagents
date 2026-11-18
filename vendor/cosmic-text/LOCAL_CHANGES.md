Based on crates.io cosmic-text 0.19.0, checksum
be17b688510d934ce13f48a2beba700e11583e281e0fda99c22bb256a14eda73.
Source, upstream test targets, font fixtures and licenses are retained. Local
changes combine the additive snapshot API below, wrapped ellipsis corrections,
a checkout identity anchor and font matching without reading font files.
Screenshot documentation is omitted.

Local API: immutable full-context LayoutSnapshot and visual projections.
This is not an incremental shaper. Creating a snapshot requires existing complete
layout and consumes Buffer, moving its layout vectors while releasing duplicate
ShapeLine caches. It rejects unshaped lines without shaping them. project copies
only selected visual-line glyphs into a display Buffer, preserving glyph IDs,
fonts, metadata, decorations, Bidi levels, x/y positions and original source-byte
mapping. Generated cluster ranges are checked before UTF-8 slicing. There is no
unsafe code or public arbitrary layout-cache injection. Projection buffers are
read-only display products; edits/resizes require rebuilding from the source.

Why an additive dependency API is required: BufferLine owns private shape/layout
caches, Paragraph.buffer is immutable, and shape_until_scroll culls physical
paragraphs rather than wrapped visual lines. Raw Weak<Buffer> alone still walks
all preceding wrapped visual lines. Arbitrary source chunks alter soft wraps,
ligatures, Arabic joining and paragraph Bidi context. This API keeps source
shaping intact and provides a bounded display projection without pretending that
initial shaping or the full snapshot's glyph memory are viewport bounded.

Changes to this API require crate tests, isolated browserless and integrated UI
checks, and application Paragraph geometry fixtures. Native selection, links and
Find must retain their source semantics. Measure CPU, input latency and process
RSS/PSS during opening, streaming, scrolling and hide/reveal; bounded display
projection alone does not establish those results. See docs/performance.md in
the application repository for the workloads and residency budgets. Upstream
adoption and application integration require separate review.

Wrapped ellipsis and checkout identity (cagents1473 / PR1483)

The union also backports the final wrapped-ellipsis separator correction from
peer ecfe42e7c30d9c26dd780cbe9fd2f2dde4570f19, based on cosmic-text PR535
head0c5d9c507798bcb779e5fbdb48ae339face06f9c, plus the peer's local
overwide-blank boundary extension. Only src/shape.rs changes glyph continuation;
no source text is trimmed. Library src/lib.rs adds the standard compile-time
CAGENTS_CHECKOUT_ROOT identity anchor, paired with the source-hook entry in
scripts/qualify-checkout-cache.py. Snapshot constructors, full-context ownership,
projections and original source-byte mappings remain unchanged from PR1316.

All eight upstream integration-test targets, both benchmarks, development
dependencies, test profile, fonts and rendering fixtures remain available. The
wrapped_ellipsis target adds the peer's three bundled-font regressions; the UI
reexport regression checks the application's selected dependency. This union
adds no new library dependency or another root Cargo patch; the existing local
cosmic-text selection and UI unicase requirement remain unchanged.

The shape correction is confined to wrapped line/height-limited ellipsis.
Multiline End, Start and Middle share continuation handling; the new focused
End controls do not independently qualify every Start/Middle/Bidi geometry.
The retained upstream oracles and final composed UI/native/cache qualification
remain required. Prior separate PR1316/1473 execution is historical evidence,
not a claim that this source union has been built or run. See PROVENANCE.md.

Font matching without reading font files

Upstream FontMatchKey::new opens every face whose weight differs from the
requested one to read its wght axis, so the first match of each family, weight,
style and stretch in a FontSystem reads every installed font file: 1588 faces
on the build host, 73 to 117 ms each time, on the thread that shapes. FontSystem now asks a face for its weight axis only when fallback
considers that face, and remembers the answer per face until the database
changes. Matching and fallback choose the same faces as before: a face of the
requested weight, or a variable face whose wght range covers it, in the same
order. A face whose file cannot be read counts as not variable until the
database changes, where upstream would read it again at the next new match.
src/font/system/weight_match_tests.rs checks that matching reads no font file,
and src/font/system/fallback_choice_tests.rs that every fallback choice over
the repository fonts, for each family, weight, style and script, is the one
upstream made.

Line strut

Every visual line reserves at least the ascent and descent of its line's
default font, like CSS's strut. Upstream centres each line in its line height
on the tallest glyphs it holds. A line holding only smaller or fallback
glyphs was therefore centred on those, and its baseline moved off the pitch
of the lines around it. The visible case was a wrapped line of nothing but
inline code, set a step below the prose but on the prose's line height. That
line sat half a pixel higher than its neighbours (cagents#3002).
ShapeLine::build looks up the default attrs' first font, which shaping
would choose for them, once per line through the cached font matches.
Layout raises each visual line's maximum ascent and descent to that font's at
the line's font size. A line that contains the default font's glyphs is
unchanged, because they already reach those maxima. The strut is skipped on
a line whose own spans set a line height shorter than the default font's
glyphs. Such a line is sized by its spans rather than by the default, as when
every span of a rich text sets a smaller size, and a strut would push its
glyphs out of the box, so it still centres on its own glyphs. A line sized
by the buffer's metrics is set in the default font and takes the strut.
tests/richtext_layout.rs
a_line_of_only_smaller_text_keeps_the_paragraph_baseline fails without the
strut, and a_line_sized_shorter_than_the_default_font_keeps_its_own_centre
covers the skipped case.

Lines that hold only fallback glyphs move onto the default font's baseline
too. That also applies outside chat, for example a terminal or editor row of
only emoji or CJK, which now lines up with rows that hold the default font.

Inline padding

Attrs::inline_padding reserves room in EM at a span's logical start and end,
like CSS inline padding. ShapeLine::build widens the span's first and last
glyph advances by it and shifts the visually leftmost one's x_offset, so the
room is laid out, wraps with the edge glyphs and lies inside their boxes,
where highlight bounds computed from glyph boxes cover it. cagents uses it for
the room around inline code inside its chip (cagents#3002). Shaping runs
break at a padded span's edges even when its font matches its neighbours', so
no ligature or cluster crosses them and the edges are always glyph
boundaries. One pass over a line's glyphs looks each glyph's edges up among
the sorted padded ranges. A padded span that crosses a hard line break is
padded at both ends of each line, like CSS's `box-decoration-break: clone`.
Room on a space that hangs at a wrap is not laid out, as that space is not.
tests/richtext_layout.rs inline_padding_lays_out_room_at_both_edges_of_a_span,
inline_padding_survives_a_same_font_ligature_edge and
inline_padding_of_ltr_code_in_an_rtl_paragraph cover it. Monospace width matching rounds advances to the cell width, so the
room is not meant for monospace-matched editors.

All upstream targets and reference images still pass.

Linear multiline insertion and deletion (cagents #3001)

Upstream Editor::insert_at inserts each new line at the same index of
Buffer::lines, which moves every line inserted before it, and delete_range
removes lines one at a time while collecting them at the front of the change.
Both are quadratic in the number of lines. Pasting 600 KiB of 80-byte lines
into the chat composer took about 170 ms, and deleting a selection that size
about 150 ms, while iced held the font system lock. insert_at now builds the
new lines and splices them in once, and delete_range drains the removed lines
once. Lines, endings, attribute splits, the returned cursor and the recorded
change are the same as upstream's. The application test
repository_widget::text_editor::tests::native_multiline_insert_and_delete_keep_lines_order_and_history
checks them, including undoing both changes.

Overscan for edge fades (cagents #2824)

`Buffer::set_overscan(above, below)` adds bands above and below the viewport. The lines in those bands are shaped and laid out, and `Buffer::draw_runs` yields them along with the visible runs, with negative `line_top` above the viewport and values past the height below it. `hit` uses `draw_runs`, so a click in a band lands on the line drawn there. `layout_runs`, scrolling, cursor reveal, PageUp/Down and the measured content height still use only the viewport, so the overscan never changes where the editor scrolls or how large it measures. The default overscan is zero, which keeps upstream behavior.

Why: an editor inside padding cut its text at the padding's inner edge, so a scrolled draft looked sliced in half (#2824). Drawing the neighboring lines into the padding lets the app fade them out instead.

Coverage: `editor_widget::tests::native_overscan_draws_text_past_the_viewport_and_reports_how_far` in cagents-ui. Remove this patch when upstream can draw text past the scroll viewport.
