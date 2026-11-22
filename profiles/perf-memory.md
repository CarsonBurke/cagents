# Memory profile

## What was measured

Resident memory is reported from `/proc/PID/smaps_rollup`: PSS is additive
across processes, while `Private_Dirty` is reclaimable only when that process
exits. RSS is retained only as a pressure signal: it double-counts shared text
and libraries.

The 20-chat workload required by this profile is one streaming retained Claude
chat and nineteen prompt-available retained Claude chats, in one isolated
development runtime. It was **not running** when this profile was written:
there were zero live retained actors and zero direct actor-child providers.
This profile deliberately leaves those cells unfilled rather than turning the
workstation's RSS observations into made-up PSS.

## Corrected live census

On 2026-11-22, the installed `cagentsd` process (PID 25755) was measured with
a parser that recognizes a glibc arena as an adjacent anonymous `rw-p` mapping
plus anonymous `---p` mapping totalling 65,536 KiB. This admits grown arenas;
the former exact-132-KiB rule did not.

| Role | Chats | Processes | PSS | Private dirty | Notes |
| --- | ---: | ---: | ---: | ---: | --- |
| daemon | no controlled chat workload | 1 | 47.97 MiB | 16.61 MiB | 27 grown glibc arena reservations; 42.23 MiB arena RSS and 15.91 MiB arena private-dirty; 56.73 MiB SwapPss |
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

The same census must be taken before and after any proposed change. In
particular, #3613 needs a 2 MiB result-frame before/after PSS comparison, and
#3619 needs 1- and 20-chat PSS/private-dirty evidence before an allocator cap
can be claimed as a memory win.

## Where memory is likely to go, and the systemic fix

The only complete live result is the daemon row above. It already shows a
systemic allocator effect: 1.69 GiB of virtual arena reservation can coexist
with 42.23 MiB resident arena pages. Reservation is not resident memory, so
`MALLOC_ARENA_MAX=1` is not a proven RAM optimization.

The retained-actor model is per-chat process overhead: its historical sample
is roughly 5.8 MiB PSS/actor. Replacing helpers with a shared actor process
could remove that fixed multiplier, but it changes isolation, failure and
resume contracts and is not justified before the 20-chat census.

Provider processes are the stronger systemic candidate, but only as a
hypothesis until their PSS is measured. If their per-provider PSS remains much
larger than actor PSS, daemon-owned, verified-idle provider retirement removes
an entire per-idle-chat provider process rather than shaving allocator metadata
inside it. That is the design in #3630: generation-fenced retirement with a
verified `claude --resume` successor, no UI polling, and no retirement for
ambiguous writes, permission waits, pre-ID sessions, or background work.

**Single highest-leverage systemic change:** retire verified-idle provider
processes and resume them transactionally on the next prompt, if the required
20-chat PSS census confirms providers dominate. It requires the #3630 design
review, daemon-owned deadlines, resume/race tests, and before/after PSS plus
first-prompt latency evidence. No lifecycle implementation should begin before
those gates pass.
