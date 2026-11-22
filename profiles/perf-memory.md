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

## Corrected live census

On 2026-11-22, the installed `cagentsd` process (PID 25755) was measured with
a parser that recognizes a glibc arena as an adjacent anonymous `rw-p` mapping
plus anonymous `---p` mapping totalling 65,536 KiB. This admits grown arenas;
the former exact-132-KiB rule did not.

| Role | Chats | Processes | PSS | Private dirty | Notes |
| --- | ---: | ---: | ---: | ---: | --- |
| daemon | no controlled chat workload | 1 | 47.97 MiB | 16.61 MiB | 27 grown glibc arena reservations; 41.26 MiB arena RSS and 15.91 MiB arena private-dirty; 56.73 MiB SwapPss |
| UI | no controlled chat workload | 0 | — | — | no `cagents-ui` process was live |
| retained actors | no controlled chat workload | 0 | — | — | non-zombie actor census empty |
| Claude providers | no controlled chat workload | 0 | — | — | direct actor-child census empty |

This corrects the allocator claim behind #3619. It does **not** establish that
any particular actor has 27 arenas: this was a daemon sample, and a process
must be measured while it exists. The old actor structural report's “five/seven
arenas, ~0.09 MiB RSS” is withdrawn. A stack total is also withdrawn: adjacent
anonymous mappings are not a reliable stack identity, so the census reports
thread count and the named main stack only, never inferred guarded stacks.

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

The only complete live result is the daemon row above. It already shows a
systemic allocator effect: 1.69 GiB of virtual arena reservation can coexist
with 41.26 MiB resident arena pages. Reservation is not resident memory, so
`MALLOC_ARENA_MAX=1` is not a proven RAM optimization.

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
