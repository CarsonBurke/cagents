# Brand marks

## Application icons

| cagents | ceditor |
| :-: | :-: |
| <img src="../../packaging/dev.cagents.Cagents.svg" width="128" alt="cagents icon"> | <img src="../../packaging/io.github.cagents.ceditor.svg" width="128" alt="ceditor icon"> |

Both icons share the same tactile 2.5D brushed-titanium switchboard chassis
and recessed obsidian bay, following GNOME Circle / Libadwaita orthographic
proportions.

- **cagents** houses three chunky vertical workspace modules — deep slate-navy,
  cool teal-mint, and a focal coral-orange active lane etched with a crisp `>`
  prompt chevron — communicating parallel coding agents with one workspace in
  focus.
- **ceditor** uses the same titanium chassis with a slim slate-navy gutter rail
  beside a warm-ivory tactile editorial page, chunky syntax bars, and a focal
  coral-orange caret so the two icons are unmistakable at a glance on a dock.

At tray and task bar sizes (16-22 px) both marks remain crisp and distinct:
three coloured vertical lanes vs. a light editorial page with a dark gutter.

## Construction

- Chassis: 108 × 101 px rounded rectangle (`rx = 18`) with an 8 px orthographic
  bottom lip (`#8A847D` → `#58544F`) and brushed warm-titanium face
  (`#DCD8D2` → `#918B83`) over a recessed obsidian bay (`#111722` → `#1D2534`).
- Modules: 2.5D tactile keycaps (`rx = 5.5–6`) with subtle top rim highlights
  and grounded bottom edges.
- Palette: deep slate-navy (`#3C4B66` → `#2A364B`), cool teal-mint
  (`#60B2A4` → `#469689`), warm ivory (`#FAF8F4` → `#EAE6DE`), and focal
  coral-orange (`#FF7847` → `#E64F1E`).

## Files

- `packaging/dev.cagents.Cagents.svg` is the source of truth for the cagents
  icon. `packaging/dev.cagents.Cagents.png` is its 128 px raster, embedded as
  the window and tray icon; regenerate it from the SVG whenever the SVG
  changes.
- `packaging/io.github.cagents.ceditor.svg` is the source of truth for the
  ceditor icon, embedded in the UI directly. Its 128 px raster,
  `packaging/io.github.cagents.ceditor.png`, is embedded as the native editor
  window icon; regenerate it whenever the SVG changes.

From the repository root, regenerate the PNG with librsvg's `rsvg-convert`:

```sh
rsvg-convert --width 128 --height 128 \
  --output packaging/dev.cagents.Cagents.png packaging/dev.cagents.Cagents.svg
rsvg-convert --width 128 --height 128 \
  --output packaging/io.github.cagents.ceditor.png packaging/io.github.cagents.ceditor.svg
```

Keep the transparent background; the window and tray loaders expect a 128×128
RGBA PNG.

## Alternates

Kept for reference; not shipped.

| Cascade | Orbit | Stack |
| :-: | :-: | :-: |
| <img src="concept-cascade.svg" width="96" alt=""> | <img src="concept-orbit.svg" width="96" alt=""> | <img src="concept-stack.svg" width="96" alt=""> |
| A prompt running ahead of its own trail. The boldest silhouette in a dock, but reads as "fast forward". | Agents orbiting a core. Polished, but says "space app". | Layered sessions. Reads as storage or a design tool. |
