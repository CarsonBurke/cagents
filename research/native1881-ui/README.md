This temporary research branch adds compiler and owning headless test evidence
for frozen before `87d17f576` and assembled after `b816e5419` (`ad664eaf6` plus two reviewed test imports). It changes no
product source. Its extra parents retain the two exact source controls for a
single fetch and one serial hosted job; they are not adopted product changes.

The Ubuntu 22 job uses the byte-identical qualified1871 Linux setup, Rust 1.93.0,
Zig 0.15.2 and jobs 2. It checks the ordinary UI binary, compiles the ordinary
UI library and runs exactly seven product App tests. It then checks the
latency-harness UI binary, compiles its separate library test graph and runs
the exact owning fixture test. The same fixture test blob is used at before
and both controls. Controls qualify only on exit101 and their owning refusal
message; compilation failures, timeouts, unrelated panics or empty selectors
cannot satisfy them. A failed positive baseline prevents control qualification.

No HOST/UI union, default-full test invocation, daemon bootstrap, provider,
SDK, real lease or native window is part of this job. Caller authority, HOME,
account scope and no-start are inherited unchanged. Hosted CI is the actual
caller, without an invented User marker. Only presence of authority is recorded.
The fixture test's isolated children preserve that same authority and use only
their already-reviewed private XDG paths and rejection-only socket.

The copied observer is exact corrected `bb1e8fd0`, independently reviewed with
kernel ownership regressions. The six closure predicates are copied verbatim
from the reviewed R4 helper. No local B mutex/admission policy is copied. Each
command has fresh observer execution and kernel PID-file-descriptor custody;
the next phase may reuse the private warm target only after strict head/tree,
command, exit, leader reaping, empty membership uncertainty/remaining family,
and closed sampler receipt checks. Metrics describe an observed subset, not
whole-job totals. Unproved cleanup holds the hosted job without another phase.

The job is capped at 60 minutes. Its first step, before checkout/setup, records
an absolute monotonic deadline 50 minutes later; the harness consumes that same
residual budget across setup and every phase, with 15 second heartbeats. The
nominal final 10 minutes are reserved for cleanup/upload. Cleanup remains
mandatory beyond the execution budget. The workflow has no local launch
admission and is prepared privately
until independent exact source review and Root's publication decision. This is
a new temporary CI research action; it does not retry the rejected fixture
issue-create operation.

Actual native interactions/screenshots and a normally built matched
CLI/daemon/browserless UI+latency-harness artifact set through the reviewed
diagnostic runner remain separate gates, as does independent native UX review.
Integrated-browser coverage is a separate affected-consumer/production delivery
gate. These logs cannot establish any of those claims.

The original research head `2819e7a6b` and failed run `40782884760` remain
reachable. That run passed the ad664 UI binary check, then failed UI library
compilation on two missing Uuid imports; no tests or controls ran. The new
after source fixes only those two test imports, independently source-reviewed
with all other product entries conserved. Its actual compiler, test and control
results remain unrun until the next authorized qualification. The earlier
binary check is historical evidence, not a matching artifact for this source.
