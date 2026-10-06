# HarfRust 0.5.2 overflow backport

Base: the crates.io `harfrust` 0.5.2 source archive, package checksum
`9da2e5ae821f6e96664977bf974d6d6a2d6682f9ccee23e62ec1d134246845f9`.
The archive's `src/`, normalized `Cargo.toml`, original manifest, and README are
retained. Its dependencies, public API, and OpenType table representations are
unchanged.

`LICENSE` is copied verbatim from upstream release tag `0.5.2`, commit
[`efdae3142ab76a2f1524d72cff9e3dfdc5afd7ca`](https://github.com/harfbuzz/harfrust/tree/efdae3142ab76a2f1524d72cff9e3dfdc5afd7ca).
The crates.io archive omitted the license file.

Backported fix: [upstream PR #397](https://github.com/harfbuzz/harfrust/pull/397),
head `c262b3f2d7d2f6e1a90a532e061075d3d8017c4e`, merged as
`6e85d3f8a8f54614b6cf4f267f4c8063181070ed`.

Only these library source files differ from the published archive:

- `src/hb/ot_layout_gsubgpos.rs`: widen the internal sequence counter and
  matching callback values from `u16` to `u32`.
- `src/hb/ot/contextual.rs`: propagate the widened matching values, converting
  font-derived `u16` classes and glyph identifiers at the callback boundary.
  The 0.5.2 `ToU16` context-rule abstraction is retained instead of importing
  newer read-fonts-dependent parsing abstractions.
- `src/hb/ot/gsub/ligature.rs`: widen the glyph identifier passed to matching.

Font counts, glyph data stored in font tables, lookup indices, and cached font
classes retain their original types. Overflow checks remain enabled. This does
not truncate or split application text to evade the shaper bug.

The existing cagents regression exercises the long Unicode JSON line that
previously overflowed the counter:

```sh
XDG_RUNTIME_DIR=/run/user/1000 CARGO_BUILD_JOBS=2 mise exec -- cargo test --locked \
  --profile dev-runtime --no-default-features --features development,ui --lib \
  structured_highlighting_is_bounded_without_truncating_full_content
```

Remove this Cargo patch when cosmic-text accepts a HarfRust release containing
this fix. The backport is tracked in [cagents #690](https://github.com/CarsonBurke/cagents/issues/690).
