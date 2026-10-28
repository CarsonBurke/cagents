# Source provenance and local change

The files in this directory originate from the crates.io `libghostty-vt-sys` 0.2.1 package (MIT OR Apache-2.0), package checksum `865fed12a8b2bba3507b3bccd0bef439e06ef1100e652fe6b71d132c41ee8db0`, source VCS `uzaaft/libghostty-rs` commit `46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0`, path `crates/libghostty-vt-sys`.

`Cargo.toml`, Rust bindings, public Rust API, and optional binding-generator source are unchanged. `README.md` has a local behavior note; `build.rs` and the tracked Ghostty patch contain the implementation changes. No Ghostty source tree is vendored. The build script retains upstream Ghostty commit `a887df42c56f6de86c0fe6da9c4eeca37931e083`.

The patch replaces the repeated serial-prefix lookup in incremental compression with an owned-list borrowed continuation. Central destruction, layout invalidation and prune reuse clear that continuation. It preserves the incremental candidate budget and verification passes, including progress across active-tail append. Native Zig regressions cover cold-node visit cost and cursor lifetime.

The build script uses a new patch-identity-specific subtree under Cargo OUT_DIR for source, installation and Zig caches. Exact source identity includes the full upstream revision and patch bytes; a matching directory requires an exact stamp and reverse patch check. A conflicting/incomplete directory is rejected rather than overwritten. Changed patch bytes trigger a rerun and select another directory. Caller GHOSTTY_SOURCE_DIR is never patched: it is checked read-only for the tracked change and rejected with an actionable error if unpatched. pkg-config cannot attest this private patch; enabling that feature warns and uses the patched source route.

`LICENSE` is copied verbatim from the wrapper VCS revision above (its root tree contains one MIT notice, no separate Apache notice). `patches/LICENSE.ghostty` is copied verbatim from the pinned Ghostty source for the code represented in the patch. The crates.io package's SPDX metadata is retained unchanged.

This donor is source-reviewed only. Build-script and native execution still require the shared lane.
