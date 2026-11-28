# Wayland event-loop patches

Source: https://github.com/pop-os/winit/tree/71ce08c043814514a8fd92d9d0599f115ae854e8/winit-wayland
License: Apache-2.0 (LICENSE copied from the upstream root).

The final run_on_demand cleanup only needs to transmit queued window destruction
requests. Flush them without adding a display.sync acknowledgement roundtrip.
Retain the original roundtrip fallback if flushing fails (including socket
backpressure), preserving its previous error/cleanup behavior. Normal application,
GPU, browser surface, LSP, and persistence cleanup are unchanged.

Both the Winit and iced SCTK dispatch loops watch for a writable socket after
`flush()` returns `WouldBlock`. They retry when the compositor drains the socket
and remove write interest when pending requests have been sent. The external
pump notifier also watches for writes while requests are pending. A fatal
connection error exits the Winit loop with the I/O errno or `EPROTO` for a
protocol error, including when calloop discovers the failure during dispatch.
`mise run test:contracts -- --test wayland_flush` exercises the shared dispatch
against a private socket pair.

Cargo.toml expands the upstream workspace metadata/dependencies. Shared dpi,
winit-core and winit-common stay on the original pinned COSMIC dependency graph.
Remove this override when the pinned upstream includes equivalent cleanup and
backpressure handling.
