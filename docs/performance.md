# Performance measurements

- [Redraw scheduling](#redraw-scheduling)
- [Native chat stage attribution](#native-chat-stage-attribution)
- [Pane open](#pane-open)
- [Terminal pipeline](#terminal-pipeline)
- [Terminal search](#terminal-search)
- [Native chat viewport](#native-chat-viewport)
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
| 1 | Parent preparation | Parent `prepare_items` in refresh |
| 2 | Child preparation | `prepare_child_cache`, including empty selection |
| 3 | Parse | Markdown source advance and content append/restart |
| 4 | Highlight | Fence scan and syntax-block update |
| 5 | Turn build | Actual `turn_rows` construction, including retained cache misses |
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

## Native chat viewport

Run the headless viewport contract with optimized application code, separately
from native application measurements:

```sh
CARGO_BUILD_JOBS=2 cargo test --locked --no-default-features --features ui \
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
the 1 MiB answer has a 150 MB RSS growth budget; after switching away, the UI
must return within 30 MB of its prior baseline. Repeat hide/reveal and close
cycles, including Hide tab. Use the isolated development runner described in
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
