# Performance measurements

- [Redraw scheduling](#redraw-scheduling)
- [Terminal pipeline](#terminal-pipeline)
- [Reference results](#reference-results)

These workloads are opt-in, local, and non-gating. Never compare unoptimized
`dev` or partially optimized `dev-runtime` results with the release baselines
below.

## Redraw scheduling

```sh
sh scripts/run-redraw-latency /tmp/cagents-redraw-reference
```

The output directory must not already exist. This `latency-harness` lane builds
the CLI, host, and browserless UI separately in **release** mode, then runs five
fresh UI processes with private HOME and XDG directories. Each run walks eight
synthetic unloaded cards with a fixed seed: 32 warmups and 256 recorded
adjacent focus changes. No user sessions or PTYs are loaded. It requires a
graphical session, `jq`, `timeout`, and the normal native release
prerequisites.

The metric is **requested state committed to first matching-view
RedrawRequested**. It measures redraw scheduling latency, not renderer
submission, compositor presentation, scanout, or arbitrary input-to-visible
latency. `card_focus`, and the planned `palette_open` and `palette_query`, are
separate actions. Superseded or unfinished generations settle as abandoned,
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
