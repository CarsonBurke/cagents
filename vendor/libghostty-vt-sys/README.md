> cagents carries a local incremental-compression correction. This fork builds
> its tracked patched source, including when the `pkg-config` feature is enabled;
> `GHOSTTY_SOURCE_DIR` must already contain the patch and is checked read-only.
> See [LOCAL_CHANGES.md](LOCAL_CHANGES.md) for source and cache behavior.

# libghostty-vt-sys

Raw FFI bindings for libghostty-vt.

- Fetches and builds `libghostty-vt.a` from ghostty sources via Zig by default.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Static linking is the baseline rather than a Cargo feature. Enable the
  additive `link-dynamic` feature to link the shared library instead.
- Set `GHOSTTY_SOURCE_DIR` to force the build to use a local Ghostty checkout.
- Set `GHOSTTY_ZIG_SYSTEM_DIR` to force Zig package resolution through a
  pre-fetched `zig build --system` directory. This is intended for Nix and other
  sandboxed package managers that cannot fetch during build scripts.
- Set `LIBGHOSTTY_VT_SYS_OPTIMIZE` to `Debug`, `ReleaseSafe`, `ReleaseFast`, or
  `ReleaseSmall` to override the Zig optimize mode used by vendored builds.
- The `pkg-config` feature keeps using the tracked patched source and emits a
  warning instead of selecting an unverified installed library. A supplied
  `GHOSTTY_SOURCE_DIR` must already contain the tracked patch.
- libghostty-vt is pre-1.0, so these bindings do not guarantee compatibility
  with arbitrary installed C API revisions.
