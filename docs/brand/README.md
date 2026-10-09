# Brand marks

## Application icons

| cagents | ceditor |
| :-: | :-: |
| <img src="../../packaging/dev.cagents.Cagents.svg" width="96" alt="cagents icon"> | <img src="../../packaging/io.github.cagents.ceditor.svg" width="96" alt="ceditor icon"> |

The cagents mark is a geometric **C** cut flat on its open side, with a
terminal block cursor sitting in the opening: the environment wraps the
place where an agent is working. ceditor uses the same frame and grammar
with a squared bracket and an I-beam caret, so the pair reads as one family
while staying distinct in a dock or task switcher.

Both marks are built to survive at 16-22 px (tray, task bar, tab icons): one
solid light glyph, one accent element, no thin strokes or small detail.

## Palette

| Role | Value |
| --- | --- |
| Frame, top | `#2B3140` |
| Frame, bottom | `#11141C` |
| Frame edge | white at 14% opacity, keeps the tile visible on dark panels |
| Glyph | `#FFFFFF` to `#D9DEE8` |
| Accent | `#8CF7CF` to `#2EC5F2` |

## Files

- `packaging/dev.cagents.Cagents.svg` is the source of truth for the cagents
  icon. `packaging/dev.cagents.Cagents.png` is its 128 px raster, embedded as
  the window and tray icon; regenerate it from the SVG whenever the SVG
  changes.
- `packaging/io.github.cagents.ceditor.svg` is the ceditor icon, embedded in
  the UI directly.

## Alternates

Kept for reference; not shipped.

| Accent tile | Lanes | Orchestrator | Panes |
| :-: | :-: | :-: | :-: |
| <img src="cagents-accent.svg" width="72" alt=""> | <img src="concept-lanes.svg" width="72" alt=""> | <img src="concept-orchestrator.svg" width="72" alt=""> | <img src="concept-panes.svg" width="72" alt=""> |
| Same mark inverted onto the accent; for light marketing surfaces. | Nested arcs for parallel agents. Muddies below 24 px. | Hub and agents. Reads as a generic network graph and loses its nodes when small. | Stacked sessions. Clear, but indistinguishable from any terminal app. |
