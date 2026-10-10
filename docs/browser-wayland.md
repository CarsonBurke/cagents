# Integrated browser Wayland requirements

The integrated browser embeds a child surface in the application's actual Wayland parent. It requires `wl_compositor`, `wl_subcompositor`, and `wp_viewporter`, each at version 1 or newer. A higher advertised version is capped to the version the embedding code supports. This check covers the embedding protocols; it does not guarantee GPU, EGL, network, or webpage support.

The application checks the registry once after its main window exists, before restoring or opening an embedded browser engine. It dispatches only events already read by the window event loop and flushes requests without blocking. A second sync acknowledges the required protocol bindings. The check has a two-second deadline starting at the parent request, including delayed window callbacks and backpressure. The rest of the workspace continues without starting Servo when the parent, protocols, or preparation are unavailable.

A browser pane and its accessible Browser host group report **Unavailable** with the exact missing required protocol names, or a fixed timeout/parent/initialization reason. Arbitrary registry names and backend diagnostics are not shown. Saved URLs, browser history, and human pane labels remain intact. **Open in system browser** remains available and resolves the current descriptor when activated. Capability failure is retained for that parent window; navigation retries do not restart engines or repeat the registry check. Reopening the application creates a new parent and a new check. Browserless builds continue to offer saved pages in the system browser and do not run this probe.

Parent teardown joins all browser workers and reapers, retires the probe generation, and explicitly clears both pending and prepared protocol queues before the parent owner closes. A late callback holding a factory reference cannot recreate a released queue or install a result on a replacement window. No background reader, blocking registry roundtrip, or modification to the UI's socket flags is used.

## Qualification

Run the isolated CLI, host, browserless UI, and integrated UI checks sequentially. The optional `browser-protocol-fixture` feature enables a real owned socket/registry test server; it is not a general development dependency and must not enter CLI/host dependency graphs:

```sh
cargo test --locked --no-default-features --lib capability::wayland_requirement_tests
cargo test --locked --no-default-features --features browser-protocol-fixture --lib browser::fallback::protocol_fixture
cargo test --locked --features integrated-browser --lib app::browser_backend
```

The controlled wire fixture covers a capable registry, each missing global, removal after an announcement, binding acknowledgment, pending-only polling with an unserviced server, and explicit release while a late factory reference survives. It owns the client reader to simulate the event loop's delivery; this does not prove Winit's integration or graphical browser behavior.

Before release, exercise matched executables in private native sessions with provider-free local pages. On a capable private Wayland compositor, verify that Winit delivers events to the private queue, the browser becomes ready, and restoring and creating additional browser panes succeeds. Verify terminal and other workspace panes remain usable. For a compositor or controlled native fixture omitting each required protocol, verify the exact missing protocol is visible and accessible, no Servo engine starts, repeated open/restore/retry actions do not repeat engine registration, and the original saved URL opens through the genuine system-browser action. Hold or delay registry replies to verify the bounded timeout, then deliver late replies and close/replace the parent to verify stale results cannot install a factory. Inspect owned socket/process teardown. Repeat the saved-descriptor/system-browser path in browserless mode.

Record actual results separately from source tests. A controlled registry test does not establish compositor capability, GPU support, accessible webpage content, or the native event-pump lifetime contract.
