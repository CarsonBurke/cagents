# Brand marks

## Application icons

| cagents | ceditor |
| :-: | :-: |
| <img src="../../packaging/dev.cagents.Cagents.svg" width="128" alt="cagents icon"> | <img src="../../packaging/io.github.cagents.ceditor.svg" width="128" alt="ceditor icon"> |

Both icons are obsidian panes in the GNOME Circle style: physical objects with
a 2.5D bottom edge, a soft drop shadow and a coloured top edge, rather than
glyphs on a tile.

- **cagents** is an active session pane, edged in coral, carrying an agent
  robot, with a teal-edged session pane behind it, offset up and to the right.
  It reads as an agent at work with more sessions running alongside.
- **ceditor** is a fan of obsidian code pages. The teal-edged front page shows
  a gutter, syntax-coloured lines and a highlighted active line ending in a
  coral I-beam caret; coral- and slate-edged pages sit behind it.

From 24 px up the marks stay distinct: a coral robot in a square pane against
a fanned stack of lined pages. At 16 px they reduce to their colours, an orange
mark on a dark pane against a dark page with a teal top.

## Construction

- Panes and pages: obsidian face (`#3B4660` → `#2A3245` → `#222939`, back pane
  `#303A50` → `#1C2230`) over a 4-5 px darker edge (`#11151F`), with a white rim
  light and a coloured top edge (8 px on panes, 15 px on pages; `rx = 8-10`).
  Everything casts a soft shadow; the cagents front pane casts it to the right
  onto the pane behind.
- Robot: coral head (`#FF8A55` → `#EE5A24`, side `#B83D16`) with a dark visor,
  teal eyes, slate ears and an antenna.
- Palette: coral (`#FF7A48`), teal (`#6CC2B3`, `#74CCBC` → `#46988A`), slate
  (`#8EA4C8`, `#7C8FB2` → `#5A6B8C`) and obsidian.
- Both drawings are scaled to fill the 120 px live area COSMIC's own icons use,
  keeping 4 px of the 128 px grid clear on every side, shadows included.

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
| A prompt running ahead of its own trail. The boldest silhouette in a dock, but reads as "fast forward". | Agents orbiting a core. Polished, but says "space app". | Sessions as flat stacked slabs. Without a figure on the front one, reads as storage or a design tool. |
