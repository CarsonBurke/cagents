# Crisp SVG sampling patch

Source: `iced/wgpu` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/wgpu), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The standalone manifest preserves the pinned dependency graph and feature defaults outside the upstream workspace.

The pinned renderer draws every SVG with the nearest-texel sampler and rounds each quad corner to the pixel grid on its own. An SVG is rasterized at `ceil(logical size × scale)` physical pixels, so:

- An upright icon whose physical size is fractional, such as 14 px at 125%, is drawn into a quad one pixel narrower or wider than its raster, depending on where it sits. Nearest sampling then drops or repeats a row or column of texels, and the icon looks jagged.
- A rotated SVG, such as the activity spinner, is resampled off the pixel grid with nearest sampling, which turns its antialiased edges into steps.

Upright vectors are now drawn from the physical pixel nearest their origin with exactly their raster's extent, so nearest sampling maps one texel to one pixel. Rotated vectors use the linear sampler without snapping. A tinted vector now gives its color to every texel, transparent ones included, so filtering does not blend black into its edges.

Raster images are unchanged. `an_upright_vector_covers_exactly_its_raster` covers the alignment; native zoomed crops at fractional scales cover the result. Remove this patch when the pinned upstream renderer samples SVGs equivalently.

Run the regression as a standalone package:

```sh
cargo test --manifest-path vendor/iced-wgpu/Cargo.toml --features svg an_upright_vector
```

Cargo cannot run it with `-p iced_wgpu` from the parent workspace because this vendored package is not a workspace member. Its standalone lockfile and build artifacts are local test output.
