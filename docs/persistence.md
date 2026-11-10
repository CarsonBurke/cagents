# Workspace persistence

This document describes who owns durable workspace state, how it is stored and
changed, and the staged plan that moves the current design to that target. It
is the reference for changes to `src/persistence.rs`, `src/session.rs`,
`src/host/model/` and the runtime directory's receipts and caches.

Status: design accepted for staged implementation. Each stage below names what
has landed; until a stage lands, the "Current design" section describes the
behavior you will find in the code.

## Current design and its failures

Measurements are from one long-lived installation (read-only), which is the
shape this design must stay fast for: 18 projects, 76 channels, 1856 view
scopes, 1769 saved cards in the primary window store and 1709 in a second one.

### Two authorities for one fact

Pane membership has two writers:

- The daemon's `workspace-model.sqlite3` holds `shared_views`, one row per
  scope with the whole ordered pane list (`panes`) and its tombstone set
  (`retired`) as JSON text.
- Every window keeps a full private copy of the workspace in its own
  `sessions.sqlite3` (`primary` at the data root, others under
  `windows/<slot>/`). A new window is seeded by `VACUUM INTO` from another
  window's store (`SessionRuntime::initialize_window`), then saves
  independently. A reused slot restores the old copy, never a fresh one.

The window's `cards` table carries the same descriptor the daemon's pane
carries (`shared_kind` strips only browser scroll and the repository project),
plus local presentation. Each window reconciles its copy against the daemon
model and publishes diffs back. When the two disagree, for example a close that
reached one window store but not the daemon, or a pane that is in a window
store but in no daemon view, each window keeps its own answer. Issue #2429 is
this: two panes stay open in three window stores while one of them is in no
shared view at all, so navigation cannot reach it and the close cannot stick.

### The working set is the whole history

Closing a card sets `existence = Closed` in place. Nothing removes it, so the
live `WorkspaceState.cards` holds every card the window ever opened: 1769 cards,
about 95% of them tombstones. Every consequence is proportional to history, not
to what is open:

- Every flush (250 ms debounce, 1 s cap) serializes and hashes every card,
  deserializes the stored `chat::Session` of every chat card before the hash
  check, and issues an extra query per closed terminal.
- Every flush rewrites 15 navigation metadata blobs unconditionally (about
  80 KB: `card_scopes`, `pending_chat_admissions`, `threads`, drafts, ...),
  several of them keyed by every card ever opened.
- Startup reads every card with one query per card.
- `saved_conversations` and `read_navigable_session_ids` scan and parse every
  chat row.

### Whole-document writes in the daemon

A view edit reads the scope's whole pane list, applies the change in memory and
rewrites the whole JSON document. Checking that a hosted resource has one pane
(`resource_owner`) parses every view. After each edit the daemon rebuilds the
full model with one query and one JSON parse per scope (`views::all`), and
every other window answers the pushed edit by reading that full model again.

### No lifecycle

- `shared_view_edits` keeps one row per accepted operation forever: 45,970
  rows, 11 MB of a 32 MB store, 36,608 for one scope.
- 1812 of 1856 views are empty; most belong to worktrees that no longer exist.
- `View.retired` only grows.
- Closed cards are never pruned from window stores.

### Runtime directory leaks (tmpfs, RAM-backed)

- Retained Claude helpers publish a `.final` receipt with the full final
  transcript snapshot (up to 34 MiB) and a small `.retired` receipt. Nothing
  removes either: `final_snapshot::remove` has no caller. 106 pairs held 58 MB.
- A transcript cache directory with an actor journal is reclaimable only after
  its helper writes a drained marker on retirement. A helper that is killed,
  crashes or is abandoned never does, and the journal is never reopened by any
  code. The cache sweep runs only at daemon start. 25 such directories held
  145 MB, the oldest six days old (#1064).

### Provider history is re-indexed on every open

Opening, restoring or reading a Claude conversation indexes the whole provider
JSONL from byte 0 (`claude.rs` `History::index`). Files in use reach 60 MB;
the index lives only as long as one actor.

### Duplicated chat descriptors

One `chat::Session` is stored in each window store's `cards`, in the daemon's
pane JSON, in `pending_chat_admissions`, and in `.final` receipts.

## Ownership

| State | Owner | Shared by windows | Store |
| --- | --- | --- | --- |
| Projects, channels, navigation order | daemon | yes | `workspace-model.sqlite3` |
| Pane membership, order, width, titles, worktree filing, descriptor | daemon | yes | `workspace-model.sqlite3` |
| Closed-pane history (resume, reopen, identity fences) | daemon | yes | `workspace-model.sqlite3` |
| Chat drafts and draft attachments | daemon | yes | `workspace-model.sqlite3` |
| Active channel/worktree, focused pane, per-worktree focus | window | no | window store |
| Window layout (`window_view_v1`), workspace history | window | no | window store |
| Per-pane presentation: browser scroll, editor caret and file, residency | window | no | window store |
| Utility panes (settings, accounts, automations, work), anchored before a shared pane | window | no | window store |
| Pending creations and admissions (`pending_terminal_creations`, `pending_chat_admissions`) | window until the daemon accepts them | no | window store, removed on acceptance |
| Retained chat copies (`local_only_cards`) | window | no | window store |
| Worktree operations journal (`pending_operations`) | window host | no | window store |
| Review cache | window host | no | review cache store |
| Retained actor receipts, transcript caches | runtime | n/a | `$XDG_RUNTIME_DIR/cagents` |

Rules:

- A fact has exactly one durable writer. A window never persists a copy of
  daemon-owned state; it holds the last model it received in memory and asks
  for it again after a restart.
- A window's store is small and private to its slot. It is created empty, never
  copied from another window, and only ever describes that window.
- Drafts move to the daemon because a chat pane is shared: two windows showing
  one conversation must not keep two drafts that each overwrite the other.
- Utility panes stay per window because they have no shared resource. They
  sit in a shared order, so each stores the shared pane it precedes rather than
  an index.
- `card_scopes` (which worktree a pane is filed under) is shared state today
  stored per window; it becomes the pane's `filing` column.

## Daemon schema

All tables are `STRICT`. Integer surrogate keys replace the serde-JSON scope
key that was schema 22's primary key. Schema 23 (stage 3) ships `view_scopes`,
`view_revision_clock`, `panes`, `retired_panes` and `view_operations`;
`closed_panes`, `drafts` and `draft_attachments` arrive with stage 5, which
moves that history into the daemon.

```sql
CREATE TABLE view_scopes(
  id INTEGER PRIMARY KEY,
  channel TEXT UNIQUE REFERENCES channels(id) ON DELETE CASCADE,
  repository TEXT REFERENCES projects(repository) ON DELETE CASCADE,
  worktree TEXT,
  revision INTEGER NOT NULL CHECK(revision >= 0),
  UNIQUE(repository, worktree),
  CHECK((channel IS NULL) = (repository IS NOT NULL)),
  CHECK((repository IS NULL) = (worktree IS NULL))
) STRICT;

CREATE TABLE view_revision_clock(          -- one row: the last revision issued
  id INTEGER PRIMARY KEY CHECK(id = 1),
  last INTEGER NOT NULL CHECK(last >= 0)
) STRICT;

CREATE TABLE panes(
  id TEXT PRIMARY KEY,                       -- CardId
  scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,                 -- sparse order key
  title TEXT NOT NULL,
  user_title TEXT,
  width_fraction REAL NOT NULL CHECK(width_fraction > 0),
  pending_terminal_start INTEGER NOT NULL CHECK(pending_terminal_start IN (0,1)),
  filing TEXT,                               -- Pane.scope: worktree the pane is filed under
  kind TEXT NOT NULL CHECK(kind IN ('terminal','chat','browser','pull_requests','repository')),
  resource TEXT,                             -- hosted terminal/chat/browser id
  provider TEXT,                             -- chat panes
  provider_session TEXT,                     -- chat panes, once known
  descriptor_format INTEGER NOT NULL,
  descriptor TEXT NOT NULL,
  UNIQUE(kind, resource)
) STRICT;
CREATE INDEX panes_by_scope ON panes(scope, position, id);

CREATE TABLE retired_panes(
  scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
  pane TEXT NOT NULL,
  ordinal INTEGER NOT NULL,                  -- retirement order within the scope
  retired_at_unix_ms INTEGER NOT NULL,
  PRIMARY KEY(scope, pane)
) STRICT, WITHOUT ROWID;

CREATE TABLE view_operations(
  operation TEXT PRIMARY KEY,
  scope INTEGER NOT NULL REFERENCES view_scopes(id) ON DELETE CASCADE,
  applied_at_unix_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX view_operations_by_age ON view_operations(applied_at_unix_ms);

CREATE TABLE closed_panes(
  id TEXT PRIMARY KEY,
  channel TEXT, repository TEXT, worktree TEXT,   -- last scope, not a FK
  kind TEXT NOT NULL,
  resource TEXT,
  provider TEXT,
  provider_session TEXT,
  title TEXT NOT NULL,
  user_title TEXT,
  working_directory TEXT,
  closed_at_unix_ms INTEGER NOT NULL,
  reason TEXT NOT NULL,
  descriptor_format INTEGER,                 -- NULL when only the identity is known
  descriptor TEXT
) STRICT;
CREATE INDEX closed_panes_recent ON closed_panes(closed_at_unix_ms DESC);
CREATE INDEX closed_panes_by_session ON closed_panes(provider, provider_session);
CREATE INDEX closed_panes_by_resource ON closed_panes(resource);

CREATE TABLE drafts(
  pane TEXT PRIMARY KEY REFERENCES panes(id) ON DELETE CASCADE,
  text TEXT NOT NULL,
  revision INTEGER NOT NULL,
  updated_at_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE draft_attachments(
  pane TEXT NOT NULL REFERENCES drafts(pane) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  kind TEXT NOT NULL,
  path TEXT,
  asset TEXT,                                -- managed image identity
  media_type TEXT,
  PRIMARY KEY(pane, ordinal)
) STRICT, WITHOUT ROWID;
```

`shared_view_panes` (the ownership index) disappears: `panes.id` is the
primary key and `(kind, resource)` is unique, so "one scope per pane" and "one
pane per hosted resource of a kind" are constraints instead of a separate index
kept in step by hand. The constraint matches `same_resource`, which compares
resources only within one kind (chat and terminal ids share a type). A draft
belongs to its pane and goes with it; closing a pane with a draft keeps the
draft text in its `closed_panes` row's descriptor, as reopening does today. `shared_view_terminal_sources`, `channel_work_placement_epochs`
and `shared_terminal_retention_pins` keep their columns; the sources and pins
reference `panes`, and placement epochs stay keyed by pane id without a
foreign key, deleted when their pane leaves.

### What is a column and what is a descriptor

Everything that is keyed, joined, filtered, ordered, constrained or edited
independently is a column: pane identity, scope, order, width, titles, kind,
the hosted resource, the provider and provider session, and the working
directory of closed chats. Collections are tables (`panes`, `retired_panes`,
`view_operations`, `draft_attachments`), never arrays inside a value.

The descriptor is what remains of a `CardKind` payload: a launch recipe with
its argument list and environment map, a host session proof, terminal
continuity, a browser history. These are nested values that are only ever
read and replaced whole, together with their pane. They are stored as JSON with
an explicit `descriptor_format`. JSON is chosen over a binary encoding such as
postcard on purpose: it is self-describing, so a reader can skip a field a
newer writer added and a newer reader can default a field an older writer
omitted. A positional binary encoding would make every added field a format
break that older binaries, including a rolled-back installation, cannot read.
`descriptor_format` changes only when a field changes meaning; readers refuse
formats they do not know instead of guessing.

### Ordering and revisions

`position` is a sparse integer (`1 << 20` apart on insert at the end, the
midpoint on insert before a pane). When two neighbors are adjacent, the
transaction renumbers that scope, which costs O(panes in the scope), a handful.
Order is `(position, id)`, so a tie is still a total order; there is no unique
constraint on position, which SQLite would check row by row in the middle of a
renumbering update.

A scope's revision is drawn from one model-wide clock, `view_revision_clock`,
when the scope changes, not counted per scope from 1. A scope that is deleted
and later recreated therefore never repeats a revision a stale client still
holds, so its `expected_revision` conflicts and rebases instead of matching by
accident. The clock is its own row rather than `metadata.sequence` because
adopting or rebinding a pane changes its view without advancing the model
sequence. Conversion starts the clock at the largest stored revision. Schema
22 has no clock, so a store converted back and forward again restarts it at
the largest revision that survived; a scope deleted under 23 whose revision
was larger than every surviving one could then be recreated with a revision
a stale client still holds. That needs a rollback and a re-upgrade between
the client's last read and its next edit, and is accepted.

Writing a view compares it with the stored rows and changes only those that
differ: a moved pane updates its `position`, a retitled pane its row, and the
scope its revision. Panes on the longest run whose stored positions still
increase keep them; the rest take midpoints between their neighbors.

## Write paths

### Daemon edits are row changes

This section is the target once closed history moves into the daemon (stage
5). Stage 3 already writes only the rows an edit changed, by comparing the
edited view with the stored rows, but still applies the edit to the view in
memory, keeps no `closed_panes`, and derives placement and retention from the
whole view before and after.

`ModelEdit::ChangeView` keeps its wire shape. Applying one is O(change):

| Change | SQL |
| --- | --- |
| `Insert`, `InsertIfEmpty` | look up `(kind, resource)`, the id in `panes`, `retired_panes` and `closed_panes`; if any holds it, record the operation and change nothing, as today; otherwise `INSERT INTO panes` |
| `Update` | `UPDATE panes SET ...` for the fields that differ between `before` and `pane` |
| `Move` | `UPDATE panes SET position` |
| `Remove` of a present pane | `DELETE FROM panes`, `INSERT INTO retired_panes`, `INSERT INTO closed_panes` |
| `Remove` of an absent id (seed closed lists) | `INSERT INTO retired_panes` only |
| `Transfer` | `UPDATE panes SET scope, position`; retire the id in the source and un-retire it in the destination; no `closed_panes` row |
| Agent retirement (`retire_agent`) | as `Remove`, with its own reason |
| Channel delete, project close | every pane of the affected scopes becomes a `closed_panes` row before the scopes cascade |

Constraint violations are never the way an edit is refused: every case the
current code treats as a recorded no-op stays a recorded no-op, checked before
any write. Each edit sets `view_scopes.revision` for the scopes it changed and
records its `operation` in `view_operations`. The conflict checks keep their current
semantics: `expected_revision` against the scope's revision, and
`source_update_conflicts` against the stored row. Other derived state that
`write` maintains today (channel auto-title reset, terminal retention pins,
channel work placement) observes the changed rows instead of diffing two whole
views.

`SeedView` remains while any window can hold a local copy (until stage 5);
it is applied as individual inserts with the same refusal rules. Because
every window start seeds its open panes, tombstones (`retired_panes`,
`closed_panes`) and empty scopes must not be collected before then: deleting
one would let a diverged window copy re-insert a closed pane.

### Subscribers get deltas

`ServerMessage::WorkspaceModelEdited` carries the edit and its effects
(`workspace_model::delta::EditEffects`): every changed scope's revision before
and after the transaction, the pane ids it closed, and canonical project,
channel, navigation and agent rows. `None` before identifies a new scope;
`None` after identifies a deleted one. A transfer names both scopes and closes
nothing. Channel deletion and project close name every removed scope and pane.
Canonical metadata includes assigned ranks, conditional title changes and the
settled navigation order, so none requires a full-model refresh.

The observer keeps a `delta::Mirror`. It applies a `ChangeView` when its held
revision equals the notification's base revision; otherwise it reads only the
affected scope with `ReadWorkspaceView { scope }`. Scope reads return the view
and the snapshot's model sequence. Seeds and execution changes also read only
the scopes they name. The UI installs complete changed views by revision and
metadata by per-entity sequence, preserving independently delivered changes
when an edit receipt overtakes an observer notification. A full read occurs
only on connection or registry identity changes. Full reads scan `panes` once,
ordered by `(scope, position, id)`.

For settled channel presentation, the UI projects only the changed scope and
refreshes pane chrome. This requires unchanged membership, source descriptors
and filing, complete ownership evidence, and no pending admission or lifecycle
obligation. A retained unavailable browser surface keeps its error and recovery
controls. Structural changes, inconsistent local placement and derived worktree
membership retain full reconciliation; this stage does not remove all global
projection work.

Current windows publish through `ApplyWorkspaceEdit`, whose `WorkspaceEdit`
reply carries an `EditReceipt` rather than the model. A revision conflict reads
only the source scope before retrying with the original operation identity and
source descriptor fence. Older CLI clients retain `ReadWorkspaceModel` and
`EditWorkspaceModel` with their original replies; their model watches are
refused because they cannot decode the new notification. Window ownership and
lifecycle checks use `ReadWorkspaceMetadata` for only the registry identity,
projects or agents they need; they never load panes or retired history.

Execution writes are captured by connection-private TEMP tables and SQL
triggers. Their inventory commits or rolls back with the rows, including
writes made by transaction helpers used by retained actors and automations.
Draining it reads only changed identities. Public edits publish their pending
execution inventory before applying their own transaction, so an interleaving
edit cannot erase an unannounced execution change. These tables do not change
the durable schema or its downgrade contract.

The initial subscription returns a `WorkspaceSnapshot` with closed identities
from retained tombstones, excluding panes that still have an owner after a
transfer. The window keeps these identities in a set, adds every notification's
closed ids before adoption, and retains its known closures across reconnects to
the same registry. A changed registry identity resets that set. Stage 5 adds
durable closed history for deleted scopes; until then the window keeps their
received closures and the existing channel/project deletion fences.

### Windows write only what changed

The window store holds per-window rows (focus, active scope, per-pane
presentation, layout, utility panes). The session writer receives typed
changes rather than whole `WorkspaceState` snapshots and upserts the affected
rows. The 250 ms debounce stays, applied per row key, so a scroll burst costs
one row write. A clean shutdown still flushes.

Drafts are written through the daemon (`EditDraft { pane, revision, text,
attachments }`), debounced in the window per pane and applied with a revision
check, so a stale window cannot overwrite a newer draft from another window.

## Lifecycle and garbage collection

| Data | Kept while | Then |
| --- | --- | --- |
| `closed_panes` chat rows with a provider session | 180 days, at most 5000 rows, and never while its resource is hosted or pinned | deleted oldest first |
| `closed_panes` other rows | 30 days, and never while its resource is hosted or pinned | deleted |
| `retired_panes`, empty `view_scopes` | until no window can seed from a local copy (stage 5) | then 30 days |
| `view_operations` | 7 days after `applied_at_unix_ms` (`OPERATION_RETENTION`); conversion from schema 22 starts every existing row's age at the conversion | deleted by the daemon's runtime collector, at most 5000 rows every 10 minutes. `operation_seen` is the only replay guard, and a replay it misses fails the revision check as a *retryable* conflict, so a rebasing client could apply it twice. A window's publisher therefore never resends a pane change more than 24 hours (`REPLAY_LIMIT`) after first sending it, and a test holds the retention at six times that. A pending terminal's insert, whose operation is the pane's id, is sent again by a restarted window regardless of age; that is safe because an insert of a pane the view holds or retired changes nothing |
| Worktree scopes whose project is closed | cascade, after their panes become `closed_panes` rows | deleted with the project |
| `.final` receipt snapshot | until the daemon commits the actor's retirement | rewritten without the transcript and marked `compacted`; the proof fields stay so same-boot readers keep validating, and reading it as a transcript is an error |
| `.final` of an actor that ended without retiring | until reboot | kept whole: a close whose reply was lost publishes its transcript from it |
| `.final` and `.retired` receipts | until their ownership row is collected | deleted, `.retired` first; ownership rows need their own lifecycle (below) |
| Transcript cache, drained | until its store drops | deleted (today) |
| Transcript cache, undrained, unleased | 24 hours after the newest write in the directory | deleted |
| `native_actor_ownerships`, `native_actor_retirements` | not collected today | to be designed: collectable once the boot has changed, or the pane is permanently closed and its retirement committed |

Collection runs in the daemon on a background thread every 10 minutes and
once at start, each pass bounded in rows so a large backlog drains over a few
passes instead of one long transaction. Transcript pruning
(`SessionRuntime::prune_transcripts`) keeps the transcripts of every resource
in `panes` and `closed_panes`, and removes nothing when the daemon cannot be
asked, so a transcript lives as long as the closed pane that names it.

## Provider history index

The daemon keeps a persistent index per provider transcript file in its data
directory, keyed by the file's device and inode and validated by its size,
modification time and a hash of the bytes up to the indexed end offset
(maintained incrementally as the index grows). Visibility is not a property of
one record: a later record can retract earlier ones, and the shown branch is
the newest visible leaf of the whole graph. The index therefore stores every
node (uuid, parent, order, byte offset and length, whether it is a message),
the retracted set, and the tool and agent references, and recomputes the chain
from them on load, which is in-memory work over small rows. Opening a
conversation:

1. Validates the index against the file. If the file only grew, indexes the
   appended bytes from the stored end offset.
2. If the indexed prefix changed or the file was replaced, re-indexes from 0.
3. Recomputes the visible chain and serves the newest page by seeking to
   stored offsets.

Reopening a 60 MB conversation then reads the appended bytes and one page,
not the whole file. The request and reply shapes do not change.

## Migration

Migration runs once in the daemon on the first start of the version that ships
stage 5, when the update flow has closed every window (the existing full
upgrade path requires this), and inside one transaction. A backup copy of
every input store is written with `VACUUM INTO` under
`backups/persistence-<timestamp>/` first.

Inputs: the daemon's views and tombstones, `channels`, `projects`,
`closed_projects`, and the window stores of this device returned by
`instance::window_database_paths()`. `device-*` slots belong to another
device's daemon and are migrated by that device's daemon. Stores that no
running or restorable window uses (inactive snapshots, `recovery-*` slots)
contribute only closed history, never open panes.

A tombstone is user intent unless its reason is "absent from a newer
workspace snapshot", which a window writes for rows it did not mention. That
covers "closed by user", deleted channels, removed projects and worktrees,
and tombstones with no reason. `Existence::Closing` is an unacknowledged
close and is replayed as one.

Rules, per card id:

1. A pane in a daemon view stays in that view. The daemon has been the
   membership authority since view schema 8; a window copy cannot remove it.
2. A pane the daemon closed stays closed: its id is in a view's retired set,
   its channel no longer exists, or its project is closed.
3. A card with a user-intent tombstone in any store and in no daemon view is
   closed. A card open in one store and closed in another is never reopened.
4. A card with a user-intent tombstone that the daemon still holds in a view
   stays open (rule 1), and its pane shows that the close did not reach the
   daemon, with Close as the action. Closing it then is one daemon edit.
5. A card that is open in every active store holding it, in no daemon view,
   and not closed by rule 2 is placed in the view of its saved filing
   (`card_scopes`) if that scope exists. This is the #2429 pane that no view
   held. Without a live scope it becomes a closed pane, which keeps it
   resumable without inventing a placement.
6. Every closed card becomes a `closed_panes` row, with its descriptor when a
   store holds one and its identity alone otherwise.
7. Drafts: per pane, the most recently saved non-empty draft wins; other
   windows' different drafts are kept beside it as notices rather than
   discarded.
8. Per-window state (focus, active scope, layout, presentation, utility panes)
   goes into each window's new store from its own old store.

After a successful migration the old window stores are left untouched and a
marker file beside each records that it was migrated. Every reader that
enumerates window stores today moves to daemon queries in the same stage: the
upgrade inspector (`read_open_migration_sessions`), transcript pruning
(`read_live_terminal_ids`), session links (`read_navigable_session_ids`) and
project removal (`read_project_session_owners`). Enumeration skips migrated
stores, so an old copy can never feed a later upgrade or seed.

## Compatibility and rollback

- The daemon schema changes in stages 3 and 5 are new `user_version`s. The
  daemon compares the store's whole layout with the one it expects, so an
  older daemon refuses the migrated store outright. Restoring the pre-migration
  backup is not a rollback: it discards every daemon write since, including
  native actor ownerships and retirements that running helpers depend on.
  Each schema stage therefore ships a converter back to the previous layout
  (`cagents daemon downgrade-store <version>`, which runs the matched
  `cagentsd --downgrade-store`), run by the installer's rollback path before
  it starts the previous daemon, and tested in both directions on a
  realistic store. The backup is kept only for manual recovery.
- A downgrade step must leave a store the older daemon can edit, not only
  open. Restoring a table the newer schema dropped means restoring its
  invariants too: the view writers of schemas 22 and 23 refuse any edit to a
  view holding a pane without a `channel_work_placement_epochs` row, so a step
  that brings back `channel_work` tables writes one epoch per pane and sets
  the placement clock, as the channel-work migration does.
- The wire changes (scope revisions on edits, `ReadWorkspaceView`, draft
  edits) bump `PROTOCOL_VERSION`. Window and daemon versions are a matched set,
  so the in-app update performs a full daemon upgrade, with its existing
  confirmation and checkpoint, when crossing this boundary.
- New windows use a new store file name per slot, so an older UI that is
  restored by rollback finds its own `sessions.sqlite3` unchanged.
- Stages 1 and 2 change no wire contract and no daemon schema; they ship as
  compatible updates. Neither changes a window-store schema either.

## How this resolves the reported problems

- #2429 (closes that do not stick, unreachable panes): there is one membership
  record. A close is one daemon transaction; no window copy can keep or revive
  the pane. Migration rule 4 makes the unreachable pane reachable.
- #2440 (50 s startup with thousands of views): view listings are one query and
  pane searches skip empty views (stage 2); startup stops loading 1700
  tombstoned cards once closed history moves to the daemon (stage 5); empty views
  are collected once no window can seed (stage 5). Projection work scales with
  open panes.
- #1064 (tmpfs transcript caches): orphaned caches are swept continuously with
  bounded retention, and retired receipts shrink to their proof. Bounding a
  live chat's transcript WAL, the other half of #1064, is separate work.
- Save cost: a flush writes the rows that changed; nothing in the steady state
  is proportional to history.

## Alternatives considered

- Keep per-window copies and reconcile harder. Every reconciliation rule so
  far has needed another exception (the `SeedView` closed list, `retired`,
  adoption of re-pointed chat identities). Two writers for one fact keep
  producing divergence; removing the second writer removes the class.
- Event-sourced log as the store. The edit log would be the source of truth
  and views a projection. It makes compaction mandatory and every read a
  fold; the workspace needs current state with a short retry-dedupe window,
  which tables give directly.
- A CRDT document per scope. Windows are online clients of one local daemon;
  there is no partition to merge across, so a CRDT's cost buys nothing.
- Binary descriptors (postcard). Smaller, but positional: see "What is a
  column and what is a descriptor".

## Stages

Each stage is an independently mergeable PR with tests and a before/after
measurement on a realistic-size workspace.

1. Runtime directory (#2481): compact `.final` receipts after the daemon
   commits retirement; reclaim unleased journals 24 h after their last write;
   both at startup and every 10 minutes. Deleting receipts waits for the
   ownership-row lifecycle.
2. Store quick wins without wire or daemon schema changes (#2489): navigation
   metadata is written only when its bytes change, and maps are serialized in
   key order so equal navigation is equal bytes; an unchanged chat skips the
   stored-row identity comparison; the archived-descriptor flag and pending
   admissions no longer cost a query or a scan per card; daemon view listings
   use one query and pane searches read only occupied views. An unchanged
   flush of 1769 cards writes 1 row instead of 16 and 1 WAL page instead of
   5. Deferred: edit-log pruning (see the lifecycle table), single-query card
   load and a `saved_conversations` index (both measured at a few
   milliseconds, within noise), and skipping closed cards before serializing
   them, which stage 5 does properly by dropping closed cards from window
   state.
3. Normalized daemon view schema (`view_scopes`, `view_revision_clock`,
   `panes`, `retired_panes`, `view_operations`), row-level edits, 7-day
   operation retention, and `cagents daemon downgrade-store`, with which a failed
   update converts the store back to the schema it recorded before the
   replacement daemon started; no wire change.
   Conversion keeps every pane a view holds open, in list order. A pane
   listed in two views stays in the one schema 22's index named, and a second
   pane for one resource gives way to the first, as schema 22 resolved both;
   the view a pane leaves retires its id and takes a new revision, so neither
   a stale seed nor a stale edit can bring it back, and a pane that gives way
   loses its retention pin with its placement. Only an unreadable view
   refuses, leaving the store at 22. On a copy of the user's store (1858
   views, 88 panes, 46012 applied operations; `local` profile on fw16,
   medians of 25, two runs each): an edit to the busiest view costs 7 ms and
   29 to 49 KB of WAL instead of 9 to 11 ms and 58 to 82 KB; a window
   re-seeding panes the view already holds costs 6 ms instead of 21 to 24 ms;
   the model read costs 4 ms instead of 5 to 6 ms; open is unchanged at 3 to
   4 ms. Converting once takes 0.4 to 0.7 s and converting back 0.3 s. The
   file's contents shrink from 31.8 MB to 25.0 MB, but the file keeps its
   high-water mark (37.7 MB) as free pages SQLite reuses until the converted
   operations age out. Closed panes and drafts move to stage 5.
4. Deltas to subscribers and per-scope reads (#2573; protocol bump). Edges carry
   changed revisions and closed ids; a revision gap reads only that scope.
   Reconnects and changed registry identities establish full baselines, whose
   pane read is one ordered query. Settled channel presentation reuses scope
   projection and notification attribution; uncertain ownership or lifecycle
   state retains full reconciliation. Window-owned copies remain until stage 5.
5. Daemon-owned closed-pane history and drafts; windows become subscribers
   with per-window stores; migration from all window stores.
6. Persistent provider history index.
