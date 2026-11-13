# Repository workflow

You are a senior app developer tasked with creating a great Agent Development Environment (ADE) product.

## Deliver changes through pull requests

- Use Issues, worktrees and PRs to manage your work.
- Many agents share GitHub account and secondary limits. Read CI, head, review and merge state with `scripts/gh-pr-status NUMBER -R OWNER/REPOSITORY --json`: concurrent local callers share one read per PR every two minutes. Do not use `gh pr checks --watch`, `gh run watch` or independent polling loops. Read again when needed; an idle cache issues no requests. This works with older installed CLIs: it bootstraps a cached CLI from local `origin/main` through the build gate. While that build is unavailable, a shared single-REST fallback returns PR/head/merge state with CI and review decision explicitly unknown. Inspect `data.stale`, `data.error`, `data.retry_at`, `limitations` and the returned head before acting; unknown checks do not mean passing. See `docs/development.md` for cache and cleanup details.
- Use `scripts/gh-observe run --real /usr/bin/gh -- bash` only when opting a repository agent shell into request measurement (see `docs/development.md`). Observation must fail open and add no admission waits or retries. Never install a global gh wrapper or enable live global cooldown state. Keep optional, reviewed content serialization separate from measurement. Honor the failed request's `Retry-After`; a primary reset applies to its account and API resource, so exhausted GraphQL must not block available REST quota. Do not query `gh api rate_limit` in a retry loop.
- Do your work in PRs against `main`.
- GitHub CI compiles no Rust, so two PRs that are each green can still break `main` together. Just before merging, bring the branch up to date with `origin/main`, compile all targets on that exact head in every lane it touches (`scripts/cargo-gate check --locked --no-default-features --all-targets`, and again with `--features host` and `--features ui`), and merge with `--match-head-commit` so nothing lands untested.
- The lead owns the complete user outcome through implementation, independent review, validation and merge. Give parallel workers explicit boundaries; review the assembled experience before calling it ready. A subtask passing tests does not establish that the feature works well.
- For a change people interact with, state the user's task and intended before/after behavior in the PR. Follow the latest approved design and corrections across the affected flow; preserve necessary controls and information when simplifying it.
- For interaction changes, ask an independent reviewer to assess the user experience as well as correctness. Give them the intended flow, actual native evidence and tested head, not just a diff. The reviewer reports concrete usability failures and missing evidence; the lead resolves usability blockers and the evidence gaps that prevent validating the changed flow before calling it ready or merging. A broken primary flow, lost essential controls or unexplained recurring stalls cannot be waived by listing a follow-up. Refresh the affected evidence and review when later edits change the flow; retain earlier evidence only where it still applies. Record nonblocking limitations separately, merge qualified work and sync main.

## Pull request screenshots

- PRs that change something people see must include before and after screenshots in the PR description. Use only two side-by-side image columns, labeled Before and After; put the page or interaction-state description beneath each pair. Do not add a State, explanation, or other text column beside the images: it reduces their display size.
- Capture each pair through the actual native app, using the development runner, with the same viewport, scale, theme, data, and interaction state. `scripts/native-evidence` does this from an SSH shell; see `docs/development.md`. A hosted agent runs it over plain `ssh fw16`: its own shell carries `CAGENTS_AGENT_AUTHORITY`, which the script refuses. Never unset, strip or forward that authority to get past a refusal. Identify the before revision and the tested after head. Capture the affected states, including expanded, empty, error, and narrow layouts when the change affects them.
- Save images as WebP with descriptive names such as `before-channel-prs.webp` and `after-channel-prs.webp`. Check that they show the intended change and contain no secrets or private user content.
- Keep screenshots off the product PR branch. Commit only the images under `pr-screenshots/` on a separate `<branch>-screenshots` branch, and embed immutable links in the description: `https://github.com/CarsonBurke/cagents/blob/<sha>/pr-screenshots/<name>.webp?raw=true`. Keep the screenshot branch so the images remain reachable.
- Refresh the comparison when a later change alters the visible result. If a state could not be captured, say exactly what is missing; mockups and component-rendered fixtures must be labeled and do not establish actual native UI validation. Fixtures loaded into the running native app can validate interaction; identify which provider or production behavior they do not exercise.

## Design for the person using the app

- Use iced/cosmic components and theming where they serve the interaction; build custom components where they do not. Start from what the user needs to understand or do, rather than exposing the backend's structure.
- Give each view a clear primary action and visual hierarchy. Keep routine activity compact, disclose technical detail on demand, and make state, scope and recovery understandable. Internal prompts, skill instructions, transport identifiers and implementation notes are not user messages. Avoid duplicate navigation and contradictory-looking states.
- Exercise the complete changed flow in the actual native app. Choose representative data and states: populated and empty, working and finished, errors or stale data where relevant, and narrow and wide layouts. Check keyboard focus, hover targets, scrolling and transitions, not only a static happy-path screenshot. Use fixtures when sufficient and label them; use a live provider only when the behavior requires one.
- Judge responsiveness with realistic conversation length and concurrent agents. For changes to rendering, polling or data preparation, compare before/after frame stalls and resource use on the same workload. A spinner or hover freezing with unrelated updates is a product defect even when tests pass. Keep expensive work off the UI path.
- The independent reviewer should ask: Can the user finish the intended task? Is important information visible at the right time? Do the labels and states make sense without implementation knowledge? What breaks during interaction or at realistic scale? Inspect the native evidence and exercise uncertain behavior; identify untested claims explicitly. Review effort should match the affected behavior, not expand into an unrelated redesign.
- When review or user feedback exposes a pattern, check neighboring instances and fix the shared cause. Do not close the issue merely because the pictured instance changed; capture the applicable regression with a focused test or repeatable interaction, and link any bounded follow-up.

## Choose the cheapest correct build lane

- Run Cargo through `scripts/cargo-gate`. Rust compiles only on the build host (`fw16`): the gate sends every compiling command there, brings back the executables, docs and messages callers use, and waits while the host is unavailable rather than building here. Set `CAGENTS_BUILD_REMOTE=local` only when a build must run on this host. Use `scripts/cargo-gate exec COMMAND` for a script of Cargo commands, such as anything you would have run under `flock /var/tmp/cagents-cargo.lock`; its `cargo` leads back to the gate. Do not hold a build lock while tests or the app run.
- Start with the isolated boundary that owns the change:
  - CLI: `scripts/cargo-gate check --locked --no-default-features --bin cagents`
  - daemon/terminal host: `scripts/cargo-gate check --locked --no-default-features --features host --bin cagentsd`
  - browserless UI: `scripts/cargo-gate check --locked --no-default-features --features ui --bin cagents-ui`
  - integrated UI: `scripts/cargo-gate check --locked --features integrated-browser --bin cagents-ui`
- The default `full` feature selects production identity only. `ui`, `host`, and
  `integrated-browser` select implementation graphs; never reunify `ui` and
  `host` in one Cargo invocation just to shorten a command.
- Before finishing a feature or dependency change, run every affected isolated boundary. UI-only builds must not resolve Ghostty; host-only builds must not resolve libcosmic, Servo, WebRender, or SpiderMonkey.

## Run the application through the development runner

- Run native instances, profiling and measurements on fw16 where possible, keeping this host free for the user.
- Use `scripts/run-development --browserless` for ordinary UI, session, sidebar, settings, and terminal work.
- Use `scripts/run-development` when the changed behavior requires the embedded browser.
- Do not use a direct `cargo run` as the normal UI workflow. The runner builds the three matched executables with separate feature sets, then selects the isolated development application ID, state, and daemon.
- The runner uses the `dev-runtime` profile so UI dependencies remain optimized. Keep ordinary `dev` checks unoptimized; do not restore a wildcard `opt-level` under `[profile.dev.package."*"]`.
- Add `--debug --log-file /tmp/cagents-ui.log` when diagnosing UI behavior.
- Add `--fresh-daemon` after changing daemon, terminal-host, IPC/protocol, or process-lifecycle code. It uses the daemon lock and a Linux pidfd rather than the wire protocol, so it remains safe across protocol changes; this terminates terminals hosted by that daemon.
- Do not run two development sessions without `--instance` at the same time; they share development state. Give each concurrent native run its own `--instance NAME`.

## Preserve cross-feature behavior

- Browserless mode must preserve saved browser descriptors and open their URLs in the system browser. It must not discard or rewrite browser session state merely because Servo is absent.
- `cagents`, `cagents-ui`, and `cagentsd` are a matched set. Build and exercise them together when their shared behavior changes.

## Verify the changed surface

- Run the narrow test target that covers the changed contract.
- A bare `cargo test` runs only the shared CLI lane: the `ui` and `host` unit tests and every gated integration target are compiled out under the default `full` feature. Run `scripts/cargo-gate test --locked --no-default-features --all-targets`, then the same with `--features host` and with `--features ui`, to cover the boundaries the change touches.
- For UI changes, launch the actual native UI with the development runner and exercise the changed path. A successful compile alone is insufficient.
  - If you need live testing with an agent (fixture isn't sufficient) you must use the cheapest model, such as haiku/luna on low reasoning.
- Fixtures, fake providers and the development runner are yours to maintain, like product code. When one is missing a state, is flaky, leaks processes or refuses a legitimate use, fix or extend it in your PR rather than working around it, and leave it easier for the next agent. Point fixture providers at their own config directory; never write fixture files into the real `~/.claude` or `~/.codex`.
- For browser or feature-boundary changes, verify both browserless and integrated lanes.
- Use `scripts/check-format` after Rust edits. It checks Cargo roots and the Rust source trees hidden behind `include!` macros without rewriting files.

The full runner option reference, test lanes, resource guidance, and troubleshooting are in `docs/development.md`.
