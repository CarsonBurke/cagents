# Iced keyed child reconciliation patch

Source: `iced/core` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/core), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The manifest preserves the pinned dependency graph outside its upstream workspace.

`Tree::diff_children_custom` now assembles reconciled children in widget order. The pinned implementation diffs named children at their old indices and truncates before matching. Reordering a named button across an unnamed spacer leaves the spacer's childless tree under the button, which panics during layout. Reordering, inserting, and removing children must retain each surviving named tree at its new index. Unnamed children retain positional matching; repeated names are consumed in order instead of overwriting a prior tree. Custom names must still be unique within a native frame because the runtime named-state cache is keyed by name.

Regression coverage includes core tree state continuity and a native `UserInterface` rebuild of mixed named buttons and spacers in `crates/cagents-ui/src/interaction.rs`. Remove this patch when the pinned upstream core provides equivalent reconciliation.

Run the core regressions as a standalone package:

```sh
cargo test --manifest-path vendor/iced-core/Cargo.toml reconciliation_tests
```

Cargo cannot run these dev-dependency tests with `-p iced_core` from the parent workspace because this vendored package is not a workspace member. Its standalone lockfile and build artifacts are local test output.

## Explicit click timestamps

`mouse::Click::new_at` accepts an explicit timestamp so gesture fixtures can
exercise real event dispatch without depending on test execution speed.
`Click::new` still uses `Instant::now`; the existing consecutive-click policy
is unchanged, including the inclusive 300 ms deadline, position tolerance,
button matching, and single/double/triple progression.

Run the focused standalone timing regression with:

```sh
cargo test --manifest-path vendor/iced-core/Cargo.toml \
  mouse::click::tests::consecutive_clicks_include_the_deadline_and_reset_after_it
```

## Custom ID hashing

Custom widget IDs now hash their names, matching their existing equality comparison. Regression tests cover rebuilt named IDs, unique IDs, and sets. This implementation is selected without accessibility; the corresponding accessibility ID implementation is patched in `vendor/iced-accessibility`.

## Drag destination registration order

`DndDestinationRectangles::into_rectangles` retains insertion order for internal
widget traversal. Scrollable and scale wrappers extract, transform, and append
these lists; reversing on each extraction made overlapping target priority
depend on the number of enclosing wrappers. The patched Winit consumer uses
`into_native_rectangles` to reverse once when registering the final native list.
App regressions cover plain, nested surface, real Scrollable, file-drop scaling,
and combined wrappers with a preceding root sibling.

## Text cursor blink schedule

`text::Blink` is the one schedule for every text cursor: an interval, and a
timeout after the last input when the cursor rests visible and needs no more
frames. Its default, 500 ms with a ten-second timeout, matches GTK's
`gtk-cursor-blink-timeout`. iced's `TextEditor` and `TextInput`
(`vendor/iced-widget`), libcosmic's `TextInput` (`vendor/libcosmic`), and the
app's code editor and terminal use it, so a focused field stops repainting its
window twice a second while nobody types.

`Blink::current()` is the schedule those cursors draw with: the desktop's, once
the app has read it and recorded it with `Blink::set_preferred`, otherwise the
default. A desktop that turns blinking off records `Blink::STEADY`, whose zero
timeout keeps the cursor visible and asks for no frames at all.

```sh
cargo test --manifest-path vendor/iced-core/Cargo.toml text::blink
```

## Focus stops

`Focusable::is_focus_stop`, true by default, lets a widget that cannot be
activated now, such as a disabled button, stay out of keyboard traversal while
still being reported to operations. `count`, `focus_next` and `focus_previous`
consider only stops, plus the focused widget, so traversal moves on from a
control disabled while it held focus. `focus` by id is unchanged. libcosmic's
button uses this (`vendor/libcosmic/PATCH.md`); the app regression is
`ui_controls::button::tests::disabled_buttons_are_not_focus_stops`.

## Relative span size

`Span::relative_size` scales a span's own `size`, or the paragraph's size
when it has none. Unlike `size`, it
keeps the paragraph's line height when the span sets none. Inline code in
chat prose uses it to sit a step smaller than the sentence around it without
changing the height of the lines it falls on (cagents#3002). `Span::metrics`
resolves a span's font size and line height in a paragraph. The renderer's
`Paragraph::with_spans` (`vendor/iced-graphics`) and the app's own shaping
for long paragraphs both take a span's metrics from it, so they agree.

`Span::inline_padding` lays out room in logical pixels at a span's start and
end, like CSS inline padding. Unlike the draw-only `padding`, it moves the
text around the span and lies inside its glyph bounds, so the highlight
covers it. Inline code uses it to keep its chip clear of the letters and
punctuation beside it. It maps to cosmic-text's `Attrs::inline_padding`
(`vendor/cosmic-text/LOCAL_CHANGES.md`).

```sh
cargo test --manifest-path vendor/iced-core/Cargo.toml text::span_metrics_tests
```

# Editor overscan (#2824)

`text::Editor` has three default methods, `set_overscan`, `overflow` and `caret`. They let the widget ask the renderer to lay out and draw lines into its padding, and report how far the text continues past each end of the text area, up to one line height, whether or not the overscan draws it (cagents #3044). `caret` returns the line the caret is on while nothing is selected, so the app can keep that line out of the fade. The defaults do nothing and report zero, so editors without overscan behave as upstream. See vendor/cosmic-text/LOCAL_CHANGES.md.

## Animation frames that only present (#3581)

A widget whose drawing changes with time alone can ask for frames that
present the window's last drawing again instead of drawing its widgets:
`Shell::request_animation` with a `window::AnimationRequest` (every display
frame, or once per interval). Requests combine to the most frequent one,
through `Shell::merge` like redraw requests, and last until the window next
draws its widgets, so a widget renews its request on every
`RedrawRequested` it still animates on.

The renderer does the animating. `Svg::spin` takes a `svg::Spin`, a steady
turn from an instant with a period; `Spin::angle` gives the turn at a given
instant. The pinned wgpu renderer adds it to the SVG's rotation when it
prepares a frame (see `vendor/iced-wgpu/README.cagents.md`). A renderer that
ignores it, such as the software fallback, draws the SVG at its `rotation`,
which the widget sets to the angle at its last redraw event, so between
the frames that draw the widgets such a spinner holds still.

The shell side is in `vendor/iced-runtime` (`UserInterface::is_drawn`) and
`vendor/iced-winit` (when a frame may only present). The app's activity
spinner (`crates/cagents-ui/src/ui_controls/gauge.rs`) is the user. Remove
this when the pinned dependency offers an equivalent way to animate without
drawing every widget.

Run its standalone tests with:

```sh
scripts/cargo-gate test --manifest-path vendor/iced-core/Cargo.toml -- svg::tests window::animation_request
```
