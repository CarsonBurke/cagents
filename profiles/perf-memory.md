# Memory profile

## What was measured

Resident memory is reported from `/proc/PID/smaps_rollup`: PSS is additive
across processes, while `Private_Dirty` is reclaimable only when that process
exits. RSS is retained only as a pressure signal: it double-counts shared text
and libraries.

The native 20-chat fixture evidence for #3613 uses twenty settled Claude
chats, each with a 2 MiB tool-result frame. It is representative of the
idle-retention memory problem, but it is not the separate one-streaming,
nineteen-idle workload required to profile active streaming costs. The latter
remains unmeasured; no number below is extrapolated from RSS.

## Native 20-chat fixture census

`native/3613/pair.log` compares `main` `83192f4f9` with the frame-slack
release. All figures are KiB from the same isolated native fixture after every
chat settled.

| Role | Processes | PSS before → after | Private dirty before → after |
| --- | ---: | ---: | ---: |
| UI | 1 | 140,699 → 144,733 | 132,712 → 136,504 |
| daemon | 1 | 45,269 → 43,225 | 42,288 → 40,212 |
| retained actors | 20 | 257,061 → 211,945 | 208,812 → 163,824 |
| fixture providers | 20 | 17,807 → 17,831 | 17,192 → 17,224 |
| total | 42 | 460,836 → 417,734 | 401,004 → 357,764 |

The release cuts actor PSS by 45,116 KiB and actor private memory by 44,988
KiB: about 2.2 MiB per retained chat. Total private memory falls 43,240 KiB
(10.8%). This is the measured basis for #3613. The fixture provider is a tiny
shell process, so its row does not estimate a real Claude/Node provider.

## Corrected live daemon census

On 2026-11-22 at 09:46–09:47 UTC, `/proc/PID/smaps_rollup` was sampled three
times for each live daemon. Every reported field was identical across the three
samples. This is a live workload, not the required controlled 20-chat fixture:
the shared daemon was hosting agent sessions, while the isolated daemon had no
identified retained actors.

| daemon | Threads | PSS | Private dirty | Anonymous | SwapPss | glibc 64 MiB reservations |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| shared, active PID 25755 | 23 | 87,023 KiB | 65,124 KiB | 77,544 KiB | 34,964 KiB | 28 |
| isolated, quiet PID 3532793 | 22 | 8,618 KiB | 2,412 KiB | 2,892 KiB | 2,700 KiB | 25 |

An arena reservation is an adjacent anonymous `rw-p` and `---p` pair totalling
65,536 KiB. That admits grown arenas; the former exact-132-KiB rule did not.
For the active daemon, its 28 reservations had 76,668 KiB RSS/PSS and
64,212 KiB private-dirty (the paired guard regions reserve 1.64 GiB without
being resident). The quiet daemon retained 25 reservations but only 1,840 KiB
RSS/PSS and 1,820 KiB private-dirty in them. Its 1.56 GiB of guard address
space is therefore not a memory saving opportunity by itself.

The active daemon's mapping census attributes approximately 77.2 MiB PSS to
anonymous mappings, 9.4 MiB to `cagentsd` text/relocations, 0.2 MiB to the
heap, 0.04 MiB to shared libraries, and 12 KiB to its named main stack. The
named stack is deliberately not multiplied by thread count: adjacent anonymous
mappings are not a reliable stack identity. The active daemon has 35.0 MiB
SwapPss, so its resident PSS must not be presented as total allocated state.

This corrects the allocator claim behind #3619. It does **not** establish that
any particular actor has 28 arenas, or that reducing `M_ARENA_MAX` would reduce
resident memory: the quiet daemon is the counterexample. The old actor
structural report's “five/seven arenas, ~0.09 MiB RSS” is withdrawn. A process
must be measured while it exists and while its workload is known.

The earlier two-actor observation (37.29 MiB summed RSS, 11.59 MiB summed PSS,
0.59 MiB private-dirty) came directly from `smaps_rollup`, not the flawed arena
classifier. It is useful historical context (~5.8 MiB PSS/actor), but is not a
fresh 20-chat result and must not justify a change by itself.

The workstation report's provider range, 70–420 MiB RSS, is likewise not a
PSS measurement. Node executable and library pages can be shared, so neither
that range nor its sum establishes the providers' private footprint.

## Required 20-chat census

Run from a genuine user shell, with one isolated runtime and no unrelated
cagents processes selected:

```sh
/usr/bin/python3 -I scripts/measure-cagents-memory \
  --environ "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
```

Record one row each for UI, daemon, actors, and direct actor-child providers,
including process count, PSS, `Private_Dirty`, `Anonymous`, `Swap`, and
`SwapPss`. Capture it after the nineteenth idle chat settles and while the
twentieth streams. For allocator analysis, identify each 64 MiB glibc
reservation by the adjacent-map sum rule above; do not infer stacks from
anonymous-map adjacency.

The same census must be taken before and after any proposed change. #3613 now
has its 2 MiB result-frame comparison above. #3619 is closed: allocator arena
reservation alone is not a RAM win.

## Where memory is likely to go, and the systemic fix

The live daemon samples identify allocation retention, not an allocator knob:
the active daemon's arena mappings contain 76.7 MiB PSS, while the quiet daemon
keeps nearly as many reservations at 1.8 MiB PSS. Reservation count and virtual
address size are not resident memory, so `MALLOC_ARENA_MAX=1` is not a proven
RAM optimization. A controlled 19-idle/1-streaming census must first attribute
the active daemon's anonymous pages to long-lived state versus allocator
high-water marks.

The retained-actor model is per-chat process overhead. In this fixture it is
10.35 MiB PSS per actor after the frame release, and the release eliminates the
largest measured private high-water allocation. Replacing helpers with a shared
actor process could remove part of that fixed multiplier, but changes isolation,
failure and resume contracts; it needs a separate design and the streaming
workload before it is proposed.

Provider retirement is withdrawn by the user and #3630 is not planned. No
provider lifecycle implementation should be pursued from this profile.

**Single highest-leverage systemic change still worth evaluating:** consolidate
the per-chat retained-actor helper model only if the streaming 20-chat census
shows its fixed PSS multiplier remains dominant after #3613. It would require
an explicit shared-process isolation/recovery design, not an idle-retirement
policy. This document is a draft discussion artifact and is not intended to
land on `main`.
