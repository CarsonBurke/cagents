# Releasing

- [Building a release](#building-a-release)
- [Artifacts](#artifacts)
- [Publishing](#publishing)
- [Comparing package variants](#comparing-package-variants)

Release builds and publication run **locally**, never in GitHub Actions.
Pushing a tag does not start a hosted build.

## Building a release

`scripts/build-release-linux` builds an exact Git tag in a private, rootless
Ubuntu 22.04 environment with Rust 1.93, Zig 0.15.2, and Git 2.55. This
keeps the glibc 2.35 baseline instead of inheriting a newer workstation's libc.
The builder runs locked release-profile tests for the isolated CLI, host,
browserless UI, integrated UI, and editor boundaries, then packages and checks
the shipped binaries and desktop metadata.

Install `bubblewrap`, `curl`, `git`, and the standard archive tools locally,
then build:

```sh
./scripts/build-release-linux --ref v0.1.2 --output /tmp/cagents-v0.1.2
```

Run `./scripts/build-release-linux --help` for the cache location and other
details. `CARGO_BUILD_JOBS` defaults to 2 inside the builder.

## Artifacts

- `cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `cagents_${VERSION}_amd64.deb`
- `cagents-${VERSION}-sizes.txt`
- `cagents-${VERSION}-artifact-comparison.txt`
- `SHA256SUMS`
- `install.sh`
- `cagents-browserless-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `cagents-browserless_${VERSION}_amd64.deb`
- `cagents-browserless-${VERSION}-sizes.txt`
- `SHA256SUMS-browserless`
- `ceditor-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `SHA256SUMS-ceditor`
- `release-metadata.json`

Artifact selection is explicitly platform-aware. Only Linux x86_64 GNU has a
supported build and package lane today. Windows and macOS need their own native
ports, validation, packaging, and signing before their targets can be enabled;
the release tooling does not publish placeholder binaries for them.

## Publishing

Verify first, then publish:

```sh
./scripts/publish-release --version 0.1.2 --dist /tmp/cagents-v0.1.2 --verify-only
gh auth login --hostname github.com
./scripts/publish-release --version 0.1.2 --dist /tmp/cagents-v0.1.2
```

Both manifests and the existing tag must agree on the release version. The
publisher verifies local and remote tag identity, uploads a private snapshot to
its own draft, downloads and verifies the served bytes, then publishes. It
never moves tags or overwrites a published release. Authentication stays in the
local `gh` configuration, outside the build environment.

## Comparing package variants

`scripts/package-linux.sh` builds the full package by default and writes
browserless build outputs to a separate Cargo target directory. Run both
variants to compare their measurement reports without either overwriting the
other:

```sh
./scripts/package-linux.sh --variant full
./scripts/package-linux.sh --variant browserless
cat dist/cagents-"${VERSION}"-sizes.txt \
  dist/cagents-browserless-"${VERSION}"-sizes.txt
```
