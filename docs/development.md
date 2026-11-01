# Development

- [Prerequisites](#prerequisites)
- [Feature boundaries](#feature-boundaries)
- [Development runner](#development-runner)
- [Build and test resource usage](#build-and-test-resource-usage)
- [Testing](#testing)
- [Installer and package checks](#installer-and-package-checks)
- [Troubleshooting](#troubleshooting)
- [Standalone editor](#standalone-editor)

The README's [Run from source](../README.md#run-from-source) section is the
quick start; this page is the full reference. Repository tooling is shell
(POSIX `sh`, plus Bash for the release and benchmark scripts) and Rust. Python 3
is also required by the ceditor development/installation scripts, experimental
hot runner, and the full Servo dependency graph.

## Prerequisites

Development builds require Rust 1.93, Zig 0.15.2, and the native dependencies
used by the release build. On Ubuntu 22.04:

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

Install Zig 0.15.2 from [ziglang.org](https://ziglang.org/download/) (or with
`mise install`, which reads `mise.toml`) and make sure `zig` is on `PATH`. The
terminal host uses Ghostty's native library, so daemon builds require Zig.

Ordinary debug builds automatically use the isolated development instance;
optimized builds are production unless the Cargo `development` feature is
enabled. See [Daemon and upgrades](daemon.md#development-instance).

## Feature boundaries

The CLI, UI client, and terminal host have separate Cargo feature boundaries.
Start with the boundary that owns your change:

```sh
# Control CLI: no UI, Ghostty, SQLite, or Servo graph.
cargo check --locked --no-default-features --bin cagents

# Terminal daemon: Ghostty/PTY host, no UI or Servo graph.
cargo check --locked --no-default-features --features host --bin cagentsd

# Browserless UI client: libcosmic, no Ghostty or Servo graph.
cargo check --locked --no-default-features --features ui --bin cagents-ui

# Integrated UI client: libcosmic and Servo, no Ghostty graph.
cargo check --locked --features integrated-browser --bin cagents-ui

# Standalone editor: separate package using the browserless UI graph.
cargo check --locked --package ceditor --bin ceditor
```

Changes to ceditor or shared UI, editor, or notification public contracts also
require the standalone editor check above. The workspace's
`default-members = ["."]` selects only cagents: root UI checks and root
`--all-targets` tests do not compile the downstream ceditor binary. Run the
ceditor check separately from the CLI, host, and UI checks.

The default `full` feature selects production identity only; `ui`, `host`, and
`integrated-browser` select implementation graphs. Cargo features are
package-wide, so combining `ui` and `host` in one `--bins` invocation would undo
that isolation. Build a matched browserless release as three feature-disjoint
targets:

```sh
cargo build --locked --release --no-default-features --bin cagents
cargo build --locked --release --no-default-features --features host --bin cagentsd
cargo build --locked --release --no-default-features --features ui --bin cagents-ui
```

`cagents`, `cagents-ui`, and `cagentsd` are a matched set and must stay
together; running one binary from an unrelated directory is not a supported
layout.

Companion launch paths use fixed product names with the target executable suffix
and retain the selected executable's directory. A missing companion does not
select another installation from PATH or the current directory. Notification
activation retains its separate validated absolute-path lookup. This naming
rule does not qualify Windows/macOS builds or add app-bundle/libexec layouts.

## Linux display backends

The ADE shell and standalone editor explicitly enable both libcosmic's Linux
X11 and Wayland backends. The active session determines the native parent
window; an X11-only session does not require a running Wayland compositor.
For an isolated X11 launch, provide that session's `DISPLAY` and `XAUTHORITY`
and unset `WAYLAND_DISPLAY` and `WAYLAND_SOCKET` in the child environment.
Continue to use the development runner so the CLI and daemon match the UI.

Embedded browser surfaces currently require a Wayland parent. Browserless
builds preserve saved browser descriptors and offer **Open in system browser**.
Enabling the X11 shell does not add an X11 browser renderer. Integrated X11
capability handling is tracked in [#9](https://github.com/CarsonBurke/cagents/issues/9).

The root system-backed Wayland protocol dependencies and explicit libcosmic
and portal Wayland feature forwarding are Linux target dependencies. This
separation does not make the Linux dependency graph Wayland-free: the Linux UI
supports both backends, and pinned iced/winit dependencies also select their own
platform features. It does not qualify Windows or macOS application builds;
process ownership, IPC and integrated browser portability remain separate work.

## Development runner

Launch the native UI through `scripts/run-development`, not a direct
`cargo run`:

```sh
scripts/run-development --browserless   # no Servo; saved browser URLs open externally
scripts/run-development                 # with the embedded browser
```

| Option | Effect |
| --- | --- |
| `--browserless` | Build without Servo and open saved browser URLs in the system browser |
| `--hot` | Experimental browserless Linux body-only UI hot reload; requires the exact recorded cargo-hot installation and bubblewrap |
| `--hot-integrated` | Explicitly select embedded-browser hot reload instead |
| `--debug` | Opt into local raw stderr tracing/panic output and Rust backtraces (`CAGENTS_LOCAL_DEBUG=1`, `cagents=debug,cagentsd=debug,warn`); an explicit `RUST_LOG` or `RUST_BACKTRACE` takes precedence |
| `--fresh-daemon` | Stop the existing development daemon, its sessions and background subagents after all three builds succeed, before launching |
| `--log-file PATH` | Write UI tracing and panic output to `PATH` (relative to the calling directory); build diagnostics stay in the terminal |
| `-- ARGS...` | Pass remaining arguments to the UI |

```sh
scripts/run-development --browserless --debug
scripts/run-development --debug --fresh-daemon
scripts/run-development --browserless --debug --log-file /tmp/cagents-ui.log
```

Use `--fresh-daemon` after changing daemon, terminal-host, IPC/protocol, or
process-lifecycle code, so the UI cannot reconnect to an older development
daemon. It compiles a feature-matched control CLI, pins and validates the
isolated daemon through a Linux pidfd, and signals it without depending on the
old wire protocol. Selecting `--fresh-daemon` explicitly permits stopping every hosted development
terminal/chat job and its background subagents. Background work will not resume
automatically. The runner prints this impact before signaling the daemon.

How the runner works:

- It builds the three sibling executables with separate feature sets and runs
  the selected UI through Cargo, so configured target directories and target
  triples are honored. UI-only compilation never builds Ghostty; daemon-only
  compilation never builds libcosmic or Servo.
- The normal runner enables `integrated-browser` only for the UI; the matched
  CLI and daemon keep the same full-product identity without resolving Servo.
- It uses the `dev-runtime` profile, which keeps UI dependencies optimized for
  usable performance while the ordinary `dev` profile stays cheap for checks.
- The development instance has its own application ID, configuration, session
  database, transcripts, application lease, and
  `$XDG_RUNTIME_DIR/cagents-development/` daemon, so it runs beside an installed
  production instance.
- Browserless and integrated development sessions share that state; do not run
  them at the same time.

The browserless UI keeps saved browser descriptors and opens their URLs through
the system browser instead of compiling Servo.

### Required build isolation (opt-in)

The source runner supports an explicit Linux-only mode for isolating its owned
Cargo children. Supply an **already qualified, stable** `cagentsd` helper built
with the required build-scope entry and the SHA256 of that exact executable:

```sh
scripts/run-development --browserless \
  --build-scope-helper /absolute/stable/cagentsd \
  --build-scope-sha256 EXPECTED_64_LOWERCASE_HEX_DIGITS

# The same paired options precede --review-alerts / -- on the editor runner.
scripts/run-ceditor-development \
  --build-scope-helper /absolute/stable/cagentsd \
  --build-scope-sha256 EXPECTED_64_LOWERCASE_HEX_DIGITS -- /path/to/project
```

Both options are required together. The runner never discovers an installed
helper, builds one to bootstrap itself, or falls back to unisolated Cargo when
this mode was requested. Hot reload is unsupported in this mode and refuses.
The artifact selector verifies a regular executable helper and its SHA256
before **each** build; malformed input, a changed helper, failed manager proof,
or failed enrollment stops the workflow. Hash checking is a path-based
preflight, **not atomic every-exec attestation**. The caller must exclude all
other writers or replacements of the helper and its path ancestors throughout
the workflow, and retain its independent source/tool qualification. Do not use
a helper inside an output location that these builds can overwrite.

The internal helper accepts only the builtin `build` and `check` commands, plus
builtin `test` when an exact standalone `--no-run` occurs before the first Cargo
`--` separator. Bare `test`, a `--no-run` only after that separator,
`--no-run=value` forms, aliases and other commands are refused before enrollment
or execution. Arguments are forwarded unchanged. `test --no-run` can still run
source build scripts while compiling test executables; it does not execute test
binaries and does not provide a general test-run mode. Existing runner scripts
continue to request only `build`.
A manager peer invisible in the caller's PID namespace or owned by another user
is a genuine refusal; compiling the helper does not prove enrollment.

Only the original Cargo child enrolls, before same-PID exec. The outer runner,
artifact reader, evidence owner and later UI/CLI launches remain outside that
build scope. Arguments, working directory, inherited environment, streams,
Cargo targets/jobs and `dev-runtime` profile retain the maintained workflow's
meaning. The three CLI, host and UI builds stay separate and use Cargo JSON to
select their exact executable paths. All three must succeed in one selected
directory before `--fresh-daemon` stops the old daemon; the selected CLI and UI
then run directly, without a later `cargo run` that could rebuild outside the
scope. The ceditor selector retains its exact selected-path output and original
build status behavior; the editor runner forwards these options to each of its
existing build lanes and launches from the original calling directory.

This opt-in source mode does not qualify a helper ELF, its first-build bootstrap,
the manager/kernel path, or native behavior merely by existing. Those require
separate admitted validation. The fake-command regression source in
`tests/required-build-scope.py` checks workflow routing, not real scope membership.
No memory cap or immunity from an ancestor's OOM policy is implied, and no
daemon restart/recovery, provider cleanup or signal authority is added. Existing
ordinary mode and optional provider-scope behavior remain separate.

### Experimental hot reload

The opt-in lane uses the same application modules and startup entry in a separate
`cagents-ui-hot` binary. This matters because cargo-hot patches its selected
binary only: the normal UI's views live in the separate `cagents` library. Its
source watcher covers `src/`, including those actual views, without maintaining
another module list: both roots include `src/modules.rs`. Normal and release
defaults do not enable `hot-reload`.

Thread-local storage for panic recovery, the inspector Markdown cache and browser
engine ownership stays in the ordinary library's `src/runtime_state.rs`. Concrete,
non-inlined accessors keep executable TLS relocations out of shared hot patches,
while retaining one storage identity across patches. Application types stay in
their selected root; the cache crosses only dependency-owned Markdown item types.
This uses the stable compiler without TLS model flags or bootstrap overrides.

Install the pinned external tool explicitly if you choose this lane; the runner
never installs tools. Bubblewrap and Python3 must already be available on Linux.
The runner checks Cargo's recorded installation source/version before building
or stopping the development daemon. Cargo-hot does not provide a version flag.

```sh
cargo install --locked --git https://github.com/hecrj/cargo-hot.git \
  --rev 0d57d0664bf2e1c16ba30c34bcfe78cd3eed665f cargo-hot
scripts/run-development --hot
# Explicitly opt into the more expensive embedded-browser hot build:
scripts/run-development --hot-integrated
```

The helper builds the native Linux CLI and daemon separately using locked
`dev-runtime` builds, resolves and fetches the selected UI dependency graph with
locked Cargo metadata, then stages the matched CLI/daemon beside cargo-hot's
private executable bundle. Cargo-hot builds the UI directly; an ordinary UI
preflight build would produce an unused executable. Cargo-hot always targets
the native host triple, adds a linker and saved compilation objects, and may need
a substantial first build even with the ordinary runner's warmed cache.
`CARGO_BUILD_JOBS` controls its spawned build jobs. Only one hot session can run
at a time: the upstream client/server uses localhost port 1100, and all development
sessions share state.

Every helper Cargo invocation runs with `Cargo.lock` mounted read-only in a
private bubblewrap mount namespace. This also refuses atomic replacement/unlink
of that file. The ordinary user's filesystem, device, desktop and network access
are retained; source edits remain writable. The hot tool itself runs with Cargo
offline because it does not forward `--locked`. A missing/incompatible lock or
uncached dependency fails instead of rewriting the repository lock. Resolve any
intentional dependency change through ordinary locked-development maintenance
and restart; no hash-only lock guard or automatic fallback is used.

Change view/handler **bodies** only while patching. Changes to State/Message
layout, module declarations, runtime-state storage/accessors, dependencies, startup,
features/profiles, daemon or terminal protocol
need a cold restart with the normal runner (and `--fresh-daemon` for host/shared
changes). A watched source change is not proof that it was safely patched.
`--hot --fresh-daemon` intent is consumed by the hot entry only after cargo-hot
has successfully linked and launched the actual replacement executable, after
shared argument parsing and before ordinary UI startup.
It verifies the matched sibling CLI version, stops the development jobs and
waits for the private socket to close. Any replacement build failure leaves the
existing daemon running. Help/invalid arguments/internal helper launches cannot
consume restart intent. Valid named/device/new-window arguments remain supported.

Qualification for this lane includes an actual view-only edit, unchanged UI PID
and retained in-memory draft, plus ordinary browserless/integrated builds and
the real lock mount regression (`python3 tests/hot-development.py`). Source
implementation alone does not establish native hotpatch acceptance.

## Build and test resource usage

Repository-local Cargo configuration (`.cargo/config.toml`) defaults to **two
concurrent compilation jobs**, including `cargo test` and every build in the
runner. Cargo otherwise uses the logical CPU count, which can create
substantial RAM pressure on high-core-count or shared machines. Override it
with standard Cargo controls: `CARGO_BUILD_JOBS` also applies to the runner,
and `-j` takes precedence for a single invocation. Run the isolated feature
lanes sequentially, not as concurrent cold builds.

Worktrees can share Cargo's target and build directories. Cargo makes path
packages relocatable and compares source timestamps, so an older checkout can
otherwise reuse another checkout's code and version stamp. The local Cargo
configuration supplies `CAGENTS_CHECKOUT_ROOT`; our workspace crates track it
in compiler dep-info. The version build script tracks it at compilation and
execution: a runtime watch alone can rerun another checkout's compiled script
body. Switching checkouts rebuilds owned code while keeping the third-party
cache warm. Keep this identity reference in new owned workspace crates. Run Cargo
from this checkout or its descendants so it reads the local configuration.

`python3 scripts/qualify-checkout-cache.py` verifies the boundary using a small,
offline workspace in `/var/tmp`: an unfixed control reproduces stale reuse,
then the actual configuration and build script follow A → B → A with the same
commit, different dirty sources and stamping-script bodies, and old timestamps.
It also verifies that an unchanged dependency remains cached. Pass
`--evidence PATH` to retain the fixture and Cargo logs in a new directory. Run it
sequentially with other Cargo workloads. Its historical-producer control also
demonstrates the supported package bootstrap below and checks that unrelated
dependency bytes and mtimes remain unchanged.

A historical checkout without the compile-time checkout anchor can replace a
shared build-script executable. Returning to an anchored checkout can then
rerun that historical body without recompiling it, even though relative inputs
and the runtime manifest path are correct. The checkout identity does not
retroactively repair that missing compiler dependency.

If a selected package demonstrably lacks that anchor, or a source/body
qualification shows this mismatch, explicitly admit a bootstrap for that
package before trusting its generated output. First record the selected
checkout commit and dirty diff, package name/version, Cargo version, effective
`CARGO_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR` (including Cargo config), exact
isolated command and the mismatching observation. Stop other writers using
those same build directories. From the selected checkout, run a package-scoped
clean, then immediately repeat the **same isolated command** with the **same
Cargo environment, profile, target and build directories**. For example, after
identifying `libsqlite3-sys` as the affected package in a browserless UI check:

```sh
# Explicit maintenance operation: only after evidence identifies this package.
cargo clean --package libsqlite3-sys
CARGO_BUILD_JOBS=2 cargo check --locked --no-default-features --features ui --bin cagents-ui
```

Use `name@version` if the package name is ambiguous. Preserve the clean output,
complete check output and the source/stamp or generated-body observation that
originally detected the mismatch. A successful check alone does not prove that
the expected source body or native behavior was selected. Recheck that same
observation before using the artifacts, and retain the original failed result.
This is an opt-in recovery for an identified package; do not add a global cache
clean, change source mtimes, switch build directories during recovery, or
infer a historical producer from timestamps. Other cached packages remain
available, though Cargo may rebuild dependents of the selected package.

```sh
# Reduce compilation concurrency further on a memory-constrained host.
CARGO_BUILD_JOBS=1 scripts/run-development --browserless

# Opt into more compilation jobs when system RAM is available.
cargo check --locked -j 4 --no-default-features --features ui --bin cagents-ui
```

Compilation jobs and test threads are separate controls: `-j` does not
serialize tests within a test binary. For a resource-heavy test target, append
`-- --test-threads=1`; there is no repository-wide test-thread cap. A test-name
filter limits execution, not compilation of the selected feature graph, so
start with the narrow feature lane and `--lib` or `--test NAME` rather than
`--all-features`.

The jobs setting is not a hard process or memory limit. In the pinned
dependencies, parallel `cc` builds share Cargo's jobserver; CMake uses it for
supported Makefile builds and otherwise may start a separate pool based on
`NUM_JOBS`. SpiderMonkey's Make 4.4 compatibility path also starts a separate
pool sized by `NUM_JOBS`, potentially approaching twice the Cargo job count.
Ghostty's build script invokes `zig build` without forwarding a job limit, so
Cargo's setting does not bound Zig's internal parallelism. A single compiler or
linker can still need substantial RAM. The reduced dependency debug information
and optimized `dev-runtime` dependencies are intentional.

Rust, C/C++, Zig, and linker allocations consume **system RAM**, not GPU VRAM.
Lowering Cargo concurrency does not reclaim memory held by other workloads or
guarantee a build fits when RAM and swap are already under pressure. Browserless
lanes avoid the large Servo/SpiderMonkey graph when embedded-browser behavior is
not being changed.

### Process scope isolation

Optional systemd user scopes receive numeric PIDs in the OS user manager's PID
namespace. Before submitting one, the daemon connects to the fixed
`/run/user/UID/systemd/private` peer and requires its kernel credentials and
exact peer pidfd to match this caller. The pidfd's procfs fdinfo must then name
exactly one PID namespace, ours, read from a procfs whose own view is our
namespace. The manager holds `CAP_WAKE_ALARM`, so ptrace-gated routes such as
`PIDFD_GET_PID_NAMESPACE` or `/proc/PID/ns` are denied to the daemon and cannot
serve as proof. The proof retains the connection and pidfd through the existing
user-bus request. It does not infer the manager's identity from a bus broker, a
nonzero peer PID, a hardcoded namespace inode or a numeric `/proc/PID` lookup.

This relies on the existing trusted OS manager and its well-known user-bus
service routing; it does not protect against a hostile same-user replacement
of those OS services. The request's routing and launch authority are unchanged.
Both namespace reports are read with `openat2` beneath one `/proc` descriptor
verified as procfs, without crossing mounts or magic links. If the peer or its
namespace cannot be proved, no scope request is attempted, ordinary launch is
retained without claiming separate OOM isolation, and the refusal's reason is
returned to the caller. An actual request or membership failure in a proved context
still reports failure. Scope-dependent tests report unqualified isolation when
that same proof is unavailable, and keep their strict membership assertions
when it is available. Kernel-peer regressions use private Unix sockets without
submitting requests to the OS manager.

See the [kernel pidfd fdinfo format](https://github.com/torvalds/linux/blob/master/fs/pidfs.c)
and [systemd's numeric scope PID handling](https://github.com/systemd/systemd/blob/main/src/core/dbus-scope.c).

## Testing

A bare `cargo test` runs only the shared CLI lane: under the default `full`
feature, the `ui` and `host` unit tests are compiled out. Cargo skips integration
targets whose required features are absent, rather than compiling and linking
empty harnesses. The host-only terminal workload example is also skipped outside
its lane. Cover the boundaries a change touches; prepare the HOST prerequisites
below before running that lane:

```sh
cargo test --locked --no-default-features --all-targets
cargo test --locked --no-default-features --features host --all-targets
cargo test --locked --no-default-features --features ui --all-targets
```

HOST fixtures that exec retained actors require
`CAGENTS_TEST_RETAINED_ACTOR_HELPER` to name a `cagentsd` built from the same
checkout and source state as the tests. Build it immediately before testing,
keeping sources, Cargo environment, target, configuration and any explicit
profile overrides fixed. Ordinary build and test profile defaults differ as
expected.
The existing artifact helper reads Cargo's exact executable path, including
configured target directories/triples. A matching version alone does not prove
that two dirty source trees match.

Use a fresh private temporary directory with a short, physical absolute path
outside Git ancestry: neither it nor any ancestor may contain a `.git` file,
directory or symlink. For this Linux example, verify that `XDG_RUNTIME_DIR`
meets those conditions before creating the root. The **final** nested Unix
socket path must be shorter than 108 bytes, including fixture suffixes. For
example, `/.tmpXXXXXX/tmp/cagents-codex-<36-byte UUID>/control.sock` adds 79
bytes, so that layout requires a temporary base of at most 28 bytes. Longer
fixture layouts need more room; a short base alone is not proof.

```sh
(
    TMPDIR=$(mktemp -d "${XDG_RUNTIME_DIR:?}/tXXXXXX") || exit
    export TMPDIR
    printf 'Fixture root (retain until children settle): %s\n' "$TMPDIR"
    CAGENTS_TEST_RETAINED_ACTOR_HELPER=$(
        python3 scripts/build-ceditor-product cagentsd --locked \
            --no-default-features --features host --bin cagentsd
    ) || exit
    export CAGENTS_TEST_RETAINED_ACTOR_HELPER
    cargo test --locked --no-default-features --features host --all-targets
)
```

A helper build failure prevents the test command. Use the same preparation for
narrow HOST targets that need retained actors. Keep the temporary root and any
separately retained fixture evidence until their original children have settled;
this recipe deliberately installs no automatic deletion trap. Do not change
socket limits, remove enclosing Git markers or silently select another runtime
root to make a failed preparation pass.

Prefer the narrow target that covers the changed contract, for example:

```sh
cargo test --locked --no-default-features --features host --lib terminal::
cargo test --locked --no-default-features --features host --test terminal_load_contract
cargo test --locked --no-default-features --features ui --test session_restore_contract
```

The four named-tree reconciliation contracts share their cases between the root
UI target and a focused workspace package. For edits confined to vendored
iced-core tree reconciliation, run the focused package first:

```sh
cargo test --locked -p cagents-iced-contracts
```

The package uses the UI's exact iced-core Git revision; the root Cargo patch
resolves both targets to `vendor/iced-core`, with the production `a11y` Id
implementation. It compiles without the application, libcosmic renderer, or
native UI runtime. The cases do not render; the isolated harness uses an
uninhabited renderer. Retain validation through the UI's concrete renderer when
changing the tree contract or UI integration:

```sh
cargo test --locked --no-default-features --features ui --test iced_tree_reconciliation
```

Explicitly selecting a target with required features absent, including through a
`--test '*'` glob, reports the missing feature instead of succeeding with zero
tests. Use `--all-targets` to select all targets available in a feature lane.

The 40,000-path mutation regression lives in `repository_scale_contract`, so a
library-suite run does not also create and mutate that filesystem fixture.
`--all-targets` still runs it in both HOST and UI lanes. When changing repository
mutations or pathspec handling, include its explicit target:

```sh
cargo test --locked --no-default-features --features host --test repository_scale_contract
```

The fixture retains its disk-backed private namespace, subprocess deadline and
process-family cleanup checks. Its test count and workload are unchanged.

Run `scripts/check-format` after Rust edits. It runs `cargo fmt --check` for
Cargo roots and directly checks `src/modules.rs` and nested Rust-source
`include!` fragments, which rustfmt does not discover through macro expansion.
Both stages report their diagnostics and any failure makes the command fail;
the check never rewrites source or formats vendored packages. When adding a
Rust-source `include!`, update the explicit list in the script. Its lightweight
regression is `python3 tests/check_format.py` (mocked command routing and bounded
real-rustfmt fixtures, with no Cargo/application build). Set
`CHECK_FORMAT_CARGO_FIXTURE=1` for the additional dependency-free temporary Cargo
fixture; this still runs only `cargo fmt`, with no application build.

For UI changes, launch the real UI with
the runner and exercise the changed path. For browser or feature-boundary
changes, verify both the browserless and integrated lanes. Performance
workloads are described in [Performance](performance.md).

## Installer and package checks

Repeatable upgrade-policy checks:

```sh
cargo test --locked --no-default-features --test installer_lifecycle_contract
scripts/smoke-package-upgrades
```

The second command requires Bubblewrap and `dpkg`. It uses an unprivileged,
networkless root filesystem with fixture executable processes, including an
unlinked old executable. It installs, upgrades, and removes fixture Debian
packages using the generated maintainer-script contract, checks
mixed-generation interruption, and preserves unrelated files and fixture user
data. It does not signal the host daemon or install anything on the host.

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| `required command not found: zig` | Install Zig 0.15.2 and put it on `PATH` (`mise install` reads `mise.toml`). |
| The UI talks to an old daemon after you changed daemon, host, or IPC code | Relaunch with `--fresh-daemon`. |
| `development daemon did not stop within 5 seconds` | Look for a lingering development daemon with `pgrep -a cagentsd`, let it exit (or stop that process), then retry `--fresh-daemon`. The socket is `$XDG_RUNTIME_DIR/cagents-development/cagentsd.sock`. |
| Compilation exhausts RAM or swap | Lower `CARGO_BUILD_JOBS`, use `--browserless`, and avoid concurrent cold builds; see [resource usage](#build-and-test-resource-usage). |
| A UI problem needs logs | Add `--debug --log-file /tmp/cagents-ui.log` and read the file after reproducing. |
| New links do not open in browserless mode | Install `xdg-utils`, or use a desktop with a working portal. |

## Standalone editor

```sh
scripts/run-ceditor-development /path/to/project
scripts/run-ceditor-development /path/to/file.rs
scripts/run-ceditor-development --review-alerts /path/to/project
scripts/install-ceditor                  # default prefix ~/.local
scripts/install-ceditor --prefix /absolute/path
```

Preview supports UTF-8 text, including extensionless files. Plain text preserves
its literal content; extensionless `README` uses Markdown. Mermaid fences in
Markdown and standalone `.mmd` / `.mermaid` files render natively, without a
browser or Node.js. The native renderer supports flowcharts, sequence, class,
state, ER, pie and other Mermaid diagram types; it may differ from Mermaid.js
in styling or syntax coverage. Unsupported or oversized diagrams keep their
source visible alongside an error. Rendering is debounced, uses at most two
workers, and retains at most 24 diagrams / 32 MiB of pixels per document.

The development runner uses an isolated application and settings identity.
The editor runner and installer require Python 3 to read executable paths from
Cargo's build output. This honors configured targets and output directories,
including `build.target` in Cargo configuration and `CARGO_BUILD_TARGET`.

Desktop alerts are sent through the notification portal under the daemon's
application ID. When the portal accepts that ID, the daemon owns it on the session
bus and receives clicks as `org.freedesktop.Application.ActivateAction` calls
carrying the notification server's activation token, which the compositor
validates before granting foreground activation. The portal only accepts IDs with an installed desktop entry; without
`dev.cagents.Cagents.Development.desktop`, development alerts fall back to the
portal's `ActionInvoked` signal, which still selects the pane but may carry no
token, and Wayland compositors such as niri then leave the window unraised.

Successful registration and action routing do not guarantee foreground activation.
Native validation with KWin 6.4.4 and COSMIC Notifications alpha 7 observed the
correct conversation and acknowledgment while the notification service returned
a refused activation token and ordinary X11 activation requests left the window
behind the active window. The existing-window X11 path does not yet propagate an
X11 startup-notification ID; granted foreground activation on this combination
remains unverified. Focus remains subject to the compositor's policy.

Standalone Ceditor review alerts use the shared daemon notification broker. The
`--review-alerts` runner builds the CLI, daemon, and editor in separate feature
lanes; the daemon starts lazily when GitHub review requests are scanned. Ordinary
editing does not require a running daemon. Release archives include these three
matched executables in a private `lib/ceditor` tree. The source installer switches
that private tree atomically and exposes only `bin/ceditor`, leaving a separate
Cagents installation untouched. Existing private builds remain available to
running windows and daemons; update Ceditor with its original installer, not the
Cagents self-updater. Opening an alert for the other application requires that
application to be installed alongside the daemon or in an absolute PATH directory.
