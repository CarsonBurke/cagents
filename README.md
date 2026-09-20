# cagents

cagents is a native Wayland desktop application for organizing coding-agent sessions, repositories, Git worktrees, terminals, and embedded browser surfaces in one workspace. It also provides a scriptable CLI for the persistent terminal daemon.

> [!IMPORTANT]
> This repository is hosted privately at `CarsonBurke/cagents`. Release downloads require an authenticated GitHub CLI account with access to that repository; tagged release builds stamp the repository into the released copy of `install.sh` automatically.

## Platform and runtime requirements

The initial supported release target is **Linux x86_64 with glibc**, built on Ubuntu 22.04, in a **Wayland** session. X11, musl, ARM, and other operating systems or package formats are not currently supported. A normal graphical login should provide an absolute `XDG_RUNTIME_DIR`; cagents uses it for the private daemon socket.

The `.deb` declares `git (>= 2.36)`, `libc6 (>= 2.35)`, Fontconfig, FreeType, GCC/libstdc++, Wayland client, xkbcommon, and zlib runtime dependencies. The portable archive needs equivalent libraries and Git 2.36 or newer for NUL-delimited worktree discovery. Ubuntu 22.04's default Git 2.34 must be upgraded separately. A shell, PTY support, and Linux `/proc` are required for hosted terminals.

The GitHub CLI, `gh`, is optional for local-only use but required for cagents GitHub and pull-request operations. Install it with your distribution, then authenticate as the same desktop user:

```sh
gh auth login
gh auth status
```

cagents delegates authentication to `gh`; it does not ask for or store a GitHub token itself.

## Run from source

Development builds require Rust 1.93, Zig 0.15.2, and the native dependencies used by the release workflow. On Ubuntu 22.04:

```sh
sudo apt-get update
sudo apt-get install --yes --no-install-recommends \
  build-essential clang cmake git libclang-dev libdbus-1-dev \
  libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev libgbm-dev \
  libgl1-mesa-dev libssl-dev libudev-dev libwayland-dev \
  libxkbcommon-dev ninja-build pkg-config python3 xdg-utils xz-utils
rustup toolchain install 1.93.0 --profile minimal
rustup default 1.93.0
```

Install Zig 0.15.2 from [ziglang.org](https://ziglang.org/download/) and ensure `zig` is on `PATH`. The terminal host uses Ghostty's native library, so daemon builds require Zig. Ordinary debug builds automatically use the isolated development instance; optimized builds are production unless the Cargo `development` feature is explicitly enabled.

The CLI, UI client, and terminal host have separate Cargo feature boundaries:

```sh
# Control CLI: no UI, Ghostty, SQLite, or Servo graph.
cargo check --locked --no-default-features --bin cagents

# Terminal daemon: Ghostty/PTY host, no UI or Servo graph.
cargo check --locked --no-default-features --features host --bin cagentsd

# Browserless UI client: libcosmic, no Ghostty or Servo graph.
cargo check --locked --no-default-features --features ui --bin cagents-ui

# Integrated UI client: libcosmic and Servo, no Ghostty graph.
cargo check --locked --features integrated-browser --bin cagents-ui
```

Cargo features are package-wide, so combining `ui` and `host` in one `--bins` invocation would undo that isolation. Build a matched browserless release as three feature-disjoint targets:

```sh
cargo build --locked --release --no-default-features --bin cagents
cargo build --locked --release --no-default-features --features host --bin cagentsd
cargo build --locked --release --no-default-features --features ui --bin cagents-ui
```

Browserless builds keep terminals, repositories, worktrees, GitHub pull-request flows, and persistence. New links use the desktop portal and fall back to the system `xdg-open` handler when a portal is unavailable; Debian packages install `xdg-utils`, and portable/source users must provide it for that fallback. A restored browser card retains its saved URL, history, scroll position, and fidelity, shows that the integrated browser is unavailable, and offers **Open in system browser**. Running only one binary from an unrelated directory is not a supported layout; `cagents`, `cagents-ui`, and `cagentsd` must remain together.

Launch the native UI through the development runner:

```sh
scripts/run-development --browserless
scripts/run-development
```

The browserless UI preserves saved browser descriptors and opens their URLs through the system browser instead of compiling Servo. The normal development runner enables `integrated-browser` only for the UI target; the matched CLI and daemon retain the same full-product identity without resolving Servo.

For UI diagnosis, add `--debug`. It enables Rust backtraces and `cagents=debug,cagentsd=debug,warn` tracing; an explicit `RUST_LOG` or `RUST_BACKTRACE` value takes precedence. Use `--fresh-daemon` after changing daemon, terminal-host, or protocol code so the UI cannot reconnect to an older development daemon:

```sh
scripts/run-development --browserless --debug
scripts/run-development --debug --fresh-daemon
scripts/run-development --browserless --debug --log-file /tmp/cagents-ui.log
```

`--log-file` captures UI tracing and panic output in a stable artifact while Cargo diagnostics remain in the invoking terminal. Fresh-daemon mode compiles a feature-matched control CLI, pins and validates the isolated daemon through a Linux pidfd, and signals it without depending on the old wire protocol; stopping it terminates its hosted development terminals. `scripts/run-development` is POSIX shell; the repository contains no Python development tooling. Python remains a native build prerequisite for the full Servo dependency graph.

The runner builds the three sibling executables with separate Cargo feature sets and executes the selected UI through Cargo, so configured target directories and target triples are honored. UI-only compilation never builds Ghostty, and daemon-only compilation never builds libcosmic or Servo. Its `dev-runtime` profile keeps UI dependencies optimized for usable UI performance while the ordinary `dev` profile stays cheap for checks. The development instance has its own application ID, configuration, session database, transcripts, application lease, and `$XDG_RUNTIME_DIR/cagents-development/` daemon, so it can run beside the installed production instance. `cagents`, `cagents-ui`, and `cagentsd` must remain together.

### Build and test resource usage

Repository-local Cargo configuration defaults to **two concurrent compilation jobs**, including compilation for `cargo test` and every build in `scripts/run-development`. Cargo otherwise defaults to the logical CPU count, which can create substantial compiler RAM pressure on high-core-count or shared machines. Override this with standard Cargo controls: `CARGO_BUILD_JOBS` also applies to the runner, while `-j` takes precedence for an individual Cargo invocation. Run the isolated feature lanes sequentially, not as independent concurrent cold builds.

```sh
# Reduce compilation concurrency further on a memory-constrained host.
CARGO_BUILD_JOBS=1 scripts/run-development --browserless

# Opt into more compilation jobs when system RAM is available.
cargo check --locked -j 4 --no-default-features --features ui --bin cagents-ui

# Test only the owning boundary and target; keep UI and host separate.
cargo test --locked --no-default-features --features host --lib terminal::
cargo test --locked --no-default-features --features host --test terminal_load_contract
cargo test --locked --no-default-features --features ui --test session_restore_contract
```

Compilation jobs and test execution threads are separate controls: `-j` does not serialize tests within a test binary. For a specific resource-heavy test target, append `-- --test-threads=1`; there is no repository-wide test-thread cap. A test-name filter limits execution, not compilation of the selected target and feature graph, so start with the narrow feature lane and `--lib` or `--test NAME`, rather than `--all-features`.

The jobs setting is not a hard process or memory limit. In the pinned dependencies, parallel `cc` builds share Cargo's jobserver; CMake uses it for supported Makefile builds and otherwise can launch a separate pool based on `NUM_JOBS`. SpiderMonkey's Make 4.4 compatibility path also starts a separate pool sized by `NUM_JOBS`, potentially approaching twice the Cargo job count alongside Cargo work. Ghostty's build script invokes `zig build` without forwarding a job limit, so Cargo's setting alone does not guarantee bounded Zig internal parallelism. A single compiler or linker can still require substantial RAM. The existing reduced dependency debug information and optimized `dev-runtime` dependencies remain intentional; use ordinary `cargo check` for cheap checks and the runner for usable UI performance.

Rust, C/C++, Zig, and linker allocations consume **system RAM**, not GPU VRAM. GPU memory reported by tools such as `nvidia-smi` belongs to GPU workloads (for example, rendering or ML), not these compiler jobs. Those workloads may also consume system RAM; lowering Cargo concurrency does not reclaim their allocations or guarantee a build will fit when RAM and swap are already under pressure. Browserless lanes avoid the large Servo/SpiderMonkey compilation graph when embedded-browser behavior is not being changed.

### Redraw scheduling measurements

```sh
sh scripts/run-redraw-latency /tmp/cagents-redraw-reference
```

The output directory must not already exist. This opt-in `latency-harness` lane
builds the CLI, host, and browserless UI separately in **release** mode, then runs
five fresh UI processes with private HOME/XDG directories. Each run walks eight
synthetic unloaded cards using a fixed seed: 32 warmups and 256 recorded adjacent
focus changes. No user sessions or PTYs are loaded. A graphical session, `jq`,
`timeout`, and the normal native release prerequisites are required.

The metric is **requested state committed → first matching-view
RedrawRequested**. It is redraw scheduling latency, not renderer submission,
compositor presentation, scanout, or arbitrary input-to-visible latency.
`card_focus`, future `palette_open`, and future `palette_query` remain separate
actions. Superseded or unfinished generations settle as abandoned, never as a
successful redraw.

Private raw JSON, median/p95/p99/worst summaries, a reference-machine manifest,
and the strict sample schema are emitted. Reports allowlist revision, dirty
state, fixture/build mode, session/compositor, scale, numeric hardware facts,
local interaction sequences and durations. They contain no user paths, titles,
URLs, query text, terminal contents, user identifiers, or stable entity IDs.
The disabled recorder is an explicit compile-time no-op: it reads no recorder
clock and allocates nothing. Provisional **50 ms p95 / 100 ms p99** budgets remain
non-gating until a named reference baseline is accepted. Do not compare ordinary
unoptimized `dev` or partially optimized `dev-runtime` results with this release
baseline.

The `linux-headless-release-reference` fixture records five fresh-process runs
on a Ryzen 9 9900X, NVIDIA RTX 5090/Vulkan UI, and Sway 1.12 headless
GLES2 compositor at 1280×800, scale 1.0: **16.16 ms median, 18.32 ms p95,
19.75 ms p99, 25.32 ms worst**, across 1,280 samples. Raw samples and machine
facts are in `fixtures/linux-headless-release-reference.json`. This is a
non-gating local reference, not a physical-display baseline.

### Terminal pipeline measurements

```sh
scripts/benchmark-terminal all
# Use only a dedicated test daemon: the fixture creates terminal descriptors.
scripts/benchmark-terminal all --socket /absolute/private/runtime/cagentsd.sock
RUST_LOG=cagents::terminal_pipeline=trace scripts/benchmark-terminal input
```

These release workloads exercise idle, burst, short-line, sustained, full-screen
ANSI, large/resized, and four-terminal output, plus single/multi-terminal input
probes. Default mode uses real PTYs in-process; `--socket` uses the matched daemon
and production hosted IPC connection. Both drive synthetic 60 Hz snapshot
acknowledgements. Their input-to-snapshot budgets are **50 ms p95 / 100 ms p99**;
JSON records the measured percentiles and whether they pass. This is not an
input-to-present claim. Daemon mode retains exited descriptors, so do not use a
personal or production daemon.
The initial non-gating release throughput budgets are **50 MiB/s aggregate**
for conventional sustained single/four-terminal output and **5 seconds**
for the 32 MiB pathological short-line workload. JSON includes actual emitted
bytes, throughput, and pass/fail; process startup and final-state observation
are included in wall time.

Snapshot capture consumes generation-matched draw credit rather than capturing
every PTY batch. Parsing and semantic events continue without credit, dirty state
is retained, and EOF publishes final contents. Idle terminals do not request a
continuous redraw loop. Long-lived hosted and daemon IPC connections reuse frame
buffers. Opt-in `cagents::terminal_pipeline=trace` records parse/capture,
encode/decode, UI apply, and draw stages where exercised; tracing overhead is
excluded from release throughput claims. Summarize a collected log with
`target/release/examples/terminal-workload --summarize-trace /path/to/trace`.
Missing stages are reported, not estimated. Physical scanout remains unmeasured.

The same reference records all nine untraced backend/IPC workloads and a
separate traced native run. Iced draw was the largest instrumented CPU stage
(4.06 s accumulated; 0.88 ms p95), ahead of snapshot capture (1.30 s) and
IPC encoding (1.19 s); concurrent stage totals are not additive latency.
Native virtual-keyboard input to a matching compositor screencopy pixel,
including observer overhead, measured **37.85/47.91 ms p95/p99** for one
terminal and **38.82/65.10 ms** with three additional updating TUI terminals
(100 samples each). Both fit the 50/100 ms input budgets. This is an upper
bound to compositor-visible pixels, not presentation timestamps or physical
scanout. The fixture uses `terminal-workload --emit visual-input`, which
accepts `r`, `g`, and `b` to repaint with distinct RGB colors; `q` exits.
The settled idle native terminal produced no additional terminal draws
and no measurable UI CPU ticks in a three-second observation.

## Install a release

Authenticate the GitHub CLI as your desktop user with access to the private
repository, then choose a published version:

```sh
gh auth login --hostname github.com
gh auth status --hostname github.com
REPOSITORY='CarsonBurke/cagents'
VERSION='0.1.2'
```

### Recommended: download, verify, inspect, then install

This flow pins an exact release, verifies both downloaded files against its checksum manifest, lets you read the installer before executing it, and supplies the already-verified archive back to the installer:

```sh
(
set -eu
download_dir="$(mktemp -d)"
trap 'rm -rf "$download_dir"' 0
trap 'exit 1' HUP INT TERM
archive="cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"

gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern install.sh --pattern SHA256SUMS --pattern "$archive" \
  --dir "$download_dir"

(
  cd "$download_dir"
  sha256sum --ignore-missing --strict --check SHA256SUMS
)
less "${download_dir}/install.sh"

CAGENTS_REPOSITORY="$REPOSITORY" \
CAGENTS_ARCHIVE="${download_dir}/${archive}" \
CAGENTS_CHECKSUMS="${download_dir}/SHA256SUMS" \
  sh "${download_dir}/install.sh" --version "$VERSION"
)
```

SHA-256 confirms that the files match the release manifest; it does not protect against compromise of the GitHub repository or release itself. Inspect the tag, release, and installer according to your trust requirements.

The installer uses that existing `gh` authentication for both latest-release
selection and pinned asset downloads; it never copies tokens into its state,
URLs, or command arguments. Missing authentication for the canonical private
repository fails before changing the installation. Supplying both local
`CAGENTS_ARCHIVE` and `CAGENTS_CHECKSUMS` keeps installation offline. Anonymous
HTTPS downloads are only a fallback for explicitly selected public repositories,
not for the private canonical release.

The installer never uses `sudo` and refuses to run as root. It installs an immutable version below `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`, switches the `current` symlink atomically, creates desktop metadata links under `${XDG_DATA_HOME:-$HOME/.local/share}`, and exposes only `$HOME/.local/bin/cagents` on `PATH`. The application menu entry uses that installed binary and icon by absolute path, so launching from the desktop does not depend on `PATH`. Terminal use of the `cagents` command still requires `$HOME/.local/bin` on `PATH`.

Rerun the installer normally for a compatible UI update: the existing daemon and
its live PTYs stay running. To explicitly replace the daemon as well, append
`--restart-daemon` to the verified installer invocation above. That opt-in requires
checkpoint support in the **currently installed** CLI and daemon; it resumes
supported shells and exact agent conversations in new processes, not the old PTYs.
See [Daemon and user data](#daemon-and-user-data) for the recovery and legacy
bootstrap restrictions. `--restart-daemon` cannot be combined with uninstall or
purge.

### Convenience authenticated install

This shorter flow uses the same GitHub account but executes the downloaded
installer without inspecting it or independently verifying its checksum. Use it
only if you trust the GitHub repository and release. It resolves latest once and
uses that exact tag for both the installer and archive:

```sh
(
  gh auth status --hostname github.com || exit 1
  tag="$(gh release view --repo github.com/CarsonBurke/cagents --json tagName --jq .tagName)" || exit 1
  case "$tag" in v*) ;; *) exit 1 ;; esac
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  gh release download "$tag" --repo github.com/CarsonBurke/cagents \
    --pattern install.sh --dir "$download_dir" &&
    sh "$download_dir/install.sh" --version "${tag#v}"
)
```

To pin a version, download its installer and pass the same version to `sh`.
Append `--restart-daemon` only when an explicit full daemon update is intended:

```sh
(
  gh auth status --hostname github.com || exit 1
  VERSION=0.1.2
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  gh release download "v${VERSION}" --repo github.com/CarsonBurke/cagents \
    --pattern install.sh --dir "$download_dir" &&
    sh "$download_dir/install.sh" --version "$VERSION"
)
```

### Debian package

Download the release package and checksum manifest, verify the package, then let APT resolve its shared-library dependencies:

```sh
gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern "cagents_${VERSION}_amd64.deb" --pattern SHA256SUMS
sha256sum --ignore-missing --strict --check SHA256SUMS
sudo apt install git "./cagents_${VERSION}_amd64.deb"
```

The Debian package installs the three executables together under `/usr/libexec/cagents`, exposes `/usr/bin/cagents`, and installs the desktop entry, icon, and AppStream metadata system-wide.

The separately named browserless package can be installed alongside the full package. It uses `/usr/libexec/cagents-browserless`, exposes `/usr/bin/cagents-browserless`, and installs separately identified desktop and AppStream metadata:

```sh
gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern "cagents-browserless_${VERSION}_amd64.deb" --pattern SHA256SUMS-browserless
sha256sum --ignore-missing --strict --check SHA256SUMS-browserless
sudo apt install git "./cagents-browserless_${VERSION}_amd64.deb"
cagents-browserless ui
```

### Portable archive

The archive is self-contained as a directory layout, not as a static executable. Keep its `bin` directory intact:

```sh
gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern "cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" --pattern SHA256SUMS
sha256sum --ignore-missing --strict --check SHA256SUMS
tar -xzf "cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
"./cagents-${VERSION}/bin/cagents" ui
```

## Use cagents

Launch **cagents** from the desktop application menu, or launch it from a terminal:

```sh
cagents ui
```

- Open repositories from the **Projects** pane on the left. Select a worktree to
  restore its terminals; selecting it again does not create another terminal.
  Opening an already registered repository selects it without duplicating its
  project or disturbing existing sessions.
  **New terminal** (`Ctrl+T`) starts in that worktree and inserts immediately to
  the right of its focused column.
  **Clone focused pane** (`Alt+E`) opens another pane of the same kind:
  terminals launch a separate process and native chats start a separate
  conversation; browser and repository panes open at the same location.
  Cloning does not share the original running process or conversation.
  **New channel** (`Alt+G` or the project header's channel-plus button)
  starts an independent native conversation in the project's main worktree.
  Channels have their own sidebar rows and panes, separate from worktree panes;
  select a channel again to return to it. A channel is a container: it holds
  the chats opened while it was selected, and its row shows how many when there
  is more than one.
  In cagents, editor tabs use the ceditor mark; recognized agent terminals and
  native chats use their OMP, Claude, or Codex provider mark.
  Each tab strip contains only its selected worktree or channel. The **New tab**
  menu offers a terminal, native chat, OMP, Claude Code, Codex, or browser tab,
  with provider icons and keyboard navigation; there is no Workspaces dropdown.
  Drag a project header to reorder projects; the order survives restarts.
  Project headers stay visible above their worktrees while scrolling. The
  primary worktree stays first; the sort control selects newest, oldest, or
  alphabetical order for the remaining worktrees.
  Worktree labels prefer the matching pull request title, falling back to a
  readable branch basename without account prefixes. Tooltips and search retain
  the full Git branch. Quiet worktree names are muted; pull-request icons retain
  their semantic state colors. Unread updates remain emphasized until acknowledged;
  viewing a waiting agent clears its attention badge without changing its activity.
  The All, Attention, and Pull requests views retain independent sidebar scroll positions.
- The right-hand inspector has icon-only **Files**, **Changes**, **Pull request**,
  and **Details** tabs below the window header, separated from its content by a
  border; hover for tooltips. Files browses the selected worktree,
  Changes provides diffs, staging, commits, push/publish, and history,
  Pull request inspects the worktree's matching PR, and Details follows the
  focused card. Both file and change lists support **Tree** and **Flat** modes.
  Use the sidebar toggle to hide or reveal the inspector. Right-click project or worktree
  rows for their actions; there is no ellipsis menu button. Worktree rows show
  matching pull requests and their reported CI status. Icon tooltips appear
  after one second. Change rows color their status/counts and replace them with
  action buttons on hover. Untracked text files show measured additions;
  binary, unsafe, oversized, or budget-limited files retain unknown counts.
  The Changes tab highlights local or committed branch changes; committed files
  appear below staged and unstaged files, compared against the known default
  branch's merge base. Visible repository views refresh automatically while
  the application is focused. File filters open from their search icons.
- Switching worktrees remembers each strip's horizontal scroll position and
  focused column. Hidden terminal presentation is released while hosted PTYs
  keep running, and editor buffers remain intact. Scroll positions, worktree
  selection, and column focus survive restarts independently in each window,
  including when a shared browser column has focus. Application close
  checks unsaved editors in every worktree, not only the visible one.
  Deleted worktrees disappear from the sidebar without transferring their
  panes to a parent worktree.
- Use `cagents ui --new-window` for another independently movable window.
  `cagents ui --window monitor-two`
  opens or activates a named durable window; plain `cagents ui` activates the
  primary window. New windows copy the source workspace once, then save their
  own selection, pane widths, inspector, and scroll positions. Existing terminal
  and native-chat IDs still refer to the same daemon-owned processes: input and
  explicit process closure affect every window displaying that session. Closing
  only a window leaves those processes running.
  Closed windows retain recovery lineage across successive full upgrades; they
  do not need to be opened for every release.
- Hold **Alt+Q / Alt+W** or **Alt+Left / Alt+Right** to move through columns.
  Mouse-wheel and trackpad scrolling stay inside pane content; they do not pan
  the workspace strip. **Alt+R** cycles the focused pane through **1/3 → 1/2 →
  2/3 → full** viewport width and repeats while held. These widths remain
  viewport-relative when the window is resized.
  Navigation scrolls only far enough to reveal the target pane. New panes
  default to half the viewport width. **Activate panes on hover** is enabled
  by default and changes focus without scrolling; disable it in Settings
  to require a click.
- When creating a worktree, enter a branch name, a Linear issue ID such as
  `APP-42`, or an `https://linear.app/.../issue/APP-42/title-slug` URL. URLs use
  the issue ID and supplied title slug, without fetching Linear metadata.
  The managed parent defaults to `worktrees/` relative to the repository root;
  Settings accepts an absolute or repository-relative parent, and the creation
  dialog also accepts an explicit destination.
- Open **Settings** from the lower-left gear. Valid changes save automatically;
  invalid drafts show an error without replacing the last valid setting.
  Appearance includes system/light/dark themes and interface scaling.
  Terminal controls include font family and size, line spacing, padding,
  cursor shape, and independent **Dark**, **Light**, and **Solarized dark**
  palettes. The default cursor is a beam. A thin draggable scrollbar appears
  only on the focused terminal when scrollback is available.
  The modal dims the workspace and preserves input focus until dismissed.
  Interface text uses the desktop's installed font and Fontconfig fallbacks;
  install Open Sans and Noto Sans Mono if those families are not already available.
- The editor saves automatically after a pause by default. Conflicts, save
  errors, and durability warnings stop automatic saving and remain visible;
  autosave does not silently overwrite external changes. Disable **Save files
  automatically** in Settings for manual saving.
  **Wrap long lines** is off by default. Editor and terminal text default to
  14 logical pixels; saved terminal font preferences remain unchanged.
  Images open as previews. File/view/edit/diff controls occupy a separate
  toolbar above the content; unavailable save and comparison actions are
  disabled. Editable content has a line-number gutter; its draggable scrollbar
  appears only when content overflows, including long wrapped lines. Working-tree diffs edit the real file buffer
  with the same selection, IME, and safe-save behavior; removed content is
  selectable read-only context. Staged and historical content remain read-only.
  File/mode switches retain bounded, in-memory viewport coordinates without
  retaining duplicate buffers or writing each scroll to disk. Comparisons load
  on demand and cancelled jobs stop; subtle backgrounds preserve syntax contrast.
  File and diff views retain syntax highlighting, including CUDA sources.
  Contextual external-editor icons open the current project, worktree, or file
  without a separate application-picker dialog.
- When a daemon incarnation changes, cagents authenticates the replacement and
  checks for surviving owners before recovering safe shell or exact agent
  launches. Unknown or orphaned processes block recovery rather than risking
  duplicate writers. **Retry** explicitly authorizes saved launch recipes that
  cannot be replayed automatically. Transcripts remain available; recovery
  starts or adopts a proven replacement, not the old terminal process.

### Standalone editor

`ceditor` is a separate native application in the same Cargo workspace. It
shares the editor and repository UI without starting the cagents daemon,
terminal host, or embedded browser. File-list mode and settings persist independently.
Open tabs, order, and the active tab restore on launch; explicit paths are also
opened, and a new session with no paths browses the current directory.
**Ctrl+T** or the plus control after the last tab creates an untitled note in a
scratch folder next to the editor settings; **Ctrl+S** then chooses where that
note lives. Standalone tabs omit the redundant application icon and use the same
compact geometry and pane separators as cagents.

Language-server assistance is off by default in ceditor. Syntax highlighting,
editing, saving, and Git work without rust-analyzer or another language server.
Enable **Language-server assistance** in Editor settings for semantic diagnostics,
completion, and symbol information; disabling it shuts down the editor's services.
The choice persists independently of the `language_servers` configuration.
Service failures appear in the header's **Notices** tray, with readable error
details and server stderr rather than a log block below the buffer. Closing the
tray preserves the evidence; **Dismiss notices** clears it.

```sh
scripts/run-ceditor-development /path/to/project
scripts/run-ceditor-development /path/to/file.rs
scripts/install-ceditor
ceditor /path/to/project
```

The installer defaults to `~/.local`; use `--prefix /absolute/path` to install
elsewhere. The development runner uses an isolated application/settings identity.

Right-click a file to rename, duplicate, or copy its absolute or relative path.
Rename and duplicate never overwrite an existing destination. Unsaved changes
are resolved before switching files, and cancelling a file chooser leaves the
current document and error state unchanged.

### Native agent chat

Supported OMP, Claude, and Codex terminal sessions offer **Switch to chat**.
The switch uses an exact provider conversation identity, not terminal-text
scraping, a most-recent-session lookup, or a fork. The switch opens the detected
conversation directly when process-bound exact identity is available. A confirmed
fresh OMP source can switch before publishing a saved conversation: cagents
waits for the replacement's resumable identity before stopping the source.
Missing or ambiguous established identities leave the terminal intact; resume
the exact conversation in the provider CLI and retry.
For Codex's shared server, copy the full conversation UUID from the intended
CLI's `/status`, preview its title and directory read-only, then explicitly
confirm attachment. The source terminal remains running; cagents does not choose
the most recent conversation or infer its identity from the working directory.
Native handoff preserves the source process's explicit provider account,
environment, and trust-store paths. If credentials or provider settings cannot
be transferred safely, the switch refuses before stopping the live terminal;
it never silently substitutes the daemon's account.
The new-tab menu also starts a native conversation without a source terminal.
GUI-only credentials and authenticated proxy configuration travel through an
execution-only channel rather than the saved session descriptor.
That context remains in daemon memory when returning to a terminal and switching
back. A full daemon upgrade refuses before shutdown if its supported recovery
path cannot reproduce the same execution environment; it does not serialize
credentials into the upgrade checkpoint or silently choose another account.
A successful return retains only a lightweight replay receipt. The GUI releases
it after its terminal replacement is durably saved, including retries after
restart or early terminal exit; unresolved handoffs block a full upgrade.

- **OMP and Claude:** switching performs a stop-and-resume handoff. Active work
  is interrupted, the CLI stops, and native chat resumes the same conversation.
  Returning to the terminal stops the native writer before resuming the CLI.
  Unsent drafts, attachments, and pending commands block return so they cannot
  be silently lost.
  This is not a simultaneous live view; available history depends on the
  provider. The original launch recipe and terminal descriptor are retained.
- **Codex:** join the exact existing conversation directly over the shared
  daemon's local Unix socket, leaving the original CLI alive. Launches using
  `--remote unix://PATH`, `--remote=unix://PATH`, and existing `--sock` proxy
  recipes are supported; bare `unix://` uses the existing default socket under
  `CODEX_HOME`. Remote `ws://` and `wss://` endpoints are not supported.
  If that session cannot be joined safely, cagents keeps the terminal and
  reports an error; it does not launch a proxy process, start a replacement
  daemon, or fall back to a separate conversation. Returning to the terminal
  detaches the native view while active work continues.

The native view presents streamed messages, tool activity, and explicit provider
approval/input requests. Controls appear according to provider capabilities:
OMP supports steering, queued follow-ups, attachments, compaction, model and
thinking selection; Claude supports attachments, advertised models and slash
commands, compaction when advertised, and permission modes. Codex controls
depend on the joined session's advertised direct-input and model capabilities;
image attachments require an image-capable model. Unsupported controls are not
substituted with terminal keystrokes. Usage and cost reflect provider reports.

Changing worktrees or closing the application window leaves daemon-owned chat
work running in the background. Explicit **Stop and close chat** interrupts
active work and waits for daemon acknowledgement before removing the card.
Disconnects retain the draft and show **Reconnect to this session** rather than
silently starting another conversation.

Transcripts use a private disk-backed cache and bounded, pageable display
windows rather than keeping the entire conversation in UI memory. Use **Load
earlier history** and **Jump to latest** to navigate. This cache is not a
permanent archive or a fixed total disk quota; explicit chat close releases it,
and provider session storage remains the source for resumed history.

### Terminal and repository operations

The control CLI includes human-readable output by default and stable JSON envelopes for automation:

```sh
cagents daemon status
cagents terminal list
cagents terminal spawn -- bash -lc 'printf "hello from cagents\\n"'
cagents --json terminal list
cagents daemon stop --require-empty
cagents daemon stop --confirm
```

Run `cagents --help` or `cagents terminal --help` for the complete command set.

`daemon stop --require-empty` atomically rejects both established terminals and
in-flight spawn reservations. `--expected-generation N` additionally rejects an
inventory changed since preflight. `daemon stop --confirm` requires an interactive
terminal, shows the exact count, and commits a daemon-bound confirmation valid for
60 seconds; inventory changes require a new confirmation. A successful response
means **termination requested**, not that every PTY has exited. The explicit
`daemon stop` command without these flags remains a force-stop operation.

`cagents daemon prepare-upgrade --yes` is the explicit durable full-update handoff.
Without `--yes` it refuses before contacting the daemon. Its JSON success command
is `daemon.prepare_upgrade`, with `terminals` and `chats` checkpoint counts; success
means checkpoint persisted and shutdown requested, so callers must still wait for
`daemon status` to report stopped. Prefer the installer's `--restart-daemon` flow
when replacing an installed release.

If identified agents have no exact resume identity, the default remains to refuse
the upgrade and list every blocker. `daemon prepare-upgrade --yes --relaunch-agents`
explicitly accepts fresh conversations for those agents. The checkpoint records
this choice and their cards say **Relaunched after upgrade**. Unrelated shell jobs
still block the upgrade. The installer accepts `--restart-daemon --relaunch-agents`
only when the installed CLI supports recording this acceptance.

`daemon status` reports both `cli_version` and `daemon_version`; the latter is
read from the running daemon and is null when no daemon can report its build.
Ctrl-click a printed HTTP or HTTPS address in a terminal to open it using the
configured browser behavior.

If pull-request creation fails, cagents restores the submitted title, description,
branches, and draft setting. If the remote outcome is uncertain, the form remains
editable but another creation attempt is blocked until you check GitHub and confirm
that no pull request was created.

GitHub refresh immediately revokes mutation authority. Missing CLI,
unauthenticated, unassociated, loading, failed, and ready-empty are distinct
states; any retained PR rows are stale display data, not an authorized mutation
target. Only a matching refresh generation can enable creation again.

Worktree creation records a versioned request and exclusive mutation identity in
the session journal before directory or Git effects. The operation stays visible
through service-reported validation, base resolution, Git, and reconciliation.
**Cancel** requests termination once; it does not promise rollback. Restart
rehydrates journal records without replaying commands and checks process leases,
Git locks, exact checkout identity, and checkout-completion evidence. Only proven
**NotCreated** permits a fresh operation ID; **OutcomeUnknown** requires manual
inspection. A Git ownership lock is retained through the durable Created commit;
failure to release it is reported rather than hidden.

Closing an idle shell card does not require confirmation. A running agent,
foreground child, background job, or uncertain process state opens **Cancel**
and **Close** confirmation, with Close focused. Pointer close and Ctrl+W share
this policy; Cancel/Escape preserve the card and restore focus. The decision
uses the process tree, not shell prompt instrumentation: a shell builtin with
no child process can close without confirmation. **Don't ask me again** persists
the explicit opt-out. Confirmation revalidates terminal identity and daemon
proof; an unreachable daemon leaves the card intact.

Worktree rows distinguish unread updates from **working** and **waiting** agent
states. Attention is tracked per pane: activating a pane acknowledges only
that pane, not every session in the selected worktree. OMP uses its explicit terminal-title
lifecycle, Codex uses the existing local shared-daemon API, and Claude uses
`agents --json` from the detected CLI installation and account. Native chat
uses provider protocol events. Unsupported or unavailable evidence remains
unknown; output volume alone never means an agent is working, and activity
evidence never authorizes conversation recovery.

Passive Codex/Claude queries are bounded and cached, run outside the terminal
input loop, and do not request hidden terminal snapshots. Hidden processes
continue while their terminal presentation is suspended. The terminal view
has no activity-status strip.

## Daemon and user data

There is no system daemon or service unit. The standard production installation starts its per-user sibling `cagentsd` immediately and registers it for graphical-login autostart. While that daemon is running, cagents exposes a system-tray item that opens the UI and can stop the daemon. The daemon remains independent of the UI so terminals survive window and UI-process restarts.

Full and browserless production builds share the established daemon socket and
session store. Protocol **10** deliberately retains that namespace, preventing a
second daemon owner while an incompatible older process is still running;
the handshake rejects incompatible clients without terminating their PTYs. Upgrade
the three companion executables together. Each window holds an exclusive slot
lease and its own session database; a shared compatibility fence excludes older
single-window UIs. Startup rejects an older running UI or Debian peer package
that predates the lease marker. Upgrade that peer before switching variants.

By default, `install.sh` verifies the staged CLI against the running daemon before
switching `current`. Compatible upgrades preserve the daemon PID, session proof,
and PTYs. An incompatible default upgrade can stop only an atomically empty
daemon. Otherwise it leaves the current installation intact: finish the terminals,
or explicitly confirm their termination using the installed CLI, then retry.
Older CLIs without the atomic shutdown operation fail closed; there is no
force-stop fallback. If publishing launchers fails, the installer restores the
previous version without stopping compatible sessions. Existing version
directories are retained.

For an explicit full update from the command line, close the application windows,
then run the verified installer with `--restart-daemon`. The in-app **Update**
action coordinates window saving and closing automatically.
Before switching any executable, it checks that the old installed CLI advertises
`daemon prepare-upgrade`, calls that CLI with
`--json daemon prepare-upgrade --yes`, and waits for the old daemon to report that
it has stopped. Preparation is daemon-owned: it validates recovery authority,
freezes mutations, durably checkpoints supported sessions, and only then requests
shutdown. Unsupported sessions, in-flight reservations, or stale authorization
reject preparation rather than terminating sessions. The installer never builds a
checkpoint from `terminal list` and never falls back to ordinary `daemon stop`.

The replacement restores supported shell launches and the same provider-backed
agent conversations, including native chats, only after proving that previous
writers are gone. These are **new processes and PTYs**, not preservation of shell
memory, foreground jobs, or arbitrary commands. Unknown conversation identity,
unsupported command recipes, and surviving or ambiguous owners block recovery;
the durable checkpoint remains available instead of inventing a fresh
conversation or replaying commands. Keep the matching provider session storage.
Empty process arguments or environment can occur during `exec`, so recovery
requires inspectable evidence before restarting sessions. An intentionally empty
environment is accepted only when Linux exposes its initialized zero-length
address range; missing or redacted ranges cannot authorize a duplicate writer.
An old shell's unsaved in-memory variables or jobs are not a resumable shell
snapshot.
Daemon startup waiting scales with the checkpoint inventory, including the
provider-confirmation allowance for each native chat. A missing listening socket
does not mean shutdown is complete while the daemon still holds its inventory
lease.

If publication fails after checkpointing, the installer restores the previous
version and starts its daemon against the durable handoff. A replacement startup
failure rolls back only when its status proves it stopped; if startup state is
ambiguous, the installer retains the new installation and recovery state rather
than risking a second writer. Inspect `cagents daemon status` before retrying.

An interrupted checkpoint is not blindly replayed. After confirming that every
previous provider writer has stopped, `cagents daemon abandon-upgrade --yes`
archives the retained checkpoint without resuming any process. It requires a
stopped daemon and refuses to overwrite an earlier archive. Reopen the exact
conversations manually using their provider session storage; do not delete
checkpoint evidence merely to bypass an ownership error.

Legacy installations without `daemon prepare-upgrade` cannot provide this
handoff. `--restart-daemon` refuses without changing the active installation or
stopping sessions. For the one-time bootstrap, finish those sessions or explicitly
terminate only disposable sessions with the installed CLI, wait for its daemon
to exit, and then install the new release normally. This bootstrap does **not**
preserve legacy PTYs or manufacture conversation recovery.

Debian upgrades never send signals to daemons. The package records its protocol
and retains root-private executable-digest evidence under
`/var/lib/cagents-package-protocols/`, allowing an older compatible executable to
remain running across repeated upgrades. A running incompatible or unknown
executable makes `preinst` fail visibly; the administrator must arrange an
explicit user-owned shutdown before retrying. Removal similarly refuses while
that package owns a running daemon, even if empty. Package scripts do not connect
to user IPC as root or inspect terminal contents. An already-installed legacy
package's outgoing maintainer scripts cannot be changed by the incoming package;
stop its daemon explicitly before the first transition to this policy.

Cargo debug builds and builds with the explicit `development` feature use a separate daemon, session store, application lease, and runtime namespace. They do not register a tray item or autostart entry, so source-tree development can run beside the installed production application. Release builds without that feature are production builds.

cagents follows the XDG base-directory convention. Production paths are:

- Settings: `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
- Session database: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/sessions.sqlite3`
- Named-window databases: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/windows/<name>/sessions.sqlite3`
- Full-upgrade checkpoint: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/daemon-upgrade/`
- Terminal transcripts: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/transcripts/`
- Cache, when present: `${XDG_CACHE_HOME:-$HOME/.cache}/cagents/`
- Daemon socket and lock: `$XDG_RUNTIME_DIR/cagents/` (shared by full and browserless builds; protocol changes do not create a second owner)

Development substitutes `cagents-development` for the final `cagents` directory component.

Program files installed by `install.sh` live separately under `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`.

### Settings

`settings.toml` uses the version 3 schema below. The original runtime and layout
keys remain required; newer preferences are optional when reading older files
and use the documented defaults. Unknown keys, unsupported schema versions,
malformed TOML, conflicting shortcuts, and values outside the documented ranges
are rejected.

```toml
schema_version = 3
max_hot_cards = 6
hibernation_delay_ms = 30000
focus_follows_mouse = true
app_theme = "system"
ui_scale = 1.0
terminal_font_size = 13.0
terminal_font_family = ""
terminal_line_height = 1.286
terminal_padding = 8.0
terminal_cursor_style = "beam"
terminal_theme = "dark"
sidebar_width = 260.0
inspector_width = 380.0
default_column_fraction = 0.5
terminal_scrollback_lines = 10000
link_open_destination = "integrated_browser"
link_open_gesture = "control_click"
managed_worktree_directory = "worktrees/"
editor_auto_save = true
editor_wrap_lines = false

[keyboard_shortcuts]
new_terminal = ["Ctrl+T"]
new_channel = ["Alt+G"]
clone_pane = ["Alt+E"]
close_focused_column = ["Ctrl+W"]
search_projects = ["Ctrl+F"]
previous_column = ["Alt+Q", "Alt+Left"]
next_column = ["Alt+W", "Alt+Right"]
cycle_pane_width = ["Alt+R"]
```

| Key | Type | Default | Accepted values |
| --- | --- | --- | --- |
| `schema_version` | integer | `3` | exactly `3` |
| `max_hot_cards` | non-negative integer | `6` | `1` through `9223372036854775807` |
| `hibernation_delay_ms` | non-negative integer | `30000` | `0` through `9223372036854775807` milliseconds |
| `focus_follows_mouse` | boolean | `true` | activate the pane under the pointer without scrolling |
| `app_theme` | string | `"system"` | `"system"`, `"dark"`, or `"light"` |
| `ui_scale` | float | `1.0` | finite values from `0.8` through `1.5`, inclusive; multiplies display scaling |
| `terminal_font_size` | float | `13.0` | finite values from `8.0` through `40.0`, inclusive |
| `terminal_font_family` | string | `""` | empty for the system monospace font, or a non-whitespace family name up to 128 bytes. The family must be fixed-pitch; one whose glyph widths differ falls back to the system monospace font, because the grid is a single pitch |
| `terminal_line_height` | float | `1.286` | finite values from `1.0` through `2.0`, inclusive; relative to font size |
| `terminal_padding` | float | `8.0` | finite values from `0.0` through `24.0`, inclusive; logical pixels |
| `terminal_cursor_style` | string | `"beam"` | `"application"` respects the terminal program; `"block"`, `"beam"`, or `"underline"` overrides it |
| `terminal_theme` | string | `"dark"` | `"dark"`, `"light"`, or `"solarized_dark"`; independent of `app_theme` |
| `terminal_scrollback_lines` | non-negative integer | `10000` | `0` through `1000000` lines for newly started terminals |
| `sidebar_width` | float | `260.0` | finite values from `220.0` through `420.0`, inclusive |
| `inspector_width` | float | `380.0` | finite values from `320.0` through `480.0`, inclusive |
| `default_column_fraction` | float | `0.5` | finite values from `0.25` through `1.0`, inclusive |
| `link_open_destination` | string | `"integrated_browser"` | `"integrated_browser"` or `"system_default"` |
| `link_open_gesture` | string | `"control_click"` | `"control_click"` or `"control_shift_click"` |
| `managed_worktree_directory` | string | `"worktrees/"` | nonempty path without control characters; absolute or relative to the repository root |
| `editor_auto_save` | boolean | `true` | `true` or `false`; automatically save edited files after a pause, stopping on conflicts or save errors |
| `editor_wrap_lines` | boolean | `false` | wrap long native-editor lines instead of keeping a horizontally navigable line |
| `keyboard_shortcuts.*` | array of strings | shown above | one or more chords, each containing Ctrl, Alt, or Super and one A-Z, 0-9, Left, or Right key; Shift is optional. Every chord in the table must be distinct, and a bound chord is reserved rather than forwarded to the process inside a card. `cycle_pane_width`, `new_channel`, and `clone_pane` may be empty to leave them unbound |

Version 1 and 2 TOML files are upgraded while loading. Sidebar widths accepted
by version 1 are clamped to the current visible range; other values are
preserved. Version 2 files receive the default inspector width.
Older shortcut tables receive `cycle_pane_width = ["Alt+R"]` unless another
action already owns Alt+R. In that case the existing binding is preserved and
pane-width cycling remains unbound (`[]`) until you assign it a free chord.
Likewise, older shortcut tables receive `new_channel = ["Alt+G"]` and
`clone_pane = ["Alt+E"]` for missing actions unless another action already owns
the corresponding chord; then only that new action remains unbound until
configured. A table that still spells the action `new_thread`, the name it had
before channels were named, is read under the new key without losing its
chords. Explicit bindings, including an existing `Ctrl+Shift+T` new-channel
binding or an empty list, are preserved rather than replaced by the new defaults.
An explicitly configured conflicting binding is still rejected.

Settings text fields save valid edits after a short pause; closing the dialog
also applies valid pending edits. Invalid fields and disk-write failures stay
visible as errors. Existing explicit preferences, including an
`"application"` cursor, are preserved rather than replaced with newer defaults.

On first use after upgrading, cagents migrates a sibling `settings.json` only
when `settings.toml` does not exist. It validates the JSON, writes and syncs the
TOML replacement, then makes a best-effort attempt to remove the JSON file. An
existing TOML file always takes precedence; malformed TOML is reported instead
of falling back to JSON. The configuration directory and settings file are
restricted to modes `0700` and `0600`, and symbolic-link destinations are
rejected.

New terminal cards, and `cagents terminal spawn` when no command is supplied,
select the first valid executable shell from the effective user's passwd entry,
`SHELL`, then `/bin/sh`. This resolution applies only when creating a new
default terminal. Explicit CLI commands and launch recipes restored from saved
sessions are used unchanged.

## Uninstall or purge

For an `install.sh` installation, run the installed helper directly. Normal uninstall preserves settings, sessions, transcripts, and caches:

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall"
```

To remove both program files and all cagents user data:

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall" --purge
```

The downloaded installer offers the equivalent `--uninstall` and `--purge` options, and both scripts refuse root execution. If a custom `--prefix` was used during installation, invoke that prefix's `current/bin/cagents-uninstall` with the same `--prefix`.

Both user uninstall entrypoints use atomic empty-only shutdown. If live terminals
or spawn reservations remain, removal fails without terminating them or removing
program files. Finish them or run `cagents daemon stop --confirm` first.

Remove a Debian installation with:

```sh
sudo apt remove cagents             # full package
sudo apt remove cagents-browserless # browserless package
```

APT removal and purge do not delete files in the user's XDG directories.

Repeatable upgrade-policy checks:

```sh
cargo test --locked --no-default-features --test installer_lifecycle_contract
scripts/smoke-package-upgrades
```

The second command requires Bubblewrap and `dpkg`, and uses an unprivileged,
networkless root filesystem with fixture executable processes, including an
unlinked old executable. It installs/upgrades/removes fixture Debian packages
using the generated maintainer-script contract, checks mixed-generation
interruption, and preserves unrelated files and fixture user data. It does not
signal the host daemon or install a package on the host.

## Releases

Release builds and publication run **locally**, never in GitHub Actions. Use
`scripts/build-release-linux` to build an exact Git tag in a private, rootless
Ubuntu 22.04 environment with Rust 1.93, Zig 0.15.2, and Git 2.55. This keeps the Linux
glibc 2.35 baseline instead of inheriting a newer workstation's libc.
The builder runs locked release-profile tests for isolated CLI, host, browserless
UI, integrated UI, and editor boundaries, then packages and checks the shipped
binaries and desktop metadata. It produces these checksummed artifacts:

- `cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `cagents_${VERSION}_amd64.deb`
- `cagents-${VERSION}-sizes.txt`
- `cagents-${VERSION}-artifact-comparison.txt`
- `SHA256SUMS`
- `install.sh`
- `cagents-browserless-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `cagents-browserless_${VERSION}_amd64.deb`
- `cagents-browserless-${VERSION}-sizes.txt`
- `SHA256SUMS-browserless`
- `ceditor-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `SHA256SUMS-ceditor`
- `release-metadata.json`

Artifact selection is explicitly platform-aware. Only Linux x86_64 GNU has a
supported build/package lane today. Windows and macOS require their own native
ports, validation, packaging, and signing before their targets can be enabled;
the release tooling does not publish placeholder binaries for them.

The package script keeps the existing full package as its default and writes browserless build outputs to a separate Cargo target directory. Run both variants to compare their generated measurement reports without either artifact overwriting the other:

```sh
./scripts/package-linux.sh --variant full
./scripts/package-linux.sh --variant browserless
cat dist/cagents-"${VERSION}"-sizes.txt \
  dist/cagents-browserless-"${VERSION}"-sizes.txt
```

For a release, install `bubblewrap`, `curl`, `git`, and the standard archive tools
locally, then build and publish explicitly:

```sh
./scripts/build-release-linux --ref v0.1.2 --output /tmp/cagents-v0.1.2
./scripts/publish-release --version 0.1.2 --dist /tmp/cagents-v0.1.2 --verify-only
gh auth login --hostname github.com
./scripts/publish-release --version 0.1.2 --dist /tmp/cagents-v0.1.2
```

Both manifests and the existing tag must agree on the release version. The
publisher verifies local and remote tag identity, uploads a private snapshot to
its own draft, downloads and verifies the served bytes, then publishes. It never
moves tags or overwrites a published release. Authentication stays in the local
`gh` configuration, outside the build environment. Pushing a tag does not start
a hosted build.

### In-app updates

Full self-managed installations check authenticated GitHub releases at startup
and every six hours. When a newer stable version is available, an update icon
appears in the top-right application controls; its tooltip names the version.
Clicking it starts an explicit update, not an automatic background installation.
Source, browserless, and OS-package installations retain their original update
method instead of being replaced by the application.

The detached updater rechecks the selected release, verifies and seals downloaded
bytes, and asks every open window to save. Dirty editors, unsent native-chat
input or attachments, pending chat transitions, active repository mutations,
and save failures veto the update. Prepared windows reject new user mutations
but still process outstanding completions and recheck blockers before closing.
After a final durable flush,
the updater closes windows, runs the installer's full-upgrade handoff, and reopens
the same named windows from the current installed executable. Failure status and
the private installer log remain under the installation's `.update/` directory.
After resolving a veto or failure, the warning offers **Retry update** while
the selected release remains available; diagnostic files are retained.

## License

cagents is licensed under the MIT License. See `LICENSE`.
