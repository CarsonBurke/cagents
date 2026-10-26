# Repository workflow

You are a senior app developer tasked with creating a great Agent Development Environment (ADE) product.

## Deliver changes through pull requests

- Use Issues, worktrees and PRs to manage your work.
- Do your work in PRs against `main`.
- Oversee, sub review, and merge PRs as you see reasonable; and sync main.

## Pull request screenshots

- PRs that change something people see must include before and after screenshots in the PR description, shown side by side and labeled for the page or interaction state.
- Capture each pair through the actual native app, using the development runner, with the same viewport, scale, theme, data, and interaction state. Identify the before revision and the tested after head. Capture the affected states, including expanded, empty, error, and narrow layouts when the change affects them.
- Save images as WebP with descriptive names such as `before-channel-prs.webp` and `after-channel-prs.webp`. Check that they show the intended change and contain no secrets or private user content.
- Keep screenshots off the product PR branch. Commit only the images under `pr-screenshots/` on a separate `<branch>-screenshots` branch, and embed immutable links in the description: `https://github.com/CarsonBurke/cagents/blob/<sha>/pr-screenshots/<name>.webp?raw=true`. Keep the screenshot branch so the images remain reachable.
- Refresh the comparison when a later change alters the visible result. If a state could not be captured, say exactly what is missing; mockups or fixture screenshots must be labeled and do not establish actual native UI validation.

## Development and product

- We use iced/cosmic for our UI. It can be very performant, scalable and maintainable when done right.
- cosmic comes with a lot of priors in the form of widgets and theming. Some of this is useful but don't over rely on it. We will often want our own custom components with our own design language, especially for more complex UI and better UX.
  - When working on UI, think like a skilled web/app designer.
  - Think about UI/UX like linear, vscode, zed IDE, and Orca ADE.
- Things need to be performant, scalable, and powerful. Think about how users will use the app and how to maximize their power with it, and ensure the app isn't holding them back; examples: can't run too many agents because it's laggy or ram hungry; or feature is missing that would otherwise improve their workflow.

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
- Do not run independent cold Cargo builds concurrently. They contend for the shared build directory and memory; use lower `CARGO_BUILD_JOBS` when the host is already under memory pressure.

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
  - If you need live testing with an agent (fixture isn't sufficient) you must use the cheapest model, such as haiku/luna on low reasoning.
- For browser or feature-boundary changes, verify both browserless and integrated lanes.
- Use `cargo fmt --check` after Rust edits.

The full runner option reference, test lanes, resource guidance, and troubleshooting are in `docs/development.md`.
