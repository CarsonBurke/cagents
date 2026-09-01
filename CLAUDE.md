# Repository workflow

## Deliver changes through pull requests

- Start each change from current `main` on a dedicated branch.
- Push the branch and open a pull request targeting `main`; do not commit new work directly to `main`.

## Choose the cheapest correct build lane

- Start with the isolated boundary that owns the change:
  - CLI: `cargo check --locked --no-default-features --bin cagents`
  - daemon/terminal host: `cargo check --locked --no-default-features --features host --bin cagentsd`
  - browserless UI: `cargo check --locked --no-default-features --features ui --bin cagents-ui`
  - integrated UI: `cargo check --locked --features integrated-browser --bin cagents-ui`
- The default `full` feature selects production identity only. `ui`, `host`, and
  `integrated-browser` select implementation graphs; never reunify `ui` and
  `host` in one Cargo invocation just to shorten a command.
- Before finishing a feature or dependency change, run every affected isolated boundary. UI-only builds must not resolve Ghostty; host-only builds must not resolve libcosmic, Servo, WebRender, or SpiderMonkey.
- Do not run independent cold Cargo builds concurrently. They contend for the shared build directory and memory; lower `CARGO_BUILD_JOBS` when the host is already under memory pressure.

## Run the application through the development runner

- Use `scripts/run-development --browserless` for ordinary UI, session, sidebar, settings, and terminal work.
- Use `scripts/run-development` when the changed behavior requires the embedded browser.
- Do not use a direct `cargo run` as the normal UI workflow. The runner builds the three matched executables with separate feature sets, then selects the isolated development application ID, state, and daemon.
- The runner uses the `dev-runtime` profile so UI dependencies remain optimized. Keep ordinary `dev` checks unoptimized; do not restore a wildcard `opt-level` under `[profile.dev.package."*"]`.
- Add `--debug --log-file /tmp/cagents-ui.log` when diagnosing UI behavior.
- Add `--fresh-daemon` after changing daemon, terminal-host, IPC/protocol, or process-lifecycle code. It uses the daemon lock and a Linux pidfd rather than the wire protocol, so it remains safe across protocol changes; this terminates terminals hosted by that daemon.
- Do not run browserless and integrated development sessions concurrently; they share development state.

## Preserve cross-feature behavior

- Browserless mode must preserve saved browser descriptors and open their URLs in the system browser. It must not discard or rewrite browser session state merely because Servo is absent.
- `cagents`, `cagents-ui`, and `cagentsd` are a matched set. Build and exercise them together when their shared behavior changes.

## Verify the changed surface

- Run the narrow test target that covers the changed contract.
- A bare `cargo test` runs only the shared CLI lane: the `ui` and `host` unit tests and every gated integration target are compiled out under the default `full` feature. Run `cargo test --locked --no-default-features --all-targets`, then the same with `--features host` and with `--features ui`, to cover the boundaries the change touches.
- For UI changes, launch the actual native UI with the development runner and exercise the changed path. A successful compile alone is insufficient.
- For browser or feature-boundary changes, verify both browserless and integrated lanes.
- Use `cargo fmt --check` after Rust edits.

The human-facing setup, dependency prerequisites, runner options, and troubleshooting examples are in `README.md` under **Run from source**. Repository development tooling is shell and Rust; Python is only an upstream native prerequisite of the full Servo build graph.
