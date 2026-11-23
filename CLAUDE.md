# Working in this repository

You are building an Agent Development Environment (ADE) for the person who uses it.

The practices below say what each is for. Do what the change needs: a copy fix, a test-only change or something the user has already judged needs far less than a new interaction flow. What the user asks for comes first.

These protect other people and agents, so they hold regardless:

- **Shared machines.** This host and fw16 are shared with the user and other agents. Never generate artificial load (busy loops, stress tools, extra parallel copies of a build or test to reproduce a flaky failure). Diagnose timing problems by reading the race, adding instrumentation or rerunning normally. If a failure appears only under load, report that.
- **Shared GitHub limits.** Many agents share one account's API limits. Read PR state through `scripts/gh-pr-status` (below), not polling loops, `gh pr checks --watch`, `gh run watch` or `gh api rate_limit` in a loop. Honor a failed request's `Retry-After`. Exhausted GraphQL doesn't block REST.
- **Authority.** Never unset, strip or forward `CAGENTS_AGENT_AUTHORITY` to get past a refusal, and never install a global `gh` wrapper or enable live global cooldown state.
- **The user's real homes.** Point fixture providers at their own config directory. Never write fixture files into the real `~/.claude` or `~/.codex`.
- **Main stays green.** If main is broken, fix it forward ahead of other work. Check for an open "Main Rust build sentinel" issue before assuming a compile error is yours.

## Delivering changes

- Work in issues, worktrees (under `.worktrees/`) and PRs against `main`. Prefer many small merges over a queue of finished work.
- Read CI, head, review and merge state with `scripts/gh-pr-status NUMBER -R OWNER/REPOSITORY --json`. Concurrent callers share one read per PR every two minutes, and an idle cache makes no requests. Check `data.stale`, `data.error`, `data.retry_at`, `limitations` and the returned head. Unknown checks don't mean passing. After pushing, add `--expect-head "$(git rev-parse HEAD)"`: exit 5 means the snapshot predates your push. `scripts/gh-observe run --real /usr/bin/gh -- bash` opts a shell into request measurement. It must fail open. See `docs/development.md`.
- Land with `scripts/land-pr NUMBER -R OWNER/REPOSITORY` once CI is green and the change looks right. It merges at the tested head and keeps AI attribution out of the squash message. Update a branch only when it conflicts or depends on something newer. GitHub runs no CI on a conflicting PR, so confirm the PR is mergeable after each push.
- GitHub CI compiles no Rust, so compile the lanes your change touches before asking for review (see the build lanes below). Two individually green PRs can still break main together. When main has moved a lot under a change that touches shared types, a throwaway local merge compile before landing is cheap insurance. The fw16 main build sentinel catches the rest.
- The lead owns the user's outcome through implementation, review and merge. Give parallel workers clear boundaries, and check the assembled experience: a subtask passing its tests doesn't show that the feature works well.
- For a change people interact with, say in the PR what the user is trying to do and how the behavior changes. Follow the latest approved design and the user's corrections across the whole affected flow. When you simplify, keep the controls and information people need.

## Review

- Get an independent review when a second pair of eyes is likely to catch something: non-trivial logic, concurrency, security, protocol changes, or a new interaction. Match its depth to the change's size and risk. A trivial or already-judged change may need none.
- For interaction changes, the reviewer should judge the experience as well as the code: Can the user finish the task? Is the important information visible at the right time? Do labels and states make sense without knowing the implementation? What breaks during interaction or at realistic scale? Give them the intended flow and evidence, not just the diff.
- A broken primary flow, lost essential controls or recurring stalls block a merge. Anything else can merge with a linked follow-up.

## Evidence for visible changes

Screenshots let a reviewer or the user judge a visible change without running it, so skip them when there's no visible effect, the difference is too small to see, or the user has already seen it.

When you do capture:

- Use the actual native app through the development runner, with the same viewport, scale, theme, data and interaction state for Before and After. `scripts/native-evidence` does this from an SSH shell. A hosted agent runs it over plain `ssh fw16`. See `docs/development.md`.
- Show two side-by-side image columns, labeled Before and After, with the state described beneath each pair. A text column beside them shrinks the images.
- Before usually comes from `origin/main`. fw16 keeps executables for the main commits the sentinel built, usually the tip, so that side rarely compiles anything. A Before from an earlier capture of the same state on main can be reused.
- fw16 has a few native session slots shared by every agent, and a session may hold one for at most 30 minutes. Build both sides before taking a slot, capture everything in one session, and release it. Don't hold up a merge on a second capture round for cosmetic edits. Say what changed after the captured head.
- Save WebP images with descriptive names (`before-channel-prs.webp`). Check that they show the change and contain no secrets or private content. Commit them only under `pr-screenshots/` on a separate `<branch>-screenshots` branch, keep that branch, and link them immutably: `https://github.com/CarsonBurke/cagents/blob/<sha>/pr-screenshots/<name>.webp?raw=true`.
- Label mockups and component-rendered fixtures as such: they don't show the native UI. Fixtures loaded into the running app are the normal way to validate. Name a gap only when the change depends on provider behavior a fixture can't reproduce.

## Designing for the person using the app

- Start from what the user needs to understand or do, not from the backend's structure. Use iced/cosmic components and theming where they serve the interaction, and build custom ones where they don't.
- Give each view a clear primary action and visual hierarchy. Keep routine activity compact, and show technical detail on demand. Make state, scope and recovery understandable. Internal prompts, skill instructions, transport identifiers and implementation notes are not user messages. Avoid duplicate navigation and contradictory-looking states.
- Exercise the changed flow in the running app with representative states: populated and empty, working and finished, errors and stale data, narrow and wide. Where they matter, check focus, hover, scrolling and transitions too.
- Judge responsiveness at realistic scale: long conversations, concurrent agents. For rendering, polling or data-preparation changes, compare frame stalls and resource use before and after on the same workload. A spinner or hover freezing during unrelated updates is a defect even when tests pass. Keep expensive work off the UI thread.
- When feedback exposes a pattern, fix the shared cause and check the neighboring cases, not just the pictured instance. Add a focused test or a repeatable interaction for it, and link any bounded follow-up.

## Tasks and build lanes

- `mise tasks` lists the supported interface: `setup`, `doctor`, `dev`, `check:*`, `test:*`, `fmt`, `ci`, `install`, `package` and `release:*`. Pass arguments after `--`. A lane task's arguments replace its default `--all-targets`. `mise run doctor` explains a missing prerequisite or an unreachable build host. In a new worktree of this repository, run `mise trust`. Trust nothing from outside it.
- Rust compiles only on the build host, fw16. The compiling tasks run Cargo through `scripts/cargo-gate`, which sends the work there, brings back what callers need, and waits while the host is unavailable. Use `scripts/cargo-gate exec COMMAND` for a script of Cargo commands. Set `CAGENTS_BUILD_REMOTE=local` only when a build must run here. Don't hold a build lock while tests or the app run.
- Compile the lanes your change can affect, and skip the ones it can't:
  - CLI: `mise run check:cli`
  - daemon or terminal host: `mise run check:host`
  - browserless UI: `mise run check:ui`
  - integrated browser: `mise run check:browser`
  - shared code: `mise run check` (all three)

  Narrow with `-- --bin …` while iterating. The default `--all-targets` also checks tests, and like the main sentinel, these fail on the workspace's own warnings, which a narrowed run can miss.
- The application (`crates/cagents-ui/src/app` and the other UI-only modules) is the `cagents-ui` package. The root package is the shared library with the `cagents` and `cagentsd` binaries. A direct Cargo command for the UI needs `-p cagents-ui`.
- The default `full` feature selects production identity only. `ui`, `host` and `integrated-browser` select implementation graphs. Never combine `ui` and `host` in one Cargo invocation: UI-only builds must not resolve Ghostty, and host-only builds must not resolve libcosmic, Servo, WebRender or SpiderMonkey.

## Running the app

- Run native instances, profiling and measurements on fw16 where you can, to keep this host free for the user.
- Use `mise run dev` (`scripts/run-development --browserless`) for ordinary UI, session, sidebar, settings and terminal work. Use `mise run dev:browser` when the behavior needs the embedded browser. Prefer the runner to `cargo run`: it builds the three matched executables with their own feature sets and selects the isolated development app ID, state and daemon.
- The runner uses the `dev-runtime` profile so UI dependencies stay optimized. Keep ordinary `dev` checks unoptimized, and don't restore a wildcard `opt-level` under `[profile.dev.package."*"]`.
- Add `-- --debug --log-file /tmp/cagents-ui.log` to diagnose UI behavior. Add `--fresh-daemon` after changing daemon, terminal-host, IPC/protocol or process-lifecycle code. It ends the terminals that daemon hosts. Give each concurrent native run its own `--instance NAME`, because runs without one share development state.
- If a test needs a live agent because a fixture won't do, use the cheapest model at low reasoning.

## Cross-feature behavior

- Browserless mode keeps saved browser descriptors and opens their URLs in the system browser. It must not discard or rewrite browser session state just because Servo is absent.
- `cagents`, `cagents-ui` and `cagentsd` are a matched set. Build and exercise them together when shared behavior changes. For browser or feature-boundary changes, check both the browserless and integrated lanes.

## Testing

- Run the narrow tests that cover the contract you changed. A bare `cargo test` runs only the CLI lane. Use `mise run test:cli`, `test:host` and `test:ui` (or `mise run test`), narrowed with `-- --test TARGET`. Run one lane's library tests with `mise run test:lib -- LANE --prefix MODULE::`. LANE is `cli`, `host`, `ui`, `shared-ui` or `browser`. `test:lib` doesn't deny warnings and is the fast loop for UI library tests.
- Run `mise run fmt:check` after Rust edits (`mise run fmt` rewrites). `mise run ci` runs what GitHub CI runs, without compiling Rust.
- Fixtures, fake providers and the development runner are product code too. When one is missing a state, is flaky, leaks processes or refuses a legitimate use, fix it in your PR and leave it easier for the next agent.

The full runner options, test lanes, resource guidance and troubleshooting are in `docs/development.md`.
