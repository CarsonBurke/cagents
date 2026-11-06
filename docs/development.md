# Development

- [Prerequisites](#prerequisites)
- [Feature boundaries](#feature-boundaries)
- [Development runner](#development-runner)
- [Build and test resource usage](#build-and-test-resource-usage)
- [Testing](#testing)
- [Native evidence](#native-evidence)
- [Installer and package checks](#installer-and-package-checks)
- [Troubleshooting](#troubleshooting)
- [Standalone editor](#standalone-editor)

The README's [Run from source](../README.md#run-from-source) section is the
quick start; this page is the full reference. Repository tooling is shell
(POSIX `sh`, plus Bash for the release and benchmark scripts) and Rust. Python 3
is also required by the Cargo gate, ceditor development/installation scripts,
experimental hot runner, and the full Servo dependency graph.

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
scripts/cargo-gate check --locked --no-default-features --bin cagents

# Terminal daemon: Ghostty/PTY host, no UI or Servo graph.
scripts/cargo-gate check --locked --no-default-features --features host --bin cagentsd

# Browserless UI client: libcosmic, no Ghostty or Servo graph.
scripts/cargo-gate check --locked --no-default-features --features ui --bin cagents-ui

# Integrated UI client: libcosmic and Servo, no Ghostty graph.
scripts/cargo-gate check --locked --features integrated-browser --bin cagents-ui

# Standalone editor: separate package using the browserless UI graph.
scripts/cargo-gate check --locked --package ceditor --bin ceditor
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
scripts/cargo-gate build --locked --release --no-default-features --bin cagents
scripts/cargo-gate build --locked --release --no-default-features --features host --bin cagentsd
scripts/cargo-gate build --locked --release --no-default-features --features ui --bin cagents-ui
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
| `--instance NAME` | Run an isolated instance in a headless niri-harness session; see [Parallel instances](#parallel-instances) |
| `--stop-instance NAME` | Stop that instance's daemon, its jobs and its harness session; keep its storage |
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

- It builds the three sibling executables with separate feature sets in one
  slot of the [build gate](#sharing-a-host-between-several-checkouts), then
  runs them from the target directory Cargo reports, so configured target
  directories are honored.
  UI-only compilation never builds Ghostty; daemon-only compilation never
  builds libcosmic or Servo.
- A launch with nothing to build skips the gate and starts at once, printing
  `The matched set is already built for this checkout; skipping the build.`
  See [Up-to-date launches](#up-to-date-launches).
- The normal runner enables `integrated-browser` only for the UI. Both lanes
  build the same CLI and daemon, without `full`: that feature selects the
  shipped product's application ID, peer package and self-update, none of which
  a development build uses, so switching lanes rebuilds only the UI.
- It uses the `dev-runtime` profile, which keeps UI dependencies optimized for
  usable performance while the ordinary `dev` profile stays cheap for checks.
- The development instance has its own application ID, configuration, session
  database, transcripts, application lease, and
  `$XDG_RUNTIME_DIR/cagents-development/` daemon, so it runs beside an installed
  production instance.
- Browserless and integrated development sessions share that state; do not run
  them at the same time. Use [parallel instances](#parallel-instances) instead.

### Up-to-date launches

Waiting for a build slot only to learn that nothing needs building can take
minutes on a busy host, so the runner decides first whether the matched set is
already current. After each successful build it writes a record,
`.run-development-ui` or `.run-development-integrated-browser`, beside the
executables in `target/dev-runtime/`. The next launch skips the gate only if
all of these still hold:

- The lane, `rustc -vV`, the checked-out commit and the Zig toolchain match the
  record.
- The build environment matches: `RUST*`, `CARGO_*` (except `CARGO_BUILD_JOBS`
  and `CARGO_HOME`), the C toolchain and `pkg-config` variables, and every
  variable a build script asked Cargo to watch.
- All three executables are the files that build produced.
- No file changed after the build started: anything in the checkout outside
  `target/`, `.git/`, `website/`, `dist/` and nested worktrees; any file in
  the executables' Cargo dependency lists; any absolute path a build script
  watches, such as the shared repository's Git index; and any Cargo
  configuration file Cargo would load for the checkout.

Otherwise the launch builds through the gate as before, and Cargo makes the
real decision. The check never runs Cargo's build, so nothing compiles outside
a slot. Launching the other lane rebuilds the UI and invalidates this lane's
record. Delete the record to force the next launch through the gate.

### Parallel instances

Several people or agents can each validate a change in the native app at once
by giving every run its own instance:

```sh
scripts/run-development --browserless --instance pr2064
niri-harness --session cagents-pr2064 observe
scripts/run-development --instance pr2064 --fresh-daemon   # rebuild and replace it
scripts/run-development --stop-instance pr2064
```

An instance name is 1–24 ASCII letters, digits or hyphens. The runner builds
the matched set as usual, then starts the UI in a headless
`niri-harness` session named
`cagents-NAME` and returns once its window maps. Drive and capture it with
`niri-harness --session cagents-NAME ...`.

| Shared by default | Per instance |
| --- | --- |
| Storage `~/.local/share/cagents-development`, config, state | `cagents-development-NAME` under each XDG root, selected by `CAGENTS_DEVELOPMENT_INSTANCE` |
| Daemon under `$XDG_RUNTIME_DIR/cagents-development/` | The harness session's private runtime directory |
| Application ID and activation on the session bus | The harness session's private D-Bus |

`CAGENTS_DEVELOPMENT_INSTANCE` only affects development builds; production
ignores it. The runner sets it for `--instance` and clears it otherwise, so an
inherited value never selects an instance. The instance's daemon gives it to
every agent and terminal it starts, so a `cagents` they run reaches that
daemon. Provider accounts and tools keep the caller's `HOME`, so an
instance's agents use the same provider sign-ins.

The runner starts the session without the caller's `WAYLAND_DISPLAY`,
`WAYLAND_SOCKET`, `NIRI_SOCKET`, `DISPLAY` and `XAUTHORITY`, and stops an
instance's daemon through `niri-harness exec`, so nothing it runs for an
instance can reach the caller's desktop. Agents should also set
`CAGENTS_DEVELOPMENT_NESTED_ONLY=1`: the runner then refuses to start a UI
without `--instance` unless it runs inside a session through
`niri-harness --session NAME exec -- scripts/run-development ...`, so a failed
nested session cannot put a UI on the user's desktop. It requires a session's
private `nh-*` runtime directory and no `DISPLAY` or `WAYLAND_SOCKET`, which is
what `niri-harness exec` provides.

Stopping a harness session deletes its runtime directory, so stop an instance
with `--stop-instance`, which stops its daemon first. `niri-harness stop` alone
leaves that daemon running without a socket. An instance's storage remains
for its next launch; delete `~/.local/share/cagents-development-NAME` and its
config and state siblings to discard it.

Instances started from one checkout run that checkout's executables, so start
them all in the same lane: all `--browserless` or all integrated. The two lanes
write their UI to the same path, and launching the other lane rebuilds it under
the running instances. Run the other lane from a second checkout.

An idle instance takes about 0.4 GiB: the UI, its daemon, a headless niri
compositor and the session's portals. Memory rarely limits how many run at
once; building them does. Each checkout compiles its own matched set through
`scripts/cargo-gate`. Once a checkout is built, its later launches skip the
gate and start in seconds; see [Up-to-date launches](#up-to-date-launches).

The browserless UI keeps saved browser descriptors and opens their URLs through
the system browser instead of compiling Servo.

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

### Sharing a host between several checkouts

Rust compiles on the [build host](#building-on-another-host); this section
describes how the gate admits builds on the host that compiles, whether that is
the build host itself or this one with `CAGENTS_BUILD_REMOTE=local`. When
several agents build on one host, run Cargo through the build gate rather than
a single global lock:

```sh
scripts/cargo-gate check --locked --no-default-features --features ui --bin cagents-ui
scripts/cargo-gate test --locked --no-default-features --features host --all-targets
```

The gate admits several builds at once, up to one per 4 GiB of RAM after an 8
GiB reserve, at most one per 4 CPUs and at most 4 (`CAGENTS_BUILD_SLOTS`
overrides the count). Builds spend most of their time in a few crates that
compile or link on one CPU, so each busies about 1.4 CPUs on average, and
memory and CPU pressure decide how many run. On a host whose CPUs are already
saturated, by other builds or other work, a further build slows every running
one rather than finishing sooner, so a build starts beside others only while
the kernel's CPU pressure (`some avg10` in `/proc/pressure/cpu`, the share of
the last 10 seconds in which some task waited for a CPU) is below 60%
(`CAGENTS_BUILD_CPU_PRESSURE`; `0` disables this check and the next), and not
within 10 seconds of the newest build starting
(`CAGENTS_BUILD_CPU_SETTLE_SECS`), before that 10 second average can show it. A
host that does not report pressure is treated as having room. Each build has an
expected peak: 3 GiB for anything in the CLI lane, 4 GiB for other checks, 7
GiB for a host build or test build, 9 GiB for a UI one and 8 GiB for an `exec`
script such as the development runner; a slot held by a plain `flock` counts as
9 GiB. `CAGENTS_BUILD_COST_GIB` overrides it. A build starts beside others only
when the memory available, less what the running builds are expected to take
beyond what they already hold, covers its own, and while the kernel's memory
pressure (`some avg10` in `/proc/pressure/memory`, the share of time some task
stalled on reclaim) is below 10% (`CAGENTS_BUILD_MEMORY_PRESSURE`; `0` disables
it). A lone build always starts, whatever the memory or pressure. Each build gets a
share of the CPUs among the builds running once it starts, between 4 and 8 (the
jobs the costs were measured with) and at most half the CPUs, as
`CARGO_BUILD_JOBS`, which also caps a larger value; `-j` still wins. For
`test`, `bench` and `run` only the build holds a slot, and the tests or program
run after it is released, so do not wrap the gate in another lock. Rustdoc
compiles doctests while they run, so `cargo test --doc` keeps its slot
throughout; a plain `cargo test` compiles its doctests after the slot is
released. The development runner builds its matched set through the gate
itself, and skips the gate when the set is [already
current](#up-to-date-launches).

The running builds are those of the whole host, not only of one family of
slots. Builds in the shared families, whose first lock is named
`/var/tmp/cagents-*.lock` (such as `/var/tmp/cagents-cargo.lock` and
`/var/tmp/cagents-fw16-cargo.lock`), and builds in the slots offloaded to the
build host each record themselves in `/var/tmp/cagents-builds-UID`
(`CAGENTS_BUILD_REGISTRY`; empty disables it), naming the lock they hold. Every
family prices the others' running builds beside its own, and a build is lone
only when nothing on the host builds. Without this each family filled the same
memory: on fw16, three families admitted builds independently and the host
swapped about 20 GiB each way every six minutes. A private family, such as a
test's or a native session's own lock elsewhere (including one in a directory
under `/var/tmp` named like a shared lock), stays out of the registry unless
the variable names one. A record whose lock is free is stale and ignored, and
one whose holder has exited is priced at the largest cost. Families decide one
at a time, under the registry's `.decision` lock. A slot held by a plain
`flock`, by a gate from before the registry or by an offload agent sent from
such a checkout writes no record, so only its own family prices it.

So that no build waits forever behind smaller ones from other families that
keep fitting, a build refused room while it holds a free slot records itself
as waiting. Once refused for 120 seconds (`CAGENTS_BUILD_WAIT_PRIORITY_SECS`,
read on the host that admits the build; `0` disables it), the build refused
longest on the host starts beside the running builds whatever the memory or
pressure. Only one such build ramps up at a time: while one admitted that way
started less than 10 minutes ago and its holder lives, the next waits. Time
spent queued behind its own family, or waiting for one of its slots, does not
count, and a build no longer refused for 10 seconds loses its place.

The first slot is the plain lock file `/var/tmp/cagents-cargo.lock`, so a
command still run under `flock --close /var/tmp/cagents-cargo.lock` takes exactly
one slot. Without `--close`, every process the command starts inherits the lock,
so a test's daemon that outlives it blocks everyone until that daemon exits. Killing
a `flock --close` frees the slot even if its command keeps running. A gated command
started under a running `flock` on a slot runs at once in that slot; a slot that any
other process holds is waited for. Set `CAGENTS_BUILD_SLOTS` to override the count.

Admission waits are unbounded by default. To give one invocation a budget,
set `CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS` to a positive integer, for example
`CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS=120 scripts/cargo-gate check --locked --no-default-features --bin cagents`.
The single budget includes metadata, disk reclamation/seeding, queue, slot,
memory and CPU waits. Expiry exits 124 after withdrawing owned waiters and cleaning
up preflight descendants; polling and the existing cleanup grace can delay
exit. Once a slot is admitted, the build and subsequent tests/program have no
admission timeout. Commands already inside a held slot still run directly.

To gate a script that runs several Cargo commands, replace
`flock /var/tmp/cagents-cargo.lock COMMAND` with
`scripts/cargo-gate exec COMMAND`. Where Rust compiles on [another
host](#building-on-another-host), each Cargo command in it goes there on its
own, unless `exec --hold` holds one slot there for all of them. Building here,
it holds one slot until the command exits, with or without `--hold`, and gated
commands inside it run in that slot; gating each Cargo command instead keeps
test runs out of the slot.

Queue and slot lock requests die with their gate, including when it is killed
without a chance to handle a signal. Waiters retain only their lock descriptor
and standard streams; inherited pipes and locks remain with the gate for the
admitted command.

Cancelling a held command keeps its slot until the command and its descendants
have exited. The Linux Python 3 supervisor sends TERM, allows the command one
shared two-second grace period, then kills and reaps remaining descendants,
including children that created a separate session. Successful commands may leave background cache
services running without holding a slot. The regression contract uses private
locks and owned fixture processes only: `python3 tests/cargo_gate.py -v`.

`scripts/lock-holders [LOCK...]` shows who holds each lock under
`/var/tmp/cagents-*.lock*`, whether they or their descendants are using CPU, and who
is waiting. It notes a lock whose holders aren't a recognized build or session, and
one whose holders used no CPU during its one-second sample. Neither makes a holder a
leftover: a script can hold a lock and sleep between its own steps, and only its
owner can say. Nor does a recognized name clear one: a leftover that looks like a
build gets at most the idle note, so read the holders it lists. Process age isn't
hold time: a process can start long before it takes a lock, or after the lock was
taken if it inherited it. The kernel doesn't record when a lock was taken, but a
holder that took the lock itself is marked, and took it at most its age ago. The tool
only reads `/proc`, so ending a process stays your decision.

The feature lanes coexist in one build directory: a no-op check of one lane
stays a no-op after building the others. With a per-checkout build directory,
such as `build-dir = "/var/tmp/cargo-build{workspace-root}/target"` in
`~/.cargo/config.toml`, the cost per worktree is the first build of each lane. On a 24-thread, 60 GiB host
a cold check took 35 s for the CLI, 53 s for the host, 62 s for the UI and
126 s for the integrated UI; the single `cagents` crate is most of each.

### Building on another host

Rust compiles on the build host named by `CAGENTS_BUILD_REMOTE` (default
`fw16`, reached with `ssh`), never here: `scripts/cargo-gate` sends every Cargo
command that compiles there through `scripts/cargo-offload`, without taking a
slot here. Commands that compile nothing, such as `metadata`, `tree` or
`clean`, run here at once. Under `scripts/cargo-gate exec COMMAND`, a `cargo`
first on `PATH` leads every Cargo command in COMMAND back to the gate, so a
script of builds, such as `scripts/build-ceditor-product`, builds there too,
each command with its own admission. `scripts/cargo-gate exec --hold COMMAND`
leases one slot there first, at the `exec` cost, and holds it until COMMAND
exits: each gated Cargo command in it builds in that slot, one at a time, with
no admission of its own. It is for a script that only builds, such as the
development runner's three: a launch waits in the host's queue once rather
than three times. A command that runs tests or anything slow between its builds
would keep the slot leased and its memory reserved while it did, so leave
`--hold` off for those. A Cargo command run from another checkout, or after the
hold's lease has ended, leases a slot of its own. A Cargo reached any other way inside COMMAND, by path,
through `$CARGO`, `rustup run` or `mise exec`, would compile here, so `exec`
also sets `RUSTC_WRAPPER` to `scripts/cargo-gate-rustc-guard`, which refuses to
compile a crate and says why; Cargo's queries of rustc pass, and a wrapper the caller set still runs.
Programs launched by `cargo run` inside that `exec` inherit the same Cargo
shim and guard. Their named `cargo` reenters the gate after the program's
build releases its slot; their absolute `$CARGO` cannot compile here.
On the build host itself, or with
`CAGENTS_BUILD_REMOTE=local`, commands build here through the local slots
above. A gate without `scripts/cargo-offload` beside it refuses to build
unless `CAGENTS_BUILD_REMOTE=local` is set.

The host has build slots under
`~/.cache/cagents-offload/<repository>/slot-N`, each with its own checkout and
target directory. A further slot is made when every slot is leased, up to
`CAGENTS_BUILD_REMOTE_SLOTS` (default 8), and only while the host's free space
holds another 50 GiB of build output above its floor, beyond the 50 GiB each
slot made in the last 12 hours is taken to be filling to; how many build at
once is then down to memory and CPU, below. Unless `CAGENTS_BUILD_REMOTE_JOBS`
is set, each build gets the host's CPUs divided among the builds leased when
it starts, as `CARGO_BUILD_JOBS`: at least 4, and at most 8 or half the CPUs
where that is more than 4, as builds here get. A request from the same worktree
and directory for the same build as one building now, a command that runs
nothing there, waits for that slot rather than build it again in another,
outside the queue meanwhile, and then gets back in line: once it ends, little
is left to compile. A
build copies the
checkout's tracked and untracked, not ignored, files into a free slot and
removes the slot's other files. It prefers the slot this worktree used last,
then one another worktree has built in. A slot's checkout path never changes,
so a worktree reuses what any other built in that slot and recompiles only
what differs. Files are copied when their content differs, and a copied file
gets the time it arrives: Cargo judges the checkout's crates, including the
`[patch]` crates under `vendor/`, by modification time, so keeping this
checkout's older time would leave another worktree's build of that file in
place. A slot last leased by a gate from before this rule rebuilds its sources
once. A slot is admitted as the gate admits builds here: beside other
running builds, only when the host's available memory, less what those builds
are expected to take beyond what they already hold, covers this build's
expected peak (at least 2 GiB), while the host's memory pressure (`some
avg10` in `/proc/pressure/memory`) is below 10%, and while its CPU pressure
(`/proc/pressure/cpu`) is below 40%; a lone build always starts. The running
builds counted include those the host's own gate admits in its shared
families, through the registry described [above](#sharing-a-host-between-several-checkouts),
where these slots record their builds in turn. A plain
`cargo test` runs its tests there, in the slot, which stays leased until they
end. Cargo builds everything a test run needs before it runs any test, so the
compile's expected peak stops applying once it only runs executables it built
into a `deps` directory: from then on the slot is priced at the most its
processes have held since its tests began, sampled every 2 s, and never below
2 GiB. Builds the host admits meanwhile may take the memory the compile no
longer reserves, while whatever the tests hold stays counted. Growth beyond
that peak is not reserved, as the memory of tests the gate runs on this host
is not; memory pressure then holds further builds back. Running a compiler or build
script again, as doctests do, prices it as a build again. Without this, slots
running only tests on fw16 reserved about 18 of its 26 GiB available, and
queued builds waited beside idle slots. One build's
steps share one ssh connection, which ends with it. Output streams back and
the exit status is Cargo's; cancelling the gate ends the command's processes
on the host before the slot is free again. Commands there run without core
dumps, which the host's `systemd-coredump` would otherwise store; set
`CAGENTS_BUILD_REMOTE_CORE_DUMPS=1` to keep them.

What callers here use comes back. `build`, `rustc`, `run`, and `test` or
`bench` with `--no-run` copy back the executables Cargo reported, into this
host's target directory, or its build directory for test harnesses when
the build host also keeps one apart from its target directory. Either way
they arrive at the paths Cargo's messages, and its JSON messages
(`--message-format=json...`), name here. `run` builds there and runs the program here, with Cargo's `CARGO_*`
settings. `doc` copies `target/doc` back, per `--target`, and `--open` opens it
here. `--timings` copies the report back. `fix` and `clippy --fix` apply their
fixes here, to the files they changed there, after the same uncommitted-change
checks Cargo makes. Path options inside the checkout, such as
`--manifest-path`, `--target-dir` and `--artifact-dir`, are translated, and a
`CAGENTS_` setting naming a file in the checkout or its target, such as
`CAGENTS_TEST_RETAINED_ACTOR_HELPER`, names the host's copy of it there; a
path outside the checkout is refused with status 78, as are commands that
cannot run there faithfully, each saying why and naming
`CAGENTS_BUILD_REMOTE=local`. The version stamp is worked out here and passed
as `CAGENTS_BUILD_METADATA`, since the host's copy has no Git history.

While the host is unreachable, does not answer within
`CAGENTS_BUILD_REMOTE_ANSWER_SECONDS` (default 30), or has less than
`CAGENTS_BUILD_REMOTE_FREE_FLOOR_GIB` free (default 40; it first discards the
leased slot's own build output), the command waits and tries again every
`CAGENTS_BUILD_REMOTE_RETRY_SECONDS` (default 15), saying why once and then
once a minute. It never builds here instead. `CAGENTS_BUILD_ADMISSION_TIMEOUT_SECONDS`
bounds that wait, and the wait for a slot there, as it does a wait here; on
expiry the command exits 124 naming the cause. The build there has no
timeout. Set `CAGENTS_BUILD_REMOTE=local` to build here.

A checkout's gate defers to the installed one, `/var/tmp/cagents-cargo-gate`
(`CAGENTS_BUILD_INSTALLED_GATE`; empty disables this), when the installed
`gate_version` is newer, so a worktree that has fallen behind `main` still
builds the way `main` does. Raise `gate_version` in `scripts/cargo-gate` with
any change to the gate or its helpers. `scripts/install-cargo-gate`
installs a checkout's gate and refuses an older version over a newer one.

Merged gate changes install themselves. Each installation is its own
directory under `/var/tmp/cagents-cargo-gate.d`, and the installed names are
links into it, so a running gate keeps its own helpers while a newer one
takes over. `/var/tmp/cagents-cargo-gate.installation` records each file's
Git blob, the origin and commit it came from, and when. When a gate runs in
a checkout whose `origin/main` it has not yet judged,
`scripts/cargo-gate-sync.py` compares main's files with that record. It
installs main's gate when they differ and main either contains the recorded
commit or changed the gate, by its first-parent history, after the
installation; then main's gate runs the command, and the run says so. An
installation from elsewhere, such as a branch under test or changed files,
stays until main changes the gate again; one of a newer `gate_version`
than main's stays until main's is as new. A branch that main squashed or rebased has the
same files, so nothing happens. Checkouts of a fork leave the installation
alone. When main's gate cannot be installed, every run says so, with the
reason in `/var/tmp/cagents-cargo-gate.install-failed`, and it is tried
again only once main moves. Replaced installations stay a week for builds
still running them. The installation waits for a fetch of
main, and it runs main's installer, so fetched main code installs itself.
`CAGENTS_BUILD_INSTALL_SYNC=0` turns this off.

Only settings that change what Cargo, rustc or the tests do reach the host:
`RUST_*`, `RUSTFLAGS`, `RUSTDOCFLAGS`, `CARGO_TERM_*`, `CARGO_PROFILE_*`,
`CARGO_INCREMENTAL` and other `CAGENTS_` settings, except `CAGENTS_BUILD_*` and names that look like
credentials (containing `TOKEN`, `KEY`, `SECRET`, `PASS`, `AUTH`,
`CREDENTIAL` or `COOKIE`). They travel on the connection's standard input,
not its command line. The command may open as many files as the host's
hard limit allows, whatever the soft limit here: an ssh session's usual
1024 fails descriptor-heavy suites. This user's Cargo `[profile]` settings are applied
there; its build settings, such as `jobs` and `build-dir`, describe this host
and are not. Each slot's commands get a private `TMPDIR`,
`$XDG_RUNTIME_DIR/cof/<repository>-<N>` (mode 0700) on the host, short enough
for the Unix sockets tests bind under it and emptied before the next command
there; a HOST test lane leaves about 12 MB. It follows the ancestry, home and
socket constraints in the HOST recipe below rather than selecting `/tmp`,
`/var/tmp`, `~/.cache` or the home directory: agent
sandboxes create `.git` placeholders in the first three, recovery placement
deliberately refuses any enclosing `.git`, and fixtures refuse roots inside
the home directory. The runtime directory exists while the host has a login
session for the user, which the connection itself provides; without one,
temporary files fall back to the slot, and `test` stops with status 78 if a
marker is above it. While a
`.git` sits in any of its ancestors on the host, `test` stops with status 78
and names it, since fixtures that check Git ancestry fail under one and
building here instead would hide the leak. Stop and investigate its ownership;
do not remove a shared marker to make tests pass. Other commands still run there. Cargo's registry and Git caches and the toolchain on the
host are shared with anything else built there; Cargo locks its caches itself.

### Compilation jobs and shared build directories

Repository-local Cargo configuration (`.cargo/config.toml`) defaults to **two
concurrent compilation jobs**, including `cargo test` and every build in the
runner. Cargo otherwise uses the logical CPU count, which can create
substantial RAM pressure on high-core-count or shared machines. Override it
with standard Cargo controls: `CARGO_BUILD_JOBS` also applies to the runner,
and `-j` takes precedence for a single invocation; with several build slots,
`scripts/cargo-gate` sets or caps `CARGO_BUILD_JOBS` to a share of the CPUs.
Run the isolated feature lanes one after another, each through the gate.

Give each worktree its own build directory, which the user-wide
`build-dir = "/var/tmp/cargo-build{workspace-root}/target"` setting already
does; do not point several worktrees at one with `CARGO_BUILD_BUILD_DIR`.
Cargo names a workspace crate's artifacts by its path relative to the
workspace, so worktrees on different commits overwrite each other's `cagents`
and vendored crates, and each build after another worktree's recompiles them:
1–3 minutes of a build slot every time. A new worktree does not need to start
cold. Before its first gated build, `scripts/cargo-gate` runs
`scripts/seed-build-dir`, which copies the main checkout's compiled
dependencies into the new build directory with btrfs reflinks. The copy takes
under a minute and shares file data with the main checkout's until either side
rewrites a file, so it costs only file system metadata. Seeding leaves out the
fingerprints of this checkout's own and vendored crates, the packages with no
`source` in `Cargo.lock`, so Cargo always rebuilds them and the crates that
depend on them. Cargo would otherwise judge them by source times alone, and a
copied fingerprint can be newer than this checkout's different sources.
Incremental caches are not copied. Seeding does nothing on a file system
without reflinks, when the main checkout has no build or this checkout has no
`Cargo.lock`, for a directory that has been built in, or with
`CAGENTS_BUILD_SEED=0`; run `scripts/seed-build-dir` by hand to seed a
directory a plain `cargo` would create. After copying a target by hand, run
`scripts/seed-build-dir --forget-local TARGET` from the checkout that will build
there, before its first build. It fails when it can't find this checkout's
crates in `Cargo.lock`, or can't search or clean `TARGET`; a `TARGET` with no
fingerprints has nothing to forget. Seeded directories carry Cargo's
markers, so `scripts/prune-build-dirs` reclaims them once they go unused.

The gate's helpers run from the gate's own directory. Install a changed gate
for every checkout with `scripts/install-cargo-gate`, never by copying
`scripts/cargo-gate` alone: it stages the gate with its helpers, checks that
the staged gate runs a command, renames them into `/var/tmp`, and checks the
result.

Build directories share one disk, and Cargo never removes one. Worktrees'
`target/`s, `CARGO_TARGET_DIR`s under `~/.cache` and `/var/tmp`, and test
roots under `~/.local/state` all stay until something removes them.
`scripts/build-disk-guard`, which `scripts/cargo-gate` runs before every
local build, removes them with `scripts/prune-build-dirs` in two ways:

- **On a schedule.** While the file system has room, at most every `CAGENTS_BUILD_PRUNE_INTERVAL_HOURS` (default 6) per user and host, it starts the pruner in the background, in a session of its own at idle CPU and I/O priority, with none of the build's open files. The build doesn't wait for it. A run that is cut short or killed is started again by the next build; none starts while one is running.
- **Running low.** When the file system has less than twice a floor free (5% of its size, at most 64 GiB), it starts the pruner in the background at once, and again each hour, at low rather than idle priority. Builds still don't wait. This run also removes, at once, the build directories of removed workspaces in the mirrored `/var/tmp/cargo-build` layout; on the schedule they wait six hours.
- **Below the floor.** The guard starts the same background run, unless one did within the hour. The build waits for the pruner, at most every ten minutes across the host, or for the run already pruning. After `CAGENTS_BUILD_PRUNE_BUDGET_SECONDS` (default 300) it starts no new search or removal, so the build waits about that long at most. If there is still too little room, the guard refuses the build and says why. With no pruner beside it and no `CAGENTS_BUILD_PRUNE`, it says it has nothing to reclaim with. A `CAGENTS_BUILD_PRUNE` command runs before the build whenever there is less than twice the floor free.

The pruner searches the repository's main checkout first, then the mirrored `/var/tmp/cargo-build`, then `~/.cache`, `~/.local/state` and `/var/tmp`, six levels deep. `CAGENTS_BUILD_PRUNE_ROOTS` replaces the first three. It skips a checkout's committed directories, which hold only sources.

It recognizes build output by Cargo's own markers, not by name or place: a root `.rustc_info.json`, or a `CACHEDIR.TAG` beside a profile's `.fingerprint`. It removes a build directory only when all of these hold:

- nothing inside it has been written or changed for `CAGENTS_BUILD_PRUNE_IDLE_DAYS` (default 7), or four times that for another main checkout's own build, which seeds that checkout's worktrees;
- every Cargo lock in it is free;
- no process has its working directory, an open file, its executable or a mapping inside it.

Under those locks it renames the directory aside, so a build that starts afterwards begins with an empty directory rather than a partly removed one. Still holding them, it puts the renamed copy back if a process opened it meanwhile, such as a build waiting for one of those locks; afterwards, if anything wrote to it. A directory holding a `.git` or `Cargo.toml` is a checkout and never counts as build output. A gated build marks its build directory as used before it waits for a slot, so another build's pruner doesn't take it meanwhile.

It never touches:
- the build directory being guarded;
- the build of this repository's main checkout, in its `target/` or the mirror;
- `~/.local/share`;
- the offload slots, which their leases manage;
- other users' files;
- anything reached through a symlink.

What it removed goes to `/var/tmp/cagents-prune-build-dirs-UID.tsv` (`CAGENTS_BUILD_PRUNE_STATE` sets the prefix). The last two background runs' output goes to `.log` and `.log.1` beside it. Run `scripts/prune-build-dirs --verbose` without `--apply` to see what it would remove and why the rest stays. On the build host, gated builds take this path too. Builds offloaded from other hosts write only into the slots there, and the leases manage those. Cargo run without the gate is pruned only when a gated build next runs on that host. Remove worktrees you no longer need. Set `CAGENTS_BUILD_FREE_FLOOR_GIB` to change the floor, or `0` to disable the guard.

The build script that stamps `--version` reruns only when the stamp could
change, because each rerun recompiles `cagents`. It watches this checkout's
`HEAD` and `HEAD` reflog. While the checkout is clean it also watches the
compiled sources (`src`, `crates`, `vendor`, `assets`, `packaging`,
`fixtures`, `tests/fixtures`, the manifest, lockfile, toolchain, Cargo
configuration and build script), and while it is dirty, the index, which
restoring or stashing rewrites. It reads `git status` without refreshing the
index, and it does not watch the shared `packed-refs`, which pruning or
deleting any branch rewrites. An edit outside those sources, or undoing an edit
by hand, updates the `.dirty` suffix at the next rerun.

Worktrees that do share a build directory stay correct. Cargo makes path
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
CARGO_BUILD_JOBS=2 scripts/cargo-gate check --locked --no-default-features --features ui --bin cagents-ui
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
scripts/cargo-gate check --locked -j 4 --no-default-features --features ui --bin cagents-ui
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
scripts/cargo-gate test --locked --no-default-features --all-targets
scripts/cargo-gate test --locked --no-default-features --features host --all-targets
scripts/cargo-gate test --locked --no-default-features --features ui --all-targets
```

An SSH login has a soft limit of 1024 open files, and whatever it starts, with
`nohup` or `setsid` included, inherits that. A UI test run can exceed it and fail
with `Too many open files`, so run `ulimit -n 65536` first.

HOST fixtures that exec retained actors require
`CAGENTS_TEST_RETAINED_ACTOR_HELPER` to name a `cagentsd` built from the same
checkout and source state as the tests. Build it immediately before testing,
keeping sources, Cargo environment, target, configuration and any explicit
profile overrides fixed. Ordinary build and test profile defaults differ as
expected.
The existing artifact helper reads Cargo's exact executable path, including
configured target directories/triples. A matching version alone does not prove
that two dirty source trees match.

For a local HOST run, select an existing private **disk-backed** base with a
short, physical absolute path. Verify its filesystem with `findmnt -T PATH`;
`/tmp`, `/dev/shm` and `XDG_RUNTIME_DIR` can be RAM-backed. Neither the base nor
any ancestor may contain a `.git` file, directory or symlink. The base must also
be outside the account home reported by `getent passwd "$(id -u)"`, even when
`HOME` points elsewhere: bundled-skill tests deliberately refuse installation
beneath the passwd home. A private `~/.cot` therefore does not qualify.

Use a directory of our own under `/var/tmp`, such as `/var/tmp/ct`, created
with `mkdir -m 700`. `/var/tmp` is disk-backed on fw16; verify it on another
host. Do not borrow another application's tree, and do not change shared
directories or Git markers. The **final** nested Unix
socket path must be shorter than 108 bytes, including fixture suffixes. For
example, `/.tmpXXXXXX/tmp/cagents-codex-<36-byte UUID>/control.sock` adds 79
bytes, so that layout requires a temporary base of at most 28 bytes. Longer
fixture layouts need more room; a short base alone is not proof.

```sh
# Set this only after verifying the base above.
mkdir -p -m 700 /var/tmp/ct
export CAGENTS_TEST_TMP_BASE=/var/tmp/ct
(
    TMPDIR=$(mktemp -d "${CAGENTS_TEST_TMP_BASE:?}/tXXXXXX") || exit
    export TMPDIR
    printf 'Fixture root (retain until children settle): %s\n' "$TMPDIR"
    CAGENTS_TEST_RETAINED_ACTOR_HELPER=$(
        scripts/cargo-gate exec python3 scripts/build-ceditor-product cagentsd --locked \
            --no-default-features --features host --bin cagentsd
    ) || exit
    export CAGENTS_TEST_RETAINED_ACTOR_HELPER
    scripts/cargo-gate test --locked --no-default-features --features host --all-targets
)
```

A helper build failure prevents the test command. Use the same preparation for
narrow HOST targets that need retained actors. Keep the temporary root and any
separately retained fixture evidence until their original children have settled;
this recipe deliberately installs no automatic deletion trap. Do not change
socket limits, remove enclosing Git markers or silently select another fixture
root to make a failed preparation pass.

Tests that spawn the retained helper call `retained_fixture::run_isolated()`
first. The helper calls `setsid` and enters its own systemd scope, so killing a
process group or cgroup misses it. `run_isolated()` reruns the exact test as a
worker beneath a private subreaper. When the worker exits, for any reason, the
subreaper kills and reaps every remaining descendant. Only killing the
subreaper itself, for example by the OOM killer, leaves them behind. After a
passing worker it prints how many it killed: a nonzero count means the product
left processes behind. Same-test reexecs that call `env_clear()` must pass
`retained_fixture::owner_env()` through.

Prefer the narrow target that covers the changed contract, for example:

```sh
scripts/cargo-gate test --locked --no-default-features --features host --lib terminal::
scripts/cargo-gate test --locked --no-default-features --features host --test terminal_load_contract
scripts/cargo-gate test --locked --no-default-features --features ui --test session_restore_contract
```

For focused root library tests, compile one isolated test harness and run the
Cargo-reported executable directly, after its build admission has ended:

```sh
scripts/test-lib cli search_match::tests::overlapping_occurrences_keep_the_best_boundary_rank
scripts/test-lib host --prefix terminal:: --test-threads 1
scripts/test-lib ui --profile dev-runtime --prefix app::chat_ui::
```

Use actual full test names, or repeat `--prefix MODULE::` for literal module
groups. Prefixes expand to ordinary full names and overlapping selections run
once. The command rejects missing names, duplicate exact names, empty groups,
ambiguous artifacts and ignored-only selections. An explicitly named ignored
test is refused; ignored tests within a prefix are visibly skipped. Use the
existing explicit opt-in workflow for ignored cases, after satisfying their
fixture and authority requirements.

The helper fixes `--locked --no-default-features --package cagents --lib` and
selects only the requested CLI, HOST or UI boundary. It preserves the caller's
Cargo environment, stdin, test exit status and cancellation. It submits one
test-compilation command through `scripts/cargo-gate`, then performs discovery
and exact execution without Cargo. Set the usual `CARGO_BUILD_JOBS`,
`CARGO_TARGET_DIR` and `CAGENTS_BUILD_LOCK` as needed. The gate's existing build
directory seeding policy also applies; `CAGENTS_BUILD_SEED=0` disables it. Keep
source and target unchanged until the command finishes; a detected executable
replacement is refused.

Run the helper outside `cargo-gate exec`, a parent `flock`, or a coordinator's
held build lease. It refuses inherited admission because a child cannot release
its parent's lock. The ordinary `cargo-gate test` commands above remain useful
for integration targets, broader suites and explicit ignored-test workflows.

The selected IPC source contract can be checked without compiling Rust:

```sh
scripts/check-protocol-contract
python3 -I -B tests/protocol_contract.py -v
scripts/check-protocol-contract --base-ref origin/main
```

It covers `ClientRequest`, `ServerMessage` and `ServerResponse` in
`src/terminal/ipc.rs`, `Provider` in `src/agent.rs`, and `Run`, `Summary` and
`Reply` in `src/agent_execution.rs`. Ordered fields, variants, types, relevant
attributes and discriminants are recorded as Rust tokens; comments, formatting,
runtime methods and standard nonserialization derives do not change the record.
This partial contract covers the parallel protocol-number collisions reported
in #2119. It is a source guard, not a serializer schema or a wire fingerprint.

When a covered declaration changes, choose a fresh `PROTOCOL_VERSION`, then run
`scripts/check-protocol-contract --record`. Commit the new
`tests/fixtures/protocol-contract/<version>.json` alongside the source change.
Keep old snapshots unchanged. Two branches choosing the same new version add
the same snapshot path: different contracts cause a Git add/add conflict. After
combining the source changes, choose another unused version and regenerate its
snapshot rather than keeping one branch's stale snapshot.

Script CI supplies the PR base or previous main revision from the GitHub event,
fetching that exact object if the checkout is shallow. The guard refuses changed
or deleted base snapshots and unbumped selected declarations relative to that
base, including its first adoption before snapshots existed. A missing base
fails explicitly. A local check without `--base-ref` confirms the current
snapshot only; it does not establish historical immutability.

The guard selects by declaration name; it does not resolve modules, imported
types or aliases, expand macros, evaluate enclosing-module `cfg` or numeric
expressions, or inspect custom serializers, dependency changes
and semantic compatibility rules. Changes outside these seven declarations
still need protocol review and, where applicable, a version bump. Two divergent
branches cannot detect one another before comparison or merge. The historical
upgrade inspector's supported-version policy remains a separate decision.

The four named-tree reconciliation contracts share their cases between the root
UI target and a focused workspace package. For edits confined to vendored
iced-core tree reconciliation, run the focused package first:

```sh
scripts/cargo-gate test --locked -p cagents-iced-contracts
```

The same package checks that a rebuilt named widget keeps the native
accessibility ID it already published, so an assistive-technology action
queued before the rebuild still reaches it.

The package uses the UI's exact iced-core Git revision; the root Cargo patch
resolves both targets to `vendor/iced-core`, with the production `a11y` Id
implementation. It compiles without the application, libcosmic renderer, or
native UI runtime. The cases do not render; the isolated harness uses an
uninhabited renderer. Retain validation through the UI's concrete renderer when
changing the tree contract or UI integration:

```sh
scripts/cargo-gate test --locked --no-default-features --features ui --test iced_tree_reconciliation
```

Explicitly selecting a target with required features absent, including through a
`--test '*'` glob, reports the missing feature instead of succeeding with zero
tests. Use `--all-targets` to select all targets available in a feature lane.

The 40,000-path mutation regression lives in `repository_scale_contract`, so a
library-suite run does not also create and mutate that filesystem fixture.
`--all-targets` still runs it in both HOST and UI lanes. When changing repository
mutations or pathspec handling, include its explicit target:

```sh
scripts/cargo-gate test --locked --no-default-features --features host --test repository_scale_contract
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

The explicit-source stage can reuse a successful check only when its source,
configuration, environment and formatter inputs match. Each invocation reads
formatter binaries and libraries once, then reopens them and checks their full
file identity before reusing those digests for its second snapshot. No tool
digest or file-metadata shortcut is retained between invocations. Unverifiable
inputs run the ordinary formatter instead.

For UI changes, launch the real UI with
the runner and exercise the changed path. For browser or feature-boundary
changes, verify both the browserless and integrated lanes. Performance
workloads are described in [Performance](performance.md).

## Native evidence

`scripts/native-evidence` captures before/after screenshots of the actual native
app, from a shell where the desktop session or `DISPLAY` is unavailable, such as
plain SSH. It runs headless: KWin's virtual backend hosts a nested niri session,
driven by `niri-harness`, running `scripts/run-development --browserless`.
Each task keeps everything in one directory outside Git ancestry. Avoid `/tmp`,
`/var/tmp` and `~/.cache`, where Codex sandboxes can leave an empty `.git` that
fixtures refuse.

Build before taking the native lock, and never wait for Cargo while holding it: every other native session queues behind you. A `scripts/run-development` launch under that lock, whether an ancestor has it open or it runs in a nested niri session while the lock is held, won't compile or join the Cargo queue: Cargo may confirm the build is current, a compile fails naming the crate, and after `CAGENTS_NATIVE_LOCK_BUILD_SECONDS` (default 180) the launch gives up. `build` takes one `scripts/cargo-gate` slot. A session runs the real `scripts/run-development`, but until the harness reports it ready, shortly after its window appears, its `cargo` only accepts what `build` compiled, with the same arguments. Anything else ends the session at once with the reason and releases the lock, and `start` returns once that is done, so `build` can follow straight away. After that it is real Cargo, so a terminal or language server in the app works; one that runs Cargo before then ends the session. Real Cargo run from inside a ready session still holds the native lock while it works, so keep builds out of a session. A hand-written session needs the same care: prebuild, give the runner a private `CAGENTS_BUILD_LOCK` with `CAGENTS_BUILD_SLOTS=1`, close the lock descriptor in children (`9>&-`), bound its stop command with `timeout`, and run `niri-harness` without a mise shim, which re-executes itself forever under a private `XDG_DATA_HOME`.

```sh
export CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/my-task
git worktree add --detach .worktrees/my-task-before origin/main

# A build slot only. Seed a new target copy-on-write from a finished one.
scripts/native-evidence build before .worktrees/my-task-before --seed-target /var/tmp/other/target
scripts/native-evidence build after . --seed-target "$CAGENTS_EVIDENCE_TASK/target-before"

# Prepare data once, then keep it. Every later start restores this seed.
scripts/native-evidence start before
scripts/native-evidence cli project open "$CAGENTS_EVIDENCE_TASK/fixture/project"
scripts/native-evidence stop
scripts/native-evidence save-seed

scripts/native-evidence start before --width 1200
scripts/native-evidence drive observe
scripts/native-evidence drive click 'Open repository'
scripts/native-evidence shot channel-prs     # shots/before-channel-prs.webp
scripts/native-evidence stop

scripts/native-evidence start after --width 1200
# ...the same steps...
scripts/native-evidence stop
```

Run `scripts/native-evidence --help` for every command. Beyond a hand-written
harness, it provides:

- **Isolation.** The fixture has its own HOME, XDG directories, daemon and runtime
  directory, and the compositor has separate ones, so a session leaves the desktop's
  settings alone. The app starts from an empty environment: nothing from your shell
  reaches it except locale and terminal settings, and where Cargo and rustup live. `claude` is the offline
  [synthetic provider](chat-composer-fixture.md), the same file on both sides, and
  `gh` always fails. Files in `$CAGENTS_EVIDENCE_FIXTURE/home/.claude` switch fixture
  behaviors. `env CMD` runs your own setup in this environment, with the fixture's
  root in `CAGENTS_EVIDENCE_FIXTURE`. Terminals and agents still enroll in scopes on
  the host's user manager, as development sessions do.
- **Matching evidence.** `start` refuses a checkout that changed since its build.
  `shots/manifest.tsv` records each image's revision, source state, requested
  width and size, so a pair can be compared and named in the PR.
- **A shared path caveat.** With no worktree selected, the Workspace page shows
  the app's working directory, which is the checkout and so differs between
  sides. Select the fixture project's worktree before capturing a pair.
- **One session at a time.** `start` waits for `/var/tmp/cagents-fw16-native.lock`
  (override with `CAGENTS_EVIDENCE_NATIVE_LOCK`). Stop as soon as you have the
  evidence. A session gives the lock up after `--limit` seconds even if its
  owner has gone. `build` uses `/var/tmp/cagents-fw16-cargo.lock` the same way.
- **Who holds the lock.** `scripts/lock-holders /var/tmp/cagents-fw16-native.lock` shows
  the holder and whether it is doing anything, when `start` seems stuck.
- **Cleanup.** `stop` stops the fixture daemon, then ends any process that
  inherited the task's private runtime directory. That path is the proof of
  ownership, so nothing else on the host is touched.

Like `fixture_env`, it exits 77 under inherited agent authority and never removes
that authority, so a hosted agent shell cannot use it. Remove the task directory
when the PR merges; its targets are large.

## Installer and package checks

Repeatable upgrade-policy checks:

```sh
scripts/cargo-gate test --locked --no-default-features --test installer_lifecycle_contract
scripts/smoke-package-upgrades
```

The second command requires Bubblewrap and `dpkg`. It uses an unprivileged,
networkless root filesystem with fixture executable processes, including an
unlinked old executable. It installs, upgrades, and removes fixture Debian
packages using the generated maintainer-script contract, checks
mixed-generation interruption, and preserves unrelated files and fixture user
data. It does not signal the host daemon or install anything on the host.

## Installing your checkout

`scripts/install-local` replaces your installed cagents with the committed
`HEAD`:

```sh
scripts/install-local --build-host fw16
```

It builds the three matched executables with the `local` Cargo profile,
versioned `<version>+local.<short sha>`. That profile keeps release identity
and optimization but skips link-time optimization and single-unit code
generation. Each build is incremental, so after the first one only changed
code is recompiled. The script stages the archive with
`install.sh --stage-only` and runs `cagents daemon upgrade`, which shows each
affected session and asks before replacing anything. `--stage-only` stops
after staging so you can finish from the application's update notice instead.

The build runs in the repository's `.worktrees/.install-local`, a detached
checkout the script owns, so its target directory stays warm and edits in your
other checkouts never reach the binaries. With `--build-host`, `HEAD` is pushed
to `refs/cagents/install-local` in the repository at the same path on that host
and built there. The archive is copied back, and the script refuses it if it
needs a newer glibc than this machine has. Uncommitted tracked changes are
refused so that the version names its exact source. If that version is already
current, the build is skipped. After staging, older `+local` builds are removed,
except the two most recent and any that a running process still uses.

The script first fetches and checks what `HEAD` is, because a checkout nobody
updated would install old code. It refuses a detached `HEAD` that is not the
tip of `origin/main`, and a branch that is behind its upstream, and prints the
command that updates the checkout, such as
`git checkout main && git pull --ff-only`. Pass `--this-commit` to build that
exact commit anyway, for example to install a known-good build again. Any other
branch is built, and the script names it.

Published releases still come from `scripts/build-release-linux`; see
[Releasing](releasing.md).

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| `required command not found: zig` | Install Zig 0.15.2 and put it on `PATH` (`mise install` reads `mise.toml`). |
| The UI talks to an old daemon after you changed daemon, host, or IPC code | Relaunch with `--fresh-daemon`. |
| `development daemon did not stop within 5 seconds` | Look for a lingering development daemon with `pgrep -a cagentsd`, let it exit (or stop that process), then retry `--fresh-daemon`. The socket is `$XDG_RUNTIME_DIR/cagents-development/cagentsd.sock`. |
| Compilation exhausts RAM or swap | Lower `CARGO_BUILD_JOBS`, use `--browserless`, and build through `scripts/cargo-gate` rather than starting builds side by side; see [resource usage](#build-and-test-resource-usage). |
| A UI problem needs logs | Add `--debug --log-file /tmp/cagents-ui.log` and read the file after reproducing. |
| New links do not open in browserless mode | Install `xdg-utils`, or use a desktop with a working portal. |

## Scripts and mise shims

A mise shim on `PATH` under a moved `HOME` or `XDG_DATA_HOME`, as in a fixture, finds
itself and re-executes without end: no error, and the process keeps its pid and
restarts forever, outliving a `timeout` on its parent. So scripts here name
`/usr/bin/python3` rather than finding `python3` on `PATH`, and a host running them
needs it there. `scripts/cargo-gate` also sets `MISE_DATA_DIR` from the shims directory
on `PATH`, so what it runs can move `HOME` without that loop.

Separately, a shim under a `mise.toml` that mise has not been told to trust stops at
mise's trust prompt and waits for an answer whenever its stderr is a terminal, even with
stdin from `/dev/null` or stdout piped. The gate's own helpers no longer reach it, but
`scripts/cargo-gate exec python3` and a bare `python3` still do. The gate does not answer
for you, because trusting a checkout's configuration is your decision. Run `mise trust`
in a new worktree, and in the checkout above it if that isn't trusted either. Taking
the shims off `PATH` also avoids the prompt, but removes the tools only they provide,
such as `zig`. With stderr elsewhere mise skips the untrusted file instead, so a tool
only that file pins can fail at once with `No version is set for shim`.

A harness of your own that moves `HOME` should take the shims off `PATH` and name the
tools it needs by their real paths. mise keeps its trust list under `HOME`, so setting
`MISE_DATA_DIR` stops the loop but not the prompt. Run it in a systemd scope with a time
limit too, which stops everything it started.

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
