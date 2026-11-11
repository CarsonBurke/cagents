# Development

- [Prerequisites](#prerequisites)
- [Feature boundaries](#feature-boundaries)
- [Development runner](#development-runner)
- [Build and test resource usage](#build-and-test-resource-usage)
- [Testing](#testing)
- [Shared GitHub traffic](#shared-github-traffic)
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

Each of these commands waits in the build host's queue on its own, and on a
busy host the wait is most of its time: on fw16 under a full queue, the CLI
check above took 26 s to compile and 19 minutes in all. The CLI, host and
browserless UI lanes' tests (`scripts/cargo-gate test ... --all-targets`,
[below](#testing)) type-check everything the lane's check does, so when you are
about to run them, skip that check. The integrated UI has no test lane; keep
its check.

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
| `--daemon-log PATH` | Append the development daemon's tracing and panic output to `PATH` (relative to the calling directory), and imply `--debug`. A daemon is otherwise started with its output on `/dev/null`. It also works with `--instance`, and applies to the daemon the launch starts, so add `--fresh-daemon` to replace one that is already running |
| `--instance NAME` | Run an isolated instance in a headless niri-harness session; see [Parallel instances](#parallel-instances) |
| `--stop-instance NAME` | Stop that instance's daemon, its jobs and its harness session; keep its storage |
| `--list-instances` | List the development daemons running on this computer, with checkout, age, instance, who started them, when they stop and how to stop them; see [Finding and stopping instances](#finding-and-stopping-instances) |
| `--keep` | With `--instance`: keep the instance after the session that started it ends; see [How long an instance runs](#how-long-an-instance-runs) |
| `--ttl SECONDS` | With `--instance`: stop the instance that long after it starts |
| `--gc` | Stop instances whose starter ended longer ago than their grace period, other than kept ones, and clear what dead harness sessions left. Fails without stopping anything or removing storage when `niri-harness list` fails |
| `--storage DAYS` | With `--gc`: also delete the storage of instances that are not running and that nothing touched for `DAYS` days |
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
for its next launch; `--gc --storage DAYS` deletes what has gone unused, or
delete `~/.local/share/cagents-development-NAME` and its config, state and
cache siblings to discard one.

#### Stopping instances from a cagents session

An agent in a terminal or chat that cagents hosts can stop and replace the
instances it starts: `--stop-instance NAME` and `--instance NAME --fresh-daemon`
work there as they do in your own terminal, with the session's
`CAGENTS_AGENT_AUTHORITY` left in place. The CLI's
`daemon stop-for-rebuild`, which both use, decides from the process tree:

- It stops a development daemon with a runtime directory of its own: any
  `--instance`, or the unnamed one inside a harness session or a fixture's
  private `XDG_RUNTIME_DIR`.
- It refuses the daemon that hosts the session, or one hosting a session it
  descends from, since stopping that would end them. Every process a daemon
  hosts runs below one of the daemon's own processes, so this holds for hosts
  of any version and after a daemon restart. `niri-harness` detaches an
  instance from the session that started it, so the runner records the
  runtime directories of the daemons hosting that session in
  `CAGENTS_DEVELOPMENT_HOSTED_BY` for every app it starts, and the check reads
  it from each daemon above the caller. A session in instance B, started from
  a session in instance A, can't stop A either.
- It refuses when it can't tell: no daemon above the hosted caller, or one
  whose environment it can't read.
- It refuses the unnamed development daemon in the login runtime
  (`/run/user/UID/cagents-development`), which belongs to the person at the
  computer, and every production daemon. A production CLI still refuses all
  lifecycle commands from a hosted session.

Other lifecycle commands, such as `daemon stop` and `daemon upgrade`, remain
refused in every hosted session. Stopping an instance also ends whatever else
runs in its harness session, so the runner refuses `--stop-instance` and
`--fresh-daemon` for an instance whose daemon hosts the caller, or a session it
descends from, before it asks the CLI. Like the CLI, it refuses a hosted caller
whose hosts it can't tell.

#### Finding and stopping instances

`scripts/run-development --list-instances` lists every development daemon
running on this computer, whoever started it:

```text
pr2064, pid 1118559, running 02:17:40
  checkout:   /home/me/cagents/.worktrees/pr2064
  runtime:    /run/user/1000/nh-MwPpz5r1
  started by: claude (pid 2417058), which has ended, in a cagents session
  lifetime:   stops 600 seconds after its starter ends
  stop:       scripts/run-development --stop-instance pr2064
```

The checkout is marked `(removed)` when its worktree is gone, and `executable
rebuilt since` when the daemon runs an executable that a later build replaced.
`started by` comes from a record `--instance` keeps in the session's runtime
directory: for a hosted caller, the process its cagents session runs, such as
the agent's provider; otherwise the shell that ran the runner. An instance whose
starter has ended is usually safe to stop. Daemons started another way have no
record, and no runner session to stop, so the listing names their process ID;
`kill PID` ends such a daemon and its jobs. The listing says when a daemon hosts
the session that runs it, or one that started it, or is the login session's own
development daemon, instead of offering a stop command. In a cagents session
whose hosts can't be told, it offers none. It finds development daemons by the
instance lock they hold, so it also lists those built outside a checkout's
`target/`.

#### How long an instance runs

An instance stops by itself 10 minutes after the session that started it ends:
the agent's provider process for a hosted caller, otherwise the shell that ran
the runner. The grace period is meant to leave an instance alone while its
agent moves on to its next step. A restarted provider is a new process,
though, so the instance doesn't pass to it: it still stops 10 minutes after the
first one ended. Outside a cagents session the starter is the runner's parent
process. A tool that runs each command in a shell of its own, as an agent in a
plain terminal does, ends that shell as soon as the command returns, so its
instance stops 10 minutes later; start it with `--keep` or `--ttl` instead. `CAGENTS_INSTANCE_GRACE_SECONDS` changes the 10 minutes for
the instances a launch starts.

- `--keep` keeps it until someone stops it, so another agent or a person can
  go on using it. Use it when you start an instance for someone else, or
  from a one-shot shell such as `ssh fw16 'scripts/run-development ...'`, whose
  shell ends at once.
- `--ttl SECONDS` stops it that long after it starts, kept or not, which suits
  a measurement or a review with a known length.

A watcher that the launch leaves detached applies this every 30 seconds, and
ends with the session however that ends. Each stop is logged, with its
reason, in `~/.local/state/cagents-development-NAME/run-development.log`. The
watcher doesn't ask the daemon to stop, as `--stop-instance` does, because the
CLI that would ask may no longer be able to tell its hosts. Instead it sends
`TERM`, the signal `daemon stop-for-rebuild` sends, to every process started
with the instance's runtime directory. Ten seconds later it sends `KILL` to
what remains, then stops the harness session and removes the runtime
directory. Jobs the daemon started in systemd scopes of their own are found
the same way, by that directory in their environment. Their scopes are not
stopped as units, so a job that cleared its environment is missed. An instance started with
`--keep` and no `--ttl` has no watcher.

The next `--instance` launch and `--gc` repair what a watcher could not:

- A harness session that ended without being stopped, such as a killed
  compositor, is cleared: what still runs in its runtime is stopped and the
  directory removed, so the name can be launched again rather than refused.
  A session that changed in the last minute may still be starting, so it is
  left alone, as `niri-harness gc` does.
- An instance whose watcher was killed is stopped once its lifetime has
  passed, measured from its start.
- `--gc` also stops instances started by older runners, which recorded no
  lifetime, once their starter has ended; it names any running instance with
  no record at all, and leaves it.

None of them stops an instance whose daemon hosts the caller, or one that a
session it descends from runs in; a hosted caller whose hosts can't be told
stops nothing. `--gc --storage 14` also deletes the storage of instances that
aren't running and that nothing touched for 14 days. An instance counts as
running while its harness session is live or any process was started for it.

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
host that does not report pressure is treated as having room.

When a build starts waiting for CPU, the gate says what holds the CPUs. Once
it has released its admission decision, it samples the host's processes for
about a second. It reports how many CPUs the running builds use and how many
everything else uses. It then names up to three process sessions outside the
builds that use at least half a CPU each, and the kernel's own threads when
they do. For each session it gives the command that started it and how long
ago, the CPUs it uses, how many processes it has and its busiest process. If
the session's leader has exited, it says so. Other sessions are summed. The
message ends with how to look at the first session, `ps -s SESSION`, and to
end it, `pkill -s SESSION`. It never offers `pkill` for a session holding
cagents itself, such as the installed daemon that hosts every terminal and
agent; for the session of the wait or of anything that started it, such as
the waiting agent; or for the system's session, one started from the desktop
or a terminal's. It says which it is instead. The process count is
everything in the session, as `pkill -s` would end it. The message points at
`scripts/bounded --list` when the checkout has that script. Sampling gives up
after 10 seconds, as reading a process stuck on a hung mount can block:

```
cargo-gate: builds use 6.2 CPUs and everything else 9.3 CPUs; besides builds, the CPUs go to session 2270967 (bash -s, started 1d12h ago): 8.9 CPUs in 11 processes, most in pid 2272069 (python3 -I stress.py --workers 8); and 4 more sessions using 0.4 CPUs. `ps -s 2270967` lists session 2270967's processes, and `pkill -s 2270967` ends them
```

When no other session uses half a CPU, and everything else together uses
less than one CPU, or less than a quarter of what the builds use when that is
more, the message begins `nothing else uses much; the running builds hold
them` instead. It begins `nothing uses much at the moment` when the builds
use less than half a CPU, less than everything else, or, with everything
else, less than half the host's CPUs: pressure is a ten-second average, so a
wait can follow a burst that has already ended. When everything else uses more than that, but no
session half a CPU, it ends `though no session outside them uses half a
CPU`.

A session is what one login, ssh command or terminal started. A loop that
keeps starting short-lived commands is one session, and the time of the
commands it has reaped counts as its own. The builds are:

- the processes below a running build's holder, or below a process holding
  a slot lock, which includes one taken by a plain `flock`. The slot locks
  are the waiting family's, those the build registry names, and on the host
  each `/var/tmp/cagents-*.lock` slot family with a `.queue` beside it; other
  locks there, such as a native-evidence session's, are not builds;
- processes whose environment marks them as a running build's:
  `CAGENTS_BUILD_SLOT_HELD` for a gated command, `CAGENTS_OFFLOAD_SLOT` for an
  offloaded one;
- processes working in a leased offload slot, as its file copies do.

A process in an offload slot, or with its mark, that started before the
process holding the slot's lock is left from an earlier build, and it is
named.
Whatever a gated command detaches stays below the gate's command supervisor,
which is a subreaper. A plain `flock` holder's command carries no mark, so
whatever it detaches without the lock's descriptor is named as something else.
An offloaded build's wait says the same about the build host, whose commands
it says to run `there`.

Each build has an expected peak: 3 GiB for anything in the CLI lane, 4 GiB for other checks, 7
GiB for a host build or test build, 9 GiB for a UI one, 8 GiB for an `exec`
script and 9 GiB for an `exec --hold` one such as the development runner's; a
slot held by a plain `flock` counts as 9 GiB. `CAGENTS_BUILD_COST_GIB`
overrides it. A build starts beside others only
when the memory available, less what the running builds are expected to take
beyond what they already hold, covers its own, and while the kernel's memory
pressure (`some avg10` in `/proc/pressure/memory`, the share of time some task
stalled on reclaim) is below 10% (`CAGENTS_BUILD_MEMORY_PRESSURE`; `0` disables
it). A lone build always starts, whatever the memory or pressure. Each build gets a
share of the CPUs among the builds running once it starts, between 4 and 8 (the
jobs the costs were measured with) and at most half the CPUs, as
`CARGO_BUILD_JOBS`, which also caps a larger value; `-j` still wins. For
`test`, `bench` and `run` only the build holds a slot, and the tests or program
run after it is released, so do not wrap the gate in another lock. Tests and
benchmarks run under a memory limit; see [Memory limits](#memory-limits). Rustdoc
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

### Watching main after merges

GitHub's script checks do not compile Rust. On fw16,
`scripts/main-build-sentinel --repository /absolute/path/to/cagents` fetches main
at most once every two minutes and checks every new first-parent commit in
order. Its first run checks the current tip. Each commit gets three separate
`scripts/cargo-gate check --locked --no-default-features --all-targets` commands:
CLI, `--features host`, and `--features ui`. These use the ordinary shared queue
and the persistent sentinel worktree cache, without an urgent admission or a
held slot between lanes.
The sentinel neither merges nor modifies another checkout or native session.

After this change merges, install **one** user timer on fw16, from an updated
main checkout. Put this in `~/.config/systemd/user/cagents-main-sentinel.service`,
replacing `/absolute/path/to/cagents` in both places:

```ini
[Unit]
Description=Compile cagents main after merges

[Service]
Type=oneshot
ExecStart=/absolute/path/to/cagents/scripts/main-build-sentinel --repository /absolute/path/to/cagents
TimeoutStartSec=infinity
```

Put this in `~/.config/systemd/user/cagents-main-sentinel.timer`:

```ini
[Unit]
Description=Watch cagents main every two minutes

[Timer]
OnBootSec=2min
OnUnitInactiveSec=2min

[Install]
WantedBy=timers.target
```

Before enabling the timer, create its checkout with
`git worktree add --detach .worktrees/main-build-sentinel origin/main` and run
`mise trust .worktrees/main-build-sentinel/mise.toml` from the repository, as well
as trusting the main checkout's `mise.toml` if needed. The noninteractive service
cannot answer a mise trust prompt.

Then run `systemctl --user daemon-reload` and
`systemctl --user enable --now cagents-main-sentinel.timer`. The user manager
must stay running, and `git fetch origin main` and `gh` must already be signed
in as the repository account. Inspect it with
`journalctl --user -u cagents-main-sentinel.service`; stop future runs with
`systemctl --user disable --now cagents-main-sentinel.timer`. Stopping the
service also cancels its current gate command; it does not stop other builds.

The detached `.worktrees/main-build-sentinel` checkout belongs to the sentinel.
A repository lock prevents simultaneous runs from changing it during a check.
State and full per-commit lane logs live in
`~/.local/state/cagents-main-sentinel` (`--state` overrides it). On failure it
creates or updates one issue titled **Main Rust build sentinel: compilation
failure**, reopens it if closed, and includes the SHA, parents, last passing SHA,
lane statuses and diagnostic tails. A later passing commit adds a recovery
comment; a person closes the issue. Successful commits need no GitHub API call.
Notification failures retain the completed result for retry without rebuilding;
rate limits defer attempts until the reset from `gh api rate_limit`. Other
notification attempts wait at least two minutes. A main force-push resets the
baseline to its new tip. This detects failures after merge; it does not replace
checking an up-to-date branch before merge.

The offline regression uses disposable Git history and stand-ins for the gate
and GitHub: `python3 -I -B tests/main_build_sentinel.py -v`. It does not compile
Rust or change real issues. Remove old logs under the sentinel's state directory
when their evidence is no longer needed; leave `state.json` in place to preserve
its checked cursor and issue number.

### Building on another host

Rust compiles on the build host named by `CAGENTS_BUILD_REMOTE` (default
`fw16`, reached with `ssh`), never here: `scripts/cargo-gate` sends every Cargo
command that compiles there through `scripts/cargo-offload`, without taking a
slot here. Commands that compile nothing, such as `metadata`, `tree` or
`clean`, run here at once. Under `scripts/cargo-gate exec COMMAND`, a `cargo`
first on `PATH` leads every Cargo command in COMMAND back to the gate, so a
script of builds, such as `scripts/build-ceditor-product`, builds there too,
each command with its own admission. `scripts/cargo-gate exec --hold COMMAND`
leases one slot there first, at the cost of a UI build, and holds it until
COMMAND exits: each gated Cargo command in it builds in that slot, one at a
time, with no admission of its own. It is for a script that only builds, such
as the development runner's three: a launch waits in the host's queue once
rather than three times. Tests run in that slot too, priced by what they hold
while they run and as a UI build again once they end. A Cargo command run from
another checkout, or after the hold's lease has ended, leases a slot of its
own. Anything slow between its builds would keep the slot leased and its memory
reserved while it ran, so leave `--hold` off for such a script. A Cargo
reached any other way inside COMMAND, by path, through `$CARGO`, `rustup run`
or `mise exec`, would compile here, so `exec` also sets `RUSTC_WRAPPER` to
`scripts/cargo-gate-rustc-guard`, which refuses to compile a crate and says
why; Cargo's queries of rustc pass, and a wrapper the caller set still runs.
Programs launched by `cargo run` inside that `exec` inherit the same Cargo
shim and guard. Their named `cargo` reenters the gate after the program's
build releases its slot; their absolute `$CARGO` cannot compile here.
On the build host itself, or with
`CAGENTS_BUILD_REMOTE=local`, commands build here through the local slots
above. With `CAGENTS_BUILD_REMOTE_LOOPBACK=1`, the build host's own commands
build in its offloaded slots instead, as another host's would: the gate runs
the same commands and copies through a local stand-in for ssh, which starts
them in the user's login shell (`CAGENTS_BUILD_REMOTE_LOOPBACK_SHELL` names
another) from a clean environment, as sshd would, and only what the caller
uses comes back. `scripts/native-evidence` builds this way. A gate without
`scripts/cargo-offload` beside it refuses to build unless
`CAGENTS_BUILD_REMOTE=local` is set.

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

Each slot's build output is bounded by its lease. Cargo removes nothing, and
every feature lane, profile and kind of target of the `cagents` crate keeps
an incremental session of 1–6 GiB, so slots on fw16 grew to about 97 GiB
each, 40–60 GiB of it incremental state. Once a lease holds a slot, and no
earlier command of the slot's is still running, it keeps the incremental
state of the crates the slot compiled most recently until they fill
`CAGENTS_BUILD_REMOTE_INCREMENTAL_GIB` (default 20), the one that crosses it
included, and moves the rest out of the target; with `0` it keeps none. Every
6 hours it also moves out compiled units no build has looked at for 3 days,
such as old versions after a dependency update. Cargo reads the fingerprint
of every unit in a build's graph, even one it doesn't compile, so a unit is
dated by its `.fingerprint/<package>-<hash>` directory and its outputs: a
build dependency's rlib or a compiled build script is read only when it is
rebuilt, so judged alone it would look unused in a lane that builds daily.
A stale unit's fingerprint goes first, then its `deps/<crate>-<hash>` files
and `build/<package>-<hash>` directory, so a unit moved out only in part is
rebuilt. It skips this where the file system keeps no access times. Below twice the host's floor, the larger of
`CAGENTS_BUILD_REMOTE_FREE_FLOOR_GIB` and the [guard's](#compilation-jobs-and-shared-build-directories)
default, a slot keeps a quarter of the budget, its dependencies are looked
through at once, and the lease does the same to every other slot no lease
holds, under that slot's locks; it holds their lease locks shared, which no
lease, admission or hold takes for a running build. A crate whose session a `rustc` holds stays,
whatever started it. Each crate's state is moved out with one rename into the
slot's `evicted` directory, which Cargo never reads, so a build never finds
part of one; a process of its own then removes it at idle CPU and I/O
priority, or, when a build waits for the space, at low priority beside any
idle removal already under way. The next trim of the slot finishes a removal
that was cut short. Rustc remakes an evicted session, so that crate's next build compiles
it in full rather than incrementally; Cargo rebuilds a removed dependency.
Incremental compilation stays on in every lane: on fw16, a one-line edit in
the UI crate rebuilt in 19–31 s for `check`, 50–60 s for the `dev-runtime`
build and 68–74 s for the library tests, against 54–79 s, 104–145 s and
166–217 s with `CARGO_INCREMENTAL=0`.
Measuring a full slot's sessions takes 1–2 seconds; a finalized session
never changes, so each is measured once and recorded in the slot's
`incremental-sizes`, and a lease measures only what the last build made. One
that takes over 5 seconds says it is trimming the slot.

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
- **Running low.** When the file system has less than twice a floor free (5% of its size, at most 64 GiB), it starts the pruner in the background at once, and again each hour, at low rather than idle priority. Builds still don't wait. This run also removes, at once, the build directories of removed workspaces in the mirrored `/var/tmp/cargo-build` layout, and native-evidence builds unused for an hour; on the schedule they wait six and twelve hours.
- **Below the floor.** The guard starts the same background run, unless one did within the hour. The build waits for the pruner, at most every ten minutes across the host, or for the run already pruning. After `CAGENTS_BUILD_PRUNE_BUDGET_SECONDS` (default 300) it starts no new search or removal, so the build waits about that long at most. If there is still too little room, the guard refuses the build and says why. With no pruner beside it and no `CAGENTS_BUILD_PRUNE`, it says it has nothing to reclaim with. A `CAGENTS_BUILD_PRUNE` command runs before the build whenever there is less than twice the floor free.

The pruner searches the repository's main checkout first, then the mirrored `/var/tmp/cargo-build`, then `~/.cache`, `~/.local/state` and `/var/tmp`, six levels deep. `CAGENTS_BUILD_PRUNE_ROOTS` replaces the last three. `~` is the account's home from the password database, because a test fixture's builds run under a private `HOME` that holds none of the account's build output, and a search of only that would find nothing yet count as a complete run. When `HOME` differs, its `~/.cache` and `~/.local/state` are searched as well, for a user who keeps builds under a `HOME` of their own. `~/.local/share` and the offload slots are never touched under either: their leases bound them, as [described above](#building-on-another-host), and before each of the guard's background runs and its run below the floor, `scripts/cargo-offload --trim-slots` beside it bounds the slots under the account's `~/.cache/cagents-offload` (`CAGENTS_BUILD_OFFLOAD_SLOTS`; empty for none) that no lease holds, under their leases' locks, as a lease would. A background run's account of it is in `/var/tmp/cagents-prune-build-dirs-UID.slots.log`. Below the floor the build waits for that trim and its removals beside the pruner, within the same budget. A `CAGENTS_BUILD_PRUNE` command replaces both. It skips a checkout's committed directories, which hold only sources.

It recognizes build output by Cargo's own markers, not by name or place: a root `.rustc_info.json`, or a `CACHEDIR.TAG` beside a profile's `.fingerprint`. A [native-evidence](#native-evidence) task's `target-before` and `target-after`, beside the task's `.native-evidence` marker, are build output too. It removes a build directory only when all of these hold:

- nothing inside it has been written or changed for `CAGENTS_BUILD_PRUNE_IDLE_DAYS` (default 7), or four times that for another main checkout's own build, which seeds that checkout's worktrees, or 12 hours for a native-evidence task's, 1 hour while running low;
- every Cargo lock in it is free;
- no process has its working directory, an open file, its executable or a mapping inside it, and, for a native-evidence task's, no `native-evidence` process names the task with `--task`, as its session does from before it waits for the native lock.

Under those locks it renames the directory aside, so a build that starts afterwards begins with an empty directory rather than a partly removed one. Still holding them, it puts the renamed copy back if a process opened it meanwhile, such as a build waiting for one of those locks; afterwards, if anything wrote to it. A directory holding a `.git` or `Cargo.toml` is a checkout and never counts as build output. A gated build marks its build directory as used before it waits for a slot, so another build's pruner doesn't take it meanwhile.

It never touches:
- the build directory being guarded;
- the build of this repository's main checkout, in its `target/` or the mirror;
- `~/.local/share`;
- the offload slots, which their leases manage;
- other users' files;
- anything reached through a symlink.

What it removed goes to `/var/tmp/cagents-prune-build-dirs-UID.tsv` (`CAGENTS_BUILD_PRUNE_STATE` sets the prefix). The last two background runs' output goes to `.log` and `.log.1` beside it. Run `scripts/prune-build-dirs --verbose` without `--apply` to see what it would remove and why the rest stays. On the build host, gated builds take this path too. Builds offloaded from other hosts write only into the slots there, and the leases manage those. Cargo run without the gate is pruned only when a gated build next runs on that host. Remove worktrees you no longer need. Set `CAGENTS_BUILD_FREE_FLOOR_GIB` to change the floor, or `0` to disable the guard.

Each executable states its version with `cagents::stamp_version!()`, invoked
once as an item at its root; the library answers `cagents::version()` from
that. The macro stamps from a constructor that runs before `main` and before
the holders' own constructors, which serve in place of `main`.
The stamp is read where the macro expands, so a new commit or a move to another
build slot relinks the executables and leaves the library compiled. Never read
`CAGENTS_BUILD_METADATA` or `CAGENTS_CHECKOUT_METADATA` in the library or in
`build.rs`: Cargo would recompile the whole library for each new stamp. A
process that never stamps, such as a unit test, reports
`<version>+unstamped`.

The gate works out the stamp for each Cargo command once it has a slot, here
or for the build host, and passes it with `CAGENTS_BUILD_METADATA_SUPPLIED=1`,
so `build.rs` leaves the checkout alone. A program a local `run` or `test`
starts gets the caller's stamp back through a Cargo runner, unless Cargo
already has a runner configured, so a bare Cargo it starts reads its own
checkout. Gated and bare Cargo commands sharing one build directory, such as
an editor's `cargo check` beside gated builds on the same host, rerun
`build.rs` and recompile `cagents` each time they alternate. Without the gate, `build.rs` reads the checkout's
Git state and reruns, recompiling `cagents`, only when the stamp could change.
It watches this checkout's `HEAD` and `HEAD` reflog. While the checkout is
clean it also watches the compiled sources (`src`, `crates`, `vendor`,
`assets`, `packaging`, `fixtures`, `tests/fixtures`, the manifest, lockfile,
toolchain, Cargo configuration and build script), and while it is dirty, the
index, which restoring or stashing rewrites. It reads `git status` without
refreshing the index, and it does not watch the shared `packed-refs`, which
pruning or deleting any branch rewrites. An edit outside those sources, or
undoing an edit by hand, updates the `.dirty` suffix at the next rerun.

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

Product tests live in the library, so select them with `--lib`, not `--bin`.
`--bin cagents` and `--bin cagents-ui` have no tests and refuse a filter rather
than pass by running none, except that `--bin cagents-ui` does not refuse with the
`hot-reload` feature. `--bin cagentsd` has only a few of its own and does
not refuse. A filter that matches nothing still exits 0 on any target, so check
the reported test count before relying on a green run.

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

### Memory limits

A runaway test can take the whole host with it: one mutation run reached 41 GiB
of memory and 70 GiB of swap at about 18 GiB a minute, and FW16 stalled for about
12 minutes until the kernel killed it. `scripts/memory-capped COMMAND` runs a
command and its children in a systemd scope limited to 16 GiB of memory
(`CAGENTS_MEMORY_MAX`; `0` turns the limit off) and 4 GiB of swap
(`CAGENTS_MEMORY_SWAP_MAX`), stops the scope when they go over, says so and exits
137. The swap limit matters as much as the memory one, because without it a
runaway just moves into swap. For scale, the whole UI lib test binary peaks at
about 3 GiB and the host one at about 1 GiB. A native evidence session with 8 busy
chats and 6 long terminals peaked at about 800 MiB, and the integrated app at about
8 GiB after ten loads of deliberately heavy pages, apparently because Servo kept
pages it had navigated away from. A long integrated session on heavy sites can reach
the limit. The notice names the app and how to raise it.

`scripts/cargo-gate test` and `bench` run under it, inside a held slot too, as do
`scripts/test-lib`, the app in `scripts/run-development`, `--instance` included,
and a `scripts/native-evidence` session. A command already under any memory limit
keeps that one, even a looser one, and `CAGENTS_MEMORY_MAX` then has no effect. A
capped command leaves the caller's cgroup, so a caller's CPU quota stops applying
to it. It doesn't cover outer `cargo-gate test --doc`, `cargo-gate run`, `--hot`,
or a harness that runs Cargo or a test binary itself. Test and bench commands
sent to a build host through `cargo-offload`, including loopback and commands
inside `exec --hold`, carry the same helper and memory/swap settings to the
host. The whole remote Cargo test/bench command, including any compilation and
doctests, runs inside the limit; `--no-run` only compiles and is not capped.
A missing helper refuses the offloaded test rather than silently losing its cap.

A script's `cargo test` under `cargo-gate exec` reaches the gate's `cargo`, so it is
covered, and so is its compile, which runs inside the limit there. A UI test rebuild
peaked at about 11 GiB with page cache, so it fits, but a cold build and an
integrated one are unmeasured.

**Writing a mutation harness:** a mutant can turn a bounded loop into an endless
one that allocates, so run each mutant through `scripts/cargo-gate test` or
`scripts/test-lib`, or put `scripts/memory-capped` in front of the command, and
give it a lower limit than the default if you know what the tests need. Don't use `ulimit -v`: it limits
address space rather than memory, and these binaries reserve far more than they
touch. A `signal: 9` from a mutant is a result to read, and doesn't show that a
test caught it. A test that allocates on purpose needs a ceiling of its own: a
mutant may remove the very limit it was testing, and did.

**Scratch files in agent sessions:** use a fresh `mktemp -d` directory per batch
of work and remove old ones by exact name. Don't run a bare wildcard delete such as
`cd DIR; rm -f *`, or reword one to get past a guard: most of the ones agents ran
stalled with no process and no result, apparently held for an approval nobody could
give, though that mechanism is inferred. Treat a tool call with no process and no
result as a held approval rather than a slow command.

Where no scope can be made, as without a user manager, the command runs without a
limit and says so.

### Time limits for stress and repro workloads

A workload started over SSH outlives the connection: sshd's per-connection
process exits when the client goes, but nothing tells the shell and loop it
started. One probe loop kept FW16's CPUs about 71% busy for 36 hours, and every
build waited behind it. Run a stress loop, a reproduction, a soak or a
measurement under `scripts/bounded`:

```sh
scripts/bounded --ttl 1800 --name terminal-close-probe -- ./probe-loop
```

It stops the workload when the first of these happens:

- its owner ends. That is the nearest SSH connection above it. Failing that, it
  is the process a cagents daemon runs for the session above it, such as an
  agent's provider. Failing both, it is the shell that ran it. `--owner PID`
  names another process.
- `--ttl` seconds pass. The default is 3600.
- the wrapper itself gets TERM, INT or HUP.

It sends TERM to everything the workload started, then KILL after 10 seconds.
That covers every process still in the workload's process group, and any
process that left the group but still carries the workload's `CAGENTS_BOUNDED`
environment mark. When the command exits by itself, whatever it left running
in the background goes too. The exit status is the command's own. It is 124
when `--ttl` stopped the workload, 143 when the owner ended or the wrapper was
stopped, and 127 or 126 when the command could not be run. A stop for any of
those reasons prints why. Clearing up after a command that exited by itself
prints nothing.

`scripts/bounded --list` shows what runs bounded on the host: whose it is, how
long it has left, and how to stop it. If the wrapper itself was killed with
KILL, its workload has lost its watcher. `--list` says so, and
`scripts/bounded --reap` stops what it left.

The workload runs in a session of its own, without the terminal. Ctrl-C stops
it, but it can't prompt on `/dev/tty`, and Ctrl-Z pauses only the wrapper.
Nesting is safe: an inner `scripts/bounded` adds its mark to the outer one's,
so stopping the outer one stops both. Cap memory as well by nesting
`scripts/memory-capped` inside it:
`scripts/bounded --ttl 1800 -- scripts/memory-capped ./probe-loop`.

Three things escape it:

- a process that both left the group and cleared its environment, such as a
  daemon started with `env -i`.
- a setuid or non-dumpable process outside the group, whose environment can't
  be read.
- a connection that dies silently, such as a network drop. Its `sshd-session`
  lives on until SSH's keepalive gives up, so only `--ttl` stops the workload
  in time.

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
its lane. Cover the boundaries a change touches:

```sh
scripts/cargo-gate test --locked --no-default-features --all-targets
scripts/cargo-gate test --locked --no-default-features --features host --all-targets
scripts/cargo-gate test --locked --no-default-features --features ui --all-targets
```

An SSH login has a soft limit of 1024 open files, and whatever it starts, with
`nohup` or `setsid` included, inherits that. A UI test run can exceed it and fail
with `Too many open files`, so run `ulimit -n 65536` first.

The gate prepares HOST fixtures automatically. Before running HOST tests it
builds `cagentsd` in the same admitted build slot, from the same source copy and
Cargo environment, then sets `CAGENTS_TEST_RETAINED_ACTOR_HELPER` to Cargo's exact
executable path. It keeps the test command's profile, target, feature and config
options; with no profile option, the helper uses the test profile. A failed helper
build stops the command before the tests run. CLI and UI lanes do not build it.
This also works for narrow HOST library tests. An explicitly supplied helper
variable is preserved, for qualifications that prepare their own matched binary.

Through the gate, which is how a hosted agent runs it, there is no base to
choose. The build host gives each slot a short, private `TMPDIR` such as
`/run/user/1000/cof/cagents-0`, which is within the socket budget and outside
the home and any `.git`, and which the slot's next command clears. It is in
the runtime directory, which is memory on fw16, unlike the disk-backed base a
local run uses: the slot holds one command's files at a time. If that
directory isn't private to you, the slot falls back to its own `tmp` under
`~/.cache`, which is too long for the socket budget and inside the home, where
the bundled-skill tests below refuse to install.

The ordinary HOST command needs no environment setup:

```sh
scripts/cargo-gate test --locked --no-default-features --features host --all-targets
```

For a local HOST run, with `CAGENTS_BUILD_REMOTE=local`, select an existing
private **disk-backed** base with a short, physical absolute path. Verify its
filesystem with `findmnt -T PATH`; `/tmp`, `/dev/shm` and `XDG_RUNTIME_DIR` can
be RAM-backed. Neither the base nor any ancestor may contain a `.git` file,
directory or symlink. The base must also be outside the account home reported
by `getent passwd "$(id -u)"`, even when `HOME` points elsewhere: bundled-skill
tests deliberately refuse installation beneath the passwd home. A private `~/.cot` therefore does not qualify.

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
(
    export CAGENTS_BUILD_REMOTE=local
    TMPDIR=$(mktemp -d /var/tmp/ct/tXXXXXX) || exit
    export TMPDIR
    printf 'Fixture root (retain until children settle): %s\n' "$TMPDIR"
    scripts/cargo-gate test --locked --no-default-features --features host --all-targets
)
```

Keep the temporary root and any separately retained fixture evidence until their
original children have settled;
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
Cargo-reported executable directly, after its build admission has ended.

`--no-run` only compiles tests. A later direct harness launch, including
`scripts/test-lib host`, must receive its own matched helper, because it runs
outside the gate. Prepare it immediately before that launch, without changing
sources, Cargo environment, target or profile:

```sh
(
    CAGENTS_TEST_RETAINED_ACTOR_HELPER=$(
        scripts/cargo-gate exec /usr/bin/python3 scripts/build-ceditor-product cagentsd --locked \
            --no-default-features --features host --profile test --bin cagentsd
    ) || exit
    export CAGENTS_TEST_RETAINED_ACTOR_HELPER
    scripts/test-lib host --prefix terminal:: --test-threads 1
)
```

Use the same profile option in both commands when overriding it. The gate
translates a supplied helper path inside the checkout or target to its build-host
copy; a path outside both is refused. A matching version alone does not prove
that binaries from two dirty source trees match. CLI and UI focused runs need no
helper:

```sh
scripts/test-lib cli search_match::tests::overlapping_occurrences_keep_the_best_boundary_rank
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

## Shared GitHub traffic

Use the shared, on-demand status cache instead of a watcher per agent:

```sh
cagents gh pr-status 1234 -R CarsonBurke/cagents --json
```

The command requires an explicit repository, needs no daemon, and reads PR,
head, CI, review and merge state in one GraphQL request. Local callers with
the same gh credential context share one refresh per PR every 120 seconds;
when nobody reads it, it makes no requests. Check `data.stale`, `data.error`,
`data.retry_at` and `data.snapshot.pull_request.headRefOid` before acting.
A failed refresh exits nonzero and labels any retained snapshot stale.
Exit zero confirms a readable snapshot; inspect `checks_state` to decide
whether CI passed, failed or is pending.
The aggregate check state covers all contexts; `checks_complete=false` means
the displayed context list stops at 100. No reported checks means unknown,
not passing. Enterprise repositories also accept `--hostname HOST`.

The app and Ceditor share a resumable review-request scan per repository and
credential context. A settled scan waits 60 seconds, and continuation pages
wait at least six seconds. Background monitoring still serves desktop review
alerts; hidden inspector and review panes retain their existing visibility
guards. Private snapshots and resource-aware cooldowns live under
`${XDG_STATE_HOME:-~/.local/state}/cagents/github-status`. Token environment
and gh account/configuration changes select another cache. Rotating a keyring
secret without changing its account/configuration is not detected locally.

To measure a repository agent shell without generating additional requests,
opt that shell into observation explicitly:

```sh
scripts/gh-observe run --real /usr/bin/gh -- bash
# Optional HTTP header/cost profiling for supported JSON operations:
scripts/gh-observe run --http --real /usr/bin/gh -- bash
# Exit that child shell to end observation; the parent PATH is unchanged.
scripts/gh-observe report --since 2026-11-09T23:31:00+00:00 \
    --until 2026-11-10T00:01:00+00:00 > github-traffic.json
```

The observer is measurement only. It never admits, queues, retries or defers
GitHub commands, even when legacy coordination files or environment settings
exist. Request headers are logged as evidence, not used to delay another
request. Do not install it into `~/.local/bin`, change a login shell's PATH,
or route the running app or other projects through it. A child process outside
the selected repository/worktrees executes the real gh without observation.
The passive runtime stays under that repository's Git common directory so
background descendants can keep resolving gh after the launcher exits; only
the opted-in child environment places it on PATH.

The daily JSONL logs are private files under
`${XDG_STATE_HOME:-~/.local/state}/cagents/gh-observe`. They record timestamps,
caller process ancestry/session IDs, subcommands, GraphQL operation shapes,
invocation metadata and, when `--http` supports the operation, HTTP status,
duration and allowlisted rate-limit headers. Credentials,
arguments, GraphQL variables and response bodies are discarded. Operation
names and endpoint paths can identify private repositories; keep raw logs
local and publish only the needed aggregates. Remove old daily logs when the
measurement no longer needs them.

Normal observation execs the real gh with its original arguments, environment,
stdin, stdout, stderr and exit status; it does not enable HTTP debug or filter
output. Logging/setup failures pass through to the real command.

`--http` explicitly enables HTTP metadata profiling only for supported JSON
operations. Raw REST API bodies, diffs, logs, downloads and unknown operations
remain invocation-only and do not enable debug output. The profiler filters
injected debug and never prints request/response bodies. Unexpected opaque or
unframed responses are conservatively suppressed, including ambiguous stderr
inside such dumps, rather than exposed as diagnostics; treat them as coverage
gaps. Explicit debug and interactive stderr retain their original behavior and
also log invocations only. Absolute gh paths, other hosts, connectors and Git's
own HTTPS traffic bypass observation.

Exact GraphQL primary cost is reported only when the original query asks
`rateLimit { cost }`; quota headers are cumulative, so their deltas cannot be
attributed to one concurrent caller. Secondary point estimates use GitHub's
published defaults and are separate from primary cost. Set
`CAGENTS_GH_OBSERVE=0` to bypass observation inside an opted-in shell.

Content serialization is separate from observation. Invoke it only when
intentionally serializing selected writes; it never installs a wrapper or
activates global state:

```sh
scripts/gh-coordinate.py --real /usr/bin/gh --wait 30 -- \
    pr comment 1234 -R CarsonBurke/cagents --body-file comment.txt
```

Only the documented allowlist of PR/issue content commands acquires a slot.
Reads, local helpers and unsupported commands execute immediately. Selected
writes must identify their repository explicitly, share state only through
that repository's Git common directory, and start at least eight seconds apart.
Waiting is bounded; `--wait 0` refuses a busy slot immediately with exit 75 and
`cagents-gh: deferred`, before starting gh. Callers setting
`CAGENTS_GH_NONBLOCKING=1` always use immediate refusal, including when a
positive `--wait` was supplied. A started mutation is never retried
or timed out by the helper. It does not read response headers, transfer
cooldowns between requests, or apply a GraphQL reset to REST. One gh command
may create multiple contents, and independent repositories do not share this
slot; this is not enforcement of an account-wide GitHub quota.

## Native evidence

For which fixture to use, how they fit together and how to extend or repair
one, see [Fixtures](fixtures.md).

`start SIDE --width W --height H` requests both logical viewport dimensions;
the default is 1500x900. The task's private KWin rules size its nested compositor
framebuffer before niri applies width fitting, so a narrow width no longer
reduces the height with it. `pair` forwards `--height` to both starts. A capture
refuses a window larger than its observed logical viewport (allowing two pixels
for scaling round-off); request a larger width/height instead of accepting a
cropped image. This changes the capture viewport, not application layouts.

`scripts/native-evidence` captures before/after screenshots of the actual native
app, from a shell where the desktop session or `DISPLAY` is unavailable, such as
plain SSH. It runs headless: KWin's virtual backend hosts a nested niri session,
driven by `niri-harness`, running `scripts/run-development --browserless`.
Each task keeps everything in one directory outside Git ancestry. Avoid `/tmp`,
`/var/tmp` and `~/.cache`, where Codex sandboxes can leave an empty `.git` that
fixtures refuse.

Build before taking the native lock, and never wait for Cargo while holding it: every other native session queues behind you. A `scripts/run-development` launch under that lock, whether an ancestor has it open or it runs in a nested niri session while the lock is held, won't compile or join the Cargo queue: Cargo may confirm the build is current, a compile fails naming the crate, and after `CAGENTS_NATIVE_LOCK_BUILD_SECONDS` (default 180) the launch gives up. `build` holds one of the build host's [shared slots](#building-on-another-host) for its three builds, even on the build host itself, so it reuses what that slot last compiled and leaves the build output there; the side's target gets only the three executables, about 0.5 GiB. With `CAGENTS_BUILD_REMOTE=local` it builds a full Cargo target in the task instead. A session runs the real `scripts/run-development`, but until the harness reports it ready, shortly after its window appears, its `cargo` only accepts what `build` compiled, with the same arguments. Anything else ends the session at once with the reason and releases the lock, and `start` returns once that is done, so `build` can follow straight away. After that it is real Cargo, so a terminal or language server in the app works; one that runs Cargo before then ends the session. Real Cargo run from inside a ready session still holds the native lock while it works, so keep builds out of a session. A hand-written session needs the same care: prebuild, give the runner a private `CAGENTS_BUILD_LOCK` with `CAGENTS_BUILD_SLOTS=1`, close the lock descriptor in children (`9>&-`), bound its stop command with `timeout`, and run `niri-harness` without a mise shim, which re-executes itself forever under a private `XDG_DATA_HOME`.

```sh
export CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/pr1234-prs
git worktree add --detach .worktrees/my-task-before origin/main

# A shared build slot only; the executables come back to the task.
scripts/native-evidence build before .worktrees/my-task-before
scripts/native-evidence build after .

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
  `gh` always fails unless `$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge`
  exists, which makes it an offline, signed-in github.com with project avatars
  seeded from `$CAGENTS_EVIDENCE_FIXTURE/avatars` (see
  [Fixtures](fixtures.md#an-offline-github)); `curl` always fails, and Git
  reaches only local repositories. Files in `$CAGENTS_EVIDENCE_FIXTURE/home/.claude`
  switch fixture behaviors. `env CMD` runs your own setup in this environment,
  with the fixture's root in `CAGENTS_EVIDENCE_FIXTURE`; between sessions it
  also works, under the last session's side, for setup that needs the daemon
  stopped, such as converting a store. Terminals and agents still enroll in scopes on
  the host's user manager, as development sessions do.
- **Seeded chats reopen.** A chat records its provider home's device and inode
  and won't reopen once that directory is replaced, and a transcript's
  location records its file's. So `start` restores the seed into the existing
  fixture in place, with `rsync --inplace --delete`, instead of recreating it,
  and a chat saved in the seed reopens on both sides. `start` needs rsync
  whenever there is a seed and a fixture. `save-seed` records the provider
  home's identity; while the fixture's differs, as it does once the fixture
  directory is removed or for a seed saved by an older tool, every `start` says
  the seed's chats won't reopen. Recreate them and save the seed again.
  Extended attributes and ACLs are not restored.
- **UI-only restart.** To measure restored chats while their providers keep
  running, close the app window normally, then run `relaunch` in the same
  session. It reuses the recorded executables through the development runner
  and refuses compilation under the native lock. `relaunch` stays in the
  foreground until that UI exits; run measurements from another shell and
  finish with `stop`. This preserves the daemon and live providers, whereas
  `stop` ends them. Add `--chat-stages` to record UI-thread chat stage timings
  under `logs/` on normal close; these measure synchronous work rather than
  compositor presentation. Recreate the same workload on each revision when comparing
  startup; a saved descriptor whose provider was killed is not a live chat.
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
- **File choosers.** `start SIDE --portal gtk` runs xdg-desktop-portal with
  the GTK backend on the session's private bus, so the app's Attach dialog
  opens inside the session, where `drive` can operate it: `drive key ctrl+l`,
  `drive type PATH`, `drive key Return`. `kde`, `gnome` and `cosmic` select
  those backends where they are installed.
- **Who holds the lock.** `scripts/lock-holders /var/tmp/cagents-fw16-native.lock` shows
  the holder and whether it is doing anything, when `start` seems stuck.
- **Cleanup.** `stop` stops the fixture daemon, then ends any process that
  inherited the task's private runtime directory. That path is the proof of
  ownership, so nothing else on the host is touched.
- **Reclaimed builds.** `scripts/prune-build-dirs` removes a task's
  `target-before` and `target-after` once the task has gone unused for 12
  hours, or 1 hour while the disk is low. `build`, `start` and the end of a
  session each count as use of both sides, and it never removes a target that
  a build or session has open or whose executables are running, nor either
  side while any `native-evidence` process names the task, including a session
  still waiting for the native lock. A side whose
  target went says so; build it again, which in a shared slot recompiles only
  what changed since that slot's last build. Screenshots, logs and the fixture
  stay. Tasks outside the pruner's roots, `~/.cache`, `~/.local/state` and
  `/var/tmp`, are not searched. When it compiles on this host, `build` also
  runs `scripts/build-disk-guard`, as a gated build here does, so it starts
  the pruner on its schedule and is refused when the disk is below its floor.

Like `fixture_env`, it exits 77 under inherited agent authority and never removes
that authority, so a hosted agent shell cannot run it locally; it runs it on the
build host instead, as the next section describes. When the PR merges, remove
the task's host side with `scripts/native-evidence --remote fw16 --task DIR
clean`, then the task directory here.

### Native evidence from a cagents session

A terminal or chat that cagents hosts runs `native-evidence` on the build host
over plain SSH. An SSH login there carries none of the session's agent
authority, and the host's own copy of `native-evidence` still refuses any that
reached it. Add `--remote HOST` (or set `CAGENTS_EVIDENCE_REMOTE`); every
command is the same, and the screenshots come back here:

```sh
mkdir -p ~/.local/state/cev-steps
cat >~/.local/state/cev-steps/pr1234-prs.sh <<'STEPS'
evidence cli project open '{fixture}/project'
evidence drive click 'Open repository'
evidence shot channel-prs
STEPS
git fetch origin main
scripts/native-evidence --remote fw16 --task ~/.local/state/cev/pr1234-prs \
    pair --before origin/main --after . --steps ~/.local/state/cev-steps/pr1234-prs.sh --width 1200
ls ~/.local/state/cev/pr1234-prs/shots   # before-channel-prs.webp after-channel-prs.webp manifest.tsv
scripts/native-evidence --remote fw16 --task ~/.local/state/cev/pr1234-prs clean   # once the PR merges
```

`pair` takes minutes: it builds two sides and may wait up to 30 minutes for
the host's native lock while another session runs. Run it in the background,
or with a command timeout of at least an hour. A command killed by a short
tool timeout drops the SSH connection, and that ends the work on the host too.

`pair` builds both sides, then for each starts a session, runs the steps file
on the host with `SIDE` set and `evidence` standing for `native-evidence` there,
stops the session even when a step fails, and brings back the shots. The
individual commands work too, for a session you drive step by step:

```sh
export CAGENTS_EVIDENCE_REMOTE=fw16 CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/pr1234-prs
scripts/native-evidence build before origin/main
scripts/native-evidence build after .
scripts/native-evidence start after --width 1200
scripts/native-evidence drive observe
scripts/native-evidence shot channel-prs    # prints the local path
scripts/native-evidence stop
```

- **Sources.** `build SIDE SOURCE` takes a checkout directory or a revision of
  the repository you run it from. A checkout's uncommitted and untracked files
  are included, ignored ones are not, and the checkout, its index and its
  branch are left alone: the tool records the working tree as a commit, pushes
  it to `refs/native-evidence/TASK/SIDE` in the host's repository (the same path
  as this one, or `CAGENTS_EVIDENCE_REMOTE_REPOSITORY`), and checks it out at
  `.worktrees/cev-TASK-SIDE` there. `sources.tsv` in the task records each
  side's local checkout, its HEAD, and the commit that was built, which is the
  revision `shots/manifest.tsv` names. Every untracked file that isn't
  ignored goes to the host, so make sure a stray file, such as a local `.env`,
  is ignored or removed before you build.
- **One tool.** Both sides run the `native-evidence` and synthetic provider of
  the checkout you run it from, copied to `.worktrees/cev-TASK-tool`, as a local
  run would.
- **The task.** Its directory's name, 1–18 letters, digits, dots, hyphens or
  underscores, names `~/.local/state/cev/NAME` on the host. Locally it holds only
  `shots/` and `sources.tsv`. `where` prints the host's paths. The host
  records which machine and task directory a task belongs to, and refuses
  every command from another, and leaves alone one it didn't start, such as
  a task someone runs on the host directly, so two agents that pick the same
  name don't build, stop or clean each other's work: name the task after your
  PR, such as `pr2282-reopen`. Only `build` and `pair` take a name on the
  host; `status`, `fetch` and the rest never claim one. A task an earlier
  version of the route started has no record: the refusal prints the one-line
  command that claims it, to run on the host if the task is yours. On the
  build host itself, run `scripts/native-evidence` without `--remote`; a task
  directory that is the host's own is refused.
- **Fixture paths.** `{fixture}` in `cli` and `env` arguments becomes the
  fixture's root on the host, both on the command line here and in a steps
  file. A path you type into a file chooser is a host path, so put the file to
  attach under the fixture first, as in `evidence env sh -c 'cp ... "$CAGENTS_EVIDENCE_FIXTURE/project/"'`.
- **Cleaning up.** `clean` stops the task's session, if any, and removes its
  checkouts, the refs that pinned their commits, and its task directory from
  the host. The shots here stay. If the session won't stop, `clean` removes
  nothing and says so. A side can't be rebuilt while its session
  runs, so the checkout is never rewritten under it. Stop the session first.
- **Login shells.** Every script travels to `bash -s` on standard input, so the
  host's login shell may be fish. Commands get no standard input from here.

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
after staging; finish later with `cagents daemon upgrade`. The staged build
always supplies the updater. A source daemon whose protocol is too old needs
an intermediate build first.

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

## Rehearsing an update

`scripts/rehearse-update` runs installed archives in a disposable user environment.
It measures what an update does to two windows, their saved panes, channels,
agents, chats, terminals and a long-lived terminal child. It never builds or
changes product code. Use a fresh, absent root outside a checkout, under
`/var/tmp`, `/tmp` or `/dev/shm`, at most 40 bytes long, such as `/var/tmp/cr/u1`.
`init` records its absolute path in `.rehearse-update`; `cleanup` requires that
marker to match the root before removing it.

Run from a genuine user shell. A hosted agent uses plain `ssh fw16`; never
remove or forward `CAGENTS_AGENT_AUTHORITY`, `CAGENTS_AGENT_TOKEN` or
`CAGENTS_SESSION_TOKEN` to bypass the harness's refusal. On fw16, fetch the
branch and use your own worktree under the repository's `.worktrees/`.

Each input directory must contain
`cagents-VERSION-x86_64-unknown-linux-gnu.tar.gz` and `SHA256SUMS`. Supply the
matching revision's `install.sh`, for example from `git show SHA:install.sh`.
The archive bytes, rather than the harness checkout, select the tested code.

```sh
scripts/rehearse-update --root /var/tmp/cr/u1 run \
  --old-dir /var/tmp/cagents-rehearsal/builds/0.1.3+local.991daa43e \
  --old-version 0.1.3+local.991daa43e \
  --old-installer /var/tmp/cagents-rehearsal/builds/0.1.3+local.991daa43e/install.sh \
  --new-dir /var/tmp/cagents-rehearsal/builds/0.1.3+local.d25093651 \
  --new-version 0.1.3+local.d25093651 \
  --new-installer /var/tmp/cagents-rehearsal/builds/0.1.3+local.d25093651/install.sh \
  --answer yes
```

The scenario installs OLD, starts a KWin virtual desktop with nested niri,
opens the main and `second` windows, seeds offline work, records and captures
both windows, stages NEW, and invokes the staged CLI's `daemon upgrade` on a
controlling pseudo-terminal. `--answer` responds to each observed `[Y/n]`
confirmation; `--yes` instead passes the updater's approval flag. `--working`
is typed at the choice offered while agents are mid-turn (empty presses Enter,
which waits for them; `i` interrupts them). `--stop` is typed at the destructive
stop list; it defaults to empty, which cancels, so a rehearsal stops panes only
with `--stop stop`, as a person would type. Extra upgrade flags use
`--upgrade-arg=--interrupt`; the harness adds no policy automatically. Refusals are measured outcomes: even
a deliberately expected refusal, such as `--uncarryable`, reports `Outcome: FAIL`.

Add `--ceditor /var/tmp/cagents-rehearsal/builds/ceditor-07e7efc/bin` to launch
that build with its matched `cagents` and `cagentsd`, in the same private runtime.
The scenario verifies its mapped window too. The default seed includes Claude
chat and console agents, an offline Codex app-server, two untitled conversations
in one project, a channel containing ` Details: `, a shell and `sleep 86400`.
Use `--untitled 0|1|2`, `--no-codex`, `--no-console` and `--no-child` to vary that baseline.

Use `--codex-pause 90` to keep the offline Codex turn active at the update
prompt. `--codex-never-confirm-interrupt` acknowledges interruption without a
completion receipt; `--codex-never-confirm-close` retains the connection to
exercise Prepared recovery. These options configure the same fake peer through
the private root's `codex-fixture.json`, restoring its `CAGENTS_TOOL_ROWS_*`
settings after native launches filter the caller's environment. Accepted turn
notifications and interruption requests are recorded in
`logs/codex-provider.jsonl` with their thread and turn identities.

Use `--expect-refusal interrupt` with the unconfirmed-interrupt peer, or
`--expect-refusal cancel --answer no` to decline the review. These scenarios
pass only when the exact expected refusal preserves OLD's installed version,
daemon process, windows and inventory. The interrupt scenario also checks its
30-second settlement bound and absence of an interrupted receipt.
`--require-prepared` records the checkpoint transitions and requires this job's
source proof in Prepared. `--plain-restart` then exercises an ordinary daemon
start after recovery: it pins only the private daemon by its socket peer,
executable, namespace and lock, simulates a crash with a pidfd, and verifies the
original census again through the public command. Both windows must reopen the
paused history and console without starting provider work. The original
recovered views and records remain under `recovered/`.

Optional problem states each have their own switch:

- `--no-init`: a Claude peer that accepts work without reporting a session ID.
- `--exit-mid-turn`: a Claude peer that exits without a completion receipt.
- `--ignore-term`: a terminal job that ignores SIGTERM.
- `--uncarryable`: a live job plus OLD's `prepare-upgrade --yes
  --no-relaunch-agents` probe. Local jobs normally carry, so this option
  temporarily replaces the private holder registry directory with a file,
  retaining the directory beside it. This makes preservation unavailable and
  exercises the active-job refusal. `stop` restores the directory before
  cleanup. If preparation unexpectedly succeeds, it can stop the private
  daemon; the report identifies that outcome.
- `--unreachable`: SIGSTOP the selected terminal's private holder.
- `--fail-new-daemon`: after staging, replace only the private candidate's daemon
  entrypoint with a script that exits 1 on normal startup. Informational,
  source-inspection and store-maintenance commands still use the candidate's
  real binary. The scenario requires the worker to report this failure, restore
  OLD as `current`, serve daemon requests again, reopen OLD's windows and retain
  the seeded work. The fault and original binary digests are saved in
  `logs/startup-fault.txt`; `logs/daemon-start-failure.txt` records actual
  execution. This option never modifies either input archive. Use `--no-console`
  to test rollback with ordinary terminals. With a managed console, add
  `--expect-console-refusal` instead: the scenario requires a refusal before
  startup, no execution receipt, the original daemon serving and all work intact.

A seeded state does not guarantee a particular refusal on every revision.
`seeds.json` preserves spawn results, OLD's actual inventories, verification
failures and the preparation probe. Retained Claude actors can carry work
without an exact resume ID on builds that support that protocol.

The root owns HOME, XDG directories, XDG_RUNTIME_DIR, CLAUDE_CONFIG_DIR and
CODEX_HOME. Fakes precede every real provider on PATH. This is environment and
fixture isolation, not an OS network sandbox. The installed daemon and editor
service use that private namespace. Terminals and holders deliberately register
transient scopes with the real systemd user manager for update fidelity, just
as the development runner does: `src/daemon/process_scope.rs` and
`src/upgrade.rs` connect to the hard-coded `/run/user/UID/systemd/private`.
Before killing private processes, `stop` attributes their cgroup scope names
using their private HOME or XDG_RUNTIME_DIR. It then checks the real user
manager's running scopes and fails if any attributed scope survives. It never
stops a unit it did not attribute to this root.

The desktop holds
`/var/tmp/cagents-fw16-native.lock` (override with
`CAGENTS_EVIDENCE_NATIVE_LOCK`), shared with native evidence work. `desktop start`
waits in that lock's queue for as long as it takes, reporting every five minutes;
only the desktop's startup after it takes the lock is bounded (five minutes), so a
busy host delays a rehearsal instead of failing it. `run` stops its desktop and all
private processes on completion; keep manual sessions short.
Do not run the harness inside `native-evidence`: both acquire the same native
lock, so the inner invocation would wait on its caller.

`report.txt` contains step durations, the complete update transcript, prompts,
refusals, comparison changes and seed verification. `records/before/` and
`records/after/` contain CLI envelopes and stable `snapshot.json` inventories,
including read-only SQLite/WAL card and terminal recipe rows for each window.
`shots/` contains a WebP for each mapped window before and after. A missing
inventory is an evidence failure, not proof that no items existed. A nonzero
exit means an update refusal, lost/changed state, or a harness/evidence failure;
read the report to distinguish them. Healthy completion and chat resumption
are listed as changes without being counted as losses.

For a manual investigation, compose the same commands:

```sh
scripts/rehearse-update --root /var/tmp/cr/u1 status
scripts/rehearse-update --root /var/tmp/cr/u1 compare before after
# On a manually initialized/installed root:
scripts/rehearse-update --root /var/tmp/cr/u2 desktop start --limit 300
scripts/rehearse-update --root /var/tmp/cr/u2 launch main
scripts/rehearse-update --root /var/tmp/cr/u2 launch second
scripts/rehearse-update --root /var/tmp/cr/u2 seed --no-init
scripts/rehearse-update --root /var/tmp/cr/u2 record before
scripts/rehearse-update --root /var/tmp/cr/u2 drive --json observe --no-text
scripts/rehearse-update --root /var/tmp/cr/u2 desktop stop
scripts/rehearse-update --root /var/tmp/cr/u2 cleanup
```

`env CMD...` runs inside the private user; `app CMD...` also receives the nested
display. `ceditor start BIN_DIR`, `record NAME`, `shot NAME` and `update DIR
VERSION INSTALLER --answer yes` are available separately. `stop` retains evidence;
`cleanup` stops everything and removes the marked root. Copy reports and images
elsewhere before cleanup. Run the pure checks with
`/usr/bin/python3 tests/rehearse_update.py`; no compositor or Rust build is needed.

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
