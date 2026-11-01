# Offline chat composer validation

`tests/fixtures/chat-composer-claude.sh` is a fake Claude stream-json executable
for native UI checks. It requires `jq`, makes no network requests, and uses a
synthetic account. It advertises asynchronous message support, holds its first
turn busy, acknowledges queued-message removal, and settles on interrupt.
Later turns complete immediately. Busy messages deliberately stay queued;
this fixture checks composer interactions, not provider delivery.

There are two routes. An agent uses the broker below. A person in a terminal
outside cagents can also follow the direct steps after it.

## From an agent: the native fixture broker

A hosted session carries `CAGENTS_AGENT_AUTHORITY`, so the direct steps refuse
to run there, and that guard stays. Do not unset, proxy or forge the capability
to get past it. Instead, ask the person to start the broker once, in their own
terminal outside cagents and inside their niri session:

```sh
scripts/native-fixture-broker
```

It exits 77 if agent authority is present in its environment or in any ancestor
process's. It listens on `$XDG_RUNTIME_DIR/native-fixture-broker.sock` (mode
0600). Start it from a commit you have reviewed, because it runs with your full
authority; it prints its path and SHA-256 when it starts. Ctrl-C, SIGTERM or
closing the terminal stops its fixtures and keeps their directories.

An agent then drives a named fixture from any shell, with its environment
unchanged. The client sends one structured request and never reads or forwards
the capability. If no broker is listening it says so and exits 69; do not start
one yourself.

```sh
scripts/native-fixture start composer --checkout "$repo/.worktrees/my-branch" --width 1600
# Prints owner and root. Later commands need --owner; put it before the name.
scripts/native-fixture run-ui --owner "$owner" composer
scripts/native-fixture status --owner "$owner" composer
scripts/native-fixture cli --owner "$owner" composer -- project open "$root/sandbox/project"
scripts/native-fixture cli --owner "$owner" composer -- --json agent spawn --provider claude \
  --model haiku --surface chat --repository "$root/sandbox/project" \
  --prompt 'Can you investigate why the Kaggle submission failed?'
scripts/native-fixture wait --owner "$owner" composer 'Kaggle' --timeout 60
scripts/native-fixture click --owner "$owner" composer 'Steer'
scripts/native-fixture type --owner "$owner" composer 'a literal message'
scripts/native-fixture viewport --owner "$owner" composer --width 480
scripts/native-fixture screenshot --owner "$owner" composer after-queued-steer
scripts/native-fixture stop --owner "$owner" composer --keep
```

The checkout must be beneath this repository's `.worktrees/`, so add one with
`git worktree add` if you work elsewhere. At most two fixtures run at once, so a
before and an after can be live together.

- **Building:** `run-ui` runs `scripts/run-development --browserless` and returns
  while it builds; follow `sandbox/runner.log` and `sandbox/ui.log` under the
  root. Each checkout builds into its own target under
  `.worktrees/native-fixture-targets/`, because sandboxed build scripts can write
  there. The first build of each checkout is cold, and these directories stay
  until you delete them. `cli` is refused until this fixture's runner has built a
  binary whose Cargo dep-info names this checkout. Only one fixture builds at a
  time: `run-ui` refuses with `build_busy` while another is building, so run it
  again once that fixture's `status` says `building` is false. Unreadable or
  malformed runner state remains busy or unavailable; it does not permit a
  second build. An unacknowledged runner launch also keeps that build exclusion
  until existing completion or holder-exit evidence is available; it does not
  make the CLI usable. Each launch uses a new log: first `sandbox/runner.log`,
  then `sandbox/runner-2.log`, and so on. `run-ui` and `status` report the current
  log; previous logs remain intact and their completion markers do not complete
  a later attempt. An existing attempt-log path refuses launch before its child
  is started. CLI use requires the current attempt's successful launch reply.
- **Viewport:** `start --width` and `viewport --width` set the app's logical width
  from 320 to 3840, through the nested output's scale, and fit it again before
  input and screenshots if the host has resized the nested window. The host lays
  out that window, so its framebuffer and height are not chosen here. `start`,
  `viewport` and `screenshot` report what was reached, and `matches` is false when
  the width could not be, such as a narrow width on a small nested window. Compare
  those values across a before/after pair, and keep the host window the same size
  between them.
- **Screenshots** land in `shots/` under the root as PNG; convert them to WebP for
  a PR.
- **Stopping:** after the original holder's exit and a successful nested-session
  stop are confirmed, `stop` removes the fixture directory unless given `--keep`,
  and lists under `left_behind` anything it could not remove. Uncertain waits,
  failed session stops or resource closes retain the name and private controls;
  a failed request stays failed even if later settlement permits cleanup. A kept
  directory, or one left by a killed broker, blocks a new `start` of that name
  until `scripts/native-fixture clear NAME`. A failed `start` frees its name only
  after that same cleanup is confirmed. During broker shutdown, new work is
  refused and accepted requests and original children are drained. A stopping
  diagnostic can remain indefinitely while ownership is uncertain; request
  timeouts do not promise safe release. If the broker is killed outright, its sandboxes die with it, but its
  nested niri windows stay until the broker next starts or `clear` runs.
- **Owner:** the owner value only stops agents using each other's fixtures by
  accident. It is not a security control.

The optional behaviors below use `sandbox/home/.claude` under the root, which you
can write from your own shell. Also `move`, `key`, `scroll` and `observe`; see
`scripts/native-fixture --help`.

### Fixed project-close scenes

The optional `project-close` recipe supplies only the PR1937 `edit`,
`unchanged`, and `reopen` scenes. Choose the scene, layout and source-side label
at `start`, with all four recipe fields and an explicit width. Wide requires
1600 and narrow requires 900; incompatible widths refuse before dispatch.
The scene needs an actual native viewport of at least 900x600. A label or a
requested width does not establish the reached viewport, height, scale or pixels;
inspect the existing viewport report and native frames.

```sh
scripts/native-fixture start close-before --checkout "$before_checkout" \
  --width 900 --recipe project-close --scene edit --layout narrow --side before
scripts/native-fixture run-ui --owner "$owner" close-before
```

Use the owner returned by this start. The same original held private runtime
bind is mounted at the literal
`/tmp/cagents-1937-native-<edit|unchanged|reopen>-<wide|narrow>-<before|after>`.
It has exactly six original mode0700 children: `home`, `data`, `config`,
`cache`, `state`, and `runtime`. HOME and all five XDG variables point to those
children; the only scene selector is
`CAGENTS_DEVELOPMENT_PROJECT_CLOSE_FIXTURE=<scene>`. The held nested Wayland
socket is mounted at `runtime/nested-wayland`. No environment, root/prefix,
argv, cwd, executable, source-head, clock or callback override is accepted.
Cargo/Rustup/target/PATH/Zig/network/limits and the original caller/opaque
authority policy remain unchanged. Ordinary and Accounts recipes retain their
existing behavior.

The original exclusive root-creation helper records ownership before any child
is created. Existing roots or children are refused, never adopted, reset or
cleared to make a start appear fresh. The consumer permits the reviewed
prelaunch build contents in its private HOME/cache and the declared nested
Wayland socket; it still refuses a consumed scene marker or an existing seed.
Do not delete either or alter directory contents to force freshness. This
recipe is single-use even when the original launch reply is lost. `viewport`
can only re-fit the original width; use a fresh fixture to change a label or
width. The existing original wait/host-stop/close uncertainty still retains
ownership and can keep shutdown incomplete. These creation records do not
prove ownership against arbitrary same-user changes.

The source consumer seeds a private editor file at `<literal-root>/project/notes.txt`.
After actual `PR1937_NATIVE` scene readiness, use the fixture's ordinary matched
CLI to close that literal project. The fixture holds the original successful
completion **after** its real cleanup/IPC work has finished. For `edit`, click
the actual editor and type synthetic text only after the original callback is
held; for `unchanged`, keep it clean; for `reopen`, use ordinary CLI project open
for the same literal path and wait for the actual peer-reopen observation.
Release that original callback only through the existing native `key ... F9`
input with no modifiers; keep the editor focused for the edit comparison.
There is no callback/release RPC or manufactured reducer message.
Maintained runner stdout and stderr go to the original `ui.log`, not runner
stdout. A receipt alone is not native input, rendered pixels, retirement or
process-settlement evidence.

Use separately bound reviewed Before and After fixture sources, fresh roots,
matched executables and actual native captures; side labels do not check Git or
ELF identity. An older checkout may ignore an unknown selector, so require the
actual scene readiness receipt and independently bound consumer source instead
of accepting an ordinary UI as the fixture. The intended comparison is late
edit retention versus the old completed-callback retirement, with clean
retirement and peer reopen as controls. This carrier does not supply the late
save, durability-warning or recovery scenes, active-census hold, provider jobs
or complete PR1937 acceptance.

This is source preparation, not native or build admission. A genuine User must
start a separately reviewed broker after stable resource/build admission. No
helper/operator/display qualification, current runtime, native screenshots,
independent UX, performance or cleanup outcome is supplied here. Existing
per-process limits are not an aggregate memory cgroup, and this recipe does not
change target/cache policy or permissions.

### Fixed Accounts scenes

The optional `accounts-quota` recipe selects only these inert scenes:
`claude-sparse`, `codex-sparse`, `claude-malformed`, `codex-malformed`,
`claude-delayed`, `codex-delayed`, `claude-ticket`, `codex-ticket`,
`claude-startup`, `codex-startup`, and `claude-catalog`. Select it at `start`;
the scene, layout and source-side labels cannot change afterward. All four
recipe fields and an explicit width are required. No environment, root path,
extra command arguments or clock override can be supplied.

```sh
scripts/native-fixture start accounts-before --checkout "$before_checkout" \
  --width 1600 --recipe accounts-quota --scene claude-sparse --layout wide --side before
scripts/native-fixture run-ui --owner "$owner" accounts-before
```

Use the owner returned by that `start`. Use matching 1600-pixel wide and
480-pixel narrow requests for a comparison, with a fresh fixture for each
scene/layout/side. These are proposed capture sizes, not proof of the reached
viewport; inspect the existing viewport reports and actual native frames.
For this recipe, `viewport` can only re-fit the original requested width.

The fixed selector reaches the same `scripts/run-development --browserless`
matched CLI/HOST/UI builds and default local UI window. Inside the sandbox,
the existing held private runtime directory is mounted at
`/tmp/cagents-1938-native-<scene>-<wide|narrow>-<before|after>`. Its distinct
`data`, `config`, `cache`, `state` and `runtime` children are owned mode0700
directories; all five XDG variables point there. The held nested Wayland socket
is mounted at that namespace's `runtime/nested-wayland`. HOME stays private.
Existing roots are refused, not adopted or repaired, and failed setup does not
automatically delete an unowned preexisting runtime root.

All starts, including the ordinary recipe, exclusively create their private
control, runtime and repository fixture roots. A live start records each
successful creation before any later setup step, and automatic failed-start
cleanup or stop can remove only those roots it created. An earlier failed stale
recovery does not authorize a later start to borrow its controls. Explicit
`clear` and startup stale recovery retain their existing host-stop requirements.
Shared parent directories remain the existing trusted starting points; these
creation records do not establish ownership against arbitrary same-user changes.

Accounts launches are single-use, including a lost launch reply. A second
`run-ui` refuses; safely stop/clear the fixture and start a fresh one instead.
Never delete `accounts-native-started` or reset logs to reuse a scene. Original
wait/close/host-stop uncertainty still retains ownership and can keep shutdown
incomplete. `--keep` keeps the repository fixture logs/screenshots; the private
runtime-backed Accounts namespace remains subject to normal runtime cleanup.

Source pairs are separate from side labels. The original ten-scene ports are
Before `dc2a10b0f3b925e5c2546722004782b629d4de3b` and After
`48f5254a97e6b8865c40f7d9a1cbc73f41c530f2`. The catalog pair is Before
`471bd5316a54ec9f4ed888f207dc11ceeffead8a` and After
`aa0606e351fc4a54ed097a1878655bd216caa856`; catalog Before is the old Accounts
feature/main composition, not the whole main baseline. Owners must bind actual
source/build/ELF evidence. An incompatible checkout fails fixture startup;
there is no ordinary-mode fallback. Catalog uses the fixed Nov1 UTC seed and
refuses at Nov2 2026 00:00 UTC; no broker time override extends it. F8 supplies
only the fixture's fixed typed Loaded event, not arbitrary callback data.

This recipe supplies source controls, not build or native admission. The real
three-graph build still needs a separately decided stable resource/build
admission. Existing per-checkout targets and per-process limits are not an
aggregate memory cgroup cap. A genuine User must start the reviewed broker;
actual bootstrap, matched executables, native screenshots/interaction and UX
review remain required. Synthetic Accounts rows and receipts do not qualify
provider/account/home/daemon behavior or grant User authority.

### What the sandbox can reach

Each fixture runs in a bubblewrap sandbox with its own HOME, XDG directories,
runtime directory, daemon and project. It has no network, so builds use only what
is cached. It sees none of your real HOME, credentials, `~/.local/share/cagents*`
or `$XDG_RUNTIME_DIR/cagents*`. `claude` there is this fixture script.

- **Read-only:** `/usr`, `/etc`, `/sys`, the Rust toolchain, Zig, Cargo's `bin`,
  `registry` and `git` directories, Zig's package cache, the checkout, and this
  repository's `.git`.
- **Writable:** the checkout's Cargo target, the fixture's `sandbox/` directory and
  runtime directory, tmpfs at `/tmp`, `/var` and Cargo's home, and the nested
  Wayland socket. `/dev/dri` is also bound, for the GPU.
- **Limits:** per-process address space (128 GiB) and file size (32 GiB), 8192
  processes, and no core files. Total memory is not capped, because that needs a
  cgroup, which this does not set up. Several fixtures building at once can still
  exhaust the host's memory. `run-ui` learns a build has finished from a line in
  a log the sandbox can write, so sandbox code can hold other fixtures' builds
  back until its own fixture is stopped.

On the host side only the broker, `niri-harness` and `niri` run. Nested niri runs
nothing from a checkout. Commands reach the sandbox over an inherited socket, not
a path, and its replies are bounded and treated as untrusted. A sandbox can forge
what its own commands appear to have done, including its own `status`, so treat
fixture output as evidence about that checkout only.

The broker protects you from code running inside the sandbox. It does not protect
you from an agent's shell of the same user, which can already reach the same files
and sockets, including `.worktrees/` and the broker's state under
`$XDG_RUNTIME_DIR/native-fixture-broker/`. It is a supported route that removes
the need for a workaround, not a boundary against that shell.

## Directly: from a person's own terminal

Use a new directory beneath the repository's ignored `.worktrees/` for every
run. Keep its HOME, XDG data/config/cache, daemon runtime, and project isolated
from the development application you normally use. For example, from the
checkout being validated:

```sh
fixture_root="$PWD/.worktrees/chat-composer-fixture"
fixture_runtime=$(mktemp -d "$XDG_RUNTIME_DIR/composer.XXXXXX")
mkdir -p "$fixture_root"/{home,data,config,cache,bin,project}
ln -s "$PWD/tests/fixtures/chat-composer-claude.sh" "$fixture_root/bin/claude"
git init --initial-branch=main "$fixture_root/project"
```

Run the UI through `scripts/run-development --browserless`. Prefix both the
runner and every CLI command with the same fixture environment:

```sh
fixture_env() {
  if [ "${CAGENTS_AGENT_AUTHORITY+x}" ]; then
    printf '%s\n' 'This fixture requires a genuine User shell; inherited agent authority was not modified.' >&2
    return 77
  fi
  env -u CLAUDE_CONFIG_DIR \
    HOME="$fixture_root/home" XDG_RUNTIME_DIR="$fixture_runtime" \
    XDG_DATA_HOME="$fixture_root/data" XDG_CONFIG_HOME="$fixture_root/config" \
    XDG_CACHE_HOME="$fixture_root/cache" PATH="$fixture_root/bin:$PATH" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" "$@"
}
fixture_env scripts/run-development --browserless --debug --log-file "$fixture_root/ui.log"
```

Keep the existing Cargo target/cache configuration when using a worktree; do
not start another build while the runner builds its matched executables. If the
harness help exposes `session start NAME --root ROOT --cmd CMD`, start a nested
Niri session with `niri-harness session start composer --root "$fixture_root/niri" --no-window-wait --cmd WRAPPER`,
where `WRAPPER` is a shell command that runs the fixture runner script. Before replacing
`XDG_RUNTIME_DIR`, convert the inherited relative `WAYLAND_DISPLAY` to an
absolute path (`$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`). Keep the inherited
`NIRI_SOCKET`, and use only this owned nested session for input and screenshots.
The runtime directory needs a short path because Unix sockets have a path-length
limit; keep it under the original XDG runtime directory rather than the checkout.

After the UI starts, use its newly built `cagents` CLI under the fixture
environment to create the project and synthetic agent:

```sh
fixture_cli="${CARGO_TARGET_DIR:-$PWD/target}/dev-runtime/cagents"
fixture_env "$fixture_cli" project open "$fixture_root/project"
fixture_env "$fixture_cli" --json agent spawn --provider claude --model haiku --surface chat \
  --repository "$fixture_root/project" \
  --prompt 'Can you investigate why the Kaggle submission failed?'
```

Open the resulting chat in the UI and check:

1. Enter a message and choose the **Steer** icon; hover its queued timing icon
   to confirm next-step delivery.
2. Enter a long URL and choose the **Queue** icon; hover its timing icon to
   confirm next-turn delivery. Its preview truncates within the available width
   without hiding its actions.
3. Capture the busy composer, including a separate unsent draft.
4. Recall a queued message with the **Edit** pencil; confirm its text moves into the
   composer and its queued entry disappears. Queue it again if needed.
5. Remove every queued entry, then click the **Stop** icon. Confirm the composer returns
   to its idle send state. Clear the draft and capture that state.
6. Repeat the queued-state checks at a narrow window width, including keyboard
   focus and the split composer toolbar.

Store screenshots under `docs/screenshots/` for PR evidence. Stop the owned
nested session, close its synthetic agent, and stop only its isolated daemon
when finished. The fixture directory can then be removed.

The daemon removes `CAGENTS_*` variables from a provider's environment, so
optional fixture behaviors are switched on by files in
`$fixture_root/home/.claude`, the fixture's Claude config directory. The
reply and error files are read for every prompt; the others take effect from
the provider's next start.

To check how an answer renders, copy a Markdown file to `fixture-reply.md`.
Each later turn answers with that file's contents instead of the default
sentence, so replacing the file changes the next answer. A few turns with a
long file give a transcript long enough to scroll.

For provider failure checks, write a diagnostic such as `Failed to
authenticate: OAuth session expired and could not be refreshed` to
`fixture-image-prompt-error`. Any prompt containing an image then settles with
that error before any reply. One Send of text plus images must show one failure
notice.

To check where a failure is drawn after delivered work, write a diagnostic to
`fixture-delivery-error` instead. Every prompt then gets a reply, a background
command's completion notice, a follow-up, and that error. The notice must
appear once, after the follow-up.

For tool disclosure checks, create `fixture-verbosity`. Each turn emits a
streamed MCP call with a large prompt, a Skill call, a failed MCP call, and a
multiline Bash command. It records complete tool inputs and results in the
synthetic provider history, so the same transcript can be reopened.

In the native chat, expand the activity group and check that the closed MCP
row says `src/app.rs`, the Skill row says `writing-copy`, and neither shows
internal framing. The failed `private.txt` step must retain `Permission denied`.
The shell preview must show `cat <<EOF` without its heredoc payload. Expand
individual steps and verify their complete Input/Output remains selectable
and copyable. Collapse them while the first turn is still busy, stop that
turn, and reopen the saved chat; compare the completed and restored content.
The fixture represents a reproducible payload path, not a captured user
conversation or evidence about the provider's answer prose.

For image preview and viewer checks, copy a PNG to
`"$fixture_root/home/fixture-image.png"` before spawning the agent. Native
launches drop `CAGENTS_*` variables, so the fixture reads the image from its
provider home; `CAGENTS_FIXTURE_IMAGE` overrides the path when the script runs
directly. Each turn then also returns that image as a provider-produced base64
image block, recorded in the synthetic history like other frames. Paste an image into the composer to
cover a user-attached image in the same transcript. Use synthetic images only.
