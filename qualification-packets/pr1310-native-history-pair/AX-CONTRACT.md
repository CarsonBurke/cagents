# History accessibility access limits

This is a correction to the real `HistoryDriver` from
`f86fbeaf50b38260e575e59e122d1f77932038fd`, not an execution entry or a
replacement accessibility implementation. The Before/After actions, physical
input, alpha checks, capture/PIL fences and query-result observations keep their
original task. Nothing in this change qualifies native results or authorizes a
desktop, process, account, service signal or resource release.

The installed source contract is recorded in
`/tmp/coord-b9-pr1310-f86-actual-AX-timeout-contract.json` (SHA256
`c5e0056fe92dddced63f5a580eb917933e1f7fd1fba3a98691eb54064ad43a48`).
The driver still needs the real modules and original owner; its standalone
`ROOT_INTERFACE_UNSUPPLIED` refusal remains unchanged.

## Actual API and original budget

Installed `pyatspi/Accessibility.py:254` aliases `setTimeout` to
`Atspi.set_timeout`. The installed `Atspi-2.0.gir` and `atspi-misc.h` declare
`void atspi_set_timeout(gint val, gint startup_time)`, with both arguments in
milliseconds. The driver calls the real setter immediately before each access:

1. Check the same original owner, source, cancellation and absolute budget.
2. Floor the positive remaining seconds to milliseconds, cap at 200, and refuse
   a remainder below one millisecond. There is no minimum rounding up or reset.
3. Call `setTimeout(milliseconds, -1)`, then one real accessor.
4. Check the original root again before using the returned value.

The second argument `-1` disables startup grace. The normal timeout argument is
always positive; its `-1` value would disable the timeout and is never used.
No reliance is placed on undocumented zero semantics in the installed build.
The setting is process-global, so concurrent accessibility callers are not
admitted by this source.

Every actual getter/property/interface call in this driver uses `_ax_call`:
Registry desktop, child counts/children, application PID, role/role name/name,
state set/contains, Component interface/extents/focus, Action interface/count/
names, and Text interface/character count/text. Constants and the returned
Python `BoundingBox` scalar properties are local data. The installed
`text.py:584-591` maps `characterCount` to `Atspi.Text.get_character_count`.

`grabFocus` is a side effect and is called once. If a native call raises, the
exact original exception is re-raised. A later failing postcheck is retained as
its cause, with a note identifying the actual chronology. If a native return is
followed by a failed postcheck, that return is not admitted evidence; an effect
may already have happened. Neither case permits an input retry. Read-only
polling retains the same original deadline and does not repeat an input gesture.

## Data and traversal limits

| Item | Limit and refusal |
| --- | --- |
| Desktop applications | Read count once; integer from 0 through 999 |
| Tree nodes plus pending references | At most 10,000, checked before enqueuing children |
| Children of one node | Read count once; integer from 0 through 999 |
| Tree path | At most 64 components; refuse expansion beyond the limit |
| Button actions | Read count once; integer from 0 through 16 |
| Name, role name, action name | String of at most 4,096 UTF-8 bytes |
| Query fixture map | At most 16 string values, each at most 4,096 characters |
| Query read | Bound actual `characterCount` by the longest admitted query, then `getText(0, count)`; require returned string length and a second actual count to equal the first |
| Snapshot nodes JSON | Account each complete node before retaining it; at most 1.5 MiB |

A missing child/state set, negative count, defunct object, unsupported interface
or oversized value refuses the observation. A partial tree or truncated string
is never reported as a complete observation. Query text is never requested with
the unbounded `getText(0, -1)` form or an end offset beyond its observed count.

The string caps are **post-return** checks: they do not preempt allocation inside
GI or libdbus. A preliminary character-length check bounds the driver's own
subsequent UTF-8 encoding allocation. Counts do not make a tree atomic. A changing tree
or character range can still raise or fail a consistency check, and remains a
refusal rather than fabricated absence. These are finite data limits, not a
measured responsiveness claim. Matching counts do not prove atomic content or
the identity of an applied submit/callback; that existing witness remains absent.

For a single successful traversal with `D` desktop apps, `N` tree nodes and `E`
tree edges, the public-access count is
`1 + 1 + 2*D + N + E <= 21,999`. Each description uses at most `12 + A <= 28`
additional wrapped accesses, including state membership reads. Thus a maximum
snapshot has at most 301,999 public-access dispatches. These counts exclude the
setter and original-owner checks, and are not counts of underlying D-Bus RPCs.
The absolute budget can refuse much earlier. Whole-flow read polling has no
invented fixed iteration count or successful-completion guarantee.

The original owner remains the only JSONL writer. Its 2 MiB per-record and
64 MiB total limits, original FD/source/generation/PID/XID/thread attribution,
and failure/cleanup handling are unchanged. Snapshot accounting reserves room
inside those limits; it does not override them.

## What the setter cannot prove

The upstream implementation examined was GNOME `at-spi2-core` tag `2.60.0`, whose
official mirror links commit
`d8ab833f0230fccc009c271d9f23f53df2a32c88`. Its
[`atspi-misc.c`](https://github.com/GNOME/at-spi2-core/blob/2.60.0/atspi/atspi-misc.c)
routes several method/property helpers through dbind, but its
[`atspi-accessible.c`](https://github.com/GNOME/at-spi2-core/blob/2.60.0/atspi/atspi-accessible.c)
PID lookup directly calls libdbus with the default timeout instead. The installed
libdbus header distinguishes `USE_DEFAULT (-1)` from `INFINITE`; no specific
default duration is inferred. Bus bootstrap, cache work and reentrant processing
have additional paths. One public accessor can perform several internal calls.
The exact installed C-library build has not been mapped to that upstream tag.

Importing GI/pyatspi/PIL and native library initialization are also outside this
setter. No modules are imported here as a fallback and no timeout thread,
per-call subprocess, replacement AX stack or guessed environment knob is added.
The unchanged `0ffb14657a90fb56d62130f8c29492c691844c06` owner pins its original
thread and entry frame; moving these calls elsewhere silently would break that
custody. Root must still supply reviewed original-entry containment/cancellation
for these seams or establish complete supported installed-call coverage.
Kernel metadata I/O, conversion, Python scheduling and native reentry remain
cooperative. This correction does **not** establish a hard per-accessor or whole
wall-time bound.

Actual functional AX values can support the limited observed flow even when
service exit statuses remain `None`. No API address, source hash, requested
timeout, Cargo result or successful record write may be promoted to `AX0`, native
qualification or clean service closure. All actual imports, accessibility calls,
timeouts, cancellation, native images, performance and UX results remain
**NULL/UNRUN**.

## Owning oracle descriptions, not executed tests

- A remainder below one millisecond must refuse before setter/accessor dispatch;
  no rounded-up request or input can occur.
- A native exception followed by a postcheck exception must preserve the native
  exception as primary and the later failure as cause; no second `grabFocus` or
  physical input may follow.
- A successful `grabFocus` return followed by a failed root postcheck must remain
  an uncertain effect, with no retry or physical Return.
- A desktop/node count read must be used once; negative or excessive counts,
  pending overflow, depth overflow and missing children must refuse before a
  partial tree can be recorded.
- Excess actions, name bytes or incremental snapshot bytes must refuse without
  truncating or manufacturing an empty result.
- Empty query text must read the valid `0..0` range. Oversized character count or
  a count/text race must refuse; no whole-text read is allowed.
- Static wrapping inventory must cover all native calls and properties while
  the original tool arguments, capture, physical gestures, result polling and
  standalone refusal remain conserved.

These descriptions are source oracles only. They have not been implemented as a
mock consumer or run as extracted functions or native cases.
