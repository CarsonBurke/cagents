# Transcript performance profile

## Scope and measurement status

The transcript path has two different workloads and they must not be conflated:

1. A long, settled reader being opened, resized, scrolled, or switched away
   from and back to.
2. A streaming reader where only the live tail changes while the rest of the
   transcript remains idle.

The reproducible headless measurement at reviewed head `901b6f94` used 10,000
virtual rows, a 600 px reader, and 30 warmed `Anchored::layout` passes. It
measured **37.408 ms/pass**. The reader had not been scrolled, so this is a
lower bound for the prior design: an operation requested every alias in the
loaded page on each anchored pass.

The native end-to-end measurement has to be run by the lead from the
workstation because hosted sessions carry `CAGENTS_AGENT_AUTHORITY`. The
existing steps are in
`.worktrees/.codex-perf/perf-transcript-native-run.sh`. It must record, on the
same fixture and fw16 revision before/after:

- `CAGENTS_UI_TRACE` frame stalls while opening a 20-chat workspace, scrolling
  a thousand-turn chat, resizing it, and switching between two long chats;
- UI-thread CPU from `/proc/<ui-pid>/task/*/stat` over those phases;
- wakeups, RSS, IPC bytes, and workspace-save bytes while one chat streams and
  the other nineteen chats are idle.

That native result is required before an implementation PR is reviewable.

## Observed cost and its pattern

| Cost | Evidence | Pattern | Why it scales poorly |
| --- | --- | --- | --- |
| Anchored layout walks aliases for history geometry | 37.408 ms/pass for 10,000 warmed rows in the headless fixture | Unbounded whole-page traversal | A resize or settle pass can repeat the walk up to four times. |
| Anchored layout builds item-ID maps | `Anchored::layout` rebuilds maps from every transcript item | Recompute rather than memoize by revision | Even a no-op relayout pays O(all items), before any visible row is laid out. |
| Reader memory retains reported rows | Position restoration keeps rows reported by previous passes | Cache without a viewport bound | A reader which visits much of history can turn a subsequent pass back into an all-history request. |
| Streaming and idle readers share the same retained-tree machinery | The active tail and settled history both enter transcript layout | Whole snapshot/view work instead of revisioned deltas | A tail update must not reshape, rebuild, or relayout settled offscreen history. |

The currently known 37.408 ms headless layout cost alone exceeds a 60 Hz frame
budget by more than 2x. It does not account for markdown shaping, images,
tool-output widgets, native drawing, scrolling event processing, or the
streaming view publication path; the native trace is therefore the authority
for total UI-thread cost.

## Required O(visible) architecture

The transcript should be a revisioned document with a virtual viewport, not a
widget tree that rediscovers the document every layout.

### 1. Immutable per-turn records

Prepare each turn once off the UI thread into an immutable record:

- stable turn/part identity;
- estimated and then measured block height;
- prepared markdown/text layout keyed by width, theme/font revision, and turn
  content revision;
- image/tool-output metadata and intrinsic dimensions;
- searchable byte-range index and aliases.

A streaming turn publishes a new revision only for its changed tail. Settled
turn records keep their shaped text and metadata; they never rebuild merely
because another turn streams or the sidebar changes.

### 2. Transcript-owned indexes, updated by deltas

Keep the following indexes in persistent transcript state, updated only when
the document revision inserts, removes, or changes a turn:

- a prefix-sum/Fenwick height index for offset-to-row and row-to-offset;
- `alias -> row(s)` and `logical range -> row` indexes;
- a turn-ID to prepared-record index;
- a bounded reader-anchor cache containing the shown rows plus explicit
  nearest boundary anchors.

`Anchored::layout` must borrow these indexes. It must not construct HashMaps
from every item, clone every `Id`, or scan aliases. Find and restoration query
one alias/logical range through the index in O(log n) or O(1), then ask the
virtual viewport to materialize that row.

### 3. A bounded visible window

Layout and draw only the visible rows plus a fixed pixel overscan. The window
owns the only retained native widget trees. Moving the viewport evicts trees
outside that window but retains their immutable prepared record and measured
height.

Reader restoration stores a bounded semantic anchor: primary visible turn ID,
offset within it, viewport geometry, and at most adjacent boundary turn IDs.
It does **not** retain every row encountered during scrolling. If the primary
turn disappears, resolve the nearest surviving boundary through the persistent
index.

### 4. Explicit revisions and invalidation

Separate revisions for document content, prepared presentation, width/theme,
viewport, and reader position determine the minimal invalidation:

- a wheel changes only viewport/window membership;
- a resize reflows visible prepared records and adjusts their height-index
  entries;
- a streaming append changes its one tail record and the height suffix;
- a switch selects another reader's stored semantic anchor and realizes just
  its window;
- a Find selects one indexed row and requests its materialization.

No app-wide tick, full conversation view reconstruction, or all-turn markdown
reshaping is permitted for these operations.

## What this replaces

This design replaces `Anchored::layout`'s per-pass `ItemBounds` operation,
per-layout maps over `self.items`, and history-wide placeholder reporting. It
also replaces reader memory whose set of requested aliases grows with prior
scrolling. The existing virtual column's prefix sums and stable descriptor
identity are useful foundations, but its alias index must become
transcript-owned persistent state rather than an operation-time workaround.

## Highest-leverage systemic change

Build a transcript document store with persistent height/alias/logical indexes
and a revisioned visible-window widget. This single change removes the whole
class of O(all turns) layout, alias traversal, and re-shaping work, while also
giving streaming a natural delta boundary. It requires moving prepared turn
records and indexes out of ephemeral `view()` construction, defining stable
revision ownership, migrating Find/reader restoration to semantic anchors, and
adding native fixtures plus trace assertions for the 20-chat workload.
