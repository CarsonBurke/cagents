# cosmic-text 0.19.0 wrapped ellipsis backport

Base: the crates.io `cosmic-text` 0.19.0 source archive, package checksum
`be17b688510d934ce13f48a2beba700e11583e281e0fda99c22bb256a14eda73`.
The archive's complete `src/`, original manifest, README, and MIT/Apache-2.0
licenses are retained. Library dependencies and public API are unchanged.
The normalized manifest omits upstream integration-test/benchmark targets and
their development dependencies, since their image snapshots and sample corpus
are not vendored. It declares the focused cagents regression target instead.

Backported fix: [upstream PR #535](https://github.com/pop-os/cosmic-text/pull/535),
head `0c5d9c507798bcb779e5fbdb48ae339face06f9c` (open when backported),
with one local boundary correction described below.
Two library files differ from the published source:

- `src/shape.rs`: after committing a word-wrapped visual line, the final
  ellipsized line starts at the next word, not at the preceding separator that
  ordinary wrapping already omitted. The upstream correction is extended for
  an overwide word whose following blank overflows by itself: skip that blank
  just as the ordinary wrapping continuation does. This is an index correction,
  not trimming of authored text.
- `src/lib.rs`: the repository's standard `CAGENTS_CHECKOUT_ROOT` compile-time
  anchor prevents stale reuse when worktrees share Cargo artifacts. It is
  registered in `scripts/qualify-checkout-cache.py` with the other local patches.

The fix is shared by congruent LTR and RTL spans. Incongruent spans traverse
words backward and use an exclusive end index; that branch is unchanged.
No source text is trimmed or rewritten. Intentional paragraph indentation,
internal spaces, glyph wrapping, and one-line ellipsis retain their behavior.

The geometry regression uses the repository's existing Open Sans font and
`tests/fonts/NotoSansHebrew.ttf` from the same cosmic-text source archive. The
latter's upstream SIL Open Font License is retained alongside it. Its font
database is otherwise empty, making it independent of installed system fonts.
It compares ordinary wrapping with two- and three-line end ellipsis for LTR,
RTL, mixed-direction text, both word-wrapping modes, and two widths. It also
checks that intentional leading spaces survive. A separate overwide-word and
repeated-separator regression probes four widths (60, 88, 110 and 180). The
overwide-word case fails with the upstream-only backport at Word/60/two lines
(source byte34 rather than ordinary wrapping's35), and passes with the local
boundary correction.

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p cosmic-text --test wrapped_ellipsis
```

This dependency is only selected by UI feature boundaries. Remove the Cargo
patch when the selected upstream release contains the fix. Tracked in
[cagents #1473](https://github.com/CarsonBurke/cagents/issues/1473).
