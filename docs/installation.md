# Installing, updating, and removing cagents

- [Requirements](#requirements)
- [Install with the verified installer](#install-with-the-verified-installer)
- [Convenience install](#convenience-install)
- [Debian packages](#debian-packages)
- [Portable archive](#portable-archive)
- [What the installer does](#what-the-installer-does)
- [Updating](#updating)
- [Uninstall or purge](#uninstall-or-purge)

Releases are public at [CarsonBurke/cagents](https://github.com/CarsonBurke/cagents/releases).
The installer and the commands below download them over HTTPS with `curl`; no
GitHub account is needed. You can also download release assets directly from
the releases page; tagged builds stamp the repository into the released copy of
`install.sh` automatically.

Run the installation and removal commands below from a system terminal outside
cagents. Terminals opened in cagents are hosted sessions; these protected commands
refuse there even when you type them yourself.

## Requirements

The supported release target is **Linux x86_64 with glibc**, built on Ubuntu
22.04, running in a **Wayland** session. X11, musl, ARM, and other operating
systems or package formats are not supported yet. A normal graphical login
provides an absolute `XDG_RUNTIME_DIR`, which cagents uses for the private
daemon socket. The archive installer rejects glibc versions older than 2.35
and unknown version results before installing or updating. Existing installations
can still be removed on older glibc systems.

The `.deb` declares `git (>= 2.36)`, `libc6 (>= 2.35)`, Fontconfig, FreeType,
GCC/libstdc++, Wayland client, xkbcommon, ALSA (`libasound2`, which dictation's
microphone capture uses, through PipeWire or PulseAudio where they run) and zlib
runtime dependencies. The
full package also requires `libegl1` for the integrated browser; the browserless
package does not add this dependency. The portable archive needs equivalent
libraries. Git 2.36 or newer is required for
NUL-delimited worktree discovery, so Ubuntu 22.04's default Git 2.34 must be
upgraded separately. Hosted terminals need a shell, PTY support, and Linux
`/proc`.

Interface text uses the desktop's installed fonts with Fontconfig fallbacks.
Install Open Sans and Noto Sans Mono if they are not already available.

The GitHub CLI, `gh`, is optional for local-only use but required for GitHub
and pull-request features. Installing and updating cagents do not need it.
Install it from your distribution and authenticate as the same desktop user:

```sh
gh auth login --hostname github.com
gh auth status --active --hostname github.com --json hosts
```

cagents delegates authentication to `gh`; it never asks for or stores a GitHub
token itself.

Pull-request features require the JSON authentication-status capability shown
above. If the command reports `unknown flag: --json`, upgrade GitHub CLI using
the [official installation instructions](https://cli.github.com/). Older
distribution packages can authenticate successfully while lacking this
capability; refreshing or signing in again does not add it.

## Install with the verified installer

This flow resolves the latest release once, pins that exact tag for every
download, verifies both the installer and archive against the release checksum
manifest, and hands the verified archive back to the installer. It is also the
command shown on the website:

```sh
(
  set -eu
  releases='https://github.com/CarsonBurke/cagents/releases'
  latest="$(curl -fsSL --proto '=https' -o /dev/null -w '%{url_effective}' "$releases/latest")"
  tag="${latest##*/}"
  case "$tag" in v[0-9]*) ;; *) echo 'Expected a versioned release tag' >&2; exit 1 ;; esac
  version="${tag#v}"
  download_dir="$(mktemp -d)"
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  archive="cagents-${version}-x86_64-unknown-linux-gnu.tar.gz"

  (
    cd "$download_dir"
    curl -fsSL --proto '=https' --fail-early --remote-name-all \
      "$releases/download/$tag/install.sh" "$releases/download/$tag/SHA256SUMS" \
      "$releases/download/$tag/$archive"
    awk -v archive="$archive" \
      '$2 == "install.sh" { print; installer++ } $2 == archive { print; payload++ }
       END { exit(installer != 1 || payload != 1) }' \
      SHA256SUMS > selected-checksums
    sha256sum --strict --check selected-checksums
  )

  CAGENTS_ARCHIVE="$download_dir/$archive" \
  CAGENTS_CHECKSUMS="$download_dir/SHA256SUMS" \
    sh "$download_dir/install.sh" --version "$version"
)
```

To inspect the installer first, add `less "$download_dir/install.sh"` just
before the `CAGENTS_ARCHIVE` line. To pin a particular version, replace the
`latest=` and `tag=` lines with that exact tag, for example `tag=v0.1.2`.

SHA-256 confirms that the files match the release manifest. It does not protect
against a compromised repository or release, so inspect the tag, release, and
installer according to your own trust requirements.

Supplying both `CAGENTS_ARCHIVE` and `CAGENTS_CHECKSUMS` keeps installation
offline. Without them, the installer downloads the release itself with `curl`.
For another repository set with `CAGENTS_REPOSITORY`, which may be private, it
uses an authenticated GitHub CLI when there is one. It never copies tokens into
its state, URLs, or command arguments.

Run `sh install.sh --help` for every option.

## Convenience install

This shorter flow runs the downloaded installer without inspecting it or
checking its checksum yourself; the installer still verifies the archive it
downloads. Use it only if you trust the repository and release:

```sh
(
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  releases='https://github.com/CarsonBurke/cagents/releases'
  latest="$(curl -fsSL --proto '=https' -o /dev/null -w '%{url_effective}' "$releases/latest")" || exit 1
  tag="${latest##*/}"
  case "$tag" in v[0-9]*) ;; *) echo 'Expected a versioned release tag' >&2; exit 1 ;; esac
  curl -fsSL --proto '=https' -o "$download_dir/install.sh" "$releases/download/$tag/install.sh" &&
    sh "$download_dir/install.sh" --version "${tag#v}"
)
```

It looks up the latest release once, so the installer it runs and the version
it installs always match. To install a particular version instead, download
that tag's installer and name the version:

```sh
(
  VERSION=0.1.2
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  curl -fsSL --proto '=https' -o "$download_dir/install.sh" \
    "https://github.com/CarsonBurke/cagents/releases/download/v${VERSION}/install.sh" &&
    sh "$download_dir/install.sh" --version "$VERSION"
)
```

## Debian packages

Download the package and checksum manifest, verify the package, then let APT
resolve its shared-library dependencies:

```sh
VERSION='0.1.2'
base="https://github.com/CarsonBurke/cagents/releases/download/v${VERSION}"
curl -fsSL --proto '=https' --fail-early --remote-name-all "$base/cagents_${VERSION}_amd64.deb" "$base/SHA256SUMS"
sha256sum --ignore-missing --strict --check SHA256SUMS
sudo apt install git "./cagents_${VERSION}_amd64.deb"
```

The package installs the three executables together under
`/usr/libexec/cagents`, exposes `/usr/bin/cagents`, and installs the desktop
entry, icon, and AppStream metadata system-wide.

A separately named **browserless** package, without the embedded Servo browser,
can be installed alongside the full package. It uses
`/usr/libexec/cagents-browserless`, exposes `/usr/bin/cagents-browserless`, and
installs separately identified desktop and AppStream metadata:

```sh
curl -fsSL --proto '=https' --fail-early --remote-name-all \
  "$base/cagents-browserless_${VERSION}_amd64.deb" "$base/SHA256SUMS-browserless"
sha256sum --ignore-missing --strict --check SHA256SUMS-browserless
sudo apt install git "./cagents-browserless_${VERSION}_amd64.deb"
cagents-browserless ui
```

Browserless builds keep terminals, repositories, worktrees, GitHub
pull-request flows, and persistence. New links open through the desktop portal,
falling back to `xdg-open` when no portal is available; Debian packages install
`xdg-utils`, and portable or source users must provide it for that fallback. A
restored browser pane keeps its saved URL, history, scroll position, and
fidelity, shows that the integrated browser is unavailable, and offers **Open
in system browser**.

How Debian upgrades treat a running daemon is described in
[Daemon and upgrades](daemon.md#debian-packages).

## Portable archive

The archive is self-contained as a directory layout, not as a static
executable. Keep its `bin` directory intact: `cagents`, `cagents-ui`,
`cagentsd`, and the dictation helper `cagents-dictate` must stay together.
`share/doc/cagents/THIRD-PARTY-NOTICES` lists the open-source libraries the
helper includes and their licenses.

```sh
VERSION='0.1.2'
base="https://github.com/CarsonBurke/cagents/releases/download/v${VERSION}"
curl -fsSL --proto '=https' --fail-early --remote-name-all \
  "$base/cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" "$base/SHA256SUMS"
sha256sum --ignore-missing --strict --check SHA256SUMS
tar -xzf "cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
"./cagents-${VERSION}/bin/cagents" ui
```

## What the installer does

`install.sh` never uses `sudo` and refuses to run as root. It installs an
immutable version below `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`
(or `--prefix PATH`), switches the `current` symlink atomically, creates desktop
metadata links under `${XDG_DATA_HOME:-$HOME/.local/share}`, and exposes only
`$HOME/.local/bin/cagents` on `PATH`. The application menu entry uses the
installed binary and icon by absolute path, so launching from the desktop does
not depend on `PATH`; using `cagents` from a terminal does.

Those links point into `current`, so an update reaches them. If a desktop
tool has replaced one with a plain copy, and the copy is byte-for-byte the
installed file, the installer puts the link back and says so. It refuses
anything else at those paths, such as an edited copy, a directory, or a link
to somewhere else, and leaves it unchanged. If the installation then fails,
the copy is restored.

The standard installation starts its per-user `cagentsd` immediately and
registers it for graphical-login autostart. See [Daemon and
upgrades](daemon.md) for how the daemon, sessions, and upgrades interact.

## Updating

### In-app updates

Full self-managed (`install.sh`) installations, including packages built
locally with a `+local` version, check for a newer release at startup and every
six hours, and whenever you choose **Check for updates** in About. A check
reads the latest release's signed `release-metadata-2.json` through github.com's
public download links, so it needs `curl` but no GitHub sign-in, and spends no
GitHub API quota. The update is offered only when that metadata carries a
valid signature by a key the installed build trusts (see
[Releasing](releasing.md#signing)). The metadata names the SHA-256 of every
file the update downloads, including the installer, and the updater checks each
download against it. A newer stable version appears in a notice above the
workspace. Dismissing it with × retains the update in the notices popover.
Build metadata identifies the installed binaries but does not make a published
release with the same base version count as an upgrade.

Clicking it stages verified bytes, probes the candidate's protocol, and presents
its actual session impact before installation. Held terminals and supported
native Codex servers continue in the same processes. Other native chats wait
for idle, or you can explicitly choose interruption and exact resumption.
Changed session ownership or impact requires another confirmation. A compatible
replacement reopens windows while keeping the daemon and sessions alive.
Newer matched sets already installed under the same managed installation,
including local builds, use this same flow. Whichever window you update
from, the newer build's own updater performs the update, so its checks decide
what continues. Supported source protocols are wire-proven for the updater's
lifecycle operations. If the running daemon's protocol is not one of them, the
update is refused before anything changes; follow the cold installation the
refusal describes. Builds that cannot update themselves say why in About instead
of offering an update: a package installed by the system package manager
names it, a build from source asks you to install a release, and a build
without `curl` asks for it. Browserless builds do not download full releases.

The detached updater rechecks the selected release, verifies and seals the
downloaded bytes, and asks every open window to save. Dirty editors, unsent
native-chat input or attachments, pending chat transitions, active repository
mutations, and save failures veto the update. Prepared windows reject new user
mutations but still process outstanding completions and recheck blockers
before closing. After a final durable flush, the updater closes the windows,
runs the installer's full-upgrade handoff, and reopens the same named windows
from the newly installed executable. Failure status and the private installer
log stay under the installation's `.update/` directory. After you resolve a
veto or failure, the warning offers **Retry update** while the selected release
is still available. Starting a new update keeps the current job and keeps
verified successful jobs for at least 30 days. Failed or unresolved jobs
retain the inputs needed for recovery.

### From the command line

Rerun the installer for a compatible update: the existing daemon and its live
PTYs keep running. By default the installer verifies the staged CLI against the
running daemon before switching `current`. An incompatible build is installed
without replacing the running daemon; saved windows reopen with that daemon's
matching UI. Finish the staged update with `cagents daemon upgrade`, or with
the application's update control (see [In-app updates](#in-app-updates)). The
command displays the same impact, asks for approval, and waits for completion.
`--yes` skips the prompt; `--interrupt` additionally permits active native
turns to stop. The review groups panes by what happens after the update. Panes
that cannot reconnect reopen stopped; each lists how to reopen its conversation
or restart its terminal. The update keeps the saved panes and their files.
If the running service still reports an item that needs attention, resolve it
before updating. A changed inventory requires another review.

To make a verified build available without saving or closing running windows,
append `--stage-only` to the installer invocation. It publishes the complete
candidate while preserving unsaved windows and the running daemon; it does not
start, stop or checkpoint them. The existing application's update control then
offers the reviewed apply/reopen flow. Resolve any save blockers before applying:
staging makes the candidate available but does not complete the application handoff.
This option cannot be combined with restart, relaunch, uninstall or purge options.

#### Withdraw or re-select a staged build

After a successful install or stage, cagents keeps the version served by
`current`, the version it replaced, and any version still used by a running
process or the installation's runtime owner. A recovered or abandoned retry
candidate can be removed after two weeks, once a standalone install has
verified recovery is settled. Unrecognized files and directories are left alone.

To change which retained version `current` serves without applying it, add
`--select VERSION` to `--stage-only`.

```sh
CAGENTS_ARCHIVE=dist/cagents-RELEASE-x86_64-unknown-linux-gnu.tar.gz \
CAGENTS_CHECKSUMS=dist/SHA256SUMS \
sh dist/install.sh --select 0.1.3+local.582a054bc --stage-only
```

Select the version your windows and daemon already run to withdraw a staged
build: the windows stop offering **Update and reopen**, and launching cagents
opens that same version. Select the staged build again to restore its offer
exactly as staging left it. Selection installs nothing and removes no version;
it does not close windows, start, stop or checkpoint the daemon.

The installer and archive only provide a verified coordinator, so the installed
builds do not need to support selection. Run it from a release that does,
either a local `dist/` directory or a downloaded release (`--version` names that
release, not the selected one). Selection holds the same launch fence and
ownership checks as `--stage-only`, and it refuses when:

- the version is not a complete matched set in this installation's `versions/`;
- the daemon and the live windows run different versions, for example after
  windows reopened on a compatible build that shares an older daemon. Selection
  never closes windows, so it cannot withdraw that build;
- windows or a daemon are running, and the version is neither the one they run
  nor a build staged over it;
- the staged build was recorded as sharing the running daemon, and that daemon
  has since stopped. Start the original version's daemon before re-selecting
  the staged build;
- an application update is in progress, or an interrupted update's recovery
  is still pending;
- the version is a candidate the installer has not verified, for example one
  kept after an update rolled back or could not verify it was ready. Builds
  that an earlier release installed through an application update are
  refused the same way unless they are current or running. Install that
  release again to verify it;
- `current` changed while the selection was being prepared.

With no window or daemon running, any complete installed version other than
such a candidate can be selected. Selecting an older version this way is like
installing an older release on purpose: its daemon cannot open a shared
workspace store that a newer release converted (see [Daemon](daemon.md)).

A withdrawn build stays installed, so windows do not offer to download that
release again; select it to restore it.

Run it from your own terminal; an agent session cannot select. A focused
window rechecks the installation every 10 seconds and an unfocused one when it
regains focus, so a window can briefly keep showing a withdrawn offer.
Clicking it then is refused with "the installed candidate changed" and changes
nothing.

For an explicitly authorized direct daemon replacement, append
`--restart-daemon` to the installer invocation. The coordinator saves and closes
application windows before checkpointing. Supported holders retain live
processes; other recoverable sessions resume in new processes. This legacy
direct path authorizes interruption and requires checkpoint support in the
running daemon's own CLI. Prefer the application or `daemon upgrade` for a
session-by-session preview and waiting for native turns.

| Option | Effect |
| --- | --- |
| `--stage-only` | Publish a verified candidate without closing windows or changing the daemon |
| `--select VERSION` | With `--stage-only`, serve an installed version instead of publishing this release |
| `--restart-daemon` | Checkpoint supported sessions and replace the running daemon |
| `--require-empty-daemon` | With `--restart-daemon`, refuse rather than interrupt live sessions |
| `--relaunch-agents` | With `--restart-daemon`, relaunch agents that have no exact resume identity into their resume picker (requires an installed CLI that can record this) |
| `--no-relaunch-agents` | With `--restart-daemon`, refuse the upgrade while any agent has no exact resume identity |

Without either relaunch option, the installed CLI decides: it follows the
`relaunch_agents_on_upgrade` setting, which is on by default. An installed CLI
older than that setting never relaunches unless `--relaunch-agents` is given.

`--restart-daemon` cannot be combined with `--uninstall` or `--purge`. The full
handoff, rollback behavior, and the one-time bootstrap from legacy
installations are described in [Daemon and upgrades](daemon.md#full-daemon-upgrades).

## Uninstall or purge

For an `install.sh` installation, run the installed helper. A normal uninstall
keeps settings, sessions, transcripts, caches and diagnostics:

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall"
```

To remove both program files and all cagents user data, meaning the `cagents`
directories under `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`, `$XDG_CACHE_HOME` and
`$XDG_STATE_HOME` (see [Files and directories](configuration.md#files-and-directories)):

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall" --purge
```

The downloaded installer offers the equivalent `--uninstall` and `--purge`
options, and both scripts refuse to run as root. If you installed with a custom
`--prefix`, run that prefix's `current/bin/cagents-uninstall` with the same
`--prefix`.

Both entry points shut the daemon down only if it is empty, atomically. If live
terminals or spawn reservations remain, removal fails without terminating them
or removing program files. Finish them, or run `cagents daemon stop --confirm`
first. Removal also refuses, without removing anything, while the daemon is
still starting or stopping or an application update is in progress; retry once
it settles. It removes the updater's state with the program files, so the same
prefix can be reinstalled. Files you added to the prefix are kept, along with
its installation marker. Like installation, removal refuses a `--prefix` that is
a symlink; pass the real directory. It uses util-linux `flock` to hold the
updater's lock.

Remove a Debian installation with:

```sh
sudo apt remove cagents             # full package
sudo apt remove cagents-browserless # browserless package
```

APT removal and purge do not delete files in users' XDG directories.
