# Releasing

- [Building a release](#building-a-release)
- [Artifacts](#artifacts)
- [macOS](#macos)
- [Native platform roadmap](#native-platform-roadmap)
- [Publishing](#publishing)
- [Signing](#signing)
- [Comparing package variants](#comparing-package-variants)

GitHub Actions builds and signs eligible releases. The release operator
publishes the draft after rehearsing the update on fw16. Releases that need
the recovery key are built and signed on the local release path.

## Building a release

`scripts/build-release-linux` builds an exact Git tag in a private, rootless
Ubuntu 22.04 environment with Rust 1.93, Zig 0.15.2, and Git 2.55. This
keeps the glibc 2.35 baseline instead of inheriting a newer workstation's libc.
It runs no tests itself: every lane's tests (CLI, host, browserless UI,
integrated UI, dictation helper and editor) gate a release earlier, when main's
build sentinel on fw16 runs them on each commit it checks, in the ordinary test
profile ([Watching main after merges](development.md#watching-main-after-merges)).
`mise run release -- build` requires the tagged commit's passing verdict. The
builder packages the shipped binaries with `release` and checks them and the
desktop metadata. Both cagents packages ship the dictation helper `cagents-dictate`,
linked against the sherpa-onnx libraries `scripts/sherpa-onnx-libs` verifies,
with its third-party notices; the builder and the publisher refuse a package
from a source with the helper that lacks either.

The build stages one copy of the dictation helper for both cagents variants.
The standalone editor ships the same CLI and daemon as the browserless
package. Each cagents variant builds its own CLI, daemon and UI for its
product identity. Cargo runs one build at a time while a single-CPU worker
packages the previous variant. The package size reports count the shared
helper's build time under the full variant.

Before exposing the bundle, the builder installs and removes each Debian
package and extracts each portable archive in a disposable Ubuntu 24.04 runtime
image, all five at once. This runtime checks actual package dependencies, shipped commands,
matched daemon startup as the production instance (which shows the shipped
binaries were built without debug assertions or the development feature, the
setting their whole production identity follows) and safe shutdown, live-daemon removal guards, and
preservation of unrelated files and saved app data. The compiler environment
remains Ubuntu 22.04; runtime smoke does not replace the glibc baseline checks.
Before compiling, the builder validates the runtime cache or provisions it with
network access using `scripts/prepare-release-smoke-rootfs`. Every artifact
execution uses a separate networkless namespace with private user state. The
runtime cache defaults to
`$XDG_CACHE_HOME/cagents-release-smoke/ubuntu-24.04-amd64` (or under `~/.cache`),
and `CAGENTS_RELEASE_SMOKE_CACHE` overrides it. Each cache keeps one pinned base
archive and one provisioned runtime image. The next preparation reclaims scratch
directories left by interrupted preparations while it holds the cache lock.

Install `bubblewrap`, `curl`, `git`, and the standard archive tools locally,
then build:

```sh
mise run release:build -- --ref v0.1.2 --output /var/tmp/cagents-v0.1.2
```

`release:build` runs `scripts/build-release-linux`; its `--help` gives the cache
location and other details. `CARGO_BUILD_JOBS` defaults to one job per CPU,
limited to one per 2 GiB of available memory; set it lower on a shared host.
Download and build temporary files stay in its private
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
- with a macOS build ([macOS](#macos)): `cagents-${VERSION}-aarch64-apple-darwin.tar.gz`,
  `install-macos.sh`, and `SHA256SUMS-macos`, which covers those two
- `release-metadata-2.json`: the version, source commit, daemon protocol,
  release notes, and every other asset's size and SHA-256 (schema 2)
- `release-metadata-2.json.sig`: its signature (see [Signing](#signing))
- `release-metadata.json` and `release-metadata.json.sig`: what builds up to
  0.2.3 read (see [Trust generations](#trust-generations))

A bundle holds its metadata as `release-metadata.json` and its signature as
`release-metadata.json.sig`; the publisher gives them their published names.

Artifact selection is explicitly platform-aware. The metadata's `targets` lists
Linux x86_64 GNU, and `aarch64-apple-darwin` when the release carries the
macOS build; a release without it is exactly what one was before macOS builds
existed, and builds that predate them ignore the macOS entry and its assets.
Windows needs its own native port, validation, packaging, and signing before
its target can be enabled; the release tooling does not publish placeholder
binaries.

## macOS

`scripts/package-macos` builds the Apple Silicon release on a Mac:
`cagents-${VERSION}-aarch64-apple-darwin.tar.gz`, `install-macos.sh` and
`SHA256SUMS-macos`. It needs Xcode's command line tools and rustup; the
toolchain comes from `rust-toolchain.toml`. It builds the four executables with
the release profile for macOS 11 or newer: the CLI, the daemon, the UI with
the production identity `dev.cagents.Cagents` and without the integrated
browser, and the dictation helper, linked against the pinned sherpa-onnx static
libraries. The UI and the helper carry an embedded `Info.plist` section, so the
microphone permission prompt names cagents and its purpose.

The archive holds one version tree laid out as on Linux, so the install guard
reads it the same way, as companion lookup and the updater will once they
support macOS:

```
cagents-VERSION/bin/{cagents,cagents-ui,cagentsd,cagents-dictate}
cagents-VERSION/Cagents.app
cagents-VERSION/share/doc/cagents/{LICENSE,THIRD-PARTY-NOTICES}
```

`install-macos.sh` installs it under `~/Library/Application Support/cagents-install`
(`--prefix` overrides it):

- `versions/VERSION/` is the tree, and `current` links to the version that
  serves. The installer switches it atomically, keeps the version it replaced
  for rollback (named in `.previous`), and removes older trees that no running
  process uses, or none when `lsof` cannot tell.
- `Cagents.app` is a stable launcher bundle. Its executable, a small C program
  (`packaging/macos/launcher.c`), runs the real path of `current/bin/cagents-ui`.
  Launch Services and the Dock keep one application across updates, because
  the bundle they know never moves. `~/Applications/Cagents.app` links
  to it; Spotlight and Launchpad may not list a link, so open it from
  `~/Applications` or keep it in the Dock.
- `~/.local/bin/cagents` links to `current/bin/cagents`.
- `~/Library/LaunchAgents/dev.cagents.Cagents.Daemon.plist` runs
  `versions/VERSION/bin/cagents daemon start` once at login, as Linux's
  autostart entry does. It is not kept alive: the daemon outlives the job, and
  `cagents daemon stop` stays the way to stop it. The installer rewrites it
  for each version it installs.

The installer verifies the archive against `SHA256SUMS-macos`, accepts exactly
the release's members, refuses links in the tree, and checks every signature in
a staging directory before it moves the release into place. It holds a lock
on the prefix while it works. It installs over a running daemon only of the
same version, asking the installed CLI, which speaks that daemon's protocol:
the daemon cannot yet hand its sessions to another version on macOS, so it
says how to stop it instead, and it refuses when it cannot tell. `--uninstall`
removes the installation, its links, and the login item when that runs this
installation; `--purge` also removes saved app data, and needs the installed
CLI to check for a running daemon first. Only an installation in the account's
own home registers `Cagents.app` with Launch Services or unloads its login
item, so a test install under a private `HOME`, as the package's smoke test
makes, leaves the account's own alone.

Every executable and the bundle are signed ad hoc (`codesign --sign -`), which
Apple Silicon requires to run anything. Files that `curl`, `tar` or
`install-macos.sh` download carry no quarantine attribute, so Gatekeeper does
not assess them. A browser download is quarantined, and the installer clears
the attribute on the tree it installs. Gatekeeper blocks an ad hoc signed app
opened from a quarantined download, as from a disk image, which is why the
release installs from a terminal.

Notarization, which would let people open a downloaded app without a warning,
needs an Apple Developer ID Application certificate and an App Store Connect
API key for `notarytool`. Signing would use `codesign --options runtime
--timestamp --sign "Developer ID Application: ..."` on each executable and
then the bundle, with the `com.apple.security.device.audio-input` entitlement
on the dictation helper, whose microphone the hardened runtime otherwise
denies. The notary service takes a zip, disk image or package, not a tarball:
the signed tree would be zipped with `ditto -c -k --keepParent`, submitted
with `xcrun notarytool submit --wait`, and the ticket stapled to `Cagents.app`
with `xcrun stapler staple` before the release archive is made. A bare
executable cannot hold a stapled ticket, so Gatekeeper would look the CLI's up
online. None of this is set up, and the credentials would stay outside source and build
artifacts like the release signing key.

To cut a release with the macOS build, give `cut` or `build` the Mac with
`--macos-host MAC` (or `CAGENTS_RELEASE_MACOS_HOST`). The release exports the
commit, copies it to `~/Library/Caches/cagents-release/VERSION` on the Mac and
runs its `scripts/package-macos` there, which builds, signs and installs it in
a private home and prefix to check the commands, bundle and login item. It
brings the three files back into the bundle before the metadata is written and
signed, then removes the export; the Cargo target stays in
`~/Library/Caches/cagents-release/target` for the next release. The publisher
then requires all three files, checks `SHA256SUMS-macos` and the archive's
members, and adds a macOS section to the release notes. GitHub Actions builds
no macOS release yet.

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
| macOS Apple Silicon | Packaging, installer and release lane ([macOS](#macos)); the daemon and UI ports are in progress | Qualify the native runtime, then publish the first release with the macOS build |
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

For the local signing path, fw16 prebuilds and smoke-tests main's newest
compiled commit in the background
([Prebuilding release bundles](development.md#prebuilding-release-bundles)).
Main always declares the next version: publishing a release opens and lands the
pull request that starts the next patch version. A release goes out in three
steps, each through `mise run release` (`scripts/release --help`). From 0.2.5
on, GitHub Actions builds and signs it instead, and it is published once its
draft is rehearsed; see [Signing in GitHub Actions](#signing-in-github-actions).

1. `mise run release -- cut --key KEY` picks the newest bundle of main whose
   tests passed in main's sentinel. It shows how many commits that bundle is
   behind main and offers to wait for a prebuild of the tip instead; `--wait`
   waits without asking, building the tip at normal priority while its tests
   run beside it. A failed or unavailable test verdict ends that wait.
   `--commit SHA` picks a particular bundle. It then:
   - copies the bundle to `~/.local/state/cagents-release/VERSION`, where
     VERSION is the one the commit declares;
   - checks every file against the size and digest recorded when it was built
     and smoke-tested;
   - writes the release notes an installed cagents shows before it updates. By
     default these are the `feat`, `fix` and `perf` commit subjects since the
     previous tag; `--notes FILE` supplies them instead (at most 64 KiB);
   - tags the commit `vVERSION` locally;
   - signs the metadata with KEY (or `CAGENTS_RELEASE_SIGNING_KEY`); touch the
     security key when it asks. Without a key, it prints the signing command
     (see [Signing](#signing)).
2. `mise run release -- rehearse VERSION` rehearses installed cagents updating
   from the newest published release to the signed cut, on the build host,
   the way a person updates in the app. It uses the released commit's
   `scripts/rehearse-update`
   ([Rehearsing an update](development.md#rehearsing-an-update)):
   - a private user with its own headless desktop installs the published
     release;
   - it opens three windows, one holding nothing, and seeds chats, agents, a
     channel, terminals with a long-lived child and a chat whose provider has
     already stopped;
   - the published build's own updater finds the cut in a file feed made
     from the cut bundle, checks its signature against the release keys, and
     updates;
   - it requires every pane, chat, agent and terminal to survive, NEW to
     serve, the windows to reopen on NEW, and no reopened window to show
     "Update failed" or "Update not installed";
   - then, as after a reboot, it closes the windows, stops NEW's daemon and
     reopens them, and requires the same again.

   It takes about 12 minutes, plus the published release's download the
   first time. Interrupting it stops the run on the host too. The options
   it passes come from this checkout and the harness from the cut commit, so
   a cut older than the harness options (#3919, #3938) is refused by them.

   Either way the update must end `Complete` with no refusal. A reopened
   window reports anything else as "Update failed" or "Update not installed",
   even when the work survived. In the app it is the published build's worker
   that writes that status, so a worker that refuses the cut's protocol after
   a good install fails the rehearsal (#3877). With `--staged` it instead runs
   the cut's own staged `cagents daemon upgrade`, which needs no signature.

   The rehearsal's private user goes in `/var/tmp/cr/release-VERSION` on the
   host. If `/var/tmp` has a `.git` above it, such as the empty mountpoints
   a sandbox masking `.git` leaves behind, it goes in `/dev/shm/cr` instead.
   It takes about 700 MB and is removed once the rehearsal passes; the
   report is kept in `~/.cache/cagents-release-rehearsal/passed-VERSION.txt`,
   headed, for the in-app update, by the SHA-256 of the metadata the update
   read and the release it updated from; a staged pass has no such header.
   A later rehearsal of the version removes it as soon as it starts on the
   host, so only the latest outcome stands. If a
   `.git` appears above the root while the rehearsal runs, a failure says
   so instead of reporting it as a regression, and you rehearse again. A
   rehearsal that fails in `/var/tmp` keeps its report, records and
   screenshots there until the next rehearsal of that version replaces them.
   One that fails in `/dev/shm`, which is memory, keeps only its report and
   screenshots, in `~/.cache/cagents-release-rehearsal/failed-VERSION`.
3. `mise run release -- publish VERSION` asks you to type the version, then
   runs `scripts/publish-release --push-tag`. It checks the signature against
   the keys the tag lists, the metadata against every asset, the checksums,
   the archive layouts and an offline smoke install. Only then does it push
   the tag, so a refused bundle leaves nothing on origin. It uploads to a draft
   it owns, downloads and compares the served bytes, then publishes. It never
   moves tags or overwrites a published release. Authentication stays in the
   local `gh` configuration, outside the build environment. Publishing makes
   the release the update every installed cagents offers within six hours.
   Finally, it opens the pull request that sets main to the next patch version
   (`scripts/release bump`) and lands it with `scripts/land-pr`. If any step
   fails, fix the cause and run the same command again: it skips a release
   already published and lands a bump pull request already open.

A cut that is never published can be replaced: `cut` again replaces the local
bundle and tag of an unpublished version once the new copy checks out, and
asks first if the earlier one is signed. A bump pull request that conflicts
with main is made again on main and landed.

To build a tag without a prebuild, as before, bump and tag it yourself, then
run `mise run release -- build VERSION`. This builds the tag on the build host
with `scripts/build-release-linux`, from the tag's own checkout, admitted
through `scripts/cargo-gate` beside other builds, and requires the commit's
passing test verdict from main's sentinel. It brings the verified bundle back
to the same place, ready for signing.

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

`release-metadata.json.sig` may hold several signatures, one after another
(at most four, nothing else). Builds from 0.2.4 on accept it when one is by a
key they trust.

### Trust generations

Every release must be verifiable by every build that is offered it, or that
build is stranded with no way to update. Releases therefore belong to trust
generations, numbered by `GENERATION` in `src/update/feed.rs`. A build reads
only its own generation's metadata name: `release-metadata.json` for
generation 1 (up to 0.2.3, which also accepts only one signature),
`release-metadata-2.json` for generation 2 (from 0.2.4), and so on.

A release publishes its own metadata under its generation's name. Under each
older generation's name it carries the metadata of the release that began the
next generation, byte for byte. A build of an older generation is offered that
release, installs it, and from there reaches the current one. Each step is
signed with a key the build taking it already trusts. 0.2.4 begins generation
2: every later release carries 0.2.4's metadata as `release-metadata.json`.
Never delete a release that began a generation.

`publish-release` refuses a release that would strand anyone. It counts the
releases published on GitHub, read from their tags in the checkout;
`--verify-only` counts every release tag in the tagged history.

- Its signature must verify with the keys of its tagged source, and with those
  of every earlier release tag of its generation.
- A release that begins a generation must verify with the keys of every
  release of the generation before. When that generation is 1, it must hold
  exactly one signature.
- A generation follows the one before it, never skipping one or going back.
- The metadata it carries for older generations must be the one their
  generation's successor published, signed as those builds read it. For
  publication, it downloads them from GitHub and compares any `--bridge DIR`
  copy. `--verify-only` reads them from `--bridge DIR`, which holds them under
  their published names.

`cagents upgrade VERSION` run on an older generation, for a version past its
bridge, reads the bridge's metadata and refuses it; it updates to the bridge
first. `release-metadata feed --generation G --bridge DIR` lays out a file
feed the same way, for rehearsing the two-step update.

Within a generation, every release must carry a signature by a key that
every earlier release of the generation trusts. A key added partway through a
generation is therefore only a co-signer: releases must still also be signed
by an older key. A key is a sole signer only from the start of a generation,
so the Actions key is listed from 0.2.4, the first release of generation 2.
To drop a key,
including a compromised one, begin a new generation: raise `GENERATION`, add
its name to `GENERATIONS`, and remove the key from `release-keys.pub`. Sign
that release with a key that every build of the current generation trusts,
such as the security key, then sign later releases with the remaining keys.
Builds that have not yet installed it still trust the dropped key, and its
holder can sign updates for them until they do.

### Signing in GitHub Actions

From 0.2.5 on, releases are built and signed by the Release workflow
(`.github/workflows/release.yml`) with a plain `ssh-ed25519` key that only
GitHub holds, so releasing needs no touch. The workflow leaves the signed
release as a draft, which is published only once its update has been
rehearsed. The security key stays listed in `release-keys.pub` as the
recovery key, and signs locally whatever Actions may not:

1. `mise run release -- cut --actions` picks the commit as above and tags it
   `vVERSION` locally. It copies and signs nothing.
2. `mise run release -- publish VERSION --actions` checks the tag as the
   workflow's admission will. If eligible, it requires the tagged commit's
   passing test verdict on fw16 and records it as the `cagents/fw16-tests`
   commit status before pushing the tag. The publishing account needs write
   access to commit statuses. The push starts the workflow, and the command
   prints its page and what to run next. It exits non-zero until the release
   is published. If a pushed tag is missing its passing status, run this
   command again to record it, then rerun the failed Release workflow.
3. Once the workflow has left the draft, `mise run release -- rehearse
   VERSION --from-draft` rehearses the in-app update to it, as
   [Publishing](#publishing) describes. It refuses while the tag's workflow
   run is still going, or if it did not succeed. The build host downloads the
   draft itself, so `gh` there must be logged in with access to the
   repository's drafts (push access). It serves the draft in a file feed under
   its published names, so the published build reads the same metadata names,
   signatures and bridges it will read from GitHub. A pass records the
   SHA-256 of the draft's own metadata, which names every file's digest, and
   the release it updated from.
4. `mise run release -- publish VERSION --actions` again reads that record
   from the build host and, after you type the version (or with `--yes`),
   runs `scripts/publish-release --promote SHA256`. It refuses if the newest
   published release is no longer the one the rehearsal updated from. It
   downloads the draft, repeats every check on it except the smoke install,
   which the workflow ran, and compares what it serves under the older
   generations' names with what the bridge release publishes. It refuses unless the draft's metadata
   has the rehearsed digest and its assets are unchanged since it read
   them, then publishes the draft and lands the next version's bump as
   `publish` does. With no passing rehearsal of that draft it refuses and
   says to rehearse it.

The workflow's jobs:

- **admit** proceeds only for a tag on a commit on main whose manifests declare
  the tag's version, which is newer than every published release and not
  published itself. It admits the tag for signing only when all of these hold:
  - the tag does not begin a trust generation;
  - every published release of its generation trusts the Actions key, as
    `publish-release` requires;
  - the tagged `release-keys.pub` lists exactly the repository variable
    `RELEASE_TRUSTED_KEYS`;
  - the exact tagged commit has a passing `cagents/fw16-tests` status.

  The release is cut, signed and published locally for 0.2.4, for any change
  of trusted keys or generation, and when no Actions key is configured.
- **build** has no secrets. It builds, packages and smoke-tests the tagged
  source with `scripts/build-release-linux`, as fw16 does.
- **sign** is the only job that can read the key: the environment secret
  `RELEASE_SIGNING_KEY` of the `release` environment, which deploys only from
  `v*` tags. It checks this run's bundle against its metadata, version and
  commit, signs the metadata in memory-backed storage, and checks the signature
  against `RELEASE_SIGNING_PUBLIC_KEY`.
- **publish** runs `scripts/publish-release --draft` with the workflow's
  token. It repeats every check, including the keys of every build that reads
  the release and the bridges it carries, uploads the release as a draft,
  compares the bytes GitHub serves, and publishes nothing.

Run by hand (`gh workflow run release.yml --ref main`), the workflow only
builds that commit, to rehearse a release build; nothing is signed.

One-time setup, by the repository owner. First make a fine-grained personal
access token at <https://github.com/settings/personal-access-tokens/new>:

- Repository access: only `CarsonBurke/cagents`.
- Expiration: one day.
- Repository permissions: Environments and Variables, read and write.

Then run:

```sh
repo=CarsonBurke/cagents
# The environment that holds the key, usable only from release tags.
gh api -X PUT repos/$repo/environments/release --input - <<< \
  '{"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}}'
gh api -X POST repos/$repo/environments/release/deployment-branch-policies -f name='v*' -f type=tag
# Release tags cannot be moved or deleted.
gh api -X POST repos/$repo/rulesets --input - <<< \
  '{"name":"Release tags","target":"tag","enforcement":"active","conditions":{"ref_name":{"include":["refs/tags/v*"],"exclude":[]}},"rules":[{"type":"deletion"},{"type":"update"}]}'
# The token, usable only by a workflow on main.
gh api -X PUT repos/$repo/environments/release-keygen --input - <<< \
  '{"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}}'
gh api -X POST repos/$repo/environments/release-keygen/deployment-branch-policies -f name=main -f type=branch
gh secret set RELEASE_KEYGEN_TOKEN --env release-keygen -R $repo   # paste the token
# Make the key inside a workflow run, so nobody sees its private half.
gh workflow run release-signing-key.yml -R $repo --ref main
```

The Release signing key workflow (`scripts/release-actions keygen`) refuses
if a key exists. It stores the private key as the `release` environment's
secret. It stores the public key, and the keys releases may trust, as the
repository variables `RELEASE_SIGNING_PUBLIC_KEY` and `RELEASE_TRUSTED_KEYS`.
However it ends, it deletes `RELEASE_KEYGEN_TOKEN`. It then prints the line to
add to `release-keys.pub`. GITHUB_TOKEN cannot write secrets, hence the
temporary token; revoke it once the run is done. Merge that line before
cutting 0.2.4, which the security key signs. From 0.2.5 on, Actions signs.

The rehearsal record lives on the build host, which agents running as the
same user can write, and promotion uses your own `gh` login. It stops a
release from being published before its update was rehearsed, or with other
bytes than were rehearsed, by mistake; like the checks above, it does not stop
someone who can already publish on GitHub.

What this does not protect against: GitHub, and whoever can administer the
repository, are trusted. A holder of an admin or write token can push a tag
whose workflow is changed, change the environment's policy, or have a job
print the secret, and so sign any release. The checks above stop mistakes
and a tag off main, not a determined administrator.

To recover from a leaked Actions key:

1. Delete the secret and the variables.
2. Run the key workflow again for a new key.
3. Begin a new trust generation (above) whose `release-keys.pub` lists the
   security key and the new key, but not the leaked one. Set
   `RELEASE_TRUSTED_KEYS` to that list, then cut, sign with the security key,
   and publish it locally.

Releases after it are signed in Actions again.

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
runtime smoke cache uses provisioning version 4, which adds ALSA for the
dictation helper; choose a new cache directory (`CAGENTS_RELEASE_SMOKE_CACHE`)
when upgrading a previously prepared image.
