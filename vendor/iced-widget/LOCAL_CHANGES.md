Based on iced_widget 0.14.2 from pop-os/libcosmic revision
caec74c2559924443f12fc6faf97a5bcefe6271d, directory iced/widget. The source,
build script and upstream MIT license are kept. The manifest expands the
upstream workspace's dependency specifications and features unchanged, so the
dependency graph is the same; only the workspace lints are left out. Assets used
only by documentation are omitted. Root Cargo.toml selects this copy through the
existing libcosmic `[patch]`.

Build script adds the standard compile-time CAGENTS_CHECKOUT_ROOT identity anchor and runtime environment watch, paired with the build-hook entry in scripts/qualify-checkout-cache.py.

Local change: text cursors stop blinking (src/text_editor.rs, src/text_input.rs)

TextEditor and TextInput take their blink from `iced_core::text::Blink`, which
is vendored in vendor/iced-core. The cursor still alternates every 500 ms. Once
the field has gone ten seconds without input, the cursor rests visible and the
widget asks for no more frames. Any input that already restarted the blink
upstream restarts it here: typing, a binding, a click or regaining window focus.
TextEditor also restarts it on text committed by an input method, which upstream
left alone because its blink never stopped. Visibility and frame scheduling are
otherwise unchanged. The local CURSOR_BLINK_INTERVAL_MILLIS constants are
replaced by the shared schedule, read through `Blink::current()` so the app can
apply the desktop's blink interval and timeout, or hold the cursor steady when
the desktop turns blinking off.

Why: upstream, a focused field asks for a redraw every 500 ms for as long as it
keeps focus. Each redraw repaints the whole window. A chat composer holds focus
after a chat opens or a message is sent, so the app kept repainting while the
user read. On fw16 that was about 1% of a core, half the settled UI thread's
CPU, and five of its seven wakeups a second. The ten-second rest matches GTK's
gtk-cursor-blink-timeout default and VTE. The app's code editor
(src/editor_widget.rs), terminal (src/terminal/widget.rs) and libcosmic's
TextInput (vendor/libcosmic/PATCH.md) use the same schedule.

Coverage: vendor/iced-core's text::blink tests cover the schedule. The app's
terminal tests cover the rest and its restart, and docs/performance.md records
the native measurement ("Idle UI wakeups"). Remove this copy when the pinned
upstream widgets stop blinking on their own.

Local change: inline code styling (src/markdown.rs)

`Style::inline_code` styles a span as inline code. The app's own Markdown
renderer and this one share it, so chat prose and every `markdown::view`
surface draw inline code the same way. `Style` gains
`inline_code_relative_size`, the size of inline code relative to the text
around it, set through `Span::relative_size`, and
`inline_code_inline_padding`, room laid out before and after inline code
inside its background, set through `Span::inline_padding`. The defaults of
1.0 and 0.0 keep upstream's size and layout. `inline_code_color` becomes an `Option`, where `None`
leaves inline code in the color of the text around it. The default stays
upstream's white.

Why: monospace letters at the prose's size read larger and heavier than
the sentence around them, and full-strength ink made code brighter than the
prose (cagents#3002). The app's regressions are in
crates/cagents-ui/src/app/chat_ui/selectable_prose.rs and flow_layout.rs.

## Text editor overscan (cagents #2824)

`TextEditor::overscan(above, below)` lets text draw into the editor's top and bottom padding. The widget sets the overscan, limited to the padding, on its editor during layout, and clips the text, caret and selection to the text area plus that band, further limited to the inside of the editor's own border width, so drawn text never covers that border (cagents #3044). `overscan_bounds` computes that area: each extended edge moves inward onto a whole device pixel at the renderer style's scale factor. The renderer rounds clip rectangles to device pixels, so a fractional edge next to an antialiased border would otherwise take in a row of that border and paint over it. The placeholder still uses the text area. `Content::overflow` reports how far the text continues past each end of the text area, up to one line height, so the app can fade the band and the edge line when there is more, and `Content::caret` returns the line being edited, which that fade keeps clear of (`ui_controls::editable::editor`, `fade_into_padding`). `TextEditor::current_style` returns the style for the status the editor was last drawn in, so that fade can match the editor's own background.

Why: a scrolled composer draft was sliced at the padding's inner edge (#2824). Remove this when upstream editors can draw into their padding.

Dragging a selection into a band selects the line drawn there, and cursor reveal then scrolls it into view. The drag therefore scrolls one line for each pointer move inside the band, and stops while the pointer is still. iced still reports drags only within the widget's bounds.

## Scroller minimum length (cagents #3102)

A scrollable's scroller is proportional to the visible share of its content, but no shorter than 32 logical pixels (`MIN_SCROLLER_LENGTH` in src/scrollable.rs). On a track too short for that, it is at least half the track, so it always has room to travel: a drag is read back across that travel, and a scroller filling its track would divide by zero. Its position is the share of the scrollable content travelled, across the track left after the scroller, so it still reaches both ends. That is the mapping a drag already reads back through `Scrollbar::scroll_percentage_y`. Where the proportional length is above the minimum, the scroller is drawn where upstream drew it.

Why: upstream's minimum is 2 pixels. A transcript sized for its whole conversation, not just its loaded window, can be thousands of viewports tall, so its scroller shrank to a dot that was hard to see and to grab (cagents #3102). A track click centres the scroller on the pointer, so a click within half the scroller of either end lands on that end. Coverage: the `scrollable::tests` in this crate (`scripts/cargo-gate test --locked -p iced_widget --lib scrollable::`). Remove this when upstream's minimum is usable on very tall content.

## Grid accessibility (cagents #3153)

`Grid` forwards its cells' accessibility nodes, in reading order, the way `Row` and `Column` do. Upstream's `Grid` has no `a11y_nodes`, so every control inside one was missing from the accessibility tree. The transcript's image gallery is a `Grid` of buttons, one per picture (crates/cagents-ui/src/app/chat_ui/images/tiles.rs); its tests read the tiles' names from that tree. Remove this when upstream's `Grid` forwards them.

## Mouse area exit without a cursor (cagents #3637)

`MouseArea` publishes `on_exit` when it was hovered and a pointer event arrives with no cursor position: the pointer left the window, a scrollable withholds the cursor because it is outside the scrollable's viewport, or a layer above makes it levitate. The area then counts as left, so moving back over it publishes `on_enter` again.

Why: this copy returned early whenever the cursor had no position, before its hover bookkeeping, so none of those cases ended the hover. The workspace tab strip is a scrollable whose tabs track hover through `on_enter`/`on_exit`; moving the pointer from a tab down into the pane left that tab drawn hovered, with its close button, beside the selected tab (cagents #3637). Upstream iced compares the cursor position on every event and so ends the hover. Coverage: `app::tests::tab_hover_ends_when_the_pointer_leaves_the_strip` in crates/cagents-ui. Remove this when the pinned upstream mouse area ends hover without a cursor.
