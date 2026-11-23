# Releasing

- [Building a release](#building-a-release)
- [Artifacts](#artifacts)
- [Native platform roadmap](#native-platform-roadmap)
- [Publishing](#publishing)
- [Signing](#signing)
- [Comparing package variants](#comparing-package-variants)

Release builds and publication run **locally**, never in GitHub Actions.
Pushing a tag does not start a hosted build.

## Building a release

`scripts/build-release-linux` builds an exact Git tag in a private, rootless
Ubuntu 22.04 environment with Rust 1.93, Zig 0.15.2, and Git 2.55. This
keeps the glibc 2.35 baseline instead of inheriting a newer workstation's libc.
The builder runs locked tests for the isolated CLI, host, browserless UI,
integrated UI, and editor boundaries with the `release-test` profile: release
semantics (optimized, production identity, no debug assertions) without the
link-time optimization and single codegen unit of shipped binaries. It then
packages the shipped binaries with `release` and checks them and the desktop
metadata.

Before exposing the bundle, the builder installs and removes each Debian
package and extracts each portable archive in a disposable Ubuntu 24.04 runtime
image. This runtime checks actual package dependencies, shipped commands,
matched daemon startup and safe shutdown, live-daemon removal guards, and
preservation of unrelated files and saved app data. The compiler environment
remains Ubuntu 22.04; runtime smoke does not replace the glibc baseline checks.
The runtime image is prepared once with network access by
`scripts/prepare-release-smoke-rootfs`; every artifact execution uses a separate
networkless namespace with private user state. The runtime cache defaults to
`$XDG_CACHE_HOME/cagents-release-smoke/ubuntu-24.04-amd64` (or under `~/.cache`),
and `CAGENTS_RELEASE_SMOKE_CACHE` overrides it.

Install `bubblewrap`, `curl`, `git`, and the standard archive tools locally,
then build:

```sh
mise run release:build -- --ref v0.1.2 --output /var/tmp/cagents-v0.1.2
```

`release:build` runs `scripts/build-release-linux`; its `--help` gives the cache
location and other details. `CARGO_BUILD_JOBS` defaults to one job per CPU,
limited to one per 2 GiB of available memory; set it lower on a shared host.
Tests run two at a time. Download and build temporary files stay in its private
disk cache rather than a RAM-backed `/tmp`. The cache also keeps one checkout of
the source, and each build rewrites only the files that changed, so Cargo
recompiles only the crates they belong to.

## Native integrated browser capability smoke

Before publishing integrated artifacts, follow [the Wayland capability qualification](browser-wayland.md#qualification) with matched native executables. The browser requires `wl_compositor`, `wl_subcompositor`, and `wp_viewporter` at version 1 or newer. Verify a capable compositor and missing-protocol/timeout states, normal workspace startup, retained descriptors, and the system-browser action. Source registry fixtures do not replace Winit/native validation. Browserless artifacts retain their external-browser behavior.

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
- `release-metadata.json`: the version, source commit, daemon protocol,
  release notes, and every other asset's size and SHA-256 (schema 2)
- `release-metadata.json.sig`: its signature (see [Signing](#signing))

Artifact selection is explicitly platform-aware. Only Linux x86_64 GNU has a
supported build and package lane today. Windows and macOS need their own native
ports, validation, packaging, and signing before their targets can be enabled;
the release tooling does not publish placeholder binaries for them.

## Native platform roadmap

[#7](https://github.com/CarsonBurke/cagents/issues/7) tracks Windows and macOS
build, packaging and release support. These are proposed ports, not supported
download targets. Release publication remains local; adding a native validation
lane does not require moving builds, signing credentials or publication into
GitHub Actions. PR validation coverage and its execution infrastructure still
need to be established for each target.

| Target | Current release status | Decision or prerequisite |
| --- | --- | --- |
| Linux x86_64 GNU | Supported local build/package lane | Keep matched full, browserless and editor artifacts and native smoke coverage |
| Windows x64 | No supported native application or package | Qualify runtime and companion discovery before choosing installer layout |
| Windows ARM64 | Unqualified; inclusion undecided | Choose support policy and qualify dependencies/toolchains before enabling artifacts |
| macOS Apple Silicon | No supported native application or bundle | Qualify runtime before implementing `.app` layout and installation |
| macOS Intel | Unqualified; inclusion undecided | Explicitly choose support policy; an Apple Silicon build does not validate Intel |

Each new target progresses through the following gates. Passing an isolated
crate check is useful evidence, but does not establish application support.

1. **Runtime and build boundaries.** Check and test the CLI, daemon, browserless
   UI, integrated UI and standalone editor on the native target, keeping the
   daemon/UI graphs isolated. Establish native IPC, process/session ownership,
   filesystem/storage, shell/PTY and display behavior. Preserve saved browser
   descriptors when embedded surfaces are unavailable, and report the actual
   browser capability. Linux X11 support in [#2](https://github.com/CarsonBurke/cagents/issues/2)
   is complementary; it is not a Windows or macOS port.
2. **Provider-free native smoke.** Run a matched executable set in an isolated
   native session. Open the UI, use settings and a hosted terminal, and exercise
   reconnect, daemon shutdown and restart without invoking a paid provider.
   Check both browserless preservation and the target's actual integrated
   browser capability. Record source identity, toolchain, architecture and
   binary identities before proposing installable artifacts.
3. **Installation and lifecycle.** Choose the native layout and explicit
   discovery of each package's matched companions, including the standalone
   editor's private set. Fixed target executable suffixes in [#5](https://github.com/CarsonBurke/cagents/issues/5)
   are only one prerequisite; app-bundle/libexec lookup and native process
   ownership still require qualification. Verify install, launch, PATH/launcher
   integration, data permissions, upgrade across daemon versions, and uninstall
   in a clean native VM. Never select a different installation to compensate
   for a missing matched companion.
4. **Metadata, signing and publication.** Define target-specific application
   identity and version metadata, then qualify Windows signing or macOS signing,
   notarization and stapling as appropriate. Keep credentials outside source and
   build artifacts. Publish checksummed artifacts only after the native VM smoke
   also verifies upgrade and uninstall. Unsupported architectures remain absent
   from artifact selection; a filename or packaging skeleton is not a port.

For each enabled target, maintain repeatable native validation for pull requests
and exact-tag release builds. Select the architecture policy, native execution
infrastructure and signing ownership explicitly before adding a lane. The Linux
release builder's glibc baseline, artifact names and current updater/install
contract remain Linux-specific until a qualified target has its own contract.

## Publishing

A release goes out in five steps, each through `mise run release`
(`scripts/release --help`):

1. `mise run release -- bump 0.2.0` sets the product manifests and
   `Cargo.lock` to the new version. Land that through a pull request, then tag
   the merged commit and push the tag:
   `git tag -a v0.2.0 -m v0.2.0 COMMIT && git push origin v0.2.0`.
2. `mise run release -- build 0.2.0` builds the tag on the build host (`fw16`,
   or `--build-host HOST`) with `scripts/build-release-linux` from the tag's own
   checkout, admitted through `scripts/cargo-gate` beside other builds, and
   brings the verified bundle back to `~/.local/state/cagents-release/0.2.0`.
   The bundle's `release-metadata.json` carries the release notes an installed
   cagents shows before it updates: by default the `feat`, `fix` and `perf`
   commit subjects since the previous tag, or a file passed with
   `--notes FILE` (at most 64 KiB). Read them with
   `jq -r .notes ~/.local/state/cagents-release/0.2.0/release-metadata.json`.
3. Sign the metadata with your own key (see [Signing](#signing)). No repository
   script signs: `ssh-keygen -Y sign -n cagents-release -f KEY
   ~/.local/state/cagents-release/0.2.0/release-metadata.json`.
4. Rehearse the update from the previous release through a file feed (below).
5. `mise run release -- publish 0.2.0` asks you to type the version, then runs
   `scripts/publish-release`. It checks the signature against the keys the tag
   lists, the metadata against every asset, the checksums, the archive layouts
   and an offline smoke install before uploading anything. It uploads to a
   draft it owns, downloads and compares the served bytes, then publishes. It
   never moves tags or overwrites a published release. Authentication stays in
   the local `gh` configuration, outside the build environment. Publishing
   makes the release the update every installed cagents offers within six
   hours.

`mise run release:verify -- --version 0.2.0 --dist DIR` runs the same checks
without publishing. Both modes repeat the offline artifact smoke against a
private snapshot of the bundle. To verify a bundle built elsewhere, prepare the
runtime image first, or supply an existing Ubuntu 24.04 amd64 runtime image
with `--smoke-rootfs DIR`. Missing dependencies, failed install/remove scripts,
hanging commands, or a failed daemon handshake stop verification. No package is
installed on the host.

### Rehearsing

`mise run release -- feed 0.2.0 --feed DIR` adds the signed bundle to a file
feed with GitHub's layout: `DIR/download/v0.2.0/` holds every asset and,
while 0.2.0 is the newest there, `DIR/latest/download/` holds its signed
metadata. Add the previous release the same way to rehearse an update from it.
An installed build reads that feed only when it is a development build or was
built with `CAGENTS_UPDATE_REHEARSAL_KEYS`, and `CAGENTS_UPDATE_FEED` is
`file:///DIR`; see [Rehearsing an update](development.md#rehearsing-an-update).

## Signing

An installed build offers a release only when the release's
`release-metadata.json` (schema 2) carries a valid signature,
`release-metadata.json.sig`, made by a key listed in
`src/update/release-keys.pub`:

```sh
ssh-keygen -Y sign -f KEY -n cagents-release release-metadata.json
```

Schema 2 adds `notes` (what changed, in Markdown) and `assets`, which maps
every uploaded file name to its `size` and `sha256`. The updater reads the
archive, `SHA256SUMS` and `install.sh` digests from it; `SHA256SUMS` must
agree with them.

The updater accepts `ssh-ed25519` keys and `sk-ssh-ed25519@openssh.com`
security keys, and requires a security key's signature to record a touch.
Prefer a security key: hosted agents run as the same user and can read a key
file, but cannot touch a security key. Do not load a release key into a
long-lived `ssh-agent`, which would sign for any process of the user.

To replace a key, publish a release signed by the old key whose
`release-keys.pub` lists the new one. To withdraw a compromised key, publish a
release, signed by a remaining key, that no longer lists it. A build trusts
exactly the keys it was built with, so builds that have not yet installed that
release keep trusting the withdrawn key, and its holder can still sign updates
for them until they do.

A signature proves who made a release, not that it is the newest. Whoever can
publish on GitHub can point `latest` at an older signed release: installed
builds are then not offered anything newer, though never anything older than
what they run.

Development builds, and rehearsal builds compiled with
`CAGENTS_UPDATE_REHEARSAL_KEYS` (extra public key lines), read releases from
`CAGENTS_UPDATE_FEED` instead of GitHub: an `https://` URL or `file:///`
directory with the same layout (`latest/download/…` and `download/vVERSION/…`).
Published builds ignore that variable.

## Comparing package variants

`scripts/package-linux.sh` builds the full package by default and writes
browserless build outputs to a separate Cargo target directory. Run both
variants to compare their measurement reports without either overwriting the
other:

```sh
mise run package -- --variant full
mise run package -- --variant browserless
cat dist/cagents-"${VERSION}"-sizes.txt \
  dist/cagents-browserless-"${VERSION}"-sizes.txt
```

Automation schedules use the operating system's IANA timezone database. Both
Debian variants depend on `tzdata`; portable Linux installations also need that
package (or the distribution's equivalent zoneinfo database). The build and
offline runtime images install it explicitly, including UTC and named regional
zones, so minimal images exercise the same schedule contract as desktops. The
runtime smoke cache uses provisioning version 2; choose a new cache directory
when upgrading a previously prepared version-1 image.
