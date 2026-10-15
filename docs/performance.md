# Performance measurements

- [Redraw scheduling](#redraw-scheduling)
- [Pane open](#pane-open)
- [Terminal pipeline](#terminal-pipeline)
- [Terminal search](#terminal-search)
- [Reference results](#reference-results)

These workloads are opt-in, local, and non-gating. Never compare unoptimized
`dev` or partially optimized `dev-runtime` results with the release baselines
below.

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
release throughput claims. Summarize a collected log with:

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

## Connection transport resources

Terminal attachments and daemon connections share a transport runtime with two
I/O threads and four dispatcher workers. Requests on one connection remain
serial, including the handshake and resume replay. Blocking host operations
and outbound serialization use the fixed dispatcher, whose admission limit is
64 jobs. Socket reads and writes use nonblocking readiness rather than one
reader and writer thread per connection. The transport runtime is separate
from the PTY reactor so host requests cannot park PTY output reads.

A retiring client keeps its admission permit until unregistration finishes.
This bounds queued cleanup by the admitted client count and allows retirement
to run before ordinary dispatcher work. Per-frame writes have a five-second
deadline; peer closure and delivery overflow still close both socket halves.
UI terminal clients retain their bounded command/event queues and read credit.
Ordinary idle attachments have no polling timer. Permanent handshake errors
back off while awaiting space in the existing public semantic event receiver
and remain cancelable when the handle is dropped.

The isolated transport census tests warm shared resources before recording
`/proc/self/task`, then attach thirty peers and thirty hosted terminal handles.
These tests cover transport growth only; the full native measurement must also
include the daemon's emulator workers, holder PSS, eight provider chats, and
`scripts/benchmark-terminal input` p99 on the same workload before and after.
