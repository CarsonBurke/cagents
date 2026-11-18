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

`Editor::set_overscan` forwards to cosmic-text's `Buffer::set_overscan`, and `Editor::overflow` reports how far the drawn runs reach above and below the text area, clamped to the overscan. Selection regions, the caret and the highlighter window cover the overscan bands too, so a selection or highlight continues into the padding. See vendor/cosmic-text/LOCAL_CHANGES.md; the app regression is `editor_widget::tests::native_overscan_draws_text_past_the_viewport_and_reports_how_far`.
