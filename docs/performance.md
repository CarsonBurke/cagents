# Performance measurements

- [Redraw scheduling](#redraw-scheduling)
- [Activity spinner cost](#activity-spinner-cost)
- [Idle UI wakeups](#idle-ui-wakeups)
- [Native chat stage attribution](#native-chat-stage-attribution)
- [Pane open](#pane-open)
- [Terminal pipeline](#terminal-pipeline)
- [Terminal search](#terminal-search)
- [Busy terminals and the update tail](#busy-terminals-and-the-update-tail)
- [Workspace model edits](#workspace-model-edits)
- [Native chat viewport](#native-chat-viewport)
- [Idle retained actors](#idle-retained-actors)
- [Work board polling](#work-board-polling)
- [Git watcher during builds](#git-watcher-during-builds)
- [Reference results](#reference-results)

These workloads are opt-in, local, and non-gating. Never compare unoptimized
`dev` or partially optimized `dev-runtime` results with the release baselines
below.

`dev-runtime` optimizes dependencies only: `[profile.dev-runtime.package."*"]`
does not cover the workspace's own crates, so `cagents`, `cagents-ui` and the
other workspace members build unoptimized, and a native measurement of
application code roughly doubles. Build a native measurement with
`CARGO_PROFILE_DEV_RUNTIME_OPT_LEVEL=2`, which `scripts/native-evidence` and
the build gate forward to the build host, and check that Cargo reports
`[optimized + debuginfo]`. Debug assertions stay on in that build except in
`wgpu-core`; confirm a renderer or allocator cost on a release build before
acting on it.

## Redraw scheduling

```sh
sh scripts/run-redraw-latency /tmp/cagents-redraw-reference
```

The output directory must not already exist. Relative paths are resolved from
the directory where the script is invoked. This `latency-harness` lane builds
the CLI, host, and browserless UI separately in **release** mode, then runs five
fresh UI processes with private HOME and XDG directories. Each run walks eight
synthetic unloaded cards with a fixed seed: 32 warmups and 256 recorded
adjacent focus changes. No user sessions or PTYs are loaded. It requires a
graphical session, `jq`, `timeout`, and the normal native release
prerequisites.

The metric is **requested state committed to first matching-view
RedrawRequested**. It measures redraw scheduling latency, not renderer
submission, compositor presentation, scanout, or arbitrary input-to-visible
latency. The recorder currently samples `card_focus`. The command palette is
available in the application; recording its `palette_open` and `palette_query`
actions is planned. Superseded or unfinished generations settle as abandoned,
never as a successful redraw.

The run emits private raw JSON, median/p95/p99/worst summaries, a
reference-machine manifest, and the strict sample schema
(`fixtures/redraw-scheduling.schema.json`). Reports allowlist revision, dirty
state, fixture and build mode, session and compositor, scale, numeric hardware
facts, and local interaction sequences and durations. They contain no user
paths, titles, URLs, query text, terminal contents, user identifiers, or stable
entity IDs. The disabled recorder is an explicit compile-time no-op: it reads no
clock and allocates nothing. Provisional **50 ms p95 / 100 ms p99** budgets stay
non-gating until a named reference baseline is accepted.

`sh scripts/run-redraw-latency --scroll` runs a separate deterministic,
in-process terminal scroll workload (no compositor, daemon, or release build).
It reports what one viewport scroll command costs, and so what applying one
coalesced move per frame saves over applying every wheel notch.

## Activity spinner cost

A visible activity spinner turns every display frame while the main window is
focused (`SPINNER_ANIMATION` in `crates/cagents-ui/src/ui_controls/gauge.rs`).
It does not redraw the window to do it: on its redraw event it asks for
animation frames, and the window presents its last drawing again, turned by
the renderer's clock, until something else needs the widgets drawn. See
[What a spinner frame costs](#what-a-spinner-frame-costs). An unfocused window
may still be in view, so its spinners keep turning, but slower: one frame
every `SPINNER_BACKGROUND_FRAME` (500 ms), turning the arc thirty degrees,
`SPINNER_BACKGROUND_STEP`. Measure the UI process with one busy agent on
screen:

1. Run `scripts/run-development --browserless` without `--debug` in a nested
   session, using the isolated environment from
   [chat-composer-fixture.md](chat-composer-fixture.md). The fixture's first
   turn stays busy, so its channel shows sidebar, tab and footer spinners
   without a real model.
2. Open the busy channel, leave the pointer still, and read `utime + stime`
   from `/proc/<ui-pid>/stat` across a 20 s interval. Also read the main
   thread's `voluntary_ctxt_switches` from `/proc/<ui-pid>/task/<ui-pid>/status`.
3. Repeat with another window focused, then with the agent interrupted for the
   idle floor.

Local reference: Ryzen 7 7840HS, Radeon RX 7600S, `dev-runtime` build, nested
niri 26.04 inside a headless KWin 6.4 virtual output, 1596x929, scale 1.0.
This was measured when the focused cadence was 100 ms, before the frame-cost
changes in the next section. Each row reports 20 s samples, as a percentage
of one core:

| State | Before (`c07b6f78e`) | After |
| --- | --- | --- |
| One busy agent, window focused | 11.1-12.6% (73-86 wakeups/s) | 4.1-4.2% (30-34 wakeups/s) |
| One busy agent, another window focused | 9.1-9.5% (79 wakeups/s) | 1.15-1.25% (5-6 wakeups/s) |
| Agent interrupted, window focused | not measured | 1.6% (7 wakeups/s) |

Two changes produced the focused result. At the 100 ms cadence alone, the UI
still used 11.8% of a core because Iced's event loop kept a deadline that had
already elapsed. After a redraw scheduled a later deadline, the event loop
polled with a zero timeout until that deadline arrived (about 630
`epoll_pwait` calls per second). The vendored `iced-winit` now replaces an
elapsed deadline with the requested one; see `vendor/iced-winit/README.cagents.md`.
Each remaining step still repaints the whole window, which is why a busy
agent still costs about 3% of a core above idle in this build. Slowing
unfocused spinners instead of stopping them costs little: an earlier revision
that stopped them entirely measured 1.0-1.6% with another window focused
(3 wakeups/s).

### What a spinner frame costs

A spinner asks its window for animation frames (`Shell::request_animation`)
and draws itself as an SVG that spins from an instant with a period
(`Svg::spin`). While nothing else has changed since the window last drew its
widgets, iced-winit answers such a frame without updating or drawing the
widget tree, and iced_wgpu presents the layers it already has: it prepares
only the image layers holding a spinning SVG, at the frame's instant, and
renders. Anything else that needs a drawing, an event, a rebuild, a layout
or a timed redraw, draws the window as before, and the spinners renew their
requests on its redraw event. The vendored crates' READMEs (`iced-core`,
`iced-runtime`, `iced-winit`, `iced-graphics`, `iced-wgpu`) describe each
part (#3581).

Before that, every spinner step redrew the whole window: the redraw event and
drawing walked the whole widget tree, and the present prepared every layer.
Two changes had made that cheaper (#3564): iced_wgpu keeps a text group's
prepared glyphs while it draws the same text in the same place (#3571), and a
sidebar row looks for focus only after an event that may move it (#3575).
Even so, a step cost about 3-4 ms of the UI thread, which held the cadence at
50 ms, twenty frames a second.

Workload: fw16 (Ryzen 7 7840HS), nested KWin at 1500x900, scale 1. The
`realistic` seed, with four fixture Claude agents working in one channel, both
chat panes showing long transcripts and the pointer still. Every build is
`dev-runtime` at opt-level 2 with frame pointers; how the application crates
were raised to it is not recorded (see the top of this page). Each row is one run: 30 s of
`/proc` CPU time for the UI's main thread, plus the ranges of the
`CAGENTS_UI_TRACE` frame lines (5 s windows) from the same run. fw16 was
shared and loaded (load average 13-21 on 16 threads), so compare rows taken
back to back rather than with older tables:

| Build | Window | Frames | UI main thread | Frame work p50 | Frame work p95 | Interval p95 |
| --- | --- | --- | --- | --- | --- | --- |
| `baad545db`, 50 ms steps | focused | 20/s | 8.5% | 3.0-3.5 ms | 5.9-17 ms | 51-55 ms |
| animation frames only | focused | 60/s | 9.6% | 1.1-1.3 ms | 1.9-3.3 ms | 18-20 ms |
| animation frames and retained layers | focused | 60/s | 7.3% | 0.71-0.81 ms | 0.8-1.9 ms | 17.5-19 ms |
| `baad545db` | another window focused | 2/s | 1.9% | 5-13 ms | 15-16 ms | 501 ms |
| animation frames and retained layers | another window focused | 2/s | 1.7% | 2.6-3.1 ms | 16-25 ms | 509-518 ms |

So spinners turn three times as often for less of the UI thread than before.
The frames are paced by the compositor's frame callbacks (16.7 ms at 60 Hz).
Frame work p99 stays at 12-16 ms because of the full draws in the window,
about one or two a second from the chat's clock and the agents' output,
which cost what they did before. A spinner-only frame now costs about
0.7 ms, most of it encoding a render pass over every layer of the window:
wgpu has no partial present, and keeping the rest of the window in an
offscreen texture to composite spinners over is not done. An unfocused
window keeps its 500 ms cadence; with the window hidden behind another one,
the compositor sends no frame callbacks and the app drew once a second at
1.5% of a core.

The software renderer (iced_tiny_skia) is not vendored and ignores
`Svg::spin`, so under it a spinner turns only when the window draws its
widgets, while the window still asks for a frame every display refresh. An
accessible window finds its focused widget, an operation, before it draws,
so it can still present between draws.

## Idle UI wakeups

A focused text cursor blinks for ten seconds after the last input, then rests
visible and asks for no more frames, following GTK's
`gtk-cursor-blink-timeout`. Every cursor shares that schedule:
`iced_core::text::Blink` in `vendor/iced-core`, used by iced's text editor and
text input (`vendor/iced-widget`), libcosmic's text input, the app's code editor
and the terminal. Before, each focused cursor repainted the whole window every
500 ms for as long as it held focus, and the chat composer holds focus after a
chat opens or a message is sent.

Where the desktop states its own blink through the settings portal
(`org.gnome.desktop.interface` on GNOME, GTK and COSMIC), cursors use its
interval and timeout instead, and follow changes while the app runs. With
blinking turned off, every cursor is steady and asks for no frames, including a
terminal whose program requested a blinking cursor.

To measure it, seed a fixture with 30 settled chats, start a session with
`scripts/native-evidence start SIDE --ui-trace`, spawn three agents whose
first turn stays busy, then interrupt them. Leave the composer focused and the
pointer still. Sample each UI thread's `voluntary_ctxt_switches` and
`schedstat` CPU time from `/proc/PID/task/*` over 60 s, 30 s after spawning
and 20 s after interrupting. The `ui-trace` summaries give the redraw count and
time per five-second window.

Local reference: fw16 (Ryzen 7 7840HS), `dev-runtime` build, headless nested
session at 1500x900. Before is `374370965`; after is `2aec4b05c`. CPU is a
percentage of one core:

| State | Before | After |
| --- | --- | --- |
| Agents settled, composer focused: UI thread | 7.0 wakeups/s, 2.0% | 1.7 wakeups/s, 1.1% |
| Agents settled, composer focused: redraws | 2.7/s | 0.8/s |
| Agents settled, composer focused: clipboard thread | 6.2 wakeups/s | 2.2 wakeups/s |
| Three agents working: UI thread | 26.4 wakeups/s, 6.1% | 23.5 wakeups/s, 5.5% |

The after build also has the wgpu-core change below, which makes every frame
cheaper, so the CPU columns credit both changes. The wakeups and redraws are
the cursor's alone. With agents working, the spinners then asked for ten
frames a second, so the cursor's frames made little difference there. Neither side logged a UI-thread
call over 50 ms after startup. The remaining settled redraws follow the app's
own updates.

`dev-runtime` builds also turn off wgpu-core's debug assertions. With them,
WGPU captured a backtrace whenever it took its snatch lock, on every frame, and
the runner's `RUST_BACKTRACE=1` made that a real stack walk: about 16% of the
UI thread's profile samples with three agents working. Release builds never
paid it. The CPU difference in the working row is within the noise of the
shared build host.

## UI thread trace

Development UI builds started with `CAGENTS_UI_TRACE` set log, every five
seconds, how much wall time the UI thread spent in `update` (by the message's
outer two variant names, such as `ChatUi(Stream`), `view`, `subscription`, and
the window content's layout, draw, event handling (by event kind), tree diff,
widget operations, accessibility tree and overlay collection. Beneath each
total, the sidebar's and the workspace's shares are broken out, and within
the workspace each pane's share by kind, such as `pane chat` or
`pane terminal`. `view` is also split into `sidebar`, `workspace`, `chrome`
and `bars and overlays`, and `subscription` into the application's domain
subscriptions, such as `chat` or `conversation pull request`. Each call that
takes 50 ms or more is logged as it returns, as `ui-trace slow PHASE LABEL MS`.
A nested `update` counts toward the outermost one. Overlays' own layout and
drawing, the libcosmic chrome around the application view, and the renderer's
presentation run outside the measured root, so the thread's CPU time exceeds
the traced total.
The `Projection` phase breaks out the share of `update` spent on its
projection tail, without adding to any total. A label such as
`Full Sidebar(OpenChannel` or `Unchanged ChatUi(Wake` names the projection an
update took and its message; `Camera` is the full projection's panning path,
and `Postlude` the work every update ends with. `stage chrome`,
`stage sidebar_channels` and the other `stage` labels time the tail's stages
across all updates.
Each five-second report also covers the frames the shell presented, on any of
its surfaces: how many, the p50/p95/p99/max **work** of each, and the same
quantiles of the **interval** between consecutive presentations, as
`ui-trace   Frames n=N work ...` and `ui-trace   Frames interval ...`. A
frame's work is the wall time the shell spent handling events since it
presented the previous frame, up to presenting this one: the updates, view,
layout, drawing and presentation it took, but not time spent waiting for
events, nor winit's own dispatch outside the shell's event handler. The first
frame also carries startup, including creating the compositor. The interval
shows pacing against the display's refresh only while the UI draws
continuously, as during streaming or an animation: the shell draws on demand,
so the first frame after an idle stretch reports that stretch as its interval.
Popups, menus and tooltips are surfaces of their own, so presenting one beside
the main window adds a short interval and splits the work between the two. A
window with no frames and no other traced work prints nothing, so an idle UI
that draws nothing stays silent. The shell reports frames through
`iced_runtime::frame`, a hook in the vendored runtime; see
`vendor/iced-winit/README.cagents.md`.
Beneath the frame lines, `ui-trace   Present STAGE n=N ...` gives the same
quantiles for presenting each frame: `total` wall time, the UI thread's `cpu`
time over it, and its stages, `acquire` (waiting for a surface image),
`prepare` (uploading layer data), `render` (recording the render pass, which
wgpu only queues), `trim` (releasing caches and atlas space the frame no
longer uses, on a full draw), `finish` (wgpu validating and encoding the queued passes
into a command buffer), `submit` (handing it to the GPU queue) and `swap`
(handing the image back), and `Present layers` how many layers each frame
drew. Where `cpu` falls short of `total`, the thread was blocked, as on the
GPU or the compositor, or preempted on a loaded host; see
`vendor/iced-graphics/README.cagents.md`.
Each key press, not counting repeats, logs `ui-trace latency key press MS`,
the time from the shell handing the key to the window until the next frame
is presented. A key that changes nothing visible reports the wait for
whatever frame comes next, so read it for keys that type or move.
`scripts/native-evidence start SIDE --ui-trace` sets the variable for a session;
the lines go to its `logs/ui-SIDE.log`. Without the variable a scope reads one
flag, and other builds compile the trace out.

Use it to tell which work keeps the UI thread busy when the app stalls. Pair it
with `/proc/PID/task/PID/syscall` sampling to see whether the thread is ever
blocked rather than busy: a thread in `epoll_pwait` is idle, while `futex`,
`read` or `wait4` on the UI thread is a stall that no amount of view caching
fixes.

## Native chat stage attribution

Development UI builds can record bounded, content-free **inclusive wall time**
for selected chat stages. Set `CAGENTS_CHAT_STAGE_OUTPUT` before startup to a
fresh absolute filename in an existing directory owned by the effective user
with mode `0700`. Use a dedicated private fixture and the normal development
runner; the variable does not grant a build or native testing slot. The runner
selects the `development` feature explicitly, including in `dev-runtime`;
`debug_assertions` does not control this recorder. Ordinary builds ignore the
variable and compile the recording path out. Development builds with the variable
unset keep an inert hot path with no clock read or allocation.

Startup reads the setting once and retains the private parent directory FD.
Normal application close writes an exclusive `0600` file relative to that FD,
without replacing an existing file or following a leaf symlink. A later rename
of the parent does not redirect output to a replacement directory. Output is
not written from `Drop`, per frame, or by a timer. A failed or abnormal close
can leave no report or a partial report; do not interpret it as zero work.

Schema `1`, clock `1` means Linux `CLOCK_MONOTONIC` nanoseconds. The report has
fixed stage indices, numeric counts and timing fields; it contains no chat text,
paths, entity IDs or event payloads:

| Index | Stage | Measured caller |
| --- | --- | --- |
| 0 | Refresh | `Runtime::refresh`, including its early returns |
| 1 | Parent preparation | Parent `prepare_transcript_cache` in refresh |
| 2 | Child preparation | `prepare_child_cache`, including empty selection |
| 3 | Parse | Markdown source advance and content append/restart |
| 4 | Highlight | Fence scan and syntax-block update |
| 5 | Turn build | Actual `turn_part_view` construction of one row of an exchange, including retained cache misses |
| 6 | Shape | Cold selectable-prose `ParagraphBuffer::with_spans` |
| 7 | Retained clone | Cached layout `Node::clone` on a retained layout hit |

These hooks cover named synchronous stages, not every text renderer, widget
layout, event, GPU operation or application task. A quiet cache hit can have no
turn construction or shaping calls. Missing stage time is not evidence that the
remaining work costs nothing.

The recorder retains at most 1024 one-second buckets relative to
`origin_monotonic_ns`. Elapsed spans are split by overlap across bucket
boundaries, with a loop bounded by that capacity. Started and ended endpoints
belong to their own half-open buckets. `crossing_calls` marks a span touching a
bucket without being wholly contained in it; an endpoint at the right boundary
belongs to the next bucket. Per-bucket maxima describe overlap in that bucket;
summary maxima describe whole calls. Summary counts, wall time, clock failures
and outside-horizon values saturate. Unmatched endpoints or failed clocks must
remain visible in analysis.

Join complete bucket ranges to the fixture's actual emission timestamps and
the sampler's actual read timestamps. Do not assign an entire cross-boundary
span to its finishing bucket. Nested stages overlap: refresh includes parsing
and highlighting. Their totals are not exclusive
time and cannot be summed into CPU utilization. Wall time includes scheduler
delay; these are not on-CPU stacks, input latency or presentation timestamps.

Before claiming a CPU improvement, compare fresh identical-corpus cases using
the same optimized instrumentable binary with the variable unset and set.
Keep source, provider rate, selected scope, focus preparation, observer actions
and process identity receipts fixed. Record sampler overhead and ambient
pressure; repeat controls separately when needed to measure variance. Source
tests and headless `frame_cost` workloads do not replace native measurements,
selection/Copy/accessibility validation, or complete history preservation.

The focused recorder tests require the UI lane; include `development` for
output-custody tests:

```sh
cargo test --locked --no-default-features --features development,ui --lib performance::chat_stage::recording::tests
```

Run builds and tests only through the repository's serialized admission process.
This command is a test reference, not an automatic workload or a current
performance qualification.

## Pane open

```sh
RUST_LOG=cagents::pane_open=trace scripts/run-development --browserless --debug --log-file /var/tmp/cagents-pane-open.log
```

The `cagents::pane_open` target timestamps one pane's way onto the screen in
the explicitly captured local debug log, keyed by card: `requested` when the card is created,
`start_dispatched` when its terminal start is handed to the daemon,
`recovered` and `attached` on the blocking start, `started` when the UI holds
the terminal surface, and `visibility` for each change of mounting class.
`wake` (trace level) marks every terminal frame the UI drains, so the first one
after `started` is the first content and the last one is the settled prompt.

A new pane mounts in the update that creates it, at its final width, and is
revealed through its animating slot; nothing waits for the layout transition.
Local reference on a Ryzen 7 7840HS under a headless niri with software
rendering, `dev-runtime` build, 16 terminal opens per side in a project
worktree, alternating baseline and change:

| Milestone | Before | After |
| --- | --- | --- |
| Pane contents mounted | 249–340 ms | 2–4 ms |
| Terminal surface in the UI | 46–124 ms | 54–85 ms |
| First terminal frame the UI drains | 49–135 ms, into an unmounted pane | 57–89 ms |
| Last frame of the opening prompt | 397–549 ms | 100–231 ms |

These are scheduling timestamps from one process, not presentation times.

## Terminal pipeline

The daemon delivers live and completed terminals' semantic events on one shared
`terminal-event-notify` thread. Registering a terminal replays its startup events
before later metadata and completion events; callbacks never run under the
publishing worker's locks. This removes the per-terminal `cagentsd-events-*`
monitor. Holder-control reports also use one `terminal-io` reactor thread.
Each watcher retains its authenticated original socket and root pidfd; a partial
frame or idle holder cannot block other watchers. Reads use per-call
`MSG_DONTWAIT`, preserving the control writer's blocking descriptor flags.
PTY parsing, PTY readers, and UI socket transport still have separate thread
costs; these changes alone do not meet a total idle-plus-one-thread-per-terminal
budget.

For multi-terminal memory comparisons, report daemon and UI RSS separately
and use PSS for holder processes: their executable mappings are shared, so
summing holder RSS counts those shared pages repeatedly. Record thread counts
alongside RSS/PSS and input p99 on the same workload and build profile.

```sh
scripts/benchmark-terminal all
# Use only a dedicated test daemon: the fixture creates terminal descriptors.
scripts/benchmark-terminal all --socket /absolute/private/runtime/cagentsd.sock
RUST_LOG=cagents::terminal_pipeline=trace scripts/benchmark-terminal input
```

`scripts/benchmark-terminal` runs the `terminal-workload` example in release
mode with the `host` feature. The workloads cover idle, burst, short-line,
sustained, full-screen ANSI, large/resized, and four-terminal output, plus
single- and multi-terminal input probes. Default mode uses real PTYs in process;
`--socket` uses the matched daemon and the production hosted IPC connection.
Both drive synthetic 60 Hz snapshot acknowledgements. Daemon mode retains exited
descriptors, so never point it at a personal or production daemon.
The script resolves relative socket and trace paths from the calling directory.

Budgets:

- Input to snapshot: **50 ms p95 / 100 ms p99**. JSON records the measured
  percentiles and whether they pass. This is not an input-to-present claim.
- Throughput (initial, non-gating): **50 MiB/s aggregate** for conventional
  sustained single- and four-terminal output, and **5 seconds** for the 32 MiB
  pathological short-line workload. JSON includes emitted bytes, throughput,
  and pass/fail; process startup and final-state observation are included in
  wall time.

How the pipeline stays cheap: snapshot capture consumes generation-matched draw
credit rather than capturing every PTY batch. Parsing and semantic events
continue without credit, dirty state is retained, and EOF publishes final
contents. Idle terminals do not request a continuous redraw loop. Long-lived
hosted and daemon IPC connections reuse frame buffers.

Opt-in `cagents::terminal_pipeline=trace` records parse/capture, encode/decode,
UI apply, and draw stages where exercised; tracing overhead is excluded from
release throughput claims. The daemon also records `redraw_host_lock`: how long
each redraw broadcast that queued a frame held the host lock, summed over its
holds. It does not include time spent waiting for the lock. The daemon prints
these stages only with `CAGENTS_LOCAL_DEBUG=1` and `RUST_LOG` set. Summarize a
collected log with:

```sh
target/release/examples/terminal-workload --summarize-trace /path/to/trace
```

Missing stages are reported, not estimated. Physical scanout remains
unmeasured.

`terminal-workload --emit visual-input` runs only the output generator, for use
inside the native UI: it accepts `r`, `g`, and `b` to repaint with distinct RGB
colors, and `q` exits.

## Terminal search

```sh
cargo test --release --locked --no-default-features --features host --lib \
  benchmark_ -- --ignored --nocapture --test-threads=1
```

These opt-in owner-thread workloads print five raw durations and their median.
Matcher-only cases cover ASCII misses, dense case-insensitive hits, a long
repetitive wrapped prefix without a match, Unicode case folding, Unicode
misses, and partial case-fold expansion misses. The live
case includes Ghostty formatting and cell lookup; the completed-history case
uses the production 8 MiB archive budget. Terminal/history setup is outside
timing, and each timed sample includes result creation and disposal. They do
not measure UI latency or compositor presentation.

Misses scan all retained rows. Dense hits stop at the 10,001st match, returning
10,000 results with the truncation flag; they do not scan the entire fixture.
Both live and completed cases print their actual retained row counts and assert
minimum fixture sizes and expected results. Compare each workload against its
own baseline, since the archive budget can retain fewer rows than live history.

The matcher uses substring search to skip nonmatching spans while preserving
whole-source-character boundaries for Unicode case-fold expansions and greedy
non-overlapping matches. Live ASCII formatter rows append and lowercase in
bulk. Unicode cells retain their emulator-derived column mapping; completed
rows preserve their gaps and cell widths. Boundary checks precede substring
search so impossible expansion candidates do not repeatedly rebuild its searcher.

Local reference on a Ryzen 9 9900X, release mode, against `9772b55`: three
alternating baseline/optimized rounds with five samples each, pinned to CPU 23.
These are pooled medians on a shared host, not performance budgets. The fixture
retained 7,132 live rows and 6,317 completed rows. Raw samples and build facts
are in [terminal-search-reference.json](../fixtures/terminal-search-reference.json).

| Workload | Before | After | Speedup |
| --- | --- | --- | --- |
| Live history, miss | 11.49 ms | 4.26 ms | 2.70× |
| Live history, capped hits | 8.13 ms | 3.15 ms | 2.58× |
| Completed history, miss | 14.60 ms | 3.81 ms | 3.83× |
| Completed history, capped hits | 11.28 ms | 2.86 ms | 3.95× |
| Matcher ASCII, miss | 91.41 ms | 5.31 ms | 17.21× |
| Matcher ASCII, capped hits | 5.72 ms | 0.45 ms | 12.66× |
| Matcher wrapped repetitive prefix, miss | 652.23 ms | 47.30 ms | 13.79× |
| Matcher Unicode, capped hits | 4.05 ms | 1.89 ms | 2.14× |

Matcher misses process 40,000 rows; matcher dense hits process about 2,501
rows before reaching the result limit. Each live/completed capped-hit workload
processes about 5,001 rows. The baseline uses the original matcher and live
ASCII ingestion with the same benchmark fixtures; its bulk-append benchmark
helper delegates to the original character-by-character implementation.
The two additional Unicode-miss guard workloads have optimized samples only
in this reference; no before/after claim is made for those cases.

## Busy terminals and the update tail

After most messages, `Cagents::update` runs a tail of whole-workspace
projection: sidebar channels and worktree panes, worktree protections,
selection, source control, transcript probes and chrome. Terminal wakes that
change no metadata, and repaint-only ticks (`ChatUi` ticks and, in the
integrated build, browser progress ticks), now skip it. A wake whose only
change is a reported terminal title (OSC 0/2) runs only chrome, which includes
the window title. Reported titles on their own cause at most one save, and
one publication to shared views, every three seconds. The unit tests in `crates/cagents-ui/src/app/terminal_output_tests.rs`
count the tail passes, chrome syncs and saves each message causes.

The full projection is the default. An update projects less only where the
handler for its message declares what it changed, by returning a narrower
`Projection` beside its task, so a new handler is correct before it is fast.
A handler that returns before the projection makes the same claim. Typing is
the busiest case: a key that a text field took, a key release and a
modifier change answer nothing in the workspace and project nothing, and the
pointer crossing the tab strip changes only which tab is drawn hovered. A key
that does something, such as Ctrl+Tab moving the focus or Escape closing a
dialog, projects everything in its own update rather than leaving the
projection to whatever message comes next.

`app/projection_oracle.rs` checks each narrower declaration. After an update
whose projection was narrower than `Full`, it runs the full projection and
compares what the workspace derives: the sidebar model and state, card chrome
and indices, terminal ordinals, pane and window titles, inspectors, transcript
probes, worktree owners and protections, the panes whose working directory
and find bar are kept, and which source control, branch comparison and
history reads are current. It does not compare the tasks the projection
returns. Every unit test built with debug assertions runs it and fails,
naming the message and the stale part. Before an outermost update whose
message can project narrowly it first brings that state up to date, because
tests often arrange state directly and leave its projection to the next
update. An update whose handler returned before projecting is checked when
it returns if its state was brought up to date first. Otherwise the next
update that brings the state up to date reports what it found stale against
that earlier update, rather than repairing it silently; stale state found
after an update that did project is put down to a test's own arrangement.

Set `CAGENTS_PROJECTION_ORACLE` for a development build to run the same
check natively. It logs to the `cagents::projection_oracle` target, both a
stale declaration and state found stale before an update, with the update
before it. Each check keeps what its full projection repaired and runs the
tasks it returned, so such a session behaves as though every update
projected everything: it cannot show a stale title or sidebar, and only its
log shows whether a declaration was wrong. The check adds full projections
to narrower updates, so the timing tests turn it off and native measurements
run without the variable.

The workload for these declarations is the `realistic` native evidence seed
(18 projects, 76 channels, 1769 cards) at 1600x1000 under `--ui-trace`, with
three fixture Claude agents streaming long replies, one shown. The run
records idle streaming for 15 s, then types four 77-character sentences into
the shown conversation's composer with `drive type`. It then hovers 14
sidebar rows and 4 tabs twice, scrolls the sidebar, and switches channels 8
times. One run per side on fw16 (`dev-runtime`) compares `a831f307a`, which
traces the stages and projects everything for every key, with the change for
issue #2954:

| Phase | Before | After |
| --- | --- | --- |
| Typing, full projections | 336 `KeyEvent`, 2304 ms | none from keys (2 `RefreshUi` ticks) |
| Typing, worst window's frame work p95 / p99 | 557 / 877 ms | 24 / 57 ms |
| Typing, busy per second of trace | 643 ms | 302 ms |
| Hover, `ChromeUi(HoverTab)` updates | 16, 165 ms | 12, 1.6 ms |
| Whole run, traced busy time | 30.5 s over 146 s | 15.7 s over 136 s |

Before, about two thirds of each key's projection was `stage chrome`, which
rebuilds every card's chrome. A visible streaming conversation's
`ChatUi(Wake)` costs about 2 ms of update each, outside the projection, and is
the largest remaining cost while typing.

The native workload has no script. In the browserless development runner,
inside a nested niri session with fresh private HOME and XDG directories, open
20 terminals with Ctrl+Shift+T. Then use `cagents terminal write` to start one
of these in every terminal:

- Title churn: `bash -c 'i=0; while :; do printf "\033]0;build step %d\007" $((i++)); sleep 0.1; done'`
  (20 terminals, 10 titles per second each).
- Streaming: `bash -c 'while :; do yes cagents-N | head -c 200M; done'`.

After five seconds, sample `utime+stime` from `/proc/<ui>/task/<ui>/stat`
(UI main thread), `/proc/<ui>/stat` (UI process) and the matched `cagentsd`
over 20-second windows.

Local reference on a Ryzen 7 7840HS, `dev-runtime`, `c07b6f78e` (before)
against the #1103 branch (after). Each build had one fresh run with two
windows per workload. The host was shared with other builds, with a load
average of 35 to 45, so treat the differences as indicative only.

| Workload | Main thread before | Main thread after | Daemon before | Daemon after |
| --- | --- | --- | --- | --- |
| Idle, 20 terminals | 0.4% | 0.2% | 0.1% | 0.2% |
| Title churn | 83.7%, 80.9% | 53.7%, 53.2% | 43.7%, 42.8% | 34.6%, 33.4% |
| Streaming | 15.5%, 14.8% | 17.4%, 14.8% | 423%, 420% | 443%, 418% |

Streaming output already took the metadata-unchanged fast path before this
change, so it is unchanged. Its main-thread profile is terminal widget
resolution, layout, accessibility projection and allocation. After the change,
the remaining title-churn time is mostly the same view, layout and render work
that each title causes; `sync_chrome` is about 0.2% of samples.

## Workspace model edits

A pane edit in one window reaches every other window as a model edge. The
window applies the edge's effects to its copy and, for a settled presentation
change such as a pane title, projects only that scope. Two trace targets show
where that work goes; with both disabled these paths read no clock and
allocate nothing.

- `cagents::workspace_pipeline=debug` records the inclusive wall time of each
  shared-model update, including its projection tail, and of each replica
  snapshot timer and capture.
- `cagents::workspace_projection=trace` records whether each update projected
  one scope or the whole workspace, with the reason when it fell back. A unit
  fixture's eligibility does not show that a native workload qualifies, so
  count selected and full projections in the native run.

Measure edits on the generated large workspace with fixture agents streaming,
one of them visible, and drive a fixed number of pane title edits at a fixed
interval through `ApplyWorkspaceEdit` from a separate client. Compare UI
main-thread CPU and the UI thread trace's update and frame times over the edit
interval on both sides.

Start the native evidence session with `--ui-trace --workspace-trace` to
record both inclusive update time and the selected/full projection decisions.
`scripts/native-evidence open-window window-2` opens the peer through the
development runner using the same recorded binaries, without compiling under
the native lock. Its trace is `logs/ui-SIDE-window-2.log`; the primary trace
is `logs/ui-SIDE.log`.

The current-main comparison for the UI half of this work is tracked in
[#2789](https://github.com/CarsonBurke/cagents/pull/2789). Its measurement table
identifies the actual application revisions, workload, raw receipts and
qualifications; historical #2573 results are not measurements of this branch.

## Native chat viewport

Run the headless viewport contract with optimized application code, separately
from native application measurements:

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p cagents-ui \
  --profile dev-runtime --config 'profile.dev-runtime.opt-level=2' \
  --config 'profile.dev-runtime.debug-assertions=false' --lib \
  chat_ui::view::frame_cost::viewport_cost_with_a_megabyte_answer_and_three_hundred_tools \
  -- --ignored --nocapture --test-threads=1
```

The workload compares a 16 KiB answer with ten tool rows against single 1 MiB
paragraph, many-paragraph and fenced-code answers with 300 tool rows, at a fixed
viewport. It warms the real pane/composer, then records refresh separately from
view, native widget layout, pointer update, headless draw and widget/cache
disposal CPU time. The event total includes those latter stages. Its median must
be at most 2 ms and at most twice the small workload. These are explicit test
budgets, not claims of achieved results. The test excludes GPU submission and
compositor presentation. Run the neighboring long-streaming workloads separately
for a busy parent and a selected running child whose parent is idle. Streaming
fixtures apply validated `Presentation` deltas and record append lineage. They
complete and verify each current rendered publication before sampling its steady
frame, and separately report synchronous fixture wall time for refresh, the owned
parser engine, installation and geometry settling. This phase is diagnostic work
attribution, not native worker scheduling/input latency. An unconsumed new source
or a Loading placeholder cannot count as the rendered streaming workload. Native
80-delta/second input and completion timing must be measured separately.

A native comparison must use the same scripted provider, workload text, pane
geometry and build profile on both revisions. Record the exact commit and dirty
state, process identity, sampled UI CPU/RSS/PSS, and time boundaries alongside
raw samples. Exercise `huge 1024`, `stream 300 300`, 43 composer characters,
40 wheel notches, Find, selection/copy, and a hidden `tools 300` child. Opening
the 1 MiB answer has a 150 MB RSS growth budget. Switching away keeps the
most recently hidden chats warm (`crates/cagents-ui/src/app/chat_ui/warm.rs`): at most 8, within
an estimated 96 MiB of heap together, and none over 32 MiB, which goes cold as
it hides. The estimate is calibrated against the heap cooling frees, measured
by `frame_cost::what_a_warm_hidden_chat_holds`. After switching away from a
chat that goes cold, the UI must return within 30 MB of its prior baseline;
while chats are warm, within 30 MB of that baseline plus their estimate. Record
RSS for 1, 4, 8 and 9 hidden chats, the ninth sending the first cold. Freed
heap is not always returned to the system, so compare RSS after the repeat
cycles too, not only after the first. Repeat hide/reveal and close cycles,
including Hide tab. Use the isolated development runner described in
[development.md](development.md), preserving existing sessions.

Full-context shaping holds one output permit through UI consumption; queued
completion messages do not release it. Accepted installation releases the permit
while the visible paragraph keeps its geometry. Discarded results retain the
permit through background disposal. Parser and shaper workers wait for earlier
retirement before allocating another generation. A hide can retire its existing
active graph as one batch; this bounds generation amplification, not total live
glyph storage or process RSS. Optional `cagents::memory=trace` includes pending
retirement item counts for native visit/hide qualification. Actual permit release
also publishes one coalesced availability event; visible deferred readers retry
without depending on unrelated input, provider output, or another redraw. Parser
disposal checks continuation/reference allocations even when the document has no
rendered items. The selected growing source retains its Markdown parser
continuation across verified appends. A changed fence rebuilds its syntax
palettes on the worker; only unchanged matching fences share prepared palettes.
Worker placement does not establish acceptable streaming CPU or input latency;
those costs remain part of native qualification.

Nested reader bookmarks retain a byte length and full source digest, alongside
geometry and the exact logical reader identity. After hiding releases bodies,
the admitted parser worker verifies that exact prefix in the authoritative new
code/tool source, including its partial final hash chunk. Only the matching
context/reader bookmark advances to the verified new digest. Inputs and outputs
are bounded by the existing 1,024 bookmark entries; unbookmarked sources need no
extra verification pass. No source bytes or acceptance graph remain hidden.
Changed tools with exact remembered reader paths use the same admitted worker
even while collapsed. Reopening preserves pending verification; installation
uses the current disclosure state. The worker also builds immutable tool text
and source descriptors, shared through visible collapse/reopen instead of
rehashing/copying the full body during UI installation. Hiding evicts these
body owners with the rest of the cache. A rewrite cannot overwrite a valid
bookmark before verification.

Literal user messages use the same admitted preparation engine without Markdown
interpretation. The worker prepares their full text, legacy collapsed preview
and selection checkpoints. Cold views show a bounded preview until the exact
current source is ready. A queued first message moves its canonical string into
an immutable local source and retains the same source reservation through its
final raw destruction, including cancellation before a subscription starts.
Compact local follow-ups prepare their preview at explicit submission. Provider
queue previews reuse immutable records across unrelated publications; changed
records are copied only within the existing 128-message, 16 KiB source-preview
limits, with the existing truncation marker appended. Comparing those bounded
sources on a changed snapshot remains UI work.
Copy resolves the exact current record at explicit intent and preserves raw
bytes independently of its displayed preview.

Search results, Notice bodies and Image descriptions/paths use the same admitted
literal preparation boundary. Stable logical readers preserve their displayed
text for selection and Find without interpreting it as Markdown. Search fields
share prepared document storage; long Notice previews preserve the existing
three-line policy. Ready values must match the current canonical item before
installation, and rejected or evicted payloads use the existing reclaimer.

Find Next/Previous retains its existing matching-turn/item counts and temporary
disclosure ownership. Raw group discovery, provider-aware disclosure plans,
rendered-block lookup and lazy visible Flow highlights share one try-admitted
worker. A fixed aggregate small-input path handles tiny searches synchronously.
Unicode query folding and its matcher table are shared across phases. Large
sources are scanned on the worker, with cancellation checked inside both query
preparation and source matching. Highlights bind the exact painted ready source;
their existing 4,096-range limit applies to each block. Output reservations stay
with painted range owners, including retained widget clones.
Retained Flow widgets read current marks against the exact ready prefix they
paint, so an asynchronous completion does not require rebuilding the element.
A late per-block limit notice appears below the text without moving its glyph
origin. Accepted query contexts carry a separate render revision when their
exact tool-disclosure bindings change; unrelated publications reuse it.

Link destinations share immutable storage through parser output, native spans,
hit testing and reader actions. Rendering does not clone a long URL just because
its displayed label is short. Resolving a clicked destination remains explicit
UI work and preserves its complete destination.

Every normal raw outcome, including failure, retains the same admission through
message consumption or background disposal. Query and attempt epochs reject old
errors before they can change a newer search. Metadata pressure retires displaced
plans and waits for actual metadata capacity release, independently of worker-slot
wakes. Pending searches and failed searches have distinct visible states; failure
does not masquerade as a successful zero-match result. Exact unchanged groups can
remain available during unrelated streamed publications while changed groups
await current indexing.

Each selected group is resolved to its first matching rendered logical block and
UTF-8 byte. Navigation
then realizes cheap offscreen descriptors, mounted chunks and actual ready glyph
geometry, including horizontal readers. One initial coarse realization may be
needed while matching worker geometry is pending; it belongs to the intent's
maximum sixteen actual coordinate corrections and absolute thirty-second
deadline. Waiting for that geometry does not consume further corrections,
including across verified source publications or unrelated completion wakes.
Parser and shape completions wake pending geometry without a timer subscription.
Matching metadata without rendered plaintext keeps the coarse group anchor.
Loading is never an exact glyph match.
Independent intent and rendered-document epochs guard both async replies and
the eventual native scroll mutation. Query/context/disclosure/hide changes and
native wheel, thumb, selection drag, touch, resize and keyboard navigation revoke
queued corrections. A Find scroll does not masquerade as new user input.

The final owners of parser requests, canonical continuations and logical
selection catalogs transfer their raw allocations to the same reclaimer. This
also covers subscriptions and retained widgets that outlive cache eviction.
Canceled and discarded work releases reservations after its actual owned payloads.
Independent native widget/read clones still contribute to measured event and
cache-disposal costs; this is not a guarantee that every UI allocation frees on
the reclaimer.

The pane inspector's diagnostics disclose known source, prepared-presentation
and logical-selection payload owners for the selected chat. These counters
exclude opaque parser/font/renderer/image allocations and allocator retention;
they are not RSS or a complete heap estimate. Use process measurements to verify
residency budgets rather than inferring them from logical byte counts.

Transcript row factories share one UI-local immutable AST root per parser
publication, using index paths for quotes, lists and table cells. Ordinary
publications clone that graph once on the parser worker; the unchanged
inline/code text backing is shared. This removes recursive factory clones
during view rebuilding. Final raw graph disposal runs on the persistent
reclaimer even when retained widgets outlive the cache. Raw graphs in rejected
output are dropped inside the existing retirement batch before its admission
permit and FIFO barrier. Coordination recognition and all rich prompt, brief
and result parts use the same provider-aware admitted parser worker; the UI
detects preparation candidates from bounded item metadata. Exact-source
disclosure changes reuse those parts and their palettes. Canonical coordination
graphs and recognition
metadata also retire off the UI thread. Process measurements must include the
extra publication graph containers. The worker also prepares aggregate subtree
sizes, code line counts and row estimates. A recursive tree of short quote/list
children cannot opt into a dense column merely because each immediate list is
small. Rows share slices of one flat logical outline. Binding that outline to
an item/root namespace first visits logical metadata on the UI thread; later
views reuse the binding. Virtual column geometry/index rebuilding still visits
its aliases on a revision change. These costs are not claimed constant-time or
small for arbitrary rich documents.
Actual native rich/hide CPU, RSS/PSS/HWM and input/frame qualification remains
required.

Parser admission retains at most 128 selected-source entries and 72 MiB of
queued source bytes; these are source reservations, not RSS estimates.
Temporary pressure leaves the immutable source in its authoritative snapshot
and resumes automatically from a metadata cursor after entry or final
source-lease release. Each resumed pump visits at most 128 positions; initial
cache enumeration still visits the selected history. The watch sends a
current-state event when subscribed so an idle UI cannot miss capacity released
during subscription installation. Newest text gets first admission; historical
inputs retain deterministic order. Sources that individually cannot fit the
budget remain explicit errors. No shell/JSON parse runs on the UI during
pressure; commands of at most 2,048 bytes can be excluded using the
recognizer's existing program-name predicate. Full rich coordination parts are
still reparsed on changed publications; off-thread ownership does not make this
cost incremental.

## Idle retained actors

Each retained Claude agent keeps a `cagents-native --cagents-native-actor`
process alive between turns. An idle actor should not wake: its serve loop
reaps adopted tool orphans when a child exits, read from a signalfd, and its
`cagents-commands` cgroup sweeper backs off from 1 s to 8 s once a provider
that reports its own activity has been quiet.

`scripts/measure-idle-wakeups` counts context switches and on-CPU time per
thread name from `/proc` alone, without signalling anything, so it is safe to
run against the actors of a live session:

```sh
scripts/measure-idle-wakeups --seconds 60
```

`scripts/measure-idle-actors BIN_DIR [COUNT] [SECONDS]` builds a private
fixture (HOME, XDG directories, runtime directory and project) from a matched
`cagents` and `cagentsd`, spawns COUNT agents with the offline fixture provider
one at a time, waits for each to settle, then reports RSS, PSS and private
memory from `smaps_rollup` for the fixture daemon, actors and providers before
counting actor wakeups. Run it at both one and twenty actors for a memory
comparison. It removes every process and directory it created on exit. Run it
from a genuine user shell, such as `ssh fw16`; it refuses agent authority.

`scripts/measure-cagents-memory --environ NAME=VALUE` is the same read-only
RSS/PSS census for a native development session, including its UI. Pass the
session's private runtime directory so it cannot include another session.
Set `CAGENTS_IDLE_ACTOR_FRAME_BYTES=2097152` when comparing a fix for idle
protocol-frame retention: the fixture emits one 2 MiB tool result per actor,
then waits for every actor to settle before the census.

Reference on fw16 (Ryzen 7 7840HS, 16 threads), `dev-runtime`, 45 idle actors,
60 s, `0b8b9aace` (before) against `6e62e851a` (after). The host was shared
with other builds, so treat CPU as indicative; the wakeup counts are timer
periods and barely depend on load.

| Wakeups/s | Before | After |
| --- | --- | --- |
| `cagents-native` (serve loop) | 188.47 | 0 |
| `cagents-command` (cgroup sweep) | 46.30 | 5.92 |
| `cagents-chat` | 0 | 0 |
| Total | 234.77 | 5.92 |
| Per actor | 5.22 | 0.13 |

CPU across all 45 actors fell from 3.32% to 0.08% of one core.

`/proc` truncates the `cagents-commands` thread name to `cagents-command`.
The remaining wakeups are that sweeper's 8 s ceiling, about one per actor every
7.6 s: cgroup v2 has no fork notification, so the sweep is what finds a command
started without an announcement.

## Work board polling

The daemon's Work board keeps each open project's Git facts current. The Git
watcher covers projects within a budget of 8192 directory watches, and each
checkout's tracked directories count against it, so a repository with
hundreds of worktrees is polled instead. Each worktree of a polled project
comes due on its own. A worktree with an edit or agent session in the active
window is read every 15 s, and any other every 120 s; reads due within the
next 15 s share one scan. Previously one recent edit anywhere made the whole
project re-read every worktree every 15 s.

`scripts/measure-work-polling BIN_DIR [WORKTREES] [SECONDS] [ACTIVE]
[DIRECTORIES]` builds a private fixture from a matched `cagents` and
`cagentsd`: a project of WORKTREES worktrees (default 250) with DIRECTORIES
tracked directories each (default 40), which it checks exceeds the watch
budget, and ACTIVE (default 3) freshly edited worktrees. It first reports the
load: when every checkout had a status, when the daemon held all the watches
it then kept, and the Git commands and CPU until the daemon settled. Then it
counts the Git commands and `git status` reads the daemon starts and the CPU
it and its exited children use over SECONDS (default 300). Commands are
counted from Git's own trace2 records, which every fixture process writes, so
even the shortest are seen; each is attributed to the process that started
it, and the CLI's are reported apart from the daemon's. The fixture's
processes and setup commits read no Git configuration outside it. It removes
everything it created on exit and refuses agent authority; run it from
`ssh fw16`.

Reference on fw16 (Ryzen 7 7840HS), `dev-runtime`, 250 worktrees, 3 active,
300 s, `0ac78d249` (before) against `9b8523e9a` (after):

| | Before | After |
| --- | --- | --- |
| `git status` per second | 15.79 | 2.47 |
| Git CPU, % of one core | 7.22 | 1.71 |
| Daemon CPU, % of one core | 1.83 | 0.45 |

A fixture checkout holds 40 files. A real checkout costs more per read: on a
347-worktree repository with 8 active worktrees, the scanner's Git processes
took at least half a core before this change.

### Watch planning

A watch plan needs each worktree's Git directories and the paths its index
tracks. It read the directories with two `git rev-parse` processes per
worktree and the paths with `git ls-files`, and the window's fallback scan,
which names every worktree, repeated all three each minute. The directories
now come from the checkout's `.git` marker and `commondir`, as Git's own
discovery reads them. On a local filesystem whose attributes show every
write, such as ext4, btrfs, XFS or tmpfs, a census is kept while its index
file keeps the inode, size and times it had when read: Git renames a new index
over the old one, and an in-place writer is missed only if it keeps the same
size within one timestamp tick. On any other filesystem the index is read
again each time.

The daemon's first load on fw16, `dev-runtime`, 3 active worktrees,
`c82fe6d5c` (before) against `eb04813f3` (after), a build of this change
before review made it find shared censuses lazily and keep none on a
filesystem it does not know; neither changes what a local fixture runs. The
host was shared with other work:

| Worktrees | Daemon load Git commands, before | After | `git rev-parse`, before | After |
| --- | --- | --- | --- | --- |
| 250 | 1,263 | 761 | 502 | 0 |
| 1,000 | 5,013 | 3,011 | 2,002 | 0 |
| 3,000 | 15,020 | 9,018 | 6,002 | 0 |

Those runs counted every fixture process's commands as the daemon's. The one
other, which the counts above leave out, is the `git rev-parse` of the CLI's
`cagents project open`, as a later run at 250 worktrees that attributes each
command showed (761 for the daemon, that one for the CLI); before, the
daemon's are exactly two per checkout. At 3,000 worktrees the daemon's first
watch was registered about 49 s after the project opened rather than 57 s;
those runs timed the first watch rather than the last, so both are
approximate. The load used 3.7 s of daemon CPU rather than 6.2 s, and 25.2 s
of Git CPU rather than 32.9 s. Polling over the 180 s after the load is
unchanged: 1.58, 5.74 and 16.86 `git status` reads per second at 250, 1,000
and 3,000 worktrees, because the watch budget leaves each project uncovered.
The load still reads every status twice: once in the scan that builds the
plan, and once after its watches are registered.

The window gains most. With a 1,000-worktree project open in the native app
on the same host, and counted from trace2 over 180 s once the load had
settled, the window started 11,473 Git commands (63.7 per second) before and
4,006 (22.3 per second) after: one `git status` and one `git config` per
worktree per fallback scan. Its own CPU fell from 3.7% to 1.3% of a core, and
its Git children's from 18.6% to 6.1%.

## Git watcher during builds

The daemon's Git watcher (`notify-rs inotify loop`, from the `notify` crate)
covers each project's tracked directories and Git metadata. It asks the kernel
for changes only: creations, removals, renames, content and metadata writes.
An inotify watch that includes opens wakes the thread whenever `git status`, a
compiler or an editor merely reads a watched file or lists a watched directory.
Ignored output such as `target/` is never watched, because content watches come
from the tracked-directory census.

`scripts/measure-watch-wakeups BIN_DIR [SECONDS]` opens a shared clone of the
checkout as the only project of a private daemon fixture. It then checks the
clone's CLI through the build gate over and over, touching `src/main.rs` before
each check, while `scripts/measure-idle-wakeups` counts the daemon's threads.
Run it from a genuine user shell on the build host, with rustup's Cargo first
on `PATH`:

```sh
scripts/measure-watch-wakeups ~/.local/state/cev/watch/target-after/dev-runtime 120
```

Reference on fw16 (Ryzen 7 7840HS, 16 threads), `dev-runtime`, 120 s of
builds, `7c7430da9` (before, notify 8.2) against `3611b064f` (after, notify 9
with `EventKindMask::CORE`). Both sides used the same fixture from the after
checkout; the host was shared with other builds.

| Daemon, per second | Before | After |
| --- | --- | --- |
| `notify-rs inoti` wakeups | 937.08 | 0.47 |
| `work-scan` wakeups | 3.05 | 5.75 |
| All daemon threads, wakeups | 943.03 | 10.05 |
| All daemon threads, CPU % of a core | 0.81 | 0.10 |
| Checks completed | 26 | 49 |

`work-scan` wakes to read a worktree's status after a real change. It followed
the build rate on both sides, about 14 wakeups per check, so each touched source
still reaches the Work board. The watcher's remaining 0.47 wakeups/s are about
one per touch (0.41/s).

A live daemon with 8,715 watches across several large projects showed the same
pattern. A 60 s replica of its watch set saw 714 events/s, 701 of them opens.

## Reference results

The `linux-headless-release-reference` fixture was recorded on a Ryzen 9 9900X,
an NVIDIA RTX 5090 (Vulkan UI), and a Sway 1.12 headless GLES2 compositor at
1280x800, scale 1.0. Raw samples and machine facts are in
`fixtures/linux-headless-release-reference.json`. It is a non-gating local
reference, not a physical-display baseline.

Redraw scheduling, five fresh-process runs, 1,280 samples:

| Median | p95 | p99 | Worst |
| --- | --- | --- | --- |
| 16.16 ms | 18.32 ms | 19.75 ms | 25.32 ms |

Terminal pipeline: the same reference records all nine untraced backend/IPC
workloads and a separate traced native run. Iced draw was the largest
instrumented CPU stage (4.06 s accumulated; 0.88 ms p95), ahead of snapshot
capture (1.30 s) and IPC encoding (1.19 s); concurrent stage totals are not
additive latency.

Native virtual-keyboard input to a matching compositor screencopy pixel,
including observer overhead, 100 samples each:

| Scenario | p95 | p99 |
| --- | --- | --- |
| One terminal | 37.85 ms | 47.91 ms |
| Plus three updating TUI terminals | 38.82 ms | 65.10 ms |

Both fit the 50/100 ms input budgets. This is an upper bound to
compositor-visible pixels, not presentation timestamps or physical scanout. The
settled idle native terminal produced no additional terminal draws and no
measurable UI CPU ticks in a three-second observation.
