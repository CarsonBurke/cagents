# cosmic-text 0.19.0 joint local changes

Base: crates.io cosmic-text0.19.0 archive, SHA256
be17b688510d934ce13f48a2beba700e11583e281e0fda99c22bb256a14eda73.
The retained original manifest, README and MIT/Apache-2.0 licenses identify
the upstream package. Local library changes combine two independently
reviewed feature lines; neither standalone provenance statement replaces
the other's contract.

- PR1316 source9fb2ae2c0da9c1991440efbdc05f3747d401bd2c adds immutable
  consuming full-context LayoutSnapshot/projection APIs. Its buffer.rs,
  buffer_line.rs and layout_snapshot.rs bytes remain exact, as do snapshot
  exports and source maps. This union therefore DOES have an additive public
  API relative to upstream. LOCAL_CHANGES.md records the need and boundaries.
- Issue1473/PR1483 peer ecfe42e7c30d9c26dd780cbe9fd2f2dde4570f19 backports
  cosmic-text PR535 head0c5d9c507798bcb779e5fbdb48ae339face06f9c, plus its
  local overwide-blank correction. The final shape.rs is byte-identical to
  that peer: after an ordinary word-wrap break the final ellipsized line
  starts at the next word; an overflowing blank advances past that blank.
  Authored text and indentation are not trimmed or normalized.
- lib.rs adds the compile-time CAGENTS_CHECKOUT_ROOT identity anchor. The
  cache qualifier adds only the matching cosmic source-hook name, preserving
  PR1316's historical unanchored-producer/package-bootstrap and unrelated
  artifact-retention proofs.

The union retains PR1316's complete eight upstream integration-test targets,
two benchmarks, development dependencies, test profile, source/fonts and
rendering fixtures, then adds the peer wrapped_ellipsis target with its three
regressions. The peer standalone normalized-manifest omission of those tests
and development targets is NOT applied. Existing LayoutSnapshot contracts
and UI source/copy/Find/native-reading controls are preserved.

The focused regression uses existing bundled Open Sans plus the peer's
NotoSansHebrew.ttf and NotoSans-LICENSE, verified byte-for-byte against the
same0.19.0 archive's font and SIL Open Font License. No system-font fallback
or external download is needed for these fixed fixtures.

The root Cargo manifest/lock remain unchanged: one existing local cosmic
patch, no duplicate package/version choice, and the existing UI unicase
requirement. Library dependencies are unchanged by this union, while the
additive snapshot public API is deliberately preserved.

Prior peer focused/causal receipts and PR1316 source/native/test receipts
retain their original heads. This external source union is unbuilt/unrun.
Final composed isolated UI/integrated boundaries, retained upstream geometry
and snapshot oracles, cache identity, actual native title/reader behavior
and scoped performance evidence require independent qualification. The
upstream fix's merge/release status is not inferred from historical metadata;
remove the temporary shape backport only when a selected release includes it,
and assess the snapshot API separately rather than deleting its authority.
