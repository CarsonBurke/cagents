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
