Based on iced_widget 0.14.2 from pop-os/libcosmic revision
caec74c2559924443f12fc6faf97a5bcefe6271d, directory iced/widget. The source,
build script and upstream MIT license are kept. The manifest expands the
upstream workspace's dependency specifications and features unchanged, so the
dependency graph is the same; only the workspace lints are left out. Assets used
only by documentation are omitted. Root Cargo.toml selects this copy through the
existing libcosmic `[patch]`.

Local change: text cursors stop blinking (src/text_editor.rs, src/text_input.rs)

TextEditor and TextInput take their blink from `iced_core::text::Blink`, which
is vendored in vendor/iced-core. The cursor still alternates every 500 ms. Once
the field has gone ten seconds without input, the cursor rests visible and the
widget asks for no more frames. Any input that already restarted the blink
upstream restarts it here: typing, a binding, a click or regaining window focus.
Visibility and frame scheduling are otherwise unchanged. The local
CURSOR_BLINK_INTERVAL_MILLIS constants are replaced by the shared schedule.

Why: upstream, a focused field asks for a redraw every 500 ms for as long as it
keeps focus. Each redraw repaints the whole window. A chat composer holds focus
after a chat opens or a message is sent, so the app kept repainting while the
user read. On fw16 that cost about 1% of a core, half the idle UI process. The
ten-second rest matches GTK's gtk-cursor-blink-timeout default and VTE. The
app's code editor (src/editor_widget.rs), terminal (src/terminal/widget.rs) and
libcosmic's TextInput (vendor/libcosmic/PATCH.md) use the same schedule.

Coverage: vendor/iced-core's text::blink tests cover the schedule. The app's
terminal tests cover the rest and its restart, and docs/performance.md
records the native measurement ("Idle UI wakeups"). Remove this copy when the
pinned upstream widgets stop blinking on their own.
