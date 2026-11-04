# Rich paragraph ellipsis patch

Source: `iced/graphics` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/graphics), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The standalone manifest preserves the pinned dependency graph and feature defaults.

`Paragraph::with_spans` now applies the requested ellipsis strategy to the native text buffer, matching `Paragraph::with_text`. The pinned implementation stores the strategy but never applies it to rich paragraphs, so a highlighted one-line label clips instead of showing an ellipsis. This correction preserves source spans and avoids measuring/truncating labels on the application update thread.

The application regression compares native plain/rich paragraph bounds at narrow widths and after resizing. Native palette validation must also exercise highlighted long labels. Remove this patch when the pinned upstream renderer applies equivalent rich-text ellipsis.
Additional native editor changes:

Local addition: text::Editor::set_tab_width forwards to cosmic-text's native
buffer, invalidating selection geometry and line highlighting. This lets the
application change tab stops without rewriting source bytes or hit-test columns.

Local addition: text::Editor::selection_bounds exposes native expanded word/line
selection positions without allocating or normalizing a selected-text copy.
