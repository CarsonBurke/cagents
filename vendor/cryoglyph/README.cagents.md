# Reusable prepared glyphs

Source: [iced-rs/cryoglyph](https://github.com/iced-rs/cryoglyph/tree/e429a025df36ab8145708acb309080ae3deec17a), revision `e429a025df36ab8145708acb309080ae3deec17a`, the revision iced_wgpu pins. The upstream licenses and README are included; the examples, benchmarks and their dev-dependencies are left out. `src/lib.rs` carries the shared-checkout anchor.

`TextRenderer::prepare` walks every glyph of every text area each time it runs: it looks the glyph up in the atlas, marks it in use and writes its vertex. iced_wgpu prepares each layer's text on every frame, so a frame that only turns an activity spinner walked every visible glyph in the window. That was about a fifth of such a frame's time on the UI thread (#3564).

`TextRenderer::reuse` keeps what the last `prepare` drew for another frame. It marks each distinct glyph that `prepare` drew as in use and recently used, as `prepare` would, and returns `false` if one has left the atlas or moved within it since, for instance because another renderer evicted it while this one was not drawing. The caller then prepares again before rendering. `prepare` records each glyph it draws, with its atlas position; the first `reuse` deduplicates that list.

Atlas growth keeps every glyph's position, and the shader scales positions by the atlas size, so growth alone does not invalidate a reuse. The caller is responsible for knowing that the text areas, resolution and color mode are the ones `prepare` last saw; iced_wgpu compares them (vendor/iced-wgpu/README.cagents.md).

Remove this patch when the pinned upstream renderer can keep prepared text across frames.
