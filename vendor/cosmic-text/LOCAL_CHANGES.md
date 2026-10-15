Based on crates.io cosmic-text 0.19.0, checksum
be17b688510d934ce13f48a2beba700e11583e281e0fda99c22bb256a14eda73.
Source and licenses are unchanged except for the additive prototype API below.

Local prototype: immutable full-context LayoutSnapshot and visual projections.
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
ligatures, Arabic joining and paragraph Bidi context. This prototype keeps source
shaping intact and provides a bounded display projection without pretending that
initial shaping or the full snapshot's glyph memory are viewport bounded.

This concern is UNCOMPILED and UNMEASURED. API/source review, crate tests, isolated
UI/integrated dependency checks, exact app Paragraph geometry fixtures, native
selection/link/Find exercise and measured CPU/RSS budgets are still required.
Upstream adoption and application integration must be reviewed separately.
