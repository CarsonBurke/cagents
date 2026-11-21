# Crisp SVG sampling patch

Source: `iced/wgpu` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/wgpu), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The standalone manifest preserves the pinned dependency graph and feature defaults outside the upstream workspace.

The pinned renderer draws every SVG with the nearest-texel sampler and rounds each quad corner to the pixel grid on its own. An SVG is rasterized at `ceil(logical size × scale)` physical pixels, so:

- An upright icon whose physical size is fractional, such as 14 px at 125%, can be drawn into a quad one pixel smaller than its raster, depending on where it sits. Nearest sampling then skips a row or column of texels, and the icon looks jagged.
- A rotated SVG, such as the activity spinner, is resampled off the pixel grid with nearest sampling, which turns its antialiased edges into steps.

Upright vectors are now drawn from the physical pixel nearest their origin with exactly their raster's extent, so nearest sampling maps one texel to one pixel. Rotated vectors use the linear sampler without snapping. A tinted vector now gives its color to every texel, transparent ones included, so filtering does not blend black into its edges.

Raster images are unchanged. `an_upright_vector_covers_exactly_its_raster` covers the alignment; native zoomed crops at fractional scales cover the result. Remove this patch when the pinned upstream renderer samples SVGs equivalently.

Run the regression as a standalone package:

```sh
scripts/cargo-gate test --manifest-path vendor/iced-wgpu/Cargo.toml --features svg an_upright_vector
```

Cargo cannot run it with `-p iced_wgpu` from the parent workspace because this vendored package is not a workspace member. Its standalone lockfile and build artifacts are local test output.

# Editor overscan (#2824)

The editor's text area iterates `Buffer::draw_runs` instead of `layout_runs`, so the lines in the overscan bands reach the glyph renderer. The clip still comes from the widget. iced_tiny_skia is not vendored, and the pinned copy draws `layout_runs`, so the software fallback draws no text in the bands. The selection regions, caret and the app's fade come from iced-graphics and the widget, so they still extend into the bands there, and a click in a band lands on a line that renderer did not draw.

# Reuse of unchanged text (#3564)

`text::State::prepare` keeps, for each text group it prepares, the group's text, clip bounds, transformation and the viewport resolution. When the next frame draws a group that matches at the same position in the frame, it asks the group's renderer to `reuse` its glyphs (vendor/cryoglyph/README.cagents.md) instead of preparing them again, and prepares them as before if that fails. Paragraphs, editors and raw buffers compare by identity, using `is_same_live` from vendor/iced-graphics: none is changed in place while a frame refers to it. Cached text compares by value, and a reused group keeps its shaped cached text with `Cache::keep` (vendor/iced-graphics), as preparing it would. A group that is not drawn again releases what it kept when the frame is trimmed.

`drawn_identity` in `crates/iced-contracts` covers the identity contract; `mise run test:contracts` runs it. A native run with an activity spinner over a long transcript covers the reuse itself.
