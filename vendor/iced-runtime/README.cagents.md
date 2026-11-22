# Iced runtime pointer dispatch patch

Source: `iced/runtime` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/runtime), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. Upstream MIT license is included. The manifest is normalized to retain the same pinned dependency graph outside the upstream workspace.

Cagents issue #416: Winit queues input until `AboutToWait`, then supplies the window's final cursor position for the entire batch. An earlier press can consequently target a later pointer position, including another tab's close button.

`src/user_interface.rs` replays cursor movement and leave events in order for both overlays and base widgets. The previous cursor survives cache rebuilds and relayouts, so a batch beginning with a button event uses the preceding batch's position. Overlay masking is evaluated per event; hover feedback still uses the final cursor. If a new interface receives a press before its first position event, its location is unknown and cannot activate an unrelated later target.

Regression tests run through `UserInterface::update` in `crates/cagents-ui/src/reorder.rs` and `crates/cagents-ui/src/ui_controls.rs`, covering complete batched drags/clicks, rebuilds, and overlay crossing. Direct widget-event tests alone cannot reproduce the dispatch bug. Remove this patch when the pinned upstream runtime preserves equivalent per-event positions.

`src/frame.rs` adds the frame-timing hook the vendored shell reports presented frames through; see `vendor/iced-winit/README.cagents.md`.

## Whether the last drawing still shows the interface (#3581)

`UserInterface::is_drawn` reports whether the last `draw` still shows the
interface: no event or `operate` call has reached it since, and it has not
been rebuilt or relaid out (both build a new interface). `State::Updated`
also carries the `window::AnimationRequest` the widgets made during the
update. The shell uses both to present the renderer's retained drawing for
an animation frame instead of updating and drawing every widget; see
`vendor/iced-core/README.cagents.md`, "Animation frames that only present".
Any event marks the interface undrawn, even one no widget redraws for, so a
widget that changes its look without asking for a redraw is still drawn on
the next animation frame, as it was before. Remove this with the iced-core
change.
