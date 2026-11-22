# Rich paragraph ellipsis patch

Source: `iced/graphics` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/graphics), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The standalone manifest preserves the pinned dependency graph and feature defaults.

`Paragraph::with_spans` now applies the requested ellipsis strategy to the native text buffer, matching `Paragraph::with_text`. The pinned implementation stores the strategy but never applies it to rich paragraphs, so a highlighted one-line label clips instead of showing an ellipsis. This correction preserves source spans and avoids measuring/truncating labels on the application update thread.

The application regression compares native plain/rich paragraph bounds at narrow widths and after resizing. Native palette validation must also exercise highlighted long labels. Remove this patch when the pinned upstream renderer applies equivalent rich-text ellipsis.

# Lazy editor construction

`Editor::with_text` (`src/text/editor.rs`) no longer shapes the text it is given. The pinned constructor shapes every line, because the new buffer has no height yet, and it does so under the process-wide font write lock that every other text layout waits on. The first `update` usually discards that shaping anyway, because it gives the lines the widget's font; it also sizes the buffer, and then shapes only what its viewport shows. Restoring or recalling a 600 KB chat draft held the UI thread for about 300 ms in an optimized build (#2973); the file editor had worked around the same constructor since #675.

`editor_widget::tests::native_constructor_defers_shaping_to_the_viewport` checks that a long draft stays unshaped until the editor is laid out, and that laying it out shapes only its viewport. Remove this patch when the pinned upstream constructor stops shaping before it has bounds.

# Relative span size

`text::to_span_attributes` converts a span to the cosmic-text attributes it
is shaped with. It takes the span's font size and line height from
`Span::metrics` (`vendor/iced-core/README.cagents.md`), so a span with a
relative size is shaped at that size on the paragraph's line height, and it
converts `Span::inline_padding` to EM at the span's size.
`Paragraph::with_spans` and the app's own shaping for long paragraphs both use
it, so they agree. Spans without these are shaped as upstream shapes them.

# Editor overscan (#2824)

`Editor::set_overscan` forwards to cosmic-text's `Buffer::set_overscan`, and `Editor::overflow` reports how far the text continues above and below the text area, up to the buffer's line height. It counts what is scrolled out of view above and what is left below from the scroll position and line layouts, counting a line that is not laid out as one line height, so it does not depend on what the overscan draws (cagents #3044). `Editor::caret` is the caret on its drawn line, with the buffer's line height, and `None` while a range is selected or the caret's line is not drawn. Selection regions, the caret and the highlighter window cover the overscan bands too, so a selection or highlight continues into the padding. See vendor/cosmic-text/LOCAL_CHANGES.md; the app regression is `editor_widget::tests::native_overscan_draws_text_past_the_viewport_and_reports_how_far`.

# Drawn-text identity (#3564)

`paragraph::Weak::is_same_live` and `editor::Weak::is_same_live` tell whether two weak references refer to the same live paragraph or editor. A paragraph is reshaped with `Arc::make_mut` and an editor is edited by unwrapping and rewrapping its `Arc`, so either moves to a new allocation while a frame's weak reference is outstanding; the same reference therefore means the same contents. iced_wgpu uses this to reuse a frame's prepared text (vendor/iced-wgpu/README.cagents.md). `cache::Cache::keep` keeps an entry through the next trim without allocating it, so a group whose glyphs are reused keeps its shaped cached text. `drawn_identity` in `crates/iced-contracts` covers both.

# Layer stack revision (#3581)

`layer::Stack::revision` changes whenever the stack's layers may have: on a
reset, a pushed clip, or a borrow of the current layer to draw. Merging leaves
it alone: the renderer reads the revision first and merges only what it is
about to prepare, since merging merged layers can still move primitives
between layers with different clips. iced_wgpu presents
unchanged layers again without preparing them while it holds
(vendor/iced-wgpu/README.cagents.md). `layer_revision` in
`crates/iced-contracts` covers it.

# Presentation timing

`present` (`src/present.rs`) times one presentation by stage: acquiring the
surface image, preparing and rendering the layers, finishing and submitting
the command buffer and swapping, with the presenting thread's CPU time over
all of it and the number of layers drawn. The
shell brackets each presentation with `begin` and `end` only while the
application traces frames (vendor/iced-winit/README.cagents.md), and the wgpu
compositor marks each stage with `lap` (vendor/iced-wgpu/README.cagents.md).
Until a shell enables it, each call touches one thread-local cell and reads no
clock.
It answers whether a cheap frame's presentation is CPU encoding or waiting on
the GPU or the compositor. `present_timing` in `crates/iced-contracts` covers
it. Remove it when the pinned dependency offers
equivalent timing.
