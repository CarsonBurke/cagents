# Repository workflow

## Choose the cheapest correct build lane

- Start with the browserless lane unless the change touches the embedded browser, Servo integration, browser capability gating, or default feature behavior.
- Browserless check: `cargo check --locked --no-default-features --features development --bins`
- Integrated-browser check: `cargo check --locked --features development --bins`
- The production default includes `integrated-browser`. Do not change that default to make local development faster.
- Before finishing a Cargo feature or dependency change, run both checks. Browserless builds must not resolve or compile Servo, WebRender, or SpiderMonkey.

## Run the application through the development runner

- Use `scripts/run-development --browserless` for ordinary UI, session, sidebar, settings, and terminal work.
- Use `scripts/run-development` when the changed behavior requires the embedded browser.
- Do not use a direct `cargo run` as the normal UI workflow. The runner builds the three sibling executables together and selects the isolated development application ID, state, and daemon.
- The runner uses the `dev-runtime` profile so UI dependencies remain optimized. Keep ordinary `dev` checks unoptimized; do not restore a wildcard `opt-level` under `[profile.dev.package."*"]`.
- Add `--debug --log-file /tmp/cagents-ui.log` when diagnosing UI behavior.
- Add `--fresh-daemon` after changing daemon, terminal-host, IPC/protocol, or process-lifecycle code. It validates and signals the old development daemon before rebuilding, so it remains safe across protocol changes; this terminates terminals hosted by that daemon.
- Do not run browserless and integrated development sessions concurrently; they share development state.

## Preserve cross-feature behavior

- Browserless mode must preserve saved browser descriptors and open their URLs in the system browser. It must not discard or rewrite browser session state merely because Servo is absent.
- `cagents`, `cagents-ui`, and `cagentsd` are a matched set. Build and exercise them together when their shared behavior changes.

## Verify the changed surface

- Run the narrow test target that covers the changed contract.
- For UI changes, launch the actual native UI with the development runner and exercise the changed path. A successful compile alone is insufficient.
- For browser or feature-boundary changes, verify both browserless and integrated lanes.
- Use `cargo fmt --check` after Rust edits.

The human-facing setup, dependency prerequisites, runner options, and troubleshooting examples are in `README.md` under **Run from source**. Repository development tooling is shell and Rust; Python is only an upstream native prerequisite of the full Servo build graph.
