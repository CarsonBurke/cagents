# Temporary Wayland shutdown patch

Source: https://github.com/pop-os/winit/tree/71ce08c043814514a8fd92d9d0599f115ae854e8/winit-wayland
License: Apache-2.0 (LICENSE copied from the upstream root).

The final run_on_demand cleanup only needs to transmit queued window destruction
requests. Flush them without adding a display.sync acknowledgement roundtrip.
Retain the original roundtrip fallback if flushing fails (including socket
backpressure), preserving its previous error/cleanup behavior. Normal application,
GPU, browser surface, LSP, and persistence cleanup are unchanged.

Cargo.toml expands the upstream workspace metadata/dependencies. Shared dpi,
winit-core and winit-common stay on the original pinned COSMIC dependency graph.
Remove this override when the pinned upstream includes equivalent exit behavior.
