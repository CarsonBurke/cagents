# PR1627 bounded HOST qualification

The user needs an actionable failure when a native provider exits before its
session starts, while retaining exact process ownership and a usable Close
action. The current change adds a bounded, sanitized stderr tail to the existing
actor error. It is not the direct-stdio full-log export requested by issue1584.

This private research branch adds exactly six files to production source
`0e86d6c5e51a261f113165f21051c3da384a7602` / tree
`ea2880ac0fae1061b5982800d99eecf8b0171d66`. The normal ancestry retains the
immutable `b1e4c1c4d713c0b23a8c6807a31612aa8aa4b0af` privacy control without
adopting its source. That control removes only withholding of a truncated
leading stderr fragment; the entire final owning test module is byte-identical.
The manifest pins both sources, three changed product files, test-module bytes,
supervisor, workflow, setup and harness. No Cargo or tests have run for this
qualification, and publication/execution still requires the lead's grant.

## One finite experiment

The single Ubuntu22 job has a 60-minute hard limit. Its first step records an
absolute 50-minute execution deadline before checkout/setup, with ten minutes
nominally reserved for cleanup and upload. Rust1.93 and checksum-pinned Zig0.15.2
use the qualified setup bytes already present in the product tree; this is setup
provenance, not a claim that a previous whole workflow qualified this source.

All phases run serially with two build jobs and one private target:

1. Exact0e86 HOST `cagentsd` bin check.
2. HOST libtest compile; select the single owning artifact from that Cargo JSON,
   checking profile, features, target custody and executable hash.
3. Six exact owning parents listed in the manifest: privacy fragments,
   small/flood stderr with adoption, continuous stderr with Close/reap, exited
   original directory refusal, partial/oversized frames, and legacy adoption.
4. Only after every positive passes, compile exactb1e4 and run the identical
   privacy test. It must return actual101, the named owning failure and final
   0P1F, with the specific `!summary.contains(CANARY)` assertion. A compile error,
   timeout or unrelated panic never qualifies the control.

The corrected bb1 observer and kernel closure predicates are copied exactly
from the reviewed supervisor. Every phase must prove actual exit, exact source
and argv, leader reaping, sampler closure, no remaining owned members and no
uncertain membership before the next phase. Metrics are a subset. Cancellation
uses the retained observer child's pidfd after its handler is installed. Missing
closure holds the job and forbids target reuse; it cannot become a passing
result. No numeric PID/PPID ownership assumption or local B admission is used.

## Caller, helper and environment

The first workflow step admits only a dedicated hosted runner where
`CAGENTS_AGENT_AUTHORITY`, `CAGENTS_AGENT_TOKEN` and `CAGENTS_SESSION_TOKEN`
are all naturally absent. It enumerates environment names only, so an empty or
non-Unicode value still causes refusal, with marker names as the only diagnostic.
This check precedes budget-file/environment writes, checkout, toolchain setup
and private runtime setup. The harness repeats the same three-name refusal
before any child, source checkout, observer, Cargo or actor work. Neither check
reads, hashes, removes or replaces a role-marker value, nor rewrites HOME,
no-start or account selections.

The first frozen research head `da795671` checked only authority and did so too
late to guard setup; the lead found this real admission gap before publication.
That head and its source-only review remain preserved. This normal private
successor requires fresh whole-composition review; its earlier source clearance
does not approve this revision.

This older exact source has no caller-scope preflight in HostLease acquire or
native transport spawn. Its test actors therefore are not authorized for local
execution under the inherited Agent scope. The three actor parents use private
runtime/state directories and synthetic Python or shell children. The holder
is the same freshly compiled `/proc/self/exe`, entered by its existing init-array
handler with private native protocol3. No installed helper, daemon, public
socket, actual provider login, network or public application is needed.
`authorize_command` checks only explicit provider-home path selections, which
these fixtures leave empty. The production synthetic Spawn command itself
clears its child environment; the research supervisor does not.

Two actors use the existing90-second isolated fixture wrapper. The original
directory actor's inner self-exec has no deadline, so the outer180-second phase
and complete bb1 family cleanup are mandatory. Pure privacy, frame and legacy
JSON selectors do not create provider actors.

## Native evidence still required

No actual native evidence was recovered for either the old2f715 fixture proposal
or current0e86. Its earlier five HOST parents and unrelated boundary checks are
historical only. The UI and public protocol source are unchanged by1627; this
does not establish the newly populated error state, details or clipboard text.

Use actual matched development-runner artifacts and a legitimate natural User
operator, or an independently qualified authority-preserving diagnostic route.
Do not reuse the old normal-UI/private-HOME bootstrap under inherited Agent
authority. A small synthetic native state can validate display only if the
lead approves it; it cannot qualify provider startup, auth or holder custody.

The missing affected comparison is an actual native error after a synthetic
provider's natural exit, beforec07 versus exact0e86, with matching data/theme/
scale/viewport. Capture collapsed and expanded Details at wide and narrow
widths, visible exit status and final useful lines, omission/invalid-UTF8 notes,
and absence of an intentionally synthetic credential canary. Verify Copy details
contains only the sanitized display, text selection/scrolling/focus works, Close
cleans up the exact original and a subsequent ordinary action completes.
The b1e4 privacy control may be inspected with synthetic content only and never
delivered. Any display-only fixture must label missing actor/provider coverage.

Before/After WebP screenshots belong on a separate screenshots branch and must
be embedded as two image columns, with state description below each pair.
This research does not install, package, alter the public desktop, close1584,
establish universal secret detection, or make1627 ready to merge. The rejected
1627 body update must not be retried through another route.
