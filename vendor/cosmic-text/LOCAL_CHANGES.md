Based on crates.io cosmic-text 0.19.0, checksum
be17b688510d934ce13f48a2beba700e11583e281e0fda99c22bb256a14eda73.
Source, upstream test targets, font fixtures and licenses are retained. Source
differs only by the additive API below. Screenshot documentation is
omitted.

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
