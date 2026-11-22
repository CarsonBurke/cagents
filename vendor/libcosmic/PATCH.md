# Targeted native accessibility button actions

Upstream: https://github.com/pop-os/libcosmic.git
Revision: caec74c2559924443f12fc6faf97a5bcefe6271d
Bundled cosmic-icons submodule: 5252095787cc96e2aed64604158f94e450703455

This copy contains only the libcosmic package, its embedded fonts, translations,
and scalable bundled icons required by build.rs on non-Unix/macOS targets.
Sibling packages and the iced workspace are not vendored here. Their original
path dependencies point to the exact same pinned Git revision. Inherited registry
dependencies are expanded from the upstream workspace with their original version
and default-feature settings. Package features are unchanged.

The button event helper now checks IdEq against its own widget ID before handling
or capturing native accessibility actions. Upstream ignored this ID and captured
all actions, preventing later siblings (including editable workspace fields) from
receiving them and letting an unrelated button react to Click. Native numeric IDs
must match the numeric component of a named widget ID, hence IdEq instead of
ordinary PartialEq.

The application regression mounts unrelated buttons before an editable field,
then dispatches numeric IDs from the emitted accessibility tree. It checks editing
reaches the field and a targeted click reaches only the intended button.

# Overlays inside buttons keep their screen translation

`Button::overlay` and the Wayland `Tooltip::overlay` added their own layout
position to the `translation` they pass to their content. iced layouts are
already absolute, so every overlay raised from inside a button, such as a
tooltip or popover on a control within a list row or card, was displaced by
the button's origin. Both now pass `translation` through unchanged, as iced's
own button does.

The application regressions raise a tooltip and a popover from a control in an
offset `button::custom` and check that each overlay is anchored to its control,
with any ancestor translation applied once.

# Text input cursors stop blinking

`TextInput` blinks with `iced_core::text::Blink` (see `vendor/iced-core`). Ten
seconds after its last input, the cursor rests visible and the input asks for
no more redraws. Upstream it repainted the window every 500 ms for as long as it
kept focus. Typing, a click, an input-method commit or returning to the window
restarts the blink; the last is new, since upstream's blink never stopped.
It reads the schedule through `Blink::current()`, so the desktop's blink
interval and timeout apply, and a desktop that turns blinking off holds the
cursor steady.

# Disabled buttons are not focus stops

`Button::operate` reported every button to focus operations as a stop, so Tab
and Shift+Tab landed on buttons without `on_press` or `on_press_down`, which
nothing can activate. A disabled button now reports its focus state through a
wrapper whose `Focusable::is_focus_stop` is false (see
`vendor/iced-core/README.cagents.md`), so traversal passes it over. It is still
reported, because the app locates buttons such as a selected tab through focus
operations. A button disabled while focused keeps its focus, as Refresh does
while it runs, and Tab moves on from it.

The application regression `disabled_buttons_are_not_focus_stops` in
`src/ui_controls/button.rs` covers traversal in both directions, locating, and
disabling a focused button.

# The settings portal reconnects with a backoff

`desktop_settings` reconnected to the desktop settings portal as soon as both
of its change streams ended. When the session bus is gone, every call fails at
once, so the subscription spun and logged five errors per pass (about 6,000
passes while an application shut down after its session ended). It now waits
before reconnecting, one second after a working connection ends and doubling
after each consecutive failure up to a minute, as it already did when the
proxy could not be created. A received setting resets the wait.
