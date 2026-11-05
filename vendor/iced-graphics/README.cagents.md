# Rich paragraph ellipsis patch

Source: `iced/graphics` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/graphics), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The standalone manifest preserves the pinned dependency graph and feature defaults.

`Paragraph::with_spans` now applies the requested ellipsis strategy to the native text buffer, matching `Paragraph::with_text`. The pinned implementation stores the strategy but never applies it to rich paragraphs, so a highlighted one-line label clips instead of showing an ellipsis. This correction preserves source spans and avoids measuring/truncating labels on the application update thread.

The application regression compares native plain/rich paragraph bounds at narrow widths and after resizing. Native palette validation must also exercise highlighted long labels. Remove this patch when the pinned upstream renderer applies equivalent rich-text ellipsis.
