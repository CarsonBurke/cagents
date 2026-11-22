# Iced futures: subscription events that are never dropped

Source: `iced/futures` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/futures), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. Upstream MIT license is included. The manifest is normalized to retain the same pinned dependency graph outside the upstream workspace.

## Every event reaches a listener that falls behind (#3644)

Upstream, the subscription `Tracker` gave each subscription a channel of 100
runtime events, queued with `try_send`, and dropped what did not fit with the
warning `Error sending event to subscription: TrySendError { kind: Full }`. A
listener's stream stops reading while it waits for the runtime to accept its
last message, so when the main thread fell behind, listeners such as
`event::listen_with` lost key presses and window events. Their filters run
only after an event is queued, so an application could not avoid it.

`src/subscription/tracker/queue.rs` replaces that channel with a queue that
never drops an event. It grows only while its stream waits on the runtime,
and it replaces a queued redraw or cursor move with a newer one of the same
kind, window and status when nothing came between them, since a listener only
needs the latest. It logs once its backlog passes 1024 events, and again each
time the backlog doubles. A subscription whose stream drops its event input
still receives nothing, as before.

The vendored `iced_winit` also stops broadcasting the resize request it adds
to every window's batch; see `vendor/iced-winit/README.cagents.md`.

The contract tests are `subscription_events` in `crates/iced-contracts`. Remove
this patch when the pinned upstream tracker stops dropping events.
